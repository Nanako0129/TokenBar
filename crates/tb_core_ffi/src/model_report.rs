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

/// Report options for the per-model report.
///
/// Providers must stay separate. Under tokscale's default `ClientModel`
/// grouping a model served by two providers folds into ONE row whose
/// `provider` is a comma-joined string (`"nvidia, deepseek"`) and whose tokens
/// are summed — see `aggregate_model_usage_entries` in tokscale-core. That
/// merge is lossy: no downstream consumer can split the row back apart, so any
/// per-provider attribution has to be decided here, at the producer.
fn model_report_options(
    context: &crate::LocalSourceContext,
    year: Option<String>,
) -> tokscale_core::ReportOptions {
    let mut options = context.report_options(year, None);
    options.group_by = tokscale_core::GroupBy::ClientProviderModel;
    options
}

/// Build the per-model report for `year` (empty string = all time).
pub(crate) fn run(context: &crate::LocalSourceContext, year: &str) -> Result<Value, String> {
    let year = normalize_year(year)?;
    let options = model_report_options(context, year);

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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FixtureHome(PathBuf);

    impl FixtureHome {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "tokenbar-model-report-{}-{}-{label}",
                std::process::id(),
                nanos
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FixtureHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug, serde::Deserialize)]
    struct TestModelPerformance {
        #[serde(rename = "msPer1KTokens")]
        ms_per_1k_tokens: Option<f64>,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    struct TestModelUsage {
        client: String,
        merged_clients: Option<String>,
        workspace_key: Option<String>,
        workspace_label: Option<String>,
        session_id: Option<String>,
        model: String,
        provider: String,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        reasoning: i64,
        message_count: i32,
        cost: f64,
        performance: TestModelPerformance,
    }

    struct TestModelReport {
        entries: Vec<TestModelUsage>,
    }

    fn model_report_direct(
        home: &Path,
        client: &str,
        group_by: Option<tokscale_core::GroupBy>,
    ) -> tokscale_core::ModelReport {
        let context = crate::LocalSourceContext::for_home(home.to_path_buf());
        let mut options = model_report_options(&context, None);
        // report_options defaults to environment roots for production. Tests
        // must opt out so only the fixture home and requested lane are read.
        options.use_env_roots = false;
        options.clients = Some(vec![client.to_string()]);
        if let Some(group_by) = group_by {
            options.group_by = group_by;
        }

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokscale_core::get_model_report(options))
            .unwrap()
    }

    #[test]
    fn model_report_child() {
        let Some(home) = std::env::var_os("TOKENBAR_MODEL_REPORT_HOME") else {
            return;
        };
        let client = std::env::var("TOKENBAR_MODEL_REPORT_CLIENT").unwrap();
        let group_by = match std::env::var("TOKENBAR_MODEL_REPORT_GROUP_BY")
            .unwrap_or_default()
            .as_str()
        {
            "client,model" => Some(tokscale_core::GroupBy::ClientModel),
            "client,provider,model" => Some(tokscale_core::GroupBy::ClientProviderModel),
            _ => None,
        };
        let report = model_report_direct(Path::new(&home), &client, group_by);
        println!(
            "TOKENBAR_MODEL_REPORT_RESULT={}",
            serde_json::to_string(&report.entries).unwrap()
        );
    }

    fn model_report(
        context: &crate::LocalSourceContext,
        client: &str,
        group_by: Option<tokscale_core::GroupBy>,
    ) -> TestModelReport {
        let fixture_home = context.home_dir.as_ref().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "model_report::tests::model_report_child",
                "--nocapture",
            ])
            .env("TOKENBAR_MODEL_REPORT_HOME", fixture_home)
            .env("TOKENBAR_MODEL_REPORT_CLIENT", client)
            .env(
                "TOKENBAR_MODEL_REPORT_GROUP_BY",
                group_by.map(|group| group.to_string()).unwrap_or_default(),
            )
            // The pricing configuration is process-local in tokscale; keep it
            // hermetic in the child instead of mutating the test runner.
            .env("TOKSCALE_CONFIG_DIR", fixture_home.join(".tokscale-config"))
            .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "model report child failed: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let marker = "TOKENBAR_MODEL_REPORT_RESULT=";
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json = stdout
            .lines()
            .find_map(|line| line.strip_prefix(marker))
            .unwrap_or_else(|| {
                panic!(
                    "model report child emitted no result: {}",
                    String::from_utf8_lossy(&output.stdout)
                )
            });
        TestModelReport {
            entries: serde_json::from_str(json).unwrap(),
        }
    }

    fn write_jcode_fixture(home: &Path, snapshot: &str, journal: Option<&str>) {
        let sessions = home.join(".jcode/sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("session_attr.json"), snapshot).unwrap();
        if let Some(journal) = journal {
            fs::write(sessions.join("session_attr.journal.jsonl"), journal).unwrap();
        }
    }

    fn write_mux_fixture(home: &Path, contents: &str) {
        let session = home.join(".mux/sessions/workspace-empty");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("session-usage.json"), contents).unwrap();
    }

    #[test]
    fn production_options_group_by_client_provider_model() {
        // This anchors the producer choice: reverting the assignment to the
        // default ClientModel would make provider attribution lossy again.
        let options = model_report_options(
            &crate::LocalSourceContext::for_home(PathBuf::from("/unused")),
            None,
        );
        assert_eq!(
            options.group_by,
            tokscale_core::GroupBy::ClientProviderModel
        );
    }

    #[test]
    fn model_report_keeps_jcode_providers_split() {
        let home = FixtureHome::new("split");
        // The snapshot and journal deliberately reuse one model id while the
        // journal metadata overrides only its provider, exercising the parser
        // seam that would otherwise be impossible to split downstream.
        write_jcode_fixture(
            home.path(),
            r#"{"id":"session_attr","provider_key":"openai","model":"shared-provider-model","working_dir":"/fixture","messages":[{"id":"snapshot-assistant","role":"assistant","timestamp":"2026-06-16T12:00:01Z","token_usage":{"input_tokens":101,"output_tokens":17,"cache_read_input_tokens":23,"cache_creation_input_tokens":29,"reasoning_output_tokens":31}}]}"#,
            Some(
                r#"{"meta":{"provider_key":"nvidia","model":"shared-provider-model","working_dir":"/fixture"},"append_messages":[{"id":"journal-assistant","role":"assistant","timestamp":"2026-06-16T12:00:02Z","token_usage":{"input_tokens":211,"output_tokens":37,"cache_read_input_tokens":43,"cache_creation_input_tokens":47,"reasoning_output_tokens":53}}]}
"#,
            ),
        );
        let context = crate::LocalSourceContext::for_home(home.path().to_path_buf());

        let split = model_report(&context, "jcode", None).entries;
        assert_eq!(split.len(), 2);
        let openai = split
            .iter()
            .find(|entry| entry.provider == "openai")
            .unwrap();
        let nvidia = split
            .iter()
            .find(|entry| entry.provider == "nvidia")
            .unwrap();
        assert_eq!(openai.client, "jcode");
        assert_eq!(openai.model, "shared-provider-model");
        assert_eq!(openai.input, 101);
        assert_eq!(openai.output, 17);
        assert_eq!(openai.cache_read, 23);
        assert_eq!(openai.cache_write, 29);
        assert_eq!(openai.reasoning, 31);
        assert_eq!(
            openai.input
                + openai.output
                + openai.cache_read
                + openai.cache_write
                + openai.reasoning,
            201
        );
        assert_eq!(nvidia.client, "jcode");
        assert_eq!(nvidia.model, "shared-provider-model");
        assert_eq!(nvidia.input, 211);
        assert_eq!(nvidia.output, 37);
        assert_eq!(nvidia.cache_read, 43);
        assert_eq!(nvidia.cache_write, 47);
        assert_eq!(nvidia.reasoning, 53);
        assert_eq!(
            nvidia.input
                + nvidia.output
                + nvidia.cache_read
                + nvidia.cache_write
                + nvidia.reasoning,
            391
        );

        let merged_report =
            model_report(&context, "jcode", Some(tokscale_core::GroupBy::ClientModel));
        assert_eq!(merged_report.entries.len(), 1);
        let merged = &merged_report.entries[0];
        assert_eq!(merged.client, "jcode");
        assert_eq!(merged.model, "shared-provider-model");
        assert_eq!(merged.message_count, 2);
        let mut merged_providers = merged.provider.split(", ").collect::<Vec<_>>();
        merged_providers.sort_unstable();
        assert_eq!(merged_providers, vec!["nvidia", "openai"]);
        assert_eq!(merged.input, 312);
        assert_eq!(merged.output, 54);
        assert_eq!(merged.cache_read, 66);
        assert_eq!(merged.cache_write, 76);
        assert_eq!(merged.reasoning, 84);
        assert_eq!(openai.input + nvidia.input, merged.input);
        assert_eq!(openai.output + nvidia.output, merged.output);
        assert_eq!(openai.cache_read + nvidia.cache_read, merged.cache_read);
        assert_eq!(openai.cache_write + nvidia.cache_write, merged.cache_write);
        assert_eq!(openai.reasoning + nvidia.reasoning, merged.reasoning);
    }

    #[test]
    fn model_report_keeps_empty_mux_provider_visible() {
        let home = FixtureHome::new("empty-provider");
        write_mux_fixture(
            home.path(),
            r#"{"version":1,"byModel":{"providerless-model":{"input":{"tokens":307},"cached":{"tokens":11},"cacheCreate":{"tokens":13},"output":{"tokens":19},"reasoning":{"tokens":17}},"nvidia:providerless-model":{"input":{"tokens":401},"cached":{"tokens":29},"cacheCreate":{"tokens":31},"output":{"tokens":23},"reasoning":{"tokens":37}}},"lastRequest":{"timestamp":1781601600000}}"#,
        );
        let context = crate::LocalSourceContext::for_home(home.path().to_path_buf());

        let report = model_report(&context, "mux", None);
        assert_eq!(report.entries.len(), 2);
        let entry = report
            .entries
            .iter()
            .find(|entry| entry.provider.is_empty())
            .unwrap();
        // A provider-less mux key must remain a visible empty-provider row;
        // dropping it would hide usage, while merging it would lose its lane.
        assert_eq!(entry.client, "mux");
        assert_eq!(entry.model, "providerless-model");
        assert_eq!(entry.provider, "");
        assert_eq!(entry.input, 307);
        assert_eq!(entry.output, 19);
        assert_eq!(entry.cache_read, 11);
        assert_eq!(entry.cache_write, 13);
        assert_eq!(entry.reasoning, 17);
        let nvidia = report
            .entries
            .iter()
            .find(|entry| entry.provider == "nvidia")
            .unwrap();
        assert_eq!(nvidia.model, "providerless-model");
        assert_eq!(nvidia.input, 401);
        assert_eq!(nvidia.output, 23);
        assert_eq!(nvidia.cache_read, 29);
        assert_eq!(nvidia.cache_write, 31);
        assert_eq!(nvidia.reasoning, 37);
    }

    #[test]
    fn model_report_single_provider_matches_client_model() {
        let home = FixtureHome::new("single-provider");
        write_jcode_fixture(
            home.path(),
            r#"{"id":"session_attr","provider_key":"openai","model":"single-provider-model","working_dir":"/fixture","messages":[{"id":"assistant","role":"assistant","timestamp":"2026-06-16T12:00:01Z","token_usage":{"input_tokens":211,"output_tokens":37,"cache_read_input_tokens":43,"cache_creation_input_tokens":47,"reasoning_output_tokens":53}}]}"#,
            None,
        );
        let context = crate::LocalSourceContext::for_home(home.path().to_path_buf());

        let provider_grouped = model_report(&context, "jcode", None);
        let client_grouped =
            model_report(&context, "jcode", Some(tokscale_core::GroupBy::ClientModel));
        assert_eq!(provider_grouped.entries.len(), 1);
        assert_eq!(client_grouped.entries.len(), 1);
        let provider_entry = &provider_grouped.entries[0];
        let client_entry = &client_grouped.entries[0];
        // One provider should not change the Models card's established row or
        // numbers when the grouping gains the provider dimension.
        assert_eq!(provider_entry.client, client_entry.client);
        assert_eq!(provider_entry.merged_clients, client_entry.merged_clients);
        assert_eq!(provider_entry.model, client_entry.model);
        assert_eq!(provider_entry.provider, client_entry.provider);
        assert_eq!(provider_entry.workspace_key, client_entry.workspace_key);
        assert_eq!(provider_entry.workspace_label, client_entry.workspace_label);
        assert_eq!(provider_entry.session_id, client_entry.session_id);
        assert_eq!(provider_entry.input, client_entry.input);
        assert_eq!(provider_entry.output, client_entry.output);
        assert_eq!(provider_entry.cache_read, client_entry.cache_read);
        assert_eq!(provider_entry.cache_write, client_entry.cache_write);
        assert_eq!(provider_entry.reasoning, client_entry.reasoning);
        assert_eq!(provider_entry.message_count, client_entry.message_count);
        assert_eq!(provider_entry.cost, client_entry.cost);
        assert_eq!(
            provider_entry.performance.ms_per_1k_tokens,
            client_entry.performance.ms_per_1k_tokens
        );
    }

    /// #766 clamps corrupt Antigravity varints to `i64::MAX` per bucket. Two
    /// such buckets in one model entry must saturate the mapped `total`, not
    /// overflow it (a plain `+` panics in debug / wraps in release).
    fn entry(input: i64, output: i64, cache_read: i64, cache_write: i64, reasoning: i64) -> tokscale_core::ModelUsage {
        tokscale_core::ModelUsage {
            client: "antigravity_cli".to_string(),
            merged_clients: None,
            workspace_key: None,
            workspace_label: None,
            session_id: None,
            model: "gemini-3-pro".to_string(),
            provider: "antigravity".to_string(),
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
            message_count: 1,
            cost: 0.0,
            performance: tokscale_core::ModelPerformance::default(),
        }
    }

    fn wrap(entries: Vec<tokscale_core::ModelUsage>) -> tokscale_core::ModelReport {
        tokscale_core::ModelReport {
            entries,
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_write: 0,
            total_messages: 1,
            total_cost: 0.0,
            processing_time_ms: 0,
        }
    }

    #[test]
    fn total_saturates_on_overlarge_buckets() {
        let report = wrap(vec![entry(i64::MAX, i64::MAX, 0, 0, 0)]);
        let mapped = map_report(report);
        assert_eq!(mapped.entries[0].total, i64::MAX);
    }

    /// The two-MAX-field case above only pins `input`/`output` into the fold.
    /// Pin the other three fields too: nonzero `input`/`output`/`reasoning`
    /// plus a clamped `cache_write`, so `cache_read`/`cache_write` inclusion
    /// is independently exercised, not just present-but-untested.
    #[test]
    fn total_saturates_when_cache_write_is_overlarge() {
        let report = wrap(vec![entry(10, 20, i64::MAX, i64::MAX, 5)]);
        let mapped = map_report(report);
        assert_eq!(mapped.entries[0].total, i64::MAX);
    }

    /// The saturating cases can't catch a dropped operand (another MAX field
    /// keeps the total at MAX), so pin every field's inclusion with distinct
    /// powers of two: omitting any one operand changes the exact sum.
    #[test]
    fn total_includes_every_token_field() {
        let report = wrap(vec![entry(1, 2, 4, 8, 16)]);
        let mapped = map_report(report);
        assert_eq!(mapped.entries[0].total, 31);
    }
}
