//! Kiro subscription quota — ported from mana.bar's `KiroProvider`.
//!
//! Kiro (the AWS CodeWhisperer agent) exposes a per-account subscription quota
//! at `/getUsageLimits`, reporting a single monthly allowance as an absolute
//! used amount against a limit, plus the next reset time. We authenticate with
//! the Bearer token Kiro already stored — either the kiro-cli SQLite store or
//! the Kiro IDE token file (`kiro_integrations.rs`) — so the card appears
//! whenever Kiro is signed in. The one allowance maps to one `UsageWindow`.
//!
//! Two contracts this file honors, both stricter than mana.bar's original:
//!
//! - **Out-of-range is invalid, not clamped.** `provider-quota-pace.md` classes
//!   a non-finite or out-of-bounds percentage as `invalid`: it must not be
//!   recorded. A `current` above `limit`, a non-positive `limit`, or a negative
//!   amount drops the window rather than becoming a plausible `100%` card.
//! - **An expired reset invalidates the reading, not just the reset.** The
//!   endpoint always reports the NEXT reset, so one at or before now cannot
//!   describe the current cycle. `provider-quota-pace.md` classes that as
//!   `invalid`, which is not recorded at all: the response is terminal rather
//!   than a card with the reset quietly removed, which would read as healthy
//!   and — since `usable_success` admits Kiro on a non-empty window — would be
//!   written into the last-good cache over the previous good reading. An
//!   ABSENT reset is a different reading and keeps the window; the provider
//!   naming no cycle end is not the same as naming an impossible one.
//!
//! The window carries no duration evidence: the endpoint reports the reset
//! instant but not the cycle length (a Kiro plan resets on the account's own
//! billing date, not a fixed calendar month), so the pace lifecycle learns the
//! duration (learning-duration), exactly as OpenCode Go does.

use crate::agent_account_scope::{self, AccountScope, AccountScopeError};
use crate::agent_usage::{
    clean_plan, provider_http_client_builder, read_response_body, request_after_verified_binding,
    AgentIdentity, ProviderCacheBinding, ProviderFetchFailure, ResponseReadFailure,
    TransportErrorFacts, TransportPhase, UsageWindow,
};
use crate::kiro_integrations::KiroCredential;
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;

const USAGE_URL: &str = "https://codewhisperer.us-east-1.amazonaws.com/getUsageLimits";
const WINDOW_LABEL: &str = "Monthly";
const WINDOW_KEY: &str = "usage.v1";

pub(crate) struct KiroData {
    pub identity: Option<AgentIdentity>,
    pub account_scope: Result<AccountScope, AccountScopeError>,
    pub cache_binding: ProviderCacheBinding,
    pub windows: Vec<UsageWindow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageResponse {
    #[serde(default)]
    subscription_info: Option<SubscriptionInfo>,
    #[serde(default)]
    next_date_reset: Option<f64>,
    #[serde(default)]
    usage_breakdown_list: Vec<UsageBreakdown>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionInfo {
    #[serde(default)]
    subscription_title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageBreakdown {
    #[serde(default)]
    current_usage_with_precision: Option<f64>,
    #[serde(default)]
    usage_limit_with_precision: Option<f64>,
}

pub(crate) async fn fetch(
    now: DateTime<Utc>,
    credential: KiroCredential,
) -> Result<KiroData, ProviderFetchFailure> {
    let verified = agent_account_scope::resolve_credential(
        "kiro",
        credential.semantic_source,
        &credential.canonical_location,
        &credential.marker,
    )
    .map(|account_scope| {
        let cache_binding = ProviderCacheBinding::primary(account_scope.clone());
        (account_scope, cache_binding)
    })
    .map_err(|_| ProviderFetchFailure::terminal("Kiro account identity could not be verified."));
    let (account_scope, cache_binding, response) =
        request_after_verified_binding(verified, |(account_scope, cache_binding)| async move {
            let client = provider_http_client_builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|_| {
                    ProviderFetchFailure::terminal("Kiro usage client could not be created.")
                })?;
            let mut url = reqwest::Url::parse(USAGE_URL).map_err(|_| {
                ProviderFetchFailure::terminal("Kiro usage URL could not be built.")
            })?;
            // The profile ARN scopes the quota to the signed-in account; the API
            // also answers without it, so an absent ARN is not a failure.
            if let Some(profile_arn) = credential.profile_arn.as_deref() {
                url.query_pairs_mut().append_pair("profileArn", profile_arn);
            }
            let response = client
                .get(url)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", credential.request_token),
                )
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|error| {
                    ProviderFetchFailure::from_send_error(
                        "Kiro usage request failed. Retrying automatically.",
                        Some(cache_binding.clone()),
                        &error,
                    )
                })?;
            Ok((account_scope, cache_binding, response))
        })
        .await?;
    let status = response.status().as_u16();
    let body = read_response_body(status, false, || async {
        response.text().await.map_err(|error| {
            TransportErrorFacts::from_reqwest(&error, TransportPhase::ResponseBody)
        })
    })
    .await
    .map_err(|failure| match failure {
        ResponseReadFailure::Transient(diagnostic) => ProviderFetchFailure::transient(
            "Kiro usage request failed. Retrying automatically.",
            Some(cache_binding.clone()),
            diagnostic,
        ),
        ResponseReadFailure::Terminal(401 | 403) => {
            ProviderFetchFailure::terminal("Kiro credentials expired or lack access.")
        }
        ResponseReadFailure::Terminal(status) => ProviderFetchFailure::terminal(format!(
            "Kiro usage API rejected the request (status {status})."
        )),
    })?;
    let (plan, windows) = decode_usage_response(&body, now)?;
    Ok(KiroData {
        identity: Some(AgentIdentity { email: None, plan }),
        account_scope: Ok(account_scope),
        cache_binding,
        windows,
    })
}

