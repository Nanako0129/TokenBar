//! Grok Build session parser.
//!
//! Grok Build writes JSON-RPC session updates under
//! `~/.grok/sessions/<urlencoded-workspace>/<session-id>/updates.jsonl`.
//! Session rollups also land in sibling `signals.json` (including
//! `totalTokensBeforeCompaction` and `contextTokensUsed`). Legacy update logs
//! expose cumulative `totalTokens` counters without a stable input/output split,
//! so this parser records positive deltas and reconciles `signals.json` totals.
//! Recent releases also write per-inference token buckets to the global
//! `~/.grok/logs/unified.jsonl`, which replaces legacy rows for covered sessions.

use super::utils::{
    extract_i64, extract_string, file_modified_timestamp_ms, parse_timestamp_value,
    read_file_or_none,
};
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

const CLIENT_ID: &str = "grok";
const PROVIDER_ID: &str = "xai";
const UNKNOWN_MODEL: &str = "grok-unknown";
const COMPACTION_MIN_DROP_TOKENS: i64 = 32_000;
const UNIFIED_LOG_DEDUP_PREFIX: &str = "grok-unified:";

#[derive(Debug, Clone)]
struct GrokMetadata {
    session_id: String,
    model_id: Option<String>,
    timestamp: i64,
    workspace_key: Option<String>,
    workspace_label: Option<String>,
}

#[derive(Debug, Clone)]
struct ActiveTurn {
    baseline_total: i64,
    max_total: i64,
    completed_epoch_tokens: i64,
    timestamp: i64,
    model_id: String,
    turn_index: usize,
}

impl ActiveTurn {
    fn new(baseline_total: i64, timestamp: i64, model_id: String, turn_index: usize) -> Self {
        Self {
            baseline_total,
            max_total: baseline_total,
            completed_epoch_tokens: 0,
            timestamp,
            model_id,
            turn_index,
        }
    }

    fn observe_total(&mut self, total: i64, timestamp: i64) {
        if total > self.max_total {
            self.max_total = total;
            self.timestamp = timestamp;
        }
    }

    fn start_new_counter_epoch(&mut self, total: i64, timestamp: i64) {
        self.completed_epoch_tokens = self
            .completed_epoch_tokens
            .saturating_add(self.max_total.saturating_sub(self.baseline_total));
        self.baseline_total = 0;
        self.max_total = total;
        self.timestamp = timestamp;
    }

    fn into_message(self, metadata: &GrokMetadata) -> Option<UnifiedMessage> {
        let token_delta = self
            .completed_epoch_tokens
            .saturating_add(self.max_total.saturating_sub(self.baseline_total));
        if token_delta <= 0 {
            return None;
        }

        let model_id = if self.model_id.trim().is_empty() {
            UNKNOWN_MODEL.to_string()
        } else {
            self.model_id
        };

        let mut message = UnifiedMessage::new_with_dedup(
            CLIENT_ID,
            model_id,
            PROVIDER_ID,
            metadata.session_id.clone(),
            self.timestamp,
            TokenBreakdown {
                input: token_delta,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
            Some(format!("grok:{}:{}", metadata.session_id, self.turn_index)),
        );
        message.set_workspace(
            metadata.workspace_key.clone(),
            metadata.workspace_label.clone(),
        );
        message.is_turn_start = true;
        Some(message)
    }
}

pub fn parse_grok_updates_file(path: &Path) -> Vec<UnifiedMessage> {
    if path.file_name().and_then(|name| name.to_str()) != Some("updates.jsonl") {
        return Vec::new();
    }

    let metadata = read_metadata(path);
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };

    let mut messages = Vec::new();
    let mut current_model = metadata
        .model_id
        .clone()
        .unwrap_or_else(|| UNKNOWN_MODEL.to_string());
    let mut last_total: Option<i64> = None;
    let mut last_total_timestamp = metadata.timestamp;
    let mut active_turn: Option<ActiveTurn> = None;
    let mut turn_index = 0usize;

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if let Some(model_id) = extract_model_id(&value) {
            current_model = model_id;
            if let Some(turn) = active_turn.as_mut() {
                if turn.model_id == UNKNOWN_MODEL {
                    turn.model_id = current_model.clone();
                }
            }
        }

        let timestamp = extract_timestamp_ms(&value).unwrap_or(metadata.timestamp);
        if is_user_message_chunk(&value) {
            if let Some(turn) = active_turn.take() {
                if let Some(message) = turn.into_message(&metadata) {
                    messages.push(message);
                }
            }

            active_turn = Some(ActiveTurn::new(
                last_total.unwrap_or(0),
                timestamp,
                current_model.clone(),
                turn_index,
            ));
            turn_index = turn_index.saturating_add(1);
        }

        let Some(total_tokens) = extract_total_tokens(&value) else {
            continue;
        };
        if total_tokens < 0 {
            continue;
        }

