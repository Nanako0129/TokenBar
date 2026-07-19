//! Kimi CLI / Kimi Code session parser
//!
//! Parses wire.jsonl from both `kimi-cli` and `kimi-code`.
//!
//! ~/.kimi/sessions/[GROUP_ID]/[SESSION_UUID]/wire.jsonl
//!   Token data comes from StatusUpdate messages.
//!
//! ~/.kimi-code/sessions/[WORKSPACE]/[SESSION]/agents/[AGENT]/wire.jsonl
//!   Token data comes from usage.record lines.

use super::utils::file_modified_timestamp_ms;
use super::UnifiedMessage;
use crate::TokenBreakdown;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Top-level wire.jsonl line: either metadata or a timestamped message
#[derive(Debug, Deserialize)]
struct WireLine {
    timestamp: Option<f64>,
    message: Option<WireMessage>,
    #[serde(rename = "type")]
    line_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(rename = "type")]
    msg_type: String,
    payload: Option<StatusPayload>,
}

#[derive(Debug, Deserialize)]
struct StatusPayload {
    token_usage: Option<TokenUsage>,
    #[allow(dead_code)]
    message_id: Option<String>,
}

/// Token usage counts shared by both wire formats.
///
/// Legacy kimi-cli StatusUpdate payloads use snake_case field names;
/// kimi-code usage.record lines use the camelCase aliases.
#[derive(Debug, Deserialize)]
struct TokenUsage {
    #[serde(alias = "inputOther")]
    input_other: Option<i64>,
    output: Option<i64>,
    #[serde(alias = "inputCacheRead")]
    input_cache_read: Option<i64>,
    #[serde(alias = "inputCacheCreation")]
    input_cache_creation: Option<i64>,
}

impl TokenUsage {
    /// Clamp negative counts to zero and build a breakdown.
    /// Returns `None` when every count is zero so callers can skip the entry.
    fn to_breakdown(&self) -> Option<TokenBreakdown> {
        let input = self.input_other.unwrap_or(0).max(0);
        let output = self.output.unwrap_or(0).max(0);
        let cache_read = self.input_cache_read.unwrap_or(0).max(0);
        let cache_write = self.input_cache_creation.unwrap_or(0).max(0);

        if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
            return None;
        }

        Some(TokenBreakdown {
            input,
            output,
            cache_read,
            cache_write,
            // Kimi wire protocols do not expose reasoning tokens; all reasoning included in output.
            reasoning: 0,
        })
    }
}

/// Default model name when config.json is not available
const DEFAULT_MODEL: &str = "kimi-for-coding";
const DEFAULT_PROVIDER: &str = "moonshot";

/// Locate the legacy Kimi CLI config consumed by `parse_kimi_file`.
pub(crate) fn kimi_config_path(wire_path: &Path) -> Option<PathBuf> {
    if is_kimi_code_path(wire_path) {
        return None;
    }
    let sessions_dir = wire_path.parent()?.parent()?.parent()?;
    Some(sessions_dir.parent()?.join("config.json"))
}

/// Read model name from ~/.kimi/config.json if available
fn read_model_from_config(wire_path: &Path) -> String {
    if let Some(config_path) = kimi_config_path(wire_path) {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            if let Ok(bytes) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(model) = bytes.get("model").and_then(|v| v.as_str()) {
                    if !model.is_empty() {
                        return model.to_string();
                    }
                }
            }
        }
    }
    DEFAULT_MODEL.to_string()
}

