//! Grok Build session parser.
//!
//! Grok Build writes JSON-RPC session updates under
//! `~/.grok/sessions/<urlencoded-workspace>/<session-id>/updates.jsonl`.
//!
//! **Primary path (modern logs):** each `sessionUpdate: turn_completed` carries
//! authoritative per-turn `params.update.usage` (`inputTokens`, `outputTokens`,
//! `reasoningTokens`, `cachedReadTokens`, optional `modelUsage`,
//! `costUsdTicks`). Those are the real API totals; multi-call turns re-bill
//! context, so they dwarf the context-window counter.
//!
//! **Legacy / live path:** older logs (and the open turn before
//! `turn_completed`) only expose cumulative `params._meta.totalTokens`, which
//! tracks **context occupancy**, not cumulative spend. Positive deltas of that
//! counter are recorded as input tokens; local compaction epochs bank large
//! rewinds; sibling `signals.json` can reconcile remaining context undercount
//! when the usage path is unavailable.
//!
//! **Unified log:** recent releases also write per-inference token buckets to
//! the global `~/.grok/logs/unified.jsonl`, which replaces legacy/update rows
//! for covered sessions via `prefer_unified_log_messages`.


use super::utils::{
    extract_i64, extract_string, file_modified_timestamp_ms, parse_timestamp_value,
    read_file_or_none,
};
use super::{normalize_workspace_key, workspace_label_from_key, CostSource, UnifiedMessage};
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
/// xAI stamps `costUsdTicks` as nano-USD (`1e9` ticks = $1). Observed magnitudes
/// match that scale on real sessions; if the unit is ever proven different, only
/// cost mapping changes.
const COST_USD_TICKS_PER_DOLLAR: f64 = 1_000_000_000.0;

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

#[derive(Debug, Clone)]
struct ParsedUsage {
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    cached_read_tokens: i64,
    cost_usd_ticks: i64,
    api_duration_ms: Option<i64>,
    model_id: Option<String>,
}

impl ParsedUsage {
    fn has_positive_tokens(&self) -> bool {
        self.input_tokens > 0
            || self.output_tokens > 0
            || self.reasoning_tokens > 0
            || self.cached_read_tokens > 0
    }

    /// Grok's `inputTokens` includes cache reads. TokenBar totals sum every
    /// bucket, so net uncached input into `input` and put cache in `cache_read`.
    fn token_breakdown(&self) -> TokenBreakdown {
        let cache_read = self.cached_read_tokens.max(0);
        let input = self.input_tokens.max(0).saturating_sub(cache_read);
        TokenBreakdown {
            input,
            output: self.output_tokens.max(0),
            cache_read,
            cache_write: 0,
            reasoning: self.reasoning_tokens.max(0),
        }
    }