pub(crate) fn decode_usage_response(
    body: &str,
    now: DateTime<Utc>,
) -> Result<(Option<String>, Vec<UsageWindow>), ProviderFetchFailure> {
    let response: UsageResponse = serde_json::from_str(body)
        .map_err(|_| ProviderFetchFailure::terminal("Kiro usage response could not be decoded."))?;
    let plan = response
        .subscription_info
        .and_then(|info| info.subscription_title)
        .filter(|title| !title.trim().is_empty())
        .map(clean_plan);
    let resets_at = match reset_from_epoch_seconds(response.next_date_reset, now) {
        ResetEvidence::Absent => None,
        ResetEvidence::Valid(reset) => Some(reset),
        // `provider-quota-pace.md` classes an expired reset as `invalid`, and an
        // invalid reading is not recorded. Emitting the percentage with the
        // reset quietly removed would present it as a healthy learning-duration
        // card, and because `usable_success` admits Kiro on a non-empty window,
        // that card would enter the last-good cache and overwrite the previous
        // good reading — the outcome the classification exists to prevent.
        ResetEvidence::Expired => {
            return Err(ProviderFetchFailure::terminal(
                "Kiro usage API reported a quota reset that has already passed.",
            ));
        }
    };
    let windows = response
        .usage_breakdown_list
        .first()
        .and_then(|breakdown| map_window(breakdown, resets_at, now))
        .map(|window| vec![window])
        .unwrap_or_default();
    if windows.is_empty() {
        // A 200 with no usable allowance is more likely a malformed or empty
        // payload than a real healthy state; do not present a 0% card for it.
        return Err(ProviderFetchFailure::terminal(
            "Kiro usage API returned no usable quota window.",
        ));
    }
    Ok((plan, windows))
}

/// What `nextDateReset` established, which is not the same question as what it
/// contained. An absent reset and an expired one are different readings: the
/// first says the provider did not name a cycle end, the second says it named
/// one that cannot be true, since the field reports the NEXT reset.
enum ResetEvidence {
    /// No reset reported, or a value that resolves to no instant at all. The
    /// window is kept and the pace lifecycle learns the duration.
    Absent,
    Valid(DateTime<Utc>),
    /// Reported, but at or before `now`.
    Expired,
}

/// Kiro reports `nextDateReset` as epoch seconds.
fn reset_from_epoch_seconds(value: Option<f64>, now: DateTime<Utc>) -> ResetEvidence {
    let Some(seconds) = value.filter(|value| value.is_finite()) else {
        return ResetEvidence::Absent;
    };
    let Some(reset) = Utc.timestamp_opt(seconds as i64, 0).single() else {
        return ResetEvidence::Absent;
    };
    if reset > now {
        ResetEvidence::Valid(reset)
    } else {
        ResetEvidence::Expired
    }
}

