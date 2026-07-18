//! Kiro session parser
//!
//! Parses session data from three Kiro sources:
//! 1. CLI JSON + same-stem JSONL: ~/.kiro/sessions/cli/*.json + *.jsonl
//! 2. macOS IDE globalStorage: snapshot, execution, and workspace-session files
//! 3. CLI SQLite: ~/Library/Application Support/kiro-cli/data.sqlite3
//!    (conversations_v2 table with history[*].request_metadata)
//!
//! Token counts from all three sources are surfaced as estimated usage. The
//! estimates use the source's available context, character, or metadata hints;
//! downstream must not treat them as provider-reported token counts.

use super::utils::file_modified_timestamp_ms;
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use tracing::warn;

const CLIENT_ID: &str = "kiro";
const PROVIDER_ID: &str = "amazon-bedrock";
const UNKNOWN_MODEL: &str = "unknown";

#[derive(Debug, Deserialize)]
struct KiroSessionHeader {
    session_id: Option<String>,
    cwd: Option<String>,
    session_state: Option<KiroSessionState>,
}

#[derive(Debug, Deserialize)]
struct KiroSessionState {
    rts_model_state: Option<KiroRtsModelState>,
    conversation_metadata: Option<KiroConversationMetadata>,
}

#[derive(Debug, Deserialize)]
struct KiroRtsModelState {
    model_info: Option<KiroModelInfo>,
}

#[derive(Debug, Deserialize)]
struct KiroModelInfo {
    model_id: Option<String>,
    context_window_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct KiroConversationMetadata {
    user_turn_metadatas: Option<Vec<KiroTurnMetadata>>,
}

#[derive(Debug, Deserialize)]
struct KiroTurnMetadata {
    input_token_count: Option<i64>,
    output_token_count: Option<i64>,
    end_timestamp: Option<serde_json::Value>,
    total_request_count: Option<i32>,
    message_ids: Option<Vec<Option<String>>>,
    context_usage_percentage: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct KiroJsonlEntry {
    kind: String,
    data: Option<KiroJsonlData>,
}

#[derive(Debug, Deserialize)]
struct KiroJsonlData {
    message_id: Option<String>,
    content: Option<Vec<KiroContentPart>>,
    meta: Option<KiroEntryMeta>,
}

#[derive(Debug, Deserialize)]
struct KiroContentPart {
    kind: Option<String>,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KiroEntryMeta {
    timestamp: Option<f64>,
}

#[derive(Debug, Clone, Default)]
struct KiroMessageContent {
    prompt_chars: usize,
    assistant_chars: usize,
    prompt_timestamp_ms: Option<i64>,
}

/// Return the message sidecar consumed by a Kiro CLI session header.
/// GlobalStorage and `.chat` artifacts are self-contained.
pub(crate) fn kiro_related_messages_path(session_path: &Path) -> Option<PathBuf> {
    if is_kiro_global_storage_source(session_path) {
        return None;
    }
    Some(session_path.with_extension("jsonl"))
}

pub fn parse_kiro_file(path: &Path) -> Vec<UnifiedMessage> {
    if is_kiro_global_storage_source(path) {
        return parse_kiro_global_storage_file(path);
    }

    let fallback_timestamp = file_modified_timestamp_ms(path);

    let mut json_bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Vec::new(),
    };

    let header = match simd_json::from_slice::<KiroSessionHeader>(&mut json_bytes) {
        Ok(header) => header,
        Err(_) => return Vec::new(),
    };

    let session_id = header
        .session_id
        .unwrap_or_else(|| session_id_from_path(path));
    let model_id = header
        .session_state
        .as_ref()
        .and_then(|state| state.rts_model_state.as_ref())
        .and_then(|state| state.model_info.as_ref())
        .and_then(|info| info.model_id.as_deref())
        .filter(|model| !model.trim().is_empty())
        .unwrap_or(UNKNOWN_MODEL)
        .to_string();
    let workspace_key = header.cwd.as_deref().and_then(normalize_workspace_key);
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);
    let context_window = header
        .session_state
        .as_ref()
        .and_then(|state| state.rts_model_state.as_ref())
        .and_then(|state| state.model_info.as_ref())
        .and_then(|info| info.context_window_tokens)
        .unwrap_or(0);
    let turns = header
        .session_state
        .and_then(|state| state.conversation_metadata)
        .and_then(|metadata| metadata.user_turn_metadatas)
        .unwrap_or_default();

    let Some(jsonl_path) = kiro_related_messages_path(path) else {
        return Vec::new();
    };
    let mut content_by_message_id: HashMap<String, KiroMessageContent> = HashMap::new();

    if let Ok(jsonl_file) = std::fs::File::open(&jsonl_path) {
        let reader = BufReader::new(jsonl_file);
        let mut pending_prompt: Option<(usize, Option<i64>)> = None;

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
            let entry = match simd_json::from_slice::<KiroJsonlEntry>(&mut bytes) {
                Ok(entry) => entry,
                Err(_) => continue,
            };

            let Some(data) = entry.data else {
                continue;
            };
            let Some(message_id) = data.message_id else {
                continue;
            };

            let text_chars = text_char_count(data.content.as_deref());

            match entry.kind.as_str() {
                "Prompt" => {
                    let timestamp_ms = data
                        .meta
                        .and_then(|meta| meta.timestamp)
                        .map(seconds_to_millis);
                    pending_prompt = Some((text_chars, timestamp_ms));
                }
                "AssistantMessage" => {
                    let message = content_by_message_id.entry(message_id).or_default();
                    if let Some((prompt_chars, prompt_ts)) = pending_prompt.take() {
                        message.prompt_chars += prompt_chars;
                        if message.prompt_timestamp_ms.is_none() {
                            message.prompt_timestamp_ms = prompt_ts;
                        }
                    }
                    message.assistant_chars += text_chars;
                }
                _ => {}
            }
        }
    }

