//! Antigravity (Google Code Assist) usage/quota — ported from codexbar's
//! Antigravity provider. Antigravity 2.0 (the IDE and the `antigravity-cli`
//! that replaced `gemini-cli`) does NOT persist per-message token counts
//! locally, so a contribution-graph style breakdown isn't derivable; the quota
//! API is the only usable signal. We mirror codexbar's `auto` source:
//!
//! 1. **Local IDE API (`cli`)** — when Antigravity is running, find its
//!    `language_server` process (carrying a `--csrf_token`), discover its
//!    listening port via `lsof`, and call the local Connect-RPC `GetUserStatus`
//!    over loopback TLS. Live, no token refresh, no disk writes.
//! 2. **OAuth remote (`oauth`)** — otherwise read the shared Google creds at
//!    `~/.gemini/oauth_creds.json`, refresh against Google (client id/secret
//!    scanned from the installed Antigravity.app binary), and hit the
//!    `cloudcode-pa.googleapis.com` Code Assist quota endpoints.
//!
//! Both yield per-model "remaining fraction + reset" which map to `UsageWindow`s.

use crate::agent_account_scope::{
    self, AccountScope, AccountScopeError, AuthoritativeIdKind, RefreshCheckpoint,
    RefreshScopeTransaction,
};
use crate::agent_usage::{clean_plan, parse_datetime, percent_encode, AgentIdentity, UsageWindow};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, value::RawValue, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const LANG_SERVICE: &str = "/exa.language_server_pb.LanguageServerService/GetUserStatus";
const CODE_ASSIST_BASE: &str = "https://cloudcode-pa.googleapis.com/v1internal";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REFRESH_SAFETY_SECS: i64 = 60;

pub(crate) struct Fetched {
    pub source: String,
    pub identity: Option<AgentIdentity>,
    pub account_scope: Result<AccountScope, AccountScopeError>,
    pub windows: Vec<UsageWindow>,
}

/// Auto: prefer the live Local IDE API; fall back to the OAuth remote API.
pub(crate) async fn fetch(now: DateTime<Utc>) -> Result<Fetched, String> {
    match fetch_local_ide(now).await {
        Ok(local) if !local.windows.is_empty() => Ok(local),
        local_result => {
            let local_err = local_result.err();
            match fetch_oauth_remote(now).await {
                Ok(remote) => Ok(remote),
                Err(remote_err) => Err(local_err
                    .map(|le| format!("{remote_err} (local IDE: {le})"))
                    .unwrap_or(remote_err)),
            }
        }
    }
}

// ── Local IDE API ───────────────────────────────────────────────────────────

struct ProcInfo {
    pid: i32,
    csrf_token: String,
    extension_port: Option<u16>,
    extension_csrf: Option<String>,
}