        match last_total {
            Some(previous) if total_tokens < previous => {
                if is_compaction_reset(previous, total_tokens) {
                    if active_turn.is_none() {
                        let mut turn = ActiveTurn::new(
                            0,
                            last_total_timestamp,
                            current_model.clone(),
                            turn_index,
                        );
                        turn.observe_total(previous, last_total_timestamp);
                        active_turn = Some(turn);
                        turn_index = turn_index.saturating_add(1);
                    }
                    if let Some(turn) = active_turn.as_mut() {
                        turn.start_new_counter_epoch(total_tokens, timestamp);
                    }
                    last_total_timestamp = timestamp;
                    last_total = Some(total_tokens);
                } else {
                    // Grok also emits small intermediate rewinds while streaming
                    // tool updates; those are counter jitter, not compaction.
                    continue;
                }
            }
            Some(previous) if total_tokens == previous => {
                last_total_timestamp = timestamp;
            }
            Some(previous) => {
                if active_turn.is_none() {
                    active_turn = Some(ActiveTurn::new(
                        previous,
                        timestamp,
                        current_model.clone(),
                        turn_index,
                    ));
                    turn_index = turn_index.saturating_add(1);
                }
                if let Some(turn) = active_turn.as_mut() {
                    turn.observe_total(total_tokens, timestamp);
                }
                last_total_timestamp = timestamp;
                last_total = Some(total_tokens);
            }
            None => {
                if let Some(turn) = active_turn.as_mut() {
                    turn.observe_total(total_tokens, timestamp);
                }
                last_total_timestamp = timestamp;
                last_total = Some(total_tokens);
            }
        }
    }

    if let Some(turn) = active_turn {
        if let Some(message) = turn.into_message(&metadata) {
            messages.push(message);
        }
    }

    if messages.is_empty() {
        if let Some(total_tokens) = last_total.filter(|tokens| *tokens > 0) {
            let aggregate_turn = ActiveTurn {
                baseline_total: 0,
                max_total: total_tokens,
                completed_epoch_tokens: 0,
                timestamp: last_total_timestamp,
                model_id: current_model.clone(),
                turn_index: 0,
            };
            if let Some(message) = aggregate_turn.into_message(&metadata) {
                messages.push(message);
            }
        }
    }

    append_signals_reconciliation(path, &metadata, &mut messages, &current_model);
    messages
}

/// Parses Grok Build's append-only unified log. Each `shell.turn.inference_done`
/// record reports a prompt total that includes cached prompt tokens and a
/// completion total that includes reasoning tokens. Tokscale stores the
/// non-overlapping component buckets so their sum remains the source total.
pub fn parse_grok_unified_log_file(path: &Path) -> Vec<UnifiedMessage> {
    if path.file_name().and_then(|name| name.to_str()) != Some("unified.jsonl") {
        return Vec::new();
    }

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };

    let fallback_timestamp = file_modified_timestamp_ms(path);
    let mut fallback_model_by_pid = HashMap::new();
    let mut model_by_pid_and_session = HashMap::new();
    let mut model_by_session = HashMap::new();
    let mut seen = HashSet::new();
    let mut messages = Vec::new();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if let Some(pid) = unified_log_process_start_pid(&value) {
            // The unified log survives process restarts, so an OS-reused PID
            // must not inherit model authority from the previous process.
            fallback_model_by_pid.remove(&pid);
            model_by_pid_and_session.retain(|(model_pid, _), _| *model_pid != pid);
            continue;
        }

        if let Some((pid, model_session_id, model_id)) = unified_log_model_change(&value) {
            match (pid, model_session_id) {
                (Some(pid), Some(session_id)) => {
                    model_by_pid_and_session.insert((pid, session_id), model_id);
                }
                (None, Some(session_id)) => {
                    model_by_pid_and_session
                        .retain(|(_, existing_session), _| existing_session != &session_id);
                    model_by_session.insert(session_id, model_id);
                }
                (Some(pid), None) => {
                    fallback_model_by_pid.insert(pid, model_id);
                }
                (None, None) => {}
            }
            continue;
        }

        if value.get("msg").and_then(Value::as_str) != Some("shell.turn.inference_done") {
            continue;
        }

        let Some(session_id) =
            extract_string(value.get("sid")).filter(|session_id| !session_id.trim().is_empty())
        else {
            continue;
        };
        let Some(context) = value.get("ctx") else {
            continue;
        };
        let Some(prompt_tokens) = required_non_negative_i64(context.get("prompt_tokens")) else {
            continue;
        };
        let Some(mut cached_prompt_tokens) =
            optional_non_negative_i64(context.get("cached_prompt_tokens"))
        else {
            continue;
        };
        let Some(completion_tokens) = required_non_negative_i64(context.get("completion_tokens"))
        else {
            continue;
        };
        let Some(reasoning_tokens) = optional_non_negative_i64(context.get("reasoning_tokens"))
        else {
            continue;
        };
        cached_prompt_tokens = cached_prompt_tokens.min(prompt_tokens);

        let loop_index = match context.get("loop_index") {
            Some(value) => {
                let Some(loop_index) = required_non_negative_i64(Some(value)) else {
                    continue;
                };
                loop_index
            }
            None => 1,
        };
        let Some(pid) = optional_non_negative_i64(value.get("pid")) else {
            continue;
        };
        let timestamp = value
            .get("ts")
            .and_then(parse_timestamp_value)
            .unwrap_or(fallback_timestamp);
        let reasoning = reasoning_tokens.min(completion_tokens);
        let dedup_key = format!(
            "{UNIFIED_LOG_DEDUP_PREFIX}{session_id}:{timestamp}:{pid}:{loop_index}:{prompt_tokens}:{cached_prompt_tokens}:{completion_tokens}:{reasoning_tokens}"
        );
        if !seen.insert(dedup_key.clone()) {
            continue;
        }

        let model_id = model_by_pid_and_session
            .get(&(pid, session_id.clone()))
            .or_else(|| model_by_session.get(&session_id))
            .or_else(|| fallback_model_by_pid.get(&pid))
            .cloned()
            .unwrap_or_else(|| UNKNOWN_MODEL.to_string());
        let mut message = UnifiedMessage::new_with_dedup(
            CLIENT_ID,
            model_id,
            PROVIDER_ID,
            session_id,
            timestamp,
            TokenBreakdown {
                input: prompt_tokens.saturating_sub(cached_prompt_tokens),
                output: completion_tokens.saturating_sub(reasoning),
                cache_read: cached_prompt_tokens,
                cache_write: 0,
                reasoning,
            },
            0.0,
            Some(dedup_key),
        );
        // The unified log records one inference for each tool-loop iteration.
        // In observed Grok logs, loop one starts the user turn; later loops do
        // not represent additional user interactions or messages.
        message.is_turn_start = loop_index == 1;
        message.message_count = i32::from(message.is_turn_start);
        messages.push(message);
    }

    messages
}

