//! C-ABI bridge over tokscale-core for the Swift app.
//!
//! Contract: every entry point returns a heap-allocated, NUL-terminated JSON
//! string; the caller must release it with `tb_free`. Entry points are
//! synchronous — Swift calls them from a background thread.
//!
//! Envelope: every entry point (except the legacy `tb_probe`) wraps its
//! payload as `{"ok":true,"data":<payload>}` on success and
//! `{"ok":false,"err":"..."}` on failure. The `data` shapes mirror the Tauri
//! frontend contract (`src/lib/types.ts` / `src/lib/agentUsage.ts` in the
//! TokenBar-tokcat repo) exactly.
//!
//! The report modules are ports of the Tauri backend modules of the same
//! names (TokenBar-tokcat/src-tauri/src/*.rs) with the Tauri command plumbing
//! stripped; keep them diffable against the originals.

mod agent_account_scope;
mod agent_antigravity;
mod agent_copilot;
mod agent_grok;
mod agent_quota_duration;
mod agent_quota_history;
#[cfg(target_os = "windows")]
mod agent_storage_windows;
mod agent_usage;
mod agents_report;
mod filter_parity_probe;
mod hourly_report;
mod model_report;
mod opencode_integrations;
mod usage_graph;
mod usage_tail;

use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_char, CStr, CString};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use usage_tail::UsageTailer;

fn select_user_home(home: Option<PathBuf>, platform_home: Option<PathBuf>) -> Option<PathBuf> {
    home.filter(|path| !path.as_os_str().is_empty())
        .or(platform_home)
}

/// Resolve the user's home without requiring `HOME`, which is normally absent
/// for Windows GUI and Task Scheduler launches.
pub(crate) fn user_home_dir() -> Option<PathBuf> {
    select_user_home(
        std::env::var_os("HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

/// Snapshot the local source roots used by every FFI report and parse path.
/// Environment roots are process-startup configuration: changing them requires
/// restarting the FFI process. Cache keys intentionally do not fingerprint roots.
#[derive(Debug, Clone)]
pub(crate) struct LocalSourceContext {
    home_dir: Option<PathBuf>,
}

impl LocalSourceContext {
    pub(crate) fn current() -> Self {
        Self {
            home_dir: user_home_dir(),
        }
    }

    /// Point the local-source scan at a fixture home so report tests never
    /// read the developer's real session files.
    #[cfg(test)]
    pub(crate) fn for_home(home_dir: PathBuf) -> Self {
        Self {
            home_dir: Some(home_dir),
        }
    }

    pub(crate) fn report_options(
        &self,
        year: Option<String>,
        clients: Option<Vec<String>>,
    ) -> tokscale_core::ReportOptions {
        tokscale_core::ReportOptions {
            home_dir: self
                .home_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            use_env_roots: true,
            year,
            clients,
            ..Default::default()
        }
    }

    pub(crate) fn parse_options(
        &self,
        year: Option<String>,
        clients: Option<Vec<String>>,
    ) -> tokscale_core::LocalParseOptions {
        tokscale_core::LocalParseOptions {
            home_dir: self
                .home_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            use_env_roots: true,
            year,
            clients,
            ..Default::default()
        }
    }
}

/// Serve `tb_graph` from cache when the last computation is at most this old;
/// `tb_refresh_graph` always recomputes. Mirrors the Tauri app's oneshot cache.
const ONESHOT_MAX_AGE_SECS: u64 = 30;
/// Re-parse cadence for the live tail. In the Tauri app a background loop
/// ticks every 10s; the staticlib spawns no threads, so the tail ticks lazily:
/// `tb_usage_trace` / `tb_tokens_per_min` re-parse at most once per interval
/// and serve cached state in between.
const TAIL_TICK_SECS: u64 = 10;

/// Multi-thread runtime for the async/network entry points (`tb_agent_usage`).
/// Lazily initialized on first use; lives for the process lifetime.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build tokio runtime for tb_core_ffi")
});

#[derive(Default)]
struct PublicationGate {
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PublicationGenerationExhausted;

static AGENT_USAGE_PUBLICATION_GATE: LazyLock<Mutex<PublicationGate>> =
    LazyLock::new(|| Mutex::new(PublicationGate::default()));

#[derive(Debug, Clone, Default)]
struct QuotaCurveBindingState {
    generation: u64,
    series: BTreeMap<(String, String), agent_quota_history::SeriesKey>,
}

static QUOTA_CURVE_BINDINGS: LazyLock<RwLock<QuotaCurveBindingState>> =
    LazyLock::new(|| RwLock::new(QuotaCurveBindingState::default()));

#[cfg(test)]
static TEST_QUOTA_CURVE_HISTORY_PATH: LazyLock<Mutex<Option<PathBuf>>> =
    LazyLock::new(|| Mutex::new(None));

/// Serialize the complete publication path and assign its order at gate entry;
/// this gate is the sole generation source, not a timestamp or caller ordering.
/// Exhaustion fails closed instead of publishing a duplicate generation.
fn with_publication_gate<T>(
    gate: &Mutex<PublicationGate>,
    body: impl FnOnce(u64) -> T,
) -> Result<T, PublicationGenerationExhausted> {
    let mut state = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = state
        .generation
        .checked_add(1)
        .ok_or(PublicationGenerationExhausted)?;
    state.generation = generation;
    Ok(body(generation))
}

fn with_agent_usage_publication_gate<T>(
    body: impl FnOnce(u64) -> T,
) -> Result<T, PublicationGenerationExhausted> {
    with_publication_gate(&AGENT_USAGE_PUBLICATION_GATE, body)
}

fn replace_quota_curve_bindings(
    generation: u64,
    series: impl IntoIterator<Item = agent_quota_history::SeriesKey>,
) {
    let series = series
        .into_iter()
        .map(|key| ((key.provider_id.clone(), key.window_key.clone()), key))
        .collect();
    let mut state = QUOTA_CURVE_BINDINGS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *state = QuotaCurveBindingState { generation, series };
}

fn serialize_agent_usage_with_bindings<F>(
    generation: u64,
    bindings: Vec<agent_quota_history::SeriesKey>,
    serialize: F,
) -> Result<serde_json::Value, String>
where
    F: FnOnce() -> Result<serde_json::Value, String>,
{
    let value = serialize()?;
    replace_quota_curve_bindings(generation, bindings);
    Ok(value)
}

fn serialize_agent_usage_payload(
    generation: u64,
    payload: &agent_usage::AgentUsagePayload,
) -> Result<serde_json::Value, String> {
    let bindings = payload.quota_curve_series();
    serialize_agent_usage_with_bindings(generation, bindings, || {
        serde_json::to_value(payload).map_err(|error| format!("serialize agent usage: {error}"))
    })
}

/// Cap rayon's global thread pool to 2 workers. tokscale-core uses rayon for
/// parallel log parsing (55+ par_iter sites); the default pool size is num_cpus
/// which is fine for a one-shot CLI but ruinous for a resident menu-bar daemon:
/// each idle worker busy-waits before parking, and every 10s poll wakes the
/// entire pool for trivial mtime-check work. 2 threads keep I/O parallelism
/// while cutting idle spinning overhead by ~80%.
static RAYON_INIT: LazyLock<()> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build_global()
        .ok();
});

