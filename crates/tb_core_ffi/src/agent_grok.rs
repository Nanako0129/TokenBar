//! Grok Build subscription quota — two meters, shown as two windows.
//!
//! Grok Build stores OIDC credentials at `$GROK_HOME/auth.json` (default
//! `~/.grok/auth.json`). TokenBar refreshes the access token against
//! `auth.x.ai` and reads usage from two views of the same private billing
//! endpoint the CLI uses:
//!
//!   Weekly:  GET /v1/billing?format=credits  -> creditUsagePercent / GrokBuild
//!   Monthly: GET /v1/billing                 -> used / monthlyLimit
//!
//! The weekly view is the SuperGrok weekly credit meter (the primary "will I
//! run out this week" number). Right after a weekly reset with no usage yet, it
//! OMITS the percent fields while still reporting the period — that is a genuine
//! 0%, not an error, and was the original cause of the card erroring. The
//! monthly view is the included-allowance meter (percent = used / monthlyLimit)
//! over a monthly period. The monthly call is best-effort with a short timeout:
//! a failure or hang there never sinks the card when the weekly meter succeeded.
//! Omit the card entirely when no Grok auth is on disk (same stance as Copilot).

use crate::agent_account_scope::{
    self, AccountScope, AccountScopeError, RefreshCheckpoint, RefreshScopeTransaction,
};
use crate::agent_quota_duration::DurationEvidence;
use crate::agent_usage::{
    provider_http_client_builder, read_response_body, request_after_verified_binding,
    AgentIdentity, ProviderCacheBinding, ProviderFetchFailure, RefreshTargetError,
    ResponseReadFailure, TransportErrorFacts, TransportPhase, UsageWindow,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{value::RawValue, Value};
use std::fs;
use std::path::{Path, PathBuf};

const GROK_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// Weekly SuperGrok credits meter. Returns `creditUsagePercent` — the shared
/// weekly credit pool — plus a `productUsage[]` decomposition of that same pool,
/// over a weekly `currentPeriod`. Right after a weekly reset, with no usage
/// recorded yet, xAI OMITS those percent fields — the period is still reported,
/// so that state is a genuine 0%, not an error.
const GROK_CREDITS_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// Monthly included-allowance meter. The default view reports `monthlyLimit` +
/// `used` over a monthly billing period (percent = used / monthlyLimit).
const GROK_MONTHLY_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing";
/// Refresh a few minutes early so a clock-skewed expiry doesn't 401 the billing call.
const ACCESS_SKEW_SECS: i64 = 120;
/// Best-effort monthly GET budget. Grok is joined in `agent_usage::run`, so a
/// full 30s hang on the additive meter would stall the whole quota payload after
/// the weekly meter is already ready.
const MONTHLY_TIMEOUT_SECS: u64 = 5;
/// The exact `currentPeriod.type` xAI stamps on the weekly credits meter. Matched
/// exactly (not by substring) so `..._BIWEEKLY` / `..._NOT_WEEKLY` can't pass.
const WEEKLY_PERIOD_TYPE: &str = "USAGE_PERIOD_TYPE_WEEKLY";
const WEEKLY_WINDOW_KEY: &str = "billing.weekly.v1";
/// The weekly pace series, versioned apart from the card id on purpose.
///
/// Quota history keys a series by (provider, account scope, window key) and
/// fits run-out forecasts over that exact series. Every sample recorded before
/// the pool fix holds the `GrokBuild` product share, and every sample after it
/// holds the whole weekly credit pool; for anyone who also spends credits
/// outside the CLI those are different quantities, and a fit spanning the
/// splice would report a burn rate nothing consumed. `v2` starts a clean
/// series, and the stale `v1` one falls out through the ordinary
/// inactive-series eviction.
///
/// The card id deliberately stays `v1`: it is what a stored tray or card window
/// selection is written against, and moving it would leave anyone who pinned
/// the weekly window pointing at a card that no longer exists. Losing a few
/// cycles of pace history is the intended cost here; silently unresolving a
/// saved selection is not.
const WEEKLY_PACE_SERIES_KEY: &str = "billing.weekly.v2";
const MONTHLY_WINDOW_KEY: &str = "billing.monthly.v1";

pub(crate) struct GrokData {
    pub identity: Option<AgentIdentity>,
    pub account_scope: Result<AccountScope, AccountScopeError>,
    pub cache_binding: Option<ProviderCacheBinding>,
    pub windows: Vec<UsageWindow>,
}

#[derive(Debug, Clone)]
struct GrokCredentials {
    auth_path: PathBuf,
    entry_key: String,
    access_token: String,
    refresh_token: String,
    client_id: String,
    expires_at: Option<DateTime<Utc>>,
    email: Option<String>,
    /// Full auth.json so we can patch only this entry and keep siblings intact.
    raw_json: Value,
}

impl GrokCredentials {
    fn scope_marker(&self) -> Option<&str> {
        let marker = self.refresh_token.trim();
        (!marker.is_empty()).then_some(marker)
    }

    fn scope_location(&self) -> Result<String, AccountScopeError> {
        agent_account_scope::canonical_file_location(&self.auth_path, Some(&self.entry_key))
    }

    fn resolve_account_scope(&self) -> Result<AccountScope, AccountScopeError> {
        let marker = self
            .scope_marker()
            .ok_or(AccountScopeError::NoTrustedEvidence)?;
        agent_account_scope::resolve_credential(
            "grok",
            "grok-auth-json",
            &self.scope_location()?,
            marker.as_bytes(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct BillingResponse {
    #[serde(default)]
    config: Option<BillingConfig>,
    /// Present on some older CLI payloads; optional today.
    #[serde(default, rename = "subscriptionTiers")]
    subscription_tiers: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingConfig {
    #[serde(default)]
    current_period: Option<UsagePeriod>,
    /// Unified-billing allowance and consumption (`{ "val": n }`). Percent is
    /// `used / monthly_limit`. Present in the default billing view.
    #[serde(default)]
    monthly_limit: Option<CentVal>,
    #[serde(default)]
    used: Option<CentVal>,
    /// RPC-shaped consumption (`usage.totalUsed`); accepted defensively in case
    /// an account nests the consumed amount under `usage` instead of `used`.
    #[serde(default)]
    usage: Option<UnifiedUsage>,
    /// Credits-view percent fields (`?format=credits`).
    #[serde(default)]
    credit_usage_percent: Option<Box<RawValue>>,
    #[serde(default)]
    product_usage: Option<Vec<ProductUsage>>,
    #[serde(default)]
    billing_period_start: Option<String>,
    #[serde(default)]
    billing_period_end: Option<String>,
}

/// xAI wraps billing amounts as `{ "val": n }` on the wire.
#[derive(Debug, Deserialize)]
struct CentVal {
    #[serde(default)]
    val: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnifiedUsage {
    #[serde(default)]
    total_used: Option<CentVal>,
}

#[derive(Debug, Deserialize)]
struct UsagePeriod {
    #[serde(default, rename = "type")]
    period_type: Option<String>,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
}

/// One product's share of the weekly credit pool. The `product` name is
/// deliberately not decoded: no product is privileged any more, because a
/// share cannot answer the pool window whichever product it belongs to, and
/// any product's usage refutes an empty week equally.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProductUsage {
    #[serde(default)]
    usage_percent: Option<Box<RawValue>>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Fetch Grok quota when local auth exists. A genuinely absent credential
/// omits the optional card; every present-credential failure stays typed.
pub(crate) async fn fetch(now: DateTime<Utc>) -> Result<Option<GrokData>, ProviderFetchFailure> {
    let credentials = match load_credentials() {
        Ok(Some(credentials)) => credentials,
        Ok(None) => return Ok(None),
        Err(_) => {
            return Err(ProviderFetchFailure::terminal(
                "Grok credentials could not be loaded.",
            ));
        }
    };
    fetch_with_credentials(credentials, now).await.map(Some)
}

async fn retry_grok_weekly_after_auth_with<Refresh, RefreshFuture, Retry, RetryFuture>(
    credentials: GrokCredentials,
    refresh: Refresh,
    retry: Retry,
) -> Result<
    (
        GrokCredentials,
        AccountScope,
        Option<ProviderCacheBinding>,
        String,
    ),
    ProviderFetchFailure,
>
where
    Refresh: FnOnce(PathBuf, String) -> RefreshFuture,
    RefreshFuture: std::future::Future<
        Output = Result<
            (GrokCredentials, AccountScope, Option<ProviderCacheBinding>),
            ProviderFetchFailure,
        >,
    >,
    Retry: FnOnce(GrokCredentials, Option<ProviderCacheBinding>) -> RetryFuture,
    RetryFuture: std::future::Future<Output = Result<String, ProviderFetchFailure>>,
{
    if credentials.scope_marker().is_none() {
        return Err(ProviderFetchFailure::terminal(
            "Grok OAuth token expired or invalid. Run `grok` to log in again.",
        ));
    }
    let (refreshed, scope, binding) =
        refresh(credentials.auth_path.clone(), credentials.entry_key.clone()).await?;
    let weekly_body = retry(refreshed.clone(), binding.clone()).await?;
    Ok((refreshed, scope, binding, weekly_body))
}

async fn fetch_with_credentials(
    credentials: GrokCredentials,
    now: DateTime<Utc>,
) -> Result<GrokData, ProviderFetchFailure> {
    let verified = if credentials_needs_refresh(&credentials, now) {
        refresh_credentials(&credentials.auth_path, &credentials.entry_key, false).await
    } else {
        credentials
            .resolve_account_scope()
            .map(|account_scope| {
                let cache_binding = ProviderCacheBinding::primary(account_scope.clone());
                (credentials, account_scope, Some(cache_binding))
            })
            .map_err(|_| {
                ProviderFetchFailure::terminal("Grok account identity could not be verified.")
            })
    };

    let (mut credentials, mut account_scope, mut cache_binding, client, response) =
        request_after_verified_binding(
            verified,
            |(credentials, account_scope, cache_binding)| async move {
                let client = provider_http_client_builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .map_err(|_| {
                        ProviderFetchFailure::terminal("Grok billing client could not be created.")
                    })?;
                let response = client
                    .get(GROK_CREDITS_URL)
                    .bearer_auth(&credentials.access_token)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .header(reqwest::header::USER_AGENT, "TokenBar")
                    .send()
                    .await
                    .map_err(|error| {
                        ProviderFetchFailure::from_send_error(
                            "Grok weekly billing request failed. Retrying automatically.",
                            cache_binding.clone(),
                            &error,
                        )
                    })?;
                Ok((credentials, account_scope, cache_binding, client, response))
            },
        )
        .await?;
    let status = response.status().as_u16();

    let weekly_body = if matches!(status, 401 | 403) {
        let retry_client = client.clone();
        let (refreshed, scope, binding, body) = retry_grok_weekly_after_auth_with(
            credentials,
            |auth_path, entry_key| async move {
                refresh_credentials(&auth_path, &entry_key, true).await
            },
            move |refreshed, binding| async move {
                let retry = retry_client
                    .get(GROK_CREDITS_URL)
                    .bearer_auth(&refreshed.access_token)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .header(reqwest::header::USER_AGENT, "TokenBar")
                    .send()
                    .await
                    .map_err(|error| {
                        ProviderFetchFailure::from_send_error(
                            "Grok weekly billing retry failed. Retrying automatically.",
                            binding.clone(),
                            &error,
                        )
                    })?;
                let retry_status = retry.status().as_u16();
                read_response_body(retry_status, false, || async {
                    retry.text().await.map_err(|error| {
                        TransportErrorFacts::from_reqwest(&error, TransportPhase::ResponseBody)
                    })
                })
                .await
                .map_err(|failure| grok_response_failure(failure, binding))
            },
        )
        .await?;
        credentials = refreshed;
        account_scope = scope;
        cache_binding = binding;
        body
    } else {
        read_response_body(status, false, || async {
            response.text().await.map_err(|error| {
                TransportErrorFacts::from_reqwest(&error, TransportPhase::ResponseBody)
            })
        })
        .await
        .map_err(|failure| grok_response_failure(failure, cache_binding.clone()))?
    };

    let monthly_body = fetch_monthly_best_effort(&credentials).await;
    build_grok_data(
        &weekly_body,
        monthly_body.as_deref(),
        &credentials,
        now,
        Ok(account_scope),
        cache_binding,
    )
}

fn grok_response_failure(
    failure: ResponseReadFailure,
    attempt_binding: Option<ProviderCacheBinding>,
) -> ProviderFetchFailure {
    match failure {
        ResponseReadFailure::Transient(diagnostic) => ProviderFetchFailure::transient(
            "Grok weekly billing request failed. Retrying automatically.",
            attempt_binding,
            diagnostic,
        ),
        ResponseReadFailure::Terminal(401 | 403) => ProviderFetchFailure::terminal(
            "Grok OAuth token expired or invalid. Run `grok` to log in again.",
        ),
        ResponseReadFailure::Terminal(status) => ProviderFetchFailure::terminal(format!(
            "Grok billing API rejected the request (status {status})."
        )),
    }
}

/// GET the monthly default billing view. Never refreshes credentials on 4xx —
/// the weekly call already owns token repair — and uses a short timeout so a
/// hung additive request cannot stall the joined quota payload.
async fn fetch_monthly_best_effort(credentials: &GrokCredentials) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(MONTHLY_TIMEOUT_SECS))
        .build()
        .ok()?;
    let response = client
        .get(GROK_MONTHLY_URL)
        .bearer_auth(&credentials.access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, "TokenBar")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.text().await.ok()
}

/// Assemble the card's windows from the weekly credits view (required) and the
/// monthly unified view (additive only). Success requires a weekly window —
/// monthly-only is not enough to show the card, so a missing/unusable weekly
/// meter errors even when monthly parsed cleanly.
fn build_grok_data(
    credits_body: &str,
    monthly_body: Option<&str>,
    credentials: &GrokCredentials,
    now: DateTime<Utc>,
    account_scope: Result<AccountScope, AccountScopeError>,
    cache_binding: Option<ProviderCacheBinding>,
) -> Result<GrokData, ProviderFetchFailure> {
    let credits: BillingResponse = serde_json::from_str(credits_body).map_err(|_| {
        ProviderFetchFailure::terminal("Grok billing response could not be decoded.")
    })?;

    let weekly = credits
        .config
        .as_ref()
        .and_then(|config| weekly_window(config, now))
        .ok_or_else(|| {
            ProviderFetchFailure::terminal("Grok billing response had no usable weekly usage.")
        })?;

    let mut windows = vec![weekly];
    if let Some(body) = monthly_body {
        if let Ok(monthly) = serde_json::from_str::<BillingResponse>(body) {
            if let Some(config) = monthly.config.as_ref() {
                if let Some(window) = monthly_window(config, now) {
                    windows.push(window);
                }
            }
        }
    }

    Ok(GrokData {
        identity: Some(AgentIdentity {
            email: credentials.email.clone(),
            plan: credits
                .subscription_tiers
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string()),
        }),
        account_scope,
        cache_binding,
        windows,
    })
}

/// Weekly SuperGrok credits window. When the credits view reports a valid
/// percent, use it. Empty-week 0% is allowed only when percent fields are truly
/// *absent* and a self-contained weekly `currentPeriod` proves the meter exists
/// (fresh reset, no usage yet). Present-but-unparsable or out-of-range percent
/// is a failed reading — never synthesize 0% for bad data. A malformed, partial,
/// or non-weekly period with no usable percent is "unknown", not zero.
fn weekly_window(config: &BillingConfig, now: DateTime<Utc>) -> Option<UsageWindow> {
    let used_percent = match weekly_used_percent(config) {
        WeeklyPercent::Value(pct) => pct,
        // Absent percent is a genuine 0% ONLY when a self-contained weekly
        // `currentPeriod` proves the meter exists: the weekly type AND a
        // positive start→end window must come from that same object, not a mix
        // of a weekly-typed period and unrelated flat billing dates.
        WeeklyPercent::Absent if self_contained_weekly_period(config).is_some() => 0.0,
        WeeklyPercent::Absent | WeeklyPercent::Invalid => return None,
    };
    let period = period_bounds(config);
    let mut window = UsageWindow::from_provider_used_percent(
        "Weekly".to_string(),
        used_percent,
        period.end,
        now,
    )
    .with_identity(
        WEEKLY_WINDOW_KEY,
        Some(WEEKLY_PACE_SERIES_KEY.to_string()),
        period.duration,
        None,
    );
    if period.invalid_evidence {
        window.unavailable("invalidEvidence");
    }
    Some(window)
}

/// The positive window length of a self-contained weekly `currentPeriod` — the
/// exact weekly type plus a `start < end` pair parsed from that same object —
/// or `None`. This is the "the weekly meter exists" signal that lets an absent
/// percent be read as 0%; it deliberately does not consult the flat
/// `billingPeriod*` fields, so a partial or non-weekly period cannot borrow an
/// unrelated window to masquerade as a weekly meter.
fn self_contained_weekly_period(config: &BillingConfig) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let period = config.current_period.as_ref()?;
    if period.period_type.as_deref() != Some(WEEKLY_PERIOD_TYPE) {
        return None;
    }
    let start = period.start.as_deref().and_then(parse_timestamp)?;
    let end = period.end.as_deref().and_then(parse_timestamp)?;
    (end > start).then_some((start, end))
}

/// Result of reading weekly percent fields. Distinguishes true omission (empty
/// week may be 0%) from a present-but-bad value (must not become 0%).
#[derive(Debug, Clone, Copy, PartialEq)]
enum WeeklyPercent {
    Value(f64),
    Absent,
    Invalid,
}

/// `creditUsagePercent` is the only field that can answer this window. A
/// present but unparsable/out-of-range value is `Invalid`, not `Absent`.
///
/// `creditUsagePercent` is the weekly credit *pool* — the meter that decides
/// when requests start being refused. `productUsage[]` decomposes that same
/// pool per product (`GrokBuild`, `GrokChat`, `Api`, ...), so a product row is
/// at most the pool figure and is under it by whatever the other products
/// spent. Reading `GrokBuild` as the window therefore under-reported depletion
/// for anyone who also used Grok outside the CLI, and said "4% left" against an
/// exhausted pool (issue #240).
///
/// That is why no product row is read as a *value*, not even as a fallback: a
/// share of the pool is not a reading of the pool, and one recorded under the
/// weekly pace series would splice a second quantity into it the moment a later
/// refresh restored the pool field.
///
/// A product row can still *refute*. Absent percent fields are read as an empty
/// week further down, and since the pool is at least as full as any single
/// product's share, a product reporting usage proves the week is not empty. A
/// present `usagePercent` that is not a parsable zero therefore fails the
/// reading closed rather than letting a fabricated 0% through.
fn weekly_used_percent(config: &BillingConfig) -> WeeklyPercent {
    if let Some(raw) = config.credit_usage_percent.as_deref() {
        return match valid_percentage(raw) {
            Some(pct) => WeeklyPercent::Value(pct),
            None => WeeklyPercent::Invalid,
        };
    }
    let usage_seen = config.product_usage.iter().flatten().any(|product| {
        product
            .usage_percent
            .as_deref()
            .is_some_and(|raw| valid_percentage(raw) != Some(0.0))
    });
    if usage_seen {
        WeeklyPercent::Invalid
    } else {
        WeeklyPercent::Absent
    }
}

/// Monthly included-allowance window: percent = used / monthlyLimit. Only
/// emitted when the consumed amount is *explicitly present* and non-negative —
/// xAI reports a genuine zero as `{ "val": 0 }` (the `history` cycles do
/// exactly this), so an absent `used`/`usage.totalUsed` means "unknown", not
/// zero. A negative consumption is invalid meter data and must not clamp into
/// a healthy 0%-used window.
fn monthly_window(config: &BillingConfig, now: DateTime<Utc>) -> Option<UsageWindow> {
    let limit = config
        .monthly_limit
        .as_ref()
        .and_then(|c| c.val)
        .filter(|v| *v > 0)?;
    let used = config
        .used
        .as_ref()
        .and_then(|c| c.val)
        .or_else(|| {
            config
                .usage
                .as_ref()
                .and_then(|u| u.total_used.as_ref())
                .and_then(|c| c.val)
        })
        .filter(|v| *v >= 0)?;
    let used_percent = (used as f64 / limit as f64 * 100.0).clamp(0.0, 100.0);
    let period = period_bounds(config);
    let mut window = UsageWindow::from_provider_used_percent(
        "Monthly".to_string(),
        used_percent,
        period.end,
        now,
    )
    .with_identity(
        MONTHLY_WINDOW_KEY,
        Some(MONTHLY_WINDOW_KEY.to_string()),
        period.duration,
        None,
    );
    if period.invalid_evidence {
        window.unavailable("invalidEvidence");
    }
    Some(window)
}

fn valid_percentage(raw: &RawValue) -> Option<f64> {
    serde_json::from_str::<f64>(raw.get())
        .ok()
        .filter(|pct| pct.is_finite() && (0.0..=100.0).contains(pct))
}

struct PeriodMeta {
    end: Option<DateTime<Utc>>,
    duration: Option<DurationEvidence>,
    invalid_evidence: bool,
}

/// Reset instant and duration evidence from a config's period. Prefers the
/// explicit `currentPeriod`, falling back to the flat `billingPeriodStart/End`.
/// Identity (label / window_key) is fixed per meter and is not derived here.
fn period_bounds(config: &BillingConfig) -> PeriodMeta {
    let period = config.current_period.as_ref();
    let start_raw = period
        .and_then(|period| period.start.as_deref())
        .or(config.billing_period_start.as_deref());
    let end_raw = period
        .and_then(|period| period.end.as_deref())
        .or(config.billing_period_end.as_deref());
    let start = start_raw.and_then(parse_timestamp);
    let end = end_raw.and_then(parse_timestamp);
    let (duration, invalid_evidence) = match (start_raw, end_raw, start, end) {
        (Some(_), Some(_), Some(start), Some(end)) if end > start => (
            Some(DurationEvidence::provider(
                end.timestamp(),
                (end - start).num_seconds(),
            )),
            false,
        ),
        (Some(_), Some(_), Some(_), Some(_)) => (None, true),
        (Some(_), Some(_), _, _) => (None, true),
        (Some(_), None, _, _) => (None, false),
        (None, Some(_), _, Some(_)) => (None, false),
        (None, Some(_), _, None) => (None, true),
        _ => (None, false),
    };
    PeriodMeta {
        end,
        duration,
        invalid_evidence,
    }
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            // Accept trailing "Z" variants chrono sometimes chokes on without offset parse.
            chrono::DateTime::parse_from_str(value.trim(), "%Y-%m-%dT%H:%M:%S%.f%z")
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        })
}

fn credentials_needs_refresh(credentials: &GrokCredentials, now: DateTime<Utc>) -> bool {
    if credentials.access_token.trim().is_empty() {
        return true;
    }
    match credentials.expires_at {
        Some(exp) => exp <= now + chrono::Duration::seconds(ACCESS_SKEW_SECS),
        None => false, // unknown expiry — try current token first
    }
}

async fn refresh_credentials(
    auth_path: &Path,
    entry_key: &str,
    force: bool,
) -> Result<(GrokCredentials, AccountScope, Option<ProviderCacheBinding>), ProviderFetchFailure> {
    let refresh = agent_account_scope::begin_refresh("grok").map_err(|_| {
        ProviderFetchFailure::terminal("Grok credential refresh lock is unavailable.")
    })?;
    refresh_credentials_with(
        auth_path,
        entry_key,
        force,
        &refresh,
        request_refresh,
        validate_refresh_target,
        save_credentials,
        |_| Ok(()),
    )
    .await
}

async fn request_refresh(
    refresh_token: String,
    client_id: String,
    attempt_binding: ProviderCacheBinding,
) -> Result<TokenResponse, ProviderFetchFailure> {
    let client = provider_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| ProviderFetchFailure::terminal("Grok refresh client could not be created."))?;
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", client_id.as_str()),
    ]
    .iter()
    .map(|(k, v)| {
        format!(
            "{}={}",
            crate::agent_usage::percent_encode(k),
            crate::agent_usage::percent_encode(v)
        )
    })
    .collect::<Vec<_>>()
    .join("&");

    let response = client
        .post(GROK_TOKEN_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, "TokenBar")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form)
        .send()
        .await
        .map_err(|error| {
            ProviderFetchFailure::from_send_error(
                "Grok token refresh failed. Retrying automatically.",
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
            "Grok token refresh failed. Retrying automatically.",
            Some(attempt_binding),
            diagnostic,
        ),
        ResponseReadFailure::Terminal(_) => ProviderFetchFailure::terminal(
            "Grok token refresh was rejected. Run `grok` to log in again.",
        ),
    })?;

    serde_json::from_str(&body).map_err(|_| {
        ProviderFetchFailure::terminal("Grok token refresh response could not be decoded.")
    })
}

