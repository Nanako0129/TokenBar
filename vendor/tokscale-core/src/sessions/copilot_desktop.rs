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
    created_at: Option<String>,
    agent: Option<String>,
}

#[derive(Debug, Default)]
struct SessionStateMetadata {
    model: Option<String>,
    cwd: Option<String>,
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

    let mut stmt = match conn.prepare(
        r#"
        SELECT
            id,
            model,
            total_input_tokens,
            total_output_tokens,
            total_cached_tokens,
            total_reasoning_tokens,
            created_at,
            agent
        FROM sessions
        WHERE total_input_tokens > 0
           OR total_output_tokens > 0
           OR total_cached_tokens > 0
           OR total_reasoning_tokens > 0
        "#,
    ) {
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
            model: row.get(1)?,
            total_input_tokens: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            total_output_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            total_cached_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            total_reasoning_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            created_at: row.get(6)?,
            agent: row.get(7)?,
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

pub(crate) fn session_state_event_paths(db_path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let Some(copilot_root) = db_path.parent() else {
        return Ok(Vec::new());
    };
    let entries = match std::fs::read_dir(copilot_root.join("session-state")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            return Err(std::io::Error::other(
                "Copilot Desktop session-state entry is not a directory",
            ));
        }
        let path = entry.path().join("events.jsonl");
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                std::fs::File::open(&path)?;
                paths.push(path);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

fn session_row_to_message(db_path: &Path, row: CopilotDesktopSessionRow) -> UnifiedMessage {
    let metadata = read_session_state_metadata(db_path, &row.id);
    let model_id = metadata
        .model
        .as_deref()
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

    let mut message = UnifiedMessage::new_with_dedup(
        "copilot",
        model_id,
        provider_id,
        row.id.clone(),
        timestamp_ms,
        super::copilot::normalize_input_tokens(
            row.total_input_tokens,
            row.total_output_tokens,
            row.total_cached_tokens,
            0,
            row.total_reasoning_tokens,
        ),
        0.0,
        Some(format!("copilot-desktop:{}", row.id)),
    );
    message.agent = row
        .agent
        .map(|agent| agent.trim().to_string())
        .filter(|agent| !agent.is_empty());

    if let Some(workspace_key) = metadata.cwd.as_deref().and_then(normalize_workspace_key) {
        let workspace_label = workspace_label_from_key(&workspace_key);
        message.set_workspace(Some(workspace_key), workspace_label);
    }

    message
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
            "session.start" if metadata.cwd.is_none() => {
                metadata.cwd = event
                    .pointer("/data/context/cwd")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|cwd| !cwd.is_empty())
                    .map(str::to_string);
            }
            "session.model_change" => {
                if let Some(model) = event
                    .pointer("/data/newModel")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty() && model != &"auto")
                {
                    metadata.model = Some(model.to_string());
                }
            }
            _ => {}
        }
    }

    metadata
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
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })
        .or_else(|| {
            let numeric = value.parse::<i64>().ok()?;
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
    use crate::sessions::CostSource;
    use rusqlite::{params, Connection};
    use std::fs::{self, File};
    use std::io::Write;

    fn create_copilot_desktop_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE sessions (
                id TEXT,
                model TEXT,
                total_input_tokens INTEGER,
                total_output_tokens INTEGER,
                total_cached_tokens INTEGER,
                total_reasoning_tokens INTEGER,
                total_nano_aiu INTEGER,
                created_at TEXT,
                agent TEXT
            );
            "#,
        )
        .unwrap();
        conn
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_session(
        conn: &Connection,
        id: &str,
        model: &str,
        input: i64,
        output: i64,
        cached: i64,
        reasoning: i64,
        nano_aiu: i64,
        agent: Option<&str>,
    ) {
        conn.execute(
            r#"
            INSERT INTO sessions (
                id, model, total_input_tokens, total_output_tokens,
                total_cached_tokens, total_reasoning_tokens, total_nano_aiu,
                created_at, agent
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                id,
                model,
                input,
                output,
                cached,
                reasoning,
                nano_aiu,
                "2026-07-01T12:34:56Z",
                agent
            ],
        )
        .unwrap();
    }

    fn write_events(root: &Path, session_id: &str, lines: &[&str]) -> PathBuf {
        let events_dir = root.join("session-state").join(session_id);
        fs::create_dir_all(&events_dir).unwrap();
        let path = events_dir.join("events.jsonl");
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn parse_copilot_desktop_db_reads_token_sessions_and_agent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(
            &conn,
            "session-1",
            "gpt-5.1-codex",
            100,
            50,
            25,
            10,
            0,
            Some(" github.copilot.default "),
        );
        drop(conn);

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.client, "copilot");
        assert_eq!(message.model_id, "gpt-5.1-codex");
        assert_eq!(message.provider_id, "openai");
        assert_eq!(message.session_id, "session-1");
        assert_eq!(message.timestamp, 1_782_909_296_000);
        assert_eq!(message.tokens.input, 75);
        assert_eq!(message.tokens.output, 50);
        assert_eq!(message.tokens.cache_read, 25);
        assert_eq!(message.tokens.cache_write, 0);
        assert_eq!(message.tokens.reasoning, 10);
        assert_eq!(message.agent.as_deref(), Some("github.copilot.default"));
        assert_eq!(message.cost, 0.0);
        assert_eq!(message.cost_source, CostSource::Unknown);
        assert_eq!(
            message.dedup_key.as_deref(),
            Some("copilot-desktop:session-1")
        );
    }

    #[test]
    fn parse_copilot_desktop_db_skips_aiu_only_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 0, 0, 0, 0, 42, None);
        drop(conn);

        assert!(parse_copilot_desktop_db(&db_path).is_empty());
    }

    #[test]
    fn parse_copilot_desktop_db_enriches_model_and_workspace_from_events() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 50, 0, 0, 0, None);
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
    fn session_state_dependencies_are_sorted_and_token_independent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        File::create(&db_path).unwrap();
        let z = write_events(dir.path(), "z-session", &["{}"]);
        let a = write_events(dir.path(), "a-session", &["{}"]);
        fs::create_dir_all(dir.path().join("session-state/missing-events")).unwrap();

        assert_eq!(session_state_event_paths(&db_path).unwrap(), vec![a, z]);
    }

    #[test]
    fn session_state_dependency_probe_reports_non_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        File::create(&db_path).unwrap();
        fs::create_dir_all(dir.path().join("session-state")).unwrap();
        File::create(dir.path().join("session-state/not-a-directory")).unwrap();

        assert!(session_state_event_paths(&db_path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn session_state_dependency_probe_reports_unreadable_events() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        File::create(&db_path).unwrap();
        let events = write_events(dir.path(), "session-1", &["{}"]);
        fs::set_permissions(&events, fs::Permissions::from_mode(0o000)).unwrap();

        let result = session_state_event_paths(&db_path);
        fs::set_permissions(&events, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(result.is_err());
    }

    #[test]
    fn parse_timestamp_handles_sqlite_and_numeric_forms() {
        assert_eq!(
            parse_iso8601_timestamp_ms("2026-07-01 12:34:56.789"),
            Some(1_782_909_296_789)
        );
        assert_eq!(
            parse_iso8601_timestamp_ms("2026-07-01T12:34:56.789"),
            Some(1_782_909_296_789)
        );
        assert_eq!(
            parse_iso8601_timestamp_ms("1782909296"),
            Some(1_782_909_296_000)
        );
        assert_eq!(
            parse_iso8601_timestamp_ms("1782909296789"),
            Some(1_782_909_296_789)
        );
        assert_eq!(parse_iso8601_timestamp_ms("not-a-timestamp"), None);
    }
}