/// Dispatches between Grok's legacy per-session updates and its newer unified
/// log without accepting unrelated JSONL files under the Grok home directory.
pub fn parse_grok_file(path: &Path) -> Vec<UnifiedMessage> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("updates.jsonl") => parse_grok_updates_file(path),
        Some("unified.jsonl") => parse_grok_unified_log_file(path),
        _ => Vec::new(),
    }
}

/// Uses the richer, per-inference unified log for sessions it covers. Legacy
/// updates remain a fallback for sessions absent from that log, avoiding an
/// additive merge of two representations of the same activity.
pub fn prefer_unified_log_messages(mut messages: Vec<UnifiedMessage>) -> Vec<UnifiedMessage> {
    let unified_sessions: HashSet<String> = messages
        .iter()
        .filter(|message| is_unified_log_message(message))
        .map(|message| message.session_id.clone())
        .collect();

    if unified_sessions.is_empty() {
        return messages;
    }

    let mut legacy_models = HashMap::new();
    let mut legacy_workspaces = HashMap::new();
    for message in messages
        .iter()
        .filter(|message| !is_unified_log_message(message))
    {
        if message.model_id != UNKNOWN_MODEL {
            match legacy_models.entry(message.session_id.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Some(message.model_id.clone()));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if entry.get().as_ref() != Some(&message.model_id) {
                        entry.insert(None);
                    }
                }
            }
        }

        let workspace = (
            message.workspace_key.clone(),
            message.workspace_label.clone(),
        );
        if workspace == (None, None) {
            continue;
        }

        match legacy_workspaces.entry(message.session_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Some(workspace));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().as_ref() != Some(&workspace) {
                    entry.insert(None);
                }
            }
        }
    }

    for message in messages
        .iter_mut()
        .filter(|message| is_unified_log_message(message))
    {
        if message.model_id == UNKNOWN_MODEL {
            if let Some(Some(model_id)) = legacy_models.get(&message.session_id) {
                message.model_id = model_id.clone();
            }
        }
        if message.workspace_key.is_none() && message.workspace_label.is_none() {
            if let Some(Some((workspace_key, workspace_label))) =
                legacy_workspaces.get(&message.session_id)
            {
                message.set_workspace(workspace_key.clone(), workspace_label.clone());
            }
        }
    }

    messages
        .into_iter()
        .filter(|message| {
            is_unified_log_message(message) || !unified_sessions.contains(&message.session_id)
        })
        .collect()
}

fn is_unified_log_message(message: &UnifiedMessage) -> bool {
    message
        .dedup_key
        .as_deref()
        .is_some_and(|key| key.starts_with(UNIFIED_LOG_DEDUP_PREFIX))
}

fn unified_log_process_start_pid(value: &Value) -> Option<i64> {
    if value.get("msg").and_then(Value::as_str) != Some("AuthManager::new") {
        return None;
    }
    required_non_negative_i64(value.get("pid"))
}

fn unified_log_model_change(value: &Value) -> Option<(Option<i64>, Option<String>, String)> {
    let pid = match value.get("pid") {
        Some(value) => Some(required_non_negative_i64(Some(value))?),
        None => None,
    };
    let context = value.get("ctx")?;
    let model_id = match value.get("msg").and_then(Value::as_str)? {
        "model changed" => extract_string(context.get("model")),
        "model catalog: notifying clients" => extract_string(context.get("current_model_id")),
        "backend_search: model switch" => extract_string(context.get("new_model"))
            .or_else(|| extract_string(context.get("model")))
            .or_else(|| extract_string(context.get("current_model_id"))),
        "subagent model resolved" => {
            extract_string(context.get("model_id")).or_else(|| extract_string(context.get("model")))
        }
        _ => None,
    }?;

    let session_id =
        extract_string(value.get("sid")).filter(|session_id| !session_id.trim().is_empty());
    (!model_id.trim().is_empty() && (pid.is_some() || session_id.is_some()))
        .then_some((pid, session_id, model_id))
}

fn required_non_negative_i64(value: Option<&Value>) -> Option<i64> {
    extract_i64(value).filter(|value| *value >= 0)
}

fn optional_non_negative_i64(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(value) => required_non_negative_i64(Some(value)),
        None => Some(0),
    }
}

fn is_compaction_reset(previous: i64, current: i64) -> bool {
    previous.saturating_sub(current) >= COMPACTION_MIN_DROP_TOKENS
        && current.saturating_mul(2) <= previous
}

fn non_negative_i64(value: Option<&Value>) -> i64 {
    extract_i64(value).unwrap_or(0).max(0)
}

