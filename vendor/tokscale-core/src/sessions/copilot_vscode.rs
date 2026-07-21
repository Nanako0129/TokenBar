use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::provider_identity::inferred_provider_from_model;
use crate::TokenBreakdown;
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

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let workspace = read_workspace_for_file(path);

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
                if let Some(arr) = obj.pointer("/v/requests").and_then(Value::as_array) {
                    requests.extend(arr.iter().cloned());
                }
            }
            2 => {
                if let Some(k) = obj.get("k").and_then(Value::as_array) {
                    let is_requests = k
                        .first()
                        .and_then(Value::as_str)
                        .map(|s| s == "requests")
                        .unwrap_or(false);
                    if is_requests {
                        if let Some(arr) = obj.get("v").and_then(Value::as_array) {
                            requests.extend(arr.iter().cloned());
                        }
                    }
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

fn request_to_message(
    req: &Value,
    session_id: &str,
    workspace: &Option<(String, Option<String>)>,
) -> Option<UnifiedMessage> {
    let prompt_tokens = req
        .get("promptTokens")
        .and_then(Value::as_i64)
        .or_else(|| {
            req.pointer("/result/metadata/promptTokens")
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);

    let completion_tokens = req
        .get("completionTokens")
        .and_then(Value::as_i64)
        .or_else(|| {
            req.pointer("/result/metadata/outputTokens")
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);

    if prompt_tokens == 0 && completion_tokens == 0 {
        return None;
    }

    let timestamp_ms = req.get("timestamp").and_then(Value::as_i64).unwrap_or(0);

    let resolved_model = req
        .pointer("/result/metadata/resolvedModel")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let model_id_raw = req
        .get("modelId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

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

    let reasoning_tokens: i64 = req
        .pointer("/result/metadata/toolCallRounds")
        .and_then(Value::as_array)
        .map(|rounds| {
            rounds
                .iter()
                .filter_map(|r| r.pointer("/thinking/tokens").and_then(Value::as_i64))
                .sum()
        })
        .unwrap_or(0);

    let tokens = TokenBreakdown {
        input: prompt_tokens.max(0),
        output: completion_tokens.max(0),
        cache_read: 0,
        cache_write: 0,
        reasoning: reasoning_tokens.max(0),
    };

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

    let mut message = UnifiedMessage::new_with_dedup(
        "copilot",
        model_id,
        provider_id,
        session_id.to_string(),
        timestamp_ms,
        tokens,
        0.0,
        Some(dedup_key),
    );

    if let Some((key, label)) = workspace {
        message.set_workspace(Some(key.clone()), label.clone());
    }

    Some(message)
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
        "/agent",
        "/result/metadata/agent",
        "/response/agent",
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

    let folder = obj
        .get("folder")
        .and_then(Value::as_str)
        .or_else(|| obj.get("workspace").and_then(Value::as_str))?;

    let workspace_key = workspace_key_from_folder_uri(folder)?;
    let workspace_label = workspace_label_from_key(&workspace_key);
    Some((workspace_key, workspace_label))
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
}
