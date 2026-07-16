//! Account-scoped Codex Weekly historical pace.
//!
//! The history file is deliberately a clean-start v2 store.  Its owner is the
//! Rust FFI layer: raw quota readings are validated, sampled, persisted, and
//! evaluated here so the presentation layer receives one coherent projection
//! instead of recomputing ETA or risk from an incomplete set of scalars.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

const SCHEMA_VERSION: u32 = 2;
const V2_FILE_NAME: &str = "codex-weekly-history-v2.json";
const WRITE_INTERVAL_SECS: i64 = 30 * 60;
const WRITE_DELTA_PERCENT: f64 = 1.0;
const SAMPLE_BUCKET_SECS: i64 = 30 * 60;
const RETENTION_SECS: i64 = 56 * 86_400;
const MIN_WEEK_SAMPLES: usize = 6;
const BOUNDARY_COVERAGE_SECS: i64 = 24 * 60 * 60;
const MIN_HISTORICAL_WEEKS: usize = 3;
const MIN_RISK_WEEKS: usize = 5;
const GRID_POINT_COUNT: usize = 169;
const RECENCY_TAU_WEEKS: f64 = 3.0;
const EPSILON: f64 = 1e-9;
const RUNOUT_THRESHOLD_PERCENT: f64 = 100.0 - EPSILON;