fn effective_total_from_signals(value: &Value) -> i64 {
    let before = non_negative_i64(value.get("totalTokensBeforeCompaction"));
    let total = non_negative_i64(value.get("totalTokens"));
    match value.get("contextTokensUsed") {
        None => before.saturating_add(total),
        Some(ctx) => total.max(before.saturating_add(non_negative_i64(Some(ctx)))),
    }
}

fn model_id_from_signals(value: &Value) -> Option<String> {
    extract_string(value.get("primaryModelId")).or_else(|| {
        value
            .get("modelsUsed")
            .and_then(|models| models.as_array())
            .and_then(|models| models.first())
            .and_then(|model| extract_string(Some(model)))
    })
}

fn append_signals_reconciliation(
    updates_path: &Path,
    metadata: &GrokMetadata,
    messages: &mut Vec<UnifiedMessage>,
    fallback_model: &str,
) {
    let signals_path = match sibling(updates_path, "signals.json") {
        Some(path) => path,
        None => return,
    };
    let data = match read_file_or_none(&signals_path) {
        Some(data) => data,
        None => return,
    };
    let value: Value = match serde_json::from_slice(&data) {
        Ok(value) => value,
        Err(_) => return,
    };

    let signals_total = effective_total_from_signals(&value);
    if signals_total <= 0 {
        return;
    }

    let updates_total: i64 = messages.iter().map(|message| message.tokens.input).sum();
    let extra = signals_total.saturating_sub(updates_total);
    if extra <= 0 {
        return;
    }

    let model_id = model_id_from_signals(&value)
        .filter(|model| !model.trim().is_empty())
        .or_else(|| metadata.model_id.clone())
        .unwrap_or_else(|| fallback_model.to_string());
    // Anchor the reconciliation delta to the last recorded update activity rather
    // than signals.json's mtime. The mtime advances every time Grok rewrites the
    // rollup for a live session, which would migrate this whole (potentially
    // multi-million-token) extra to a new day on each rescan and retroactively
    // shrink the prior day's total. The last update timestamp only moves when
    // genuine new activity is recorded, so the delta stays put across rescans.
    let timestamp = messages
        .iter()
        .map(|message| message.timestamp)
        .max()
        .unwrap_or(metadata.timestamp);

    let mut message = UnifiedMessage::new_with_dedup(
        CLIENT_ID,
        model_id,
        PROVIDER_ID,
        metadata.session_id.clone(),
        timestamp,
        TokenBreakdown {
            input: extra,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        },
        0.0,
        Some(format!("grok:{}:signals", metadata.session_id)),
    );
    message.message_count = 0;
    message.set_workspace(
        metadata.workspace_key.clone(),
        metadata.workspace_label.clone(),
    );
    messages.push(message);
}

fn read_metadata(path: &Path) -> GrokMetadata {
    let session_dir = path.parent();
    let session_id = session_dir
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .filter(|id| !id.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();

    let workspace_key = session_dir
        .and_then(|dir| dir.parent())
        .and_then(|workspace_dir| workspace_dir.file_name())
        .and_then(|name| name.to_str())
        .map(percent_decode_lossy)
        .and_then(|decoded| normalize_workspace_key(&decoded));
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);

    let fallback_timestamp = file_modified_timestamp_ms(path);
    let mut metadata = GrokMetadata {
        session_id,
        model_id: None,
        timestamp: fallback_timestamp,
        workspace_key,
        workspace_label,
    };

    if let Some(summary_path) = sibling(path, "summary.json") {
        read_summary_metadata(&summary_path, &mut metadata);
    }
    if let Some(events_path) = sibling(path, "events.jsonl") {
        read_events_metadata(&events_path, &mut metadata);
    }
    if let Some(signals_path) = sibling(path, "signals.json") {
        read_signals_metadata(&signals_path, &mut metadata);
    }

    metadata
}

fn read_signals_metadata(path: &Path, metadata: &mut GrokMetadata) {
    let Some(data) = read_file_or_none(path) else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&data) else {
        return;
    };

    if metadata.model_id.is_none() {
        metadata.model_id = model_id_from_signals(&value);
    }
}

fn read_summary_metadata(path: &Path, metadata: &mut GrokMetadata) {
    let Some(data) = read_file_or_none(path) else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&data) else {
        return;
    };

    if metadata.model_id.is_none() {
        metadata.model_id = extract_string(value.get("current_model_id"))
            .or_else(|| extract_string(value.get("model_id")));
    }

    if let Some(timestamp) = value
        .get("updated_at")
        .or_else(|| value.get("created_at"))
        .and_then(parse_timestamp_value)
    {
        metadata.timestamp = timestamp;
    }
}

fn read_events_metadata(path: &Path, metadata: &mut GrokMetadata) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };

    for line in BufReader::new(file).lines().map_while(Result::ok).take(500) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if metadata.model_id.is_none() {
            metadata.model_id = extract_string(value.get("model_id"));
        }
        if metadata.session_id == "unknown" {
            if let Some(session_id) = extract_string(value.get("session_id")) {
                metadata.session_id = session_id;
            }
        }
        if let Some(timestamp) = value.get("ts").and_then(parse_timestamp_value) {
            metadata.timestamp = timestamp;
        }

        if metadata.model_id.is_some() && metadata.session_id != "unknown" {
            break;
        }
    }
}

fn sibling(path: &Path, file_name: &str) -> Option<PathBuf> {
    Some(path.parent()?.join(file_name))
}

