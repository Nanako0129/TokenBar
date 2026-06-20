use crate::agent_antigravity;
use crate::agent_copilot;
use crate::agent_history;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsagePayload {
    generated_at: String,
    agents: Vec<AgentUsageSnapshot>,
    /// Subscription-type providers opencode is authenticated against (its
    /// `auth.json` `type: "oauth"` entries), e.g. ["Codex", "Copilot"]. Surfaced
    /// so the user can see which agent subscriptions opencode also draws on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    opencode_subscriptions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsageSnapshot {
    client_id: String,
    source: String,
    updated_at: String,
    identity: Option<AgentIdentity>,
    windows: Vec<UsageWindow>,
    credits: Option<CreditsSnapshot>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdentity {
    pub(crate) email: Option<String>,
    pub(crate) plan: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    label: String,
    used_percent: f64,
    remaining_percent: f64,
    resets_at: Option<String>,
    reset_text: Option<String>,
    /// Total length of this rate-limit window in minutes. Lets the frontend
    /// derive a usage *pace* (expected vs actual at this point in the window).
    window_minutes: Option<i64>,
    /// Expected used-percent at this point in the window derived from *historical*
    /// usage samples (not the naive linear elapsed/duration). Only Codex weekly
    /// carries this once enough completed weeks have accrued; everything else is
    /// `None` and the frontend falls back to linear pace.
    #[serde(skip_serializing_if = "Option::is_none")]
    historical_expected_percent: Option<f64>,
    /// Probability (0..1) the window empties before its reset at the historical
    /// burn rate. Companion to `historical_expected_percent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    run_out_probability: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditsSnapshot {
    remaining: Option<f64>,
    unlimited: bool,
}

impl UsageWindow {
    /// Build a window from a "remaining fraction" (0..1) — the shape Antigravity
    /// reports per model. Used-percent is derived; pace/window fields stay empty.
    pub(crate) fn from_fraction(
        label: String,
        remaining_fraction: f64,
        resets_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Self {
        let remaining = (remaining_fraction * 100.0).clamp(0.0, 100.0);
        UsageWindow {
            label,
            used_percent: (100.0 - remaining).max(0.0),
            remaining_percent: remaining,
            resets_at: resets_at.map(|d| d.to_rfc3339_opts(SecondsFormat::Millis, true)),
            reset_text: resets_at.map(|d| reset_text(d, now)),
            window_minutes: None,
            historical_expected_percent: None,
            run_out_probability: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn label_for_test(&self) -> &str {
        &self.label
    }

    #[cfg(test)]
    pub(crate) fn remaining_for_test(&self) -> f64 {
        self.remaining_percent
    }
}

#[derive(Debug, Clone)]
struct CodexCredentials {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    account_id: Option<String>,
    last_refresh: Option<DateTime<Utc>>,
    auth_path: PathBuf,
    raw_json: Value,
}

#[derive(Debug, Clone)]
struct ClaudeCredentials {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    scopes: Vec<String>,
    rate_limit_tier: Option<String>,
    subscription_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeCredentialsRoot {
    #[serde(default, rename = "claudeAiOauth")]
    claude_ai_oauth: Option<ClaudeCredentialsOauth>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeCredentialsOauth {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<f64>,
    scopes: Option<Vec<String>>,
    rate_limit_tier: Option<String>,
    subscription_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexUsageResponse {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit: Option<CodexRateLimit>,
    #[serde(default)]
    additional_rate_limits: Option<Vec<CodexAdditionalRateLimit>>,
    #[serde(default)]
    credits: Option<CodexCredits>,
}

#[derive(Debug, Deserialize)]
struct CodexRateLimit {
    #[serde(default)]
    primary_window: Option<CodexWindow>,
    #[serde(default)]
    secondary_window: Option<CodexWindow>,
}

#[derive(Debug, Clone, Deserialize)]
struct CodexWindow {
    used_percent: f64,
    reset_at: i64,
    limit_window_seconds: i64,
}

#[derive(Debug, Deserialize)]
struct CodexAdditionalRateLimit {
    #[serde(default)]
    limit_name: Option<String>,
    #[serde(default)]
    metered_feature: Option<String>,
    #[serde(default)]
    rate_limit: Option<CodexRateLimit>,
}

#[derive(Debug, Deserialize)]
struct CodexCredits {
    #[serde(default)]
    unlimited: bool,
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    balance: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
struct ClaudeUsageResponse {
    #[serde(default)]
    five_hour: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_oauth_apps: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_opus: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_design: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_claude_design: Option<ClaudeWindow>,
    #[serde(default)]
    claude_design: Option<ClaudeWindow>,
    #[serde(default)]
    design: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_omelette: Option<ClaudeWindow>,
    #[serde(default)]
    omelette: Option<ClaudeWindow>,
    #[serde(default)]
    omelette_promotional: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_routines: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_claude_routines: Option<ClaudeWindow>,
    #[serde(default)]
    claude_routines: Option<ClaudeWindow>,
    #[serde(default)]
    routines: Option<ClaudeWindow>,
    #[serde(default)]
    routine: Option<ClaudeWindow>,
    #[serde(default)]
    seven_day_cowork: Option<ClaudeWindow>,
    #[serde(default)]
    cowork: Option<ClaudeWindow>,
    #[serde(default)]
    extra_usage: Option<ClaudeExtraUsage>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClaudeWindow {
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeExtraUsage {
    #[serde(default)]
    is_enabled: bool,
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    monthly_limit: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    used_credits: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    utilization: Option<f64>,
    #[serde(default)]
    currency: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

pub async fn run() -> AgentUsagePayload {
    let generated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let (codex, claude, antigravity, copilot) = tokio::join!(
        fetch_codex(),
        fetch_claude(),
        fetch_antigravity(),
        fetch_copilot()
    );
    let mut agents = vec![codex, claude, antigravity];
    // Copilot only appears when signed in (via opencode); skip a bare not-signed-in error card.
    if let Some(copilot) = copilot {
        agents.push(copilot);
    }
    AgentUsagePayload {
        generated_at,
        agents,
        opencode_subscriptions: crate::opencode_integrations::detect_subscriptions(),
    }
}

async fn fetch_copilot() -> Option<AgentUsageSnapshot> {
    // No opencode Copilot auth → no card at all (rather than an error row).
    crate::opencode_integrations::github_copilot_token()?;
    let now = Utc::now();
    Some(match agent_copilot::fetch(now).await {
        Ok(data) => AgentUsageSnapshot {
            client_id: "copilot".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: data.identity,
            windows: data.windows,
            credits: None,
            error: None,
        },
        Err(error) => AgentUsageSnapshot {
            client_id: "copilot".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: None,
            windows: Vec::new(),
            credits: None,
            error: Some(error),
        },
    })
}

async fn fetch_antigravity() -> AgentUsageSnapshot {
    let now = Utc::now();
    match agent_antigravity::fetch(now).await {
        Ok(fetched) => AgentUsageSnapshot {
            client_id: "antigravity".to_string(),
            source: fetched.source,
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: fetched.identity,
            windows: fetched.windows,
            credits: None,
            error: None,
        },
        Err(error) => AgentUsageSnapshot {
            client_id: "antigravity".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: None,
            windows: Vec::new(),
            credits: None,
            error: Some(error),
        },
    }
}

async fn fetch_codex() -> AgentUsageSnapshot {
    match fetch_codex_inner().await {
        Ok(snapshot) => snapshot,
        Err(error) => AgentUsageSnapshot {
            client_id: "codex".to_string(),
            source: "oauth".to_string(),
            updated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: None,
            windows: Vec::new(),
            credits: None,
            error: Some(error),
        },
    }
}

/// Claude's `/api/oauth/usage` rate-limits aggressively (and the budget is
/// shared with any other monitor on the account, e.g. codexbar). Modeled on
/// codexbar's ClaudeOAuthUsageRateLimitGate: after a 429, stop hitting the
/// endpoint until Retry-After (default 5 min) and serve the last good
/// snapshot so the card keeps its data instead of flashing an error.
struct ClaudeUsageGate {
    blocked_until: Option<DateTime<Utc>>,
    last_good: Option<AgentUsageSnapshot>,
}

static CLAUDE_USAGE_GATE: Mutex<ClaudeUsageGate> = Mutex::new(ClaudeUsageGate {
    blocked_until: None,
    last_good: None,
});

/// Lock the gate, recovering from a poisoned mutex instead of panicking. Under
/// the release profile's unwind + FFI-boundary `catch_unwind` (see `guarded` in
/// lib.rs), a panic caught mid-section poisons this static; `into_inner()` keeps
/// the 429 gate working for the rest of the process instead of wedging every
/// later `tb_agent_usage` call — same stance as the live-tail lock in lib.rs.
fn lock_gate() -> std::sync::MutexGuard<'static, ClaudeUsageGate> {
    CLAUDE_USAGE_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn claude_gate_blocked_until(now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mut gate = lock_gate();
    match gate.blocked_until {
        Some(until) if until > now => Some(until),
        Some(_) => {
            gate.blocked_until = None;
            None
        }
        None => None,
    }
}

fn claude_gate_record_rate_limit(retry_after: Option<DateTime<Utc>>, now: DateTime<Utc>) {
    let blocked_until = retry_after
        .filter(|until| *until > now)
        .unwrap_or_else(|| now + chrono::Duration::minutes(5));
    lock_gate().blocked_until = Some(blocked_until);
}

fn claude_gate_record_success(snapshot: &AgentUsageSnapshot) {
    let mut gate = lock_gate();
    gate.blocked_until = None;
    gate.last_good = Some(snapshot.clone());
}

/// While the gate is closed, prefer the cached snapshot (its `updated_at`
/// stays honest); with nothing cached yet, surface a countdown error.
fn claude_gate_fallback(blocked_until: DateTime<Utc>, now: DateTime<Utc>) -> AgentUsageSnapshot {
    if let Some(snapshot) = lock_gate().last_good.clone() {
        return snapshot;
    }
    let wait_secs = (blocked_until - now).num_seconds().max(0);
    AgentUsageSnapshot {
        client_id: "claude".to_string(),
        source: "oauth".to_string(),
        updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
        identity: None,
        windows: Vec::new(),
        credits: None,
        error: Some(format!(
            "Claude OAuth usage endpoint is rate limited. Retrying automatically in ~{}s.",
            wait_secs
        )),
    }
}

fn parse_retry_after(value: Option<&reqwest::header::HeaderValue>) -> Option<DateTime<Utc>> {
    let raw = value?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(seconds) = raw.parse::<i64>() {
        return (seconds >= 0).then(|| Utc::now() + chrono::Duration::seconds(seconds));
    }
    DateTime::parse_from_rfc2822(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

async fn fetch_claude() -> AgentUsageSnapshot {
    let now = Utc::now();
    if let Some(blocked_until) = claude_gate_blocked_until(now) {
        return claude_gate_fallback(blocked_until, now);
    }
    match fetch_claude_inner().await {
        Ok(snapshot) => {
            claude_gate_record_success(&snapshot);
            snapshot
        }
        Err(error) => {
            // A 429 inside fetch_claude_inner arms the gate; fall back to the
            // cached snapshot rather than blanking the card.
            let now = Utc::now();
            if let Some(blocked_until) = claude_gate_blocked_until(now) {
                return claude_gate_fallback(blocked_until, now);
            }
            AgentUsageSnapshot {
                client_id: "claude".to_string(),
                source: "oauth".to_string(),
                updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
                identity: None,
                windows: Vec::new(),
                credits: None,
                error: Some(error),
            }
        }
    }
}

async fn fetch_codex_inner() -> Result<AgentUsageSnapshot, String> {
    let mut credentials = load_codex_credentials()?;
    if credentials_needs_refresh(credentials.last_refresh) {
        if credentials
            .refresh_token
            .as_deref()
            .unwrap_or("")
            .is_empty()
        {
            return Err(
                "Codex OAuth token needs refresh but auth.json has no refresh token.".to_string(),
            );
        }
        credentials = refresh_codex_credentials(credentials).await?;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build Codex OAuth client: {}", e))?;

    let mut request = client
        .get(CODEX_USAGE_URL)
        .bearer_auth(&credentials.access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, "TokenBar");
    if let Some(account_id) = credentials.account_id.as_deref().filter(|s| !s.is_empty()) {
        request = request.header("ChatGPT-Account-Id", account_id);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("Codex OAuth request failed: {}", e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("read Codex OAuth response: {}", e))?;

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(
            "Codex OAuth token expired or invalid. Run `codex` to log in again.".to_string(),
        );
    }
    if !status.is_success() {
        return Err(format!("Codex usage API returned {}.", status.as_u16()));
    }

    let usage: CodexUsageResponse =
        serde_json::from_str(&body).map_err(|e| format!("decode Codex usage response: {}", e))?;
    let now = Utc::now();
    let identity = Some(AgentIdentity {
        email: credentials.id_token.as_deref().and_then(jwt_email),
        plan: usage.plan_type.as_deref().map(clean_plan).or_else(|| {
            credentials
                .id_token
                .as_deref()
                .and_then(jwt_plan)
                .map(clean_plan)
        }),
    });
    let mut windows = codex_windows(
        usage.rate_limit.as_ref(),
        usage.additional_rate_limits.as_deref(),
        now,
    );
    let account_key = codex_account_key(&credentials, identity.as_ref());
    enrich_codex_weekly_history(&mut windows, &account_key, now);
    if windows.is_empty() && usage.credits.as_ref().and_then(|c| c.balance).is_none() {
        return Err("Codex usage API returned no rate-limit windows.".to_string());
    }

    Ok(AgentUsageSnapshot {
        client_id: "codex".to_string(),
        source: "oauth".to_string(),
        updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
        identity,
        windows,
        credits: usage.credits.map(|credits| CreditsSnapshot {
            remaining: credits.balance,
            unlimited: credits.unlimited,
        }),
        error: None,
    })
}

async fn fetch_claude_inner() -> Result<AgentUsageSnapshot, String> {
    let mut credentials = load_claude_credentials()?;
    if claude_credentials_expired(&credentials) {
        credentials = refresh_claude_credentials(&credentials).await?;
    }

    if !credentials.scopes.is_empty()
        && !credentials
            .scopes
            .iter()
            .any(|scope| scope == "user:profile")
    {
        return Err(
            "Claude OAuth token lacks the user:profile scope. Run `claude logout && claude login`."
                .to_string(),
        );
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build Claude OAuth client: {}", e))?;

    let response = client
        .get(CLAUDE_USAGE_URL)
        .bearer_auth(&credentials.access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, claude_user_agent())
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .map_err(|e| format!("Claude OAuth request failed: {}", e))?;
    let status = response.status();
    let retry_after = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER))
    } else {
        None
    };
    let body = response
        .text()
        .await
        .map_err(|e| format!("read Claude OAuth response: {}", e))?;

    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(
            "Claude OAuth token expired or invalid. Run `claude` to re-authenticate.".to_string(),
        );
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(
            "Claude OAuth usage was denied. Run `claude logout && claude login` to grant user:profile."
                .to_string(),
        );
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        claude_gate_record_rate_limit(retry_after, Utc::now());
        return Err(
            "Claude OAuth usage endpoint is rate limited. Backing off automatically.".to_string(),
        );
    }
    if !status.is_success() {
        return Err(format!("Claude usage API returned {}.", status.as_u16()));
    }

    let usage: ClaudeUsageResponse =
        serde_json::from_str(&body).map_err(|e| format!("decode Claude usage response: {}", e))?;
    let now = Utc::now();
    let windows = claude_windows(&usage, now);
    if windows.is_empty() {
        return Err("Claude usage API returned no rate-limit windows.".to_string());
    }

    Ok(AgentUsageSnapshot {
        client_id: "claude".to_string(),
        source: "oauth".to_string(),
        updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
        identity: Some(AgentIdentity {
            email: None,
            plan: first_non_empty([
                credentials.subscription_type.as_deref(),
                credentials.rate_limit_tier.as_deref(),
            ])
            .map(clean_plan),
        }),
        windows,
        credits: claude_credits(usage.extra_usage.as_ref()),
        error: None,
    })
}

fn load_codex_credentials() -> Result<CodexCredentials, String> {
    let auth_path = codex_home().join("auth.json");
    let raw = fs::read_to_string(&auth_path)
        .map_err(|_| "Codex auth.json not found. Run `codex` to log in.".to_string())?;
    let raw_json: Value =
        serde_json::from_str(&raw).map_err(|e| format!("decode Codex auth.json: {}", e))?;

    if raw_json
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .is_some_and(|key| !key.trim().is_empty())
    {
        return Err(
            "Codex is using API-key auth; OAuth usage limits require `codex login`.".to_string(),
        );
    }

    let tokens = raw_json
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| "Codex auth.json exists but contains no OAuth tokens.".to_string())?;
    let access_token = string_key(tokens, "access_token", "accessToken")
        .ok_or_else(|| "Codex auth.json has no access token.".to_string())?;
    let refresh_token = string_key(tokens, "refresh_token", "refreshToken");
    let id_token = string_key(tokens, "id_token", "idToken");
    let account_id = string_key(tokens, "account_id", "accountId");
    let last_refresh = raw_json
        .get("last_refresh")
        .and_then(Value::as_str)
        .and_then(parse_datetime);

    Ok(CodexCredentials {
        access_token,
        refresh_token,
        id_token,
        account_id,
        last_refresh,
        auth_path,
        raw_json,
    })
}

fn load_claude_credentials() -> Result<ClaudeCredentials, String> {
    if let Some(credentials) = load_claude_credentials_from_environment()? {
        return Ok(credentials);
    }

    if let Some(raw) = load_claude_credentials_from_keychain()? {
        return parse_claude_credentials_data(&raw);
    }

    let credentials_path = claude_credentials_path();
    match fs::read_to_string(&credentials_path) {
        Ok(raw) => return parse_claude_credentials_data(&raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "read Claude credentials file {}: {}",
                credentials_path.display(),
                error
            ));
        }
    }

    Err("Claude OAuth credentials not found. Run `claude` to authenticate.".to_string())
}

fn load_claude_credentials_from_environment() -> Result<Option<ClaudeCredentials>, String> {
    let token = std::env::var("TOKENBAR_CLAUDE_OAUTH_TOKEN")
        .or_else(|_| std::env::var("TOKCAT_CLAUDE_OAUTH_TOKEN"))
        .or_else(|_| std::env::var("CODEXBAR_CLAUDE_OAUTH_TOKEN"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(access_token) = token else {
        return Ok(None);
    };
    let scopes = std::env::var("TOKENBAR_CLAUDE_OAUTH_SCOPES")
        .or_else(|_| std::env::var("TOKCAT_CLAUDE_OAUTH_SCOPES"))
        .or_else(|_| std::env::var("CODEXBAR_CLAUDE_OAUTH_SCOPES"))
        .unwrap_or_default()
        .split([',', ' '])
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect();
    Ok(Some(ClaudeCredentials {
        access_token,
        refresh_token: None,
        expires_at: None,
        scopes,
        rate_limit_tier: None,
        subscription_type: None,
    }))
}

fn parse_claude_credentials_data(raw: &str) -> Result<ClaudeCredentials, String> {
    let root: ClaudeCredentialsRoot =
        serde_json::from_str(raw).map_err(|e| format!("decode Claude OAuth credentials: {}", e))?;
    let oauth = root
        .claude_ai_oauth
        .ok_or_else(|| "Claude OAuth credentials are missing claudeAiOauth.".to_string())?;
    let access_token = oauth
        .access_token
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "Claude OAuth credentials have no access token.".to_string())?;
    let expires_at = oauth
        .expires_at
        .and_then(|millis| Utc.timestamp_millis_opt(millis as i64).single());
    Ok(ClaudeCredentials {
        access_token,
        refresh_token: oauth
            .refresh_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty()),
        expires_at,
        scopes: oauth.scopes.unwrap_or_default(),
        rate_limit_tier: oauth.rate_limit_tier,
        subscription_type: oauth.subscription_type,
    })
}

#[cfg(target_os = "macos")]
fn load_claude_credentials_from_keychain() -> Result<Option<String>, String> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
        .output()
        .map_err(|e| format!("read Claude Keychain credentials: {}", e))?;
    if !output.status.success() {
        return Ok(None);
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|_| "Claude Keychain credentials are not UTF-8 JSON.".to_string())?;
    let raw = raw.trim_matches(['\r', '\n']).to_string();
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(raw))
}

#[cfg(not(target_os = "macos"))]
fn load_claude_credentials_from_keychain() -> Result<Option<String>, String> {
    Ok(None)
}

async fn refresh_codex_credentials(
    credentials: CodexCredentials,
) -> Result<CodexCredentials, String> {
    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .ok_or_else(|| "Codex auth.json has no refresh token.".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build Codex refresh client: {}", e))?;
    let body = serde_json::json!({
        "client_id": CODEX_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "scope": "openid profile email"
    });
    let response = client
        .post(CODEX_REFRESH_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Codex token refresh failed: {}", e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("read Codex refresh response: {}", e))?;
    if !status.is_success() {
        return Err("Codex OAuth refresh failed. Run `codex` to log in again.".to_string());
    }
    let json: Value =
        serde_json::from_str(&body).map_err(|e| format!("decode Codex refresh response: {}", e))?;

    let refreshed = CodexCredentials {
        access_token: json
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or(&credentials.access_token)
            .to_string(),
        refresh_token: json
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(credentials.refresh_token),
        id_token: json
            .get("id_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(credentials.id_token),
        account_id: credentials.account_id,
        last_refresh: Some(Utc::now()),
        auth_path: credentials.auth_path,
        raw_json: credentials.raw_json,
    };
    save_codex_credentials(&refreshed)?;
    Ok(refreshed)
}

async fn refresh_claude_credentials(
    credentials: &ClaudeCredentials,
) -> Result<ClaudeCredentials, String> {
    let refresh_token = credentials.refresh_token.as_deref().ok_or_else(|| {
        "Claude OAuth token is expired and has no refresh token. Run `claude`.".to_string()
    })?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build Claude refresh client: {}", e))?;
    let response = client
        .post(CLAUDE_REFRESH_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form_urlencoded(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLAUDE_CLIENT_ID),
        ]))
        .send()
        .await
        .map_err(|e| format!("Claude OAuth refresh failed: {}", e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("read Claude refresh response: {}", e))?;
    if !status.is_success() {
        return Err("Claude OAuth refresh failed. Run `claude` to re-authenticate.".to_string());
    }
    let token_response: ClaudeRefreshResponse = serde_json::from_str(&body)
        .map_err(|e| format!("decode Claude refresh response: {}", e))?;
    Ok(ClaudeCredentials {
        access_token: token_response.access_token,
        refresh_token: token_response
            .refresh_token
            .or_else(|| credentials.refresh_token.clone()),
        expires_at: Some(Utc::now() + chrono::Duration::seconds(token_response.expires_in)),
        scopes: credentials.scopes.clone(),
        rate_limit_tier: credentials.rate_limit_tier.clone(),
        subscription_type: credentials.subscription_type.clone(),
    })
}

fn save_codex_credentials(credentials: &CodexCredentials) -> Result<(), String> {
    let mut raw = credentials.raw_json.clone();
    raw["tokens"]["access_token"] = Value::String(credentials.access_token.clone());
    if let Some(refresh_token) = &credentials.refresh_token {
        raw["tokens"]["refresh_token"] = Value::String(refresh_token.clone());
    }
    if let Some(id_token) = &credentials.id_token {
        raw["tokens"]["id_token"] = Value::String(id_token.clone());
    }
    if let Some(account_id) = &credentials.account_id {
        raw["tokens"]["account_id"] = Value::String(account_id.clone());
    }
    raw["last_refresh"] = Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true));
    let data =
        serde_json::to_vec_pretty(&raw).map_err(|e| format!("encode Codex auth.json: {}", e))?;
    fs::write(&credentials.auth_path, data).map_err(|e| format!("save Codex auth.json: {}", e))
}

/// Stable per-account key for scoping historical samples, so switching ChatGPT
/// accounts doesn't mix usage curves. Prefer the account id, fall back to email.
fn codex_account_key(credentials: &CodexCredentials, identity: Option<&AgentIdentity>) -> String {
    credentials
        .account_id
        .clone()
        .filter(|id| !id.is_empty())
        .or_else(|| identity.and_then(|i| i.email.clone()))
        .unwrap_or_else(|| "default".to_string())
}

/// Record the live Codex weekly reading and, once enough past weeks exist, fill
/// the window's `historical_expected_percent` / `run_out_probability` so the
/// frontend can offer a history-based pace alongside the linear one.
fn enrich_codex_weekly_history(windows: &mut [UsageWindow], account_key: &str, now: DateTime<Utc>) {
    for window in windows.iter_mut() {
        if !window.label.eq_ignore_ascii_case("Weekly") {
            continue;
        }
        let (Some(resets_str), Some(minutes)) =
            (window.resets_at.as_deref(), window.window_minutes)
        else {
            continue;
        };
        let Some(resets_at) = parse_datetime(resets_str) else {
            continue;
        };
        if let Some(pace) = agent_history::record_and_evaluate(
            account_key,
            resets_at.timestamp(),
            minutes,
            window.used_percent,
            now.timestamp(),
        ) {
            window.historical_expected_percent = Some(pace.expected_percent);
            window.run_out_probability = pace.run_out_probability;
        }
    }
}

fn codex_windows(
    rate_limit: Option<&CodexRateLimit>,
    additional_rate_limits: Option<&[CodexAdditionalRateLimit]>,
    now: DateTime<Utc>,
) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    if let Some(rate_limit) = rate_limit {
        let mut primary = rate_limit.primary_window.clone();
        let mut secondary = rate_limit.secondary_window.clone();
        if role(primary.as_ref()) == Some("weekly") && role(secondary.as_ref()) != Some("weekly") {
            std::mem::swap(&mut primary, &mut secondary);
        }

        if let Some(window) = primary {
            windows.push(map_window("Session", window, now));
        }
        if let Some(window) = secondary {
            windows.push(map_window("Weekly", window, now));
        }
    }

    let mut seen = windows
        .iter()
        .map(|w| w.label.clone())
        .collect::<HashSet<_>>();
    for extra in additional_rate_limits.unwrap_or(&[]) {
        let Some(rate_limit) = extra.rate_limit.as_ref() else {
            continue;
        };
        let Some(window) = rate_limit
            .primary_window
            .clone()
            .or_else(|| rate_limit.secondary_window.clone())
        else {
            continue;
        };
        let label = additional_limit_label(extra);
        if seen.insert(label.clone()) {
            windows.push(map_window(&label, window, now));
        }
    }
    windows
}

fn claude_windows(usage: &ClaudeUsageResponse, now: DateTime<Utc>) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    push_claude_window(&mut windows, "Session", usage.five_hour.as_ref(), now);
    push_claude_window(&mut windows, "Weekly", usage.seven_day.as_ref(), now);
    push_claude_window(
        &mut windows,
        "OAuth Apps",
        usage.seven_day_oauth_apps.as_ref(),
        now,
    );
    push_claude_window(&mut windows, "Sonnet", usage.seven_day_sonnet.as_ref(), now);
    push_claude_window(&mut windows, "Opus", usage.seven_day_opus.as_ref(), now);
    push_claude_window(&mut windows, "Designs", usage.design_window(), now);
    push_claude_window(&mut windows, "Daily Routines", usage.routines_window(), now);
    if let Some(extra) = claude_extra_usage_window(usage.extra_usage.as_ref()) {
        windows.push(extra);
    }
    windows
}

impl ClaudeUsageResponse {
    fn design_window(&self) -> Option<&ClaudeWindow> {
        [
            self.seven_day_design.as_ref(),
            self.seven_day_claude_design.as_ref(),
            self.claude_design.as_ref(),
            self.design.as_ref(),
            self.seven_day_omelette.as_ref(),
            self.omelette.as_ref(),
            self.omelette_promotional.as_ref(),
        ]
        .into_iter()
        .flatten()
        .next()
    }

    fn routines_window(&self) -> Option<&ClaudeWindow> {
        [
            self.seven_day_routines.as_ref(),
            self.seven_day_claude_routines.as_ref(),
            self.claude_routines.as_ref(),
            self.routines.as_ref(),
            self.routine.as_ref(),
            self.seven_day_cowork.as_ref(),
            self.cowork.as_ref(),
        ]
        .into_iter()
        .flatten()
        .next()
    }
}

fn push_claude_window(
    windows: &mut Vec<UsageWindow>,
    label: &str,
    window: Option<&ClaudeWindow>,
    now: DateTime<Utc>,
) {
    if let Some(mapped) = window.and_then(|window| map_claude_window(label, window, now)) {
        windows.push(mapped);
    }
}

fn map_claude_window(
    label: &str,
    window: &ClaudeWindow,
    now: DateTime<Utc>,
) -> Option<UsageWindow> {
    let used = window.utilization?.clamp(0.0, 100.0);
    let resets_at = window.resets_at.as_deref().and_then(parse_datetime);
    Some(UsageWindow {
        label: label.to_string(),
        used_percent: used,
        remaining_percent: (100.0 - used).max(0.0),
        resets_at: resets_at.map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true)),
        reset_text: resets_at.map(|date| reset_text(date, now)),
        window_minutes: claude_window_minutes(label),
        historical_expected_percent: None,
        run_out_probability: None,
    })
}

fn claude_extra_usage_window(extra: Option<&ClaudeExtraUsage>) -> Option<UsageWindow> {
    let extra = extra?;
    if !extra.is_enabled {
        return None;
    }
    let used = extra.utilization.or_else(|| {
        let used = extra.used_credits?;
        let limit = extra.monthly_limit?;
        if limit > 0.0 {
            Some((used / limit) * 100.0)
        } else {
            None
        }
    })?;
    let reset_text = match (extra.used_credits, extra.monthly_limit) {
        (Some(used), Some(limit)) => Some(format!(
            "Monthly cap: {} / {}",
            format_currency_minor_units(used, extra.currency.as_deref()),
            format_currency_minor_units(limit, extra.currency.as_deref())
        )),
        _ => None,
    };
    Some(UsageWindow {
        label: "Extra usage".to_string(),
        used_percent: used.clamp(0.0, 100.0),
        remaining_percent: (100.0 - used).max(0.0),
        resets_at: None,
        reset_text,
        window_minutes: None,
        historical_expected_percent: None,
        run_out_probability: None,
    })
}

fn claude_credits(extra: Option<&ClaudeExtraUsage>) -> Option<CreditsSnapshot> {
    let extra = extra?;
    if !extra.is_enabled {
        return None;
    }
    let remaining = match (extra.monthly_limit, extra.used_credits) {
        (Some(limit), Some(used)) => Some(((limit - used) / 100.0).max(0.0)),
        _ => None,
    };
    Some(CreditsSnapshot {
        remaining,
        unlimited: false,
    })
}

fn format_currency_minor_units(value: f64, currency: Option<&str>) -> String {
    let major = value / 100.0;
    match currency.unwrap_or("USD").trim().to_uppercase().as_str() {
        "USD" => format!("${:.2}", major),
        code if !code.is_empty() => format!("{:.2} {}", major, code),
        _ => format!("${:.2}", major),
    }
}

fn additional_limit_label(limit: &CodexAdditionalRateLimit) -> String {
    let source = first_non_empty([
        limit.limit_name.as_deref(),
        limit.metered_feature.as_deref(),
    ])
    .unwrap_or("Codex extra limit");
    let lower = source.to_lowercase();
    if lower.contains("spark") {
        return "Codex Spark".to_string();
    }
    clean_limit_label(source)
}

fn first_non_empty(values: [Option<&str>; 2]) -> Option<&str> {
    values
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
}

fn clean_limit_label(value: &str) -> String {
    value
        .replace(['_', '-'], " ")
        .split_whitespace()
        .map(|part| {
            if part.eq_ignore_ascii_case("gpt") {
                "GPT".to_string()
            } else if part.eq_ignore_ascii_case("codex") {
                "Codex".to_string()
            } else {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn map_window(label: &str, window: CodexWindow, now: DateTime<Utc>) -> UsageWindow {
    let resets_at = if window.reset_at > 0 {
        Utc.timestamp_opt(window.reset_at, 0).single()
    } else {
        None
    };
    let used = window.used_percent.clamp(0.0, 100.0);
    UsageWindow {
        label: label.to_string(),
        used_percent: used,
        remaining_percent: (100.0 - used).max(0.0),
        resets_at: resets_at.map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true)),
        reset_text: resets_at.map(|date| reset_text(date, now)),
        window_minutes: (window.limit_window_seconds > 0).then_some(window.limit_window_seconds / 60),
        historical_expected_percent: None,
        run_out_probability: None,
    }
}

/// Standard Claude window lengths by label, since the API doesn't report them:
/// the session bucket is 5h, everything else is the 7-day weekly family.
fn claude_window_minutes(label: &str) -> Option<i64> {
    Some(if label.eq_ignore_ascii_case("Session") { 300 } else { 10_080 })
}

fn role(window: Option<&CodexWindow>) -> Option<&'static str> {
    match window?.limit_window_seconds {
        18_000 => Some("session"),
        604_800 => Some("weekly"),
        _ => None,
    }
}

pub(crate) fn reset_text(reset: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (reset - now).num_seconds();
    if seconds <= 0 {
        return "Resets now".to_string();
    }
    let minutes = (seconds + 59) / 60;
    if minutes < 60 {
        return format!("Resets in {}m", minutes);
    }
    let hours = minutes / 60;
    let mins = minutes % 60;
    // Anything spanning a day or more reads in days+hours so the weekly windows
    // stay consistent across agents (Claude reported 47h, Codex 2d — unify both
    // to days); sub-day windows (sessions) keep the hours/minutes form.
    if hours < 24 {
        if mins > 0 {
            return format!("Resets in {}h {}m", hours, mins);
        }
        return format!("Resets in {}h", hours);
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours > 0 {
        format!("Resets in {}d {}h", days, rem_hours)
    } else {
        format!("Resets in {}d", days)
    }
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn claude_credentials_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".claude/.credentials.json"))
        .unwrap_or_else(|| PathBuf::from(".claude/.credentials.json"))
}

fn credentials_needs_refresh(last_refresh: Option<DateTime<Utc>>) -> bool {
    let Some(last_refresh) = last_refresh else {
        return true;
    };
    (Utc::now() - last_refresh).num_days() > 8
}

fn claude_credentials_expired(credentials: &ClaudeCredentials) -> bool {
    credentials
        .expires_at
        .is_some_and(|expires_at| Utc::now() >= expires_at)
}

pub(crate) fn parse_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

fn claude_user_agent() -> String {
    std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .and_then(|stdout| stdout.split_whitespace().next().map(str::to_string))
        .filter(|version| !version.is_empty())
        .map(|version| format!("claude-code/{}", version))
        .unwrap_or_else(|| "claude-code/2.1.0".to_string())
}

fn form_urlencoded(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub(crate) fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{:02X}", byte)),
        }
    }
    encoded
}