    turns
        .into_iter()
        .enumerate()
        .filter_map(|(index, turn)| {
            let message_ids = turn.message_ids.unwrap_or_default();
            let mut prompt_chars = 0;
            let mut assistant_chars = 0;
            let mut prompt_timestamp_ms = None;

            for message_id in message_ids.iter().flatten() {
                let Some(content) = content_by_message_id.get(message_id) else {
                    continue;
                };
                prompt_chars += content.prompt_chars;
                assistant_chars += content.assistant_chars;
                if prompt_timestamp_ms.is_none() {
                    prompt_timestamp_ms = content.prompt_timestamp_ms;
                }
            }

            // NOTE: when explicit per-turn counts are absent (the common case —
            // Kiro currently reports zero), input/output below are ESTIMATED, not
            // measured: input is derived from context_usage_percentage *
            // context_window and output from char_count / 4. Downstream must not
            // treat these as exact token counts.
            let explicit_input = turn.input_token_count.unwrap_or(0).max(0);
            let explicit_output = turn.output_token_count.unwrap_or(0).max(0);
            let input = if explicit_input > 0 {
                explicit_input
            } else if context_window > 0 {
                let ctx_pct = turn.context_usage_percentage.unwrap_or(0.0);
                if ctx_pct > 0.0 {
                    ((context_window as f64) * ctx_pct / 100.0) as i64
                } else {
                    estimate_tokens(prompt_chars)
                }
            } else {
                estimate_tokens(prompt_chars)
            };
            let output = if explicit_output > 0 {
                explicit_output
            } else {
                estimate_tokens(assistant_chars)
            };

            if input + output == 0 {
                return None;
            }

            let end_timestamp_ms = parse_timestamp_value(turn.end_timestamp.as_ref());
            let duration_ms = duration_between_ms(prompt_timestamp_ms, end_timestamp_ms);
            let timestamp = prompt_timestamp_ms
                .or(end_timestamp_ms)
                .unwrap_or(fallback_timestamp);

            let mut message = UnifiedMessage::new_with_dedup(
                CLIENT_ID,
                model_id.clone(),
                PROVIDER_ID,
                session_id.clone(),
                timestamp,
                TokenBreakdown {
                    input,
                    output,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0,
                Some(format!("{}:{}", session_id, index)),
            );
            message.message_count = turn.total_request_count.unwrap_or(1).max(1);
            message.duration_ms = duration_ms;
            message.is_turn_start = true;
            message.set_workspace(workspace_key.clone(), workspace_label.clone());
            Some(message)
        })
        .collect()
}

fn text_char_count(content: Option<&[KiroContentPart]>) -> usize {
    content
        .unwrap_or_default()
        .iter()
        .filter(|part| part.kind.as_deref().is_none_or(|kind| kind == "text"))
        .filter_map(|part| part.data.as_deref())
        .map(str::chars)
        .map(Iterator::count)
        .sum()
}

fn estimate_tokens(chars: usize) -> i64 {
    chars.div_ceil(4) as i64
}

fn seconds_to_millis(seconds: f64) -> i64 {
    // Scale fractional seconds to milliseconds (preserving sub-second
    // precision), then clamp into i64 range. The `f64 as i64` cast saturates
    // rather than wrapping on out-of-range/garbage timestamps, so the
    // seconds->ms conversion cannot overflow.
    let millis = seconds * 1000.0;
    if millis.is_nan() {
        0
    } else {
        millis.clamp(i64::MIN as f64, i64::MAX as f64) as i64
    }
}

fn duration_between_ms(start_ms: Option<i64>, end_ms: Option<i64>) -> Option<i64> {
    let duration = end_ms?.saturating_sub(start_ms?);
    (duration > 0).then_some(duration)
}

fn parse_timestamp_value(value: Option<&serde_json::Value>) -> Option<i64> {
    match value? {
        serde_json::Value::Number(number) => number.as_f64().map(|timestamp| {
            if timestamp.abs() < 1_000_000_000_000.0 {
                seconds_to_millis(timestamp)
            } else {
                timestamp as i64
            }
        }),
        serde_json::Value::String(timestamp) => chrono::DateTime::parse_from_rfc3339(timestamp)
            .ok()
            .map(|dt| dt.timestamp_millis())
            .or_else(|| timestamp.parse::<f64>().ok().map(seconds_to_millis)),
        _ => None,
    }
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn is_kiro_global_storage_path(path: &Path) -> bool {
    let mut saw_global_storage = false;
    let mut saw_extension_root = false;
    for component in path.components() {
        let component = component.as_os_str().to_string_lossy();
        saw_global_storage |= component == "globalStorage";
        saw_extension_root |= component == "kiro.kiroagent";
    }
    saw_global_storage && saw_extension_root
}

fn is_kiro_chat_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("chat"))
}

pub(crate) fn is_kiro_global_storage_source(path: &Path) -> bool {
    is_kiro_global_storage_path(path) || is_kiro_chat_path(path)
}

