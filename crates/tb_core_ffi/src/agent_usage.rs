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
use hyper_util::client::legacy::connect::dns::{
    GaiResolver as HyperGaiResolver, Name as HyperDnsName,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tower_service::Service;

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
    /// Monotonic order assigned by the Rust publication gate, not by wall time
    /// or provider completion order.
    publication_generation: u64,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_diagnostic: Option<SafeTransportDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TransportCategory {
    Timeout,
    Dns,
    Tls,
    ConnectionRefused,
    ConnectionReset,
    Connect,
    Request,
    ResponseBody,
    RateLimited,
    ServerError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SafeTransportDiagnostic {
    category: TransportCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_code: Option<i32>,
}

impl SafeTransportDiagnostic {
    pub(crate) fn from_facts(facts: TransportErrorFacts) -> Self {
        let category = if facts.is_timeout {
            TransportCategory::Timeout
        } else {
            match facts.raw_os_code {
                Some(61 | 111 | 10061) => TransportCategory::ConnectionRefused,
                Some(54 | 104 | 10054) => TransportCategory::ConnectionReset,
                _ if facts.is_dns => TransportCategory::Dns,
                _ if facts.is_tls => TransportCategory::Tls,
                _ if facts.is_connect => TransportCategory::Connect,
                _ if facts.phase == TransportPhase::ResponseBody => TransportCategory::ResponseBody,
                _ => TransportCategory::Request,
            }
        };
        Self {
            category,
            status: None,
            os_code: facts.raw_os_code,
        }
    }

    fn rate_limited(status: u16) -> Self {
        Self {
            category: TransportCategory::RateLimited,
            status: (100..=599).contains(&status).then_some(status),
            os_code: None,
        }
    }

    fn server_error(status: u16) -> Self {
        Self {
            category: TransportCategory::ServerError,
            status: (100..=599).contains(&status).then_some(status),
            os_code: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportPhase {
    Request,
    ResponseBody,
}

#[derive(Debug)]
struct DnsResolutionError {
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl DnsResolutionError {
    fn new(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

impl std::fmt::Display for DnsResolutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DNS resolution failed")
    }
}

impl std::error::Error for DnsResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug, Clone, Copy)]
struct TypedGaiResolver;

impl reqwest::dns::Resolve for TypedGaiResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let parsed_name = name.as_str().parse::<HyperDnsName>();
        Box::pin(async move {
            let parsed_name = parsed_name.map_err(|source| {
                Box::new(DnsResolutionError::new(source))
                    as Box<dyn std::error::Error + Send + Sync>
            })?;
            let addresses = HyperGaiResolver::new()
                .call(parsed_name)
                .await
                .map_err(|source| {
                    Box::new(DnsResolutionError::new(source))
                        as Box<dyn std::error::Error + Send + Sync>
                })?;
            Ok(Box::new(addresses) as reqwest::dns::Addrs)
        })
    }
}

pub(crate) fn provider_http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().dns_resolver(TypedGaiResolver)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransportErrorFacts {
    is_timeout: bool,
    is_connect: bool,
    is_dns: bool,
    is_tls: bool,
    phase: TransportPhase,
    raw_os_code: Option<i32>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TransportSourceFacts {
    is_dns: bool,
    is_tls: bool,
    raw_os_code: Option<i32>,
}

fn transport_source_facts(error: &(dyn std::error::Error + 'static)) -> TransportSourceFacts {
    let mut sources = vec![error];
    let mut facts = TransportSourceFacts::default();
    while let Some(current) = sources.pop() {
        if let Some(io_error) = current.downcast_ref::<std::io::Error>() {
            if facts.raw_os_code.is_none() {
                facts.raw_os_code = io_error.raw_os_error();
            }
            if let Some(inner) = io_error.get_ref() {
                sources.push(inner);
            }
        }
        facts.is_dns |= current.downcast_ref::<DnsResolutionError>().is_some();
        facts.is_tls |= current.downcast_ref::<rustls::Error>().is_some();
        if let Some(source) = current.source() {
            sources.push(source);
        }
    }
    facts
}

impl TransportErrorFacts {
    pub(crate) fn from_reqwest(error: &reqwest::Error, phase: TransportPhase) -> Self {
        let source_facts = transport_source_facts(error);
        Self {
            is_timeout: error.is_timeout(),
            is_connect: error.is_connect(),
            is_dns: source_facts.is_dns,
            is_tls: source_facts.is_tls,
            phase,
            raw_os_code: source_facts.raw_os_code,
        }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(
        is_timeout: bool,
        is_connect: bool,
        phase: TransportPhase,
        raw_os_code: Option<i32>,
    ) -> Self {
        Self {
            is_timeout,
            is_connect,
            is_dns: false,
            is_tls: false,
            phase,
            raw_os_code,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderCacheBinding {
    primary: AccountScope,
    corroborating: Option<AccountScope>,
}

impl ProviderCacheBinding {
    pub(crate) fn new(primary: AccountScope, corroborating: Option<AccountScope>) -> Self {
        Self {
            primary,
            corroborating,
        }
    }

    pub(crate) fn primary(primary: AccountScope) -> Self {
        Self::new(primary, None)
    }
}

pub(crate) async fn request_after_verified_binding<B, T, E, F, Future>(
    binding: Result<B, E>,
    request: F,
) -> Result<T, E>
where
    F: FnOnce(B) -> Future,
    Future: std::future::Future<Output = Result<T, E>>,
{
    request(binding?).await
}

#[derive(Debug, Clone)]
pub(crate) enum ProviderFetchFailure {
    Transient {
        display: String,
        attempt_binding: Option<ProviderCacheBinding>,
        transport_diagnostic: SafeTransportDiagnostic,
    },
    Terminal {
        display: String,
    },
}

impl ProviderFetchFailure {
    pub(crate) fn transient(
        display: impl Into<String>,
        attempt_binding: Option<ProviderCacheBinding>,
        transport_diagnostic: SafeTransportDiagnostic,
    ) -> Self {
        Self::Transient {
            display: display.into(),
            attempt_binding,
            transport_diagnostic,
        }
    }

    pub(crate) fn terminal(display: impl Into<String>) -> Self {
        Self::Terminal {
            display: display.into(),
        }
    }

    pub(crate) fn from_send_error(
        display: impl Into<String>,
        attempt_binding: Option<ProviderCacheBinding>,
        error: &reqwest::Error,
    ) -> Self {
        if error.is_builder() {
            return Self::terminal(display);
        }
        Self::transient(
            display,
            attempt_binding,
            SafeTransportDiagnostic::from_facts(TransportErrorFacts::from_reqwest(
                error,
                TransportPhase::Request,
            )),
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ProviderFetchOutcome {
    Absent,
    Success {
        snapshot: AgentUsageSnapshot,
        cache_binding: Option<ProviderCacheBinding>,
    },
    Failure(ProviderFetchFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseReadFailure {
    Transient(SafeTransportDiagnostic),
    Terminal(u16),
}

pub(crate) async fn read_response_body<F, Future>(
    status: u16,
    allow_forbidden_body: bool,
    read: F,
) -> Result<String, ResponseReadFailure>
where
    F: FnOnce() -> Future,
    Future: std::future::Future<Output = Result<String, TransportErrorFacts>>,
{
    if status == 429 {
        return Err(ResponseReadFailure::Transient(
            SafeTransportDiagnostic::rate_limited(status),
        ));
    }
    if (500..=599).contains(&status) {
        return Err(ResponseReadFailure::Transient(
            SafeTransportDiagnostic::server_error(status),
        ));
    }
    let may_read = (200..=299).contains(&status) || (allow_forbidden_body && status == 403);
    if !may_read {
        return Err(ResponseReadFailure::Terminal(status));
    }
    match read().await {
        Ok(body) => Ok(body),
        Err(_) if status == 403 => Err(ResponseReadFailure::Terminal(status)),
        Err(facts) => Err(ResponseReadFailure::Transient(
            SafeTransportDiagnostic::from_facts(facts),
        )),
    }
}

#[derive(Debug, Clone)]
struct LastGoodEntry {
    binding: ProviderCacheBinding,
    snapshot: AgentUsageSnapshot,
}

#[derive(Debug, Default)]
struct ProviderLastGoodCache {
    entries: HashMap<String, LastGoodEntry>,
}

impl ProviderLastGoodCache {
    fn clean_for(
        &self,
        client_id: &str,
        binding: &ProviderCacheBinding,
    ) -> Option<AgentUsageSnapshot> {
        self.entries
            .get(client_id)
            .filter(|entry| &entry.binding == binding)
            .map(|entry| entry.snapshot.clone())
    }

    fn replace(
        &mut self,
        client_id: &str,
        binding: ProviderCacheBinding,
        mut snapshot: AgentUsageSnapshot,
    ) {
        snapshot.error = None;
        snapshot.transport_diagnostic = None;
        self.entries
            .insert(client_id.to_string(), LastGoodEntry { binding, snapshot });
    }

    fn clear(&mut self, client_id: &str) {
        self.entries.remove(client_id);
    }
}

static PROVIDER_LAST_GOOD: LazyLock<Mutex<ProviderLastGoodCache>> =
    LazyLock::new(|| Mutex::new(ProviderLastGoodCache::default()));

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

#[derive(Debug)]
struct CodexCredentialWriteReceipt {
    path: PathBuf,
    previous_root: Value,
    persisted_root: Value,
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
    /// Full credentials JSON captured at reload. The target object is the
    /// optimistic write guard; top-level siblings are merged from the current
    /// store at save time.
    raw_root: Option<Value>,
    /// Exact Keychain account whose item was read at refresh reload. A later
    /// write-back may validate this identity, but must never retarget it.
    keychain_account: Option<String>,
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

#[derive(Debug)]
enum ClaudeLoginResolution {
    Absent,
    ExplicitLogout,
    Ready(ClaudeCredentials),
    Terminal,
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

fn finite_codex_balance(credits: Option<&CodexCredits>) -> Option<f64> {
    credits
        .and_then(|credits| credits.balance)
        .filter(|balance| balance.is_finite())
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

fn empty_error_snapshot(
    client_id: &str,
    source: &str,
    now: DateTime<Utc>,
    display: String,
    transport_diagnostic: Option<SafeTransportDiagnostic>,
) -> AgentUsageSnapshot {
    AgentUsageSnapshot {
        client_id: client_id.to_string(),
        source: source.to_string(),
        updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
        identity: None,
        account_scope: Err(AccountScopeError::NoTrustedEvidence),
        windows: Vec::new(),
        credits: None,
        error: Some(display),
        transport_diagnostic,
    }
}

fn usable_success(snapshot: &AgentUsageSnapshot) -> bool {
    match snapshot.client_id.as_str() {
        "codex" => {
            !snapshot.windows.is_empty()
                || snapshot
                    .credits
                    .as_ref()
                    .and_then(|credits| credits.remaining)
                    .is_some_and(f64::is_finite)
        }
        "grok" => snapshot
            .windows
            .iter()
            .any(|window| window.card_id == "billing.weekly.v1"),
        "claude" | "copilot" | "antigravity" => !snapshot.windows.is_empty(),
        _ => false,
    }
}

fn lock_last_good(
    cache: &Mutex<ProviderLastGoodCache>,
) -> std::sync::MutexGuard<'_, ProviderLastGoodCache> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn apply_provider_outcome_with<F>(
    cache: &Mutex<ProviderLastGoodCache>,
    client_id: &str,
    failure_source: &str,
    now: DateTime<Utc>,
    outcome: ProviderFetchOutcome,
    mut enrich: F,
) -> Option<AgentUsageSnapshot>
where
    F: FnMut(&mut AgentUsageSnapshot),
{
    match outcome {
        ProviderFetchOutcome::Absent => {
            lock_last_good(cache).clear(client_id);
            None
        }
        ProviderFetchOutcome::Success {
            mut snapshot,
            cache_binding,
        } => {
            match snapshot.account_scope.as_ref() {
                Ok(_) | Err(AccountScopeError::NoTrustedEvidence) => {}
                Err(_) => {
                    lock_last_good(cache).clear(client_id);
                    let source = snapshot.source.clone();
                    return Some(empty_error_snapshot(
                        client_id,
                        &source,
                        now,
                        format!(
                            "{} account identity could not be verified.",
                            clean_plan(client_id)
                        ),
                        None,
                    ));
                }
            }

            enrich(&mut snapshot);
            let cacheable = snapshot.account_scope.is_ok()
                && snapshot.error.is_none()
                && snapshot.transport_diagnostic.is_none()
                && usable_success(&snapshot);
            let mut cache = lock_last_good(cache);
            match (cacheable, cache_binding) {
                (true, Some(binding)) => cache.replace(client_id, binding, snapshot.clone()),
                _ => cache.clear(client_id),
            }
            Some(snapshot)
        }
        ProviderFetchOutcome::Failure(ProviderFetchFailure::Terminal { display }) => {
            lock_last_good(cache).clear(client_id);
            Some(empty_error_snapshot(
                client_id,
                failure_source,
                now,
                display,
                None,
            ))
        }
        ProviderFetchOutcome::Failure(ProviderFetchFailure::Transient {
            display,
            attempt_binding,
            transport_diagnostic,
        }) => {
            let fallback = attempt_binding
                .as_ref()
                .and_then(|binding| lock_last_good(cache).clean_for(client_id, binding));
            let Some(mut snapshot) = fallback else {
                lock_last_good(cache).clear(client_id);
                return Some(empty_error_snapshot(
                    client_id,
                    failure_source,
                    now,
                    display,
                    Some(transport_diagnostic),
                ));
            };
            snapshot.account_scope = Err(AccountScopeError::NoTrustedEvidence);
            snapshot.error = Some(display);
            snapshot.transport_diagnostic = Some(transport_diagnostic);
            Some(snapshot)
        }
    }
}

fn apply_provider_outcome(
    client_id: &str,
    failure_source: &str,
    now: DateTime<Utc>,
    outcome: ProviderFetchOutcome,
) -> Option<AgentUsageSnapshot> {
    apply_provider_outcome_with(
        &PROVIDER_LAST_GOOD,
        client_id,
        failure_source,
        now,
        outcome,
        |snapshot| enrich_snapshot(snapshot, now.timestamp()),
    )
}

pub async fn run(publication_generation: u64) -> AgentUsagePayload {
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
        publication_generation,
        agents,
        opencode_subscriptions: crate::opencode_integrations::detect_subscriptions(),
    }
}

async fn fetch_grok() -> Option<AgentUsageSnapshot> {
    let now = Utc::now();
    let outcome = match agent_grok::fetch(now).await {
        Ok(Some(data)) => ProviderFetchOutcome::Success {
            cache_binding: data.cache_binding,
            snapshot: AgentUsageSnapshot {
                client_id: "grok".to_string(),
                source: "oauth".to_string(),
                updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
                identity: data.identity,
                account_scope: data.account_scope,
                windows: data.windows,
                credits: None,
                error: None,
                transport_diagnostic: None,
            },
        },
        Ok(None) => ProviderFetchOutcome::Absent,
        Err(failure) => ProviderFetchOutcome::Failure(failure),
    };
    apply_provider_outcome("grok", "oauth", now, outcome)
}

async fn fetch_copilot() -> Option<AgentUsageSnapshot> {
    let now = Utc::now();
    let outcome = match crate::opencode_integrations::github_copilot_credential() {
        crate::opencode_integrations::GitHubCopilotCredentialLoad::Absent => {
            ProviderFetchOutcome::Absent
        }
        crate::opencode_integrations::GitHubCopilotCredentialLoad::Terminal(display) => {
            ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(display))
        }
        crate::opencode_integrations::GitHubCopilotCredentialLoad::Present(credential) => {
            match agent_copilot::fetch(now, credential).await {
                Ok(data) => ProviderFetchOutcome::Success {
                    cache_binding: Some(data.cache_binding),
                    snapshot: AgentUsageSnapshot {
                        client_id: "copilot".to_string(),
                        source: "oauth".to_string(),
                        updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
                        identity: data.identity,
                        account_scope: data.account_scope,
                        windows: data.windows,
                        credits: None,
                        error: None,
                        transport_diagnostic: None,
                    },
                },
                Err(failure) => ProviderFetchOutcome::Failure(failure),
            }
        }
    };
    apply_provider_outcome("copilot", "oauth", now, outcome)
}

async fn fetch_antigravity() -> AgentUsageSnapshot {
    let now = Utc::now();
    let outcome = match agent_antigravity::fetch(now).await {
        Ok(fetched) => ProviderFetchOutcome::Success {
            cache_binding: fetched.cache_binding,
            snapshot: AgentUsageSnapshot {
                client_id: "antigravity".to_string(),
                source: fetched.source,
                updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
                identity: fetched.identity,
                account_scope: fetched.account_scope,
                windows: fetched.windows,
                credits: None,
                error: None,
                transport_diagnostic: None,
            },
        },
        Err(failure) => ProviderFetchOutcome::Failure(failure),
    };
    apply_provider_outcome("antigravity", "oauth", now, outcome)
        .expect("Antigravity is a required provider card")
}

async fn fetch_codex() -> AgentUsageSnapshot {
    let now = Utc::now();
    apply_provider_outcome("codex", "oauth", now, fetch_codex_inner().await)
        .expect("Codex is a required provider card")
}

/// Claude's `/api/oauth/usage` rate-limits aggressively. The gate stores only
/// the cooldown deadline and the exact opaque binding that triggered it; display
/// snapshots live exclusively in the provider last-good cache.
#[derive(Debug, Default)]
struct ClaudeUsageGate {
    blocked_until: Option<DateTime<Utc>>,
    binding: Option<ProviderCacheBinding>,
}

impl ClaudeUsageGate {
    fn blocked_until_for(
        &mut self,
        binding: &ProviderCacheBinding,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        if self.binding.as_ref() != Some(binding) {
            self.blocked_until = None;
            self.binding = None;
            return None;
        }
        match self.blocked_until {
            Some(until) if until > now => Some(until),
            Some(_) => {
                self.blocked_until = None;
                self.binding = None;
                None
            }
            None => None,
        }
    }

    fn record_rate_limit(
        &mut self,
        binding: ProviderCacheBinding,
        retry_after: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) {
        self.blocked_until = Some(
            retry_after
                .filter(|until| *until > now)
                .unwrap_or_else(|| now + chrono::Duration::minutes(5)),
        );
        self.binding = Some(binding);
    }

    fn clear(&mut self) {
        self.blocked_until = None;
        self.binding = None;
    }
}

static CLAUDE_USAGE_GATE: Mutex<ClaudeUsageGate> = Mutex::new(ClaudeUsageGate {
    blocked_until: None,
    binding: None,
});

fn lock_gate() -> std::sync::MutexGuard<'static, ClaudeUsageGate> {
    CLAUDE_USAGE_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn claude_gate_failure(
    binding: ProviderCacheBinding,
    blocked_until: DateTime<Utc>,
    now: DateTime<Utc>,
) -> ProviderFetchFailure {
    let wait_secs = (blocked_until - now).num_seconds().max(0);
    ProviderFetchFailure::transient(
        format!(
            "Claude OAuth usage endpoint is rate limited. Retrying automatically in ~{wait_secs}s."
        ),
        Some(binding),
        SafeTransportDiagnostic::rate_limited(429),
    )
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
    let (failure_source, outcome) = fetch_claude_inner().await;
    apply_provider_outcome("claude", failure_source, now, outcome)
        .expect("Claude is a required provider card")
}

async fn fetch_codex_inner() -> ProviderFetchOutcome {
    let loaded = match load_codex_credentials() {
        Ok(credentials) => credentials,
        Err(display) => {
            return ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(display));
        }
    };
    let verified = if credentials_needs_refresh(loaded.last_refresh) {
        refresh_codex_credentials(&loaded.auth_path).await
    } else {
        resolve_codex_cache_binding(&loaded)
            .map(|binding| (loaded, binding))
            .map_err(|_| {
                ProviderFetchFailure::terminal("Codex account identity could not be verified.")
            })
    };
    let (credentials, cache_binding, response) =
        match request_after_verified_binding(verified, |(credentials, cache_binding)| async move {
            let client = provider_http_client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|_| {
                    ProviderFetchFailure::terminal("Codex usage client could not be created.")
                })?;
            let request_account_id = credentials
                .account_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let mut request = client
                .get(CODEX_USAGE_URL)
                .bearer_auth(&credentials.access_token)
                .header(reqwest::header::ACCEPT, "application/json")
                .header(reqwest::header::USER_AGENT, "TokenBar");
            if let Some(account_id) = request_account_id {
                request = request.header("ChatGPT-Account-Id", account_id);
            }
            let response = request.send().await.map_err(|error| {
                ProviderFetchFailure::from_send_error(
                    "Codex usage request failed. Retrying automatically.",
                    Some(cache_binding.clone()),
                    &error,
                )
            })?;
            Ok((credentials, cache_binding, response))
        })
        .await
        {
            Ok(verified) => verified,
            Err(failure) => return ProviderFetchOutcome::Failure(failure),
        };
    let request_account_id = credentials
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let status = response.status().as_u16();
    let body = match read_response_body(status, false, || async {
        response.text().await.map_err(|error| {
            TransportErrorFacts::from_reqwest(&error, TransportPhase::ResponseBody)
        })
    })
    .await
    {
        Ok(body) => body,
        Err(ResponseReadFailure::Transient(diagnostic)) => {
            return ProviderFetchOutcome::Failure(ProviderFetchFailure::transient(
                "Codex usage request failed. Retrying automatically.",
                Some(cache_binding),
                diagnostic,
            ));
        }
        Err(ResponseReadFailure::Terminal(401 | 403)) => {
            return ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                "Codex OAuth token expired or invalid. Run `codex` to log in again.",
            ));
        }
        Err(ResponseReadFailure::Terminal(status)) => {
            return ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(format!(
                "Codex usage API rejected the request (status {status})."
            )));
        }
    };

    let mut usage: CodexUsageResponse = match serde_json::from_str(&body) {
        Ok(usage) => usage,
        Err(_) => {
            return ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                "Codex usage response could not be decoded.",
            ));
        }
    };
    let now = Utc::now();
    let account_scope = cache_binding
        .corroborating
        .clone()
        .unwrap_or_else(|| cache_binding.primary.clone());
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
    let finite_balance = finite_codex_balance(usage.credits.as_ref());
    if let Some(credits) = usage.credits.as_mut() {
        credits.balance = finite_balance;
    }
    if windows.is_empty() && finite_balance.is_none() {
        return ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
            "Codex usage API returned no usable quota data.",
        ));
    }

    if let Some(request_account_id) = request_account_id {
        let _ = crate::agent_quota_history::migrate_codex_v2(
            request_account_id,
            account_scope.as_str(),
            now.timestamp(),
        );
    }

    ProviderFetchOutcome::Success {
        snapshot: AgentUsageSnapshot {
            client_id: "codex".to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity,
            account_scope: Ok(account_scope),
            windows,
            credits: usage.credits.map(|credits| CreditsSnapshot {
                remaining: credits.balance,
                unlimited: credits.unlimited,
            }),
            error: None,
            transport_diagnostic: None,
        },
        cache_binding: Some(cache_binding),
    }
}

fn resolve_codex_cache_binding(
    credentials: &CodexCredentials,
) -> Result<ProviderCacheBinding, AccountScopeError> {
    let primary = agent_account_scope::resolve_credential(
        "codex",
        credentials.scope_slot.semantic_source,
        &credentials.scope_slot.canonical_location,
        credentials.scope_marker(),
    )?;
    let corroborating = credentials
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|account_id| {
            agent_account_scope::resolve_authoritative(
                "codex",
                AuthoritativeIdKind::OpaqueId,
                account_id,
            )
        })
        .transpose()?;
    Ok(ProviderCacheBinding::new(primary, corroborating))
}

fn claude_cache_binding(
    credentials: &ClaudeCredentials,
) -> Result<ProviderCacheBinding, AccountScopeError> {
    credentials
        .resolve_account_scope()
        .map(ProviderCacheBinding::primary)
}

fn clear_claude_gate_for_login_resolution(
    login: &ClaudeLoginResolution,
    gate: &mut ClaudeUsageGate,
) {
    if matches!(
        login,
        ClaudeLoginResolution::Absent | ClaudeLoginResolution::ExplicitLogout
    ) {
        gate.clear();
    }
}

async fn fetch_claude_inner() -> (&'static str, ProviderFetchOutcome) {
    if let Some(token) = resolve_claude_code_oauth_token().await {
        return fetch_claude_setup_token(token).await;
    }

    let login = load_claude_login_credentials();
    clear_claude_gate_for_login_resolution(&login, &mut lock_gate());
    fetch_claude_login_or_setup_with(
        login,
        |credentials| async move {
            let verified = claude_cache_binding(&credentials).map_err(|_| {
                ProviderFetchFailure::terminal("Claude account identity could not be verified.")
            });
            request_after_verified_binding(verified, |binding| async move {
                Ok(fetch_claude_oauth_usage(credentials, binding).await)
            })
            .await
            .unwrap_or_else(|failure| ("oauth", ProviderFetchOutcome::Failure(failure)))
        },
        resolve_claude_keychain_token,
        fetch_claude_setup_token,
    )
    .await
}

async fn fetch_claude_setup_token(
    token: ResolvedClaudeToken,
) -> (&'static str, ProviderFetchOutcome) {
    let credentials = claude_credentials_from_access_token(token);
    let verified = claude_cache_binding(&credentials).map_err(|_| {
        ProviderFetchFailure::terminal("Claude setup-token account identity could not be verified.")
    });
    let outcome = request_after_verified_binding(verified, |binding| async move {
        Ok(claude_header_snapshot(
            &credentials,
            Utc::now(),
            Ok(binding.primary.clone()),
            Some(binding),
        )
        .await)
    })
    .await
    .unwrap_or_else(ProviderFetchOutcome::Failure);
    ("setup-token", outcome)
}

async fn fetch_claude_login_or_setup_with<Login, LoginFuture, LoadSetup, Setup, SetupFuture>(
    login: ClaudeLoginResolution,
    request_login: Login,
    load_setup: LoadSetup,
    request_setup: Setup,
) -> (&'static str, ProviderFetchOutcome)
where
    Login: FnOnce(ClaudeCredentials) -> LoginFuture,
    LoginFuture: std::future::Future<Output = (&'static str, ProviderFetchOutcome)>,
    LoadSetup: FnOnce() -> Result<Option<ResolvedClaudeToken>, String>,
    Setup: FnOnce(ResolvedClaudeToken) -> SetupFuture,
    SetupFuture: std::future::Future<Output = (&'static str, ProviderFetchOutcome)>,
{
    match login {
        ClaudeLoginResolution::Ready(credentials) => request_login(credentials).await,
        ClaudeLoginResolution::Terminal => (
            "oauth",
            ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                CLAUDE_CREDENTIALS_LOAD_ERROR,
            )),
        ),
        ClaudeLoginResolution::Absent | ClaudeLoginResolution::ExplicitLogout => {
            match load_setup() {
                Ok(Some(token)) => request_setup(token).await,
                Ok(None) => (
                    "unconfigured",
                    ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                        CLAUDE_UNCONFIGURED_ERROR,
                    )),
                ),
                Err(_) => (
                    "setup-token",
                    ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                        CLAUDE_CREDENTIALS_LOAD_ERROR,
                    )),
                ),
            }
        }
    }
}