/// year → (computed-at, source token, mapped graph payload). Same role as
/// the Tauri AppState cache, plus a change token: when the cache entry ages
/// past the oneshot window but the topology-sensitive token still matches,
/// the entry is re-stamped and served — an idle machine never pays for a full
/// re-aggregation just because time passed.
type GraphCacheEntry = (Instant, u64, serde_json::Value);
static GRAPH_CACHE: LazyLock<Mutex<HashMap<String, GraphCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static TAILER: LazyLock<UsageTailer> = LazyLock::new(UsageTailer::new);
/// Live-tail tick bookkeeping. `last` is the completion time of the most recent
/// successful re-parse; `in_flight` is set while a re-parse is running so a
/// concurrent poller serves the cached window instead of launching a duplicate.
struct TickState {
    last: Option<Instant>,
    in_flight: bool,
}
static TAIL_TICK: Mutex<TickState> = Mutex::new(TickState {
    last: None,
    in_flight: false,
});

fn into_raw_json(json: String) -> *mut c_char {
    // A JSON payload should never contain interior NULs; fall back to an
    // error object instead of returning a dangling/null pointer.
    CString::new(json)
        .unwrap_or_else(|_| CString::new(r#"{"ok":false,"err":"interior NUL"}"#).unwrap())
        .into_raw()
}

fn envelope(result: Result<serde_json::Value, String>) -> *mut c_char {
    let json = match result {
        Ok(data) => serde_json::json!({"ok": true, "data": data}).to_string(),
        Err(err) => serde_json::json!({"ok": false, "err": err}).to_string(),
    };
    into_raw_json(json)
}

/// Run an FFI entry-point body, converting any panic into an error envelope
/// instead of letting it unwind across the C ABI. The release profile unwinds
/// (see the `[profile.release]` note in Cargo.toml): a panic inside one report —
/// a serde error, a bad slice index in a ported module, an `.expect()` — is
/// caught here and degrades that single call to `{"ok":false,...}`, leaving the
/// rest of the menu-bar app running rather than aborting the whole process. The
/// default panic hook still prints the panic location to stderr before we catch.
///
/// `AssertUnwindSafe` is sound here. Process-wide std::sync Mutex state is
/// updated only while locked, and each lock helper recovers poison on the next
/// call via `into_inner()`. The live tail's parking_lot Mutexes never poison and
/// release cleanly on unwind.
/// A caught panic can leave a cache entry stale or a tail tick un-run, never
/// torn: the next call re-derives the graph, and `tail_tick_if_stale` clears its
/// in-flight flag without stamping on a tick panic so the tail re-parses next.
fn guarded(name: &str, body: impl FnOnce() -> *mut c_char) -> *mut c_char {
    LazyLock::force(&RAYON_INIT);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(ptr) => ptr,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("panic");
            envelope(Err(format!("{} panicked: {}", name, detail)))
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaCurvePoint {
    sampled_at: i64,
    used_percent: f64,
    reset_at: i64,
    duration_seconds: i64,
    duration_source: agent_quota_duration::DurationSource,
    origin: agent_quota_history::SampleOrigin,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaCurveCoverage {
    oldest_sampled_at: i64,
    newest_sampled_at: i64,
    sample_count: usize,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaCurvePayload {
    points: Vec<QuotaCurvePoint>,
    coverage: QuotaCurveCoverage,
    active_reset_at: Option<i64>,
    generation: u64,
}

fn quota_curve_payload(
    series: &agent_quota_history::SeriesState,
    generation: u64,
) -> Result<serde_json::Value, String> {
    let mut samples = series.samples.clone();
    samples.sort_by(|left, right| {
        left.sampled_at
            .cmp(&right.sampled_at)
            .then(left.reset_at.cmp(&right.reset_at))
            .then(left.duration_seconds.cmp(&right.duration_seconds))
            .then(left.used_percent.total_cmp(&right.used_percent))
    });
    let oldest_sampled_at = samples
        .first()
        .map(|sample| sample.sampled_at)
        .ok_or_else(|| "quota pace history is absent".to_string())?;
    let newest_sampled_at = samples
        .last()
        .map(|sample| sample.sampled_at)
        .ok_or_else(|| "quota pace history is absent".to_string())?;
    let points = samples
        .into_iter()
        .map(|sample| QuotaCurvePoint {
            sampled_at: sample.sampled_at,
            used_percent: sample.used_percent,
            reset_at: sample.reset_at,
            duration_seconds: sample.duration_seconds,
            duration_source: sample.duration_source,
            origin: sample.origin,
        })
        .collect::<Vec<_>>();
    serde_json::to_value(QuotaCurvePayload {
        coverage: QuotaCurveCoverage {
            oldest_sampled_at,
            newest_sampled_at,
            sample_count: points.len(),
        },
        points,
        active_reset_at: series.active_reset_at,
        generation,
    })
    .map_err(|error| format!("serialize quota curve: {error}"))
}

fn history_error_message(error: agent_quota_history::HistoryError) -> String {
    error.to_string()
}

fn quota_curve_result_with_reader<R, H>(
    client_id: &str,
    window_key: &str,
    generation: u64,
    before_history: H,
    read_history: R,
) -> Result<serde_json::Value, String>
where
    H: FnOnce(),
    R: FnOnce(
        &agent_quota_history::SeriesKey,
    ) -> Result<
        Option<agent_quota_history::SeriesState>,
        agent_quota_history::HistoryError,
    >,
{
    if client_id.trim().is_empty() {
        return Err("client_id is invalid".to_string());
    }
    if window_key.trim().is_empty() {
        return Err("window_key is invalid".to_string());
    }

    let key = {
        let state = QUOTA_CURVE_BINDINGS
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.generation != 0 && state.generation != generation {
            return Err("quota curve generation is expired".to_string());
        }
        state
            .series
            .get(&(client_id.to_string(), window_key.to_string()))
            .cloned()
            .ok_or_else(|| "quota curve binding is unavailable".to_string())?
    };

    before_history();
    let Some(series) = read_history(&key).map_err(history_error_message)? else {
        return Ok(serde_json::Value::Null);
    };
    if series.samples.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    quota_curve_payload(&series, generation)
}

fn quota_curve_result(
    client_id: &str,
    window_key: &str,
    generation: u64,
) -> Result<serde_json::Value, String> {
    #[cfg(test)]
    if let Some(path) = TEST_QUOTA_CURVE_HISTORY_PATH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return quota_curve_result_with_reader(
            client_id,
            window_key,
            generation,
            || {},
            move |key| {
                agent_quota_history::read_series_at_path(key, &path, chrono::Utc::now().timestamp())
            },
        );
    }

    quota_curve_result_with_reader(
        client_id,
        window_key,
        generation,
        || {},
        |key| agent_quota_history::read_series(key, chrono::Utc::now().timestamp()),
    )
}

unsafe fn required_string_from(value: *const c_char, name: &str) -> Result<String, String> {
    if value.is_null() {
        return Err(format!("{name} is required"));
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map(str::to_string)
        .map_err(|_| format!("{name} is not valid UTF-8"))
}

/// Read an optional year filter from the C side. NULL or empty/whitespace
/// means "all time" (the report modules' empty-string behavior).
///
/// # Safety
/// `year` must be NULL or a valid NUL-terminated string.
unsafe fn year_from(year: *const c_char) -> Result<String, String> {
    if year.is_null() {
        return Ok(String::new());
    }
    unsafe { CStr::from_ptr(year) }
        .to_str()
        .map(str::to_string)
        .map_err(|_| "year filter is not valid UTF-8".to_string())
}

/// Read an optional client filter from the C side: a comma-joined list of
/// canonical client ids. NULL or empty/whitespace means "all clients" (`None`),
/// exactly the pre-filter behavior. Blank entries between commas are dropped.
///
/// # Safety
/// `clients` must be NULL or a valid NUL-terminated string.
unsafe fn clients_from(clients: *const c_char) -> Result<Option<Vec<String>>, String> {
    if clients.is_null() {
        return Ok(None);
    }
    let raw = unsafe { CStr::from_ptr(clients) }
        .to_str()
        .map_err(|_| "client filter is not valid UTF-8".to_string())?;
    let list: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    Ok(if list.is_empty() { None } else { Some(list) })
}

fn graph_cached(year: &str, max_age: Duration) -> Option<serde_json::Value> {
    // Read the entry and release the lock before any filesystem I/O — never hold
    // GRAPH_CACHE across the source-state probe below (mirrors graph_compute,
    // which probes outside the lock too), so concurrent tb_graph callers don't
    // queue behind one another's stat sweep.
    let (fresh_enough, token, data) = {
        let cache = GRAPH_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        let (at, token, data) = cache.get(year)?;
        (at.elapsed() <= max_age, *token, data.clone())
    };
    if fresh_enough {
        return Some(data);
    }
    // Aged out — but if no source state changed since the compute, the graph
    // cannot have changed either. Probe with the lock released, then re-acquire
    // briefly to re-stamp so the next calls inside the oneshot window skip the
    // probe entirely. A lost re-stamp (entry evicted/replaced meanwhile) just
    // degrades to the next call re-probing — benign.
    let context = LocalSourceContext::current();
    let fresh =
        tokscale_core::local_source_change_token(&context.parse_options(None, None)).ok()?;
    if fresh == token {
        let mut cache = GRAPH_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = cache.get_mut(year) {
            entry.0 = Instant::now();
        }
        return Some(data);
    }
    None
}

fn graph_compute(year: &str) -> Result<serde_json::Value, String> {
    // Probe before parsing: a source write or topology change that lands
    // mid-compute changes the token, so the next aged-out read recomputes
    // rather than serving a graph that missed it. Keep the same context for
    // both paths so the probe and report scan observe identical source roots.
    let context = LocalSourceContext::current();
    let token =
        tokscale_core::local_source_change_token(&context.parse_options(None, None)).unwrap_or(0);
    let data = usage_graph::run(&context, year)?;
    GRAPH_CACHE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(year.to_string(), (Instant::now(), token, data.clone()));
    Ok(data)
}

fn lock_tick() -> std::sync::MutexGuard<'static, TickState> {
    TAIL_TICK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Clears the in-flight flag when dropped, on both the success and the panic
/// path (a `TAILER.tick()` that unwinds, caught at the FFI boundary). On panic
/// `last` is left unstamped, so the next poll re-ticks immediately instead of
/// suppressing re-parse for the interval.
struct TickGuard;
impl Drop for TickGuard {
    fn drop(&mut self) {
        lock_tick().in_flight = false;
    }
}

/// Re-parse the live tail if the last *completed* tick is older than
/// `TAIL_TICK_SECS`, unless a re-parse is already running. Single-flight: a
/// second concurrent poller sees `in_flight` (or a fresh `last`) and serves the
/// cached window immediately — it neither blocks on the lock nor launches a
/// duplicate parse that could overwrite a newer one (last-writer-wins). The
/// heavy `TAILER.tick()` runs with no lock held; the stamp is taken only after
/// it completes, so a slow (> `TAIL_TICK_SECS`) parse can't be seen as stale
/// mid-flight, and a tick panic leaves `last` unstamped to retry next call.
fn tail_tick_if_stale() {
    let claimed = {
        let mut st = lock_tick();
        if st.in_flight {
            false
        } else {
            let stale = st
                .last
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(TAIL_TICK_SECS));
            if stale {
                st.in_flight = true;
            }
            stale
        }
    };
    if claimed {
        let _guard = TickGuard; // clears in_flight on drop (success or panic)
        TAILER.tick();
        lock_tick().last = Some(Instant::now()); // success only — panic skips this
    }
}

/// Smoke probe: parse all local clients and report the message count.
/// Proves the staticlib links and tokscale-core can read this machine.
/// (Legacy Phase 0 shape: `{"ok":true,"messages":N}`, no `data` wrapper.)
#[no_mangle]
pub extern "C" fn tb_probe() -> *mut c_char {
    guarded("tb_probe", || {
        let context = LocalSourceContext::current();
        let json = match tokscale_core::parse_local_clients(context.parse_options(None, None)) {
            Ok(pm) => format!(r#"{{"ok":true,"messages":{}}}"#, pm.messages.len()),
            Err(e) => serde_json::json!({"ok": false, "err": e}).to_string(),
        };
        into_raw_json(json)
    })
}

/// Contribution-graph payload (`UsagePayload` in types.ts) for `year`
/// (NULL/empty = all time). Serves a cached payload when one was computed
/// within the last `ONESHOT_MAX_AGE_SECS`.
///
/// # Safety
/// `year` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_graph(year: *const c_char) -> *mut c_char {
    guarded("tb_graph", || {
        envelope(unsafe { year_from(year) }.and_then(|year| {
            if let Some(data) = graph_cached(&year, Duration::from_secs(ONESHOT_MAX_AGE_SECS)) {
                return Ok(data);
            }
            graph_compute(&year)
        }))
    })
}

/// Force-recompute the contribution graph for `year`, bypassing the cache.
///
/// # Safety
/// `year` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_refresh_graph(year: *const c_char) -> *mut c_char {
    guarded("tb_refresh_graph", || {
        envelope(unsafe { year_from(year) }.and_then(|year| graph_compute(&year)))
    })
}

/// Per-model report (`ModelReport` in types.ts) for `year` (NULL/empty = all time).
///
/// # Safety
/// `year` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_model_report(year: *const c_char) -> *mut c_char {
    guarded("tb_model_report", || {
        let context = LocalSourceContext::current();
        envelope(unsafe { year_from(year) }.and_then(|year| model_report::run(&context, &year)))
    })
}

/// Per-hour report (`HourlyReport` in types.ts) for `year` (NULL/empty = all
/// time), restricted to `clients` (NULL/empty = all clients; comma-joined
/// canonical ids otherwise). The filter is applied in the streaming scan so
/// shared-hour buckets carry only the selected clients' totals.
///
/// # Safety
/// `year` and `clients` must each be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_hourly_report(
    year: *const c_char,
    clients: *const c_char,
) -> *mut c_char {
    guarded("tb_hourly_report", || {
        let context = LocalSourceContext::current();
        envelope(unsafe { year_from(year) }.and_then(|year| {
            let clients = unsafe { clients_from(clients) }?;
            hourly_report::run(&context, &year, clients)
        }))
    })
}

/// Per-(sub-)agent report (`AgentsReport` in types.ts) for `year`
/// (NULL/empty = all time), restricted to `clients` (NULL/empty = all clients;
/// comma-joined canonical ids otherwise). The filter is applied in the
/// streaming scan so agent buckets shared across clients carry only the
/// selected clients' totals.
///
/// # Safety
/// `year` and `clients` must each be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_agents_report(
    year: *const c_char,
    clients: *const c_char,
) -> *mut c_char {
    guarded("tb_agents_report", || {
        let context = LocalSourceContext::current();
        envelope(unsafe { year_from(year) }.and_then(|year| {
            let clients = unsafe { clients_from(clients) }?;
            agents_report::run(&context, &year, clients)
        }))
    })
}

