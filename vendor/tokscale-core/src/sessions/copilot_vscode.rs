use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::provider_identity::inferred_provider_from_model;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub fn parse_copilot_vscode_sessions(paths: &[PathBuf]) -> Vec<UnifiedMessage> {
    paths.iter().flat_map(|path| parse_file(path)).collect()
}

fn parse_file(path: &Path) -> Vec<UnifiedMessage> {
    let session_id = match path.file_stem().and_then(|s| s.to_str()) {
        Some(stem) => stem.to_string(),
        None => return Vec::new(),
    };

    let workspace = read_workspace_for_file(path);

    // Legacy full-session JSON (older VS Code shape): whole file is one object
    // with a `requests` array. Prefer this when the extension is `.json` (not
    // `.jsonl`). If the file is JSONL-shaped multi-line despite `.json`, fall
    // back to the mutation-log path below.
    let is_legacy_json = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));

    if is_legacy_json {
        if let Ok(raw) = std::fs::read_to_string(path) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                if let Ok(obj) = serde_json::from_str::<Value>(trimmed) {
                    if let Some(requests) = extract_requests_array(&obj) {
                        return requests
                            .iter()
                            .filter_map(|req| request_to_message(req, &session_id, &workspace))
                            .collect();
                    }
                    // Whole-file JSON without a requests array: not a chat
                    // session payload (config/junk). Do not line-scan.
                    return Vec::new();
                }
                // Not a single JSON object — may be JSONL misnamed as .json.
            }
        }
    }

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    // Replay the VS Code chatSessions mutation log into a final requests array.
    // kind:0 snapshots set the whole list; kind:1 and kind:2 set-at-path patches
    // under "requests" (full array replace, index replace, or nested field set).
    // Outside `requests`, other kind:1 payloads are ignored — we do not invent
    // non-requests kind:1 semantics.
    let mut requests: Vec<Value> = Vec::new();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = obj.get("kind").and_then(Value::as_i64).unwrap_or(-1);
        match kind {
            0 => {
                // Full document snapshot: replace requests from v.requests when present.
                if let Some(arr) = obj.pointer("/v/requests").and_then(Value::as_array) {
                    requests = arr.clone();
                }
            }
            // kind:1 and kind:2 both carry set-at-path patches. Real sessions use
            // either form for `k:["requests",N,"completionTokens"]`-style updates.
            1 | 2 => {
                if let Some(k) = obj.get("k").and_then(Value::as_array) {
                    apply_requests_patch(&mut requests, k, obj.get("v"));
                }
            }
            _ => {}
        }
    }

    requests
        .iter()
        .filter_map(|req| request_to_message(req, &session_id, &workspace))
        .collect()
}

/// Extract a `requests` array from a legacy full-session JSON object.
/// Accepts top-level `requests`, nested `v.requests`, or snapshot-style
/// pointers already used by the mutation-log path. Unrelated JSON without a
/// requests array returns `None` so junk is not treated as a session.
fn extract_requests_array(obj: &Value) -> Option<Vec<Value>> {
    const PATHS: &[&str] = &["/requests", "/v/requests"];
    for path in PATHS {
        if let Some(arr) = obj.pointer(path).and_then(Value::as_array) {
            return Some(arr.clone());
        }
    }
    None
}

/// Apply a kind:1 or kind:2 mutation-log entry whose path starts at `"requests"`.
///
/// Supported path shapes (minimal correct subset):
/// - `k == ["requests"]` — full array replace when `v` is an array
/// - `k == ["requests", N]` — set/replace request at index N
/// - `k == ["requests", N, ...path]` — deep-set into request N along `path`
///
/// Non-`requests` roots are ignored. Index segments may be JSON numbers or
/// digit strings. Missing intermediate objects/arrays are created as needed.
fn apply_requests_patch(requests: &mut Vec<Value>, k: &[Value], v: Option<&Value>) {
    let Some(root) = k.first().and_then(Value::as_str) else {
        return;
    };
    if root != "requests" {
        return;
    }
    let Some(v) = v else {
        return;
    };

    // Full array replace: k == ["requests"], v is the whole requests array.
    if k.len() == 1 {
        if let Some(arr) = v.as_array() {
            *requests = arr.clone();
        }
        return;
    }

    let Some(idx) = path_segment_as_index(&k[1]) else {
        return;
    };
    // Grow with empty objects so sparse index patches (common after a kind:0
    // stub with requests:[]) can land at the intended slot.
    while requests.len() <= idx {
        requests.push(Value::Object(serde_json::Map::new()));
    }

    if k.len() == 2 {
        requests[idx] = v.clone();
        return;
    }

    deep_set_path(&mut requests[idx], &k[2..], v);
}

fn path_segment_as_index(seg: &Value) -> Option<usize> {
    if let Some(n) = seg.as_u64() {
        return usize::try_from(n).ok();
    }
    if let Some(n) = seg.as_i64() {
        if n < 0 {
            return None;
        }
        return usize::try_from(n).ok();
    }
    if let Some(s) = seg.as_str() {
        return s.parse::<usize>().ok();
    }
    None
}

fn path_segment_as_key(seg: &Value) -> Option<String> {
    if let Some(s) = seg.as_str() {
        return Some(s.to_string());
    }
    if let Some(n) = seg.as_i64() {
        return Some(n.to_string());
    }
    if let Some(n) = seg.as_u64() {
        return Some(n.to_string());
    }
    None
}

/// Set `target` at `path` to `v`, creating intermediate objects/arrays.
fn deep_set_path(target: &mut Value, path: &[Value], v: &Value) {
    if path.is_empty() {
        *target = v.clone();
        return;
    }

    // Last segment: assign into the current container.
    if path.len() == 1 {
        if let Some(idx) = path_segment_as_index(&path[0]) {
            if !target.is_array() {
                *target = Value::Array(Vec::new());
            }
            if let Some(arr) = target.as_array_mut() {
                while arr.len() <= idx {
                    arr.push(Value::Null);
                }
                arr[idx] = v.clone();
            }
            return;
        }
        if let Some(key) = path_segment_as_key(&path[0]) {
            if !target.is_object() {
                *target = Value::Object(serde_json::Map::new());
            }
            if let Some(obj) = target.as_object_mut() {
                obj.insert(key, v.clone());
            }
            return;
        }
        return;
    }

    // Intermediate segment: ensure container then recurse.
    if let Some(idx) = path_segment_as_index(&path[0]) {
        if !target.is_array() {
            *target = Value::Array(Vec::new());
        }
        if let Some(arr) = target.as_array_mut() {
            while arr.len() <= idx {
                arr.push(Value::Object(serde_json::Map::new()));
            }
            deep_set_path(&mut arr[idx], &path[1..], v);
        }
        return;
    }
    if let Some(key) = path_segment_as_key(&path[0]) {
        if !target.is_object() {
            *target = Value::Object(serde_json::Map::new());
        }
        if let Some(obj) = target.as_object_mut() {
            let entry = obj
                .entry(key)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            deep_set_path(entry, &path[1..], v);
        }
    }
}