async fn fetch_claude_oauth_usage(
    credentials: ClaudeCredentials,
    pre_binding: ProviderCacheBinding,
) -> (&'static str, ProviderFetchOutcome) {
    fetch_claude_login_usage_with(
        credentials,
        pre_binding,
        Utc::now(),
        |binding, now| {
            let mut gate = lock_gate();
            gate.blocked_until_for(binding, now)
        },
        |credentials| async move { refresh_claude_credentials(&credentials).await },
        |credentials, account_scope, cache_binding| async move {
            claude_header_snapshot(&credentials, Utc::now(), Ok(account_scope), cache_binding).await
        },
        |credentials, account_scope, cache_binding, gate_binding| async move {
            fetch_claude_oauth_usage_request(
                &credentials,
                account_scope,
                cache_binding,
                gate_binding,
            )
            .await
        },
    )
    .await
}

async fn fetch_claude_login_usage_with<
    Gate,
    Refresh,
    RefreshFuture,
    Header,
    HeaderFuture,
    Oauth,
    OauthFuture,
>(
    credentials: ClaudeCredentials,
    pre_binding: ProviderCacheBinding,
    now: DateTime<Utc>,
    blocked_until_for: Gate,
    refresh: Refresh,
    header: Header,
    oauth: Oauth,
) -> (&'static str, ProviderFetchOutcome)
where
    Gate: FnOnce(&ProviderCacheBinding, DateTime<Utc>) -> Option<DateTime<Utc>>,
    Refresh: FnOnce(ClaudeCredentials) -> RefreshFuture,
    RefreshFuture: std::future::Future<
        Output = Result<
            (
                ClaudeCredentials,
                AccountScope,
                Option<ProviderCacheBinding>,
            ),
            ProviderFetchFailure,
        >,
    >,
    Header: FnOnce(ClaudeCredentials, AccountScope, Option<ProviderCacheBinding>) -> HeaderFuture,
    HeaderFuture: std::future::Future<Output = ProviderFetchOutcome>,
    Oauth: FnOnce(
        ClaudeCredentials,
        AccountScope,
        Option<ProviderCacheBinding>,
        ProviderCacheBinding,
    ) -> OauthFuture,
    OauthFuture: std::future::Future<Output = (&'static str, ProviderFetchOutcome)>,
{
    let header_route = !credentials.scopes.is_empty()
        && !credentials
            .scopes
            .iter()
            .any(|scope| scope == "user:profile");
    if !header_route {
        if let Some(blocked_until) = blocked_until_for(&pre_binding, now) {
            return (
                "oauth",
                ProviderFetchOutcome::Failure(claude_gate_failure(pre_binding, blocked_until, now)),
            );
        }
    }

    let (credentials, account_scope, cache_binding) = if claude_credentials_expired(&credentials) {
        match refresh(credentials).await {
            Ok(refreshed) => refreshed,
            Err(failure) => return ("oauth", ProviderFetchOutcome::Failure(failure)),
        }
    } else {
        let account_scope = pre_binding.primary.clone();
        (credentials, account_scope, Some(pre_binding))
    };

    if header_route {
        return (
            "setup-token",
            header(credentials, account_scope, cache_binding).await,
        );
    }

    let gate_binding = ProviderCacheBinding::primary(account_scope.clone());
    oauth(credentials, account_scope, cache_binding, gate_binding).await
}

async fn fetch_claude_oauth_usage_request(
    credentials: &ClaudeCredentials,
    account_scope: AccountScope,
    cache_binding: Option<ProviderCacheBinding>,
    gate_binding: ProviderCacheBinding,
) -> (&'static str, ProviderFetchOutcome) {
    let client = match provider_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return (
                "oauth",
                ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                    "Claude usage client could not be created.",
                )),
            );
        }
    };

    let response = match client
        .get(CLAUDE_USAGE_URL)
        .bearer_auth(&credentials.access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, claude_user_agent())
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return (
                "oauth",
                ProviderFetchOutcome::Failure(ProviderFetchFailure::from_send_error(
                    "Claude usage request failed. Retrying automatically.",
                    cache_binding,
                    &error,
                )),
            );
        }
    };
    let status = response.status().as_u16();
    let retry_after = (status == 429)
        .then(|| parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER)))
        .flatten();
    let body = match read_response_body(status, true, || async {
        response.text().await.map_err(|error| {
            TransportErrorFacts::from_reqwest(&error, TransportPhase::ResponseBody)
        })
    })
    .await
    {
        Ok(body) => body,
        Err(ResponseReadFailure::Transient(diagnostic)) => {
            if status == 429 {
                lock_gate().record_rate_limit(gate_binding, retry_after, Utc::now());
            }
            return (
                "oauth",
                ProviderFetchOutcome::Failure(ProviderFetchFailure::transient(
                    "Claude usage request failed. Retrying automatically.",
                    cache_binding,
                    diagnostic,
                )),
            );
        }
        Err(ResponseReadFailure::Terminal(401)) => {
            return (
                "oauth",
                ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                    "Claude OAuth token expired or invalid. Run `claude` to re-authenticate.",
                )),
            );
        }
        Err(ResponseReadFailure::Terminal(403)) => {
            return (
                "oauth",
                ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                    "Claude OAuth usage was denied. Run `claude logout && claude login` to grant user:profile.",
                )),
            );
        }
        Err(ResponseReadFailure::Terminal(status)) => {
            return (
                "oauth",
                ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(format!(
                    "Claude usage API rejected the request (status {status})."
                ))),
            );
        }
    };

    if status == 403 {
        if body.contains("user:profile") {
            return (
                "setup-token",
                claude_header_snapshot(credentials, Utc::now(), Ok(account_scope), cache_binding)
                    .await,
            );
        }
        return (
            "oauth",
            ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                "Claude OAuth usage was denied. Run `claude logout && claude login` to grant user:profile.",
            )),
        );
    }

    let usage: ClaudeUsageResponse = match serde_json::from_str(&body) {
        Ok(usage) => usage,
        Err(_) => {
            return (
                "oauth",
                ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                    "Claude usage response could not be decoded.",
                )),
            );
        }
    };
    let now = Utc::now();
    let windows = claude_windows(&usage, now);
    if windows.is_empty() {
        return (
            "oauth",
            ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                "Claude usage API returned no usable quota windows.",
            )),
        );
    }
    lock_gate().clear();

    (
        "oauth",
        ProviderFetchOutcome::Success {
            snapshot: AgentUsageSnapshot {
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
                account_scope: Ok(account_scope),
                windows,
                credits: claude_credits(usage.extra_usage.as_ref()),
                error: None,
                transport_diagnostic: None,
            },
            cache_binding,
        },
    )
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

