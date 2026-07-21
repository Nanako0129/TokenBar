//! GitHub Copilot Desktop SQLite parser.
//!
//! The macOS desktop app stores aggregate token totals in `~/.copilot/data.db`
//! and per-session event metadata in `~/.copilot/session-state/{session_id}`.

use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::provider_identity::inferred_provider_from_model;
use chrono::{DateTime, NaiveDateTime};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug)]
struct CopilotDesktopSessionRow {
    id: String,
    model: Option<String>,
    total_input_tokens: i64,
    total_output_tokens: i64,
    total_cached_tokens: i64,
    total_reasoning_tokens: i64,
    /// Billed AIU in nano-units (1e9 nano = 1 AI credit = $0.01 USD).
    /// Zero/absent means no provider-reported cost; pricing may estimate later.
    total_nano_aiu: i64,
    created_at: Option<String>,
}

#[derive(Debug, Default)]
struct SessionStateMetadata {
    /// First non-empty `session.model_change` (highest priority among events).
    model_change: Option<String>,
    /// `session.start` `selectedModel` when no model_change is present.
    selected_model: Option<String>,
    /// First non-empty `assistant.usage` `data.model` fallback.
    usage_model: Option<String>,
    cwd: Option<String>,
}

impl SessionStateMetadata {
    /// Preference: first model_change → session.start selectedModel →
    /// first assistant.usage model. DB `sessions.model` is applied by the caller.
    fn resolved_model(&self) -> Option<&str> {
        self.model_change
            .as_deref()
            .or(self.selected_model.as_deref())
            .or(self.usage_model.as_deref())
    }
}

/// True when `sessions.total_nano_aiu` exists (newer Desktop schema).
/// Older installs omit the column; treat missing as 0 rather than failing prepare.
fn sessions_has_total_nano_aiu(conn: &Connection) -> bool {
    let Ok(mut stmt) = conn.prepare("PRAGMA table_info(sessions)") else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    for name in rows.flatten() {
        if name == "total_nano_aiu" {
            return true;
        }
    }
    false
}

pub fn parse_copilot_desktop_db(db_path: &Path) -> Vec<UnifiedMessage> {
    let conn = match Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to open Copilot Desktop database"
            );
            return Vec::new();
        }
    };

    // Older Desktop DBs predate total_nano_aiu. Probe columns so prepare does
    // not fail and zero out the whole Desktop lane.
    let has_nano_aiu = sessions_has_total_nano_aiu(&conn);
    let sql = if has_nano_aiu {
        r#"
        SELECT
            id,
            title,
            model,
            total_input_tokens,
            total_output_tokens,
            total_cached_tokens,
            total_reasoning_tokens,
            total_nano_aiu,
            created_at
        FROM sessions
        WHERE total_input_tokens > 0
           OR total_output_tokens > 0
           OR total_cached_tokens > 0
           OR total_reasoning_tokens > 0
        "#
    } else {
        r#"
        SELECT
            id,
            title,
            model,
            total_input_tokens,
            total_output_tokens,
            total_cached_tokens,
            total_reasoning_tokens,
            created_at
        FROM sessions
        WHERE total_input_tokens > 0
           OR total_output_tokens > 0
           OR total_cached_tokens > 0
           OR total_reasoning_tokens > 0
        "#
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to prepare Copilot Desktop sessions query"
            );
            return Vec::new();
        }
    };

    let rows = match stmt.query_map([], |row| {
        Ok(CopilotDesktopSessionRow {
            id: row.get(0)?,
            model: row.get(2)?,
            total_input_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            total_output_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            total_cached_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            total_reasoning_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            total_nano_aiu: if has_nano_aiu {
                row.get::<_, Option<i64>>(7)?.unwrap_or(0)
            } else {
                0
            },
            created_at: if has_nano_aiu {
                row.get(8)?
            } else {
                row.get(7)?
            },
        })
    }) {
        Ok(rows) => rows,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to execute Copilot Desktop sessions query"
            );
            return Vec::new();
        }
    };

    rows.filter_map(|row| match row {
        Ok(row) => Some(session_row_to_message(db_path, row)),
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to decode Copilot Desktop session row"
            );
            None
        }
    })
    .collect()
}