async fn refresh_credentials_with<R, Request, RequestFuture, Preflight, Save, Checkpoint>(
    auth_path: &Path,
    entry_key: &str,
    force: bool,
    refresh: &R,
    request: Request,
    preflight: Preflight,
    save: Save,
    mut checkpoint: Checkpoint,
) -> Result<(GrokCredentials, AccountScope, Option<ProviderCacheBinding>), ProviderFetchFailure>
where
    R: RefreshScopeTransaction + ?Sized,
    Request: FnOnce(String, String, ProviderCacheBinding) -> RequestFuture,
    RequestFuture: std::future::Future<Output = Result<TokenResponse, ProviderFetchFailure>>,
    Preflight: FnOnce(&GrokCredentials) -> Result<(), RefreshTargetError>,
    Save: FnOnce(&GrokCredentials) -> Result<(), RefreshTargetError>,
    Checkpoint: FnMut(RefreshCheckpoint) -> Result<(), ProviderFetchFailure>,
{
    let mut credentials = load_credentials_entry_from(auth_path, Some(entry_key))
        .map_err(|_| ProviderFetchFailure::terminal("Grok credentials could not be reloaded."))?
        .ok_or_else(|| {
            ProviderFetchFailure::terminal("Grok auth entry disappeared during refresh.")
        })?;
    checkpoint(RefreshCheckpoint::Reloaded)?;
    let needs_refresh = force || credentials_needs_refresh(&credentials, Utc::now());
    let old_marker = credentials
        .scope_marker()
        .ok_or_else(|| {
            ProviderFetchFailure::terminal("Grok OAuth credential has no trusted refresh token.")
        })?
        .to_string();
    if needs_refresh && credentials.client_id.trim().is_empty() {
        return Err(ProviderFetchFailure::terminal(
            "Grok auth.json is missing oidc_client_id.",
        ));
    }
    let location = credentials
        .scope_location()
        .map_err(|_| ProviderFetchFailure::terminal("Grok auth location could not be verified."))?;
    let pre_scope = refresh
        .resolve_current("grok-auth-json", &location, old_marker.as_bytes())
        .map_err(|_| {
            ProviderFetchFailure::terminal("Grok account identity could not be verified.")
        })?;
    let pre_binding = ProviderCacheBinding::primary(pre_scope.clone());
    if !needs_refresh {
        return Ok((credentials, pre_scope, Some(pre_binding)));
    }

    let tokens = request(
        old_marker.clone(),
        credentials.client_id.clone(),
        pre_binding,
    )
    .await?;
    checkpoint(RefreshCheckpoint::NetworkReturned)?;
    let access_token = tokens.access_token.trim();
    if access_token.is_empty() {
        return Err(ProviderFetchFailure::terminal(
            "Grok token refresh response had no access token.",
        ));
    }
    credentials.access_token = access_token.to_string();
    if let Some(refresh_token) = tokens
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        credentials.refresh_token = refresh_token.to_string();
    }
    if let Some(expires_in) = tokens.expires_in {
        credentials.expires_at = Some(Utc::now() + chrono::Duration::seconds(expires_in.max(0)));
    }
    let new_marker = credentials
        .scope_marker()
        .ok_or_else(|| {
            ProviderFetchFailure::terminal(
                "Grok refreshed credential has no trusted refresh token.",
            )
        })?
        .to_string();
    preflight(&credentials).map_err(grok_refresh_target_failure)?;
    let scope = refresh
        .transfer(
            "grok-auth-json",
            &location,
            old_marker.as_bytes(),
            new_marker.as_bytes(),
        )
        .map_err(|_| {
            ProviderFetchFailure::terminal("Grok credential lineage could not be preserved.")
        })?;
    checkpoint(RefreshCheckpoint::MetadataHandled)?;

    let save_result = save(&credentials);
    checkpoint(RefreshCheckpoint::CredentialsPersisted)?;
    let cache_binding = match save_result {
        Ok(()) => Some(ProviderCacheBinding::primary(
            refresh
                .resolve_current("grok-auth-json", &location, new_marker.as_bytes())
                .map_err(|_| {
                    ProviderFetchFailure::terminal(
                        "Grok account identity could not be verified after refresh.",
                    )
                })?,
        )),
        Err(error) if error.is_persistence() => None,
        Err(error) => return Err(grok_refresh_target_failure(error)),
    };

    Ok((credentials, scope, cache_binding))
}

