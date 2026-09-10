//! Grok Bot weekly quota, kept separate from Grok Build.
//!
//! This adapter adds a quota source; it does not add a Bot session parser.
//!
//! Prefer the standalone app's active account in
//! `~/Library/Application Support/Grok Bot/sand-secrets.json`. Its access token
//! is encrypted by Electron safeStorage, using "Grok Bot Safe Storage" in the
//! macOS Keychain. Query DashboardService/GetSandUsageStatus with that account
//! and its selected team, just as the desktop app does. Fall back to the
//! Cursor IDE's `state.vscdb` only when no desktop login exists.
//! Credentials are read-only and never logged or persisted by TokenBar.
//! Grok Bot owns token refresh; expired logins produce an actionable error.
//!
//! `TOKENBAR_GROK_BOT_SECRETS` / `TOKENBAR_CURSOR_STATE_VSCDB` override the
//! desktop store / IDE database paths (tests, debugging).

use crate::agent_account_scope::{
    self, AccountScope, AccountScopeError, AuthoritativeIdKind, HistoryScope,
};
use crate::agent_quota_duration::DurationEvidence;
use crate::agent_usage::{
    provider_http_client_builder, read_response_body, AgentIdentity, ProviderCacheBinding,
    ProviderFetchFailure, ResponseReadFailure, TransportErrorFacts, TransportPhase, UsageWindow,
};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const GROK_BOT_USAGE_URL: &str = "https://cursor.com/api/dashboard/get-sand-usage-status";
const GROK_BOT_DESKTOP_USAGE_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetSandUsageStatus";
/// `state.vscdb` can be briefly locked by a running Cursor; wait rather than
/// fail the poll.
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_millis(3000);
pub(crate) const WEEKLY_WINDOW_KEY: &str = "weekly.v1";

#[derive(Debug)]
pub(crate) struct GrokBotData {
    pub identity: Option<AgentIdentity>,
    pub account_scope: Result<AccountScope, AccountScopeError>,
    pub history_scope: Result<HistoryScope, AccountScopeError>,
    pub cache_binding: Option<ProviderCacheBinding>,
    pub windows: Vec<UsageWindow>,
}

#[derive(Clone)]
struct CursorCredentials {
    user_id: String,
    access_token: String,
}

enum GrokBotCredentials {
    Desktop {
        access_token: String,
        team_id: Option<u64>,
    },
    Cursor(CursorCredentials),
}

impl GrokBotCredentials {
    /// The cache binds the exact request credential and every account selector.
    /// The common resolver HMACs this material; no raw token is persisted.
    fn scope_material(&self) -> (&'static str, PathBuf, String) {
        match self {
            Self::Desktop {
                access_token,
                team_id,
            } => (
                "grok-bot-desktop",
                desktop_secrets_path(),
                serde_json::json!([access_token, team_id]).to_string(),
            ),
            Self::Cursor(credentials) => (
                "grok-bot-cursor-dashboard",
                cursor_state_db_path(),
                serde_json::json!([credentials.user_id, credentials.access_token]).to_string(),
            ),
        }
    }

    fn resolve_account_scope(&self) -> Result<AccountScope, AccountScopeError> {
        let (source, path, marker) = self.scope_material();
        let location = agent_account_scope::canonical_file_location(&path, None)?;
        agent_account_scope::resolve_credential("grok-bot", source, &location, marker.as_bytes())
    }

    /// Called only after the server accepts the request. A desktop JWT subject
    /// identifies the authenticated owner; the selected team also scopes usage.
    /// If no owner is available, keep the quota but do not mix durable history
    /// under an installation-wide identity. Token rotation must not split it.
    fn history_owner(&self) -> Option<String> {
        let (owner, team) = match self {
            Self::Desktop {
                access_token,
                team_id,
            } => {
                let payload = access_token.split('.').nth(1)?;
                let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(payload.trim_end_matches('='))
                    .ok()?;
                let claims: Value = serde_json::from_slice(&bytes).ok()?;
                let subject = claims.get("sub")?.as_str()?.trim();
                if subject.is_empty() {
                    return None;
                }
                (subject.to_string(), *team_id)
            }
            Self::Cursor(credentials) => (credentials.user_id.clone(), None),
        };
        // JSON tuple encoding keeps owner/team boundaries unambiguous.
        Some(serde_json::json!([owner, team]).to_string())
    }
}

