use crate::agent_account_scope::{
    self, AccountScope, AccountScopeError, AuthoritativeIdKind, RefreshCheckpoint,
    RefreshScopeTransaction,
};
use crate::agent_antigravity;
use crate::agent_copilot;
use crate::agent_grok;
use crate::agent_quota_duration::{DurationEvidence, DurationSource, DurationUnavailableReason};
use crate::agent_quota_history::{
    BatchObservationResult, HistoricalPace, HistoryError, HistoryOutcome, QuotaObservation,
    SeriesKey,
};
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
// Minimal-request endpoint whose response headers carry the unified rate-limit
// windows. Used as a fallback for inference-only `claude setup-token` tokens,
// which get HTTP 403 on the oauth/usage endpoint (it requires user:profile).
const CLAUDE_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
// Cheapest model for the header probe. Alias (not a dated snapshot) so it
// outlives model retirements.
const CLAUDE_PROBE_MODEL: &str = "claude-haiku-4-5";
// Keychain generic-password service holding a RAW setup-token (`sk-ant-oat01-…`),
// the launch-method-independent way to hand TokenBar a token for the limits card:
//   security add-generic-password -a "$USER" -s tokenbar-claude-oauth-token -w "<token>"
const CLAUDE_RAW_TOKEN_KEYCHAIN_SERVICE: &str = "tokenbar-claude-oauth-token";

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
    #[serde(skip)]
    pub(crate) account_scope: Result<AccountScope, AccountScopeError>,
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
pub struct HistoricalPacePayload {
    pub(crate) expected_used_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) eta_seconds: Option<f64>,
    pub(crate) will_last_to_reset: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) run_out_probability: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum PaceState {
    LearningDuration,
    LearningHistory,
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PaceStatusPayload {
    state: PaceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_source: Option<DurationSource>,
    complete_cycles: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UsageWindow {
    card_id: String,
    label: String,
    used_percent: f64,
    remaining_percent: f64,
    resets_at: Option<String>,
    reset_text: Option<String>,
    /// Legacy compatibility only. Wire serialization derives this from
    /// `duration_seconds`; provider adapters must never use this as identity.
    window_minutes: Option<i64>,
    window_key: Option<String>,
    duration_seconds: Option<i64>,
    duration_source: Option<DurationSource>,
    provider_duration: Option<DurationEvidence>,
    contract_duration: Option<DurationEvidence>,
    pace_status: PaceStatusPayload,
    historical_pace: Option<HistoricalPacePayload>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditsSnapshot {
    remaining: Option<f64>,
    unlimited: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageWindowWire<'a> {
    card_id: &'a str,
    label: &'a str,
    used_percent: f64,
    remaining_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_minutes: Option<i64>,
    pace_status: &'a PaceStatusPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    historical_pace: Option<&'a HistoricalPacePayload>,
}

impl Serialize for UsageWindow {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate_wire().map_err(serde::ser::Error::custom)?;
        UsageWindowWire {
            card_id: &self.card_id,
            label: &self.label,
            used_percent: self.used_percent,
            remaining_percent: self.remaining_percent,
            resets_at: self.resets_at.as_deref(),
            reset_text: self.reset_text.as_deref(),
            window_minutes: self.duration_seconds.map(|seconds| seconds / 60),
            pace_status: &self.pace_status,
            historical_pace: self.historical_pace.as_ref(),
        }
        .serialize(serializer)
    }
}

impl UsageWindow {
    /// Build a window from a "remaining fraction" (0..1) — the shape Antigravity
    /// reports per model. Used-percent is derived; identity and duration are
    /// attached by the provider adapter before the snapshot is emitted.
    pub(crate) fn from_fraction(
        label: String,
        remaining_fraction: f64,
        resets_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Self {
        Self::from_used_percent(
            label,
            (1.0 - remaining_fraction) * 100.0,
            resets_at,
            now,
            None,
        )
    }

    /// Build a window from an absolute used-percent (0..100), with an optional
    /// legacy duration hint. The hint is retained only for existing tests and
    /// converted to exact seconds before any wire serialization.
    pub(crate) fn from_used_percent(
        label: String,
        used_percent: f64,
        resets_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        window_minutes: Option<i64>,
    ) -> Self {
        let used = used_percent.clamp(0.0, 100.0);
        let remaining = (100.0 - used).clamp(0.0, 100.0);
        let duration_seconds = window_minutes
            .filter(|minutes| *minutes > 0)
            .and_then(|minutes| minutes.checked_mul(60));
        let mut window = UsageWindow {
            card_id: "row.unassigned.v1".to_string(),
            label,
            used_percent: used,
            remaining_percent: remaining,
            resets_at: resets_at.map(|d| d.to_rfc3339_opts(SecondsFormat::Millis, true)),
            reset_text: resets_at.map(|d| reset_text(d, now)),
            window_minutes,
            window_key: None,
            duration_seconds,
            duration_source: duration_seconds.map(|_| DurationSource::Contract),
            provider_duration: None,
            contract_duration: duration_seconds.map(DurationEvidence::contract),
            pace_status: PaceStatusPayload {
                state: PaceState::Unavailable,
                window_key: None,
                duration_seconds: None,
                duration_source: None,
                complete_cycles: 0,
                reason: Some("windowIdentity".to_string()),
            },
            historical_pace: None,
        };
        window.refresh_initial_pace_status();
        window
    }

    /// Preserve the raw provider reading until identity is attached so the
    /// generic adapter can classify invalid evidence. `with_identity` then
    /// restores finite display percentages before any wire serialization.
    pub(crate) fn from_provider_used_percent(
        label: String,
        used_percent: f64,
        resets_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Self {
        let mut window = Self::from_used_percent(label, used_percent, resets_at, now, None);
        window.used_percent = used_percent;
        window.remaining_percent = 100.0 - used_percent;
        window
    }

    pub(crate) fn from_provider_fraction(
        label: String,
        remaining_fraction: f64,
        resets_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Self {
        Self::from_provider_used_percent(label, (1.0 - remaining_fraction) * 100.0, resets_at, now)
    }

    pub(crate) fn try_from_provider_used_percent(
        label: String,
        used_percent: f64,
        resets_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Option<Self> {
        (used_percent.is_finite() && (0.0..=100.0).contains(&used_percent))
            .then(|| Self::from_provider_used_percent(label, used_percent, resets_at, now))
    }

    pub(crate) fn try_from_provider_fraction(
        label: String,
        remaining_fraction: f64,
        resets_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Option<Self> {
        (remaining_fraction.is_finite() && (0.0..=1.0).contains(&remaining_fraction))
            .then(|| Self::from_provider_fraction(label, remaining_fraction, resets_at, now))
    }

    /// Attach provider-semantic presentation and history identity plus the
    /// frozen provider/contract duration evidence.
    pub(crate) fn with_identity(
        mut self,
        card_id: impl Into<String>,
        window_key: Option<String>,
        provider_duration: Option<DurationEvidence>,
        contract_duration: Option<DurationEvidence>,
    ) -> Self {
        let invalid_reading =
            !self.used_percent.is_finite() || !(0.0..=100.0).contains(&self.used_percent);
        if invalid_reading {
            self.used_percent = if self.used_percent.is_finite() {
                self.used_percent.clamp(0.0, 100.0)
            } else {
                0.0
            };
            self.remaining_percent = 100.0 - self.used_percent;
        }
        self.card_id = card_id.into();
        self.window_key = window_key;
        self.provider_duration = provider_duration;
        self.contract_duration = contract_duration;
        self.duration_seconds = self
            .provider_duration
            .or(self.contract_duration)
            .map(|evidence| evidence.duration_seconds)
            .filter(|duration| *duration > 0);
        self.duration_source = if self.provider_duration.is_some() {
            Some(DurationSource::Provider)
        } else if self.contract_duration.is_some() {
            Some(DurationSource::Contract)
        } else {
            None
        };
        self.window_minutes = self.duration_seconds.map(|seconds| seconds / 60);
        self.refresh_initial_pace_status();
        if invalid_reading && self.window_key.is_some() {
            self.unavailable("invalidEvidence");
        }
        self
    }

    fn refresh_initial_pace_status(&mut self) {
        if self.window_key.is_none() {
            self.duration_seconds = None;
            self.duration_source = None;
            self.window_minutes = None;
            self.pace_status = PaceStatusPayload {
                state: PaceState::Unavailable,
                window_key: None,
                duration_seconds: None,
                duration_source: None,
                complete_cycles: 0,
                reason: Some("windowIdentity".to_string()),
            };
            self.historical_pace = None;
            return;
        }
        if self.resets_at.is_none() {
            self.duration_seconds = None;
            self.duration_source = None;
            self.window_minutes = None;
            self.pace_status = PaceStatusPayload {
                state: PaceState::Unavailable,
                window_key: self.window_key.clone(),
                duration_seconds: None,
                duration_source: None,
                complete_cycles: 0,
                reason: Some("missingReset".to_string()),
            };
            self.historical_pace = None;
            return;
        }
        let state = if self.duration_seconds.is_some() {
            PaceState::LearningHistory
        } else {
            PaceState::LearningDuration
        };
        self.pace_status = PaceStatusPayload {
            state,
            window_key: self.window_key.clone(),
            duration_seconds: self.duration_seconds,
            duration_source: self.duration_source,
            complete_cycles: 0,
            reason: None,
        };
        self.historical_pace = None;
    }

    pub(crate) fn unavailable(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        self.duration_seconds = None;
        self.duration_source = None;
        self.window_minutes = None;
        self.historical_pace = None;
        self.pace_status = PaceStatusPayload {
            state: PaceState::Unavailable,
            window_key: self.window_key.clone(),
            duration_seconds: None,
            duration_source: None,
            complete_cycles: 0,
            reason: Some(reason),
        };
    }

    fn validate_wire(&self) -> Result<(), String> {
        if self.card_id.trim().is_empty() {
            return Err("pace cardId must be non-empty".to_string());
        }
        if self.window_key != self.pace_status.window_key {
            return Err("pace windowKey internal and nested values differ".to_string());
        }
        if self.duration_seconds != self.pace_status.duration_seconds {
            return Err("pace durationSeconds internal and nested values differ".to_string());
        }
        if self.duration_source != self.pace_status.duration_source {
            return Err("pace durationSource internal and nested values differ".to_string());
        }
        if self.window_minutes != self.duration_seconds.map(|seconds| seconds / 60) {
            return Err("pace windowMinutes must derive from durationSeconds".to_string());
        }
        if self.duration_seconds.is_none()
            && self.duration_source.is_some()
            && !(self.pace_status.state == PaceState::LearningDuration
                && self.duration_source == Some(DurationSource::Observed))
        {
            return Err("pace durationSource requires a duration".to_string());
        }
        if let Some(window_key) = self.pace_status.window_key.as_deref() {
            if window_key.trim().is_empty() {
                return Err("pace windowKey must be non-empty".to_string());
            }
        }
        let identity_unavailable = self.pace_status.state == PaceState::Unavailable
            && self.pace_status.reason.as_deref() == Some("windowIdentity");
        if self.pace_status.window_key.is_none() != identity_unavailable {
            return Err("pace windowKey identity invariant failed".to_string());
        }
        if let Some(duration) = self.pace_status.duration_seconds {
            if duration <= 0 {
                return Err("pace durationSeconds must be positive".to_string());
            }
            if self.pace_status.duration_source.is_none() {
                return Err("pace durationSource is required with durationSeconds".to_string());
            }
        }
        match self.pace_status.state {
            PaceState::Available => {
                if self.pace_status.duration_seconds.is_none() || self.historical_pace.is_none() {
                    return Err("available pace requires duration and historicalPace".to_string());
                }
            }
            PaceState::LearningHistory => {
                if self.pace_status.duration_seconds.is_none() || self.historical_pace.is_some() {
                    return Err("learningHistory pace invariant failed".to_string());
                }
            }
            PaceState::LearningDuration => {
                if self.pace_status.duration_seconds.is_some() || self.historical_pace.is_some() {
                    return Err("learningDuration pace invariant failed".to_string());
                }
            }
            PaceState::Unavailable => {
                if self.historical_pace.is_some() || self.pace_status.reason.as_deref().is_none() {
                    return Err("unavailable pace invariant failed".to_string());
                }
            }
        }
        if let Some(historical) = &self.historical_pace {
            if !historical.expected_used_percent.is_finite()
                || !(0.0..=100.0).contains(&historical.expected_used_percent)
                || historical
                    .eta_seconds
                    .is_some_and(|eta| !eta.is_finite() || eta < 0.0)
                || historical.run_out_probability.is_some_and(|probability| {
                    !probability.is_finite() || !(0.0..=1.0).contains(&probability)
                })
                || (historical.eta_seconds.is_none() != historical.will_last_to_reset)
            {
                return Err("historicalPace contains contradictory values".to_string());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn label_for_test(&self) -> &str {
        &self.label
    }

    #[cfg(test)]
    pub(crate) fn remaining_for_test(&self) -> f64 {
        self.remaining_percent
    }

    #[cfg(test)]
    pub(crate) fn resets_at_for_test(&self) -> Option<&str> {
        self.resets_at.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn window_minutes_for_test(&self) -> Option<i64> {
        self.duration_seconds.map(|seconds| seconds / 60)
    }

    #[cfg(test)]
    pub(crate) fn pace_window_key_for_test(&self) -> Option<&str> {
        self.pace_status.window_key.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn pace_reason_for_test(&self) -> Option<&str> {
        self.pace_status.reason.as_deref()
    }
}

#[derive(Debug, Clone)]
struct CredentialSlot {
    semantic_source: &'static str,
    canonical_location: String,
}

#[derive(Debug, Clone)]
struct ResolvedClaudeToken {
    access_token: String,
    scope_slot: CredentialSlot,
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
    scope_slot: CredentialSlot,
}

impl CodexCredentials {
    fn scope_marker(&self) -> &[u8] {
        self.refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .unwrap_or_else(|| self.access_token.trim())
            .as_bytes()
    }
}

#[derive(Debug, Clone)]
struct ClaudeCredentials {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    scopes: Vec<String>,
    rate_limit_tier: Option<String>,
    subscription_type: Option<String>,
    /// Where the credentials were read from, so a rotated token can be written
    /// back to the same place (the Claude CLI shares this store).
    source: ClaudeCredentialSource,
    /// Full credentials JSON as loaded, so a write-back preserves fields we
    /// don't model (merge-update rather than overwrite).
    raw_root: Option<Value>,
    scope_slot: CredentialSlot,
}

impl ClaudeCredentials {
    fn scope_marker(&self) -> Option<&[u8]> {
        match self.source {
            ClaudeCredentialSource::Keychain | ClaudeCredentialSource::File => self
                .refresh_token
                .as_deref()
                .filter(|token| !token.is_empty())
                .map(str::as_bytes),
            ClaudeCredentialSource::Environment => Some(self.access_token.as_bytes()),
        }
    }

    fn resolve_account_scope(&self) -> Result<AccountScope, AccountScopeError> {
        let marker = self
            .scope_marker()
            .ok_or(AccountScopeError::NoTrustedEvidence)?;
        agent_account_scope::resolve_credential(
            "claude",
            self.scope_slot.semantic_source,
            &self.scope_slot.canonical_location,
            marker,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeCredentialSource {
    Keychain,
    File,
    /// Token injected via env var — read-only, has no refresh token.
    Environment,
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
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    primary_window: Option<CodexWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
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
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    five_hour: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day_oauth_apps: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day_opus: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day_sonnet: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day_design: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day_claude_design: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    claude_design: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    design: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day_omelette: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    omelette: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    omelette_promotional: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day_routines: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day_claude_routines: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    claude_routines: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    routines: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    routine: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    seven_day_cowork: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    cowork: Option<ClaudeWindow>,
    #[serde(default, deserialize_with = "deserialize_optional_raw")]
    extra_usage: Option<ClaudeExtraUsage>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClaudeWindow {
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
}

impl ClaudeWindow {
    fn has_valid_utilization(&self) -> bool {
        self.utilization
            .is_some_and(|used| used.is_finite() && (0.0..=100.0).contains(&used))
    }
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
    #[serde(default, deserialize_with = "deserialize_optional_non_empty_string")]
    refresh_token: Option<String>,
    expires_in: i64,
}

pub async fn run() -> AgentUsagePayload {
    let generated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let (codex, claude, antigravity, copilot, grok) = tokio::join!(
        fetch_codex(),
        fetch_claude(),
        fetch_antigravity(),
        fetch_copilot(),
        fetch_grok()
    );
    let mut agents = vec![codex, claude, antigravity];
    // Copilot only appears when signed in (via opencode); skip a bare not-signed-in error card.
    if let Some(copilot) = copilot {
        agents.push(copilot);
    }
    // Grok only appears when ~/.grok/auth.json has credentials.
    if let Some(grok) = grok {
        agents.push(grok);
    }
    AgentUsagePayload {
        generated_at,
        agents,
        opencode_subscriptions: crate::opencode_integrations::detect_subscriptions(),
    }
}

async fn fetch_grok() -> Option<AgentUsageSnapshot> {
    let now = Utc::now();
    let mut snapshot = match agent_grok::fetch(now).await? {
        Ok(data) => AgentUsageSnapshot {
            client_id: "grok".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: data.identity,
            account_scope: data.account_scope,
            windows: data.windows,
            credits: None,
            error: None,
        },
        Err(error) => AgentUsageSnapshot {
            client_id: "grok".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: None,
            account_scope: Err(AccountScopeError::NoTrustedEvidence),
            windows: Vec::new(),
            credits: None,
            error: Some(error),
        },
    };
    enrich_snapshot(&mut snapshot, now.timestamp());
    Some(snapshot)
}

async fn fetch_copilot() -> Option<AgentUsageSnapshot> {
    // No opencode Copilot auth → no card at all (rather than an error row).
    let credential = crate::opencode_integrations::github_copilot_credential()?;
    let now = Utc::now();
    let mut snapshot = match agent_copilot::fetch(now, credential).await {
        Ok(data) => AgentUsageSnapshot {
            client_id: "copilot".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: data.identity,
            account_scope: data.account_scope,
            windows: data.windows,
            credits: None,
            error: None,
        },
        Err(error) => AgentUsageSnapshot {
            client_id: "copilot".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: None,
            account_scope: Err(AccountScopeError::NoTrustedEvidence),
            windows: Vec::new(),
            credits: None,
            error: Some(error),
        },
    };
    enrich_snapshot(&mut snapshot, now.timestamp());
    Some(snapshot)
}

async fn fetch_antigravity() -> AgentUsageSnapshot {
    let now = Utc::now();
    let mut snapshot = match agent_antigravity::fetch(now).await {
        Ok(fetched) => AgentUsageSnapshot {
            client_id: "antigravity".to_string(),
            source: fetched.source,
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: fetched.identity,
            account_scope: fetched.account_scope,
            windows: fetched.windows,
            credits: None,
            error: None,
        },
        Err(error) => AgentUsageSnapshot {
            client_id: "antigravity".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: None,
            account_scope: Err(AccountScopeError::NoTrustedEvidence),
            windows: Vec::new(),
            credits: None,
            error: Some(error),
        },
    };
    enrich_snapshot(&mut snapshot, now.timestamp());
    snapshot
}

async fn fetch_codex() -> AgentUsageSnapshot {
    match fetch_codex_inner().await {
        Ok(snapshot) => snapshot,
        Err(error) => AgentUsageSnapshot {
            client_id: "codex".to_string(),
            source: "oauth".to_string(),
            updated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: None,
            account_scope: Err(AccountScopeError::NoTrustedEvidence),
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
    if let Some(mut snapshot) = lock_gate().last_good.clone() {
        // A cached 429 response is not current account-scope evidence. Keeping
        // the stale scope here would let the next enrichment write history for
        // an account that was not authenticated by this poll.
        snapshot.account_scope = Err(AccountScopeError::NoTrustedEvidence);
        return snapshot;
    }
    let wait_secs = (blocked_until - now).num_seconds().max(0);
    AgentUsageSnapshot {
        client_id: "claude".to_string(),
        source: "oauth".to_string(),
        updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
        identity: None,
        account_scope: Err(AccountScopeError::NoTrustedEvidence),
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
        Ok(mut snapshot) => {
            enrich_snapshot(&mut snapshot, now.timestamp());
            // Cache the display-ready snapshot. A later 429 fallback returns it
            // without another enrichment pass, so no history write occurs and
            // the last-good typed pace remains intact.
            claude_gate_record_success(&snapshot);
            snapshot
        }
        Err(error) => {
            // A 429 inside fetch_claude_inner arms the gate; fall back to the
            // cached, already-enriched snapshot rather than blanking the card.
            let now = Utc::now();
            if let Some(blocked_until) = claude_gate_blocked_until(now) {
                return claude_gate_fallback(blocked_until, now);
            }

            // "unconfigured" == no credential at all, so the UI shows a setup
            // prompt; every other error is a real failure of a present credential.
            let source = if error.as_str() == CLAUDE_UNCONFIGURED_ERROR {
                "unconfigured"
            } else {
                "oauth"
            };
            let mut snapshot = AgentUsageSnapshot {
                client_id: "claude".to_string(),
                source: source.to_string(),
                updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
                identity: None,
                account_scope: Err(AccountScopeError::NoTrustedEvidence),
                windows: Vec::new(),
                credits: None,
                error: Some(error),
            };
            enrich_snapshot(&mut snapshot, now.timestamp());
            snapshot
        }
    }
}

async fn fetch_codex_inner() -> Result<AgentUsageSnapshot, String> {
    let mut credentials = load_codex_credentials()?;
    let mut refreshed_scope = None;
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
        let refreshed = refresh_codex_credentials(&credentials.auth_path).await?;
        credentials = refreshed.0;
        refreshed_scope = Some(refreshed.1);
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
    let request_account_id = credentials
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(account_id) = request_account_id {
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
    let account_scope = resolve_codex_account_scope(
        refreshed_scope,
        request_account_id,
        |account_id| {
            agent_account_scope::resolve_authoritative(
                "codex",
                AuthoritativeIdKind::OpaqueId,
                account_id,
            )
        },
        || {
            agent_account_scope::resolve_credential(
                "codex",
                credentials.scope_slot.semantic_source,
                &credentials.scope_slot.canonical_location,
                credentials.scope_marker(),
            )
        },
    );
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
    let windows = codex_windows(
        usage.rate_limit.as_ref(),
        usage.additional_rate_limits.as_deref(),
        now,
    );
    if windows.is_empty() && usage.credits.as_ref().and_then(|c| c.balance).is_none() {
        return Err("Codex usage API returned no rate-limit windows.".to_string());
    }

    // Legacy v2 migration is deliberately gated on the successful request that
    // actually carried this account header and on the accepted scope result.
    if let (Some(request_account_id), Ok(scope)) = (request_account_id, &account_scope) {
        let _ = crate::agent_quota_history::migrate_codex_v2(
            request_account_id,
            scope.as_str(),
            now.timestamp(),
        );
    }

    let mut snapshot = AgentUsageSnapshot {
        client_id: "codex".to_string(),
        source: "oauth".to_string(),
        updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
        identity,
        account_scope,
        windows,
        credits: usage.credits.map(|credits| CreditsSnapshot {
            remaining: credits.balance,
            unlimited: credits.unlimited,
        }),
        error: None,
    };
    enrich_snapshot(&mut snapshot, now.timestamp());
    Ok(snapshot)
}

fn resolve_codex_account_scope<ResolveAuthoritative, ResolveCredential>(
    refreshed_scope: Option<Result<AccountScope, AccountScopeError>>,
    request_account_id: Option<&str>,
    resolve_authoritative: ResolveAuthoritative,
    resolve_credential: ResolveCredential,
) -> Result<AccountScope, AccountScopeError>
where
    ResolveAuthoritative: FnOnce(&str) -> Result<AccountScope, AccountScopeError>,
    ResolveCredential: FnOnce() -> Result<AccountScope, AccountScopeError>,
{
    if let Some(Err(error)) = refreshed_scope.as_ref() {
        return Err(*error);
    }
    if let Some(account_id) = request_account_id {
        return resolve_authoritative(account_id);
    }
    refreshed_scope.unwrap_or_else(resolve_credential)
}

async fn fetch_claude_inner() -> Result<AgentUsageSnapshot, String> {
    // Mirror Claude Code's auth precedence: CLAUDE_CODE_OAUTH_TOKEN (our env, or
    // harvested from the user's ~/.zshrc) outranks a stored subscription /login,
    // because Claude Code itself consumes that token first. So TokenBar reports
    // the account Claude Code is actually spending against, read from the
    // ratelimit headers. (This is why the harvest runs even for /login users.)
    if let Some(token) = resolve_claude_code_oauth_token().await {
        return claude_header_snapshot(
            &claude_credentials_from_access_token(token),
            Utc::now(),
            None,
        )
        .await;
    }

    // A stored full login (TokenBar env override / Keychain / file) uses the
    // richer oauth/usage endpoint. Any failure -- a login that can't refresh, or
    // a credentials file that exists but can't be read (permissions / I/O) -- is
    // deferred: we still try the tokenbar Keychain setup-token below, and surface
    // the error only if that misses too. So a stale login / read error never
    // strands a working setup-token, yet a genuine failure isn't masked by the
    // generic "unconfigured" setup prompt.
    let deferred_error: Option<String> = match load_claude_login_credentials() {
        Ok(Some(credentials)) => match fetch_claude_oauth_usage(credentials).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(login_error) => Some(login_error),
        },
        Ok(None) => None,
        Err(read_error) => Some(read_error),
    };

    // Last resort: the tokenbar-claude-oauth-token Keychain item reads limits
    // straight from the ratelimit headers (no oauth/usage GET, no 429 gate).
    if let Some(token) = resolve_claude_keychain_token() {
        return claude_header_snapshot(
            &claude_credentials_from_access_token(token),
            Utc::now(),
            None,
        )
        .await;
    }

    Err(deferred_error.unwrap_or_else(|| CLAUDE_UNCONFIGURED_ERROR.to_string()))
}

async fn fetch_claude_oauth_usage(
    mut credentials: ClaudeCredentials,
) -> Result<AgentUsageSnapshot, String> {
    let mut refreshed_scope = None;
    if claude_credentials_expired(&credentials) {
        let refreshed = refresh_claude_credentials(&credentials).await?;
        credentials = refreshed.0;
        refreshed_scope = Some(refreshed.1);
    }

    if !credentials.scopes.is_empty()
        && !credentials
            .scopes
            .iter()
            .any(|scope| scope == "user:profile")
    {
        // Inference-only token declared explicit non-user:profile scopes — skip
        // the (guaranteed-403) oauth/usage GET and read limits from headers.
        return claude_header_snapshot(&credentials, Utc::now(), refreshed_scope).await;
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
        // oauth/usage requires user:profile. An inference-only token (e.g.
        // `claude setup-token`) is denied *specifically* for that scope — fall
        // back to the unified rate-limit headers, which it *is* allowed to read.
        // Any other 403 keeps the actionable re-auth error (and skips the probe,
        // so we don't spend an inference call on an unrelated denial).
        if body.contains("user:profile") {
            return claude_header_snapshot(&credentials, Utc::now(), refreshed_scope).await;
        }
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
    let account_scope = refreshed_scope.unwrap_or_else(|| credentials.resolve_account_scope());

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
        account_scope,
        windows,
        credits: claude_credits(usage.extra_usage.as_ref()),
        error: None,
    })
}

/// Fallback for inference-only tokens (`claude setup-token`): the oauth/usage
/// endpoint requires `user:profile`, but a minimal `/v1/messages` request the
/// token *can* make returns `anthropic-ratelimit-unified-*` headers carrying the
/// same Session/Weekly windows. Reads headers on 200 AND 429 (an over-limit
/// token still returns them). Does NOT arm the oauth/usage rate-limit gate.
/// Cache for the header-probe windows. The probe is a real `/v1/messages`
/// inference (it spends the very budget it measures), so reuse the result across
/// the frequent quota polls (60s popover / 300s tray) instead of probing on
/// every refresh. Keyed on the token so a changed token re-probes.
/// `(fetched_at, token, windows)` — the token keys the entry so a changed token
/// re-probes rather than serving another account's cached windows.
type ClaudeHeaderCacheEntry = (DateTime<Utc>, String, Vec<UsageWindow>);
static CLAUDE_HEADER_CACHE: Mutex<Option<ClaudeHeaderCacheEntry>> = Mutex::new(None);
const CLAUDE_HEADER_TTL_SECS: i64 = 300;

/// Refresh the relative `reset_text` on cached header windows so a 300s-cached
/// probe doesn't show a frozen countdown. Returns None if any window's reset has
/// already passed — the cache is then stale, so the caller re-probes for fresh
/// utilization instead of serving post-reset numbers.
fn refresh_cached_windows(windows: &[UsageWindow], now: DateTime<Utc>) -> Option<Vec<UsageWindow>> {
    let mut refreshed = Vec::with_capacity(windows.len());
    for window in windows {
        let mut window = window.clone();
        if let Some(reset) = window.resets_at.as_deref().and_then(parse_datetime) {
            if now >= reset {
                return None;
            }
            window.reset_text = Some(reset_text(reset, now));
        }
        refreshed.push(window);
    }
    Some(refreshed)
}

async fn fetch_claude_via_headers(access_token: &str) -> Result<Vec<UsageWindow>, String> {
    {
        let now = Utc::now();
        let guard = CLAUDE_HEADER_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((fetched_at, token, windows)) = guard.as_ref() {
            if token == access_token && (now - *fetched_at).num_seconds() < CLAUDE_HEADER_TTL_SECS {
                if let Some(refreshed) = refresh_cached_windows(windows, now) {
                    return Ok(refreshed);
                }
            }
        }
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build Claude header-probe client: {}", e))?;

    let response = client
        .post(CLAUDE_MESSAGES_URL)
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, claude_user_agent())
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .json(&serde_json::json!({
            "model": CLAUDE_PROBE_MODEL,
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .map_err(|e| format!("Claude header probe failed: {}", e))?;

    let status = response.status();
    // Read headers before consuming the body — this returns an owned Vec, ending
    // the borrow of `response`.
    let windows = parse_unified_ratelimit_windows(response.headers(), Utc::now());

    if status.is_success() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        if windows.is_empty() {
            return Err("Claude header probe returned no unified rate-limit headers.".to_string());
        }
        {
            let mut guard = CLAUDE_HEADER_CACHE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *guard = Some((Utc::now(), access_token.to_string(), windows.clone()));
        }
        return Ok(windows);
    }

    let body = response.text().await.unwrap_or_default();
    Err(format!(
        "Claude header probe returned {} ({}).",
        status.as_u16(),
        body.chars().take(200).collect::<String>()
    ))
}

/// Build a Claude snapshot from the unified rate-limit headers. Shared by the
/// scope-guard and HTTP-403 branches of `fetch_claude_inner`. `source` is
/// `"setup-token"` — it doubles as the limits-card badge, so it names the auth
/// method the user recognizes rather than the fetch mechanism, and still lets
/// telemetry tell it apart from the richer oauth/usage path.
async fn claude_header_snapshot(
    credentials: &ClaudeCredentials,
    now: DateTime<Utc>,
    account_scope: Option<Result<AccountScope, AccountScopeError>>,
) -> Result<AgentUsageSnapshot, String> {
    let windows = fetch_claude_via_headers(&credentials.access_token).await?;
    let account_scope = account_scope.unwrap_or_else(|| credentials.resolve_account_scope());
    Ok(AgentUsageSnapshot {
        client_id: "claude".to_string(),
        source: "setup-token".to_string(),
        updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
        identity: Some(AgentIdentity {
            email: None,
            plan: first_non_empty([
                credentials.subscription_type.as_deref(),
                credentials.rate_limit_tier.as_deref(),
            ])
            .map(clean_plan),
        }),
        account_scope,
        windows,
        credits: None,
        error: None,
    })
}

fn load_codex_credentials() -> Result<CodexCredentials, String> {
    load_codex_credentials_from(&codex_home().join("auth.json"))
}

fn load_codex_credentials_from(auth_path: &Path) -> Result<CodexCredentials, String> {
    let raw = fs::read_to_string(auth_path)
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
        auth_path: auth_path.to_path_buf(),
        raw_json,
        scope_slot: CredentialSlot {
            semantic_source: "codex-auth-json",
            canonical_location: agent_account_scope::canonical_file_location(
                auth_path,
                Some("tokens"),
            )
            .map_err(|_| "Codex auth location cannot be scoped safely.".to_string())?,
        },
    })
}

/// Marker error for "no Claude credential is configured at all" (as opposed to a
/// credential that exists but failed). `fetch_claude` turns this into a snapshot
/// with `source == "unconfigured"`, so the UI shows a setup prompt rather than a
/// red error.
const CLAUDE_UNCONFIGURED_ERROR: &str = "Claude OAuth credentials not found. Run `claude` to authenticate, or set CLAUDE_CODE_OAUTH_TOKEN / add a `tokenbar-claude-oauth-token` Keychain item to use a setup-token.";

/// Full-login credentials: structured `claudeAiOauth` blobs (Keychain
/// `Claude Code-credentials`, then `~/.claude/.credentials.json`) plus the
/// TokenBar env override. These carry refresh tokens / scopes / expiry and go
/// through the richer oauth/usage endpoint. A present-but-logged-out entry (has
/// `claudeAiOauth` but no `accessToken` — the #26 daily-logout state) or an
/// unparseable blob is skipped, not treated as a hard error, so a configured
/// setup-token can still take over.
fn load_claude_login_credentials() -> Result<Option<ClaudeCredentials>, String> {
    if let Some(credentials) = load_claude_credentials_from_environment()? {
        return Ok(Some(credentials));
    }
    if let Some(raw) = load_claude_credentials_from_keychain()? {
        if let Ok(credentials) =
            parse_claude_credentials_data(&raw, ClaudeCredentialSource::Keychain)
        {
            return Ok(Some(credentials));
        }
    }
    match fs::read_to_string(claude_credentials_path()) {
        Ok(raw) => {
            if let Ok(credentials) =
                parse_claude_credentials_data(&raw, ClaudeCredentialSource::File)
            {
                return Ok(Some(credentials));
            }
            // Parsed but unusable (logged-out / no accessToken): fall through.
            Ok(None)
        }
        // Absent is normal (no file login). A genuine read failure (permissions /
        // I/O) is a real problem — return it so the caller can surface the
        // actionable error after setup-token fallbacks miss, rather than the
        // generic "unconfigured" setup prompt.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "read Claude credentials file {}: {}",
            claude_credentials_path().display(),
            error
        )),
    }
}

/// `CLAUDE_CODE_OAUTH_TOKEN` as Claude Code itself resolves it: this process's
/// own environment (covers `launchctl setenv` / terminal launch), then a
/// login-shell harvest of the user's `~/.zshrc` (so a plain export a
/// Finder-launched GUI app never inherits is still found). Per Claude Code's
/// auth precedence this outranks a stored subscription `/login`.
async fn resolve_claude_code_oauth_token() -> Option<ResolvedClaudeToken> {
    if let Some(access_token) = claude_direct_env_token() {
        return Some(ResolvedClaudeToken {
            access_token,
            scope_slot: CredentialSlot {
                semantic_source: "claude-code-environment",
                canonical_location: "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            },
        });
    }
    harvest_shell_env_token()
        .await
        .map(|access_token| ResolvedClaudeToken {
            access_token,
            scope_slot: CredentialSlot {
                semantic_source: "claude-code-login-shell",
                canonical_location: "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            },
        })
}

/// The `tokenbar-claude-oauth-token` Keychain item (a TokenBar-specific setup
/// token). A last-resort fallback, below the stored `/login`.
fn resolve_claude_keychain_token() -> Option<ResolvedClaudeToken> {
    load_claude_raw_token_from_keychain()
        .ok()
        .flatten()
        .map(|access_token| ResolvedClaudeToken {
            access_token,
            scope_slot: CredentialSlot {
                semantic_source: "claude-setup-keychain",
                canonical_location: CLAUDE_RAW_TOKEN_KEYCHAIN_SERVICE.to_string(),
            },
        })
}

fn load_claude_credentials_from_environment() -> Result<Option<ClaudeCredentials>, String> {
    let token = [
        "TOKENBAR_CLAUDE_OAUTH_TOKEN",
        "TOKCAT_CLAUDE_OAUTH_TOKEN",
        "CODEXBAR_CLAUDE_OAUTH_TOKEN",
    ]
    .into_iter()
    .find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|value| (name, value))
    });
    let Some((source_name, access_token)) = token else {
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
        source: ClaudeCredentialSource::Environment,
        raw_root: None,
        scope_slot: CredentialSlot {
            semantic_source: "claude-environment",
            canonical_location: source_name.to_string(),
        },
    }))
}

fn parse_claude_credentials_data(
    raw: &str,
    source: ClaudeCredentialSource,
) -> Result<ClaudeCredentials, String> {
    let raw_root: Value =
        serde_json::from_str(raw).map_err(|e| format!("decode Claude OAuth credentials: {}", e))?;
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
        source,
        raw_root: Some(raw_root),
        scope_slot: claude_login_scope_slot(source)?,
    })
}

fn claude_login_scope_slot(source: ClaudeCredentialSource) -> Result<CredentialSlot, String> {
    match source {
        ClaudeCredentialSource::Keychain => Ok(CredentialSlot {
            semantic_source: "claude-login-keychain",
            canonical_location: CLAUDE_KEYCHAIN_SERVICE.to_string(),
        }),
        ClaudeCredentialSource::File => Ok(CredentialSlot {
            semantic_source: "claude-login-file",
            canonical_location: agent_account_scope::canonical_file_location(
                &claude_credentials_path(),
                Some("claudeAiOauth"),
            )
            .map_err(|_| "Claude credential location cannot be scoped safely.".to_string())?,
        }),
        ClaudeCredentialSource::Environment => {
            Err("environment credentials require an explicit account-scope slot".to_string())
        }
    }
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

/// Build credentials from a bare access token (no refresh/expiry/scope metadata).
/// Used by the setup-token delivery paths (env var, shell harvest, raw keychain);
/// empty scopes make `fetch_claude_inner` skip the scope guard and reach the
/// header fallback on the resulting oauth/usage 403.
fn claude_credentials_from_access_token(token: ResolvedClaudeToken) -> ClaudeCredentials {
    ClaudeCredentials {
        access_token: token.access_token,
        refresh_token: None,
        expires_at: None,
        scopes: Vec::new(),
        rate_limit_tier: None,
        subscription_type: None,
        // A bare setup-token has no refresh token and no backing store to write
        // to, so treat it as read-only — save_claude_credentials skips it.
        source: ClaudeCredentialSource::Environment,
        raw_root: None,
        scope_slot: token.scope_slot,
    }
}

/// C — `CLAUDE_CODE_OAUTH_TOKEN` from this process's own environment (covers
/// `launchctl setenv` and terminal-launched runs).
fn claude_direct_env_token() -> Option<String> {
    claude_token_from_lookup(|key| std::env::var(key).ok())
}

fn claude_token_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    lookup("CLAUDE_CODE_OAUTH_TOKEN")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Cache for the shell-harvested token — harvesting spawns a full interactive
/// login shell, so we do it at most once per TTL rather than per poll.
static CLAUDE_HARVEST_CACHE: Mutex<Option<(DateTime<Utc>, Option<String>)>> = Mutex::new(None);
// A found token rarely changes → cache it for an hour. Because the harvest now
// runs for every user (to mirror Claude Code's CLAUDE_CODE_OAUTH_TOKEN-before-
// /login precedence), a miss is also cached for a while so we don't re-spawn a
// login shell on every poll; a freshly-added `~/.zshrc` export is picked up
// within this window, or immediately on app restart (which clears the cache).
const CLAUDE_HARVEST_TTL_SECS: i64 = 3600;
const CLAUDE_HARVEST_NEGATIVE_TTL_SECS: i64 = 1800;

/// D — harvest `CLAUDE_CODE_OAUTH_TOKEN` from the user's login shell, so a plain
/// `~/.zshrc` export is picked up even though a Finder/login-item GUI app does
/// not inherit shell environments. Cached; returns None on timeout/miss so the
/// keychain fallback can still fire.
async fn harvest_shell_env_token() -> Option<String> {
    // Scope the guard so it is dropped before the `.await` below (never hold a
    // std Mutex across an await). Recover a poisoned lock (like `lock_gate`) so a
    // stray panic can't permanently disable the cache and reintroduce a per-poll
    // shell spawn.
    {
        let guard = CLAUDE_HARVEST_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((fetched_at, token)) = guard.as_ref() {
            let ttl = if token.is_some() {
                CLAUDE_HARVEST_TTL_SECS
            } else {
                CLAUDE_HARVEST_NEGATIVE_TTL_SECS
            };
            if (Utc::now() - *fetched_at).num_seconds() < ttl {
                return token.clone();
            }
        }
    }
    let token = harvest_shell_env_token_uncached().await;
    {
        let mut guard = CLAUDE_HARVEST_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some((Utc::now(), token.clone()));
    }
    token
}

#[cfg(target_os = "macos")]
async fn harvest_shell_env_token_uncached() -> Option<String> {
    // Interactive (-i) so ~/.zshrc is sourced (login -l alone runs ~/.zprofile
    // only). Null-delimited markers isolate the value from any rc stdout chatter;
    // rc noise (p10k/gitstatus warnings) goes to stderr, which we discard.
    let shell = detect_login_shell();
    let script = "printf '\\0__TB_OAT_S__\\0%s\\0__TB_OAT_E__\\0' \"$CLAUDE_CODE_OAUTH_TOKEN\"";
    let future = tokio::process::Command::new(&shell)
        .args(["-l", "-i", "-c", script])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // On the 5s timeout the future is dropped; kill the child so a hanging rc
        // (e.g. a blocking prompt) doesn't leave an orphaned login shell running.
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), future)
        .await
        .ok()?
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let start_marker = "\0__TB_OAT_S__\0";
    let end_marker = "\0__TB_OAT_E__\0";
    let start = stdout.find(start_marker)? + start_marker.len();
    let rest = &stdout[start..];
    let end = rest.find(end_marker)?;
    let token = rest[..end].trim().to_string();
    (!token.is_empty()).then_some(token)
}

#[cfg(not(target_os = "macos"))]
async fn harvest_shell_env_token_uncached() -> Option<String> {
    None
}

/// Resolve the user's login shell for the harvest. `$SHELL` is usually unset for
/// a launchd-spawned GUI app, so fall back to Directory Services.
#[cfg(target_os = "macos")]
fn detect_login_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL") {
        let shell = shell.trim();
        if !shell.is_empty() {
            return shell.to_string();
        }
    }
    if let Some(user) = current_username() {
        if let Ok(output) = std::process::Command::new("/usr/bin/dscl")
            .args([".", "-read", &format!("/Users/{}", user), "UserShell"])
            .output()
        {
            if output.status.success() {
                if let Ok(text) = String::from_utf8(output.stdout) {
                    // "UserShell: /bin/zsh"
                    if let Some(path) = text.split_whitespace().nth(1) {
                        if !path.is_empty() {
                            return path.to_string();
                        }
                    }
                }
            }
        }
    }
    "/bin/zsh".to_string()
}

#[cfg(target_os = "macos")]
fn current_username() -> Option<String> {
    if let Ok(user) = std::env::var("USER") {
        let user = user.trim();
        if !user.is_empty() {
            return Some(user.to_string());
        }
    }
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-un")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let user = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!user.is_empty()).then_some(user)
}

/// B — a RAW setup-token stored in the `tokenbar-claude-oauth-token` Keychain
/// service. Works regardless of launch method (unlike the env var), which is why
/// it's the reliable fallback for a Finder/login-item GUI app.
#[cfg(target_os = "macos")]
fn load_claude_raw_token_from_keychain() -> Result<Option<String>, String> {
    let output = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            CLAUDE_RAW_TOKEN_KEYCHAIN_SERVICE,
            "-w",
        ])
        .output()
        .map_err(|e| format!("read TokenBar Claude token from Keychain: {}", e))?;
    if !output.status.success() {
        return Ok(None);
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|_| "TokenBar Claude Keychain token is not UTF-8.".to_string())?;
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        return Ok(None);
    }
    Ok(Some(raw))
}

#[cfg(not(target_os = "macos"))]
fn load_claude_raw_token_from_keychain() -> Result<Option<String>, String> {
    Ok(None)
}

async fn refresh_codex_credentials(
    auth_path: &Path,
) -> Result<(CodexCredentials, Result<AccountScope, AccountScopeError>), String> {
    let refresh = agent_account_scope::begin_refresh("codex")
        .map_err(|_| "Codex credential refresh lock is unavailable.".to_string())?;
    refresh_codex_credentials_with(
        auth_path,
        &refresh,
        request_codex_refresh,
        save_codex_credentials,
        |_| Ok(()),
    )
    .await
}

async fn request_codex_refresh(refresh_token: String) -> Result<Value, String> {
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
    serde_json::from_str(&body).map_err(|e| format!("decode Codex refresh response: {}", e))
}

async fn refresh_codex_credentials_with<R, Request, RequestFuture, Save, Checkpoint>(
    auth_path: &Path,
    refresh: &R,
    request: Request,
    save: Save,
    mut checkpoint: Checkpoint,
) -> Result<(CodexCredentials, Result<AccountScope, AccountScopeError>), String>
where
    R: RefreshScopeTransaction + ?Sized,
    Request: FnOnce(String) -> RequestFuture,
    RequestFuture: std::future::Future<Output = Result<Value, String>>,
    Save: FnOnce(&CodexCredentials) -> Result<(), String>,
    Checkpoint: FnMut(RefreshCheckpoint) -> Result<(), String>,
{
    // Another TokenBar process may have refreshed while this caller waited.
    // Reload the exact request-bearing record only after the refresh lock.
    let credentials = load_codex_credentials_from(auth_path)?;
    checkpoint(RefreshCheckpoint::Reloaded)?;
    if !credentials_needs_refresh(credentials.last_refresh) {
        let scope = refresh.resolve_current(
            credentials.scope_slot.semantic_source,
            &credentials.scope_slot.canonical_location,
            credentials.scope_marker(),
        );
        return Ok((credentials, scope));
    }

    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "Codex auth.json has no refresh token.".to_string())?
        .to_string();
    let old_marker = credentials.scope_marker().to_vec();
    let json = request(refresh_token).await?;
    checkpoint(RefreshCheckpoint::NetworkReturned)?;

    let response = json.as_object();
    let refreshed = CodexCredentials {
        access_token: response
            .and_then(|tokens| string_key(tokens, "access_token", "accessToken"))
            .unwrap_or(credentials.access_token),
        refresh_token: response
            .and_then(|tokens| string_key(tokens, "refresh_token", "refreshToken"))
            .or(credentials.refresh_token),
        id_token: response
            .and_then(|tokens| string_key(tokens, "id_token", "idToken"))
            .or(credentials.id_token),
        account_id: credentials.account_id,
        last_refresh: Some(Utc::now()),
        auth_path: credentials.auth_path,
        raw_json: credentials.raw_json,
        scope_slot: credentials.scope_slot,
    };
    let marker_rotated = refreshed.scope_marker() != old_marker.as_slice();
    let scope = refresh.transfer(
        refreshed.scope_slot.semantic_source,
        &refreshed.scope_slot.canonical_location,
        &old_marker,
        refreshed.scope_marker(),
    );
    checkpoint(RefreshCheckpoint::MetadataHandled)?;
    // A rotated marker may reach disk only after its lineage transfer is durable.
    // The refreshed access token remains usable in memory for this poll.
    if marker_rotated && scope.is_err() {
        return Ok((refreshed, scope));
    }
    save(&refreshed)?;
    checkpoint(RefreshCheckpoint::CredentialsPersisted)?;
    Ok((refreshed, scope))
}

async fn refresh_claude_credentials(
    original: &ClaudeCredentials,
) -> Result<(ClaudeCredentials, Result<AccountScope, AccountScopeError>), String> {
    let refresh = agent_account_scope::begin_refresh("claude")
        .map_err(|_| "Claude credential refresh lock is unavailable.".to_string())?;
    refresh_claude_credentials_with(
        original,
        &refresh,
        reload_claude_credentials,
        request_claude_refresh,
        save_claude_credentials,
        |_| Ok(()),
    )
    .await
}

async fn request_claude_refresh(refresh_token: String) -> Result<ClaudeRefreshResponse, String> {
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
            ("refresh_token", &refresh_token),
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
    serde_json::from_str(&body).map_err(|e| format!("decode Claude refresh response: {}", e))
}

async fn refresh_claude_credentials_with<R, Reload, Request, RequestFuture, Save, Checkpoint>(
    original: &ClaudeCredentials,
    refresh: &R,
    reload: Reload,
    request: Request,
    save: Save,
    mut checkpoint: Checkpoint,
) -> Result<(ClaudeCredentials, Result<AccountScope, AccountScopeError>), String>
where
    R: RefreshScopeTransaction + ?Sized,
    Reload: FnOnce(&ClaudeCredentials) -> Result<ClaudeCredentials, String>,
    Request: FnOnce(String) -> RequestFuture,
    RequestFuture: std::future::Future<Output = Result<ClaudeRefreshResponse, String>>,
    Save: FnOnce(&ClaudeCredentials) -> Result<(), String>,
    Checkpoint: FnMut(RefreshCheckpoint) -> Result<(), String>,
{
    let credentials = reload(original)?;
    checkpoint(RefreshCheckpoint::Reloaded)?;
    if !claude_credentials_expired(&credentials) {
        let scope = match credentials.scope_marker() {
            Some(marker) => refresh.resolve_current(
                credentials.scope_slot.semantic_source,
                &credentials.scope_slot.canonical_location,
                marker,
            ),
            None => Err(AccountScopeError::NoTrustedEvidence),
        };
        return Ok((credentials, scope));
    }

    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            "Claude OAuth token is expired and has no refresh token. Run `claude`.".to_string()
        })?
        .to_string();
    let old_marker = refresh_token.as_bytes().to_vec();
    let token_response = request(refresh_token).await?;
    checkpoint(RefreshCheckpoint::NetworkReturned)?;
    let refreshed = ClaudeCredentials {
        access_token: token_response.access_token,
        refresh_token: token_response
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .or_else(|| credentials.refresh_token.clone()),
        expires_at: Some(Utc::now() + chrono::Duration::seconds(token_response.expires_in)),
        scopes: credentials.scopes.clone(),
        rate_limit_tier: credentials.rate_limit_tier.clone(),
        subscription_type: credentials.subscription_type.clone(),
        source: credentials.source,
        raw_root: credentials.raw_root.clone(),
        scope_slot: credentials.scope_slot.clone(),
    };
    let new_marker = refreshed.scope_marker();
    let marker_rotated = new_marker.is_some_and(|marker| marker != old_marker.as_slice());
    let scope = match new_marker {
        Some(new_marker) => refresh.transfer(
            refreshed.scope_slot.semantic_source,
            &refreshed.scope_slot.canonical_location,
            &old_marker,
            new_marker,
        ),
        None => Err(AccountScopeError::NoTrustedEvidence),
    };
    checkpoint(RefreshCheckpoint::MetadataHandled)?;
    // A rotated marker may reach the shared provider store only after its
    // lineage transfer is durable. The new access token remains usable in
    // memory for this poll.
    if marker_rotated && scope.is_err() {
        return Ok((refreshed, scope));
    }
    if let Err(error) = save(&refreshed) {
        eprintln!("tb_core_ffi: failed to persist refreshed Claude credentials: {error}");
    }
    checkpoint(RefreshCheckpoint::CredentialsPersisted)?;
    Ok((refreshed, scope))
}

fn reload_claude_credentials(original: &ClaudeCredentials) -> Result<ClaudeCredentials, String> {
    match original.source {
        ClaudeCredentialSource::Keychain => {
            let raw = load_claude_credentials_from_keychain()?.ok_or_else(|| {
                "Claude Keychain credentials disappeared during refresh.".to_string()
            })?;
            parse_claude_credentials_data(&raw, ClaudeCredentialSource::Keychain)
        }
        ClaudeCredentialSource::File => {
            let raw = fs::read_to_string(claude_credentials_path())
                .map_err(|e| format!("reload Claude credentials file: {e}"))?;
            parse_claude_credentials_data(&raw, ClaudeCredentialSource::File)
        }
        ClaudeCredentialSource::Environment => {
            Err("Claude environment credentials cannot be refreshed in place.".to_string())
        }
    }
}

/// Merge the rotated access/refresh tokens back into the credentials store they
/// came from, preserving every other field the Claude CLI wrote.
fn save_claude_credentials(credentials: &ClaudeCredentials) -> Result<(), String> {
    match credentials.source {
        ClaudeCredentialSource::Keychain => {
            save_claude_credentials_to_keychain(&merge_claude_credentials_json(credentials)?)
        }
        ClaudeCredentialSource::File => {
            save_claude_credentials_to_file(credentials, &claude_credentials_path())
        }
        ClaudeCredentialSource::Environment => Ok(()),
    }
}

fn save_claude_credentials_to_file(
    credentials: &ClaudeCredentials,
    path: &Path,
) -> Result<(), String> {
    atomic_write(path, &merge_claude_credentials_json(credentials)?)
}

/// Replace `path` atomically: write a sibling temp file, then rename over the
/// target. A crash or partial write leaves the original credentials intact
/// rather than a truncated file that would break both TokenBar and the Claude
/// CLI (the rename is atomic within one filesystem).
fn atomic_write(path: &Path, data: &str) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "credentials path {} has no parent directory",
            path.display()
        )
    })?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {}", parent.display(), e))?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("credentials");
    // Per-write-unique temp name (pid + a monotonic seq). The O_EXCL open below
    // must never collide with an orphan a crashed earlier write left at a fixed
    // path, or every later write-back in this long-lived process would fail with
    // AlreadyExists and silently stop persisting rotated tokens.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(".{}.tmp.{}.{}", file_name, std::process::id(), seq));

    // Stage into the temp, fsync it, then rename over the target. Create with
    // O_EXCL + 0600 up front: the mode-at-creation closes the umask-default
    // window a write-then-chmod leaves the secret readable in, and O_EXCL
    // refuses to follow a symlink pre-seeded at the temp path.
    let staged = (|| -> Result<(), String> {
        use std::io::Write as _;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp)
            .map_err(|e| format!("create {}: {}", tmp.display(), e))?;
        file.write_all(data.as_bytes())
            .map_err(|e| format!("write {}: {}", tmp.display(), e))?;
        // Flush data to disk before the rename so a power loss can't leave the
        // renamed file pointing at never-written blocks — the crash-safety this
        // function's doc-comment promises.
        file.sync_all()
            .map_err(|e| format!("sync {}: {}", tmp.display(), e))
    })();
    // Any failure after the temp exists removes it, so a transient write error
    // can't strand an orphan that wedges the next write.
    if let Err(error) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("replace {}: {}", path.display(), error));
    }
    // Persist the rename itself so it survives a power loss right afterward.
    #[cfg(unix)]
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Merge the rotated tokens into the loaded credentials JSON, preserving any
/// other fields, and return it serialized. Pure so it's unit-testable.
fn merge_claude_credentials_json(credentials: &ClaudeCredentials) -> Result<String, String> {
    let mut root = credentials
        .raw_root
        .clone()
        .unwrap_or_else(|| serde_json::json!({ "claudeAiOauth": {} }));
    let oauth = root
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "Claude credentials JSON has no claudeAiOauth object.".to_string())?;
    oauth.insert(
        "accessToken".to_string(),
        Value::String(credentials.access_token.clone()),
    );
    if let Some(refresh) = &credentials.refresh_token {
        oauth.insert("refreshToken".to_string(), Value::String(refresh.clone()));
    }
    if let Some(expires_at) = credentials.expires_at {
        oauth.insert(
            "expiresAt".to_string(),
            Value::Number(expires_at.timestamp_millis().into()),
        );
    }
    serde_json::to_string(&root).map_err(|e| format!("encode Claude credentials: {}", e))
}