/// The full load → quarantine/record → save → evaluate cycle is serialized in
/// this process.  This matters because FFI calls can arrive concurrently from
/// multiple polling tasks and a read-modify-write without this guard loses
/// samples silently.
static HISTORY_CYCLE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalPace {
    pub expected_percent: f64,
    pub eta_seconds: Option<f64>,
    pub will_last_to_reset: bool,
    pub run_out_probability: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Sample {
    account_key: String,
    resets_at: i64,
    window_minutes: i64,
    used_percent: f64,
    sampled_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Store {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    samples: Vec<Sample>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            samples: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct WeekProfile {
    resets_at: i64,
    curve: Vec<f64>,
}

#[derive(Debug)]
struct LoadedStore {
    store: Store,
    recovered_corrupt: bool,
    normalized_changed: bool,
}

/// Record the current Codex Weekly reading and return a coherent historical
/// projection when at least three complete historical weeks are available.
///
/// An unknown account owner is fail-closed: no path is loaded or created.  The
/// public entry point resolves the production v2 path; tests and callers that
/// need hermetic storage use [`record_and_evaluate_at_path`].
pub fn record_and_evaluate(
    account_key: &str,
    resets_at: i64,
    window_minutes: i64,
    used_percent: f64,
    now: i64,
) -> Option<HistoricalPace> {
    let path = store_path()?;
    record_and_evaluate_at_path(
        account_key,
        resets_at,
        window_minutes,
        used_percent,
        now,
        &path,
    )
}

/// Testable path-injected variant of [`record_and_evaluate`].  The production
/// runtime never passes the legacy path; this helper exists so persistence and
/// recovery behavior can be tested without touching user data.
pub(crate) fn record_and_evaluate_at_path(
    account_key: &str,
    resets_at: i64,
    window_minutes: i64,
    used_percent: f64,
    now: i64,
    path: &Path,
) -> Option<HistoricalPace> {
    if account_key.trim().is_empty()
        || !used_percent.is_finite()
        || !valid_current_window(resets_at, window_minutes, now)
    {
        return None;
    }

    let _guard = HISTORY_CYCLE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let loaded = load_store(path, now).ok()?;
    let mut store = loaded.store;
    let normalized_reset = normalize_reset(resets_at);
    let mut changed = loaded.normalized_changed;
    if prune_samples(&mut store.samples, now) {
        changed = true;
    }

    // A zero reading is useful as the current evaluator input, but it is not a
    // learning sample.  In particular, a sliding reset horizon must not create
    // a synthetic complete week out of repeated zero readings.
    let accepts_sample = used_percent.is_finite() && used_percent > 0.0 && used_percent <= 100.0;
    let mut accepted_sample = false;
    if accepts_sample {
        let sample = Sample {
            account_key: account_key.trim().to_string(),
            resets_at: normalized_reset,
            window_minutes,
            used_percent,
            sampled_at: now,
        };
        if record_sample(&mut store.samples, sample) {
            changed = true;
            accepted_sample = true;
        }
    }

    // A corrupt source is not allowed to produce a result until a valid cycle
    // has successfully replaced it with a new v2 file.  A zero reading is
    // intentionally non-persistent, so it cannot complete that recovery save.
    if loaded.recovered_corrupt && !accepted_sample {
        return None;
    }
    if changed && save_store_atomic(path, &store).is_err() {
        return None;
    }

    evaluate_samples(
        &store.samples,
        account_key.trim(),
        normalized_reset,
        window_minutes,
        now,
        used_percent,
    )
}

fn valid_current_window(resets_at: i64, window_minutes: i64, now: i64) -> bool {
    let Some(duration) = window_minutes.checked_mul(60) else {
        return false;
    };
    if duration <= 0 {
        return false;
    }
    let normalized_reset = normalize_reset(resets_at);
    let Some(time_until_reset) = normalized_reset.checked_sub(now) else {
        return false;
    };
    time_until_reset > 0 && time_until_reset <= duration
}

/// Accept a sample only when the normalized window changed, thirty minutes
/// elapsed, or usage moved by at least one percentage point.  Accepted values
/// are then deduplicated by the normalized reset and a thirty-minute sample
/// bucket.
fn should_accept(samples: &[Sample], next: &Sample) -> bool {
    let prior = samples
        .iter()
        .filter(|sample| {
            sample.account_key == next.account_key && sample.window_minutes == next.window_minutes
        })
        .max_by_key(|sample| sample.sampled_at);
    match prior {
        None => true,
        Some(prior) => {
            prior.resets_at != next.resets_at
                || next.sampled_at.saturating_sub(prior.sampled_at) >= WRITE_INTERVAL_SECS
                || (next.used_percent - prior.used_percent).abs() >= WRITE_DELTA_PERCENT
        }
    }
}

fn record_sample(samples: &mut Vec<Sample>, next: Sample) -> bool {
    if !valid_sample(&next) || !should_accept(samples, &next) {
        return false;
    }

    let bucket = sample_bucket(next.sampled_at);
    if let Some(index) = samples.iter().position(|sample| {
        sample.account_key == next.account_key
            && sample.resets_at == next.resets_at
            && sample.window_minutes == next.window_minutes
            && sample_bucket(sample.sampled_at) == bucket
    }) {
        // Keep the newest reading in a bucket.  An out-of-order accepted
        // reading may still change a value, but must not replace a newer one.
        if samples[index].sampled_at <= next.sampled_at {
            samples[index] = next;
            true
        } else {
            false
        }
    } else {
        samples.push(next);
        true
    }
}

fn prune_samples(samples: &mut Vec<Sample>, now: i64) -> bool {
    let cutoff = now.saturating_sub(RETENTION_SECS);
    let before = samples.len();
    samples.retain(|sample| sample.sampled_at >= cutoff);
    samples.len() != before
}

fn sample_bucket(sampled_at: i64) -> i64 {
    sampled_at.div_euclid(SAMPLE_BUCKET_SECS)
}

fn valid_sample(sample: &Sample) -> bool {
    if sample.account_key.trim().is_empty()
        || sample.window_minutes <= 0
        || !sample.used_percent.is_finite()
        || !(0.0 < sample.used_percent && sample.used_percent <= 100.0)
    {
        return false;
    }
    let Some(duration) = sample.window_minutes.checked_mul(60) else {
        return false;
    };
    let normalized_reset = normalize_reset(sample.resets_at);
    let Some(window_start) = normalized_reset.checked_sub(duration) else {
        return false;
    };
    sample.sampled_at >= window_start && sample.sampled_at <= normalized_reset
}

fn normalize_reset(value: i64) -> i64 {
    // Quota timestamps are ordinary Unix seconds.  Using f64 here mirrors the
    // upstream nearest-bucket behavior while keeping the code independent of a
    // date/time crate in this persistence module.
    ((value as f64 / 300.0).round() as i64).saturating_mul(300)
}

fn load_store(path: &Path, now: i64) -> io::Result<LoadedStore> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadedStore {
                store: Store::default(),
                recovered_corrupt: false,
                normalized_changed: false,
            });
        }
        Err(error) => return Err(error),
    };

    let parsed = serde_json::from_slice::<Store>(&bytes)
        .ok()
        .filter(|store| store.schema_version == SCHEMA_VERSION);
    let Some(mut store) = parsed else {
        quarantine_corrupt(path, now)?;
        return Ok(LoadedStore {
            store: Store::default(),
            recovered_corrupt: true,
            normalized_changed: false,
        });
    };

    // Normalize and reject invalid entries on read as well as on write.  This
    // makes a hand-edited but structurally valid v2 file fail closed per sample
    // rather than letting an invalid sample create a complete week.
    let original_samples = store.samples.clone();
    for sample in &mut store.samples {
        sample.account_key = sample.account_key.trim().to_string();
        sample.resets_at = normalize_reset(sample.resets_at);
    }
    store.samples.retain(valid_sample);
    dedupe_samples(&mut store.samples);
    let normalized_changed = store.samples != original_samples;
    Ok(LoadedStore {
        store,
        recovered_corrupt: false,
        normalized_changed,
    })
}

fn dedupe_samples(samples: &mut Vec<Sample>) {
    let mut deduped: BTreeMap<(String, i64, i64, i64), Sample> = BTreeMap::new();
    for sample in samples.drain(..) {
        let key = (
            sample.account_key.clone(),
            sample.resets_at,
            sample.window_minutes,
            sample_bucket(sample.sampled_at),
        );
        match deduped.get(&key) {
            Some(existing) if existing.sampled_at >= sample.sampled_at => {}
            _ => {
                deduped.insert(key, sample);
            }
        }
    }
    *samples = deduped.into_values().collect();
}

fn quarantine_corrupt(path: &Path, now: i64) -> io::Result<PathBuf> {
    quarantine_corrupt_with(path, now, |source, destination| {
        fs::rename(source, destination)
    })
}