/// Returns `None` only when neither app has a login. Unreadable desktop auth
/// must surface an error, never silently switch to a different IDE account.
pub(crate) async fn fetch(now: DateTime<Utc>) -> Result<Option<GrokBotData>, ProviderFetchFailure> {
    // Keychain may ask the user for access; do not block the async runtime.
    let loaded = match tokio::task::spawn_blocking(load_credentials).await {
        Ok(result) => result,
        Err(_) => {
            return Err(ProviderFetchFailure::terminal(
                "Could not read the Grok Bot login.",
            ))
        }
    };
    let credentials = match loaded {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(None),
        Err(e) => return Err(ProviderFetchFailure::terminal(e)),
    };
    fetch_with_credentials(credentials, now).await.map(Some)
}

async fn fetch_with_credentials(
    credentials: GrokBotCredentials,
    now: DateTime<Utc>,
) -> Result<GrokBotData, ProviderFetchFailure> {
    let scope = credentials.resolve_account_scope().map_err(|_| {
        ProviderFetchFailure::terminal("Grok Bot account identity could not be verified.")
    })?;
    let binding = Some(ProviderCacheBinding::primary(scope.clone()));
    let client = provider_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| {
            ProviderFetchFailure::terminal("Grok Bot usage client could not be created.")
        })?;

    let response = usage_request(&client, &credentials)
        .send()
        .await
        .map_err(|error| {
            ProviderFetchFailure::from_send_error(
                "Grok Bot usage request failed. Retrying automatically.",
                binding.clone(),
                &error,
            )
        })?;

    let status = response.status().as_u16();
    let body = read_response_body(status, false, || async {
        response.text().await.map_err(|error| {
            TransportErrorFacts::from_reqwest(&error, TransportPhase::ResponseBody)
        })
    })
    .await
    .map_err(|failure| response_failure(failure, &credentials, binding.clone()))?;

    let mut data = map_response(&body, now).map_err(ProviderFetchFailure::terminal)?;
    data.account_scope = Ok(scope);
    data.cache_binding = binding;
    data.history_scope = match credentials.history_owner() {
        Some(owner) => agent_account_scope::resolve_history_scope(
            "grok-bot",
            Some((AuthoritativeIdKind::OpaqueId, &owner)),
        ),
        None => Err(AccountScopeError::NoTrustedEvidence),
    };
    Ok(data)
}

fn response_failure(
    failure: ResponseReadFailure,
    credentials: &GrokBotCredentials,
    binding: Option<ProviderCacheBinding>,
) -> ProviderFetchFailure {
    match failure {
        ResponseReadFailure::Transient(diagnostic) => ProviderFetchFailure::transient(
            "Grok Bot usage request failed. Retrying automatically.",
            binding,
            diagnostic,
        ),
        ResponseReadFailure::Terminal(401 | 403) => {
            let app = match credentials {
                GrokBotCredentials::Desktop { .. } => "Grok Bot",
                GrokBotCredentials::Cursor(_) => "Cursor",
            };
            ProviderFetchFailure::terminal(format!(
                "{app} login expired. Open {app} and sign in again, then refresh."
            ))
        }
        ResponseReadFailure::Terminal(status) => {
            ProviderFetchFailure::terminal(format!("Grok Bot usage API returned {status}."))
        }
    }
}

fn usage_request(
    client: &reqwest::Client,
    credentials: &GrokBotCredentials,
) -> reqwest::RequestBuilder {
    let request = match credentials {
        GrokBotCredentials::Desktop {
            access_token,
            team_id,
        } => {
            let mut request = client
                .post(GROK_BOT_DESKTOP_USAGE_URL)
                .bearer_auth(access_token)
                .header("connect-protocol-version", "1")
                .header("x-cursor-client-type", "sand")
                .header("x-ghost-mode", "true");
            if let Some(team_id) = team_id {
                request = request.header("x-cursor-team-id", team_id.to_string());
            }
            request
        }
        GrokBotCredentials::Cursor(credentials) => client
            .post(GROK_BOT_USAGE_URL)
            .header(reqwest::header::ORIGIN, "https://cursor.com")
            .header(reqwest::header::REFERER, "https://cursor.com/dashboard")
            .header(
                reqwest::header::COOKIE,
                format!(
                    "WorkosCursorSessionToken={}%3A%3A{}",
                    credentials.user_id, credentials.access_token
                ),
            ),
    };
    request
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, "TokenBar")
        .body("{}")
}