fn extract_model_id(value: &Value) -> Option<String> {
    for path in [
        &["params", "update", "_meta", "modelId"][..],
        &["params", "_meta", "modelId"][..],
        &["params", "modelId"][..],
        &["model_id"][..],
        &["modelId"][..],
        &["model"][..],
    ] {
        if let Some(model_id) = get_path(value, path).and_then(|value| extract_string(Some(value)))
        {
            if !model_id.trim().is_empty() {
                return Some(model_id);
            }
        }
    }
    None
}

fn extract_total_tokens(value: &Value) -> Option<i64> {
    for path in [
        &["params", "_meta", "totalTokens"][..],
        &["params", "update", "_meta", "totalTokens"][..],
        &["params", "update", "totalTokens"][..],
        &["params", "totalTokens"][..],
        &["usage", "totalTokens"][..],
        &["totalTokens"][..],
    ] {
        if let Some(total) = get_path(value, path).and_then(|value| extract_i64(Some(value))) {
            return Some(total);
        }
    }
    None
}

fn extract_timestamp_ms(value: &Value) -> Option<i64> {
    for path in [
        &["params", "_meta", "agentTimestampMs"][..],
        &["params", "update", "_meta", "agentTimestampMs"][..],
        &["params", "timestamp"][..],
        &["timestamp"][..],
        &["ts"][..],
    ] {
        if let Some(timestamp) = get_path(value, path).and_then(parse_timestamp_value) {
            return Some(timestamp);
        }
    }
    None
}

fn is_user_message_chunk(value: &Value) -> bool {
    get_path(value, &["params", "update", "sessionUpdate"]).and_then(|value| value.as_str())
        == Some("user_message_chunk")
}

fn get_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
}