fn session_row_to_message(db_path: &Path, row: CopilotDesktopSessionRow) -> UnifiedMessage {
    let metadata = read_session_state_metadata(db_path, &row.id);
    let model_id = metadata
        .resolved_model()
        .or(row.model.as_deref())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("auto")
        .to_string();
    let provider_id = inferred_provider_from_model(&model_id)
        .unwrap_or("github-copilot")
        .to_string();

    let timestamp_ms = row
        .created_at
        .as_deref()
        .and_then(parse_iso8601_timestamp_ms)
        .unwrap_or_else(|| {
            warn!(
                session_id = %row.id,
                created_at = ?row.created_at,
                "Copilot Desktop session has unparseable created_at; defaulting to 0"
            );
            0
        });

    // total_nano_aiu is Desktop-billed AI credit in nano-units
    // (1e9 nano = 1 AI credit = $0.01 USD). When non-zero this is the
    // authoritative USD cost and must skip later token-based reprice via
    // ProviderReported. See `copilot::copilot_aiu_nano_to_usd`.
    let (cost, has_provider_cost) = if row.total_nano_aiu > 0 {
        (super::copilot::copilot_aiu_nano_to_usd(row.total_nano_aiu), true)
    } else {
        (0.0, false)
    };

    let mut message = UnifiedMessage::new_with_dedup(
        "copilot",
        model_id,
        provider_id,
        row.id.clone(),
        timestamp_ms,
        // Copilot reports input tokens inclusive of cache reads (same convention
        // as the OTEL exporter that feeds this same session data). Reuse the
        // shared normalizer so the desktop-DB and OTEL paths never diverge and
        // additive pricing does not double-charge the cached portion.
        super::copilot::normalize_input_tokens(
            row.total_input_tokens,
            row.total_output_tokens,
            row.total_cached_tokens,
            0,
            row.total_reasoning_tokens,
        ),
        cost,
        Some(format!("copilot-desktop:{}", row.id)),
    );

    if has_provider_cost {
        message.mark_provider_reported_cost();
    }

    if let Some(workspace_key) = metadata.cwd.as_deref().and_then(normalize_workspace_key) {
        let workspace_label = workspace_label_from_key(&workspace_key);
        message.set_workspace(Some(workspace_key), workspace_label);
    }

    message
}

/// Paths to per-session `events.jsonl` files that enrich Desktop DB rows with
/// model and workspace metadata. Used by live-tail change tokens and mtime
/// probes so a session-state-only rewrite still invalidates caches.
pub(crate) fn copilot_desktop_related_event_paths(db_path: &Path) -> Vec<PathBuf> {
    let Some(session_state) = db_path.parent().map(|root| root.join("session-state")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&session_state) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("events.jsonl"))
        .filter(|path| path.is_file())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths
}

fn read_session_state_metadata(db_path: &Path, session_id: &str) -> SessionStateMetadata {
    let Some(copilot_root) = db_path.parent() else {
        return SessionStateMetadata::default();
    };
    let events_path = copilot_root
        .join("session-state")
        .join(session_id)
        .join("events.jsonl");

    read_events_metadata(&events_path)
}