/// Pure response mapping, split out so the endpoint contract is unit-testable
/// without network or credentials. Accepts the dashboard's camelCase and
/// snake_case shapes (both observed in the wild).
pub(crate) fn map_response(body: &str, now: DateTime<Utc>) -> Result<GrokBotData, String> {
    let payload: Value = serde_json::from_str(body)
        .map_err(|_| "Grok Bot usage response could not be decoded.".to_string())?;
    let obj = payload
        .as_object()
        .ok_or_else(|| "Cursor returned an unexpected response.".to_string())?;

    if obj
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .is_some()
    {
        return Err("Grok Bot usage is unavailable. Open Grok Bot, then refresh.".to_string());
    }
    if obj
        .get("usesPooledEnterpriseAllowance")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Err(
            "Grok Bot uses a pooled team allowance; no individual weekly quota is available."
                .to_string(),
        );
    }

    let used = first_f64(obj, &["usagePercent", "usage_percent"])
        .ok_or_else(|| "Cursor omitted the Grok Bot usage percentage.".to_string())?;
    if !used.is_finite() || !(0.0..=100.0).contains(&used) {
        return Err("Cursor returned an invalid Grok Bot usage percentage.".to_string());
    }

    let reset = first_timestamp(obj, &["nextResetTimestampUtc", "next_reset_timestamp_utc"])
        .ok_or_else(|| "Cursor omitted the Grok Bot reset time.".to_string())?;
    let start = first_timestamp(obj, &["currentPeriodStart", "current_period_start"]);
    let duration = start
        .map(|start| DurationEvidence::provider(reset.timestamp(), (reset - start).num_seconds()));

    Ok(GrokBotData {
        account_scope: Err(AccountScopeError::NoTrustedEvidence),
        history_scope: Err(AccountScopeError::NoTrustedEvidence),
        cache_binding: None,
        identity: obj
            .get("grokPlanLabel")
            .and_then(Value::as_str)
            .filter(|plan| !plan.is_empty())
            .map(|plan| AgentIdentity {
                email: None,
                plan: Some(plan.to_string()),
            }),
        windows: vec![UsageWindow::from_used_percent(
            "Weekly".to_string(),
            used,
            Some(reset),
            now,
            None,
        )
        .with_identity(
            WEEKLY_WINDOW_KEY,
            Some(WEEKLY_WINDOW_KEY.to_string()),
            duration,
            None,
        )],
    })
}

fn first_f64(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_f64))
}

/// Accept ISO-8601 strings and epoch seconds-or-milliseconds (number or
/// numeric string) — the dashboard has sent all three shapes.
fn first_timestamp(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<DateTime<Utc>> {
    keys.iter().find_map(|k| parse_timestamp(obj.get(*k)?))
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(n) => {
            let secs = n.as_f64()?;
            let secs = if secs > 100_000_000_000.0 {
                secs / 1000.0
            } else {
                secs
            };
            DateTime::from_timestamp(secs as i64, 0)
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Ok(secs) = trimmed.parse::<f64>() {
                let secs = if secs > 100_000_000_000.0 {
                    secs / 1000.0
                } else {
                    secs
                };
                if let Some(dt) = DateTime::from_timestamp(secs as i64, 0) {
                    return Some(dt);
                }
            }
            DateTime::parse_from_rfc3339(trimmed)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        }
        _ => None,
    }
}

fn load_credentials() -> Result<Option<GrokBotCredentials>, String> {
    #[cfg(target_os = "macos")]
    let mut key = None;
    load_credentials_from_sources(
        &desktop_secrets_path(),
        &cursor_state_db_path(),
        |ciphertext| {
            #[cfg(target_os = "macos")]
            {
                if key.is_none() {
                    key = Some(crate::macos_safe_storage::Key::from_keychain(
                        "Grok Bot Safe Storage",
                    )?);
                }
                key.as_ref().unwrap().decrypt(ciphertext)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = ciphertext;
                Err("Grok Bot desktop login is only supported on macOS.".to_string())
            }
        },
    )
}

fn load_credentials_from_sources(
    desktop_path: &Path,
    cursor_path: &Path,
    decrypt: impl FnMut(&[u8]) -> Result<String, String>,
) -> Result<Option<GrokBotCredentials>, String> {
    if let Some(credentials) = load_desktop_credentials_from(desktop_path, decrypt)? {
        return Ok(Some(credentials));
    }
    load_credentials_from(cursor_path).map(|value| value.map(GrokBotCredentials::Cursor))
}