fn request_to_message(
    req: &Value,
    session_id: &str,
    workspace: &Option<(String, Option<String>)>,
) -> Option<UnifiedMessage> {
    // Usage may live at request root, under `result.*` (common snapshot shape),
    // or under `response.result.*` / `response.*` after kind:1/2 deep-sets of
    // `k:["requests",N,"response"]`. Prefer root/`result` first.
    let prompt_tokens = request_i64_first(
        req,
        &[
            "/promptTokens",
            "/result/metadata/promptTokens",
            "/response/result/metadata/promptTokens",
            "/response/metadata/promptTokens",
            "/response/promptTokens",
        ],
    );

    let completion_tokens = request_i64_first(
        req,
        &[
            "/completionTokens",
            "/result/metadata/outputTokens",
            "/result/metadata/completionTokens",
            "/response/result/metadata/outputTokens",
            "/response/result/metadata/completionTokens",
            "/response/metadata/outputTokens",
            "/response/completionTokens",
        ],
    );

    if prompt_tokens == 0 && completion_tokens == 0 {
        return None;
    }

    let timestamp_ms = request_i64_first(
        req,
        &[
            "/timestamp",
            "/response/timestamp",
            "/response/result/timestamp",
            "/result/timestamp",
        ],
    );

    let resolved_model = request_str_first(
        req,
        &[
            "/result/metadata/resolvedModel",
            "/response/result/metadata/resolvedModel",
            "/response/metadata/resolvedModel",
            "/response/resolvedModel",
        ],
    );

    let model_id_raw = request_str_first(
        req,
        &["/modelId", "/response/modelId", "/result/modelId"],
    );

    // Strict Copilot marker: do not treat bare `resolvedModel` as sufficient.
    // Accept only when modelId is under the `copilot/` provider namespace, or a
    // participant/agent metadata field clearly names Copilot. Generic chat
    // providers that only carry tokens + resolvedModel must be rejected.
    if !is_copilot_request(req, model_id_raw) {
        return None;
    }

    let model_id = resolved_model
        .or_else(|| model_id_raw.map(|m| m.strip_prefix("copilot/").unwrap_or(m)))
        .unwrap_or("auto")
        .to_string();

    let provider_id = inferred_provider_from_model(&model_id)
        .unwrap_or("github-copilot")
        .to_string();

    let reasoning_tokens = request_reasoning_tokens(req);

    // Cache buckets: align with OTEL/Desktop via shared normalize_input_tokens
    // (OTEL reports cache-inclusive input; same netting applies here when
    // promptTokens already includes cacheRead).
    let cache_read = request_cache_read_tokens(req);
    let cache_write = request_cache_write_tokens(req);
    let tokens = super::copilot::normalize_input_tokens(
        prompt_tokens,
        completion_tokens,
        cache_read,
        cache_write,
        reasoning_tokens,
    );

    // Include requestId when present so two distinct requests that share a
    // session_id and millisecond timestamp do not collapse into one key.
    let request_id = req
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let dedup_key = match request_id {
        Some(rid) => format!("copilot-vscode:{session_id}:{timestamp_ms}:{rid}"),
        None => format!("copilot-vscode:{session_id}:{timestamp_ms}"),
    };

    let (cost, has_billed_credit) = request_billed_cost(req);

    let mut message = UnifiedMessage::new_with_dedup(
        "copilot",
        model_id,
        provider_id,
        session_id.to_string(),
        timestamp_ms,
        tokens,
        cost,
        Some(dedup_key),
    );

    // Provider-billed AIU/credits are authoritative; skip later reprice.
    if has_billed_credit {
        message.mark_provider_reported_cost();
    }

    if let Some((key, label)) = workspace {
        message.set_workspace(Some(key.clone()), label.clone());
    }

    Some(message)
}

/// Prefer the first **positive** i64 among JSON pointers (order = priority).
///
/// If every candidate is missing or non-positive, returns the first present
/// non-positive value, or `0` when all paths are absent.
///
/// Root placeholder zeros must not shadow nonzero nested usage after kind:1/2
/// patches (e.g. `promptTokens: 0` at request root with real counts under
/// `/response/result/metadata/promptTokens`). Same for timestamps (`0` → 1970).
fn request_i64_first(req: &Value, paths: &[&str]) -> i64 {
    let mut first_non_positive: Option<i64> = None;
    for path in paths {
        if let Some(n) = req.pointer(path).and_then(json_number_as_i64) {
            if n > 0 {
                return n;
            }
            if first_non_positive.is_none() {
                first_non_positive = Some(n);
            }
        }
    }
    first_non_positive.unwrap_or(0)
}

/// First non-empty string at any JSON pointer.
fn request_str_first<'a>(req: &'a Value, paths: &[&str]) -> Option<&'a str> {
    for path in paths {
        if let Some(s) = req
            .pointer(path)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(s);
        }
    }
    None
}

/// Reasoning tokens from toolCallRounds under result / response.result.
fn request_reasoning_tokens(req: &Value) -> i64 {
    const PATHS: &[&str] = &[
        "/result/metadata/toolCallRounds",
        "/response/result/metadata/toolCallRounds",
        "/response/metadata/toolCallRounds",
    ];
    for path in PATHS {
        if let Some(rounds) = req.pointer(path).and_then(Value::as_array) {
            let sum: i64 = rounds
                .iter()
                .filter_map(|r| r.pointer("/thinking/tokens").and_then(Value::as_i64))
                .sum();
            if sum > 0 {
                return sum;
            }
        }
    }
    0
}

/// Read cache-read tokens from common request / result / response key shapes.
fn request_cache_read_tokens(req: &Value) -> i64 {
    const KEYS: &[&str] = &[
        "/cacheReadTokens",
        "/cache_read_tokens",
        "/cacheRead",
        "/result/metadata/cacheReadTokens",
        "/result/metadata/cache_read_tokens",
        "/result/metadata/cacheRead",
        "/response/result/metadata/cacheReadTokens",
        "/response/result/metadata/cache_read_tokens",
        "/response/result/metadata/cacheRead",
        "/response/metadata/cacheReadTokens",
        "/response/cacheReadTokens",
    ];
    for path in KEYS {
        if let Some(n) = req.pointer(path).and_then(json_number_as_i64) {
            if n > 0 {
                return n;
            }
        }
    }
    0
}