/// Extract session ID from the wire.jsonl path
/// Path format: ~/.kimi/sessions/GROUP_ID/SESSION_UUID/wire.jsonl
fn extract_session_id(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Check whether a wire.jsonl path belongs to kimi-code.
///
/// kimi-code writes `<root>/sessions/WORKSPACE/SESSION/agents/AGENT/wire.jsonl`
/// while legacy kimi-cli writes `<root>/sessions/GROUP/UUID/wire.jsonl`, so the
/// grandparent directory component (`agents`) distinguishes the formats. The
/// layout under the root is created by kimi-code itself, so this holds for the
/// default `~/.kimi-code` root and custom `KIMI_CODE_HOME` roots alike.
pub fn is_kimi_code_path(path: &Path) -> bool {
    path.parent()
        .and_then(|agent_dir| agent_dir.parent())
        .and_then(|agents_dir| agents_dir.file_name())
        .is_some_and(|name| name == "agents")
}

/// Extract session ID from a kimi-code wire.jsonl path.
/// Path format: ~/.kimi-code/sessions/WORKSPACE/SESSION_UUID/agents/AGENT/wire.jsonl
fn extract_session_id_from_kimi_code_path(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Strip the `kimi-code/` prefix from model IDs emitted by kimi-code.
fn normalize_kimi_code_model(model: &str) -> String {
    model
        .strip_prefix("kimi-code/")
        .unwrap_or(model)
        .to_string()
}

/// Kimi Code wire.jsonl line structure.
#[derive(Debug, Deserialize)]
struct KimiCodeWireLine {
    #[serde(rename = "type")]
    line_type: String,
    model: Option<String>,
    usage: Option<TokenUsage>,
    #[serde(rename = "usageScope")]
    usage_scope: Option<String>,
    time: Option<i64>,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default, rename = "eventId")]
    event_id: Option<Value>,
    #[serde(default, rename = "turnId")]
    turn_id: Option<Value>,
    #[serde(default, rename = "requestId")]
    request_id: Option<Value>,
    #[serde(default, rename = "messageId")]
    message_id: Option<Value>,
}

/// Build a stable, explainable identity for a Kimi Code usage record.
///
/// The session, event timestamp/identity, model, scope, and complete token
/// payload are retained so an exact replay collapses without merging two calls
/// that happen to have the same token counts but belong to different turns.
fn identity_value(value: &Option<Value>) -> String {
    value.as_ref().map(ToString::to_string).unwrap_or_default()
}

fn kimi_code_dedup_key(
    session_id: &str,
    wire_line: &KimiCodeWireLine,
    model: &str,
    tokens: &TokenBreakdown,
) -> String {
    format!(
        "kimi:{session_id}:code:{line_type}:{scope}:{time}:{id}:{event_id}:{turn_id}:{request_id}:{message_id}:{model}:{input}:{output}:{cache_read}:{cache_write}:{reasoning}",
        line_type = wire_line.line_type,
        scope = wire_line.usage_scope.as_deref().unwrap_or(""),
        time = wire_line.time.unwrap_or(0),
        id = identity_value(&wire_line.id),
        event_id = identity_value(&wire_line.event_id),
        turn_id = identity_value(&wire_line.turn_id),
        request_id = identity_value(&wire_line.request_id),
        message_id = identity_value(&wire_line.message_id),
        input = tokens.input,
        output = tokens.output,
        cache_read = tokens.cache_read,
        cache_write = tokens.cache_write,
        reasoning = tokens.reasoning,
    )
}

/// Parse a Kimi Code wire.jsonl file.
pub fn parse_kimi_code_file(path: &Path) -> Vec<UnifiedMessage> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let session_id = extract_session_id_from_kimi_code_path(path);
    let fallback_timestamp = file_modified_timestamp_ms(path);
    let reader = BufReader::new(file);
    let mut messages: Vec<UnifiedMessage> = Vec::new();
    let mut seen = HashSet::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut bytes = trimmed.as_bytes().to_vec();
        let wire_line = match simd_json::from_slice::<KimiCodeWireLine>(&mut bytes) {
            Ok(wl) => wl,
            Err(_) => continue,
        };

        // `step.end` also carries usage, but duplicates the same usage.record
        // emitted in the same turn, so only usage.record is counted.
        if wire_line.line_type != "usage.record" {
            continue;
        }

        // Missing and session-scoped usage is bookkeeping such as compaction;
        // only explicit turn-scoped records represent billable calls.
        if wire_line.usage_scope.as_deref() != Some("turn") {
            continue;
        }

        let Some(tokens) = wire_line.usage.as_ref().and_then(TokenUsage::to_breakdown) else {
            continue;
        };

        let model = wire_line
            .model
            .as_deref()
            .map(normalize_kimi_code_model)
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let timestamp_ms = wire_line.time.unwrap_or(fallback_timestamp);
        let dedup_key = kimi_code_dedup_key(&session_id, &wire_line, &model, &tokens);
        if !seen.insert(dedup_key.clone()) {
            continue;
        }

        messages.push(UnifiedMessage::new_with_dedup(
            "kimi",
            model,
            DEFAULT_PROVIDER,
            session_id.clone(),
            timestamp_ms,
            tokens,
            0.0,
            Some(dedup_key),
        ));
    }

    messages
}