fn desktop_secrets_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TOKENBAR_GROK_BOT_SECRETS").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/Application Support/Grok Bot/sand-secrets.json")
}

fn load_desktop_credentials_from(
    path: &Path,
    mut decrypt: impl FnMut(&[u8]) -> Result<String, String>,
) -> Result<Option<GrokBotCredentials>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(
                "Could not read Grok Bot login data. Open Grok Bot, then refresh.".to_string(),
            )
        }
    };
    let malformed =
        || "Grok Bot login data is invalid. Open Grok Bot and sign in again.".to_string();
    let root: Value = serde_json::from_str(&raw).map_err(|_| malformed())?;
    let root = root.as_object().ok_or_else(malformed)?;
    // Current versions keep a JSON-encoded account map. Only the active
    // account may supply credentials; never pick an arbitrary saved account.
    let accounts: Option<Value> = root
        .get("cursor-accounts")
        .map(|value| {
            let encoded = value.as_str().ok_or_else(malformed)?;
            serde_json::from_str(encoded).map_err(|_| malformed())
        })
        .transpose()?;
    let active = match accounts.as_ref() {
        Some(record) => {
            let map = record
                .get("accounts")
                .and_then(Value::as_object)
                .ok_or_else(malformed)?;
            match record.get("active") {
                Some(Value::Null) => None,
                Some(Value::String(id)) => Some(
                    map.get(id)
                        .and_then(Value::as_object)
                        .ok_or_else(malformed)?,
                ),
                _ => return Err(malformed()),
            }
        }
        None => None,
    };
    // Legacy stores have no account map. Once a map exists, a signed-out or
    // incomplete active account must never resurrect a stale top-level login.
    let login = match (accounts.as_ref(), active) {
        (None, _) => root,
        (Some(_), Some(active)) => active,
        (Some(_), None) => return Ok(None),
    };
    let Some(stored_token) = login.get("cursor-access-token") else {
        return Ok(None);
    };
    let access_token =
        decode_desktop_secret(stored_token.as_str().ok_or_else(malformed)?, &mut decrypt)?;
    if access_token.is_empty() {
        return Err(malformed());
    }
    let team_id = login
        .get("cursor-selected-team-id")
        .map(|value| {
            let team = decode_desktop_secret(value.as_str().ok_or_else(malformed)?, &mut decrypt)?;
            team.parse::<u64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(malformed)
        })
        .transpose()?;
    Ok(Some(GrokBotCredentials::Desktop {
        access_token,
        team_id,
    }))
}

fn decode_desktop_secret(
    value: &str,
    decrypt: &mut impl FnMut(&[u8]) -> Result<String, String>,
) -> Result<String, String> {
    let malformed =
        || "Grok Bot login data could not be decoded. Open Grok Bot and sign in again.".to_string();
    if let Some(encoded) = value.strip_prefix("plaintext:v1:") {
        return String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| malformed())?,
        )
        .map_err(|_| malformed());
    }
    let encoded = if let Some(scoped) = value.strip_prefix("scoped:v1:") {
        let (scope, encoded) = scoped.split_once(':').ok_or_else(malformed)?;
        if scope.len() != 64 || !scope.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(malformed());
        }
        encoded
    } else {
        value
    };
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| malformed())?;
    decrypt(&ciphertext)
}

fn cursor_state_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TOKENBAR_CURSOR_STATE_VSCDB").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .map(|home| home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"))
        .unwrap_or_else(|| PathBuf::from("state.vscdb"))
}