fn read_events_metadata(events_path: &Path) -> SessionStateMetadata {
    let file = match std::fs::File::open(events_path) {
        Ok(file) => file,
        Err(_) => return SessionStateMetadata::default(),
    };

    let mut metadata = SessionStateMetadata::default();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            continue;
        };

        match event_type {
            "session.start" => {
                if metadata.cwd.is_none() {
                    metadata.cwd = event
                        .pointer("/data/context/cwd")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|cwd| !cwd.is_empty())
                        .map(str::to_string);
                }
                // Initial model when the user never emits model_change.
                // Nested `data.context.selectedModel` is accepted as a fallback.
                if metadata.selected_model.is_none() {
                    metadata.selected_model = first_non_empty_str(
                        &event,
                        &["/data/selectedModel", "/data/context/selectedModel"],
                    )
                    .filter(|model| *model != "auto")
                    .map(str::to_string);
                }
            }
            // Prefer the first non-empty model_change: session token totals are
            // whole-session aggregates, so the last mid-session switch must not
            // claim the entire session.
            "session.model_change" if metadata.model_change.is_none() => {
                if let Some(model) = event
                    .pointer("/data/newModel")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty() && *model != "auto")
                {
                    metadata.model_change = Some(model.to_string());
                }
            }
            // First assistant.usage model only (do not let later turns overwrite
            // a model already established by model_change / selectedModel /
            // an earlier usage event).
            "assistant.usage" if metadata.usage_model.is_none() => {
                if let Some(model) = event
                    .pointer("/data/model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty() && *model != "auto")
                {
                    metadata.usage_model = Some(model.to_string());
                }
            }
            _ => {}
        }
    }

    metadata
}

fn first_non_empty_str<'a>(event: &'a Value, paths: &[&str]) -> Option<&'a str> {
    paths.iter().find_map(|path| {
        event
            .pointer(path)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    })
}

