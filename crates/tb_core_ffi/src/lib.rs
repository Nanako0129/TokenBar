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

mod agent_antigravity;
mod agent_copilot;
mod agent_history;
mod agent_usage;
mod agents_report;
mod hourly_report;
mod model_report;
mod opencode_integrations;
mod usage_graph;
mod usage_tail;

use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use usage_tail::UsageTailer;

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

/// year → (computed-at, source token, mapped graph payload). Same role as
/// the Tauri AppState cache, plus a change token: when the cache entry ages
/// past the oneshot window but `latest_source_mtime_ms` still matches the
/// token, the entry is re-stamped and served — an idle machine never pays
/// for a full re-aggregation just because time passed.
type GraphCacheEntry = (Instant, u64, serde_json::Value);
static GRAPH_CACHE: LazyLock<Mutex<HashMap<String, GraphCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static TAILER: LazyLock<UsageTailer> = LazyLock::new(UsageTailer::new);
static TAIL_LAST_TICK: Mutex<Option<Instant>> = Mutex::new(None);

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

fn graph_cached(year: &str, max_age: Duration) -> Option<serde_json::Value> {
    let mut cache = GRAPH_CACHE.lock().ok()?;
    let (at, token, data) = cache.get_mut(year)?;
    if at.elapsed() <= max_age {
        return Some(data.clone());
    }
    // Aged out — but if no source file changed since the compute, the graph
    // cannot have changed either. Re-stamp so the next calls inside the
    // oneshot window skip the probe entirely.
    let fresh = tokscale_core::latest_source_mtime_ms(&Default::default()).ok()?;
    if fresh == *token {
        *at = Instant::now();
        return Some(data.clone());
    }
    None
}

fn graph_compute(year: &str) -> Result<serde_json::Value, String> {
    // Probe before parsing: a write that lands mid-compute moves the mtime
    // past this token, so the next aged-out read recomputes rather than
    // serving a graph that missed it.
    let token = tokscale_core::latest_source_mtime_ms(&Default::default()).unwrap_or(0);
    let data = usage_graph::run(year)?;
    if let Ok(mut cache) = GRAPH_CACHE.lock() {
        cache.insert(year.to_string(), (Instant::now(), token, data.clone()));
    }
    Ok(data)
}

/// Re-parse the live tail if the last tick is stale (or never happened),
/// otherwise leave the cached event window in place.
fn tail_tick_if_stale() {
    let mut last = match TAIL_LAST_TICK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let stale = last.is_none_or(|at| at.elapsed() >= Duration::from_secs(TAIL_TICK_SECS));
    if stale {
        TAILER.tick();
        *last = Some(Instant::now());
    }
}

/// Smoke probe: parse all local clients and report the message count.
/// Proves the staticlib links and tokscale-core can read this machine.
/// (Legacy Phase 0 shape: `{"ok":true,"messages":N}`, no `data` wrapper.)
#[no_mangle]
pub extern "C" fn tb_probe() -> *mut c_char {
    let opts = tokscale_core::LocalParseOptions::default();
    let json = match tokscale_core::parse_local_clients(opts) {
        Ok(pm) => format!(r#"{{"ok":true,"messages":{}}}"#, pm.messages.len()),
        Err(e) => serde_json::json!({"ok": false, "err": e}).to_string(),
    };
    into_raw_json(json)
}

/// Contribution-graph payload (`UsagePayload` in types.ts) for `year`
/// (NULL/empty = all time). Serves a cached payload when one was computed
/// within the last `ONESHOT_MAX_AGE_SECS`.
///
/// # Safety
/// `year` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_graph(year: *const c_char) -> *mut c_char {
    envelope(unsafe { year_from(year) }.and_then(|year| {
        if let Some(data) = graph_cached(&year, Duration::from_secs(ONESHOT_MAX_AGE_SECS)) {
            return Ok(data);
        }
        graph_compute(&year)
    }))
}

/// Force-recompute the contribution graph for `year`, bypassing the cache.
///
/// # Safety
/// `year` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_refresh_graph(year: *const c_char) -> *mut c_char {
    envelope(unsafe { year_from(year) }.and_then(|year| graph_compute(&year)))
}

/// Per-model report (`ModelReport` in types.ts) for `year` (NULL/empty = all time).
///
/// # Safety
/// `year` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_model_report(year: *const c_char) -> *mut c_char {
    envelope(unsafe { year_from(year) }.and_then(|year| model_report::run(&year)))
}

/// Per-hour report (`HourlyReport` in types.ts) for `year` (NULL/empty = all time).
///
/// # Safety
/// `year` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_hourly_report(year: *const c_char) -> *mut c_char {
    envelope(unsafe { year_from(year) }.and_then(|year| hourly_report::run(&year)))
}

/// Per-(sub-)agent report (`AgentsReport` in types.ts) for `year`
/// (NULL/empty = all time).
///
/// # Safety
/// `year` must be NULL or a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn tb_agents_report(year: *const c_char) -> *mut c_char {
    envelope(unsafe { year_from(year) }.and_then(|year| agents_report::run(&year)))
}

/// Live per-(client, agent, model) trace buckets over the trailing
/// `window_secs`. Field names are snake_case (`tokens_per_min`), matching the
/// Tauri `TraceBucket` serialization the frontend consumes.
#[no_mangle]
pub extern "C" fn tb_usage_trace(window_secs: i64) -> *mut c_char {
    tail_tick_if_stale();
    envelope(
        serde_json::to_value(TAILER.trace(window_secs))
            .map_err(|e| format!("serialize usage trace: {}", e)),
    )
}

/// Live tokens/min estimate: `{"tokensPerMin": <f32>}`. Same 10-minute-window
/// rate the Tauri `get_tokens_per_min` command reports.
#[no_mangle]
pub extern "C" fn tb_tokens_per_min() -> *mut c_char {
    tail_tick_if_stale();
    envelope(Ok(
        serde_json::json!({"tokensPerMin": TAILER.rate_in_window(600)}),
    ))
}

/// OAuth quota cards (`AgentUsagePayload` in agentUsage.ts) for
/// codex/claude/antigravity/copilot, fetched concurrently. Network-bound —
/// call from a background thread. Per-provider failures land in each
/// snapshot's `error` field; the call itself only fails on serialization.
#[no_mangle]
pub extern "C" fn tb_agent_usage() -> *mut c_char {
    let payload = RUNTIME.block_on(agent_usage::run());
    envelope(serde_json::to_value(payload).map_err(|e| format!("serialize agent usage: {}", e)))
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