fn load_credentials_from(db_path: &Path) -> Result<Option<CursorCredentials>, String> {
    if !db_path.is_file() {
        return Ok(None);
    }
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| "Could not read the Cursor login database.".to_string())?;
    conn.busy_timeout(SQLITE_BUSY_TIMEOUT)
        .map_err(|_| "Could not read the Cursor login database.".to_string())?;

    let mut stmt = conn
        .prepare(
            "SELECT key, value FROM ItemTable WHERE key IN \
             ('cursorAuth/accessToken','glass.lastSignedInAuthId','cursorAuth/cachedScopedProfile')",
        )
        .map_err(|_| "Could not read the Cursor login database.".to_string())?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|_| "Could not read the Cursor login database.".to_string())?
        .collect::<Result<_, _>>()
        .map_err(|_| "Could not read the Cursor login database.".to_string())?;

    let mut token: Option<String> = None;
    let mut identity: Option<String> = None;
    let mut profile: Option<String> = None;
    for (key, value) in &rows {
        let normalized = normalized_stored_string(value);
        match key.as_str() {
            "cursorAuth/accessToken" => token = normalized,
            "glass.lastSignedInAuthId" => identity = normalized,
            "cursorAuth/cachedScopedProfile" => profile = normalized,
            _ => {}
        }
    }

    let (Some(token), user_id) = (
        token.filter(|t| !t.is_empty()),
        identity
            .as_deref()
            .and_then(extract_user_id)
            .or_else(|| profile.as_deref().and_then(extract_user_id)),
    ) else {
        return Ok(None);
    };
    let Some(user_id) = user_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(CursorCredentials {
        user_id,
        access_token: token,
    }))
}

/// Values in `state.vscdb` are sometimes JSON-encoded strings (wrapped in an
/// extra layer of quotes) — unwrap one layer when present, else use as-is.
fn normalized_stored_string(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    if value.starts_with('"') {
        if let Ok(Value::String(inner)) = serde_json::from_str(value) {
            return if inner.is_empty() { None } else { Some(inner) };
        }
    }
    Some(value.to_string())
}