#[derive(Debug, Default)]
struct KiroSnapshotTextCounts {
    prompt_chars: usize,
    assistant_chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KiroSnapshotRole {
    Prompt,
    Assistant,
}

fn collect_kiro_snapshot_text(
    value: &Value,
    counts: &mut KiroSnapshotTextCounts,
    mut role: Option<KiroSnapshotRole>,
) {
    match value {
        Value::Object(map) => {
            // `role` is authoritative, so unknown roles clear inheritance. `type`
            // also labels neutral payload nodes such as `text`; only explicit
            // tool/unknown types clear the inherited conversation role.
            if let Some(kind) = map.get("role").and_then(Value::as_str) {
                role = match kind {
                    "user" | "prompt" | "human" => Some(KiroSnapshotRole::Prompt),
                    "assistant" | "response" | "bot" => Some(KiroSnapshotRole::Assistant),
                    _ => None,
                };
            }
            if let Some(kind) = map.get("type").and_then(Value::as_str) {
                role = match kind {
                    "user" | "prompt" | "human" => Some(KiroSnapshotRole::Prompt),
                    "assistant" | "response" | "bot" => Some(KiroSnapshotRole::Assistant),
                    "tool" | "unknown" => None,
                    _ => role,
                };
            }

            // These keys are aliases within each group. Equal subtrees are one
            // payload; distinct subtrees are all meaningful conversation data.
            for group in [
                &["prompt", "response", "content", "text", "message"][..],
                &[
                    "messages",
                    "conversation",
                    "chat",
                    "transcript",
                    "entries",
                    "events",
                    "history",
                ][..],
                &["parts", "items", "nodes"][..],
            ] {
                let mut visited: Vec<&Value> = Vec::new();
                for key in group {
                    if let Some(item) = map.get(*key) {
                        if visited.contains(&item) {
                            continue;
                        }
                        visited.push(item);
                        collect_kiro_snapshot_text(item, counts, role);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_kiro_snapshot_text(item, counts, role);
            }
        }
        Value::String(text) => match role {
            Some(KiroSnapshotRole::Prompt) => counts.prompt_chars += text.chars().count(),
            Some(KiroSnapshotRole::Assistant) => counts.assistant_chars += text.chars().count(),
            None => {}
        },
        _ => {}
    }
}

fn find_kiro_snapshot_model_id(value: &Value) -> Option<String> {
    fn is_pseudo_model(model: &str) -> bool {
        matches!(
            model.to_ascii_lowercase().as_str(),
            "agent" | "auto" | "qdev"
        )
    }

    match value {
        Value::Object(map) => {
            for key in ["model_id", "modelId", "model"] {
                if let Some(model) = map.get(key).and_then(Value::as_str) {
                    let model = model.trim();
                    if !model.is_empty() && !is_pseudo_model(model) {
                        return Some(model.to_string());
                    }
                }
            }
            for key in [
                "messages",
                "conversation",
                "chat",
                "transcript",
                "entries",
                "events",
                "history",
                "prompt",
                "response",
                "content",
                "text",
                "message",
                "parts",
                "items",
                "nodes",
                "promptLogs",
                "completionOptions",
            ] {
                if let Some(item) = map.get(key) {
                    if let Some(model) = find_kiro_snapshot_model_id(item) {
                        return Some(model);
                    }
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_kiro_snapshot_model_id),
        _ => None,
    }
}

fn kiro_global_storage_workspace(path: &Path) -> Option<String> {
    let mut components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned());
    while let Some(component) = components.next() {
        if component == "kiro.kiroagent" {
            let workspace = components.next()?;
            if workspace == "workspace-sessions" {
                let nested_workspace = components.next()?;
                return components.next().map(|_| nested_workspace);
            }
            return Some(workspace);
        }
    }
    None
}

fn parse_kiro_global_storage_file(path: &Path) -> Vec<UnifiedMessage> {
    let fallback_timestamp = file_modified_timestamp_ms(path);
    let json = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(_) => return Vec::new(),
    };
    let value: Value = match serde_json::from_str(&json) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };

    if let Some(messages) = try_parse_kiro_execution_file(&value, path) {
        return messages;
    }
    if value.get("executions").is_some() && value.get("version").is_some() {
        return Vec::new();
    }
    if let Some(messages) = try_parse_kiro_workspace_session(&value, path, fallback_timestamp) {
        return messages;
    }
    // Generic role/content traversal is valid only for legacy `.chat` snapshots.
    // JSON and extensionless sources must match an execution or workspace-session
    // shape above, otherwise mirrored project data could be counted as usage.
    if path.extension().and_then(|extension| extension.to_str()) != Some("chat") {
        return Vec::new();
    }

    let file_stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let workspace = kiro_global_storage_workspace(path);
    let workspace_key = workspace.as_deref().and_then(normalize_workspace_key);
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);
    let session_id = match workspace.as_deref() {
        Some(workspace) => format!("{workspace}/{file_stem}"),
        None => file_stem.to_string(),
    };
    let model_id = find_kiro_snapshot_model_id(&value).unwrap_or_else(|| "auto".to_string());

    let mut counts = KiroSnapshotTextCounts::default();
    collect_kiro_snapshot_text(&value, &mut counts, None);
    let input = estimate_tokens(counts.prompt_chars);
    let output = estimate_tokens(counts.assistant_chars);
    if input + output == 0 {
        return Vec::new();
    }

    let dedup_key = match value.get("executionId").and_then(Value::as_str) {
        Some(execution_id) => format!("{session_id}:globalstorage:exec:{execution_id}"),
        None => format!("{session_id}:globalstorage"),
    };
    let mut message = UnifiedMessage::new_with_dedup(
        CLIENT_ID,
        model_id,
        PROVIDER_ID,
        session_id,
        fallback_timestamp,
        TokenBreakdown {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        },
        0.0,
        Some(dedup_key),
    );
    message.message_count = 1;
    message.is_turn_start = true;
    message.set_workspace(workspace_key, workspace_label);
    vec![message]
}

fn try_parse_kiro_execution_file(value: &Value, path: &Path) -> Option<Vec<UnifiedMessage>> {
    let obj = value.as_object()?;
    let execution_id = obj.get("executionId")?.as_str()?;
    let actions: &[Value] = match obj.get("actions") {
        Some(actions) => actions.as_array()?.as_slice(),
        None if path.extension().and_then(|extension| extension.to_str()) != Some("chat")
            && obj.get("status").is_some()
            && obj
                .get("context")
                .and_then(|context| context.get("messages"))
                .and_then(Value::as_array)
                .is_some() =>
        {
            &[]
        }
        None => return None,
    };
    if obj.get("status").and_then(Value::as_str) != Some("succeed") {
        return Some(Vec::new());
    }

    let session_id = obj
        .get("chatSessionId")
        .and_then(Value::as_str)
        .unwrap_or(execution_id)
        .to_string();
    let start_time = parse_timestamp_value(obj.get("startTime"));
    let timestamp = start_time.unwrap_or_else(|| file_modified_timestamp_ms(path));
    let end_time = parse_timestamp_value(obj.get("endTime"));
    let duration_ms = duration_between_ms(start_time.or(Some(timestamp)), end_time);

    let output_chars: usize = actions
        .iter()
        .filter(|action| {
            matches!(
                action.get("actionType").and_then(Value::as_str),
                Some("say") | Some("reasoning")
            )
        })
        .filter_map(|action| action.get("output"))
        .map(|output| {
            output
                .as_str()
                .map(str::chars)
                .map(Iterator::count)
                .or_else(|| {
                    output
                        .get("message")
                        .and_then(Value::as_str)
                        .map(|text| text.chars().count())
                })
                .unwrap_or(0)
        })
        .sum();

    let context_input_chars = obj
        .get("context")
        .and_then(|context| context.get("messages"))
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("entries").and_then(Value::as_array))
                .flatten()
                .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|entry| entry.get("text").and_then(Value::as_str))
                .map(|text| text.chars().count())
                .sum::<usize>()
        })
        .unwrap_or(0);
    let input_data_chars = obj
        .get("input")
        .and_then(|input| input.get("data"))
        .and_then(|data| data.get("messages"))
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .map(|message| {
                    if let Some(parts) = message.get("content").and_then(Value::as_array) {
                        parts
                            .iter()
                            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .map(|text| text.chars().count())
                            .sum()
                    } else {
                        message
                            .get("content")
                            .and_then(Value::as_str)
                            .map(|text| text.chars().count())
                            .unwrap_or(0)
                    }
                })
                .sum::<usize>()
        })
        .unwrap_or(0);

    let input = estimate_tokens(context_input_chars + input_data_chars);
    let output = estimate_tokens(output_chars);
    if input + output == 0 {
        return Some(Vec::new());
    }

    let model_id = find_kiro_snapshot_model_id(value).unwrap_or_else(|| "auto".to_string());
    let workspace = kiro_global_storage_workspace(path);
    let workspace_key = workspace.as_deref().and_then(normalize_workspace_key);
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);
    let mut message = UnifiedMessage::new_with_dedup(
        CLIENT_ID,
        model_id,
        PROVIDER_ID,
        session_id,
        timestamp,
        TokenBreakdown {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        },
        0.0,
        Some(format!("execution:{execution_id}")),
    );
    message.message_count = 1;
    message.is_turn_start = true;
    message.duration_ms = duration_ms;
    message.set_workspace(workspace_key, workspace_label);
    Some(vec![message])
}