/// Parse a Kimi CLI wire.jsonl file
pub fn parse_kimi_file(path: &Path) -> Vec<UnifiedMessage> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let model = read_model_from_config(path);
    let session_id = extract_session_id(path);

    let reader = BufReader::new(file);
    let mut messages: Vec<UnifiedMessage> = Vec::new();
    let mut keyed_indices: HashMap<String, usize> = HashMap::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut bytes = trimmed.as_bytes().to_vec();
        let wire_line = match simd_json::from_slice::<WireLine>(&mut bytes) {
            Ok(wl) => wl,
            Err(_) => continue,
        };

        // Skip metadata lines (first line: {"type": "metadata", ...})
        if wire_line.line_type.as_deref() == Some("metadata") {
            continue;
        }

        let message = match wire_line.message {
            Some(m) => m,
            None => continue,
        };

        // Only process StatusUpdate messages
        if message.msg_type != "StatusUpdate" {
            continue;
        }

        let payload = match message.payload {
            Some(p) => p,
            None => continue,
        };

        let token_usage = match payload.token_usage {
            Some(u) => u,
            None => continue,
        };

        // Convert Unix seconds (float) to milliseconds, fallback to file mtime
        let timestamp_ms = wire_line
            .timestamp
            .map(|ts| (ts * 1000.0) as i64)
            .unwrap_or_else(|| file_modified_timestamp_ms(path));

        let Some(tokens) = token_usage.to_breakdown() else {
            continue;
        };

        let dedup_key = payload.message_id;

        let message = UnifiedMessage::new_with_dedup(
            "kimi",
            model.clone(),
            DEFAULT_PROVIDER,
            session_id.clone(),
            timestamp_ms,
            tokens,
            0.0,
            dedup_key,
        );
        push_or_replace_status_update(&mut messages, &mut keyed_indices, message);
    }

    messages
}

fn should_replace_status_update(existing: &UnifiedMessage, candidate: &UnifiedMessage) -> bool {
    let existing_total = existing.tokens.total();
    let candidate_total = candidate.tokens.total();

    candidate_total > existing_total
        || (candidate_total == existing_total && candidate.timestamp >= existing.timestamp)
}