    fn cost_usd(&self) -> Option<f64> {
        if self.cost_usd_ticks <= 0 {
            return None;
        }
        Some(self.cost_usd_ticks as f64 / COST_USD_TICKS_PER_DOLLAR)
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

    let mut usage_messages = Vec::new();
    let mut context_messages = Vec::new();
    let mut current_model = metadata
        .model_id
        .clone()
        .unwrap_or_else(|| UNKNOWN_MODEL.to_string());
    let mut last_total: Option<i64> = None;
    let mut last_total_timestamp = metadata.timestamp;
    let mut active_turn: Option<ActiveTurn> = None;
    let mut turn_index = 0usize;
    let mut usage_turn_index = 0usize;
    let mut saw_usage = false;
    // Context baseline after the latest completed usage turn so a live open turn
    // only counts post-completion context growth (no double-count).
    let mut context_baseline_after_usage: Option<i64> = None;
    // After saw_usage, a user_message_chunk with no known context counter must
    // not open ActiveTurn at 0 (full occupancy would double-count completed
    // usage). Defer until the first post-usage totalTokens establishes baseline.
    let mut pending_post_usage_live_partial = false;

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

        // Primary path: authoritative per-turn API usage.
        if is_turn_completed(&value) {
            if let Some(usage_value) = get_path(&value, &["params", "update", "usage"]) {
                let prompt_id = get_path(&value, &["params", "update", "prompt_id"])
                    .and_then(|v| extract_string(Some(v)));
                let emitted = emit_usage_messages(
                    usage_value,
                    &metadata,
                    &current_model,
                    timestamp,
                    usage_turn_index,
                    prompt_id.as_deref(),
                );
                if !emitted.is_empty() {
                    saw_usage = true;
                    usage_turn_index = usage_turn_index.saturating_add(1);
                    usage_messages.extend(emitted);
                    // Close any open context turn without emitting — usage owns
                    // the completed turn's totals.
                    active_turn = None;
                    pending_post_usage_live_partial = false;
                    // If this line also carries a context counter, advance the
                    // baseline from it but do not open a new ActiveTurn from
                    // the completed turn's context growth (would double-count).
                    if let Some(total) = extract_total_tokens(&value).filter(|t| *t >= 0) {
                        context_baseline_after_usage = Some(total);
                        last_total = Some(total);
                        last_total_timestamp = timestamp;
                    } else if let Some(total) = last_total {
                        context_baseline_after_usage = Some(total);
                    }
                    continue;
                }
            }
        }

        if is_user_message_chunk(&value) {
            if !saw_usage {
                if let Some(turn) = active_turn.take() {
                    if let Some(message) = turn.into_message(&metadata) {
                        context_messages.push(message);
                    }
                }
                pending_post_usage_live_partial = false;
                let baseline = last_total.unwrap_or(0);
                active_turn = Some(ActiveTurn::new(
                    baseline,
                    timestamp,
                    current_model.clone(),
                    turn_index,
                ));
                turn_index = turn_index.saturating_add(1);
            } else {
                // Usage path: discard any open context turn (completed turns are
                // owned by usage). Never zero-base a post-usage live partial —
                // without a known context counter, wait for the first post-usage
                // totalTokens instead of treating full occupancy as delta.
                if let Some(baseline) = context_baseline_after_usage.or(last_total) {
                    pending_post_usage_live_partial = false;
                    active_turn = Some(ActiveTurn::new(
                        baseline,
                        timestamp,
                        current_model.clone(),
                        turn_index,
                    ));
                    turn_index = turn_index.saturating_add(1);
                } else {
                    pending_post_usage_live_partial = true;
                    active_turn = None;
                }
            }
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
                        // After usage, only user_message_chunk may open a live
                        // partial. Compaction without an open user turn only
                        // advances the counter baseline.
                        if saw_usage {
                            last_total_timestamp = timestamp;
                            last_total = Some(total_tokens);
                            context_baseline_after_usage = Some(total_tokens);
                            // First known counter after deferred user turn:
                            // open at the post-compaction total (baseline only).
                            if pending_post_usage_live_partial {
                                pending_post_usage_live_partial = false;
                                active_turn = Some(ActiveTurn::new(
                                    total_tokens,
                                    timestamp,
                                    current_model.clone(),
                                    turn_index,
                                ));
                                turn_index = turn_index.saturating_add(1);
                            }
                            continue;
                        }
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
                    // After saw_usage / completed usage, do not open a new
                    // ActiveTurn from non-user updates that only bump
                    // `_meta.totalTokens`. Only user_message_chunk may start a
                    // post-usage live partial; context-only growth advances the
                    // baseline so the next user turn does not inherit it.
                    if saw_usage {
                        last_total_timestamp = timestamp;
                        last_total = Some(total_tokens);
                        context_baseline_after_usage = Some(total_tokens);
                        if pending_post_usage_live_partial {
                            // Deferred user turn: first/next counter is baseline
                            // only (no zero-base full-occupancy emission).
                            pending_post_usage_live_partial = false;
                            active_turn = Some(ActiveTurn::new(
                                total_tokens,
                                timestamp,
                                current_model.clone(),
                                turn_index,
                            ));
                            turn_index = turn_index.saturating_add(1);
                        }
                        continue;
                    }
                    let baseline = previous;
                    active_turn = Some(ActiveTurn::new(
                        baseline,
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
                // First context counter observed in this file.
                last_total_timestamp = timestamp;
                last_total = Some(total_tokens);
                if saw_usage {
                    context_baseline_after_usage = Some(total_tokens);
                    if pending_post_usage_live_partial {
                        // User already opened a post-usage turn with no known
                        // baseline: use this first counter as baseline only
                        // (delta 0), not as growth from zero.
                        pending_post_usage_live_partial = false;
                        active_turn = Some(ActiveTurn::new(
                            total_tokens,
                            timestamp,
                            current_model.clone(),
                            turn_index,
                        ));
                        turn_index = turn_index.saturating_add(1);
                    } else if let Some(turn) = active_turn.as_mut() {
                        // Should not zero-base after usage; observe only if a
                        // turn already exists with a real baseline.
                        turn.observe_total(total_tokens, timestamp);
                    }
                } else if let Some(turn) = active_turn.as_mut() {
                    turn.observe_total(total_tokens, timestamp);
                }
            }
        }
    }

    if saw_usage {
        // Live partial: open turn after the last completed usage only.
        if let Some(turn) = active_turn {
            if let Some(message) = turn.into_message(&metadata) {
                usage_messages.push(message);
            }
        }
        // signals.json is context occupancy — do not reconcile against usage.
        // Keep any legacy context turns that completed before the first usage
        // record (pre-upgrade prefix of a mixed session).
        if context_messages.is_empty() {
            return usage_messages;
        }
        context_messages.extend(usage_messages);
        return context_messages;
    }

    if let Some(turn) = active_turn {
        if let Some(message) = turn.into_message(&metadata) {
            context_messages.push(message);
        }
    }

    if context_messages.is_empty() {
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
                context_messages.push(message);
            }
        }
    }

    append_signals_reconciliation(path, &metadata, &mut context_messages, &current_model);
    context_messages
}