fn percent_decode_lossy(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                decoded.push((high << 4) | low);
                i += 3;
                continue;
            }
        }

        decoded.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(
        updates_jsonl: &str,
        summary_json: Option<&str>,
        signals_json: Option<&str>,
    ) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let session_dir = temp
            .path()
            .join(".grok")
            .join("sessions")
            .join("%2Ftmp%2Fproject")
            .join("session-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        let updates_path = session_dir.join("updates.jsonl");
        std::fs::write(&updates_path, updates_jsonl).unwrap();
        if let Some(summary_json) = summary_json {
            std::fs::write(session_dir.join("summary.json"), summary_json).unwrap();
        }
        if let Some(signals_json) = signals_json {
            std::fs::write(session_dir.join("signals.json"), signals_json).unwrap();
        }
        (temp, updates_path)
    }

    fn write_unified_fixture(unified_jsonl: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let logs_dir = temp.path().join(".grok/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let path = logs_dir.join("unified.jsonl");
        std::fs::write(&path, unified_jsonl).unwrap();
        (temp, path)
    }

    fn test_message(session_id: &str, dedup_key: &str) -> UnifiedMessage {
        UnifiedMessage::new_with_dedup(
            CLIENT_ID,
            "grok-build",
            PROVIDER_ID,
            session_id,
            1_700_000_000_000,
            TokenBreakdown::default(),
            0.0,
            Some(dedup_key.to_string()),
        )
    }

    #[test]
    fn parses_unified_log_token_breakdown_without_double_counting_reasoning() {
        let (_temp, path) = write_unified_fixture(
            r#"{"ts":"2023-11-14T22:13:19Z","pid":17,"sid":"session-1","msg":"model changed","ctx":{"model":"grok-composer-2.5-fast"}}
{"ts":"2023-11-14T22:13:19Z","pid":17,"msg":"model catalog: notifying clients","ctx":{"current_model_id":"grok-4.5"}}
{"ts":"2023-11-14T22:13:20Z","pid":17,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":100,"cached_prompt_tokens":60,"completion_tokens":25,"reasoning_tokens":5}}
{"ts":"2023-11-14T22:13:21Z","pid":17,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"loop_index":2,"prompt_tokens":80,"cached_prompt_tokens":0,"completion_tokens":12,"reasoning_tokens":0}}
{"ts":"2023-11-14T22:13:20Z","pid":17,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":100,"cached_prompt_tokens":60,"completion_tokens":25,"reasoning_tokens":5}}
{"ts":"2023-11-14T22:13:22Z","pid":17,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"loop_index":3,"prompt_tokens":10,"cached_prompt_tokens":11,"completion_tokens":1,"reasoning_tokens":0}}"#,
        );

        let messages = parse_grok_unified_log_file(&path);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].client, CLIENT_ID);
        assert_eq!(messages[0].model_id, "grok-composer-2.5-fast");
        assert_eq!(messages[0].session_id, "session-1");
        assert_eq!(messages[0].tokens.input, 40);
        assert_eq!(messages[0].tokens.cache_read, 60);
        assert_eq!(messages[0].tokens.output, 20);
        assert_eq!(messages[0].tokens.reasoning, 5);
        assert_eq!(messages[0].tokens.total(), 125);
        assert_eq!(messages[0].message_count, 1);
        assert!(messages[0].is_turn_start);
        assert_eq!(messages[1].tokens.input, 80);
        assert_eq!(messages[1].tokens.output, 12);
        assert_eq!(messages[1].message_count, 0);
        assert!(!messages[1].is_turn_start);
        assert_eq!(messages[2].tokens.input, 0);
        assert_eq!(messages[2].tokens.cache_read, 10);
        assert_eq!(messages[2].tokens.output, 1);
        assert_eq!(messages[2].tokens.total(), 11);
        assert_eq!(messages[2].message_count, 0);
        assert!(!messages[2].is_turn_start);
    }

    #[test]
    fn unified_log_counts_missing_loop_index_as_first_loop() {
        let (_temp, path) = write_unified_fixture(
            r#"{"ts":"2023-11-14T22:13:20Z","pid":17,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"prompt_tokens":100,"completion_tokens":25}}
{"ts":"2023-11-14T22:13:21Z","pid":17,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"loop_index":2,"prompt_tokens":80,"completion_tokens":12}}"#,
        );

        let messages = parse_grok_unified_log_file(&path);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message_count, 1);
        assert!(messages[0].is_turn_start);
        assert_eq!(messages[1].message_count, 0);
        assert!(!messages[1].is_turn_start);
    }

    #[test]
    fn unified_log_keeps_distinct_inferences_that_share_base_identity() {
        let (_temp, path) = write_unified_fixture(
            r#"{"ts":"2023-11-14T22:13:20Z","pid":17,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":100,"cached_prompt_tokens":60,"completion_tokens":25,"reasoning_tokens":5}}
{"ts":"2023-11-14T22:13:20Z","pid":17,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":120,"cached_prompt_tokens":70,"completion_tokens":30,"reasoning_tokens":6}}
{"ts":"2023-11-14T22:13:20Z","pid":17,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":100,"cached_prompt_tokens":60,"completion_tokens":25,"reasoning_tokens":5}}"#,
        );

        let messages = parse_grok_unified_log_file(&path);

        assert_eq!(messages.len(), 2);
        assert_ne!(messages[0].dedup_key, messages[1].dedup_key);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.tokens.total())
                .sum::<i64>(),
            275
        );
    }

    #[test]
    fn unified_log_applies_pidless_session_model_switch() {
        let (_temp, path) = write_unified_fixture(
            r#"{"ts":"2023-11-14T22:13:18Z","pid":17,"msg":"model catalog: notifying clients","ctx":{"current_model_id":"grok-4.5"}}
{"ts":"2023-11-14T22:13:19Z","pid":17,"sid":"session-with-model-event","msg":"model changed","ctx":{"model":"grok-composer-2.5-fast"}}
{"ts":"2023-11-14T22:13:20Z","pid":17,"sid":"session-with-model-event","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":10,"completion_tokens":1}}
{"ts":"2023-11-14T22:13:21Z","sid":"session-with-model-event","msg":"model changed","ctx":{"model":"grok-4.1-fast"}}
{"ts":"2023-11-14T22:13:22Z","pid":17,"sid":"session-with-model-event","msg":"shell.turn.inference_done","ctx":{"loop_index":2,"prompt_tokens":15,"completion_tokens":2}}
{"ts":"2023-11-14T22:13:23Z","pid":17,"sid":"session-without-model-event","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":20,"completion_tokens":2}}"#,
        );

        let messages = parse_grok_unified_log_file(&path);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].model_id, "grok-composer-2.5-fast");
        assert_eq!(messages[1].model_id, "grok-4.1-fast");
        assert_eq!(messages[2].model_id, "grok-4.5");
    }

    #[test]
    fn unified_log_expires_pid_scoped_models_on_process_restart() {
        let (_temp, path) = write_unified_fixture(
            r#"{"ts":"2023-11-14T22:13:17Z","sid":"session-stable","msg":"model changed","ctx":{"model":"grok-session"}}
{"ts":"2023-11-14T22:13:18Z","pid":17,"msg":"model catalog: notifying clients","ctx":{"current_model_id":"grok-old"}}
{"ts":"2023-11-14T22:13:19Z","pid":17,"sid":"session-old","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":10,"completion_tokens":1}}
{"ts":"2023-11-14T22:13:20Z","pid":17,"msg":"AuthManager::new","src":"shell","ctx":{}}
{"ts":"2023-11-14T22:13:21Z","pid":17,"sid":"session-stable","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":15,"completion_tokens":1}}
{"ts":"2023-11-14T22:13:22Z","pid":17,"sid":"session-new","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":20,"completion_tokens":2}}
{"ts":"2023-11-14T22:13:23Z","pid":17,"msg":"model catalog: notifying clients","ctx":{"current_model_id":"grok-new"}}
{"ts":"2023-11-14T22:13:24Z","pid":17,"sid":"session-new","msg":"shell.turn.inference_done","ctx":{"loop_index":2,"prompt_tokens":30,"completion_tokens":3}}"#,
        );

        let messages = parse_grok_unified_log_file(&path);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].model_id, "grok-old");
        assert_eq!(messages[1].model_id, "grok-session");
        assert_eq!(messages[2].model_id, UNKNOWN_MODEL);
        assert_eq!(messages[3].model_id, "grok-new");
    }

    #[test]
    fn selector_suppresses_covered_legacy_without_dropping_partial_fallback() {
        let mut covered_legacy = test_message("covered", "grok:covered:0");
        covered_legacy.tokens = TokenBreakdown {
            input: 900,
            output: 80,
            cache_read: 70,
            cache_write: 60,
            reasoning: 50,
        };
        covered_legacy.message_count = 7;
        covered_legacy.set_workspace(
            Some("/tmp/project".to_string()),
            Some("project".to_string()),
        );

        let mut legacy_only = test_message("legacy-only", "grok:legacy-only:0");
        legacy_only.tokens.input = 17;
        legacy_only.message_count = 3;

        let mut covered_unified = test_message("covered", "grok-unified:covered:1:17:1");
        covered_unified.model_id = UNKNOWN_MODEL.to_string();
        covered_unified.tokens = TokenBreakdown {
            input: 40,
            output: 20,
            cache_read: 60,
            cache_write: 0,
            reasoning: 5,
        };
        covered_unified.message_count = 1;

        let raw = vec![covered_legacy, legacy_only, covered_unified];
        let selected = prefer_unified_log_messages(raw.clone());

        assert_eq!(selected.len(), 2);
        let covered = selected
            .iter()
            .find(|message| message.session_id == "covered" && is_unified_log_message(message))
            .unwrap();
        assert_eq!(covered.model_id, "grok-build");
        assert_eq!(covered.workspace_key.as_deref(), Some("/tmp/project"));
        assert_eq!(covered.workspace_label.as_deref(), Some("project"));
        assert!(selected
            .iter()
            .any(|message| message.session_id == "legacy-only"));
        let token_buckets =
            selected
                .iter()
                .fold(TokenBreakdown::default(), |mut total, message| {
                    total.input += message.tokens.input;
                    total.output += message.tokens.output;
                    total.cache_read += message.tokens.cache_read;
                    total.cache_write += message.tokens.cache_write;
                    total.reasoning += message.tokens.reasoning;
                    total
                });
        assert_eq!(
            token_buckets,
            TokenBreakdown {
                input: 57,
                output: 20,
                cache_read: 60,
                cache_write: 0,
                reasoning: 5,
            }
        );
        assert_eq!(token_buckets.total(), 142);
        assert_eq!(
            selected
                .iter()
                .map(|message| message.message_count)
                .sum::<i32>(),
            4
        );
        assert_ne!(
            raw.iter()
                .map(|message| message.tokens.total())
                .sum::<i64>(),
            142,
            "additive legacy + unified handling would double-count the covered session"
        );
    }

    #[test]
    fn selector_result_set_is_input_order_independent() {
        let mut legacy = test_message("covered", "grok:covered:0");
        legacy.tokens.input = 999;
        legacy.message_count = 9;
        let mut unified = test_message("covered", "grok-unified:covered:1:17:1");
        unified.tokens.cache_read = 12;
        unified.tokens.reasoning = 3;
        let fallback = test_message("legacy-only", "grok:legacy-only:0");

        let forward =
            prefer_unified_log_messages(vec![legacy.clone(), unified.clone(), fallback.clone()]);
        let reverse = prefer_unified_log_messages(vec![fallback, unified, legacy]);

        let signature = |messages: Vec<UnifiedMessage>| {
            let mut signature: Vec<_> = messages
                .into_iter()
                .map(|message| {
                    (
                        message.dedup_key.unwrap(),
                        message.tokens,
                        message.message_count,
                    )
                })
                .collect();
            signature.sort_by(|left, right| left.0.cmp(&right.0));
            signature
        };

        assert_eq!(signature(forward), signature(reverse));
    }

    #[test]
    fn selector_keeps_unknown_model_when_legacy_models_conflict() {
        let mut legacy_a = test_message("covered", "grok:covered:0");
        legacy_a.model_id = "grok-model-a".to_string();
        let mut legacy_b = test_message("covered", "grok:covered:1");
        legacy_b.model_id = "grok-model-b".to_string();
        let mut unified = test_message("covered", "grok-unified:covered:1:17:1");
        unified.model_id = UNKNOWN_MODEL.to_string();

        for raw in [
            vec![legacy_a.clone(), legacy_b.clone(), unified.clone()],
            vec![legacy_b.clone(), unified.clone(), legacy_a.clone()],
        ] {
            let selected = prefer_unified_log_messages(raw);
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].model_id, UNKNOWN_MODEL);
        }
    }

    #[test]
    fn parses_grok_total_token_deltas_by_turn() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"available_commands_update"},"_meta":{"totalTokens":100,"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk","_meta":{"modelId":"grok-composer-2.5-fast"}},"_meta":{"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_thought_chunk"},"_meta":{"totalTokens":250,"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":300,"agentTimestampMs":1700000003000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk","_meta":{"modelId":"grok-composer-2.5-fast"}},"_meta":{"agentTimestampMs":1700000004000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":450,"agentTimestampMs":1700000005000}}}"#,
            Some(
                r#"{"current_model_id":"grok-composer-2.5-fast","updated_at":"2023-11-14T22:13:20Z"}"#,
            ),
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].client, "grok");
        assert_eq!(messages[0].model_id, "grok-composer-2.5-fast");
        assert_eq!(messages[0].provider_id, "xai");
        assert_eq!(messages[0].session_id, "session-1");
        assert_eq!(messages[0].tokens.input, 200);
        assert_eq!(messages[0].tokens.output, 0);
        assert_eq!(messages[0].timestamp, 1700000003000);
        assert_eq!(messages[0].workspace_key.as_deref(), Some("/tmp/project"));
        assert_eq!(messages[0].workspace_label.as_deref(), Some("project"));
        assert_eq!(messages[1].tokens.input, 150);
        assert_eq!(messages[1].timestamp, 1700000005000);
    }

    #[test]
    fn uses_summary_model_when_update_model_is_missing() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":220,"agentTimestampMs":1700000001000}}}"#,
            Some(
                r#"{"current_model_id":"grok-composer-2.5-fast","updated_at":"2023-11-14T22:13:20Z"}"#,
            ),
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "grok-composer-2.5-fast");
        assert_eq!(messages[0].tokens.input, 220);
    }

    #[test]
    fn ignores_repeated_and_decreasing_total_tokens() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"available_commands_update"},"_meta":{"totalTokens":100,"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk","_meta":{"modelId":"grok-composer-2.5-fast"}},"_meta":{"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":150,"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":150,"agentTimestampMs":1700000003000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":120,"agentTimestampMs":1700000004000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":200,"agentTimestampMs":1700000005000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].timestamp, 1700000005000);
    }

    #[test]
    fn counts_compaction_reset_as_a_new_counter_epoch() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk","_meta":{"modelId":"grok-build"}},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_thought_chunk"},"_meta":{"totalTokens":180000,"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_thought_chunk"},"_meta":{"totalTokens":40000,"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":50000,"agentTimestampMs":1700000003000}}}"#,
            None,
            Some(
                r#"{"primaryModelId":"grok-build","totalTokensBeforeCompaction":180000,"contextTokensUsed":50000}"#,
            ),
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 230000);
        assert_eq!(messages[0].timestamp, 1700000003000);
        assert_eq!(messages[0].message_count, 1);
    }

    #[test]
    fn compaction_epoch_survives_without_signals_reconciliation() {
        // Signals-absent compaction: this is the case the local counter-epoch
        // delta exists for. Upstream treats every counter rewind as jitter and
        // `continue`s, so without signals.json to backfill the lost total it
        // reports only the pre-compaction peak (180000). The epoch accumulation
        // must survive on its own: first epoch 180000 + second epoch 500000.
        let updates = r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk","_meta":{"modelId":"grok-build"}},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_thought_chunk"},"_meta":{"totalTokens":180000,"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_thought_chunk"},"_meta":{"totalTokens":40000,"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":500000,"agentTimestampMs":1700000003000}}}"#;

        let (_temp, path) = write_fixture(updates, None, None);
        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 680000);
        assert_eq!(messages[0].timestamp, 1700000003000);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            680000
        );

        // Idempotence with signals present: when signals.json's totals match the
        // epochs the parser already accumulated (before-compaction 180000 +
        // context-used 500000 = 680000), the difference-based reconciliation
        // (`extra = signals_total - updates_total`) is <= 0 and contributes
        // nothing — the two mechanisms are complementary, not additive.
        let (_temp2, path2) = write_fixture(
            updates,
            None,
            Some(
                r#"{"primaryModelId":"grok-build","totalTokensBeforeCompaction":180000,"contextTokensUsed":500000}"#,
            ),
        );
        let reconciled = parse_grok_updates_file(&path2);
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].tokens.input, 680000);
        assert_eq!(
            reconciled
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            680000
        );
    }

    #[test]
    fn preserves_total_tokens_without_model_metadata() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"available_commands_update"},"_meta":{"totalTokens":120,"agentTimestampMs":1700000000000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, UNKNOWN_MODEL);
        assert_eq!(messages[0].tokens.input, 120);
        assert_eq!(messages[0].timestamp, 1700000000000);
    }

    #[test]
    fn creates_unknown_model_turn_without_model_metadata() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"available_commands_update"},"_meta":{"totalTokens":100,"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":250,"agentTimestampMs":1700000002000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, UNKNOWN_MODEL);
        assert_eq!(messages[0].tokens.input, 150);
        assert_eq!(messages[0].timestamp, 1700000002000);
    }

    #[test]
    fn adds_signals_reconciliation_when_compaction_exceeds_updates() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk","_meta":{"modelId":"grok-build"}},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":171056,"agentTimestampMs":1700000001000}}}"#,
            None,
            Some(
                r#"{"primaryModelId":"grok-build","totalTokensBeforeCompaction":3224659,"contextTokensUsed":172309}"#,
            ),
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 171056);
        assert_eq!(messages[1].tokens.input, 3225912);
        assert_eq!(messages[1].model_id, "grok-build");
        assert_eq!(messages[1].message_count, 0);
        assert_eq!(
            messages[1].dedup_key.as_deref(),
            Some("grok:session-1:signals")
        );
        assert_eq!(
            messages
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            3396968
        );
    }

    #[test]
    fn signals_reconciliation_anchors_timestamp_to_last_update_not_file_mtime() {
        // The signals.json is written "now" (mtime far in the future relative to
        // the update timestamps). The reconciliation delta must be dated by the
        // last recorded update (1700000001000), NOT the signals.json mtime, so a
        // live session's extra does not migrate to a new day on every rescan.
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk","_meta":{"modelId":"grok-build"}},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":171056,"agentTimestampMs":1700000001000}}}"#,
            None,
            Some(
                r#"{"primaryModelId":"grok-build","totalTokensBeforeCompaction":3224659,"contextTokensUsed":172309}"#,
            ),
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[1].dedup_key.as_deref(),
            Some("grok:session-1:signals")
        );
        assert_eq!(messages[1].timestamp, 1700000001000);
    }

    #[test]
    fn skips_signals_reconciliation_when_updates_already_cover_signals() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":500,"agentTimestampMs":1700000001000}}}"#,
            None,
            Some(r#"{"primaryModelId":"grok-build","contextTokensUsed":400}"#),
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 500);
    }

    #[test]
    fn uses_signals_model_when_updates_model_is_missing() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"available_commands_update"},"_meta":{"totalTokens":50,"agentTimestampMs":1700000000000}}}"#,
            None,
            Some(r#"{"primaryModelId":"grok-composer-2.5-fast","contextTokensUsed":250}"#),
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 50);
        assert_eq!(messages[1].tokens.input, 200);
        assert_eq!(messages[1].model_id, "grok-composer-2.5-fast");
    }
}