/// Read cache-write / cache-creation tokens from common key shapes.
fn request_cache_write_tokens(req: &Value) -> i64 {
    const KEYS: &[&str] = &[
        "/cacheWriteTokens",
        "/cache_write_tokens",
        "/cacheWrite",
        "/cacheCreationTokens",
        "/cache_creation_tokens",
        "/result/metadata/cacheWriteTokens",
        "/result/metadata/cache_write_tokens",
        "/result/metadata/cacheWrite",
        "/result/metadata/cacheCreationTokens",
        "/result/metadata/cache_creation_tokens",
        "/response/result/metadata/cacheWriteTokens",
        "/response/result/metadata/cache_write_tokens",
        "/response/result/metadata/cacheWrite",
        "/response/result/metadata/cacheCreationTokens",
        "/response/result/metadata/cache_creation_tokens",
        "/response/metadata/cacheWriteTokens",
        "/response/cacheWriteTokens",
    ];
    for path in KEYS {
        if let Some(n) = req.pointer(path).and_then(json_number_as_i64) {
            if n > 0 {
                return n;
            }
        }
    }
    0
}

/// Extract provider-billed **USD** cost from a VS Code chat request, if present.
///
/// Preference order:
/// 1. Numeric nano AIU fields (`nanoAiu`, `copilotUsageNanoAiu`) at request root
///    or under `result` / `result.metadata` — convert via
///    [`super::copilot::copilot_aiu_nano_to_usd`] (1e9 nano = 1 AI credit = $0.01).
/// 2. Labeled credit/dollar string under `result.details` (and common nested
///    keys) when no numeric nano field is present. Credits/AIU are scaled to
///    USD (`* 0.01`); `$…` amounts are already USD and are not rescaled.
///
/// Returns `(cost_usd, is_provider_reported)`.
fn request_billed_cost(req: &Value) -> (f64, bool) {
    const NANO_PATHS: &[&str] = &[
        "/nanoAiu",
        "/copilotUsageNanoAiu",
        "/result/nanoAiu",
        "/result/copilotUsageNanoAiu",
        "/result/metadata/nanoAiu",
        "/result/metadata/copilotUsageNanoAiu",
        "/response/nanoAiu",
        "/response/copilotUsageNanoAiu",
        "/response/result/nanoAiu",
        "/response/result/copilotUsageNanoAiu",
        "/response/result/metadata/nanoAiu",
        "/response/result/metadata/copilotUsageNanoAiu",
        "/response/metadata/nanoAiu",
        "/response/metadata/copilotUsageNanoAiu",
    ];
    for path in NANO_PATHS {
        if let Some(nano) = req.pointer(path).and_then(json_number_as_i64) {
            if nano > 0 {
                // nano AIU → USD: 1e9 nano = 1 AI credit = $0.01.
                return (super::copilot::copilot_aiu_nano_to_usd(nano), true);
            }
        }
    }

    // Labeled credit/USD under result.details when numeric nano fields are absent.
    // Nested credit-named numeric fields are AI credits; `…/cost` is treated as USD.
    // Also try `response.result.details` after kind:1/2 response deep-sets.
    const DETAIL_PATHS: &[&str] = &[
        "/result/details",
        "/result/details/credit",
        "/result/details/credits",
        "/result/details/cost",
        "/result/details/billedCredit",
        "/result/details/billed_credit",
        "/response/result/details",
        "/response/result/details/credit",
        "/response/result/details/credits",
        "/response/result/details/cost",
        "/response/result/details/billedCredit",
        "/response/result/details/billed_credit",
        "/response/details",
        "/response/details/credit",
        "/response/details/credits",
        "/response/details/cost",
        "/response/details/billedCredit",
        "/response/details/billed_credit",
    ];
    for path in DETAIL_PATHS {
        if let Some(raw) = req.pointer(path).and_then(Value::as_str) {
            if let Some(cost_usd) = parse_credit_string_to_usd(raw) {
                if cost_usd > 0.0 {
                    return (cost_usd, true);
                }
            }
        }
        // details may itself be an object with numeric credit/cost fields.
        if let Some(n) = req.pointer(path).and_then(json_number_as_f64_raw) {
            if n > 0.0 {
                let cost_usd = if detail_path_is_credit_units(path) {
                    super::copilot::copilot_ai_credits_to_usd(n)
                } else {
                    // `/result/details/cost` (and bare numeric `/result/details`)
                    // treated as already-USD — no *0.01.
                    n
                };
                if cost_usd > 0.0 {
                    return (cost_usd, true);
                }
            }
        }
    }

    (0.0, false)
}

fn detail_path_is_credit_units(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    // credit / credits / billedCredit / billed_credit — not plain "cost".
    lower.contains("credit") || lower.contains("aiu") || lower.contains("billed")
}

fn json_number_as_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
        .or_else(|| {
            v.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| f as i64)
        })
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// Raw JSON number only (no string credit parse — callers decide USD conversion).
fn json_number_as_f64_raw(v: &Value) -> Option<f64> {
    v.as_f64()
        .filter(|f| f.is_finite())
        .or_else(|| v.as_i64().map(|n| n as f64))
        .or_else(|| v.as_u64().map(|n| n as f64))
}

/// Parse a labeled credit/dollar detail string into **USD**.
///
/// Only numbers clearly attached to credit/AIU/USD labels are accepted:
/// - `$12.3` / `Cost: $0.05` → USD as-is
/// - `16.3 credits`, `1 AIU`, `2 AI credits` → credit units × $0.01
///
/// Bare first numbers and model/multiplier noise (`GPT 5.4 • 2x`) return `None`
/// so model versions are never mistaken for billed cost.
fn parse_credit_string_to_usd(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(usd) = parse_dollar_amount(trimmed) {
        return Some(usd);
    }
    if let Some(credits) = parse_credit_labeled_amount(trimmed) {
        return Some(super::copilot::copilot_ai_credits_to_usd(credits));
    }
    None
}

/// `$(\d+(?:\.\d+)?)` anywhere in the string (case-insensitive noise ok).
fn parse_dollar_amount(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if let Some((num, _)) = parse_decimal_at(s, j) {
                if num > 0.0 {
                    return Some(num);
                }
            }
        }
        i += 1;
    }
    None
}

/// `(\d+(?:\.\d+)?)\s*(credits?|aiu|ai\s*credits?)` — case-insensitive.
/// Picks the number that is **attached** to a credit label, not a bare model
/// version like `5.4` in `GPT 5.4 • 2x`.
fn parse_credit_labeled_amount(s: &str) -> Option<f64> {
    let lower = s.to_ascii_lowercase();
    let bytes = s.as_bytes();
    let lower_bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let is_num_start = bytes[i].is_ascii_digit()
            || (bytes[i] == b'.'
                && i + 1 < bytes.len()
                && bytes[i + 1].is_ascii_digit());
        if !is_num_start {
            i += 1;
            continue;
        }
        let Some((num, end)) = parse_decimal_at(s, i) else {
            i += 1;
            continue;
        };
        let mut j = end;
        while j < lower_bytes.len() && lower_bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let rest = &lower[j..];
        if label_is_credit_unit(rest) && num > 0.0 {
            return Some(num);
        }
        i = end.max(i + 1);
    }
    None
}

