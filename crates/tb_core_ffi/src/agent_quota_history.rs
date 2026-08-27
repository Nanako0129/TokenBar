//! Schema-4 provider-neutral quota pace history transaction.
//!
//! The schema counter and the `v3` in the store's filename are two different
//! numbers: the filename names the store family and was fixed when the store
//! was introduced, while `HISTORY_SCHEMA_VERSION` is the shape of what is
//! inside it and is now `4`.
//!
//! The locked v3 transaction owns sampling, cycle-aware retention, migration,
//! and the coherent historical evaluator. Provider adapters resolve identity
//! and duration, then call the APIs here; no provider fetch lives in this module.

#![allow(dead_code)]

use crate::agent_account_scope::HistoryScope;
use crate::agent_quota_duration::{
    self, observe_reset, valid_duration, DurationEvidence, DurationResolution, DurationSource,
    DurationUnavailableReason, ObservedState,
};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

pub(crate) const HISTORY_SCHEMA_VERSION: u32 = 4;
/// The version this store shipped as from v1.10.0 through v1.13.3, which is
/// every version that has written the file. `migrate_store_to_current` upgrades
/// it in memory on load; see `load_store_at_with_mode`.
pub(crate) const HISTORY_SCHEMA_VERSION_V3: u32 = 3;
/// `v3` in these names is the store *family*, fixed when the file was
/// introduced — not its schema version. The version lives in
/// `HISTORY_SCHEMA_VERSION` and in the file's own `schemaVersion` field. A v4
/// store is read from and written to these same paths; renaming them would
/// strand every existing file.
pub(crate) const HISTORY_FILE_NAME: &str = "quota-pace-history-v3.json";
pub(crate) const HISTORY_LOCK_FILE_NAME: &str = "quota-pace-v3.lock";
pub(crate) const LEGACY_V2_FILE_NAME: &str = "codex-weekly-history-v2.json";
/// Floor for `phase_bucket_count`, and the count every window used to get.
pub(crate) const PHASE_BUCKET_COUNT: usize = 48;
/// The only factor `phase_bucket_count` may apply to the floor. Whole, so the
/// finer grid nests inside the coarser one and no existing sample can change
/// which cell it shares — see `phase_bucket_count`.
pub(crate) const PHASE_BUCKET_MULTIPLE: usize = 4;
/// Ceiling for `phase_bucket_count`, and what a long window actually gets.
pub(crate) const MAX_PHASE_BUCKET_COUNT: usize = PHASE_BUCKET_COUNT * PHASE_BUCKET_MULTIPLE;
pub(crate) const GRID_POINT_COUNT: usize = 169;
pub(crate) const MAX_SERIES: usize = 512;
pub(crate) const MAX_SAMPLES: usize = 65_536;
pub(crate) const MAX_SAMPLES_PER_CYCLE: usize = PHASE_BUCKET_COUNT;
pub(crate) const MIN_COMPLETE_BUCKETS: usize = 6;
pub(crate) const MAX_PHASE_GAP: f64 = 0.30;
pub(crate) const RETENTION_MIN_SECONDS: i64 = 56 * 86_400;
pub(crate) const RETENTION_MAX_SECONDS: i64 = 400 * 86_400;
pub(crate) const RETENTION_MIN_CYCLES: usize = 8;
pub(crate) const RETENTION_MAX_CYCLES: usize = 128;
pub(crate) const RUNOUT_THRESHOLD_PERCENT: f64 = 100.0 - 1e-9;
pub(crate) const EPSILON: f64 = 1e-9;

static HISTORY_PROCESS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    static SAVE_CALL_COUNT: Cell<u64> = Cell::new(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageMode {
    System,
    Generic,
}

impl StorageMode {
    fn uses_windows_secure_storage(self) -> bool {
        cfg!(target_os = "windows") && matches!(self, Self::System)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryError {
    StorageUnavailable,
    LockOpen,
    LockAcquire,
    LockRelease,
    Read,
    Corrupt,
    CorruptQuarantine,
    InvalidSeriesKey,
    StoreCapacity,
    Serialize,
    AtomicSave,
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::StorageUnavailable => "quota pace storage is unavailable",
            Self::LockOpen => "quota pace lock could not be opened",
            Self::LockAcquire => "quota pace lock could not be acquired",
            Self::LockRelease => "quota pace lock could not be released",
            Self::Read => "quota pace history could not be read",
            Self::Corrupt => "quota pace history is corrupt and was quarantined",
            Self::CorruptQuarantine => "quota pace history could not be quarantined",
            Self::InvalidSeriesKey => "quota pace series key is invalid",
            Self::StoreCapacity => "quota pace history store capacity is exhausted",
            Self::Serialize => "quota pace history could not be serialized",
            Self::AtomicSave => "quota pace history could not be saved atomically",
        })
    }
}

impl std::error::Error for HistoryError {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SeriesKey {
    pub(crate) provider_id: String,
    pub(crate) account_scope: String,
    pub(crate) window_key: String,
}

impl SeriesKey {
    /// The scope argument is a `HistoryScope`, never an `AccountScope`: a key
    /// that outlives a credential rotation cannot be derived from the credential.
    /// The persisted field name stays `accountScope` because renaming it would
    /// orphan every record already on disk, and no migration renames fields —
    /// the version-4 upgrade adds `QuotaSample.plan` and touches nothing else.
    pub(crate) fn new(
        provider_id: impl Into<String>,
        history_scope: &HistoryScope,
        window_key: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            account_scope: history_scope.as_str().to_string(),
            window_key: window_key.into(),
        }
    }

    /// Rebuild a key from strings already persisted in the store. This is not
    /// choosing a scope, so it deliberately does not go through `HistoryScope`;
    /// giving `HistoryScope` a string constructor would reopen the hole `new`
    /// closes.
    fn from_stored_parts(provider_id: &str, account_scope: &str, window_key: &str) -> Self {
        Self {
            provider_id: provider_id.to_string(),
            account_scope: account_scope.to_string(),
            window_key: window_key.to_string(),
        }
    }

    fn is_valid(&self) -> bool {
        !self.provider_id.trim().is_empty()
            && !self.account_scope.trim().is_empty()
            && !self.window_key.trim().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SampleOrigin {
    LiveV3,
    ImportedV2,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct QuotaSample {
    pub(crate) reset_at: i64,
    pub(crate) duration_seconds: i64,
    pub(crate) duration_source: DurationSource,
    pub(crate) used_percent: f64,
    pub(crate) sampled_at: i64,
    pub(crate) origin: SampleOrigin,
    /// The subscription plan this sample was taken under. Always `None` for
    /// now: the field exists so the schema bump and its migration land once,
    /// ahead of the contract that decides where a plan value may be read from.
    /// `None` means "not recorded", never "no plan" — nothing may infer a
    /// value for a sample taken before this field existed, because a plan
    /// change in the user's past would be recorded as a fact that never
    /// happened, and no downstream check could tell.
    ///
    /// `default` covers v3 samples, which carry no key. Deliberately not
    /// `skip_serializing_if`: writing `"plan": null` is what makes a v4 file
    /// unreadable by a v3 build's `deny_unknown_fields`, which is a property
    /// the downgrade test pins rather than papers over.
    ///
    /// **This field must never enter `sample_key`.** Anything that changes
    /// that key's range is a data migration even when it looks like a
    /// constant: two samples the old range separated can collide under the
    /// new one, a duplicate key makes `validate_series` call the series
    /// corrupt, and the remedy is quarantining the whole file. A per-sample
    /// flaw then destroys every series. That is not hypothetical — it
    /// happened on this store when the phase-bucket count moved from a fixed
    /// 48 to a per-duration value, and 168 is not a multiple of 48 (see
    /// `PHASE_BUCKET_MULTIPLE`). Pooling by plan belongs in the consumer, not
    /// in the identity of a stored sample.
    #[serde(default)]
    pub(crate) plan: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SeriesState {
    pub(crate) provider_id: String,
    pub(crate) account_scope: String,
    pub(crate) window_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) active_reset_at: Option<i64>,
    pub(crate) last_activity_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rollover: Option<ObservedState>,
    pub(crate) samples: Vec<QuotaSample>,
}

impl SeriesState {
    fn new(key: &SeriesKey, now: i64) -> Self {
        Self {
            provider_id: key.provider_id.clone(),
            account_scope: key.account_scope.clone(),
            window_key: key.window_key.clone(),
            active_reset_at: None,
            last_activity_at: now,
            rollover: None,
            samples: Vec::new(),
        }
    }

    fn key(&self) -> SeriesKey {
        SeriesKey::from_stored_parts(&self.provider_id, &self.account_scope, &self.window_key)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Store {
    schema_version: u32,
    series: Vec<SeriesState>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryOutcome {
    Ready {
        duration_seconds: i64,
        source: DurationSource,
        sampled: bool,
    },
    LearningDuration,
    Unavailable(DurationUnavailableReason),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HistoricalPace {
    pub(crate) expected_percent: f64,
    pub(crate) eta_seconds: Option<f64>,
    pub(crate) will_last_to_reset: bool,
    pub(crate) run_out_probability: Option<f64>,
}

/// The one product-wide out-of-sample quality threshold shared by v3 and v2.
pub(crate) const EXPECTED_FIT_RMSE_PP: f64 = 6.0;
pub(crate) const TAIL_PHASE_THRESHOLD: f64 = 0.75;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FitPoint {
    pub(crate) phase: f64,
    pub(crate) bucket: usize,
    pub(crate) used_percent: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FitCycleInput {
    pub(crate) recency_weight: f64,
    pub(crate) points: Vec<FitPoint>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompletedFitResult {
    pub(crate) historical_curve: Vec<f64>,
    pub(crate) overall_rmse: f64,
    pub(crate) tail_rmse: Option<f64>,
    pub(crate) tail_cycle_count: usize,
    pub(crate) total_weight: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PartialFitResult {
    pub(crate) beta: f64,
    pub(crate) walk_forward_rmse: f64,
}

pub(crate) fn fit_quality(rmse: f64) -> Option<f64> {
    if !rmse.is_finite() || rmse < 0.0 || rmse > EXPECTED_FIT_RMSE_PP + EPSILON {
        return None;
    }
    Some((1.0 - rmse / 12.0).clamp(0.0, 1.0))
}

pub(crate) fn completed_blend_weight(fit_quality: f64, total_weight: f64) -> Option<f64> {
    if !fit_quality.is_finite()
        || !total_weight.is_finite()
        || !(0.0..=1.0).contains(&fit_quality)
        || total_weight <= EPSILON
    {
        return None;
    }
    let evidence_share = total_weight / (total_weight + 1.0);
    let weight = fit_quality * evidence_share;
    weight.is_finite().then_some(weight.clamp(0.0, 1.0))
}

pub(crate) fn partial_blend_weight(fit_quality: f64) -> Option<f64> {
    if !fit_quality.is_finite() || !(0.0..=1.0).contains(&fit_quality) {
        return None;
    }
    Some((0.5 * fit_quality).clamp(0.0, 0.5))
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FitWorkCounters {
    pub(crate) series_key_comparisons: usize,
    pub(crate) target_sample_reads: usize,
    pub(crate) non_target_sample_reads: usize,
    pub(crate) target_profiles_built: usize,
    pub(crate) non_target_profiles_built: usize,
    pub(crate) walk_forward_fits: usize,
    pub(crate) lobo_folds: usize,
    pub(crate) loco_folds: usize,
}

#[cfg(test)]
thread_local! {
    static FIT_WORK_COUNTERS: RefCell<FitWorkCounters> = RefCell::new(FitWorkCounters::default());
}

#[cfg(test)]
pub(crate) fn reset_fit_work_counters() {
    FIT_WORK_COUNTERS.with(|counters| *counters.borrow_mut() = FitWorkCounters::default());
}

#[cfg(test)]
pub(crate) fn fit_work_counters() -> FitWorkCounters {
    FIT_WORK_COUNTERS.with(|counters| *counters.borrow())
}

#[cfg(test)]
fn add_fit_work(update: impl FnOnce(&mut FitWorkCounters)) {
    FIT_WORK_COUNTERS.with(|counters| update(&mut counters.borrow_mut()));
}

#[cfg(not(test))]
fn add_fit_work<F>(_update: F)
where
    F: FnOnce(&mut FitWorkCounters),
{
}

fn count_series_key_comparisons(count: usize) {
    add_fit_work(|c| c.series_key_comparisons += count);
}

fn count_target_sample_reads(count: usize) {
    add_fit_work(|c| c.target_sample_reads += count);
}

fn count_target_profiles_built(count: usize) {
    add_fit_work(|c| c.target_profiles_built += count);
}

fn count_walk_forward_fits(count: usize) {
    add_fit_work(|c| c.walk_forward_fits += count);
}

fn count_lobo_folds(count: usize) {
    add_fit_work(|c| c.lobo_folds += count);
}

fn count_loco_folds(count: usize) {
    add_fit_work(|c| c.loco_folds += count);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MigrationOutcome {
    pub(crate) imported_samples: usize,
    pub(crate) skipped_samples: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct QuotaObservation {
    pub(crate) key: SeriesKey,
    pub(crate) reset_at: Option<i64>,
    pub(crate) used_percent: f64,
    pub(crate) provider: Option<DurationEvidence>,
    pub(crate) contract: Option<DurationEvidence>,
}

pub(crate) type BatchObservationResult =
    Result<(HistoryOutcome, Option<HistoricalPace>, usize), HistoryError>;

#[derive(Debug, Clone)]
enum PreparedObservation {
    Early(BatchObservationResult),
    Candidate(DurationResolution),
}

#[derive(Debug, Clone, Copy)]
struct ObservationAdmission {
    index: usize,
    resolution: DurationResolution,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyV2Sample {
    account_key: String,
    resets_at: i64,
    window_minutes: i64,
    used_percent: f64,
    sampled_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyV2Store {
    schema_version: u32,
    samples: Vec<LegacyV2Sample>,
}

#[derive(Debug)]
struct LoadedStore {
    store: Store,
    quarantined: bool,
}

/// Record a provider-neutral quota observation in the production v3 store.
/// Account scope and stable window identity must already have been resolved by
/// the caller; this function never invents either value.
pub(crate) fn record_observation(
    key: SeriesKey,
    reset_at: Option<i64>,
    used_percent: f64,
    now: i64,
    provider: Option<DurationEvidence>,
    contract: Option<DurationEvidence>,
) -> Result<HistoryOutcome, HistoryError> {
    record_observation_and_evaluate(key, reset_at, used_percent, now, provider, contract)
        .map(|(outcome, _)| outcome)
}

/// Record and evaluate in one locked v3 transaction. Stage 4 provider adapters
/// use this entry point so the returned projection describes the same committed
/// store snapshot that accepted the observation.
pub(crate) fn record_observation_and_evaluate(
    key: SeriesKey,
    reset_at: Option<i64>,
    used_percent: f64,
    now: i64,
    provider: Option<DurationEvidence>,
    contract: Option<DurationEvidence>,
) -> Result<(HistoryOutcome, Option<HistoricalPace>), HistoryError> {
    let path = production_history_path().ok_or(HistoryError::StorageUnavailable)?;
    record_observation_at_path_and_evaluate_with_clock_and_mode(
        key,
        reset_at,
        used_percent,
        now,
        provider,
        contract,
        &path,
        StorageMode::System,
        unix_now,
    )
}

pub(crate) fn production_history_path() -> Option<PathBuf> {
    let preferred = dirs::data_dir()?.join("com.nyanako.tokenbar");
    #[cfg(target_os = "windows")]
    let directory =
        crate::agent_storage_windows::resolve_secure_storage_directory(&preferred).ok()?;
    #[cfg(not(target_os = "windows"))]
    let directory = preferred;
    Some(directory.join(HISTORY_FILE_NAME))
}

pub(crate) fn read_series_at_path(
    key: &SeriesKey,
    path: &Path,
    now: i64,
) -> Result<Option<SeriesState>, HistoryError> {
    read_series_at_path_with_mode(StorageMode::Generic, key, path, now)
}

pub(crate) fn read_series(key: &SeriesKey, now: i64) -> Result<Option<SeriesState>, HistoryError> {
    let path = production_history_path().ok_or(HistoryError::StorageUnavailable)?;
    read_series_at_path_with_mode(StorageMode::System, key, &path, now)
}

fn read_series_at_path_with_mode(
    mode: StorageMode,
    key: &SeriesKey,
    path: &Path,
    now: i64,
) -> Result<Option<SeriesState>, HistoryError> {
    if !key.is_valid() {
        return Err(HistoryError::InvalidSeriesKey);
    }
    let directory = path.parent().ok_or(HistoryError::StorageUnavailable)?;
    ensure_real_directory_with_mode(mode, directory)
        .map_err(|_| HistoryError::StorageUnavailable)?;

    let _process_guard = HISTORY_PROCESS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock_file = open_history_lock(mode, &directory.join(HISTORY_LOCK_FILE_NAME))?;
    let result = load_store_at_with_mode(mode, path, now, now).and_then(|loaded| {
        if loaded.quarantined {
            Err(HistoryError::Corrupt)
        } else {
            Ok(find_target_series(&loaded.store, key).cloned())
        }
    });
    let unlock = fs2::FileExt::unlock(&lock_file).map_err(|_| HistoryError::LockRelease);
    match (result, unlock) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(series), Ok(())) => Ok(series),
    }
}

/// Import only the legacy Codex records bound to the account ID used by the
/// successful request. The caller supplies the already-resolved opaque history
/// scope — the same one the live recording path keys on, so imported samples
/// cannot land in a series the live path never touches again — and this API
/// never turns a legacy raw key into a v3 scope.
pub(crate) fn migrate_codex_v2(
    request_account_id: &str,
    history_scope: &HistoryScope,
    now: i64,
) -> Result<MigrationOutcome, HistoryError> {
    let Some(preferred) = dirs::data_dir().map(|directory| directory.join("com.nyanako.tokenbar"))
    else {
        return Err(HistoryError::StorageUnavailable);
    };
    #[cfg(target_os = "windows")]
    let destination = crate::agent_storage_windows::resolve_secure_storage_directory(&preferred)
        .map_err(|_| HistoryError::StorageUnavailable)?;
    #[cfg(not(target_os = "windows"))]
    let destination = preferred.clone();
    migrate_codex_v2_at_paths_with_clock_and_mode(
        request_account_id,
        history_scope,
        now,
        &preferred.join(LEGACY_V2_FILE_NAME),
        &destination.join(HISTORY_FILE_NAME),
        StorageMode::System,
        unix_now,
    )
}

pub(crate) fn migrate_codex_v2_at_paths(
    request_account_id: &str,
    history_scope: &HistoryScope,
    now: i64,
    v2_path: &Path,
    v3_path: &Path,
) -> Result<MigrationOutcome, HistoryError> {
    migrate_codex_v2_at_paths_with_clock_and_mode(
        request_account_id,
        history_scope,
        now,
        v2_path,
        v3_path,
        StorageMode::Generic,
        unix_now,
    )
}

fn migrate_codex_v2_at_paths_with_clock_and_mode(
    request_account_id: &str,
    history_scope: &HistoryScope,
    now: i64,
    v2_path: &Path,
    v3_path: &Path,
    mode: StorageMode,
    transaction_clock: impl FnOnce() -> i64,
) -> Result<MigrationOutcome, HistoryError> {
    let accepted_account = request_account_id.trim();
    if accepted_account.is_empty() || history_scope.as_str().trim().is_empty() {
        return Ok(MigrationOutcome {
            imported_samples: 0,
            skipped_samples: 0,
        });
    }
    let bytes = match read_legacy_v2_with_mode(mode, v2_path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) | Err(_) => {
            return Ok(MigrationOutcome {
                imported_samples: 0,
                skipped_samples: 0,
            })
        }
    };
    let legacy = match serde_json::from_slice::<LegacyV2Store>(&bytes) {
        Ok(store) if store.schema_version == 2 => store,
        _ => {
            // Corrupt or unsupported v2 is deliberately left byte-for-byte at
            // its original path. It is evidence, not a migration marker.
            return Ok(MigrationOutcome {
                imported_samples: 0,
                skipped_samples: 0,
            });
        }
    };

    let mut candidates = Vec::new();
    let mut skipped = 0;
    for sample in legacy.samples {
        let account_matches = sample.account_key.trim() == accepted_account;
        let valid = sample.window_minutes == 10_080
            && sample.sampled_at <= now
            && sample.used_percent.is_finite()
            && (0.0 < sample.used_percent && sample.used_percent <= 100.0);
        let normalized_reset = normalize_legacy_reset(sample.resets_at);
        let in_bounds = normalized_reset
            .checked_sub(10_080 * 60)
            .is_some_and(|start| {
                start <= sample.sampled_at && sample.sampled_at <= normalized_reset
            });
        if !account_matches || !valid || !in_bounds {
            skipped += 1;
            continue;
        }
        candidates.push(QuotaSample {
            reset_at: normalized_reset,
            duration_seconds: 10_080 * 60,
            duration_source: DurationSource::Provider,
            used_percent: sample.used_percent,
            sampled_at: sample.sampled_at,
            origin: SampleOrigin::ImportedV2,
            plan: None,
        });
    }
    if candidates.is_empty() {
        return Ok(MigrationOutcome {
            imported_samples: 0,
            skipped_samples: skipped,
        });
    }

    let key = SeriesKey::new("codex", history_scope, "main.weekly.v1");
    if !key.is_valid() {
        return Err(HistoryError::InvalidSeriesKey);
    }
    let transaction = |store: &mut Store| {
        let merge = match store
            .series
            .binary_search_by(|series| series.key().cmp(&key))
        {
            Ok(index) => {
                let series = &mut store.series[index];
                let merge = merge_imported_samples(series, &candidates);
                if let Some(activity) = merge.accepted_last_activity_at {
                    series.last_activity_at = series.last_activity_at.max(activity);
                }
                merge
            }
            Err(index) => {
                let mut series = SeriesState::new(&key, now);
                let merge = merge_imported_samples(&mut series, &candidates);
                if merge.imported_samples == 0 {
                    return Ok(0);
                }
                series.last_activity_at = merge.accepted_last_activity_at.unwrap_or(now);
                store.series.insert(index, series);
                merge
            }
        };
        if merge.imported_samples == 0 {
            return Ok(0);
        }
        let active_keys = BTreeSet::from([key.clone()]);
        retain_store(store, now, &active_keys)?;
        Ok(merge.imported_samples)
    };
    let imported =
        with_locked_transaction_with_mode(mode, v3_path, now, transaction_clock, transaction)?;
    Ok(MigrationOutcome {
        imported_samples: imported,
        skipped_samples: skipped,
    })
}

#[derive(Debug, Clone, Copy)]
struct MergeOutcome {
    imported_samples: usize,
    accepted_last_activity_at: Option<i64>,
}

fn merge_imported_samples(series: &mut SeriesState, imported: &[QuotaSample]) -> MergeOutcome {
    let mut merged = BTreeMap::new();
    for sample in series
        .samples
        .iter()
        .cloned()
        .chain(imported.iter().cloned())
    {
        let key = sample_key(&sample);
        let selected = match merged.remove(&key) {
            Some(existing) => choose_sample(existing, sample),
            None => sample,
        };
        merged.insert(key, selected);
    }
    let before = series.samples.clone();
    series.samples = merged.into_values().collect();
    series.samples.sort_by(sample_order);
    let accepted = series
        .samples
        .iter()
        .filter(|sample| sample.origin == SampleOrigin::ImportedV2)
        .filter(|sample| !before.iter().any(|old| old == *sample))
        .collect::<Vec<_>>();
    MergeOutcome {
        imported_samples: accepted.len(),
        accepted_last_activity_at: accepted.iter().map(|sample| sample.sampled_at).max(),
    }
}

fn choose_sample(existing: QuotaSample, candidate: QuotaSample) -> QuotaSample {
    if origin_order(candidate.origin) != origin_order(existing.origin) {
        return if origin_order(candidate.origin) > origin_order(existing.origin) {
            candidate
        } else {
            existing
        };
    }
    if candidate.sampled_at != existing.sampled_at {
        return if candidate.sampled_at > existing.sampled_at {
            candidate
        } else {
            existing
        };
    }
    if candidate.used_percent.total_cmp(&existing.used_percent) != std::cmp::Ordering::Equal {
        return if candidate.used_percent > existing.used_percent {
            candidate
        } else {
            existing
        };
    }
    let candidate_bytes = serde_json::to_vec(&candidate).unwrap_or_default();
    let existing_bytes = serde_json::to_vec(&existing).unwrap_or_default();
    if candidate_bytes < existing_bytes {
        candidate
    } else {
        existing
    }
}

/// Record a complete provider snapshot in one locked transaction. The active
/// key set may include emitted cards that have no observation in this poll.
pub(crate) fn record_observations_and_evaluate(
    emitted_active_keys: &[SeriesKey],
    observations: &[QuotaObservation],
    now: i64,
) -> Result<Vec<BatchObservationResult>, HistoryError> {
    let path = production_history_path().ok_or(HistoryError::StorageUnavailable)?;
    record_observations_at_path_and_evaluate_with_clock_and_mode(
        emitted_active_keys,
        observations,
        now,
        &path,
        StorageMode::System,
        unix_now,
    )
}

/// Testable path-injected batch variant. Load, admission, mutation, retention,
/// atomic save, and all evaluations share one v3 transaction.
pub(crate) fn record_observations_at_path_and_evaluate(
    emitted_active_keys: &[SeriesKey],
    observations: &[QuotaObservation],
    now: i64,
    path: &Path,
) -> Result<Vec<BatchObservationResult>, HistoryError> {
    record_observations_at_path_and_evaluate_with_clock_and_mode(
        emitted_active_keys,
        observations,
        now,
        path,
        StorageMode::Generic,
        unix_now,
    )
}

/// Testable path-injected single-observation compatibility wrapper.
pub(crate) fn record_observation_at_path(
    key: SeriesKey,
    reset_at: Option<i64>,
    used_percent: f64,
    now: i64,
    provider: Option<DurationEvidence>,
    contract: Option<DurationEvidence>,
    path: &Path,
) -> Result<HistoryOutcome, HistoryError> {
    record_observation_at_path_and_evaluate(
        key,
        reset_at,
        used_percent,
        now,
        provider,
        contract,
        path,
    )
    .map(|(outcome, _)| outcome)
}

pub(crate) fn record_observation_at_path_and_evaluate(
    key: SeriesKey,
    reset_at: Option<i64>,
    used_percent: f64,
    now: i64,
    provider: Option<DurationEvidence>,
    contract: Option<DurationEvidence>,
    path: &Path,
) -> Result<(HistoryOutcome, Option<HistoricalPace>), HistoryError> {
    record_observation_at_path_and_evaluate_with_clock_and_mode(
        key,
        reset_at,
        used_percent,
        now,
        provider,
        contract,
        path,
        StorageMode::Generic,
        unix_now,
    )
}

#[allow(clippy::too_many_arguments)]
fn record_observation_at_path_with_clock(
    key: SeriesKey,
    reset_at: Option<i64>,
    used_percent: f64,
    now: i64,
    provider: Option<DurationEvidence>,
    contract: Option<DurationEvidence>,
    path: &Path,
    transaction_clock: impl FnOnce() -> i64,
) -> Result<HistoryOutcome, HistoryError> {
    record_observation_at_path_and_evaluate_with_clock_and_mode(
        key,
        reset_at,
        used_percent,
        now,
        provider,
        contract,
        path,
        StorageMode::Generic,
        transaction_clock,
    )
    .map(|(outcome, _)| outcome)
}

#[allow(clippy::too_many_arguments)]
fn record_observation_at_path_and_evaluate_with_clock_and_mode(
    key: SeriesKey,
    reset_at: Option<i64>,
    used_percent: f64,
    now: i64,
    provider: Option<DurationEvidence>,
    contract: Option<DurationEvidence>,
    path: &Path,
    mode: StorageMode,
    transaction_clock: impl FnOnce() -> i64,
) -> Result<(HistoryOutcome, Option<HistoricalPace>), HistoryError> {
    let observation = QuotaObservation {
        key,
        reset_at,
        used_percent,
        provider,
        contract,
    };
    if let Some(result) = preflight_single_observation(&observation, now)? {
        return result.map(|(outcome, historical, _)| (outcome, historical));
    }
    let active_keys = [observation.key.clone()];
    let observations = [observation];
    let mut results = record_observations_at_path_and_evaluate_with_clock_and_mode(
        &active_keys,
        &observations,
        now,
        path,
        mode,
        transaction_clock,
    )?;
    let (outcome, historical, _) = results.pop().ok_or(HistoryError::Serialize)??;
    Ok((outcome, historical))
}

fn preflight_single_observation(
    observation: &QuotaObservation,
    now: i64,
) -> Result<Option<BatchObservationResult>, HistoryError> {
    if !observation.key.is_valid() {
        return Err(HistoryError::InvalidSeriesKey);
    }
    Ok(match prepare_observation(observation, now) {
        PreparedObservation::Early(result) => Some(result),
        PreparedObservation::Candidate(_) => None,
    })
}

fn prepare_observation(observation: &QuotaObservation, now: i64) -> PreparedObservation {
    let resolution = agent_quota_duration::resolve_duration(
        now,
        observation.reset_at,
        observation.provider,
        observation.contract,
        None,
    );
    if let DurationResolution::Unavailable(reason) = resolution {
        return PreparedObservation::Early(Ok((HistoryOutcome::Unavailable(reason), None, 0)));
    }
    if !observation.used_percent.is_finite() || !(0.0..=100.0).contains(&observation.used_percent) {
        return PreparedObservation::Early(Ok((
            HistoryOutcome::Unavailable(DurationUnavailableReason::InvalidEvidence),
            None,
            0,
        )));
    }
    PreparedObservation::Candidate(resolution)
}

#[allow(clippy::too_many_arguments)]
fn record_observations_at_path_and_evaluate_with_clock_and_mode(
    emitted_active_keys: &[SeriesKey],
    observations: &[QuotaObservation],
    now: i64,
    path: &Path,
    mode: StorageMode,
    transaction_clock: impl FnOnce() -> i64,
) -> Result<Vec<BatchObservationResult>, HistoryError> {
    record_observations_at_path_and_evaluate_with_clock_and_mode_and_save(
        emitted_active_keys,
        observations,
        now,
        path,
        mode,
        transaction_clock,
        |path, store| save_store_atomic_with_mode(mode, path, store),
    )
}

#[allow(clippy::too_many_arguments)]
fn record_observations_at_path_and_evaluate_with_clock_and_mode_and_save(
    emitted_active_keys: &[SeriesKey],
    observations: &[QuotaObservation],
    now: i64,
    path: &Path,
    mode: StorageMode,
    transaction_clock: impl FnOnce() -> i64,
    save: impl Fn(&Path, &Store) -> io::Result<()>,
) -> Result<Vec<BatchObservationResult>, HistoryError> {
    let active_keys = validate_batch_keys(emitted_active_keys, observations)?;
    let mut results = vec![None; observations.len()];
    let mut admissions = Vec::new();
    let mut candidate_keys = BTreeSet::new();
    for (index, observation) in observations.iter().enumerate() {
        match prepare_observation(observation, now) {
            PreparedObservation::Early(result) => results[index] = Some(result),
            PreparedObservation::Candidate(resolution) => {
                admissions.push(ObservationAdmission { index, resolution });
                candidate_keys.insert(observation.key.clone());
            }
        }
    }

    let transaction = |store: &mut Store| {
        retain_store(store, now, &active_keys)?;
        let admitted = admit_observation_keys(store, &active_keys, &candidate_keys, now)?;
        for admission in &admissions {
            let observation = &observations[admission.index];
            if !admitted.contains(&observation.key) {
                results[admission.index] = Some(Err(HistoryError::StoreCapacity));
                continue;
            }
            let key_index = store
                .series
                .binary_search_by(|series| series.key().cmp(&observation.key))
                .map_err(|_| HistoryError::Serialize)?;
            let stale = is_stale_observation(
                &store.series[key_index],
                observation.reset_at.ok_or(HistoryError::Serialize)?,
                now,
            );
            let outcome = if stale {
                stale_outcome(admission.resolution)
            } else {
                let series = &mut store.series[key_index];
                let outcome = match admission.resolution {
                    DurationResolution::Ready {
                        duration_seconds,
                        source,
                    } => apply_known_duration(
                        series,
                        observation.reset_at.ok_or(HistoryError::Serialize)?,
                        duration_seconds,
                        source,
                        observation.used_percent,
                        now,
                    ),
                    DurationResolution::LearningDuration => apply_observed_duration(
                        series,
                        observation.reset_at.ok_or(HistoryError::Serialize)?,
                        observation.used_percent,
                        now,
                    )?,
                    DurationResolution::Unavailable(_) => return Err(HistoryError::Serialize),
                };
                if !matches!(outcome, HistoryOutcome::Unavailable(_)) {
                    series.last_activity_at = series.last_activity_at.max(now);
                }
                outcome
            };
            results[admission.index] = Some(Ok((outcome, None, 0)));
        }
        retain_store(store, now, &active_keys)?;
        for admission in &admissions {
            let Some(Ok((outcome, _, _))) = results[admission.index].as_ref() else {
                continue;
            };
            let observation = &observations[admission.index];
            let Some(reset_at) = observation.reset_at else {
                continue;
            };
            let (pace, complete_cycles) = duration_for_outcome(*outcome)
                .map(|duration| {
                    let calculation = calculate_target(
                        store,
                        &observation.key,
                        reset_at,
                        duration,
                        observation.used_percent,
                        now,
                    );
                    (calculation.pace, calculation.complete_cycles)
                })
                .unwrap_or((None, 0));
            results[admission.index] = Some(Ok((*outcome, pace, complete_cycles)));
        }
        Ok(())
    };
    with_locked_transaction_with_save_and_mode(
        mode,
        path,
        now,
        transaction_clock,
        save,
        transaction,
    )?;

    results
        .into_iter()
        .map(|result| result.ok_or(HistoryError::Serialize))
        .collect()
}

fn validate_batch_keys(
    emitted_active_keys: &[SeriesKey],
    observations: &[QuotaObservation],
) -> Result<BTreeSet<SeriesKey>, HistoryError> {
    let mut active_keys = BTreeSet::new();
    for key in emitted_active_keys {
        if !key.is_valid() || !active_keys.insert(key.clone()) {
            return Err(HistoryError::InvalidSeriesKey);
        }
    }
    let mut observation_keys = BTreeSet::new();
    for observation in observations {
        if !observation.key.is_valid() || !observation_keys.insert(observation.key.clone()) {
            return Err(HistoryError::InvalidSeriesKey);
        }
        active_keys.insert(observation.key.clone());
    }
    Ok(active_keys)
}

fn admit_observation_keys(
    store: &mut Store,
    active_keys: &BTreeSet<SeriesKey>,
    candidate_keys: &BTreeSet<SeriesKey>,
    now: i64,
) -> Result<BTreeSet<SeriesKey>, HistoryError> {
    if store.series.len() > MAX_SERIES {
        return Err(HistoryError::StoreCapacity);
    }
    let existing_candidates = candidate_keys
        .iter()
        .filter(|key| {
            store
                .series
                .binary_search_by(|series| series.key().cmp(key))
                .is_ok()
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let new_candidates = candidate_keys
        .difference(&existing_candidates)
        .cloned()
        .collect::<Vec<_>>();
    let available = MAX_SERIES.saturating_sub(store.series.len());
    let needed_evictions = new_candidates.len().saturating_sub(available);
    if needed_evictions > 0 {
        let mut inactive = store
            .series
            .iter()
            .filter(|series| {
                let key = series.key();
                !series_is_active(series, &key, active_keys, now)
                    && !existing_candidates.contains(&key)
            })
            .map(|series| (series.last_activity_at, series.key()))
            .collect::<Vec<_>>();
        inactive.sort();
        for (_, key) in inactive.into_iter().take(needed_evictions) {
            if let Ok(index) = store
                .series
                .binary_search_by(|series| series.key().cmp(&key))
            {
                store.series.remove(index);
            }
        }
    }
    let mut admitted = existing_candidates;
    let available = MAX_SERIES.saturating_sub(store.series.len());
    for key in new_candidates.into_iter().take(available) {
        let index = store
            .series
            .binary_search_by(|series| series.key().cmp(&key))
            .unwrap_or_else(|index| index);
        store.series.insert(index, SeriesState::new(&key, now));
        admitted.insert(key);
    }
    Ok(admitted)
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn stale_outcome(resolution: DurationResolution) -> HistoryOutcome {
    match resolution {
        DurationResolution::Ready {
            duration_seconds,
            source,
        } => HistoryOutcome::Ready {
            duration_seconds,
            source,
            sampled: false,
        },
        DurationResolution::LearningDuration => HistoryOutcome::LearningDuration,
        DurationResolution::Unavailable(reason) => HistoryOutcome::Unavailable(reason),
    }
}

fn rollover_reset_at(state: &ObservedState) -> i64 {
    match state {
        ObservedState::Watching { reset_at, .. } | ObservedState::Ready { reset_at, .. } => {
            *reset_at
        }
        ObservedState::Candidate { new_reset_at, .. } => *new_reset_at,
    }
}

fn tracked_reset_at(series: &SeriesState) -> Option<i64> {
    series
        .active_reset_at
        .or_else(|| series.rollover.as_ref().map(rollover_reset_at))
        .or_else(|| series.samples.iter().map(|sample| sample.reset_at).max())
}

fn is_stale_observation(series: &SeriesState, reset_at: i64, now: i64) -> bool {
    now < series.last_activity_at
        || (now == series.last_activity_at
            && tracked_reset_at(series).is_some_and(|tracked| reset_at < tracked))
}

fn is_backward_reset(series: &SeriesState, reset_at: i64, duration_seconds: i64) -> bool {
    tracked_reset_at(series).is_some_and(|tracked| {
        normalize_reset(reset_at, duration_seconds) < normalize_reset(tracked, duration_seconds)
    })
}

fn requires_observed_relearning(series: &SeriesState) -> bool {
    if series.active_reset_at.is_some() || series.samples.is_empty() {
        return false;
    }
    let Some(rollover) = series.rollover.as_ref() else {
        return false;
    };
    let newest_sample = series.samples.iter().map(|sample| sample.reset_at).max();
    match rollover {
        ObservedState::Watching { reset_at, .. } => {
            newest_sample.is_some_and(|sample_reset| sample_reset > *reset_at)
        }
        ObservedState::Candidate { old_reset_at, .. } => {
            newest_sample.is_some_and(|sample_reset| sample_reset > *old_reset_at)
        }
        ObservedState::Ready { .. } => false,
    }
}

fn apply_known_duration(
    series: &mut SeriesState,
    reset_at: i64,
    duration_seconds: i64,
    source: DurationSource,
    used_percent: f64,
    now: i64,
) -> HistoryOutcome {
    let backward = is_backward_reset(series, reset_at, duration_seconds);
    let relearning = requires_observed_relearning(series);
    let transition = match observe_reset(series.rollover.as_ref(), reset_at, now) {
        Ok(transition) => transition,
        Err(reason) => return HistoryOutcome::Unavailable(reason),
    };
    series.rollover = Some(transition.state);
    if backward || relearning {
        series.active_reset_at = None;
        return HistoryOutcome::LearningDuration;
    }
    series.active_reset_at = Some(reset_at);
    clean_active_partial_duration(series, reset_at, duration_seconds, now);
    let sampled = add_sample_if_new(
        series,
        reset_at,
        duration_seconds,
        source,
        used_percent,
        now,
    );
    HistoryOutcome::Ready {
        duration_seconds,
        source,
        sampled,
    }
}

fn apply_observed_duration(
    series: &mut SeriesState,
    reset_at: i64,
    used_percent: f64,
    now: i64,
) -> Result<HistoryOutcome, HistoryError> {
    let transition = match observe_reset(series.rollover.as_ref(), reset_at, now) {
        Ok(transition) => transition,
        Err(reason) => return Ok(HistoryOutcome::Unavailable(reason)),
    };
    let duration_seconds = transition.duration_seconds;
    series.rollover = Some(transition.state);
    if let Some(duration_seconds) = duration_seconds {
        series.active_reset_at = Some(reset_at);
        clean_active_partial_duration(series, reset_at, duration_seconds, now);
        // Once an observed rollover is confirmed, subsequent polls use the
        // same phase-bucket admission rules as provider/contract samples. The
        // duplicate flag only describes the rollover state transition; it must
        // not suppress useful later samples in the ready cycle.
        let sampled = add_sample_if_new(
            series,
            reset_at,
            duration_seconds,
            DurationSource::Observed,
            used_percent,
            now,
        );
        Ok(HistoryOutcome::Ready {
            duration_seconds,
            source: DurationSource::Observed,
            sampled,
        })
    } else {
        if series.active_reset_at != Some(reset_at) {
            series.active_reset_at = None;
        }
        Ok(HistoryOutcome::LearningDuration)
    }
}

fn clean_active_partial_duration(
    series: &mut SeriesState,
    active_reset_at: i64,
    accepted_duration: i64,
    now: i64,
) {
    let active_samples = series
        .samples
        .iter()
        .filter(|sample| is_active_group_sample(active_reset_at, sample))
        .cloned()
        .collect::<Vec<_>>();
    if active_samples.is_empty() {
        return;
    }

    let group_reset = active_samples
        .first()
        .map(|sample| normalize_reset(sample.reset_at, sample.duration_seconds));
    let is_complete = active_reset_at <= now
        && group_reset
            .filter(|reset| {
                active_samples.iter().all(|sample| {
                    normalize_reset(sample.reset_at, sample.duration_seconds) == *reset
                })
            })
            .and_then(|reset| retention_cycle_descriptor(reset, &active_samples, now))
            .is_some();
    if is_complete {
        return;
    }

    series.samples.retain(|sample| {
        !is_active_group_sample(active_reset_at, sample)
            || sample.duration_seconds == accepted_duration
    });
    series.samples.sort_by(sample_order);
}

fn add_sample_if_new(
    series: &mut SeriesState,
    reset_at: i64,
    duration_seconds: i64,
    source: DurationSource,
    used_percent: f64,
    sampled_at: i64,
) -> bool {
    // `0.0 <=`, not `0.0 <`. A fresh window reads 0% used, and rejecting it
    // meant no cycle ever recorded its own start: the first stored sample was
    // always after some consumption, so `usedPercent` — the span between the
    // lowest and highest reading — understated every cycle by whatever had been
    // spent before the app first saw a non-zero number.
    //
    // Zero is not a "no data" sentinel here. By the time `enrich_snapshot_with`
    // builds an observation it has already dropped `PaceState::Unavailable`,
    // an unparseable reset and any non-finite reading, and its own range check
    // is `(0.0..=100.0)` — inclusive. The store was the only layer treating a
    // measured zero as an absence.
    if !(valid_duration(duration_seconds)
        && used_percent.is_finite()
        && 0.0 <= used_percent
        && used_percent <= 100.0)
    {
        return false;
    }
    let normalized_reset = normalize_sample_reset(reset_at, duration_seconds, sampled_at);
    let candidate = QuotaSample {
        reset_at: normalized_reset,
        duration_seconds,
        duration_source: source,
        used_percent,
        sampled_at,
        origin: SampleOrigin::LiveV3,
        plan: None,
    };
    if !validate_sample(&candidate) {
        return false;
    }
    let candidate_key = sample_key(&candidate);
    if let Some(index) = series
        .samples
        .iter()
        .position(|sample| sample_key(sample) == candidate_key)
    {
        let existing = &series.samples[index];
        // A cycle's own zero is never replaced. Admitting it (see the range
        // check above) bought nothing while this path could overwrite it: a
        // window that moves a point before its first bucket closes puts the
        // later reading in the same bucket, and the replacement guards below
        // all pass — later timestamp, larger value — so the stored cycle began
        // at a nonzero minimum again and `usedPercent`, the span between the
        // lowest and highest reading, understated it by exactly the amount the
        // zero was recorded to capture.
        //
        // Only the zero. Every other bucket keeps "newest wins", because
        // elsewhere the latest level is what the profile wants; at the start of
        // a cycle the BASELINE is, and there is only one sample that can carry
        // it.
        if existing.used_percent == 0.0 && used_percent > 0.0 {
            return false;
        }
        if (used_percent - existing.used_percent).abs() < 1.0
            || sampled_at < existing.sampled_at
            || (sampled_at == existing.sampled_at && used_percent <= existing.used_percent)
        {
            return false;
        }
        series.samples[index] = candidate;
        return true;
    }

    let cycle_count = series
        .samples
        .iter()
        .filter(|sample| {
            normalize_reset(sample.reset_at, sample.duration_seconds) == normalized_reset
        })
        .count();
    if cycle_count >= phase_bucket_count(duration_seconds) {
        return false;
    }
    series.samples.push(candidate);
    true
}

fn sample_key(sample: &QuotaSample) -> (i64, usize) {
    let normalized_reset = normalize_reset(sample.reset_at, sample.duration_seconds);
    (
        normalized_reset,
        phase_bucket(normalized_reset, sample.duration_seconds, sample.sampled_at),
    )
}

fn sample_order(left: &QuotaSample, right: &QuotaSample) -> std::cmp::Ordering {
    left.reset_at
        .cmp(&right.reset_at)
        .then(left.duration_seconds.cmp(&right.duration_seconds))
        .then(left.sampled_at.cmp(&right.sampled_at))
        .then(left.used_percent.total_cmp(&right.used_percent))
        .then_with(|| origin_order(left.origin).cmp(&origin_order(right.origin)))
        .then_with(|| source_order(left.duration_source).cmp(&source_order(right.duration_source)))
}

fn origin_order(origin: SampleOrigin) -> u8 {
    match origin {
        SampleOrigin::ImportedV2 => 0,
        SampleOrigin::LiveV3 => 1,
    }
}

fn source_order(source: DurationSource) -> u8 {
    match source {
        DurationSource::Provider => 0,
        DurationSource::Contract => 1,
        DurationSource::Observed => 2,
    }
}

fn series_order(left: &SeriesState, right: &SeriesState) -> std::cmp::Ordering {
    left.key().cmp(&right.key())
}

fn normalize_reset(reset_at: i64, duration_seconds: i64) -> i64 {
    let quantum = duration_seconds
        .checked_div(100)
        .unwrap_or(0)
        .clamp(60, 300);
    let quantum = quantum.max(1);
    let quotient = reset_at.div_euclid(quantum);
    let remainder = reset_at.rem_euclid(quantum);
    let rounded = if remainder.saturating_mul(2) >= quantum {
        quotient.saturating_add(1)
    } else {
        quotient
    };
    rounded.saturating_mul(quantum)
}

fn normalize_sample_reset(reset_at: i64, duration_seconds: i64, sampled_at: i64) -> i64 {
    normalize_reset(reset_at, duration_seconds).clamp(
        sampled_at,
        sampled_at.saturating_add(duration_seconds.max(1)),
    )
}

fn normalize_legacy_reset(reset_at: i64) -> i64 {
    let quantum = 300_i64;
    let quotient = reset_at.div_euclid(quantum);
    let remainder = reset_at.rem_euclid(quantum);
    let rounded = if remainder >= quantum / 2 {
        quotient.saturating_add(1)
    } else {
        quotient
    };
    rounded.saturating_mul(quantum)
}

fn phase(sample: &QuotaSample) -> f64 {
    let normalized_reset = normalize_reset(sample.reset_at, sample.duration_seconds);
    let Some(remaining) = normalized_reset.checked_sub(sample.sampled_at) else {
        return 0.0;
    };
    (1.0 - remaining as f64 / sample.duration_seconds as f64).clamp(0.0, 1.0)
}

/// How many phase buckets a cycle of this length is divided into.
///
/// Derived from the duration rather than fixed, because a fixed count divides
/// every window into the same NUMBER of buckets and therefore gives long
/// windows coarse ones: at 48, a 5-hour session window buckets every six
/// minutes and a 7-day weekly window every 3.5 hours. A freshly reset weekly
/// window then holds one sample for its first 3.5 hours no matter how often
/// the app polls, because a second sample in the same bucket replaces the
/// first rather than joining it.
///
/// A WHOLE MULTIPLE of the floor, never an arbitrary hourly count. This is the
/// part that cannot be relaxed, and it cost a real store to learn: bucket
/// boundaries sit at `k / count` of the cycle, so a count that is not a
/// multiple of the previous one produces a grid that does not NEST inside it,
/// and two samples separated by the old grid can land in one new cell.
///
/// The first attempt was `duration / 1 hour`, which gives 168 for a weekly
/// window — and 168/48 is 3.5. On this author's own store that merged three
/// pairs of samples that 48 buckets had kept apart. `validate_series` treats a
/// duplicate sample key as corruption, and the response to a corrupt store is
/// quarantine, so a resolution change silently became a history wipe. The data
/// survived in the `.corrupt-*` file, which is the only reason this is a story
/// rather than a loss.
///
/// So: 48, or 4x that. A 7-day window at 192 buckets is 52.5 minutes each —
/// past the "one per hour" this was asked for — and a monthly one lands at
/// 3.75 hours. Short windows keep 48 and are byte-identical: five hours at 48
/// is already finer than the 60-second poll can fill.
pub(crate) fn phase_bucket_count(duration_seconds: i64) -> usize {
    // Above two days. A 48-hour window already buckets at one hour with the
    // floor, so nothing shorter has anything to gain.
    if duration_seconds > 2 * 86_400 {
        PHASE_BUCKET_COUNT * PHASE_BUCKET_MULTIPLE
    } else {
        PHASE_BUCKET_COUNT
    }
}

fn phase_bucket(reset_at: i64, duration_seconds: i64, sampled_at: i64) -> usize {
    let count = phase_bucket_count(duration_seconds);
    let remaining = reset_at.saturating_sub(sampled_at);
    let u = (1.0 - remaining as f64 / duration_seconds.max(1) as f64).clamp(0.0, 1.0);
    ((u * count as f64).floor() as usize).min(count - 1)
}

fn validate_sample(sample: &QuotaSample) -> bool {
    let Some(cycle_started_at) = sample.reset_at.checked_sub(sample.duration_seconds) else {
        return false;
    };
    valid_duration(sample.duration_seconds)
        && cycle_started_at <= sample.sampled_at
        && sample.sampled_at <= sample.reset_at
        && sample.used_percent.is_finite()
        // Inclusive of zero — see the admission rule in `admit`. `QuotaCurve`'s
        // Swift decoder mirrors this function exactly and must widen with it,
        // or a store containing a legitimate 0 would decode as corrupt.
        && (0.0 <= sample.used_percent && sample.used_percent <= 100.0)
}

fn validate_series(series: &SeriesState) -> bool {
    let key = series.key();
    let mut sample_keys = BTreeSet::new();
    let mut cycle_counts: BTreeMap<i64, usize> = BTreeMap::new();
    let samples_valid = series.samples.iter().all(|sample| {
        if !validate_sample(sample) || !sample_keys.insert(sample_key(sample)) {
            return false;
        }
        let reset = normalize_reset(sample.reset_at, sample.duration_seconds);
        let count = cycle_counts.entry(reset).or_default();
        *count += 1;
        // Same duration-derived cap `admit` enforces. A fixed one here would
        // call a store the writer legitimately produced corrupt — a weekly
        // cycle now holds up to 168 samples — and quarantine would then delete
        // the history this cap exists to bound.
        *count <= phase_bucket_count(sample.duration_seconds)
    });
    let rollover_valid = series.rollover.as_ref().is_none_or(|rollover| {
        if !agent_quota_duration::validate_observed_state(rollover) {
            return false;
        }
        match rollover {
            ObservedState::Watching { reset_at, .. }
            | ObservedState::Candidate {
                new_reset_at: reset_at,
                ..
            } => series
                .active_reset_at
                .is_none_or(|active_reset| active_reset == *reset_at),
            ObservedState::Ready { reset_at, .. } => series.active_reset_at == Some(*reset_at),
        }
    });
    let activity_valid = series
        .samples
        .iter()
        .all(|sample| series.last_activity_at >= sample.sampled_at)
        && series
            .rollover
            .as_ref()
            .is_none_or(|rollover| match rollover {
                ObservedState::Watching { last_seen_at, .. } => {
                    series.last_activity_at >= *last_seen_at
                }
                ObservedState::Candidate {
                    first_new_seen_at, ..
                } => series.last_activity_at >= *first_new_seen_at,
                ObservedState::Ready { last_seen_at, .. } => {
                    series.last_activity_at >= *last_seen_at
                }
            });
    key.is_valid() && rollover_valid && samples_valid && activity_valid
}

fn validate_store(store: &Store) -> bool {
    store.schema_version == HISTORY_SCHEMA_VERSION
        && store
            .series
            .windows(2)
            .all(|pair| series_order(&pair[0], &pair[1]).is_lt())
        && store.series.iter().all(validate_series)
}

fn validate_store_at(store: &Store, now: i64) -> bool {
    validate_store(store)
        && store
            .series
            .iter()
            .all(|series| series.last_activity_at <= now)
}

/// The activity timestamp `validate_series`'s `activity_valid` holds a
/// rollover to, per variant. `Candidate` has no `last_seen_at`; its activity
/// timestamp is `first_new_seen_at`. Cycle boundaries (`reset_at`,
/// `new_reset_at`, `old_reset_at`, `cycle_started_at`) are deliberately not
/// consulted here — see the module-level design note on `repair_store_at`.
fn rollover_activity_at(rollover: &ObservedState) -> i64 {
    match rollover {
        ObservedState::Watching { last_seen_at, .. }
        | ObservedState::Ready { last_seen_at, .. } => *last_seen_at,
        ObservedState::Candidate {
            first_new_seen_at, ..
        } => *first_new_seen_at,
    }
}

/// Repair a structurally valid store (`validate_store` already passed) whose
/// clock disagrees with the reading transaction: some series' derived
/// timestamps lead `upper_bound`. Structural failures never reach this
/// function; they still quarantine at the call site.
///
/// Every detection threshold below is `upper_bound` — the same ceiling
/// `validate_store_at` failed against, which is what "leads the ceiling"
/// means. Only the clamp target is `observation_now`: `is_stale_observation`
/// compares against `observation_now`, so clamping to `upper_bound` would
/// leave a repaired series' clock above the transaction body's clock, reject
/// this poll's observation as stale, and repeat identically on every future
/// load. Rollover detection only ever looks at activity timestamps
/// (`rollover_activity_at`), never at cycle boundaries: `reset_at` and
/// friends are future boundaries that lead `upper_bound` in essentially
/// every healthy rollover, and including them would drop every in-flight
/// rollover on every load.
///
/// **Precondition: `observation_now <= upper_bound`.** Every current caller
/// satisfies it — the transaction passes `observation_now.max(lock_time)` as
/// the ceiling, and the two read paths pass the same value for both. A caller
/// that broke it could produce a clamped `last_activity_at` above the ceiling,
/// which the post-body `validate_store_at` would reject, failing the whole
/// transaction for every provider rather than just one series.
/// Forget samples this build cannot place, keeping everything it can.
///
/// The three sample-level invariants `validate_series` enforces — each sample
/// individually valid, no two sharing a `sample_key`, and no cycle over its
/// bucket cap — are all statements about ONE sample being unusable. Failing the
/// store on them means one unusable sample discards every usable one beside it,
/// and the pace history's whole value is that it accumulates over weeks.
///
/// The newest sample wins a key collision, matching `admit`, which replaces
/// within a bucket rather than appending. Order is preserved otherwise, so a
/// store that needed no repair comes out byte-identical.
fn drop_unplaceable_samples(mut store: Store) -> Store {
    for series in &mut store.series {
        let mut seen: BTreeMap<(i64, usize), usize> = BTreeMap::new();
        let mut per_cycle: BTreeMap<i64, usize> = BTreeMap::new();
        let mut kept: Vec<QuotaSample> = Vec::with_capacity(series.samples.len());
        for sample in series.samples.drain(..) {
            if !validate_sample(&sample) {
                continue;
            }
            let key = sample_key(&sample);
            if let Some(index) = seen.get(&key).copied() {
                // Same bucket: keep the newer reading, as `admit` would.
                if sample.sampled_at > kept[index].sampled_at {
                    kept[index] = sample;
                }
                continue;
            }
            let reset = normalize_reset(sample.reset_at, sample.duration_seconds);
            let count = per_cycle.entry(reset).or_default();
            if *count >= phase_bucket_count(sample.duration_seconds) {
                continue;
            }
            *count += 1;
            seen.insert(key, kept.len());
            kept.push(sample);
        }
        series.samples = kept;
    }
    store
}

fn repair_store_at(mut store: Store, upper_bound: i64, observation_now: i64) -> Store {
    debug_assert!(
        observation_now <= upper_bound,
        "repair_store_at requires observation_now <= upper_bound"
    );
    // A series whose own sample evidence leads the ceiling cannot be
    // verified against any clock the reader trusts; drop it wholesale so a
    // sibling's history is not held hostage by it.
    store.series.retain(|series| {
        !series
            .samples
            .iter()
            .any(|sample| sample.sampled_at > upper_bound)
    });

    for series in &mut store.series {
        if series.last_activity_at <= upper_bound {
            continue; // gated: a series at or below the ceiling is untouched
        }
        if series
            .rollover
            .as_ref()
            .is_some_and(|rollover| rollover_activity_at(rollover) > upper_bound)
        {
            series.rollover = None;
        }
        let floor = series
            .samples
            .iter()
            .map(|sample| sample.sampled_at)
            .max()
            .into_iter()
            .chain(series.rollover.as_ref().map(rollover_activity_at))
            .max();
        let clamped = series.last_activity_at.min(observation_now);
        series.last_activity_at = floor.map_or(clamped, |floor| floor.max(clamped));
    }
    store
}

fn duration_for_resolution(resolution: DurationResolution) -> Option<i64> {
    match resolution {
        DurationResolution::Ready {
            duration_seconds, ..
        } => Some(duration_seconds),
        DurationResolution::LearningDuration | DurationResolution::Unavailable(_) => None,
    }
}

fn duration_for_outcome(outcome: HistoryOutcome) -> Option<i64> {
    match outcome {
        HistoryOutcome::Ready {
            duration_seconds, ..
        } => Some(duration_seconds),
        HistoryOutcome::LearningDuration | HistoryOutcome::Unavailable(_) => None,
    }
}

fn find_target_series<'a>(store: &'a Store, key: &SeriesKey) -> Option<&'a SeriesState> {
    for series in &store.series {
        count_series_key_comparisons(1);
        if series.key() == *key {
            return Some(series);
        }
    }
    None
}

#[derive(Debug, Clone)]
struct RetentionCycleDescriptor {
    reset_at: i64,
    duration_seconds: i64,
    cycle_started_at: i64,
    samples: Vec<QuotaSample>,
}

#[derive(Debug, Clone)]
struct CycleProfile {
    reset_at: i64,
    duration_seconds: i64,
    cycle_started_at: i64,
    points: Vec<FitPoint>,
    curve: Vec<f64>,
}

fn median_i64(values: impl IntoIterator<Item = i64>) -> Option<i64> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[middle])
    } else {
        Some(values[middle - 1].saturating_add(values[middle]) / 2)
    }
}

fn cycle_duration(samples: &[QuotaSample]) -> Option<i64> {
    median_i64(samples.iter().map(|sample| sample.duration_seconds))
        .filter(|duration| valid_duration(*duration))
}

fn grouped_samples(samples: &[QuotaSample]) -> BTreeMap<i64, Vec<QuotaSample>> {
    let mut groups = BTreeMap::new();
    for sample in samples.iter().filter(|sample| validate_sample(sample)) {
        groups
            .entry(normalize_reset(sample.reset_at, sample.duration_seconds))
            .or_insert_with(Vec::new)
            .push(sample.clone());
    }
    groups
}

fn retention_cycle_descriptor(
    reset_at: i64,
    samples: &[QuotaSample],
    now: i64,
) -> Option<RetentionCycleDescriptor> {
    if reset_at > now || samples.len() < MIN_COMPLETE_BUCKETS {
        return None;
    }
    let duration_seconds = cycle_duration(samples)?;
    let mut buckets = BTreeSet::new();
    let mut phases = Vec::with_capacity(samples.len());
    for sample in samples {
        if !validate_sample(sample)
            || normalize_reset(sample.reset_at, sample.duration_seconds) != reset_at
        {
            return None;
        }
        buckets.insert(sample_key(sample).1);
        phases.push(phase(sample));
    }
    if buckets.len() < MIN_COMPLETE_BUCKETS {
        return None;
    }
    phases.sort_by(f64::total_cmp);
    let boundary = (0.10_f64).min(86_400.0 / duration_seconds as f64);
    let has_start = phases
        .first()
        .is_some_and(|phase| *phase <= boundary + EPSILON);
    let has_end = phases
        .last()
        .is_some_and(|phase| *phase + EPSILON >= 1.0 - boundary);
    if !has_start || !has_end {
        return None;
    }
    let max_gap = phases
        .iter()
        .copied()
        .fold((0.0_f64, 0.0_f64), |(largest, previous), current| {
            (largest.max(current - previous), current)
        })
        .0
        .max(1.0 - phases.last().copied().unwrap_or(0.0));
    if max_gap > MAX_PHASE_GAP + EPSILON {
        return None;
    }
    let cycle_started_at = reset_at.checked_sub(duration_seconds)?;
    Some(RetentionCycleDescriptor {
        reset_at,
        duration_seconds,
        cycle_started_at,
        samples: samples.to_vec(),
    })
}

fn fit_points_from_samples(samples: &[QuotaSample]) -> Vec<FitPoint> {
    samples
        .iter()
        .map(|sample| FitPoint {
            phase: phase(sample),
            bucket: sample_key(sample).1,
            used_percent: sample.used_percent,
        })
        .collect()
}

fn cycle_profile_from_descriptor(descriptor: &RetentionCycleDescriptor) -> Option<CycleProfile> {
    if descriptor
        .samples
        .iter()
        .any(|sample| sample.duration_seconds != descriptor.duration_seconds)
    {
        return None;
    }
    let points = fit_points_from_samples(&descriptor.samples);
    Some(CycleProfile {
        reset_at: descriptor.reset_at,
        duration_seconds: descriptor.duration_seconds,
        cycle_started_at: descriptor.cycle_started_at,
        curve: reconstruct_fit_curve(&points),
        points,
    })
}

fn cycle_profile(reset_at: i64, samples: &[QuotaSample], now: i64) -> Option<CycleProfile> {
    retention_cycle_descriptor(reset_at, samples, now)
        .and_then(|descriptor| cycle_profile_from_descriptor(&descriptor))
}

fn retention_cycles(series: &SeriesState, now: i64) -> Vec<RetentionCycleDescriptor> {
    let active_future = series.active_reset_at.filter(|reset| *reset > now);
    grouped_samples(&series.samples)
        .into_iter()
        .filter(|(_, samples)| {
            !active_future.is_some_and(|active| {
                samples
                    .iter()
                    .any(|sample| is_active_group_sample(active, sample))
            })
        })
        .filter_map(|(reset_at, samples)| retention_cycle_descriptor(reset_at, &samples, now))
        .collect()
}

/// Whether a stored sample belongs to the cycle the window is still inside.
///
/// Both sides are normalized, and that is the whole point. `series.active_reset_at`
/// holds the RAW provider value while every stored sample holds
/// `normalize_sample_reset(...)`, so an exact comparison between them fails
/// whenever the provider's reset is not already on the quantum — which is the
/// ordinary case, not the exotic one (codex was off by 62s, grok by 19s). This
/// predicate is the producer's own answer and is published per point so no
/// consumer has to re-derive a quantization rule across the FFI boundary.
pub(crate) fn is_active_group_sample(active_reset_at: i64, sample: &QuotaSample) -> bool {
    validate_sample(sample)
        && normalize_reset(sample.reset_at, sample.duration_seconds)
            == normalize_reset(active_reset_at, sample.duration_seconds)
}
fn reconstruct_cycle_curve(samples: &[QuotaSample]) -> Vec<f64> {
    reconstruct_fit_curve(&fit_points_from_samples(samples))
}

fn reconstruct_fit_curve(points: &[FitPoint]) -> Vec<f64> {
    let mut points = points
        .iter()
        .filter(|point| {
            point.phase.is_finite()
                && (0.0..=1.0).contains(&point.phase)
                && point.used_percent.is_finite()
        })
        .map(|point| (point.phase, point.used_percent.clamp(0.0, 100.0)))
        .collect::<Vec<_>>();
    points.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.total_cmp(&right.1)));

    let mut monotone = Vec::with_capacity(points.len() + 2);
    let mut running_max = 0.0_f64;
    for (phase, value) in points {
        running_max = running_max.max(value);
        monotone.push((phase, running_max));
    }
    let end = monotone.last().map(|(_, value)| *value).unwrap_or(0.0);
    monotone.push((0.0, 0.0));
    monotone.push((1.0, end));
    monotone.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.total_cmp(&right.1)));
    running_max = 0.0;
    for (_, value) in &mut monotone {
        running_max = running_max.max(*value);
        *value = running_max.clamp(0.0, 100.0);
    }

    let mut curve = vec![0.0; GRID_POINT_COUNT];
    let mut upper = 1usize;
    for (index, value) in curve.iter_mut().enumerate() {
        let phase = index as f64 / (GRID_POINT_COUNT - 1) as f64;
        while upper < monotone.len() && monotone[upper].0 < phase {
            upper += 1;
        }
        if phase <= monotone[0].0 {
            *value = monotone[0].1;
        } else if phase >= monotone[monotone.len() - 1].0 {
            *value = monotone[monotone.len() - 1].1;
        } else {
            let high = monotone[upper.min(monotone.len() - 1)];
            let low = monotone[upper.saturating_sub(1)];
            let ratio = if high.0 <= low.0 {
                0.0
            } else {
                ((phase - low.0) / (high.0 - low.0)).clamp(0.0, 1.0)
            };
            *value = low.1 + (high.1 - low.1) * ratio;
        }
    }
    let mut maximum = 0.0_f64;
    for value in &mut curve {
        maximum = maximum.max(*value);
        *value = maximum.clamp(0.0, 100.0);
    }
    curve
}

fn historical_cycles(series: &SeriesState, current_reset_at: i64, now: i64) -> Vec<CycleProfile> {
    retention_cycles(series, now)
        .into_iter()
        .filter(|descriptor| descriptor.reset_at < current_reset_at)
        .filter_map(|descriptor| cycle_profile_from_descriptor(&descriptor))
        .collect()
}

fn series_nominal_duration(series: &SeriesState, now: i64) -> i64 {
    let completed = retention_cycles(series, now);
    median_i64(completed.iter().map(|cycle| cycle.duration_seconds))
        .or_else(|| {
            series
                .rollover
                .as_ref()
                .and_then(agent_quota_duration::observed_duration)
        })
        .or_else(|| median_i64(series.samples.iter().map(|sample| sample.duration_seconds)))
        .unwrap_or(1)
        .clamp(1, agent_quota_duration::MAX_DURATION_SECONDS)
}

fn retention_limits(nominal_duration: i64) -> (usize, i64) {
    let nominal = nominal_duration.max(1) as f64;
    let cycles = (28.0 * 86_400.0 / nominal).ceil() as usize;
    let retained = cycles.clamp(RETENTION_MIN_CYCLES, RETENTION_MAX_CYCLES);
    let horizon = (retained as i64)
        .saturating_mul(nominal_duration.max(1))
        .max(RETENTION_MIN_SECONDS)
        .clamp(RETENTION_MIN_SECONDS, RETENTION_MAX_SECONDS);
    (retained, horizon)
}

fn clear_stale_rollover(series: &mut SeriesState, now: i64) {
    let stale = series
        .rollover
        .as_ref()
        .is_some_and(|rollover| match rollover {
            ObservedState::Watching { reset_at, .. } => reset_at
                .checked_add(agent_quota_duration::ROLLOVER_GRACE_SECONDS)
                .is_some_and(|deadline| now > deadline),
            ObservedState::Candidate { old_reset_at, .. } => old_reset_at
                .checked_add(agent_quota_duration::ROLLOVER_GRACE_SECONDS)
                .is_some_and(|deadline| now > deadline),
            ObservedState::Ready { reset_at, .. } => reset_at
                .checked_add(agent_quota_duration::ROLLOVER_GRACE_SECONDS)
                .is_some_and(|deadline| now > deadline),
        });
    if stale {
        series.rollover = None;
        series.active_reset_at = None;
    }
}

fn retain_series(series: &mut SeriesState, now: i64) {
    let now = now.max(series.last_activity_at);
    clear_stale_rollover(series, now);
    let nominal = series_nominal_duration(series, now);
    let (retained_cycles, horizon) = retention_limits(nominal);
    let cutoff = now.saturating_sub(horizon);
    let mut keep_completed = retention_cycles(series, now)
        .iter()
        .filter(|cycle| cycle.reset_at >= cutoff)
        .map(|cycle| cycle.reset_at)
        .collect::<Vec<_>>();
    keep_completed.sort_unstable_by(|left, right| right.cmp(left));
    keep_completed.truncate(retained_cycles);
    let keep_completed = keep_completed.into_iter().collect::<BTreeSet<_>>();
    let active_reset = series.active_reset_at;
    series.samples.retain(|sample| {
        let reset = normalize_reset(sample.reset_at, sample.duration_seconds);
        keep_completed.contains(&reset)
            || active_reset.is_some_and(|active| is_active_group_sample(active, sample))
    });
    series.samples.sort_by(sample_order);

    if series.samples.is_empty()
        && series.rollover.is_some()
        && series.last_activity_at < now.saturating_sub(RETENTION_MIN_SECONDS)
    {
        series.rollover = None;
        series.active_reset_at = None;
    }
    if series.samples.is_empty() && series.rollover.is_none() {
        series.active_reset_at = None;
    }
}

fn series_is_active(
    series: &SeriesState,
    key: &SeriesKey,
    active_keys: &BTreeSet<SeriesKey>,
    now: i64,
) -> bool {
    if active_keys.contains(key) {
        return true;
    }
    let reset_active = series.active_reset_at.is_some_and(|reset| {
        reset >= now.saturating_sub(agent_quota_duration::ROLLOVER_GRACE_SECONDS)
    });
    let rollover_active = series.rollover.as_ref().is_some_and(|rollover| {
        rollover_reset_at(rollover)
            >= now.saturating_sub(agent_quota_duration::ROLLOVER_GRACE_SECONDS)
    });
    reset_active || rollover_active
}

fn evict_inactive_series(
    store: &mut Store,
    active_keys: &BTreeSet<SeriesKey>,
    now: i64,
) -> Result<(), HistoryError> {
    if store.series.len() <= MAX_SERIES {
        return Ok(());
    }
    let mut inactive = store
        .series
        .iter()
        .filter(|series| !series_is_active(series, &series.key(), active_keys, now))
        .map(|series| (series.last_activity_at, series.key()))
        .collect::<Vec<_>>();
    inactive.sort();
    for (_, key) in inactive {
        if store.series.len() <= MAX_SERIES {
            break;
        }
        if let Ok(index) = store
            .series
            .binary_search_by(|series| series.key().cmp(&key))
        {
            store.series.remove(index);
        }
    }
    if store.series.len() > MAX_SERIES {
        return Err(HistoryError::StoreCapacity);
    }
    Ok(())
}

fn evict_old_completed_samples(store: &mut Store, now: i64) -> Result<(), HistoryError> {
    let mut candidates = Vec::new();
    for series in &store.series {
        let active_reset = series.active_reset_at;
        for (reset_at, samples) in grouped_samples(&series.samples) {
            if active_reset.is_some_and(|active| {
                samples
                    .iter()
                    .any(|sample| is_active_group_sample(active, sample))
            }) {
                continue;
            }
            if retention_cycle_descriptor(reset_at, &samples, now).is_some() {
                candidates.push((
                    reset_at,
                    series.provider_id.clone(),
                    series.account_scope.clone(),
                    series.window_key.clone(),
                ));
            }
        }
    }
    candidates.sort();
    while store
        .series
        .iter()
        .map(|series| series.samples.len())
        .sum::<usize>()
        > MAX_SAMPLES
    {
        let Some((reset_at, provider_id, account_scope, window_key)) = candidates.first().cloned()
        else {
            return Err(HistoryError::StoreCapacity);
        };
        candidates.remove(0);
        if let Some(series) = store.series.iter_mut().find(|series| {
            series.provider_id == provider_id
                && series.account_scope == account_scope
                && series.window_key == window_key
        }) {
            series.samples.retain(|sample| {
                normalize_reset(sample.reset_at, sample.duration_seconds) != reset_at
            });
        }
    }
    Ok(())
}

fn retain_store(
    store: &mut Store,
    now: i64,
    active_keys: &BTreeSet<SeriesKey>,
) -> Result<(), HistoryError> {
    for series in &mut store.series {
        retain_series(series, now);
    }
    store.series.retain(|series| {
        !series.samples.is_empty()
            || series.rollover.is_some()
            || active_keys.contains(&series.key())
    });
    store.series.sort_by(series_order);
    evict_inactive_series(store, active_keys, now)?;
    evict_old_completed_samples(store, now)?;
    store.series.retain(|series| {
        !series.samples.is_empty()
            || series.rollover.is_some()
            || active_keys.contains(&series.key())
    });
    store.series.sort_by(series_order);
    Ok(())
}

fn interpolate_curve(curve: &[f64], phase: f64) -> f64 {
    if curve.is_empty() {
        return 0.0;
    }
    if curve.len() == 1 {
        return curve[0];
    }
    let scaled = phase.clamp(0.0, 1.0) * (curve.len() - 1) as f64;
    let lower = scaled.floor() as usize;
    let upper = (lower + 1).min(curve.len() - 1);
    if lower == upper {
        curve[lower]
    } else {
        curve[lower] + (curve[upper] - curve[lower]) * (scaled - lower as f64)
    }
}

#[derive(Debug, Clone, Copy)]
struct BucketError {
    mse: f64,
    tail_mse: Option<f64>,
    tail_count: usize,
}

fn compare_fit_point_slices(left: &[FitPoint], right: &[FitPoint]) -> std::cmp::Ordering {
    for (left, right) in left.iter().zip(right) {
        let order = left
            .bucket
            .cmp(&right.bucket)
            .then(left.phase.total_cmp(&right.phase))
            .then(left.used_percent.total_cmp(&right.used_percent));
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

fn fit_points_by_bucket(points: &[FitPoint]) -> Option<Vec<(usize, Vec<FitPoint>)>> {
    let mut buckets = BTreeMap::<usize, Vec<FitPoint>>::new();
    for point in points {
        if !point.phase.is_finite()
            || !(0.0..=1.0).contains(&point.phase)
            || !point.used_percent.is_finite()
            || !(0.0 < point.used_percent && point.used_percent <= 100.0)
        {
            return None;
        }
        buckets.entry(point.bucket).or_default().push(*point);
    }
    let mut buckets = buckets.into_iter().collect::<Vec<_>>();
    for (_, points) in &mut buckets {
        points.sort_by(|left, right| {
            left.phase
                .total_cmp(&right.phase)
                .then(left.used_percent.total_cmp(&right.used_percent))
        });
    }
    buckets.sort_by(|left, right| {
        left.1[0]
            .phase
            .total_cmp(&right.1[0].phase)
            .then(left.0.cmp(&right.0))
    });
    (!buckets.is_empty()).then_some(buckets)
}

fn through_origin_beta(points: &[FitPoint]) -> Option<f64> {
    let mut ordered = points.to_vec();
    ordered.sort_by(|left, right| {
        left.bucket
            .cmp(&right.bucket)
            .then(left.phase.total_cmp(&right.phase))
            .then(left.used_percent.total_cmp(&right.used_percent))
    });
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for point in &ordered {
        numerator += point.phase * point.used_percent;
        denominator += point.phase * point.phase;
    }
    if !numerator.is_finite() || !denominator.is_finite() || denominator <= EPSILON {
        return None;
    }
    let beta = numerator / denominator;
    beta.is_finite().then_some(beta)
}

fn bucket_error(model: &[f64], points: &[FitPoint]) -> Option<BucketError> {
    let mut sum = 0.0;
    let mut count = 0usize;
    let mut tail_sum = 0.0;
    let mut tail_count = 0usize;
    for point in points {
        let residual = interpolate_curve(model, point.phase) - point.used_percent;
        if !residual.is_finite() {
            return None;
        }
        sum += residual * residual;
        count += 1;
        if point.phase + EPSILON >= TAIL_PHASE_THRESHOLD {
            tail_sum += residual * residual;
            tail_count += 1;
        }
    }
    if count == 0 || !sum.is_finite() {
        return None;
    }
    let mse = sum / count as f64;
    let tail_mse = (tail_count > 0).then(|| tail_sum / tail_count as f64);
    mse.is_finite().then_some(BucketError {
        mse,
        tail_mse,
        tail_count,
    })
}

fn aggregate_weighted_rms(values: &[(f64, f64)]) -> Option<f64> {
    let mut weighted_sum = 0.0;
    let mut total_weight = 0.0;
    for (value, weight) in values {
        if !value.is_finite() || !weight.is_finite() || *weight <= EPSILON {
            return None;
        }
        weighted_sum += value * weight;
        total_weight += weight;
    }
    if !weighted_sum.is_finite() || !total_weight.is_finite() || total_weight <= EPSILON {
        return None;
    }
    let rms = (weighted_sum / total_weight).sqrt();
    rms.is_finite().then_some(rms)
}

fn fit_lobo_cycle(points: &[FitPoint]) -> Option<(f64, Option<f64>, usize)> {
    let buckets = fit_points_by_bucket(points)?;
    let mut bucket_mse = Vec::with_capacity(buckets.len());
    let mut tail_sum = 0.0;
    let mut tail_count = 0usize;
    for (held_bucket, held_points) in &buckets {
        let training = points
            .iter()
            .copied()
            .filter(|point| point.bucket != *held_bucket)
            .collect::<Vec<_>>();
        if training.is_empty() {
            return None;
        }
        let model = reconstruct_fit_curve(&training);
        let error = bucket_error(&model, held_points)?;
        bucket_mse.push(error.mse);
        if let Some(mse) = error.tail_mse {
            tail_sum += mse * error.tail_count as f64;
            tail_count += error.tail_count;
        }
        count_lobo_folds(1);
    }
    if bucket_mse.is_empty() {
        return None;
    }
    let mse = bucket_mse.iter().sum::<f64>() / bucket_mse.len() as f64;
    let tail_mse = (tail_count > 0).then_some(tail_sum / tail_count as f64);
    mse.is_finite().then_some((mse, tail_mse, tail_count))
}

fn fit_loco_cycle(
    held_out: &FitCycleInput,
    other_curves: &[(&[f64], f64)],
) -> Option<(f64, Option<f64>, usize)> {
    if other_curves.is_empty() {
        return None;
    }
    let buckets = fit_points_by_bucket(&held_out.points)?;
    let mut bucket_mse = Vec::with_capacity(buckets.len());
    let mut tail_sum = 0.0;
    let mut tail_count = 0usize;
    for (_, held_points) in buckets {
        let mut residual_sum = 0.0;
        let mut count = 0usize;
        let mut bucket_tail_sum = 0.0;
        let mut bucket_tail_count = 0usize;
        for point in &held_points {
            let values = other_curves
                .iter()
                .map(|(curve, _)| interpolate_curve(curve, point.phase))
                .collect::<Vec<_>>();
            let weights = other_curves
                .iter()
                .map(|(_, weight)| *weight)
                .collect::<Vec<_>>();
            let predicted = weighted_median(&values, &weights);
            let residual = predicted - point.used_percent;
            if !residual.is_finite() {
                return None;
            }
            residual_sum += residual * residual;
            count += 1;
            if point.phase + EPSILON >= TAIL_PHASE_THRESHOLD {
                bucket_tail_sum += residual * residual;
                bucket_tail_count += 1;
            }
        }
        if count == 0 {
            return None;
        }
        bucket_mse.push(residual_sum / count as f64);
        tail_sum += bucket_tail_sum;
        tail_count += bucket_tail_count;
    }
    if bucket_mse.is_empty() {
        return None;
    }
    count_loco_folds(1);
    let mse = bucket_mse.iter().sum::<f64>() / bucket_mse.len() as f64;
    let tail_mse = (tail_count > 0).then_some(tail_sum / tail_count as f64);
    mse.is_finite().then_some((mse, tail_mse, tail_count))
}

pub(crate) fn fit_completed_cycles(cycles: &[FitCycleInput]) -> Option<CompletedFitResult> {
    if cycles.is_empty() {
        return None;
    }
    let mut normalized = Vec::with_capacity(cycles.len());
    for cycle in cycles {
        if !cycle.recency_weight.is_finite()
            || cycle.recency_weight <= EPSILON
            || cycle.points.is_empty()
        {
            return None;
        }
        let points = fit_points_by_bucket(&cycle.points)?
            .into_iter()
            .flat_map(|(_, points)| points)
            .collect::<Vec<_>>();
        normalized.push((
            cycle.recency_weight,
            points,
            reconstruct_fit_curve(&cycle.points),
        ));
    }
    normalized.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| compare_fit_point_slices(&left.1, &right.1))
            .then_with(|| {
                left.2
                    .iter()
                    .zip(&right.2)
                    .map(|(left, right)| left.total_cmp(right))
                    .find(|order| *order != std::cmp::Ordering::Equal)
                    .unwrap_or_else(|| left.2.len().cmp(&right.2.len()))
            })
    });

    let mut lobo = Vec::with_capacity(normalized.len());
    for (weight, points, _) in &normalized {
        let (mse, tail_mse, tail_count) = fit_lobo_cycle(points)?;
        lobo.push((*weight, mse, tail_mse, tail_count));
    }
    let lobo_overall = aggregate_weighted_rms(
        &lobo
            .iter()
            .map(|(weight, mse, _, _)| (*mse, *weight))
            .collect::<Vec<_>>(),
    )?;

    let loco = if normalized.len() >= 2 {
        let mut values = Vec::with_capacity(normalized.len());
        for (index, (weight, points, _)) in normalized.iter().enumerate() {
            let held_out = FitCycleInput {
                recency_weight: *weight,
                points: points.clone(),
            };
            let other_curves = normalized
                .iter()
                .enumerate()
                .filter(|(other_index, _)| *other_index != index)
                .map(|(_, (other_weight, _, curve))| (curve.as_slice(), *other_weight))
                .collect::<Vec<_>>();
            let (mse, tail_mse, tail_count) = fit_loco_cycle(&held_out, &other_curves)?;
            values.push((*weight, mse, tail_mse, tail_count));
        }
        Some(values)
    } else {
        None
    };
    let overall_rmse = match &loco {
        Some(values) => lobo_overall.max(aggregate_weighted_rms(
            &values
                .iter()
                .map(|(weight, mse, _, _)| (*mse, *weight))
                .collect::<Vec<_>>(),
        )?),
        None => lobo_overall,
    };

    let total_weight = normalized.iter().map(|(weight, _, _)| *weight).sum::<f64>();
    if !total_weight.is_finite() || total_weight <= EPSILON {
        return None;
    }
    let historical_curve = (0..GRID_POINT_COUNT)
        .map(|index| {
            let values = normalized
                .iter()
                .map(|(weight, _, curve)| (curve[index], *weight))
                .collect::<Vec<_>>();
            let values_only = values.iter().map(|(value, _)| *value).collect::<Vec<_>>();
            let weights = values.iter().map(|(_, weight)| *weight).collect::<Vec<_>>();
            weighted_median(&values_only, &weights)
        })
        .collect::<Vec<_>>();
    if historical_curve.iter().any(|value| !value.is_finite()) {
        return None;
    }

    let (tail_rmse, tail_cycle_count) = match loco {
        Some(values) => {
            let lobo_tail_values: Option<Vec<(f64, f64)>> = lobo
                .iter()
                .filter(|(_, _, _, count)| *count >= 3)
                .map(|(weight, _, mse, _)| mse.map(|value| (value, *weight)))
                .collect();
            let lobo_tail = lobo_tail_values.and_then(|values| aggregate_weighted_rms(&values));
            let loco_tail_values: Option<Vec<(f64, f64)>> = values
                .iter()
                .filter(|(_, _, _, count)| *count >= 3)
                .map(|(weight, _, mse, _)| mse.map(|value| (value, *weight)))
                .collect();
            let loco_tail = loco_tail_values.and_then(|values| aggregate_weighted_rms(&values));
            let complete_tail = lobo
                .iter()
                .zip(values.iter())
                .filter(
                    |((_, _, lobo_mse, lobo_count), (_, _, loco_mse, loco_count))| {
                        lobo_count >= &3
                            && loco_count >= &3
                            && lobo_mse.is_some()
                            && loco_mse.is_some()
                    },
                )
                .count();
            (
                lobo_tail.zip(loco_tail).map(|(lobo, loco)| lobo.max(loco)),
                complete_tail,
            )
        }
        None => {
            let tail_values: Option<Vec<(f64, f64)>> = lobo
                .iter()
                .filter(|(_, _, _, count)| *count >= 3)
                .map(|(weight, _, mse, _)| mse.map(|value| (value, *weight)))
                .collect();
            let tail = tail_values.and_then(|values| aggregate_weighted_rms(&values));
            let count = lobo.iter().filter(|(_, _, _, count)| *count >= 3).count();
            (tail, count)
        }
    };

    Some(CompletedFitResult {
        historical_curve,
        overall_rmse,
        tail_rmse,
        tail_cycle_count,
        total_weight,
    })
}

pub(crate) fn fit_partial_current(points: &[FitPoint]) -> Option<PartialFitResult> {
    let buckets = fit_points_by_bucket(points)?;
    if buckets.len() < 6 {
        return None;
    }
    let mut holdout_mse = Vec::with_capacity(buckets.len().saturating_sub(3));
    for index in 3..buckets.len() {
        let training = buckets[..index]
            .iter()
            .flat_map(|(_, points)| points.iter().copied())
            .collect::<Vec<_>>();
        let beta = through_origin_beta(&training)?;
        holdout_mse.push(heldout_linear_error(beta, &buckets[index].1)?);
        count_walk_forward_fits(1);
    }
    if holdout_mse.len() < 3 {
        return None;
    }
    let walk_forward_rmse = (holdout_mse.iter().sum::<f64>() / holdout_mse.len() as f64).sqrt();
    let beta = through_origin_beta(points)?;
    if !walk_forward_rmse.is_finite() || !beta.is_finite() {
        return None;
    }
    Some(PartialFitResult {
        beta,
        walk_forward_rmse,
    })
}

fn heldout_linear_error(beta: f64, points: &[FitPoint]) -> Option<f64> {
    if !beta.is_finite() || points.is_empty() {
        return None;
    }
    let mse = points
        .iter()
        .map(|point| {
            let residual = beta * point.phase - point.used_percent;
            residual * residual
        })
        .sum::<f64>()
        / points.len() as f64;
    mse.is_finite().then_some(mse)
}

fn first_crossing(phase_now: f64, curve: &[f64], shift: f64, actual_at_now: f64) -> Option<f64> {
    if curve.len() < 2 {
        return None;
    }
    let denominator = (curve.len() - 1) as f64;
    let mut previous_phase = phase_now;
    let mut previous_value = actual_at_now;
    let start = ((phase_now * denominator).floor() as usize + 1).clamp(1, curve.len() - 1);
    for (index, value) in curve.iter().enumerate().skip(start) {
        let phase = index as f64 / denominator;
        if phase <= phase_now + EPSILON {
            continue;
        }
        let shifted = *value + shift;
        if previous_value < RUNOUT_THRESHOLD_PERCENT && shifted >= RUNOUT_THRESHOLD_PERCENT {
            let delta = shifted - previous_value;
            if delta.abs() <= EPSILON {
                return Some(phase);
            }
            let ratio = ((100.0 - previous_value) / delta).clamp(0.0, 1.0);
            return Some((previous_phase + ratio * (phase - previous_phase)).clamp(phase_now, 1.0));
        }
        previous_phase = phase;
        previous_value = shifted;
    }
    None
}

#[derive(Debug, Clone)]
struct TargetCalculation {
    complete_cycles: usize,
    pace: Option<HistoricalPace>,
}

fn calculate_target(
    store: &Store,
    key: &SeriesKey,
    reset_at: i64,
    duration_seconds: i64,
    actual: f64,
    now: i64,
) -> TargetCalculation {
    let Some(series) = find_target_series(store, key) else {
        return TargetCalculation {
            complete_cycles: 0,
            pace: None,
        };
    };
    count_target_sample_reads(series.samples.len());
    let current_reset = normalize_reset(reset_at, duration_seconds);
    let cycles = historical_cycles(series, current_reset, now);
    count_target_profiles_built(cycles.len());
    TargetCalculation {
        complete_cycles: cycles.len(),
        pace: evaluate_current_from_series(
            series,
            &cycles,
            reset_at,
            duration_seconds,
            actual,
            now,
        ),
    }
}

fn evaluate_current(
    store: &Store,
    key: &SeriesKey,
    reset_at: i64,
    duration_seconds: i64,
    actual: f64,
    now: i64,
) -> Option<HistoricalPace> {
    let series = find_target_series(store, key)?;
    let cycles = historical_cycles(series, normalize_reset(reset_at, duration_seconds), now);
    evaluate_current_from_series(series, &cycles, reset_at, duration_seconds, actual, now)
}

fn evaluate_current_from_series(
    series: &SeriesState,
    cycles: &[CycleProfile],
    reset_at: i64,
    duration_seconds: i64,
    actual: f64,
    now: i64,
) -> Option<HistoricalPace> {
    if !valid_duration(duration_seconds)
        || !actual.is_finite()
        || !(0.0..=100.0).contains(&actual)
        || reset_at <= now
    {
        return None;
    }
    let normalized_current_reset = normalize_reset(reset_at, duration_seconds);
    let nominal_duration = median_i64(cycles.iter().map(|cycle| cycle.duration_seconds))
        .unwrap_or(duration_seconds)
        .clamp(1, agent_quota_duration::MAX_DURATION_SECONDS);
    let tau_cycles = (7.0 * 86_400.0 / nominal_duration as f64).clamp(3.0, 64.0);
    let weighted = cycles
        .iter()
        .map(|cycle| {
            let age_cycles = ((normalized_current_reset - cycle.reset_at).max(0) as f64)
                / nominal_duration as f64;
            let weight = (-age_cycles / tau_cycles).exp();
            (cycle, weight)
        })
        .collect::<Vec<_>>();
    if weighted
        .iter()
        .any(|(_, weight)| !weight.is_finite() || *weight <= EPSILON)
    {
        return None;
    }
    let fit_inputs = weighted
        .iter()
        .map(|(cycle, weight)| FitCycleInput {
            recency_weight: *weight,
            points: cycle.points.clone(),
        })
        .collect::<Vec<_>>();
    if let Some(fit) = fit_completed_cycles(&fit_inputs) {
        if let Some(quality) = fit_quality(fit.overall_rmse) {
            if let Some(pace) = evaluate_completed_projection(
                cycles,
                &weighted,
                fit,
                quality,
                reset_at,
                duration_seconds,
                actual,
                now,
            ) {
                return Some(pace);
            }
        }
    }
    evaluate_partial_projection(
        series,
        normalized_current_reset,
        reset_at,
        duration_seconds,
        actual,
        now,
    )
}

fn evaluate_completed_projection(
    cycles: &[CycleProfile],
    weighted: &[(&CycleProfile, f64)],
    fit: CompletedFitResult,
    quality: f64,
    reset_at: i64,
    duration_seconds: i64,
    actual: f64,
    now: i64,
) -> Option<HistoricalPace> {
    let lambda = completed_blend_weight(quality, fit.total_weight)?;
    if fit.historical_curve.len() != GRID_POINT_COUNT {
        return None;
    }
    let denominator = (GRID_POINT_COUNT - 1) as f64;
    let mut expected_curve = fit.historical_curve;
    for (index, value) in expected_curve.iter_mut().enumerate() {
        let linear = 100.0 * index as f64 / denominator;
        *value = (lambda * *value + (1.0 - lambda) * linear).clamp(0.0, 100.0);
    }
    let mut expected_max = 0.0_f64;
    for value in &mut expected_curve {
        expected_max = expected_max.max(*value);
        *value = expected_max;
    }

    let elapsed = duration_seconds.saturating_sub(reset_at.saturating_sub(now));
    let phase_now = (elapsed as f64 / duration_seconds as f64).clamp(0.0, 1.0);
    let expected_now = interpolate_curve(&expected_curve, phase_now).clamp(0.0, 100.0);
    let total_weight = weighted.iter().map(|(_, weight)| *weight).sum::<f64>();
    let squared_weight = weighted
        .iter()
        .map(|(_, weight)| weight * weight)
        .sum::<f64>();
    if !total_weight.is_finite() || total_weight <= EPSILON || squared_weight <= EPSILON {
        return None;
    }
    let n_eff = total_weight * total_weight / squared_weight;
    if !n_eff.is_finite() {
        return None;
    }

    let mut weighted_run_out_mass = 0.0;
    let mut crossing_candidates = Vec::new();
    for (cycle, weight) in weighted {
        let mut extended = cycle.curve.clone();
        if let Some(cap_index) = extended
            .iter()
            .position(|value| *value >= RUNOUT_THRESHOLD_PERCENT)
            .filter(|index| *index > 0 && *index < extended.len() - 1)
        {
            let cap_phase = cap_index as f64 / denominator;
            let slope = extended[cap_index] / cap_phase;
            if slope.is_finite() {
                for (index, value) in extended.iter_mut().enumerate().skip(cap_index) {
                    *value = slope * index as f64 / denominator;
                }
            }
        }
        let historical_now = interpolate_curve(&extended, phase_now);
        let shift = actual - historical_now;
        let shifted_end = extended.last().copied().unwrap_or(0.0) + shift;
        if shifted_end >= RUNOUT_THRESHOLD_PERCENT {
            weighted_run_out_mass += *weight;
            if let Some(crossing) = first_crossing(phase_now, &extended, shift, actual) {
                crossing_candidates.push((
                    (crossing - phase_now).max(0.0) * duration_seconds as f64,
                    *weight,
                ));
            }
        }
    }

    let smoothed = ((weighted_run_out_mass + 0.5) / (total_weight + 1.0)).clamp(0.0, 1.0);
    let span = cycles
        .iter()
        .map(|cycle| cycle.reset_at)
        .max()?
        .saturating_sub(cycles.iter().map(|cycle| cycle.cycle_started_at).min()?);
    let risk_span =
        (4 * median_i64(cycles.iter().map(|cycle| cycle.duration_seconds))?.max(1)).max(7 * 86_400);
    let tail_pass = fit.tail_cycle_count == cycles.len()
        && fit
            .tail_rmse
            .is_some_and(|rmse| fit_quality(rmse).is_some());
    let risk_gate = cycles.len() >= 5 && n_eff >= 4.0 && span >= risk_span && tail_pass;
    let mut run_out_probability = risk_gate.then_some(smoothed);
    let mut will_last = smoothed < 0.5;
    let mut eta_seconds = None;
    if actual >= 100.0 {
        run_out_probability = Some(1.0);
        will_last = false;
        eta_seconds = Some(0.0);
    } else if !will_last {
        if crossing_candidates.is_empty() {
            will_last = true;
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
        will_last_to_reset: will_last,
        run_out_probability,
    })
}

fn evaluate_partial_projection(
    series: &SeriesState,
    normalized_current_reset: i64,
    reset_at: i64,
    duration_seconds: i64,
    actual: f64,
    now: i64,
) -> Option<HistoricalPace> {
    let active_reset = series.active_reset_at?;
    if normalize_reset(active_reset, duration_seconds) != normalized_current_reset {
        return None;
    }
    let active_samples = series
        .samples
        .iter()
        .filter(|sample| is_active_group_sample(active_reset, sample))
        .filter(|sample| sample.sampled_at <= now)
        .collect::<Vec<_>>();
    if active_samples.is_empty()
        || active_samples.iter().any(|sample| {
            sample.duration_seconds != duration_seconds
                || normalize_reset(sample.reset_at, sample.duration_seconds)
                    != normalized_current_reset
        })
    {
        return None;
    }
    let points = active_samples
        .iter()
        .map(|sample| FitPoint {
            phase: phase(sample),
            bucket: sample_key(sample).1,
            used_percent: sample.used_percent,
        })
        .collect::<Vec<_>>();
    let buckets = fit_points_by_bucket(&points)?;
    let phases = points.iter().map(|point| point.phase).collect::<Vec<_>>();
    let span =
        phases.iter().copied().reduce(f64::max)? - phases.iter().copied().reduce(f64::min)?;
    if buckets.len() < MIN_COMPLETE_BUCKETS || span + EPSILON < 0.10 {
        return None;
    }
    let fit = fit_partial_current(&points)?;
    let quality = fit_quality(fit.walk_forward_rmse)?;
    let lambda = partial_blend_weight(quality)?;
    let elapsed = duration_seconds.saturating_sub(reset_at.saturating_sub(now));
    let u_now = (elapsed as f64 / duration_seconds as f64).clamp(0.0, 1.0);
    let trend_demand = |phase: f64| (fit.beta * phase).max(0.0);
    let linear_demand = |phase: f64| 100.0 * phase;
    let base_demand =
        |phase: f64| lambda * trend_demand(phase) + (1.0 - lambda) * linear_demand(phase);
    let expected_now = base_demand(u_now).clamp(0.0, 100.0);
    if actual >= 100.0 {
        return Some(HistoricalPace {
            expected_percent: expected_now,
            eta_seconds: Some(0.0),
            will_last_to_reset: false,
            run_out_probability: Some(1.0),
        });
    }
    let base_slope = lambda * fit.beta + (1.0 - lambda) * 100.0;
    if !base_slope.is_finite() || base_slope <= EPSILON {
        return None;
    }
    let shift = actual - base_demand(u_now);
    let shifted_end = base_demand(1.0) + shift;
    let (eta_seconds, will_last_to_reset) = if shifted_end < 100.0 - EPSILON {
        (None, true)
    } else {
        let crossing = (100.0 - shift) / base_slope;
        if !crossing.is_finite() || crossing <= u_now || crossing > 1.0 {
            return None;
        }
        (
            Some(((crossing.clamp(u_now, 1.0) - u_now) * duration_seconds as f64).max(0.0)),
            false,
        )
    };
    if will_last_to_reset != eta_seconds.is_none() {
        return None;
    }
    Some(HistoricalPace {
        expected_percent: expected_now,
        eta_seconds,
        will_last_to_reset,
        run_out_probability: None,
    })
}

fn with_locked_transaction_with_mode<T>(
    mode: StorageMode,
    path: &Path,
    observation_now: i64,
    transaction_clock: impl FnOnce() -> i64,
    body: impl FnOnce(&mut Store) -> Result<T, HistoryError>,
) -> Result<T, HistoryError> {
    with_locked_transaction_with_save_and_mode(
        mode,
        path,
        observation_now,
        transaction_clock,
        |path, store| save_store_atomic_with_mode(mode, path, store),
        body,
    )
}

fn with_locked_transaction_with_save_and_mode<T>(
    mode: StorageMode,
    path: &Path,
    observation_now: i64,
    transaction_clock: impl FnOnce() -> i64,
    save: impl Fn(&Path, &Store) -> io::Result<()>,
    body: impl FnOnce(&mut Store) -> Result<T, HistoryError>,
) -> Result<T, HistoryError> {
    let directory = path.parent().ok_or(HistoryError::StorageUnavailable)?;
    ensure_real_directory_with_mode(mode, directory)
        .map_err(|_| HistoryError::StorageUnavailable)?;

    let _process_guard = HISTORY_PROCESS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock_file = open_history_lock(mode, &directory.join(HISTORY_LOCK_FILE_NAME))?;
    let lock_time = transaction_clock();
    let upper_bound = observation_now.max(lock_time);

    let loaded = load_store_at_with_mode(mode, path, upper_bound, observation_now);
    let result = match loaded {
        Ok(mut loaded) => {
            let before = loaded.store.clone();
            let result = body(&mut loaded.store);
            match result {
                Ok(value) => {
                    if !validate_store_at(&loaded.store, upper_bound) {
                        Err(HistoryError::Serialize)
                    } else if loaded.store == before {
                        Ok(value)
                    } else if save(path, &loaded.store).is_err() {
                        Err(HistoryError::AtomicSave)
                    } else {
                        Ok(value)
                    }
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    };
    let unlock = fs2::FileExt::unlock(&lock_file).map_err(|_| HistoryError::LockRelease);
    match (result, unlock) {
        (Err(error), _) => Err(error),
        (Ok(_value), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn read_legacy_v2_with_mode(_mode: StorageMode, path: &Path) -> io::Result<Option<Vec<u8>>> {
    #[cfg(target_os = "windows")]
    if _mode.uses_windows_secure_storage() {
        return read_owner_only_with_mode(_mode, path);
    }

    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_owner_only_with_mode(mode: StorageMode, path: &Path) -> io::Result<Option<Vec<u8>>> {
    let Some(mut file) = open_existing_owner_only_with_mode(mode, path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    verify_open_regular_file_with_mode(mode, path, &file)?;
    Ok(Some(bytes))
}

fn load_store(path: &Path, now: i64) -> Result<LoadedStore, HistoryError> {
    load_store_at_with_mode(StorageMode::Generic, path, now, now)
}

/// Upgrade a store deserialized from disk to `HISTORY_SCHEMA_VERSION`.
///
/// v3 → v4 adds `QuotaSample::plan`, which `#[serde(default)]` has already
/// filled with `None` by the time this runs. Nothing else differs, so the
/// upgrade is the version stamp alone — and deliberately nothing more. Reading
/// a plan for samples taken before the field existed would turn a known
/// unknown into an unknown error: the user's history contains a real plan
/// change, and a backfilled value would be wrong for one side of it while
/// every downstream check kept working on the wrong answer.
///
/// **Placed between deserialization and `validate_store`, and the version
/// equality inside `validate_store` is left exactly as strict.** That check
/// demands an exact version match
/// and is also the gate `validate_store_at` applies before a transaction
/// writes (`save_store_atomic_with_mode`). Relaxing it there would let a store
/// still stamped `3` reach the writer and produce a file whose version field
/// says 3 while its samples carry `plan` — a shape no version of this code can
/// read back, written irreversibly.
///
/// **The upgrade is lazy, and that is the chosen semantics.** The transaction
/// snapshots `before` from the value this function returns and writes only when
/// the store actually changed, so a bare version bump does not dirty it and
/// does not trigger a write; `read_series_at_path_with_mode` never writes at
/// all. The file therefore stays `"schemaVersion": 3` on disk until the next
/// transaction that had a reason to write anyway, re-upgrading in memory on
/// every load until then. Chosen over an eager upgrade because writing is the
/// only irreversible act here, and because a file still stamped 3 is one an
/// older build can still read — the downgrade window closes when the file is
/// rewritten, not when this code ships. The cost is that the on-disk format
/// converts at an unpredictable moment, which is why it is documented here,
/// in `architecture.md`, and in `verification.md`: someone inspecting the file
/// and finding `3` would otherwise read it as a failed migration.
///
/// A store at any other version is returned untouched for `validate_store` to
/// reject, which quarantines it. That covers a file written by a future build.
///
/// **Ordering against `drop_unplaceable_samples`** (PR #227, which occupies
/// this same position): the upgrade runs first. The two are commutative today
/// — that pass reads only `series.samples` and this one writes only
/// `schema_version` — so the order is chosen for the case where they stop
/// being commutative. A migration that ever rewrites samples must run before
/// the pass that judges whether a sample is placeable, or the judging happens
/// against the shape the previous version wrote. Version first, samples
/// second, and a future migration inherits the order rather than re-deciding
/// it under merge pressure.
fn migrate_store_to_current(mut store: Store) -> Store {
    if store.schema_version == HISTORY_SCHEMA_VERSION_V3 {
        store.schema_version = HISTORY_SCHEMA_VERSION;
    }
    store
}

fn load_store_at_with_mode(
    mode: StorageMode,
    path: &Path,
    validation_now: i64,
    quarantine_now: i64,
) -> Result<LoadedStore, HistoryError> {
    let Some(bytes) = read_owner_only_with_mode(mode, path).map_err(|_| HistoryError::Read)? else {
        return Ok(LoadedStore {
            store: Store::default(),
            quarantined: false,
        });
    };

    // Drop offending SAMPLES before condemning the FILE. A duplicate sample key
    // and a cycle over its bucket cap are per-sample facts, and quarantine is
    // per-store: a resolution change once turned three bad keys out of 2,775
    // samples into 31 series moved aside and every window back to "learning
    // history". Nothing about that file was untrustworthy — 99.9% of it was the
    // same data it had been a minute earlier.
    //
    // Quarantine stays for what it was for: bytes that will not parse, a
    // schema version this build does not speak, series out of order — things
    // that say the file did not come from here. A sample this build cannot
    // place is a sample to forget, not a history to burn.
    let parsed = serde_json::from_slice::<Store>(&bytes)
        .ok()
        // Version before samples, and the reason is not one you can observe
        // here. `drop_unplaceable_samples` judges every sample against the
        // CURRENT bucket rules and per-cycle cap, so a migration that rewrites
        // `sampled_at`, `reset_at`, or `duration_seconds` must run before it —
        // otherwise its output is judged by coordinates it was in the middle
        // of changing. v3→v4 rewrites none of those (it stamps the version and
        // relies on `serde(default)` for `plan`), so these two commute today:
        // read and write sets are disjoint. The constraint is prospective, and
        // it is written down now because the migration that needs it will not
        // be able to add it retroactively.
        .map(migrate_store_to_current)
        .map(drop_unplaceable_samples)
        .filter(|store| validate_store(store));
    if let Some(store) = parsed {
        let store = if validate_store_at(&store, validation_now) {
            store
        } else {
            repair_store_at(store, validation_now, quarantine_now)
        };
        return Ok(LoadedStore {
            store,
            quarantined: false,
        });
    }

    quarantine_corrupt_with_mode(mode, path, quarantine_now)
        .map_err(|_| HistoryError::CorruptQuarantine)?;
    Ok(LoadedStore {
        store: Store::default(),
        quarantined: true,
    })
}

fn quarantine_corrupt_with_mode(_mode: StorageMode, path: &Path, now: i64) -> io::Result<PathBuf> {
    #[cfg(target_os = "windows")]
    if _mode.uses_windows_secure_storage() {
        let directory = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing quota pace history directory",
            )
        })?;
        let directory_handle =
            crate::agent_storage_windows::ensure_secure_storage_directory(directory)?;
        for suffix in 0..=u32::MAX {
            let name = if suffix == 0 {
                format!("quota-pace-history-v3.corrupt-{now}.json")
            } else {
                format!("quota-pace-history-v3.corrupt-{now}.{suffix}.json")
            };
            let candidate = directory.join(name);
            match crate::agent_storage_windows::quarantine_secure_file_candidate(
                &directory_handle,
                directory,
                path,
                &candidate,
            ) {
                Ok(()) => return Ok(candidate),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "unable to choose a quota pace quarantine name",
        ));
    }

    quarantine_corrupt(path, now)
}

fn quarantine_corrupt(path: &Path, now: i64) -> io::Result<PathBuf> {
    quarantine_corrupt_with(path, now, |source| fs::remove_file(source))
}

fn quarantine_corrupt_with<U>(path: &Path, now: i64, unlink: U) -> io::Result<PathBuf>
where
    U: Fn(&Path) -> io::Result<()>,
{
    quarantine_corrupt_with_ops(
        path,
        now,
        |source, candidate| fs::hard_link(source, candidate),
        unlink,
    )
}

fn quarantine_corrupt_with_ops<L, U>(
    path: &Path,
    now: i64,
    mut link: L,
    unlink: U,
) -> io::Result<PathBuf>
where
    L: FnMut(&Path, &Path) -> io::Result<()>,
    U: Fn(&Path) -> io::Result<()>,
{
    let source = open_existing_owner_only(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "quota pace history disappeared before quarantine",
        )
    })?;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    for suffix in 0..=u32::MAX {
        let name = if suffix == 0 {
            format!("quota-pace-history-v3.corrupt-{now}.json")
        } else {
            format!("quota-pace-history-v3.corrupt-{now}.{suffix}.json")
        };
        let candidate = directory.join(name);
        verify_open_regular_file(path, &source)?;
        match link(path, &candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
        if let Err(error) = verify_open_regular_file(path, &source) {
            rollback_quarantine_link(&candidate, &source);
            return Err(error);
        }
        if let Err(error) = verify_open_regular_file(&candidate, &source) {
            rollback_quarantine_link(&candidate, &source);
            return Err(error);
        }
        if let Err(error) = unlink(path) {
            rollback_quarantine_link(&candidate, &source);
            return Err(error);
        }
        return Ok(candidate);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to choose a quota pace quarantine name",
    ))
}

fn save_store_atomic_with_mode(mode: StorageMode, path: &Path, store: &Store) -> io::Result<()> {
    #[cfg(test)]
    SAVE_CALL_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    #[cfg(target_os = "windows")]
    if mode.uses_windows_secure_storage() {
        return save_store_atomic_windows_secure_with_replace(
            path,
            store,
            crate::agent_storage_windows::replace_secure_file,
        );
    }

    save_store_atomic_with_sync(
        path,
        store,
        |temp, destination| tokscale_core::fs_atomic::replace_file(temp, destination),
        |directory| sync_directory_with_mode(mode, directory),
    )
}

#[cfg(test)]
pub(crate) fn save_call_count() -> u64 {
    SAVE_CALL_COUNT.with(Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_save_call_count() {
    SAVE_CALL_COUNT.with(|count| count.set(0));
}

#[cfg(target_os = "windows")]
fn save_store_atomic_windows_secure_with_replace(
    path: &Path,
    store: &Store,
    replace: impl FnOnce(&File, &Path, &Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing quota pace history directory",
        )
    })?;
    let payload = serialize_store_canonical(store)?;
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(HISTORY_FILE_NAME);
    let temp_path = directory.join(format!(".{file_name}.tmp-{}-{counter}", std::process::id()));
    let mut temp_created = false;

    let result = (|| {
        let directory_handle =
            crate::agent_storage_windows::ensure_secure_storage_directory(directory)?;
        let mut file = crate::agent_storage_windows::create_new_secure_file(&temp_path)?;
        temp_created = true;
        file.write_all(&payload)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        replace(&directory_handle, directory, &temp_path, path)
    })();
    if result.is_err() && temp_created {
        cleanup_windows_secure_history_temp(&temp_path);
    }
    result
}

#[cfg(target_os = "windows")]
fn cleanup_windows_secure_history_temp(path: &Path) {
    let Ok(file) = crate::agent_storage_windows::open_existing_secure_file(path, false) else {
        return;
    };
    if crate::agent_storage_windows::verify_secure_file_path(&file, path).is_ok() {
        let _ = fs::remove_file(path);
    }
}

fn save_store_atomic_with<F>(path: &Path, store: &Store, replace: F) -> io::Result<()>
where
    F: Fn(&Path, &Path) -> io::Result<()>,
{
    save_store_atomic_with_sync(path, store, replace, sync_directory)
}

fn serialize_store_canonical(store: &Store) -> io::Result<Vec<u8>> {
    let mut canonical = store.clone();
    canonical.series.sort_by(series_order);
    for series in &mut canonical.series {
        series.samples.sort_by(sample_order);
    }
    serde_json::to_vec_pretty(&canonical).map_err(io::Error::other)
}

fn save_store_atomic_with_sync<F, S>(
    path: &Path,
    store: &Store,
    replace: F,
    sync: S,
) -> io::Result<()>
where
    F: Fn(&Path, &Path) -> io::Result<()>,
    S: Fn(&Path) -> io::Result<()>,
{
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_real_directory(directory)?;
    let payload = serialize_store_canonical(store)?;
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(HISTORY_FILE_NAME);
    let temp_path = directory.join(format!(".{file_name}.tmp-{}-{counter}", std::process::id()));

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path)?;
        file.write_all(&payload)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        replace(&temp_path, path)?;
        Ok::<(), io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
        return result;
    }

    // replace is the commit point; directory sync is best-effort because it
    // cannot restore the previous file after the rename has succeeded.
    let _ = sync(directory);
    Ok(())
}

fn sync_directory_with_mode(_mode: StorageMode, directory: &Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    if _mode.uses_windows_secure_storage() {
        let directory = crate::agent_storage_windows::ensure_secure_storage_directory(directory)?;
        return crate::agent_storage_windows::flush_secure_storage_directory(&directory);
    }

    sync_directory(directory)
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

fn ensure_real_directory_with_mode(_mode: StorageMode, directory: &Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    if _mode.uses_windows_secure_storage() {
        drop(crate::agent_storage_windows::ensure_secure_storage_directory(directory)?);
        return Ok(());
    }

    ensure_real_directory(directory)
}

fn ensure_real_directory(directory: &Path) -> io::Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "quota pace storage is not a real directory",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(directory)?;
        }
        Err(error) => return Err(error),
    }

    let path_metadata = fs::symlink_metadata(directory)?;
    if !path_metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quota pace storage is not a real directory",
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let file = File::open(directory)?;
        verify_open_directory(directory, &file)?;
        file.set_permissions(fs::Permissions::from_mode(0o700))?;
        verify_open_directory(directory, &file)?;
    }
    Ok(())
}

fn open_history_lock(_mode: StorageMode, path: &Path) -> Result<File, HistoryError> {
    #[cfg(target_os = "windows")]
    if _mode.uses_windows_secure_storage() {
        return crate::agent_storage_windows::open_secure_lock_file(path)
            .map_err(|_| HistoryError::LockOpen);
    }

    let file = open_owner_only(path).map_err(|_| HistoryError::LockOpen)?;
    file.lock_exclusive()
        .map_err(|_| HistoryError::LockAcquire)?;
    Ok(file)
}

fn open_owner_only(path: &Path) -> io::Result<File> {
    let mut create = OpenOptions::new();
    create.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        create.mode(0o600);
    }
    match create.open(path) {
        Ok(file) => secure_open_regular_file(path, file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            require_regular_file_path(path)?;
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            secure_open_regular_file(path, file)
        }
        Err(error) => Err(error),
    }
}

fn open_existing_owner_only_with_mode(_mode: StorageMode, path: &Path) -> io::Result<Option<File>> {
    #[cfg(target_os = "windows")]
    if _mode.uses_windows_secure_storage() {
        return match crate::agent_storage_windows::open_existing_secure_file(path, false) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        };
    }

    open_existing_owner_only(path)
}

fn open_existing_owner_only(path: &Path) -> io::Result<Option<File>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "quota pace artifact is not a regular file",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let file = OpenOptions::new().read(true).open(path)?;
    secure_open_regular_file(path, file).map(Some)
}

fn require_regular_file_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quota pace artifact is not a regular file",
        ))
    }
}

fn secure_open_regular_file(path: &Path, file: File) -> io::Result<File> {
    verify_open_regular_file(path, &file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    verify_open_regular_file(path, &file)?;
    Ok(file)
}

fn verify_open_regular_file_with_mode(
    _mode: StorageMode,
    path: &Path,
    file: &File,
) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    if _mode.uses_windows_secure_storage() {
        return crate::agent_storage_windows::verify_secure_file_path(file, path);
    }

    verify_open_regular_file(path, file)
}

fn verify_open_regular_file(path: &Path, file: &File) -> io::Result<()> {
    let file_metadata = file.metadata()?;
    let path_metadata = fs::symlink_metadata(path)?;
    if !file_metadata.file_type().is_file() || !path_metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quota pace artifact is not a regular file",
        ));
    }
    #[cfg(unix)]
    if !same_file(&file_metadata, &path_metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quota pace artifact changed while opening",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_open_directory(path: &Path, file: &File) -> io::Result<()> {
    let file_metadata = file.metadata()?;
    let path_metadata = fs::symlink_metadata(path)?;
    if !file_metadata.file_type().is_dir()
        || !path_metadata.file_type().is_dir()
        || !same_file(&file_metadata, &path_metadata)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quota pace storage changed while opening",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn rollback_quarantine_link(path: &Path, source: &File) {
    if verify_open_regular_file(path, source).is_ok() {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {

    /// Mechanical bridge for fixtures that only need a distinct scope identity.
    fn test_key(
        provider_id: impl Into<String>,
        history_scope: impl AsRef<str>,
        window_key: impl Into<String>,
    ) -> SeriesKey {
        SeriesKey::new(
            provider_id,
            &HistoryScope::for_test(history_scope.as_ref()),
            window_key,
        )
    }
    use super::*;
    use chrono::Utc;
    #[cfg(target_os = "windows")]
    use std::ffi::OsStr;
    #[cfg(target_os = "windows")]
    use std::mem::size_of;
    #[cfg(target_os = "windows")]
    use std::os::windows::fs::{symlink_file, OpenOptionsExt as _};
    #[cfg(target_os = "windows")]
    use std::os::windows::io::AsRawHandle as _;
    #[cfg(target_os = "windows")]
    use std::process::{Child, Command, Stdio};
    #[cfg(target_os = "windows")]
    use std::ptr::{null, null_mut};
    #[cfg(target_os = "windows")]
    use std::time::{Duration, Instant};
    use std::time::{SystemTime, UNIX_EPOCH};
    #[cfg(target_os = "windows")]
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
    #[cfg(target_os = "windows")]
    use windows_sys::Win32::Security::Authorization::{SetSecurityInfo, SE_FILE_OBJECT};
    #[cfg(target_os = "windows")]
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    #[cfg(target_os = "windows")]
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, READ_CONTROL, WRITE_DAC,
    };

    const HOUR: i64 = 3_600;
    const DAY: i64 = 86_400;

    #[cfg(target_os = "windows")]
    const CROSS_PROCESS_WORKER_TEST: &str =
        "agent_quota_history::tests::windows_secure_cross_process_worker";
    #[cfg(target_os = "windows")]
    const CROSS_PROCESS_V3_ENV: &str = "TB_TOKENBAR_TEST_QUOTA_HISTORY_V3_PATH";
    #[cfg(target_os = "windows")]
    const CROSS_PROCESS_ACCOUNT_ENV: &str = "TB_TOKENBAR_TEST_QUOTA_HISTORY_ACCOUNT";
    #[cfg(target_os = "windows")]
    const CROSS_PROCESS_USED_ENV: &str = "TB_TOKENBAR_TEST_QUOTA_HISTORY_USED";
    #[cfg(target_os = "windows")]
    const CROSS_PROCESS_READY_ENV: &str = "TB_TOKENBAR_TEST_QUOTA_HISTORY_READY_PATH";
    #[cfg(target_os = "windows")]
    const CROSS_PROCESS_START_ENV: &str = "TB_TOKENBAR_TEST_QUOTA_HISTORY_START_PATH";
    #[cfg(target_os = "windows")]
    const CROSS_PROCESS_NOW_ENV: &str = "TB_TOKENBAR_TEST_QUOTA_HISTORY_NOW";
    #[cfg(target_os = "windows")]
    const CROSS_PROCESS_RESET_ENV: &str = "TB_TOKENBAR_TEST_QUOTA_HISTORY_RESET_AT";
    #[cfg(target_os = "windows")]
    const CROSS_PROCESS_START_FILE: &str = "tokenbar-test-quota-history-start.marker";

    #[cfg(target_os = "windows")]
    struct WindowsSecureTempCleanup(PathBuf);

    #[cfg(target_os = "windows")]
    impl Drop for WindowsSecureTempCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn key(account: &str) -> SeriesKey {
        test_key("copilot", account, "premium_interactions.v1")
    }

    fn temp_path(label: &str) -> (PathBuf, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "tokenbar-quota-v3-{}-{}-{label}",
            std::process::id(),
            nonce
        ));
        fs::create_dir_all(&directory).unwrap();
        (directory.clone(), directory.join(HISTORY_FILE_NAME))
    }

    #[cfg(target_os = "windows")]
    fn windows_secure_temp_path(label: &str) -> (PathBuf, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "tokenbar-quota-v3-windows-secure-{}-{}-{label}",
            std::process::id(),
            nonce
        ));
        let handle = crate::agent_storage_windows::ensure_secure_storage_directory(&directory)
            .expect("create exact secure history directory");
        crate::agent_storage_windows::verify_storage_handle(handle.as_raw_handle() as HANDLE)
            .expect("history directory DACL is exact");
        drop(handle);
        (directory.clone(), directory.join(HISTORY_FILE_NAME))
    }

    #[cfg(target_os = "windows")]
    fn write_windows_secure_file(path: &Path, bytes: &[u8]) {
        let mut file = crate::agent_storage_windows::create_new_secure_file(path)
            .expect("create exact secure history fixture");
        file.write_all(bytes).expect("write secure history fixture");
        file.flush().expect("flush secure history fixture");
        file.sync_all().expect("sync secure history fixture");
        crate::agent_storage_windows::verify_storage_handle(file.as_raw_handle() as HANDLE)
            .expect("fixture DACL is exact");
        crate::agent_storage_windows::verify_secure_file_path(&file, path)
            .expect("fixture path identity is exact");
    }

    #[cfg(target_os = "windows")]
    fn windows_secure_file_snapshot(path: &Path) -> (Vec<u8>, u64, [u8; 16]) {
        let mut file = crate::agent_storage_windows::open_existing_secure_file(path, false)
            .expect("open exact secure history fixture");
        crate::agent_storage_windows::verify_storage_handle(file.as_raw_handle() as HANDLE)
            .expect("secure file DACL is exact");
        crate::agent_storage_windows::verify_secure_file_path(&file, path)
            .expect("secure file path identity is exact");
        let mut identity = FILE_ID_INFO::default();
        assert_ne!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle() as HANDLE,
                    FileIdInfo,
                    (&mut identity as *mut FILE_ID_INFO).cast(),
                    size_of::<FILE_ID_INFO>() as u32,
                )
            },
            0
        );
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read secure fixture");
        crate::agent_storage_windows::verify_secure_file_path(&file, path)
            .expect("secure file identity is stable after read");
        (
            bytes,
            identity.VolumeSerialNumber,
            identity.FileId.Identifier,
        )
    }

    #[cfg(target_os = "windows")]
    fn windows_file_identity(path: &Path) -> (u64, [u8; 16]) {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .expect("open Windows file identity fixture");
        let mut identity = FILE_ID_INFO::default();
        assert_ne!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle() as HANDLE,
                    FileIdInfo,
                    (&mut identity as *mut FILE_ID_INFO).cast(),
                    size_of::<FILE_ID_INFO>() as u32,
                )
            },
            0
        );
        (identity.VolumeSerialNumber, identity.FileId.Identifier)
    }

    #[cfg(target_os = "windows")]
    fn make_windows_path_permissive(path: &Path, is_directory: bool) {
        let mut options = OpenOptions::new();
        options.access_mode(READ_CONTROL | WRITE_DAC);
        if is_directory {
            options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options
            .open(path)
            .expect("open fixture security descriptor");
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null(),
                null(),
            )
        };
        assert_eq!(status, ERROR_SUCCESS);
    }

    #[cfg(target_os = "windows")]
    fn assert_no_windows_secure_temp(directory: &Path) {
        let prefix = format!(".{HISTORY_FILE_NAME}.tmp-");
        assert!(!fs::read_dir(directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&prefix)
        }));
    }

    #[cfg(target_os = "windows")]
    fn assert_no_windows_corrupt_candidate(directory: &Path) {
        assert!(!fs::read_dir(directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("quota-pace-history-v3.corrupt-")
        }));
    }

    #[cfg(target_os = "windows")]
    fn create_windows_secure_marker(path: &Path) -> io::Result<()> {
        let file = crate::agent_storage_windows::create_new_secure_file(path)?;
        file.sync_all()?;
        crate::agent_storage_windows::verify_storage_handle(file.as_raw_handle() as HANDLE)?;
        crate::agent_storage_windows::verify_secure_file_path(&file, path)?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn windows_secure_marker_exists(path: &Path) -> io::Result<bool> {
        let file = match crate::agent_storage_windows::open_existing_secure_file(path, false) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        crate::agent_storage_windows::verify_storage_handle(file.as_raw_handle() as HANDLE)?;
        crate::agent_storage_windows::verify_secure_file_path(&file, path)?;
        Ok(true)
    }

    #[cfg(target_os = "windows")]
    fn validate_cross_process_child_paths(path: &Path, ready: &Path, start: &Path) {
        let directory = path.parent().expect("cross-process v3 path has a parent");
        let temp = std::env::temp_dir();
        assert!(path.is_absolute());
        assert_eq!(directory.parent(), Some(temp.as_path()));
        assert!(directory
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("tokenbar-quota-v3-windows-secure-")));
        assert_eq!(path.file_name(), Some(OsStr::new(HISTORY_FILE_NAME)));
        assert_eq!(ready.parent(), Some(directory));
        assert_eq!(start.parent(), Some(directory));
        assert!(matches!(
            ready.file_name().and_then(OsStr::to_str),
            Some(
                "tokenbar-test-quota-history-ready-a.marker"
                    | "tokenbar-test-quota-history-ready-b.marker"
            )
        ));
        assert_eq!(
            start.file_name(),
            Some(OsStr::new(CROSS_PROCESS_START_FILE))
        );
        assert_ne!(ready, start);
        assert_ne!(ready, path);
        assert_ne!(start, path);
    }

    #[cfg(target_os = "windows")]
    fn spawn_windows_cross_process_worker(
        path: &Path,
        account: &str,
        used_percent: f64,
        ready: &Path,
        start: &Path,
        now: i64,
        reset_at: i64,
    ) -> io::Result<Child> {
        Command::new(std::env::current_exe()?)
            .arg(CROSS_PROCESS_WORKER_TEST)
            .arg("--exact")
            .arg("--test-threads=1")
            .env(CROSS_PROCESS_V3_ENV, path)
            .env(CROSS_PROCESS_ACCOUNT_ENV, account)
            .env(CROSS_PROCESS_USED_ENV, used_percent.to_string())
            .env(CROSS_PROCESS_READY_ENV, ready)
            .env(CROSS_PROCESS_START_ENV, start)
            .env(CROSS_PROCESS_NOW_ENV, now.to_string())
            .env(CROSS_PROCESS_RESET_ENV, reset_at.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }

    #[cfg(target_os = "windows")]
    fn finish_windows_test_children(
        children: Vec<Child>,
        terminate_running: bool,
    ) -> (bool, String) {
        let mut all_success = true;
        let mut diagnostics = String::new();
        for (index, mut child) in children.into_iter().enumerate() {
            if terminate_running {
                let running = match child.try_wait() {
                    Ok(status) => status.is_none(),
                    Err(error) => {
                        all_success = false;
                        diagnostics.push_str(&format!(
                            "child {index} try_wait before termination failed: {error}\n"
                        ));
                        true
                    }
                };
                if running {
                    if let Err(error) = child.kill() {
                        all_success = false;
                        diagnostics
                            .push_str(&format!("child {index} termination failed: {error}\n"));
                    }
                }
            }
            match child.wait_with_output() {
                Ok(output) => {
                    all_success &= output.status.success();
                    diagnostics.push_str(&format!(
                        "child {index} status={}\nstdout:\n{}\nstderr:\n{}\n",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                Err(error) => {
                    all_success = false;
                    diagnostics.push_str(&format!("child {index} wait failed: {error}\n"));
                }
            }
        }
        (all_success, diagnostics)
    }

    #[cfg(target_os = "windows")]
    fn fail_windows_cross_process(children: Vec<Child>, reason: impl AsRef<str>) -> ! {
        let (_, diagnostics) = finish_windows_test_children(children, true);
        panic!("{}\n{}", reason.as_ref(), diagnostics);
    }

    #[cfg(unix)]
    fn unix_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn provider(reset_at: i64, duration_seconds: i64) -> Option<DurationEvidence> {
        Some(DurationEvidence::provider(reset_at, duration_seconds))
    }

    fn observation(
        key: SeriesKey,
        reset_at: i64,
        used_percent: f64,
        duration_seconds: i64,
    ) -> QuotaObservation {
        QuotaObservation {
            key,
            reset_at: Some(reset_at),
            used_percent,
            provider: provider(reset_at, duration_seconds),
            contract: None,
        }
    }

    fn batch_series(key: SeriesKey, now: i64, active_reset_at: Option<i64>) -> SeriesState {
        let rollover = active_reset_at.map(|reset_at| ObservedState::Watching {
            reset_at,
            first_seen_at: now,
            last_seen_at: now,
            consecutive_count: 1,
        });
        SeriesState {
            provider_id: key.provider_id,
            account_scope: key.account_scope,
            window_key: key.window_key,
            active_reset_at,
            last_activity_at: now,
            rollover,
            samples: complete_cycle(now - DAY, DAY, 60.0),
        }
    }

    fn rollover_only_series(
        key: SeriesKey,
        last_activity_at: i64,
        reset_at: i64,
        candidate: bool,
    ) -> SeriesState {
        let rollover = if candidate {
            ObservedState::Candidate {
                old_reset_at: last_activity_at,
                old_seen_at: last_activity_at - 60,
                new_reset_at: reset_at,
                first_new_seen_at: last_activity_at,
            }
        } else {
            ObservedState::Watching {
                reset_at,
                first_seen_at: last_activity_at,
                last_seen_at: last_activity_at,
                consecutive_count: 1,
            }
        };
        SeriesState {
            provider_id: key.provider_id,
            account_scope: key.account_scope,
            window_key: key.window_key,
            active_reset_at: Some(reset_at),
            last_activity_at,
            rollover: Some(rollover),
            samples: Vec::new(),
        }
    }

    fn record(
        path: &Path,
        account: &str,
        reset_at: Option<i64>,
        used_percent: f64,
        now: i64,
        provider: Option<DurationEvidence>,
        contract: Option<DurationEvidence>,
    ) -> HistoryOutcome {
        record_observation_at_path(
            key(account),
            reset_at,
            used_percent,
            now,
            provider,
            contract,
            path,
        )
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn record_at_lock_time(
        path: &Path,
        account: &str,
        reset_at: Option<i64>,
        used_percent: f64,
        now: i64,
        provider: Option<DurationEvidence>,
        contract: Option<DurationEvidence>,
        lock_time: i64,
    ) -> HistoryOutcome {
        record_observation_at_path_with_clock(
            key(account),
            reset_at,
            used_percent,
            now,
            provider,
            contract,
            path,
            || lock_time,
        )
        .unwrap()
    }

    fn read_store(path: &Path) -> Store {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn quota_sample(
        reset_at: i64,
        duration_seconds: i64,
        phase: f64,
        used_percent: f64,
        origin: SampleOrigin,
    ) -> QuotaSample {
        let sampled_at =
            reset_at - duration_seconds + (phase.clamp(0.0, 1.0) * duration_seconds as f64) as i64;
        QuotaSample {
            reset_at: normalize_sample_reset(reset_at, duration_seconds, sampled_at),
            duration_seconds,
            duration_source: DurationSource::Provider,
            used_percent,
            sampled_at,
            origin,
            plan: None,
        }
    }

    fn complete_cycle(reset_at: i64, duration_seconds: i64, end: f64) -> Vec<QuotaSample> {
        [0.01, 0.10, 0.25, 0.40, 0.60, 0.75, 0.90, 0.99]
            .into_iter()
            .enumerate()
            .map(|(index, phase)| {
                quota_sample(
                    reset_at,
                    duration_seconds,
                    phase,
                    (end * phase).max(0.1) + index as f64 * 0.01,
                    SampleOrigin::LiveV3,
                )
            })
            .collect()
    }

    fn seeded_series(
        provider_id: &str,
        account_scope: &str,
        window_key: &str,
        current_reset: i64,
        duration_seconds: i64,
        cycles: usize,
    ) -> SeriesState {
        let key = test_key(provider_id, account_scope, window_key);
        let mut samples = Vec::new();
        for offset in 1..=cycles {
            samples.extend(complete_cycle(
                current_reset - offset as i64 * duration_seconds,
                duration_seconds,
                80.0,
            ));
        }
        SeriesState {
            provider_id: key.provider_id,
            account_scope: key.account_scope,
            window_key: key.window_key,
            active_reset_at: Some(current_reset),
            last_activity_at: current_reset - duration_seconds / 2,
            rollover: Some(ObservedState::Watching {
                reset_at: current_reset,
                first_seen_at: current_reset - duration_seconds / 2,
                last_seen_at: current_reset - duration_seconds / 2,
                consecutive_count: 1,
            }),
            samples,
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_cross_process_worker() {
        let Some(path) = std::env::var_os(CROSS_PROCESS_V3_ENV).map(PathBuf::from) else {
            return;
        };
        let account = std::env::var(CROSS_PROCESS_ACCOUNT_ENV)
            .expect("cross-process worker account is present");
        let used_percent = std::env::var(CROSS_PROCESS_USED_ENV)
            .expect("cross-process worker usage is present")
            .parse::<f64>()
            .expect("cross-process worker usage is numeric");
        let ready = PathBuf::from(
            std::env::var_os(CROSS_PROCESS_READY_ENV)
                .expect("cross-process worker ready path is present"),
        );
        let start = PathBuf::from(
            std::env::var_os(CROSS_PROCESS_START_ENV)
                .expect("cross-process worker start path is present"),
        );
        let now = std::env::var(CROSS_PROCESS_NOW_ENV)
            .expect("cross-process worker now is present")
            .parse::<i64>()
            .expect("cross-process worker now is numeric");
        let reset_at = std::env::var(CROSS_PROCESS_RESET_ENV)
            .expect("cross-process worker reset is present")
            .parse::<i64>()
            .expect("cross-process worker reset is numeric");

        assert!(account.starts_with("tokenbar-test-cross-process-"));
        assert!(used_percent.is_finite() && (0.0 < used_percent && used_percent <= 100.0));
        assert_eq!(reset_at.checked_sub(now), Some(DAY));
        validate_cross_process_child_paths(&path, &ready, &start);
        create_windows_secure_marker(&ready).expect("create exact secure ready marker");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match windows_secure_marker_exists(&start) {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => panic!("secure start marker verification failed: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for secure start marker"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(matches!(
            record_observation_at_path_and_evaluate_with_clock_and_mode(
                key(&account),
                Some(reset_at),
                used_percent,
                now,
                provider(reset_at, DAY),
                None,
                &path,
                StorageMode::System,
                || now,
            ),
            Ok((HistoryOutcome::Ready { sampled: true, .. }, None))
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_cross_process_lock_preserves_both_samples() {
        let (directory, path) = windows_secure_temp_path("cross-process-lock");
        let cleanup = WindowsSecureTempCleanup(directory.clone());
        let ready_paths = [
            directory.join("tokenbar-test-quota-history-ready-a.marker"),
            directory.join("tokenbar-test-quota-history-ready-b.marker"),
        ];
        let start_path = directory.join(CROSS_PROCESS_START_FILE);
        let workers = [
            ("tokenbar-test-cross-process-a", 17.0),
            ("tokenbar-test-cross-process-b", 43.0),
        ];
        let now = 12_000_000_000_i64;
        let reset_at = now + DAY;
        let mut children = Vec::with_capacity(workers.len());
        for ((account, used_percent), ready) in workers.iter().zip(&ready_paths) {
            match spawn_windows_cross_process_worker(
                &path,
                account,
                *used_percent,
                ready,
                &start_path,
                now,
                reset_at,
            ) {
                Ok(child) => children.push(child),
                Err(error) => fail_windows_cross_process(
                    children,
                    format!("spawn cross-process secure history worker failed: {error}"),
                ),
            }
        }

        let ready_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut all_ready = true;
            for ready in &ready_paths {
                match windows_secure_marker_exists(ready) {
                    Ok(true) => {}
                    Ok(false) => all_ready = false,
                    Err(error) => fail_windows_cross_process(
                        children,
                        format!("secure ready marker verification failed: {error}"),
                    ),
                }
            }
            if all_ready {
                break;
            }

            let mut early_exit = None;
            for (index, child) in children.iter_mut().enumerate() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        early_exit = Some(format!(
                            "child {index} exited before both secure ready markers: {status}"
                        ));
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        early_exit = Some(format!("child {index} try_wait failed: {error}"));
                        break;
                    }
                }
            }
            if let Some(reason) = early_exit {
                fail_windows_cross_process(children, reason);
            }
            if Instant::now() >= ready_deadline {
                fail_windows_cross_process(children, "timed out waiting for secure ready markers");
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        if let Err(error) = create_windows_secure_marker(&start_path) {
            fail_windows_cross_process(
                children,
                format!("create exact secure start marker failed: {error}"),
            );
        }

        let exit_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut all_exited = true;
            let mut child_failure = None;
            for (index, child) in children.iter_mut().enumerate() {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => {}
                    Ok(Some(status)) => {
                        child_failure = Some(format!("child {index} failed: {status}"));
                        break;
                    }
                    Ok(None) => all_exited = false,
                    Err(error) => {
                        child_failure = Some(format!("child {index} try_wait failed: {error}"));
                        break;
                    }
                }
            }
            if let Some(reason) = child_failure {
                fail_windows_cross_process(children, reason);
            }
            if all_exited {
                break;
            }
            if Instant::now() >= exit_deadline {
                fail_windows_cross_process(children, "timed out waiting for history workers");
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let (all_success, diagnostics) = finish_windows_test_children(children, false);
        assert!(all_success, "cross-process workers failed:\n{diagnostics}");

        let final_snapshot = windows_secure_file_snapshot(&path);
        assert_eq!(final_snapshot, windows_secure_file_snapshot(&path));
        let store = serde_json::from_slice::<Store>(&final_snapshot.0).unwrap();
        assert!(validate_store_at(&store, now));
        assert_eq!(store.series.len(), 2);
        for (account, used_percent) in workers {
            let expected_key = key(account);
            let series = store
                .series
                .iter()
                .find(|series| series.key() == expected_key)
                .unwrap();
            assert_eq!(series.samples.len(), 1);
            let sample = &series.samples[0];
            assert_eq!(sample.origin, SampleOrigin::LiveV3);
            assert_eq!(sample.used_percent, used_percent);
            assert_eq!(sample.sampled_at, now);
            assert_eq!(sample.reset_at, reset_at);
        }
        assert_no_windows_secure_temp(&directory);
        assert_no_windows_corrupt_candidate(&directory);
        let lock_path = directory.join(HISTORY_LOCK_FILE_NAME);
        let lock = windows_secure_file_snapshot(&lock_path);
        assert!(lock.0.is_empty());
        assert_eq!(lock, windows_secure_file_snapshot(&lock_path));

        drop(cleanup);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_active_series_capacity_fails_closed_without_replacement() {
        let (directory, path) = windows_secure_temp_path("capacity-fail-closed");
        let cleanup = WindowsSecureTempCleanup(directory.clone());
        let now = 12_100_000_200_i64;
        let reset_at = now + DAY;
        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: (0..MAX_SERIES)
                .map(|index| {
                    rollover_only_series(
                        key(&format!("secure-capacity-active-{index:04}")),
                        now,
                        reset_at,
                        false,
                    )
                })
                .collect(),
        };
        store.series.sort_by(series_order);
        assert!(validate_store_at(&store, now));
        write_windows_secure_file(&path, &serialize_store_canonical(&store).unwrap());
        let before = windows_secure_file_snapshot(&path);

        assert_eq!(
            record_observation_at_path_and_evaluate_with_clock_and_mode(
                key("secure-capacity-new"),
                Some(reset_at),
                25.0,
                now,
                provider(reset_at, DAY),
                None,
                &path,
                StorageMode::System,
                || now,
            ),
            Err(HistoryError::StoreCapacity)
        );

        let after = windows_secure_file_snapshot(&path);
        assert_eq!(after, before);
        let decoded = serde_json::from_slice::<Store>(&after.0).unwrap();
        assert!(validate_store_at(&decoded, now));
        assert_eq!(decoded.series.len(), MAX_SERIES);
        assert!(!decoded
            .series
            .iter()
            .any(|series| series.key() == key("secure-capacity-new")));
        assert_no_windows_secure_temp(&directory);
        assert_no_windows_corrupt_candidate(&directory);
        let lock_path = directory.join(HISTORY_LOCK_FILE_NAME);
        let lock = windows_secure_file_snapshot(&lock_path);
        assert!(lock.0.is_empty());
        assert_eq!(lock, windows_secure_file_snapshot(&lock_path));

        drop(cleanup);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_stale_rollover_eviction_admits_new_series_atomically() {
        let (directory, path) = windows_secure_temp_path("retention-admission");
        let cleanup = WindowsSecureTempCleanup(directory.clone());
        let now = 12_200_000_100_i64;
        let reset_at = now + DAY;
        let stale_key = key("secure-retention-stale");
        let new_key = key("secure-retention-new");
        let mut series = vec![rollover_only_series(
            stale_key.clone(),
            now - 57 * DAY,
            now + 90 * DAY,
            false,
        )];
        series.extend((0..MAX_SERIES - 1).map(|index| {
            rollover_only_series(
                key(&format!("secure-retention-active-{index:04}")),
                now,
                reset_at,
                false,
            )
        }));
        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series,
        };
        store.series.sort_by(series_order);
        assert!(validate_store_at(&store, now));
        write_windows_secure_file(&path, &serialize_store_canonical(&store).unwrap());
        let before = windows_secure_file_snapshot(&path);

        assert!(matches!(
            record_observation_at_path_and_evaluate_with_clock_and_mode(
                new_key.clone(),
                Some(reset_at),
                35.0,
                now,
                provider(reset_at, DAY),
                None,
                &path,
                StorageMode::System,
                || now,
            ),
            Ok((HistoryOutcome::Ready { sampled: true, .. }, None))
        ));

        let after = windows_secure_file_snapshot(&path);
        assert_ne!(after.0, before.0);
        assert_ne!((after.1, after.2), (before.1, before.2));
        assert_eq!(after, windows_secure_file_snapshot(&path));
        let retained = serde_json::from_slice::<Store>(&after.0).unwrap();
        assert!(validate_store_at(&retained, now));
        assert_eq!(retained.series.len(), MAX_SERIES);
        assert!(!retained
            .series
            .iter()
            .any(|series| series.key() == stale_key));
        assert_eq!(
            retained
                .series
                .iter()
                .filter(|series| { series.account_scope.starts_with("secure-retention-active-") })
                .count(),
            MAX_SERIES - 1
        );
        let admitted = retained
            .series
            .iter()
            .find(|series| series.key() == new_key)
            .unwrap();
        assert_eq!(admitted.samples.len(), 1);
        assert_eq!(admitted.samples[0].origin, SampleOrigin::LiveV3);
        assert_eq!(admitted.samples[0].used_percent, 35.0);
        assert_eq!(admitted.samples[0].sampled_at, now);
        assert_eq!(admitted.samples[0].reset_at, reset_at);
        assert_no_windows_secure_temp(&directory);
        assert_no_windows_corrupt_candidate(&directory);
        let lock_path = directory.join(HISTORY_LOCK_FILE_NAME);
        let lock = windows_secure_file_snapshot(&lock_path);
        assert!(lock.0.is_empty());
        assert_eq!(lock, windows_secure_file_snapshot(&lock_path));

        drop(cleanup);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_read_only_transaction_verifies_directory_history_and_lock() {
        let (directory, path) = windows_secure_temp_path("read-only");
        let bytes = serde_json::to_vec_pretty(&Store::default()).unwrap();
        write_windows_secure_file(&path, &bytes);
        let before = windows_secure_file_snapshot(&path);
        let now = 4_000_000_000_i64;

        with_locked_transaction_with_mode(
            StorageMode::System,
            &path,
            now,
            || now,
            |store| {
                assert_eq!(store, &Store::default());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(windows_secure_file_snapshot(&path), before);
        assert_no_windows_secure_temp(&directory);
        assert_no_windows_corrupt_candidate(&directory);
        let directory_handle =
            crate::agent_storage_windows::ensure_secure_storage_directory(&directory).unwrap();
        crate::agent_storage_windows::verify_storage_handle(
            directory_handle.as_raw_handle() as HANDLE
        )
        .unwrap();
        drop(directory_handle);
        sync_directory_with_mode(StorageMode::System, &directory).unwrap();

        let lock_path = directory.join(HISTORY_LOCK_FILE_NAME);
        let first_lock = windows_secure_file_snapshot(&lock_path);
        let second_lock = windows_secure_file_snapshot(&lock_path);
        assert_eq!(first_lock, second_lock);
        assert!(first_lock.0.is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_rejects_permissive_directory_reparse_history_and_permissive_lock() {
        {
            let (directory, path) = windows_secure_temp_path("permissive-directory");
            let bytes = serde_json::to_vec_pretty(&Store::default()).unwrap();
            write_windows_secure_file(&path, &bytes);
            let before = windows_secure_file_snapshot(&path);
            make_windows_path_permissive(&directory, true);

            assert_eq!(
                with_locked_transaction_with_mode(
                    StorageMode::System,
                    &path,
                    4_100_000_000,
                    || 4_100_000_000,
                    |_| Ok(()),
                ),
                Err(HistoryError::StorageUnavailable)
            );
            assert_eq!(windows_secure_file_snapshot(&path), before);
            assert!(
                crate::agent_storage_windows::ensure_secure_storage_directory(&directory).is_err()
            );
            assert!(!directory.join(HISTORY_LOCK_FILE_NAME).exists());
            assert_no_windows_secure_temp(&directory);
            assert_no_windows_corrupt_candidate(&directory);
            fs::remove_dir_all(directory).unwrap();
        }

        {
            let (directory, path) = windows_secure_temp_path("reparse-history");
            let target = directory.join("history-target.json");
            let bytes = serde_json::to_vec_pretty(&Store::default()).unwrap();
            write_windows_secure_file(&target, &bytes);
            let before = windows_secure_file_snapshot(&target);
            symlink_file(&target, &path).unwrap();

            assert_eq!(
                with_locked_transaction_with_mode(
                    StorageMode::System,
                    &path,
                    4_200_000_000,
                    || 4_200_000_000,
                    |_| Ok(()),
                ),
                Err(HistoryError::Read)
            );
            assert_eq!(windows_secure_file_snapshot(&target), before);
            assert!(fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_no_windows_secure_temp(&directory);
            assert_no_windows_corrupt_candidate(&directory);
            fs::remove_dir_all(directory).unwrap();
        }

        {
            let (directory, path) = windows_secure_temp_path("permissive-lock");
            let bytes = serde_json::to_vec_pretty(&Store::default()).unwrap();
            write_windows_secure_file(&path, &bytes);
            let before = windows_secure_file_snapshot(&path);
            let lock_path = directory.join(HISTORY_LOCK_FILE_NAME);
            write_windows_secure_file(&lock_path, b"lock-sentinel");
            make_windows_path_permissive(&lock_path, false);
            let lock_bytes = fs::read(&lock_path).unwrap();

            assert_eq!(
                with_locked_transaction_with_mode(
                    StorageMode::System,
                    &path,
                    4_300_000_000,
                    || 4_300_000_000,
                    |_| Ok(()),
                ),
                Err(HistoryError::LockOpen)
            );
            assert_eq!(windows_secure_file_snapshot(&path), before);
            assert_eq!(fs::read(&lock_path).unwrap(), lock_bytes);
            assert!(
                crate::agent_storage_windows::open_existing_secure_file(&lock_path, false).is_err()
            );
            assert_no_windows_secure_temp(&directory);
            assert_no_windows_corrupt_candidate(&directory);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_first_save_and_atomic_replacement_install_staged_identity() {
        let (directory, path) = windows_secure_temp_path("save-activation");
        let now = 4_400_000_000_i64;
        let reset = now + DAY;

        assert!(matches!(
            record_observation_at_path_and_evaluate_with_clock_and_mode(
                key("secure-save"),
                Some(reset),
                10.0,
                now,
                provider(reset, DAY),
                None,
                &path,
                StorageMode::System,
                || now,
            ),
            Ok((HistoryOutcome::Ready { sampled: true, .. }, None))
        ));
        let first = windows_secure_file_snapshot(&path);
        let first_store = serde_json::from_slice::<Store>(&first.0).unwrap();
        assert_eq!(first_store.schema_version, HISTORY_SCHEMA_VERSION);
        assert_eq!(first_store.series[0].samples.len(), 1);
        let lock = windows_secure_file_snapshot(&directory.join(HISTORY_LOCK_FILE_NAME));
        assert!(lock.0.is_empty());
        assert_no_windows_secure_temp(&directory);

        let later = now + HOUR;
        let observation_key = key("secure-save");
        let observations = [observation(observation_key.clone(), reset, 20.0, DAY)];
        let staged = std::cell::RefCell::new(None);
        let results = record_observations_at_path_and_evaluate_with_clock_and_mode_and_save(
            std::slice::from_ref(&observation_key),
            &observations,
            later,
            &path,
            StorageMode::System,
            || later,
            |path, store| {
                save_store_atomic_windows_secure_with_replace(
                    path,
                    store,
                    |directory_handle, directory, temp, destination| {
                        staged.replace(Some(windows_secure_file_snapshot(temp)));
                        crate::agent_storage_windows::replace_secure_file(
                            directory_handle,
                            directory,
                            temp,
                            destination,
                        )
                    },
                )
            },
        )
        .unwrap();
        assert!(matches!(
            &results[0],
            Ok((HistoryOutcome::Ready { sampled: true, .. }, _, _))
        ));

        let staged = staged.into_inner().unwrap();
        let committed = windows_secure_file_snapshot(&path);
        assert_eq!(committed, staged);
        assert_ne!((committed.1, committed.2), (first.1, first.2));
        assert_ne!(committed.0, first.0);
        let committed_store = serde_json::from_slice::<Store>(&committed.0).unwrap();
        assert_eq!(committed_store.schema_version, HISTORY_SCHEMA_VERSION);
        assert_eq!(committed_store.series[0].samples.len(), 2);
        assert_no_windows_secure_temp(&directory);
        assert_no_windows_corrupt_candidate(&directory);

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_pre_commit_replace_failure_preserves_last_good_and_cleans_staged_temp() {
        let (directory, path) = windows_secure_temp_path("save-pre-commit-failure");
        let now = 4_410_000_000_i64;
        let reset = now + DAY;
        record_observation_at_path_and_evaluate_with_clock_and_mode(
            key("secure-pre-commit"),
            Some(reset),
            10.0,
            now,
            provider(reset, DAY),
            None,
            &path,
            StorageMode::System,
            || now,
        )
        .unwrap();
        let last_good = windows_secure_file_snapshot(&path);

        let later = now + HOUR;
        let observation_key = key("secure-pre-commit");
        let observations = [observation(observation_key.clone(), reset, 20.0, DAY)];
        let staged = std::cell::RefCell::new(None);
        assert_eq!(
            record_observations_at_path_and_evaluate_with_clock_and_mode_and_save(
                std::slice::from_ref(&observation_key),
                &observations,
                later,
                &path,
                StorageMode::System,
                || later,
                |path, store| {
                    save_store_atomic_windows_secure_with_replace(
                        path,
                        store,
                        |_directory_handle, _directory, temp, _destination| {
                            staged.replace(Some(windows_secure_file_snapshot(temp)));
                            Err(io::Error::other("injected pre-commit replace failure"))
                        },
                    )
                },
            ),
            Err(HistoryError::AtomicSave)
        );

        let staged = staged.into_inner().unwrap();
        assert_ne!(staged.0, last_good.0);
        serde_json::from_slice::<Store>(&staged.0).unwrap();
        assert_eq!(windows_secure_file_snapshot(&path), last_good);
        assert_no_windows_secure_temp(&directory);
        assert_no_windows_corrupt_candidate(&directory);

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_post_commit_error_reports_failure_without_false_rollback() {
        let (directory, path) = windows_secure_temp_path("save-post-commit-error");
        let now = 4_420_000_000_i64;
        let reset = now + DAY;
        record_observation_at_path_and_evaluate_with_clock_and_mode(
            key("secure-post-commit"),
            Some(reset),
            10.0,
            now,
            provider(reset, DAY),
            None,
            &path,
            StorageMode::System,
            || now,
        )
        .unwrap();
        let last_good = windows_secure_file_snapshot(&path);

        let later = now + HOUR;
        let observation_key = key("secure-post-commit");
        let observations = [observation(observation_key.clone(), reset, 20.0, DAY)];
        let staged = std::cell::RefCell::new(None);
        assert_eq!(
            record_observations_at_path_and_evaluate_with_clock_and_mode_and_save(
                std::slice::from_ref(&observation_key),
                &observations,
                later,
                &path,
                StorageMode::System,
                || later,
                |path, store| {
                    save_store_atomic_windows_secure_with_replace(
                        path,
                        store,
                        |directory_handle, directory, temp, destination| {
                            staged.replace(Some(windows_secure_file_snapshot(temp)));
                            crate::agent_storage_windows::replace_secure_file(
                                directory_handle,
                                directory,
                                temp,
                                destination,
                            )?;
                            Err(io::Error::other("injected post-commit verification error"))
                        },
                    )
                },
            ),
            Err(HistoryError::AtomicSave)
        );

        let staged = staged.into_inner().unwrap();
        let committed = windows_secure_file_snapshot(&path);
        assert_eq!(committed, staged);
        assert_ne!((committed.1, committed.2), (last_good.1, last_good.2));
        assert_ne!(committed.0, last_good.0);
        let committed_store = serde_json::from_slice::<Store>(&committed.0).unwrap();
        assert_eq!(committed_store.schema_version, HISTORY_SCHEMA_VERSION);
        assert_eq!(committed_store.series[0].samples.len(), 2);
        assert_no_windows_secure_temp(&directory);
        assert_no_windows_corrupt_candidate(&directory);

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_corrupt_history_uses_collision_safe_quarantine_then_rebuilds() {
        let (directory, path) = windows_secure_temp_path("quarantine-activation");
        write_windows_secure_file(&path, b"corrupt-secure-v3");
        let source = windows_secure_file_snapshot(&path);
        let now = 4_500_000_000_i64;
        let collision = directory.join(format!("quota-pace-history-v3.corrupt-{now}.json"));
        write_windows_secure_file(&collision, b"existing-collision");
        let collision_before = windows_secure_file_snapshot(&collision);
        let source_absent_before_save = std::cell::Cell::new(false);
        let observation_key = key("secure-quarantine");
        let observations = [observation(observation_key.clone(), now + DAY, 10.0, DAY)];

        let results = record_observations_at_path_and_evaluate_with_clock_and_mode_and_save(
            std::slice::from_ref(&observation_key),
            &observations,
            now,
            &path,
            StorageMode::System,
            || now,
            |path, store| {
                source_absent_before_save.set(matches!(
                    fs::symlink_metadata(path),
                    Err(error) if error.kind() == io::ErrorKind::NotFound
                ));
                save_store_atomic_windows_secure_with_replace(
                    path,
                    store,
                    crate::agent_storage_windows::replace_secure_file,
                )
            },
        )
        .unwrap();
        assert!(matches!(
            &results[0],
            Ok((HistoryOutcome::Ready { sampled: true, .. }, _, _))
        ));
        assert!(source_absent_before_save.get());

        let quarantined = directory.join(format!("quota-pace-history-v3.corrupt-{now}.1.json"));
        assert_eq!(windows_secure_file_snapshot(&quarantined), source);
        assert_eq!(windows_secure_file_snapshot(&collision), collision_before);
        let rebuilt = windows_secure_file_snapshot(&path);
        assert_ne!((rebuilt.1, rebuilt.2), (source.1, source.2));
        let rebuilt_store = serde_json::from_slice::<Store>(&rebuilt.0).unwrap();
        assert_eq!(rebuilt_store.schema_version, HISTORY_SCHEMA_VERSION);
        assert_eq!(rebuilt_store.series[0].samples.len(), 1);
        assert_no_windows_secure_temp(&directory);

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_secure_v2_valid_import_is_idempotent_and_invalid_inputs_remain_read_only() {
        let now = 6_000_000_000_i64;
        let duration = 10_080 * 60;
        let reset = now - 2 * duration;
        let valid_v2_samples = complete_cycle(reset, duration, 80.0)
            .into_iter()
            .map(|sample| {
                serde_json::json!({
                    "accountKey": "acct",
                    "resetsAt": reset,
                    "windowMinutes": 10080,
                    "usedPercent": sample.used_percent,
                    "sampledAt": sample.sampled_at
                })
            })
            .collect::<Vec<_>>();
        let valid_v2 = serde_json::to_vec_pretty(&serde_json::json!({
            "schemaVersion": 2,
            "samples": valid_v2_samples
        }))
        .unwrap();

        {
            let (directory, v3_path) = windows_secure_temp_path("migration-valid");
            let v2_path = directory.join(LEGACY_V2_FILE_NAME);
            write_windows_secure_file(&v2_path, &valid_v2);
            let v2_before = windows_secure_file_snapshot(&v2_path);
            let v2_mtime = fs::metadata(&v2_path).unwrap().modified().unwrap();

            assert_eq!(
                migrate_codex_v2_at_paths_with_clock_and_mode(
                    "acct",
                    &HistoryScope::for_test("opaque-scope"),
                    now,
                    &v2_path,
                    &v3_path,
                    StorageMode::System,
                    || now,
                ),
                Ok(MigrationOutcome {
                    imported_samples: 8,
                    skipped_samples: 0,
                })
            );
            assert_eq!(windows_secure_file_snapshot(&v2_path), v2_before);
            assert_eq!(
                fs::metadata(&v2_path).unwrap().modified().unwrap(),
                v2_mtime
            );
            let v3_after_first = windows_secure_file_snapshot(&v3_path);
            let migrated = serde_json::from_slice::<Store>(&v3_after_first.0).unwrap();
            assert_eq!(migrated.schema_version, HISTORY_SCHEMA_VERSION);
            assert_eq!(migrated.series.len(), 1);
            assert_eq!(migrated.series[0].account_scope, "opaque-scope");
            assert_eq!(migrated.series[0].samples.len(), 8);
            assert_eq!(
                migrated.series[0].samples[0].origin,
                SampleOrigin::ImportedV2
            );
            let lock = windows_secure_file_snapshot(&directory.join(HISTORY_LOCK_FILE_NAME));
            assert!(lock.0.is_empty());
            assert_no_windows_secure_temp(&directory);
            assert_no_windows_corrupt_candidate(&directory);

            assert_eq!(
                migrate_codex_v2_at_paths_with_clock_and_mode(
                    "acct",
                    &HistoryScope::for_test("opaque-scope"),
                    now,
                    &v2_path,
                    &v3_path,
                    StorageMode::System,
                    || now,
                ),
                Ok(MigrationOutcome {
                    imported_samples: 0,
                    skipped_samples: 0,
                })
            );
            assert_eq!(windows_secure_file_snapshot(&v2_path), v2_before);
            assert_eq!(
                fs::metadata(&v2_path).unwrap().modified().unwrap(),
                v2_mtime
            );
            assert_eq!(windows_secure_file_snapshot(&v3_path), v3_after_first);
            assert_no_windows_secure_temp(&directory);
            assert_no_windows_corrupt_candidate(&directory);
            fs::remove_dir_all(directory).unwrap();
        }

        {
            let (directory, v3_path) = windows_secure_temp_path("migration-permissive-v2");
            let v2_path = directory.join(LEGACY_V2_FILE_NAME);
            write_windows_secure_file(&v2_path, &valid_v2);
            make_windows_path_permissive(&v2_path, false);
            write_windows_secure_file(
                &v3_path,
                &serde_json::to_vec_pretty(&Store::default()).unwrap(),
            );
            let v2_before = fs::read(&v2_path).unwrap();
            let v2_mtime = fs::metadata(&v2_path).unwrap().modified().unwrap();
            let v3_before = windows_secure_file_snapshot(&v3_path);

            assert_eq!(
                migrate_codex_v2_at_paths_with_clock_and_mode(
                    "acct",
                    &HistoryScope::for_test("opaque-scope"),
                    now,
                    &v2_path,
                    &v3_path,
                    StorageMode::System,
                    || now,
                ),
                Ok(MigrationOutcome {
                    imported_samples: 0,
                    skipped_samples: 0,
                })
            );
            assert_eq!(fs::read(&v2_path).unwrap(), v2_before);
            assert_eq!(
                fs::metadata(&v2_path).unwrap().modified().unwrap(),
                v2_mtime
            );
            assert!(
                crate::agent_storage_windows::open_existing_secure_file(&v2_path, false).is_err()
            );
            assert_eq!(windows_secure_file_snapshot(&v3_path), v3_before);
            assert!(!directory.join(HISTORY_LOCK_FILE_NAME).exists());
            assert_no_windows_secure_temp(&directory);
            assert_no_windows_corrupt_candidate(&directory);
            fs::remove_dir_all(directory).unwrap();
        }

        {
            let (directory, v3_path) = windows_secure_temp_path("migration-corrupt-v2");
            let v2_path = directory.join(LEGACY_V2_FILE_NAME);
            write_windows_secure_file(&v2_path, b"corrupt-secure-v2");
            write_windows_secure_file(
                &v3_path,
                &serde_json::to_vec_pretty(&Store::default()).unwrap(),
            );
            let v2_before = windows_secure_file_snapshot(&v2_path);
            let v2_mtime = fs::metadata(&v2_path).unwrap().modified().unwrap();
            let v3_before = windows_secure_file_snapshot(&v3_path);

            assert_eq!(
                migrate_codex_v2_at_paths_with_clock_and_mode(
                    "acct",
                    &HistoryScope::for_test("opaque-scope"),
                    now,
                    &v2_path,
                    &v3_path,
                    StorageMode::System,
                    || now,
                ),
                Ok(MigrationOutcome {
                    imported_samples: 0,
                    skipped_samples: 0,
                })
            );
            assert_eq!(windows_secure_file_snapshot(&v2_path), v2_before);
            assert_eq!(
                fs::metadata(&v2_path).unwrap().modified().unwrap(),
                v2_mtime
            );
            assert_eq!(windows_secure_file_snapshot(&v3_path), v3_before);
            assert!(!directory.join(HISTORY_LOCK_FILE_NAME).exists());
            assert_no_windows_secure_temp(&directory);
            assert_no_windows_corrupt_candidate(&directory);
            fs::remove_dir_all(directory).unwrap();
        }

        {
            let (directory, v3_path) = windows_secure_temp_path("migration-missing-v2");
            let v2_path = directory.join(LEGACY_V2_FILE_NAME);
            write_windows_secure_file(
                &v3_path,
                &serde_json::to_vec_pretty(&Store::default()).unwrap(),
            );
            let v3_before = windows_secure_file_snapshot(&v3_path);

            assert_eq!(
                migrate_codex_v2_at_paths_with_clock_and_mode(
                    "acct",
                    &HistoryScope::for_test("opaque-scope"),
                    now,
                    &v2_path,
                    &v3_path,
                    StorageMode::System,
                    || now,
                ),
                Ok(MigrationOutcome {
                    imported_samples: 0,
                    skipped_samples: 0,
                })
            );
            assert!(!v2_path.exists());
            assert_eq!(windows_secure_file_snapshot(&v3_path), v3_before);
            assert!(!directory.join(HISTORY_LOCK_FILE_NAME).exists());
            assert_no_windows_secure_temp(&directory);
            assert_no_windows_corrupt_candidate(&directory);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_system_history_skips_insecure_legacy_v2_and_writes_live_v3_to_fallback() {
        let (root, _) = temp_path("resolved-secure-root");
        let preferred = root.join("com.nyanako.tokenbar");
        let fallback = root.join("com.nyanako.tokenbar.secure");
        fs::create_dir(&preferred).expect("create inherited legacy preferred root");
        assert!(
            crate::agent_storage_windows::ensure_secure_storage_directory(&preferred).is_err(),
            "legacy preferred root is not exact secure"
        );

        let now = 6_100_000_000_i64;
        let v2_path = preferred.join(LEGACY_V2_FILE_NAME);
        fs::write(
            &v2_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 2,
                "samples": [{
                    "accountKey": "acct",
                    "resetsAt": now + 10_080 * 60,
                    "windowMinutes": 10_080,
                    "usedPercent": 25.0,
                    "sampledAt": now
                }]
            }))
            .unwrap(),
        )
        .expect("write legacy v2 through general filesystem API");
        make_windows_path_permissive(&v2_path, false);
        let v2_bytes = fs::read(&v2_path).unwrap();
        let v2_mtime = fs::metadata(&v2_path).unwrap().modified().unwrap();
        let v2_identity = windows_file_identity(&v2_path);

        let resolved = crate::agent_storage_windows::resolve_secure_storage_directory(&preferred)
            .expect("resolve exact secure fallback");
        assert_eq!(resolved, fallback);
        let v3_path = resolved.join(HISTORY_FILE_NAME);
        assert_eq!(
            migrate_codex_v2_at_paths_with_clock_and_mode(
                "acct",
                &HistoryScope::for_test("opaque-scope"),
                now,
                &v2_path,
                &v3_path,
                StorageMode::System,
                || now,
            ),
            Ok(MigrationOutcome {
                imported_samples: 0,
                skipped_samples: 0,
            })
        );
        assert!(!v3_path.exists());
        assert!(!resolved.join(HISTORY_LOCK_FILE_NAME).exists());
        assert_eq!(fs::read(&v2_path).unwrap(), v2_bytes);
        assert_eq!(
            fs::metadata(&v2_path).unwrap().modified().unwrap(),
            v2_mtime
        );
        assert_eq!(windows_file_identity(&v2_path), v2_identity);
        assert!(
            crate::agent_storage_windows::open_existing_secure_file(&v2_path, false).is_err(),
            "legacy v2 remains insecure and is never repaired in place"
        );

        let reset = now + DAY;
        assert!(matches!(
            record_observation_at_path_and_evaluate_with_clock_and_mode(
                key("fallback-live-v3"),
                Some(reset),
                30.0,
                now,
                provider(reset, DAY),
                None,
                &v3_path,
                StorageMode::System,
                || now,
            ),
            Ok((HistoryOutcome::Ready { sampled: true, .. }, None))
        ));
        let v3 = windows_secure_file_snapshot(&v3_path);
        let store = serde_json::from_slice::<Store>(&v3.0).unwrap();
        assert_eq!(store.schema_version, HISTORY_SCHEMA_VERSION);
        assert_eq!(store.series.len(), 1);
        assert!(
            windows_secure_file_snapshot(&resolved.join(HISTORY_LOCK_FILE_NAME))
                .0
                .is_empty()
        );
        assert_no_windows_secure_temp(&resolved);
        assert_no_windows_corrupt_candidate(&resolved);

        assert_eq!(fs::read(&v2_path).unwrap(), v2_bytes);
        assert_eq!(
            fs::metadata(&v2_path).unwrap().modified().unwrap(),
            v2_mtime
        );
        assert_eq!(windows_file_identity(&v2_path), v2_identity);
        assert!(!preferred.join(HISTORY_FILE_NAME).exists());
        assert!(!preferred.join(HISTORY_LOCK_FILE_NAME).exists());
        assert!(
            crate::agent_storage_windows::ensure_secure_storage_directory(&preferred).is_err(),
            "legacy preferred ACL remains unchanged"
        );

        fs::remove_dir_all(root).unwrap();
    }

    /// Rewrite an on-disk v4 store into the exact shape a v3 build wrote:
    /// version stamp back to 3, `plan` key gone from every sample.
    ///
    /// Derived from a real file rather than hand-written JSON so it cannot
    /// drift out of sync with the rest of the schema, and it asserts the key
    /// was actually there to remove — otherwise a regression that stopped
    /// serializing `plan` would quietly turn this into a v4-loads-v4 test.
    fn downgrade_file_to_v3(path: &Path) -> usize {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value["schemaVersion"], HISTORY_SCHEMA_VERSION);
        value["schemaVersion"] = serde_json::json!(HISTORY_SCHEMA_VERSION_V3);
        let mut samples = 0;
        for series in value["series"].as_array_mut().unwrap() {
            for sample in series["samples"].as_array_mut().unwrap() {
                assert!(
                    sample.as_object_mut().unwrap().remove("plan").is_some(),
                    "the fixture must start from a file that serializes `plan`"
                );
                samples += 1;
            }
        }
        assert!(samples > 0, "an empty fixture makes every later claim vacuous");
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        samples
    }

    /// V1a, V1b and V2 in one function, because the order is load-bearing and
    /// invisible in the code: if the v3 store were quarantined instead of
    /// upgraded, the store would be empty and "every sample's plan is None"
    /// would pass on the empty set. The sample count is asserted first and
    /// carried forward so that cannot happen silently.
    #[test]
    fn a_v3_store_upgrades_in_memory_keeps_every_sample_and_converts_on_disk_only_when_written() {
        let (directory, path) = temp_path("v3-upgrade");
        let now = 1_000_000;
        let reset = now + 7 * DAY;

        // Build a real multi-series, multi-sample store through the shipping
        // write path, then take it back to v3.
        // Offsets are spaced by more than one phase bucket. A 7-day window is
        // split into `phase_bucket_count(7 * DAY)` buckets — 192, since
        // anything over two days takes the `PHASE_BUCKET_MULTIPLE` step — so a
        // bucket is 52.5 minutes and 4h apart is comfortably clear. Samples
        // inside one bucket replace each other, which would leave a fixture far
        // smaller than it reads as; the spacing is deliberately loose enough to
        // survive another change to the bucket count.
        for (account, offset, used) in [
            ("a", 0, 10.0),
            ("a", 4 * HOUR, 20.0),
            ("a", 8 * HOUR, 30.0),
            ("b", 0, 15.0),
            ("b", 4 * HOUR, 25.0),
        ] {
            assert!(matches!(
                record(
                    &path,
                    account,
                    Some(reset),
                    used,
                    now + offset,
                    provider(reset, 7 * DAY),
                    None
                ),
                HistoryOutcome::Ready { sampled: true, .. }
            ));
        }
        let before = read_store(&path);
        let series_count = before.series.len();
        assert_eq!(series_count, 2);
        let sample_count = downgrade_file_to_v3(&path);
        assert_eq!(sample_count, 5);

        // V1a — the upgrade happens on load, loses nothing, and does not
        // quarantine.
        let loaded =
            load_store_at_with_mode(StorageMode::System, &path, now + 12 * HOUR, now + 12 * HOUR)
                .unwrap();
        assert!(!loaded.quarantined, "a v3 store was quarantined instead of upgraded");
        assert_eq!(loaded.store.schema_version, HISTORY_SCHEMA_VERSION);
        assert_eq!(loaded.store.series.len(), series_count);
        assert_eq!(
            loaded
                .store
                .series
                .iter()
                .map(|series| series.samples.len())
                .sum::<usize>(),
            sample_count
        );
        assert_eq!(loaded.store, before, "the upgrade changed something other than the version");

        // V1b — the upgrade is lazy. Loading is not a reason to write, so the
        // file is still stamped 3 and still carries no `plan` key.
        let on_disk: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk["schemaVersion"], HISTORY_SCHEMA_VERSION_V3,
            "loading rewrote the file, so the eager-upgrade risk this slice avoids is present"
        );
        assert!(
            on_disk["series"][0]["samples"][0]
                .as_object()
                .unwrap()
                .get("plan")
                .is_none()
        );

        // V2 — a transaction that had a reason to write converts the file, and
        // every sample it writes, old and new, has no plan.
        assert!(matches!(
            record(
                &path,
                "a",
                Some(reset),
                40.0,
                now + 12 * HOUR,
                provider(reset, 7 * DAY),
                None
            ),
            HistoryOutcome::Ready { sampled: true, .. }
        ));
        let after = read_store(&path);
        assert_eq!(after.schema_version, HISTORY_SCHEMA_VERSION);
        assert_eq!(
            after
                .series
                .iter()
                .map(|series| series.samples.len())
                .sum::<usize>(),
            sample_count + 1
        );
        assert!(
            after
                .series
                .iter()
                .flat_map(|series| series.samples.iter())
                .all(|sample| sample.plan.is_none()),
            "a plan value was written, and nothing in this slice has one to write"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    /// V5. The hermetic fixture above proves the upgrade on a store this test
    /// file built; this one proves it on a store the shipping app actually
    /// wrote, which is the only kind that exists on a user's disk.
    ///
    /// **The operator supplies a copy.** Point
    /// `TOKENBAR_QUOTA_STORE_COPY_SOURCE` at a file you copied out yourself:
    ///
    /// ```text
    /// cp "$HOME/Library/Application Support/com.nyanako.tokenbar/quota-pace-history-v3.json" /tmp/store-copy.json
    /// TOKENBAR_QUOTA_STORE_COPY_SOURCE=/tmp/store-copy.json cargo test -p tb_core_ffi -- --ignored real_store
    /// ```
    ///
    /// A source inside the application-data directory is refused by this
    /// function, not by how it is invoked. `#[ignore]` and an unset variable
    /// are properties of the caller and decay differently from the body:
    /// `cargo test -- --ignored` is a normal thing to run, and a variable
    /// exported once into a shell profile is a normal thing to forget. A check
    /// here cannot be un-set by either.
    ///
    /// Byte-equality at the end would only prove this test did not modify the
    /// source. It proves nothing about a panic between the copy and the
    /// assertion, and nothing at all if the process is killed. Not opening the
    /// live file is the only guarantee that survives those.
    #[test]
    #[ignore = "needs a store copy the operator made; see the doc comment"]
    fn a_real_store_upgrades_without_losing_series_or_samples() {
        let source = std::env::var("TOKENBAR_QUOTA_STORE_COPY_SOURCE")
            .expect("set TOKENBAR_QUOTA_STORE_COPY_SOURCE to a copy you made");
        let source = Path::new(&source);
        // Resolve first: a symlink into the data directory is the same file.
        let resolved = fs::canonicalize(source).expect("resolve the source path");
        if let Some(home) = crate::user_home_dir() {
            let protected = home.join("Library/Application Support");
            assert!(
                !resolved.starts_with(&protected),
                "refusing to open {} — it is inside the application-data directory. \
                 Copy the store out and point the variable at the copy.",
                resolved.display()
            );
        }
        let original = fs::read(&resolved).expect("read the store copy");

        let (directory, path) = temp_path("real-store-copy");
        fs::write(&path, &original).unwrap();

        let raw: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let series_before = raw["series"].as_array().unwrap().len();
        let samples_before: usize = raw["series"]
            .as_array()
            .unwrap()
            .iter()
            .map(|series| series["samples"].as_array().unwrap().len())
            .sum();
        assert!(samples_before > 0, "an empty source proves nothing");

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let loaded = load_store_at_with_mode(StorageMode::System, &path, now, now).unwrap();

        assert!(!loaded.quarantined, "the real store was quarantined");
        assert_eq!(loaded.store.schema_version, HISTORY_SCHEMA_VERSION);
        assert_eq!(loaded.store.series.len(), series_before);
        // Two passes run between deserialization and this assertion: the
        // upgrade, and `drop_unplaceable_samples`. A healthy store should lose
        // nothing to either, so a drop here is worth failing on whichever one
        // caused it — but read the failure carefully before assuming it was
        // the migration.
        assert_eq!(
            loaded
                .store
                .series
                .iter()
                .map(|series| series.samples.len())
                .sum::<usize>(),
            samples_before,
            "a real store lost samples on load — either the migration or \
             `drop_unplaceable_samples` rejected data this build should place"
        );
        assert!(
            loaded
                .store
                .series
                .iter()
                .flat_map(|series| series.samples.iter())
                .all(|sample| sample.plan.is_none()),
            "a plan value appeared on a sample that predates the field"
        );

        assert_eq!(fs::read(&resolved).unwrap(), original, "the source was modified");
        assert_eq!(fs::read(&path).unwrap(), original, "the copy was rewritten on load");

        fs::remove_dir_all(directory).unwrap();
    }

    /// V3. Uses a `Some` value even though nothing writes one yet: with only
    /// `None` in play, a field that was dropped from the wire entirely would
    /// still round-trip.
    #[test]
    fn the_plan_field_round_trips_through_the_wire_format() {
        let sample = quota_sample(2_000_000, 7 * DAY, 0.5, 42.0, SampleOrigin::LiveV3);
        let with_plan = QuotaSample {
            plan: Some("pro".to_string()),
            ..sample.clone()
        };
        for original in [sample, with_plan] {
            let bytes = serde_json::to_vec(&original).unwrap();
            let restored = serde_json::from_slice::<QuotaSample>(&bytes).unwrap();
            assert_eq!(restored, original);
        }
    }

    /// V6. Makes the one-way nature of the conversion falsifiable rather than
    /// asserted in prose: a build that predates `plan` rejects a v4 file, so a
    /// user who downgrades after their file has been rewritten loses the
    /// history to quarantine. That is the cost the lazy upgrade defers — it
    /// does not remove it.
    #[test]
    fn a_v3_reader_cannot_parse_a_v4_store() {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        #[allow(dead_code)]
        struct V3Sample {
            reset_at: i64,
            duration_seconds: i64,
            duration_source: DurationSource,
            used_percent: f64,
            sampled_at: i64,
            origin: SampleOrigin,
        }

        let v4 = serde_json::to_vec(&QuotaSample {
            plan: None,
            ..quota_sample(2_000_000, 7 * DAY, 0.5, 42.0, SampleOrigin::LiveV3)
        })
        .unwrap();
        assert!(
            serde_json::from_slice::<V3Sample>(&v4).is_err(),
            "a v3 reader accepted a v4 sample, so this test proves nothing about the downgrade"
        );
    }

    #[test]
    fn writes_current_schema_sorted_series_and_exact_sample_fields() {
        let (directory, path) = temp_path("schema");
        let now = 1_000_000;
        let reset = now + 7 * DAY;
        assert!(matches!(
            record(&path, "b", Some(reset), 10.0, now, provider(reset, 7 * DAY), None),
            HistoryOutcome::Ready {
                duration_seconds,
                source: DurationSource::Provider,
                sampled: true
            } if duration_seconds == 7 * DAY
        ));
        assert!(matches!(
            record(
                &path,
                "a",
                Some(reset),
                20.0,
                now,
                provider(reset, 7 * DAY),
                None
            ),
            HistoryOutcome::Ready { sampled: true, .. }
        ));
        let store = read_store(&path);
        assert_eq!(store.schema_version, HISTORY_SCHEMA_VERSION);
        assert_eq!(
            store
                .series
                .iter()
                .map(|series| series.account_scope.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(store.series[0].samples[0].duration_seconds, 7 * DAY);
        assert_eq!(store.series[0].samples[0].origin, SampleOrigin::LiveV3);
        assert_eq!(store.series[0].samples[0].sampled_at, now);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn restart_continues_watching_and_candidate_across_transactions() {
        let (directory, path) = temp_path("restart");
        let old_reset = 2_000_000;
        let new_reset = old_reset + 7 * DAY;
        assert_eq!(
            record(
                &path,
                "acct",
                Some(old_reset),
                0.0,
                old_reset - 20 * 60,
                None,
                None
            ),
            HistoryOutcome::LearningDuration
        );
        assert_eq!(
            record(
                &path,
                "acct",
                Some(old_reset),
                0.0,
                old_reset - 5 * 60,
                None,
                None
            ),
            HistoryOutcome::LearningDuration
        );
        assert_eq!(
            record(
                &path,
                "acct",
                Some(new_reset),
                10.0,
                old_reset + 5 * 60,
                None,
                None,
            ),
            HistoryOutcome::LearningDuration
        );
        assert!(matches!(
            record(
                &path,
                "acct",
                Some(new_reset),
                12.0,
                old_reset + 10 * 60,
                None,
                None,
            ),
            HistoryOutcome::Ready {
                duration_seconds,
                source: DurationSource::Observed,
                sampled: true
            } if duration_seconds == 7 * DAY
        ));
        let store = read_store(&path);
        assert_eq!(store.series[0].samples.len(), 1);
        assert_eq!(store.series[0].samples[0].sampled_at, old_reset + 10 * 60);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_history_is_quarantined_byte_for_byte_then_rebuilt() {
        let (directory, path) = temp_path("corrupt");
        let corrupt = b"{not-json\nquota-v3";
        fs::write(&path, corrupt).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let now = 3_000_000;
        let reset = now + 7 * DAY;
        assert!(matches!(
            record(
                &path,
                "acct",
                Some(reset),
                10.0,
                now,
                provider(reset, 7 * DAY),
                None
            ),
            HistoryOutcome::Ready { sampled: true, .. }
        ));
        let quarantined = directory.join(format!("quota-pace-history-v3.corrupt-{now}.json"));
        assert_eq!(fs::read(&quarantined).unwrap(), corrupt);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&quarantined).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let recovered = read_store(&path);
        assert_eq!(recovered.schema_version, HISTORY_SCHEMA_VERSION);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn history_lock_symlink_fails_closed_without_touching_target() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let (directory, path) = temp_path("lock-symlink");
        let target = directory.with_extension("external-lock");
        let lock_path = directory.join(HISTORY_LOCK_FILE_NAME);
        let original = b"external-lock-target";
        fs::write(&target, original).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let original_mode = unix_mode(&target);
        symlink(&target, &lock_path).unwrap();

        let now = 3_100_000;
        let reset = now + DAY;
        assert_eq!(
            record_observation_at_path(
                key("acct"),
                Some(reset),
                10.0,
                now,
                provider(reset, DAY),
                None,
                &path,
            ),
            Err(HistoryError::LockOpen)
        );
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(unix_mode(&target), original_mode);
        assert!(fs::symlink_metadata(&lock_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!path.exists());

        fs::remove_dir_all(directory).unwrap();
        fs::remove_file(target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn active_history_symlink_fails_closed_without_touching_target() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let (directory, path) = temp_path("active-symlink");
        let target = directory.with_extension("external-history");
        let original = b"external-history-target";
        fs::write(&target, original).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let original_mode = unix_mode(&target);
        symlink(&target, &path).unwrap();

        let now = 3_200_000;
        let reset = now + DAY;
        assert_eq!(
            record_observation_at_path(
                key("acct"),
                Some(reset),
                10.0,
                now,
                provider(reset, DAY),
                None,
                &path,
            ),
            Err(HistoryError::Read)
        );
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(unix_mode(&target), original_mode);
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!directory
            .join(format!("quota-pace-history-v3.corrupt-{now}.json"))
            .exists());

        fs::remove_dir_all(directory).unwrap();
        fs::remove_file(target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn history_final_directory_symlink_fails_before_chmod_or_lock_creation() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let (directory, path) = temp_path("directory-symlink");
        fs::remove_dir(&directory).unwrap();
        let target = directory.with_extension("external-directory");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let original_mode = unix_mode(&target);
        symlink(&target, &directory).unwrap();

        let now = 3_300_000;
        let reset = now + DAY;
        assert_eq!(
            record_observation_at_path(
                key("acct"),
                Some(reset),
                10.0,
                now,
                provider(reset, DAY),
                None,
                &path,
            ),
            Err(HistoryError::StorageUnavailable)
        );
        assert_eq!(unix_mode(&target), original_mode);
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert!(fs::symlink_metadata(&directory)
            .unwrap()
            .file_type()
            .is_symlink());

        fs::remove_file(directory).unwrap();
        fs::remove_dir(target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_history_quarantine_hard_link_closes_collision_race() {
        use std::cell::Cell;
        use std::os::unix::fs::{symlink, MetadataExt as _, PermissionsExt as _};

        let (directory, path) = temp_path("quarantine-reservation-race");
        let corrupt = b"history-race-source";
        fs::write(&path, corrupt).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let source_inode = fs::metadata(&path).unwrap().ino();
        let now = 3_350_000;
        let collision = directory.join(format!("quota-pace-history-v3.corrupt-{now}.json"));
        let missing_target = directory.with_extension("race-dangling-target");
        let raced = Cell::new(false);

        let quarantined = quarantine_corrupt_with_ops(
            &path,
            now,
            |source, candidate| {
                if !raced.replace(true) {
                    symlink(&missing_target, candidate)?;
                }
                fs::hard_link(source, candidate)
            },
            |source| fs::remove_file(source),
        )
        .unwrap();

        assert_eq!(
            quarantined,
            directory.join(format!("quota-pace-history-v3.corrupt-{now}.1.json"))
        );
        assert!(fs::symlink_metadata(&collision)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!missing_target.exists());
        assert_eq!(fs::read(&quarantined).unwrap(), corrupt);
        assert_eq!(unix_mode(&quarantined), 0o600);
        assert_eq!(fs::metadata(&quarantined).unwrap().ino(), source_inode);
        assert!(!path.exists());

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dangling_history_quarantine_collision_is_not_overwritten() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let (directory, path) = temp_path("dangling-quarantine");
        let corrupt = b"corrupt-history-with-dangling-collision";
        fs::write(&path, corrupt).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let now = 3_400_000;
        let collision = directory.join(format!("quota-pace-history-v3.corrupt-{now}.json"));
        let missing_target = directory.with_extension("missing-quarantine-target");
        symlink(&missing_target, &collision).unwrap();

        let reset = now + DAY;
        assert!(matches!(
            record_observation_at_path(
                key("acct"),
                Some(reset),
                10.0,
                now,
                provider(reset, DAY),
                None,
                &path,
            ),
            Ok(HistoryOutcome::Ready { sampled: true, .. })
        ));
        assert!(fs::symlink_metadata(&collision)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!missing_target.exists());
        let quarantined = directory.join(format!("quota-pace-history-v3.corrupt-{now}.1.json"));
        assert_eq!(fs::read(&quarantined).unwrap(), corrupt);
        assert_eq!(unix_mode(&quarantined), 0o600);
        assert_eq!(unix_mode(&path), 0o600);

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restored_history_is_tightened_before_read_without_changing_bytes() {
        use std::os::unix::fs::PermissionsExt as _;

        let (directory, path) = temp_path("restored-mode");
        let bytes = serde_json::to_vec_pretty(&Store::default()).unwrap();
        fs::write(&path, &bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let loaded = load_store(&path, 3_500_000).unwrap();
        assert_eq!(loaded.store, Store::default());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(unix_mode(&path), 0o600);

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestor_is_allowed_when_final_history_directory_is_real() {
        use std::os::unix::fs::symlink;

        let (seed, _) = temp_path("ancestor-symlink");
        fs::remove_dir(&seed).unwrap();
        let real_parent = seed.with_extension("real-parent");
        let linked_parent = seed.with_extension("linked-parent");
        let final_directory = linked_parent.join("com.nyanako.tokenbar");
        let path = final_directory.join(HISTORY_FILE_NAME);
        fs::create_dir(&real_parent).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();

        let now = 3_600_000;
        let reset = now + DAY;
        assert!(matches!(
            record_observation_at_path(
                key("acct"),
                Some(reset),
                10.0,
                now,
                provider(reset, DAY),
                None,
                &path,
            ),
            Ok(HistoryOutcome::Ready { sampled: true, .. })
        ));
        assert!(fs::symlink_metadata(&final_directory)
            .unwrap()
            .file_type()
            .is_dir());

        fs::remove_file(linked_parent).unwrap();
        fs::remove_dir_all(real_parent).unwrap();
    }

    #[test]
    fn quarantine_unlink_failure_rolls_back_link_and_preserves_source() {
        let (directory, path) = temp_path("quarantine-failure");
        let corrupt = b"preserve-me";
        fs::write(&path, corrupt).unwrap();
        let result = quarantine_corrupt_with(&path, 123, |_source| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
        #[cfg(unix)]
        assert_eq!(unix_mode(&path), 0o600);
        let candidate = directory.join("quota-pace-history-v3.corrupt-123.json");
        assert!(matches!(
            fs::symlink_metadata(candidate),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_failure_keeps_last_valid_bytes_and_removes_temp() {
        let (directory, path) = temp_path("atomic-failure");
        let now = 4_000_000;
        let reset = now + 7 * DAY;
        let store = Store::default();
        let original = b"last-valid-v3-bytes";
        fs::write(&path, original).unwrap();
        let result = save_store_atomic_with(&path, &store, |_temp, _destination| {
            Err(io::Error::other("injected replace failure"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!directory
            .join(format!(".{HISTORY_FILE_NAME}.tmp-{}-0", std::process::id()))
            .exists());
        let _ = reset;
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn post_replace_directory_sync_failure_keeps_committed_bytes() {
        let (directory, path) = temp_path("post-replace-sync");
        let original = b"old-v3-bytes";
        fs::write(&path, original).unwrap();
        let store = Store::default();
        let result = save_store_atomic_with_sync(
            &path,
            &store,
            tokscale_core::fs_atomic::replace_file,
            |_directory| Err(io::Error::other("injected directory sync failure")),
        );
        assert!(result.is_ok());
        assert_ne!(fs::read(&path).unwrap(), original);
        assert_eq!(
            serde_json::from_slice::<Store>(&fs::read(&path).unwrap()).unwrap(),
            store
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn account_isolation_and_concurrent_transactions_do_not_lose_updates() {
        let (directory, path) = temp_path("concurrency");
        let now = 5_000_000;
        let reset = now + 7 * DAY;
        let left = path.clone();
        let right = path.clone();
        let one = std::thread::spawn(move || {
            record_observation_at_path(
                key("account-a"),
                Some(reset),
                10.0,
                now,
                provider(reset, 7 * DAY),
                None,
                &left,
            )
            .unwrap()
        });
        let two = std::thread::spawn(move || {
            record_observation_at_path(
                key("account-b"),
                Some(reset),
                20.0,
                now,
                provider(reset, 7 * DAY),
                None,
                &right,
            )
            .unwrap()
        });
        assert!(matches!(one.join().unwrap(), HistoryOutcome::Ready { .. }));
        assert!(matches!(two.join().unwrap(), HistoryOutcome::Ready { .. }));
        let store = read_store(&path);
        assert_eq!(store.series.len(), 2);
        assert_eq!(store.series[0].account_scope, "account-a");
        assert_eq!(store.series[1].account_scope, "account-b");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_reset_and_invalid_reading_are_typed_without_touching_store() {
        let (directory, path) = temp_path("invalid");
        let now = 6_000_000;
        assert_eq!(
            record(
                &path,
                "acct",
                None,
                10.0,
                now,
                provider(now + DAY, DAY),
                Some(DurationEvidence::contract(DAY)),
            ),
            HistoryOutcome::Unavailable(DurationUnavailableReason::MissingReset)
        );
        assert!(!path.exists());
        assert_eq!(
            record(&path, "acct", Some(now + DAY), f64::NAN, now, None, None),
            HistoryOutcome::Unavailable(DurationUnavailableReason::InvalidEvidence)
        );
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_duration_evidence_does_not_create_series_or_store() {
        let (directory, path) = temp_path("invalid-duration");
        let now = 6_500_000;
        let reset = now + 7 * DAY;
        assert_eq!(
            record(
                &path,
                "acct",
                Some(reset),
                10.0,
                now,
                Some(DurationEvidence::provider(reset + 1, 5 * HOUR)),
                Some(DurationEvidence::contract(7 * DAY)),
            ),
            HistoryOutcome::Unavailable(DurationUnavailableReason::InvalidEvidence)
        );
        assert!(!path.exists());
        assert!(!directory.join(HISTORY_LOCK_FILE_NAME).exists());

        assert_eq!(
            record(
                &path,
                "acct",
                Some(reset),
                10.0,
                now,
                None,
                Some(DurationEvidence::contract(5 * HOUR)),
            ),
            HistoryOutcome::Unavailable(DurationUnavailableReason::InvalidEvidence)
        );
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn provider_precedes_contract_and_invalid_provider_does_not_fall_through() {
        let (directory, path) = temp_path("precedence");
        let now = 7_000_000;
        let provider_reset = now + 5 * HOUR;
        let result = record(
            &path,
            "acct",
            Some(provider_reset),
            10.0,
            now,
            provider(provider_reset, 5 * HOUR),
            Some(DurationEvidence::contract(7 * DAY)),
        );
        assert_eq!(
            result,
            HistoryOutcome::Ready {
                duration_seconds: 5 * HOUR,
                source: DurationSource::Provider,
                sampled: true,
            }
        );

        let contract_reset = now + 7 * DAY;
        let result = record(
            &path,
            "other",
            Some(contract_reset),
            10.0,
            now,
            Some(DurationEvidence::provider(contract_reset + 1, 5 * HOUR)),
            Some(DurationEvidence::contract(7 * DAY)),
        );
        assert_eq!(
            result,
            HistoryOutcome::Unavailable(DurationUnavailableReason::InvalidEvidence)
        );
        assert_eq!(read_store(&path).series.len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn observed_confirmation_accepts_exact_boundary_and_rejects_timeout() {
        let (directory, path) = temp_path("boundary");
        let old = 8_000_000;
        let new = old + 7 * DAY;
        record(
            &path,
            "exact",
            Some(old),
            0.0,
            old - agent_quota_duration::ROLLOVER_GRACE_SECONDS,
            None,
            None,
        );
        record(&path, "exact", Some(old), 0.0, old - 60, None, None);
        record(&path, "exact", Some(new), 10.0, old, None, None);
        assert!(matches!(
            record(&path, "exact", Some(new), 20.0, old + agent_quota_duration::ROLLOVER_GRACE_SECONDS, None, None),
            HistoryOutcome::Ready {
                duration_seconds,
                source: DurationSource::Observed,
                sampled: true,
            } if duration_seconds == 7 * DAY
        ));

        record(
            &path,
            "timeout",
            Some(old),
            0.0,
            old - agent_quota_duration::ROLLOVER_GRACE_SECONDS,
            None,
            None,
        );
        record(&path, "timeout", Some(old), 0.0, old - 60, None, None);
        record(&path, "timeout", Some(new), 10.0, old, None, None);
        assert_eq!(
            record(
                &path,
                "timeout",
                Some(new),
                20.0,
                old + agent_quota_duration::ROLLOVER_GRACE_SECONDS + 1,
                None,
                None,
            ),
            HistoryOutcome::LearningDuration
        );
        let store = read_store(&path);
        let timeout_series = store
            .series
            .iter()
            .find(|series| series.account_scope == "timeout")
            .unwrap();
        assert!(timeout_series.samples.is_empty());
        assert!(matches!(
            timeout_series.rollover,
            Some(ObservedState::Watching {
                reset_at,
                consecutive_count: 1,
                ..
            }) if reset_at == new
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn observed_duplicate_sliding_backward_and_missed_boundaries_do_not_sample() {
        let (directory, path) = temp_path("edges");
        let old = 8_000_000;
        let new = old + 7 * DAY;
        assert_eq!(
            record(&path, "acct", Some(old), 10.0, old - 20 * 60, None, None),
            HistoryOutcome::LearningDuration
        );
        assert_eq!(
            record(&path, "acct", Some(old), 10.0, old - 5 * 60, None, None),
            HistoryOutcome::LearningDuration
        );
        assert_eq!(
            record(&path, "acct", Some(new), 10.0, old + 5 * 60, None, None),
            HistoryOutcome::LearningDuration
        );
        let duplicate = record(&path, "acct", Some(new), 10.0, old + 6 * 60, None, None);
        assert!(matches!(
            duplicate,
            HistoryOutcome::Ready { sampled: true, .. }
        ));
        let before = read_store(&path).series[0].samples.len();
        let duplicate_again = record(&path, "acct", Some(new), 20.0, old + 7 * 60, None, None);
        assert!(matches!(
            duplicate_again,
            HistoryOutcome::Ready { sampled: true, .. }
        ));
        assert_eq!(read_store(&path).series[0].samples.len(), before);

        let sliding = record(
            &path,
            "acct",
            Some(new + DAY),
            20.0,
            new - 2 * DAY,
            None,
            None,
        );
        assert_eq!(sliding, HistoryOutcome::LearningDuration);
        let backward = record(
            &path,
            "acct",
            Some(new - DAY),
            20.0,
            new - 2 * DAY + HOUR,
            None,
            None,
        );
        assert_eq!(backward, HistoryOutcome::LearningDuration);
        let missed = record(
            &path,
            "acct",
            Some(new + 7 * DAY),
            20.0,
            new + agent_quota_duration::ROLLOVER_GRACE_SECONDS + 1,
            None,
            None,
        );
        assert_eq!(missed, HistoryOutcome::LearningDuration);
        assert_eq!(read_store(&path).series[0].samples.len(), 0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unready_readings_never_become_retroactive_samples() {
        let (directory, path) = temp_path("no-retroactive");
        let old = 9_000_000;
        let new = old + 7 * DAY;
        record(&path, "acct", Some(old), 80.0, old - 20 * 60, None, None);
        record(&path, "acct", Some(old), 90.0, old - 5 * 60, None, None);
        record(&path, "acct", Some(new), 95.0, old + 5 * 60, None, None);
        assert!(read_store(&path).series[0].samples.is_empty());
        record(&path, "acct", Some(new), 100.0, old + 10 * 60, None, None);
        let store = read_store(&path);
        assert_eq!(store.series[0].samples.len(), 1);
        assert_eq!(store.series[0].samples[0].reset_at, new);
        assert_eq!(store.series[0].samples[0].sampled_at, old + 10 * 60);
        fs::remove_dir_all(directory).unwrap();
    }

    /// A fresh window reads 0% used, and the store used to reject it, so no
    /// cycle ever recorded its own start: the first stored sample was always
    /// after some consumption, and the span between the lowest and highest
    /// reading understated every cycle by whatever had been spent before the
    /// app first saw a non-zero number.
    ///
    /// Zero is a reading, not an absence. `enrich_snapshot_with` has already
    /// dropped `PaceState::Unavailable`, an unparseable reset and any
    /// non-finite value before it builds an observation, and its own range
    /// check is inclusive — the store was the only layer disagreeing.
    #[test]
    fn a_fresh_window_records_its_own_zero() {
        let (directory, path) = temp_path("zero-start");
        let reset = 9_000_000 + 7 * DAY;
        // Provider duration evidence, so the series is ready on the first poll
        // rather than spending it learning the window length — which is the
        // production case for every window that reports its own reset.
        let provider = Some(DurationEvidence::provider(reset, 7 * DAY));
        record(&path, "acct", Some(reset), 0.0, reset - 7 * DAY + 60, provider, None);
        let store = read_store(&path);
        assert_eq!(store.series[0].samples.len(), 1);
        assert_eq!(store.series[0].samples[0].used_percent, 0.0);
        // And the cycle's span now starts where the cycle did: a later reading
        // makes the difference the full 30 points, not 30 minus whatever was
        // spent before the first non-zero sample landed.
        record(&path, "acct", Some(reset), 30.0, reset - 3 * DAY, provider, None);
        let grown = read_store(&path);
        let used: Vec<f64> = grown.series[0]
            .samples
            .iter()
            .map(|sample| sample.used_percent)
            .collect();
        assert!(used.contains(&0.0) && used.contains(&30.0));
        fs::remove_dir_all(directory).unwrap();
    }

    /// A fixed bucket count gives long windows coarse buckets: at 48, a 7-day
    /// cycle buckets every 3.5 hours, so readings an hour apart replaced each
    /// other and a freshly reset weekly window held one sample for hours while
    /// its headline moved. The count now follows the duration — one bucket per
    /// hour, floored at 48 so short windows are untouched and capped at 168.
    /// Admitting the zero bought nothing while the in-bucket replacement could
    /// overwrite it: a window that moves a point before its first bucket closes
    /// puts the later reading in the same bucket, every replacement guard
    /// passes, and the cycle starts at a nonzero minimum again — understating
    /// `usedPercent` by exactly the amount the zero was recorded to capture.
    #[test]
    fn a_cycles_zero_survives_a_later_reading_in_its_bucket() {
        let (directory, path) = temp_path("zero-not-replaced");
        let reset = 9_000_000 + 5 * HOUR;
        let provider = Some(DurationEvidence::provider(reset, 5 * HOUR));
        let start = reset - 5 * HOUR;
        record(&path, "acct", Some(reset), 0.0, start + 60, provider, None);
        // Same bucket (5h / 48 = 6.25 minutes), later, and far enough above to
        // clear the one-point replacement threshold.
        record(&path, "acct", Some(reset), 4.0, start + 240, provider, None);
        let store = read_store(&path);
        let used: Vec<f64> = store.series[0]
            .samples
            .iter()
            .map(|sample| sample.used_percent)
            .collect();
        assert_eq!(
            used,
            vec![0.0],
            "the zero is kept, and the later reading in its bucket does not replace it"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn long_windows_bucket_by_the_hour() {
        assert_eq!(phase_bucket_count(5 * HOUR), 48, "short windows unchanged");
        assert_eq!(phase_bucket_count(7 * DAY), 192, "a weekly window is sub-hourly");
        assert_eq!(phase_bucket_count(30 * DAY), 192, "and a monthly one is capped");
        // The invariant that matters more than any of those numbers: every
        // count is a whole multiple of the floor, so each grid NESTS inside the
        // coarser one. A count that is not — 168, which is 3.5x — puts samples
        // that 48 buckets separated into one cell, and `validate_series` reads
        // a duplicate sample key as corruption, which quarantines the store.
        // A resolution change then becomes a history wipe.
        for seconds in [HOUR, 5 * HOUR, DAY, 2 * DAY, 7 * DAY, 30 * DAY, 400 * DAY] {
            let count = phase_bucket_count(seconds);
            assert_eq!(
                count % PHASE_BUCKET_COUNT,
                0,
                "bucket count for {seconds}s must be a whole multiple of the floor"
            );
            assert!(count <= MAX_PHASE_BUCKET_COUNT);
        }

        // The behaviour the count exists for: two readings an hour apart inside
        // a weekly cycle are two samples, not one replacing the other.
        let (directory, path) = temp_path("hourly-buckets");
        let reset = 9_000_000 + 7 * DAY;
        let provider = Some(DurationEvidence::provider(reset, 7 * DAY));
        record(&path, "acct", Some(reset), 10.0, reset - 5 * DAY, provider, None);
        record(
            &path,
            "acct",
            Some(reset),
            12.0,
            // An hour and a minute, not exactly an hour: a bucket boundary
            // falls on every hour here, and `1 - 428400/604800` evaluates to
            // 48.99999999999999 rather than 49, so a sample landing exactly on
            // one is a knife-edge that says nothing about the rule. Real polls
            // are a minute apart and never sit on the boundary.
            reset - 5 * DAY + HOUR + 60,
            provider,
            None,
        );
        let store = read_store(&path);
        assert_eq!(
            store.series[0].samples.len(),
            2,
            "an hour apart is two buckets in a weekly cycle"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    /// A duplicate sample key is one unusable SAMPLE, and quarantine discards
    /// the whole FILE. Changing the bucket resolution once produced three
    /// duplicates out of 2,775 samples and cost 31 series — every window back
    /// to "learning history" — for data that was 99.9% intact.
    #[test]
    fn one_unplaceable_sample_does_not_discard_the_rest() {
        let (directory, path) = temp_path("repair-not-quarantine");
        let reset = 9_000_000 + 7 * DAY;
        let duration = 7 * DAY;
        let mut series = SeriesState::new(&key("acct"), reset - duration);
        // Two readings the CURRENT bucket rule puts in one cell, which is what a
        // resolution change produces from a store written under the old one.
        // 0.100 and 0.101 of a 7-day cycle are ten minutes apart and share one
        // 52.5-minute bucket, so they collide while still having an order —
        // which is what makes "the newer one wins" testable at all.
        series.samples = vec![
            quota_sample(reset, duration, 0.100, 10.0, SampleOrigin::LiveV3),
            quota_sample(reset, duration, 0.101, 11.0, SampleOrigin::LiveV3),
            quota_sample(reset, duration, 0.500, 40.0, SampleOrigin::LiveV3),
        ];
        series.last_activity_at = reset - 60;
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series],
        };
        fs::write(&path, serde_json::to_vec(&store).unwrap()).unwrap();

        let loaded = load_store_at_with_mode(
            StorageMode::System,
            &path,
            reset,
            reset,
        )
        .unwrap();
        assert!(
            !loaded.quarantined,
            "one unplaceable sample must not condemn the file"
        );
        let kept = &loaded.store.series[0].samples;
        assert_eq!(kept.len(), 2, "the collision is dropped, the rest survives");
        assert!(
            kept.iter().any(|sample| sample.used_percent == 40.0),
            "the sample in its own bucket is untouched"
        );
        // The newer reading wins the collision, matching `admit`'s replacement.
        assert!(kept.iter().any(|sample| sample.used_percent == 11.0));
        assert!(!path.with_extension("corrupt").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn copilot_calendar_contract_accepts_exact_month_boundaries() {
        let (directory, path) = temp_path("copilot-calendar");
        let reset = "2024-03-01T00:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap()
            .timestamp();
        let now = reset - HOUR;
        assert_eq!(
            record(
                &path,
                "acct",
                Some(reset),
                10.0,
                now,
                None,
                Some(DurationEvidence::contract(
                    crate::agent_quota_duration::copilot_calendar_duration(reset).unwrap(),
                )),
            ),
            HistoryOutcome::Ready {
                duration_seconds: 29 * DAY,
                source: DurationSource::Contract,
                sampled: true,
            }
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn known_route_can_coexist_with_real_observed_confirmation() {
        let (directory, path) = temp_path("known-and-observed");
        let old = 12_000_000;
        let new = old + 7 * DAY;
        let known_duration = 7 * DAY;
        assert!(matches!(
            record(
                &path,
                "acct",
                Some(old),
                10.0,
                old - 20 * 60,
                provider(old, known_duration),
                None,
            ),
            HistoryOutcome::Ready {
                source: DurationSource::Provider,
                ..
            }
        ));
        assert_eq!(
            record(&path, "acct", Some(old), 0.0, old - 5 * 60, None, None),
            HistoryOutcome::LearningDuration
        );
        assert_eq!(
            record(&path, "acct", Some(new), 0.0, old + 5 * 60, None, None),
            HistoryOutcome::LearningDuration
        );
        assert!(matches!(
            record(&path, "acct", Some(new), 20.0, old + 10 * 60, None, None),
            HistoryOutcome::Ready {
                source: DurationSource::Observed,
                duration_seconds,
                ..
            } if duration_seconds == 7 * DAY
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn history_directory_file_and_lock_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let (directory, path) = temp_path("permissions");
        let now = 10_000_000;
        let reset = now + DAY;
        let _ = record(
            &path,
            "acct",
            Some(reset),
            10.0,
            now,
            provider(reset, DAY),
            None,
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join(HISTORY_LOCK_FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn validate_series_rejects_cross_field_mismatches_and_stale_activity() {
        let key = key("acct");
        let mut series = SeriesState::new(&key, 2_000_000);
        let watching_reset = 2_100_000;
        let watching_last_seen = watching_reset - 60;
        series.rollover = Some(ObservedState::Watching {
            reset_at: watching_reset,
            first_seen_at: watching_reset - 120,
            last_seen_at: watching_last_seen,
            consecutive_count: 2,
        });
        series.active_reset_at = Some(watching_reset + 1);
        series.last_activity_at = watching_last_seen;
        assert!(!validate_series(&series));

        let old_reset = 3_000_000;
        let new_reset = old_reset + DAY;
        let first_new_seen_at = old_reset + 60;
        series.rollover = Some(ObservedState::Candidate {
            old_reset_at: old_reset,
            old_seen_at: old_reset - 60,
            new_reset_at: new_reset,
            first_new_seen_at,
        });
        series.active_reset_at = Some(old_reset);
        series.last_activity_at = first_new_seen_at;
        assert!(!validate_series(&series));
        series.active_reset_at = Some(new_reset);
        series.last_activity_at = first_new_seen_at - 1;
        assert!(!validate_series(&series));

        series.rollover = None;
        series.active_reset_at = Some(new_reset);
        series.last_activity_at = first_new_seen_at;
        series.samples.clear();
        assert!(validate_series(&series));

        let sample_reset = 4_000_000;
        series.active_reset_at = None;
        series.last_activity_at = sample_reset - 1;
        series.samples = vec![QuotaSample {
            reset_at: sample_reset,
            duration_seconds: DAY,
            duration_source: DurationSource::Provider,
            used_percent: 10.0,
            sampled_at: sample_reset,
            origin: SampleOrigin::LiveV3,
            plan: None,
        }];
        assert!(!validate_series(&series));
        series.last_activity_at = sample_reset + 60;
        let retained_activity = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series.clone()],
        };
        assert!(validate_store_at(&retained_activity, sample_reset + 60));
        assert!(!validate_store_at(&retained_activity, sample_reset + 59));
    }

    #[test]
    fn late_ready_history_is_quarantined_before_observed_fallback() {
        let (directory, path) = temp_path("late-ready");
        let now = 14_000_000;
        let reset = now + DAY;
        let late_confirmed = now + agent_quota_duration::ROLLOVER_GRACE_SECONDS + 1;
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![SeriesState {
                provider_id: "copilot".into(),
                account_scope: "acct".into(),
                window_key: "premium_interactions.v1".into(),
                active_reset_at: Some(reset),
                last_activity_at: late_confirmed,
                rollover: Some(ObservedState::Ready {
                    cycle_started_at: now,
                    reset_at: reset,
                    duration_seconds: DAY,
                    confirmed_at: late_confirmed,
                    last_seen_at: late_confirmed,
                }),
                samples: Vec::new(),
            }],
        };
        let bytes = serde_json::to_vec_pretty(&store).unwrap();
        fs::write(&path, &bytes).unwrap();

        assert_eq!(
            record(&path, "acct", Some(reset), 10.0, now, None, None),
            HistoryOutcome::LearningDuration
        );
        assert_eq!(
            fs::read(directory.join(format!("quota-pace-history-v3.corrupt-{now}.json"))).unwrap(),
            bytes
        );
        let recovered = read_store(&path);
        assert_eq!(recovered.series.len(), 1);
        assert_eq!(recovered.series[0].active_reset_at, None);
        assert!(recovered.series[0].samples.is_empty());
        assert!(matches!(
            recovered.series[0].rollover,
            Some(ObservedState::Watching {
                reset_at,
                consecutive_count: 1,
                ..
            }) if reset_at == reset
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn future_last_activity_is_repaired_not_quarantined() {
        // This used to be the pre-repair quarantine fixture: a purely
        // structural false-alarm — a metadata-only clock lead, no future
        // sample, no rollover — that used to discard the sample below along
        // with the rest of the store. The loader now repairs it in place.
        let (directory, path) = temp_path("future-activity");
        let now = 16_000_000;
        let lock_time = now + 1;
        let reset = now + DAY;
        let existing_sample = QuotaSample {
            reset_at: reset,
            duration_seconds: DAY,
            duration_source: DurationSource::Provider,
            used_percent: 10.0,
            sampled_at: now,
            origin: SampleOrigin::LiveV3,
            plan: None,
        };
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![SeriesState {
                provider_id: "copilot".into(),
                account_scope: "acct".into(),
                window_key: "premium_interactions.v1".into(),
                active_reset_at: Some(reset),
                last_activity_at: lock_time + 1,
                rollover: None,
                samples: vec![existing_sample.clone()],
            }],
        };
        let bytes = serde_json::to_vec_pretty(&store).unwrap();
        fs::write(&path, &bytes).unwrap();

        assert_eq!(
            record_at_lock_time(&path, "acct", Some(reset), 10.0, now, None, None, lock_time,),
            HistoryOutcome::LearningDuration
        );
        assert!(
            !directory
                .join(format!("quota-pace-history-v3.corrupt-{now}.json"))
                .exists(),
            "a purely metadata-ahead series must not quarantine the store"
        );
        let recovered = read_store(&path);
        assert_eq!(recovered.series.len(), 1);
        assert_eq!(recovered.series[0].active_reset_at, Some(reset));
        assert_eq!(
            recovered.series[0].samples,
            vec![existing_sample],
            "the pre-existing sample must survive the repair"
        );
        assert!(matches!(
            recovered.series[0].rollover,
            Some(ObservedState::Watching {
                reset_at,
                consecutive_count: 1,
                ..
            }) if reset_at == reset
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stale_caller_after_newer_commit_preserves_history_without_quarantine_or_lost_update() {
        let (directory, path) = temp_path("stale-caller-order");
        let now = 17_000_000;
        let newer_now = now + 10;
        let stale_lock_time = newer_now + 10;
        let reset = now + DAY;

        assert!(matches!(
            record_at_lock_time(
                &path,
                "acct",
                Some(reset),
                10.0,
                newer_now,
                provider(reset, DAY),
                None,
                newer_now,
            ),
            HistoryOutcome::Ready { sampled: true, .. }
        ));
        assert!(matches!(
            record_at_lock_time(
                &path,
                "acct",
                Some(reset),
                20.0,
                now,
                provider(reset, DAY),
                None,
                stale_lock_time,
            ),
            HistoryOutcome::Ready { sampled: false, .. }
        ));

        assert!(!directory
            .join(format!("quota-pace-history-v3.corrupt-{now}.json"))
            .exists());
        let store = read_store(&path);
        assert_eq!(store.series.len(), 1);
        assert_eq!(store.series[0].active_reset_at, Some(reset));
        assert_eq!(store.series[0].last_activity_at, newer_now);
        assert_eq!(store.series[0].samples.len(), 1);
        assert_eq!(store.series[0].samples[0].sampled_at, newer_now);
        assert!(matches!(
            store.series[0].rollover,
            Some(ObservedState::Watching {
                reset_at,
                consecutive_count: 1,
                ..
            }) if reset_at == reset
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    // --- Clock disagreement is repaired per series, not quarantined
    // wholesale. Contract and the four cross-baseline traps that shaped it:
    // docs/knowledge/plans/provider-quota-pace.md, section
    // "Clock disagreement is repaired, not quarantined".
    // section 4 for the design this fixture set proves.

    fn watching_rollover(reset_at: i64, first_seen_at: i64, last_seen_at: i64) -> ObservedState {
        ObservedState::Watching {
            reset_at,
            first_seen_at,
            last_seen_at,
            consecutive_count: 1,
        }
    }

    #[test]
    fn clock_lead_incident_both_series_survive_with_every_sample_and_no_quarantine() {
        let (directory, path) = temp_path("clock-incident");
        let upper_bound = 20_000_000;
        let observation_now = upper_bound;
        let cycle_started_at = upper_bound - 3 * HOUR;
        let duration_seconds = 5 * HOUR;
        let reset = cycle_started_at + duration_seconds;

        let session_samples = vec![
            quota_sample(reset, duration_seconds, 0.10, 12.0, SampleOrigin::LiveV3),
            quota_sample(reset, duration_seconds, 0.30, 30.0, SampleOrigin::LiveV3),
            quota_sample(reset, duration_seconds, 0.55, 55.0, SampleOrigin::LiveV3),
        ];
        let session = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "session.v1".into(),
            active_reset_at: Some(reset),
            last_activity_at: session_samples.iter().map(|s| s.sampled_at).max().unwrap(),
            rollover: None,
            samples: session_samples.clone(),
        };

        let weekly_samples = vec![
            quota_sample(reset, duration_seconds, 0.20, 20.0, SampleOrigin::LiveV3),
            quota_sample(reset, duration_seconds, 0.45, 48.0, SampleOrigin::LiveV3),
        ];
        let lead_activity = upper_bound + 48;
        let weekly = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "weekly.v1".into(),
            active_reset_at: Some(reset),
            last_activity_at: lead_activity,
            rollover: Some(watching_rollover(reset, lead_activity - 120, lead_activity)),
            samples: weekly_samples.clone(),
        };

        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![session.clone(), weekly],
        };
        store.series.sort_by(series_order);
        assert!(!validate_store_at(&store, upper_bound)); // fails before the fix
        let bytes = serde_json::to_vec_pretty(&store).unwrap();
        fs::write(&path, &bytes).unwrap();

        let loaded =
            load_store_at_with_mode(StorageMode::Generic, &path, upper_bound, observation_now)
                .unwrap();
        assert!(!loaded.quarantined);
        assert!(validate_store_at(&loaded.store, upper_bound)); // passes after the fix
        assert!(!directory
            .join(format!(
                "quota-pace-history-v3.corrupt-{observation_now}.json"
            ))
            .exists());

        let recovered_session = loaded
            .store
            .series
            .iter()
            .find(|s| s.window_key == "session.v1")
            .unwrap();
        assert_eq!(
            *recovered_session, session,
            "sibling series must be untouched, field for field"
        );

        let recovered_weekly = loaded
            .store
            .series
            .iter()
            .find(|s| s.window_key == "weekly.v1")
            .unwrap();
        assert_eq!(
            recovered_weekly.samples, weekly_samples,
            "every sample survives"
        );
        assert_eq!(recovered_weekly.samples.len(), 2);
        assert!(recovered_weekly.last_activity_at <= upper_bound);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn future_sample_evidence_drops_only_that_series() {
        let (directory, path) = temp_path("clock-future-sample");
        let upper_bound = 21_000_000;
        let observation_now = upper_bound;
        let reset = upper_bound + DAY;

        let healthy_samples = vec![quota_sample(
            reset,
            2 * DAY,
            0.10,
            10.0,
            SampleOrigin::LiveV3,
        )];
        let healthy = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "session.v1".into(),
            active_reset_at: Some(reset),
            last_activity_at: healthy_samples[0].sampled_at,
            rollover: None,
            samples: healthy_samples.clone(),
        };

        // This sample's own sampled_at leads the ceiling, which forces
        // last_activity_at (>= every sample) to lead it too, per
        // activity_valid — this series is structurally valid but
        // unverifiable, not merely metadata-ahead.
        let tainted_sample = QuotaSample {
            reset_at: reset,
            duration_seconds: 2 * DAY,
            duration_source: DurationSource::Provider,
            used_percent: 40.0,
            sampled_at: upper_bound + 5,
            origin: SampleOrigin::LiveV3,
            plan: None,
        };
        let tainted = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "weekly.v1".into(),
            active_reset_at: Some(reset),
            last_activity_at: tainted_sample.sampled_at,
            rollover: None,
            samples: vec![tainted_sample],
        };

        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![healthy.clone(), tainted],
        };
        store.series.sort_by(series_order);
        assert!(!validate_store_at(&store, upper_bound));
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let loaded =
            load_store_at_with_mode(StorageMode::Generic, &path, upper_bound, observation_now)
                .unwrap();
        assert!(!loaded.quarantined);
        assert!(validate_store_at(&loaded.store, upper_bound));
        assert_eq!(loaded.store.series.len(), 1);
        assert_eq!(loaded.store.series[0], healthy, "sibling is untouched");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rollover_drop_per_variant_keeps_every_sample() {
        let (directory, path) = temp_path("clock-rollover-variants");
        let upper_bound = 22_000_000;
        let observation_now = upper_bound;
        let lead = upper_bound + 30;
        let reset = upper_bound + DAY;

        let sample = || quota_sample(reset, 2 * DAY, 0.10, 10.0, SampleOrigin::LiveV3);

        let watching = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "watching.v1".into(),
            active_reset_at: Some(reset),
            last_activity_at: lead,
            rollover: Some(watching_rollover(reset, lead - 100, lead)),
            samples: vec![sample()],
        };

        // Ready's confirmed_at must land within ROLLOVER_GRACE_SECONDS of its
        // own cycle_started_at (not of `lead`), so this cycle is built with
        // its own well-separated reset rather than reusing `reset`.
        let ready_cycle_started_at = upper_bound - DAY;
        let ready_duration_seconds = 3 * DAY;
        let ready_reset_at = ready_cycle_started_at + ready_duration_seconds;
        let ready = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "ready.v1".into(),
            active_reset_at: Some(ready_reset_at),
            last_activity_at: lead,
            rollover: Some(ObservedState::Ready {
                cycle_started_at: ready_cycle_started_at,
                reset_at: ready_reset_at,
                duration_seconds: ready_duration_seconds,
                confirmed_at: ready_cycle_started_at + 50,
                last_seen_at: lead,
            }),
            samples: vec![sample()],
        };

        // Candidate has no last_seen_at at all; its activity timestamp for
        // activity_valid is first_new_seen_at. Its confirmation window is
        // ROLLOVER_GRACE_SECONDS around old_reset_at, so old_reset_at must
        // sit close to upper_bound for first_new_seen_at (== lead, beyond
        // upper_bound) to still fall inside that window.
        let old_reset = upper_bound - 100;
        let candidate = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "candidate.v1".into(),
            active_reset_at: None,
            last_activity_at: lead,
            rollover: Some(ObservedState::Candidate {
                old_reset_at: old_reset,
                old_seen_at: old_reset - 60,
                new_reset_at: reset,
                first_new_seen_at: lead,
            }),
            samples: vec![sample()],
        };

        for series in [watching, ready, candidate] {
            let store = Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![series.clone()],
            };
            assert!(!validate_store_at(&store, upper_bound));
            fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

            let loaded =
                load_store_at_with_mode(StorageMode::Generic, &path, upper_bound, observation_now)
                    .unwrap();
            assert!(!loaded.quarantined, "{}", series.window_key);
            assert!(
                validate_store_at(&loaded.store, upper_bound),
                "{}",
                series.window_key
            );
            assert_eq!(loaded.store.series.len(), 1, "{}", series.window_key);
            assert!(
                loaded.store.series[0].rollover.is_none(),
                "{} rollover must be dropped",
                series.window_key
            );
            assert_eq!(
                loaded.store.series[0].samples, series.samples,
                "{} keeps every sample",
                series.window_key
            );
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn repaired_series_resumes_recording_in_same_transaction() {
        let (directory, path) = temp_path("clock-resume-same-txn");
        let observation_now = 23_000_000;
        let lock_time = observation_now + 500;
        let upper_bound = lock_time;
        let reset = observation_now + DAY;
        let duration_seconds = 2 * DAY;

        let existing_sample = QuotaSample {
            reset_at: reset,
            duration_seconds,
            duration_source: DurationSource::Provider,
            used_percent: 5.0,
            // Far enough from observation_now to land in a different
            // phase bucket, so the new poll adds a sample rather than
            // updating this one in place.
            sampled_at: observation_now - 50_000,
            origin: SampleOrigin::LiveV3,
            plan: None,
        };
        let series = SeriesState {
            provider_id: "copilot".into(),
            account_scope: "acct".into(),
            window_key: "premium_interactions.v1".into(),
            active_reset_at: Some(reset),
            last_activity_at: upper_bound + 10, // leads the ceiling
            rollover: None,
            samples: vec![existing_sample.clone()],
        };
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series],
        };
        assert!(!validate_store_at(&store, upper_bound));
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let outcome = record_at_lock_time(
            &path,
            "acct",
            Some(reset),
            10.0,
            observation_now,
            provider(reset, duration_seconds),
            None,
            lock_time,
        );
        assert!(
            !matches!(outcome, HistoryOutcome::Ready { sampled: false, .. })
                && !matches!(outcome, HistoryOutcome::Unavailable(_)),
            "the repaired series must accept this poll's observation, got {outcome:?}"
        );
        assert!(!directory
            .join(format!(
                "quota-pace-history-v3.corrupt-{observation_now}.json"
            ))
            .exists());

        let recovered = read_store(&path);
        assert_eq!(recovered.series.len(), 1);
        assert_eq!(
            recovered.series[0].samples.len(),
            2,
            "recovery must not depend on any sibling series changing"
        );
        assert!(recovered.series[0]
            .samples
            .iter()
            .any(|sample| sample.sampled_at == observation_now));
        assert!(recovered.series[0].samples.contains(&existing_sample));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn repair_floor_binds_for_a_surviving_sample() {
        let upper_bound = 100;
        let observation_now = 50;
        let sample = QuotaSample {
            reset_at: 1_000,
            duration_seconds: 1_000,
            duration_source: DurationSource::Provider,
            used_percent: 10.0,
            sampled_at: 80, // strictly between observation_now and upper_bound
            origin: SampleOrigin::LiveV3,
            plan: None,
        };
        let series = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "session.v1".into(),
            active_reset_at: None,
            last_activity_at: upper_bound + 5,
            rollover: None,
            samples: vec![sample],
        };
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series],
        };

        let repaired = repair_store_at(store, upper_bound, observation_now);
        assert_eq!(
            repaired.series[0].last_activity_at, 80,
            "floor must not clamp below the surviving sample"
        );
        assert!(validate_series(&repaired.series[0]));
        assert!(validate_store_at(&repaired, upper_bound));
    }

    #[test]
    fn clock_repair_loader_ignores_future_rollover_boundary() {
        let (directory, path) = temp_path("clock-rollover-boundary");
        let upper_bound = 32_000_000;
        let observation_now = upper_bound - 100;
        let rollover_activity = upper_bound - 50;
        let far_reset = upper_bound + 10 * DAY;
        let trigger = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "weekly.v1".into(),
            active_reset_at: Some(far_reset),
            last_activity_at: upper_bound + 5,
            rollover: Some(watching_rollover(
                far_reset,
                rollover_activity - 10,
                rollover_activity,
            )),
            samples: Vec::new(),
        };
        let sibling = SeriesState {
            provider_id: "copilot".into(),
            account_scope: "acct".into(),
            window_key: "premium_interactions.v1".into(),
            active_reset_at: None,
            last_activity_at: upper_bound - 10,
            rollover: None,
            samples: Vec::new(),
        };
        let sibling_only = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![sibling.clone()],
        };
        assert!(validate_store_at(&sibling_only, upper_bound));

        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![trigger.clone(), sibling.clone()],
        };
        store.series.sort_by(series_order);
        assert!(validate_store(&store));
        assert!(
            !validate_store_at(&store, upper_bound),
            "the trigger's observed activity must force the loader through repair"
        );
        let original_bytes = serde_json::to_vec_pretty(&store).unwrap();
        fs::write(&path, &original_bytes).unwrap();

        reset_save_call_count();
        let loaded =
            load_store_at_with_mode(StorageMode::Generic, &path, upper_bound, observation_now)
                .unwrap();
        assert!(!loaded.quarantined);

        let mut expected_trigger = trigger;
        expected_trigger.last_activity_at = rollover_activity;
        let mut expected = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![expected_trigger, sibling],
        };
        expected.series.sort_by(series_order);
        assert_eq!(
            loaded.store, expected,
            "repair must retain a rollover whose activity is trusted even when its reset boundary is in the future"
        );
        assert!(validate_store_at(&loaded.store, upper_bound));
        assert_eq!(save_call_count(), 0, "loader repair must remain zero-save");
        assert_eq!(
            fs::read(&path).unwrap(),
            original_bytes,
            "loader repair must not rewrite fixture bytes"
        );
        assert!(!directory
            .join(format!(
                "quota-pace-history-v3.corrupt-{observation_now}.json"
            ))
            .exists());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn repair_gate_leaves_a_series_at_or_below_the_ceiling_untouched_and_unsaved() {
        let (directory, path) = temp_path("clock-gate");
        let observation_now = 30_000_000;
        let lock_time = observation_now + 100;
        let upper_bound = lock_time;
        let reset = observation_now + DAY;
        // observation_now < last_activity_at <= upper_bound: a healthy band,
        // e.g. a fresh SeriesState::new() from a faster concurrent writer.
        // No active_reset_at/rollover/samples: retain_series clears a stray
        // active_reset_at whenever both are empty, which would look like an
        // unrelated mutation and defeat the "no save occurs" assertion below.
        let series = SeriesState {
            provider_id: "copilot".into(),
            account_scope: "acct".into(),
            window_key: "premium_interactions.v1".into(),
            active_reset_at: None,
            last_activity_at: observation_now + 50,
            rollover: None,
            samples: Vec::new(),
        };
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series.clone()],
        };
        assert!(
            validate_store_at(&store, upper_bound),
            "this store is already healthy"
        );
        let original_bytes = serde_json::to_vec_pretty(&store).unwrap();
        fs::write(&path, &original_bytes).unwrap();

        // Not a substitute for this fixture: there floor == last_activity_at,
        // so an ungated clamp would be a no-op and this gate's absence would
        // be invisible.
        let outcome = record_at_lock_time(
            &path,
            "acct",
            Some(reset),
            10.0,
            observation_now,
            provider(reset, DAY),
            None,
            lock_time,
        );
        assert!(
            matches!(outcome, HistoryOutcome::Ready { sampled: false, .. })
                || matches!(outcome, HistoryOutcome::LearningDuration),
            "an untouched series is above the body's clock and must reject as stale, got {outcome:?}"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original_bytes,
            "a series at or below the ceiling must not be rewritten"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn clock_repair_loader_preserves_sibling_at_upper_bound() {
        let (directory, path) = temp_path("clock-gate-loader");
        let upper_bound = 31_000_000;
        let observation_now = upper_bound - 100;
        let trigger = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "session.v1".into(),
            active_reset_at: None,
            last_activity_at: upper_bound + 50,
            rollover: None,
            samples: Vec::new(),
        };
        let sibling = SeriesState {
            provider_id: "copilot".into(),
            account_scope: "acct".into(),
            window_key: "premium_interactions.v1".into(),
            active_reset_at: None,
            last_activity_at: upper_bound,
            rollover: None,
            samples: Vec::new(),
        };
        let sibling_only = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![sibling.clone()],
        };
        assert!(validate_store_at(&sibling_only, upper_bound));

        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![trigger.clone(), sibling.clone()],
        };
        store.series.sort_by(series_order);
        assert!(validate_store(&store));
        assert!(
            !validate_store_at(&store, upper_bound),
            "the trigger must force the loader through repair"
        );
        let original_bytes = serde_json::to_vec_pretty(&store).unwrap();
        fs::write(&path, &original_bytes).unwrap();

        reset_save_call_count();
        let loaded =
            load_store_at_with_mode(StorageMode::Generic, &path, upper_bound, observation_now)
                .unwrap();
        assert!(!loaded.quarantined);

        let mut expected_trigger = trigger;
        expected_trigger.last_activity_at = observation_now;
        let mut expected = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![expected_trigger, sibling],
        };
        expected.series.sort_by(series_order);
        assert_eq!(
            loaded.store, expected,
            "repair must not lower a sibling timestamp at the inclusive ceiling"
        );
        assert!(validate_store_at(&loaded.store, upper_bound));
        assert_eq!(save_call_count(), 0, "loader repair must remain zero-save");
        assert_eq!(
            fs::read(&path).unwrap(),
            original_bytes,
            "loader repair must not rewrite fixture bytes"
        );
        assert!(!directory
            .join(format!(
                "quota-pace-history-v3.corrupt-{observation_now}.json"
            ))
            .exists());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn healthy_store_with_far_future_reset_at_is_untouched_no_repair_no_save() {
        let (directory, path) = temp_path("clock-healthy");
        let upper_bound = 24_000_000;
        let observation_now = upper_bound;
        let far_reset = upper_bound + 30 * DAY; // days beyond upper_bound, as normal
                                                // A cycle that started well before upper_bound, so the sample's own
                                                // sampled_at (unlike the far-future reset_at boundary) stays behind
                                                // the ceiling.
        let sample = QuotaSample {
            reset_at: far_reset,
            duration_seconds: 60 * DAY,
            duration_source: DurationSource::Provider,
            used_percent: 8.0,
            sampled_at: upper_bound - 20,
            origin: SampleOrigin::LiveV3,
            plan: None,
        };
        let series = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "weekly.v1".into(),
            active_reset_at: Some(far_reset),
            last_activity_at: upper_bound - 10,
            rollover: Some(watching_rollover(
                far_reset,
                upper_bound - 200,
                upper_bound - 10,
            )),
            samples: vec![sample],
        };
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series.clone()],
        };
        assert!(validate_store_at(&store, upper_bound));
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let loaded =
            load_store_at_with_mode(StorageMode::Generic, &path, upper_bound, observation_now)
                .unwrap();
        assert!(!loaded.quarantined);
        assert_eq!(loaded.store, store, "no repair on an already-healthy store");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn structural_corruption_still_quarantines_under_the_new_loader_branch() {
        let (directory, path) = temp_path("clock-structural");
        let now = 25_000_000;
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION + 1, // structural: wrong schema
            series: Vec::new(),
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let loaded = load_store_at_with_mode(StorageMode::Generic, &path, now, now).unwrap();
        assert!(loaded.quarantined);
        assert_eq!(loaded.store, Store::default());
        assert!(directory
            .join(format!("quota-pace-history-v3.corrupt-{now}.json"))
            .exists());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_json_still_quarantines_under_the_new_loader_branch() {
        let (directory, path) = temp_path("clock-malformed");
        let now = 26_000_000;
        fs::write(&path, b"{not-json-at-all").unwrap();

        let loaded = load_store_at_with_mode(StorageMode::Generic, &path, now, now).unwrap();
        assert!(loaded.quarantined);
        assert_eq!(loaded.store, Store::default());
        assert!(directory
            .join(format!("quota-pace-history-v3.corrupt-{now}.json"))
            .exists());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn repair_is_a_fixed_point() {
        let upper_bound = 27_000_000;
        let observation_now = upper_bound - 1_000;
        let reset = upper_bound + DAY;
        let lead = upper_bound + 60;
        let series = SeriesState {
            provider_id: "claude".into(),
            account_scope: "acct".into(),
            window_key: "weekly.v1".into(),
            active_reset_at: Some(reset),
            last_activity_at: lead,
            rollover: Some(watching_rollover(reset, lead - 100, lead)),
            samples: vec![quota_sample(
                reset,
                2 * DAY,
                0.10,
                20.0,
                SampleOrigin::LiveV3,
            )],
        };
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series],
        };

        let once = repair_store_at(store.clone(), upper_bound, observation_now);
        let twice = repair_store_at(once.clone(), upper_bound, observation_now);
        assert_eq!(once, twice);
    }

    #[test]
    fn stale_caller_does_not_fit_active_samples_from_its_future() {
        let (directory, path) = temp_path("stale-caller-future-fit");
        let duration = DAY;
        let cycle_start = 17_100_000;
        let reset_at = cycle_start + duration;
        let key = test_key("fixture", "stale-scope", "window.v1");
        let mut newest_result = None;
        for phase in [0.10, 0.20, 0.30, 0.40, 0.50, 0.60] {
            let now = cycle_start + (phase * duration as f64) as i64;
            let results = record_observations_at_path_and_evaluate(
                std::slice::from_ref(&key),
                &[observation(key.clone(), reset_at, phase * 80.0, duration)],
                now,
                &path,
            )
            .unwrap();
            newest_result = Some(results[0].as_ref().unwrap().clone());
        }
        let (_, newest_pace, complete_cycles) = newest_result.unwrap();
        assert_eq!(complete_cycles, 0);
        assert!(newest_pace.is_some());

        let stale_now = cycle_start + (0.05 * duration as f64) as i64;
        let results = record_observations_at_path_and_evaluate(
            std::slice::from_ref(&key),
            &[observation(key.clone(), reset_at, 4.0, duration)],
            stale_now,
            &path,
        )
        .unwrap();
        let (outcome, stale_pace, complete_cycles) = results[0].as_ref().unwrap();
        assert!(matches!(
            outcome,
            HistoryOutcome::Ready { sampled: false, .. }
        ));
        assert_eq!(*complete_cycles, 0);
        assert!(stale_pace.is_none());

        let store = read_store(&path);
        assert_eq!(store.series[0].samples.len(), 6);
        assert!(store.series[0]
            .samples
            .iter()
            .all(|sample| sample.sampled_at > stale_now));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stale_previous_cycle_caller_preserves_newer_completed_history() {
        let (directory, path) = temp_path("stale-previous-cycle-retention");
        let duration = DAY;
        let completed_reset = 17_200_000 + duration;
        let active_reset = completed_reset + duration;
        let key = test_key("fixture", "stale-rollover-scope", "window.v1");
        let active_phases = [0.10, 0.20, 0.30, 0.40, 0.50, 0.60];
        let mut series = current_series(&key, active_reset, duration, &active_phases, 80.0);
        let completed = complete_cycle(completed_reset, duration, 80.0);
        let completed_sample_count = completed.len();
        series.samples.extend(completed);
        series.samples.sort_by(sample_order);
        let newer_last_activity = series.last_activity_at;
        fs::write(
            &path,
            serde_json::to_vec_pretty(&Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![series],
            })
            .unwrap(),
        )
        .unwrap();

        let stale_now = completed_reset - duration / 20;
        let results = record_observations_at_path_and_evaluate(
            std::slice::from_ref(&key),
            &[observation(key.clone(), completed_reset, 76.0, duration)],
            stale_now,
            &path,
        )
        .unwrap();
        let (outcome, pace, _) = results[0].as_ref().unwrap();
        assert!(matches!(
            outcome,
            HistoryOutcome::Ready { sampled: false, .. }
        ));
        assert!(pace.is_none());

        let store = read_store(&path);
        assert_eq!(store.series[0].last_activity_at, newer_last_activity);
        assert_eq!(
            store.series[0]
                .samples
                .iter()
                .filter(|sample| {
                    normalize_reset(sample.reset_at, sample.duration_seconds)
                        == normalize_reset(completed_reset, duration)
                })
                .count(),
            completed_sample_count
        );
        assert_eq!(
            store.series[0]
                .samples
                .iter()
                .filter(|sample| is_active_group_sample(active_reset, sample))
                .count(),
            active_phases.len()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stale_older_reset_after_newer_commit_preserves_state_and_rejects_observed_duration() {
        let (directory, path) = temp_path("stale-older-reset");
        let now = 18_000_000;
        let old_reset = now + DAY;
        let new_reset = old_reset + 7 * DAY;

        assert_eq!(
            record_at_lock_time(
                &path,
                "acct",
                Some(new_reset),
                10.0,
                now,
                provider(new_reset, 8 * DAY),
                None,
                now,
            ),
            HistoryOutcome::Ready {
                duration_seconds: 8 * DAY,
                source: DurationSource::Provider,
                sampled: true,
            }
        );
        assert_eq!(
            record_at_lock_time(
                &path,
                "acct",
                Some(old_reset),
                20.0,
                now,
                provider(old_reset, DAY),
                None,
                now + 10,
            ),
            HistoryOutcome::Ready {
                duration_seconds: DAY,
                source: DurationSource::Provider,
                sampled: false,
            }
        );
        assert_eq!(
            record_at_lock_time(
                &path,
                "acct",
                Some(old_reset),
                20.0,
                now,
                None,
                None,
                now + 10,
            ),
            HistoryOutcome::LearningDuration
        );

        assert!(!directory
            .join(format!("quota-pace-history-v3.corrupt-{now}.json"))
            .exists());
        let store = read_store(&path);
        assert_eq!(store.series.len(), 1);
        assert_eq!(store.series[0].active_reset_at, Some(new_reset));
        assert_eq!(store.series[0].last_activity_at, now);
        assert_eq!(store.series[0].samples.len(), 1);
        assert_eq!(store.series[0].samples[0].reset_at, new_reset);
        assert_eq!(store.series[0].samples[0].duration_seconds, 8 * DAY);
        assert_eq!(store.series[0].samples[0].sampled_at, now);
        assert!(matches!(
            store.series[0].rollover,
            Some(ObservedState::Watching {
                reset_at,
                consecutive_count: 1,
                ..
            }) if reset_at == new_reset
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn later_known_backward_reset_invalidates_duration_without_sampling() {
        let now = 20_000_000;
        let old_reset = now + 2 * DAY;
        let new_reset = old_reset + 7 * DAY;
        let later = now + HOUR;
        let cases = [
            (
                "provider",
                Some(DurationEvidence::provider(new_reset, 9 * DAY)),
                None,
                Some(DurationEvidence::provider(old_reset, 2 * DAY)),
                None,
                DurationSource::Provider,
            ),
            (
                "contract",
                None,
                Some(DurationEvidence::contract(9 * DAY)),
                None,
                Some(DurationEvidence::contract(2 * DAY)),
                DurationSource::Contract,
            ),
        ];

        for (label, new_provider, new_contract, old_provider, old_contract, old_source) in cases {
            let (directory, path) = temp_path(label);
            assert!(matches!(
                record_at_lock_time(
                    &path,
                    "acct",
                    Some(new_reset),
                    10.0,
                    now,
                    new_provider,
                    new_contract,
                    now,
                ),
                HistoryOutcome::Ready { sampled: true, .. }
            ));
            assert_eq!(
                record_at_lock_time(
                    &path,
                    "acct",
                    Some(old_reset),
                    20.0,
                    later,
                    old_provider,
                    old_contract,
                    later + 10,
                ),
                HistoryOutcome::LearningDuration
            );

            let after_backward = read_store(&path);
            assert_eq!(after_backward.series[0].active_reset_at, None);
            assert_eq!(after_backward.series[0].last_activity_at, later);
            assert!(after_backward.series[0].samples.is_empty());
            assert!(matches!(
                after_backward.series[0].rollover,
                Some(ObservedState::Watching {
                    reset_at,
                    consecutive_count: 1,
                    ..
                }) if reset_at == old_reset
            ));

            let repeat = later + HOUR;
            assert_eq!(
                record_at_lock_time(
                    &path,
                    "acct",
                    Some(old_reset),
                    30.0,
                    repeat,
                    old_provider,
                    old_contract,
                    repeat + 10,
                ),
                HistoryOutcome::Ready {
                    duration_seconds: 2 * DAY,
                    source: old_source,
                    sampled: true,
                }
            );
            let after_known_repeat = read_store(&path);
            assert_eq!(
                after_known_repeat.series[0].active_reset_at,
                Some(old_reset)
            );
            assert_eq!(after_known_repeat.series[0].last_activity_at, repeat);
            assert_eq!(after_known_repeat.series[0].samples.len(), 1);
            assert_eq!(
                after_known_repeat.series[0].samples[0].reset_at,
                normalize_sample_reset(old_reset, 2 * DAY, repeat)
            );
            assert!(matches!(
                after_known_repeat.series[0].rollover,
                Some(ObservedState::Watching {
                    reset_at,
                    consecutive_count: 2,
                    ..
                }) if reset_at == old_reset
            ));

            assert_eq!(
                record_at_lock_time(
                    &path,
                    "acct",
                    Some(old_reset),
                    40.0,
                    repeat + HOUR,
                    None,
                    None,
                    repeat + HOUR + 10,
                ),
                HistoryOutcome::LearningDuration
            );
            let after_repeat = read_store(&path);
            assert_eq!(after_repeat.series[0].active_reset_at, Some(old_reset));
            assert_eq!(after_repeat.series[0].last_activity_at, repeat + HOUR);
            assert_eq!(after_repeat.series[0].samples.len(), 1);
            assert_eq!(
                after_repeat.series[0].samples[0].reset_at,
                normalize_sample_reset(old_reset, 2 * DAY, repeat)
            );
            assert!(matches!(
                after_repeat.series[0].rollover,
                Some(ObservedState::Watching {
                    reset_at,
                    consecutive_count: 2,
                    ..
                }) if reset_at == old_reset
            ));
            assert!(!directory
                .join(format!("quota-pace-history-v3.corrupt-{later}.json"))
                .exists());
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn known_reset_jitter_stays_in_normalized_cycle_and_keeps_partial_history() {
        let (directory, path) = temp_path("known-reset-jitter");
        let now = 1_000_000;
        let duration = DAY;
        let reset = now + duration;
        let jittered_reset = reset - 1;

        assert_eq!(
            record(
                &path,
                "acct",
                Some(reset),
                10.0,
                now,
                provider(reset, duration),
                None,
            ),
            HistoryOutcome::Ready {
                duration_seconds: duration,
                source: DurationSource::Provider,
                sampled: true,
            }
        );
        let before = read_store(&path);
        let before_series = &before.series[0];
        assert_eq!(before_series.samples.len(), 1);
        assert_eq!(
            normalize_reset(before_series.active_reset_at.unwrap(), duration),
            before_series.samples[0].reset_at
        );

        assert_eq!(
            record(
                &path,
                "acct",
                Some(jittered_reset),
                10.5,
                now + 60,
                provider(jittered_reset, duration),
                None,
            ),
            HistoryOutcome::Ready {
                duration_seconds: duration,
                source: DurationSource::Provider,
                sampled: false,
            }
        );
        let after = read_store(&path);
        let after_series = &after.series[0];
        assert_eq!(after_series.samples.len(), 1);
        assert_eq!(after_series.samples[0].used_percent, 10.0);
        assert_eq!(
            normalize_reset(after_series.active_reset_at.unwrap(), duration),
            after_series.samples[0].reset_at
        );
        assert_eq!(
            after_series.samples[0].reset_at,
            normalize_reset(reset, duration)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn later_backward_observation_restarts_learning_after_newer_commit() {
        let (directory, path) = temp_path("later-backward");
        let now = 19_000_000;
        let old_reset = now + 2 * DAY;
        let new_reset = old_reset + 7 * DAY;
        let later = now + HOUR;

        assert!(matches!(
            record_at_lock_time(
                &path,
                "acct",
                Some(new_reset),
                10.0,
                now,
                provider(new_reset, 9 * DAY),
                None,
                now,
            ),
            HistoryOutcome::Ready { sampled: true, .. }
        ));
        assert_eq!(
            record_at_lock_time(
                &path,
                "acct",
                Some(old_reset),
                20.0,
                later,
                None,
                None,
                later + 10,
            ),
            HistoryOutcome::LearningDuration
        );

        let store = read_store(&path);
        assert_eq!(store.series[0].active_reset_at, None);
        assert_eq!(store.series[0].last_activity_at, later);
        assert!(store.series[0].samples.is_empty());
        assert!(matches!(
            store.series[0].rollover,
            Some(ObservedState::Watching {
                reset_at,
                consecutive_count: 1,
                ..
            }) if reset_at == old_reset
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ready_history_requires_matching_active_reset() {
        let now = 15_000_000;
        let reset = now + DAY;
        let confirmed_at = now + 10 * 60;
        for (label, active_reset_at) in [
            ("ready-active-mismatch", Some(reset + 1)),
            ("ready-active-none", None),
        ] {
            let (directory, path) = temp_path(label);
            let store = Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![SeriesState {
                    provider_id: "copilot".into(),
                    account_scope: "acct".into(),
                    window_key: "premium_interactions.v1".into(),
                    active_reset_at,
                    last_activity_at: confirmed_at,
                    rollover: Some(ObservedState::Ready {
                        cycle_started_at: now,
                        reset_at: reset,
                        duration_seconds: DAY,
                        confirmed_at,
                        last_seen_at: confirmed_at,
                    }),
                    samples: Vec::new(),
                }],
            };
            let bytes = serde_json::to_vec_pretty(&store).unwrap();
            fs::write(&path, &bytes).unwrap();

            assert_eq!(
                record(&path, "acct", Some(reset), 10.0, now, None, None),
                HistoryOutcome::LearningDuration
            );
            assert_eq!(
                fs::read(directory.join(format!("quota-pace-history-v3.corrupt-{now}.json")))
                    .unwrap(),
                bytes
            );
            let recovered = read_store(&path);
            assert_eq!(recovered.series[0].active_reset_at, None);
            assert!(recovered.series[0].samples.is_empty());
            assert!(matches!(
                recovered.series[0].rollover,
                Some(ObservedState::Watching {
                    reset_at,
                    consecutive_count: 1,
                    ..
                }) if reset_at == reset
            ));
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn validate_sample_requires_current_cycle_bounds() {
        let reset = 13_000_000;
        let sample = |sampled_at| QuotaSample {
            reset_at: reset,
            duration_seconds: DAY,
            duration_source: DurationSource::Provider,
            used_percent: 10.0,
            sampled_at,
            origin: SampleOrigin::LiveV3,
            plan: None,
        };
        assert!(validate_sample(&sample(reset - DAY)));
        assert!(validate_sample(&sample(reset - 1)));
        assert!(validate_sample(&sample(reset)));
        assert!(!validate_sample(&sample(reset - DAY - 1)));
        assert!(!validate_sample(&sample(reset + 1)));
    }

    #[test]
    fn invalid_series_key_never_creates_store() {
        let (directory, path) = temp_path("key");
        let invalid = test_key("provider", "", "window.v1");
        let result = record_observation_at_path(
            invalid,
            Some(10_000 + DAY),
            10.0,
            10_000,
            None,
            None,
            &path,
        );
        assert_eq!(result, Err(HistoryError::InvalidSeriesKey));
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn known_observation_does_not_fake_observed_ready() {
        let (directory, path) = temp_path("known-then-observed");
        let now = 11_000_000;
        let reset = now + 5 * HOUR;
        assert!(matches!(
            record(
                &path,
                "acct",
                Some(reset),
                10.0,
                now,
                provider(reset, 5 * HOUR),
                None
            ),
            HistoryOutcome::Ready { sampled: true, .. }
        ));
        assert_eq!(
            record(&path, "acct", Some(reset), 20.0, now + HOUR, None, None),
            HistoryOutcome::LearningDuration
        );
        let store = read_store(&path);
        assert_eq!(store.series[0].samples.len(), 1);
        assert_eq!(store.series[0].samples[0].duration_seconds, 5 * HOUR);
        assert_eq!(store.series[0].active_reset_at, Some(reset));
        assert!(matches!(
            store.series[0].rollover,
            Some(ObservedState::Watching {
                reset_at,
                consecutive_count: 2,
                ..
            }) if reset_at == reset
        ));

        let changed_reset = reset + DAY;
        assert_eq!(
            record(
                &path,
                "acct",
                Some(changed_reset),
                20.0,
                now + HOUR,
                None,
                None,
            ),
            HistoryOutcome::LearningDuration
        );
        assert_eq!(read_store(&path).series[0].active_reset_at, None);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn batch_result_reports_complete_cycles_after_retention() {
        let (directory, path) = temp_path("batch-complete-cycles");
        let now = 8_000_000_000_i64;
        let duration = DAY;
        let current_reset = now + duration;
        let key = test_key("fixture", "scope", "window.v1");
        let mut series = seeded_series(
            &key.provider_id,
            &key.account_scope,
            &key.window_key,
            current_reset,
            duration,
            35,
        );
        series.last_activity_at = now;
        series.rollover = Some(ObservedState::Watching {
            reset_at: current_reset,
            first_seen_at: now,
            last_seen_at: now,
            consecutive_count: 1,
        });
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series],
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let results = record_observations_at_path_and_evaluate(
            std::slice::from_ref(&key),
            &[observation(key.clone(), current_reset, 40.0, duration)],
            now,
            &path,
        )
        .unwrap();
        let (_, _, complete_cycles) = results[0].as_ref().unwrap();
        assert_eq!(*complete_cycles, retention_limits(duration).0);

        let retained = read_store(&path);
        let retained_cycles = historical_cycles(
            &retained.series[0],
            normalize_reset(current_reset, duration),
            now,
        );
        assert_eq!(retained_cycles.len(), *complete_cycles);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn generic_evaluator_has_phase_invariant_expected_and_exact_confidence_gates() {
        let durations = [5 * HOUR, 7 * DAY, 28 * DAY, 29 * DAY, 30 * DAY, 31 * DAY];
        for duration in durations {
            let now = 1_000_000_000;
            let current_reset = now + duration;
            let key = test_key("fixture", "opaque", "quota.v1");
            let expected_cycles = if duration < DAY { 6 } else { 3 };
            let mature_cycles = if duration < DAY { 36 } else { 5 };
            let mut series = seeded_series(
                &key.provider_id,
                &key.account_scope,
                &key.window_key,
                current_reset,
                duration,
                mature_cycles,
            );
            let store = Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![series.clone()],
            };
            let three_store = Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![{
                    series.samples.truncate(expected_cycles * 8);
                    series
                }],
            };
            let expected = evaluate_current(&three_store, &key, current_reset, duration, 45.0, now)
                .unwrap_or_else(|| {
                    panic!("three complete cycles pass expected gate for {duration}")
                });
            assert!(expected.expected_percent.is_finite());
            assert!(expected.run_out_probability.is_none());

            let mature = evaluate_current(&store, &key, current_reset, duration, 45.0, now)
                .expect("five complete cycles pass expected and risk gates");
            assert!(mature
                .run_out_probability
                .is_none_or(|probability| (0.0..=1.0).contains(&probability)));
        }
    }

    #[test]
    fn evaluator_partial_coverage_gap_and_exact_half_tie_are_fail_closed() {
        assert_eq!(weighted_median(&[10.0, 20.0], &[1.0, 1.0]), 10.0);
        let reset = 2_000_000;
        let duration = 7 * DAY;
        let mut samples = complete_cycle(reset, duration, 80.0);
        samples.truncate(5);
        assert!(cycle_profile(reset, &samples, reset + 1).is_none());

        let gapped = [0.01, 0.10, 0.20, 0.30, 0.40, 0.99]
            .into_iter()
            .map(|phase| {
                quota_sample(
                    reset,
                    duration,
                    phase,
                    phase * 80.0 + 1.0,
                    SampleOrigin::LiveV3,
                )
            })
            .collect::<Vec<_>>();
        assert!(cycle_profile(reset, &gapped, reset + 1).is_none());

        let middle_only = [0.30, 0.40, 0.50, 0.60, 0.70, 0.80]
            .into_iter()
            .map(|phase| {
                quota_sample(
                    reset,
                    duration,
                    phase,
                    phase * 80.0 + 1.0,
                    SampleOrigin::LiveV3,
                )
            })
            .collect::<Vec<_>>();
        assert!(cycle_profile(reset, &middle_only, reset + 1).is_none());
    }

    #[test]
    fn retention_keeps_latest_cycle_horizon_and_current_partial_only() {
        let duration = 7 * DAY;
        let now = 3_000_000_000;
        let current_reset = now + duration;
        let mut series = seeded_series(
            "provider",
            "scope",
            "window.v1",
            current_reset,
            duration,
            12,
        );
        let current = complete_cycle(current_reset, duration, 70.0)
            .into_iter()
            .take(2)
            .collect::<Vec<_>>();
        let other_partial = quota_sample(
            current_reset + duration,
            duration,
            0.5,
            35.0,
            SampleOrigin::LiveV3,
        );
        let stale_reset = current_reset - 20 * duration;
        series.samples.extend(current.clone());
        series.samples.push(other_partial.clone());
        series
            .samples
            .extend(complete_cycle(stale_reset, duration, 70.0));
        retain_series(&mut series, now);
        let groups = grouped_samples(&series.samples);
        assert_eq!(groups.len(), RETENTION_MIN_CYCLES + 1);
        assert_eq!(
            series.samples.len(),
            RETENTION_MIN_CYCLES * 8 + current.len()
        );
        assert!(current.iter().all(|sample| series.samples.contains(sample)));
        assert!(!series.samples.contains(&other_partial));
    }

    #[test]
    fn deterministic_series_eviction_uses_activity_then_full_key_order() {
        let now = 4_000_000_000;
        let mut store = Store::default();
        for index in 0..=MAX_SERIES {
            store.series.push(SeriesState {
                provider_id: "provider".into(),
                account_scope: format!("scope-{index:04}"),
                window_key: "window.v1".into(),
                active_reset_at: None,
                last_activity_at: now,
                rollover: None,
                samples: vec![QuotaSample {
                    reset_at: now - DAY,
                    duration_seconds: DAY,
                    duration_source: DurationSource::Provider,
                    used_percent: 10.0,
                    sampled_at: now - DAY,
                    origin: SampleOrigin::LiveV3,
                    plan: None,
                }],
            });
        }
        store.series.sort_by(series_order);
        let active = BTreeSet::from([test_key("provider", "scope-active", "window.v1")]);
        evict_inactive_series(&mut store, &active, now).unwrap();
        assert_eq!(store.series.len(), MAX_SERIES);
        assert!(!store
            .series
            .iter()
            .any(|series| series.account_scope == "scope-0000"));
        assert!(store
            .series
            .iter()
            .any(|series| series.account_scope == format!("scope-{MAX_SERIES:04}")));
    }

    #[test]
    fn active_capacity_overflow_rolls_back_without_replacing_last_valid_store() {
        let (directory, path) = temp_path("capacity-rollback");
        let now = 5_000_000_000;
        let reset = now + DAY;
        let mut store = Store::default();
        for index in 0..MAX_SERIES {
            store.series.push(SeriesState {
                provider_id: "copilot".into(),
                account_scope: format!("active-{index:04}"),
                window_key: "premium_interactions.v1".into(),
                active_reset_at: Some(reset),
                last_activity_at: now,
                rollover: Some(ObservedState::Watching {
                    reset_at: reset,
                    first_seen_at: now,
                    last_seen_at: now,
                    consecutive_count: 1,
                }),
                samples: Vec::new(),
            });
        }
        store.series.sort_by(series_order);
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
        let before = fs::read(&path).unwrap();
        let result = record_observation_at_path(
            key("new-active"),
            Some(reset),
            10.0,
            now,
            provider(reset, DAY),
            None,
            &path,
        );
        assert_eq!(result, Err(HistoryError::StoreCapacity));
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn batch_emitted_existing_without_observation_stays_active() {
        let (directory, path) = temp_path("batch-emitted-active");
        let now = 5_250_000_000_i64;
        let emitted = test_key("provider", "scope-0000", "window.v1");
        let mut store = Store::default();
        for index in 0..=MAX_SERIES {
            store.series.push(batch_series(
                test_key("provider", format!("scope-{index:04}"), "window.v1"),
                now,
                None,
            ));
        }
        store.series.sort_by(series_order);
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let results = record_observations_at_path_and_evaluate(
            std::slice::from_ref(&emitted),
            &[],
            now,
            &path,
        )
        .unwrap();
        assert!(results.is_empty());
        let retained = read_store(&path);
        assert_eq!(retained.series.len(), MAX_SERIES);
        assert!(retained.series.iter().any(|series| series.key() == emitted));
        assert!(!retained
            .series
            .iter()
            .any(|series| series.account_scope == "scope-0001"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn batch_observation_key_protects_existing_history_without_explicit_emission() {
        let (directory, path) = temp_path("batch-observation-active");
        let now = 5_275_000_000_i64;
        let existing = test_key("provider", "scope-0000", "window.v1");
        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![batch_series(existing.clone(), now, None)],
        };
        for index in 1..=MAX_SERIES {
            store.series.push(batch_series(
                test_key("provider", format!("scope-{index:04}"), "window.v1"),
                now,
                None,
            ));
        }
        store.series.sort_by(series_order);
        let before = store
            .series
            .iter()
            .find(|series| series.key() == existing)
            .unwrap()
            .samples
            .clone();
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let results = record_observations_at_path_and_evaluate(
            &[],
            &[observation(existing.clone(), now + DAY, 80.0, DAY)],
            now,
            &path,
        )
        .unwrap();
        assert!(results[0].is_ok());

        let retained = read_store(&path);
        let existing = retained
            .series
            .iter()
            .find(|series| series.key() == existing)
            .unwrap();
        assert_eq!(retained.series.len(), MAX_SERIES);
        assert_eq!(existing.samples.len(), before.len() + 1);
        assert!(existing.samples[..before.len()]
            .iter()
            .zip(before.iter())
            .all(|(actual, expected)| actual == expected));
        assert!(!retained
            .series
            .iter()
            .any(|series| series.account_scope == "scope-0001"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stale_rollover_only_series_releases_capacity_after_fifty_six_days() {
        let now = 5_290_000_000_i64;
        let future_reset = now + 90 * DAY;
        let idle = now - 57 * DAY;
        for (label, candidate) in [("watching", false), ("candidate", true)] {
            let (directory, path) = temp_path(&format!("stale-rollover-{label}"));
            let stale_key = test_key("provider", format!("stale-{label}"), "window.v1");
            let mut store = Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![rollover_only_series(
                    stale_key.clone(),
                    idle,
                    future_reset,
                    candidate,
                )],
            };
            for index in 0..MAX_SERIES - 1 {
                store.series.push(batch_series(
                    test_key("provider", format!("active-{index:04}"), "window.v1"),
                    now,
                    Some(now + DAY),
                ));
            }
            store.series.sort_by(series_order);
            fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

            let new_key = test_key("provider", format!("new-{label}"), "window.v1");
            let results = record_observations_at_path_and_evaluate(
                &[],
                &[observation(new_key.clone(), now + DAY, 10.0, DAY)],
                now,
                &path,
            )
            .unwrap();
            assert!(results[0].is_ok());
            let retained = read_store(&path);
            assert_eq!(retained.series.len(), MAX_SERIES);
            assert!(!retained
                .series
                .iter()
                .any(|series| series.key() == stale_key));
            assert!(retained.series.iter().any(|series| series.key() == new_key));
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn rollover_only_retention_keeps_fifty_five_day_idle_and_emitted_stale_key_for_poll() {
        let now = 5_300_000_000_i64;
        let future_reset = now + 90 * DAY;
        let (directory, path) = temp_path("rollover-boundary-55d");
        let key = test_key("provider", "boundary-55d", "window.v1");
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![rollover_only_series(
                key.clone(),
                now - 55 * DAY,
                future_reset,
                false,
            )],
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
        record_observations_at_path_and_evaluate(&[], &[], now, &path).unwrap();
        let retained = read_store(&path);
        assert_eq!(retained.series.len(), 1);
        assert!(retained.series[0].rollover.is_some());
        fs::remove_dir_all(directory).unwrap();

        for (label, candidate) in [("watching", false), ("candidate", true)] {
            let (directory, path) = temp_path(&format!("rollover-boundary-{label}"));
            let key = test_key("provider", format!("boundary-{label}"), "window.v1");
            let stale_store = Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![rollover_only_series(
                    key.clone(),
                    now - 57 * DAY,
                    future_reset,
                    candidate,
                )],
            };
            fs::write(&path, serde_json::to_vec_pretty(&stale_store).unwrap()).unwrap();
            record_observations_at_path_and_evaluate(std::slice::from_ref(&key), &[], now, &path)
                .unwrap();
            let protected = read_store(&path);
            assert_eq!(protected.series.len(), 1);
            assert_eq!(protected.series[0].key(), key);
            assert!(protected.series[0].samples.is_empty());
            assert!(protected.series[0].rollover.is_none());
            assert_eq!(protected.series[0].active_reset_at, None);

            record_observations_at_path_and_evaluate(&[], &[], now, &path).unwrap();
            assert!(read_store(&path).series.is_empty());
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn batch_existing_future_active_series_precedes_new_candidate() {
        let (directory, path) = temp_path("batch-existing-active");
        let now = 5_300_000_000_i64;
        let active = test_key("provider", "active", "window.v1");
        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![batch_series(active.clone(), now, Some(now + DAY))],
        };
        for index in 0..MAX_SERIES - 1 {
            store.series.push(batch_series(
                test_key("provider", format!("inactive-{index:04}"), "window.v1"),
                now,
                None,
            ));
        }
        store.series.sort_by(series_order);
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
        let new_key = test_key("provider", "new", "window.v1");
        let results = record_observations_at_path_and_evaluate(
            &[],
            &[observation(new_key.clone(), now + DAY, 20.0, DAY)],
            now,
            &path,
        )
        .unwrap();
        assert!(results[0].is_ok());
        let retained = read_store(&path);
        assert_eq!(retained.series.len(), MAX_SERIES);
        assert!(retained.series.iter().any(|series| series.key() == active));
        assert!(retained.series.iter().any(|series| series.key() == new_key));
        assert!(!retained
            .series
            .iter()
            .any(|series| series.account_scope == "inactive-0000"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn batch_new_candidates_admit_by_key_not_input_order() {
        let now = 5_350_000_000_i64;
        let active_keys = (0..MAX_SERIES - 2)
            .map(|index| test_key("provider", format!("active-{index:04}"), "window.v1"))
            .collect::<Vec<_>>();
        let base = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: active_keys
                .iter()
                .cloned()
                .map(|key| batch_series(key, now, Some(now + DAY)))
                .collect(),
        };
        let new_a = test_key("provider", "new-a", "window.v1");
        let new_b = test_key("provider", "new-b", "window.v1");
        let new_c = test_key("provider", "new-c", "window.v1");
        let cases = [
            (
                "batch-admission-reversed",
                vec![
                    observation(new_c.clone(), now + DAY, 30.0, DAY),
                    observation(new_b.clone(), now + DAY, 20.0, DAY),
                    observation(new_a.clone(), now + DAY, 10.0, DAY),
                ],
            ),
            (
                "batch-admission-sorted",
                vec![
                    observation(new_a.clone(), now + DAY, 10.0, DAY),
                    observation(new_b.clone(), now + DAY, 20.0, DAY),
                    observation(new_c.clone(), now + DAY, 30.0, DAY),
                ],
            ),
        ];
        let mut persisted_keys = Vec::new();
        for (label, observations) in cases {
            let (directory, path) = temp_path(label);
            fs::write(&path, serde_json::to_vec_pretty(&base).unwrap()).unwrap();
            let results =
                record_observations_at_path_and_evaluate(&active_keys, &observations, now, &path)
                    .unwrap();
            if observations[0].key == new_c {
                assert!(matches!(&results[0], Err(HistoryError::StoreCapacity)));
                assert!(results[1].is_ok());
                assert!(results[2].is_ok());
            } else {
                assert!(results[0].is_ok());
                assert!(results[1].is_ok());
                assert!(matches!(&results[2], Err(HistoryError::StoreCapacity)));
            }
            let store = read_store(&path);
            assert_eq!(store.series.len(), MAX_SERIES);
            assert!(store.series.iter().any(|series| series.key() == new_a));
            assert!(store.series.iter().any(|series| series.key() == new_b));
            assert!(!store.series.iter().any(|series| series.key() == new_c));
            persisted_keys.push(
                store
                    .series
                    .iter()
                    .map(SeriesState::key)
                    .collect::<Vec<_>>(),
            );
            fs::remove_dir_all(directory).unwrap();
        }
        assert_eq!(persisted_keys[0], persisted_keys[1]);
    }

    #[test]
    fn batch_save_failure_preserves_pre_transaction_bytes() {
        let (directory, path) = temp_path("batch-save-failure");
        let now = 5_400_000_000_i64;
        let existing = batch_series(test_key("provider", "existing", "window.v1"), now, None);
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![existing],
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
        let before = fs::read(&path).unwrap();
        let observations = vec![
            observation(
                test_key("provider", "batch-a", "window.v1"),
                now + DAY,
                10.0,
                DAY,
            ),
            observation(
                test_key("provider", "batch-b", "window.v1"),
                now + DAY,
                20.0,
                DAY,
            ),
        ];
        let result = record_observations_at_path_and_evaluate_with_clock_and_mode_and_save(
            &[],
            &observations,
            now,
            &path,
            StorageMode::Generic,
            || now,
            |_path, _store| Err(io::Error::other("injected batch save failure")),
        );
        assert!(matches!(result, Err(HistoryError::AtomicSave)));
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn batch_duplicate_observation_key_fails_closed_without_mutation() {
        let (directory, path) = temp_path("batch-duplicate");
        let now = 5_450_000_000_i64;
        let store = Store::default();
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
        let before = fs::read(&path).unwrap();
        let item = observation(
            test_key("provider", "duplicate", "window.v1"),
            now + DAY,
            10.0,
            DAY,
        );
        let result =
            record_observations_at_path_and_evaluate(&[], &[item.clone(), item], now, &path);
        assert_eq!(result, Err(HistoryError::InvalidSeriesKey));
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn same_phase_bucket_requires_one_point_usage_delta() {
        let now = 5_500_000_000;
        let duration = DAY;
        let reset = now + duration;
        let mut series = SeriesState::new(&key("sample-throttle"), now);
        assert!(add_sample_if_new(
            &mut series,
            reset,
            duration,
            DurationSource::Provider,
            10.0,
            now,
        ));
        assert!(!add_sample_if_new(
            &mut series,
            reset,
            duration,
            DurationSource::Provider,
            10.9,
            now + 60,
        ));
        assert!(add_sample_if_new(
            &mut series,
            reset,
            duration,
            DurationSource::Provider,
            11.0,
            now + 60,
        ));
        assert!(!add_sample_if_new(
            &mut series,
            reset,
            duration,
            DurationSource::Provider,
            11.9,
            now + 60,
        ));
        assert!(add_sample_if_new(
            &mut series,
            reset,
            duration,
            DurationSource::Provider,
            12.0,
            now + 60,
        ));
        assert_eq!(series.samples.len(), 1);
        assert_eq!(series.samples[0].used_percent, 12.0);
    }

    #[test]
    fn codex_v2_migration_is_account_bound_idempotent_and_read_only() {
        let (directory, v3_path) = temp_path("migration");
        let v2_path = directory.join(LEGACY_V2_FILE_NAME);
        let now = 6_000_000_000;
        let duration = 10_080 * 60;
        let reset = now - 2 * duration;
        let sampled = reset - duration / 2;
        let mut legacy = serde_json::json!({
            "schemaVersion": 2,
            "samples": [
                {"accountKey": " acct ", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 40.0, "sampledAt": sampled},
                {"accountKey": "acct", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 30.0, "sampledAt": reset - duration + duration / 10},
                {"accountKey": "acct", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 35.0, "sampledAt": reset - duration + duration / 4},
                {"accountKey": "acct", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 45.0, "sampledAt": reset - duration + duration * 2 / 5},
                {"accountKey": "acct", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 55.0, "sampledAt": reset - duration + duration * 3 / 5},
                {"accountKey": "acct", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 65.0, "sampledAt": reset - duration + duration * 3 / 4},
                {"accountKey": "acct", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 72.0, "sampledAt": reset - duration + duration * 9 / 10},
                {"accountKey": "acct", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 78.0, "sampledAt": reset - duration + duration * 99 / 100},
                {"accountKey": "other", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 90.0, "sampledAt": sampled},
                {"accountKey": "acct@example.com", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 90.0, "sampledAt": sampled},
                {"accountKey": "acct", "resetsAt": reset, "windowMinutes": 300, "usedPercent": 20.0, "sampledAt": sampled},
                {"accountKey": "acct", "resetsAt": reset, "windowMinutes": 10080, "usedPercent": 0.0, "sampledAt": sampled}
            ]
        });
        fs::write(&v2_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        let v2_before = fs::read(&v2_path).unwrap();
        let v2_mtime = fs::metadata(&v2_path).unwrap().modified().unwrap();

        let live = quota_sample(reset, duration, 0.5, 99.0, SampleOrigin::LiveV3);
        let existing = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![SeriesState {
                provider_id: "codex".into(),
                account_scope: "opaque-scope".into(),
                window_key: "main.weekly.v1".into(),
                active_reset_at: None,
                last_activity_at: sampled,
                rollover: None,
                samples: vec![live],
            }],
        };
        fs::write(&v3_path, serde_json::to_vec_pretty(&existing).unwrap()).unwrap();

        let first = migrate_codex_v2_at_paths(
            "acct",
            &HistoryScope::for_test("opaque-scope"),
            now,
            &v2_path,
            &v3_path,
        )
        .unwrap();
        assert_eq!(first.imported_samples, 7);
        assert_eq!(fs::read(&v2_path).unwrap(), v2_before);
        assert_eq!(
            fs::metadata(&v2_path).unwrap().modified().unwrap(),
            v2_mtime
        );
        let migrated = read_store(&v3_path);
        assert_eq!(migrated.series.len(), 1);
        assert_eq!(
            migrated.series[0].last_activity_at,
            reset - duration + duration * 99 / 100
        );
        assert!(migrated.series[0]
            .samples
            .iter()
            .any(|sample| sample.origin == SampleOrigin::LiveV3 && sample.used_percent == 99.0));
        assert!(
            migrated.series[0]
                .samples
                .iter()
                .any(|sample| sample.origin == SampleOrigin::ImportedV2
                    && sample.used_percent == 30.0)
        );

        let bytes_after_first = fs::read(&v3_path).unwrap();
        let second = migrate_codex_v2_at_paths(
            "acct",
            &HistoryScope::for_test("opaque-scope"),
            now,
            &v2_path,
            &v3_path,
        )
        .unwrap();
        assert_eq!(second.imported_samples, 0);
        assert_eq!(fs::read(&v3_path).unwrap(), bytes_after_first);

        legacy["samples"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "accountKey": "acct",
                "resetsAt": reset,
                "windowMinutes": 10080,
                "usedPercent": 50.0,
                "sampledAt": reset - duration + duration * 55 / 100
            }));
        fs::write(&v2_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        let third = migrate_codex_v2_at_paths(
            "acct",
            &HistoryScope::for_test("opaque-scope"),
            now,
            &v2_path,
            &v3_path,
        )
        .unwrap();
        assert_eq!(third.imported_samples, 1);
        assert_ne!(fs::read(&v3_path).unwrap(), bytes_after_first);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_collision_losers_do_not_advance_activity() {
        let (directory, v3_path) = temp_path("migration-collision-loser");
        let v2_path = directory.join(LEGACY_V2_FILE_NAME);
        let now = 6_500_000_000_i64;
        let duration = 10_080 * 60;
        let reset = now - 2 * duration;
        let live = quota_sample(reset, duration, 0.5, 99.0, SampleOrigin::LiveV3);
        let candidate_sampled_at = live.sampled_at + 60;
        fs::write(
            &v2_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 2,
                "samples": [{
                    "accountKey": "acct",
                    "resetsAt": reset,
                    "windowMinutes": 10080,
                    "usedPercent": 20.0,
                    "sampledAt": candidate_sampled_at
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let existing = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![SeriesState {
                provider_id: "codex".into(),
                account_scope: "scope".into(),
                window_key: "main.weekly.v1".into(),
                active_reset_at: None,
                last_activity_at: live.sampled_at,
                rollover: None,
                samples: vec![live.clone()],
            }],
        };
        fs::write(&v3_path, serde_json::to_vec_pretty(&existing).unwrap()).unwrap();
        let before = fs::read(&v3_path).unwrap();
        let outcome = migrate_codex_v2_at_paths(
            "acct",
            &HistoryScope::for_test("scope"),
            now,
            &v2_path,
            &v3_path,
        )
        .unwrap();
        assert_eq!(outcome.imported_samples, 0);
        assert_eq!(
            read_store(&v3_path).series[0].last_activity_at,
            live.sampled_at
        );
        assert_eq!(fs::read(&v3_path).unwrap(), before);
        assert!(!directory
            .join(format!("quota-pace-history-v3.corrupt-{now}.json"))
            .exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_stale_caller_uses_post_lock_validation_clock() {
        let (directory, v3_path) = temp_path("migration-stale-caller");
        let v2_path = directory.join(LEGACY_V2_FILE_NAME);
        let committed_now = 7_500_000_000_i64;
        let stale_now = committed_now - 60;
        let duration = 10_080 * 60;
        let reset = committed_now - 2 * duration;
        let imported = complete_cycle(reset, duration, 80.0);
        let legacy_samples = imported
            .iter()
            .map(|sample| {
                serde_json::json!({
                    "accountKey": "acct",
                    "resetsAt": reset,
                    "windowMinutes": 10080,
                    "usedPercent": sample.used_percent,
                    "sampledAt": sample.sampled_at
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            &v2_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 2,
                "samples": legacy_samples
            }))
            .unwrap(),
        )
        .unwrap();
        let existing = complete_cycle(reset - duration, duration, 60.0);
        fs::write(
            &v3_path,
            serde_json::to_vec_pretty(&Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![SeriesState {
                    provider_id: "codex".into(),
                    account_scope: "scope".into(),
                    window_key: "main.weekly.v1".into(),
                    active_reset_at: None,
                    last_activity_at: committed_now,
                    rollover: None,
                    samples: existing,
                }],
            })
            .unwrap(),
        )
        .unwrap();

        let outcome = migrate_codex_v2_at_paths_with_clock_and_mode(
            "acct",
            &HistoryScope::for_test("scope"),
            stale_now,
            &v2_path,
            &v3_path,
            StorageMode::Generic,
            || committed_now + 1,
        )
        .unwrap();
        assert_eq!(outcome.imported_samples, 8);
        let store = read_store(&v3_path);
        assert_eq!(store.series.len(), 1);
        assert_eq!(store.series[0].samples.len(), 16);
        assert!(store.series[0]
            .samples
            .iter()
            .any(|sample| sample.origin == SampleOrigin::LiveV3));
        assert!(store.series[0]
            .samples
            .iter()
            .any(|sample| sample.origin == SampleOrigin::ImportedV2));
        assert!(!directory
            .join(format!("quota-pace-history-v3.corrupt-{stale_now}.json"))
            .exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_corrupt_inputs_and_v1_sentinel_are_left_untouched() {
        let (directory, v3_path) = temp_path("migration-corrupt");
        let v2_path = directory.join(LEGACY_V2_FILE_NAME);
        let v1_path = directory.join("codex-weekly-history.json");
        let v1_bytes = b"legacy-v1-sentinel";
        fs::write(&v1_path, v1_bytes).unwrap();
        fs::write(&v2_path, b"not-json").unwrap();
        let v3_before = b"existing-v3-bytes";
        fs::write(&v3_path, v3_before).unwrap();
        let outcome = migrate_codex_v2_at_paths(
            "acct",
            &HistoryScope::for_test("scope"),
            7_000_000_000,
            &v2_path,
            &v3_path,
        )
        .unwrap();
        assert_eq!(outcome.imported_samples, 0);
        assert_eq!(fs::read(&v2_path).unwrap(), b"not-json");
        assert_eq!(fs::read(&v3_path).unwrap(), v3_before);
        assert_eq!(fs::read(&v1_path).unwrap(), v1_bytes);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_v3_migration_quarantines_then_rebuilds_atomically() {
        let (directory, v3_path) = temp_path("migration-v3-corrupt");
        let v2_path = directory.join(LEGACY_V2_FILE_NAME);
        let now = 8_000_000_000;
        let reset = now - 2 * 10_080 * 60;
        fs::write(
            &v2_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 2,
                "samples": [{
                    "accountKey": "acct",
                    "resetsAt": reset,
                    "windowMinutes": 10080,
                    "usedPercent": 25.0,
                    "sampledAt": reset - 10_080 * 60 / 2
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let corrupt = b"corrupt-v3-evidence";
        fs::write(&v3_path, corrupt).unwrap();
        let outcome = migrate_codex_v2_at_paths(
            "acct",
            &HistoryScope::for_test("scope"),
            now,
            &v2_path,
            &v3_path,
        )
        .unwrap();
        assert_eq!(outcome.imported_samples, 1);
        assert_eq!(
            fs::read(directory.join(format!("quota-pace-history-v3.corrupt-{now}.json"))).unwrap(),
            corrupt
        );
        assert_eq!(read_store(&v3_path).series[0].account_scope, "scope");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn global_sample_cap_evicts_old_completed_cycles_but_protects_current_partial() {
        let now = 9_000_000_000;
        let duration = DAY;
        let cycle_count = MAX_SAMPLES / MAX_SAMPLES_PER_CYCLE + 1;
        let mut samples = Vec::with_capacity(cycle_count * MAX_SAMPLES_PER_CYCLE + 8);
        for offset in 1..=cycle_count {
            samples.extend(complete_cycle(
                now - offset as i64 * duration,
                duration,
                80.0,
            ));
        }
        let current_reset = now + duration;
        let current = complete_cycle(current_reset, duration, 40.0);
        samples.extend(current.clone());
        let series = SeriesState {
            provider_id: "provider".into(),
            account_scope: "scope".into(),
            window_key: "window.v1".into(),
            active_reset_at: Some(current_reset),
            last_activity_at: now,
            rollover: Some(ObservedState::Watching {
                reset_at: current_reset,
                first_seen_at: now,
                last_seen_at: now,
                consecutive_count: 1,
            }),
            samples,
        };
        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series],
        };
        evict_old_completed_samples(&mut store, now).unwrap();
        let retained = &store.series[0];
        assert!(retained.samples.len() <= MAX_SAMPLES);
        assert!(current
            .iter()
            .all(|sample| retained.samples.contains(sample)));
        assert!(retained
            .samples
            .iter()
            .any(|sample| sample.reset_at == current[0].reset_at));
    }

    #[test]
    fn global_sample_eviction_tie_uses_series_key_order() {
        let now = 12_000_000_000_i64;
        let duration = DAY;
        let cycles_per_series = MAX_SAMPLES / (2 * 8) + 1;
        let make_series = |provider_id: &str| {
            let mut samples = Vec::with_capacity(cycles_per_series * 8);
            for offset in 1..=cycles_per_series {
                samples.extend(complete_cycle(
                    now - offset as i64 * duration,
                    duration,
                    80.0,
                ));
            }
            SeriesState {
                provider_id: provider_id.into(),
                account_scope: "scope".into(),
                window_key: "window.v1".into(),
                active_reset_at: None,
                last_activity_at: now,
                rollover: None,
                samples,
            }
        };
        let mut store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![make_series("b"), make_series("a")],
        };
        let oldest_reset = normalize_reset(now - cycles_per_series as i64 * duration, duration);
        let next_oldest_reset =
            normalize_reset(now - (cycles_per_series as i64 - 1) * duration, duration);
        evict_old_completed_samples(&mut store, now).unwrap();
        assert!(
            store
                .series
                .iter()
                .map(|series| series.samples.len())
                .sum::<usize>()
                <= MAX_SAMPLES
        );
        let series_a = store
            .series
            .iter()
            .find(|series| series.provider_id == "a")
            .unwrap();
        let series_b = store
            .series
            .iter()
            .find(|series| series.provider_id == "b")
            .unwrap();
        assert!(!series_a
            .samples
            .iter()
            .any(|sample| normalize_reset(sample.reset_at, duration) == oldest_reset));
        assert!(series_a
            .samples
            .iter()
            .any(|sample| normalize_reset(sample.reset_at, duration) == next_oldest_reset));
        assert!(!series_b
            .samples
            .iter()
            .any(|sample| normalize_reset(sample.reset_at, duration) == oldest_reset));
        assert!(series_b
            .samples
            .iter()
            .any(|sample| normalize_reset(sample.reset_at, duration) == next_oldest_reset));
    }

    #[test]
    fn same_origin_collision_uses_timestamp_usage_then_serialized_tuple() {
        let reset = 11_000_000_000;
        let duration = 7 * DAY;
        let base = quota_sample(reset, duration, 0.5, 50.0, SampleOrigin::ImportedV2);
        let mut later = base.clone();
        later.sampled_at += 1;
        assert_eq!(choose_sample(base.clone(), later.clone()), later);

        let mut higher = base.clone();
        higher.used_percent = 51.0;
        assert_eq!(choose_sample(base.clone(), higher.clone()), higher);

        let mut contract = base.clone();
        contract.duration_source = DurationSource::Contract;
        let expected =
            if serde_json::to_vec(&contract).unwrap() < serde_json::to_vec(&base).unwrap() {
                contract.clone()
            } else {
                base.clone()
            };
        assert_eq!(choose_sample(base, contract), expected);
    }

    #[test]
    fn migration_two_first_runs_merge_without_duplicate_series() {
        let (directory, v3_path) = temp_path("migration-two-process");
        let v2_path = directory.join(LEGACY_V2_FILE_NAME);
        let now = 10_000_000_000_i64;
        let duration = 10_080 * 60;
        let reset = now - 2 * duration;
        let phases = [0.01, 0.10, 0.25, 0.40, 0.60, 0.75, 0.90, 0.99];
        let legacy_samples = phases
            .into_iter()
            .map(|phase| {
                serde_json::json!({
                    "accountKey": "acct",
                    "resetsAt": reset,
                    "windowMinutes": 10080,
                    "usedPercent": phase * 80.0 + 1.0,
                    "sampledAt": reset - duration + (phase * duration as f64) as i64
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            &v2_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schemaVersion": 2,
                "samples": legacy_samples
            }))
            .unwrap(),
        )
        .unwrap();
        let left_v2 = v2_path.clone();
        let left_v3 = v3_path.clone();
        let right_v2 = v2_path.clone();
        let right_v3 = v3_path.clone();
        let left = std::thread::spawn(move || {
            migrate_codex_v2_at_paths(
                "acct",
                &HistoryScope::for_test("opaque"),
                now,
                &left_v2,
                &left_v3,
            )
            .unwrap()
        });
        let right = std::thread::spawn(move || {
            migrate_codex_v2_at_paths(
                "acct",
                &HistoryScope::for_test("opaque"),
                now,
                &right_v2,
                &right_v3,
            )
            .unwrap()
        });
        let outcomes = [left.join().unwrap(), right.join().unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.imported_samples)
                .sum::<usize>(),
            8
        );
        let store = read_store(&v3_path);
        assert_eq!(store.series.len(), 1);
        assert_eq!(store.series[0].samples.len(), 8);
        assert!(store.series[0]
            .samples
            .iter()
            .all(|sample| sample.origin == SampleOrigin::ImportedV2));
        fs::remove_dir_all(directory).unwrap();
    }

    fn line_fit_points(beta: f64, phases: &[f64]) -> Vec<FitPoint> {
        phases
            .iter()
            .enumerate()
            .map(|(index, phase)| FitPoint {
                phase: *phase,
                bucket: index,
                used_percent: beta * phase,
            })
            .collect()
    }

    fn current_series(
        key: &SeriesKey,
        reset_at: i64,
        duration_seconds: i64,
        points: &[f64],
        beta: f64,
    ) -> SeriesState {
        let samples = points
            .iter()
            .map(|phase| {
                quota_sample(
                    reset_at,
                    duration_seconds,
                    *phase,
                    (beta * phase).clamp(0.1, 100.0),
                    SampleOrigin::LiveV3,
                )
            })
            .collect::<Vec<_>>();
        SeriesState {
            provider_id: key.provider_id.clone(),
            account_scope: key.account_scope.clone(),
            window_key: key.window_key.clone(),
            active_reset_at: Some(reset_at),
            last_activity_at: samples
                .iter()
                .map(|sample| sample.sampled_at)
                .max()
                .unwrap_or(reset_at),
            rollover: Some(ObservedState::Watching {
                reset_at,
                first_seen_at: samples
                    .iter()
                    .map(|sample| sample.sampled_at)
                    .min()
                    .unwrap_or(reset_at),
                last_seen_at: samples
                    .iter()
                    .map(|sample| sample.sampled_at)
                    .max()
                    .unwrap_or(reset_at),
                consecutive_count: 1,
            }),
            samples,
        }
    }

    #[test]
    fn fit_kernel_rejects_leakage_and_shares_inclusive_quality_policy() {
        let phases = [0.10, 0.20, 0.30, 0.40, 0.50, 0.60];
        let smooth = line_fit_points(80.0, &phases);
        let partial = fit_partial_current(&smooth).expect("smooth prefix fit");
        assert!(partial.walk_forward_rmse <= EXPECTED_FIT_RMSE_PP);
        assert!(fit_quality(EXPECTED_FIT_RMSE_PP).is_some());
        assert!(fit_quality(EXPECTED_FIT_RMSE_PP + EPSILON).is_some());
        assert!(fit_quality(EXPECTED_FIT_RMSE_PP + 2.0 * EPSILON).is_none());
        assert!(fit_quality(-EPSILON).is_none());
        let mut invalid = smooth.clone();
        invalid[0].used_percent = 0.0;
        assert!(fit_partial_current(&invalid).is_none());

        let mut future_jump = smooth.clone();
        for point in future_jump.iter_mut().skip(3) {
            point.used_percent += 30.0;
        }
        assert!(fit_partial_current(&future_jump)
            .is_none_or(|fit| fit.walk_forward_rmse > EXPECTED_FIT_RMSE_PP));

        let completed = fit_completed_cycles(&[FitCycleInput {
            recency_weight: 1.0,
            points: smooth,
        }])
        .expect("single complete fit");
        assert!(completed.overall_rmse.is_finite());
        assert_eq!(completed.total_weight, 1.0);
        assert!(completed.historical_curve.len() == GRID_POINT_COUNT);
        assert!(completed_blend_weight(1.0, 1.0).is_some_and(|weight| weight <= 0.5));
        assert_eq!(partial_blend_weight(1.0), Some(0.5));
    }

    #[test]
    fn projection_phase_uses_exact_reset_after_normalized_identity_jitter() {
        let duration = DAY;
        let reset_at = 30_000_000_i64 + duration;
        let jittered_reset = reset_at + 120;
        assert_eq!(
            normalize_reset(reset_at, duration),
            normalize_reset(jittered_reset, duration)
        );
        let now = reset_at - duration / 5;
        let key = test_key("fixture", "exact-reset", "window.v1");
        let series = seeded_series(
            &key.provider_id,
            &key.account_scope,
            &key.window_key,
            reset_at,
            duration,
            5,
        );
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![series.clone()],
        };
        let baseline = calculate_target(&store, &key, reset_at, duration, 60.0, now)
            .pace
            .expect("baseline completed projection");
        let jittered = calculate_target(&store, &key, jittered_reset, duration, 60.0, now)
            .pace
            .expect("jittered completed projection");
        assert!((baseline.expected_percent - jittered.expected_percent).abs() > 1e-6);

        let partial = current_series(
            &key,
            reset_at,
            duration,
            &[0.10, 0.20, 0.30, 0.40, 0.50, 0.60],
            80.0,
        );
        let baseline = evaluate_partial_projection(
            &partial,
            normalize_reset(reset_at, duration),
            reset_at,
            duration,
            50.0,
            now,
        )
        .expect("baseline partial projection");
        let jittered = evaluate_partial_projection(
            &partial,
            normalize_reset(jittered_reset, duration),
            jittered_reset,
            duration,
            50.0,
            now,
        )
        .expect("jittered partial projection");
        assert!((baseline.expected_percent - jittered.expected_percent).abs() > 1e-6);
    }

    #[test]
    fn partial_fit_rejects_span_identity_and_duration_contradictions() {
        let duration = DAY;
        let reset_at = 31_000_000_i64 + duration;
        let key = test_key("fixture", "partial-guards", "window.v1");
        let phases = [0.10, 0.12, 0.14, 0.16, 0.18, 0.19];
        let series = current_series(&key, reset_at, duration, &phases, 80.0);
        let normalized = normalize_reset(reset_at, duration);
        let now = reset_at - duration + (0.19 * duration as f64) as i64;
        assert!(
            evaluate_partial_projection(&series, normalized, reset_at, duration, 20.0, now)
                .is_none()
        );

        let mut missing_active = series.clone();
        missing_active.active_reset_at = None;
        assert!(evaluate_partial_projection(
            &missing_active,
            normalized,
            reset_at,
            duration,
            20.0,
            now,
        )
        .is_none());

        let mut mismatched_active = series.clone();
        mismatched_active.active_reset_at = Some(reset_at + duration);
        assert!(evaluate_partial_projection(
            &mismatched_active,
            normalized,
            reset_at,
            duration,
            20.0,
            now,
        )
        .is_none());

        let mut mismatched_duration = series;
        mismatched_duration.samples[0].duration_seconds = 2 * duration;
        assert!(evaluate_partial_projection(
            &mismatched_duration,
            normalized,
            reset_at,
            duration,
            20.0,
            now,
        )
        .is_none());
    }

    #[test]
    fn partial_current_transaction_can_be_available_without_complete_cycles() {
        let (directory, path) = temp_path("partial-fit");
        let duration = DAY;
        let reset_at = 20_000_100 + duration;
        let key = test_key("fixture", "partial-scope", "window.v1");
        let phases = [0.10, 0.20, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80];
        let mut final_result = None;
        for phase in phases {
            let now = reset_at - duration + (phase * duration as f64) as i64;
            let results = record_observations_at_path_and_evaluate(
                std::slice::from_ref(&key),
                &[observation(key.clone(), reset_at, phase * 80.0, duration)],
                now,
                &path,
            )
            .unwrap();
            final_result = Some(results[0].as_ref().unwrap().clone());
        }
        let (_, pace, complete_cycles) = final_result.unwrap();
        assert_eq!(complete_cycles, 0);
        assert!(pace.is_some_and(|pace| {
            pace.run_out_probability.is_none()
                && pace.eta_seconds.is_none() == pace.will_last_to_reset
        }));
        let persisted = read_store(&path);
        assert!(persisted.series[0]
            .samples
            .iter()
            .all(|sample| sample.reset_at == normalize_reset(reset_at, duration)));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn full_transaction_duration_cleanup_persists_and_matches_same_duration_control() {
        let duration = DAY;
        let normalized_reset = normalize_reset(40_000_000_000_i64, duration);
        let reset_at = normalized_reset + 60;
        let now = normalized_reset;
        let key = test_key("fixture", "cleanup-transaction", "window.v1");
        let mut malformed = current_series(
            &key,
            reset_at,
            duration,
            &[0.10, 0.20, 0.30, 0.40, 0.50, 0.60],
            80.0,
        );
        malformed.samples.push(quota_sample(
            reset_at,
            2 * duration,
            0.75,
            60.0,
            SampleOrigin::LiveV3,
        ));
        let control = current_series(
            &key,
            reset_at,
            duration,
            &[0.10, 0.20, 0.30, 0.40, 0.50, 0.60],
            80.0,
        );
        let malformed_directory = temp_path("duration-cleanup-transaction");
        let control_directory = temp_path("duration-cleanup-control");
        fs::write(
            &malformed_directory.1,
            serde_json::to_vec_pretty(&Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![malformed],
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            &control_directory.1,
            serde_json::to_vec_pretty(&Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![control],
            })
            .unwrap(),
        )
        .unwrap();
        let malformed_result = record_observations_at_path_and_evaluate(
            std::slice::from_ref(&key),
            &[observation(key.clone(), reset_at, 48.0, duration)],
            now,
            &malformed_directory.1,
        )
        .unwrap();
        let control_result = record_observations_at_path_and_evaluate(
            std::slice::from_ref(&key),
            &[observation(key.clone(), reset_at, 48.0, duration)],
            now,
            &control_directory.1,
        )
        .unwrap();
        let malformed_store = read_store(&malformed_directory.1);
        let control_store = read_store(&control_directory.1);
        assert_eq!(malformed_result, control_result);
        assert_eq!(
            malformed_store.series[0].samples,
            control_store.series[0].samples
        );
        assert!(malformed_store.series[0].samples.iter().all(|sample| {
            !is_active_group_sample(reset_at, sample) || sample.duration_seconds == duration
        }));
        assert!(!malformed_store.series[0].samples.iter().any(|sample| {
            is_active_group_sample(reset_at, sample) && sample.duration_seconds == 2 * duration
        }));
        fs::remove_dir_all(malformed_directory.0).unwrap();
        fs::remove_dir_all(control_directory.0).unwrap();
    }

    #[test]
    fn partial_current_rejects_future_jump_and_enforces_active_duration_cleanup() {
        let (directory, path) = temp_path("partial-fit-leakage");
        let duration = DAY;
        let reset_at = 21_000_000 + duration;
        let key = test_key("fixture", "partial-leak", "window.v1");
        let phases = [0.10, 0.20, 0.30, 0.40, 0.50, 0.60];
        let mut result = None;
        for (index, phase) in phases.into_iter().enumerate() {
            let now = reset_at - duration + (phase * duration as f64) as i64;
            let used = if index < 3 {
                phase * 80.0
            } else {
                phase * 80.0 + 30.0
            };
            result = Some(
                record_observations_at_path_and_evaluate(
                    std::slice::from_ref(&key),
                    &[observation(key.clone(), reset_at, used, duration)],
                    now,
                    &path,
                )
                .unwrap()[0]
                    .as_ref()
                    .unwrap()
                    .clone(),
            );
        }
        assert!(result.unwrap().1.is_none());

        let mut series = read_store(&path).series.remove(0);
        series.samples.push(quota_sample(
            reset_at,
            2 * duration,
            0.75,
            60.0,
            SampleOrigin::LiveV3,
        ));
        let before = series.samples.len();
        let outcome = apply_known_duration(
            &mut series,
            reset_at,
            duration,
            DurationSource::Provider,
            56.0,
            reset_at - duration * 4 / 10,
        );
        assert!(matches!(outcome, HistoryOutcome::Ready { .. }));
        assert!(series.samples.len() < before);
        assert!(series.samples.iter().all(|sample| {
            !is_active_group_sample(reset_at, sample) || sample.duration_seconds == duration
        }));
        assert!(!series.samples.iter().any(|sample| {
            is_active_group_sample(reset_at, sample) && sample.duration_seconds == 2 * duration
        }));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stale_partial_quality_cannot_rescue_or_pollute_current_transaction() {
        let duration = DAY;
        let current_reset = 42_000_000_000_i64 + duration;
        let now = current_reset - duration * 4 / 10;
        let phases = [0.10, 0.20, 0.30, 0.40, 0.50, 0.60];
        let key = test_key("fixture", "stale-isolation", "window.v1");
        let make_samples = |reset_at: i64, jagged: bool| {
            phases
                .iter()
                .enumerate()
                .map(|(index, phase)| {
                    let used = if jagged && index >= 3 {
                        phase * 80.0 + 30.0
                    } else {
                        phase * 80.0
                    };
                    quota_sample(reset_at, duration, *phase, used, SampleOrigin::LiveV3)
                })
                .collect::<Vec<_>>()
        };
        for (label, stale_jagged, current_jagged, expect_pace) in [
            ("stale-high-current-low", false, true, false),
            ("stale-low-current-high", true, false, true),
        ] {
            let (directory, path) = temp_path(label);
            let stale_reset = current_reset - 2 * duration;
            let mut samples = make_samples(stale_reset, stale_jagged);
            samples.extend(make_samples(current_reset, current_jagged));
            let store = Store {
                schema_version: HISTORY_SCHEMA_VERSION,
                series: vec![SeriesState {
                    provider_id: key.provider_id.clone(),
                    account_scope: key.account_scope.clone(),
                    window_key: key.window_key.clone(),
                    active_reset_at: Some(current_reset),
                    last_activity_at: now,
                    rollover: Some(ObservedState::Watching {
                        reset_at: current_reset,
                        first_seen_at: now,
                        last_seen_at: now,
                        consecutive_count: 1,
                    }),
                    samples,
                }],
            };
            fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
            let results = record_observations_at_path_and_evaluate(
                std::slice::from_ref(&key),
                &[observation(key.clone(), current_reset, 48.0, duration)],
                now,
                &path,
            )
            .unwrap();
            let (_, pace, complete_cycles) = results[0].as_ref().unwrap();
            assert_eq!(
                *complete_cycles, 0,
                "{label} must not count stale partial history"
            );
            assert_eq!(pace.is_some(), expect_pace, "{label} current-only result");
            let persisted = read_store(&path);
            assert!(persisted.series[0].samples.iter().all(|sample| {
                normalize_reset(sample.reset_at, duration)
                    == normalize_reset(current_reset, duration)
            }));
            assert!(!persisted.series[0].samples.iter().any(|sample| {
                normalize_reset(sample.reset_at, duration) == normalize_reset(stale_reset, duration)
            }));
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn mixed_duration_complete_group_is_retained_but_not_fit_eligible() {
        let duration = DAY;
        let reset_at = 22_000_000 + duration;
        let now = reset_at + 1;
        let mixed = [DAY, 2 * DAY, DAY, 2 * DAY, DAY, 2 * DAY, DAY, 2 * DAY]
            .into_iter()
            .enumerate()
            .map(|(index, duration)| {
                quota_sample(
                    reset_at,
                    duration,
                    [0.01, 0.10, 0.25, 0.40, 0.60, 0.75, 0.90, 0.99][index],
                    80.0 * [0.01, 0.10, 0.25, 0.40, 0.60, 0.75, 0.90, 0.99][index],
                    SampleOrigin::LiveV3,
                )
            })
            .collect::<Vec<_>>();
        assert!(
            retention_cycle_descriptor(normalize_reset(reset_at, duration), &mixed, now).is_some()
        );
        assert!(cycle_profile(normalize_reset(reset_at, duration), &mixed, now).is_none());

        let key = test_key("fixture", "mixed", "window.v1");
        let mut series = SeriesState {
            provider_id: key.provider_id.clone(),
            account_scope: key.account_scope.clone(),
            window_key: key.window_key.clone(),
            active_reset_at: Some(now + duration),
            last_activity_at: now,
            rollover: None,
            samples: mixed.clone(),
        };
        retain_series(&mut series, now);
        assert_eq!(series.samples.len(), mixed.len());
        assert!(historical_cycles(&series, now + duration, now).is_empty());
    }

    #[test]
    fn completed_fit_holdouts_cover_smooth_jagged_and_loco_conflict_cases() {
        let phases = [0.01, 0.10, 0.25, 0.40, 0.60, 0.75, 0.90, 0.99];
        let smooth = line_fit_points(80.0, &phases);
        let single = fit_completed_cycles(&[FitCycleInput {
            recency_weight: 1.0,
            points: smooth.clone(),
        }])
        .expect("single smooth LOBO fit");
        assert!(single.overall_rmse <= EXPECTED_FIT_RMSE_PP);
        assert!(fit_quality(single.overall_rmse).is_some());

        let jagged = phases
            .iter()
            .enumerate()
            .map(|(index, phase)| FitPoint {
                phase: *phase,
                bucket: index,
                used_percent: if index == phases.len() - 1 {
                    100.0
                } else {
                    80.0 * phase
                },
            })
            .collect::<Vec<_>>();
        let jagged_fit = fit_completed_cycles(&[FitCycleInput {
            recency_weight: 1.0,
            points: jagged.clone(),
        }])
        .expect("single jagged fit");
        assert!(interpolate_curve(&jagged_fit.historical_curve, 0.99) > 95.0);
        assert!(jagged_fit.overall_rmse > EXPECTED_FIT_RMSE_PP);
        assert!(fit_quality(jagged_fit.overall_rmse).is_none());

        let agree = fit_completed_cycles(&[
            FitCycleInput {
                recency_weight: 3.0,
                points: smooth.clone(),
            },
            FitCycleInput {
                recency_weight: 1.0,
                points: smooth.clone(),
            },
        ])
        .expect("two agreeing cycles");
        assert!(agree.overall_rmse <= EXPECTED_FIT_RMSE_PP);

        let conflict = fit_completed_cycles(&[
            FitCycleInput {
                recency_weight: 9.0,
                points: line_fit_points(40.0, &phases),
            },
            FitCycleInput {
                recency_weight: 1.0,
                points: line_fit_points(100.0, &phases),
            },
        ])
        .expect("two conflicting cycles");
        assert!(conflict.overall_rmse > EXPECTED_FIT_RMSE_PP);
        assert!(fit_quality(conflict.overall_rmse).is_none());

        let weighted = aggregate_weighted_rms(&[(1.0, 9.0), (100.0, 1.0)]).unwrap();
        let wrong_unweighted = ((1.0_f64 + 100.0) / 2.0).sqrt();
        assert!((weighted - 10.9_f64.sqrt()).abs() < 1e-12);
        assert!(fit_quality(weighted).is_some());
        assert!(fit_quality(wrong_unweighted).is_none());
    }

    #[test]
    fn partial_projection_covers_beta_actual_and_crossing_boundaries() {
        let phases = [0.10, 0.20, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80];
        for beta in [80.0, 100.0, 120.0] {
            let fit =
                fit_partial_current(&line_fit_points(beta, &phases)).expect("linear partial fit");
            assert!((fit.beta - beta).abs() < 1e-9, "beta={beta}");
            assert!(fit.walk_forward_rmse <= EXPECTED_FIT_RMSE_PP, "beta={beta}");
        }

        let duration = DAY;
        let reset_at = 43_000_000_000_i64 + duration;
        let key = test_key("fixture", "partial-boundaries", "window.v1");
        let series = current_series(&key, reset_at, duration, &phases, 80.0);
        let now = reset_at - duration + (0.80 * duration as f64) as i64;
        let normalized = normalize_reset(reset_at, duration);
        let low = evaluate_partial_projection(&series, normalized, reset_at, duration, 20.0, now)
            .expect("low actual projection");
        assert!(low.eta_seconds.is_none());
        assert!(low.will_last_to_reset);

        let high = evaluate_partial_projection(&series, normalized, reset_at, duration, 95.0, now)
            .expect("high actual projection");
        assert_eq!(low.expected_percent, high.expected_percent);
        assert!(low.run_out_probability.is_none());
        assert!(high.run_out_probability.is_none());
        assert!(high.eta_seconds.is_some());
        assert!(!high.will_last_to_reset);
        assert!(high.eta_seconds.unwrap() > 0.0);

        let exhausted =
            evaluate_partial_projection(&series, normalized, reset_at, duration, 100.0, now)
                .expect("exhausted fact override");
        assert_eq!(exhausted.eta_seconds, Some(0.0));
        assert!(!exhausted.will_last_to_reset);
        assert_eq!(exhausted.run_out_probability, Some(1.0));
    }

    #[test]
    fn partial_work_counter_is_bounded_at_forty_five_walk_forward_fits() {
        let (directory, path) = temp_path("partial-fit-counter");
        let duration = 5 * HOUR;
        let reset_at = 23_000_000 + duration;
        let now = reset_at - 60;
        let key = test_key("fixture", "counter-partial", "window.v1");
        let samples = (0..PHASE_BUCKET_COUNT)
            .map(|bucket| {
                let phase = (bucket as f64 + 0.25) / PHASE_BUCKET_COUNT as f64;
                quota_sample(
                    reset_at,
                    duration,
                    phase,
                    (phase * 80.0).max(0.1),
                    SampleOrigin::LiveV3,
                )
            })
            .collect::<Vec<_>>();
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![SeriesState {
                provider_id: key.provider_id.clone(),
                account_scope: key.account_scope.clone(),
                window_key: key.window_key.clone(),
                active_reset_at: Some(reset_at),
                last_activity_at: now,
                rollover: Some(ObservedState::Watching {
                    reset_at,
                    first_seen_at: now,
                    last_seen_at: now,
                    consecutive_count: 1,
                }),
                samples,
            }],
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
        reset_fit_work_counters();
        let results = record_observations_at_path_and_evaluate(
            std::slice::from_ref(&key),
            &[observation(key.clone(), reset_at, 79.0, duration)],
            now,
            &path,
        )
        .unwrap();
        assert!(results[0].as_ref().unwrap().1.is_some());
        let counters = fit_work_counters();
        assert_eq!(counters.walk_forward_fits, 45);
        assert_eq!(counters.target_sample_reads, PHASE_BUCKET_COUNT);
        assert_eq!(counters.target_profiles_built, 0);
        assert_eq!(counters.non_target_sample_reads, 0);
        assert_eq!(counters.non_target_profiles_built, 0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn fit_and_projection_are_exactly_permutation_invariant() {
        let phases = [0.10, 0.20, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80];
        let first = line_fit_points(80.0, &phases);
        let second = line_fit_points(90.0, &phases);
        let ordered = fit_completed_cycles(&[
            FitCycleInput {
                recency_weight: 1.0,
                points: first.clone(),
            },
            FitCycleInput {
                recency_weight: 0.5,
                points: second.clone(),
            },
        ])
        .expect("ordered completed fit");
        let mut reversed_first = first.clone();
        reversed_first.reverse();
        let mut reversed_second = second.clone();
        reversed_second.reverse();
        let permuted = fit_completed_cycles(&[
            FitCycleInput {
                recency_weight: 0.5,
                points: reversed_second,
            },
            FitCycleInput {
                recency_weight: 1.0,
                points: reversed_first,
            },
        ])
        .expect("permuted completed fit");
        assert_eq!(ordered.historical_curve, permuted.historical_curve);
        assert_eq!(ordered.overall_rmse, permuted.overall_rmse);
        assert_eq!(ordered.tail_rmse, permuted.tail_rmse);
        assert_eq!(ordered.tail_cycle_count, permuted.tail_cycle_count);
        assert_eq!(ordered.total_weight, permuted.total_weight);

        let duration = DAY;
        let reset_at = 51_000_000_000_i64 + duration;
        let key = test_key("fixture", "permutation", "window.v1");
        let mut first_series = current_series(&key, reset_at, duration, &phases, 80.0);
        let mut second_series = first_series.clone();
        second_series.samples.reverse();
        let now = reset_at - duration + (0.80 * duration as f64) as i64;
        let ordered_pace = evaluate_partial_projection(
            &first_series,
            normalize_reset(reset_at, duration),
            reset_at,
            duration,
            70.0,
            now,
        )
        .expect("ordered partial projection");
        first_series.samples.reverse();
        let permuted_pace = evaluate_partial_projection(
            &second_series,
            normalize_reset(reset_at, duration),
            reset_at,
            duration,
            70.0,
            now,
        )
        .expect("permuted partial projection");
        assert_eq!(ordered_pace, permuted_pace);
    }

    #[test]
    fn completed_tail_quality_controls_risk_but_exhausted_overrides_it() {
        let duration = 2 * DAY;
        let current_reset = 50_000_000_000_i64 + duration;
        let now = current_reset - duration / 2;
        let phases = [0.10, 0.30, 0.50, 0.70, 0.90];
        let points = line_fit_points(80.0, &phases);
        let cycles = (0..6)
            .map(|index| {
                let reset_at = current_reset - (2 * index as i64 + 1) * duration;
                CycleProfile {
                    reset_at,
                    duration_seconds: duration,
                    cycle_started_at: reset_at - duration,
                    points: points.clone(),
                    curve: reconstruct_fit_curve(&points),
                }
            })
            .collect::<Vec<_>>();
        let weighted = cycles.iter().map(|cycle| (cycle, 1.0)).collect::<Vec<_>>();
        let computed_fit = fit_completed_cycles(
            &cycles
                .iter()
                .map(|cycle| FitCycleInput {
                    recency_weight: 1.0,
                    points: cycle.points.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .expect("computed tail fit");
        assert_eq!(computed_fit.tail_cycle_count, 0);
        let historical_curve = (0..GRID_POINT_COUNT)
            .map(|index| 100.0 * index as f64 / (GRID_POINT_COUNT - 1) as f64)
            .collect::<Vec<_>>();
        let make_fit = |tail_rmse, tail_cycle_count| CompletedFitResult {
            historical_curve: historical_curve.clone(),
            overall_rmse: 0.0,
            tail_rmse,
            tail_cycle_count,
            total_weight: 6.0,
        };
        let insufficient_tail = evaluate_completed_projection(
            &cycles,
            &weighted,
            computed_fit.clone(),
            1.0,
            current_reset,
            duration,
            50.0,
            now,
        )
        .expect("overall fit with insufficient tail");
        assert!(insufficient_tail.run_out_probability.is_none());

        let missing_tail_cycle = evaluate_completed_projection(
            &cycles,
            &weighted,
            make_fit(Some(EXPECTED_FIT_RMSE_PP), 5),
            1.0,
            current_reset,
            duration,
            50.0,
            now,
        )
        .expect("overall fit with one tail-unvalidated cycle");
        assert!(missing_tail_cycle.run_out_probability.is_none());

        let exact_tail = evaluate_completed_projection(
            &cycles,
            &weighted,
            make_fit(Some(EXPECTED_FIT_RMSE_PP), 6),
            1.0,
            current_reset,
            duration,
            50.0,
            now,
        )
        .expect("exact tail threshold");
        assert!(exact_tail.run_out_probability.is_some());

        let over_tail = evaluate_completed_projection(
            &cycles,
            &weighted,
            make_fit(Some(EXPECTED_FIT_RMSE_PP + 2.0 * EPSILON), 6),
            1.0,
            current_reset,
            duration,
            50.0,
            now,
        )
        .expect("over-threshold tail still returns pace");
        assert!(over_tail.run_out_probability.is_none());

        let exhausted = evaluate_completed_projection(
            &cycles,
            &weighted,
            computed_fit,
            1.0,
            current_reset,
            duration,
            100.0,
            now,
        )
        .expect("exhausted fact override");
        assert_eq!(exhausted.run_out_probability, Some(1.0));
        assert_eq!(exhausted.eta_seconds, Some(0.0));
        assert!(!exhausted.will_last_to_reset);
    }

    #[test]
    fn completed_work_counter_hits_maximum_retention_bounds() {
        let (directory, path) = temp_path("completed-fit-counter");
        let duration = 5 * HOUR;
        let current_reset = 24_000_000_000_i64 + duration;
        let now = current_reset - 60;
        let key = test_key("fixture", "counter-complete", "window.v1");
        let mut samples = Vec::with_capacity(129 * PHASE_BUCKET_COUNT);
        for offset in 1..=RETENTION_MAX_CYCLES {
            let reset = current_reset - offset as i64 * duration;
            samples.extend((0..PHASE_BUCKET_COUNT).map(|bucket| {
                let phase = (bucket as f64 + 0.25) / PHASE_BUCKET_COUNT as f64;
                quota_sample(
                    reset,
                    duration,
                    phase,
                    (phase * 80.0).max(0.1),
                    SampleOrigin::LiveV3,
                )
            }));
        }
        samples.extend((0..PHASE_BUCKET_COUNT).map(|bucket| {
            let phase = (bucket as f64 + 0.25) / PHASE_BUCKET_COUNT as f64;
            quota_sample(
                current_reset,
                duration,
                phase,
                (phase * 80.0).max(0.1),
                SampleOrigin::LiveV3,
            )
        }));
        let store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![SeriesState {
                provider_id: key.provider_id.clone(),
                account_scope: key.account_scope.clone(),
                window_key: key.window_key.clone(),
                active_reset_at: Some(current_reset),
                last_activity_at: now,
                rollover: Some(ObservedState::Watching {
                    reset_at: current_reset,
                    first_seen_at: now,
                    last_seen_at: now,
                    consecutive_count: 1,
                }),
                samples,
            }],
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
        reset_fit_work_counters();
        let results = record_observations_at_path_and_evaluate(
            std::slice::from_ref(&key),
            &[observation(key.clone(), current_reset, 79.0, duration)],
            now,
            &path,
        )
        .unwrap();
        assert_eq!(results[0].as_ref().unwrap().2, RETENTION_MAX_CYCLES);
        let counters = fit_work_counters();
        assert_eq!(
            counters.lobo_folds,
            RETENTION_MAX_CYCLES * PHASE_BUCKET_COUNT
        );
        assert_eq!(counters.loco_folds, RETENTION_MAX_CYCLES);
        assert_eq!(counters.target_sample_reads, 129 * PHASE_BUCKET_COUNT);
        assert_eq!(counters.target_profiles_built, RETENTION_MAX_CYCLES);
        assert_eq!(counters.non_target_sample_reads, 0);
        assert_eq!(counters.non_target_profiles_built, 0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn mixed_duration_retention_keeps_slots_bounded_without_fit_work() {
        let duration = 5 * HOUR;
        let current_reset = (53_000_000_000_i64 + duration + 899).div_euclid(900) * 900;
        let now = current_reset - 60;
        let key = test_key("fixture", "mixed-retention", "window.v1");
        let phases = (0..PHASE_BUCKET_COUNT)
            .map(|bucket| (bucket as f64 + 0.25) / PHASE_BUCKET_COUNT as f64)
            .collect::<Vec<_>>();
        let make_cycle = |reset_at: i64, mixed: bool| {
            phases
                .iter()
                .enumerate()
                .map(|(bucket, phase)| {
                    let sample_duration = if mixed && bucket % 2 == 0 {
                        2 * duration
                    } else {
                        duration
                    };
                    quota_sample(
                        reset_at,
                        sample_duration,
                        *phase,
                        (*phase * 80.0).max(0.1),
                        SampleOrigin::LiveV3,
                    )
                })
                .collect::<Vec<_>>()
        };
        let build_series = |with_mixed: bool, include_old: bool| {
            let mut samples = Vec::new();
            let first_offset = if include_old { 1 } else { 2 };
            for offset in first_offset..=128 {
                samples.extend(make_cycle(
                    current_reset - offset as i64 * duration,
                    with_mixed && offset == 1,
                ));
            }
            if with_mixed && !include_old {
                samples.extend(make_cycle(current_reset - duration, true));
            }
            samples.extend(make_cycle(current_reset, false));
            SeriesState {
                provider_id: key.provider_id.clone(),
                account_scope: key.account_scope.clone(),
                window_key: key.window_key.clone(),
                active_reset_at: Some(current_reset),
                last_activity_at: now,
                rollover: Some(ObservedState::Watching {
                    reset_at: current_reset,
                    first_seen_at: now,
                    last_seen_at: now,
                    consecutive_count: 1,
                }),
                samples,
            }
        };

        let mut control_series = build_series(false, false);
        let mut mixed_series = build_series(true, false);
        retain_series(&mut control_series, now);
        retain_series(&mut mixed_series, now);
        let control_store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![control_series],
        };
        let mixed_store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: vec![mixed_series],
        };
        reset_fit_work_counters();
        let control = calculate_target(&control_store, &key, current_reset, duration, 79.0, now);
        let control_counters = fit_work_counters();
        reset_fit_work_counters();
        let mixed = calculate_target(&mixed_store, &key, current_reset, duration, 79.0, now);
        let mixed_counters = fit_work_counters();
        assert_eq!(control.complete_cycles, mixed.complete_cycles);
        assert_eq!(control.pace, mixed.pace);
        assert_eq!(
            control_counters.target_profiles_built,
            mixed_counters.target_profiles_built
        );
        assert_eq!(control_counters.lobo_folds, mixed_counters.lobo_folds);
        assert_eq!(control_counters.loco_folds, mixed_counters.loco_folds);
        assert_eq!(
            mixed_counters.target_sample_reads,
            control_counters.target_sample_reads + PHASE_BUCKET_COUNT
        );
        assert_eq!(mixed.complete_cycles, 127);
        let mixed_groups = grouped_samples(&mixed_store.series[0].samples);
        let mixed_reset = normalize_reset(current_reset - duration, duration);
        assert!(cycle_profile(mixed_reset, mixed_groups.get(&mixed_reset).unwrap(), now).is_none());

        let mut overfull = build_series(true, true);
        overfull
            .samples
            .extend(make_cycle(current_reset - 129 * duration, false));
        retain_series(&mut overfull, now);
        assert_eq!(
            grouped_samples(&overfull.samples).len(),
            RETENTION_MAX_CYCLES + 1
        );
        assert!(overfull.samples.len() <= (RETENTION_MAX_CYCLES + 1) * PHASE_BUCKET_COUNT);
        assert!(!overfull.samples.iter().any(|sample| {
            normalize_reset(sample.reset_at, duration)
                == normalize_reset(current_reset - 129 * duration, duration)
        }));
        assert!(overfull.samples.iter().any(|sample| {
            normalize_reset(sample.reset_at, duration)
                == normalize_reset(current_reset - duration, duration)
        }));
    }

    #[test]
    fn target_calculation_isolated_from_unrelated_series_contents_and_order() {
        let duration = 5 * HOUR;
        let current_reset = 52_000_000_000_i64 + duration;
        let now = current_reset - duration / 2;
        let target_key = test_key("fixture", "target", "window.v1");
        let target = seeded_series(
            &target_key.provider_id,
            &target_key.account_scope,
            &target_key.window_key,
            current_reset,
            duration,
            5,
        );
        let make_unrelated = |rich: bool| {
            (0..32)
                .map(|index| {
                    let key = test_key("fixture", format!("unrelated-{index:03}"), "window.v1");
                    let samples = if rich {
                        (1..=4)
                            .flat_map(|offset| {
                                complete_cycle(
                                    current_reset - offset as i64 * duration,
                                    duration,
                                    95.0,
                                )
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    SeriesState {
                        provider_id: key.provider_id,
                        account_scope: key.account_scope,
                        window_key: key.window_key,
                        active_reset_at: None,
                        last_activity_at: now,
                        rollover: None,
                        samples,
                    }
                })
                .collect::<Vec<_>>()
        };
        let mut before = make_unrelated(false);
        before.push(target.clone());
        let mut after = vec![target];
        after.extend(make_unrelated(true));
        let before_store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: before,
        };
        let after_store = Store {
            schema_version: HISTORY_SCHEMA_VERSION,
            series: after,
        };
        reset_fit_work_counters();
        let before_result = calculate_target(
            &before_store,
            &target_key,
            current_reset,
            duration,
            60.0,
            now,
        );
        let before_counters = fit_work_counters();
        reset_fit_work_counters();
        let after_result = calculate_target(
            &after_store,
            &target_key,
            current_reset,
            duration,
            60.0,
            now,
        );
        let after_counters = fit_work_counters();
        assert_eq!(before_result.complete_cycles, after_result.complete_cycles);
        assert_eq!(before_result.pace, after_result.pace);
        assert_eq!(
            before_counters.target_sample_reads,
            after_counters.target_sample_reads
        );
        assert_eq!(
            before_counters.target_profiles_built,
            after_counters.target_profiles_built
        );
        assert_eq!(before_counters.lobo_folds, after_counters.lobo_folds);
        assert_eq!(before_counters.loco_folds, after_counters.loco_folds);
        assert_eq!(before_counters.non_target_sample_reads, 0);
        assert_eq!(before_counters.non_target_profiles_built, 0);
        assert_eq!(after_counters.non_target_sample_reads, 0);
        assert_eq!(after_counters.non_target_profiles_built, 0);
        assert_ne!(
            before_counters.series_key_comparisons,
            after_counters.series_key_comparisons
        );
    }

    #[test]
    #[ignore]
    fn fit_quality_benchmark_reference() {
        use std::time::Instant;

        const STORE_SERIES_COUNT: usize = 512;
        const TARGET_INDICES: [usize; 10] = [0, 56, 112, 168, 224, 280, 336, 392, 448, 504];
        const RUNS: usize = 20;

        fn median_micros(values: &mut [u128]) -> u128 {
            values.sort_unstable();
            values[values.len() / 2]
        }

        let duration = 5 * HOUR;
        let current_reset = 24_000_000_000_i64 + duration;
        let now = current_reset - 60;
        let build_store = |include_history: bool| {
            let mut series = Vec::with_capacity(STORE_SERIES_COUNT);
            let mut target_keys = Vec::with_capacity(TARGET_INDICES.len());
            for index in 0..STORE_SERIES_COUNT {
                let account = format!("benchmark-{index:03}");
                let window = format!("window-{index:03}.v1");
                let key = test_key("fixture", &account, &window);
                let target = TARGET_INDICES.contains(&index);
                let mut samples = Vec::new();
                if target {
                    if include_history {
                        for offset in 1..=RETENTION_MAX_CYCLES {
                            let reset = current_reset - offset as i64 * duration;
                            samples.extend((0..PHASE_BUCKET_COUNT).map(|bucket| {
                                let phase = (bucket as f64 + 0.25) / PHASE_BUCKET_COUNT as f64;
                                quota_sample(
                                    reset,
                                    duration,
                                    phase,
                                    (phase * 80.0).max(0.1),
                                    SampleOrigin::LiveV3,
                                )
                            }));
                        }
                    }
                    samples.extend((0..PHASE_BUCKET_COUNT).map(|bucket| {
                        let phase = (bucket as f64 + 0.25) / PHASE_BUCKET_COUNT as f64;
                        quota_sample(
                            current_reset,
                            duration,
                            phase,
                            (phase * 80.0).max(0.1),
                            SampleOrigin::LiveV3,
                        )
                    }));
                    target_keys.push(key.clone());
                }
                series.push(SeriesState {
                    provider_id: key.provider_id.clone(),
                    account_scope: key.account_scope.clone(),
                    window_key: key.window_key.clone(),
                    active_reset_at: target.then_some(current_reset),
                    last_activity_at: now,
                    rollover: target.then(|| ObservedState::Watching {
                        reset_at: current_reset,
                        first_seen_at: now,
                        last_seen_at: now,
                        consecutive_count: 1,
                    }),
                    samples,
                });
            }
            (
                Store {
                    schema_version: HISTORY_SCHEMA_VERSION,
                    series,
                },
                target_keys,
            )
        };

        let (partial_store, partial_keys) = build_store(false);
        let (completed_store, completed_keys) = build_store(true);
        let mut partial_runs = Vec::with_capacity(RUNS);
        let mut completed_runs = Vec::with_capacity(RUNS);
        let mut windows_runs = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            reset_fit_work_counters();
            let partial_start = Instant::now();
            let partial = calculate_target(
                &partial_store,
                &partial_keys[0],
                current_reset,
                duration,
                79.0,
                now,
            );
            partial_runs.push(partial_start.elapsed().as_micros());
            assert_eq!(partial.complete_cycles, 0);
            assert!(partial.pace.is_some());
            let counters = fit_work_counters();
            assert_eq!(counters.target_sample_reads, PHASE_BUCKET_COUNT);
            assert_eq!(counters.target_profiles_built, 0);
            assert_eq!(counters.non_target_sample_reads, 0);
            assert_eq!(counters.non_target_profiles_built, 0);
            assert!(counters.walk_forward_fits <= 45);

            reset_fit_work_counters();
            let completed_start = Instant::now();
            let completed = calculate_target(
                &completed_store,
                &completed_keys[0],
                current_reset,
                duration,
                79.0,
                now,
            );
            completed_runs.push(completed_start.elapsed().as_micros());
            assert_eq!(completed.complete_cycles, RETENTION_MAX_CYCLES);
            assert!(completed.pace.is_some());
            let counters = fit_work_counters();
            assert_eq!(
                counters.target_sample_reads,
                (RETENTION_MAX_CYCLES + 1) * PHASE_BUCKET_COUNT
            );
            assert_eq!(counters.target_profiles_built, RETENTION_MAX_CYCLES);
            assert_eq!(
                counters.lobo_folds,
                RETENTION_MAX_CYCLES * PHASE_BUCKET_COUNT
            );
            assert_eq!(counters.loco_folds, RETENTION_MAX_CYCLES);
            assert_eq!(counters.non_target_sample_reads, 0);
            assert_eq!(counters.non_target_profiles_built, 0);

            reset_fit_work_counters();
            let windows_start = Instant::now();
            for key in &completed_keys {
                let calculation =
                    calculate_target(&completed_store, key, current_reset, duration, 79.0, now);
                assert_eq!(calculation.complete_cycles, RETENTION_MAX_CYCLES);
                assert!(calculation.pace.is_some());
            }
            windows_runs.push(windows_start.elapsed().as_micros());
            let counters = fit_work_counters();
            assert_eq!(
                counters.target_sample_reads,
                completed_keys.len() * (RETENTION_MAX_CYCLES + 1) * PHASE_BUCKET_COUNT
            );
            assert_eq!(
                counters.target_profiles_built,
                completed_keys.len() * RETENTION_MAX_CYCLES
            );
            assert_eq!(
                counters.lobo_folds,
                completed_keys.len() * RETENTION_MAX_CYCLES * PHASE_BUCKET_COUNT
            );
            assert_eq!(
                counters.loco_folds,
                completed_keys.len() * RETENTION_MAX_CYCLES
            );
            assert_eq!(counters.non_target_sample_reads, 0);
            assert_eq!(counters.non_target_profiles_built, 0);
        }
        println!(
            "fit_quality_benchmark_reference {{\"partialMedianUs\":{},\"maxRetentionMedianUs\":{},\"windows10MedianUs\":{}}}",
            median_micros(&mut partial_runs),
            median_micros(&mut completed_runs),
            median_micros(&mut windows_runs)
        );
    }
}