async fn fetch_local_ide(now: DateTime<Utc>) -> Result<Fetched, String> {
    let proc = detect_process()?;
    let ports = listening_ports(proc.pid)?;

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // loopback language_server uses a self-signed cert
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| format!("build Antigravity local client: {e}"))?;
    let body = json!({
        "metadata": {
            "ideName": "antigravity",
            "extensionName": "antigravity",
            "ideVersion": "unknown",
            "locale": "en",
        }
    });

    // language-server ports use the language-server CSRF; the extension server
    // (if advertised) carries its own token.
    let mut candidates: Vec<(u16, String)> = ports
        .iter()
        .map(|p| (*p, proc.csrf_token.clone()))
        .collect();
    if let Some(port) = proc.extension_port {
        if let Some(csrf) = proc.extension_csrf.as_ref() {
            candidates.push((port, csrf.clone()));
        }
        candidates.push((port, proc.csrf_token.clone()));
    }

    let mut last_err = "Antigravity local IDE API not reachable".to_string();
    for (port, csrf) in candidates {
        let url = format!("https://127.0.0.1:{port}{LANG_SERVICE}");
        let resp = client
            .post(&url)
            .header("X-Codeium-Csrf-Token", &csrf)
            .header("Connect-Protocol-Version", "1")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("local request failed: {e}");
                continue;
            }
        };
        if !resp.status().is_success() {
            last_err = format!("local API returned {}", resp.status().as_u16());
            continue;
        }
        let Ok(text) = resp.text().await else {
            continue;
        };
        match parse_user_status(&text, now) {
            Ok(mut fetched) if !fetched.windows.is_empty() || fetched.identity.is_some() => {
                fetched.account_scope = resolve_local_account_scope(fetched.identity.as_ref());
                return Ok(fetched);
            }
            Ok(_) => last_err = "local API returned no model quotas".to_string(),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn detect_process() -> Result<ProcInfo, String> {
    let output = Command::new("/bin/ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
        .map_err(|e| format!("run ps: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut saw_antigravity = false;
    for line in stdout.lines() {
        let line = line.trim_start();
        let Some((pid_str, cmd)) = line.split_once(' ') else {
            continue;
        };
        let Ok(pid) = pid_str.trim().parse::<i32>() else {
            continue;
        };
        let lower = cmd.to_lowercase();
        if !is_language_server(&lower) || !is_antigravity(&lower) {
            continue;
        }
        saw_antigravity = true;
        let Some(csrf) = extract_flag(cmd, "--csrf_token") else {
            continue;
        };
        return Ok(ProcInfo {
            pid,
            csrf_token: csrf,
            extension_port: extract_flag(cmd, "--extension_server_port")
                .and_then(|s| s.parse().ok()),
            extension_csrf: extract_flag(cmd, "--extension_server_csrf_token"),
        });
    }
    if saw_antigravity {
        Err("Antigravity is running but no CSRF token was found".to_string())
    } else {
        Err("Antigravity is not running".to_string())
    }
}

fn is_language_server(lower_cmd: &str) -> bool {
    lower_cmd.contains("language_server")
}

fn is_antigravity(lower_cmd: &str) -> bool {
    (lower_cmd.contains("--app_data_dir") && lower_cmd.contains("antigravity"))
        || lower_cmd.contains("/antigravity/")
}

/// Value of `flag` in a command line, accepting either `flag value` or `flag=value`.
fn extract_flag(cmd: &str, flag: &str) -> Option<String> {
    let idx = cmd.find(flag)?;
    let rest = &cmd[idx + flag.len()..];
    let rest = rest.trim_start_matches(['=', ' ']);
    let value: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    (!value.is_empty()).then_some(value)
}

fn listening_ports(pid: i32) -> Result<Vec<u16>, String> {
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .ok_or_else(|| "lsof not available".to_string())?;
    let output = Command::new(lsof)
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("run lsof: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let ports: BTreeSet<u16> = stdout.lines().filter_map(parse_listen_port).collect();
    if ports.is_empty() {
        Err("no listening ports for Antigravity".to_string())
    } else {
        Ok(ports.into_iter().collect())
    }
}

/// Pull the port out of an `lsof` LISTEN line, e.g. `... TCP 127.0.0.1:54321 (LISTEN)`.
fn parse_listen_port(line: &str) -> Option<u16> {
    let idx = line.find("(LISTEN)")?;
    let before = line[..idx].trim_end();
    let colon = before.rfind(':')?;
    before[colon + 1..].trim().parse().ok()
}

#[derive(Debug, Deserialize)]
struct UserStatusResponse {
    #[serde(rename = "userStatus")]
    user_status: Option<UserStatus>,
}

#[derive(Debug, Deserialize)]
struct UserStatus {
    email: Option<String>,
    #[serde(rename = "planStatus")]
    plan_status: Option<PlanStatus>,
    #[serde(rename = "cascadeModelConfigData")]
    cascade_model_config_data: Option<ModelConfigData>,
    #[serde(rename = "userTier")]
    user_tier: Option<NamedTier>,
}

#[derive(Debug, Deserialize)]
struct PlanStatus {
    #[serde(rename = "planInfo")]
    plan_info: Option<LocalPlanInfo>,
}

#[derive(Debug, Deserialize)]
struct LocalPlanInfo {
    #[serde(rename = "planDisplayName")]
    plan_display_name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "productName")]
    product_name: Option<String>,
    #[serde(rename = "planName")]
    plan_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamedTier {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelConfigData {
    #[serde(rename = "clientModelConfigs")]
    client_model_configs: Option<Vec<Box<RawValue>>>,
}

#[derive(Debug, Deserialize)]
struct AvailableModelsResponse {
    #[serde(default)]
    models: BTreeMap<String, Box<RawValue>>,
}

#[derive(Debug, Deserialize)]
struct QuotaBucketsResponse {
    #[serde(default)]
    buckets: Vec<Box<RawValue>>,
}

#[derive(Debug)]
struct ModelCandidate {
    model_id: Option<String>,
    fraction: f64,
    reset: Option<DateTime<Utc>>,
    source_index: usize,
    label: String,
}

fn valid_remaining_fraction(fraction: f64) -> bool {
    fraction.is_finite() && (0.0..=1.0).contains(&fraction)
}

fn quota_window(
    label: String,
    fraction: f64,
    reset: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    card_id: String,
    window_key: Option<String>,
) -> Option<UsageWindow> {
    UsageWindow::try_from_provider_fraction(label, fraction, reset, now)
        .map(|window| window.with_identity(card_id, window_key, None, None))
}

fn parse_user_status(body: &str, now: DateTime<Utc>) -> Result<Fetched, String> {
    let response: UserStatusResponse =
        serde_json::from_str(body).map_err(|e| format!("decode GetUserStatus: {e}"))?;
    let status = response
        .user_status
        .ok_or_else(|| "GetUserStatus missing userStatus".to_string())?;

    let configs = status
        .cascade_model_config_data
        .and_then(|d| d.client_model_configs)
        .unwrap_or_default();
    let mut selected: BTreeMap<String, ModelCandidate> = BTreeMap::new();
    let mut missing_model = Vec::new();
    for (index, config) in configs.into_iter().enumerate() {
        let Ok(config) = serde_json::from_str::<Value>(config.get()) else {
            continue;
        };
        let Some(quota) = config.get("quotaInfo") else {
            continue;
        };
        let Some(fraction) = quota.get("remainingFraction").and_then(Value::as_f64) else {
            continue;
        };
        let reset = quota
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_datetime);
        let model_id = config
            .pointer("/modelOrAlias/model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string);
        let label = config
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| model_id.clone())
            .unwrap_or_else(|| "Model".to_string());
        let candidate = ModelCandidate {
            model_id: model_id.clone(),
            fraction,
            reset,
            source_index: index,
            label,
        };
        let Some(model_id) = model_id else {
            missing_model.push(candidate);
            continue;
        };
        match selected.get(&model_id) {
            Some(current)
                if !binding_candidate_is_better(
                    candidate.fraction,
                    candidate.reset,
                    candidate.source_index,
                    current.fraction,
                    current.reset,
                    current.source_index,
                    now,
                ) => {}
            _ => {
                selected.insert(model_id, candidate);
            }
        }
    }
    let mut candidates: Vec<ModelCandidate> = selected.into_values().collect();
    candidates.extend(missing_model);
    candidates.sort_by_key(|candidate| candidate.source_index);
    let windows: Vec<UsageWindow> = candidates
        .into_iter()
        .filter_map(|candidate| {
            let (card_id, window_key) = match candidate.model_id {
                Some(model_id) => {
                    let key = format!("model.{model_id}.v1");
                    (key.clone(), Some(key))
                }
                None => (
                    format!("row.cli.config.{}.v1", candidate.source_index),
                    None,
                ),
            };
            quota_window(
                candidate.label,
                candidate.fraction,
                candidate.reset,
                now,
                card_id,
                window_key,
            )
        })
        .collect();

    let plan = status
        .user_tier
        .and_then(|t| t.name)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            status
                .plan_status
                .and_then(|p| p.plan_info)
                .and_then(local_plan_name)
        });

    let email = status.email.filter(|value| !value.trim().is_empty());
    Ok(Fetched {
        source: "cli".to_string(),
        identity: Some(AgentIdentity { email, plan }),
        // Parsing remains pure and hermetic. fetch_local_ide resolves this only
        // after the authenticated loopback response has been accepted.
        account_scope: Err(AccountScopeError::NoTrustedEvidence),
        windows,
    })
}

fn resolve_local_account_scope(
    identity: Option<&AgentIdentity>,
) -> Result<AccountScope, AccountScopeError> {
    let email = identity
        .and_then(|identity| identity.email.as_deref())
        .ok_or(AccountScopeError::NoTrustedEvidence)?;
    agent_account_scope::resolve_authoritative("antigravity", AuthoritativeIdKind::Email, email)
}

fn local_plan_name(info: LocalPlanInfo) -> Option<String> {
    [
        info.plan_display_name,
        info.display_name,
        info.product_name,
        info.plan_name,
    ]
    .into_iter()
    .flatten()
    .map(|s| s.trim().to_string())
    .find(|s| !s.is_empty())
}

// ── OAuth remote (Google Code Assist) ─────────────────────────────────────────

async fn fetch_oauth_remote(now: DateTime<Utc>) -> Result<Fetched, String> {
    let creds_path = gemini_home()
        .map(|home| home.join("oauth_creds.json"))
        .ok_or_else(|| "Could not resolve ~/.gemini".to_string())?;
    let mut creds = load_remote_credentials(&creds_path)?;
    let mut access_token = remote_access_token(&creds)?;
    let mut refreshed_scope = None;

    if remote_credentials_need_refresh(&creds, now) {
        let refreshed = refresh_access_token(&creds_path, now).await?;
        creds = refreshed.0;
        access_token = refreshed.1;
        refreshed_scope = Some(refreshed.2);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build Antigravity client: {e}"))?;

    let code_assist_body = code_assist_post(
        &client,
        "loadCodeAssist",
        &json!({
            "metadata": { "ideType": "ANTIGRAVITY", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI" }
        }),
        &access_token,
    )
    .await?;
    let code_assist: Value = serde_json::from_str(&code_assist_body)
        .map_err(|e| format!("decode Antigravity loadCodeAssist: {e}"))?;
    let project = project_id(&code_assist);
    let plan = resolve_remote_plan(&code_assist);
    let windows = fetch_model_quotas(&client, &access_token, project.as_deref(), now).await?;
    let account_scope =
        refreshed_scope.unwrap_or_else(|| resolve_remote_account_scope(&creds_path, &creds));

    Ok(Fetched {
        source: "oauth".to_string(),
        // google_accounts.active is unrelated local state, not authenticated by
        // the credential that fetched these quotas. It is neither presentation
        // identity nor account-scope evidence for the remote route.
        identity: Some(remote_identity(plan)),
        account_scope,
        windows,
    })
}

fn remote_identity(plan: Option<String>) -> AgentIdentity {
    AgentIdentity { email: None, plan }
}

fn load_remote_credentials(path: &Path) -> Result<Value, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|_| "Antigravity not logged in (no ~/.gemini/oauth_creds.json)".to_string())?;
    serde_json::from_str(&raw).map_err(|e| format!("decode oauth_creds.json: {e}"))
}

fn remote_access_token(creds: &Value) -> Result<String, String> {
    creds
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Antigravity creds have no access token".to_string())
}

fn remote_refresh_marker(creds: &Value) -> Option<&[u8]> {
    creds
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::as_bytes)
}

fn remote_credentials_need_refresh(creds: &Value, now: DateTime<Utc>) -> bool {
    let expiry_ms = creds.get("expiry_date").and_then(Value::as_f64);
    let now_ms = now.timestamp_millis() as f64;
    expiry_ms.is_none_or(|expiry| expiry <= now_ms + (REFRESH_SAFETY_SECS * 1000) as f64)
}

fn remote_scope_location(path: &Path) -> Result<String, AccountScopeError> {
    agent_account_scope::canonical_file_location(path, Some("refresh_token"))
}

fn resolve_remote_account_scope(
    path: &Path,
    creds: &Value,
) -> Result<AccountScope, AccountScopeError> {
    let marker = remote_refresh_marker(creds).ok_or(AccountScopeError::NoTrustedEvidence)?;
    agent_account_scope::resolve_credential(
        "antigravity",
        "google-oauth-creds",
        &remote_scope_location(path)?,
        marker,
    )
}

async fn refresh_access_token(
    creds_path: &Path,
    now: DateTime<Utc>,
) -> Result<(Value, String, Result<AccountScope, AccountScopeError>), String> {
    let refresh = agent_account_scope::begin_refresh("antigravity")
        .map_err(|_| "Antigravity credential refresh lock is unavailable.".to_string())?;
    refresh_access_token_with(
        creds_path,
        now,
        &refresh,
        request_access_token,
        |creds| write_creds_atomic(creds_path, creds),
        |_| Ok(()),
    )
    .await
}

async fn request_access_token(refresh_token: String) -> Result<Value, String> {
    let client = resolve_oauth_client()
        .ok_or_else(|| "Antigravity OAuth client not found. Install Antigravity.app or set ANTIGRAVITY_OAUTH_CLIENT_ID/SECRET.".to_string())?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build refresh client: {e}"))?;
    let form = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        percent_encode(&client.0),
        percent_encode(&client.1),
        percent_encode(&refresh_token),
    );
    let response = http
        .post(GOOGLE_TOKEN_URL)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form)
        .send()
        .await
        .map_err(|e| format!("Antigravity token refresh failed: {e}"))?;
    if !response.status().is_success() {
        return Err("Antigravity token refresh rejected. Re-login in Antigravity.".to_string());
    }
    response
        .json()
        .await
        .map_err(|e| format!("decode refresh response: {e}"))
}

