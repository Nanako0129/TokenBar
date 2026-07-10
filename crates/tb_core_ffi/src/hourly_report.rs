//! Per-hour usage breakdown for the popover, backed by tokscale-core's
//! `get_hourly_report`. Mirrors tokscale's TUI "Hourly" view: one entry per
//! "YYYY-MM-DD HH:00" slot with the token breakdown, message/turn counts, and
//! cost. The frontend folds these slots into a 24-hour-of-day distribution.
//!
//! Like `model_report`, this drives the async core on a short-lived
//! current-thread runtime (callers run it inside `spawn_blocking`) and maps the
//! result onto a camelCase JSON shape the frontend consumes directly.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HourlyEntry {
    /// "YYYY-MM-DD HH:00" local-time slot.
    hour: String,
    clients: Vec<String>,
    models: Vec<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    total: i64,
    message_count: i32,
    turn_count: i32,
    cost: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HourlyReportData {
    entries: Vec<HourlyEntry>,
    total_cost: f64,
}

/// Build the per-hour report for `year` (empty string = all time), restricted
/// to `clients` (None = every client). The client filter is applied in the
/// streaming scan, so shared-hour buckets carry only the selected clients'
/// tokens/cost — a membership filter downstream cannot do this because each
/// `HourAggregator` folds all clients into one mixed total.
pub fn run(year: &str, clients: Option<Vec<String>>) -> Result<Value, String> {
    let year = normalize_year(year)?;

    let options = tokscale_core::ReportOptions {
        year,
        clients,
        ..Default::default()
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build runtime: {}", e))?;
    let report = runtime.block_on(tokscale_core::get_hourly_report(options))?;

    let data = map_report(report);
    serde_json::to_value(data).map_err(|e| format!("serialize hourly report: {}", e))
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

fn map_report(report: tokscale_core::HourlyReport) -> HourlyReportData {
    HourlyReportData {
        entries: report
            .entries
            .into_iter()
            .map(|e| {
                let total = e.input + e.output + e.cache_read + e.cache_write + e.reasoning;
                HourlyEntry {
                    hour: e.hour,
                    clients: e.clients,
                    models: e.models,
                    input: e.input,
                    output: e.output,
                    cache_read: e.cache_read,
                    cache_write: e.cache_write,
                    reasoning: e.reasoning,
                    total,
                    message_count: e.message_count,
                    turn_count: e.turn_count,
                    cost: e.cost,
                }
            })
            .collect(),
        total_cost: report.total_cost,
    }
}