/// Cursor user ids look like `user_...` (20+ alphanumerics). Scan for the
/// first occurrence rather than depending on the surrounding JSON shape.
fn extract_user_id(text: &str) -> Option<String> {
    let start = text.find("user_")?;
    let rest = &text[start + "user_".len()..];
    let len = rest
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric())
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    (len >= 20).then(|| format!("user_{}", &rest[..len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn parses_camel_case_meter() {
        let data = map_response(
            r#"{
                "usagePercent": 22.5,
                "currentPeriodStart": "2026-09-08T15:40:06.727001+00:00",
                "nextResetTimestampUtc": "2026-09-15T15:40:06.727001+00:00",
                "hasNonZeroIncludedLimit": true
            }"#,
            now(),
        )
        .unwrap();
        assert_eq!(data.windows.len(), 1);
        assert_eq!(data.windows[0].label_for_test(), "Weekly");
        // 100 - 22.5 = 77.5 remaining.
        assert!((data.windows[0].remaining_for_test() - 77.5).abs() < 1e-9);
    }

    #[test]
    fn parses_snake_case_meter_with_epoch_millis() {
        let data = map_response(
            r#"{
                "usage_percent": 90,
                "current_period_start": 1788855606727,
                "next_reset_timestamp_utc": 1789460406727
            }"#,
            now(),
        )
        .unwrap();
        assert!((data.windows[0].remaining_for_test() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_out_of_range_percent() {
        for used in [-1.0, 140.0] {
            let body = serde_json::json!({"usagePercent": used,
                "nextResetTimestampUtc": "2026-09-15T15:40:06Z"})
            .to_string();
            assert!(map_response(&body, now()).is_err());
        }
    }

    #[test]
    fn missing_percent_is_an_error() {
        let err = map_response(
            r#"{"nextResetTimestampUtc": "2026-09-15T15:40:06Z"}"#,
            now(),
        )
        .unwrap_err();
        assert!(err.contains("usage percentage"), "unexpected: {err}");
    }

    #[test]
    fn missing_reset_is_an_error() {
        let err = map_response(r#"{"usagePercent": 10.0}"#, now()).unwrap_err();
        assert!(err.contains("reset time"), "unexpected: {err}");
    }

    #[test]
    fn remote_error_is_bounded() {
        let err = map_response(r#"{"error": "private-response-canary"}"#, now()).unwrap_err();
        assert_eq!(
            err,
            "Grok Bot usage is unavailable. Open Grok Bot, then refresh."
        );
    }

    #[test]
    fn non_object_body_is_an_error() {
        assert!(map_response("[1,2]", now()).is_err());
    }

    #[test]
    fn normalizes_quoted_and_plain_values() {
        assert_eq!(
            normalized_stored_string("\"abc123\"").as_deref(),
            Some("abc123")
        );
        assert_eq!(normalized_stored_string("plain").as_deref(), Some("plain"));
        assert_eq!(normalized_stored_string(""), None);
        assert_eq!(normalized_stored_string("\"\""), None);
    }

    #[test]
    fn extracts_user_id_from_identity_and_profile_shapes() {
        let id = "user_abcDEF1234567890xyzAB";
        assert_eq!(
            extract_user_id(&format!("glass-{id}-suffix")).as_deref(),
            Some(id)
        );
        assert_eq!(
            extract_user_id(&format!("{{\"id\":\"{id}\",\"x\":1}}")).as_deref(),
            Some(id)
        );
        assert_eq!(extract_user_id("user_short"), None);
        assert_eq!(extract_user_id("no id here"), None);
    }

    /// Write `rows` to a fresh temp `state.vscdb` and return (dir, path).
    fn temp_state_db(tag: &str, rows: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "tb_grokbot_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.vscdb");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
            .unwrap();
        for (key, value) in rows {
            conn.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )
            .unwrap();
        }
        drop(conn);
        (dir, path)
    }

    #[test]
    fn missing_db_yields_no_credentials() {
        let dir = std::env::temp_dir().join(format!(
            "tb_grokbot_missing_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let loaded = load_credentials_from(&dir.join("state.vscdb")).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn loads_token_and_user_id() {
        let id = "user_abcDEF1234567890xyzAB";
        let (dir, path) = temp_state_db(
            "ok",
            &[
                ("cursorAuth/accessToken", "\"tok-123\""),
                ("glass.lastSignedInAuthId", &format!("glass-{id}")),
            ],
        );
        let creds = load_credentials_from(&path)
            .unwrap()
            .expect("credentials load");
        assert_eq!(creds.access_token, "tok-123");
        assert_eq!(creds.user_id, id);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn falls_back_to_profile_for_user_id() {
        let id = "user_abcDEF1234567890xyzAB";
        let (dir, path) = temp_state_db(
            "profile",
            &[
                ("cursorAuth/accessToken", "tok-plain"),
                (
                    "cursorAuth/cachedScopedProfile",
                    &format!("{{\"userId\":\"{id}\"}}"),
                ),
            ],
        );
        let creds = load_credentials_from(&path)
            .unwrap()
            .expect("credentials load");
        assert_eq!(creds.user_id, id);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_without_user_id_yields_no_credentials() {
        let (dir, path) = temp_state_db("noid", &[("cursorAuth/accessToken", "tok-123")]);
        assert!(load_credentials_from(&path).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn encoded(value: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(value)
    }

    #[test]
    fn standalone_active_account_loads_without_an_ide_login() {
        let (dir, cursor_path) = temp_state_db("desktop", &[]);
        let desktop_path = dir.join("sand-secrets.json");
        let accounts = serde_json::json!({
            "active": "current-account",
            "accounts": {
                "old-account": { "cursor-access-token": encoded(b"wrong-account") },
                "current-account": {
                    "cursor-access-token": encoded(b"active-token"),
                    "cursor-selected-team-id": encoded(b"active-team"),
                    "cursor-refresh-token": "must-never-read-this"
                }
            }
        });
        std::fs::write(
            &desktop_path,
            serde_json::json!({
                "cursor-accounts": accounts.to_string(),
                "cursor-access-token": encoded(b"stale-legacy-token")
            })
            .to_string(),
        )
        .unwrap();
        let credentials =
            load_credentials_from_sources(&desktop_path, &cursor_path, |data| match data {
                b"active-token" => Ok("desktop-test-token".to_string()),
                b"active-team" => Ok("42".to_string()),
                _ => panic!("must read only the active access token and its team"),
            })
            .unwrap()
            .expect("standalone login must supply quota credentials");
        let request = usage_request(&reqwest::Client::new(), &credentials)
            .build()
            .unwrap();
        assert_eq!(request.url().as_str(), GROK_BOT_DESKTOP_USAGE_URL);
        assert_eq!(
            request.headers()["authorization"],
            "Bearer desktop-test-token"
        );
        assert_eq!(request.headers()["x-cursor-team-id"], "42");
        assert!(!request.headers().contains_key("cookie"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn standalone_login_takes_precedence_and_decryption_failure_is_visible() {
        let (dir, cursor_path) = temp_state_db(
            "desktop_priority",
            &[
                ("cursorAuth/accessToken", "ide-token"),
                ("glass.lastSignedInAuthId", "user_abcDEF1234567890xyzAB"),
            ],
        );
        let desktop_path = dir.join("sand-secrets.json");
        std::fs::write(
            &desktop_path,
            serde_json::json!({
                "cursor-access-token": encoded(b"native-token")
            })
            .to_string(),
        )
        .unwrap();
        let credentials =
            load_credentials_from_sources(
                &desktop_path,
                &cursor_path,
                |_| Ok("native".to_string()),
            )
            .unwrap()
            .unwrap();
        assert!(matches!(credentials, GrokBotCredentials::Desktop { .. }));
        let error = load_credentials_from_sources(&desktop_path, &cursor_path, |_| {
            Err("Keychain denied".to_string())
        })
        .err()
        .unwrap();
        assert_eq!(error, "Keychain denied");
        std::fs::write(&desktop_path, "invalid JSON").unwrap();
        assert!(
            load_credentials_from_sources(&desktop_path, &cursor_path, |_| unreachable!()).is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_desktop_login_preserves_cursor_fallback() {
        let (dir, cursor_path) = temp_state_db(
            "fallback",
            &[
                ("cursorAuth/accessToken", "ide-token"),
                ("glass.lastSignedInAuthId", "user_abcDEF1234567890xyzAB"),
            ],
        );
        let credentials = load_credentials_from_sources(
            &dir.join("missing.json"),
            &cursor_path,
            |_| unreachable!(),
        )
        .unwrap()
        .unwrap();
        let request = usage_request(&reqwest::Client::new(), &credentials)
            .build()
            .unwrap();
        assert_eq!(request.url().as_str(), GROK_BOT_USAGE_URL);
        assert!(request.headers().contains_key("cookie"));
        assert!(!request.headers().contains_key("authorization"));
        assert!(!request.headers().contains_key("x-cursor-team-id"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn signed_out_desktop_does_not_select_an_inactive_account() {
        let (dir, cursor_path) = temp_state_db("signed_out", &[]);
        let path = dir.join("sand-secrets.json");
        let accounts = serde_json::json!({"active": null, "accounts": {
            "previous-account": {"cursor-access-token": encoded(b"previous-token")}
        }});
        std::fs::write(
            &path,
            serde_json::json!({"cursor-accounts": accounts.to_string()}).to_string(),
        )
        .unwrap();
        assert!(
            load_credentials_from_sources(&path, &cursor_path, |_| panic!(
                "signed-out tokens must not be decrypted"
            ))
            .unwrap()
            .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn desktop_secret_formats_and_invalid_team() {
        let ciphertext = encoded(b"encrypted-token");
        let scoped = format!("scoped:v1:{}:{ciphertext}", "a".repeat(64));
        assert_eq!(
            decode_desktop_secret(&scoped, &mut |data| {
                assert_eq!(data, b"encrypted-token");
                Ok("decoded-token".to_string())
            })
            .unwrap(),
            "decoded-token"
        );
        assert_eq!(
            decode_desktop_secret(
                &format!("plaintext:v1:{}", encoded(b"dev-token")),
                &mut |_| unreachable!()
            )
            .unwrap(),
            "dev-token"
        );
        assert!(decode_desktop_secret("scoped:v1:invalid:abc", &mut |_| unreachable!()).is_err());

        let (dir, _) = temp_state_db("invalid_team", &[]);
        let path = dir.join("sand-secrets.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "cursor-access-token": format!("plaintext:v1:{}", encoded(b"token")),
                "cursor-selected-team-id": format!("plaintext:v1:{}", encoded(b"not-a-team"))
            })
            .to_string(),
        )
        .unwrap();
        assert!(load_desktop_credentials_from(&path, |_| unreachable!()).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn current_account_map_never_resurrects_legacy_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sand-secrets.json");
        for active in [Value::Null, Value::String("current".to_string())] {
            let accounts = serde_json::json!({"active": active, "accounts": {"current": {}}});
            std::fs::write(
                &path,
                serde_json::json!({
                    "cursor-accounts": accounts.to_string(),
                    "cursor-access-token": encoded(b"stale-legacy-token")
                })
                .to_string(),
            )
            .unwrap();
            assert!(
                load_desktop_credentials_from(&path, |_| panic!("stale login"))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn weekly_window_has_stable_identity_and_provider_duration() {
        let data = map_response(
            r#"{
            "usagePercent": 30,
            "currentPeriodStart": "2026-09-08T12:00:00Z",
            "nextResetTimestampUtc": "2026-09-15T12:00:00Z"
        }"#,
            now(),
        )
        .unwrap();
        let window = serde_json::to_value(&data.windows[0]).unwrap();
        assert_eq!(window["cardId"], "weekly.v1");
        assert_eq!(window["paceStatus"]["windowKey"], "weekly.v1");
        assert_eq!(window["paceStatus"]["durationSeconds"], 604800);
        assert_eq!(window["paceStatus"]["durationSource"], "provider");
        assert_eq!(window["paceStatus"]["state"], "learningHistory");
        let data = map_response(
            r#"{
            "usagePercent": 30, "nextResetTimestampUtc": "2026-09-15T12:00:00Z"
        }"#,
            now(),
        )
        .unwrap();
        let window = serde_json::to_value(&data.windows[0]).unwrap();
        assert_eq!(window["paceStatus"]["state"], "learningDuration");
    }

    fn desktop_token(subject: &str, signature: &str, team_id: Option<u64>) -> GrokBotCredentials {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::json!({"sub": subject}).to_string());
        GrokBotCredentials::Desktop {
            access_token: format!("header.{payload}.{signature}"),
            team_id,
        }
    }

    #[test]
    fn cache_binding_tracks_request_but_history_survives_token_rotation() {
        use crate::agent_account_scope::{test_support::TestRefreshScope, RefreshScopeTransaction};
        let resolver = TestRefreshScope::new("grok-bot", "grokbot-owner");
        let a = desktop_token("user-a", "credential-a", None);
        let rotated = desktop_token("user-a", "credential-b", None);
        let b = desktop_token("user-b", "credential-b", None);
        let team = desktop_token("user-a", "credential-a", Some(42));
        let resolve = |credentials: &GrokBotCredentials| {
            let (source, _, marker) = credentials.scope_material();
            resolver
                .resolve_current(source, "fixture-login", marker.as_bytes())
                .unwrap()
        };
        let a_scope = resolve(&a);
        assert_eq!(a_scope, resolve(&a));
        for other in [&rotated, &b, &team] {
            assert_ne!(a_scope, resolve(other));
        }
        let history = |credentials: &GrokBotCredentials| {
            let owner = credentials.history_owner().unwrap();
            resolver
                .resolve_history("grok-bot", Some((AuthoritativeIdKind::OpaqueId, &owner)))
                .unwrap()
        };
        assert_eq!(history(&a), history(&rotated));
        assert_ne!(history(&a), history(&b));
        assert_ne!(history(&a), history(&team));
        let metadata = String::from_utf8(resolver.metadata_bytes()).unwrap();
        for private in ["user-a", "credential-a", "fixture-login"] {
            assert!(!metadata.contains(private));
        }
        assert!(GrokBotCredentials::Desktop {
            access_token: "opaque-token".to_string(),
            team_id: None
        }
        .history_owner()
        .is_none());
        resolver.cleanup();
    }

    #[tokio::test]
    async fn http_failure_classification_precedes_body_and_keeps_request_binding() {
        let credentials = desktop_token("user-a", "credential-a", Some(42));
        let binding = Some(ProviderCacheBinding::primary(AccountScope::for_test(
            "account-a-team-42",
        )));
        for status in [401, 403, 404, 429, 500, 503] {
            let failure = read_response_body(status, false, || async {
                panic!("must not read error body")
            })
            .await
            .unwrap_err();
            match response_failure(failure, &credentials, binding.clone()) {
                ProviderFetchFailure::Transient {
                    attempt_binding, ..
                } => {
                    assert!(status == 429 || status >= 500);
                    assert_eq!(attempt_binding, binding);
                }
                ProviderFetchFailure::Terminal { display } => {
                    assert!([401, 403, 404].contains(&status));
                    if status != 404 {
                        assert!(display.contains("Open Grok Bot and sign in again"));
                    }
                }
            }
        }
    }

    #[test]
    fn native_meter_includes_plan_and_does_not_misreport_a_pooled_allowance() {
        let data = map_response(
            r#"{
            "usagePercent": 25, "nextResetTimestampUtc": "2026-09-15T15:40:06Z",
            "grokPlanLabel": "SuperGrok"
        }"#,
            now(),
        )
        .unwrap();
        assert_eq!(data.identity.unwrap().plan.as_deref(), Some("SuperGrok"));
        assert!(map_response(
            r#"{
            "usagePercent": 25, "nextResetTimestampUtc": "2026-09-15T15:40:06Z",
            "usesPooledEnterpriseAllowance": true
        }"#,
            now()
        )
        .unwrap_err()
        .contains("pooled team allowance"));
    }
}