/// Source-generation-aware filter parity diagnostic. The graph is always
/// freshly computed to derive the present-client list; the hourly and Agents
/// reports are then bracketed by one opaque local-source token sequence. The
/// payload intentionally exposes only bounded aggregates and classifications,
/// never source paths or raw local data.
#[no_mangle]
pub extern "C" fn tb_filter_parity_probe() -> *mut c_char {
    // Keep this boundary panic-safe without forwarding panic text that could
    // contain a private source path, model, or provider value.
    LazyLock::force(&RAYON_INIT);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let context = LocalSourceContext::current();
        filter_parity_probe::run(&context)
    }));
    match result {
        Ok(Ok(payload)) => envelope(
            serde_json::to_value(payload)
                .map_err(|_| "filter parity probe serialization failed".to_string()),
        ),
        Ok(Err(_)) | Err(_) => envelope(Err("filter parity probe failed".to_string())),
    }
}

/// Live per-(client, agent, model) trace buckets over the trailing
/// `window_secs`. Field names are snake_case (`tokens_per_min`), matching the
/// Tauri `TraceBucket` serialization the frontend consumes.
#[no_mangle]
pub extern "C" fn tb_usage_trace(window_secs: i64) -> *mut c_char {
    guarded("tb_usage_trace", || {
        tail_tick_if_stale();
        envelope(
            serde_json::to_value(TAILER.trace(window_secs))
                .map_err(|e| format!("serialize usage trace: {}", e)),
        )
    })
}