fn string_key(
    map: &serde_json::Map<String, Value>,
    snake_case: &str,
    camel_case: &str,
) -> Option<String> {
    map.get(snake_case)
        .or_else(|| map.get(camel_case))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let mut encoded = payload.replace('-', "+").replace('_', "/");
    while encoded.len() % 4 != 0 {
        encoded.push('=');
    }
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    serde_json::from_slice(&data).ok()
}

fn jwt_email(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    payload
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("https://api.openai.com/profile")
                .and_then(Value::as_object)
                .and_then(|profile| profile.get("email"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn jwt_plan(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    payload
        .get("chatgpt_plan_type")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("https://api.openai.com/auth")
                .and_then(Value::as_object)
                .and_then(|auth| auth.get("chatgpt_plan_type"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(crate) fn clean_plan(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn deserialize_optional_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse::<f64>().ok(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_retry_after_seconds_and_http_date() {
        let header = reqwest::header::HeaderValue::from_static("120");
        let parsed = parse_retry_after(Some(&header)).unwrap();
        let delta = (parsed - Utc::now()).num_seconds();
        assert!((118..=120).contains(&delta), "delta was {}", delta);

        let header = reqwest::header::HeaderValue::from_static("Fri, 21 Nov 2025 09:00:00 GMT");
        let parsed = parse_retry_after(Some(&header)).unwrap();
        assert_eq!(parsed.timestamp(), 1_763_715_600);

        let header = reqwest::header::HeaderValue::from_static("bogus");
        assert!(parse_retry_after(Some(&header)).is_none());
        assert!(parse_retry_after(None).is_none());
    }

    // Single test for the whole gate lifecycle — the gate is a process-wide
    // static, so split tests would race under the parallel test runner.
    #[test]
    fn claude_gate_blocks_then_clears() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        assert!(claude_gate_blocked_until(now).is_none());

        // 429 with no Retry-After → default 5-minute cooldown.
        claude_gate_record_rate_limit(None, now);
        let until = claude_gate_blocked_until(now).unwrap();
        assert_eq!((until - now).num_seconds(), 300);

        // No cached snapshot yet → countdown error.
        let fallback = claude_gate_fallback(until, now);
        assert!(fallback.error.unwrap().contains("~300s"));
        assert!(fallback.windows.is_empty());

        // Cooldown expiry clears the gate lazily.
        let later = now + chrono::Duration::seconds(301);
        assert!(claude_gate_blocked_until(later).is_none());

        // Success caches the snapshot; a later 429 serves it instead.
        let snapshot = AgentUsageSnapshot {
            client_id: "claude".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: None,
            windows: vec![UsageWindow {
                label: "Session".to_string(),
                used_percent: 20.0,
                remaining_percent: 80.0,
                resets_at: None,
                reset_text: None,
                window_minutes: Some(300),
                historical_expected_percent: None,
                run_out_probability: None,
            }],
            credits: None,
            error: None,
        };
        claude_gate_record_success(&snapshot);
        assert!(claude_gate_blocked_until(later).is_none());
        claude_gate_record_rate_limit(Some(later + chrono::Duration::seconds(60)), later);
        let until = claude_gate_blocked_until(later).unwrap();
        let fallback = claude_gate_fallback(until, later);
        assert!(fallback.error.is_none());
        assert_eq!(fallback.windows.len(), 1);

        // Leave the gate clean for any other test touching the static.
        claude_gate_record_success(&snapshot);
    }

    #[test]
    fn maps_codex_primary_and_secondary_windows() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let rate_limit = CodexRateLimit {
            primary_window: Some(CodexWindow {
                used_percent: 8.0,
                reset_at: 1_700_005_400,
                limit_window_seconds: 18_000,
            }),
            secondary_window: Some(CodexWindow {
                used_percent: 35.0,
                reset_at: 1_700_172_800,
                limit_window_seconds: 604_800,
            }),
        };
        let windows = codex_windows(Some(&rate_limit), None, now);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "Session");
        assert_eq!(windows[0].remaining_percent, 92.0);
        assert_eq!(windows[1].label, "Weekly");
        assert_eq!(windows[1].remaining_percent, 65.0);
    }

    #[test]
    fn maps_codex_additional_model_limits() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let extra = CodexAdditionalRateLimit {
            limit_name: Some("gpt-5.2-codex-spark".to_string()),
            metered_feature: None,
            rate_limit: Some(CodexRateLimit {
                primary_window: Some(CodexWindow {
                    used_percent: 41.0,
                    reset_at: 1_700_003_600,
                    limit_window_seconds: 18_000,
                }),
                secondary_window: None,
            }),
        };
        let windows = codex_windows(None, Some(&[extra]), now);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "Codex Spark");
        assert_eq!(windows[0].remaining_percent, 59.0);
    }

    #[test]
    fn parses_claude_credentials_file() {
        let raw = r#"{
            "claudeAiOauth": {
                "accessToken": "access",
                "refreshToken": "refresh",
                "expiresAt": 1700000000000,
                "scopes": ["user:profile"],
                "rateLimitTier": "max",
                "subscriptionType": "pro"
            }
        }"#;
        let credentials = parse_claude_credentials_data(raw).unwrap();
        assert_eq!(credentials.access_token, "access");
        assert_eq!(credentials.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(credentials.scopes, vec!["user:profile"]);
        assert_eq!(credentials.subscription_type.as_deref(), Some("pro"));
    }

    #[test]
    fn maps_claude_oauth_windows() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let usage = ClaudeUsageResponse {
            five_hour: Some(ClaudeWindow {
                utilization: Some(8.0),
                resets_at: Some("2023-11-14T23:13:20Z".to_string()),
            }),
            seven_day: Some(ClaudeWindow {
                utilization: Some(23.0),
                resets_at: Some("2023-11-17T22:13:20Z".to_string()),
            }),
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: Some(ClaudeWindow {
                utilization: Some(3.0),
                resets_at: None,
            }),
            seven_day_design: Some(ClaudeWindow {
                utilization: Some(0.0),
                resets_at: None,
            }),
            seven_day_routines: None,
            extra_usage: None,
            ..Default::default()
        };
        let windows = claude_windows(&usage, now);
        assert_eq!(windows.len(), 4);
        assert_eq!(windows[0].label, "Session");
        assert_eq!(windows[0].remaining_percent, 92.0);
        assert_eq!(windows[1].label, "Weekly");
        assert_eq!(windows[1].remaining_percent, 77.0);
        assert_eq!(windows[2].label, "Sonnet");
        assert_eq!(windows[2].remaining_percent, 97.0);
        assert_eq!(windows[3].label, "Designs");
        assert_eq!(windows[3].remaining_percent, 100.0);
    }

    #[test]
    fn decodes_claude_alias_windows_without_duplicate_error() {
        let raw = r#"{
            "five_hour": { "utilization": 5, "resets_at": "2026-05-28T14:00:00Z" },
            "seven_day": { "utilization": 23, "resets_at": "2026-05-31T14:00:00Z" },
            "seven_day_sonnet": { "utilization": 3, "resets_at": null },
            "seven_day_omelette": { "utilization": 0, "resets_at": null },
            "omelette_promotional": { "utilization": 0, "resets_at": null },
            "seven_day_cowork": { "utilization": 0, "resets_at": null }
        }"#;
        let usage: ClaudeUsageResponse = serde_json::from_str(raw).unwrap();
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let windows = claude_windows(&usage, now);
        assert_eq!(
            windows.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(),
            vec!["Session", "Weekly", "Sonnet", "Designs", "Daily Routines"]
        );
    }
}