#[cfg(target_os = "macos")]
fn save_claude_credentials_to_keychain(data: &str) -> Result<(), String> {
    // Fail closed: only update the item once we can confirm the exact account
    // the Claude CLI stored it under. `add-generic-password -U` matches on
    // (service, account), so updating with the wrong or an empty account would
    // create a SECOND "Claude Code-credentials" item and confuse the store the
    // CLI shares — worse than not persisting. If the account can't be read,
    // skip the write-back (the caller logs it); the next refresh retries.
    let account = claude_keychain_account().ok_or_else(|| {
        "could not resolve the Claude Keychain account; skipping write-back to avoid a duplicate item"
            .to_string()
    })?;
    // NOTE: `-w <data>` puts the credential JSON on the argv, briefly visible via
    // `ps` to same-user processes. security(1) has no stdin form for
    // add-generic-password (only an interactive `-w` prompt, unusable from a
    // background app) and the item is already same-user-readable once the
    // keychain is unlocked, so on a single-user Mac this narrow window is an
    // accepted trade-off; move to the SecItem API if that assumption changes.
    let status = std::process::Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            CLAUDE_KEYCHAIN_SERVICE,
            "-a",
            &account,
            "-w",
            data,
        ])
        .status()
        .map_err(|e| format!("write Claude Keychain credentials: {}", e))?;
    if !status.success() {
        return Err("security add-generic-password failed for Claude credentials.".to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn save_claude_credentials_to_keychain(_data: &str) -> Result<(), String> {
    Err("Keychain writes are only supported on macOS.".to_string())
}

/// Read the account name the Claude Keychain item is stored under so the
/// write-back updates that same item instead of creating a duplicate.
#[cfg(target_os = "macos")]
fn claude_keychain_account() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Attribute line looks like: `    "acct"<blob>="alice"`
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("\"acct\"") {
            if let Some(eq) = rest.find('=') {
                let value = rest[eq + 1..].trim();
                // security renders a non-printable acct as `0x<hex>  "ascii"`;
                // the string-scrape can't recover the real bytes, so treat it as
                // unresolved (fail closed) rather than returning a corrupt
                // account that `add-generic-password -U` would spawn a duplicate
                // "Claude Code-credentials" item under.
                if value.starts_with("0x") {
                    return None;
                }
                let value = value.trim_matches('"');
                if !value.is_empty() && value != "<NULL>" {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
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
        serde_json::to_string_pretty(&raw).map_err(|e| format!("encode Codex auth.json: {}", e))?;
    atomic_write(&credentials.auth_path, &data).map_err(|e| format!("save Codex auth.json: {}", e))
}

fn enrich_snapshot(snapshot: &mut AgentUsageSnapshot, now: i64) {
    enrich_snapshot_with(snapshot, now, |active_keys, observations, now| {
        crate::agent_quota_history::record_observations_and_evaluate(active_keys, observations, now)
    });
}

fn enrich_snapshot_with<F>(snapshot: &mut AgentUsageSnapshot, now: i64, mut record: F)
where
    F: FnMut(
        &[SeriesKey],
        &[QuotaObservation],
        i64,
    ) -> Result<Vec<BatchObservationResult>, HistoryError>,
{
    let mut card_ids = HashSet::new();
    let mut window_keys = HashSet::new();
    snapshot.windows.retain(|window| {
        let card_is_unique = !card_ids.contains(&window.card_id);
        let key_is_unique = window
            .window_key
            .as_ref()
            .is_none_or(|window_key| !window_keys.contains(window_key));
        if !card_is_unique || !key_is_unique {
            return false;
        }
        card_ids.insert(window.card_id.clone());
        if let Some(window_key) = window.window_key.as_ref() {
            window_keys.insert(window_key.clone());
        }
        true
    });

    let Ok(account_scope) = snapshot.account_scope.as_ref() else {
        for window in &mut snapshot.windows {
            if window.window_key.is_some() {
                window.unavailable("accountScope");
            }
        }
        return;
    };
    let account_scope = account_scope.as_str();
    let mut active_keys = Vec::new();
    let mut observations = Vec::new();
    let mut mapped_indices = Vec::new();

    for (index, window) in snapshot.windows.iter_mut().enumerate() {
        let Some(window_key) = window.window_key.as_deref() else {
            // The provider already classified this card as windowIdentity.
            continue;
        };
        let key = SeriesKey::new(snapshot.client_id.clone(), account_scope, window_key);
        active_keys.push(key.clone());
        if matches!(window.pace_status.state, PaceState::Unavailable) {
            // Emission protects existing history from capacity eviction, but
            // missing reset and other typed early rejects never record a sample.
            continue;
        }
        let Some(reset_at) = window
            .resets_at
            .as_deref()
            .and_then(parse_datetime)
            .map(|reset| reset.timestamp())
        else {
            window.unavailable("invalidEvidence");
            continue;
        };
        if reset_at <= now
            || !window.used_percent.is_finite()
            || !(0.0..=100.0).contains(&window.used_percent)
        {
            window.unavailable("invalidEvidence");
            continue;
        }
        observations.push(QuotaObservation {
            key,
            reset_at: Some(reset_at),
            used_percent: window.used_percent,
            provider: window.provider_duration,
            contract: window.contract_duration,
        });
        mapped_indices.push(index);
    }

    if active_keys.is_empty() {
        return;
    }

    let results = match record(&active_keys, &observations, now) {
        Ok(results) if results.len() == mapped_indices.len() => results,
        Ok(_) => {
            for index in mapped_indices {
                snapshot.windows[index].unavailable("history");
            }
            return;
        }
        Err(error) => {
            let reason = if error == HistoryError::StoreCapacity {
                "storeCapacity"
            } else {
                "history"
            };
            for index in mapped_indices {
                snapshot.windows[index].unavailable(reason);
            }
            return;
        }
    };

    for (index, result) in mapped_indices.into_iter().zip(results) {
        let window = &mut snapshot.windows[index];
        match result {
            Ok((
                HistoryOutcome::Ready {
                    duration_seconds,
                    source,
                    ..
                },
                historical,
                complete_cycles,
            )) => {
                window.duration_seconds = Some(duration_seconds);
                window.duration_source = Some(source);
                window.window_minutes = Some(duration_seconds / 60);
                match historical {
                    Some(pace) if historical_pace_is_coherent(&pace) => {
                        window.pace_status = PaceStatusPayload {
                            state: PaceState::Available,
                            window_key: window.window_key.clone(),
                            duration_seconds: Some(duration_seconds),
                            duration_source: Some(source),
                            complete_cycles,
                            reason: None,
                        };
                        window.historical_pace = Some(historical_pace_payload(pace));
                    }
                    Some(_) => {
                        window.unavailable("history");
                    }
                    None => {
                        window.pace_status = PaceStatusPayload {
                            state: PaceState::LearningHistory,
                            window_key: window.window_key.clone(),
                            duration_seconds: Some(duration_seconds),
                            duration_source: Some(source),
                            complete_cycles,
                            reason: None,
                        };
                        window.historical_pace = None;
                    }
                }
            }
            Ok((HistoryOutcome::LearningDuration, None, _)) => {
                window.duration_seconds = None;
                window.duration_source = Some(DurationSource::Observed);
                window.window_minutes = None;
                window.pace_status = PaceStatusPayload {
                    state: PaceState::LearningDuration,
                    window_key: window.window_key.clone(),
                    duration_seconds: None,
                    duration_source: Some(DurationSource::Observed),
                    complete_cycles: 0,
                    reason: None,
                };
                window.historical_pace = None;
            }
            Ok((HistoryOutcome::Unavailable(reason), _, _)) => {
                window.unavailable(duration_unavailable_reason(reason));
            }
            Err(error) => {
                window.unavailable(if error == HistoryError::StoreCapacity {
                    "storeCapacity"
                } else {
                    "history"
                });
            }
            Ok((HistoryOutcome::LearningDuration, Some(_), _)) => {
                window.unavailable("history");
            }
        }
    }
}

fn duration_unavailable_reason(reason: DurationUnavailableReason) -> &'static str {
    match reason {
        DurationUnavailableReason::MissingReset => "missingReset",
        DurationUnavailableReason::InvalidEvidence => "invalidEvidence",
    }
}

fn historical_pace_is_coherent(pace: &HistoricalPace) -> bool {
    pace.expected_percent.is_finite()
        && (0.0..=100.0).contains(&pace.expected_percent)
        && pace
            .eta_seconds
            .is_none_or(|eta| eta.is_finite() && eta >= 0.0)
        && pace
            .run_out_probability
            .is_none_or(|probability| probability.is_finite() && (0.0..=1.0).contains(&probability))
        && (pace.eta_seconds.is_none() == pace.will_last_to_reset)
}

fn historical_pace_payload(pace: HistoricalPace) -> HistoricalPacePayload {
    HistoricalPacePayload {
        expected_used_percent: pace.expected_percent,
        eta_seconds: pace.eta_seconds,
        will_last_to_reset: pace.will_last_to_reset,
        run_out_probability: pace.run_out_probability,
    }
}

fn codex_windows(
    rate_limit: Option<&CodexRateLimit>,
    additional_rate_limits: Option<&[CodexAdditionalRateLimit]>,
    now: DateTime<Utc>,
) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    let mut emitted_card_ids = HashSet::new();
    if let Some(rate_limit) = rate_limit {
        let mut main = [
            ("primary", rate_limit.primary_window.clone()),
            ("secondary", rate_limit.secondary_window.clone()),
        ];
        main.sort_by_key(|(_, window)| {
            window
                .as_ref()
                .map_or(2, |window| match window.limit_window_seconds {
                    18_000 => 0,
                    604_800 => 1,
                    _ => 2,
                })
        });
        for (slot, window) in main
            .into_iter()
            .filter_map(|(slot, window)| window.map(|window| (slot, window)))
        {
            let semantic = match window.limit_window_seconds {
                18_000 => Some(("Session", "main.session.v1")),
                604_800 => Some(("Weekly", "main.weekly.v1")),
                _ => None,
            };
            let (label, window_key) = semantic.unwrap_or(("Unknown", ""));
            let card_id = if window_key.is_empty() {
                format!("row.main.{slot}.v1")
            } else {
                window_key.to_string()
            };
            let Some(mapped) = map_window_with_identity(
                label,
                window,
                now,
                card_id.clone(),
                (!window_key.is_empty()).then(|| window_key.to_string()),
            ) else {
                continue;
            };
            if !emitted_card_ids.insert(card_id) {
                continue;
            }
            windows.push(mapped);
        }
    }

    let mut anonymous_slots = HashSet::new();
    for extra in additional_rate_limits.unwrap_or(&[]) {
        let source = additional_limit_source(extra);
        let digest = source.map(sha256_hex);
        let Some(rate_limit) = extra.rate_limit.as_ref() else {
            continue;
        };
        for (slot, window) in [
            ("primary", rate_limit.primary_window.clone()),
            ("secondary", rate_limit.secondary_window.clone()),
        ]
        .into_iter()
        .filter_map(|(slot, window)| window.map(|window| (slot, window)))
        {
            let Some(digest) = digest.as_deref() else {
                let Some(mapped) = map_window_with_identity(
                    "Unknown",
                    window,
                    now,
                    format!("row.additional.unknown.{slot}.v1"),
                    None,
                ) else {
                    continue;
                };
                if anonymous_slots.insert(slot) {
                    windows.push(mapped);
                }
                continue;
            };
            let label = additional_limit_label(extra);
            let window_key = format!("additional.{digest}.{slot}.v1");
            let Some(mapped) = map_window_with_identity(
                &label,
                window,
                now,
                window_key.clone(),
                Some(window_key.clone()),
            ) else {
                continue;
            };
            if !emitted_card_ids.insert(window_key) {
                continue;
            }
            windows.push(mapped);
        }
    }
    windows
}

fn claude_windows(usage: &ClaudeUsageResponse, now: DateTime<Utc>) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    push_claude_window(
        &mut windows,
        "Session",
        "session.v1",
        DurationEvidence::contract(300 * 60),
        usage.five_hour.as_ref(),
        now,
    );
    push_claude_window(
        &mut windows,
        "Weekly",
        "weekly.v1",
        DurationEvidence::contract(7 * 24 * 60 * 60),
        usage.seven_day.as_ref(),
        now,
    );
    push_claude_window(
        &mut windows,
        "OAuth Apps",
        "oauth_apps.weekly.v1",
        DurationEvidence::contract(7 * 24 * 60 * 60),
        usage.seven_day_oauth_apps.as_ref(),
        now,
    );
    push_claude_window(
        &mut windows,
        "Sonnet",
        "sonnet.weekly.v1",
        DurationEvidence::contract(7 * 24 * 60 * 60),
        usage.seven_day_sonnet.as_ref(),
        now,
    );
    push_claude_window(
        &mut windows,
        "Opus",
        "opus.weekly.v1",
        DurationEvidence::contract(7 * 24 * 60 * 60),
        usage.seven_day_opus.as_ref(),
        now,
    );
    push_claude_window(
        &mut windows,
        "Designs",
        "design.weekly.v1",
        DurationEvidence::contract(7 * 24 * 60 * 60),
        usage.design_window(),
        now,
    );
    push_claude_window(
        &mut windows,
        "Daily Routines",
        "routines.weekly.v1",
        DurationEvidence::contract(7 * 24 * 60 * 60),
        usage.routines_window(),
        now,
    );
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
        .find(|window| window.has_valid_utilization())
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
        .find(|window| window.has_valid_utilization())
    }
}