fn try_parse_kiro_workspace_session(
    value: &Value,
    path: &Path,
    fallback_timestamp: i64,
) -> Option<Vec<UnifiedMessage>> {
    let history = value.get("history")?.as_array()?;
    if value.get("sessionId").is_none() && value.get("selectedModel").is_none() {
        return None;
    }

    let file_stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let workspace = kiro_global_storage_workspace(path);
    let workspace_key = workspace.as_deref().and_then(normalize_workspace_key);
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);
    let session_id = value
        .get("sessionId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| match workspace.as_deref() {
            Some(workspace) => format!("{workspace}/{file_stem}"),
            None => file_stem.to_string(),
        });
    let model_id = value
        .get("selectedModel")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .unwrap_or("auto")
        .to_string();

    let mut prompt_chars = 0usize;
    let mut prompt_log_count = 0i32;
    let mut assistant_chars = 0usize;
    for entry in history {
        if let Some(prompt_logs) = entry.get("promptLogs").and_then(Value::as_array) {
            for prompt_log in prompt_logs {
                if let Some(prompt) = prompt_log.get("prompt").and_then(Value::as_str) {
                    prompt_chars += prompt.chars().count();
                    prompt_log_count += 1;
                }
            }
        }
        if entry
            .get("message")
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("assistant")
        {
            assistant_chars += entry
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(Value::as_str)
                .map(|text| text.chars().count())
                .unwrap_or(0);
        }
    }
    if prompt_chars == 0 {
        return None;
    }

    let input = estimate_tokens(prompt_chars);
    let output = estimate_tokens(assistant_chars);
    if input + output == 0 {
        return Some(Vec::new());
    }
    let mut message = UnifiedMessage::new_with_dedup(
        CLIENT_ID,
        model_id,
        PROVIDER_ID,
        session_id.clone(),
        fallback_timestamp,
        TokenBreakdown {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        },
        0.0,
        Some(format!("{session_id}:workspace-session")),
    );
    message.message_count = prompt_log_count.max(1);
    message.is_turn_start = true;
    message.set_workspace(workspace_key, workspace_label);
    Some(vec![message])
}

/// Merge Kiro file sources with exact globalStorage execution precedence.
/// Source paths stay attached through suppression and deduplication: only IDE
/// sources seed execution suppression, and identical keys deduplicate only
/// within the IDE or CLI cohort rather than colliding across them.
/// The match is exact for `(workspace, executionId)`; legacy snapshots use the
/// execution's `(workspace, chatSessionId)` and workspace-session artifacts use
/// the session id globally because they live under a separate storage subtree.
pub(crate) fn merge_kiro_source_messages(
    sources: Vec<(PathBuf, Vec<UnifiedMessage>)>,
) -> Vec<UnifiedMessage> {
    let mut executed_sessions: std::collections::HashSet<(Option<String>, String)> =
        std::collections::HashSet::new();
    let mut executed_ids: std::collections::HashSet<(Option<String>, String)> =
        std::collections::HashSet::new();
    let mut executed_session_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut tagged_messages = Vec::new();

    for (path, source_messages) in sources {
        let is_global_storage_source = is_kiro_global_storage_source(&path);
        if is_global_storage_source {
            for message in &source_messages {
                let Some(execution_id) = message
                    .dedup_key
                    .as_deref()
                    .and_then(|key| key.strip_prefix("execution:"))
                else {
                    continue;
                };
                executed_sessions
                    .insert((message.workspace_key.clone(), message.session_id.clone()));
                executed_ids.insert((message.workspace_key.clone(), execution_id.to_string()));
                executed_session_ids.insert(message.session_id.clone());
            }
        }
        tagged_messages.extend(
            source_messages
                .into_iter()
                .map(|message| (is_global_storage_source, message)),
        );
    }

    let mut seen_keys: std::collections::HashSet<(bool, String)> = std::collections::HashSet::new();
    tagged_messages
        .into_iter()
        .filter(|(is_global_storage_source, message)| {
            if !*is_global_storage_source {
                return true;
            }
            let Some(key) = message.dedup_key.as_deref() else {
                return true;
            };
            if let Some((_, execution_id)) = key.split_once(":globalstorage:exec:") {
                return !executed_ids
                    .contains(&(message.workspace_key.clone(), execution_id.to_string()));
            }
            if key.ends_with(":workspace-session") {
                return !executed_session_ids.contains(&message.session_id);
            }
            if !key.ends_with(":globalstorage") {
                return true;
            }
            let stem = message
                .session_id
                .rsplit('/')
                .next()
                .unwrap_or(&message.session_id);
            !executed_sessions.contains(&(message.workspace_key.clone(), stem.to_string()))
        })
        .filter(|(is_global_storage_source, message)| {
            message.dedup_key.as_ref().is_none_or(|key| {
                key.is_empty() || seen_keys.insert((*is_global_storage_source, key.clone()))
            })
        })
        .map(|(_, message)| message)
        .collect()
}