/// Live tokens/min estimate: `{"tokensPerMin": <f32>}`. Same 10-minute-window
/// rate the Tauri `get_tokens_per_min` command reports.
#[no_mangle]
pub extern "C" fn tb_tokens_per_min() -> *mut c_char {
    guarded("tb_tokens_per_min", || {
        tail_tick_if_stale();
        envelope(Ok(
            serde_json::json!({"tokensPerMin": TAILER.rate_in_window(600)}),
        ))
    })
}

/// OAuth quota cards (`AgentUsagePayload` in agentUsage.ts) for
/// codex/claude/antigravity/copilot/grok, fetched concurrently. Network-bound —
/// call from a background thread. Per-provider failures land in each
/// snapshot's `error` field; the call itself only fails on serialization.
/// The publication gate assigns `publicationGeneration` and serializes the
/// provider run, JSON/envelope construction, and pointer creation. The gate is
/// released before this extern function returns, so Swift still needs its own
/// generation guard for caller return/apply order.
#[no_mangle]
pub extern "C" fn tb_agent_usage() -> *mut c_char {
    with_agent_usage_publication_gate(|generation| {
        guarded("tb_agent_usage", || {
            // No outer timeout on purpose: each provider carries its own 30s
            // per-request reqwest timeout (which covers connect, so nothing hangs
            // unbounded), and they run concurrently via tokio::join!. A single outer
            // ceiling would instead collapse the whole payload to one error — losing
            // the providers that already succeeded — and could cut off the legitimate
            // expired-token path (sequential refresh + fetch, up to ~60s).
            let payload = RUNTIME.block_on(agent_usage::run(generation));
            envelope(serialize_agent_usage_payload(generation, &payload))
        })
    })
    .unwrap_or_else(|_| {
        envelope(Err(
            "agent usage publication generation exhausted".to_string()
        ))
    })
}

/// Read-only quota curve snapshot for one bound series. This path performs no
/// network request and never opens a save transaction; the history loader keeps
/// its existing quarantine-on-corrupt-file behavior.
#[no_mangle]
pub unsafe extern "C" fn tb_quota_curve(
    client_id: *const c_char,
    window_key: *const c_char,
    generation: u64,
) -> *mut c_char {
    guarded("tb_quota_curve", || {
        let result = unsafe { required_string_from(client_id, "client_id") }.and_then(|client| {
            unsafe { required_string_from(window_key, "window_key") }
                .and_then(|window| quota_curve_result(&client, &window, generation))
        });
        envelope(result)
    })
}