async fn refresh_access_token_with<R, Request, RequestFuture, Save, Checkpoint>(
    creds_path: &Path,
    now: DateTime<Utc>,
    refresh: &R,
    request: Request,
    save: Save,
    mut checkpoint: Checkpoint,
) -> Result<(Value, String, Result<AccountScope, AccountScopeError>), String>
where
    R: RefreshScopeTransaction + ?Sized,
    Request: FnOnce(String) -> RequestFuture,
    RequestFuture: std::future::Future<Output = Result<Value, String>>,
    Save: FnOnce(&Value) -> std::io::Result<()>,
    Checkpoint: FnMut(RefreshCheckpoint) -> Result<(), String>,
{
    let mut creds = load_remote_credentials(creds_path)?;
    checkpoint(RefreshCheckpoint::Reloaded)?;
    let location = remote_scope_location(creds_path)
        .map_err(|_| "Antigravity auth location cannot be scoped safely.".to_string())?;
    if !remote_credentials_need_refresh(&creds, Utc::now()) {
        let access_token = remote_access_token(&creds)?;
        let scope = match remote_refresh_marker(&creds) {
            Some(marker) => refresh.resolve_current("google-oauth-creds", &location, marker),
            None => Err(AccountScopeError::NoTrustedEvidence),
        };
        return Ok((creds, access_token, scope));
    }

    let old_marker = remote_refresh_marker(&creds)
        .ok_or_else(|| "Antigravity access token expired and no refresh token".to_string())?
        .to_vec();
    let refresh_token = std::str::from_utf8(&old_marker)
        .map_err(|_| "Antigravity refresh credential is not valid text.".to_string())?
        .to_string();
    let json = request(refresh_token).await?;
    checkpoint(RefreshCheckpoint::NetworkReturned)?;
    let access_token = json
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "refresh response missing access_token".to_string())?
        .to_string();

    if let Some(obj) = creds.as_object_mut() {
        obj.insert("access_token".into(), Value::String(access_token.clone()));
        if let Some(expires_in) = json.get("expires_in").and_then(Value::as_f64) {
            let expiry = now.timestamp_millis() as f64 + expires_in * 1000.0;
            obj.insert("expiry_date".into(), json!(expiry));
        }
        if let Some(id_token) = json.get("id_token").and_then(Value::as_str) {
            obj.insert("id_token".into(), Value::String(id_token.to_string()));
        }
        if let Some(replacement) = json
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            obj.insert(
                "refresh_token".into(),
                Value::String(replacement.to_string()),
            );
        }
    }
    let new_marker = remote_refresh_marker(&creds);
    let marker_rotated = new_marker.is_some_and(|marker| marker != old_marker.as_slice());
    let scope = match new_marker {
        Some(new_marker) => {
            refresh.transfer("google-oauth-creds", &location, &old_marker, new_marker)
        }
        None => Err(AccountScopeError::NoTrustedEvidence),
    };
    checkpoint(RefreshCheckpoint::MetadataHandled)?;
    // A rotated marker may reach disk only after its lineage transfer is durable.
    // The refreshed access token remains usable in memory for this poll.
    if marker_rotated && scope.is_err() {
        return Ok((creds, access_token, scope));
    }
    if let Err(error) = save(&creds) {
        eprintln!("tb_core_ffi: failed to persist refreshed Antigravity credentials: {error}");
    }
    checkpoint(RefreshCheckpoint::CredentialsPersisted)?;
    Ok((creds, access_token, scope))
}