pub fn parse_kiro_sqlite(db_path: &Path) -> Vec<UnifiedMessage> {
    let conn = match Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to open Kiro CLI database"
            );
            return Vec::new();
        }
    };

    let query = "SELECT key, conversation_id, value FROM conversations_v2";
    let mut stmt = match conn.prepare(query) {
        Ok(s) => s,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to prepare Kiro conversations query"
            );
            return Vec::new();
        }
    };

    let rows = match stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    }) {
        Ok(r) => r,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to execute Kiro conversations query"
            );
            return Vec::new();
        }
    };

    let mut messages = Vec::new();

    for row in rows.flatten() {
        let (cwd, conversation_id, json_str) = row;
        let parsed = match serde_json::from_str::<KiroDbConversation>(&json_str) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let context_window = parsed
            .model_info
            .as_ref()
            .and_then(|info| info.context_window_tokens)
            .unwrap_or(0);
        let model_id = parsed
            .model_info
            .as_ref()
            .and_then(|info| info.model_id.as_deref())
            .filter(|m| !m.trim().is_empty() && *m != "auto")
            .unwrap_or(UNKNOWN_MODEL)
            .to_string();
        let workspace_key = normalize_workspace_key(&cwd);
        let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);

        let history = parsed.history.unwrap_or_default();
        for (index, turn) in history.into_iter().enumerate() {
            let Some(meta) = turn.request_metadata else {
                continue;
            };

            // NOTE: these are ESTIMATED, not measured token counts. Kiro's
            // conversations_v2 does not record real per-turn token usage, so
            // input is derived from context_usage_percentage * context_window
            // and output from response_size (char_count) / 4. Downstream must
            // not treat these as exact.
            let ctx_pct = meta.context_usage_percentage.unwrap_or(0.0);
            let response_size = meta.response_size.unwrap_or(0);

            let input = if context_window > 0 && ctx_pct > 0.0 {
                ((context_window as f64) * ctx_pct / 100.0) as i64
            } else {
                0
            };
            let output = estimate_tokens(response_size);

            if input + output == 0 {
                continue;
            }

            let duration_ms = duration_between_ms(
                meta.request_start_timestamp_ms,
                meta.stream_end_timestamp_ms,
            );
            let timestamp = meta
                .request_start_timestamp_ms
                .or(meta.stream_end_timestamp_ms)
                .unwrap_or(0);

            let mut message = UnifiedMessage::new_with_dedup(
                CLIENT_ID,
                model_id.clone(),
                PROVIDER_ID,
                conversation_id.clone(),
                timestamp,
                TokenBreakdown {
                    input,
                    output,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0,
                Some(format!("{}:{}", conversation_id, index)),
            );
            message.message_count = 1;
            message.duration_ms = duration_ms;
            message.is_turn_start = true;
            message.set_workspace(workspace_key.clone(), workspace_label.clone());
            messages.push(message);
        }
    }

    messages
}

#[derive(Debug, Deserialize)]
struct KiroDbConversation {
    history: Option<Vec<KiroDbTurn>>,
    model_info: Option<KiroModelInfo>,
}

#[derive(Debug, Deserialize)]
struct KiroDbTurn {
    request_metadata: Option<KiroDbRequestMetadata>,
}

#[derive(Debug, Deserialize)]
struct KiroDbRequestMetadata {
    context_usage_percentage: Option<f64>,
    response_size: Option<usize>,
    request_start_timestamp_ms: Option<i64>,
    stream_end_timestamp_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_kiro_related_messages_path_uses_cli_same_stem() {
        assert_eq!(
            kiro_related_messages_path(Path::new("root/session.json")),
            Some(PathBuf::from("root/session.jsonl"))
        );
    }

    fn create_session_files(
        dir: &TempDir,
        stem: &str,
        json: &str,
        jsonl: &str,
    ) -> std::path::PathBuf {
        let json_path = dir.path().join(format!("{}.json", stem));
        let jsonl_path = dir.path().join(format!("{}.jsonl", stem));
        let mut f = std::fs::File::create(&json_path).unwrap();
        f.write_all(json.as_bytes()).unwrap();
        let mut f = std::fs::File::create(&jsonl_path).unwrap();
        f.write_all(jsonl.as_bytes()).unwrap();
        json_path
    }

    #[test]
    fn test_parse_kiro_estimates_tokens_from_jsonl_content() {
        let dir = TempDir::new().unwrap();
        let json = r#"{"session_id":"session-1","cwd":"/tmp/project","session_state":{"rts_model_state":{"model_info":{"model_id":"claude-sonnet-4-5"}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":0,"output_token_count":0,"turn_duration":123,"end_timestamp":1770983427,"total_request_count":2,"message_ids":["prompt-1","assistant-1"]}]}}}"#;
        let jsonl = r#"{"version":"v1","kind":"Prompt","data":{"message_id":"prompt-1","content":[{"kind":"text","data":"hello world"}],"meta":{"timestamp":1770983426.420942}}}
{"version":"v1","kind":"AssistantMessage","data":{"message_id":"assistant-1","content":[{"kind":"text","data":"response text"}]}}"#;
        let path = create_session_files(&dir, "session-1", json, jsonl);

        let messages = parse_kiro_file(&path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "kiro");
        assert_eq!(messages[0].provider_id, "amazon-bedrock");
        assert_eq!(messages[0].model_id, "claude-sonnet-4-5");
        assert_eq!(messages[0].session_id, "session-1");
        assert_eq!(messages[0].tokens.input, 3);
        assert_eq!(messages[0].tokens.output, 4);
        assert_eq!(messages[0].message_count, 2);
        assert!(messages[0].is_turn_start);
        assert_eq!(messages[0].timestamp, 1770983426420);
        assert_eq!(messages[0].duration_ms, Some(580));
        assert_eq!(messages[0].workspace_key, Some("/tmp/project".to_string()));
        assert_eq!(messages[0].workspace_label, Some("project".to_string()));
    }