fn grok_refresh_target_failure(error: RefreshTargetError) -> ProviderFetchFailure {
    let display = match error {
        RefreshTargetError::TargetMissing => "Grok credential target disappeared during refresh.",
        RefreshTargetError::TargetChanged => "Grok credential target changed during refresh.",
        RefreshTargetError::TargetMalformed => {
            "Grok credential target became malformed during refresh."
        }
        RefreshTargetError::TargetUnverified => {
            "Grok credential target could not be verified during refresh."
        }
        RefreshTargetError::Persistence => "Grok refreshed credential could not be persisted.",
    };
    ProviderFetchFailure::terminal(display)
}

fn load_credentials() -> Result<Option<GrokCredentials>, String> {
    load_credentials_from(&grok_home().join("auth.json"))
}

fn load_credentials_from(auth_path: &Path) -> Result<Option<GrokCredentials>, String> {
    load_credentials_entry_from(auth_path, None)
}

fn credential_token(
    entry: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<String>, String> {
    match entry.get(field) {
        None => Ok(None),
        Some(Value::String(token)) => {
            let token = token.trim();
            Ok((!token.is_empty()).then(|| token.to_string()))
        }
        Some(_) => Err(format!("Grok auth entry {field} is not a string.")),
    }
}

fn load_credentials_entry_from(
    auth_path: &Path,
    expected_entry_key: Option<&str>,
) -> Result<Option<GrokCredentials>, String> {
    if !auth_path.is_file() {
        return Ok(None);
    }
    let data = fs::read(auth_path).map_err(|e| format!("read Grok auth.json: {e}"))?;
    let raw: Value =
        serde_json::from_slice(&data).map_err(|e| format!("parse Grok auth.json: {e}"))?;
    let map = raw
        .as_object()
        .ok_or_else(|| "Grok auth.json is not an object.".to_string())?;

    // Use ONLY the auth.x.ai OIDC entry Grok Build writes (keys look like
    // `https://auth.x.ai::<client_id>`). Never fall back to any other entry: a
    // sibling provider's bearer/refresh tokens would then be shipped to the Grok
    // billing endpoint and, on 401, POSTed to auth.x.ai/oauth2/token. Absent that
    // entry, treat it as no Grok auth on disk and omit the card silently — the
    // same stance as a missing auth.json.
    let selected = match expected_entry_key {
        Some(expected) if is_grok_auth_entry_key(expected) => map
            .get(expected)
            .map(|entry| (expected.to_string(), entry.clone())),
        Some(_) => None,
        None => map
            .iter()
            .find(|(key, _)| is_grok_auth_entry_key(key))
            .map(|(key, entry)| (key.clone(), entry.clone())),
    };
    let Some((entry_key, entry)) = selected else {
        return Ok(None);
    };

    let obj = entry
        .as_object()
        .ok_or_else(|| "Grok auth entry is not an object.".to_string())?;

    let access_token = credential_token(obj, "key")?;
    let refresh_token = credential_token(obj, "refresh_token")?;
    if access_token.is_none() && refresh_token.is_none() {
        return Ok(None);
    }

    let client_id = obj
        .get("oidc_client_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| client_id_from_entry_key(&entry_key))
        .unwrap_or_default();

    let email = obj
        .get("email")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let expires_at = obj
        .get("expires_at")
        .and_then(|v| v.as_str())
        .and_then(parse_timestamp);

    Ok(Some(GrokCredentials {
        auth_path: auth_path.to_path_buf(),
        entry_key,
        access_token: access_token.unwrap_or_default(),
        refresh_token: refresh_token.unwrap_or_default(),
        client_id,
        expires_at,
        email,
        raw_json: raw,
    }))
}

/// True only when `key` is a genuine Grok Build OIDC entry, keyed
/// `https://auth.x.ai::<client_id>`. The issuer segment (everything before the
/// first `::` separator) must equal `https://auth.x.ai` by byte equality — a
/// substring/prefix test would let a lookalike host like
/// `https://auth.x.ai.example.com::<id>` masquerade as the real issuer and get
/// its bearer/refresh tokens shipped to the Grok billing + token endpoints. A
/// key with no `::` separator has no client-id segment and is not the shape
/// Grok writes, so it is rejected too (fail-closed).
fn is_grok_auth_entry_key(key: &str) -> bool {
    matches!(key.split_once("::"), Some((issuer, _)) if issuer == "https://auth.x.ai")
}

fn client_id_from_entry_key(key: &str) -> Option<String> {
    // Keys look like: https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828
    key.rsplit("::")
        .next()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn read_grok_refresh_target(path: &Path) -> Result<Vec<u8>, RefreshTargetError> {
    fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RefreshTargetError::TargetMissing
        } else {
            RefreshTargetError::TargetUnverified
        }
    })
}

fn grok_current_target_root(
    credentials: &GrokCredentials,
    data: &[u8],
) -> Result<Value, RefreshTargetError> {
    let current_root: Value =
        serde_json::from_slice(data).map_err(|_| RefreshTargetError::TargetMalformed)?;
    let current_map = current_root
        .as_object()
        .ok_or(RefreshTargetError::TargetMalformed)?;
    let current_entry = current_map
        .get(&credentials.entry_key)
        .ok_or(RefreshTargetError::TargetMissing)?;
    let current_object = current_entry
        .as_object()
        .ok_or(RefreshTargetError::TargetMalformed)?;
    credential_token(current_object, "key").map_err(|_| RefreshTargetError::TargetMalformed)?;
    let refresh = credential_token(current_object, "refresh_token")
        .map_err(|_| RefreshTargetError::TargetMalformed)?;
    // The loader accepts refresh-only entries; this transaction supplies the
    // missing access token, so only the trusted refresh marker is required.
    if refresh.is_none() {
        return Err(RefreshTargetError::TargetMalformed);
    }
    let expected_entry = credentials
        .raw_json
        .as_object()
        .and_then(|map| map.get(&credentials.entry_key))
        .ok_or(RefreshTargetError::TargetUnverified)?;
    if current_entry != expected_entry {
        return Err(RefreshTargetError::TargetChanged);
    }
    Ok(current_root)
}

