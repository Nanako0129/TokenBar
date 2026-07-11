//! Per-model usage breakdown for the popover, backed by tokscale-core's
//! `get_model_report`. Mirrors the design of tokscale's TUI "Models" view
//! (`crates/tokscale-cli/src/tui/ui/models.rs`): one row per model with the
//! token breakdown, message count, cost, and throughput (ms/1K), sorted by
//! cost on the frontend.
//!
//! Like `usage_graph`, this drives the async core on a short-lived
//! current-thread runtime (callers run it inside `spawn_blocking`) and maps the
//! result onto a camelCase JSON shape the frontend consumes directly.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelEntry {
    client: String,
    model: String,
    provider: String,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    total: i64,
    message_count: i32,
    cost: f64,
    /// Milliseconds per 1K tokens, when tokscale could time the model. `None`
    /// when no message in the rollup carried a usable duration.
    ms_per_1k_tokens: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelReportData {
    entries: Vec<ModelEntry>,
    total_input: i64,
    total_output: i64,
    total_cache_read: i64,
    total_cache_write: i64,
    total_messages: i32,
    total_cost: f64,
    /// Unix-seconds time the LiteLLM pricing dataset was last fetched from
    /// upstream (the on-disk cache write time). `None` before the first fetch.
    /// Surfaced as the "prices updated …" hint in the Models view.
    pricing_updated_at: Option<u64>,
}

/// Build the per-model report for `year` (empty string = all time).
pub fn run(year: &str) -> Result<Value, String> {
    let year = normalize_year(year)?;

    let options = tokscale_core::ReportOptions {
        year,
        ..Default::default()
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build runtime: {}", e))?;
    let report = runtime.block_on(tokscale_core::get_model_report(options))?;

    let data = map_report(report);
    serde_json::to_value(data).map_err(|e| format!("serialize model report: {}", e))
}

fn normalize_year(year: &str) -> Result<Option<String>, String> {
    let trimmed = year.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() == 4 && trimmed.chars().all(|c| c.is_ascii_digit()) {
        Ok(Some(trimmed.to_string()))
    } else {
        Err(format!("invalid year filter: {}", year))
    }
}

fn map_report(report: tokscale_core::ModelReport) -> ModelReportData {
    ModelReportData {
        entries: report
            .entries
            .into_iter()
            .map(|e| {
                // saturating_add so #766's i64::MAX-clamped buckets (corrupt
                // Antigravity DB) can't overflow this FFI-exposed total in
                // debug/release (see agents_report.rs's map_report for the
                // same pattern).
                let total = e
                    .input
                    .saturating_add(e.output)
                    .saturating_add(e.cache_read)
                    .saturating_add(e.cache_write)
                    .saturating_add(e.reasoning);
                ModelEntry {
                    client: e.client,
                    model: e.model,
                    provider: e.provider,
                    input: e.input,
                    output: e.output,
                    cache_read: e.cache_read,
                    cache_write: e.cache_write,
                    reasoning: e.reasoning,
                    total,
                    message_count: e.message_count,
                    cost: e.cost,
                    ms_per_1k_tokens: e.performance.ms_per_1k_tokens,
                }
            })
            .collect(),
        total_input: report.total_input,
        total_output: report.total_output,
        total_cache_read: report.total_cache_read,
        total_cache_write: report.total_cache_write,
        total_messages: report.total_messages,
        total_cost: report.total_cost,
        pricing_updated_at: tokscale_core::pricing::pricing_cached_at(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #766 clamps corrupt Antigravity varints to `i64::MAX` per bucket. Two
    /// such buckets in one model entry must saturate the mapped `total`, not
    /// overflow it (a plain `+` panics in debug / wraps in release).
    #[test]
    fn total_saturates_on_overlarge_buckets() {
        let report = tokscale_core::ModelReport {
            entries: vec![tokscale_core::ModelUsage {
                client: "antigravity_cli".to_string(),
                merged_clients: None,
                workspace_key: None,
                workspace_label: None,
                session_id: None,
                model: "gemini-3-pro".to_string(),
                provider: "antigravity".to_string(),
                input: i64::MAX,
                output: i64::MAX,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                message_count: 1,
                cost: 0.0,
                performance: tokscale_core::ModelPerformance::default(),
            }],
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_write: 0,
            total_messages: 1,
            total_cost: 0.0,
            processing_time_ms: 0,
        };

        let mapped = map_report(report);
        assert_eq!(mapped.entries[0].total, i64::MAX);
    }
}