fn emit_usage_messages(
    usage_value: &Value,
    metadata: &GrokMetadata,
    fallback_model: &str,
    timestamp: i64,
    turn_index: usize,
    prompt_id: Option<&str>,
) -> Vec<UnifiedMessage> {
    let top = parse_usage_object(usage_value, None);
    let mut rows: Vec<ParsedUsage> = Vec::new();

    if let Some(model_usage) = usage_value.get("modelUsage").and_then(|v| v.as_object()) {
        for (model_id, entry) in model_usage {
            let parsed = parse_usage_object(entry, Some(model_id.as_str()));
            if parsed.has_positive_tokens() {
                rows.push(parsed);
            }
        }
    }

    // True only when parent costUsdTicks was copied onto a modelUsage row.
    // Multi-model zero-cost siblings may then be ProviderReported $0; otherwise
    // an omitting sibling stays Unknown for apply_pricing estimation.
    let mut parent_cost_inherited = false;

    if rows.is_empty() {
        if top.has_positive_tokens() {
            rows.push(top);
        } else {
            return Vec::new();
        }
    } else {
        // Model entries sometimes only carry token buckets while the parent
        // usage object holds costUsdTicks / apiDurationMs. Inherit so we do not
        // drop provider-reported cost. Single-model: full inherit. Multi-model:
        // put parent totals on the first row only when no model entry has them
        // (avoids double-counting the parent cost across models).
        parent_cost_inherited = inherit_top_level_cost_and_duration(&mut rows, &top);
    }

    let turn_key = prompt_id
        .filter(|id| !id.trim().is_empty())
        .map(|id| format!("turn:{id}"))
        .unwrap_or_else(|| format!("turn:{turn_index}"));

    let multi_model = rows.len() > 1;
    // Only when multi-model inherits parent cost onto the first row must
    // remaining zero-cost siblings be ProviderReported at $0 so pricing cannot
    // stack estimates on top of that once-inherited total. If a model entry has
    // its own costUsdTicks and a sibling simply omits cost (no parent inherit),
    // the omitting row stays Unknown so apply_pricing can estimate it.
    let mut messages = Vec::with_capacity(rows.len());
    for (model_i, row) in rows.into_iter().enumerate() {
        let model_id = row
            .model_id
            .clone()
            .filter(|m| !m.trim().is_empty())
            .or_else(|| metadata.model_id.clone())
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| fallback_model.to_string());

        let tokens = row.token_breakdown();
        if tokens.input == 0
            && tokens.output == 0
            && tokens.cache_read == 0
            && tokens.reasoning == 0
        {
            continue;
        }

        let dedup_key = if multi_model {
            format!("grok:{}:{turn_key}:{model_id}", metadata.session_id)
        } else {
            format!("grok:{}:{turn_key}", metadata.session_id)
        };

        let (cost, cost_source) = match row.cost_usd() {
            Some(cost) => (cost, CostSource::ProviderReported),
            None if multi_model && parent_cost_inherited => {
                (0.0, CostSource::ProviderReported)
            }
            None => (0.0, CostSource::Unknown),
        };

        let mut message = UnifiedMessage::new_with_dedup(
            CLIENT_ID,
            model_id,
            PROVIDER_ID,
            metadata.session_id.clone(),
            timestamp,
            tokens,
            cost,
            Some(dedup_key),
        );
        if cost_source == CostSource::ProviderReported {
            message.mark_provider_reported_cost();
        }
        if let Some(duration_ms) = row.api_duration_ms.filter(|ms| *ms > 0) {
            message.duration_ms = Some(duration_ms);
        }
        message.set_workspace(
            metadata.workspace_key.clone(),
            metadata.workspace_label.clone(),
        );
        // Match unified-log semantics: only the first model row of a turn
        // counts as a user message so report sums do not overcount.
        message.is_turn_start = model_i == 0;
        message.message_count = i32::from(message.is_turn_start);
        messages.push(message);
    }

    messages
}