/// Release a string returned by any tb_* entry point.
///
/// # Safety
/// `p` must be a pointer previously returned by this library (or null).
#[no_mangle]
pub unsafe extern "C" fn tb_free(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            let _ = CString::from_raw(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use usage_tail::UsageTailer;

    static QUOTA_CURVE_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn select_user_home_prefers_non_empty_home() {
        let home = PathBuf::from("env-home");
        let platform_home = PathBuf::from("platform-home");
        assert_eq!(
            select_user_home(Some(home.clone()), Some(platform_home)),
            Some(home)
        );
    }

    #[test]
    fn select_user_home_uses_platform_fallback_for_missing_or_empty_home() {
        let platform_home = PathBuf::from("platform-home");
        assert_eq!(
            select_user_home(None, Some(platform_home.clone())),
            Some(platform_home.clone())
        );
        assert_eq!(
            select_user_home(Some(PathBuf::new()), Some(platform_home.clone())),
            Some(platform_home)
        );
    }

    #[test]
    fn select_user_home_returns_none_without_candidates() {
        assert_eq!(select_user_home(None, None), None);
    }

    #[test]
    fn local_source_context_builders_preserve_home_filters_and_env_roots() {
        let platform_home = PathBuf::from("platform-home");
        let context = LocalSourceContext {
            home_dir: select_user_home(None, Some(platform_home.clone())),
        };
        let year = Some("2026".to_string());
        let clients = Some(vec!["claude".to_string(), "codex".to_string()]);

        let report = context.report_options(year.clone(), clients.clone());
        let parse = context.parse_options(year.clone(), clients.clone());
        let expected_home = Some(platform_home.to_string_lossy().into_owned());

        assert_eq!(report.home_dir, expected_home);
        assert_eq!(parse.home_dir, expected_home);
        assert!(report.use_env_roots);
        assert!(parse.use_env_roots);
        assert_eq!(report.year, year);
        assert_eq!(parse.year, year);
        assert_eq!(report.clients, clients);
        assert_eq!(parse.clients, clients);
    }

    /// Read a heap JSON pointer into an owned String and free it — the test-side
    /// equivalent of Swift's `decode`/`tb_free`.
    unsafe fn take(p: *mut c_char) -> String {
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { tb_free(p) };
        s
    }

    fn quota_curve_test_guard() -> std::sync::MutexGuard<'static, ()> {
        QUOTA_CURVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset_quota_curve_bindings() {
        replace_quota_curve_bindings(0, Vec::new());
    }

    fn quota_curve_key(account_scope: &str) -> agent_quota_history::SeriesKey {
        agent_quota_history::SeriesKey::new("codex", account_scope, "weekly.v1")
    }

    fn quota_curve_temp_path(label: &str) -> (PathBuf, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "tokenbar-quota-curve-{}-{nonce}-{label}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create quota curve fixture directory");
        (
            directory.clone(),
            directory.join(agent_quota_history::HISTORY_FILE_NAME),
        )
    }

    fn record_quota_curve_sample(
        path: &std::path::Path,
        key: agent_quota_history::SeriesKey,
        reset_at: i64,
        used_percent: f64,
        now: i64,
        duration_seconds: i64,
    ) {
        agent_quota_history::record_observation_at_path(
            key,
            Some(reset_at),
            used_percent,
            now,
            Some(agent_quota_duration::DurationEvidence::provider(
                reset_at,
                duration_seconds,
            )),
            None,
            path,
        )
        .expect("record quota curve fixture sample");
    }

    fn quota_curve_value_at_path(
        path: &std::path::Path,
        client_id: &str,
        window_key: &str,
        generation: u64,
        now: i64,
    ) -> Result<serde_json::Value, String> {
        quota_curve_result_with_reader(
            client_id,
            window_key,
            generation,
            || {},
            |key| agent_quota_history::read_series_at_path(key, path, now),
        )
    }

    fn set_test_quota_curve_history_path(path: Option<PathBuf>) {
        *TEST_QUOTA_CURVE_HISTORY_PATH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = path;
    }

    #[test]
    fn quota_curve_payload_keeps_raw_points_and_wire_whitelist() {
        let _test_guard = quota_curve_test_guard();
        let series = agent_quota_history::SeriesState {
            provider_id: "codex".to_string(),
            account_scope: "opaque-account".to_string(),
            window_key: "weekly.v1".to_string(),
            active_reset_at: Some(1_000_500),
            last_activity_at: 1_000_450,
            rollover: None,
            samples: vec![
                agent_quota_history::QuotaSample {
                    reset_at: 1_000_000,
                    duration_seconds: 96,
                    duration_source: agent_quota_duration::DurationSource::Provider,
                    used_percent: 12.5,
                    // Elapsed 3 into a 96s window, deliberately: elapsed 1 sat on
                    // a point that a 169-grid regrid maps back to itself, so the
                    // exact-equality assertions below could not see one.
                    sampled_at: 999_907,
                    origin: agent_quota_history::SampleOrigin::LiveV3,
                },
                agent_quota_history::QuotaSample {
                    reset_at: 1_000_000,
                    duration_seconds: 120,
                    duration_source: agent_quota_duration::DurationSource::Contract,
                    used_percent: 55.0,
                    sampled_at: 999_945,
                    origin: agent_quota_history::SampleOrigin::ImportedV2,
                },
                agent_quota_history::QuotaSample {
                    reset_at: 1_000_500,
                    duration_seconds: 96,
                    duration_source: agent_quota_duration::DurationSource::Observed,
                    used_percent: 3.0,
                    sampled_at: 1_000_405,
                    origin: agent_quota_history::SampleOrigin::LiveV3,
                },
            ],
        };

        let value = quota_curve_payload(&series, 7).expect("serialize curve payload");
        let object = value.as_object().expect("curve payload object");
        assert_eq!(
            object
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["activeResetAt", "coverage", "generation", "points"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
        // Serialization is camelCase, so a leaked field would read `accountScope`;
        // checking only the snake_case spelling would never see it. Check both.
        let serialized = value.to_string();
        assert!(!serialized.contains("account_scope"), "{serialized}");
        assert!(!serialized.contains("accountScope"), "{serialized}");
        // Exact-whitelist `coverage` too: without it, a leak nested one level
        // down satisfies every other assertion here.
        assert_eq!(
            value["coverage"]
                .as_object()
                .expect("coverage object")
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["newestSampledAt", "oldestSampledAt", "sampleCount"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
        assert!(value.get("cycles").is_none());
        assert_eq!(value["generation"], 7);
        assert_eq!(value["activeResetAt"], 1_000_500);
        assert_eq!(value["coverage"]["oldestSampledAt"], 999_907);
        assert_eq!(value["coverage"]["newestSampledAt"], 1_000_405);
        assert_eq!(value["coverage"]["sampleCount"], 3);
        assert_eq!(value["points"][0]["sampledAt"], 999_907);
        assert_eq!(value["points"][0]["durationSeconds"], 96);
        assert_eq!(value["points"][0]["durationSource"], "provider");
        assert_eq!(value["points"][1]["sampledAt"], 999_945);
        assert_eq!(value["points"][1]["durationSeconds"], 120);
        assert_eq!(value["points"][1]["durationSource"], "contract");
        assert_eq!(value["points"][2]["sampledAt"], 1_000_405);
        assert_eq!(value["points"][2]["usedPercent"], 3.0);
        for point in value["points"].as_array().expect("points array") {
            assert_eq!(
                point
                    .as_object()
                    .expect("point object")
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>(),
                [
                    "durationSeconds",
                    "durationSource",
                    "origin",
                    "resetAt",
                    "sampledAt",
                    "usedPercent"
                ]
                .into_iter()
                .map(str::to_string)
                .collect()
            );
        }
    }

    #[test]
    fn quota_curve_binding_selects_each_account_without_store_scanning() {
        let _test_guard = quota_curve_test_guard();
        reset_quota_curve_bindings();
        let (directory, path) = quota_curve_temp_path("binding-authority");
        let now = 9_000_000;
        let reset_at = now + 96;
        record_quota_curve_sample(&path, quota_curve_key("account-a"), reset_at, 10.0, now, 96);
        record_quota_curve_sample(&path, quota_curve_key("account-b"), reset_at, 80.0, now, 96);

        replace_quota_curve_bindings(1, vec![quota_curve_key("account-a")]);
        let account_a = quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now)
            .expect("account A curve");
        assert_eq!(account_a["points"][0]["usedPercent"], 10.0);

        replace_quota_curve_bindings(2, vec![quota_curve_key("account-b")]);
        let account_b = quota_curve_value_at_path(&path, "codex", "weekly.v1", 2, now)
            .expect("account B curve");
        assert_eq!(account_b["points"][0]["usedPercent"], 80.0);
        assert_eq!(
            quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now).unwrap_err(),
            "quota curve generation is expired"
        );
        fs::remove_dir_all(directory).expect("remove binding fixture");
    }

    #[test]
    fn quota_curve_binding_lifecycle_and_serialization_failure_fail_closed() {
        let _test_guard = quota_curve_test_guard();
        reset_quota_curve_bindings();
        let (directory, path) = quota_curve_temp_path("binding-lifecycle");
        let now = 9_100_000;
        let reset_at = now + 96;
        let account_a = quota_curve_key("account-a");
        let account_b = quota_curve_key("account-b");
        record_quota_curve_sample(&path, account_a.clone(), reset_at, 10.0, now, 96);
        record_quota_curve_sample(&path, account_b, reset_at, 20.0, now, 96);
        replace_quota_curve_bindings(1, vec![account_a.clone()]);
        let before = quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now)
            .expect("previous tuple remains available");

        let serialization =
            serialize_agent_usage_with_bindings(2, vec![quota_curve_key("account-b")], || {
                Err("injected serialization failure".to_string())
            });
        assert_eq!(
            serialization,
            Err("injected serialization failure".to_string())
        );
        assert_eq!(
            quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now)
                .expect("failed publication preserves previous tuple"),
            before
        );
        assert_eq!(
            quota_curve_value_at_path(&path, "codex", "weekly.v1", 2, now).unwrap_err(),
            "quota curve generation is expired"
        );

        // Whether a given snapshot becomes a candidate is decided by
        // `AgentUsagePayload::quota_curve_series` and is asserted there against
        // real snapshots; handing an empty vector here would prove nothing about
        // that filter. What this asserts is only the consequence: a generation
        // that bound nothing serves nothing, and it also invalidates its
        // predecessor.
        replace_quota_curve_bindings(2, Vec::new());
        assert_eq!(
            quota_curve_value_at_path(&path, "codex", "weekly.v1", 2, now).unwrap_err(),
            "quota curve binding is unavailable"
        );
        assert_eq!(
            quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now).unwrap_err(),
            "quota curve generation is expired"
        );

        // Process restart clears the non-persistent table even though history remains.
        reset_quota_curve_bindings();
        assert_eq!(
            quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now).unwrap_err(),
            "quota curve binding is unavailable"
        );
        fs::remove_dir_all(directory).expect("remove lifecycle fixture");
    }

    #[test]
    fn quota_curve_reader_drops_binding_lock_before_history_io() {
        let _test_guard = quota_curve_test_guard();
        reset_quota_curve_bindings();
        let (directory, path) = quota_curve_temp_path("lock-order");
        let now = 9_200_000;
        let reset_at = now + 96;
        let key = quota_curve_key("account-a");
        record_quota_curve_sample(&path, key.clone(), reset_at, 10.0, now, 96);
        replace_quota_curve_bindings(1, vec![key]);
        let (paused_tx, paused_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (committed_tx, committed_rx) = mpsc::channel();
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            quota_curve_result_with_reader(
                "codex",
                "weekly.v1",
                1,
                || {
                    paused_tx.send(()).expect("pause reader");
                    release_rx.recv().expect("release reader");
                },
                |key| agent_quota_history::read_series_at_path(key, &reader_path, now),
            )
        });
        paused_rx.recv().expect("reader reached history boundary");
        let writer = std::thread::spawn(move || {
            replace_quota_curve_bindings(2, vec![quota_curve_key("account-b")]);
            committed_tx.send(()).expect("commit binding tuple");
        });
        committed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("binding writer must not wait for history I/O");
        release_tx.send(()).expect("release history reader");
        writer.join().expect("binding writer join");
        reader
            .join()
            .expect("history reader join")
            .expect("history reader result");
        fs::remove_dir_all(directory).expect("remove lock-order fixture");
    }

    #[test]
    fn quota_curve_binding_tuple_is_atomically_visible() {
        let _test_guard = quota_curve_test_guard();
        reset_quota_curve_bindings();
        replace_quota_curve_bindings(1, vec![quota_curve_key("account-a")]);
        let (start_tx, start_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            start_rx.recv().expect("start tuple replacement");
            replace_quota_curve_bindings(2, vec![quota_curve_key("account-b")]);
        });
        // Without requiring generation 2 to actually be observed, every read can
        // land on generation 1 and the loop proves nothing about the interval
        // where a split update would be visible.
        let mut saw_new = false;
        for index in 0..10_000 {
            let state = QUOTA_CURVE_BINDINGS
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.generation == 2 {
                saw_new = true;
            }
            match state.generation {
                1 => assert_eq!(
                    state
                        .series
                        .get(&("codex".to_string(), "weekly.v1".to_string()))
                        .map(|key| key.account_scope.as_str()),
                    Some("account-a")
                ),
                2 => assert_eq!(
                    state
                        .series
                        .get(&("codex".to_string(), "weekly.v1".to_string()))
                        .map(|key| key.account_scope.as_str()),
                    Some("account-b")
                ),
                generation => panic!("partial or invalid tuple generation {generation}"),
            }
            if index == 10 {
                start_tx.send(()).expect("trigger tuple replacement");
            }
        }
        start_tx.send(()).ok();
        writer.join().expect("tuple writer join");
        // Drain past the handover so the assertion below cannot pass on timing.
        for _ in 0..1_000 {
            let state = QUOTA_CURVE_BINDINGS
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.generation == 2 {
                saw_new = true;
                assert_eq!(
                    state
                        .series
                        .get(&("codex".to_string(), "weekly.v1".to_string()))
                        .map(|key| key.account_scope.as_str()),
                    Some("account-b")
                );
            }
        }
        assert!(saw_new, "reader never observed the replacement tuple");
    }

    #[test]
    fn quota_curve_read_path_never_reaches_atomic_save() {
        let _test_guard = quota_curve_test_guard();
        reset_quota_curve_bindings();
        let (directory, path) = quota_curve_temp_path("read-only");
        let now = 9_300_000;
        let reset_at = now + 96;
        let key = quota_curve_key("account-a");
        record_quota_curve_sample(&path, key.clone(), reset_at, 10.0, now, 96);
        let before_bytes = fs::read(&path).expect("read fixture bytes");
        agent_quota_history::reset_save_call_count();
        replace_quota_curve_bindings(1, vec![key]);
        quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now).expect("read quota curve");
        assert_eq!(agent_quota_history::save_call_count(), 0);
        assert_eq!(
            fs::read(&path).expect("read bytes after curve"),
            before_bytes
        );
        fs::remove_dir_all(directory).expect("remove read-only fixture");
    }

    #[test]
    fn quota_curve_distinguishes_absent_storage_and_corrupt_history() {
        let _test_guard = quota_curve_test_guard();
        reset_quota_curve_bindings();
        let (directory, path) = quota_curve_temp_path("errors");
        let now = 9_400_000;
        let key = quota_curve_key("account-a");
        replace_quota_curve_bindings(1, vec![key]);
        assert_eq!(
            quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now)
                .expect("missing history is a successful empty result"),
            serde_json::Value::Null
        );

        let storage_marker = directory.join("not-a-directory");
        fs::write(&storage_marker, b"marker").expect("create storage marker");
        let storage_path = storage_marker.join(agent_quota_history::HISTORY_FILE_NAME);
        assert_eq!(
            quota_curve_value_at_path(&storage_path, "codex", "weekly.v1", 1, now).unwrap_err(),
            "quota pace storage is unavailable"
        );

        fs::write(&path, b"{not-json").expect("write corrupt history");
        assert_eq!(
            quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now).unwrap_err(),
            "quota pace history is corrupt and was quarantined"
        );
        assert!(directory
            .read_dir()
            .expect("read quarantine directory")
            .flatten()
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("quota-pace-history-v3.corrupt-")));
        fs::remove_dir_all(directory).expect("remove error fixture");
    }

    #[test]
    fn quota_curve_c_abi_handles_success_expiry_null_and_invalid_utf8() {
        let _test_guard = quota_curve_test_guard();
        reset_quota_curve_bindings();
        let (directory, path) = quota_curve_temp_path("c-abi");
        let now = 9_500_000;
        let reset_at = now + 96;
        record_quota_curve_sample(&path, quota_curve_key("account-a"), reset_at, 10.0, now, 96);
        replace_quota_curve_bindings(1, vec![quota_curve_key("account-a")]);
        let (empty_directory, empty_path) = quota_curve_temp_path("c-abi-empty");
        set_test_quota_curve_history_path(Some(empty_path));
        let client = CString::new("codex").expect("client id");
        let window = CString::new("weekly.v1").expect("window key");
        let no_history = unsafe { take(tb_quota_curve(client.as_ptr(), window.as_ptr(), 1)) };
        let no_history_json: serde_json::Value =
            serde_json::from_str(&no_history).expect("decode C ABI no-history result");
        assert_eq!(no_history_json["ok"], true);
        assert!(no_history_json["data"].is_null());

        set_test_quota_curve_history_path(Some(path.clone()));
        let success = unsafe { take(tb_quota_curve(client.as_ptr(), window.as_ptr(), 1)) };
        let success_json: serde_json::Value =
            serde_json::from_str(&success).expect("decode C ABI success");
        assert_eq!(success_json["ok"], true);
        assert_eq!(success_json["data"]["points"][0]["usedPercent"], 10.0);

        let expired = unsafe { take(tb_quota_curve(client.as_ptr(), window.as_ptr(), 0)) };
        assert!(expired.contains("quota curve generation is expired"));
        let null_client = unsafe { take(tb_quota_curve(std::ptr::null(), window.as_ptr(), 1)) };
        assert!(null_client.contains("client_id is required"));
        let invalid_client = [0xff_u8, 0];
        let invalid_utf8 = unsafe {
            take(tb_quota_curve(
                invalid_client.as_ptr().cast(),
                window.as_ptr(),
                1,
            ))
        };
        assert!(invalid_utf8.contains("client_id is not valid UTF-8"));
        set_test_quota_curve_history_path(None);
        fs::remove_dir_all(empty_directory).expect("remove C ABI empty fixture");
        fs::remove_dir_all(directory).expect("remove C ABI fixture");
    }

    /// Ignored by default: ~875s, which is the whole crate's test time times
    /// two hundred. The cost is inherent to what it proves — the fixture must
    /// go through the production writer, and every one of its 6,192 samples
    /// pays a full load, validate, merge, retention and atomic-save cycle
    /// against a file that keeps growing, so the run is quadratic in file I/O.
    /// Building it any other way would bypass the retention policy this test
    /// exists to exercise, and shrinking it would let a snapshot cap between
    /// the fixture size and the true maximum pass unnoticed.
    ///
    /// Run it explicitly when touching the snapshot or retention paths:
    ///   cargo test -p tb_core_ffi -- --ignored complete_production_retention
    #[test]
    #[ignore = "~875s; run explicitly when touching snapshot or retention paths"]
    fn quota_curve_returns_the_complete_production_retention_sequence() {
        let _test_guard = quota_curve_test_guard();
        reset_quota_curve_bindings();
        let (directory, path) = quota_curve_temp_path("retention-sequence");
        let duration_seconds = 96_i64;
        let base_reset_at = 30_000_000_i64;
        let now = base_reset_at + 128 * 120;
        let key = quota_curve_key("account-retained");

        for group in 0..=128_i64 {
            let reset_at = base_reset_at + (group - 128) * 120;
            for bucket in 0..48_i64 {
                let sampled_at = reset_at - duration_seconds + 2 * bucket + 1;
                record_quota_curve_sample(
                    &path,
                    key.clone(),
                    reset_at,
                    (bucket + 1) as f64,
                    sampled_at,
                    duration_seconds,
                );
            }
        }

        let store: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read retained store"))
                .expect("decode retained store");
        let store_sample_count: usize = store["series"]
            .as_array()
            .expect("series array")
            .iter()
            .map(|series| series["samples"].as_array().expect("samples array").len())
            .sum();
        assert_eq!(store_sample_count, 6_192);

        replace_quota_curve_bindings(1, vec![key]);
        let payload = quota_curve_value_at_path(&path, "codex", "weekly.v1", 1, now)
            .expect("read complete retained sequence");
        assert_eq!(
            payload["points"].as_array().unwrap().len(),
            store_sample_count
        );
        assert_eq!(payload["coverage"]["sampleCount"], store_sample_count);
        assert_eq!(
            payload["points"][0]["sampledAt"],
            base_reset_at - 128 * 120 - 95
        );
        assert_eq!(
            payload["points"][1]["sampledAt"],
            base_reset_at - 128 * 120 - 93
        );
        assert_eq!(
            payload["points"].as_array().unwrap().last().unwrap()["usedPercent"],
            48.0
        );
        fs::remove_dir_all(directory).expect("remove retention fixture");
    }

    #[test]
    fn guarded_passes_success_through() {
        let p = guarded("tb_test", || envelope(Ok(serde_json::json!({"v": 1}))));
        let s = unsafe { take(p) };
        assert!(s.contains(r#""ok":true"#), "got: {s}");
        assert!(s.contains(r#""v":1"#), "got: {s}");
    }

    #[test]
    fn guarded_converts_panic_to_error_envelope() {
        // The whole point of the unwind + catch_unwind stance: a panic inside an
        // entry-point body must NOT unwind across the C ABI (which would abort the
        // process). It is caught and returned as {"ok":false,...} so one card
        // fails while the rest of the app keeps running.
        let p = guarded("tb_test", || panic!("boom"));
        let s = unsafe { take(p) };
        assert!(s.contains(r#""ok":false"#), "got: {s}");
        assert!(s.contains("tb_test panicked: boom"), "got: {s}");
    }

    #[test]
    fn publication_gate_serializes_pointer_publication() {
        use std::sync::{mpsc, Arc, Barrier};
        use std::thread;

        let gate = Arc::new(Mutex::new(PublicationGate::default()));
        let (events_tx, events_rx) = mpsc::channel();
        let (release_a_tx, release_a_rx) = mpsc::channel();
        let (b_called_tx, b_called_rx) = mpsc::channel();
        let a_ready = Arc::new(Barrier::new(2));

        let a_gate = Arc::clone(&gate);
        let a_ready_thread = Arc::clone(&a_ready);
        let a_events = events_tx.clone();
        let a = thread::spawn(move || {
            with_publication_gate(a_gate.as_ref(), |_| {
                a_events.send("A run").unwrap();
                a_ready_thread.wait();
                release_a_rx.recv().unwrap();

                let pointer = CString::new("A").unwrap().into_raw();
                a_events.send("A publish").unwrap();
                unsafe { tb_free(pointer) };
            })
            .unwrap();
        });
        a_ready.wait();

        let b_gate = Arc::clone(&gate);
        let b_events = events_tx.clone();
        let b = thread::spawn(move || {
            b_called_tx.send(()).unwrap();
            with_publication_gate(b_gate.as_ref(), |_| {
                b_events.send("B run").unwrap();
                let pointer = CString::new("B").unwrap().into_raw();
                b_events.send("B publish").unwrap();
                unsafe { tb_free(pointer) };
            })
            .unwrap();
        });
        b_called_rx.recv().unwrap();
        release_a_tx.send(()).unwrap();

        a.join().unwrap();
        b.join().unwrap();
        drop(events_tx);

        let events: Vec<_> = events_rx.iter().collect();
        assert_eq!(events, vec!["A run", "A publish", "B run", "B publish"]);
    }

    #[test]
    fn publication_generation_handles_return_order_reversal() {
        use std::sync::{mpsc, Arc};
        use std::thread;

        let gate = Arc::new(Mutex::new(PublicationGate::default()));
        let (a_gate_returned_tx, a_gate_returned_rx) = mpsc::channel();
        let (release_a_tx, release_a_rx) = mpsc::channel();
        let (returns_tx, returns_rx) = mpsc::channel();

        let a_gate = Arc::clone(&gate);
        let a_returns = returns_tx.clone();
        let a = thread::spawn(move || {
            let (generation, pointer) = with_publication_gate(a_gate.as_ref(), |generation| {
                let pointer = CString::new(format!(
                    "{{\"run\":\"A\",\"generation\":{generation},\"result\":\"success\"}}"
                ))
                .unwrap()
                .into_raw();
                (generation, pointer)
            })
            .unwrap();
            // The gate is released here. Pause before the wrapper records the
            // return so B can publish and return first.
            a_gate_returned_tx.send(generation).unwrap();
            release_a_rx.recv().unwrap();
            let payload = unsafe { take(pointer) };
            a_returns.send(("A returned", generation, payload)).unwrap();
        });

        assert_eq!(a_gate_returned_rx.recv().unwrap(), 1);

        let b_gate = Arc::clone(&gate);
        let b_returns = returns_tx.clone();
        let b = thread::spawn(move || {
            let (generation, pointer) = with_publication_gate(b_gate.as_ref(), |generation| {
                let pointer = CString::new(format!(
                    "{{\"run\":\"B\",\"generation\":{generation},\"result\":\"terminal\"}}"
                ))
                .unwrap()
                .into_raw();
                (generation, pointer)
            })
            .unwrap();
            let payload = unsafe { take(pointer) };
            b_returns.send(("B returned", generation, payload)).unwrap();
        });

        let b_return = returns_rx.recv().unwrap();
        assert_eq!(b_return.0, "B returned");
        assert_eq!(b_return.1, 2);
        assert_eq!(
            b_return.2,
            r#"{"run":"B","generation":2,"result":"terminal"}"#
        );

        release_a_tx.send(()).unwrap();
        let a_return = returns_rx.recv().unwrap();
        assert_eq!(a_return.0, "A returned");
        assert_eq!(a_return.1, 1);
        assert_eq!(
            a_return.2,
            r#"{"run":"A","generation":1,"result":"success"}"#
        );

        a.join().unwrap();
        b.join().unwrap();
    }

    #[test]
    fn publication_generation_exhaustion_fails_closed() {
        let gate = Mutex::new(PublicationGate {
            generation: u64::MAX - 1,
        });
        assert_eq!(
            with_publication_gate(&gate, |generation| generation).unwrap(),
            u64::MAX
        );

        let mut body_called = false;
        let exhausted = with_publication_gate(&gate, |generation| {
            body_called = true;
            generation
        });
        assert_eq!(exhausted, Err(PublicationGenerationExhausted));
        assert!(
            !body_called,
            "exhaustion must not publish a duplicate generation"
        );
    }

    #[test]
    fn publication_gate_keeps_panic_envelope_before_next_run() {
        use std::sync::{mpsc, Arc, Barrier};
        use std::thread;

        let gate = Arc::new(Mutex::new(PublicationGate::default()));
        let (events_tx, events_rx) = mpsc::channel();
        let (release_a_tx, release_a_rx) = mpsc::channel();
        let (b_called_tx, b_called_rx) = mpsc::channel();
        let a_ready = Arc::new(Barrier::new(2));

        let a_gate = Arc::clone(&gate);
        let a_ready_thread = Arc::clone(&a_ready);
        let a_events = events_tx.clone();
        let a = thread::spawn(move || {
            with_publication_gate(a_gate.as_ref(), |_| {
                let pointer = guarded("tb_test", || {
                    a_events.send("A run").unwrap();
                    a_ready_thread.wait();
                    release_a_rx.recv().unwrap();
                    panic!("boom");
                });
                a_events.send("A panic publish").unwrap();
                unsafe { tb_free(pointer) };
            })
            .unwrap();
        });
        a_ready.wait();

        let b_gate = Arc::clone(&gate);
        let b_events = events_tx.clone();
        let b = thread::spawn(move || {
            b_called_tx.send(()).unwrap();
            with_publication_gate(b_gate.as_ref(), |_| {
                b_events.send("B run").unwrap();
                let pointer = CString::new("B").unwrap().into_raw();
                b_events.send("B publish").unwrap();
                unsafe { tb_free(pointer) };
            })
            .unwrap();
        });
        b_called_rx.recv().unwrap();
        release_a_tx.send(()).unwrap();

        a.join().unwrap();
        b.join().unwrap();
        drop(events_tx);

        let events: Vec<_> = events_rx.iter().collect();
        assert_eq!(
            events,
            vec!["A run", "A panic publish", "B run", "B publish"]
        );
    }

    #[test]
    fn tick_guard_clears_in_flight_without_stamping_on_panic() {
        // Simulates a panic during TAILER.tick(): the guard, dropped mid-unwind,
        // must clear in_flight (so a later poll can re-tick) and must NOT stamp
        // `last` (so the tick is retried rather than suppressed for the interval).
        {
            let mut st = lock_tick();
            st.in_flight = true;
            st.last = None;
        }
        drop(TickGuard);
        let st = lock_tick();
        assert!(!st.in_flight);
        assert!(st.last.is_none()); // unstamped → next poll re-ticks
    }

    #[test]
    fn trace_rejects_nonpositive_window() {
        // window_secs <= 0 yields no buckets instead of an overflowed cutoff.
        let tail = UsageTailer::new();
        assert!(tail.trace(0).is_empty());
        assert!(tail.trace(-5).is_empty());
        // A pathological window must saturate, not panic/overflow.
        assert!(tail.trace(i64::MAX).is_empty());
    }
}