fn push_or_replace_status_update(
    messages: &mut Vec<UnifiedMessage>,
    keyed_indices: &mut HashMap<String, usize>,
    message: UnifiedMessage,
) {
    let dedup_key = message
        .dedup_key
        .as_ref()
        .filter(|key| !key.is_empty())
        .cloned();

    let Some(dedup_key) = dedup_key else {
        messages.push(message);
        return;
    };

    if let Some(index) = keyed_indices.get(&dedup_key).copied() {
        if should_replace_status_update(&messages[index], &message) {
            messages[index] = message;
        }
        return;
    }

    let index = messages.len();
    messages.push(message);
    keyed_indices.insert(dedup_key, index);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_kimi_config_path_requires_legacy_session_depth() {
        let wire = Path::new("root/.kimi/sessions/group/session/wire.jsonl");
        assert_eq!(
            kimi_config_path(wire),
            Some(PathBuf::from("root/.kimi/config.json"))
        );
        assert_eq!(
            kimi_config_path(Path::new(
                "root/.kimi-code/sessions/workspace/session/agents/main/wire.jsonl"
            )),
            None
        );
        assert_eq!(kimi_config_path(Path::new("wire.jsonl")), None);
    }

    fn create_test_file(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn test_parse_kimi_valid_status_update() {
        let content = r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983426.420942, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 1562, "output": 2463, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "chatcmpl-xxx"}}}"#;
        let file = create_test_file(content);

        let messages = parse_kimi_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "kimi");
        assert_eq!(messages[0].model_id, "kimi-for-coding");
        assert_eq!(messages[0].provider_id, "moonshot");
        assert_eq!(messages[0].tokens.input, 1562);
        assert_eq!(messages[0].tokens.output, 2463);
        assert_eq!(messages[0].tokens.cache_read, 0);
        assert_eq!(messages[0].tokens.cache_write, 0);
        // Timestamp: 1770983426.420942 * 1000 = 1770983426420
        assert_eq!(messages[0].timestamp, 1770983426420);
    }

    #[test]
    fn test_parse_kimi_multi_turn() {
        let content = r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983400.0, "message": {"type": "TurnBegin", "payload": {"user_input": "hello"}}}
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 100, "output": 200, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-1"}}}
{"timestamp": 1770983420.0, "message": {"type": "TurnBegin", "payload": {"user_input": "world"}}}
{"timestamp": 1770983430.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 300, "output": 400, "input_cache_read": 50, "input_cache_creation": 0}, "message_id": "msg-2"}}}"#;
        let file = create_test_file(content);

        let messages = parse_kimi_file(file.path());

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 200);
        assert_eq!(messages[1].tokens.input, 300);
        assert_eq!(messages[1].tokens.output, 400);
        assert_eq!(messages[1].tokens.cache_read, 50);
    }

    #[test]
    fn test_parse_kimi_skip_non_status_update() {
        let content = r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983400.0, "message": {"type": "TurnBegin", "payload": {"user_input": "hello"}}}
{"timestamp": 1770983410.0, "message": {"type": "ContentPart", "payload": {"type": "text", "text": "response"}}}
{"timestamp": 1770983420.0, "message": {"type": "ToolCall", "payload": {"type": "function", "id": "tool_1", "function": {"name": "ReadFile", "arguments": "{}"}}}}"#;
        let file = create_test_file(content);

        let messages = parse_kimi_file(file.path());

        assert!(messages.is_empty());
    }

    #[test]
    fn test_parse_kimi_empty_file() {
        let file = create_test_file("");

        let messages = parse_kimi_file(file.path());

        assert!(messages.is_empty());
    }

    #[test]
    fn test_parse_kimi_tool_call_multi_step() {
        // Simulates a tool-call scenario with multiple StatusUpdate messages in one turn
        let content = r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983400.0, "message": {"type": "TurnBegin", "payload": {"user_input": "read file"}}}
{"timestamp": 1770983405.0, "message": {"type": "StepBegin", "payload": {"n": 1}}}
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 500, "output": 100, "input_cache_read": 200, "input_cache_creation": 0}, "message_id": "msg-step1"}}}
{"timestamp": 1770983415.0, "message": {"type": "ToolCall", "payload": {"type": "function", "id": "tool_1", "function": {"name": "ReadFile", "arguments": "{}"}}}}
{"timestamp": 1770983420.0, "message": {"type": "StepBegin", "payload": {"n": 2}}}
{"timestamp": 1770983425.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 800, "output": 300, "input_cache_read": 400, "input_cache_creation": 100}, "message_id": "msg-step2"}}}"#;
        let file = create_test_file(content);

        let messages = parse_kimi_file(file.path());

        assert_eq!(messages.len(), 2);
        // Step 1
        assert_eq!(messages[0].tokens.input, 500);
        assert_eq!(messages[0].tokens.output, 100);
        assert_eq!(messages[0].tokens.cache_read, 200);
        assert_eq!(messages[0].tokens.cache_write, 0);
        // Step 2
        assert_eq!(messages[1].tokens.input, 800);
        assert_eq!(messages[1].tokens.output, 300);
        assert_eq!(messages[1].tokens.cache_read, 400);
        assert_eq!(messages[1].tokens.cache_write, 100);
    }

    #[test]
    fn test_parse_kimi_with_cache_tokens() {
        let content = r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1771123711.615454, "message": {"type": "StatusUpdate", "payload": {"context_usage": 0.024, "token_usage": {"input_other": 1508, "output": 205, "input_cache_read": 4864, "input_cache_creation": 0}, "message_id": "chatcmpl-2tNw2mhUNfdPMP0Jyie7gDhD"}}}"#;
        let file = create_test_file(content);

        let messages = parse_kimi_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 1508);
        assert_eq!(messages[0].tokens.output, 205);
        assert_eq!(messages[0].tokens.cache_read, 4864);
        assert_eq!(messages[0].tokens.cache_write, 0);
    }

    #[test]
    fn test_parse_kimi_deduplicates_repeated_status_updates_by_message_id() {
        let content = r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 100, "output": 10, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-progressive"}}}
{"timestamp": 1770983420.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 120, "output": 30, "input_cache_read": 5, "input_cache_creation": 0}, "message_id": "msg-progressive"}}}"#;
        let file = create_test_file(content);

        let messages = parse_kimi_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].dedup_key.as_deref(), Some("msg-progressive"));
        assert_eq!(messages[0].tokens.input, 120);
        assert_eq!(messages[0].tokens.output, 30);
        assert_eq!(messages[0].tokens.cache_read, 5);
        assert_eq!(messages[0].timestamp, 1770983420000);
    }

    #[test]
    fn test_parse_kimi_keeps_distinct_and_missing_message_ids_separate() {
        let content = r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 10, "output": 1, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-1"}}}
{"timestamp": 1770983420.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 20, "output": 2, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-2"}}}
{"timestamp": 1770983430.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 30, "output": 3, "input_cache_read": 0, "input_cache_creation": 0}}}}
{"timestamp": 1770983440.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 40, "output": 4, "input_cache_read": 0, "input_cache_creation": 0}}}}"#;
        let file = create_test_file(content);

        let messages = parse_kimi_file(file.path());

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].dedup_key.as_deref(), Some("msg-1"));
        assert_eq!(messages[1].dedup_key.as_deref(), Some("msg-2"));
        assert!(messages[2].dedup_key.is_none());
        assert!(messages[3].dedup_key.is_none());
    }

    #[test]
    fn test_parse_kimi_skips_zero_token_entries() {
        let content = r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 0, "output": 0, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-empty"}}}"#;
        let file = create_test_file(content);

        let messages = parse_kimi_file(file.path());

        assert!(messages.is_empty());
    }

    #[test]
    fn test_parse_kimi_malformed_lines() {
        let content = r#"{"type": "metadata", "protocol_version": "1.3"}
not valid json at all
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 100, "output": 200, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-1"}}}"#;
        let file = create_test_file(content);

        let messages = parse_kimi_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 100);
    }

    fn create_kimi_code_test_file(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(".kimi-code")
            .join("sessions")
            .join("workspace")
            .join("session-code")
            .join("agents")
            .join("main")
            .join("wire.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    fn kimi_code_usage(time: i64, turn_id: &str) -> String {
        format!(
            r#"{{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{{"inputOther":100,"output":50,"inputCacheRead":10,"inputCacheCreation":0}},"usageScope":"turn","time":{time},"turnId":"{turn_id}"}}"#
        )
    }

    #[test]
    fn kimi_code_is_old_parser_zero_new_parser_nonzero() {
        let content = kimi_code_usage(1_780_319_377_014, "turn-a");
        let (_dir, path) = create_kimi_code_test_file(&content);

        assert!(parse_kimi_file(&path).is_empty());
        let messages = parse_kimi_code_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "kimi");
        assert_eq!(messages[0].model_id, "kimi-for-coding");
        assert_eq!(messages[0].provider_id, "moonshot");
        assert_eq!(messages[0].session_id, "session-code");
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 50);
        assert_eq!(messages[0].tokens.cache_read, 10);
    }

    #[test]
    fn kimi_code_extreme_tokens_do_not_overflow_zero_check() {
        let content = format!(
            r#"{{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{{"inputOther":{},"output":1,"inputCacheRead":{},"inputCacheCreation":1}},"usageScope":"turn","time":1,"turnId":"turn-extreme"}}"#,
            i64::MAX,
            i64::MAX,
        );
        let (_dir, path) = create_kimi_code_test_file(&content);

        let messages = parse_kimi_code_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, i64::MAX);
        assert_eq!(messages[0].tokens.output, 1);
        assert_eq!(messages[0].tokens.cache_read, i64::MAX);
        assert_eq!(messages[0].tokens.cache_write, 1);
        assert_eq!(messages[0].tokens.total(), i64::MAX);
    }

    #[test]
    fn kimi_code_counts_only_explicit_turn_scope() {
        let content = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"usage.record","usage":{"inputOther":999,"output":999},"usageScope":"session","time":1}"#,
            r#"{"type":"usage.record","usage":{"inputOther":888,"output":888},"time":2}"#,
            kimi_code_usage(3, "turn-a"),
        );
        let (_dir, path) = create_kimi_code_test_file(&content);

        let messages = parse_kimi_code_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].timestamp, 3);
    }

    #[test]
    fn kimi_code_replay_dedup_keeps_distinct_turns() {
        let replay = kimi_code_usage(10, "turn-a");
        let distinct_turn = kimi_code_usage(10, "turn-b");
        let later_turn = kimi_code_usage(11, "turn-c");
        let content = format!("{replay}\n{replay}\n{distinct_turn}\n{later_turn}\n");
        let (_dir, path) = create_kimi_code_test_file(&content);

        let messages = parse_kimi_code_file(&path);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].timestamp, 10);
        assert_eq!(messages[1].timestamp, 10);
        assert_eq!(messages[2].timestamp, 11);
        assert_ne!(messages[0].dedup_key, messages[1].dedup_key);
        assert_ne!(messages[1].dedup_key, messages[2].dedup_key);
    }

    #[test]
    fn kimi_legacy_and_kimi_code_paths_coexist_under_one_client() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir
            .path()
            .join(".kimi")
            .join("sessions")
            .join("group")
            .join("legacy-session")
            .join("wire.jsonl");
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy_path,
            r#"{"timestamp":1770983410.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":7,"output":3},"message_id":"legacy-1"}}}"#,
        )
        .unwrap();
        let code_path = dir
            .path()
            .join(".kimi-code")
            .join("sessions")
            .join("workspace")
            .join("code-session")
            .join("agents")
            .join("main")
            .join("wire.jsonl");
        std::fs::create_dir_all(code_path.parent().unwrap()).unwrap();
        std::fs::write(&code_path, kimi_code_usage(20, "turn-code")).unwrap();

        let legacy = parse_kimi_file(&legacy_path);
        let code = parse_kimi_code_file(&code_path);
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].session_id, "legacy-session");
        assert_eq!(legacy[0].tokens.input, 7);
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].session_id, "code-session");
        assert_eq!(code[0].tokens.input, 100);
    }

    #[test]
    fn kimi_code_path_detection_uses_agents_topology() {
        assert!(is_kimi_code_path(Path::new(
            "/home/user/.kimi-code/sessions/workspace/session/agents/main/wire.jsonl"
        )));
        assert!(!is_kimi_code_path(Path::new(
            "/home/user/.kimi-code/sessions/workspace/session/wire.jsonl"
        )));
        assert!(!is_kimi_code_path(Path::new(
            "/home/user/.kimi/sessions/group/session/wire.jsonl"
        )));
    }
}