fn push_claude_window(
    windows: &mut Vec<UsageWindow>,
    label: &str,
    window_key: &str,
    contract_duration: DurationEvidence,
    window: Option<&ClaudeWindow>,
    now: DateTime<Utc>,
) {
    if let Some(mapped) = window
        .and_then(|window| map_claude_window(label, window_key, contract_duration, window, now))
    {
        windows.push(mapped);
    }
}

fn map_claude_window(
    label: &str,
    window_key: &str,
    contract_duration: DurationEvidence,
    window: &ClaudeWindow,
    now: DateTime<Utc>,
) -> Option<UsageWindow> {
    let used = window.utilization?;
    let resets_at = window.resets_at.as_deref().and_then(parse_datetime);
    UsageWindow::try_from_provider_used_percent(label.to_string(), used, resets_at, now).map(
        |window| {
            window.with_identity(
                window_key,
                Some(window_key.to_string()),
                None,
                Some(contract_duration),
            )
        },
    )
}

/// Parse the `anthropic-ratelimit-unified-{5h,7d}-{utilization,reset}` response
/// headers into Session/Weekly usage windows. Pure — no network or I/O.
///
/// Unlike the oauth/usage JSON body (`utilization` 0..100, RFC3339 reset), these
/// headers use a 0..1 fraction and a Unix-epoch-seconds reset. This is the
/// fallback source for inference-only `claude setup-token` tokens.
fn parse_unified_ratelimit_windows(
    headers: &reqwest::header::HeaderMap,
    now: DateTime<Utc>,
) -> Vec<UsageWindow> {
    let read_f64 = |name: &str| -> Option<f64> {
        headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok()
    };
    let read_i64 = |name: &str| -> Option<i64> {
        headers.get(name)?.to_str().ok()?.trim().parse::<i64>().ok()
    };
    let mut windows = Vec::new();
    if let Some(window) = unified_ratelimit_window_with_identity(
        "Session",
        "session.v1",
        DurationEvidence::contract(300 * 60),
        read_f64("anthropic-ratelimit-unified-5h-utilization"),
        read_i64("anthropic-ratelimit-unified-5h-reset"),
        now,
    ) {
        windows.push(window);
    }
    if let Some(window) = unified_ratelimit_window_with_identity(
        "Weekly",
        "weekly.v1",
        DurationEvidence::contract(7 * 24 * 60 * 60),
        read_f64("anthropic-ratelimit-unified-7d-utilization"),
        read_i64("anthropic-ratelimit-unified-7d-reset"),
        now,
    ) {
        windows.push(window);
    }
    windows
}