async fn fetch_claude_via_headers(
    access_token: &str,
    attempt_binding: Option<ProviderCacheBinding>,
) -> Result<Vec<UsageWindow>, ProviderFetchFailure> {
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

    let client = provider_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| {
            ProviderFetchFailure::terminal("Claude header-probe client could not be created.")
        })?;

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
        .map_err(|error| {
            ProviderFetchFailure::from_send_error(
                "Claude header probe failed. Retrying automatically.",
                attempt_binding.clone(),
                &error,
            )
        })?;

    let status = response.status().as_u16();
    let windows = parse_unified_ratelimit_windows(response.headers(), Utc::now());
    if (200..=299).contains(&status) || status == 429 {
        if windows.is_empty() {
            if status == 429 {
                return Err(ProviderFetchFailure::transient(
                    "Claude header probe is rate limited. Retrying automatically.",
                    attempt_binding,
                    SafeTransportDiagnostic::rate_limited(status),
                ));
            }
            return Err(ProviderFetchFailure::terminal(
                "Claude header probe returned no usable rate-limit headers.",
            ));
        }
        let mut guard = CLAUDE_HEADER_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some((Utc::now(), access_token.to_string(), windows.clone()));
        return Ok(windows);
    }
    if (500..=599).contains(&status) {
        return Err(ProviderFetchFailure::transient(
            "Claude header probe failed. Retrying automatically.",
            attempt_binding,
            SafeTransportDiagnostic::server_error(status),
        ));
    }
    Err(ProviderFetchFailure::terminal(
        if matches!(status, 401 | 403) {
            "Claude setup-token expired or lacks access.".to_string()
        } else {
            format!("Claude header probe rejected the request (status {status}).")
        },
    ))
}