fn parse_iso8601_timestamp_ms(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_millis())
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })
        .or_else(|| {
            // SQLite's default datetime() text form is space-separated and may
            // carry fractional seconds ("2026-07-01 12:34:56.789"); without this
            // branch it fails every parse above and the session lands in 1970.
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })
        .or_else(|| {
            let numeric = value.parse::<i64>().ok()?;
            // Distinguish seconds vs milliseconds: values < 10 billion are
            // assumed to be Unix seconds (common in SQLite), otherwise millis.
            if numeric > 10_000_000_000 {
                Some(numeric)
            } else {
                Some(numeric.saturating_mul(1000))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};
    use std::fs::{self, File};
    use std::io::Write;

    fn create_copilot_desktop_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE sessions (
                id TEXT,
                title TEXT,
                session_type TEXT,
                mode TEXT,
                model TEXT,
                total_input_tokens INTEGER,
                total_output_tokens INTEGER,
                total_cached_tokens INTEGER,
                total_reasoning_tokens INTEGER,
                total_nano_aiu INTEGER,
                created_at TEXT,
                agent TEXT,
                provider_id TEXT
            );
            "#,
        )
        .unwrap();
        conn
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        model: &str,
        input: i64,
        output: i64,
        cached: i64,
        reasoning: i64,
    ) {
        insert_session_with_nano_aiu(conn, id, model, input, output, cached, reasoning, 0);
    }

    fn insert_session_with_nano_aiu(
        conn: &Connection,
        id: &str,
        model: &str,
        input: i64,
        output: i64,
        cached: i64,
        reasoning: i64,
        nano_aiu: i64,
    ) {
        conn.execute(
            r#"
            INSERT INTO sessions (
                id, title, session_type, mode, model,
                total_input_tokens, total_output_tokens, total_cached_tokens,
                total_reasoning_tokens, total_nano_aiu, created_at, agent, provider_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            "#,
            params![
                id,
                "Test session",
                "chat",
                "agent",
                model,
                input,
                output,
                cached,
                reasoning,
                nano_aiu,
                "2026-07-01T12:34:56Z",
                "github.copilot.default",
                "github-copilot"
            ],
        )
        .unwrap();
    }

    fn write_events(root: &Path, session_id: &str, lines: &[&str]) {
        let events_dir = root.join("session-state").join(session_id);
        fs::create_dir_all(&events_dir).unwrap();
        let mut file = File::create(events_dir.join("events.jsonl")).unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
    }

    #[test]
    fn parse_copilot_desktop_db_reads_token_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "gpt-5.1-codex", 100, 50, 25, 10);
        drop(conn);

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.client, "copilot");
        assert_eq!(message.model_id, "gpt-5.1-codex");
        assert_eq!(message.provider_id, "openai");
        assert_eq!(message.session_id, "session-1");
        assert_eq!(message.timestamp, 1_782_909_296_000);
        // total_input_tokens is inclusive of cache reads, so the cached portion
        // (25) is normalized out of input: 100 - 25 = 75.
        assert_eq!(message.tokens.input, 75);
        assert_eq!(message.tokens.output, 50);
        assert_eq!(message.tokens.cache_read, 25);
        assert_eq!(message.tokens.cache_write, 0);
        assert_eq!(message.tokens.reasoning, 10);
        assert_eq!(
            message.dedup_key.as_deref(),
            Some("copilot-desktop:session-1")
        );
    }

    #[test]
    fn parse_copilot_desktop_db_skips_zero_token_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "gpt-5.1-codex", 0, 0, 0, 0);
        drop(conn);

        assert!(parse_copilot_desktop_db(&db_path).is_empty());
    }

    #[test]
    fn parse_copilot_desktop_db_enriches_model_and_workspace_from_events() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 50, 0, 0);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.start","data":{"context":{"cwd":"/Users/alice/project"}}}"#,
                r#"{"type":"session.model_change","data":{"newModel":"claude-sonnet-4-5"}}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.model_id, "claude-sonnet-4-5");
        assert_eq!(message.provider_id, "anthropic");
        assert_eq!(message.workspace_label.as_deref(), Some("project"));
    }

    #[test]
    fn parse_copilot_desktop_db_uses_github_copilot_provider_for_auto() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 0, 0, 0);
        drop(conn);

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].provider_id, "github-copilot");
    }

    #[test]
    fn parse_copilot_desktop_db_prefers_first_model_change() {
        // Whole-session token totals must not be attributed to a mid-session
        // model switch; keep the first non-empty model_change.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 50, 0, 0);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.start","data":{"context":{"cwd":"/Users/alice/project"}}}"#,
                r#"{"type":"session.model_change","data":{"newModel":"claude-sonnet-4-5"}}"#,
                r#"{"type":"session.model_change","data":{"newModel":"gpt-5.1-codex"}}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-4-5");
        assert_eq!(messages[0].provider_id, "anthropic");
    }

    #[test]
    fn parse_iso8601_handles_space_separated_fractional_seconds() {
        // SQLite datetime() text form; must not fall through to the 1970 default.
        let ms = parse_iso8601_timestamp_ms("2026-07-01 12:34:56.789")
            .expect("space + fractional seconds should parse");
        assert_eq!(ms, 1_782_909_296_789);

        // Sibling formats still parse.
        assert_eq!(
            parse_iso8601_timestamp_ms("2026-07-01T12:34:56Z"),
            Some(1_782_909_296_000)
        );
        assert_eq!(
            parse_iso8601_timestamp_ms("2026-07-01 12:34:56"),
            Some(1_782_909_296_000)
        );
        assert_eq!(parse_iso8601_timestamp_ms("not-a-timestamp"), None);
    }

    #[test]
    fn parse_copilot_desktop_db_reports_nano_aiu_as_provider_cost() {
        // 2_500_000_000 nano AIU => 2.5 AI credits => $0.025 USD; cost_source
        // must be ProviderReported so downstream reprice is skipped.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session_with_nano_aiu(
            &conn,
            "session-aiu",
            "gpt-5.1-codex",
            100,
            50,
            0,
            0,
            2_500_000_000,
        );
        drop(conn);

        let messages = parse_copilot_desktop_db(&db_path);
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert!(
            (message.cost - 0.025).abs() < 1e-12,
            "cost={}",
            message.cost
        );
        assert!(message.has_authoritative_cost());
        assert_eq!(message.cost_source, crate::CostSource::ProviderReported);
    }

    #[test]
    fn parse_copilot_desktop_db_one_billion_nano_is_one_cent() {
        // Hermetic: 1e9 nano AIU → cost ≈ 0.01 USD.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session_with_nano_aiu(
            &conn,
            "session-cent",
            "gpt-5.1-codex",
            10,
            5,
            0,
            0,
            1_000_000_000,
        );
        drop(conn);

        let messages = parse_copilot_desktop_db(&db_path);
        assert_eq!(messages.len(), 1);
        assert!(
            (messages[0].cost - 0.01).abs() < 1e-12,
            "cost={}",
            messages[0].cost
        );
        assert!(messages[0].has_authoritative_cost());
    }

    #[test]
    fn parse_copilot_desktop_db_uses_session_start_selected_model() {
        // No model_change: session.start selectedModel is the events model.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 50, 0, 0);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.start","data":{"selectedModel":"claude-sonnet-4-5","context":{"cwd":"/Users/alice/project"}}}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-4-5");
        assert_eq!(messages[0].provider_id, "anthropic");
    }

    #[test]
    fn parse_copilot_desktop_db_model_change_beats_selected_and_usage() {
        // model_change wins over session.start selectedModel and later
        // assistant.usage model; later usage must not overwrite.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 50, 0, 0);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.start","data":{"selectedModel":"gpt-4o"}}"#,
                r#"{"type":"session.model_change","data":{"newModel":"claude-sonnet-4-5"}}"#,
                r#"{"type":"assistant.usage","data":{"model":"gpt-5.1-codex"}}"#,
                r#"{"type":"assistant.usage","data":{"model":"o3-mini"}}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-4-5");
    }

    #[test]
    fn parse_copilot_desktop_db_uses_assistant_usage_model_without_start_or_change() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 50, 0, 0);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.start","data":{"context":{"cwd":"/tmp/ws"}}}"#,
                r#"{"type":"assistant.usage","data":{"model":"gpt-5.1-codex"}}"#,
                r#"{"type":"assistant.usage","data":{"model":"o3-mini"}}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "gpt-5.1-codex");
    }

    #[test]
    fn parse_copilot_desktop_db_zero_nano_aiu_leaves_cost_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "gpt-5.1-codex", 100, 50, 0, 0);
        drop(conn);

        let messages = parse_copilot_desktop_db(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].cost, 0.0);
        assert!(!messages[0].has_authoritative_cost());
        assert_eq!(messages[0].cost_source, crate::CostSource::Unknown);
    }

    #[test]
    fn parse_copilot_desktop_db_without_total_nano_aiu_column_still_reads_tokens() {
        // Older Desktop schema predates total_nano_aiu; prepare must not fail
        // and token rows must still surface (AIU defaults to 0).
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE sessions (
                id TEXT,
                title TEXT,
                session_type TEXT,
                mode TEXT,
                model TEXT,
                total_input_tokens INTEGER,
                total_output_tokens INTEGER,
                total_cached_tokens INTEGER,
                total_reasoning_tokens INTEGER,
                created_at TEXT,
                agent TEXT,
                provider_id TEXT
            );
            INSERT INTO sessions (
                id, title, session_type, mode, model,
                total_input_tokens, total_output_tokens, total_cached_tokens,
                total_reasoning_tokens, created_at, agent, provider_id
            ) VALUES (
                'session-legacy', 'Legacy', 'chat', 'agent', 'gpt-5.1-codex',
                100, 50, 0, 0, '2026-07-01T12:34:56Z',
                'github.copilot.default', 'github-copilot'
            );
            "#,
        )
        .unwrap();
        drop(conn);

        let messages = parse_copilot_desktop_db(&db_path);
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.session_id, "session-legacy");
        assert_eq!(message.tokens.input, 100);
        assert_eq!(message.tokens.output, 50);
        assert_eq!(message.cost, 0.0);
        assert!(!message.has_authoritative_cost());
        assert_eq!(message.cost_source, crate::CostSource::Unknown);
    }
}