/// Build one window from a unified-ratelimit header pair. Gated on utilization
/// (mirrors `map_claude_window`); reset is optional. `utilization_fraction` is
/// 0..1 (scaled ×100); `reset_epoch_seconds` is Unix seconds (like the Codex
/// `map_window` epoch handling).
fn unified_ratelimit_window_with_identity(
    label: &str,
    window_key: &str,
    contract_duration: DurationEvidence,
    utilization_fraction: Option<f64>,
    reset_epoch_seconds: Option<i64>,
    now: DateTime<Utc>,
) -> Option<UsageWindow> {
    let used = utilization_fraction? * 100.0;
    let resets_at = reset_epoch_seconds
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single());
    UsageWindow::try_from_provider_used_percent(label.to_string(), used, resets_at, now).map(
        |window| {
            window.with_identity(
                window_key,
                Some(window_key.to_string()),
                None,
                Some(contract_duration),
            )
        },
    )
}

#[cfg(test)]
fn unified_ratelimit_window(
    label: &str,
    utilization_fraction: Option<f64>,
    reset_epoch_seconds: Option<i64>,
    now: DateTime<Utc>,
) -> Option<UsageWindow> {
    let (window_key, duration) = if label.eq_ignore_ascii_case("Session") {
        ("session.v1", DurationEvidence::contract(300 * 60))
    } else {
        ("weekly.v1", DurationEvidence::contract(7 * 24 * 60 * 60))
    };
    unified_ratelimit_window_with_identity(
        label,
        window_key,
        duration,
        utilization_fraction,
        reset_epoch_seconds,
        now,
    )
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
    let mut window = UsageWindow::try_from_provider_used_percent(
        "Extra usage".to_string(),
        used,
        None,
        Utc::now(),
    )?
    .with_identity(
        "extra_usage.v1",
        Some("extra_usage.v1".to_string()),
        None,
        None,
    );
    window.reset_text = reset_text;
    Some(window)
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

fn map_window_with_identity(
    label: &str,
    window: CodexWindow,
    now: DateTime<Utc>,
    card_id: impl Into<String>,
    window_key: Option<String>,
) -> Option<UsageWindow> {
    let resets_at = (window.reset_at != 0)
        .then(|| Utc.timestamp_opt(window.reset_at, 0).single())
        .flatten();
    let provider_duration = (window.limit_window_seconds != 0)
        .then(|| DurationEvidence::provider(window.reset_at, window.limit_window_seconds));
    UsageWindow::try_from_provider_used_percent(
        label.to_string(),
        window.used_percent,
        resets_at,
        now,
    )
    .map(|window| window.with_identity(card_id, window_key, provider_duration, None))
}

fn additional_limit_source(limit: &CodexAdditionalRateLimit) -> Option<String> {
    first_non_empty([
        limit.metered_feature.as_deref(),
        limit.limit_name.as_deref(),
    ])
    .map(str::to_string)
}

fn sha256_hex(value: String) -> String {
    let digest = Sha256::digest(value.trim().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
        .or_else(|| crate::user_home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn claude_credentials_path() -> PathBuf {
    crate::user_home_dir()
        .map(|home| home.join(".claude/.credentials.json"))
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
    [snake_case, camel_case]
        .into_iter()
        .filter_map(|key| map.get(key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
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

pub(crate) fn deserialize_optional_raw<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let raw = Option::<Box<serde_json::value::RawValue>>::deserialize(deserializer)?;
    Ok(raw.and_then(|raw| serde_json::from_str(raw.get()).ok()))
}

fn deserialize_optional_non_empty_string<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value
        .as_ref()
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string))
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
    use crate::agent_account_scope::test_support::TestRefreshScope;

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

    #[test]
    fn string_key_uses_first_valid_snake_or_camel_alias() {
        let cases = [
            (
                "snake priority",
                serde_json::json!({
                    "snake_key": " snake-value ",
                    "camelKey": "camel-value"
                }),
                Some("snake-value"),
            ),
            (
                "snake missing",
                serde_json::json!({ "camelKey": " camel-value " }),
                Some("camel-value"),
            ),
            (
                "snake null",
                serde_json::json!({ "snake_key": null, "camelKey": "camel-value" }),
                Some("camel-value"),
            ),
            (
                "snake empty",
                serde_json::json!({ "snake_key": "", "camelKey": "camel-value" }),
                Some("camel-value"),
            ),
            (
                "snake whitespace",
                serde_json::json!({ "snake_key": " \t\n ", "camelKey": "camel-value" }),
                Some("camel-value"),
            ),
            (
                "snake non-string",
                serde_json::json!({
                    "snake_key": { "unexpected": true },
                    "camelKey": "camel-value"
                }),
                Some("camel-value"),
            ),
            (
                "both invalid",
                serde_json::json!({ "snake_key": false, "camelKey": "   " }),
                None,
            ),
        ];

        for (label, value, expected) in cases {
            let map = value.as_object().unwrap();
            assert_eq!(
                string_key(map, "snake_key", "camelKey").as_deref(),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn claude_refresh_response_ignores_invalid_optional_refresh_token() {
        let cases = [
            (
                "valid",
                serde_json::json!({
                    "access_token": "new-access",
                    "refresh_token": " new-refresh ",
                    "expires_in": 3600
                }),
                Some("new-refresh"),
            ),
            (
                "missing",
                serde_json::json!({ "access_token": "new-access", "expires_in": 3600 }),
                None,
            ),
            (
                "null",
                serde_json::json!({
                    "access_token": "new-access",
                    "refresh_token": null,
                    "expires_in": 3600
                }),
                None,
            ),
            (
                "empty",
                serde_json::json!({
                    "access_token": "new-access",
                    "refresh_token": "",
                    "expires_in": 3600
                }),
                None,
            ),
            (
                "whitespace",
                serde_json::json!({
                    "access_token": "new-access",
                    "refresh_token": " \t\n ",
                    "expires_in": 3600
                }),
                None,
            ),
            (
                "non-string",
                serde_json::json!({
                    "access_token": "new-access",
                    "refresh_token": { "unexpected": true },
                    "expires_in": 3600
                }),
                None,
            ),
        ];

        for (label, value, expected) in cases {
            let response: ClaudeRefreshResponse = serde_json::from_value(value).unwrap();
            assert_eq!(response.access_token, "new-access", "{label}");
            assert_eq!(response.expires_in, 3_600, "{label}");
            assert_eq!(response.refresh_token.as_deref(), expected, "{label}");
        }
        assert!(serde_json::from_value::<ClaudeRefreshResponse>(
            serde_json::json!({ "expires_in": 3600 })
        )
        .is_err());
        assert!(serde_json::from_value::<ClaudeRefreshResponse>(
            serde_json::json!({ "access_token": "new-access" })
        )
        .is_err());
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

        // Success caches the display-ready snapshot; a later OAuth 429 serves
        // those rows unchanged while dropping the stale account scope. The
        // fetch path returns this fallback without another enrichment/history
        // pass, preserving the last-good typed pace.
        let scope = TestRefreshScope::new("claude", "cached-429");
        let account_scope = scope
            .resolve_current("fixture", "cached-429", b"cached-429-marker")
            .unwrap();
        let mut snapshot = AgentUsageSnapshot {
            client_id: "claude".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: None,
            account_scope: Ok(account_scope),
            windows: vec![UsageWindow::from_provider_used_percent(
                "Session".to_string(),
                20.0,
                Some(now + chrono::Duration::hours(5)),
                now,
            )
            .with_identity(
                "session.v1",
                Some("session.v1".to_string()),
                None,
                Some(DurationEvidence::contract(300 * 60)),
            )],
            credits: None,
            error: None,
        };
        snapshot.windows[0].pace_status = PaceStatusPayload {
            state: PaceState::Available,
            window_key: Some("session.v1".to_string()),
            duration_seconds: Some(300 * 60),
            duration_source: Some(DurationSource::Contract),
            complete_cycles: 6,
            reason: None,
        };
        snapshot.windows[0].historical_pace = Some(HistoricalPacePayload {
            expected_used_percent: 35.0,
            eta_seconds: Some(1_800.0),
            will_last_to_reset: false,
            run_out_probability: Some(0.42),
        });
        claude_gate_record_success(&snapshot);
        assert!(claude_gate_blocked_until(later).is_none());
        claude_gate_record_rate_limit(Some(later + chrono::Duration::seconds(60)), later);
        let until = claude_gate_blocked_until(later).unwrap();
        let fallback = claude_gate_fallback(until, later);
        assert!(fallback.error.is_none());
        assert_eq!(fallback.windows.len(), 1);
        assert!(matches!(
            &fallback.account_scope,
            Err(AccountScopeError::NoTrustedEvidence)
        ));
        assert_eq!(fallback.windows[0].pace_status.state, PaceState::Available);
        assert_eq!(fallback.windows[0].pace_status.complete_cycles, 6);
        assert_eq!(
            fallback.windows[0]
                .historical_pace
                .as_ref()
                .map(|pace| pace.expected_used_percent),
            Some(35.0)
        );

        // Leave the gate clean for any other test touching the static.
        claude_gate_record_success(&snapshot);
        scope.cleanup();
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
        assert_eq!(windows[0].window_minutes, Some(300));
        assert_eq!(windows[1].label, "Weekly");
        assert_eq!(windows[1].remaining_percent, 65.0);
        assert_eq!(windows[1].window_minutes, Some(10_080));
    }

    #[test]
    fn stage0_freezes_codex_duration_roles_and_unknown_window_baseline() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let reversed = CodexRateLimit {
            primary_window: Some(CodexWindow {
                used_percent: 35.0,
                reset_at: 1_700_172_800,
                limit_window_seconds: 604_800,
            }),
            secondary_window: Some(CodexWindow {
                used_percent: 8.0,
                reset_at: 1_700_005_400,
                limit_window_seconds: 18_000,
            }),
        };
        let windows = codex_windows(Some(&reversed), None, now);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "Session", "codex.main.18000.session");
        assert_eq!(windows[0].card_id, "main.session.v1");
        assert_eq!(windows[0].window_key.as_deref(), Some("main.session.v1"));
        assert_eq!(windows[0].window_minutes, Some(300));
        assert_eq!(windows[1].label, "Weekly", "codex.main.604800.weekly");
        assert_eq!(windows[1].card_id, "main.weekly.v1");
        assert_eq!(windows[1].window_key.as_deref(), Some("main.weekly.v1"));
        assert_eq!(windows[1].window_minutes, Some(10_080));

        let unknown_rate_limit = CodexRateLimit {
            primary_window: Some(CodexWindow {
                used_percent: 10.0,
                reset_at: now.timestamp() + 3_600,
                limit_window_seconds: 3_600,
            }),
            secondary_window: None,
        };
        let unknown = codex_windows(Some(&unknown_rate_limit), None, now);
        assert_eq!(unknown.len(), 1);
        let unknown = &unknown[0];
        assert_eq!(unknown.card_id, "row.main.primary.v1");
        assert_eq!(unknown.window_key, None);
        assert_eq!(unknown.window_minutes, None);
        assert_eq!(unknown.pace_status.state, PaceState::Unavailable);
        assert_eq!(
            unknown.pace_status.reason.as_deref(),
            Some("windowIdentity")
        );
        let wire = serde_json::to_value(unknown).unwrap();
        assert_eq!(wire["cardId"], "row.main.primary.v1");
        assert!(wire["paceStatus"].get("windowKey").is_none());
        assert_eq!(wire["paceStatus"]["state"], "unavailable");
        assert_eq!(wire["paceStatus"]["reason"], "windowIdentity");
    }

    #[test]
    fn serializes_nested_historical_pace_without_legacy_scalars() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let mut window = UsageWindow::from_used_percent(
            "Weekly".to_string(),
            60.0,
            Some(now + chrono::Duration::hours(12)),
            now,
            Some(10_080),
        )
        .with_identity(
            "weekly.v1",
            Some("weekly.v1".to_string()),
            None,
            Some(DurationEvidence::contract(10_080 * 60)),
        );
        window.pace_status.state = PaceState::Available;
        window.historical_pace = Some(HistoricalPacePayload {
            expected_used_percent: 55.0,
            eta_seconds: Some(3_600.0),
            will_last_to_reset: false,
            run_out_probability: Some(0.8),
        });

        let value = serde_json::to_value(&window).unwrap();
        assert!(value.get("historicalPace").is_some());
        assert!(value.get("historicalExpectedPercent").is_none());
        assert!(value.get("runOutProbability").is_none());
        let historical = value.get("historicalPace").unwrap();
        assert_eq!(historical["expectedUsedPercent"], 55.0);
        assert_eq!(historical["etaSeconds"], 3_600.0);
        assert_eq!(historical["willLastToReset"], false);
        assert_eq!(historical["runOutProbability"], 0.8);
    }

    #[test]
    fn stage1_credential_markers_follow_the_frozen_provider_routes() {
        let slot = CredentialSlot {
            semantic_source: "fixture",
            canonical_location: "fixture".to_string(),
        };
        let codex = CodexCredentials {
            access_token: "codex-access".to_string(),
            refresh_token: Some("codex-refresh".to_string()),
            id_token: None,
            account_id: None,
            last_refresh: None,
            auth_path: PathBuf::new(),
            raw_json: Value::Null,
            scope_slot: slot.clone(),
        };
        assert_eq!(codex.scope_marker(), b"codex-refresh");
        let mut codex_access_only = codex.clone();
        codex_access_only.refresh_token = None;
        assert_eq!(codex_access_only.scope_marker(), b"codex-access");

        let claude_login = ClaudeCredentials {
            access_token: "claude-access".to_string(),
            refresh_token: Some("claude-refresh".to_string()),
            expires_at: None,
            scopes: Vec::new(),
            rate_limit_tier: None,
            subscription_type: None,
            source: ClaudeCredentialSource::File,
            raw_root: None,
            scope_slot: slot.clone(),
        };
        assert_eq!(
            claude_login.scope_marker(),
            Some(b"claude-refresh".as_slice())
        );
        let mut login_without_refresh = claude_login.clone();
        login_without_refresh.refresh_token = None;
        assert_eq!(login_without_refresh.scope_marker(), None);

        let claude_setup = ClaudeCredentials {
            source: ClaudeCredentialSource::Environment,
            scope_slot: slot,
            ..login_without_refresh
        };
        assert_eq!(
            claude_setup.scope_marker(),
            Some(b"claude-access".as_slice())
        );
    }

    #[test]
    fn codex_scope_precedence_keeps_refresh_failure_sticky() {
        let scope_store = TestRefreshScope::new("codex", "codex-scope-precedence");
        let refresh_scope = scope_store
            .resolve_current("fixture", "refresh", b"refresh-marker")
            .unwrap();
        let authoritative_scope = scope_store
            .resolve_current("fixture", "authoritative", b"authoritative-marker")
            .unwrap();
        let credential_scope = scope_store
            .resolve_current("fixture", "credential", b"credential-marker")
            .unwrap();
        let authoritative_calls = std::cell::Cell::new(0);
        let credential_calls = std::cell::Cell::new(0);

        let resolved = resolve_codex_account_scope(
            Some(Err(AccountScopeError::MetadataWrite)),
            Some("acct-id"),
            |_| {
                authoritative_calls.set(authoritative_calls.get() + 1);
                Ok(authoritative_scope.clone())
            },
            || {
                credential_calls.set(credential_calls.get() + 1);
                Ok(credential_scope.clone())
            },
        );
        assert_eq!(resolved, Err(AccountScopeError::MetadataWrite));
        assert_eq!(authoritative_calls.get(), 0);
        assert_eq!(credential_calls.get(), 0);

        let resolved = resolve_codex_account_scope(
            Some(Err(AccountScopeError::MetadataRead)),
            None,
            |_| {
                authoritative_calls.set(authoritative_calls.get() + 1);
                Ok(authoritative_scope.clone())
            },
            || {
                credential_calls.set(credential_calls.get() + 1);
                Ok(credential_scope.clone())
            },
        );
        assert_eq!(resolved, Err(AccountScopeError::MetadataRead));
        assert_eq!(authoritative_calls.get(), 0);
        assert_eq!(credential_calls.get(), 0);

        let resolved = resolve_codex_account_scope(
            Some(Ok(refresh_scope.clone())),
            Some("acct-id"),
            |_| {
                authoritative_calls.set(authoritative_calls.get() + 1);
                Ok(authoritative_scope.clone())
            },
            || {
                credential_calls.set(credential_calls.get() + 1);
                Ok(credential_scope.clone())
            },
        );
        assert_eq!(resolved.unwrap(), authoritative_scope);
        assert_eq!(authoritative_calls.get(), 1);
        assert_eq!(credential_calls.get(), 0);

        let resolved = resolve_codex_account_scope(
            Some(Ok(refresh_scope.clone())),
            None,
            |_| {
                authoritative_calls.set(authoritative_calls.get() + 1);
                Ok(authoritative_scope.clone())
            },
            || {
                credential_calls.set(credential_calls.get() + 1);
                Ok(credential_scope.clone())
            },
        );
        assert_eq!(resolved.unwrap(), refresh_scope);
        assert_eq!(authoritative_calls.get(), 1);
        assert_eq!(credential_calls.get(), 0);

        let resolved = resolve_codex_account_scope(
            None,
            None,
            |_| {
                authoritative_calls.set(authoritative_calls.get() + 1);
                Ok(authoritative_scope.clone())
            },
            || {
                credential_calls.set(credential_calls.get() + 1);
                Ok(credential_scope.clone())
            },
        );
        assert_eq!(resolved.unwrap(), credential_scope);
        assert_eq!(authoritative_calls.get(), 1);
        assert_eq!(credential_calls.get(), 1);
        scope_store.cleanup();
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
    fn stage0_freezes_codex_additional_identity_baseline() {
        let metered_only = CodexAdditionalRateLimit {
            limit_name: None,
            metered_feature: Some("gpt-5.2-codex-spark".to_string()),
            rate_limit: None,
        };
        assert_eq!(
            additional_limit_label(&metered_only),
            "Codex Spark",
            "codex.additional.metered-feature.primary"
        );

        let named = CodexAdditionalRateLimit {
            limit_name: Some("named-limit".to_string()),
            metered_feature: Some("metered-feature".to_string()),
            rate_limit: None,
        };
        assert_eq!(
            additional_limit_label(&named),
            "Named Limit",
            "display label remains separate from the metered-feature identity"
        );

        let anonymous = CodexAdditionalRateLimit {
            limit_name: None,
            metered_feature: None,
            rate_limit: None,
        };
        assert_eq!(additional_limit_source(&anonymous), None);

        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let both_slots = CodexAdditionalRateLimit {
            limit_name: Some("named-limit".to_string()),
            metered_feature: Some(" metered-feature ".to_string()),
            rate_limit: Some(CodexRateLimit {
                primary_window: Some(CodexWindow {
                    used_percent: 10.0,
                    reset_at: 1_700_003_600,
                    limit_window_seconds: 18_000,
                }),
                secondary_window: Some(CodexWindow {
                    used_percent: 20.0,
                    reset_at: 1_700_086_400,
                    limit_window_seconds: 604_800,
                }),
            }),
        };
        assert_eq!(
            additional_limit_source(&both_slots).as_deref(),
            Some("metered-feature")
        );
        let windows = codex_windows(None, Some(&[both_slots]), now);
        assert_eq!(
            windows.len(),
            2,
            "codex.additional.primary-secondary emits both semantic slots"
        );
        let digest = sha256_hex("metered-feature".to_string());
        let primary_key = format!("additional.{digest}.primary.v1");
        let secondary_key = format!("additional.{digest}.secondary.v1");
        assert_eq!(windows[0].card_id, primary_key);
        assert_eq!(
            windows[0].window_key.as_deref(),
            Some(windows[0].card_id.as_str())
        );
        assert_eq!(windows[1].card_id, secondary_key);
        assert_eq!(
            windows[1].window_key.as_deref(),
            Some(windows[1].card_id.as_str())
        );
    }

    #[test]
    fn codex_unknown_and_anonymous_windows_are_structural_and_skip_history() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let unknown_main = CodexRateLimit {
            primary_window: Some(CodexWindow {
                used_percent: 5.0,
                reset_at: now.timestamp() + 3_600,
                limit_window_seconds: 3_600,
            }),
            secondary_window: None,
        };
        let anonymous = |primary_used: f64, secondary_used: f64| CodexAdditionalRateLimit {
            limit_name: None,
            metered_feature: None,
            rate_limit: Some(CodexRateLimit {
                primary_window: Some(CodexWindow {
                    used_percent: primary_used,
                    reset_at: now.timestamp() + 7_200,
                    limit_window_seconds: 7_200,
                }),
                secondary_window: Some(CodexWindow {
                    used_percent: secondary_used,
                    reset_at: now.timestamp() + 86_400,
                    limit_window_seconds: 86_400,
                }),
            }),
        };
        let windows = codex_windows(
            Some(&unknown_main),
            Some(&[anonymous(10.0, 20.0), anonymous(30.0, 40.0)]),
            now,
        );
        assert_eq!(windows.len(), 3);
        assert_eq!(
            windows
                .iter()
                .map(|window| window.card_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "row.main.primary.v1",
                "row.additional.unknown.primary.v1",
                "row.additional.unknown.secondary.v1"
            ]
        );
        assert_eq!(
            windows
                .iter()
                .map(|window| window.used_percent)
                .collect::<Vec<_>>(),
            vec![5.0, 10.0, 20.0],
            "duplicate anonymous slots keep the provider-order first row"
        );
        for window in &windows[1..] {
            assert_eq!(window.label_for_test(), "Unknown");
            assert_ne!(window.label_for_test(), "Codex extra limit");
        }
        for window in &windows {
            assert_eq!(window.window_key, None);
            assert_eq!(window.pace_status.state, PaceState::Unavailable);
            assert_eq!(window.pace_status.reason.as_deref(), Some("windowIdentity"));
        }

        let scope = TestRefreshScope::new("codex", "unknown-windows");
        let account_scope = scope
            .resolve_current("fixture", "unknown-windows", b"marker")
            .unwrap();
        let mut snapshot = AgentUsageSnapshot {
            client_id: "codex".to_string(),
            source: "fixture".to_string(),
            updated_at: String::new(),
            identity: None,
            account_scope: Ok(account_scope),
            windows,
            credits: None,
            error: None,
        };
        let history_calls = std::cell::Cell::new(0);
        enrich_snapshot_with(&mut snapshot, now.timestamp(), |_, _, _| {
            history_calls.set(history_calls.get() + 1);
            Ok(Vec::new())
        });
        assert_eq!(history_calls.get(), 0);

        let wire = serde_json::to_value(&snapshot).unwrap();
        let rows = wire["windows"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert!(row["paceStatus"].get("windowKey").is_none());
            assert_eq!(row["paceStatus"]["state"], "unavailable");
            assert_eq!(row["paceStatus"]["reason"], "windowIdentity");
        }
        scope.cleanup();
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
        let credentials = parse_claude_credentials_data(raw, ClaudeCredentialSource::File).unwrap();
        assert_eq!(credentials.access_token, "access");
        assert_eq!(credentials.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(credentials.scopes, vec!["user:profile"]);
        assert_eq!(credentials.subscription_type.as_deref(), Some("pro"));
    }

    #[test]
    fn merge_claude_credentials_rotates_tokens_and_preserves_other_fields() {
        let raw = r#"{
            "claudeAiOauth": {
                "accessToken": "old-access",
                "refreshToken": "old-refresh",
                "expiresAt": 1700000000000,
                "scopes": ["user:profile"],
                "subscriptionType": "pro"
            }
        }"#;
        let mut credentials =
            parse_claude_credentials_data(raw, ClaudeCredentialSource::File).unwrap();
        credentials.access_token = "new-access".to_string();
        credentials.refresh_token = Some("new-refresh".to_string());
        credentials.expires_at = Utc.timestamp_millis_opt(1_700_009_999_000).single();

        let merged = merge_claude_credentials_json(&credentials).unwrap();
        let reparsed =
            parse_claude_credentials_data(&merged, ClaudeCredentialSource::File).unwrap();
        assert_eq!(reparsed.access_token, "new-access");
        assert_eq!(reparsed.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(
            reparsed.expires_at,
            Utc.timestamp_millis_opt(1_700_009_999_000).single()
        );
        // Untouched fields the Claude CLI wrote survive the merge.
        assert_eq!(reparsed.subscription_type.as_deref(), Some("pro"));
        assert_eq!(reparsed.scopes, vec!["user:profile"]);
    }

    #[test]
    fn atomic_write_replaces_existing_file_contents() {
        let dir = std::env::temp_dir().join(format!("tb_atomic_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        fs::write(&path, "old").unwrap();

        atomic_write(&path, "new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        // No temp turds left in the directory.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp file not cleaned up");

        let _ = fs::remove_dir_all(&dir);
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
    fn stage4_claude_json_and_headers_share_canonical_duration_contracts() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let reset = Some("2026-07-24T00:00:00Z".to_string());
        let window = |utilization| ClaudeWindow {
            utilization: Some(utilization),
            resets_at: reset.clone(),
        };
        let usage = ClaudeUsageResponse {
            five_hour: Some(window(5.0)),
            seven_day: Some(window(10.0)),
            seven_day_oauth_apps: Some(window(15.0)),
            seven_day_sonnet: Some(window(20.0)),
            seven_day_opus: Some(window(25.0)),
            ..Default::default()
        };
        let windows = claude_windows(&usage, now);
        let contracts = windows
            .iter()
            .map(|window| {
                (
                    window.card_id.as_str(),
                    window.pace_status.window_key.as_deref(),
                    window.duration_seconds,
                    window.duration_source,
                    window.pace_status.state,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            contracts,
            vec![
                (
                    "session.v1",
                    Some("session.v1"),
                    Some(18_000),
                    Some(DurationSource::Contract),
                    PaceState::LearningHistory,
                ),
                (
                    "weekly.v1",
                    Some("weekly.v1"),
                    Some(604_800),
                    Some(DurationSource::Contract),
                    PaceState::LearningHistory,
                ),
                (
                    "oauth_apps.weekly.v1",
                    Some("oauth_apps.weekly.v1"),
                    Some(604_800),
                    Some(DurationSource::Contract),
                    PaceState::LearningHistory,
                ),
                (
                    "sonnet.weekly.v1",
                    Some("sonnet.weekly.v1"),
                    Some(604_800),
                    Some(DurationSource::Contract),
                    PaceState::LearningHistory,
                ),
                (
                    "opus.weekly.v1",
                    Some("opus.weekly.v1"),
                    Some(604_800),
                    Some(DurationSource::Contract),
                    PaceState::LearningHistory,
                ),
            ]
        );

        let headers = header_map(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.11"),
            ("anthropic-ratelimit-unified-5h-reset", "1783111200"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.6"),
            ("anthropic-ratelimit-unified-7d-reset", "1783504800"),
        ]);
        let header_windows = parse_unified_ratelimit_windows(&headers, now);
        assert_eq!(header_windows.len(), 2);
        for (window, expected_key, expected_duration) in [
            (&header_windows[0], "session.v1", 18_000),
            (&header_windows[1], "weekly.v1", 604_800),
        ] {
            assert_eq!(window.card_id, expected_key);
            assert_eq!(window.pace_status.window_key.as_deref(), Some(expected_key));
            assert_eq!(window.duration_seconds, Some(expected_duration));
            assert_eq!(window.duration_source, Some(DurationSource::Contract));
            assert_eq!(window.pace_status.state, PaceState::LearningHistory);
        }
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

    #[test]
    fn stage4_claude_weekly_alias_groups_share_canonical_contracts() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let design_aliases = [
            "seven_day_design",
            "seven_day_claude_design",
            "claude_design",
            "design",
            "seven_day_omelette",
            "omelette",
            "omelette_promotional",
        ];
        for alias in design_aliases {
            let raw =
                format!(r#"{{"{alias}":{{"utilization":12,"resets_at":"2026-07-24T00:00:00Z"}}}}"#);
            let usage: ClaudeUsageResponse = serde_json::from_str(&raw).unwrap();
            let windows = claude_windows(&usage, now);
            assert_eq!(windows.len(), 1, "claude.design.aliases: {alias}");
            assert_eq!(
                windows[0].label, "Designs",
                "claude.design.aliases: {alias}"
            );
            assert_eq!(windows[0].card_id, "design.weekly.v1", "{alias}");
            assert_eq!(
                windows[0].pace_status.window_key.as_deref(),
                Some("design.weekly.v1"),
                "{alias}"
            );
            assert_eq!(windows[0].duration_seconds, Some(604_800), "{alias}");
            assert_eq!(
                windows[0].duration_source,
                Some(DurationSource::Contract),
                "{alias}"
            );
            assert_eq!(windows[0].pace_status.state, PaceState::LearningHistory);
        }

        let routines_aliases = [
            "seven_day_routines",
            "seven_day_claude_routines",
            "claude_routines",
            "routines",
            "routine",
            "seven_day_cowork",
            "cowork",
        ];
        for alias in routines_aliases {
            let raw =
                format!(r#"{{"{alias}":{{"utilization":12,"resets_at":"2026-07-24T00:00:00Z"}}}}"#);
            let usage: ClaudeUsageResponse = serde_json::from_str(&raw).unwrap();
            let windows = claude_windows(&usage, now);
            assert_eq!(windows.len(), 1, "claude.routines.aliases: {alias}");
            assert_eq!(
                windows[0].label, "Daily Routines",
                "claude.routines.aliases: {alias}"
            );
            assert_eq!(windows[0].card_id, "routines.weekly.v1", "{alias}");
            assert_eq!(
                windows[0].pace_status.window_key.as_deref(),
                Some("routines.weekly.v1"),
                "{alias}"
            );
            assert_eq!(windows[0].duration_seconds, Some(604_800), "{alias}");
            assert_eq!(
                windows[0].duration_source,
                Some(DurationSource::Contract),
                "{alias}"
            );
            assert_eq!(windows[0].pace_status.state, PaceState::LearningHistory);
        }
    }

    #[test]
    fn stage0_freezes_claude_named_windows_and_invalid_baseline() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let raw = r#"{
            "five_hour": { "utilization": 5, "resets_at": "2026-07-18T00:00:00Z" },
            "seven_day": { "utilization": 10, "resets_at": "2026-07-19T00:00:00Z" },
            "seven_day_oauth_apps": { "utilization": 15, "resets_at": "2026-07-20T00:00:00Z" },
            "seven_day_sonnet": { "utilization": 20, "resets_at": "2026-07-21T00:00:00Z" },
            "seven_day_opus": { "utilization": 25, "resets_at": "2026-07-22T00:00:00Z" }
        }"#;
        let usage: ClaudeUsageResponse = serde_json::from_str(raw).unwrap();
        let windows = claude_windows(&usage, now);
        let mapped: Vec<_> = windows
            .iter()
            .map(|window| (window.label.as_str(), window.window_minutes))
            .collect();
        assert_eq!(
            mapped,
            vec![
                ("Session", Some(300)),
                ("Weekly", Some(10_080)),
                ("OAuth Apps", Some(10_080)),
                ("Sonnet", Some(10_080)),
                ("Opus", Some(10_080)),
            ],
            "claude.named-window-contracts"
        );

        let out_of_range = UsageWindow::from_used_percent(
            "Out of range".to_string(),
            150.0,
            Some(now - chrono::Duration::seconds(1)),
            now,
            Some(-1),
        );
        assert_eq!(
            out_of_range.used_percent, 100.0,
            "invalid.out-of-range captures the current clamping baseline"
        );
        assert!(
            out_of_range.resets_at.is_some(),
            "invalid.expired-reset captures the current emitted baseline"
        );
        assert_eq!(
            out_of_range.window_minutes, None,
            "invalid.contradictory-duration is not emitted as legacy duration"
        );

        let non_finite =
            UsageWindow::from_used_percent("Non-finite".to_string(), f64::NAN, None, now, None);
        assert!(
            non_finite.used_percent.is_nan(),
            "invalid.non-finite captures the current emitted baseline"
        );
    }

    #[test]
    fn stage4_claude_extra_usage_is_active_without_recording_an_observation() {
        let window = claude_extra_usage_window(Some(&ClaudeExtraUsage {
            is_enabled: true,
            monthly_limit: Some(10_000.0),
            used_credits: Some(2_500.0),
            utilization: None,
            currency: Some("USD".to_string()),
        }))
        .unwrap();
        assert_eq!(window.label, "Extra usage");
        assert_eq!(window.card_id, "extra_usage.v1");
        assert_eq!(window.used_percent, 25.0);
        assert!(window.resets_at.is_none());
        assert_eq!(
            window.pace_status.window_key.as_deref(),
            Some("extra_usage.v1")
        );
        assert_eq!(window.pace_status.state, PaceState::Unavailable);
        assert_eq!(window.pace_status.reason.as_deref(), Some("missingReset"));
        assert!(window.duration_seconds.is_none());
        assert!(window.historical_pace.is_none());

        let scope = TestRefreshScope::new("claude", "extra-usage");
        let account_scope = scope
            .resolve_current("fixture", "extra-usage", b"extra-usage-marker")
            .unwrap();
        let expected_scope = account_scope.as_str().to_string();
        let mut snapshot = AgentUsageSnapshot {
            client_id: "claude".to_string(),
            source: "oauth".to_string(),
            updated_at: String::new(),
            identity: None,
            account_scope: Ok(account_scope),
            windows: vec![window],
            credits: None,
            error: None,
        };
        let calls = std::cell::Cell::new(0);
        enrich_snapshot_with(&mut snapshot, 1_700_000_000, |active, observations, _| {
            calls.set(calls.get() + 1);
            assert_eq!(
                active,
                &[SeriesKey::new("claude", &expected_scope, "extra_usage.v1")]
            );
            assert!(observations.is_empty());
            Ok(Vec::new())
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(
            snapshot.windows[0].pace_status.state,
            PaceState::Unavailable
        );
        assert_eq!(
            snapshot.windows[0].pace_status.reason.as_deref(),
            Some("missingReset")
        );
        scope.cleanup();
    }

    #[test]
    fn stage4_emitted_unavailable_series_survives_capacity_admission() {
        let scope = TestRefreshScope::new("claude", "emitted-capacity");
        let account_scope = scope
            .resolve_current("fixture", "capacity", b"capacity-marker")
            .unwrap();
        let account_scope_value = account_scope.as_str().to_string();
        let history_path = scope
            .root()
            .join(crate::agent_quota_history::HISTORY_FILE_NAME);
        let seed_now = 1_800_000_000_i64;
        let seed_reset = seed_now + 86_400;
        let weekly_key = SeriesKey::new("claude", &account_scope_value, "weekly.v1");
        let mut seeded_keys = vec![weekly_key.clone()];
        seeded_keys.extend(
            (0..crate::agent_quota_history::MAX_SERIES - 1).map(|index| {
                SeriesKey::new(
                    "claude",
                    &account_scope_value,
                    format!("zzzz.{index:04}.v1"),
                )
            }),
        );
        for (sample_index, sampled_at) in [
            seed_now,
            seed_now + 86_400 / 5,
            seed_now + 2 * 86_400 / 5,
            seed_now + 3 * 86_400 / 5,
            seed_now + 4 * 86_400 / 5,
            seed_reset - 1,
        ]
        .into_iter()
        .enumerate()
        {
            let seeded_observations = seeded_keys
                .iter()
                .cloned()
                .map(|key| QuotaObservation {
                    key,
                    reset_at: Some(seed_reset),
                    used_percent: 10.0 + sample_index as f64 * 10.0,
                    provider: None,
                    contract: Some(DurationEvidence::contract(86_400)),
                })
                .collect::<Vec<_>>();
            let seeded = crate::agent_quota_history::record_observations_at_path_and_evaluate(
                &seeded_keys,
                &seeded_observations,
                sampled_at,
                &history_path,
            )
            .unwrap();
            assert_eq!(seeded.len(), crate::agent_quota_history::MAX_SERIES);
        }

        let now = seed_reset + 15 * 60 + 1;
        let now_date = Utc.timestamp_opt(now, 0).single().unwrap();
        let mut weekly =
            UsageWindow::from_provider_used_percent("Weekly".to_string(), 20.0, None, now_date)
                .with_identity(
                    "weekly.v1",
                    Some("weekly.v1".to_string()),
                    None,
                    Some(DurationEvidence::contract(86_400)),
                );
        weekly.unavailable("missingReset");
        let new_window = UsageWindow::from_provider_used_percent(
            "New quota".to_string(),
            5.0,
            Some(Utc.timestamp_opt(now + 86_400, 0).single().unwrap()),
            now_date,
        )
        .with_identity(
            "new.v1",
            Some("new.v1".to_string()),
            None,
            Some(DurationEvidence::contract(86_400)),
        );
        let mut snapshot = AgentUsageSnapshot {
            client_id: "claude".to_string(),
            source: "oauth".to_string(),
            updated_at: String::new(),
            identity: None,
            account_scope: Ok(account_scope),
            windows: vec![weekly, new_window],
            credits: None,
            error: None,
        };

        enrich_snapshot_with(
            &mut snapshot,
            now,
            |active, observations, transaction_now| {
                assert_eq!(active.len(), 2);
                assert!(active.contains(&weekly_key));
                assert_eq!(observations.len(), 1);
                assert_eq!(observations[0].key.window_key, "new.v1");
                crate::agent_quota_history::record_observations_at_path_and_evaluate(
                    active,
                    observations,
                    transaction_now,
                    &history_path,
                )
            },
        );

        let store: Value = serde_json::from_slice(&fs::read(&history_path).unwrap()).unwrap();
        let series = store["series"].as_array().unwrap();
        assert_eq!(series.len(), crate::agent_quota_history::MAX_SERIES);
        assert!(series.iter().any(|entry| {
            entry["providerId"] == "claude"
                && entry["accountScope"] == account_scope_value
                && entry["windowKey"] == "weekly.v1"
        }));
        assert_eq!(
            snapshot.windows[0].pace_status.reason.as_deref(),
            Some("missingReset")
        );
        scope.cleanup();
    }

    fn header_map(pairs: &[(&'static str, &'static str)]) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                reqwest::header::HeaderName::from_static(name),
                reqwest::header::HeaderValue::from_static(value),
            );
        }
        headers
    }

    #[test]
    fn parses_unified_ratelimit_headers() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let headers = header_map(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.11"),
            ("anthropic-ratelimit-unified-5h-reset", "1783111200"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.6"),
            ("anthropic-ratelimit-unified-7d-reset", "1783504800"),
        ]);
        let windows = parse_unified_ratelimit_windows(&headers, now);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "Session");
        assert!((windows[0].used_percent - 11.0).abs() < 1e-9);
        assert!((windows[0].remaining_percent - 89.0).abs() < 1e-9);
        assert_eq!(windows[0].window_minutes, Some(300));
        assert!(windows[0].resets_at.is_some());
        assert!(windows[0].reset_text.is_some());
        assert_eq!(windows[1].label, "Weekly");
        assert!((windows[1].used_percent - 60.0).abs() < 1e-9);
        assert!((windows[1].remaining_percent - 40.0).abs() < 1e-9);
        assert_eq!(windows[1].window_minutes, Some(10_080));
    }

    #[test]
    fn unified_reset_text_is_relative() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let reset = 1_700_000_000 + 3600; // now + 1h
        let window = unified_ratelimit_window("Session", Some(0.5), Some(reset), now).unwrap();
        assert!((window.used_percent - 50.0).abs() < 1e-9);
        assert!(window.reset_text.as_deref().unwrap().contains("1h"));
    }

    #[test]
    fn unified_windows_skip_missing_and_unparseable() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        // empty -> nothing
        assert!(parse_unified_ratelimit_windows(&header_map(&[]), now).is_empty());

        // only 5h -> just Session
        let windows = parse_unified_ratelimit_windows(
            &header_map(&[("anthropic-ratelimit-unified-5h-utilization", "0.2")]),
            now,
        );
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "Session");

        // unparseable 5h + valid 7d -> just Weekly
        let windows = parse_unified_ratelimit_windows(
            &header_map(&[
                ("anthropic-ratelimit-unified-5h-utilization", "abc"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.4"),
            ]),
            now,
        );
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "Weekly");

        // utilization present, reset absent -> window with no reset fields
        let window = unified_ratelimit_window("Weekly", Some(0.4), None, now).unwrap();
        assert!(window.resets_at.is_none());
        assert!(window.reset_text.is_none());
    }

    #[test]
    fn unified_window_rejects_invalid_fraction_before_wire() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let zero = unified_ratelimit_window("Session", Some(0.0), None, now).unwrap();
        assert!((zero.used_percent - 0.0).abs() < 1e-9);
        assert!((zero.remaining_percent - 100.0).abs() < 1e-9);
        let full = unified_ratelimit_window("Session", Some(1.0), None, now).unwrap();
        assert!((full.used_percent - 100.0).abs() < 1e-9);
        assert!((full.remaining_percent - 0.0).abs() < 1e-9);

        assert!(unified_ratelimit_window("Session", Some(1.5), None, now).is_none());
        assert!(unified_ratelimit_window("Session", Some(f64::NAN), None, now).is_none());
        assert!(parse_unified_ratelimit_windows(
            &header_map(&[
                ("anthropic-ratelimit-unified-5h-utilization", "NaN"),
                ("anthropic-ratelimit-unified-5h-reset", "1700003600"),
            ]),
            now,
        )
        .is_empty());

        // None utilization -> no window
        assert!(unified_ratelimit_window("Session", None, Some(1_783_111_200), now).is_none());
    }

    #[test]
    fn provider_adapters_reject_invalid_percentages_before_wire() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        assert!(map_claude_window(
            "Session",
            "session.v1",
            DurationEvidence::contract(300 * 60),
            &ClaudeWindow {
                utilization: Some(150.0),
                resets_at: None,
            },
            now,
        )
        .is_none());
        assert!(claude_extra_usage_window(Some(&ClaudeExtraUsage {
            is_enabled: true,
            monthly_limit: None,
            used_credits: None,
            utilization: Some(f64::NAN),
            currency: None,
        }))
        .is_none());
        assert!(map_window_with_identity(
            "Weekly",
            CodexWindow {
                used_percent: -1.0,
                reset_at: 1_700_003_600,
                limit_window_seconds: 604_800,
            },
            now,
            "main.weekly.v1",
            Some("main.weekly.v1".to_string()),
        )
        .is_none());

        let valid_duplicate = codex_windows(
            Some(&CodexRateLimit {
                primary_window: Some(CodexWindow {
                    used_percent: 150.0,
                    reset_at: 1_700_003_600,
                    limit_window_seconds: 18_000,
                }),
                secondary_window: Some(CodexWindow {
                    used_percent: 20.0,
                    reset_at: 1_700_003_600,
                    limit_window_seconds: 18_000,
                }),
            }),
            None,
            now,
        );
        assert_eq!(valid_duplicate.len(), 1);
        assert_eq!(valid_duplicate[0].card_id, "main.session.v1");
        assert_eq!(valid_duplicate[0].used_percent, 20.0);
    }

    #[test]
    fn provider_payloads_isolate_malformed_percentage_rows() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        for invalid in ["1e400", r#""NaN""#] {
            let codex: CodexUsageResponse = serde_json::from_str(&format!(
                r#"{{
                    "rate_limit": {{
                        "primary_window": {{
                            "used_percent": {invalid},
                            "reset_at": 1700003600,
                            "limit_window_seconds": 18000
                        }},
                        "secondary_window": {{
                            "used_percent": 20,
                            "reset_at": 1700003600,
                            "limit_window_seconds": 18000
                        }}
                    }}
                }}"#
            ))
            .unwrap();
            let codex_windows = codex_windows(codex.rate_limit.as_ref(), None, now);
            assert_eq!(codex_windows.len(), 1);
            assert_eq!(codex_windows[0].card_id, "main.session.v1");
            assert_eq!(codex_windows[0].used_percent, 20.0);

            let claude: ClaudeUsageResponse = serde_json::from_str(&format!(
                r#"{{
                    "five_hour": {{
                        "utilization": {invalid},
                        "resets_at": "2023-11-15T00:13:20Z"
                    }},
                    "seven_day": {{
                        "utilization": 20,
                        "resets_at": "2023-11-21T22:13:20Z"
                    }},
                    "seven_day_design": {{
                        "utilization": {invalid},
                        "resets_at": "2023-11-21T22:13:20Z"
                    }},
                    "design": {{
                        "utilization": 30,
                        "resets_at": "2023-11-21T22:13:20Z"
                    }},
                    "seven_day_routines": {{
                        "utilization": {invalid},
                        "resets_at": "2023-11-21T22:13:20Z"
                    }},
                    "routines": {{
                        "utilization": 40,
                        "resets_at": "2023-11-21T22:13:20Z"
                    }},
                    "extra_usage": {{
                        "is_enabled": true,
                        "utilization": {invalid}
                    }}
                }}"#
            ))
            .unwrap();
            let claude_windows = claude_windows(&claude, now);
            assert_eq!(claude_windows.len(), 3);
            assert!(claude_windows
                .iter()
                .any(|window| window.card_id == "weekly.v1" && window.used_percent == 20.0));
            assert!(claude_windows
                .iter()
                .any(|window| window.card_id == "design.weekly.v1" && window.used_percent == 30.0));
            assert!(claude_windows.iter().any(
                |window| window.card_id == "routines.weekly.v1" && window.used_percent == 40.0
            ));
        }
    }

    #[test]
    fn reads_claude_code_oauth_token_via_lookup() {
        let token = claude_token_from_lookup(|key| match key {
            "CLAUDE_CODE_OAUTH_TOKEN" => Some("  sk-ant-oat01-test  ".to_string()),
            _ => None,
        });
        assert_eq!(token.as_deref(), Some("sk-ant-oat01-test"));
        assert!(claude_token_from_lookup(|_| None).is_none());
        assert!(claude_token_from_lookup(|_| Some("   ".to_string())).is_none());
    }

    #[test]
    fn refreshes_or_expires_cached_windows() {
        let base = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let window =
            unified_ratelimit_window("Session", Some(0.2), Some(1_700_000_000 + 3600), base)
                .unwrap();

        // 30 min later, still before the reset: reset_text recomputed to the
        // shorter countdown (not the frozen original).
        let later = base + chrono::Duration::seconds(1800);
        let refreshed = refresh_cached_windows(std::slice::from_ref(&window), later).unwrap();
        assert_eq!(refreshed.len(), 1);
        assert!(refreshed[0].reset_text.as_deref().unwrap().contains("30m"));

        // Past the reset: stale -> expire (None) so the caller re-probes.
        let after = base + chrono::Duration::seconds(3700);
        assert!(refresh_cached_windows(std::slice::from_ref(&window), after).is_none());
    }

    struct RecordingRefreshScope<'a> {
        inner: &'a TestRefreshScope,
        transfers: Mutex<Vec<(Vec<u8>, Vec<u8>)>>,
    }

    impl<'a> RecordingRefreshScope<'a> {
        fn new(inner: &'a TestRefreshScope) -> Self {
            Self {
                inner,
                transfers: Mutex::new(Vec::new()),
            }
        }

        fn transfers(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
            self.transfers.lock().unwrap().clone()
        }
    }

    impl RefreshScopeTransaction for RecordingRefreshScope<'_> {
        fn resolve_current(
            &self,
            semantic_source: &str,
            canonical_location: &str,
            marker: &[u8],
        ) -> Result<AccountScope, AccountScopeError> {
            self.inner
                .resolve_current(semantic_source, canonical_location, marker)
        }

        fn transfer(
            &self,
            semantic_source: &str,
            canonical_location: &str,
            old_marker: &[u8],
            new_marker: &[u8],
        ) -> Result<AccountScope, AccountScopeError> {
            self.transfers
                .lock()
                .unwrap()
                .push((old_marker.to_vec(), new_marker.to_vec()));
            self.inner
                .transfer(semantic_source, canonical_location, old_marker, new_marker)
        }
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

    async fn codex_test_response(refresh_token: String) -> Result<Value, String> {
        assert_eq!(refresh_token, "codex-old-refresh");
        Ok(serde_json::json!({
            "access_token": "codex-new-access",
            "refresh_token": "codex-new-refresh"
        }))
    }

    fn setup_codex_refresh(
        tag: &str,
    ) -> (TestRefreshScope, PathBuf, AccountScope, Vec<u8>, String) {
        let scope = TestRefreshScope::new("codex", tag);
        let path = scope.root().join("codex/auth.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "tokens": {
                    "access_token": " codex-old-access ",
                    "refresh_token": " codex-old-refresh ",
                    "id_token": " codex-old-id "
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let credentials = load_codex_credentials_from(&path).unwrap();
        let location = credentials.scope_slot.canonical_location.clone();
        let old_scope = scope
            .resolve_current(
                credentials.scope_slot.semantic_source,
                &location,
                credentials.scope_marker(),
            )
            .unwrap();
        let metadata = scope.metadata_bytes();
        (scope, path, old_scope, metadata, location)
    }

    async fn run_codex_refresh(
        scope: &TestRefreshScope,
        path: &Path,
        crash: Option<RefreshCheckpoint>,
    ) -> Result<(CodexCredentials, Result<AccountScope, AccountScopeError>), String> {
        refresh_codex_credentials_with(
            path,
            scope,
            codex_test_response,
            save_codex_credentials,
            checkpoint_at(crash),
        )
        .await
    }

    #[tokio::test]
    async fn codex_refresh_canonicalizes_tokens_before_transfer_and_reload() {
        for (tag, refresh_value) in [
            ("missing", None),
            ("null", Some(Value::Null)),
            ("empty", Some(Value::String(String::new()))),
            ("whitespace", Some(Value::String(" \t\n ".to_string()))),
            (
                "non-string",
                Some(serde_json::json!({ "unexpected": true })),
            ),
        ] {
            let (scope, path, old_scope, _, _) =
                setup_codex_refresh(&format!("codex-canonical-{tag}"));
            let recording = RecordingRefreshScope::new(&scope);
            let mut response = serde_json::json!({
                "access_token": { "unexpected": true },
                "accessToken": " codex-new-access ",
                "id_token": " \t\n ",
                "idToken": " codex-new-id "
            });
            if let Some(refresh_value) = refresh_value {
                response
                    .as_object_mut()
                    .unwrap()
                    .insert("refresh_token".to_string(), refresh_value);
            }

            let (refreshed, scope_outcome) = refresh_codex_credentials_with(
                &path,
                &recording,
                move |refresh_token| async move {
                    assert_eq!(refresh_token, "codex-old-refresh");
                    Ok(response)
                },
                save_codex_credentials,
                checkpoint_at(None),
            )
            .await
            .unwrap();

            assert_eq!(refreshed.access_token, "codex-new-access", "{tag}");
            assert_eq!(
                refreshed.refresh_token.as_deref(),
                Some("codex-old-refresh"),
                "{tag}"
            );
            assert_eq!(refreshed.id_token.as_deref(), Some("codex-new-id"), "{tag}");
            assert_eq!(scope_outcome.unwrap(), old_scope, "{tag}");
            assert_eq!(
                recording.transfers(),
                vec![(b"codex-old-refresh".to_vec(), b"codex-old-refresh".to_vec())],
                "{tag}"
            );

            let stored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(
                stored["tokens"]["refresh_token"],
                Value::String("codex-old-refresh".to_string()),
                "{tag}"
            );
            let reloaded = load_codex_credentials_from(&path).unwrap();
            assert_eq!(reloaded.access_token, refreshed.access_token, "{tag}");
            assert_eq!(reloaded.refresh_token, refreshed.refresh_token, "{tag}");
            assert_eq!(reloaded.id_token, refreshed.id_token, "{tag}");
            assert_eq!(reloaded.scope_marker(), refreshed.scope_marker(), "{tag}");
            assert_eq!(
                scope
                    .resolve_current(
                        reloaded.scope_slot.semantic_source,
                        &reloaded.scope_slot.canonical_location,
                        reloaded.scope_marker(),
                    )
                    .unwrap(),
                old_scope,
                "{tag}"
            );
            scope.cleanup();
        }

        let (scope, path, old_scope, _, _) = setup_codex_refresh("codex-canonical-aliases");
        let recording = RecordingRefreshScope::new(&scope);
        let response = serde_json::json!({
            "access_token": " \t ",
            "accessToken": false,
            "refresh_token": null,
            "refreshToken": " codex-new-refresh ",
            "id_token": { "unexpected": true },
            "idToken": ""
        });
        let (refreshed, scope_outcome) = refresh_codex_credentials_with(
            &path,
            &recording,
            move |refresh_token| async move {
                assert_eq!(refresh_token, "codex-old-refresh");
                Ok(response)
            },
            save_codex_credentials,
            checkpoint_at(None),
        )
        .await
        .unwrap();

        assert_eq!(refreshed.access_token, "codex-old-access");
        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("codex-new-refresh")
        );
        assert_eq!(refreshed.id_token.as_deref(), Some("codex-old-id"));
        assert_eq!(scope_outcome.unwrap(), old_scope);
        assert_eq!(
            recording.transfers(),
            vec![(b"codex-old-refresh".to_vec(), b"codex-new-refresh".to_vec())]
        );
        let stored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            stored["tokens"]["refresh_token"],
            Value::String("codex-new-refresh".to_string())
        );
        let reloaded = load_codex_credentials_from(&path).unwrap();
        assert_eq!(reloaded.access_token, refreshed.access_token);
        assert_eq!(reloaded.refresh_token, refreshed.refresh_token);
        assert_eq!(reloaded.id_token, refreshed.id_token);
        assert_eq!(reloaded.scope_marker(), refreshed.scope_marker());
        scope.cleanup();
    }

    #[tokio::test]
    async fn codex_refresh_crash_boundaries_and_scope_gate_use_production_sequence() {
        for boundary in [
            RefreshCheckpoint::Reloaded,
            RefreshCheckpoint::NetworkReturned,
            RefreshCheckpoint::MetadataHandled,
            RefreshCheckpoint::CredentialsPersisted,
        ] {
            let (scope, path, old_scope, before, location) = setup_codex_refresh("codex-crash");
            assert_eq!(
                run_codex_refresh(&scope, &path, Some(boundary))
                    .await
                    .unwrap_err(),
                "injected crash"
            );
            let stored = load_codex_credentials_from(&path).unwrap();
            assert_eq!(
                stored.refresh_token.as_deref(),
                Some(if boundary == RefreshCheckpoint::CredentialsPersisted {
                    "codex-new-refresh"
                } else {
                    "codex-old-refresh"
                })
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
                        .resolve_current("codex-auth-json", &location, b"codex-old-refresh")
                        .unwrap(),
                    old_scope
                );
                assert_eq!(
                    scope
                        .resolve_current("codex-auth-json", &location, b"codex-new-refresh")
                        .unwrap(),
                    old_scope
                );
            }
            scope.cleanup();
        }

        let (scope, path, old_scope, before, location) = setup_codex_refresh("codex-metadata-fail");
        scope.fail_metadata_save();
        let (refreshed, scope_outcome) = run_codex_refresh(&scope, &path, None).await.unwrap();
        assert_eq!(refreshed.access_token, "codex-new-access");
        assert_eq!(scope_outcome, Err(AccountScopeError::MetadataWrite));
        assert_eq!(scope.metadata_bytes(), before);
        let persisted = load_codex_credentials_from(&path).unwrap();
        assert_eq!(persisted.access_token, "codex-old-access");
        assert_eq!(
            persisted.refresh_token.as_deref(),
            Some("codex-old-refresh")
        );
        assert_eq!(
            scope
                .resolve_current("codex-auth-json", &location, persisted.scope_marker())
                .unwrap(),
            old_scope
        );
        scope.cleanup();

        let (scope, path, _old_scope, before, _) =
            setup_codex_refresh("codex-metadata-fail-unchanged");
        scope.fail_metadata_save();
        let (refreshed, scope_outcome) = refresh_codex_credentials_with(
            &path,
            &scope,
            |refresh_token| async move {
                assert_eq!(refresh_token, "codex-old-refresh");
                Ok(serde_json::json!({ "access_token": "codex-new-access" }))
            },
            save_codex_credentials,
            checkpoint_at(None),
        )
        .await
        .unwrap();
        assert_eq!(scope_outcome, Err(AccountScopeError::MetadataWrite));
        assert_eq!(scope.metadata_bytes(), before);
        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("codex-old-refresh")
        );
        let persisted = load_codex_credentials_from(&path).unwrap();
        assert_eq!(persisted.access_token, "codex-new-access");
        assert_eq!(
            persisted.refresh_token.as_deref(),
            Some("codex-old-refresh")
        );
        scope.cleanup();

        let (scope, path, old_scope, _, location) = setup_codex_refresh("codex-success");
        let (_, scope_outcome) = run_codex_refresh(&scope, &path, None).await.unwrap();
        assert_eq!(scope_outcome.unwrap(), old_scope);
        assert_eq!(
            scope
                .resolve_current("codex-auth-json", &location, b"codex-new-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();
    }

    async fn claude_test_response(refresh_token: String) -> Result<ClaudeRefreshResponse, String> {
        assert_eq!(refresh_token, "claude-old-refresh");
        Ok(ClaudeRefreshResponse {
            access_token: "claude-new-access".to_string(),
            refresh_token: Some("claude-new-refresh".to_string()),
            expires_in: 3_600,
        })
    }

    fn setup_claude_refresh(
        tag: &str,
    ) -> (
        TestRefreshScope,
        PathBuf,
        ClaudeCredentials,
        AccountScope,
        Vec<u8>,
        String,
    ) {
        let scope = TestRefreshScope::new("claude", tag);
        let path = scope.root().join("claude/.credentials.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "claude-old-access",
                "refreshToken": "claude-old-refresh",
                "expiresAt": 0
            }
        })
        .to_string();
        fs::write(&path, &raw).unwrap();
        let mut credentials =
            parse_claude_credentials_data(&raw, ClaudeCredentialSource::File).unwrap();
        credentials.scope_slot = CredentialSlot {
            semantic_source: "claude-login-file",
            canonical_location: agent_account_scope::canonical_file_location(
                &path,
                Some("claudeAiOauth"),
            )
            .unwrap(),
        };
        let location = credentials.scope_slot.canonical_location.clone();
        let old_scope = scope
            .resolve_current(
                credentials.scope_slot.semantic_source,
                &location,
                credentials.scope_marker().unwrap(),
            )
            .unwrap();
        let metadata = scope.metadata_bytes();
        (scope, path, credentials, old_scope, metadata, location)
    }

    async fn run_claude_refresh(
        scope: &TestRefreshScope,
        path: &Path,
        original: &ClaudeCredentials,
        crash: Option<RefreshCheckpoint>,
    ) -> Result<(ClaudeCredentials, Result<AccountScope, AccountScopeError>), String> {
        let reload_path = path.to_path_buf();
        let save_path = path.to_path_buf();
        refresh_claude_credentials_with(
            original,
            scope,
            move |template| {
                let raw = fs::read_to_string(&reload_path)
                    .map_err(|error| format!("reload Claude test credentials: {error}"))?;
                let mut credentials =
                    parse_claude_credentials_data(&raw, ClaudeCredentialSource::File)?;
                credentials.scope_slot = template.scope_slot.clone();
                Ok(credentials)
            },
            claude_test_response,
            move |credentials| save_claude_credentials_to_file(credentials, &save_path),
            checkpoint_at(crash),
        )
        .await
    }

    fn stored_claude_refresh_token(path: &Path) -> Option<String> {
        parse_claude_credentials_data(
            &fs::read_to_string(path).unwrap(),
            ClaudeCredentialSource::File,
        )
        .unwrap()
        .refresh_token
    }

    #[tokio::test]
    async fn claude_refresh_invalid_new_refresh_preserves_old_marker_and_store() {
        for (tag, refresh_value) in [
            ("claude-invalid-refresh-empty", serde_json::json!("")),
            (
                "claude-invalid-refresh-non-string",
                serde_json::json!({ "unexpected": true }),
            ),
        ] {
            let (scope, path, original, old_scope, _, location) = setup_claude_refresh(tag);
            let response: ClaudeRefreshResponse = serde_json::from_value(serde_json::json!({
                "access_token": "claude-new-access",
                "refresh_token": refresh_value,
                "expires_in": 3600
            }))
            .unwrap();
            let reload_path = path.clone();
            let save_path = path.clone();
            let (refreshed, scope_outcome) = refresh_claude_credentials_with(
                &original,
                &scope,
                move |template| {
                    let raw = fs::read_to_string(&reload_path)
                        .map_err(|error| format!("reload Claude test credentials: {error}"))?;
                    let mut credentials =
                        parse_claude_credentials_data(&raw, ClaudeCredentialSource::File)?;
                    credentials.scope_slot = template.scope_slot.clone();
                    Ok(credentials)
                },
                move |refresh_token| async move {
                    assert_eq!(refresh_token, "claude-old-refresh");
                    Ok(response)
                },
                move |credentials| save_claude_credentials_to_file(credentials, &save_path),
                checkpoint_at(None),
            )
            .await
            .unwrap();

            assert_eq!(refreshed.access_token, "claude-new-access");
            assert_eq!(
                refreshed.refresh_token.as_deref(),
                Some("claude-old-refresh")
            );
            assert_eq!(scope_outcome.unwrap(), old_scope);
            assert_eq!(
                scope
                    .resolve_current("claude-login-file", &location, b"claude-old-refresh")
                    .unwrap(),
                old_scope
            );
            assert_eq!(
                stored_claude_refresh_token(&path).as_deref(),
                Some("claude-old-refresh")
            );
            let stored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(
                stored["claudeAiOauth"]["refreshToken"],
                Value::String("claude-old-refresh".to_string())
            );
            scope.cleanup();
        }
    }

    #[tokio::test]
    async fn claude_refresh_crash_boundaries_and_scope_gate_use_production_sequence() {
        for boundary in [
            RefreshCheckpoint::Reloaded,
            RefreshCheckpoint::NetworkReturned,
            RefreshCheckpoint::MetadataHandled,
            RefreshCheckpoint::CredentialsPersisted,
        ] {
            let (scope, path, original, old_scope, before, location) =
                setup_claude_refresh("claude-crash");
            assert_eq!(
                run_claude_refresh(&scope, &path, &original, Some(boundary))
                    .await
                    .unwrap_err(),
                "injected crash"
            );
            assert_eq!(
                stored_claude_refresh_token(&path).as_deref(),
                Some(if boundary == RefreshCheckpoint::CredentialsPersisted {
                    "claude-new-refresh"
                } else {
                    "claude-old-refresh"
                })
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
                        .resolve_current("claude-login-file", &location, b"claude-old-refresh")
                        .unwrap(),
                    old_scope
                );
                assert_eq!(
                    scope
                        .resolve_current("claude-login-file", &location, b"claude-new-refresh")
                        .unwrap(),
                    old_scope
                );
            }
            scope.cleanup();
        }

        let (scope, path, original, old_scope, before, location) =
            setup_claude_refresh("claude-metadata-fail");
        scope.fail_metadata_save();
        let (refreshed, scope_outcome) = run_claude_refresh(&scope, &path, &original, None)
            .await
            .unwrap();
        assert_eq!(refreshed.access_token, "claude-new-access");
        assert_eq!(scope_outcome, Err(AccountScopeError::MetadataWrite));
        assert_eq!(scope.metadata_bytes(), before);
        assert_eq!(
            stored_claude_refresh_token(&path).as_deref(),
            Some("claude-old-refresh")
        );
        assert_eq!(
            scope
                .resolve_current("claude-login-file", &location, b"claude-old-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();

        let (scope, path, original, _old_scope, before, _) =
            setup_claude_refresh("claude-metadata-fail-unchanged");
        scope.fail_metadata_save();
        let reload_path = path.clone();
        let save_path = path.clone();
        let (refreshed, scope_outcome) = refresh_claude_credentials_with(
            &original,
            &scope,
            move |template| {
                let raw = fs::read_to_string(&reload_path)
                    .map_err(|error| format!("reload Claude test credentials: {error}"))?;
                let mut credentials =
                    parse_claude_credentials_data(&raw, ClaudeCredentialSource::File)?;
                credentials.scope_slot = template.scope_slot.clone();
                Ok(credentials)
            },
            |refresh_token| async move {
                assert_eq!(refresh_token, "claude-old-refresh");
                Ok(ClaudeRefreshResponse {
                    access_token: "claude-new-access".to_string(),
                    refresh_token: None,
                    expires_in: 3_600,
                })
            },
            move |credentials| save_claude_credentials_to_file(credentials, &save_path),
            checkpoint_at(None),
        )
        .await
        .unwrap();
        assert_eq!(scope_outcome, Err(AccountScopeError::MetadataWrite));
        assert_eq!(scope.metadata_bytes(), before);
        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("claude-old-refresh")
        );
        let persisted = parse_claude_credentials_data(
            &fs::read_to_string(&path).unwrap(),
            ClaudeCredentialSource::File,
        )
        .unwrap();
        assert_eq!(persisted.access_token, "claude-new-access");
        assert_eq!(
            persisted.refresh_token.as_deref(),
            Some("claude-old-refresh")
        );
        scope.cleanup();

        let (scope, path, original, old_scope, _, location) =
            setup_claude_refresh("claude-success");
        let (_, scope_outcome) = run_claude_refresh(&scope, &path, &original, None)
            .await
            .unwrap();
        assert_eq!(scope_outcome.unwrap(), old_scope);
        assert_eq!(
            scope
                .resolve_current("claude-login-file", &location, b"claude-new-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();
    }

    #[test]
    fn stage4_codex_and_claude_matrix_assigns_semantic_keys() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let rate_limit = CodexRateLimit {
            primary_window: Some(CodexWindow {
                used_percent: 8.0,
                reset_at: now.timestamp() + 18_000,
                limit_window_seconds: 18_000,
            }),
            secondary_window: Some(CodexWindow {
                used_percent: 35.0,
                reset_at: now.timestamp() + 604_800,
                limit_window_seconds: 604_800,
            }),
        };
        let codex = codex_windows(Some(&rate_limit), None, now);
        assert_eq!(
            codex[0].pace_status.window_key.as_deref(),
            Some("main.session.v1")
        );
        assert_eq!(
            codex[1].pace_status.window_key.as_deref(),
            Some("main.weekly.v1")
        );

        let claude = ClaudeUsageResponse {
            five_hour: Some(ClaudeWindow {
                utilization: Some(10.0),
                resets_at: Some("2026-07-18T00:00:00Z".to_string()),
            }),
            seven_day: Some(ClaudeWindow {
                utilization: Some(20.0),
                resets_at: Some("2026-07-19T00:00:00Z".to_string()),
            }),
            ..Default::default()
        };
        let claude = claude_windows(&claude, now);
        assert_eq!(
            claude[0].pace_status.window_key.as_deref(),
            Some("session.v1")
        );
        assert_eq!(
            claude[1].pace_status.window_key.as_deref(),
            Some("weekly.v1")
        );
        assert_eq!(claude[0].window_minutes_for_test(), Some(300));
        assert_eq!(claude[1].window_minutes_for_test(), Some(10_080));
    }

    #[test]
    fn stage4_duplicate_snapshot_rows_are_removed_before_history_and_wire() {
        let scope = TestRefreshScope::new("stage4", "duplicate-rows");
        let account_scope = scope
            .resolve_current("fixture", "duplicate", b"duplicate-marker")
            .unwrap();
        let now = 1_700_000_000;
        let reset = Utc.timestamp_opt(now + 86_400, 0).single().unwrap();
        let make_window = |label: &str, card_id: &str, window_key: &str, used: f64| {
            UsageWindow::from_provider_used_percent(
                label.to_string(),
                used,
                Some(reset),
                Utc.timestamp_opt(now, 0).single().unwrap(),
            )
            .with_identity(
                card_id,
                Some(window_key.to_string()),
                None,
                Some(DurationEvidence::contract(86_400)),
            )
        };
        let mut snapshot = AgentUsageSnapshot {
            client_id: "fixture".to_string(),
            source: "fixture".to_string(),
            updated_at: String::new(),
            identity: None,
            account_scope: Ok(account_scope),
            windows: vec![
                make_window("First", "shared-card.v1", "first.v1", 10.0),
                make_window("Duplicate key", "second-card.v1", "first.v1", 20.0),
                make_window("Duplicate card", "shared-card.v1", "third.v1", 30.0),
            ],
            credits: None,
            error: None,
        };

        enrich_snapshot_with(&mut snapshot, now, |active, observations, _| {
            assert_eq!(active.len(), 1);
            assert_eq!(observations.len(), 1);
            assert_eq!(active[0].window_key, "first.v1");
            assert_eq!(observations[0].used_percent, 10.0);
            Ok(vec![Ok((HistoryOutcome::LearningDuration, None, 0))])
        });

        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].label_for_test(), "First");
        let wire = serde_json::to_value(&snapshot).unwrap();
        let rows = wire["windows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["cardId"], "shared-card.v1");
        assert_eq!(rows[0]["paceStatus"]["windowKey"], "first.v1");
        scope.cleanup();
    }

    #[test]
    fn stage4_chained_identity_collisions_keep_only_actual_uniques() {
        let scope = TestRefreshScope::new("stage4", "chained-collisions");
        let account_scope = scope
            .resolve_current("fixture", "chained", b"chained-marker")
            .unwrap();
        let now = 1_700_000_000;
        let reset = Utc.timestamp_opt(now + 86_400, 0).single().unwrap();
        let make_window = |label: &str, card_id: &str, window_key: &str, used: f64| {
            UsageWindow::from_provider_used_percent(
                label.to_string(),
                used,
                Some(reset),
                Utc.timestamp_opt(now, 0).single().unwrap(),
            )
            .with_identity(
                card_id,
                Some(window_key.to_string()),
                None,
                Some(DurationEvidence::contract(86_400)),
            )
        };
        let mut snapshot = AgentUsageSnapshot {
            client_id: "fixture".to_string(),
            source: "fixture".to_string(),
            updated_at: String::new(),
            identity: None,
            account_scope: Ok(account_scope),
            windows: vec![
                make_window("A/X", "a.v1", "x.v1", 10.0),
                make_window("A/Y", "a.v1", "y.v1", 20.0),
                make_window("C/Y", "c.v1", "y.v1", 30.0),
                make_window("B/X", "b.v1", "x.v1", 40.0),
                make_window("B/Z", "b.v1", "z.v1", 50.0),
            ],
            credits: None,
            error: None,
        };

        enrich_snapshot_with(&mut snapshot, now, |active, observations, _| {
            assert_eq!(active.len(), 3);
            assert_eq!(observations.len(), 3);
            assert_eq!(active[0].window_key, "x.v1");
            assert_eq!(active[1].window_key, "y.v1");
            assert_eq!(active[2].window_key, "z.v1");
            assert_eq!(
                observations
                    .iter()
                    .map(|observation| observation.used_percent)
                    .collect::<Vec<_>>(),
                vec![10.0, 30.0, 50.0]
            );
            Ok(vec![
                Ok((HistoryOutcome::LearningDuration, None, 0)),
                Ok((HistoryOutcome::LearningDuration, None, 0)),
                Ok((HistoryOutcome::LearningDuration, None, 0)),
            ])
        });

        assert_eq!(snapshot.windows.len(), 3);
        assert_eq!(
            snapshot
                .windows
                .iter()
                .map(UsageWindow::label_for_test)
                .collect::<Vec<_>>(),
            vec!["A/X", "C/Y", "B/Z"]
        );
        let wire = serde_json::to_value(&snapshot).unwrap();
        let rows = wire["windows"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row["cardId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["a.v1", "c.v1", "b.v1"]
        );
        assert_eq!(
            rows.iter()
                .map(|row| row["paceStatus"]["windowKey"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["x.v1", "y.v1", "z.v1"]
        );
        scope.cleanup();
    }

    #[test]
    fn stage4_batch_maps_results_once_without_network() {
        let scope = TestRefreshScope::new("stage4", "batch-map");
        let account_scope = scope
            .resolve_current("fixture", "batch", b"batch-marker")
            .unwrap();
        let now = 1_700_000_000;
        let reset = Utc.timestamp_opt(now + 86_400, 0).single().unwrap();
        let mut snapshot = AgentUsageSnapshot {
            client_id: "fixture".to_string(),
            source: "fixture".to_string(),
            updated_at: String::new(),
            identity: None,
            account_scope: Ok(account_scope),
            windows: vec![
                UsageWindow::from_provider_used_percent(
                    "First".to_string(),
                    20.0,
                    Some(reset),
                    Utc.timestamp_opt(now, 0).single().unwrap(),
                )
                .with_identity(
                    "first.v1",
                    Some("first.v1".to_string()),
                    None,
                    Some(DurationEvidence::contract(86_400)),
                ),
                UsageWindow::from_provider_used_percent(
                    "Second".to_string(),
                    40.0,
                    Some(reset),
                    Utc.timestamp_opt(now, 0).single().unwrap(),
                )
                .with_identity(
                    "second.v1",
                    Some("second.v1".to_string()),
                    None,
                    Some(DurationEvidence::contract(86_400)),
                ),
            ],
            credits: None,
            error: None,
        };
        let calls = std::cell::Cell::new(0);
        enrich_snapshot_with(&mut snapshot, now, |active, observations, _| {
            calls.set(calls.get() + 1);
            assert_eq!(active.len(), 2);
            assert_eq!(observations.len(), 2);
            assert_eq!(active[0].window_key, "first.v1");
            assert_eq!(active[1].window_key, "second.v1");
            Ok(vec![
                Ok((HistoryOutcome::LearningDuration, None, 0)),
                Ok((
                    HistoryOutcome::Ready {
                        duration_seconds: 86_400,
                        source: DurationSource::Contract,
                        sampled: true,
                    },
                    Some(HistoricalPace {
                        expected_percent: 42.0,
                        eta_seconds: Some(900.0),
                        will_last_to_reset: false,
                        run_out_probability: Some(0.25),
                    }),
                    4,
                )),
            ])
        });
        assert_eq!(
            calls.get(),
            1,
            "one snapshot means one batch and no new request"
        );
        assert_eq!(
            snapshot.windows[0].pace_status.state,
            PaceState::LearningDuration
        );
        assert_eq!(snapshot.windows[1].pace_status.state, PaceState::Available);
        assert_eq!(snapshot.windows[1].window_minutes_for_test(), Some(1_440));
        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(wire["windows"][0]["paceStatus"]["completeCycles"], 0);
        assert_eq!(wire["windows"][1]["paceStatus"]["completeCycles"], 4);
        scope.cleanup();
    }

    #[test]
    fn stage4_learning_history_uses_batch_complete_cycles() {
        let scope = TestRefreshScope::new("stage4", "learning-history");
        let account_scope = scope
            .resolve_current("fixture", "learning", b"learning-marker")
            .unwrap();
        let now = 1_700_000_000;
        let reset = Utc.timestamp_opt(now + 86_400, 0).single().unwrap();
        let mut snapshot = AgentUsageSnapshot {
            client_id: "fixture".to_string(),
            source: "fixture".to_string(),
            updated_at: String::new(),
            identity: None,
            account_scope: Ok(account_scope),
            windows: vec![UsageWindow::from_provider_used_percent(
                "Weekly".to_string(),
                20.0,
                Some(reset),
                Utc.timestamp_opt(now, 0).single().unwrap(),
            )
            .with_identity(
                "weekly.v1",
                Some("weekly.v1".to_string()),
                None,
                Some(DurationEvidence::contract(86_400)),
            )],
            credits: None,
            error: None,
        };

        enrich_snapshot_with(&mut snapshot, now, |_, _, _| {
            Ok(vec![Ok((
                HistoryOutcome::Ready {
                    duration_seconds: 86_400,
                    source: DurationSource::Contract,
                    sampled: true,
                },
                None,
                2,
            ))])
        });

        assert_eq!(
            snapshot.windows[0].pace_status.state,
            PaceState::LearningHistory
        );
        assert_eq!(snapshot.windows[0].pace_status.complete_cycles, 2);
        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(wire["windows"][0]["paceStatus"]["completeCycles"], 2);
        scope.cleanup();
    }

    #[test]
    fn stage4_incoherent_historical_result_is_typed_unavailable() {
        let scope = TestRefreshScope::new("stage4", "incoherent-history");
        let account_scope = scope
            .resolve_current("fixture", "incoherent", b"incoherent-marker")
            .unwrap();
        let now = 1_700_000_000;
        let reset = Utc.timestamp_opt(now + 86_400, 0).single().unwrap();
        let mut snapshot = AgentUsageSnapshot {
            client_id: "fixture".to_string(),
            source: "fixture".to_string(),
            updated_at: String::new(),
            identity: None,
            account_scope: Ok(account_scope),
            windows: vec![UsageWindow::from_provider_used_percent(
                "Weekly".to_string(),
                20.0,
                Some(reset),
                Utc.timestamp_opt(now, 0).single().unwrap(),
            )
            .with_identity(
                "weekly.v1",
                Some("weekly.v1".to_string()),
                None,
                Some(DurationEvidence::contract(86_400)),
            )],
            credits: None,
            error: None,
        };

        enrich_snapshot_with(&mut snapshot, now, |_, _, _| {
            Ok(vec![Ok((
                HistoryOutcome::Ready {
                    duration_seconds: 86_400,
                    source: DurationSource::Contract,
                    sampled: true,
                },
                Some(HistoricalPace {
                    expected_percent: 42.0,
                    eta_seconds: Some(900.0),
                    will_last_to_reset: true,
                    run_out_probability: Some(0.25),
                }),
                4,
            ))])
        });

        assert_eq!(
            snapshot.windows[0].pace_status.state,
            PaceState::Unavailable
        );
        assert_eq!(
            snapshot.windows[0].pace_status.reason.as_deref(),
            Some("history")
        );
        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(wire["windows"][0]["paceStatus"]["state"], "unavailable");
        assert!(wire["windows"][0].get("historicalPace").is_none());
        scope.cleanup();
    }

    #[test]
    fn stage4_historical_eta_and_will_last_are_exactly_coherent() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let reset = now + chrono::Duration::days(1);
        let base =
            UsageWindow::from_provider_used_percent("Daily".to_string(), 30.0, Some(reset), now)
                .with_identity(
                    "daily.v1",
                    Some("daily.v1".to_string()),
                    None,
                    Some(DurationEvidence::contract(86_400)),
                );
        let cases = [
            (
                "will-last",
                HistoricalPace {
                    expected_percent: 42.0,
                    eta_seconds: None,
                    will_last_to_reset: true,
                    run_out_probability: Some(0.1),
                },
                true,
            ),
            (
                "will-run-out",
                HistoricalPace {
                    expected_percent: 42.0,
                    eta_seconds: Some(900.0),
                    will_last_to_reset: false,
                    run_out_probability: Some(0.25),
                },
                true,
            ),
            (
                "will-last-with-eta",
                HistoricalPace {
                    expected_percent: 42.0,
                    eta_seconds: Some(900.0),
                    will_last_to_reset: true,
                    run_out_probability: Some(0.25),
                },
                false,
            ),
            (
                "will-run-out-without-eta",
                HistoricalPace {
                    expected_percent: 42.0,
                    eta_seconds: None,
                    will_last_to_reset: false,
                    run_out_probability: Some(0.25),
                },
                false,
            ),
        ];

        for (label, pace, expected) in cases {
            assert_eq!(historical_pace_is_coherent(&pace), expected, "{label}");
            let mut window = base.clone();
            window.pace_status.state = PaceState::Available;
            window.historical_pace = Some(historical_pace_payload(pace));
            assert_eq!(serde_json::to_value(&window).is_ok(), expected, "{label}");
        }
    }

    #[test]
    fn stage4_scope_error_is_sticky_and_skips_history() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let mut snapshot = AgentUsageSnapshot {
            client_id: "fixture".to_string(),
            source: "fixture".to_string(),
            updated_at: String::new(),
            identity: None,
            account_scope: Err(AccountScopeError::MetadataWrite),
            windows: vec![
                UsageWindow::from_provider_used_percent(
                    "Session".to_string(),
                    20.0,
                    Some(now + chrono::Duration::hours(5)),
                    now,
                )
                .with_identity(
                    "session.v1",
                    Some("session.v1".to_string()),
                    None,
                    Some(DurationEvidence::contract(300 * 60)),
                ),
                UsageWindow::from_provider_used_percent(
                    "Unknown".to_string(),
                    30.0,
                    Some(now + chrono::Duration::hours(5)),
                    now,
                )
                .with_identity("row.unknown.v1", None, None, None),
            ],
            credits: None,
            error: None,
        };
        let calls = std::cell::Cell::new(0);
        enrich_snapshot_with(&mut snapshot, now.timestamp(), |_, _, _| {
            calls.set(calls.get() + 1);
            Ok(Vec::new())
        });
        assert_eq!(calls.get(), 0);
        assert_eq!(
            snapshot.windows[0].pace_status.reason.as_deref(),
            Some("accountScope")
        );
        assert_eq!(
            snapshot.windows[0].pace_status.state,
            PaceState::Unavailable
        );
        assert_eq!(
            snapshot.windows[1].pace_status.reason.as_deref(),
            Some("windowIdentity")
        );
        assert!(snapshot.windows[1].pace_status.window_key.is_none());
        assert!(serde_json::to_value(&snapshot).is_ok());
    }

    #[test]
    fn stage4_wire_rejects_internal_nested_drift_and_preserves_observed_learning() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let reset = now + chrono::Duration::days(1);
        let base =
            UsageWindow::from_provider_used_percent("Daily".to_string(), 30.0, Some(reset), now)
                .with_identity(
                    "daily.v1",
                    Some("daily.v1".to_string()),
                    None,
                    Some(DurationEvidence::contract(86_400)),
                );

        let mut key_drift = base.clone();
        key_drift.pace_status.window_key = Some("other.v1".to_string());
        assert!(serde_json::to_value(&key_drift).is_err());

        let mut duration_drift = base.clone();
        duration_drift.pace_status.duration_seconds = Some(3_600);
        assert!(serde_json::to_value(&duration_drift).is_err());

        let mut source_drift = base.clone();
        source_drift.pace_status.duration_source = Some(DurationSource::Provider);
        assert!(serde_json::to_value(&source_drift).is_err());

        let mut minutes_drift = base.clone();
        minutes_drift.window_minutes = Some(1);
        assert!(serde_json::to_value(&minutes_drift).is_err());

        let mut learning =
            UsageWindow::from_provider_used_percent("Learning".to_string(), 30.0, Some(reset), now)
                .with_identity("learning.v1", Some("learning.v1".to_string()), None, None);
        learning.duration_source = Some(DurationSource::Observed);
        learning.pace_status.duration_source = Some(DurationSource::Observed);
        let wire = serde_json::to_value(&learning).unwrap();
        assert_eq!(wire["paceStatus"]["state"], "learningDuration");
        assert_eq!(wire["paceStatus"]["durationSource"], "observed");
        assert!(wire["paceStatus"].get("durationSeconds").is_none());
    }

    #[test]
    fn stage4_wire_rejects_available_without_historical_pace() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let mut window = UsageWindow::from_provider_used_percent(
            "Weekly".to_string(),
            30.0,
            Some(now + chrono::Duration::days(7)),
            now,
        )
        .with_identity(
            "weekly.v1",
            Some("weekly.v1".to_string()),
            None,
            Some(DurationEvidence::contract(7 * 24 * 60 * 60)),
        );
        window.pace_status.state = PaceState::Available;
        window.historical_pace = None;
        assert!(serde_json::to_value(&window).is_err());
    }

    #[test]
    fn provider_quota_pace_v3_fixture_locks_production_serializer() {
        fn window(
            card_id: &str,
            label: &str,
            used_percent: f64,
            resets_at: Option<&str>,
            window_key: Option<&str>,
            state: PaceState,
            duration_seconds: Option<i64>,
            duration_source: Option<DurationSource>,
            complete_cycles: usize,
            reason: Option<&str>,
            historical_pace: Option<HistoricalPacePayload>,
        ) -> UsageWindow {
            UsageWindow {
                card_id: card_id.to_string(),
                label: label.to_string(),
                used_percent,
                remaining_percent: 100.0 - used_percent,
                resets_at: resets_at.map(|value| value.to_string()),
                reset_text: None,
                window_minutes: duration_seconds.map(|seconds| seconds / 60),
                window_key: window_key.map(|value| value.to_string()),
                duration_seconds,
                duration_source,
                provider_duration: None,
                contract_duration: None,
                pace_status: PaceStatusPayload {
                    state,
                    window_key: window_key.map(|value| value.to_string()),
                    duration_seconds,
                    duration_source,
                    complete_cycles,
                    reason: reason.map(|value| value.to_string()),
                },
                historical_pace,
            }
        }

        let payload = AgentUsagePayload {
            generated_at: "2026-07-10T12:00:00.000Z".to_string(),
            agents: vec![AgentUsageSnapshot {
                client_id: "provider-fixture.invalid".to_string(),
                source: "fixture.invalid".to_string(),
                updated_at: "2026-07-10T12:00:00.000Z".to_string(),
                identity: None,
                account_scope: Err(AccountScopeError::NoTrustedEvidence),
                windows: vec![
                    window(
                        "ahead.invalid",
                        "Ahead quota",
                        72.0,
                        Some("2026-07-10T15:00:00Z"),
                        Some("quota.ahead.invalid"),
                        PaceState::Available,
                        Some(18_000),
                        Some(DurationSource::Provider),
                        5,
                        None,
                        Some(HistoricalPacePayload {
                            expected_used_percent: 32.0,
                            eta_seconds: Some(3_600.0),
                            will_last_to_reset: false,
                            run_out_probability: Some(0.75),
                        }),
                    ),
                    window(
                        "behind.invalid",
                        "Behind quota",
                        28.0,
                        Some("2026-07-15T12:00:00Z"),
                        Some("quota.behind.invalid"),
                        PaceState::Available,
                        Some(604_800),
                        Some(DurationSource::Contract),
                        7,
                        None,
                        Some(HistoricalPacePayload {
                            expected_used_percent: 56.0,
                            eta_seconds: None,
                            will_last_to_reset: true,
                            run_out_probability: Some(0.2),
                        }),
                    ),
                    window(
                        "learning-history.invalid",
                        "Learning history",
                        40.0,
                        Some("2026-07-10T15:00:00Z"),
                        Some("quota.learning-history.invalid"),
                        PaceState::LearningHistory,
                        Some(18_000),
                        Some(DurationSource::Provider),
                        2,
                        None,
                        None,
                    ),
                    window(
                        "learning-duration.invalid",
                        "Learning duration",
                        40.0,
                        Some("2026-07-10T15:00:00Z"),
                        Some("quota.learning-duration.invalid"),
                        PaceState::LearningDuration,
                        None,
                        Some(DurationSource::Observed),
                        0,
                        None,
                        None,
                    ),
                    window(
                        "missing-reset.invalid",
                        "Missing reset",
                        50.0,
                        None,
                        Some("quota.missing-reset.invalid"),
                        PaceState::Unavailable,
                        None,
                        None,
                        0,
                        Some("missingReset"),
                        None,
                    ),
                    window(
                        "shared-first.invalid",
                        "Shared label",
                        10.0,
                        Some("2026-07-10T15:00:00Z"),
                        Some("quota.shared-first.invalid"),
                        PaceState::LearningHistory,
                        Some(18_000),
                        Some(DurationSource::Provider),
                        2,
                        None,
                        None,
                    ),
                    window(
                        "shared-second.invalid",
                        "Shared label",
                        20.0,
                        Some("2026-07-10T15:00:00Z"),
                        Some("quota.shared-second.invalid"),
                        PaceState::LearningHistory,
                        Some(18_000),
                        Some(DurationSource::Provider),
                        2,
                        None,
                        None,
                    ),
                ],
                credits: None,
                error: None,
            }],
            opencode_subscriptions: Vec::new(),
        };

        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../Fixtures/CrossCheck/provider-quota-pace-v3.json");
        let fixture: Value = serde_json::from_str(
            &fs::read_to_string(&fixture_path)
                .unwrap_or_else(|error| panic!("read {}: {error}", fixture_path.display())),
        )
        .unwrap_or_else(|error| panic!("decode {}: {error}", fixture_path.display()));
        assert_eq!(fixture["schemaVersion"], 3);
        assert_eq!(fixture["payload"], serde_json::to_value(payload).unwrap());
    }
}