/// True when `rest` begins with `credits?`, `aiu`, or `ai\s*credits?` at a
/// label boundary (end of string or non-alphanumeric).
fn label_is_credit_unit(rest: &str) -> bool {
    // Order matters: check `aiu` before bare `ai` prefix, and longer
    // `credits` before `credit`.
    let after = if rest.starts_with("ai credits") {
        Some(10)
    } else if rest.starts_with("ai credit") {
        Some(9)
    } else if rest.starts_with("aiu") {
        Some(3)
    } else if rest.starts_with("credits") {
        Some(7)
    } else if rest.starts_with("credit") {
        Some(6)
    } else if rest.starts_with("ai") {
        // `ai` + whitespace + credits? (regex: `ai\s*credits?`)
        let mut k = 2;
        let b = rest.as_bytes();
        // Require at least one separator unless already matched "ai credit(s)" above.
        if k < b.len() && b[k].is_ascii_whitespace() {
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            let tail = &rest[k..];
            if tail.starts_with("credits") {
                Some(k + 7)
            } else if tail.starts_with("credit") {
                Some(k + 6)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    let Some(end) = after else {
        return false;
    };
    is_label_boundary(&rest[end..])
}

fn is_label_boundary(rest: &str) -> bool {
    match rest.chars().next() {
        None => true,
        Some(c) => !c.is_ascii_alphanumeric() && c != '_',
    }
}

/// Parse a non-negative decimal number starting at byte index `start`.
/// Returns `(value, end_byte_index)`.
fn parse_decimal_at(s: &str, start: usize) -> Option<(f64, usize)> {
    let bytes = s.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    let mut i = start;
    let mut seen_digit = false;
    let mut seen_dot = false;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch.is_ascii_digit() {
            seen_digit = true;
            i += 1;
        } else if ch == b'.' && !seen_dot {
            seen_dot = true;
            i += 1;
        } else {
            break;
        }
    }
    if !seen_digit {
        return None;
    }
    let parsed: f64 = s[start..i].parse().ok()?;
    if parsed.is_finite() && parsed >= 0.0 {
        Some((parsed, i))
    } else {
        None
    }
}

/// True when the request is clearly Copilot-originated.
fn is_copilot_request(req: &Value, model_id_raw: Option<&str>) -> bool {
    if model_id_raw.is_some_and(|m| m.starts_with("copilot/")) {
        return true;
    }

    // Common participant/agent metadata paths used by VS Code chat sessions.
    const PARTICIPANT_PATHS: &[&str] = &[
        "/participant",
        "/result/metadata/participant",
        "/response/participant",
        "/response/result/metadata/participant",
        "/response/metadata/participant",
        "/agent",
        "/result/metadata/agent",
        "/response/agent",
        "/response/result/metadata/agent",
        "/response/metadata/agent",
    ];
    for path in PARTICIPANT_PATHS {
        if let Some(value) = req.pointer(path).and_then(Value::as_str) {
            if value.to_ascii_lowercase().contains("copilot") {
                return true;
            }
        }
    }

    false
}

/// Workspace metadata sibling for a VS Code chatSessions JSONL path:
/// `workspaceStorage/{hash}/chatSessions/{uuid}.jsonl` →
/// `workspaceStorage/{hash}/workspace.json`.
pub(crate) fn copilot_vscode_workspace_json_path(jsonl_path: &Path) -> Option<PathBuf> {
    let hash_dir = jsonl_path.parent()?.parent()?;
    Some(hash_dir.join("workspace.json"))
}

fn read_workspace_for_file(jsonl_path: &Path) -> Option<(String, Option<String>)> {
    // Path: workspaceStorage/{hash}/chatSessions/{uuid}.jsonl
    // workspace.json is at: workspaceStorage/{hash}/workspace.json
    let workspace_json = copilot_vscode_workspace_json_path(jsonl_path)?;

    let contents = std::fs::read_to_string(&workspace_json).ok()?;
    let obj: Value = serde_json::from_str(&contents).ok()?;

    let folder = workspace_uri_from_workspace_json(&obj)?;

    let workspace_key = workspace_key_from_folder_uri(folder)?;
    let workspace_label = workspace_label_from_key(&workspace_key);
    Some((workspace_key, workspace_label))
}

/// Resolve a folder/workspace URI string from a VS Code `workspace.json` object.
///
/// Accepts:
/// - string `folder`
/// - string `workspace`
/// - object `workspace` with string `configPath` / `path` / `uri`
fn workspace_uri_from_workspace_json(obj: &Value) -> Option<&str> {
    if let Some(folder) = obj.get("folder").and_then(Value::as_str) {
        return Some(folder);
    }
    match obj.get("workspace") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Object(map)) => map
            .get("configPath")
            .and_then(Value::as_str)
            .or_else(|| map.get("path").and_then(Value::as_str))
            .or_else(|| map.get("uri").and_then(Value::as_str)),
        _ => None,
    }
}

/// Build a stable workspace key from a VS Code `workspace.json` folder URI.
///
/// - `file:` URIs keep the decode + [`normalize_workspace_key`] path.
/// - Non-file scheme URIs (`vscode-remote://`, `vscode-vfs://`, …) are preserved
///   as keys without slash-collapsing (which would destroy the `://` authority).
/// - Bare filesystem paths go through [`normalize_workspace_key`] directly.
fn workspace_key_from_folder_uri(folder: &str) -> Option<String> {
    let trimmed = folder.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("file:") {
        return normalize_workspace_key(&decode_file_uri(trimmed));
    }
    if is_non_file_scheme_uri(trimmed) {
        return preserve_non_file_workspace_uri(trimmed);
    }
    normalize_workspace_key(trimmed)
}

/// True for scheme URIs other than `file:` that carry a `://` authority form,
/// e.g. `vscode-remote://ssh-remote+host/path`.
fn is_non_file_scheme_uri(folder: &str) -> bool {
    let Some(colon) = folder.find(':') else {
        return false;
    };
    if colon == 0 {
        return false;
    }
    let scheme = &folder[..colon];
    if scheme.eq_ignore_ascii_case("file") {
        return false;
    }
    let scheme_ok = scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.');
    scheme_ok && folder[colon + 1..].starts_with("//")
}