fn quarantine_corrupt_with<F>(path: &Path, now: i64, rename: F) -> io::Result<PathBuf>
where
    F: Fn(&Path, &Path) -> io::Result<()>,
{
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let base = format!("codex-weekly-history-v2.corrupt-{}.json", now);
    for suffix in 0..=u32::MAX {
        let name = if suffix == 0 {
            base.clone()
        } else {
            format!("codex-weekly-history-v2.corrupt-{}.{}.json", now, suffix)
        };
        let candidate = directory.join(name);
        if candidate.exists() {
            continue;
        }
        rename(path, &candidate)?;
        return Ok(candidate);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to choose a corrupt-history quarantine name",
    ))
}

fn save_store_atomic(path: &Path, store: &Store) -> io::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;

    let mut samples = store.samples.clone();
    samples.sort_by(|lhs, rhs| {
        lhs.account_key
            .cmp(&rhs.account_key)
            .then(lhs.resets_at.cmp(&rhs.resets_at))
            .then(lhs.window_minutes.cmp(&rhs.window_minutes))
            .then(lhs.sampled_at.cmp(&rhs.sampled_at))
            .then(lhs.used_percent.total_cmp(&rhs.used_percent))
    });
    let payload = serde_json::to_vec_pretty(&Store {
        schema_version: SCHEMA_VERSION,
        samples,
    })
    .map_err(io::Error::other)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(V2_FILE_NAME);
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_name = format!(".{}.tmp-{}-{}", file_name, std::process::id(), counter);
    let temp_path = directory.join(temp_name);

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(&payload)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        tokscale_core::fs_atomic::replace_file(&temp_path, path)?;
        #[cfg(unix)]
        if let Ok(directory_file) = fs::File::open(directory) {
            let _ = directory_file.sync_all();
        }
        Ok::<(), io::Error>(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn store_path() -> Option<PathBuf> {
    Some(
        dirs::data_dir()?
            .join("com.nyanako.tokenbar")
            .join(V2_FILE_NAME),
    )
}

fn build_dataset(
    samples: &[Sample],
    account_key: &str,
    current_resets_at: i64,
    window_minutes: i64,
    now: i64,
) -> Vec<WeekProfile> {
    let Some(duration) = window_minutes.checked_mul(60) else {
        return Vec::new();
    };
    if duration <= 0 {
        return Vec::new();
    }

    let current_reset = normalize_reset(current_resets_at);
    let mut normalized = samples.to_vec();
    for sample in &mut normalized {
        sample.resets_at = normalize_reset(sample.resets_at);
    }
    normalized.retain(valid_sample);
    dedupe_samples(&mut normalized);

    let mut grouped: BTreeMap<i64, Vec<Sample>> = BTreeMap::new();
    for sample in normalized {
        if sample.account_key != account_key
            || sample.window_minutes != window_minutes
            || sample.resets_at > now
            || sample.resets_at >= current_reset
        {
            continue;
        }
        grouped.entry(sample.resets_at).or_default().push(sample);
    }

    grouped
        .into_iter()
        .filter_map(|(resets_at, week_samples)| {
            let window_start = resets_at.checked_sub(duration)?;
            if !is_complete_week(&week_samples, window_start, resets_at) {
                return None;
            }
            let curve = reconstruct_curve(&week_samples, window_start, duration)?;
            Some(WeekProfile { resets_at, curve })
        })
        .collect()
}

fn is_complete_week(samples: &[Sample], window_start: i64, resets_at: i64) -> bool {
    if samples.len() < MIN_WEEK_SAMPLES {
        return false;
    }
    let has_start = samples.iter().any(|sample| {
        sample.sampled_at >= window_start
            && sample.sampled_at <= window_start.saturating_add(BOUNDARY_COVERAGE_SECS)
    });
    let has_end = samples.iter().any(|sample| {
        sample.sampled_at >= resets_at.saturating_sub(BOUNDARY_COVERAGE_SECS)
            && sample.sampled_at <= resets_at
    });
    has_start && has_end
}

fn reconstruct_curve(samples: &[Sample], window_start: i64, duration: i64) -> Option<Vec<f64>> {
    if samples.is_empty() || duration <= 0 {
        return None;
    }
    let mut points = samples
        .iter()
        .map(|sample| {
            (
                ((sample.sampled_at - window_start) as f64 / duration as f64).clamp(0.0, 1.0),
                sample.used_percent.clamp(0.0, 100.0),
            )
        })
        .collect::<Vec<_>>();
    points.sort_by(|lhs, rhs| lhs.0.total_cmp(&rhs.0).then(lhs.1.total_cmp(&rhs.1)));

    let mut monotone_points = Vec::with_capacity(points.len() + 2);
    let mut running_max: f64 = 0.0;
    for (u, value) in points {
        running_max = running_max.max(value);
        monotone_points.push((u, running_max));
    }
    let end_value = monotone_points.last()?.1;
    monotone_points.push((0.0, 0.0));
    monotone_points.push((1.0, end_value));
    monotone_points.sort_by(|lhs, rhs| lhs.0.total_cmp(&rhs.0).then(lhs.1.total_cmp(&rhs.1)));
    running_max = 0.0;
    for (_, value) in &mut monotone_points {
        running_max = running_max.max(*value);
        *value = running_max;
    }

    let mut curve = vec![0.0; GRID_POINT_COUNT];
    let mut upper_index = 1usize;
    for (index, value) in curve.iter_mut().enumerate() {
        let u = index as f64 / (GRID_POINT_COUNT - 1) as f64;
        while upper_index < monotone_points.len() && monotone_points[upper_index].0 < u {
            upper_index += 1;
        }
        if u <= monotone_points[0].0 {
            *value = monotone_points[0].1;
        } else if u >= monotone_points[monotone_points.len() - 1].0 {
            *value = monotone_points[monotone_points.len() - 1].1;
        } else {
            let hi = monotone_points[upper_index.min(monotone_points.len() - 1)];
            let lo = monotone_points[upper_index.saturating_sub(1)];
            if hi.0 <= lo.0 {
                *value = lo.1.max(hi.1);
            } else {
                let ratio = ((u - lo.0) / (hi.0 - lo.0)).clamp(0.0, 1.0);
                *value = lo.1 + (hi.1 - lo.1) * ratio;
            }
        }
    }
    let mut curve_max: f64 = 0.0;
    for value in &mut curve {
        *value = value.clamp(0.0, 100.0);
        curve_max = curve_max.max(*value);
        *value = curve_max;
    }
    Some(curve)
}

/// Evaluate a current window against v2 samples.  This function is kept
/// separate from disk I/O so evaluator fixtures can assert exact thresholds
/// and formulas without relying on a user's application-support directory.
fn evaluate_samples(
    samples: &[Sample],
    account_key: &str,
    resets_at: i64,
    window_minutes: i64,
    now: i64,
    used_percent: f64,
) -> Option<HistoricalPace> {
    if account_key.trim().is_empty()
        || !used_percent.is_finite()
        || !valid_current_window(resets_at, window_minutes, now)
    {
        return None;
    }
    let duration = window_minutes.checked_mul(60)?;
    let current_reset = normalize_reset(resets_at);
    let time_until_reset = current_reset.checked_sub(now)?;
    let elapsed = duration - time_until_reset;
    let actual = used_percent.clamp(0.0, 100.0);
    if elapsed == 0 && actual > 0.0 {
        return None;
    }
    let u_now = (elapsed as f64 / duration as f64).clamp(0.0, 1.0);

    let weeks = build_dataset(
        samples,
        account_key.trim(),
        current_reset,
        window_minutes,
        now,
    );
    if weeks.len() < MIN_HISTORICAL_WEEKS {
        return None;
    }

    let weighted_weeks = weeks
        .iter()
        .map(|week| {
            let age_weeks = ((current_reset - week.resets_at) as f64 / duration as f64).max(0.0);
            let weight = (-age_weeks / RECENCY_TAU_WEEKS).exp();
            (week, weight)
        })
        .collect::<Vec<_>>();
    let total_weight = weighted_weeks
        .iter()
        .map(|(_, weight)| *weight)
        .sum::<f64>();
    if total_weight <= EPSILON || !total_weight.is_finite() {
        return None;
    }
    let total_weight_squared = weighted_weeks
        .iter()
        .map(|(_, weight)| weight * weight)
        .sum::<f64>();
    let n_eff = if total_weight_squared > EPSILON {
        (total_weight * total_weight) / total_weight_squared
    } else {
        0.0
    };
    let lambda = ((n_eff - 2.0) / 6.0).clamp(0.0, 1.0);

    let mut expected_curve = vec![0.0; GRID_POINT_COUNT];
    let denominator = (GRID_POINT_COUNT - 1) as f64;
    for (index, value) in expected_curve.iter_mut().enumerate() {
        let historical_values = weighted_weeks
            .iter()
            .map(|(week, _)| week.curve[index])
            .collect::<Vec<_>>();
        let weights = weighted_weeks
            .iter()
            .map(|(_, weight)| *weight)
            .collect::<Vec<_>>();
        let historical_median = weighted_median(&historical_values, &weights);
        let linear_baseline = 100.0 * (index as f64 / denominator);
        *value = (lambda * historical_median + (1.0 - lambda) * linear_baseline).clamp(0.0, 100.0);
    }
    let mut expected_max: f64 = 0.0;
    for value in &mut expected_curve {
        expected_max = expected_max.max(*value);
        *value = expected_max;
    }
    let expected_now = interpolate(&expected_curve, u_now).clamp(0.0, 100.0);

    let mut weighted_run_out_mass = 0.0;
    let mut crossing_candidates = Vec::new();
    for (week, weight) in &weighted_weeks {
        let mut extended_curve = week.curve.clone();
        if let Some(cap_index) = extended_curve
            .iter()
            .position(|value| *value >= RUNOUT_THRESHOLD_PERCENT)
            .filter(|index| *index > 0 && *index < extended_curve.len() - 1)
        {
            let u_cap = cap_index as f64 / denominator;
            let value_at_cap = extended_curve[cap_index];
            let slope = value_at_cap / u_cap;
            if slope.is_finite() {
                for (index, value) in extended_curve.iter_mut().enumerate().skip(cap_index) {
                    *value = slope * (index as f64 / denominator);
                }
            }
        }

        let week_now = interpolate(&extended_curve, u_now);
        let shift = actual - week_now;
        let shifted_end = extended_curve.last().copied().unwrap_or(0.0) + shift;
        let run_out = shifted_end >= RUNOUT_THRESHOLD_PERCENT;
        if run_out {
            weighted_run_out_mass += *weight;
            if let Some(crossing_u) = first_crossing(u_now, &extended_curve, shift, actual) {
                crossing_candidates
                    .push(((crossing_u - u_now).max(0.0) * duration as f64, *weight));
            }
        }
    }

    let smoothed_probability =
        ((weighted_run_out_mass + 0.5) / (total_weight + 1.0)).clamp(0.0, 1.0);
    let mut run_out_probability = (weeks.len() >= MIN_RISK_WEEKS).then_some(smoothed_probability);
    let mut will_last_to_reset = smoothed_probability < 0.5;
    let mut eta_seconds = None;

    if actual >= 100.0 {
        will_last_to_reset = false;
        eta_seconds = Some(0.0);
        run_out_probability = Some(1.0);
    } else if !will_last_to_reset {
        if crossing_candidates.is_empty() {
            // The invariant is stronger than the probability label: never
            // expose `false + nil` when no crossing can be located.
            will_last_to_reset = true;
        } else {
            let values = crossing_candidates
                .iter()
                .map(|(eta, _)| *eta)
                .collect::<Vec<_>>();
            let weights = crossing_candidates
                .iter()
                .map(|(_, weight)| *weight)
                .collect::<Vec<_>>();
            eta_seconds = Some(weighted_median(&values, &weights).max(0.0));
        }
    }

    Some(HistoricalPace {
        expected_percent: expected_now,
        eta_seconds,
        will_last_to_reset,
        run_out_probability,
    })
}

fn first_crossing(u_now: f64, curve: &[f64], shift: f64, actual_at_now: f64) -> Option<f64> {
    if curve.len() < 2 {
        return None;
    }
    let denominator = (curve.len() - 1) as f64;
    let mut previous_u = u_now;
    let mut previous_value = actual_at_now;
    let start_index = ((u_now * denominator).floor() as usize + 1).clamp(1, curve.len() - 1);
    for (index, curve_value) in curve.iter().enumerate().skip(start_index) {
        let u = index as f64 / denominator;
        if u <= u_now + EPSILON {
            continue;
        }
        let value = *curve_value + shift;
        if previous_value < 100.0 - EPSILON && value >= 100.0 - EPSILON {
            let delta = value - previous_value;
            if delta.abs() <= EPSILON {
                return Some(u);
            }
            let ratio = ((100.0 - previous_value) / delta).clamp(0.0, 1.0);
            return Some((previous_u + ratio * (u - previous_u)).clamp(u_now, 1.0));
        }
        previous_u = u;
        previous_value = value;
    }
    None
}

fn interpolate(curve: &[f64], u: f64) -> f64 {
    if curve.is_empty() {
        return 0.0;
    }
    if curve.len() == 1 {
        return curve[0];
    }
    let scaled = u.clamp(0.0, 1.0) * (curve.len() - 1) as f64;
    let lower = scaled.floor() as usize;
    let upper = (lower + 1).min(curve.len() - 1);
    if lower == upper {
        curve[lower]
    } else {
        curve[lower] + (curve[upper] - curve[lower]) * (scaled - lower as f64)
    }
}

fn weighted_median(values: &[f64], weights: &[f64]) -> f64 {
    if values.len() != weights.len() || values.is_empty() {
        return 0.0;
    }
    let mut pairs = values
        .iter()
        .copied()
        .zip(weights.iter().copied().map(|weight| weight.max(0.0)))
        .collect::<Vec<_>>();
    pairs.sort_by(|lhs, rhs| lhs.0.total_cmp(&rhs.0));
    let total_weight = pairs.iter().map(|(_, weight)| *weight).sum::<f64>();
    if total_weight <= EPSILON {
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        return sorted[sorted.len() / 2];
    }
    let threshold = total_weight / 2.0;
    let mut cumulative = 0.0;
    let fallback = pairs.last().map(|(value, _)| *value).unwrap_or(0.0);
    for (value, weight) in pairs {
        cumulative += weight;
        if cumulative >= threshold {
            return value;
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const WEEK_MINUTES: i64 = 10_080;
    const WEEK_SECS: i64 = WEEK_MINUTES * 60;

    fn sample(account: &str, reset: i64, used: f64, sampled_at: i64) -> Sample {
        Sample {
            account_key: account.to_string(),
            resets_at: normalize_reset(reset),
            window_minutes: WEEK_MINUTES,
            used_percent: used,
            sampled_at,
        }
    }

    fn temp_path(label: &str) -> (PathBuf, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "tokenbar-history-v2-{}-{}-{}",
            std::process::id(),
            nonce,
            label
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(V2_FILE_NAME);
        (directory, path)
    }

    fn complete_week(account: &str, reset: i64, end_value: f64) -> Vec<Sample> {
        let start = reset - WEEK_SECS;
        [0.01, 0.1, 0.3, 0.5, 0.7, 0.99]
            .into_iter()
            .enumerate()
            .map(|(index, fraction)| {
                sample(
                    account,
                    reset,
                    end_value * fraction,
                    start + (fraction * WEEK_SECS as f64) as i64 + index as i64,
                )
            })
            .collect()
    }

    fn seed_weeks(account: &str, current_reset: i64, count: usize, end_value: f64) -> Vec<Sample> {
        (1..=count)
            .flat_map(|offset| {
                complete_week(
                    account,
                    current_reset - offset as i64 * WEEK_SECS,
                    end_value,
                )
            })
            .collect()
    }

    #[test]
    fn schema_is_v2_and_path_is_clean_start() {
        let (directory, path) = temp_path("schema");
        let now = 100 * WEEK_SECS;
        let reset = now + WEEK_SECS / 2;
        assert!(
            record_and_evaluate_at_path("acct", reset, WEEK_MINUTES, 10.0, now, &path).is_none()
        );
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["schemaVersion"], 2);
        assert!(value["samples"].is_array());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn zero_samples_are_not_persisted_or_evaluated_as_weeks() {
        let (directory, path) = temp_path("zero");
        let now = 100 * WEEK_SECS;
        let reset = now + WEEK_SECS / 2;
        for offset in 0..8 {
            let sampled_at = now + offset * SAMPLE_BUCKET_SECS;
            let sliding_reset = reset + offset * SAMPLE_BUCKET_SECS;
            assert!(record_and_evaluate_at_path(
                "acct",
                sliding_reset,
                WEEK_MINUTES,
                0.0,
                sampled_at,
                &path,
            )
            .is_none());
        }
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reset_jitter_and_bucket_dedupe_do_not_bypass_throttle() {
        let mut samples = Vec::new();
        let reset = 100 * WEEK_SECS + 7;
        let first = sample("acct", reset, 10.0, reset - WEEK_SECS + 1_800);
        assert!(record_sample(&mut samples, first));
        // A 60-second reset jitter normalizes to the same key and the small
        // usage change is below the one-point threshold.
        assert!(!record_sample(
            &mut samples,
            sample("acct", reset + 60, 10.1, reset - WEEK_SECS + 1_860)
        ));
        // A meaningful change in the same bucket replaces the bucket's value,
        // rather than creating a second point.
        assert!(record_sample(
            &mut samples,
            sample("acct", reset + 60, 12.0, reset - WEEK_SECS + 1_870)
        ));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].used_percent, 12.0);
    }

    #[test]
    fn complete_week_requires_six_deduped_samples_and_both_boundaries() {
        let now = 100 * WEEK_SECS;
        let current_reset = now + WEEK_SECS / 2;
        let mut samples = complete_week("acct", now - WEEK_SECS, 100.0);
        samples.pop();
        assert!(build_dataset(&samples, "acct", current_reset, WEEK_MINUTES, now).is_empty());

        let mut samples = complete_week("acct", now - WEEK_SECS, 100.0);
        samples[0].sampled_at = now - WEEK_SECS + 2 * BOUNDARY_COVERAGE_SECS;
        assert!(build_dataset(&samples, "acct", current_reset, WEEK_MINUTES, now).is_empty());
    }

    #[test]
    fn future_reset_fragment_is_not_a_complete_week() {
        let now = 100 * WEEK_SECS;
        let future_reset = now + WEEK_SECS / 2;
        let current_reset = now + WEEK_SECS;
        let samples = complete_week("acct", future_reset, 80.0);
        assert!(build_dataset(&samples, "acct", current_reset, WEEK_MINUTES, now).is_empty());
    }

    #[test]
    fn account_isolation_and_unknown_owner_fail_closed() {
        let (directory, path) = temp_path("accounts");
        let now = 100 * WEEK_SECS;
        let reset = now + WEEK_SECS / 2;
        assert!(record_and_evaluate_at_path("", reset, WEEK_MINUTES, 10.0, now, &path).is_none());
        assert!(!path.exists());

        let samples = seed_weeks("a", reset, MIN_HISTORICAL_WEEKS, 80.0);
        assert!(evaluate_samples(&samples, "b", reset, WEEK_MINUTES, now, 50.0).is_none());
        assert!(evaluate_samples(&samples, "a", reset, WEEK_MINUTES, now, 50.0).is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_bytes_are_quarantined_byte_for_byte_then_recovered() {
        let (directory, path) = temp_path("corrupt");
        let corrupt = b"{not-json\nbytes";
        fs::write(&path, corrupt).unwrap();
        let now = 100 * WEEK_SECS;
        let reset = now + WEEK_SECS / 2;
        assert!(
            record_and_evaluate_at_path("acct", reset, WEEK_MINUTES, 10.0, now, &path).is_none()
        );
        let quarantine = directory.join(format!("codex-weekly-history-v2.corrupt-{}.json", now));
        assert_eq!(fs::read(quarantine).unwrap(), corrupt);
        assert!(path.exists());
        let recovered: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(recovered["schemaVersion"], 2);
        // A later cycle must reload the recovered v2 file rather than starting
        // over, and must append the new cadence bucket without losing the first
        // valid sample.
        assert!(record_and_evaluate_at_path(
            "acct",
            reset,
            WEEK_MINUTES,
            12.0,
            now + SAMPLE_BUCKET_SECS,
            &path
        )
        .is_none());
        let recovered_store: Store = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(recovered_store.samples.len(), 2);
        assert!(recovered_store
            .samples
            .iter()
            .any(|sample| (sample.used_percent - 10.0).abs() < f64::EPSILON));
        assert!(recovered_store
            .samples
            .iter()
            .any(|sample| (sample.used_percent - 12.0).abs() < f64::EPSILON));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_quarantine_failure_preserves_original_bytes_and_path() {
        let (directory, path) = temp_path("quarantine-failure");
        let corrupt = b"corrupt but irreplaceable";
        fs::write(&path, corrupt).unwrap();
        let result = quarantine_corrupt_with(&path, 123, |_source, _destination| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected rename failure",
            ))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
        assert!(!directory
            .join("codex-weekly-history-v2.corrupt-123.json")
            .exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retention_prunes_old_samples_before_reloading_current_store() {
        let (directory, path) = temp_path("retention");
        let now = 100 * WEEK_SECS;
        let current_reset = now + WEEK_SECS / 2;
        let old_sampled_at = now - RETENTION_SECS - 1;
        let old_reset = old_sampled_at + WEEK_SECS / 2;
        let initial = Store {
            schema_version: SCHEMA_VERSION,
            samples: vec![sample("acct", old_reset, 20.0, old_sampled_at)],
        };
        fs::write(&path, serde_json::to_vec(&initial).unwrap()).unwrap();
        assert!(
            record_and_evaluate_at_path("acct", current_reset, WEEK_MINUTES, 10.0, now, &path)
                .is_none()
        );
        let reloaded: Store = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(reloaded.samples.len(), 1);
        assert_eq!(reloaded.samples[0].used_percent, 10.0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn quarantine_collision_uses_suffix_without_overwriting_evidence() {
        let (directory, path) = temp_path("collision");
        let now = 100 * WEEK_SECS;
        let base = directory.join(format!("codex-weekly-history-v2.corrupt-{}.json", now));
        let one = directory.join(format!("codex-weekly-history-v2.corrupt-{}.1.json", now));
        fs::write(&base, b"first").unwrap();
        fs::write(&one, b"second").unwrap();
        fs::write(&path, b"third").unwrap();
        let reset = now + WEEK_SECS / 2;
        assert!(
            record_and_evaluate_at_path("acct", reset, WEEK_MINUTES, 10.0, now, &path).is_none()
        );
        assert_eq!(fs::read(&base).unwrap(), b"first");
        assert_eq!(fs::read(&one).unwrap(), b"second");
        assert_eq!(
            fs::read(directory.join(format!("codex-weekly-history-v2.corrupt-{}.2.json", now)))
                .unwrap(),
            b"third"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn evaluator_thresholds_and_exhausted_override_match_contract() {
        let now = 100 * WEEK_SECS;
        let current_reset = now + WEEK_SECS / 2;
        let two = seed_weeks("acct", current_reset, 2, 80.0);
        assert!(evaluate_samples(&two, "acct", current_reset, WEEK_MINUTES, now, 50.0).is_none());

        let three = seed_weeks("acct", current_reset, 3, 80.0);
        let pace =
            evaluate_samples(&three, "acct", current_reset, WEEK_MINUTES, now, 50.0).unwrap();
        assert!(pace.run_out_probability.is_none());
        assert!(pace.eta_seconds.is_none() || pace.eta_seconds.unwrap() >= 0.0);

        // Exhaustion is observed fact, so it overrides the usual five-week
        // risk publication threshold as soon as historical pace is available.
        let exhausted =
            evaluate_samples(&three, "acct", current_reset, WEEK_MINUTES, now, 100.0).unwrap();
        assert_eq!(exhausted.eta_seconds, Some(0.0));
        assert!(!exhausted.will_last_to_reset);
        assert_eq!(exhausted.run_out_probability, Some(1.0));

        let five = seed_weeks("acct", current_reset, 5, 80.0);
        let mature =
            evaluate_samples(&five, "acct", current_reset, WEEK_MINUTES, now, 50.0).unwrap();
        assert!(mature
            .run_out_probability
            .is_some_and(|probability| (0.0..=1.0).contains(&probability)));
        assert_eq!(mature.eta_seconds.is_none(), mature.will_last_to_reset);
    }

    #[test]
    fn front_loaded_history_can_set_expectation_above_linear_pace() {
        let current_reset = 100 * WEEK_SECS;
        let now = current_reset - (9 * WEEK_SECS / 10);
        let fractions = [0.01, 0.1, 0.3, 0.5, 0.7, 0.99];
        let used_values = [5.0, 60.0, 65.0, 70.0, 75.0, 80.0];
        let mut samples = Vec::new();

        for offset in 1..=MIN_HISTORICAL_WEEKS {
            let reset = current_reset - offset as i64 * WEEK_SECS;
            let start = reset - WEEK_SECS;
            samples.extend(fractions.into_iter().zip(used_values).enumerate().map(
                |(index, (fraction, used))| {
                    sample(
                        "acct",
                        reset,
                        used,
                        start + (fraction * WEEK_SECS as f64) as i64 + index as i64,
                    )
                },
            ));
        }

        let pace =
            evaluate_samples(&samples, "acct", current_reset, WEEK_MINUTES, now, 50.0).unwrap();

        // At 10% elapsed, the linear baseline is 10%. A front-loaded but
        // quota-safe personal history must still be able to raise the blended
        // expectation above that baseline; risk and run-out remain separate.
        assert!(pace.expected_percent > 10.0);
        assert!(pace.expected_percent < 50.0);
        assert!(pace.will_last_to_reset);
    }

    #[test]
    fn reconstructed_curve_is_monotone_and_invalid_windows_fail_closed() {
        let reset = 100 * WEEK_SECS;
        let start = reset - WEEK_SECS;
        let samples = [10.0, 30.0, 25.0, 70.0, 65.0, 90.0]
            .into_iter()
            .enumerate()
            .map(|(index, used)| {
                sample("acct", reset, used, start + (index as i64 * WEEK_SECS / 5))
            })
            .collect::<Vec<_>>();
        let curve = reconstruct_curve(&samples, start, WEEK_SECS).unwrap();
        assert_eq!(curve.len(), GRID_POINT_COUNT);
        assert!(curve.windows(2).all(|pair| pair[0] <= pair[1]));

        let current_reset = reset + WEEK_SECS;
        assert!(!valid_current_window(reset, WEEK_MINUTES, reset));
        assert!(!valid_current_window(
            current_reset + 300,
            WEEK_MINUTES,
            reset
        ));
        assert!(!valid_current_window(current_reset, 0, reset));
        assert!(evaluate_samples(
            &samples,
            "acct",
            current_reset,
            WEEK_MINUTES,
            reset,
            f64::NAN,
        )
        .is_none());
    }

    #[test]
    fn current_usage_shift_and_capped_demand_produce_crossing_eta() {
        let now = 100 * WEEK_SECS;
        let current_reset = now + WEEK_SECS / 2;
        let mut samples = Vec::new();
        for offset in 1..=5 {
            let reset = current_reset - offset * WEEK_SECS;
            let start = reset - WEEK_SECS;
            let mut week = Vec::new();
            for (index, fraction) in [0.01, 0.2, 0.5, 0.7, 0.9, 0.99].into_iter().enumerate() {
                let used = if fraction <= 0.5 {
                    fraction * 200.0
                } else {
                    100.0
                };
                week.push(sample(
                    "acct",
                    reset,
                    used,
                    start + (fraction * WEEK_SECS as f64) as i64 + index as i64,
                ));
            }
            samples.extend(week);
        }
        let pace =
            evaluate_samples(&samples, "acct", current_reset, WEEK_MINUTES, now, 80.0).unwrap();
        assert!(!pace.will_last_to_reset);
        assert!(pace.eta_seconds.unwrap_or(-1.0) >= 0.0);
    }

    #[test]
    fn run_out_crossing_interpolates_with_unclamped_shifted_value() {
        let crossing = first_crossing(0.0, &[80.0, 120.0], 0.0, 80.0).unwrap();

        assert!((crossing - 0.5).abs() < EPSILON);
    }

    #[test]
    fn concurrent_cycles_preserve_both_account_samples() {
        let (directory, path) = temp_path("concurrency");
        let now = 100 * WEEK_SECS;
        let reset = now + WEEK_SECS / 2;
        let path_a = path.clone();
        let path_b = path.clone();
        let a = std::thread::spawn(move || {
            record_and_evaluate_at_path("a", reset, WEEK_MINUTES, 10.0, now, &path_a);
        });
        let b = std::thread::spawn(move || {
            record_and_evaluate_at_path("b", reset, WEEK_MINUTES, 20.0, now, &path_b);
        });
        a.join().unwrap();
        b.join().unwrap();
        let store: Store = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(store.samples.len(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn v1_sentinel_sibling_is_never_touched() {
        let (directory, path) = temp_path("legacy");
        let legacy = directory.join("codex-weekly-history.json");
        fs::write(&legacy, b"sentinel").unwrap();
        let before = fs::metadata(&legacy).unwrap().modified().unwrap();
        let now = 100 * WEEK_SECS;
        let reset = now + WEEK_SECS / 2;
        let _ = record_and_evaluate_at_path("acct", reset, WEEK_MINUTES, 10.0, now, &path);
        assert_eq!(fs::read(&legacy).unwrap(), b"sentinel");
        assert_eq!(fs::metadata(&legacy).unwrap().modified().unwrap(), before);
        fs::remove_dir_all(directory).unwrap();
    }
}
