//! Stable nil/full report parity fixture for the Native source-aware smoke
//! probe. This is test-only coverage: no vendored production behavior changes.
//!
//! The fixture intentionally combines the source identities that are most
//! likely to expose a two-level client-filter regression: a canonical client,
//! a Claude-produced `cc-mirror/*` id, a Synthetic gateway message, duplicate
//! canonical scan roots, and unattributed messages that fold into `Main`.
//! Reports are checked cold and warm so cache reuse cannot create a false
//! source-generation mismatch.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use tokscale_core::{
    generate_local_graph_report, get_agents_report, get_hourly_report, AgentReport, GraphResult,
    HourlyReport, ReportOptions,
};

struct EnvGuard {
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn set(values: &[(&'static str, &OsStr)]) -> Self {
        let previous = values
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        unsafe {
            for (key, value) in values {
                std::env::set_var(key, value);
            }
        }
        Self { previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            for (key, value) in self.previous.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Aggregate {
    entry_count: usize,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    total_tokens: i64,
    message_count: i64,
    total_cost: f64,
}

fn hourly_aggregate(report: &HourlyReport) -> Aggregate {
    let mut result = Aggregate {
        entry_count: report.entries.len(),
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        reasoning: 0,
        total_tokens: 0,
        message_count: 0,
        total_cost: report.total_cost,
    };
    for entry in &report.entries {
        result.input = result.input.saturating_add(entry.input);
        result.output = result.output.saturating_add(entry.output);
        result.cache_read = result.cache_read.saturating_add(entry.cache_read);
        result.cache_write = result.cache_write.saturating_add(entry.cache_write);
        result.reasoning = result.reasoning.saturating_add(entry.reasoning);
        result.message_count = result
            .message_count
            .saturating_add(i64::from(entry.message_count));
    }
    result.total_tokens = result
        .input
        .saturating_add(result.output)
        .saturating_add(result.cache_read)
        .saturating_add(result.cache_write)
        .saturating_add(result.reasoning);
    result
}

fn agents_aggregate(report: &AgentReport) -> Aggregate {
    let mut result = Aggregate {
        entry_count: report.entries.len(),
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        reasoning: 0,
        total_tokens: 0,
        message_count: i64::from(report.total_messages),
        total_cost: report.total_cost,
    };
    for entry in &report.entries {
        result.input = result.input.saturating_add(entry.input);
        result.output = result.output.saturating_add(entry.output);
        result.cache_read = result.cache_read.saturating_add(entry.cache_read);
        result.cache_write = result.cache_write.saturating_add(entry.cache_write);
        result.reasoning = result.reasoning.saturating_add(entry.reasoning);
    }
    result.total_tokens = result
        .input
        .saturating_add(result.output)
        .saturating_add(result.cache_read)
        .saturating_add(result.cache_write)
        .saturating_add(result.reasoning);
    result
}

#[derive(Debug, Clone, PartialEq)]
struct ReportSet {
    hourly_nil: Aggregate,
    hourly_full: Aggregate,
    agents_nil: Aggregate,
    agents_full: Aggregate,
}

fn options(home: &Path, clients: Option<Vec<String>>) -> ReportOptions {
    ReportOptions {
        home_dir: Some(home.to_string_lossy().into_owned()),
        // Keep this fixture rooted in its temporary home while still allowing
        // the duplicate TOKSCALE_EXTRA_DIRS task below to exercise canonical
        // scan-root deduplication.
        use_env_roots: true,
        clients,
        ..Default::default()
    }
}

fn reports(home: &Path, clients: &[String]) -> ReportSet {
    let hourly_nil = hourly_report(home, None);
    let hourly_full = hourly_report(home, Some(clients.to_vec()));
    let agents_nil = agents_report(home, None);
    let agents_full = agents_report(home, Some(clients.to_vec()));
    ReportSet {
        hourly_nil: hourly_aggregate(&hourly_nil),
        hourly_full: hourly_aggregate(&hourly_full),
        agents_nil: agents_aggregate(&agents_nil),
        agents_full: agents_aggregate(&agents_full),
    }
}

fn hourly_report(home: &Path, clients: Option<Vec<String>>) -> HourlyReport {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(get_hourly_report(options(home, clients)))
        .unwrap()
}

fn agents_report(home: &Path, clients: Option<Vec<String>>) -> AgentReport {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(get_agents_report(options(home, clients)))
        .unwrap()
}

fn write_fixture(home: &Path) {
    // Canonical client, duplicated under two project files with one exact
    // message id. The scanner sees the same root through TOKSCALE_EXTRA_DIRS,
    // while the streaming dedup gate sees the same canonical message key.
    for project in ["proj-a", "proj-b"] {
        let dir = home.join(format!(".config/manicode/projects/{project}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("chat-messages.json"),
            r#"[{"role":"assistant","id":"CANONICAL-DUP","metadata":{"model":"claude-sonnet-4","usage":{"inputTokens":200,"outputTokens":80}},"credits":0.02}]"#,
        )
        .unwrap();
    }

    // Plain Claude plus a cc-mirror variant produced by the Claude lane. Both
    // are unattributed and therefore must remain in the single Agents Main
    // bucket without the exact variant gate leaking plain Claude.
    let claude_dir = home.join(".claude/projects/plain");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(
        claude_dir.join("conversation.jsonl"),
        r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_plain","message":{"id":"msg_plain","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#,
    )
    .unwrap();
    let variant = home.join(".cc-mirror/kimi-code");
    let variant_config = variant.join("config");
    let variant_project = variant_config.join("projects/proj");
    std::fs::create_dir_all(&variant_project).unwrap();
    std::fs::write(
        variant.join("variant.json"),
        serde_json::json!({
            "name": "kimi-code",
            "provider": "kimi",
            "configDir": variant_config,
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        variant_project.join("session.jsonl"),
        r#"{"type":"assistant","timestamp":"2024-12-01T11:00:00.000Z","requestId":"req_variant","message":{"id":"msg_variant","model":"claude-3-5-sonnet","usage":{"input_tokens":300,"output_tokens":70}}}"#,
    )
    .unwrap();

    // Kimi is another canonical client with no agent attribution.
    let kimi_dir = home.join(".kimi/sessions/group/session");
    std::fs::create_dir_all(&kimi_dir).unwrap();
    std::fs::write(
        kimi_dir.join("wire.jsonl"),
        concat!(
            "{\"type\":\"metadata\",\"protocol_version\":\"1.3\"}\n",
            "{\"timestamp\":1770983410.0,\"message\":{\"type\":\"StatusUpdate\",\"payload\":{\"token_usage\":{\"input_other\":100,\"output\":50,\"input_cache_read\":3,\"input_cache_creation\":4},\"message_id\":\"KIMI-1\"}}}"
        ),
    )
    .unwrap();

    // Synthetic gateway traffic rides the OpenCode lane. Its canonical client
    // remains `opencode`; the synthetic matcher must therefore agree in both
    // the nil and full-list report paths without adding a fake client id.
    let opencode_dir = home.join(".local/share/opencode/storage/message/project-1");
    std::fs::create_dir_all(&opencode_dir).unwrap();
    std::fs::write(
        opencode_dir.join("msg_synthetic.json"),
        r#"{"id":"synthetic-1","sessionID":"synthetic-session","role":"assistant","modelID":"hf:deepseek-ai/DeepSeek-V3-0324","providerID":"unknown","cost":0,"tokens":{"input":10,"output":5,"reasoning":2,"cache":{"read":3,"write":4}},"time":{"created":1733011200000}}"#,
    )
    .unwrap();
}

fn graph_clients(home: &Path) -> Vec<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let graph: GraphResult = runtime
        .block_on(generate_local_graph_report(options(home, None)))
        .unwrap();
    graph.summary.clients
}

#[test]
#[serial_test::serial]
fn source_aware_filter_parity_fixture_is_stable_cold_and_warm() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let source_home = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::set(&[
        ("HOME", cache_home.path().as_os_str()),
        ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ("TOKSCALE_PRICING_CACHE_ONLY", OsStr::new("1")),
        (
            "TOKSCALE_EXTRA_DIRS",
            source_home
                .path()
                .join(".config/manicode/projects")
                .as_os_str(),
        ),
        (
            "XDG_DATA_HOME",
            source_home.path().join(".local/share").as_os_str(),
        ),
    ]);
    write_fixture(source_home.path());

    let clients = graph_clients(source_home.path());
    assert!(clients.iter().any(|client| client == "codebuff"));
    assert!(clients.iter().any(|client| client == "claude"));
    assert!(clients
        .iter()
        .any(|client| client.starts_with("cc-mirror/")));
    assert!(clients.iter().any(|client| client == "kimi"));
    assert!(clients.iter().any(|client| client == "opencode"));

    let cold = reports(source_home.path(), &clients);
    assert_eq!(
        cold.hourly_nil, cold.hourly_full,
        "cold hourly nil/full parity"
    );
    assert_eq!(
        cold.agents_nil, cold.agents_full,
        "cold Agents nil/full parity"
    );
    assert!(cold.agents_nil.entry_count >= 1);
    assert!(cold.agents_nil.message_count >= 1, "Main must retain usage");

    // Explicit slices prove the report producer, rather than a shared mixed
    // bucket, applies the requested client filter before aggregation. The
    // exact cc-mirror and synthetic seams are otherwise easy for nil/full
    // parity to miss because both paths could be wrong in the same way.
    let variant = reports(source_home.path(), &["cc-mirror/kimi-code".to_string()]);
    assert_eq!(
        (variant.hourly_full.input, variant.hourly_full.output),
        (300, 70)
    );
    assert_eq!(
        (variant.agents_full.input, variant.agents_full.output),
        (300, 70)
    );

    let claude = reports(source_home.path(), &["claude".to_string()]);
    assert_eq!(
        (claude.hourly_full.input, claude.hourly_full.output),
        (100, 50)
    );
    assert_eq!(
        (claude.agents_full.input, claude.agents_full.output),
        (100, 50)
    );

    let codebuff = reports(source_home.path(), &["codebuff".to_string()]);
    assert_eq!(
        (codebuff.hourly_full.input, codebuff.hourly_full.output),
        (200, 80)
    );
    assert_eq!(
        (codebuff.agents_full.input, codebuff.agents_full.output),
        (200, 80)
    );

    let synthetic = hourly_report(source_home.path(), Some(vec!["synthetic".to_string()]));
    assert_eq!(
        synthetic.entries.len(),
        1,
        "synthetic filter returns gateway row"
    );
    assert_eq!(
        (synthetic.entries[0].input, synthetic.entries[0].output),
        (10, 5)
    );

    let synthetic_agents = agents_report(source_home.path(), Some(vec!["synthetic".to_string()]));
    assert_eq!(
        synthetic_agents.entries.len(),
        1,
        "synthetic filter returns one Agents row"
    );
    assert_eq!(
        (
            synthetic_agents.entries[0].input,
            synthetic_agents.entries[0].output
        ),
        (10, 5)
    );

    let all_agents = agents_report(source_home.path(), None);
    let main = all_agents
        .entries
        .iter()
        .find(|entry| entry.agent == "Main")
        .expect("unattributed usage must remain in Main");
    assert_eq!((main.input, main.output), (710, 255));

    let warm = reports(source_home.path(), &clients);
    assert_eq!(
        warm, cold,
        "warm cache must preserve cold report aggregates"
    );
    assert_eq!(
        warm.hourly_nil, warm.hourly_full,
        "warm hourly nil/full parity"
    );
    assert_eq!(
        warm.agents_nil, warm.agents_full,
        "warm Agents nil/full parity"
    );
}