fn map_window(
    breakdown: &UsageBreakdown,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<UsageWindow> {
    let (current, limit) = (
        breakdown.current_usage_with_precision?,
        breakdown.usage_limit_with_precision?,
    );
    // A non-positive limit cannot yield a percentage, and a negative amount is
    // not a real reading; both are invalid rather than a healthy 0% card.
    if !current.is_finite() || !limit.is_finite() || limit <= 0.0 || current < 0.0 {
        return None;
    }
    let used_percent = (current / limit) * 100.0;
    // Drop a `current` above `limit` (over 100%) rather than clamp it: an
    // out-of-range reading is `invalid` (provider-quota-pace.md), not `100%`.
    UsageWindow::try_from_provider_used_percent(
        WINDOW_LABEL.to_string(),
        used_percent,
        resets_at,
        now,
    )
    .map(|window| window.with_identity(WINDOW_KEY, Some(WINDOW_KEY.to_string()), None, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        // 2026-09-09T00:00:00Z.
        Utc.timestamp_opt(1_788_912_000, 0).single().unwrap()
    }

    #[test]
    fn maps_the_single_allowance_and_reads_the_plan() {
        let body = r#"{
            "subscriptionInfo": {"subscriptionTitle": "kiro pro"},
            "nextDateReset": 1791504000,
            "usageBreakdownList": [
                {"currentUsageWithPrecision": 47.4, "usageLimitWithPrecision": 100.0}
            ]
        }"#;
        let (plan, windows) = decode_usage_response(body, now()).unwrap();
        assert_eq!(plan.as_deref(), Some("Kiro pro"));
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label_for_test(), "Monthly");
        // Used 47.4 -> remaining ~52.6.
        assert!((windows[0].remaining_for_test() - 52.6).abs() < 0.01);
        assert_eq!(windows[0].pace_window_key_for_test(), Some("usage.v1"));
        assert_eq!(
            windows[0].resets_at_for_test(),
            Some("2026-10-09T00:00:00.000Z")
        );
        // No cycle-length evidence -> pace learns the duration.
        assert_eq!(windows[0].window_minutes_for_test(), None);
    }

    #[test]
    fn only_the_first_breakdown_is_mapped() {
        let body = r#"{
            "usageBreakdownList": [
                {"currentUsageWithPrecision": 10.0, "usageLimitWithPrecision": 40.0},
                {"currentUsageWithPrecision": 99.0, "usageLimitWithPrecision": 100.0}
            ]
        }"#;
        let (_, windows) = decode_usage_response(body, now()).unwrap();
        assert_eq!(windows.len(), 1);
        assert!((windows[0].remaining_for_test() - 75.0).abs() < 0.01);
    }

    #[test]
    fn out_of_range_or_non_positive_limit_is_terminal() {
        for body in [
            // current above limit -> over 100%, dropped.
            r#"{"usageBreakdownList":[{"currentUsageWithPrecision":120.0,"usageLimitWithPrecision":100.0}]}"#,
            // non-positive limit cannot yield a percent.
            r#"{"usageBreakdownList":[{"currentUsageWithPrecision":0.0,"usageLimitWithPrecision":0.0}]}"#,
            // negative amount is not a real reading.
            r#"{"usageBreakdownList":[{"currentUsageWithPrecision":-1.0,"usageLimitWithPrecision":100.0}]}"#,
            // non-finite amount.
            r#"{"usageBreakdownList":[{"currentUsageWithPrecision":1e400,"usageLimitWithPrecision":100.0}]}"#,
            // missing fields.
            r#"{"usageBreakdownList":[{"currentUsageWithPrecision":10.0}]}"#,
            // empty list.
            r#"{"usageBreakdownList":[]}"#,
            r#"{}"#,
        ] {
            assert!(
                matches!(
                    decode_usage_response(body, now()),
                    Err(ProviderFetchFailure::Terminal { .. })
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn zero_usage_against_a_real_limit_is_a_healthy_window() {
        let body = r#"{"usageBreakdownList":[{"currentUsageWithPrecision":0.0,"usageLimitWithPrecision":500.0}]}"#;
        let (_, windows) = decode_usage_response(body, now()).unwrap();
        assert_eq!(windows.len(), 1);
        assert!((windows[0].remaining_for_test() - 100.0).abs() < 0.01);
    }

    #[test]
    fn an_expired_reset_is_terminal_while_an_absent_one_keeps_the_window() {
        // `nextDateReset` reports the NEXT reset, so one at or before now cannot
        // describe the current cycle. That is `invalid` evidence, and an invalid
        // reading is not recorded: dropping only the reset would leave a healthy
        // learning-duration card that `usable_success` then writes into the
        // last-good cache over the previous good reading.
        let expired = r#"{
            "nextDateReset": 1704067200,
            "usageBreakdownList": [{"currentUsageWithPrecision": 30.0, "usageLimitWithPrecision": 100.0}]
        }"#;
        assert!(matches!(
            decode_usage_response(expired, now()),
            Err(ProviderFetchFailure::Terminal { .. })
        ));

        // A reset exactly at `now` is not in the future either, so it is expired
        // rather than merely unhelpful — the boundary the comparison turns on.
        let at_now = r#"{
            "nextDateReset": 1788912000,
            "usageBreakdownList": [{"currentUsageWithPrecision": 30.0, "usageLimitWithPrecision": 100.0}]
        }"#;
        assert!(matches!(
            decode_usage_response(at_now, now()),
            Err(ProviderFetchFailure::Terminal { .. })
        ));

        // An absent reset is a different reading: the provider named no cycle
        // end, so the percentage stands and the pace lifecycle learns the
        // duration. This is the case that must NOT become terminal.
        let no_reset = r#"{"usageBreakdownList":[{"currentUsageWithPrecision":30.0,"usageLimitWithPrecision":100.0}]}"#;
        let (_, windows) = decode_usage_response(no_reset, now()).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].resets_at_for_test(), None);
        assert!((windows[0].remaining_for_test() - 70.0).abs() < 0.01);
    }

    #[test]
    fn undecodable_body_is_terminal() {
        assert!(matches!(
            decode_usage_response("not json", now()),
            Err(ProviderFetchFailure::Terminal { .. })
        ));
    }
}