    #[test]
    fn test_parse_kiro_skips_zero_content_turns() {
        let dir = TempDir::new().unwrap();
        let json = r#"{"session_id":"session-2","cwd":"/tmp","session_state":{"rts_model_state":{"model_info":{"model_id":"model"}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":0,"output_token_count":0,"message_ids":["missing"]}]}}}"#;
        let jsonl = "";
        let path = create_session_files(&dir, "session-2", json, jsonl);

        let messages = parse_kiro_file(&path);

        assert!(messages.is_empty());
    }

    #[test]
    fn test_parse_kiro_skips_malformed_jsonl_lines() {
        let dir = TempDir::new().unwrap();
        let json = r#"{"session_id":"session-3","cwd":"/tmp/project","session_state":{"rts_model_state":{"model_info":{"model_id":"claude-sonnet-4-5"}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":0,"output_token_count":0,"turn_duration":100,"end_timestamp":1770983427,"total_request_count":2,"message_ids":["prompt-3","assistant-3"]}]}}}"#;
        let jsonl = r#"{"version":"v1","kind":"Prompt","data":{"message_id":"prompt-3","content":[{"kind":"text","data":"hello world"}],"meta":{"timestamp":1770983426.420942}}}
not valid json at all
{"version":"v1","kind":"AssistantMessage","data":{"message_id":"assistant-3","content":[{"kind":"text","data":"response text"}]}}"#;
        let path = create_session_files(&dir, "session-3", json, jsonl);

        let messages = parse_kiro_file(&path);

        assert_eq!(messages.len(), 1);
        assert!(messages[0].tokens.input > 0 || messages[0].tokens.output > 0);
    }

