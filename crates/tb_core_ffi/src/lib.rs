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
mod agent_history;
mod agent_quota_duration;
mod agent_quota_history;
mod agent_usage;
mod agents_report;
mod hourly_report;
mod model_report;
mod opencode_integrations;
mod usage_graph;
mod usage_tail;

use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
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
/// `AssertUnwindSafe` is sound here. State shared across calls is the three
/// std::sync Mutex statics (GRAPH_CACHE / TAIL_TICK / CLAUDE_USAGE_GATE), each
/// recovered from poison on the next lock via `into_inner()`, plus the live
/// tail's parking_lot Mutexes, which never poison and release cleanly on unwind.
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
#[no_mangle]
pub extern "C" fn tb_agent_usage() -> *mut c_char {
    guarded("tb_agent_usage", || {
        // No outer timeout on purpose: each provider carries its own 30s
        // per-request reqwest timeout (which covers connect, so nothing hangs
        // unbounded), and they run concurrently via tokio::join!. A single outer
        // ceiling would instead collapse the whole payload to one error — losing
        // the providers that already succeeded — and could cut off the legitimate
        // expired-token path (sequential refresh + fetch, up to ~60s).
        let payload = RUNTIME.block_on(agent_usage::run());
        envelope(serde_json::to_value(payload).map_err(|e| format!("serialize agent usage: {}", e)))
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
    use usage_tail::UsageTailer;

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