fn write_creds_atomic(path: &Path, creds: &Value) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    let data = serde_json::to_vec_pretty(creds).map_err(std::io::Error::other)?;
    let directory = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "credential path has no parent",
        )
    })?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = directory.join(format!(
        ".oauth_creds.json.tokenbar.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let staged = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    std::fs::File::open(directory)?.sync_all()
}

async fn code_assist_post(
    client: &reqwest::Client,
    method: &str,
    body: &Value,
    access_token: &str,
) -> Result<String, String> {
    let resp = client
        .post(format!("{CODE_ASSIST_BASE}:{method}"))
        .bearer_auth(access_token)
        .header(reqwest::header::USER_AGENT, "antigravity")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Antigravity {method} request failed: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Antigravity Google auth expired. Re-login in Antigravity.".to_string());
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!("Antigravity {method} permission denied"));
    }
    if !status.is_success() {
        return Err(format!("Antigravity {method} returned {}", status.as_u16()));
    }
    resp.text()
        .await
        .map_err(|e| format!("read Antigravity {method}: {e}"))
}

async fn fetch_model_quotas(
    client: &reqwest::Client,
    access_token: &str,
    project: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Vec<UsageWindow>, String> {
    let body = match project {
        Some(p) => json!({ "project": p }),
        None => json!({}),
    };
    // Primary: fetchAvailableModels (per-model quotaInfo). Fall back to
    // retrieveUserQuota buckets if the catalog endpoint is denied.
    match code_assist_post(client, "fetchAvailableModels", &body, access_token)
        .await
        .and_then(|body| models_from_available(&body, now))
    {
        Ok(windows) if !windows.is_empty() => Ok(windows),
        _ => {
            let quota = code_assist_post(client, "retrieveUserQuota", &body, access_token).await?;
            buckets_from_quota(&quota, now)
        }
    }
}

fn project_id(code_assist: &Value) -> Option<String> {
    match code_assist.get("cloudaicompanionProject") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(Value::Object(obj)) => obj
            .get("value")
            .or_else(|| obj.get("id"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn resolve_remote_plan(code_assist: &Value) -> Option<String> {
    if let Some(plan_type) = code_assist
        .pointer("/planInfo/planType")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(clean_plan(plan_type));
    }
    match code_assist
        .pointer("/currentTier/id")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        Some("standard-tier") => Some("Paid".to_string()),
        Some("free-tier") => Some("Free".to_string()),
        Some("legacy-tier") => Some("Legacy".to_string()),
        _ => code_assist
            .pointer("/currentTier/name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

fn models_from_available(body: &str, now: DateTime<Utc>) -> Result<Vec<UsageWindow>, String> {
    let response: AvailableModelsResponse = serde_json::from_str(body)
        .map_err(|e| format!("decode Antigravity fetchAvailableModels: {e}"))?;
    let mut selected: BTreeMap<String, ModelCandidate> = BTreeMap::new();
    let mut missing_model = Vec::new();
    for (source_index, (raw_id, model)) in response.models.into_iter().enumerate() {
        let Ok(model) = serde_json::from_str::<Value>(model.get()) else {
            continue;
        };
        let Some(quota) = model.get("quotaInfo") else {
            continue;
        };
        let Some(fraction) = quota.get("remainingFraction").and_then(Value::as_f64) else {
            continue;
        };
        let reset = quota
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_datetime);
        let model_id = raw_id.trim().to_string();
        let model_id = (!model_id.is_empty()).then_some(model_id);
        let label = model
            .get("displayName")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                model
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or(raw_id.as_str())
            .to_string();
        let candidate = ModelCandidate {
            model_id: model_id.clone(),
            fraction,
            reset,
            source_index,
            label,
        };
        let Some(model_id) = model_id else {
            missing_model.push(candidate);
            continue;
        };
        match selected.get(&model_id) {
            Some(current)
                if !binding_candidate_is_better(
                    candidate.fraction,
                    candidate.reset,
                    candidate.source_index,
                    current.fraction,
                    current.reset,
                    current.source_index,
                    now,
                ) => {}
            _ => {
                selected.insert(model_id, candidate);
            }
        }
    }

    let mut candidates: Vec<ModelCandidate> = selected.into_values().collect();
    candidates.extend(missing_model);
    candidates.sort_by_key(|candidate| candidate.source_index);
    Ok(candidates
        .into_iter()
        .filter_map(|candidate| {
            let (card_id, window_key) = match candidate.model_id {
                Some(model_id) => {
                    let key = format!("model.{model_id}.v1");
                    (key.clone(), Some(key))
                }
                None => (format!("row.models.{}.v1", candidate.source_index), None),
            };
            quota_window(
                candidate.label,
                candidate.fraction,
                candidate.reset,
                now,
                card_id,
                window_key,
            )
        })
        .collect())
}

#[derive(Debug)]
struct QuotaBucketCandidate {
    model_id: Option<String>,
    fraction: f64,
    reset: Option<DateTime<Utc>>,
    source_index: usize,
}

fn buckets_from_quota(body: &str, now: DateTime<Utc>) -> Result<Vec<UsageWindow>, String> {
    let response: QuotaBucketsResponse = serde_json::from_str(body)
        .map_err(|e| format!("decode Antigravity retrieveUserQuota: {e}"))?;
    let mut selected: BTreeMap<String, QuotaBucketCandidate> = BTreeMap::new();
    let mut missing = Vec::new();
    for (source_index, bucket) in response.buckets.into_iter().enumerate() {
        let Ok(bucket) = serde_json::from_str::<Value>(bucket.get()) else {
            continue;
        };
        let Some(fraction) = bucket.get("remainingFraction").and_then(Value::as_f64) else {
            continue;
        };
        let reset = bucket
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_datetime);
        let model_id = bucket
            .get("modelId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string);
        let candidate = QuotaBucketCandidate {
            model_id: model_id.clone(),
            fraction,
            reset,
            source_index,
        };
        let Some(model_id) = model_id else {
            missing.push(candidate);
            continue;
        };
        match selected.get(&model_id) {
            Some(current) if !bucket_candidate_is_better(&candidate, current, now) => {}
            _ => {
                selected.insert(model_id, candidate);
            }
        }
    }

    let mut chosen: Vec<QuotaBucketCandidate> = selected.into_values().collect();
    chosen.extend(missing);
    chosen.sort_by_key(|candidate| candidate.source_index);
    Ok(chosen
        .into_iter()
        .filter_map(|candidate| {
            let label = candidate
                .model_id
                .clone()
                .unwrap_or_else(|| "Model".to_string());
            let (card_id, window_key) = match candidate.model_id {
                Some(model_id) => {
                    let key = format!("model.{model_id}.v1");
                    (key.clone(), Some(key))
                }
                None => (
                    format!("row.quota.bucket.{}.v1", candidate.source_index),
                    None,
                ),
            };
            quota_window(
                label,
                candidate.fraction,
                candidate.reset,
                now,
                card_id,
                window_key,
            )
        })
        .collect())
}

fn bucket_candidate_is_better(
    candidate: &QuotaBucketCandidate,
    current: &QuotaBucketCandidate,
    now: DateTime<Utc>,
) -> bool {
    binding_candidate_is_better(
        candidate.fraction,
        candidate.reset,
        candidate.source_index,
        current.fraction,
        current.reset,
        current.source_index,
        now,
    )
}

fn binding_candidate_is_better(
    candidate_fraction: f64,
    candidate_reset: Option<DateTime<Utc>>,
    candidate_index: usize,
    current_fraction: f64,
    current_reset: Option<DateTime<Utc>>,
    current_index: usize,
    now: DateTime<Utc>,
) -> bool {
    match (
        valid_remaining_fraction(candidate_fraction),
        valid_remaining_fraction(current_fraction),
    ) {
        (true, false) => return true,
        (false, true) => return false,
        _ => {}
    }
    match candidate_fraction.total_cmp(&current_fraction) {
        std::cmp::Ordering::Less => return true,
        std::cmp::Ordering::Greater => return false,
        std::cmp::Ordering::Equal => {}
    }
    let candidate_reset = candidate_reset.filter(|reset| *reset > now);
    let current_reset = current_reset.filter(|reset| *reset > now);
    match (candidate_reset, current_reset) {
        (Some(candidate), Some(current)) if candidate != current => return candidate < current,
        (Some(_), None) => return true,
        (None, Some(_)) => return false,
        _ => {}
    }
    candidate_index < current_index
}

// ── OAuth client discovery (scan installed Antigravity.app) ───────────────────

fn resolve_oauth_client() -> Option<(String, String)> {
    if let (Ok(id), Ok(secret)) = (
        std::env::var("ANTIGRAVITY_OAUTH_CLIENT_ID"),
        std::env::var("ANTIGRAVITY_OAUTH_CLIENT_SECRET"),
    ) {
        let (id, secret) = (id.trim().to_string(), secret.trim().to_string());
        if !id.is_empty() && !secret.is_empty() {
            return Some((id, secret));
        }
    }
    static CACHE: OnceLock<Option<(String, String)>> = OnceLock::new();
    CACHE.get_or_init(discover_client_from_app).clone()
}

fn discover_client_from_app() -> Option<(String, String)> {
    for path in client_artifact_candidates() {
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let ids = scan_client_ids(&data);
        let secrets = scan_client_secrets(&data);
        if let Some(client) = preferred_client(&ids, &secrets) {
            return Some(client);
        }
    }
    None
}

fn client_artifact_candidates() -> Vec<PathBuf> {
    let relative = [
        "Contents/Resources/bin/language_server",
        "Contents/Resources/bin/language_server_macos",
        "Contents/Resources/app/extensions/antigravity/bin/language_server_macos_arm",
        "Contents/Resources/app/extensions/antigravity/bin/language_server_macos_x64",
        "Contents/Resources/app/extensions/antigravity/bin/language_server_macos",
        "Contents/Resources/app/out/main.js",
    ];
    let mut roots = vec![PathBuf::from("/Applications/Antigravity.app")];
    if let Some(home) = crate::user_home_dir() {
        roots.push(home.join("Applications/Antigravity.app"));
    }
    roots
        .iter()
        .flat_map(|root| relative.iter().map(move |r| root.join(r)))
        .collect()
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn scan_client_ids(data: &[u8]) -> Vec<String> {
    let suffix = b".apps.googleusercontent.com";
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(pos) = find_sub(&data[i..], suffix) {
        let end = i + pos + suffix.len();
        let mut start = i + pos;
        while start > 0 && is_token_byte(data[start - 1]) {
            start -= 1;
        }
        if let Ok(candidate) = std::str::from_utf8(&data[start..end]) {
            if valid_client_id(candidate) && !out.contains(&candidate.to_string()) {
                out.push(candidate.to_string());
            }
        }
        i = i + pos + suffix.len();
    }
    out
}

fn valid_client_id(s: &str) -> bool {
    s.ends_with(".apps.googleusercontent.com")
        && s.split_once('-')
            .is_some_and(|(head, _)| !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()))
}

fn scan_client_secrets(data: &[u8]) -> Vec<String> {
    let prefix = b"GOCSPX-";
    let total = prefix.len() + 28;
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(pos) = find_sub(&data[i..], prefix) {
        let abs = i + pos;
        if abs + total <= data.len() {
            let candidate = &data[abs..abs + total];
            if candidate[prefix.len()..].iter().all(|b| is_token_byte(*b)) {
                if let Ok(s) = std::str::from_utf8(candidate) {
                    if !out.contains(&s.to_string()) {
                        out.push(s.to_string());
                    }
                }
            }
        }
        i = abs + prefix.len();
    }
    out
}

/// codexbar's pairing heuristic for the (possibly multiple) ids/secrets baked
/// into the language_server binary.
fn preferred_client(ids: &[String], secrets: &[String]) -> Option<(String, String)> {
    if ids.is_empty() || secrets.is_empty() {
        return None;
    }
    if secrets.len() == 1 && ids.len() > 1 {
        return Some((ids[ids.len() - 1].clone(), secrets[0].clone()));
    }
    let secret = if secrets.len() == ids.len() && secrets.len() > 1 {
        secrets[secrets.len() - 1].clone()
    } else {
        secrets[0].clone()
    };
    Some((ids[0].clone(), secret))
}

// ── shared ────────────────────────────────────────────────────────────────────

fn gemini_home() -> Option<PathBuf> {
    crate::user_home_dir().map(|home| home.join(".gemini"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_account_scope::test_support::TestRefreshScope;

    #[test]
    fn extracts_flags_both_forms() {
        let cmd = "/x/language_server --app_data_dir /Users/me/.gemini/antigravity --csrf_token=ABC123 --extension_server_port 4567";
        assert_eq!(extract_flag(cmd, "--csrf_token").as_deref(), Some("ABC123"));
        assert_eq!(
            extract_flag(cmd, "--extension_server_port").as_deref(),
            Some("4567")
        );
        assert!(is_language_server(&cmd.to_lowercase()));
        assert!(is_antigravity(&cmd.to_lowercase()));
    }

    #[test]
    fn parses_lsof_listen_port() {
        let line = "language_ 123 nanako 30u IPv4 0x0 0t0 TCP 127.0.0.1:54321 (LISTEN)";
        assert_eq!(parse_listen_port(line), Some(54321));
        assert_eq!(parse_listen_port("... (ESTABLISHED)"), None);
    }

    #[test]
    fn scans_and_pairs_oauth_client_from_bytes() {
        let blob = b"junk\x00123-abcDEF_g.apps.googleusercontent.com\x00\x00GOCSPX-abcdefghijklmnopqrstuvwxyz12\x00tail";
        let ids = scan_client_ids(blob);
        let secrets = scan_client_secrets(blob);
        assert_eq!(
            ids,
            vec!["123-abcDEF_g.apps.googleusercontent.com".to_string()]
        );
        assert_eq!(secrets.len(), 1);
        let client = preferred_client(&ids, &secrets).unwrap();
        assert_eq!(client.0, "123-abcDEF_g.apps.googleusercontent.com");
        assert!(client.1.starts_with("GOCSPX-"));
    }

    #[test]
    fn prefers_last_id_when_single_secret() {
        let ids = vec![
            "1-a.apps.googleusercontent.com".into(),
            "2-b.apps.googleusercontent.com".into(),
        ];
        let secrets = vec!["GOCSPX-only".into()];
        assert_eq!(
            preferred_client(&ids, &secrets).unwrap().0,
            "2-b.apps.googleusercontent.com"
        );
    }

    #[test]
    fn parses_local_user_status_quotas() {
        let now = Utc::now();
        let body = r#"{
            "userStatus": {
                "email": "me@gmail.com",
                "userTier": { "name": "Pro" },
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        { "label": "Gemini 3 Pro", "modelOrAlias": {"model":"gemini-3-pro"},
                          "quotaInfo": { "remainingFraction": 0.42, "resetTime": "2026-06-09T00:00:00Z" } },
                        { "label": "No Quota", "modelOrAlias": {"model":"x"} }
                    ]
                }
            }
        }"#;
        let fetched = parse_user_status(body, now).unwrap();
        assert_eq!(fetched.source, "cli");
        assert_eq!(
            fetched.identity.as_ref().unwrap().email.as_deref(),
            Some("me@gmail.com")
        );
        assert_eq!(
            fetched.identity.as_ref().unwrap().plan.as_deref(),
            Some("Pro")
        );
        assert_eq!(fetched.windows.len(), 1);
        assert_eq!(fetched.windows[0].label_for_test(), "Gemini 3 Pro");
        assert!((fetched.windows[0].remaining_for_test() - 42.0).abs() < 0.01);
    }

    #[test]
    fn maps_available_models_and_quota_buckets() {
        let now = Utc::now();
        let models = json!({
            "models": {
                "gemini-3-pro": { "displayName": "Gemini 3 Pro", "quotaInfo": { "remainingFraction": 0.5 } }
            }
        });
        let w = models_from_available(&models.to_string(), now).unwrap();
        assert_eq!(w.len(), 1);

        let quota = json!({
            "buckets": [
                { "modelId": "claude", "remainingFraction": 0.8 },
                { "modelId": "claude", "remainingFraction": 0.3 }
            ]
        });
        let b = buckets_from_quota(&quota.to_string(), now).unwrap();
        assert_eq!(b.len(), 1);
        assert!((b[0].remaining_for_test() - 30.0).abs() < 0.01); // lowest kept
    }

    #[test]
    fn stage4_antigravity_identity_and_duplicate_rules_are_deterministic() {
        let now = DateTime::parse_from_rfc3339("2026-07-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let models = json!({
            "models": {
                "model-A": {
                    "displayName": "Trimmed loser",
                    "quotaInfo": { "remainingFraction": 0.5, "resetTime": "2026-07-12T00:00:00Z" }
                },
                "  model-A  ": {
                    "displayName": "Trimmed winner",
                    "quotaInfo": { "remainingFraction": 0.2, "resetTime": "2026-07-13T00:00:00Z" }
                },
                "model-B": {
                    "displayName": "Shared label",
                    "quotaInfo": { "remainingFraction": 0.4, "resetTime": "2026-07-12T00:00:00Z" }
                },
                "Model-Byte-Case": {
                    "displayName": "Shared label",
                    "quotaInfo": { "remainingFraction": 0.3, "resetTime": "2026-07-13T00:00:00Z" }
                }
            }
        });
        let windows = models_from_available(&models.to_string(), now).unwrap();
        assert_eq!(windows.len(), 3);
        let model_a = windows
            .iter()
            .find(|window| window.pace_window_key_for_test() == Some("model.model-A.v1"))
            .unwrap();
        assert_eq!(model_a.label_for_test(), "Trimmed winner");
        assert!((model_a.remaining_for_test() - 20.0).abs() < 0.01);
        assert_eq!(
            windows
                .iter()
                .filter(|window| window.label_for_test() == "Shared label")
                .count(),
            2,
            "display labels never merge distinct model IDs"
        );
        assert!(windows.iter().any(|window| {
            window.pace_window_key_for_test() == Some("model.Model-Byte-Case.v1")
        }));

        let cli = r#"{
            "userStatus": {
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "label": "CLI loser",
                            "modelOrAlias": { "model": " Model-X " },
                            "quotaInfo": { "remainingFraction": 0.6, "resetTime": "2026-07-12T00:00:00Z" }
                        },
                        {
                            "label": "CLI winner",
                            "modelOrAlias": { "model": "Model-X" },
                            "quotaInfo": { "remainingFraction": 0.2, "resetTime": "2026-07-13T00:00:00Z" }
                        },
                        { "label": "Config only", "quotaInfo": { "remainingFraction": 0.7 } }
                    ]
                }
            }
        }"#;
        let fetched = parse_user_status(cli, now).unwrap();
        assert_eq!(fetched.windows.len(), 2);
        assert_eq!(
            fetched.windows[0].pace_window_key_for_test(),
            Some("model.Model-X.v1")
        );
        assert_eq!(fetched.windows[0].label_for_test(), "CLI winner");
        let missing_wire = serde_json::to_value(&fetched.windows[1]).unwrap();
        assert_eq!(missing_wire["cardId"], "row.cli.config.2.v1");
        assert_eq!(missing_wire["paceStatus"]["reason"], "windowIdentity");

        let missing_remote = models_from_available(
            &json!({
                "models": {
                    "   ": {
                        "displayName": "Remote model",
                        "quotaInfo": { "remainingFraction": 0.7 }
                    }
                }
            })
            .to_string(),
            now,
        )
        .unwrap();
        let wire = serde_json::to_value(&missing_remote[0]).unwrap();
        assert_eq!(wire["cardId"], "row.models.0.v1");
        assert_eq!(wire["paceStatus"]["reason"], "windowIdentity");

        let missing_bucket = buckets_from_quota(
            &json!({
                "buckets": [
                    { "modelId": "   ", "remainingFraction": 0.7 }
                ]
            })
            .to_string(),
            now,
        )
        .unwrap();
        let wire = serde_json::to_value(&missing_bucket[0]).unwrap();
        assert_eq!(wire["cardId"], "row.quota.bucket.0.v1");
        assert_eq!(wire["paceStatus"]["reason"], "windowIdentity");

        let duplicate = json!({
            "buckets": [
                {
                    "modelId": "same-model",
                    "remainingFraction": 0.25,
                    "resetTime": "2026-07-09T00:00:00Z"
                },
                {
                    "modelId": "same-model",
                    "remainingFraction": 0.25,
                    "resetTime": "2026-07-12T00:00:00Z"
                },
                {
                    "modelId": "same-model",
                    "remainingFraction": 0.25,
                    "resetTime": "2026-07-11T00:00:00Z"
                }
            ]
        });
        let selected = buckets_from_quota(&duplicate.to_string(), now).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0].resets_at_for_test(),
            Some("2026-07-11T00:00:00.000Z"),
            "future reset beats past reset, then earliest future reset wins"
        );
        let same_reset = parse_datetime("2026-07-11T00:00:00Z");
        assert!(!binding_candidate_is_better(
            0.25, same_reset, 1, 0.25, same_reset, 0, now
        ));
    }

    #[test]
    fn stage4_antigravity_rejects_invalid_fractions_at_every_source() {
        assert!(!valid_remaining_fraction(f64::NAN));
        assert!(!valid_remaining_fraction(f64::INFINITY));
        assert!(!valid_remaining_fraction(-0.01));
        assert!(!valid_remaining_fraction(1.01));
        assert!(valid_remaining_fraction(0.0));
        assert!(valid_remaining_fraction(1.0));

        let now = DateTime::parse_from_rfc3339("2026-07-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let assert_rows = |windows: Vec<UsageWindow>| {
            assert_eq!(windows.len(), 1);
            assert_eq!(
                windows[0].pace_window_key_for_test(),
                Some("model.valid.v1")
            );
            for key in ["model.negative.v1", "model.over.v1"] {
                assert!(
                    windows
                        .iter()
                        .all(|window| window.pace_window_key_for_test() != Some(key)),
                    "invalid quota row must not be published: {key}"
                );
            }
        };

        let cli = r#"{
            "userStatus": {
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "modelOrAlias": { "model": "valid" },
                            "quotaInfo": { "remainingFraction": 0.5 }
                        },
                        {
                            "modelOrAlias": { "model": "negative" },
                            "quotaInfo": { "remainingFraction": -0.1 }
                        },
                        {
                            "modelOrAlias": { "model": "over" },
                            "quotaInfo": { "remainingFraction": 1.1 }
                        },
                        {
                            "modelOrAlias": { "model": "missing" },
                            "quotaInfo": {}
                        }
                    ]
                }
            }
        }"#;
        assert_rows(parse_user_status(cli, now).unwrap().windows);

        let assert_overflowing_duplicate = |windows: Vec<UsageWindow>| {
            assert_eq!(windows.len(), 1);
            assert_eq!(
                windows[0].pace_window_key_for_test(),
                Some("model.same-model.v1")
            );
            assert!((windows[0].remaining_for_test() - 50.0).abs() < 0.01);
        };
        let overflowing_cli = r#"{
            "userStatus": {
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "modelOrAlias": { "model": "same-model" },
                            "quotaInfo": { "remainingFraction": 1e400 }
                        },
                        {
                            "modelOrAlias": { "model": "same-model" },
                            "quotaInfo": { "remainingFraction": 0.5 }
                        }
                    ]
                }
            }
        }"#;
        assert_overflowing_duplicate(parse_user_status(overflowing_cli, now).unwrap().windows);

        assert_rows(
            models_from_available(
                &json!({
                    "models": {
                        "valid": { "quotaInfo": { "remainingFraction": 0.5 } },
                        "negative": { "quotaInfo": { "remainingFraction": -0.1 } },
                        "over": { "quotaInfo": { "remainingFraction": 1.1 } },
                        "missing": { "quotaInfo": {} }
                    }
                })
                .to_string(),
                now,
            )
            .unwrap(),
        );

        let overflowing_models = r#"{
            "models": {
                "same-model": {
                    "quotaInfo": { "remainingFraction": 1e400 }
                },
                " same-model ": {
                    "quotaInfo": { "remainingFraction": 0.5 }
                }
            }
        }"#;
        assert_overflowing_duplicate(models_from_available(overflowing_models, now).unwrap());

        assert_rows(
            buckets_from_quota(
                &json!({
                    "buckets": [
                        { "modelId": "valid", "remainingFraction": 0.5 },
                        { "modelId": "negative", "remainingFraction": -0.1 },
                        { "modelId": "over", "remainingFraction": 1.1 },
                        { "modelId": "missing" }
                    ]
                })
                .to_string(),
                now,
            )
            .unwrap(),
        );

        let overflowing_buckets = r#"{
            "buckets": [
                { "modelId": "same-model", "remainingFraction": 1e400 },
                { "modelId": "same-model", "remainingFraction": 0.5 }
            ]
        }"#;
        assert_overflowing_duplicate(buckets_from_quota(overflowing_buckets, now).unwrap());

        let malformed_cli_row = r#"{
            "userStatus": {
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "label": 1e400,
                            "modelOrAlias": { "model": "same-model" },
                            "quotaInfo": { "remainingFraction": 0.4 }
                        },
                        {
                            "modelOrAlias": { "model": "same-model" },
                            "quotaInfo": { "remainingFraction": 0.5 }
                        }
                    ]
                }
            }
        }"#;
        assert_overflowing_duplicate(parse_user_status(malformed_cli_row, now).unwrap().windows);

        let malformed_model_row = r#"{
            "models": {
                "same-model": {
                    "displayName": 1e400,
                    "quotaInfo": { "remainingFraction": 0.4 }
                },
                " same-model ": {
                    "quotaInfo": { "remainingFraction": 0.5 }
                }
            }
        }"#;
        assert_overflowing_duplicate(models_from_available(malformed_model_row, now).unwrap());

        let malformed_bucket_fields = r#"{
            "buckets": [
                { "modelId": 42, "remainingFraction": 0.4 },
                { "modelId": "valid", "remainingFraction": 0.5 }
            ]
        }"#;
        let malformed_bucket_windows = buckets_from_quota(malformed_bucket_fields, now).unwrap();
        assert_eq!(malformed_bucket_windows.len(), 2);
        assert!(malformed_bucket_windows
            .iter()
            .any(|window| window.pace_window_key_for_test() == Some("model.valid.v1")));
        let malformed_bucket_wire = serde_json::to_value(&malformed_bucket_windows[0]).unwrap();
        assert_eq!(
            malformed_bucket_wire["paceStatus"]["reason"],
            "windowIdentity"
        );

        assert!(quota_window(
            "Non-finite".to_string(),
            f64::NAN,
            None,
            now,
            "model.non-finite.v1".to_string(),
            Some("model.non-finite.v1".to_string()),
        )
        .is_none());
        assert!(!binding_candidate_is_better(
            -0.1, None, 1, 0.5, None, 0, now
        ));
    }

    #[test]
    fn remote_scope_and_presentation_ignore_unbound_active_email() {
        let stale_active_email = "stale-other-account@example.com";
        let credentials = json!({
            "access_token": "short-lived-access",
            "refresh_token": "bound-google-refresh"
        });
        assert_eq!(
            remote_refresh_marker(&credentials),
            Some(b"bound-google-refresh".as_slice())
        );
        assert_ne!(
            remote_refresh_marker(&credentials),
            Some(stale_active_email.as_bytes())
        );
        let identity = remote_identity(Some("Paid".to_string()));
        assert_eq!(identity.email, None);
        assert_eq!(identity.plan.as_deref(), Some("Paid"));

        let access_only = json!({ "access_token": "access-is-not-the-frozen-marker" });
        assert_eq!(remote_refresh_marker(&access_only), None);
    }

    #[test]
    fn local_route_fails_closed_without_authenticated_email() {
        let fetched = parse_user_status(
            r#"{"userStatus":{"cascadeModelConfigData":{"clientModelConfigs":[]}}}"#,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(
            fetched.account_scope,
            Err(AccountScopeError::NoTrustedEvidence)
        );
    }

    #[test]
    fn resolves_remote_plan_from_tier() {
        assert_eq!(
            resolve_remote_plan(&json!({"currentTier":{"id":"free-tier"}})).as_deref(),
            Some("Free")
        );
        assert_eq!(
            resolve_remote_plan(&json!({"planInfo":{"planType":"standard"}})).as_deref(),
            Some("Standard")
        );
    }

    fn checkpoint_at(
        target: Option<RefreshCheckpoint>,
    ) -> impl FnMut(RefreshCheckpoint) -> Result<(), String> {
        move |checkpoint| {
            if Some(checkpoint) == target {
                Err("injected crash".to_string())
            } else {
                Ok(())
            }
        }
    }

    async fn test_refresh_response(refresh_token: String) -> Result<Value, String> {
        assert_eq!(refresh_token, "antigravity-old-refresh");
        Ok(json!({
            "access_token": "antigravity-new-access",
            "refresh_token": "antigravity-new-refresh",
            "expires_in": 3600
        }))
    }

    fn setup_refresh(tag: &str) -> (TestRefreshScope, PathBuf, AccountScope, Vec<u8>, String) {
        let scope = TestRefreshScope::new("antigravity", tag);
        let path = scope.root().join("antigravity/oauth_creds.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "access_token": "antigravity-old-access",
                "refresh_token": "antigravity-old-refresh",
                "expiry_date": 0
            }))
            .unwrap(),
        )
        .unwrap();
        let location = remote_scope_location(&path).unwrap();
        let old_scope = scope
            .resolve_current("google-oauth-creds", &location, b"antigravity-old-refresh")
            .unwrap();
        let metadata = scope.metadata_bytes();
        (scope, path, old_scope, metadata, location)
    }

    async fn run_refresh(
        scope: &TestRefreshScope,
        path: &Path,
        crash: Option<RefreshCheckpoint>,
    ) -> Result<(Value, String, Result<AccountScope, AccountScopeError>), String> {
        let now = DateTime::parse_from_rfc3339("2026-07-17T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        refresh_access_token_with(
            path,
            now,
            scope,
            test_refresh_response,
            |creds| write_creds_atomic(path, creds),
            checkpoint_at(crash),
        )
        .await
    }

    fn stored_refresh_token(path: &Path) -> String {
        let credentials = load_remote_credentials(path).unwrap();
        std::str::from_utf8(remote_refresh_marker(&credentials).unwrap())
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn refresh_crash_boundaries_and_scope_gate_use_production_sequence() {
        for boundary in [
            RefreshCheckpoint::Reloaded,
            RefreshCheckpoint::NetworkReturned,
            RefreshCheckpoint::MetadataHandled,
            RefreshCheckpoint::CredentialsPersisted,
        ] {
            let (scope, path, old_scope, before, location) = setup_refresh("antigravity-crash");
            assert_eq!(
                run_refresh(&scope, &path, Some(boundary))
                    .await
                    .unwrap_err(),
                "injected crash"
            );
            assert_eq!(
                stored_refresh_token(&path),
                if boundary == RefreshCheckpoint::CredentialsPersisted {
                    "antigravity-new-refresh"
                } else {
                    "antigravity-old-refresh"
                }
            );
            if matches!(
                boundary,
                RefreshCheckpoint::Reloaded | RefreshCheckpoint::NetworkReturned
            ) {
                assert_eq!(scope.metadata_bytes(), before);
            } else {
                assert_ne!(scope.metadata_bytes(), before);
                assert_eq!(
                    scope
                        .resolve_current(
                            "google-oauth-creds",
                            &location,
                            b"antigravity-old-refresh",
                        )
                        .unwrap(),
                    old_scope
                );
                assert_eq!(
                    scope
                        .resolve_current(
                            "google-oauth-creds",
                            &location,
                            b"antigravity-new-refresh",
                        )
                        .unwrap(),
                    old_scope
                );
            }
            scope.cleanup();
        }

        let (scope, path, old_scope, before, location) = setup_refresh("antigravity-metadata-fail");
        scope.fail_metadata_save();
        let (refreshed, access_token, scope_outcome) =
            run_refresh(&scope, &path, None).await.unwrap();
        assert_eq!(access_token, "antigravity-new-access");
        assert_eq!(remote_access_token(&refreshed).unwrap(), access_token);
        assert_eq!(scope_outcome, Err(AccountScopeError::MetadataWrite));
        assert_eq!(scope.metadata_bytes(), before);
        assert_eq!(stored_refresh_token(&path), "antigravity-old-refresh");
        assert_eq!(
            scope
                .resolve_current("google-oauth-creds", &location, b"antigravity-old-refresh",)
                .unwrap(),
            old_scope
        );
        scope.cleanup();

        let (scope, path, _old_scope, before, _) =
            setup_refresh("antigravity-metadata-fail-unchanged");
        scope.fail_metadata_save();
        let now = DateTime::parse_from_rfc3339("2026-07-17T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let save_path = path.clone();
        let (refreshed, access_token, scope_outcome) = refresh_access_token_with(
            &path,
            now,
            &scope,
            |refresh_token| async move {
                assert_eq!(refresh_token, "antigravity-old-refresh");
                Ok(json!({
                    "access_token": "antigravity-new-access",
                    "expires_in": 3600
                }))
            },
            move |credentials| write_creds_atomic(&save_path, credentials),
            checkpoint_at(None),
        )
        .await
        .unwrap();
        assert_eq!(scope_outcome, Err(AccountScopeError::MetadataWrite));
        assert_eq!(scope.metadata_bytes(), before);
        assert_eq!(access_token, "antigravity-new-access");
        assert_eq!(remote_access_token(&refreshed).unwrap(), access_token);
        let persisted = load_remote_credentials(&path).unwrap();
        assert_eq!(remote_access_token(&persisted).unwrap(), access_token);
        assert_eq!(stored_refresh_token(&path), "antigravity-old-refresh");
        scope.cleanup();

        let (scope, path, old_scope, _, location) = setup_refresh("antigravity-success");
        let (_, _, scope_outcome) = run_refresh(&scope, &path, None).await.unwrap();
        assert_eq!(scope_outcome.unwrap(), old_scope);
        assert_eq!(
            scope
                .resolve_current("google-oauth-creds", &location, b"antigravity-new-refresh",)
                .unwrap(),
            old_scope
        );
        scope.cleanup();
    }
}