/// Preserve a non-file workspace URI as a stable key.
///
/// Percent-decodes the authority/path tail after `://` but never collapses
/// double-slashes, so `vscode-remote://host/path` stays intact (unlike
/// [`normalize_workspace_key`], which would turn `://` into `:/`).
fn preserve_non_file_workspace_uri(folder: &str) -> Option<String> {
    let trimmed = folder.trim();
    if trimmed.is_empty() {
        return None;
    }
    let key = if let Some(idx) = trimmed.find("://") {
        let head = &trimmed[..idx + 3]; // includes "://"
        let tail = percent_decode_path(&trimmed[idx + 3..]);
        format!("{head}{tail}")
    } else {
        percent_decode_path(trimmed)
    };
    // Trim a trailing path slash only when a path segment exists beyond scheme://
    let key = if key.bytes().filter(|&b| b == b'/').count() > 2 {
        key.trim_end_matches('/').to_string()
    } else {
        key
    };
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// Decode a VS Code workspace folder URI into a filesystem-style path string
/// suitable for [`normalize_workspace_key`].
///
/// - strips `file://` or bare `file:`
/// - percent-decodes path segments (`%20` → space, etc.)
/// - Windows drive form: `/C:/Users/...` → `C:/Users/...`
/// - UNC authority form: `file://server/share/repo` → `//server/share/repo`
/// - UNC path form: `file:////server/share/repo` → `//server/share/repo`
fn decode_file_uri(folder: &str) -> String {
    // Detect authority-form UNC before strip: `file://host/...` has no third
    // slash after the scheme (contrast `file:///abs` and `file:////unc`).
    let authority_unc = folder
        .strip_prefix("file://")
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'));

    let without_scheme = if let Some(rest) = folder.strip_prefix("file://") {
        rest
    } else if let Some(rest) = folder.strip_prefix("file:") {
        rest
    } else {
        folder
    };

    let decoded = percent_decode_path(without_scheme);

    // Windows drive URI: file:///C:/Users/... → /C:/Users/... → C:/Users/...
    if decoded.len() >= 3 {
        let bytes = decoded.as_bytes();
        if bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
            && (decoded.len() == 3 || bytes[3] == b'/' || bytes[3] == b'\\')
        {
            return decoded[1..].to_string();
        }
    }

    // `file://server/share/repo` strips to `server/share/repo`; restore `//`.
    // `file:////server/share` already yields `//server/share` — leave as-is.
    if authority_unc && !decoded.starts_with("//") {
        return format!("//{decoded}");
    }

    decoded
}

/// Minimal percent-decoder for path characters. Invalid sequences are left as-is.
fn percent_decode_path(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(byte: u8) -> Option<u8> {
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
    use std::io::Write;

    fn write_jsonl(path: &Path, lines: &[&str]) {
        let mut f = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
    }

    #[test]
    fn parse_kind0_with_requests() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let path = sessions_dir.join(format!("{}.jsonl", uuid));

        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r1","timestamp":1783918304896,"modelId":"copilot/auto","completionTokens":154,"promptTokens":22079,"result":{"metadata":{"promptTokens":22079,"outputTokens":154,"resolvedModel":"gpt-5.3-codex"}}}]}}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert_eq!(m.client, "copilot");
        assert_eq!(m.session_id, uuid);
        assert_eq!(m.model_id, "gpt-5.3-codex");
        assert_eq!(m.timestamp, 1783918304896);
        assert_eq!(m.tokens.input, 22079);
        assert_eq!(m.tokens.output, 154);
        assert_eq!(m.tokens.reasoning, 0);
        assert_eq!(
            m.dedup_key.as_deref(),
            Some(format!("copilot-vscode:{}:1783918304896:r1", uuid).as_str())
        );
    }

    #[test]
    fn distinct_request_ids_same_timestamp_yield_distinct_dedup_keys() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "550e8400-e29b-41d4-a716-446655440099";
        let path = sessions_dir.join(format!("{}.jsonl", uuid));

        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-a","timestamp":1783918304896,"modelId":"copilot/auto","completionTokens":10,"promptTokens":100,"result":{"metadata":{"resolvedModel":"gpt-4o"}}},{"requestId":"r-b","timestamp":1783918304896,"modelId":"copilot/auto","completionTokens":20,"promptTokens":200,"result":{"metadata":{"resolvedModel":"gpt-4o"}}}]}}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some(format!("copilot-vscode:{uuid}:1783918304896:r-a").as_str())
        );
        assert_eq!(
            messages[1].dedup_key.as_deref(),
            Some(format!("copilot-vscode:{uuid}:1783918304896:r-b").as_str())
        );
        assert_ne!(messages[0].dedup_key, messages[1].dedup_key);
    }

    #[test]
    fn parse_kind2_array_append() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "650e8400-e29b-41d4-a716-446655440001";
        let path = sessions_dir.join(format!("{}.jsonl", uuid));

        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[]}}"#,
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r2","timestamp":1783918310000,"modelId":"copilot/auto","completionTokens":200,"promptTokens":5000,"result":{"metadata":{"promptTokens":5000,"outputTokens":200,"resolvedModel":"gpt-5.3-codex","toolCallRounds":[{"thinking":{"tokens":88}},{"thinking":{"tokens":12}}]}}}]}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert_eq!(m.tokens.input, 5000);
        assert_eq!(m.tokens.output, 200);
        assert_eq!(m.tokens.reasoning, 100);
    }

    #[test]
    fn skips_zero_token_requests() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("aaaaaaaa-0000-0000-0000-000000000000.jsonl");

        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r0","timestamp":1000,"modelId":"copilot/auto","completionTokens":0,"promptTokens":0}]}"#,
            ],
        );

        assert!(parse_copilot_vscode_sessions(&[path]).is_empty());
    }

    #[test]
    fn model_fallback_from_model_id() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("bbbbbbbb-0000-0000-0000-000000000000.jsonl");

        // No resolvedModel, only modelId with "copilot/" prefix
        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r3","timestamp":2000,"modelId":"copilot/gpt-4o","completionTokens":50,"promptTokens":300}]}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        // "copilot/" prefix stripped
        assert_eq!(messages[0].model_id, "gpt-4o");
    }

    #[test]
    fn reasoning_tokens_summed_from_tool_call_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("cccccccc-0000-0000-0000-000000000000.jsonl");

        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r4","timestamp":3000,"modelId":"copilot/auto","completionTokens":10,"promptTokens":100,"result":{"metadata":{"resolvedModel":"gpt-5.3-codex","toolCallRounds":[{"thinking":{"tokens":30}},{"thinking":{"tokens":70}}]}}}]}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.reasoning, 100);
    }

    #[test]
    fn non_copilot_model_id_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("dddddddd-0000-0000-0000-000000000000.jsonl");

        // modelId does not start with "copilot/" and no resolvedModel
        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r5","timestamp":4000,"modelId":"some-other-extension/model","completionTokens":50,"promptTokens":300}]}"#,
            ],
        );

        assert!(parse_copilot_vscode_sessions(&[path]).is_empty());
    }

    #[test]
    fn resolved_model_alone_is_not_copilot() {
        // Generic chat providers may emit tokens + resolvedModel without being
        // Copilot. Bare resolvedModel must not pass the marker check.
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("eeeeeeee-0000-0000-0000-000000000000.jsonl");

        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r6","timestamp":5000,"modelId":"other-provider/gpt-4o","completionTokens":50,"promptTokens":300,"result":{"metadata":{"resolvedModel":"gpt-4o","promptTokens":300,"outputTokens":50}}}]}"#,
            ],
        );

        assert!(
            parse_copilot_vscode_sessions(&[path]).is_empty(),
            "resolvedModel-only non-copilot rows must be rejected"
        );
    }

    #[test]
    fn participant_metadata_accepts_copilot_without_model_id_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("ffffffff-0000-0000-0000-000000000000.jsonl");

        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r7","timestamp":6000,"modelId":"auto","completionTokens":10,"promptTokens":100,"result":{"metadata":{"resolvedModel":"gpt-4o","participant":"github.copilot.default"}}}]}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "gpt-4o");
    }

    #[test]
    fn decode_file_uri_windows_drive_and_percent_encoded() {
        assert_eq!(
            decode_file_uri("file:///C:/Users/alice/repo"),
            "C:/Users/alice/repo"
        );
        assert_eq!(
            decode_file_uri("file:///Users/alice/My%20Project"),
            "/Users/alice/My Project"
        );
        assert_eq!(
            decode_file_uri("file:///Users/alice/repo"),
            "/Users/alice/repo"
        );
        // bare file: prefix
        assert_eq!(decode_file_uri("file:/Users/alice/repo"), "/Users/alice/repo");
        // UNC path form kept after decode
        assert_eq!(
            decode_file_uri("file:////server/share/repo"),
            "//server/share/repo"
        );
        // UNC authority form: host is authority, restore leading //
        assert_eq!(
            decode_file_uri("file://server/share/repo"),
            "//server/share/repo"
        );
        assert_eq!(
            decode_file_uri("file://fileserver/projects/My%20Repo"),
            "//fileserver/projects/My Repo"
        );
    }

    #[test]
    fn workspace_json_file_uri_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let hash_dir = dir.path().join("workspaceStorage").join("hash1");
        let sessions_dir = hash_dir.join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("aaaaaaaa-1111-0000-0000-000000000000.jsonl");

        std::fs::write(
            hash_dir.join("workspace.json"),
            br#"{"folder":"file:///C:/Users/alice/My%20Project"}"#,
        )
        .unwrap();

        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r8","timestamp":7000,"modelId":"copilot/auto","completionTokens":5,"promptTokens":50,"result":{"metadata":{"resolvedModel":"gpt-4o"}}}]}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].workspace_key.as_deref(),
            Some("C:/Users/alice/My Project")
        );
        assert_eq!(
            messages[0].workspace_label.as_deref(),
            Some("My Project")
        );
    }

    #[test]
    fn preserve_non_file_scheme_uris_without_collapsing_slashes() {
        assert_eq!(
            workspace_key_from_folder_uri(
                "vscode-remote://ssh-remote+dev.example/home/alice/My%20Repo"
            )
            .as_deref(),
            Some("vscode-remote://ssh-remote+dev.example/home/alice/My Repo")
        );
        assert_eq!(
            workspace_key_from_folder_uri("vscode-vfs://github/org/repo/")
                .as_deref(),
            Some("vscode-vfs://github/org/repo")
        );
        // file: still uses the filesystem decode path
        assert_eq!(
            workspace_key_from_folder_uri("file:///Users/alice/repo").as_deref(),
            Some("/Users/alice/repo")
        );
        // Bare path still normalizes
        assert_eq!(
            workspace_key_from_folder_uri("/Users/alice//repo/").as_deref(),
            Some("/Users/alice/repo")
        );
    }

    #[test]
    fn workspace_json_vscode_remote_uri_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let hash_dir = dir.path().join("workspaceStorage").join("hash-remote");
        let sessions_dir = hash_dir.join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("bbbbbbbb-2222-0000-0000-000000000000.jsonl");

        std::fs::write(
            hash_dir.join("workspace.json"),
            br#"{"folder":"vscode-remote://ssh-remote+dev.example/home/alice/My%20Repo"}"#,
        )
        .unwrap();

        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r-remote","timestamp":8000,"modelId":"copilot/auto","completionTokens":5,"promptTokens":50,"result":{"metadata":{"resolvedModel":"gpt-4o"}}}]}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].workspace_key.as_deref(),
            Some("vscode-remote://ssh-remote+dev.example/home/alice/My Repo")
        );
        assert_eq!(messages[0].workspace_label.as_deref(), Some("My Repo"));
    }

    #[test]
    fn kind0_stub_plus_index_and_nested_patches_yield_tokens() {
        // Real chatSessions often start with an empty kind:0 snapshot and fill
        // requests via kind:2 path patches (index set + nested field merges).
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("cccccccc-3333-0000-0000-000000000000.jsonl");

        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[]}}"#,
                r#"{"kind":2,"k":["requests",0],"v":{"requestId":"r-patch","timestamp":9000,"modelId":"copilot/auto"}}"#,
                r#"{"kind":2,"k":["requests",0,"promptTokens"],"v":120}"#,
                r#"{"kind":2,"k":["requests",0,"completionTokens"],"v":40}"#,
                r#"{"kind":2,"k":["requests",0,"result","metadata","resolvedModel"],"v":"gpt-4o"}"#,
                r#"{"kind":2,"k":["requests",0,"result","metadata","promptTokens"],"v":120}"#,
                r#"{"kind":2,"k":["requests",0,"result","metadata","outputTokens"],"v":40}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert_eq!(m.tokens.input, 120);
        assert_eq!(m.tokens.output, 40);
        assert_eq!(m.model_id, "gpt-4o");
        assert_eq!(m.timestamp, 9000);
    }

    #[test]
    fn kind0_stub_plus_kind1_request_path_patches_yield_tokens() {
        // Some chatSessions emit kind:1 set-at-path patches under requests
        // (same shape as kind:2). Ignoring kind:1 would leave zero usage after
        // a kind:0 stub.
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("cccccccc-kind1-0000-0000-000000000001.jsonl");

        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-kind1","timestamp":9500,"modelId":"copilot/auto"}]}}"#,
                r#"{"kind":1,"k":["requests",0,"promptTokens"],"v":80}"#,
                r#"{"kind":1,"k":["requests",0,"completionTokens"],"v":30}"#,
                r#"{"kind":1,"k":["requests",0,"result","metadata","resolvedModel"],"v":"gpt-4o"}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert_eq!(m.tokens.input, 80);
        assert_eq!(m.tokens.output, 30);
        assert_eq!(m.model_id, "gpt-4o");
        assert_eq!(m.timestamp, 9500);
    }

    #[test]
    fn full_requests_array_replace_overwrites_prior_snapshot() {
        // k == ["requests"] is a full array replace, not an append.
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("dddddddd-4444-0000-0000-000000000000.jsonl");

        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"old","timestamp":1000,"modelId":"copilot/auto","completionTokens":1,"promptTokens":1}]}}"#,
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"new","timestamp":2000,"modelId":"copilot/gpt-4o","completionTokens":25,"promptTokens":75}]}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 75);
        assert_eq!(messages[0].tokens.output, 25);
        assert_eq!(messages[0].timestamp, 2000);
        assert_eq!(messages[0].model_id, "gpt-4o");
    }

    #[test]
    fn nano_aiu_marks_provider_reported_cost() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("eeeeeeee-5555-0000-0000-000000000000.jsonl");

        // 1_500_000_000 nano AIU => 1.5 AI credits => $0.015 USD.
        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r-aiu","timestamp":10000,"modelId":"copilot/auto","completionTokens":10,"promptTokens":100,"nanoAiu":1500000000,"result":{"metadata":{"resolvedModel":"gpt-4o"}}}]}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert!((m.cost - 0.015).abs() < 1e-12, "cost={}", m.cost);
        assert!(m.has_authoritative_cost());
        assert_eq!(m.cost_source, crate::CostSource::ProviderReported);
    }

    #[test]
    fn one_billion_nano_aiu_is_one_cent_usd() {
        // Hermetic unit: 1e9 nano = 1 AI credit = $0.01.
        assert!(
            (crate::sessions::copilot::copilot_aiu_nano_to_usd(1_000_000_000) - 0.01).abs()
                < 1e-15
        );
    }

    #[test]
    fn details_multiplier_is_not_billed_cost() {
        // `GPT 5.4 • 2x` must not parse model version 5.4 (or 2) as cost.
        assert!(parse_credit_string_to_usd("GPT 5.4 • 2x").is_none());

        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("ffffffff-7777-0000-0000-000000000001.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r-mult","timestamp":12000,"modelId":"copilot/auto","completionTokens":10,"promptTokens":100,"result":{"details":"GPT 5.4 • 2x","metadata":{"resolvedModel":"gpt-5.4"}}}]}"#,
            ],
        );
        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].cost, 0.0);
        assert!(!messages[0].has_authoritative_cost());
    }

    #[test]
    fn details_credits_label_converts_to_usd() {
        // 16.3 credits * $0.01 = $0.163; model version 5.4 must not win.
        assert!(
            (parse_credit_string_to_usd("GPT 5.4 • 16.3 credits").unwrap() - 0.163).abs() < 1e-12
        );
        assert!((parse_credit_string_to_usd("$1.25").unwrap() - 1.25).abs() < 1e-12);
        // Bare number / unlabeled: no provider cost.
        assert!(parse_credit_string_to_usd("16.3").is_none());

        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("ffffffff-8888-0000-0000-000000000002.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r-cr","timestamp":13000,"modelId":"copilot/auto","completionTokens":10,"promptTokens":100,"result":{"details":"GPT 5.4 • 16.3 credits","metadata":{"resolvedModel":"gpt-5.4"}}}]}"#,
            ],
        );
        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert!((m.cost - 0.163).abs() < 1e-12, "cost={}", m.cost);
        assert!(m.has_authoritative_cost());
        assert_eq!(m.cost_source, crate::CostSource::ProviderReported);
    }

    #[test]
    fn workspace_json_object_form_config_path() {
        let dir = tempfile::tempdir().unwrap();
        let hash_dir = dir.path().join("workspaceStorage").join("hash-ws-obj");
        let sessions_dir = hash_dir.join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("ffffffff-6666-0000-0000-000000000000.jsonl");

        std::fs::write(
            hash_dir.join("workspace.json"),
            br#"{"workspace":{"configPath":"file:///Users/alice/proj.code-workspace"}}"#,
        )
        .unwrap();

        write_jsonl(
            &path,
            &[
                r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r-ws","timestamp":11000,"modelId":"copilot/auto","completionTokens":5,"promptTokens":50,"result":{"metadata":{"resolvedModel":"gpt-4o"}}}]}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].workspace_key.as_deref(),
            Some("/Users/alice/proj.code-workspace")
        );
        assert_eq!(
            messages[0].workspace_label.as_deref(),
            Some("proj.code-workspace")
        );
    }

    #[test]
    fn legacy_full_session_json_parses_requests() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "aaaaaaaa-legacy-0000-0000-000000000001";
        let path = sessions_dir.join(format!("{}.json", uuid));

        std::fs::write(
            &path,
            r#"{"version":1,"requests":[{"requestId":"r-legacy","timestamp":1782909300000,"modelId":"copilot/gpt-4o","completionTokens":15,"promptTokens":60,"result":{"metadata":{"promptTokens":60,"outputTokens":15,"resolvedModel":"gpt-4o"}}}]}"#,
        )
        .unwrap();

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, uuid);
        assert_eq!(messages[0].tokens.input, 60);
        assert_eq!(messages[0].tokens.output, 15);
        assert_eq!(messages[0].model_id, "gpt-4o");
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some(format!("copilot-vscode:{}:1782909300000:r-legacy", uuid).as_str())
        );
    }

    #[test]
    fn legacy_json_without_requests_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("not-a-session.json");
        std::fs::write(&path, r#"{"version":1,"settings":{"theme":"dark"}}"#).unwrap();
        assert!(parse_copilot_vscode_sessions(&[path]).is_empty());
    }

    #[test]
    fn cache_read_tokens_normalized_out_of_input() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("bbbbbbbb-cache-0000-0000-000000000002.jsonl");

        // promptTokens includes cacheRead (OTEL-style); normalize_input_tokens
        // nets cache_read out of input and preserves the cache bucket.
        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-cache","timestamp":1782909300000,"modelId":"copilot/gpt-4o","completionTokens":20,"promptTokens":100,"cacheReadTokens":30,"cacheWriteTokens":5,"result":{"metadata":{"promptTokens":100,"outputTokens":20,"resolvedModel":"gpt-4o"}}}]}}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert_eq!(m.tokens.input, 70, "input must net cache_read: 100-30");
        assert_eq!(m.tokens.output, 20);
        assert_eq!(m.tokens.cache_read, 30);
        assert_eq!(m.tokens.cache_write, 5);
    }

    /// After kind:1/2 deep-set of `k:["requests",N,"response"]`, usage lives
    /// under `response.result` (not root / `result`). Must still yield non-zero
    /// tokens and resolved model.
    #[test]
    fn usage_under_response_result_after_response_deep_set() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("cccccccc-resp-0000-0000-000000000003.jsonl");

        // Stub request with only modelId at root; tokens + resolved model arrive
        // solely under response.result via a deep-set patch.
        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-resp","timestamp":1782909300000,"modelId":"copilot/auto"}]}}"#,
                r#"{"kind":2,"k":["requests",0,"response"],"v":{"result":{"metadata":{"promptTokens":90,"outputTokens":25,"resolvedModel":"gpt-4o"}}}}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(
            messages.len(),
            1,
            "usage under response.result must produce a message: {messages:?}"
        );
        let m = &messages[0];
        assert_eq!(m.tokens.input, 90);
        assert_eq!(m.tokens.output, 25);
        assert_eq!(m.model_id, "gpt-4o");
        assert!(
            m.dedup_key
                .as_deref()
                .is_some_and(|k| k.contains("r-resp")),
            "dedup_key {:?}",
            m.dedup_key
        );
    }

    /// Root placeholder zeros must not shadow nonzero `/response/result` usage.
    /// kind:0 stubs often ship `promptTokens:0` / `completionTokens:0` before a
    /// response deep-set patches real counts under `response.result.metadata`.
    #[test]
    fn zero_placeholder_root_tokens_do_not_shadow_response_result() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("cccccccc-zero-0000-0000-000000000004.jsonl");

        write_jsonl(
            &path,
            &[
                // Root zeros present; real usage only under response.result.
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-zero","timestamp":0,"modelId":"copilot/auto","promptTokens":0,"completionTokens":0}]}}"#,
                r#"{"kind":2,"k":["requests",0,"response"],"v":{"timestamp":1782909300000,"result":{"metadata":{"promptTokens":120,"outputTokens":40,"resolvedModel":"gpt-4o"}}}}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(
            messages.len(),
            1,
            "root zeros must not suppress response.result usage: {messages:?}"
        );
        let m = &messages[0];
        assert_eq!(m.tokens.input, 120);
        assert_eq!(m.tokens.output, 40);
        assert_eq!(m.timestamp, 1782909300000, "prefer positive response timestamp over root 0");
        assert_eq!(m.model_id, "gpt-4o");
    }

    /// Root `timestamp: 0` must not shadow a real `/response/result/timestamp`.
    #[test]
    fn response_result_timestamp_preferred_over_root_zero() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("cccccccc-rts-0000-0000-000000000007.jsonl");

        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-rts","timestamp":0,"modelId":"copilot/auto","promptTokens":0,"completionTokens":0}]}}"#,
                r#"{"kind":2,"k":["requests",0,"response"],"v":{"result":{"timestamp":1782909300000,"metadata":{"promptTokens":55,"outputTokens":12,"resolvedModel":"gpt-4o"}}}}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1, "expected one message: {messages:?}");
        assert_eq!(
            messages[0].timestamp, 1782909300000,
            "prefer /response/result/timestamp over root 0"
        );
        assert_eq!(messages[0].tokens.input, 55);
        assert_eq!(messages[0].tokens.output, 12);
    }

    /// `copilotUsageNanoAiu` under `response` / `response.result` /
    /// `response.metadata` marks provider-reported USD (parallel to root/result).
    #[test]
    fn response_wrapped_copilot_usage_nano_aiu_is_provider_cost() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // 1.5e9 nano AIU = 1.5 AI credits = $0.015 under response.result.
        let path = sessions_dir.join("cccccccc-nano-0000-0000-000000000008.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-nano","timestamp":1782909300000,"modelId":"copilot/auto","promptTokens":10,"completionTokens":5}]}}"#,
                r#"{"kind":2,"k":["requests",0,"response"],"v":{"result":{"copilotUsageNanoAiu":1500000000,"metadata":{"promptTokens":10,"outputTokens":5,"resolvedModel":"gpt-4o"}}}}"#,
            ],
        );
        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        assert!(
            (messages[0].cost - 0.015).abs() < 1e-12,
            "1.5e9 nano → $0.015, got {}",
            messages[0].cost
        );
        assert!(messages[0].has_authoritative_cost());
        assert_eq!(messages[0].cost_source, crate::CostSource::ProviderReported);

        // Parallel under response.metadata.copilotUsageNanoAiu.
        let path2 = sessions_dir.join("cccccccc-nano-0000-0000-000000000009.jsonl");
        write_jsonl(
            &path2,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-nano2","timestamp":1782909300000,"modelId":"copilot/auto","promptTokens":10,"completionTokens":5,"response":{"metadata":{"copilotUsageNanoAiu":2000000000,"resolvedModel":"gpt-4o"}}}]}}"#,
            ],
        );
        let messages2 = parse_copilot_vscode_sessions(&[path2]);
        assert_eq!(messages2.len(), 1);
        assert!(
            (messages2[0].cost - 0.02).abs() < 1e-12,
            "2e9 nano → $0.02, got {}",
            messages2[0].cost
        );
        assert!(messages2[0].has_authoritative_cost());
    }

    /// `/response/result/metadata/cache_creation_tokens` must yield non-zero
    /// cache_write after kind:1/2 response deep-set.
    #[test]
    fn response_result_metadata_cache_creation_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("cccccccc-ccache-0000-0000-00000000000a.jsonl");

        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-ccache","timestamp":1782909300000,"modelId":"copilot/gpt-4o","promptTokens":0,"completionTokens":0}]}}"#,
                r#"{"kind":2,"k":["requests",0,"response"],"v":{"result":{"metadata":{"promptTokens":100,"outputTokens":20,"cache_creation_tokens":7,"resolvedModel":"gpt-4o"}}}}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1, "expected message: {messages:?}");
        assert_eq!(
            messages[0].tokens.cache_write, 7,
            "cache_creation_tokens under response.result.metadata must be non-zero"
        );
        assert_eq!(messages[0].tokens.output, 20);
    }

    /// `billedCredit` / `billed_credit` under `response.result.details` (and
    /// `response.details`) must mark provider-reported USD cost, same as
    /// `result.details.billedCredit`.
    #[test]
    fn response_result_details_billed_credit_is_provider_cost() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join("chatSessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("cccccccc-bill-0000-0000-000000000005.jsonl");

        // 2.5 credits × $0.01 = $0.025 under response.result.details.billedCredit.
        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-bill","timestamp":1782909300000,"modelId":"copilot/auto","promptTokens":10,"completionTokens":5}]}}"#,
                r#"{"kind":2,"k":["requests",0,"response"],"v":{"result":{"details":{"billedCredit":2.5},"metadata":{"promptTokens":10,"outputTokens":5,"resolvedModel":"gpt-4o"}}}}"#,
            ],
        );

        let messages = parse_copilot_vscode_sessions(&[path]);
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert!(
            (m.cost - 0.025).abs() < 1e-12,
            "billedCredit 2.5 credits → $0.025, got {}",
            m.cost
        );
        assert!(m.has_authoritative_cost());
        assert_eq!(m.cost_source, crate::CostSource::ProviderReported);

        // Parallel snake_case under response.details.billed_credit.
        let path2 = sessions_dir.join("cccccccc-bill-0000-0000-000000000006.jsonl");
        write_jsonl(
            &path2,
            &[
                r#"{"kind":0,"v":{"requests":[{"requestId":"r-bill2","timestamp":1782909300000,"modelId":"copilot/auto","promptTokens":10,"completionTokens":5,"response":{"details":{"billed_credit":3.0},"metadata":{"resolvedModel":"gpt-4o"}}}]}}"#,
            ],
        );
        let messages2 = parse_copilot_vscode_sessions(&[path2]);
        assert_eq!(messages2.len(), 1);
        assert!(
            (messages2[0].cost - 0.03).abs() < 1e-12,
            "billed_credit 3.0 → $0.03, got {}",
            messages2[0].cost
        );
        assert!(messages2[0].has_authoritative_cost());
    }
}