    #[test]
    fn test_parse_kiro_sqlite_sets_duration_from_request_metadata() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("data.sqlite3");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE conversations_v2 (key TEXT, conversation_id TEXT, value TEXT)",
            [],
        )
        .unwrap();
        let value = r#"{
            "model_info": {
                "model_id": "claude-sonnet-4-5",
                "context_window_tokens": 1000
            },
            "history": [{
                "request_metadata": {
                    "context_usage_percentage": 10,
                    "response_size": 40,
                    "request_start_timestamp_ms": 1770983426000,
                    "stream_end_timestamp_ms": 1770983427500
                }
            }]
        }"#;
        conn.execute(
            "INSERT INTO conversations_v2 (key, conversation_id, value) VALUES (?1, ?2, ?3)",
            (&"/tmp/project", &"conv-1", &value),
        )
        .unwrap();
        drop(conn);

        let messages = parse_kiro_sqlite(&db_path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].timestamp, 1770983426000);
        assert_eq!(messages[0].duration_ms, Some(1500));
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 10);
    }

    #[test]
    fn m15a_globalstorage_snapshot_parser_emits_usage() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(
            "Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent/workspace-a/conversation.chat",
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
                "model": "claude-sonnet-4-5",
                "messages": [
                    {"role": "user", "content": "hello from Kiro"},
                    {"role": "assistant", "content": "response from Kiro"}
                ]
            }"#,
        )
        .unwrap();

        let messages = parse_kiro_file(&path);

        assert!(
            !messages.is_empty(),
            "globalStorage fixture must produce usage; old parser returned empty"
        );
    }

    fn globalstorage_path(dir: &TempDir, relative: &str) -> PathBuf {
        let path = dir
            .path()
            .join("Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent")
            .join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    fn make_test_message(
        client: &str,
        session_id: &str,
        dedup_key: &str,
        workspace: Option<&str>,
    ) -> UnifiedMessage {
        let mut message = UnifiedMessage::new_with_dedup(
            client,
            "auto".to_string(),
            PROVIDER_ID,
            session_id.to_string(),
            1_770_983_426_000,
            TokenBreakdown {
                input: 10,
                output: 2,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
            Some(dedup_key.to_string()),
        );
        message.set_workspace(workspace.map(str::to_string), workspace.map(str::to_string));
        message
    }

    #[test]
    fn test_parse_kiro_globalstorage_snapshot_roles_aliases_and_model() {
        let dir = TempDir::new().unwrap();
        let path = globalstorage_path(&dir, "workspace-a/snapshot.chat");
        fs::write(
            &path,
            r#"{
                "model": "auto",
                "completionOptions": {"modelId": "claude-sonnet-4-5"},
                "messages": [
                    {"role": "user", "content": "abcd", "text": "abcd"},
                    {"role": "assistant", "content": "1234", "text": "1234"},
                    {"role": "tool", "content": "tool context must not count"},
                    {"role": "unknown", "content": "unknown must not count"}
                ],
                "history": [{"role": "human", "content": "efghij"}]
            }"#,
        )
        .unwrap();

        let messages = parse_kiro_file(&path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-4-5");
        assert_eq!(messages[0].tokens.input, 3);
        assert_eq!(messages[0].tokens.output, 1);
        assert_eq!(messages[0].workspace_key.as_deref(), Some("workspace-a"));
        assert_eq!(messages[0].workspace_label.as_deref(), Some("workspace-a"));
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some("workspace-a/snapshot:globalstorage")
        );
    }

    #[test]
    fn test_collect_kiro_snapshot_text_value_dedup_and_distinct_subtrees() {
        let value: Value = serde_json::from_str(
            r#"{
                "prompt": {"role": "user", "text": "hello"},
                "response": {"role": "assistant", "text": "world"},
                "messages": [{"role": "user", "content": "alpha"}],
                "entries": [{"role": "user", "content": "alpha"}],
                "history": [{"role": "user", "content": "bravo"}],
                "parts": [{"role": "assistant", "text": "cd"}],
                "items": [{"role": "assistant", "text": "cd"}]
            }"#,
        )
        .unwrap();
        let mut counts = KiroSnapshotTextCounts::default();
        collect_kiro_snapshot_text(&value, &mut counts, None);

        assert_eq!(counts.prompt_chars, 5 + 5 + 5);
        assert_eq!(counts.assistant_chars, 5 + 2);
    }

    #[test]
    fn test_collect_kiro_snapshot_text_nested_tool_and_unknown_override_parent_role() {
        let value: Value = serde_json::from_str(
            r#"{
                "messages": [
                    {"role": "user", "content": {"parts": [
                        {"type": "tool", "text": "tool input must not count"},
                        {"type": "text", "text": "ABCD"}
                    ]}},
                    {"role": "assistant", "content": {"parts": [
                        {"type": "tool", "text": "tool output must not count"},
                        {"type": "unknown", "text": "unknown type must not count"},
                        {"role": "tool", "text": "tool role must not count"},
                        {"role": "unknown", "text": "unknown role must not count"},
                        {"type": "text", "text": "WXYZ"}
                    ]}}
                ]
            }"#,
        )
        .unwrap();
        let mut counts = KiroSnapshotTextCounts::default();
        collect_kiro_snapshot_text(&value, &mut counts, None);

        assert_eq!(counts.prompt_chars, 4);
        assert_eq!(counts.assistant_chars, 4);
    }

    #[test]
    fn test_find_kiro_snapshot_model_skips_pseudo_models_recursively() {
        let value: Value = serde_json::from_str(
            r#"{
                "model_id": "agent",
                "promptLogs": [{"model": "auto"}],
                "conversation": [{"completionOptions": {"modelId": "qdev"}}],
                "history": [{"model": "claude-sonnet-4-5"}]
            }"#,
        )
        .unwrap();
        assert_eq!(
            find_kiro_snapshot_model_id(&value).as_deref(),
            Some("claude-sonnet-4-5")
        );
    }

    #[test]
    fn test_parse_kiro_execution_supports_input_output_shapes_and_model_duration() {
        let dir = TempDir::new().unwrap();
        let path = globalstorage_path(&dir, "workspace-a/execution-store/execution-one");
        fs::write(
            &path,
            r#"{
                "executionId": "exec-one",
                "chatSessionId": "chat-one",
                "status": "succeed",
                "startTime": "2026-02-13T12:00:00Z",
                "endTime": 1770984001500.0,
                "completionOptions": {"model": "claude-sonnet-4-5"},
                "context": {"messages": [{"entries": [
                    {"type": "text", "text": "context text"},
                    {"type": "image", "text": "ignored"}
                ]}]},
                "input": {"data": {"messages": [
                    {"content": "string input"},
                    {"content": [
                        {"type": "text", "text": "part input"},
                        {"type": "image", "text": "ignored"}
                    ]}
                ]}},
                "actions": [
                    {"actionType": "say", "output": "answer"},
                    {"actionType": "reasoning", "output": {"message": "thinking"}},
                    {"actionType": "tool", "output": "ignored"}
                ]
            }"#,
        )
        .unwrap();

        let messages = parse_kiro_file(&path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, "chat-one");
        assert_eq!(messages[0].model_id, "claude-sonnet-4-5");
        assert_eq!(messages[0].tokens.input, 9);
        assert_eq!(messages[0].tokens.output, 4);
        assert_eq!(messages[0].timestamp, 1770984000000);
        assert_eq!(messages[0].duration_ms, Some(1500));
        assert_eq!(messages[0].dedup_key.as_deref(), Some("execution:exec-one"));
        assert_eq!(messages[0].workspace_key.as_deref(), Some("workspace-a"));
    }

    #[test]
    fn test_parse_kiro_actionless_execution_uses_context_messages() {
        let dir = TempDir::new().unwrap();
        let path = globalstorage_path(&dir, "workspace-a/execution-store/actionless");
        fs::write(
            &path,
            r#"{
                "executionId": "exec-actionless",
                "chatSessionId": "chat-actionless",
                "status": "succeed",
                "context": {"messages": [{"entries": [
                    {"type": "text", "text": "actionless input"},
                    {"type": "image", "text": "ignored"}
                ]}]}
            }"#,
        )
        .unwrap();

        let messages = parse_kiro_file(&path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, "chat-actionless");
        assert_eq!(messages[0].tokens.input, 4);
        assert_eq!(messages[0].tokens.output, 0);
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some("execution:exec-actionless")
        );
    }

    #[test]
    fn test_parse_kiro_status_bearing_chat_without_actions_stays_snapshot() {
        let dir = TempDir::new().unwrap();
        let path = globalstorage_path(&dir, "workspace-a/status.chat");
        fs::write(
            &path,
            r#"{
                "executionId": "snapshot-execution",
                "status": "succeed",
                "messages": [
                    {"role": "user", "content": "0123456789abcdef"},
                    {"role": "assistant", "content": "response"}
                ]
            }"#,
        )
        .unwrap();

        let messages = parse_kiro_file(&path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 4);
        assert_eq!(messages[0].tokens.output, 2);
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some("workspace-a/status:globalstorage:exec:snapshot-execution")
        );
    }

    #[test]
    fn test_parse_kiro_execution_success_failed_and_container_artifacts() {
        let dir = TempDir::new().unwrap();
        let success = globalstorage_path(&dir, "workspace-a/execution-store/success.json");
        fs::write(
            &success,
            r#"{"executionId":"success","status":"succeed","startTime":1770983426,"endTime":1770983427500,"actions":[{"actionType":"say","output":{"message":"answer"}}],"input":{"data":{"messages":[{"content":"question"}]}}}"#,
        )
        .unwrap();
        let failed = globalstorage_path(&dir, "workspace-a/execution-store/failed.json");
        fs::write(
            &failed,
            r#"{"executionId":"failed","status":"failed","actions":[{"actionType":"say","output":"answer"}],"input":{"data":{"messages":[{"content":"question"}]}}}"#,
        )
        .unwrap();
        let container = globalstorage_path(&dir, "workspace-a/execution-store/executions.json");
        fs::write(&container, r#"{"version":1,"executions":[]}"#).unwrap();

        let success_messages = parse_kiro_file(&success);
        assert_eq!(success_messages.len(), 1);
        assert_eq!(success_messages[0].timestamp, 1770983426000);
        assert!(parse_kiro_file(&failed).is_empty());
        assert!(parse_kiro_file(&container).is_empty());
    }

    #[test]
    fn test_parse_kiro_globalstorage_skips_non_session_json_and_extensionless_files() {
        let dir = TempDir::new().unwrap();
        let project_json = globalstorage_path(&dir, "workspace-a/project.json");
        let mirrored_file = globalstorage_path(&dir, "workspace-a/project-store/mirror");
        let body = r#"{"messages":[{"role":"user","content":"must not count"}]}"#;
        fs::write(&project_json, body).unwrap();
        fs::write(&mirrored_file, body).unwrap();

        assert!(parse_kiro_file(&project_json).is_empty());
        assert!(parse_kiro_file(&mirrored_file).is_empty());
    }

    #[test]
    fn test_parse_kiro_workspace_session_promptlogs() {
        let dir = TempDir::new().unwrap();
        let path = globalstorage_path(&dir, "workspace-sessions/workspace-a/session.json");
        fs::write(
            &path,
            r#"{
                "sessionId": "session-1",
                "selectedModel": "claude-sonnet-4",
                "history": [
                    {"promptLogs": [{"prompt": "0123456789"}]},
                    {"promptLogs": [{"prompt": "abcdefghijklmnopqrst"}], "message": {"role": "assistant", "content": "answer"}}
                ]
            }"#,
        )
        .unwrap();

        let messages = parse_kiro_file(&path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-4");
        assert_eq!(messages[0].tokens.input, 8);
        assert_eq!(messages[0].tokens.output, 2);
        assert_eq!(messages[0].message_count, 2);
        assert_eq!(messages[0].workspace_key.as_deref(), Some("workspace-a"));
        assert_eq!(messages[0].workspace_label.as_deref(), Some("workspace-a"));
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some("session-1:workspace-session")
        );
    }

    #[test]
    fn test_kiro_related_messages_path_is_none_only_for_globalstorage() {
        let dir = TempDir::new().unwrap();
        let cli = dir.path().join("session.json");
        let chat = globalstorage_path(&dir, "workspace-a/session.chat");
        let extensionless = globalstorage_path(&dir, "workspace-a/execution-store/execution");
        assert_eq!(
            kiro_related_messages_path(&cli),
            Some(dir.path().join("session.jsonl"))
        );
        assert_eq!(kiro_related_messages_path(&chat), None);
        assert_eq!(kiro_related_messages_path(&extensionless), None);
    }

    #[test]
    fn test_kiro_suppression_ignores_cli_execution_prefix_collision() {
        let dir = TempDir::new().unwrap();
        let cli = create_session_files(
            &dir,
            "cli",
            r#"{"session_id":"execution","cwd":"workspace-a","session_state":{"rts_model_state":{"model_info":{"model_id":"cli-model"}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":1}]}}}"#,
            "",
        );
        let snapshot = globalstorage_path(&dir, "workspace-a/snapshot.chat");
        fs::write(
            &snapshot,
            r#"{"executionId":"0","messages":[{"role":"user","content":"ABCD"}]}"#,
        )
        .unwrap();

        let cli_messages = parse_kiro_file(&cli);
        let snapshot_messages = parse_kiro_file(&snapshot);
        let kept =
            merge_kiro_source_messages(vec![(cli, cli_messages), (snapshot, snapshot_messages)]);
        let keys: HashSet<_> = kept
            .iter()
            .filter_map(|message| message.dedup_key.as_deref())
            .collect();

        assert_eq!(kept.len(), 2);
        assert!(keys.contains("execution:0"));
        assert!(keys.contains("workspace-a/snapshot:globalstorage:exec:0"));
    }

    #[test]
    fn test_kiro_suppression_is_workspace_scoped_and_preserves_other_lanes() {
        let dir = TempDir::new().unwrap();
        let global_messages = vec![
            make_test_message(
                CLIENT_ID,
                "ws-a/chat-a",
                "ws-a/chat-a:globalstorage:exec:exec-1",
                Some("ws-a"),
            ),
            make_test_message(
                CLIENT_ID,
                "ws-a/chat-a",
                "ws-a/chat-a:globalstorage",
                Some("ws-a"),
            ),
            make_test_message(CLIENT_ID, "chat-a", "execution:exec-1", Some("ws-a")),
            make_test_message(
                CLIENT_ID,
                "ws-b/chat-a",
                "ws-b/chat-a:globalstorage:exec:exec-1",
                Some("ws-b"),
            ),
            make_test_message(
                CLIENT_ID,
                "ws-a/other",
                "ws-a/other:globalstorage",
                Some("ws-a"),
            ),
            make_test_message(
                CLIENT_ID,
                "ws-a/failed",
                "ws-a/failed:globalstorage:exec:exec-failed",
                Some("ws-a"),
            ),
            make_test_message(
                CLIENT_ID,
                "ws-a/unmatched",
                "ws-a/unmatched:globalstorage:exec:exec-missing",
                Some("ws-a"),
            ),
            make_test_message(
                CLIENT_ID,
                "session-1",
                "session-1:workspace-session",
                Some("ws-a"),
            ),
            make_test_message(
                CLIENT_ID,
                "session-1",
                "execution:exec-session",
                Some("ws-b"),
            ),
            make_test_message(
                CLIENT_ID,
                "session-2",
                "session-2:workspace-session",
                Some("ws-a"),
            ),
        ];
        let other_messages = vec![
            make_test_message(CLIENT_ID, "chat-a", "cli-session:0", Some("ws-a")),
            make_test_message(CLIENT_ID, "chat-a", "sqlite-session:0", Some("ws-a")),
        ];

        let kept = merge_kiro_source_messages(vec![
            (globalstorage_path(&dir, "ws-a/execution"), global_messages),
            (dir.path().join("cli.json"), other_messages),
        ]);
        let keys: HashSet<_> = kept
            .iter()
            .filter_map(|message| message.dedup_key.as_deref())
            .collect();

        assert!(!keys.contains("ws-a/chat-a:globalstorage:exec:exec-1"));
        assert!(!keys.contains("ws-a/chat-a:globalstorage"));
        assert!(keys.contains("execution:exec-1"));
        assert!(keys.contains("ws-b/chat-a:globalstorage:exec:exec-1"));
        assert!(keys.contains("ws-a/other:globalstorage"));
        assert!(keys.contains("ws-a/failed:globalstorage:exec:exec-failed"));
        assert!(keys.contains("ws-a/unmatched:globalstorage:exec:exec-missing"));
        assert!(!keys.contains("session-1:workspace-session"));
        assert!(keys.contains("execution:exec-session"));
        assert!(keys.contains("session-2:workspace-session"));
        assert!(keys.contains("cli-session:0"));
        assert!(keys.contains("sqlite-session:0"));
    }
}