fn validate_refresh_target(credentials: &GrokCredentials) -> Result<(), RefreshTargetError> {
    let data = read_grok_refresh_target(&credentials.auth_path)?;
    grok_current_target_root(credentials, &data).map(|_| ())
}

fn encode_refreshed_grok_root(
    credentials: &GrokCredentials,
    mut current_root: Value,
) -> Result<Vec<u8>, RefreshTargetError> {
    let entry = current_root
        .as_object_mut()
        .and_then(|map| map.get_mut(&credentials.entry_key))
        .and_then(Value::as_object_mut)
        .ok_or(RefreshTargetError::TargetMalformed)?;
    entry.insert(
        "key".to_string(),
        Value::String(credentials.access_token.clone()),
    );
    entry.insert(
        "refresh_token".to_string(),
        Value::String(credentials.refresh_token.clone()),
    );
    if let Some(exp) = credentials.expires_at {
        entry.insert(
            "expires_at".to_string(),
            Value::String(exp.to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
    }
    serde_json::to_vec_pretty(&current_root).map_err(|_| RefreshTargetError::Persistence)
}

fn save_credentials(credentials: &GrokCredentials) -> Result<(), RefreshTargetError> {
    save_credentials_with(credentials, atomic_write)
}

fn save_credentials_with<Write>(
    credentials: &GrokCredentials,
    write: Write,
) -> Result<(), RefreshTargetError>
where
    Write: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
{
    let data = read_grok_refresh_target(&credentials.auth_path)?;
    let current_root = grok_current_target_root(credentials, &data)?;
    let encoded = encode_refreshed_grok_root(credentials, current_root)?;
    write(&credentials.auth_path, &encoded).map_err(|_| RefreshTargetError::Persistence)
}

fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("credentials path {} has no parent", path.display()),
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth.json");
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{file_name}.tokenbar.{}.{}",
        std::process::id(),
        seq
    ));

    let staged = (|| -> std::io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()
    })();
    if let Err(error) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| crate::user_home_dir().map(|home| home.join(".grok")))
        .unwrap_or_else(|| PathBuf::from(".grok"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_account_scope::test_support::TestRefreshScope;
    use crate::agent_usage::SafeTransportDiagnostic;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    /// The two real payloads captured from the live endpoint, side by side.
    const WEEKLY_CREDITS_BODY: &str = r#"{
        "config": {
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-07-15T00:00:00+00:00",
                "end": "2026-07-22T00:00:00+00:00"
            },
            "creditUsagePercent": 4.0,
            "productUsage": [ { "product": "GrokBuild", "usagePercent": 4.0 } ],
            "isUnifiedBillingUser": true,
            "billingPeriodStart": "2026-07-15T00:00:00+00:00",
            "billingPeriodEnd": "2026-07-22T00:00:00+00:00"
        },
        "subscriptionTiers": "X Premium+"
    }"#;
    const MONTHLY_BODY: &str = r#"{
        "config": {
            "monthlyLimit": { "val": 15000 },
            "used": { "val": 216 },
            "billingPeriodStart": "2026-07-01T00:00:00+00:00",
            "billingPeriodEnd": "2026-08-01T00:00:00+00:00"
        }
    }"#;
    /// The empty-week credits payload: period present, percent fields omitted.
    const EMPTY_WEEK_CREDITS_BODY: &str = r#"{
        "config": {
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-07-15T00:00:00+00:00",
                "end": "2026-07-22T00:00:00+00:00"
            },
            "onDemandCap": { "val": 0 },
            "isUnifiedBillingUser": true,
            "billingPeriodStart": "2026-07-15T00:00:00+00:00",
            "billingPeriodEnd": "2026-07-22T00:00:00+00:00"
        }
    }"#;

    fn test_credentials() -> GrokCredentials {
        GrokCredentials {
            auth_path: PathBuf::from("/tmp/unused"),
            entry_key: "k".into(),
            access_token: "t".into(),
            refresh_token: "r".into(),
            client_id: "c".into(),
            expires_at: None,
            email: Some("user@example.com".into()),
            raw_json: Value::Object(Default::default()),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-21T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn scope_none() -> Result<AccountScope, AccountScopeError> {
        Err(AccountScopeError::NoTrustedEvidence)
    }

    #[test]
    fn builds_both_weekly_and_monthly_windows() {
        let data = build_grok_data(
            WEEKLY_CREDITS_BODY,
            Some(MONTHLY_BODY),
            &test_credentials(),
            now(),
            scope_none(),
            None,
        )
        .unwrap();
        assert_eq!(data.windows.len(), 2);
        // [0] Weekly: 4% used -> 96% remaining, resets 2026-07-22.
        assert_eq!(data.windows[0].label_for_test(), "Weekly");
        assert_eq!(
            data.windows[0].pace_window_key_for_test(),
            Some("billing.weekly.v2")
        );
        assert!((data.windows[0].remaining_for_test() - 96.0).abs() < 0.01);
        // [1] Monthly: 216/15000 = 1.44% used -> 98.56% remaining, resets 2026-08-01.
        assert_eq!(data.windows[1].label_for_test(), "Monthly");
        assert_eq!(
            data.windows[1].pace_window_key_for_test(),
            Some("billing.monthly.v1")
        );
        assert!((data.windows[1].remaining_for_test() - 98.56).abs() < 0.01);
        assert_eq!(
            data.identity.as_ref().and_then(|i| i.email.as_deref()),
            Some("user@example.com")
        );
        assert_eq!(
            data.identity.as_ref().and_then(|i| i.plan.as_deref()),
            Some("X Premium+")
        );
    }

    #[test]
    fn empty_week_shows_zero_percent_weekly_not_error() {
        // THE original bug: a freshly reset week with no usage yet omits the
        // percent fields. With a self-contained weekly period still present,
        // that is 0% used, and the card must show a Weekly window rather than
        // erroring.
        let data = build_grok_data(
            EMPTY_WEEK_CREDITS_BODY,
            None,
            &test_credentials(),
            now(),
            scope_none(),
            None,
        )
        .unwrap();
        assert_eq!(data.windows.len(), 1);
        assert_eq!(data.windows[0].label_for_test(), "Weekly");
        assert!((data.windows[0].remaining_for_test() - 100.0).abs() < 0.01);
    }

    #[test]
    fn monthly_timeout_server_error_malformed_or_invalid_still_shows_weekly() {
        // Timeout and non-success status both collapse to None at the additive
        // fetch boundary; malformed or invalid success bodies are ignored here.
        for (label, monthly_body) in [
            ("timeout", None),
            ("server error", None),
            ("malformed body", Some("not json")),
            (
                "invalid meter",
                Some(r#"{ "config": { "monthlyLimit": { "val": 0 }, "used": { "val": 10 } } }"#),
            ),
        ] {
            let data = build_grok_data(
                WEEKLY_CREDITS_BODY,
                monthly_body,
                &test_credentials(),
                now(),
                scope_none(),
                None,
            )
            .unwrap();
            assert_eq!(data.windows.len(), 1, "{label}");
            assert_eq!(data.windows[0].label_for_test(), "Weekly", "{label}");
        }
    }

    #[test]
    fn no_usable_windows_is_an_error() {
        // Credits view with neither a percent nor a period, and no monthly data,
        // has nothing to show — surface an honest error, not an empty card.
        let data = build_grok_data(
            r#"{ "config": {} }"#,
            None,
            &test_credentials(),
            now(),
            scope_none(),
            None,
        );
        assert!(data.is_err());
    }

    #[test]
    fn monthly_only_without_weekly_is_an_error() {
        // Monthly is additive after weekly succeeds. A credits body that cannot
        // build a weekly window must not produce a monthly-only card.
        let err = match build_grok_data(
            r#"{ "config": {} }"#,
            Some(MONTHLY_BODY),
            &test_credentials(),
            now(),
            scope_none(),
            None,
        ) {
            Err(e) => e,
            Ok(_) => panic!("monthly-only must not succeed"),
        };
        let ProviderFetchFailure::Terminal { display } = err else {
            panic!("missing weekly meter must be terminal");
        };
        assert!(
            display.contains("weekly"),
            "error should name the missing weekly meter: {display}"
        );

        // Non-weekly period on the credits view is also insufficient, even with
        // a clean monthly body.
        let credits_monthly_period = r#"{
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_MONTHLY",
                    "start": "2026-07-01T00:00:00+00:00",
                    "end": "2026-08-01T00:00:00+00:00"
                }
            }
        }"#;
        assert!(build_grok_data(
            credits_monthly_period,
            Some(MONTHLY_BODY),
            &test_credentials(),
            now(),
            scope_none(),
            None,
        )
        .is_err());
    }

    #[test]
    fn prefers_overall_credit_percent_over_product_row() {
        let config: BillingConfig = serde_json::from_str(
            r#"{
                "creditUsagePercent": 50.0,
                "productUsage": [
                    { "product": "GrokChat", "usagePercent": 10.0 },
                    { "product": "GrokBuild", "usagePercent": 4.0 }
                ]
            }"#,
        )
        .unwrap();
        match weekly_used_percent(&config) {
            WeeklyPercent::Value(pct) => assert!((pct - 50.0).abs() < 0.01),
            other => panic!("expected Value(50.0), got {other:?}"),
        }
    }

    #[test]
    fn exhausted_pool_is_zero_remaining_not_the_product_share() {
        // Issue #240: the pool is spent, but the CLI's own share of it is 96%,
        // so reading the product row published "4% left" on a subscription that
        // was refusing requests.
        let credits = r#"{
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-15T00:00:00+00:00",
                    "end": "2026-07-22T00:00:00+00:00"
                },
                "creditUsagePercent": 100.0,
                "productUsage": [
                    { "product": "GrokChat", "usagePercent": 4.0 },
                    { "product": "GrokBuild", "usagePercent": 96.0 }
                ]
            }
        }"#;
        let data = build_grok_data(
            credits,
            None,
            &test_credentials(),
            now(),
            scope_none(),
            None,
        )
        .unwrap();
        assert_eq!(data.windows.len(), 1);
        assert!(
            data.windows[0].remaining_for_test().abs() < 0.01,
            "exhausted weekly pool must publish 0% remaining, got {}",
            data.windows[0].remaining_for_test()
        );
    }

    #[test]
    fn product_rows_refute_an_empty_week_but_never_supply_the_pool() {
        // No pool percent, but the rows show usage. The old fallback answered
        // 12.5 here, which would have spliced a product share into the pace
        // series the next time the pool field came back.
        let usage_seen: BillingConfig = serde_json::from_str(
            r#"{
                "productUsage": [
                    { "product": "GrokChat", "usagePercent": 10.0 },
                    { "product": "GrokBuild", "usagePercent": 12.5 }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(weekly_used_percent(&usage_seen), WeeklyPercent::Invalid);

        // A GrokBuild row alone is no more of a pool reading than the pair.
        let build_only: BillingConfig = serde_json::from_str(
            r#"{ "productUsage": [ { "product": "GrokBuild", "usagePercent": 12.5 } ] }"#,
        )
        .unwrap();
        assert_eq!(weekly_used_percent(&build_only), WeeklyPercent::Invalid);

        // Explicit zeros do not refute the empty week, so a freshly reset week
        // that itemizes its products still reads as 0% rather than erroring.
        let all_zero: BillingConfig = serde_json::from_str(
            r#"{
                "productUsage": [
                    { "product": "GrokChat", "usagePercent": 0.0 },
                    { "product": "GrokBuild", "usagePercent": 0 }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(weekly_used_percent(&all_zero), WeeklyPercent::Absent);
    }

    #[test]
    fn rejects_invalid_usage_percentages_before_wire() {
        let invalid_credit: BillingConfig = serde_json::from_str(
            r#"{
                "creditUsagePercent": 150.0,
                "productUsage": [
                    { "product": "GrokBuild", "usagePercent": 12.5 }
                ]
            }"#,
        )
        .unwrap();
        // Present-but-out-of-range pool percent fails that field; do not fall back.
        assert_eq!(weekly_used_percent(&invalid_credit), WeeklyPercent::Invalid);

        for invalid in ["1e400", r#""NaN""#] {
            let malformed_credit: BillingConfig = serde_json::from_str(&format!(
                r#"{{
                    "creditUsagePercent": {invalid},
                    "productUsage": [
                        {{ "product": "GrokBuild", "usagePercent": 12.5 }}
                    ]
                }}"#
            ))
            .unwrap();
            assert_eq!(
                weekly_used_percent(&malformed_credit),
                WeeklyPercent::Invalid
            );

            // With the pool percent absent, an unreadable product row is not a
            // parsable zero, so it refutes the empty week rather than passing
            // a fabricated 0% through.
            let malformed_product: BillingConfig = serde_json::from_str(&format!(
                r#"{{
                    "productUsage": [
                        {{ "product": "GrokBuild", "usagePercent": {invalid} }}
                    ]
                }}"#
            ))
            .unwrap();
            assert_eq!(
                weekly_used_percent(&malformed_product),
                WeeklyPercent::Invalid
            );
        }

        let invalid: BillingConfig = serde_json::from_str(
            r#"{
                "creditUsagePercent": -1.0,
                "productUsage": [
                    { "product": "GrokBuild", "usagePercent": 4.0 }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(weekly_used_percent(&invalid), WeeklyPercent::Invalid);

        // Overall credit present but invalid, with no GrokBuild percent key.
        let bad_credit: BillingConfig = serde_json::from_str(
            r#"{
                "creditUsagePercent": -1.0,
                "productUsage": [ { "product": "GrokBuild" } ]
            }"#,
        )
        .unwrap();
        assert_eq!(weekly_used_percent(&bad_credit), WeeklyPercent::Invalid);

        // Truly absent percent fields.
        let absent: BillingConfig =
            serde_json::from_str(r#"{ "productUsage": [ { "product": "GrokChat" } ] }"#).unwrap();
        assert_eq!(weekly_used_percent(&absent), WeeklyPercent::Absent);
    }

    #[test]
    fn malformed_weekly_percent_with_valid_period_is_not_zero() {
        // Invalid percent must not take the empty-week 0% path just because a
        // self-contained weekly period is present.
        let config: BillingConfig = serde_json::from_str(
            r#"{
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-15T00:00:00+00:00",
                    "end": "2026-07-22T00:00:00+00:00"
                },
                "creditUsagePercent": 150.0,
                "productUsage": [
                    { "product": "GrokBuild", "usagePercent": "NaN" }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(weekly_used_percent(&config), WeeklyPercent::Invalid);
        assert!(
            weekly_window(&config, now()).is_none(),
            "invalid percent must not synthesize Weekly 0%"
        );

        // Card-level: invalid weekly + valid monthly still errors (weekly required).
        let credits = r#"{
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-15T00:00:00+00:00",
                    "end": "2026-07-22T00:00:00+00:00"
                },
                "creditUsagePercent": -5.0
            }
        }"#;
        assert!(build_grok_data(
            credits,
            Some(MONTHLY_BODY),
            &test_credentials(),
            now(),
            scope_none(),
            None,
        )
        .is_err());
    }

    #[test]
    fn maps_weekly_window_from_period() {
        let body = r#"{
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-07T15:40:06.727001+00:00",
                    "end": "2026-07-14T15:40:06.727001+00:00"
                },
                "creditUsagePercent": 4.0,
                "productUsage": [
                    { "product": "GrokBuild", "usagePercent": 4.0 }
                ],
                "billingPeriodEnd": "2026-07-14T15:40:06.727001+00:00"
            },
            "subscriptionTiers": "X Premium+"
        }"#;
        let credentials = test_credentials();
        assert_eq!(credentials.scope_marker(), Some("r"));
        let now = DateTime::parse_from_rfc3339("2026-07-11T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let data = build_grok_data(body, None, &credentials, now, scope_none(), None).unwrap();
        assert_eq!(data.windows.len(), 1);
        assert_eq!(data.windows[0].label_for_test(), "Weekly");
        assert_eq!(data.windows[0].window_minutes_for_test(), Some(10_080));
        assert_eq!(
            data.windows[0].pace_window_key_for_test(),
            Some("billing.weekly.v2")
        );
        assert!((data.windows[0].remaining_for_test() - 96.0).abs() < 0.01);
        assert_eq!(
            data.identity.as_ref().and_then(|i| i.email.as_deref()),
            Some("user@example.com")
        );
        assert_eq!(
            data.identity.as_ref().and_then(|i| i.plan.as_deref()),
            Some("X Premium+")
        );
    }

    #[test]
    fn weekly_window_rejects_malformed_or_nonweekly_period_without_percent() {
        // An empty period object is not a "meter exists" signal — no dates, no
        // type => unknown, not 0%.
        let config: BillingConfig = serde_json::from_str(r#"{ "currentPeriod": {} }"#).unwrap();
        assert!(weekly_window(&config, now()).is_none());

        // A weekly type but no parseable start/end window is still unknown.
        let config: BillingConfig =
            serde_json::from_str(r#"{ "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY" } }"#)
                .unwrap();
        assert!(weekly_window(&config, now()).is_none());

        // An explicit MONTHLY period with no percent must NOT be fabricated into
        // a Weekly 0%.
        let config: BillingConfig = serde_json::from_str(
            r#"{ "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_MONTHLY",
                    "start": "2026-07-01T00:00:00+00:00",
                    "end": "2026-08-01T00:00:00+00:00"
                } }"#,
        )
        .unwrap();
        assert!(weekly_window(&config, now()).is_none());

        // MIXED SOURCE: a weekly-typed period with NO dates of its own must not
        // borrow the flat billingPeriod* window to masquerade as a meter.
        let config: BillingConfig = serde_json::from_str(
            r#"{ "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY" },
                 "billingPeriodStart": "2026-07-15T00:00:00+00:00",
                 "billingPeriodEnd": "2026-07-22T00:00:00+00:00" }"#,
        )
        .unwrap();
        assert!(weekly_window(&config, now()).is_none());

        // SUBSTRING: types that merely CONTAIN "WEEKLY" must be rejected by the
        // exact-match gate, even with a valid self-contained window.
        for ty in ["USAGE_PERIOD_TYPE_BIWEEKLY", "USAGE_PERIOD_TYPE_NOT_WEEKLY"] {
            let config: BillingConfig = serde_json::from_str(&format!(
                r#"{{ "currentPeriod": {{
                        "type": "{ty}",
                        "start": "2026-07-15T00:00:00+00:00",
                        "end": "2026-07-22T00:00:00+00:00"
                    }} }}"#
            ))
            .unwrap();
            assert!(
                weekly_window(&config, now()).is_none(),
                "type {ty} must not be accepted as weekly"
            );
        }
    }

    #[test]
    fn monthly_window_prefers_flat_used_over_usage_total() {
        let config: BillingConfig = serde_json::from_str(
            r#"{ "monthlyLimit": { "val": 200 }, "used": { "val": 50 },
                 "usage": { "totalUsed": { "val": 999 } } }"#,
        )
        .unwrap();
        // 50/200 = 25% used -> 75% remaining.
        assert!((monthly_window(&config, now()).unwrap().remaining_for_test() - 75.0).abs() < 0.01);
    }

    #[test]
    fn monthly_window_falls_back_to_usage_total_used() {
        let config: BillingConfig = serde_json::from_str(
            r#"{ "monthlyLimit": { "val": 400 }, "usage": { "totalUsed": { "val": 100 } } }"#,
        )
        .unwrap();
        assert!((monthly_window(&config, now()).unwrap().remaining_for_test() - 75.0).abs() < 0.01);
    }

    #[test]
    fn monthly_window_explicit_zero_used_is_zero_percent() {
        // xAI reports a genuine zero as `{ "val": 0 }` -> 0% used, 100% remaining.
        let config: BillingConfig =
            serde_json::from_str(r#"{ "monthlyLimit": { "val": 15000 }, "used": { "val": 0 } }"#)
                .unwrap();
        assert!(
            (monthly_window(&config, now()).unwrap().remaining_for_test() - 100.0).abs() < 0.01
        );
    }

    #[test]
    fn monthly_window_without_explicit_used_is_none() {
        // A limit with NO explicit `used`/`totalUsed` is "unknown", not zero:
        // it must not fabricate a misleading 100%-remaining across the FFI.
        let config: BillingConfig =
            serde_json::from_str(r#"{ "monthlyLimit": { "val": 15000 } }"#).unwrap();
        assert!(monthly_window(&config, now()).is_none());
        // An empty `{ }` wrapper (val absent) is treated the same as absent.
        let config: BillingConfig = serde_json::from_str(
            r#"{ "monthlyLimit": { "val": 15000 }, "used": {}, "usage": {} }"#,
        )
        .unwrap();
        assert!(monthly_window(&config, now()).is_none());
    }

    #[test]
    fn monthly_window_rejects_negative_used() {
        // Negative consumption is invalid meter data — do not clamp to 0% healthy.
        let config: BillingConfig =
            serde_json::from_str(r#"{ "monthlyLimit": { "val": 15000 }, "used": { "val": -1 } }"#)
                .unwrap();
        assert!(monthly_window(&config, now()).is_none());

        let config: BillingConfig = serde_json::from_str(
            r#"{ "monthlyLimit": { "val": 15000 }, "usage": { "totalUsed": { "val": -10 } } }"#,
        )
        .unwrap();
        assert!(monthly_window(&config, now()).is_none());

        // Flat used wins over usage.totalUsed; a negative flat used still rejects
        // even when nested totalUsed is non-negative.
        let config: BillingConfig = serde_json::from_str(
            r#"{ "monthlyLimit": { "val": 200 }, "used": { "val": -5 },
                 "usage": { "totalUsed": { "val": 50 } } }"#,
        )
        .unwrap();
        assert!(monthly_window(&config, now()).is_none());
    }

    #[test]
    fn stage4_grok_period_routes_are_exact_and_fail_closed() {
        // Monthly duration routes: fixed Monthly identity from the monthly
        // meter, with provider duration from the period bounds.
        for (label, start, end, days) in [
            ("28-day", "2023-02-01T00:00:00Z", "2023-03-01T00:00:00Z", 28),
            ("29-day", "2024-02-01T00:00:00Z", "2024-03-01T00:00:00Z", 29),
            ("30-day", "2024-04-01T00:00:00Z", "2024-05-01T00:00:00Z", 30),
            ("31-day", "2024-05-01T00:00:00Z", "2024-06-01T00:00:00Z", 31),
        ] {
            let now = parse_timestamp(start).unwrap() + chrono::Duration::days(1);
            let config: BillingConfig = serde_json::from_value(serde_json::json!({
                "monthlyLimit": { "val": 100 },
                "used": { "val": 12 },
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_MONTHLY",
                    "start": start,
                    "end": end
                }
            }))
            .unwrap();
            let window = monthly_window(&config, now).unwrap();
            let wire = serde_json::to_value(&window).unwrap();
            assert_eq!(wire["cardId"], "billing.monthly.v1", "{label}");
            assert_eq!(
                wire["paceStatus"]["windowKey"], "billing.monthly.v1",
                "{label}"
            );
            assert_eq!(
                wire["paceStatus"]["durationSeconds"],
                days * 86_400,
                "{label}"
            );
            assert_eq!(wire["paceStatus"]["durationSource"], "provider", "{label}");
            assert_eq!(wire["paceStatus"]["state"], "learningHistory", "{label}");
        }

        // Weekly end-only (no start): still Weekly identity, learningDuration.
        let end_only: BillingConfig = serde_json::from_value(serde_json::json!({
            "creditUsagePercent": 12.0,
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "end": "2026-07-24T00:00:00Z"
            }
        }))
        .unwrap();
        let wire = serde_json::to_value(
            weekly_window(&end_only, parse_timestamp("2026-07-17T00:00:00Z").unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(wire["cardId"], "billing.weekly.v1");
        assert_eq!(wire["paceStatus"]["windowKey"], "billing.weekly.v2");
        assert_eq!(wire["paceStatus"]["state"], "learningDuration");
        assert!(wire["resetsAt"].as_str().is_some());
        assert!(wire["paceStatus"].get("durationSeconds").is_none());
        assert!(wire["paceStatus"].get("durationSource").is_none());

        // Credits meter is always Weekly identity even when currentPeriod type
        // is not weekly — the endpoint, not the period substring, owns the key.
        // Duration evidence still flows from the period bounds when valid.
        let daily_typed: BillingConfig = serde_json::from_value(serde_json::json!({
            "creditUsagePercent": 12.0,
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_DAILY",
                "start": "2026-07-17T00:00:00Z",
                "end": "2026-07-18T00:00:00Z"
            }
        }))
        .unwrap();
        let wire = serde_json::to_value(
            weekly_window(
                &daily_typed,
                parse_timestamp("2026-07-17T12:00:00Z").unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(wire["cardId"], "billing.weekly.v1");
        assert_eq!(wire["paceStatus"]["windowKey"], "billing.weekly.v2");
        assert_eq!(wire["paceStatus"]["durationSeconds"], 86_400);
        assert_eq!(wire["paceStatus"]["durationSource"], "provider");

        for (label, start, end) in [
            (
                "contradictory",
                "2026-07-18T00:00:00Z",
                "2026-07-17T00:00:00Z",
            ),
            ("malformed-start", "not-a-date", "2026-07-24T00:00:00Z"),
            ("malformed-end", "2026-07-17T00:00:00Z", "not-a-date"),
        ] {
            let config: BillingConfig = serde_json::from_value(serde_json::json!({
                "creditUsagePercent": 12.0,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": start,
                    "end": end
                }
            }))
            .unwrap();
            let window =
                weekly_window(&config, parse_timestamp("2026-07-17T12:00:00Z").unwrap()).unwrap();
            let wire = serde_json::to_value(&window).unwrap();
            assert_eq!(
                wire["paceStatus"]["windowKey"], "billing.weekly.v2",
                "{label}"
            );
            assert_eq!(wire["paceStatus"]["state"], "unavailable", "{label}");
            assert_eq!(wire["paceStatus"]["reason"], "invalidEvidence", "{label}");
            assert!(
                wire["paceStatus"].get("durationSeconds").is_none(),
                "{label}"
            );
        }
    }

    #[test]
    fn client_id_from_key() {
        assert_eq!(
            client_id_from_entry_key("https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828")
                .as_deref(),
            Some("b1a00492-073a-47ea-816f-4c329264a828")
        );
    }

    /// Write `contents` to a fresh temp `auth.json` and return (dir, path).
    fn temp_auth_json(tag: &str, contents: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "tb_grok_load_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn foreign_only_auth_json_yields_no_credentials() {
        // auth.json holding ONLY a sibling provider's entry (no auth.x.ai key)
        // must not become a request-bearing candidate: no card, and — crucially —
        // none of the foreign entry's tokens are surfaced anywhere.
        let (dir, path) = temp_auth_json(
            "foreign",
            r#"{
                "https://auth.openai.com::deadbeef": {
                    "key": "FAKE-FOREIGN-ACCESS",
                    "refresh_token": "FAKE-FOREIGN-REFRESH",
                    "oidc_client_id": "deadbeef"
                }
            }"#,
        );
        let loaded = load_credentials_from(&path).unwrap();
        assert!(
            loaded.is_none(),
            "a foreign-only auth.json must yield no Grok credentials, got {loaded:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn picks_auth_x_ai_entry_never_a_sibling() {
        // With both a sibling entry and the real auth.x.ai entry present, the
        // loader must select the auth.x.ai one and never read the sibling's
        // secrets into the request-bearing credentials.
        let (dir, path) = temp_auth_json(
            "mixed",
            r#"{
                "https://auth.openai.com::deadbeef": {
                    "key": "FAKE-FOREIGN-ACCESS",
                    "refresh_token": "FAKE-FOREIGN-REFRESH",
                    "oidc_client_id": "deadbeef"
                },
                "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {
                    "key": "FAKE-XAI-ACCESS",
                    "refresh_token": "FAKE-XAI-REFRESH"
                }
            }"#,
        );
        let creds = load_credentials_from(&path)
            .unwrap()
            .expect("auth.x.ai entry loads");
        assert!(creds.entry_key.contains("auth.x.ai"));
        assert_eq!(creds.access_token, "FAKE-XAI-ACCESS");
        assert_eq!(creds.refresh_token, "FAKE-XAI-REFRESH");
        // Client id derives from the auth.x.ai key, not the sibling's.
        assert_eq!(creds.client_id, "b1a00492-073a-47ea-816f-4c329264a828");
        // No field ever carries the sibling secrets.
        assert_ne!(creds.access_token, "FAKE-FOREIGN-ACCESS");
        assert_ne!(creds.refresh_token, "FAKE-FOREIGN-REFRESH");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_grok_auth_entry_key_requires_exact_issuer() {
        // The genuine Grok Build key shape.
        assert!(is_grok_auth_entry_key(
            "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828"
        ));
        // A lookalike subdomain must NOT match — a substring/prefix test would
        // have accepted it and shipped its tokens to the Grok endpoints.
        assert!(!is_grok_auth_entry_key(
            "https://auth.x.ai.example.com::b1a00492"
        ));
        assert!(!is_grok_auth_entry_key(
            "https://auth.x.ai.evil.example::deadbeef"
        ));
        // A foreign issuer and a shapeless key are rejected.
        assert!(!is_grok_auth_entry_key("https://auth.openai.com::deadbeef"));
        assert!(!is_grok_auth_entry_key("https://auth.x.ai"));
    }

    #[test]
    fn lookalike_only_auth_json_yields_no_credentials() {
        // A key whose issuer merely CONTAINS "auth.x.ai" (a lookalike host) must
        // not be selected: no card, and none of its tokens are read into the
        // request-bearing credentials. Fail-closed, exactly like a foreign entry.
        let (dir, path) = temp_auth_json(
            "lookalike",
            r#"{
                "https://auth.x.ai.example.com::deadbeef": {
                    "key": "FAKE-LOOKALIKE-ACCESS",
                    "refresh_token": "FAKE-LOOKALIKE-REFRESH",
                    "oidc_client_id": "deadbeef"
                }
            }"#,
        );
        let loaded = load_credentials_from(&path).unwrap();
        assert!(
            loaded.is_none(),
            "a lookalike-host auth.json must yield no Grok credentials, got {loaded:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn picks_real_issuer_over_lookalike_sibling() {
        // A lookalike entry sitting next to the genuine one must never win: the
        // loader selects the exact-issuer key and reads only its secrets.
        let (dir, path) = temp_auth_json(
            "lookalike_mixed",
            r#"{
                "https://auth.x.ai.example.com::deadbeef": {
                    "key": "FAKE-LOOKALIKE-ACCESS",
                    "refresh_token": "FAKE-LOOKALIKE-REFRESH",
                    "oidc_client_id": "deadbeef"
                },
                "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {
                    "key": "FAKE-XAI-ACCESS",
                    "refresh_token": "FAKE-XAI-REFRESH"
                }
            }"#,
        );
        let creds = load_credentials_from(&path)
            .unwrap()
            .expect("genuine auth.x.ai entry loads");
        assert_eq!(
            creds.entry_key,
            "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828"
        );
        assert_eq!(creds.access_token, "FAKE-XAI-ACCESS");
        assert_eq!(creds.refresh_token, "FAKE-XAI-REFRESH");
        assert_ne!(creds.access_token, "FAKE-LOOKALIKE-ACCESS");
        assert_ne!(creds.refresh_token, "FAKE-LOOKALIKE-REFRESH");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_credentials_surfaces_missing_entry_as_err() {
        // P2: a failed write-back must be an inspectable Err (the caller logs it
        // instead of swallowing it), not a silent success that drops a rotated
        // refresh token. An entry_key absent from raw_json is the deterministic
        // failure the seam exposes without touching the network.
        let creds = GrokCredentials {
            auth_path: PathBuf::from("/tmp/unused-grok-save"),
            entry_key: "https://auth.x.ai::missing".into(),
            access_token: "new-access".into(),
            refresh_token: "new-refresh".into(),
            client_id: "missing".into(),
            expires_at: None,
            email: None,
            raw_json: Value::Object(Default::default()),
        };
        let result = save_credentials(&creds);
        assert!(
            result.is_err(),
            "save_credentials must surface a failure as Err so the caller can log it"
        );
    }

    #[test]
    fn save_credentials_preserves_concurrent_login_and_sibling_changes() {
        let original = serde_json::json!({
            (TEST_ENTRY): {
                "key": "account-a-access",
                "refresh_token": "account-a-refresh",
                "oidc_client_id": "fixture-client"
            },
            "sibling": { "value": "before" }
        });
        let (dir, path) = temp_auth_json("save-race", &original.to_string());
        let mut credentials = load_credentials_from(&path).unwrap().unwrap();
        credentials.access_token = "account-a-refreshed-access".to_string();
        credentials.refresh_token = "account-a-refreshed-refresh".to_string();

        let switched = serde_json::json!({
            (TEST_ENTRY): {
                "key": "account-b-access",
                "refresh_token": "account-b-refresh",
                "oidc_client_id": "fixture-client"
            },
            "sibling": { "value": "after-switch" }
        });
        fs::write(&path, serde_json::to_vec_pretty(&switched).unwrap()).unwrap();
        assert!(save_credentials(&credentials).is_err());
        let after_switch: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after_switch, switched, "account B must not be overwritten");

        let sibling_changed = serde_json::json!({
            (TEST_ENTRY): original.get(TEST_ENTRY).unwrap().clone(),
            "sibling": { "value": "after-sibling-update" }
        });
        fs::write(&path, serde_json::to_vec_pretty(&sibling_changed).unwrap()).unwrap();
        save_credentials(&credentials).unwrap();
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["sibling"], sibling_changed["sibling"]);
        assert_eq!(saved[TEST_ENTRY]["key"], "account-a-refreshed-access");
        assert_eq!(
            saved[TEST_ENTRY]["refresh_token"],
            "account-a-refreshed-refresh"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "tb_grok_atomic_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(&dir);
    }

    const TEST_ENTRY: &str = "https://auth.x.ai::fixture-client";

    #[test]
    fn matching_entry_logout_remains_absent() {
        let (dir, path) = temp_auth_json(
            "logout",
            &serde_json::json!({
                (TEST_ENTRY): {
                    "key": "  ",
                    "refresh_token": "\t\n"
                }
            })
            .to_string(),
        );
        assert!(load_credentials_from(&path).unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_entry_tokens_are_trimmed_at_load() {
        let (dir, path) = temp_auth_json(
            "trimmed",
            &serde_json::json!({
                (TEST_ENTRY): {
                    "key": "  access-token\n",
                    "refresh_token": "\trefresh-token  "
                }
            })
            .to_string(),
        );
        let credentials = load_credentials_from(&path).unwrap().unwrap();
        assert_eq!(credentials.access_token, "access-token");
        assert_eq!(credentials.refresh_token, "refresh-token");
        assert_eq!(credentials.scope_marker(), Some("refresh-token"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_entry_non_string_tokens_are_terminal() {
        for (tag, field, value) in [
            ("object-access", "key", serde_json::json!({ "token": "x" })),
            ("number-access", "key", serde_json::json!(42)),
            (
                "object-refresh",
                "refresh_token",
                serde_json::json!({ "token": "x" }),
            ),
            ("number-refresh", "refresh_token", serde_json::json!(42)),
        ] {
            let mut auth = serde_json::json!({
                (TEST_ENTRY): {
                    "key": "access-token",
                    "refresh_token": "refresh-token"
                }
            });
            auth.get_mut(TEST_ENTRY)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(field.to_string(), value);
            let (dir, path) = temp_auth_json(tag, &auth.to_string());
            assert!(
                load_credentials_from(&path).is_err(),
                "{tag} must be terminal, not absent"
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn whitespace_refresh_token_is_not_a_scope_marker() {
        let mut credentials = test_credentials();
        credentials.refresh_token = " \t\r\n ".to_string();
        assert_eq!(credentials.scope_marker(), None);
        assert!(matches!(
            credentials.resolve_account_scope(),
            Err(AccountScopeError::NoTrustedEvidence)
        ));
    }

    use crate::agent_account_scope::test_support::{
        assert_refresh_crash_boundaries, assert_transient_used_lock_reloaded_binding, checkpoint_at,
    };

    #[tokio::test]
    async fn whitespace_refresh_evidence_never_binds_across_access_tokens() {
        let scope = TestRefreshScope::new("grok", "grok-whitespace-binding");
        scope
            .resolve_current("fixture", "baseline", b"baseline-marker")
            .unwrap();
        let before = scope.metadata_bytes();
        let path = scope.root().join("grok/auth.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        for access_token in ["account-a-access", "account-b-access"] {
            fs::write(
                &path,
                serde_json::to_vec_pretty(&serde_json::json!({
                    (TEST_ENTRY): {
                        "key": access_token,
                        "refresh_token": " \t ",
                        "oidc_client_id": "fixture-client",
                        "expires_at": "1970-01-01T00:00:00Z"
                    }
                }))
                .unwrap(),
            )
            .unwrap();
            let loaded = load_credentials_entry_from(&path, Some(TEST_ENTRY))
                .unwrap()
                .unwrap();
            assert_eq!(loaded.access_token, access_token);
            assert_eq!(loaded.scope_marker(), None);

            let send_count = Arc::new(AtomicUsize::new(0));
            let request_send_count = Arc::clone(&send_count);
            let failure = refresh_credentials_with(
                &path,
                TEST_ENTRY,
                true,
                &scope,
                move |_, _, _| async move {
                    request_send_count.fetch_add(1, Ordering::SeqCst);
                    Ok(TokenResponse {
                        access_token: "unexpected-access".to_string(),
                        refresh_token: Some("unexpected-refresh".to_string()),
                        expires_in: Some(3_600),
                    })
                },
                validate_refresh_target,
                save_credentials,
                checkpoint_at(None),
            )
            .await
            .unwrap_err();

            assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
            assert_eq!(send_count.load(Ordering::SeqCst), 0);
            assert_eq!(scope.metadata_bytes(), before);
        }
        scope.cleanup();
    }

    async fn grok_test_response(
        refresh_token: String,
        client_id: String,
        _attempt_binding: ProviderCacheBinding,
    ) -> Result<TokenResponse, ProviderFetchFailure> {
        assert_eq!(refresh_token, "grok-old-refresh");
        assert_eq!(client_id, "fixture-client");
        Ok(TokenResponse {
            access_token: "grok-new-access".to_string(),
            refresh_token: Some("grok-new-refresh".to_string()),
            expires_in: Some(3_600),
        })
    }

    fn setup_refresh(tag: &str) -> (TestRefreshScope, PathBuf, AccountScope, Vec<u8>, String) {
        let scope = TestRefreshScope::new("grok", tag);
        let path = scope.root().join("grok/auth.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                (TEST_ENTRY): {
                    "key": "grok-old-access",
                    "refresh_token": "grok-old-refresh",
                    "oidc_client_id": "fixture-client",
                    "expires_at": "1970-01-01T00:00:00Z",
                    "entry_sibling": {"keep": true}
                },
                "https://auth.openai.com::foreign": {"keep": "initial"},
                "root_sibling": {"writer": "initial", "revision": 1}
            }))
            .unwrap(),
        )
        .unwrap();
        let credentials = load_credentials_entry_from(&path, Some(TEST_ENTRY))
            .unwrap()
            .unwrap();
        let location = credentials.scope_location().unwrap();
        let old_scope = scope
            .resolve_current(
                "grok-auth-json",
                &location,
                credentials.scope_marker().unwrap().as_bytes(),
            )
            .unwrap();
        let metadata = scope.metadata_bytes();
        (scope, path, old_scope, metadata, location)
    }

    async fn run_refresh(
        scope: &TestRefreshScope,
        path: &Path,
        crash: Option<RefreshCheckpoint>,
    ) -> Result<(GrokCredentials, AccountScope, Option<ProviderCacheBinding>), ProviderFetchFailure>
    {
        refresh_credentials_with(
            path,
            TEST_ENTRY,
            true,
            scope,
            grok_test_response,
            validate_refresh_target,
            save_credentials,
            checkpoint_at(crash),
        )
        .await
    }

    fn stored_refresh_token(path: &Path) -> String {
        load_credentials_entry_from(path, Some(TEST_ENTRY))
            .unwrap()
            .unwrap()
            .refresh_token
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum GrokTargetCase {
        PreflightChanged,
        PreflightMissing,
        PreflightMalformed,
        PreflightUnverified,
        PostChanged,
        PostMissing,
        PostMalformed,
        PostUnverified,
        Persistence,
        Success,
        RefreshOnlySuccess,
    }

    const GROK_TARGET_CASES: [GrokTargetCase; 11] = [
        GrokTargetCase::PreflightChanged,
        GrokTargetCase::PreflightMissing,
        GrokTargetCase::PreflightMalformed,
        GrokTargetCase::PreflightUnverified,
        GrokTargetCase::PostChanged,
        GrokTargetCase::PostMissing,
        GrokTargetCase::PostMalformed,
        GrokTargetCase::PostUnverified,
        GrokTargetCase::Persistence,
        GrokTargetCase::Success,
        GrokTargetCase::RefreshOnlySuccess,
    ];

    impl GrokTargetCase {
        fn preflight_terminal(self) -> bool {
            matches!(
                self,
                Self::PreflightChanged
                    | Self::PreflightMissing
                    | Self::PreflightMalformed
                    | Self::PreflightUnverified
            )
        }

        fn post_terminal(self) -> bool {
            matches!(
                self,
                Self::PostChanged | Self::PostMissing | Self::PostMalformed | Self::PostUnverified
            )
        }

        fn continues(self) -> bool {
            matches!(
                self,
                Self::Persistence | Self::Success | Self::RefreshOnlySuccess
            )
        }

        fn cacheable(self) -> bool {
            matches!(self, Self::Success | Self::RefreshOnlySuccess)
        }

        fn expected_display(self) -> Option<&'static str> {
            match self {
                Self::PreflightMissing | Self::PostMissing => {
                    Some("Grok credential target disappeared during refresh.")
                }
                Self::PreflightChanged | Self::PostChanged => {
                    Some("Grok credential target changed during refresh.")
                }
                Self::PreflightMalformed | Self::PostMalformed => {
                    Some("Grok credential target became malformed during refresh.")
                }
                Self::PreflightUnverified | Self::PostUnverified => {
                    Some("Grok credential target could not be verified during refresh.")
                }
                Self::Persistence | Self::Success | Self::RefreshOnlySuccess => None,
            }
        }
    }

    struct RecordingGrokRefreshScope<'a> {
        inner: &'a TestRefreshScope,
        resolves: std::cell::Cell<usize>,
        transfers: std::cell::Cell<usize>,
    }

    impl RefreshScopeTransaction for RecordingGrokRefreshScope<'_> {
        fn resolve_current(
            &self,
            semantic_source: &str,
            canonical_location: &str,
            marker: &[u8],
        ) -> Result<AccountScope, AccountScopeError> {
            self.resolves.set(self.resolves.get() + 1);
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
            self.transfers.set(self.transfers.get() + 1);
            self.inner
                .transfer(semantic_source, canonical_location, old_marker, new_marker)
        }
    }

    #[derive(Clone, PartialEq, Eq)]
    enum GrokPathState {
        Missing,
        Directory,
        Bytes(Vec<u8>),
    }

    fn grok_path_state(path: &Path) -> GrokPathState {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => GrokPathState::Missing,
            Ok(metadata) if metadata.is_dir() => GrokPathState::Directory,
            Ok(_) => GrokPathState::Bytes(fs::read(path).expect("fixture file must be readable")),
            Err(_) => panic!("fixture path state could not be inspected"),
        }
    }

    fn write_grok_fixture(path: &Path, value: Value) {
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn apply_grok_target_case(path: &Path, case: GrokTargetCase) -> GrokPathState {
        match case {
            GrokTargetCase::PreflightChanged | GrokTargetCase::PostChanged => write_grok_fixture(
                path,
                serde_json::json!({
                    (TEST_ENTRY): {
                        "key": "account-b-access",
                        "refresh_token": "account-b-refresh",
                        "oidc_client_id": "fixture-client"
                    },
                    "https://auth.openai.com::foreign": {"keep": "changed"},
                    "root_sibling": {"writer": "b", "revision": 2}
                }),
            ),
            GrokTargetCase::PreflightMissing | GrokTargetCase::PostMissing => write_grok_fixture(
                path,
                serde_json::json!({
                    "https://auth.openai.com::foreign": {"keep": "present"},
                    "root_sibling": {"writer": "logout", "revision": 2}
                }),
            ),
            GrokTargetCase::PreflightMalformed => fs::write(path, b"[]").unwrap(),
            GrokTargetCase::PostMalformed => write_grok_fixture(
                path,
                serde_json::json!({
                    (TEST_ENTRY): {
                        "key": {"invalid": true},
                        "refresh_token": "grok-old-refresh"
                    },
                    "https://auth.openai.com::foreign": {"keep": "present"}
                }),
            ),
            GrokTargetCase::PreflightUnverified | GrokTargetCase::PostUnverified => {
                fs::remove_file(path).unwrap();
                fs::create_dir(path).unwrap();
            }
            GrokTargetCase::Success | GrokTargetCase::RefreshOnlySuccess => {
                let mut root: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                let root = root.as_object_mut().unwrap();
                root.insert(
                    "https://auth.openai.com::foreign".to_string(),
                    serde_json::json!({"keep": "concurrent"}),
                );
                root.insert(
                    "root_sibling".to_string(),
                    serde_json::json!({"writer": "grok-cli", "revision": 2}),
                );
                write_grok_fixture(path, Value::Object(root.clone()));
            }
            GrokTargetCase::Persistence => {}
        }
        grok_path_state(path)
    }

    #[derive(Default)]
    struct GrokTargetObservation {
        request: std::cell::Cell<usize>,
        preflight: std::cell::Cell<usize>,
        save: std::cell::Cell<usize>,
        writer: std::cell::Cell<usize>,
        retry: std::cell::Cell<usize>,
        retry_token: std::cell::Cell<bool>,
        retry_binding: std::cell::Cell<bool>,
        checkpoints: std::cell::RefCell<Vec<RefreshCheckpoint>>,
    }

    async fn run_grok_target_case(case: GrokTargetCase) {
        let (scope, path, old_scope, metadata_before, location) = setup_refresh("grok-target");
        if case == GrokTargetCase::RefreshOnlySuccess {
            let mut root: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            root[TEST_ENTRY].as_object_mut().unwrap().remove("key");
            write_grok_fixture(&path, root);
        }
        let outer = load_credentials_entry_from(&path, Some(TEST_ENTRY))
            .unwrap()
            .unwrap();
        let initial = grok_path_state(&path);
        let expected = std::rc::Rc::new(std::cell::RefCell::new(initial.clone()));
        let observation = std::rc::Rc::new(GrokTargetObservation::default());
        let recording = RecordingGrokRefreshScope {
            inner: &scope,
            resolves: std::cell::Cell::new(0),
            transfers: std::cell::Cell::new(0),
        };
        let expected_binding = case
            .cacheable()
            .then(|| ProviderCacheBinding::primary(old_scope.clone()));
        let request_path = path.clone();
        let preflight_path = path.clone();
        let request_expected = std::rc::Rc::clone(&expected);
        let preflight_expected = std::rc::Rc::clone(&expected);
        let request_observation = std::rc::Rc::clone(&observation);
        let preflight_observation = std::rc::Rc::clone(&observation);
        let save_observation = std::rc::Rc::clone(&observation);
        let checkpoint_observation = std::rc::Rc::clone(&observation);
        let retry_observation = std::rc::Rc::clone(&observation);
        let retry_binding = expected_binding.clone();
        let transaction_scope = &recording;

        let result = retry_grok_weekly_after_auth_with(
            outer,
            move |auth_path, entry_key| async move {
                refresh_credentials_with(
                    &auth_path,
                    &entry_key,
                    true,
                    transaction_scope,
                    move |refresh_token, client_id, _| async move {
                        request_observation
                            .request
                            .set(request_observation.request.get() + 1);
                        assert!(
                            refresh_token == "grok-old-refresh" && client_id == "fixture-client",
                            "REFRESH-TARGET-CHECKPOINT-SEQUENCE"
                        );
                        if case.preflight_terminal() {
                            *request_expected.borrow_mut() =
                                apply_grok_target_case(&request_path, case);
                        }
                        Ok(TokenResponse {
                            access_token: "grok-new-access".to_string(),
                            refresh_token: Some("grok-new-refresh".to_string()),
                            expires_in: Some(3_600),
                        })
                    },
                    move |credentials| {
                        preflight_observation
                            .preflight
                            .set(preflight_observation.preflight.get() + 1);
                        let result = validate_refresh_target(credentials);
                        if result.is_ok() && (case.post_terminal() || case.cacheable()) {
                            *preflight_expected.borrow_mut() =
                                apply_grok_target_case(&preflight_path, case);
                        }
                        result
                    },
                    move |credentials| {
                        save_observation.save.set(save_observation.save.get() + 1);
                        let writer_observation = std::rc::Rc::clone(&save_observation);
                        save_credentials_with(credentials, move |path, data| {
                            writer_observation
                                .writer
                                .set(writer_observation.writer.get() + 1);
                            if case == GrokTargetCase::Persistence {
                                Err(std::io::Error::other("injected writer failure"))
                            } else {
                                atomic_write(path, data)
                            }
                        })
                    },
                    move |checkpoint| {
                        checkpoint_observation
                            .checkpoints
                            .borrow_mut()
                            .push(checkpoint);
                        Ok(())
                    },
                )
                .await
            },
            move |credentials, binding| async move {
                retry_observation
                    .retry
                    .set(retry_observation.retry.get() + 1);
                retry_observation
                    .retry_token
                    .set(credentials.access_token == "grok-new-access");
                retry_observation
                    .retry_binding
                    .set(binding == retry_binding);
                Ok(WEEKLY_CREDITS_BODY.to_string())
            },
        )
        .await;

        let terminal_label = if case.preflight_terminal() {
            "REFRESH-TARGET-PREFLIGHT-TERMINAL"
        } else {
            "REFRESH-TARGET-POST-PREFLIGHT-TERMINAL"
        };
        if case == GrokTargetCase::RefreshOnlySuccess {
            assert!(result.is_ok(), "REFRESH-TARGET-REFRESH-ONLY");
        }
        if let Some(display) = case.expected_display() {
            assert!(
                matches!(&result, Err(ProviderFetchFailure::Terminal { display: actual }) if actual == display),
                "{terminal_label}"
            );
        } else {
            let (credentials, returned_scope, binding, body) = result.unwrap();
            assert!(
                credentials.access_token == "grok-new-access"
                    && returned_scope == old_scope
                    && binding == expected_binding
                    && body == WEEKLY_CREDITS_BODY,
                "REFRESH-TARGET-PERSISTENCE-FAILSOFT"
            );
        }
        assert_eq!(
            observation.request.get(),
            1,
            "REFRESH-TARGET-CHECKPOINT-SEQUENCE"
        );
        assert_eq!(
            observation.preflight.get(),
            1,
            "REFRESH-TARGET-CHECKPOINT-SEQUENCE"
        );
        assert_eq!(
            observation.save.get(),
            usize::from(!case.preflight_terminal()),
            "REFRESH-TARGET-CHECKPOINT-SEQUENCE"
        );
        assert_eq!(
            observation.writer.get(),
            usize::from(case.continues()),
            "REFRESH-TARGET-PERSISTENCE-FAILSOFT"
        );
        assert_eq!(
            observation.retry.get(),
            usize::from(case.continues()),
            "REFRESH-TARGET-USAGE-BLOCKED"
        );
        if case.continues() {
            assert!(
                observation.retry_token.get() && observation.retry_binding.get(),
                "REFRESH-TARGET-PERSISTENCE-FAILSOFT"
            );
        }
        let mut expected_checkpoints = vec![
            RefreshCheckpoint::Reloaded,
            RefreshCheckpoint::NetworkReturned,
        ];
        if !case.preflight_terminal() {
            expected_checkpoints.extend([
                RefreshCheckpoint::MetadataHandled,
                RefreshCheckpoint::CredentialsPersisted,
            ]);
        }
        assert_eq!(
            *observation.checkpoints.borrow(),
            expected_checkpoints,
            "REFRESH-TARGET-CHECKPOINT-SEQUENCE"
        );
        assert_eq!(
            recording.resolves.get(),
            1 + usize::from(case.cacheable()),
            "REFRESH-TARGET-CHECKPOINT-SEQUENCE"
        );
        assert_eq!(
            recording.transfers.get(),
            usize::from(!case.preflight_terminal()),
            "REFRESH-TARGET-CHECKPOINT-SEQUENCE"
        );
        assert!(
            (scope.metadata_bytes() == metadata_before) == case.preflight_terminal(),
            "{terminal_label}"
        );
        let current = grok_path_state(&path);
        if case.expected_display().is_some() {
            assert!(
                current == *expected.borrow(),
                "REFRESH-TARGET-DURABLE-PRESERVED"
            );
        } else if case == GrokTargetCase::Persistence {
            assert!(current == initial, "REFRESH-TARGET-PERSISTENCE-FAILSOFT");
        } else {
            let stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert!(
                stored[TEST_ENTRY]["key"] == "grok-new-access"
                    && stored[TEST_ENTRY]["refresh_token"] == "grok-new-refresh"
                    && stored[TEST_ENTRY]["entry_sibling"]["keep"] == true
                    && stored["https://auth.openai.com::foreign"]["keep"] == "concurrent"
                    && stored["root_sibling"]["writer"] == "grok-cli",
                "REFRESH-TARGET-SIBLING-PRESERVATION"
            );
        }
        let lineage_marker: &[u8] = if case.preflight_terminal() {
            b"grok-old-refresh"
        } else {
            b"grok-new-refresh"
        };
        assert!(
            scope
                .resolve_current("grok-auth-json", &location, lineage_marker)
                .is_ok_and(|resolved| resolved == old_scope),
            "{terminal_label}"
        );
        scope.cleanup();
    }

    #[tokio::test]
    async fn grok_refresh_target_matrix_blocks_required_weekly_retry() {
        for case in GROK_TARGET_CASES {
            run_grok_target_case(case).await;
        }
    }

    #[tokio::test]
    async fn refresh_crash_boundaries_and_scope_gate_use_production_sequence() {
        assert_refresh_crash_boundaries(
            "grok",
            "grok-auth-json",
            b"grok-old-refresh",
            b"grok-new-refresh",
            setup_refresh,
            |scope, path, crash| {
                Box::pin(async move { run_refresh(scope, path, crash).await.map(|_| ()) })
            },
            |path| stored_refresh_token(path).into_bytes(),
            |boundary| boundary == RefreshCheckpoint::CredentialsPersisted,
            |boundary| {
                matches!(
                    boundary,
                    RefreshCheckpoint::MetadataHandled | RefreshCheckpoint::CredentialsPersisted
                )
            },
        )
        .await;

        let (scope, path, old_scope, before, location) = setup_refresh("grok-metadata-fail");
        scope.fail_metadata_save();
        let failure = run_refresh(&scope, &path, None).await.unwrap_err();
        assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
        assert_eq!(scope.metadata_bytes(), before);
        let persisted_marker = stored_refresh_token(&path);
        assert_eq!(persisted_marker, "grok-old-refresh");
        assert_eq!(
            scope
                .resolve_current("grok-auth-json", &location, persisted_marker.as_bytes())
                .unwrap(),
            old_scope
        );
        scope.cleanup();

        let (scope, path, old_scope, _, location) = setup_refresh("grok-success");
        let (_, scope_outcome, cache_binding) = run_refresh(&scope, &path, None).await.unwrap();
        assert_eq!(scope_outcome, old_scope);
        assert_eq!(
            cache_binding,
            Some(ProviderCacheBinding::primary(old_scope.clone()))
        );
        assert_eq!(
            scope
                .resolve_current("grok-auth-json", &location, b"grok-new-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();
    }

    #[tokio::test]
    async fn refresh_persistence_failure_keeps_usage_fresh_but_uncacheable() {
        let (scope, path, old_scope, _, location) = setup_refresh("grok-save-fail");
        let (refreshed, scope_outcome, cache_binding) = refresh_credentials_with(
            &path,
            TEST_ENTRY,
            true,
            &scope,
            grok_test_response,
            validate_refresh_target,
            |_| Err(RefreshTargetError::Persistence),
            checkpoint_at(None),
        )
        .await
        .unwrap();

        assert_eq!(refreshed.access_token, "grok-new-access");
        assert_eq!(scope_outcome, old_scope);
        assert_eq!(cache_binding, None);
        assert_eq!(stored_refresh_token(&path), "grok-old-refresh");
        assert_eq!(
            scope
                .resolve_current("grok-auth-json", &location, b"grok-new-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();
    }

    #[tokio::test]
    async fn refresh_transient_uses_lock_reloaded_binding_not_outer_binding() {
        let (scope, path, inner_scope, _, location) = setup_refresh("grok-lock-binding");
        let outer_scope = scope
            .resolve_current("grok-auth-json", &location, b"outer-refresh-a")
            .unwrap();
        assert_ne!(outer_scope, inner_scope);
        let expected = ProviderCacheBinding::primary(inner_scope);
        let request_expected = expected.clone();

        let failure = refresh_credentials_with(
            &path,
            TEST_ENTRY,
            true,
            &scope,
            move |refresh_token, client_id, attempt_binding| async move {
                assert_eq!(refresh_token, "grok-old-refresh");
                assert_eq!(client_id, "fixture-client");
                assert_eq!(attempt_binding, request_expected);
                Err(ProviderFetchFailure::transient(
                    "Grok token refresh failed. Retrying automatically.",
                    Some(attempt_binding),
                    SafeTransportDiagnostic::from_facts(TransportErrorFacts::synthetic(
                        true,
                        false,
                        TransportPhase::Request,
                        None,
                    )),
                ))
            },
            validate_refresh_target,
            save_credentials,
            checkpoint_at(None),
        )
        .await
        .unwrap_err();

        assert_transient_used_lock_reloaded_binding(failure, expected);
        scope.cleanup();
    }
}