async fn claude_header_snapshot(
    credentials: &ClaudeCredentials,
    now: DateTime<Utc>,
    account_scope: Result<AccountScope, AccountScopeError>,
    cache_binding: Option<ProviderCacheBinding>,
) -> ProviderFetchOutcome {
    let windows =
        match fetch_claude_via_headers(&credentials.access_token, cache_binding.clone()).await {
            Ok(windows) => windows,
            Err(failure) => return ProviderFetchOutcome::Failure(failure),
        };
    ProviderFetchOutcome::Success {
        snapshot: AgentUsageSnapshot {
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
            transport_diagnostic: None,
        },
        cache_binding,
    }
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
const CLAUDE_CREDENTIALS_LOAD_ERROR: &str = "Claude credentials could not be loaded.";

/// Full-login credentials: structured `claudeAiOauth` blobs (Keychain
/// `Claude Code-credentials`, then `~/.claude/.credentials.json`) plus the
/// TokenBar env override. Only a genuinely missing higher-priority store falls
/// through; the explicit #26 logout shape stops full-login precedence.
fn load_claude_login_credentials() -> ClaudeLoginResolution {
    match load_claude_credentials_from_environment() {
        Ok(Some(credentials)) => return ClaudeLoginResolution::Ready(credentials),
        Ok(None) => {}
        Err(_) => return ClaudeLoginResolution::Terminal,
    }
    load_stored_claude_login_with(
        load_claude_credentials_from_keychain,
        || match fs::read_to_string(claude_credentials_path()) {
            Ok(raw) => Ok(Some(raw)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("Claude credentials file could not be read.".to_string()),
        },
    )
}

fn load_stored_claude_login_with<LoadKeychain, LoadFile>(
    load_keychain: LoadKeychain,
    load_file: LoadFile,
) -> ClaudeLoginResolution
where
    LoadKeychain: FnOnce() -> Result<Option<String>, String>,
    LoadFile: FnOnce() -> Result<Option<String>, String>,
{
    match load_keychain() {
        Ok(Some(raw)) => {
            return resolve_stored_claude_login(&raw, ClaudeCredentialSource::Keychain);
        }
        Ok(None) => {}
        Err(_) => return ClaudeLoginResolution::Terminal,
    }
    match load_file() {
        Ok(Some(raw)) => resolve_stored_claude_login(&raw, ClaudeCredentialSource::File),
        Ok(None) => ClaudeLoginResolution::Absent,
        Err(_) => ClaudeLoginResolution::Terminal,
    }
}

fn resolve_stored_claude_login(raw: &str, source: ClaudeCredentialSource) -> ClaudeLoginResolution {
    let raw_root: Value = match serde_json::from_str(raw) {
        Ok(root) => root,
        Err(_) => return ClaudeLoginResolution::Terminal,
    };
    let explicitly_logged_out = raw_root
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .is_some_and(|oauth| {
            oauth.contains_key("refreshToken")
                && match oauth.get("accessToken") {
                    None | Some(Value::Null) => true,
                    Some(Value::String(token)) => token.trim().is_empty(),
                    _ => false,
                }
        });
    if explicitly_logged_out {
        return ClaudeLoginResolution::ExplicitLogout;
    }
    match parse_claude_credentials_data(raw, source) {
        Ok(credentials) => ClaudeLoginResolution::Ready(credentials),
        Err(_) => ClaudeLoginResolution::Terminal,
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
fn resolve_claude_keychain_token() -> Result<Option<ResolvedClaudeToken>, String> {
    load_claude_raw_token_from_keychain().map(|token| {
        token.map(|access_token| ResolvedClaudeToken {
            access_token,
            scope_slot: CredentialSlot {
                semantic_source: "claude-setup-keychain",
                canonical_location: CLAUDE_RAW_TOKEN_KEYCHAIN_SERVICE.to_string(),
            },
        })
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
        keychain_account: None,
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
        keychain_account: None,
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
fn keychain_item_not_found(status: &std::process::ExitStatus) -> bool {
    status.code() == Some(44)
}

fn load_claude_credentials_from_keychain() -> Result<Option<String>, String> {
    load_claude_credentials_from_keychain_item(None)
}

#[cfg(target_os = "macos")]
fn load_claude_credentials_from_keychain_item(
    account: Option<&str>,
) -> Result<Option<String>, String> {
    let mut command = std::process::Command::new("/usr/bin/security");
    command.args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE]);
    if let Some(account) = account {
        command.args(["-a", account]);
    }
    let output = command
        .arg("-w")
        .output()
        .map_err(|e| format!("read Claude Keychain credentials: {}", e))?;
    if !output.status.success() {
        return if keychain_item_not_found(&output.status) {
            Ok(None)
        } else {
            Err("Claude Keychain credentials could not be read.".to_string())
        };
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|_| "Claude Keychain credentials are not UTF-8 JSON.".to_string())?;
    let raw = raw.trim_matches(['\r', '\n']).to_string();
    if raw.trim().is_empty() {
        return Err("Claude Keychain credentials are empty.".to_string());
    }
    Ok(Some(raw))
}

#[cfg(not(target_os = "macos"))]
fn load_claude_credentials_from_keychain_item(
    _account: Option<&str>,
) -> Result<Option<String>, String> {
    Ok(None)
}

/// Build credentials from a bare access token (no refresh/expiry/scope metadata).
/// Setup-token delivery paths use these credentials only for the header probe.
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
        keychain_account: None,
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
        return if keychain_item_not_found(&output.status) {
            Ok(None)
        } else {
            Err("TokenBar Claude Keychain token could not be read.".to_string())
        };
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|_| "TokenBar Claude Keychain token is not UTF-8.".to_string())?;
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        return Err("TokenBar Claude Keychain token is empty.".to_string());
    }
    Ok(Some(raw))
}

#[cfg(not(target_os = "macos"))]
fn load_claude_raw_token_from_keychain() -> Result<Option<String>, String> {
    Ok(None)
}

async fn refresh_codex_credentials(
    auth_path: &Path,
) -> Result<(CodexCredentials, ProviderCacheBinding), ProviderFetchFailure> {
    let refresh = agent_account_scope::begin_refresh("codex").map_err(|_| {
        ProviderFetchFailure::terminal("Codex credential refresh lock is unavailable.")
    })?;
    refresh_codex_credentials_with(
        auth_path,
        &refresh,
        request_codex_refresh,
        save_codex_credentials,
        |_| Ok(()),
    )
    .await
}

async fn request_codex_refresh(
    refresh_token: String,
    attempt_binding: ProviderCacheBinding,
) -> Result<Value, ProviderFetchFailure> {
    let client = provider_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| {
            ProviderFetchFailure::terminal("Codex refresh client could not be created.")
        })?;
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
        .map_err(|error| {
            ProviderFetchFailure::from_send_error(
                "Codex token refresh failed. Retrying automatically.",
                Some(attempt_binding.clone()),
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
    .map_err(|failure| match failure {
        ResponseReadFailure::Transient(diagnostic) => ProviderFetchFailure::transient(
            "Codex token refresh failed. Retrying automatically.",
            Some(attempt_binding),
            diagnostic,
        ),
        ResponseReadFailure::Terminal(_) => ProviderFetchFailure::terminal(
            "Codex OAuth refresh failed. Run `codex` to log in again.",
        ),
    })?;
    serde_json::from_str(&body).map_err(|_| {
        ProviderFetchFailure::terminal("Codex OAuth refresh response could not be decoded.")
    })
}

fn resolve_codex_cache_binding_with<R: RefreshScopeTransaction + ?Sized>(
    credentials: &CodexCredentials,
    refresh: &R,
) -> Result<ProviderCacheBinding, AccountScopeError> {
    let primary = refresh.resolve_current(
        credentials.scope_slot.semantic_source,
        &credentials.scope_slot.canonical_location,
        credentials.scope_marker(),
    )?;
    let corroborating = credentials
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|account_id| {
            agent_account_scope::resolve_authoritative(
                "codex",
                AuthoritativeIdKind::OpaqueId,
                account_id,
            )
        })
        .transpose()?;
    Ok(ProviderCacheBinding::new(primary, corroborating))
}

async fn refresh_codex_credentials_with<R, Request, RequestFuture, Save, Checkpoint>(
    auth_path: &Path,
    refresh: &R,
    request: Request,
    save: Save,
    mut checkpoint: Checkpoint,
) -> Result<(CodexCredentials, ProviderCacheBinding), ProviderFetchFailure>
where
    R: RefreshScopeTransaction + ?Sized,
    Request: FnOnce(String, ProviderCacheBinding) -> RequestFuture,
    RequestFuture: std::future::Future<Output = Result<Value, ProviderFetchFailure>>,
    Save: FnOnce(&CodexCredentials) -> Result<CodexCredentialWriteReceipt, String>,
    Checkpoint: FnMut(RefreshCheckpoint) -> Result<(), ProviderFetchFailure>,
{
    let credentials =
        load_codex_credentials_from(auth_path).map_err(ProviderFetchFailure::terminal)?;
    checkpoint(RefreshCheckpoint::Reloaded)?;
    let pre_binding = resolve_codex_cache_binding_with(&credentials, refresh).map_err(|_| {
        ProviderFetchFailure::terminal("Codex account identity could not be verified.")
    })?;
    if !credentials_needs_refresh(credentials.last_refresh) {
        return Ok((credentials, pre_binding));
    }

    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| ProviderFetchFailure::terminal("Codex auth.json has no refresh token."))?
        .to_string();
    let old_marker = credentials.scope_marker().to_vec();
    let json = request(refresh_token, pre_binding.clone()).await?;
    checkpoint(RefreshCheckpoint::NetworkReturned)?;

    let response = json.as_object().ok_or_else(|| {
        ProviderFetchFailure::terminal("Codex OAuth refresh response was not a JSON object.")
    })?;
    let access_token = string_key(response, "access_token", "accessToken").ok_or_else(|| {
        ProviderFetchFailure::terminal(
            "Codex OAuth refresh response contained no usable access token.",
        )
    })?;
    let refreshed = CodexCredentials {
        access_token,
        refresh_token: string_key(response, "refresh_token", "refreshToken")
            .or(credentials.refresh_token),
        id_token: string_key(response, "id_token", "idToken").or(credentials.id_token),
        account_id: credentials.account_id,
        last_refresh: Some(Utc::now()),
        auth_path: credentials.auth_path,
        raw_json: credentials.raw_json,
        scope_slot: credentials.scope_slot,
    };
    let write_receipt = save(&refreshed).map_err(|_| {
        ProviderFetchFailure::terminal("Codex refreshed credentials could not be saved.")
    })?;
    checkpoint(RefreshCheckpoint::CredentialsPersisted)?;
    let post_primary = match refresh.transfer(
        refreshed.scope_slot.semantic_source,
        &refreshed.scope_slot.canonical_location,
        &old_marker,
        refreshed.scope_marker(),
    ) {
        Ok(account_scope) => account_scope,
        Err(_) => {
            let _ = rollback_codex_credentials_if_unchanged(&write_receipt);
            return Err(ProviderFetchFailure::terminal(
                "Codex credential lineage could not be preserved.",
            ));
        }
    };
    checkpoint(RefreshCheckpoint::MetadataHandled)?;
    // The account ID is unchanged by refresh and was already verified in the
    // pre-request binding. Reuse it instead of performing a second fallible
    // metadata resolution after credentials and lineage are durable.
    let post_binding = ProviderCacheBinding::new(post_primary, pre_binding.corroborating);
    Ok((refreshed, post_binding))
}

async fn refresh_claude_credentials(
    original: &ClaudeCredentials,
) -> Result<
    (
        ClaudeCredentials,
        AccountScope,
        Option<ProviderCacheBinding>,
    ),
    ProviderFetchFailure,
> {
    let refresh = agent_account_scope::begin_refresh("claude").map_err(|_| {
        ProviderFetchFailure::terminal("Claude credential refresh lock is unavailable.")
    })?;
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

async fn request_claude_refresh(
    refresh_token: String,
    attempt_binding: ProviderCacheBinding,
) -> Result<ClaudeRefreshResponse, ProviderFetchFailure> {
    let client = provider_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| {
            ProviderFetchFailure::terminal("Claude refresh client could not be created.")
        })?;
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
        .map_err(|error| {
            ProviderFetchFailure::from_send_error(
                "Claude OAuth refresh failed. Retrying automatically.",
                Some(attempt_binding.clone()),
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
    .map_err(|failure| match failure {
        ResponseReadFailure::Transient(diagnostic) => ProviderFetchFailure::transient(
            "Claude OAuth refresh failed. Retrying automatically.",
            Some(attempt_binding),
            diagnostic,
        ),
        ResponseReadFailure::Terminal(_) => ProviderFetchFailure::terminal(
            "Claude OAuth refresh failed. Run `claude` to re-authenticate.",
        ),
    })?;
    serde_json::from_str(&body).map_err(|_| {
        ProviderFetchFailure::terminal("Claude OAuth refresh response could not be decoded.")
    })
}

async fn refresh_claude_credentials_with<R, Reload, Request, RequestFuture, Save, Checkpoint>(
    original: &ClaudeCredentials,
    refresh: &R,
    reload: Reload,
    request: Request,
    save: Save,
    mut checkpoint: Checkpoint,
) -> Result<
    (
        ClaudeCredentials,
        AccountScope,
        Option<ProviderCacheBinding>,
    ),
    ProviderFetchFailure,
>
where
    R: RefreshScopeTransaction + ?Sized,
    Reload: FnOnce(&ClaudeCredentials) -> Result<ClaudeCredentials, String>,
    Request: FnOnce(String, ProviderCacheBinding) -> RequestFuture,
    RequestFuture:
        std::future::Future<Output = Result<ClaudeRefreshResponse, ProviderFetchFailure>>,
    Save: FnOnce(&ClaudeCredentials) -> Result<(), String>,
    Checkpoint: FnMut(RefreshCheckpoint) -> Result<(), ProviderFetchFailure>,
{
    let credentials = reload(original).map_err(ProviderFetchFailure::terminal)?;
    checkpoint(RefreshCheckpoint::Reloaded)?;
    let marker = credentials.scope_marker().ok_or_else(|| {
        ProviderFetchFailure::terminal("Claude credential has no trusted account marker.")
    })?;
    let pre_scope = refresh
        .resolve_current(
            credentials.scope_slot.semantic_source,
            &credentials.scope_slot.canonical_location,
            marker,
        )
        .map_err(|_| {
            ProviderFetchFailure::terminal("Claude account identity could not be verified.")
        })?;
    let pre_binding = ProviderCacheBinding::primary(pre_scope.clone());
    if !claude_credentials_expired(&credentials) {
        return Ok((credentials, pre_scope, Some(pre_binding)));
    }

    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            ProviderFetchFailure::terminal(
                "Claude OAuth token is expired and has no refresh token. Run `claude`.",
            )
        })?
        .to_string();
    let old_marker = refresh_token.as_bytes().to_vec();
    let token_response = request(refresh_token, pre_binding).await?;
    checkpoint(RefreshCheckpoint::NetworkReturned)?;
    let access_token = token_response.access_token.trim();
    if access_token.is_empty() {
        return Err(ProviderFetchFailure::terminal(
            "Claude OAuth refresh response has no access token.",
        ));
    }
    let refreshed = ClaudeCredentials {
        access_token: access_token.to_string(),
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
        keychain_account: credentials.keychain_account.clone(),
        scope_slot: credentials.scope_slot.clone(),
    };
    let new_marker = refreshed.scope_marker().ok_or_else(|| {
        ProviderFetchFailure::terminal("Claude refreshed credential has no trusted marker.")
    })?;
    let scope = refresh
        .transfer(
            refreshed.scope_slot.semantic_source,
            &refreshed.scope_slot.canonical_location,
            &old_marker,
            new_marker,
        )
        .map_err(|_| {
            ProviderFetchFailure::terminal("Claude credential lineage could not be preserved.")
        })?;
    checkpoint(RefreshCheckpoint::MetadataHandled)?;
    let persisted = save(&refreshed).is_ok();
    checkpoint(RefreshCheckpoint::CredentialsPersisted)?;
    let cache_binding = if persisted {
        Some(ProviderCacheBinding::primary(
            refresh
                .resolve_current(
                    refreshed.scope_slot.semantic_source,
                    &refreshed.scope_slot.canonical_location,
                    new_marker,
                )
                .map_err(|_| {
                    ProviderFetchFailure::terminal(
                        "Claude account identity could not be verified after refresh.",
                    )
                })?,
        ))
    } else {
        None
    };
    Ok((refreshed, scope, cache_binding))
}

fn reload_claude_credentials(original: &ClaudeCredentials) -> Result<ClaudeCredentials, String> {
    match original.source {
        ClaudeCredentialSource::Keychain => {
            let account = claude_keychain_account().ok_or_else(|| {
                "Claude Keychain account could not be captured during refresh.".to_string()
            })?;
            let raw =
                load_claude_credentials_from_keychain_item(Some(&account))?.ok_or_else(|| {
                    "Claude Keychain credentials disappeared during refresh.".to_string()
                })?;
            let mut credentials =
                parse_claude_credentials_data(&raw, ClaudeCredentialSource::Keychain)?;
            credentials.keychain_account = Some(account);
            Ok(credentials)
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
        ClaudeCredentialSource::Keychain => save_claude_credentials_to_keychain(credentials),
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
    let current_raw = fs::read_to_string(path)
        .map_err(|e| format!("read current Claude credentials file: {e}"))?;
    let data = merge_claude_credentials_json(credentials, &current_raw)?;
    atomic_write(path, &data)
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

    // Stage into the temp, fsync it, then atomically replace the target. Create with
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
    if let Err(error) = tokscale_core::fs_atomic::replace_file(&tmp, path) {
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

/// Merge rotated tokens into the current credentials JSON only when the
/// `claudeAiOauth` object still matches the one captured at refresh reload.
/// Top-level siblings come from `current_raw`, so unrelated concurrent writes
/// survive. Pure so both file and Keychain decisions are fixture-testable.
fn merge_claude_credentials_json(
    credentials: &ClaudeCredentials,
    current_raw: &str,
) -> Result<String, String> {
    let expected_oauth = credentials
        .raw_root
        .as_ref()
        .and_then(|root| root.get("claudeAiOauth"))
        .and_then(Value::as_object)
        .ok_or_else(|| "Reloaded Claude credentials have no claudeAiOauth object.".to_string())?;
    let mut current_root: Value = serde_json::from_str(current_raw)
        .map_err(|e| format!("decode current Claude credentials: {e}"))?;
    let current_oauth = current_root
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .ok_or_else(|| "Current Claude credentials have no claudeAiOauth object.".to_string())?;
    if current_oauth != expected_oauth {
        return Err(
            "Claude credentials changed during refresh; refusing stale write-back.".to_string(),
        );
    }

    let oauth = current_root
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "Current Claude credentials have no claudeAiOauth object.".to_string())?;
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
    serde_json::to_string(&current_root).map_err(|e| format!("encode Claude credentials: {}", e))
}

fn prepare_claude_keychain_write<'a>(
    credentials: &'a ClaudeCredentials,
    current_account: Option<&str>,
    current_raw: &str,
) -> Result<(&'a str, String), String> {
    let captured_account = credentials.keychain_account.as_deref().ok_or_else(|| {
        "Claude Keychain refresh has no captured account; refusing write-back.".to_string()
    })?;
    if current_account != Some(captured_account) {
        return Err(
            "Claude Keychain account changed during refresh; refusing write-back.".to_string(),
        );
    }
    let data = merge_claude_credentials_json(credentials, current_raw)?;
    Ok((captured_account, data))
}

#[cfg(target_os = "macos")]
fn save_claude_credentials_to_keychain(credentials: &ClaudeCredentials) -> Result<(), String> {
    let captured_account = credentials.keychain_account.as_deref().ok_or_else(|| {
        "Claude Keychain refresh has no captured account; refusing write-back.".to_string()
    })?;
    let current_raw = load_claude_credentials_from_keychain_item(Some(captured_account))?
        .ok_or_else(|| "Claude Keychain credentials disappeared during refresh.".to_string())?;
    let current_account = claude_keychain_account();
    let (account, data) =
        prepare_claude_keychain_write(credentials, current_account.as_deref(), &current_raw)?;

    // NOTE: security(1) has no compare-and-swap operation. The exact-item read,
    // account guard, and target comparison close the network-wait race and the
    // write always stays pinned to the captured account. A same-item mutation
    // between this check and `-U` remains the existing CLI platform limitation.
    // `-w <data>` also puts the JSON on argv briefly; the item is already
    // same-user-readable while the Keychain is unlocked. Move to SecItem only if
    // either platform assumption changes.
    let status = std::process::Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            CLAUDE_KEYCHAIN_SERVICE,
            "-a",
            account,
            "-w",
            &data,
        ])
        .status()
        .map_err(|e| format!("write Claude Keychain credentials: {}", e))?;
    if !status.success() {
        return Err("security add-generic-password failed for Claude credentials.".to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn save_claude_credentials_to_keychain(_credentials: &ClaudeCredentials) -> Result<(), String> {
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

#[cfg(not(target_os = "macos"))]
fn claude_keychain_account() -> Option<String> {
    None
}

fn save_codex_credentials(
    credentials: &CodexCredentials,
) -> Result<CodexCredentialWriteReceipt, String> {
    let expected_tokens = credentials
        .raw_json
        .get("tokens")
        .ok_or_else(|| "Codex tokens missing from the loaded credentials.".to_string())?;
    let mut raw = load_codex_credentials_from(&credentials.auth_path)
        .map_err(|e| format!("reload Codex auth.json before saving: {}", e))?
        .raw_json;
    let current_tokens = raw
        .get("tokens")
        .ok_or_else(|| "Codex tokens disappeared before saving.".to_string())?;
    if current_tokens != expected_tokens {
        return Err("Codex tokens changed during refresh.".to_string());
    }
    let previous_root = raw.clone();
    let tokens = raw
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "Codex tokens are not an object while saving.".to_string())?;

    tokens.insert(
        "access_token".to_string(),
        Value::String(credentials.access_token.clone()),
    );
    if let Some(refresh_token) = &credentials.refresh_token {
        tokens.insert(
            "refresh_token".to_string(),
            Value::String(refresh_token.clone()),
        );
    }
    if let Some(id_token) = &credentials.id_token {
        tokens.insert("id_token".to_string(), Value::String(id_token.clone()));
    }
    if let Some(account_id) = &credentials.account_id {
        tokens.insert("account_id".to_string(), Value::String(account_id.clone()));
    }
    raw["last_refresh"] = Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true));
    let data =
        serde_json::to_string_pretty(&raw).map_err(|e| format!("encode Codex auth.json: {}", e))?;
    atomic_write(&credentials.auth_path, &data)
        .map_err(|e| format!("save Codex auth.json: {}", e))?;
    Ok(CodexCredentialWriteReceipt {
        path: credentials.auth_path.clone(),
        previous_root,
        persisted_root: raw,
    })
}

/// Restore the pre-refresh Codex root only while this refresh still owns the
/// exact root it persisted. External Codex writers do not share TokenBar's
/// refresh lock, so the compare-to-rename interval remains a known residual
/// window rather than a filesystem compare-and-swap.
fn rollback_codex_credentials_if_unchanged(
    receipt: &CodexCredentialWriteReceipt,
) -> Result<bool, String> {
    let current_raw = fs::read_to_string(&receipt.path)
        .map_err(|e| format!("read Codex auth.json before rollback: {}", e))?;
    let current_root: Value = serde_json::from_str(&current_raw)
        .map_err(|e| format!("decode Codex auth.json before rollback: {}", e))?;
    if current_root != receipt.persisted_root {
        return Ok(false);
    }
    let previous_data = serde_json::to_string_pretty(&receipt.previous_root)
        .map_err(|e| format!("encode Codex auth.json rollback: {}", e))?;
    atomic_write(&receipt.path, &previous_data)
        .map_err(|e| format!("rollback Codex auth.json: {}", e))?;
    Ok(true)
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

static CLAUDE_USER_AGENT: LazyLock<String> = LazyLock::new(detect_claude_user_agent);

fn claude_user_agent() -> &'static str {
    CLAUDE_USER_AGENT.as_str()
}

fn detect_claude_user_agent() -> String {
    let mut command = std::process::Command::new("claude");
    command.arg("--version");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    command
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| claude_user_agent_from_stdout(&output.stdout))
        .unwrap_or_else(|| "claude-code/2.1.0".to_string())
}

fn claude_user_agent_from_stdout(stdout: &[u8]) -> Option<String> {
    std::str::from_utf8(stdout)
        .ok()?
        .split_whitespace()
        .next()
        .filter(|version| !version.is_empty())
        .map(|version| format!("claude-code/{version}"))
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
    fn claude_user_agent_uses_first_version_token() {
        assert_eq!(
            claude_user_agent_from_stdout(b"2.1.5 (Claude Code)\r\n").as_deref(),
            Some("claude-code/2.1.5")
        );
        assert_eq!(claude_user_agent_from_stdout(b" \r\n"), None);
        assert_eq!(claude_user_agent_from_stdout(&[0xff]), None);
    }

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

    #[test]
    fn claude_gate_is_binding_scoped_and_expires() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let scope = TestRefreshScope::new("claude", "local-gate");
        let binding_a = ProviderCacheBinding::primary(
            scope
                .resolve_current("fixture", "account-a", b"marker-a")
                .unwrap(),
        );
        let binding_b = ProviderCacheBinding::primary(
            scope
                .resolve_current("fixture", "account-b", b"marker-b")
                .unwrap(),
        );
        let mut gate = ClaudeUsageGate::default();

        gate.record_rate_limit(binding_a.clone(), None, now);
        let until = gate.blocked_until_for(&binding_a, now).unwrap();
        assert_eq!((until - now).num_seconds(), 300);

        assert!(gate.blocked_until_for(&binding_b, now).is_none());
        assert!(gate.blocked_until_for(&binding_a, now).is_none());

        gate.record_rate_limit(
            binding_a.clone(),
            Some(now + chrono::Duration::seconds(60)),
            now,
        );
        assert!(gate
            .blocked_until_for(&binding_a, now + chrono::Duration::seconds(61))
            .is_none());
        gate.clear();
        assert!(gate.blocked_until_for(&binding_a, now).is_none());
        scope.cleanup();
    }

    #[tokio::test]
    async fn claude_login_usage_gates_current_binding_before_refresh_and_request() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let scope = TestRefreshScope::new("claude", "routed-gate");
        let binding_a = ProviderCacheBinding::primary(
            scope
                .resolve_current("fixture", "account-a", b"marker-a")
                .unwrap(),
        );
        let binding_b = ProviderCacheBinding::primary(
            scope
                .resolve_current("fixture", "account-b", b"marker-b")
                .unwrap(),
        );
        let mut credentials = ClaudeCredentials {
            access_token: "claude-access".to_string(),
            refresh_token: Some("claude-refresh".to_string()),
            expires_at: Some(Utc::now() - chrono::Duration::minutes(1)),
            scopes: vec!["user:profile".to_string()],
            rate_limit_tier: None,
            subscription_type: None,
            source: ClaudeCredentialSource::File,
            raw_root: None,
            keychain_account: None,
            scope_slot: CredentialSlot {
                semantic_source: "fixture",
                canonical_location: "fixture".to_string(),
            },
        };
        let mut gate = ClaudeUsageGate::default();
        gate.record_rate_limit(binding_a.clone(), None, now);
        let refresh_calls = std::cell::Cell::new(0);
        let header_calls = std::cell::Cell::new(0);
        let usage_calls = std::cell::Cell::new(0);

        let (source, outcome) = fetch_claude_login_usage_with(
            credentials.clone(),
            binding_a.clone(),
            now,
            |binding, at| gate.blocked_until_for(binding, at),
            |credentials| {
                refresh_calls.set(refresh_calls.get() + 1);
                let binding = binding_a.clone();
                async move { Ok((credentials, binding.primary.clone(), Some(binding))) }
            },
            |_, _, _| async {
                header_calls.set(header_calls.get() + 1);
                claude_test_success_outcome()
            },
            |_, _, _, _| async {
                usage_calls.set(usage_calls.get() + 1);
                ("oauth", claude_test_success_outcome())
            },
        )
        .await;
        assert_eq!(source, "oauth");
        assert!(matches!(
            outcome,
            ProviderFetchOutcome::Failure(ProviderFetchFailure::Transient {
                attempt_binding: Some(ref binding),
                transport_diagnostic: SafeTransportDiagnostic {
                    category: TransportCategory::RateLimited,
                    status: Some(429),
                    ..
                },
                ..
            }) if binding == &binding_a
        ));
        assert_eq!(refresh_calls.get(), 0);
        assert_eq!(header_calls.get(), 0);
        assert_eq!(usage_calls.get(), 0);

        credentials.expires_at = None;
        credentials.scopes = vec!["org:create_api_key".to_string()];
        gate.record_rate_limit(binding_a.clone(), None, now);
        let (source, outcome) = fetch_claude_login_usage_with(
            credentials.clone(),
            binding_a.clone(),
            now,
            |binding, at| gate.blocked_until_for(binding, at),
            |credentials| {
                refresh_calls.set(refresh_calls.get() + 1);
                let binding = binding_a.clone();
                async move { Ok((credentials, binding.primary.clone(), Some(binding))) }
            },
            |_, _, _| async {
                header_calls.set(header_calls.get() + 1);
                claude_test_success_outcome()
            },
            |_, _, _, _| async {
                usage_calls.set(usage_calls.get() + 1);
                ("oauth", claude_test_success_outcome())
            },
        )
        .await;
        assert_eq!(source, "setup-token");
        assert!(matches!(outcome, ProviderFetchOutcome::Success { .. }));
        assert_eq!(refresh_calls.get(), 0);
        assert_eq!(header_calls.get(), 1);
        assert_eq!(usage_calls.get(), 0);
        assert!(gate.blocked_until_for(&binding_a, now).is_some());

        credentials.scopes = vec!["user:profile".to_string()];
        gate.record_rate_limit(binding_a.clone(), None, now);
        let (source, outcome) = fetch_claude_login_usage_with(
            credentials,
            binding_b.clone(),
            now,
            |binding, at| gate.blocked_until_for(binding, at),
            |credentials| {
                refresh_calls.set(refresh_calls.get() + 1);
                let binding = binding_b.clone();
                async move { Ok((credentials, binding.primary.clone(), Some(binding))) }
            },
            |_, _, _| async {
                header_calls.set(header_calls.get() + 1);
                claude_test_success_outcome()
            },
            |_, _, _, _| async {
                usage_calls.set(usage_calls.get() + 1);
                ("oauth", claude_test_success_outcome())
            },
        )
        .await;
        assert_eq!(source, "oauth");
        assert!(matches!(outcome, ProviderFetchOutcome::Success { .. }));
        assert_eq!(refresh_calls.get(), 0);
        assert_eq!(header_calls.get(), 1);
        assert_eq!(usage_calls.get(), 1);
        scope.cleanup();
    }

    fn cache_test_snapshot(
        client_id: &str,
        account_scope: Result<AccountScope, AccountScopeError>,
        now: DateTime<Utc>,
    ) -> AgentUsageSnapshot {
        AgentUsageSnapshot {
            client_id: client_id.to_string(),
            source: "oauth".to_string(),
            updated_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            identity: Some(AgentIdentity {
                email: Some("fixture@example.invalid".to_string()),
                plan: Some("Fixture".to_string()),
            }),
            account_scope,
            windows: vec![UsageWindow::from_provider_used_percent(
                "Session".to_string(),
                20.0,
                Some(now + chrono::Duration::hours(5)),
                now,
            )
            .with_identity(
                "main.session.v1",
                Some("main.session.v1".to_string()),
                None,
                Some(DurationEvidence::contract(300 * 60)),
            )],
            credits: Some(CreditsSnapshot {
                remaining: Some(-2.5),
                unlimited: false,
            }),
            error: None,
            transport_diagnostic: None,
        }
    }

    fn claude_test_login_credentials() -> ClaudeCredentials {
        ClaudeCredentials {
            access_token: "claude-access".to_string(),
            refresh_token: Some("claude-refresh".to_string()),
            expires_at: None,
            scopes: vec!["user:profile".to_string()],
            rate_limit_tier: None,
            subscription_type: None,
            source: ClaudeCredentialSource::File,
            raw_root: None,
            keychain_account: None,
            scope_slot: CredentialSlot {
                semantic_source: "fixture",
                canonical_location: "fixture".to_string(),
            },
        }
    }

    fn claude_test_setup_token() -> ResolvedClaudeToken {
        ResolvedClaudeToken {
            access_token: "setup-access".to_string(),
            scope_slot: CredentialSlot {
                semantic_source: "fixture-setup",
                canonical_location: "fixture-setup".to_string(),
            },
        }
    }

    fn claude_test_success_outcome() -> ProviderFetchOutcome {
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        ProviderFetchOutcome::Success {
            snapshot: cache_test_snapshot("claude", Err(AccountScopeError::NoTrustedEvidence), now),
            cache_binding: None,
        }
    }

    #[tokio::test]
    async fn claude_login_precedence_falls_through_only_for_absent_credentials() {
        {
            let primary_calls = std::cell::Cell::new(0);
            let setup_loads = std::cell::Cell::new(0);
            let setup_calls = std::cell::Cell::new(0);
            let (source, outcome) = fetch_claude_login_or_setup_with(
                resolve_stored_claude_login("{", ClaudeCredentialSource::File),
                |_| async {
                    primary_calls.set(primary_calls.get() + 1);
                    ("oauth", claude_test_success_outcome())
                },
                || {
                    setup_loads.set(setup_loads.get() + 1);
                    Ok(Some(claude_test_setup_token()))
                },
                |_| async {
                    setup_calls.set(setup_calls.get() + 1);
                    ("setup-token", claude_test_success_outcome())
                },
            )
            .await;
            assert_eq!(source, "oauth");
            assert!(matches!(
                outcome,
                ProviderFetchOutcome::Failure(ProviderFetchFailure::Terminal { .. })
            ));
            assert_eq!(primary_calls.get(), 0);
            assert_eq!(setup_loads.get(), 0);
            assert_eq!(setup_calls.get(), 0);
        }

        for (label, primary_outcome) in [
            (
                "401",
                ProviderFetchOutcome::Failure(ProviderFetchFailure::terminal(
                    "Claude OAuth token expired or invalid. Run `claude` to re-authenticate.",
                )),
            ),
            (
                "transient",
                ProviderFetchOutcome::Failure(ProviderFetchFailure::transient(
                    "Claude usage request failed. Retrying automatically.",
                    None,
                    SafeTransportDiagnostic::server_error(503),
                )),
            ),
        ] {
            let primary_calls = std::cell::Cell::new(0);
            let setup_loads = std::cell::Cell::new(0);
            let setup_calls = std::cell::Cell::new(0);
            let (source, outcome) = fetch_claude_login_or_setup_with(
                ClaudeLoginResolution::Ready(claude_test_login_credentials()),
                |_| {
                    primary_calls.set(primary_calls.get() + 1);
                    async move { ("oauth", primary_outcome) }
                },
                || {
                    setup_loads.set(setup_loads.get() + 1);
                    Ok(Some(claude_test_setup_token()))
                },
                |_| async {
                    setup_calls.set(setup_calls.get() + 1);
                    ("setup-token", claude_test_success_outcome())
                },
            )
            .await;
            assert_eq!(source, "oauth", "{label}");
            match label {
                "401" => assert!(matches!(
                    outcome,
                    ProviderFetchOutcome::Failure(ProviderFetchFailure::Terminal { .. })
                )),
                _ => assert!(matches!(
                    outcome,
                    ProviderFetchOutcome::Failure(ProviderFetchFailure::Transient { .. })
                )),
            }
            assert_eq!(primary_calls.get(), 1, "{label}");
            assert_eq!(setup_loads.get(), 0, "{label}");
            assert_eq!(setup_calls.get(), 0, "{label}");
        }

        {
            let primary_calls = std::cell::Cell::new(0);
            let setup_loads = std::cell::Cell::new(0);
            let setup_calls = std::cell::Cell::new(0);
            let logged_out = resolve_stored_claude_login(
                r#"{"claudeAiOauth":{"refreshToken":"stale"}}"#,
                ClaudeCredentialSource::File,
            );
            assert!(matches!(logged_out, ClaudeLoginResolution::ExplicitLogout));
            let (source, outcome) = fetch_claude_login_or_setup_with(
                logged_out,
                |_| async {
                    primary_calls.set(primary_calls.get() + 1);
                    ("oauth", claude_test_success_outcome())
                },
                || {
                    setup_loads.set(setup_loads.get() + 1);
                    Ok(Some(claude_test_setup_token()))
                },
                |_| async {
                    setup_calls.set(setup_calls.get() + 1);
                    ("setup-token", claude_test_success_outcome())
                },
            )
            .await;
            assert_eq!(source, "setup-token");
            assert!(matches!(outcome, ProviderFetchOutcome::Success { .. }));
            assert_eq!(primary_calls.get(), 0);
            assert_eq!(setup_loads.get(), 1);
            assert_eq!(setup_calls.get(), 1);
        }
    }

    #[tokio::test]
    async fn claude_keychain_logout_blocks_stale_file_login_and_allows_setup_token() {
        const VALID_FILE_LOGIN: &str =
            r#"{"claudeAiOauth":{"accessToken":"file-access","refreshToken":"file-refresh"}}"#;

        for malformed in [
            r#"{"claudeAiOauth":{}}"#,
            r#"{"claudeAiOauth":{"accessToken":null}}"#,
        ] {
            assert!(matches!(
                resolve_stored_claude_login(malformed, ClaudeCredentialSource::Keychain),
                ClaudeLoginResolution::Terminal
            ));
        }

        let missing_file_loads = std::cell::Cell::new(0);
        let missing = load_stored_claude_login_with(
            || Ok(None),
            || {
                missing_file_loads.set(missing_file_loads.get() + 1);
                Ok(Some(VALID_FILE_LOGIN.to_string()))
            },
        );
        assert!(matches!(
            missing,
            ClaudeLoginResolution::Ready(ClaudeCredentials {
                source: ClaudeCredentialSource::File,
                ..
            })
        ));
        assert_eq!(missing_file_loads.get(), 1);

        let file_loads = std::cell::Cell::new(0);
        let login = load_stored_claude_login_with(
            || {
                Ok(Some(
                    r#"{"claudeAiOauth":{"refreshToken":"stale-file-shape"}}"#.to_string(),
                ))
            },
            || {
                file_loads.set(file_loads.get() + 1);
                Ok(Some(VALID_FILE_LOGIN.to_string()))
            },
        );
        assert!(matches!(login, ClaudeLoginResolution::ExplicitLogout));
        assert_eq!(file_loads.get(), 0);

        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let scope = TestRefreshScope::new("claude", "keychain-explicit-logout");
        let binding = ProviderCacheBinding::primary(
            scope
                .resolve_current("fixture", "logged-in-account", b"marker")
                .unwrap(),
        );
        let mut gate = ClaudeUsageGate::default();
        gate.record_rate_limit(binding.clone(), None, now);
        assert!(gate.blocked_until_for(&binding, now).is_some());
        clear_claude_gate_for_login_resolution(&login, &mut gate);
        assert!(gate.blocked_until_for(&binding, now).is_none());

        let oauth_calls = std::cell::Cell::new(0);
        let setup_loads = std::cell::Cell::new(0);
        let setup_calls = std::cell::Cell::new(0);
        let (source, outcome) = fetch_claude_login_or_setup_with(
            login,
            |_| async {
                oauth_calls.set(oauth_calls.get() + 1);
                ("oauth", claude_test_success_outcome())
            },
            || {
                setup_loads.set(setup_loads.get() + 1);
                Ok(Some(claude_test_setup_token()))
            },
            |_| async {
                setup_calls.set(setup_calls.get() + 1);
                ("setup-token", claude_test_success_outcome())
            },
        )
        .await;
        assert_eq!(source, "setup-token");
        assert!(matches!(outcome, ProviderFetchOutcome::Success { .. }));
        assert_eq!(oauth_calls.get(), 0);
        assert_eq!(setup_loads.get(), 1);
        assert_eq!(setup_calls.get(), 1);
        scope.cleanup();
    }

    fn timeout_diagnostic() -> SafeTransportDiagnostic {
        SafeTransportDiagnostic::from_facts(TransportErrorFacts::synthetic(
            true,
            false,
            TransportPhase::Request,
            None,
        ))
    }

    #[test]
    fn copilot_malformed_optional_reset_remains_success_and_keeps_last_good() {
        let scope = TestRefreshScope::new("copilot", "lossy-optional-reset");
        let account_scope = scope
            .resolve_current("fixture", "account-a", b"marker-a")
            .unwrap();
        let binding = ProviderCacheBinding::primary(account_scope.clone());
        let cache = Mutex::new(ProviderLastGoodCache::default());
        let fresh_at = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();

        apply_provider_outcome_with(
            &cache,
            "copilot",
            "oauth",
            fresh_at,
            ProviderFetchOutcome::Success {
                snapshot: cache_test_snapshot("copilot", Ok(account_scope.clone()), fresh_at),
                cache_binding: Some(binding.clone()),
            },
            |_| {},
        )
        .unwrap();

        let response_at = fresh_at + chrono::Duration::minutes(1);
        let decoded = agent_copilot::decode_usage_response(
            r#"{
                "quota_reset_date": {"credential":"token-secret"},
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 100,
                        "remaining": 60,
                        "percent_remaining": 60
                    }
                }
            }"#,
            response_at,
        );
        let outcome = match decoded {
            Ok((plan, windows)) => ProviderFetchOutcome::Success {
                snapshot: AgentUsageSnapshot {
                    client_id: "copilot".to_string(),
                    source: "oauth".to_string(),
                    updated_at: response_at.to_rfc3339_opts(SecondsFormat::Millis, true),
                    identity: Some(AgentIdentity { email: None, plan }),
                    account_scope: Ok(account_scope),
                    windows,
                    credits: None,
                    error: None,
                    transport_diagnostic: None,
                },
                cache_binding: Some(binding),
            },
            Err(failure) => ProviderFetchOutcome::Failure(failure),
        };
        let snapshot =
            apply_provider_outcome_with(&cache, "copilot", "oauth", response_at, outcome, |_| {})
                .unwrap();

        assert!(snapshot.error.is_none());
        assert_eq!(snapshot.windows.len(), 1);
        assert!((snapshot.windows[0].remaining_percent - 60.0).abs() < 0.01);
        assert!(snapshot.windows[0].resets_at.is_none());
        let cached = lock_last_good(&cache).entries["copilot"].snapshot.clone();
        assert_eq!(cached.updated_at, snapshot.updated_at);
        assert_eq!(cached.windows.len(), 1);
        assert!(cached.error.is_none());
        assert!(cached.transport_diagnostic.is_none());
        scope.cleanup();
    }

    #[test]
    fn last_good_same_binding_fallback_preserves_clean_snapshot_without_enrichment() {
        let scope = TestRefreshScope::new("codex", "last-good-same-binding");
        let account_scope = scope
            .resolve_current("fixture", "account-a", b"marker-a")
            .unwrap();
        let binding = ProviderCacheBinding::primary(account_scope.clone());
        let cache = Mutex::new(ProviderLastGoodCache::default());
        let fresh_at = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let failure_at = fresh_at + chrono::Duration::minutes(1);
        let enrich_calls = std::cell::Cell::new(0);

        let fresh = apply_provider_outcome_with(
            &cache,
            "codex",
            "oauth",
            fresh_at,
            ProviderFetchOutcome::Success {
                snapshot: cache_test_snapshot("codex", Ok(account_scope), fresh_at),
                cache_binding: Some(binding.clone()),
            },
            |snapshot| {
                enrich_calls.set(enrich_calls.get() + 1);
                snapshot.windows[0].pace_status = PaceStatusPayload {
                    state: PaceState::Available,
                    window_key: Some("main.session.v1".to_string()),
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
            },
        )
        .unwrap();
        assert_eq!(enrich_calls.get(), 1);

        let fallback = apply_provider_outcome_with(
            &cache,
            "codex",
            "oauth",
            failure_at,
            ProviderFetchOutcome::Failure(ProviderFetchFailure::transient(
                "Codex usage request failed. Retrying automatically.",
                Some(binding.clone()),
                timeout_diagnostic(),
            )),
            |_| enrich_calls.set(enrich_calls.get() + 1),
        )
        .unwrap();
        assert_eq!(enrich_calls.get(), 1);
        assert_eq!(fallback.updated_at, fresh.updated_at);
        assert_eq!(fallback.source, fresh.source);
        assert_eq!(
            fallback.identity.as_ref().unwrap().plan.as_deref(),
            Some("Fixture")
        );
        assert_eq!(fallback.windows.len(), 1);
        assert_eq!(fallback.windows[0].pace_status.complete_cycles, 6);
        assert_eq!(
            fallback.windows[0]
                .historical_pace
                .as_ref()
                .map(|pace| pace.expected_used_percent),
            Some(35.0)
        );
        assert_eq!(
            fallback
                .credits
                .as_ref()
                .and_then(|credits| credits.remaining),
            Some(-2.5)
        );
        assert!(matches!(
            fallback.account_scope,
            Err(AccountScopeError::NoTrustedEvidence)
        ));
        assert!(fallback.error.is_some());
        assert_eq!(
            fallback
                .transport_diagnostic
                .map(|diagnostic| diagnostic.category),
            Some(TransportCategory::Timeout)
        );

        let cached = lock_last_good(&cache)
            .entries
            .get("codex")
            .unwrap()
            .snapshot
            .clone();
        assert!(cached.error.is_none());
        assert!(cached.transport_diagnostic.is_none());
        drop(cached);

        let fallback_again = apply_provider_outcome_with(
            &cache,
            "codex",
            "oauth",
            failure_at + chrono::Duration::minutes(1),
            ProviderFetchOutcome::Failure(ProviderFetchFailure::transient(
                "Codex usage request failed. Retrying automatically.",
                Some(binding.clone()),
                SafeTransportDiagnostic::server_error(503),
            )),
            |_| enrich_calls.set(enrich_calls.get() + 1),
        )
        .unwrap();
        assert_eq!(enrich_calls.get(), 1);
        assert_eq!(fallback_again.updated_at, fresh.updated_at);
        assert_eq!(fallback_again.windows[0].pace_status.complete_cycles, 6);

        let dns_fallback = apply_provider_outcome_with(
            &cache,
            "codex",
            "oauth",
            failure_at + chrono::Duration::minutes(2),
            ProviderFetchOutcome::Failure(ProviderFetchFailure::transient(
                "Codex usage request failed. Retrying automatically.",
                Some(binding),
                SafeTransportDiagnostic::from_facts(TransportErrorFacts {
                    is_timeout: false,
                    is_connect: true,
                    is_dns: true,
                    is_tls: false,
                    phase: TransportPhase::Request,
                    raw_os_code: None,
                }),
            )),
            |_| enrich_calls.set(enrich_calls.get() + 1),
        )
        .unwrap();
        assert_eq!(enrich_calls.get(), 1);
        assert_eq!(dns_fallback.updated_at, fresh.updated_at);
        assert_eq!(dns_fallback.windows[0].pace_status.complete_cycles, 6);
        assert_eq!(
            dns_fallback
                .transport_diagnostic
                .map(|diagnostic| diagnostic.category),
            Some(TransportCategory::Dns)
        );
        scope.cleanup();
    }

    #[test]
    fn last_good_mismatch_unbound_terminal_and_absent_clear_cache() {
        let scope = TestRefreshScope::new("codex", "last-good-clear");
        let scope_a = scope
            .resolve_current("fixture", "account-a", b"marker-a")
            .unwrap();
        let scope_b = scope
            .resolve_current("fixture", "account-b", b"marker-b")
            .unwrap();
        let binding_a = ProviderCacheBinding::primary(scope_a.clone());
        let binding_b = ProviderCacheBinding::primary(scope_b);
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();

        for failure in [
            ProviderFetchFailure::transient("mismatch", Some(binding_b), timeout_diagnostic()),
            ProviderFetchFailure::transient("unbound", None, timeout_diagnostic()),
            ProviderFetchFailure::terminal("terminal"),
        ] {
            let cache = Mutex::new(ProviderLastGoodCache::default());
            apply_provider_outcome_with(
                &cache,
                "codex",
                "oauth",
                now,
                ProviderFetchOutcome::Success {
                    snapshot: cache_test_snapshot("codex", Ok(scope_a.clone()), now),
                    cache_binding: Some(binding_a.clone()),
                },
                |_| {},
            );
            let result = apply_provider_outcome_with(
                &cache,
                "codex",
                "oauth",
                now + chrono::Duration::seconds(1),
                ProviderFetchOutcome::Failure(failure),
                |_| panic!("failure must not enrich"),
            )
            .unwrap();
            assert!(result.windows.is_empty());
            assert!(!lock_last_good(&cache).entries.contains_key("codex"));
        }

        let cache = Mutex::new(ProviderLastGoodCache::default());
        apply_provider_outcome_with(
            &cache,
            "codex",
            "oauth",
            now,
            ProviderFetchOutcome::Success {
                snapshot: cache_test_snapshot("codex", Ok(scope_a), now),
                cache_binding: Some(binding_a),
            },
            |_| {},
        );
        assert!(apply_provider_outcome_with(
            &cache,
            "codex",
            "oauth",
            now,
            ProviderFetchOutcome::Absent,
            |_| panic!("absent must not enrich"),
        )
        .is_none());
        assert!(!lock_last_good(&cache).entries.contains_key("codex"));
        scope.cleanup();
    }

    #[test]
    fn uncacheable_or_invalid_success_clears_prior_last_good() {
        let scope = TestRefreshScope::new("antigravity", "last-good-uncacheable");
        let account_scope = scope
            .resolve_current("fixture", "account-a", b"marker-a")
            .unwrap();
        let binding = ProviderCacheBinding::primary(account_scope.clone());
        let now = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();

        let cache = Mutex::new(ProviderLastGoodCache::default());
        apply_provider_outcome_with(
            &cache,
            "antigravity",
            "oauth",
            now,
            ProviderFetchOutcome::Success {
                snapshot: cache_test_snapshot("antigravity", Ok(account_scope.clone()), now),
                cache_binding: Some(binding.clone()),
            },
            |_| {},
        );
        let anonymous = apply_provider_outcome_with(
            &cache,
            "antigravity",
            "local",
            now,
            ProviderFetchOutcome::Success {
                snapshot: cache_test_snapshot(
                    "antigravity",
                    Err(AccountScopeError::NoTrustedEvidence),
                    now,
                ),
                cache_binding: None,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(anonymous.windows.len(), 1);
        assert!(!lock_last_good(&cache).entries.contains_key("antigravity"));

        apply_provider_outcome_with(
            &cache,
            "antigravity",
            "oauth",
            now,
            ProviderFetchOutcome::Success {
                snapshot: cache_test_snapshot("antigravity", Ok(account_scope.clone()), now),
                cache_binding: Some(binding.clone()),
            },
            |_| {},
        );
        let mut empty = cache_test_snapshot("antigravity", Ok(account_scope.clone()), now);
        empty.windows.clear();
        let live_empty = apply_provider_outcome_with(
            &cache,
            "antigravity",
            "oauth",
            now,
            ProviderFetchOutcome::Success {
                snapshot: empty,
                cache_binding: Some(binding.clone()),
            },
            |_| {},
        )
        .unwrap();
        assert!(live_empty.windows.is_empty());
        assert!(!lock_last_good(&cache).entries.contains_key("antigravity"));

        apply_provider_outcome_with(
            &cache,
            "antigravity",
            "oauth",
            now,
            ProviderFetchOutcome::Success {
                snapshot: cache_test_snapshot("antigravity", Ok(account_scope), now),
                cache_binding: Some(binding),
            },
            |_| {},
        );
        let enrich_calls = std::cell::Cell::new(0);
        let invalid = apply_provider_outcome_with(
            &cache,
            "antigravity",
            "oauth",
            now,
            ProviderFetchOutcome::Success {
                snapshot: cache_test_snapshot(
                    "antigravity",
                    Err(AccountScopeError::MetadataRead),
                    now,
                ),
                cache_binding: None,
            },
            |_| enrich_calls.set(enrich_calls.get() + 1),
        )
        .unwrap();
        assert!(invalid.windows.is_empty());
        assert_eq!(enrich_calls.get(), 0);
        assert!(!lock_last_good(&cache).entries.contains_key("antigravity"));
        scope.cleanup();
    }

    #[tokio::test]
    async fn status_before_body_enforces_terminal_transient_and_claude_exception() {
        use std::cell::Cell;

        for status in [401, 403, 418, 429, 500, 503] {
            let reads = Cell::new(0);
            let result = read_response_body(status, false, || async {
                reads.set(reads.get() + 1);
                Ok("sensitive body".to_string())
            })
            .await;
            assert_eq!(reads.get(), 0, "status {status} must not read body");
            match status {
                429 | 500 | 503 => {
                    assert!(matches!(result, Err(ResponseReadFailure::Transient(_))))
                }
                _ => assert_eq!(result, Err(ResponseReadFailure::Terminal(status))),
            }
        }

        let reads = Cell::new(0);
        let body = read_response_body(200, false, || async {
            reads.set(reads.get() + 1);
            Ok("success".to_string())
        })
        .await
        .unwrap();
        assert_eq!(body, "success");
        assert_eq!(reads.get(), 1);

        let reads = Cell::new(0);
        let failure = read_response_body(200, false, || async {
            reads.set(reads.get() + 1);
            Err(TransportErrorFacts::synthetic(
                false,
                false,
                TransportPhase::ResponseBody,
                Some(54),
            ))
        })
        .await
        .unwrap_err();
        assert_eq!(reads.get(), 1);
        assert!(matches!(
            failure,
            ResponseReadFailure::Transient(SafeTransportDiagnostic {
                category: TransportCategory::ConnectionReset,
                os_code: Some(54),
                ..
            })
        ));

        let reads = Cell::new(0);
        let body = read_response_body(403, true, || async {
            reads.set(reads.get() + 1);
            Ok("missing user:profile".to_string())
        })
        .await
        .unwrap();
        assert_eq!(body, "missing user:profile");
        assert_eq!(reads.get(), 1);

        let reads = Cell::new(0);
        let failure = read_response_body(403, true, || async {
            reads.set(reads.get() + 1);
            Err(TransportErrorFacts::synthetic(
                true,
                false,
                TransportPhase::ResponseBody,
                None,
            ))
        })
        .await
        .unwrap_err();
        assert_eq!(reads.get(), 1);
        assert_eq!(failure, ResponseReadFailure::Terminal(403));
    }

    #[tokio::test]
    async fn verified_binding_failure_prevents_every_provider_request() {
        for provider in ["codex", "claude", "grok", "copilot", "antigravity"] {
            let sends = std::cell::Cell::new(0);
            let result: Result<(), &str> =
                request_after_verified_binding(Err::<(), _>("scope unavailable"), |()| async {
                    sends.set(sends.get() + 1);
                    Ok(())
                })
                .await;
            assert_eq!(result, Err("scope unavailable"), "{provider}");
            assert_eq!(sends.get(), 0, "{provider}");
        }
    }

    #[derive(Debug)]
    struct SensitiveTestError(&'static str);

    impl std::fmt::Display for SensitiveTestError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl std::error::Error for SensitiveTestError {}

    #[derive(Debug)]
    struct NestedTestError {
        source: Box<dyn std::error::Error + Send + Sync>,
    }

    impl NestedTestError {
        fn new(source: impl std::error::Error + Send + Sync + 'static) -> Self {
            Self {
                source: Box::new(source),
            }
        }
    }

    impl std::fmt::Display for NestedTestError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("nested transport failure")
        }
    }

    impl std::error::Error for NestedTestError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.source.as_ref())
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct InjectedDnsFailureResolver;

    impl reqwest::dns::Resolve for InjectedDnsFailureResolver {
        fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            Box::pin(async {
                Err(Box::new(DnsResolutionError::new(SensitiveTestError(
                    "token-secret user@example.invalid /private/credential/path",
                )))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    #[tokio::test]
    async fn typed_gai_adapter_preserves_loopback_addresses_without_network_io() {
        let name: reqwest::dns::Name = "127.0.0.1".parse().unwrap();
        let addresses = reqwest::dns::Resolve::resolve(&TypedGaiResolver, name)
            .await
            .unwrap()
            .collect::<Vec<_>>();
        assert!(!addresses.is_empty());
        assert!(addresses.iter().all(|address| address.ip().is_loopback()));
        assert!(provider_http_client_builder().build().is_ok());
    }

    #[tokio::test]
    async fn injected_typed_dns_failure_is_classified_without_source_disclosure() {
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(InjectedDnsFailureResolver)
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        let error = client
            .get("http://account-123.example.invalid/private/path?token=token-secret")
            .send()
            .await
            .unwrap_err();
        let diagnostic = SafeTransportDiagnostic::from_facts(TransportErrorFacts::from_reqwest(
            &error,
            TransportPhase::Request,
        ));
        assert_eq!(diagnostic.category, TransportCategory::Dns);
        let wire = serde_json::to_string(&diagnostic).unwrap();
        assert_eq!(wire, r#"{"category":"dns"}"#);
        for secret in [
            "token-secret",
            "user@example.invalid",
            "account-123",
            "example.invalid",
            "/private/path",
            "/private/credential/path",
        ] {
            assert!(!wire.contains(secret));
        }
    }

    #[test]
    fn nested_typed_sources_are_found_without_text_classification() {
        let dns_error = NestedTestError::new(std::io::Error::other(DnsResolutionError::new(
            SensitiveTestError("token-secret"),
        )));
        let dns_facts = transport_source_facts(&dns_error);
        assert!(dns_facts.is_dns);
        assert!(!dns_facts.is_tls);
        assert_eq!(dns_facts.raw_os_code, None);

        let tls_error = NestedTestError::new(std::io::Error::other(rustls::Error::General(
            "token-secret".to_string(),
        )));
        let tls_facts = transport_source_facts(&tls_error);
        assert!(!tls_facts.is_dns);
        assert!(tls_facts.is_tls);
        assert_eq!(tls_facts.raw_os_code, None);

        let os_error =
            NestedTestError::new(std::io::Error::other(std::io::Error::from_raw_os_error(61)));
        assert_eq!(transport_source_facts(&os_error).raw_os_code, Some(61));
    }

    #[tokio::test]
    async fn loopback_plaintext_on_tls_endpoint_is_classified_as_tls() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut client_hello = [0_u8; 1024];
            let _ = stream.read(&mut client_hello).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });
        let client = provider_http_client_builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.get(format!("https://{address}/")).send(),
        )
        .await
        .expect("loopback TLS request timed out")
        .unwrap_err();
        let facts = TransportErrorFacts::from_reqwest(&error, TransportPhase::Request);
        assert!(facts.is_tls);
        assert_eq!(
            SafeTransportDiagnostic::from_facts(facts).category,
            TransportCategory::Tls
        );
        server.await.unwrap();
    }

    #[test]
    fn transport_diagnostic_precedence_and_generic_categories_are_stable() {
        let facts =
            |is_timeout, is_connect, is_dns, is_tls, phase, raw_os_code| TransportErrorFacts {
                is_timeout,
                is_connect,
                is_dns,
                is_tls,
                phase,
                raw_os_code,
            };
        let category = |facts| SafeTransportDiagnostic::from_facts(facts).category;

        assert_eq!(
            category(facts(
                true,
                true,
                true,
                true,
                TransportPhase::Request,
                Some(61),
            )),
            TransportCategory::Timeout
        );
        assert_eq!(
            category(facts(
                false,
                true,
                true,
                true,
                TransportPhase::Request,
                Some(61),
            )),
            TransportCategory::ConnectionRefused
        );
        assert_eq!(
            category(facts(
                false,
                true,
                true,
                true,
                TransportPhase::Request,
                Some(54),
            )),
            TransportCategory::ConnectionReset
        );
        assert_eq!(
            category(facts(
                false,
                true,
                true,
                true,
                TransportPhase::Request,
                None,
            )),
            TransportCategory::Dns
        );
        assert_eq!(
            category(facts(
                false,
                true,
                false,
                true,
                TransportPhase::Request,
                None,
            )),
            TransportCategory::Tls
        );
        assert_eq!(
            category(facts(
                false,
                true,
                false,
                false,
                TransportPhase::Request,
                None,
            )),
            TransportCategory::Connect
        );
        assert_eq!(
            category(facts(
                false,
                false,
                false,
                false,
                TransportPhase::Request,
                None,
            )),
            TransportCategory::Request
        );
        assert_eq!(
            category(facts(
                false,
                false,
                false,
                false,
                TransportPhase::ResponseBody,
                None,
            )),
            TransportCategory::ResponseBody
        );
    }

    #[test]
    fn structured_transport_diagnostic_serializes_only_allowlisted_fields() {
        let diagnostic = SafeTransportDiagnostic::from_facts(TransportErrorFacts::synthetic(
            false,
            true,
            TransportPhase::Request,
            Some(61),
        ));
        let wire = serde_json::to_string(&diagnostic).unwrap();
        assert_eq!(wire, r#"{"category":"connectionRefused","osCode":61}"#);
        for secret in [
            "token-secret",
            "Authorization",
            "https://example.invalid/path?query=secret#fragment",
            "user@example.invalid",
            "account-123",
            "/private/credential/path",
        ] {
            assert!(!wire.contains(secret));
        }
        assert_eq!(
            serde_json::to_value(SafeTransportDiagnostic::rate_limited(429)).unwrap(),
            serde_json::json!({ "category": "rateLimited", "status": 429 })
        );
        assert_eq!(
            serde_json::to_value(SafeTransportDiagnostic::server_error(503)).unwrap(),
            serde_json::json!({ "category": "serverError", "status": 503 })
        );
    }

    #[test]
    fn codex_credit_is_usable_only_when_balance_is_finite() {
        let credits = |balance, unlimited| CodexCredits { balance, unlimited };

        assert_eq!(
            finite_codex_balance(Some(&credits(Some(12.5), false))),
            Some(12.5)
        );
        assert_eq!(
            finite_codex_balance(Some(&credits(Some(-2.5), false))),
            Some(-2.5),
            "finite negative balances preserve the existing present-credit semantics"
        );
        assert_eq!(
            finite_codex_balance(Some(&credits(Some(0.0), false))),
            Some(0.0)
        );
        assert_eq!(
            finite_codex_balance(Some(&credits(Some(f64::NAN), false))),
            None
        );
        assert_eq!(
            finite_codex_balance(Some(&credits(Some(f64::INFINITY), false))),
            None
        );
        assert_eq!(
            finite_codex_balance(Some(&credits(None, true))),
            None,
            "unlimited without a balance is not usable credit"
        );
        assert_eq!(finite_codex_balance(None), None);
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
            keychain_account: None,
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
    fn provider_cache_binding_requires_structural_exact_match() {
        let scope_store = TestRefreshScope::new("codex", "binding-exact-match");
        let primary_a = scope_store
            .resolve_current("fixture", "primary-a", b"primary-a")
            .unwrap();
        let primary_b = scope_store
            .resolve_current("fixture", "primary-b", b"primary-b")
            .unwrap();
        let corroborating_a = scope_store
            .resolve_current("fixture", "corroborating-a", b"corroborating-a")
            .unwrap();
        let corroborating_b = scope_store
            .resolve_current("fixture", "corroborating-b", b"corroborating-b")
            .unwrap();

        let full = ProviderCacheBinding::new(primary_a.clone(), Some(corroborating_a.clone()));
        assert_eq!(full, full.clone());
        assert_ne!(
            full,
            ProviderCacheBinding::new(primary_b, Some(corroborating_a.clone()))
        );
        assert_ne!(
            full,
            ProviderCacheBinding::new(primary_a.clone(), Some(corroborating_b))
        );
        assert_ne!(full, ProviderCacheBinding::primary(primary_a));
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
            transport_diagnostic: None,
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

        let merged = merge_claude_credentials_json(&credentials, raw).unwrap();
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
    fn claude_keychain_write_decision_pins_account_and_rejects_target_mismatch() {
        let raw_a = r#"{
            "claudeAiOauth": {
                "accessToken": "old-access",
                "refreshToken": "old-refresh",
                "expiresAt": 0
            },
            "sibling": "a"
        }"#;
        let mut credentials =
            parse_claude_credentials_data(raw_a, ClaudeCredentialSource::Keychain).unwrap();
        credentials.keychain_account = Some("account-a".to_string());
        credentials.access_token = "new-access".to_string();
        credentials.refresh_token = Some("new-refresh".to_string());

        let (account, merged) =
            prepare_claude_keychain_write(&credentials, Some("account-a"), raw_a).unwrap();
        assert_eq!(account, "account-a");
        assert_eq!(
            serde_json::from_str::<Value>(&merged).unwrap()["claudeAiOauth"]["accessToken"],
            "new-access"
        );

        assert!(prepare_claude_keychain_write(&credentials, Some("account-b"), raw_a).is_err());
        assert!(prepare_claude_keychain_write(&credentials, None, raw_a).is_err());

        let raw_changed_target = r#"{
            "claudeAiOauth": {
                "accessToken": "account-b-access",
                "refreshToken": "account-b-refresh",
                "expiresAt": 0
            },
            "sibling": "b"
        }"#;
        assert!(
            prepare_claude_keychain_write(&credentials, Some("account-a"), raw_changed_target,)
                .is_err()
        );
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

    #[cfg(target_os = "windows")]
    fn open_without_delete_sharing(path: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        options.open(path).unwrap()
    }

    #[cfg(target_os = "windows")]
    fn atomic_temp_path(dir: &Path) -> Option<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .find(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .map(|entry| entry.path())
    }

    #[cfg(target_os = "windows")]
    fn lock_staged_atomic_temp(dir: &Path) -> Option<fs::File> {
        use std::os::windows::fs::OpenOptionsExt as _;

        let mut options = fs::OpenOptions::new();
        options.read(true).share_mode(0);
        options.open(atomic_temp_path(dir)?).ok()
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn atomic_write_retries_transient_windows_destination_lock() {
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!(
            "tb_atomic_windows_transient_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        fs::write(&path, "old").unwrap();
        let destination_lock = open_without_delete_sharing(&path);

        let writer_path = path.clone();
        let writer = std::thread::spawn(move || atomic_write(&writer_path, "new"));
        let deadline = Instant::now() + Duration::from_secs(1);
        let staged_temp_lock = loop {
            if let Some(file) = lock_staged_atomic_temp(&dir) {
                break Some(file);
            }
            if writer.is_finished() || Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let staged_temp_locked = staged_temp_lock.is_some();
        if staged_temp_locked {
            std::thread::sleep(Duration::from_millis(20));
        }
        let waited_for_retry = !writer.is_finished();
        drop(staged_temp_lock);
        drop(destination_lock);
        let result = writer.join().expect("atomic writer thread panicked");

        assert!(
            staged_temp_locked,
            "atomic write never completed temp-file staging"
        );
        assert!(
            waited_for_retry,
            "atomic write did not retry the sharing denial"
        );
        result.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert!(atomic_temp_path(&dir).is_none(), "temp file not cleaned up");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn atomic_write_exhausts_windows_retry_budget_without_losing_original() {
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!(
            "tb_atomic_windows_persistent_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        fs::write(&path, "old").unwrap();
        let destination_lock = open_without_delete_sharing(&path);

        let started = Instant::now();
        let result = atomic_write(&path, "new");
        let elapsed = started.elapsed();
        drop(destination_lock);

        assert!(result.is_err(), "persistent sharing denial must fail");
        assert!(
            elapsed >= Duration::from_millis(80),
            "atomic write returned before exhausting the retry budget: {elapsed:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert!(atomic_temp_path(&dir).is_none(), "temp file not cleaned up");
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
            transport_diagnostic: None,
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
            transport_diagnostic: None,
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
        resolves: Mutex<usize>,
        transfers: Mutex<Vec<(Vec<u8>, Vec<u8>)>>,
    }

    impl<'a> RecordingRefreshScope<'a> {
        fn new(inner: &'a TestRefreshScope) -> Self {
            Self {
                inner,
                resolves: Mutex::new(0),
                transfers: Mutex::new(Vec::new()),
            }
        }

        fn resolve_count(&self) -> usize {
            *self.resolves.lock().unwrap()
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
            *self.resolves.lock().unwrap() += 1;
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

    struct MetadataFailingRefreshScope<'a> {
        inner: &'a TestRefreshScope,
    }

    impl RefreshScopeTransaction for MetadataFailingRefreshScope<'_> {
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
            self.inner.fail_metadata_save();
            self.inner
                .transfer(semantic_source, canonical_location, old_marker, new_marker)
        }
    }

    fn checkpoint_at(
        target: Option<RefreshCheckpoint>,
    ) -> impl FnMut(RefreshCheckpoint) -> Result<(), ProviderFetchFailure> {
        move |checkpoint| {
            if Some(checkpoint) == target {
                Err(ProviderFetchFailure::terminal("injected crash"))
            } else {
                Ok(())
            }
        }
    }

    async fn codex_test_response(
        refresh_token: String,
        _attempt_binding: ProviderCacheBinding,
    ) -> Result<Value, ProviderFetchFailure> {
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

    async fn run_codex_refresh<R: RefreshScopeTransaction + ?Sized>(
        scope: &R,
        path: &Path,
        crash: Option<RefreshCheckpoint>,
    ) -> Result<(CodexCredentials, ProviderCacheBinding), ProviderFetchFailure> {
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
    async fn codex_refresh_rejects_concurrent_account_switch_without_touching_b() {
        const B_BYTES: &[u8] = br#"{
  "tokens": {
    "access_token": "account-b-access",
    "refresh_token": "account-b-refresh",
    "id_token": "account-b-id",
    "account_id": "account-b"
  },
  "sibling": {"writer": "b", "revision": 2}
}
"#;
        let (scope, path, _, metadata_before, _) = setup_codex_refresh("codex-target-switch");
        let recording = RecordingRefreshScope::new(&scope);
        let request_path = path.clone();

        let failure = refresh_codex_credentials_with(
            &path,
            &recording,
            move |refresh_token, _attempt_binding| async move {
                assert_eq!(refresh_token, "codex-old-refresh");
                fs::write(&request_path, B_BYTES).unwrap();
                Ok(serde_json::json!({
                    "access_token": "codex-new-access",
                    "refresh_token": "codex-new-refresh"
                }))
            },
            save_codex_credentials,
            checkpoint_at(None),
        )
        .await
        .unwrap_err();

        assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
        assert!(recording.transfers().is_empty());
        assert_eq!(scope.metadata_bytes(), metadata_before);
        let stored_bytes = fs::read(&path).unwrap();
        assert_eq!(stored_bytes, B_BYTES);
        assert!(!String::from_utf8_lossy(&stored_bytes).contains("codex-new"));
        let stored = load_codex_credentials_from(&path).unwrap();
        assert_eq!(stored.access_token, "account-b-access");
        assert_eq!(stored.refresh_token.as_deref(), Some("account-b-refresh"));
        assert_eq!(stored.account_id.as_deref(), Some("account-b"));
        scope.cleanup();
    }

    #[tokio::test]
    async fn codex_refresh_patches_unchanged_target_and_preserves_siblings() {
        let (scope, path, old_scope, _, location) = setup_codex_refresh("codex-target-unchanged");
        let original = serde_json::json!({
            "tokens": {
                "access_token": "codex-old-access",
                "refresh_token": "codex-old-refresh",
                "id_token": "codex-old-id",
                "token_sibling": {"keep": true}
            },
            "sibling": {"writer": "before", "revision": 1}
        });
        fs::write(&path, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        let current = serde_json::json!({
            "tokens": original["tokens"].clone(),
            "sibling": {"writer": "codex-cli", "revision": 2},
            "unrelated": [1, 2, 3]
        });
        let current_bytes = serde_json::to_vec_pretty(&current).unwrap();
        let request_path = path.clone();

        let (refreshed, post_binding) = refresh_codex_credentials_with(
            &path,
            &scope,
            move |refresh_token, _attempt_binding| async move {
                assert_eq!(refresh_token, "codex-old-refresh");
                fs::write(&request_path, current_bytes).unwrap();
                Ok(serde_json::json!({
                    "access_token": "codex-new-access",
                    "refresh_token": "codex-new-refresh"
                }))
            },
            save_codex_credentials,
            checkpoint_at(None),
        )
        .await
        .unwrap();

        assert_eq!(refreshed.access_token, "codex-new-access");
        assert_eq!(post_binding.primary, old_scope);
        let stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["tokens"]["access_token"], "codex-new-access");
        assert_eq!(stored["tokens"]["refresh_token"], "codex-new-refresh");
        assert_eq!(stored["tokens"]["id_token"], "codex-old-id");
        assert_eq!(stored["tokens"]["token_sibling"]["keep"], true);
        assert_eq!(stored["sibling"]["writer"], "codex-cli");
        assert_eq!(stored["sibling"]["revision"], 2);
        assert_eq!(stored["unrelated"], serde_json::json!([1, 2, 3]));
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
        scope.cleanup();
    }

    #[tokio::test]
    async fn codex_refresh_rejects_concurrent_logout_without_restoring_a() {
        const LOGGED_OUT_BYTES: &[u8] = br#"{
  "sibling": {"writer": "logout", "revision": 2}
}
"#;
        let (scope, path, _, metadata_before, _) = setup_codex_refresh("codex-target-logout");
        let recording = RecordingRefreshScope::new(&scope);
        let request_path = path.clone();

        let failure = refresh_codex_credentials_with(
            &path,
            &recording,
            move |refresh_token, _attempt_binding| async move {
                assert_eq!(refresh_token, "codex-old-refresh");
                fs::write(&request_path, LOGGED_OUT_BYTES).unwrap();
                Ok(serde_json::json!({
                    "access_token": "codex-new-access",
                    "refresh_token": "codex-new-refresh"
                }))
            },
            save_codex_credentials,
            checkpoint_at(None),
        )
        .await
        .unwrap_err();

        assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
        assert!(recording.transfers().is_empty());
        assert_eq!(scope.metadata_bytes(), metadata_before);
        let stored_bytes = fs::read(&path).unwrap();
        assert_eq!(stored_bytes, LOGGED_OUT_BYTES);
        assert!(!String::from_utf8_lossy(&stored_bytes).contains("codex-new"));
        let stored: Value = serde_json::from_slice(&stored_bytes).unwrap();
        assert!(stored.get("tokens").is_none());
        scope.cleanup();
    }

    #[tokio::test]
    async fn codex_refresh_canonicalizes_tokens_and_preserves_unrotated_marker() {
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

            let (refreshed, post_binding) = refresh_codex_credentials_with(
                &path,
                &recording,
                move |refresh_token, _attempt_binding| async move {
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
            assert_eq!(post_binding.primary, old_scope, "{tag}");
            assert_eq!(post_binding.corroborating, None, "{tag}");
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
    }

    #[tokio::test]
    async fn codex_refresh_rejects_invalid_success_schema_before_state_or_usage() {
        for (tag, response) in [
            ("array", serde_json::json!([])),
            ("empty-object", serde_json::json!({})),
            ("null", Value::Null),
            ("bool", Value::Bool(true)),
            ("string", Value::String("codex-new-access".to_string())),
            (
                "blank-access",
                serde_json::json!({ "access_token": " \t\n " }),
            ),
            (
                "invalid-aliases",
                serde_json::json!({
                    "access_token": false,
                    "accessToken": [],
                    "refreshToken": " codex-new-refresh "
                }),
            ),
        ] {
            let (scope, path, old_scope, metadata_before, location) =
                setup_codex_refresh(&format!("codex-invalid-schema-{tag}"));
            let credentials_before = fs::read(&path).unwrap();
            let recording = RecordingRefreshScope::new(&scope);
            let save_calls = std::cell::Cell::new(0);
            let usage_calls = std::cell::Cell::new(0);

            let refresh_result = refresh_codex_credentials_with(
                &path,
                &recording,
                move |refresh_token, _attempt_binding| async move {
                    assert_eq!(refresh_token, "codex-old-refresh");
                    Ok(response)
                },
                |_| -> Result<CodexCredentialWriteReceipt, String> {
                    save_calls.set(save_calls.get() + 1);
                    Err("unexpected save".to_string())
                },
                checkpoint_at(None),
            )
            .await;
            let result: Result<(), ProviderFetchFailure> =
                request_after_verified_binding(refresh_result, |_| async {
                    usage_calls.set(usage_calls.get() + 1);
                    Ok(())
                })
                .await;

            assert!(
                matches!(result, Err(ProviderFetchFailure::Terminal { .. })),
                "{tag}"
            );
            assert!(recording.transfers().is_empty(), "{tag}");
            assert_eq!(save_calls.get(), 0, "{tag}");
            assert_eq!(usage_calls.get(), 0, "{tag}");
            assert_eq!(fs::read(&path).unwrap(), credentials_before, "{tag}");
            assert_eq!(scope.metadata_bytes(), metadata_before, "{tag}");
            let reloaded = load_codex_credentials_from(&path).unwrap();
            assert_eq!(reloaded.access_token, "codex-old-access", "{tag}");
            assert_eq!(
                reloaded.refresh_token.as_deref(),
                Some("codex-old-refresh"),
                "{tag}"
            );
            assert!(reloaded.last_refresh.is_none(), "{tag}");
            assert_eq!(
                scope
                    .resolve_current("codex-auth-json", &location, reloaded.scope_marker())
                    .unwrap(),
                old_scope,
                "{tag}"
            );
            scope.cleanup();
        }
    }

    #[tokio::test]
    async fn codex_refresh_crash_boundaries_and_scope_gate_use_production_sequence() {
        // These checkpoints model process stops, not a cross-resource transaction:
        // after credential persistence, metadata may still be the pre-refresh bytes.
        for boundary in [
            RefreshCheckpoint::Reloaded,
            RefreshCheckpoint::NetworkReturned,
            RefreshCheckpoint::CredentialsPersisted,
            RefreshCheckpoint::MetadataHandled,
        ] {
            let (scope, path, old_scope, before, location) = setup_codex_refresh("codex-crash");
            let failure = run_codex_refresh(&scope, &path, Some(boundary))
                .await
                .unwrap_err();
            assert!(matches!(
                failure,
                ProviderFetchFailure::Terminal { ref display } if display == "injected crash"
            ));
            let credentials_persisted = matches!(
                boundary,
                RefreshCheckpoint::CredentialsPersisted | RefreshCheckpoint::MetadataHandled
            );
            let stored = load_codex_credentials_from(&path).unwrap();
            assert_eq!(
                stored.refresh_token.as_deref(),
                Some(if credentials_persisted {
                    "codex-new-refresh"
                } else {
                    "codex-old-refresh"
                })
            );
            if boundary == RefreshCheckpoint::MetadataHandled {
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
            } else {
                assert_eq!(scope.metadata_bytes(), before);
            }
            scope.cleanup();
        }

        let (scope, path, old_scope, before, location) = setup_codex_refresh("codex-metadata-fail");
        let auth_before = fs::read(&path).unwrap();
        let failing = MetadataFailingRefreshScope { inner: &scope };
        let failure = run_codex_refresh(&failing, &path, None).await.unwrap_err();
        assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
        assert_eq!(scope.metadata_bytes(), before);
        assert_eq!(fs::read(&path).unwrap(), auth_before);
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

        const CONCURRENT_LOGIN_BYTES: &[u8] = br#"{
  "tokens": {
    "access_token": "concurrent-access",
    "refresh_token": "concurrent-refresh",
    "id_token": "concurrent-id",
    "account_id": "concurrent-account"
  },
  "sibling": {"writer": "codex-cli", "revision": 3}
}
"#;
        let (scope, path, _, metadata_before, _) =
            setup_codex_refresh("codex-metadata-fail-concurrent-login");
        let failing = MetadataFailingRefreshScope { inner: &scope };
        let save_path = path.clone();
        let failure = refresh_codex_credentials_with(
            &path,
            &failing,
            codex_test_response,
            move |credentials| {
                let receipt = save_codex_credentials(credentials)?;
                fs::write(&save_path, CONCURRENT_LOGIN_BYTES)
                    .map_err(|error| format!("inject concurrent Codex login: {error}"))?;
                Ok(receipt)
            },
            checkpoint_at(None),
        )
        .await
        .unwrap_err();
        assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
        assert_eq!(scope.metadata_bytes(), metadata_before);
        assert_eq!(fs::read(&path).unwrap(), CONCURRENT_LOGIN_BYTES);
        scope.cleanup();

        let (scope, path, _, metadata_before, _) = setup_codex_refresh("codex-save-fail");
        let auth_before = fs::read(&path).unwrap();
        let recording = RecordingRefreshScope::new(&scope);
        let failure = refresh_codex_credentials_with(
            &path,
            &recording,
            codex_test_response,
            |_| Err("injected save failure".to_string()),
            checkpoint_at(None),
        )
        .await
        .unwrap_err();
        assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
        assert!(recording.transfers().is_empty());
        assert_eq!(scope.metadata_bytes(), metadata_before);
        assert_eq!(fs::read(&path).unwrap(), auth_before);
        scope.cleanup();

        let (scope, path, old_scope, _, location) = setup_codex_refresh("codex-success");
        let recording = RecordingRefreshScope::new(&scope);
        let (_, post_binding) = refresh_codex_credentials_with(
            &path,
            &recording,
            codex_test_response,
            save_codex_credentials,
            checkpoint_at(None),
        )
        .await
        .unwrap();
        assert_eq!(recording.resolve_count(), 1);
        assert_eq!(post_binding.primary, old_scope);
        assert_eq!(post_binding.corroborating, None);
        assert_eq!(
            scope
                .resolve_current("codex-auth-json", &location, b"codex-new-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();
    }

    #[tokio::test]
    async fn codex_refresh_transient_uses_lock_reloaded_binding_not_outer_binding() {
        let (scope, path, inner_scope, _, location) = setup_codex_refresh("codex-lock-binding");
        let outer_scope = scope
            .resolve_current("codex-auth-json", &location, b"outer-refresh-a")
            .unwrap();
        assert_ne!(outer_scope, inner_scope);
        let expected = ProviderCacheBinding::primary(inner_scope);
        let request_expected = expected.clone();

        let failure = refresh_codex_credentials_with(
            &path,
            &scope,
            move |refresh_token, attempt_binding| async move {
                assert_eq!(refresh_token, "codex-old-refresh");
                assert_eq!(attempt_binding, request_expected);
                Err(ProviderFetchFailure::transient(
                    "Codex token refresh failed. Retrying automatically.",
                    Some(attempt_binding),
                    SafeTransportDiagnostic::from_facts(TransportErrorFacts::synthetic(
                        true,
                        false,
                        TransportPhase::Request,
                        None,
                    )),
                ))
            },
            save_codex_credentials,
            checkpoint_at(None),
        )
        .await
        .unwrap_err();

        match failure {
            ProviderFetchFailure::Transient {
                attempt_binding, ..
            } => assert_eq!(attempt_binding, Some(expected)),
            ProviderFetchFailure::Terminal { .. } => panic!("timeout must remain transient"),
        }
        scope.cleanup();
    }

    async fn claude_test_response(
        refresh_token: String,
        _attempt_binding: ProviderCacheBinding,
    ) -> Result<ClaudeRefreshResponse, ProviderFetchFailure> {
        assert_eq!(refresh_token, "claude-old-refresh");
        Ok(ClaudeRefreshResponse {
            access_token: " claude-new-access ".to_string(),
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
    ) -> Result<
        (
            ClaudeCredentials,
            AccountScope,
            Option<ProviderCacheBinding>,
        ),
        ProviderFetchFailure,
    > {
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
    async fn claude_file_refresh_rejects_concurrent_target_change_without_touching_b() {
        const B_BYTES: &[u8] = br#"{
  "claudeAiOauth": {
    "accessToken": "account-b-access",
    "refreshToken": "account-b-refresh",
    "expiresAt": 4102444800000
  },
  "sibling": {"writer": "b", "revision": 2}
}
"#;
        let (scope, path, original, old_scope, _, _) =
            setup_claude_refresh("claude-file-target-race");
        let reload_path = path.clone();
        let request_path = path.clone();
        let save_path = path.clone();
        let save_failed = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed_save_failure = std::rc::Rc::clone(&save_failed);

        let (refreshed, scope_outcome, cache_binding) = refresh_claude_credentials_with(
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
            move |refresh_token, _attempt_binding| async move {
                assert_eq!(refresh_token, "claude-old-refresh");
                fs::write(&request_path, B_BYTES).unwrap();
                Ok(ClaudeRefreshResponse {
                    access_token: "claude-new-access".to_string(),
                    refresh_token: Some("claude-new-refresh".to_string()),
                    expires_in: 3_600,
                })
            },
            move |credentials| {
                let result = save_claude_credentials_to_file(credentials, &save_path);
                observed_save_failure.set(result.is_err());
                result
            },
            checkpoint_at(None),
        )
        .await
        .unwrap();

        assert_eq!(refreshed.access_token, "claude-new-access");
        assert_eq!(scope_outcome, old_scope);
        assert_eq!(cache_binding, None);
        assert!(save_failed.get());
        assert_eq!(fs::read(&path).unwrap(), B_BYTES);
        scope.cleanup();
    }

    #[tokio::test]
    async fn claude_file_refresh_preserves_concurrent_top_level_sibling() {
        const CURRENT_WITH_NEW_SIBLING: &str = r#"{
            "claudeAiOauth": {
                "accessToken": "claude-old-access",
                "refreshToken": "claude-old-refresh",
                "expiresAt": 0
            },
            "sibling": {"writer": "claude-cli", "revision": 2}
        }"#;
        let (scope, path, original, old_scope, _, _) =
            setup_claude_refresh("claude-file-sibling-race");
        let reload_path = path.clone();
        let request_path = path.clone();
        let save_path = path.clone();

        let (refreshed, scope_outcome, cache_binding) = refresh_claude_credentials_with(
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
            move |refresh_token, _attempt_binding| async move {
                assert_eq!(refresh_token, "claude-old-refresh");
                fs::write(&request_path, CURRENT_WITH_NEW_SIBLING).unwrap();
                Ok(ClaudeRefreshResponse {
                    access_token: "claude-new-access".to_string(),
                    refresh_token: Some("claude-new-refresh".to_string()),
                    expires_in: 3_600,
                })
            },
            move |credentials| save_claude_credentials_to_file(credentials, &save_path),
            checkpoint_at(None),
        )
        .await
        .unwrap();

        assert_eq!(refreshed.access_token, "claude-new-access");
        assert_eq!(scope_outcome, old_scope);
        assert_eq!(
            cache_binding,
            Some(ProviderCacheBinding::primary(old_scope.clone()))
        );
        let stored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["claudeAiOauth"]["accessToken"], "claude-new-access");
        assert_eq!(
            stored["claudeAiOauth"]["refreshToken"],
            "claude-new-refresh"
        );
        assert_eq!(stored["sibling"]["writer"], "claude-cli");
        assert_eq!(stored["sibling"]["revision"], 2);
        scope.cleanup();
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
            let (refreshed, scope_outcome, cache_binding) = refresh_claude_credentials_with(
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
                move |refresh_token, _attempt_binding| async move {
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
            assert_eq!(scope_outcome, old_scope);
            assert_eq!(
                cache_binding,
                Some(ProviderCacheBinding::primary(old_scope.clone()))
            );
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
    async fn claude_refresh_blank_access_token_is_terminal_before_metadata_or_save() {
        let (scope, path, original, old_scope, before, location) =
            setup_claude_refresh("claude-blank-access");
        let store_before = fs::read(&path).unwrap();
        let reload_path = path.clone();
        let save_calls = std::cell::Cell::new(0);

        let failure = refresh_claude_credentials_with(
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
            |refresh_token, _attempt_binding| async move {
                assert_eq!(refresh_token, "claude-old-refresh");
                Ok(ClaudeRefreshResponse {
                    access_token: " \t\n ".to_string(),
                    refresh_token: Some("claude-new-refresh".to_string()),
                    expires_in: 3_600,
                })
            },
            |_| {
                save_calls.set(save_calls.get() + 1);
                Ok(())
            },
            checkpoint_at(None),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            failure,
            ProviderFetchFailure::Terminal { ref display }
                if display == "Claude OAuth refresh response has no access token."
        ));
        assert_eq!(save_calls.get(), 0);
        assert_eq!(scope.metadata_bytes(), before);
        assert_eq!(fs::read(&path).unwrap(), store_before);
        assert_eq!(
            scope
                .resolve_current("claude-login-file", &location, b"claude-old-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();
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
            let failure = run_claude_refresh(&scope, &path, &original, Some(boundary))
                .await
                .unwrap_err();
            assert!(matches!(
                failure,
                ProviderFetchFailure::Terminal { ref display } if display == "injected crash"
            ));
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
        let failure = run_claude_refresh(&scope, &path, &original, None)
            .await
            .unwrap_err();
        assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
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

        let (scope, path, original, old_scope, _, location) =
            setup_claude_refresh("claude-save-fail");
        let reload_path = path.clone();
        let (refreshed, scope_outcome, cache_binding) = refresh_claude_credentials_with(
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
            claude_test_response,
            |_| Err("injected save failure".to_string()),
            checkpoint_at(None),
        )
        .await
        .unwrap();
        assert_eq!(refreshed.access_token, "claude-new-access");
        assert_eq!(scope_outcome, old_scope);
        assert_eq!(cache_binding, None);
        assert_eq!(
            stored_claude_refresh_token(&path).as_deref(),
            Some("claude-old-refresh")
        );
        assert_eq!(
            scope
                .resolve_current("claude-login-file", &location, b"claude-new-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();

        let (scope, path, original, old_scope, _, location) =
            setup_claude_refresh("claude-success");
        let (_, scope_outcome, cache_binding) = run_claude_refresh(&scope, &path, &original, None)
            .await
            .unwrap();
        assert_eq!(scope_outcome, old_scope);
        assert_eq!(
            cache_binding,
            Some(ProviderCacheBinding::primary(old_scope.clone()))
        );
        assert_eq!(
            scope
                .resolve_current("claude-login-file", &location, b"claude-new-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();
    }

    #[tokio::test]
    async fn claude_refresh_transient_uses_lock_reloaded_binding_not_outer_binding() {
        let (scope, path, mut original, inner_scope, _, location) =
            setup_claude_refresh("claude-lock-binding");
        original.refresh_token = Some("outer-refresh-a".to_string());
        let outer_scope = scope
            .resolve_current("claude-login-file", &location, b"outer-refresh-a")
            .unwrap();
        assert_ne!(outer_scope, inner_scope);
        let expected = ProviderCacheBinding::primary(inner_scope);
        let request_expected = expected.clone();
        let reload_path = path.clone();

        let failure = refresh_claude_credentials_with(
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
            move |refresh_token, attempt_binding| async move {
                assert_eq!(refresh_token, "claude-old-refresh");
                assert_eq!(attempt_binding, request_expected);
                Err(ProviderFetchFailure::transient(
                    "Claude OAuth refresh failed. Retrying automatically.",
                    Some(attempt_binding),
                    SafeTransportDiagnostic::from_facts(TransportErrorFacts::synthetic(
                        true,
                        false,
                        TransportPhase::Request,
                        None,
                    )),
                ))
            },
            |_| Ok(()),
            checkpoint_at(None),
        )
        .await
        .unwrap_err();

        match failure {
            ProviderFetchFailure::Transient {
                attempt_binding, ..
            } => assert_eq!(attempt_binding, Some(expected)),
            ProviderFetchFailure::Terminal { .. } => panic!("timeout must remain transient"),
        }
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
            transport_diagnostic: None,
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
            transport_diagnostic: None,
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
            transport_diagnostic: None,
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
            transport_diagnostic: None,
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
            transport_diagnostic: None,
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
            transport_diagnostic: None,
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
            publication_generation: 1,
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
                transport_diagnostic: None,
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
        let mut serialized = serde_json::to_value(payload).unwrap();
        assert_eq!(serialized["publicationGeneration"], 1);
        serialized
            .as_object_mut()
            .expect("payload serializes as an object")
            .remove("publicationGeneration");
        assert_eq!(fixture["payload"], serialized);
    }
}