/// When `modelUsage` rows omit cost/duration, copy them from the parent usage
/// object. Single-model inherits fully; multi-model places parent totals on
/// the first row only if every model entry is missing them.
///
/// Returns `true` when parent `costUsdTicks` was copied onto a row (so multi-
/// model zero-cost siblings may be sealed as ProviderReported $0).
fn inherit_top_level_cost_and_duration(rows: &mut [ParsedUsage], top: &ParsedUsage) -> bool {
    if rows.is_empty() {
        return false;
    }
    let mut cost_inherited = false;
    if rows.len() == 1 {
        if rows[0].cost_usd_ticks <= 0 && top.cost_usd_ticks > 0 {
            rows[0].cost_usd_ticks = top.cost_usd_ticks;
            cost_inherited = true;
        }
        if rows[0].api_duration_ms.is_none() {
            rows[0].api_duration_ms = top.api_duration_ms;
        }
        return cost_inherited;
    }
    if top.cost_usd_ticks > 0 && rows.iter().all(|r| r.cost_usd_ticks <= 0) {
        rows[0].cost_usd_ticks = top.cost_usd_ticks;
        cost_inherited = true;
    }
    if top.api_duration_ms.is_some() && rows.iter().all(|r| r.api_duration_ms.is_none()) {
        rows[0].api_duration_ms = top.api_duration_ms;
    }
    cost_inherited
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

fn parse_usage_object(value: &Value, model_id: Option<&str>) -> ParsedUsage {
    ParsedUsage {
        input_tokens: non_negative_i64(value.get("inputTokens")),
        output_tokens: non_negative_i64(value.get("outputTokens")),
        reasoning_tokens: non_negative_i64(value.get("reasoningTokens")),
        cached_read_tokens: non_negative_i64(value.get("cachedReadTokens")),
        cost_usd_ticks: non_negative_i64(value.get("costUsdTicks")),
        api_duration_ms: extract_i64(value.get("apiDurationMs")).filter(|ms| *ms > 0),
        model_id: model_id
            .map(str::to_string)
            .or_else(|| extract_string(value.get("modelId")))
            .or_else(|| extract_string(value.get("model"))),
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
    // Context occupancy counter only. Do NOT read params.update.usage.totalTokens
    // here — that is per-turn API spend handled by the usage path, not a
    // cumulative context counter.
    for path in [
        &["params", "_meta", "totalTokens"][..],
        &["params", "update", "_meta", "totalTokens"][..],
        &["params", "update", "totalTokens"][..],
        &["params", "totalTokens"][..],
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

fn is_turn_completed(value: &Value) -> bool {
    get_path(value, &["params", "update", "sessionUpdate"]).and_then(|value| value.as_str())
        == Some("turn_completed")
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

    fn token_all(messages: &[UnifiedMessage]) -> i64 {
        messages
            .iter()
            .map(|m| {
                m.tokens.input
                    + m.tokens.output
                    + m.tokens.cache_read
                    + m.tokens.cache_write
                    + m.tokens.reasoning
            })
            .sum()
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

    #[test]
    fn prefers_turn_completed_usage_over_context_counter() {
        // Context peaks at 236879 (old undercount); two completed turns report
        // far larger API usage. Primary path must sum usage, not context growth.
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_thought_chunk"},"_meta":{"totalTokens":100000,"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","usage":{"inputTokens":372794,"outputTokens":8021,"totalTokens":380815,"cachedReadTokens":314112,"reasoningTokens":4144,"modelCalls":8,"apiDurationMs":136943,"costUsdTicks":2597236000,"modelUsage":{"grok-4.5-build":{"inputTokens":372794,"outputTokens":8021,"totalTokens":380815,"cachedReadTokens":314112,"reasoningTokens":4144,"modelCalls":8,"apiDurationMs":136943,"costUsdTicks":2597236000}}}},"_meta":{"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000003000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":236879,"agentTimestampMs":1700000004000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p2","usage":{"inputTokens":1000,"outputTokens":200,"totalTokens":1200,"cachedReadTokens":400,"reasoningTokens":50,"costUsdTicks":1000000000,"modelUsage":{"grok-4.5-build":{"inputTokens":1000,"outputTokens":200,"totalTokens":1200,"cachedReadTokens":400,"reasoningTokens":50,"costUsdTicks":1000000000}}}},"_meta":{"agentTimestampMs":1700000005000}}}"#,
            Some(r#"{"current_model_id":"grok-4.5"}"#),
            Some(
                r#"{"primaryModelId":"grok-4.5","contextTokensUsed":236879,"totalTokensBeforeCompaction":0}"#,
            ),
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].model_id, "grok-4.5-build");
        // input nets out cache: 372794 - 314112 = 58682
        assert_eq!(messages[0].tokens.input, 58682);
        assert_eq!(messages[0].tokens.output, 8021);
        assert_eq!(messages[0].tokens.cache_read, 314112);
        assert_eq!(messages[0].tokens.reasoning, 4144);
        assert_eq!(messages[0].duration_ms, Some(136943));
        assert_eq!(messages[0].cost_source, CostSource::ProviderReported);
        assert!((messages[0].cost - 2.597236).abs() < 1e-9);
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some("grok:session-1:turn:p1")
        );
        assert!(messages[0].is_turn_start);

        assert_eq!(messages[1].tokens.input, 600); // 1000 - 400
        assert_eq!(messages[1].tokens.output, 200);
        assert_eq!(messages[1].tokens.cache_read, 400);
        assert_eq!(messages[1].tokens.reasoning, 50);
        assert!((messages[1].cost - 1.0).abs() < 1e-12);

        // in+out+cache+reason across both turns == raw Grok input+output+reason
        // (cache is netted from input then re-added in the total).
        assert_eq!(token_all(&messages), 372794 + 8021 + 4144 + 1000 + 200 + 50);
        // Must not collapse to context peak (~236k) or signals context.
        assert!(token_all(&messages) > 380_000);
        assert!(messages.iter().all(|m| m.dedup_key.as_deref() != Some("grok:session-1:signals")));
    }

    #[test]
    fn usage_path_emits_live_partial_after_last_completed_turn() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":50000,"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","usage":{"inputTokens":10000,"outputTokens":100,"totalTokens":10100,"cachedReadTokens":0,"reasoningTokens":0}},"_meta":{"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000003000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":55000,"agentTimestampMs":1700000004000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 2);
        // Completed usage turn.
        assert_eq!(messages[0].tokens.input, 10000);
        assert_eq!(messages[0].tokens.output, 100);
        // Live open turn: context grew 50000 -> 55000 after completion.
        assert_eq!(messages[1].tokens.input, 5000);
        assert_eq!(messages[1].tokens.output, 0);
        assert!(messages[1].is_turn_start);
    }

    #[test]
    fn usage_path_does_not_double_count_completed_context_growth() {
        // Context grows a lot during the turn; only usage should count for that
        // completed turn (not both usage and the context delta).
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":200000,"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":500,"outputTokens":50,"totalTokens":550,"cachedReadTokens":0,"reasoningTokens":0}},"_meta":{"agentTimestampMs":1700000002000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 500);
        assert_eq!(messages[0].tokens.output, 50);
        assert_eq!(token_all(&messages), 550);
    }

    #[test]
    fn splits_model_usage_rows() {
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p-split","usage":{"inputTokens":300,"outputTokens":30,"totalTokens":330,"cachedReadTokens":0,"reasoningTokens":0,"modelUsage":{"grok-a":{"inputTokens":200,"outputTokens":20,"totalTokens":220,"cachedReadTokens":0,"reasoningTokens":0},"grok-b":{"inputTokens":100,"outputTokens":10,"totalTokens":110,"cachedReadTokens":0,"reasoningTokens":0}}}},"_meta":{"agentTimestampMs":1700000000000}}}"#,
            None,
            None,
        );

        let mut messages = parse_grok_updates_file(&path);
        messages.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].model_id, "grok-a");
        assert_eq!(messages[0].tokens.input, 200);
        assert_eq!(messages[1].model_id, "grok-b");
        assert_eq!(messages[1].tokens.input, 100);
        assert!(messages
            .iter()
            .all(|m| m.dedup_key.as_deref().unwrap().contains("turn:p-split:")));
        // Exactly one turn_start among the split rows.
        assert_eq!(messages.iter().filter(|m| m.is_turn_start).count(), 1);
        // message_count must not overcount a single turn split across models.
        assert_eq!(
            messages.iter().map(|m| m.message_count).sum::<i32>(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .find(|m| m.is_turn_start)
                .map(|m| m.message_count),
            Some(1)
        );
        assert!(messages
            .iter()
            .filter(|m| !m.is_turn_start)
            .all(|m| m.message_count == 0));
    }

    #[test]
    fn skips_context_counter_on_usage_line_with_total_tokens() {
        // turn_completed carries both usage and _meta.totalTokens. After the
        // usage path clears active_turn, the context arm must not reopen a
        // turn from that same line (would double-count completed growth).
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":100000,"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","usage":{"inputTokens":500,"outputTokens":50,"totalTokens":550,"cachedReadTokens":0,"reasoningTokens":0}},"_meta":{"totalTokens":200000,"agentTimestampMs":1700000002000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 500);
        assert_eq!(messages[0].tokens.output, 50);
        assert_eq!(token_all(&messages), 550);
    }

    #[test]
    fn preserves_legacy_context_turns_before_first_usage() {
        // Pre-upgrade turns only have context counters; a later turn has
        // turn_completed.usage. Keep the early legacy deltas, then usage.
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":100,"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":250,"agentTimestampMs":1700000003000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000004000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":300,"agentTimestampMs":1700000005000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p-new","usage":{"inputTokens":1000,"outputTokens":20,"totalTokens":1020,"cachedReadTokens":0,"reasoningTokens":0}},"_meta":{"agentTimestampMs":1700000006000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        // First legacy turn: 0→100; second: 100→250; then usage 1000+20.
        // (Third user_message_chunk opens a turn that is closed without emit
        // when usage arrives for that turn — only completed legacy turns
        // before saw_usage are kept via the user_message_chunk flush.)
        assert!(
            messages.len() >= 3,
            "expected legacy turns + usage, got {}",
            messages.len()
        );
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 0);
        assert_eq!(messages[1].tokens.input, 150);
        assert_eq!(messages[1].tokens.output, 0);
        let usage = messages
            .iter()
            .find(|m| m.dedup_key.as_deref() == Some("grok:session-1:turn:p-new"))
            .expect("usage turn");
        assert_eq!(usage.tokens.input, 1000);
        assert_eq!(usage.tokens.output, 20);
    }

    #[test]
    fn inherits_parent_cost_and_duration_onto_single_model_usage() {
        // Parent usage has cost/duration; model entry only has token buckets.
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p-cost","usage":{"inputTokens":100,"outputTokens":10,"totalTokens":110,"cachedReadTokens":0,"reasoningTokens":0,"apiDurationMs":42000,"costUsdTicks":2500000000,"modelUsage":{"grok-4.5-build":{"inputTokens":100,"outputTokens":10,"totalTokens":110,"cachedReadTokens":0,"reasoningTokens":0}}}},"_meta":{"agentTimestampMs":1700000000000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "grok-4.5-build");
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 10);
        assert_eq!(messages[0].duration_ms, Some(42000));
        assert_eq!(messages[0].cost_source, CostSource::ProviderReported);
        assert!((messages[0].cost - 2.5).abs() < 1e-12);
        assert_eq!(messages[0].message_count, 1);
    }

    #[test]
    fn post_usage_context_partials_require_user_message_chunk() {
        // After turn_completed.usage, non-user updates that only bump
        // `_meta.totalTokens` must not open a live partial ActiveTurn.
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":50000,"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","usage":{"inputTokens":10000,"outputTokens":100,"totalTokens":10100,"cachedReadTokens":0,"reasoningTokens":0}},"_meta":{"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":60000,"agentTimestampMs":1700000003000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_thought_chunk"},"_meta":{"totalTokens":65000,"agentTimestampMs":1700000004000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(
            messages.len(),
            1,
            "context-only growth after usage must not emit a partial, got {messages:?}"
        );
        assert_eq!(messages[0].tokens.input, 10000);
        assert_eq!(messages[0].tokens.output, 100);
        assert_eq!(token_all(&messages), 10100);
    }

    #[test]
    fn post_usage_user_turn_baseline_skips_inter_turn_context_growth() {
        // Context-only growth between completed usage and the next user turn
        // advances the baseline only; the live partial starts from the latest
        // counter, not the post-usage snapshot.
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":50000,"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","usage":{"inputTokens":10000,"outputTokens":100,"totalTokens":10100,"cachedReadTokens":0,"reasoningTokens":0}},"_meta":{"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":60000,"agentTimestampMs":1700000003000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000004000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":65000,"agentTimestampMs":1700000005000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 10000);
        assert_eq!(messages[0].tokens.output, 100);
        // Live partial: 65000 - 60000 (inter-turn growth was baselined away).
        assert_eq!(messages[1].tokens.input, 5000);
        assert_eq!(messages[1].tokens.output, 0);
        assert!(messages[1].is_turn_start);
    }

    #[test]
    fn multi_model_parent_cost_blocks_sibling_repricing() {
        // Parent cost lands on the first modelUsage row only; siblings must be
        // ProviderReported at $0 so a later pricing pass cannot estimate them
        // on top of the parent total.
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p-multi-cost","usage":{"inputTokens":300,"outputTokens":30,"totalTokens":330,"cachedReadTokens":0,"reasoningTokens":0,"costUsdTicks":2500000000,"modelUsage":{"grok-a":{"inputTokens":200,"outputTokens":20,"totalTokens":220,"cachedReadTokens":0,"reasoningTokens":0},"grok-b":{"inputTokens":100,"outputTokens":10,"totalTokens":110,"cachedReadTokens":0,"reasoningTokens":0}}}},"_meta":{"agentTimestampMs":1700000000000}}}"#,
            None,
            None,
        );

        let mut messages = parse_grok_updates_file(&path);
        messages.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].model_id, "grok-a");
        assert_eq!(messages[1].model_id, "grok-b");

        let parent_cost = 2.5;
        let total_before: f64 = messages.iter().map(|m| m.cost).sum();
        assert!(
            (total_before - parent_cost).abs() < 1e-12,
            "parent cost must appear once before pricing, got {total_before}"
        );
        assert!(messages
            .iter()
            .all(|m| m.cost_source == CostSource::ProviderReported));
        assert!(messages.iter().all(|m| m.has_authoritative_cost()));
        // Exactly one row carries the parent dollars; the sibling is $0.
        assert_eq!(
            messages.iter().filter(|m| m.cost > 0.0).count(),
            1,
            "only the first inherited row should carry parent cost"
        );
        assert!(messages.iter().any(|m| m.cost == 0.0));

        // Hermetic pricing pass mirroring apply_pricing_if_available: models
        // are resolvable at non-zero rates, but authoritative rows must not
        // be overwritten.
        let mut litellm = std::collections::HashMap::new();
        litellm.insert(
            "grok-a".to_string(),
            crate::pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        litellm.insert(
            "grok-b".to_string(),
            crate::pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = crate::pricing::PricingService::new(litellm, std::collections::HashMap::new());
        for message in &mut messages {
            if message.has_authoritative_cost() {
                continue;
            }
            let calculated = pricing.calculate_cost_with_provider(
                &message.model_id,
                Some(&message.provider_id),
                &message.tokens,
            );
            if calculated > 0.0 {
                message.cost = calculated;
                message.mark_estimated_cost();
            }
        }

        let total_after: f64 = messages.iter().map(|m| m.cost).sum();
        assert!(
            (total_after - parent_cost).abs() < 1e-12,
            "total cost must equal parent only once after pricing, got {total_after}"
        );
        assert!(messages
            .iter()
            .all(|m| m.cost_source == CostSource::ProviderReported));
    }

    #[test]
    fn post_usage_live_partial_does_not_zero_base_without_counter() {
        // Completed usage with no _meta.totalTokens ever seen, then a new user
        // turn whose first context counter is full occupancy. Must not emit
        // that occupancy as delta-from-0 (would double-count completed usage).
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","usage":{"inputTokens":10000,"outputTokens":100,"totalTokens":10100,"cachedReadTokens":0,"reasoningTokens":0}},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":50000,"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":55000,"agentTimestampMs":1700000003000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(messages.len(), 2, "usage + live partial only, got {messages:?}");
        assert_eq!(messages[0].tokens.input, 10000);
        assert_eq!(messages[0].tokens.output, 100);
        // First counter (50000) is baseline only; live partial is 55000-50000.
        assert_eq!(
            messages[1].tokens.input, 5000,
            "must not emit full occupancy 50000 as zero-based input"
        );
        assert_eq!(messages[1].tokens.output, 0);
        assert!(messages[1].is_turn_start);
        assert_eq!(token_all(&messages), 10000 + 100 + 5000);
    }

    #[test]
    fn post_usage_live_partial_waits_for_baseline_when_only_first_counter() {
        // Same shape but open turn ends at the first post-usage counter: that
        // counter establishes baseline only, so no live partial is emitted.
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","usage":{"inputTokens":8000,"outputTokens":80,"totalTokens":8080,"cachedReadTokens":0,"reasoningTokens":0}},"_meta":{"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":42000,"agentTimestampMs":1700000002000}}}"#,
            None,
            None,
        );

        let messages = parse_grok_updates_file(&path);
        assert_eq!(
            messages.len(),
            1,
            "first post-usage counter is baseline only, got {messages:?}"
        );
        assert_eq!(messages[0].tokens.input, 8000);
        assert_eq!(messages[0].tokens.output, 80);
        assert_eq!(token_all(&messages), 8080);
    }

    #[test]
    fn multi_model_own_cost_leaves_omitting_sibling_unknown() {
        // One modelUsage entry has its own costUsdTicks; the other omits cost
        // and parent does not cover both (parent has no cost). Omitting sibling
        // must stay Unknown so apply_pricing can estimate — not ProviderReported $0.
        let (_temp, path) = write_fixture(
            r#"{"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","prompt_id":"p-partial-cost","usage":{"inputTokens":300,"outputTokens":30,"totalTokens":330,"cachedReadTokens":0,"reasoningTokens":0,"modelUsage":{"grok-a":{"inputTokens":200,"outputTokens":20,"totalTokens":220,"cachedReadTokens":0,"reasoningTokens":0,"costUsdTicks":1500000000},"grok-b":{"inputTokens":100,"outputTokens":10,"totalTokens":110,"cachedReadTokens":0,"reasoningTokens":0}}}},"_meta":{"agentTimestampMs":1700000000000}}}"#,
            None,
            None,
        );

        let mut messages = parse_grok_updates_file(&path);
        messages.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        assert_eq!(messages.len(), 2);
        let a = messages.iter().find(|m| m.model_id == "grok-a").unwrap();
        let b = messages.iter().find(|m| m.model_id == "grok-b").unwrap();
        assert_eq!(a.cost_source, CostSource::ProviderReported);
        assert!((a.cost - 1.5).abs() < 1e-12);
        assert_eq!(
            b.cost_source,
            CostSource::Unknown,
            "omitting sibling must stay Unknown when parent cost was not inherited"
        );
        assert_eq!(b.cost, 0.0);
        assert!(!b.has_authoritative_cost());

        // Pricing may estimate the Unknown sibling; authoritative row stays put.
        let mut litellm = std::collections::HashMap::new();
        litellm.insert(
            "grok-b".to_string(),
            crate::pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = crate::pricing::PricingService::new(litellm, std::collections::HashMap::new());
        for message in &mut messages {
            if message.has_authoritative_cost() {
                continue;
            }
            let calculated = pricing.calculate_cost_with_provider(
                &message.model_id,
                Some(&message.provider_id),
                &message.tokens,
            );
            if calculated > 0.0 {
                message.cost = calculated;
                message.mark_estimated_cost();
            }
        }
        let a = messages.iter().find(|m| m.model_id == "grok-a").unwrap();
        let b = messages.iter().find(|m| m.model_id == "grok-b").unwrap();
        assert_eq!(a.cost_source, CostSource::ProviderReported);
        assert!((a.cost - 1.5).abs() < 1e-12);
        assert_eq!(b.cost_source, CostSource::Estimated);
        // 100 input * 0.01 + 10 output * 0.02 = 1.2
        assert!((b.cost - 1.2).abs() < 1e-12);
    }
}
