//! GitHub Copilot quota — ported from codexbar's CopilotUsageFetcher.
//!
//! Copilot has no token-usage log, but GitHub exposes a per-account quota at
//! `/copilot_internal/user` (premium interactions + chat, as percent-remaining
//! snapshots). We authenticate with the GitHub OAuth token opencode already
//! stored for its Copilot login (`~/.local/share/opencode/auth.json`), so the
//! card appears whenever Copilot is signed in there. Maps to `UsageWindow`s.

use crate::agent_account_scope::{self, AccountScope, AccountScopeError};
use crate::agent_quota_duration::{copilot_calendar_duration, DurationEvidence};
use crate::agent_usage::{
    clean_plan, provider_http_client_builder, read_response_body, request_after_verified_binding,
    AgentIdentity, ProviderCacheBinding, ProviderFetchFailure, ResponseReadFailure,
    TransportErrorFacts, TransportPhase, UsageWindow,
};
use crate::opencode_integrations::GitHubCopilotCredential;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::{value::RawValue, Value};

const COPILOT_USAGE_URL: &str = "https://api.github.com/copilot_internal/user";

pub(crate) struct CopilotData {
    pub identity: Option<AgentIdentity>,
    pub account_scope: Result<AccountScope, AccountScopeError>,
    pub cache_binding: ProviderCacheBinding,
    pub windows: Vec<UsageWindow>,
}

#[derive(Debug, Deserialize)]
struct CopilotUser {
    #[serde(default)]
    copilot_plan: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    quota_reset_date: Option<String>,
    #[serde(default)]
    quota_snapshots: Option<QuotaSnapshots>,
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        Option::<Value>::deserialize(deserializer)?.and_then(|value| match value {
            Value::String(value) => Some(value),
            _ => None,
        }),
    )
}

#[derive(Debug, Deserialize)]
struct QuotaSnapshots {
    #[serde(default)]
    premium_interactions: Option<Box<RawValue>>,
    #[serde(default)]
    chat: Option<Box<RawValue>>,
}

#[derive(Debug, Deserialize)]
struct QuotaSnapshot {
    #[serde(default)]
    entitlement: Option<f64>,
    #[serde(default)]
    remaining: Option<f64>,
    #[serde(default)]
    percent_remaining: Option<f64>,
}

pub(crate) async fn fetch(
    now: DateTime<Utc>,
    credential: GitHubCopilotCredential,
) -> Result<CopilotData, ProviderFetchFailure> {
    let verified = agent_account_scope::resolve_credential(
        "copilot",
        credential.semantic_source,
        &credential.canonical_location,
        &credential.marker,
    )
    .map(|account_scope| {
        let cache_binding = ProviderCacheBinding::primary(account_scope.clone());
        (account_scope, cache_binding)
    })
    .map_err(|_| {
        ProviderFetchFailure::terminal("GitHub Copilot account identity could not be verified.")
    });
    let (account_scope, cache_binding, response) =
        request_after_verified_binding(verified, |(account_scope, cache_binding)| async move {
            let client = provider_http_client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|_| {
                    ProviderFetchFailure::terminal("Copilot usage client could not be created.")
                })?;
            let response = client
                .get(COPILOT_USAGE_URL)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("token {}", credential.request_token),
                )
                .header(reqwest::header::ACCEPT, "application/json")
                .header(reqwest::header::USER_AGENT, "GitHubCopilotChat/0.26.7")
                .header("Editor-Version", "vscode/1.96.2")
                .header("Editor-Plugin-Version", "copilot-chat/0.26.7")
                .header("X-Github-Api-Version", "2025-04-01")
                .send()
                .await
                .map_err(|error| {
                    ProviderFetchFailure::from_send_error(
                        "Copilot usage request failed. Retrying automatically.",
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
            "Copilot usage request failed. Retrying automatically.",
            Some(cache_binding.clone()),
            diagnostic,
        ),
        ResponseReadFailure::Terminal(401 | 403) => {
            ProviderFetchFailure::terminal("GitHub Copilot token expired or lacks access.")
        }
        ResponseReadFailure::Terminal(status) => ProviderFetchFailure::terminal(format!(
            "Copilot usage API rejected the request (status {status})."
        )),
    })?;
    let (plan, windows) = decode_usage_response(&body, now)?;
    Ok(CopilotData {
        identity: Some(AgentIdentity { email: None, plan }),
        account_scope: Ok(account_scope),
        cache_binding,
        windows,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopilotMapping {
    Usable,
    PlaceholderOnly,
    Invalid,
}

#[derive(Debug)]
enum CopilotRow {
    Usable(Box<UsageWindow>),
    Placeholder,
    Invalid,
    Absent,
}

pub(crate) fn decode_usage_response(
    body: &str,
    now: DateTime<Utc>,
) -> Result<(Option<String>, Vec<UsageWindow>), ProviderFetchFailure> {
    let usage: CopilotUser = serde_json::from_str(body).map_err(|_| {
        ProviderFetchFailure::terminal("Copilot usage response could not be decoded.")
    })?;
    let (plan, windows, mapping) = map_user(usage, now);
    if windows.is_empty() && mapping != CopilotMapping::PlaceholderOnly {
        return Err(ProviderFetchFailure::terminal(
            "Copilot usage API returned no usable quota windows.",
        ));
    }
    Ok((plan, windows))
}

fn map_user(
    usage: CopilotUser,
    now: DateTime<Utc>,
) -> (Option<String>, Vec<UsageWindow>, CopilotMapping) {
    let resets_at = usage.quota_reset_date.as_deref().and_then(parse_reset_date);
    let mut windows = Vec::new();
    let mut saw_placeholder = false;
    let mut saw_invalid = false;
    if let Some(snapshots) = usage.quota_snapshots {
        for row in [
            snapshot_window_with_identity(
                "Premium",
                "premium_interactions.v1",
                snapshots.premium_interactions.as_deref(),
                resets_at,
                now,
            ),
            snapshot_window_with_identity(
                "Chat",
                "chat.v1",
                snapshots.chat.as_deref(),
                resets_at,
                now,
            ),
        ] {
            match row {
                CopilotRow::Usable(window) => windows.push(*window),
                CopilotRow::Placeholder => saw_placeholder = true,
                CopilotRow::Invalid => saw_invalid = true,
                CopilotRow::Absent => {}
            }
        }
    }
    let plan = usage
        .copilot_plan
        .filter(|plan| !plan.trim().is_empty())
        .map(clean_plan);
    let mapping = if !windows.is_empty() {
        CopilotMapping::Usable
    } else if saw_placeholder && !saw_invalid {
        CopilotMapping::PlaceholderOnly
    } else {
        CopilotMapping::Invalid
    };
    (plan, windows, mapping)
}

fn snapshot_window_with_identity(
    label: &str,
    window_key: &str,
    raw: Option<&RawValue>,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> CopilotRow {
    let Some(raw) = raw else {
        return CopilotRow::Absent;
    };
    let Ok(snapshot) = serde_json::from_str::<QuotaSnapshot>(raw.get()) else {
        return CopilotRow::Invalid;
    };
    let (Some(entitlement), Some(remaining)) = (snapshot.entitlement, snapshot.remaining) else {
        return CopilotRow::Invalid;
    };
    if !entitlement.is_finite()
        || !remaining.is_finite()
        || entitlement < 0.0
        || remaining < 0.0
        || remaining > entitlement
    {
        return CopilotRow::Invalid;
    }
    if entitlement == 0.0 {
        return CopilotRow::Placeholder;
    }
    let derived_percent = (remaining / entitlement) * 100.0;
    let percent_remaining = match snapshot.percent_remaining {
        Some(percent)
            if percent.is_finite()
                && (0.0..=100.0).contains(&percent)
                // Provider payloads may round or truncate to a whole percent.
                && (percent - derived_percent).abs() <= 1.0 =>
        {
            percent
        }
        Some(_) => return CopilotRow::Invalid,
        None => derived_percent,
    };
    let contract_duration = resets_at
        .and_then(|reset| copilot_calendar_duration(reset.timestamp()))
        .map(DurationEvidence::contract);
    CopilotRow::Usable(Box::new(
        UsageWindow::from_provider_used_percent(
            label.to_string(),
            100.0 - percent_remaining,
            resets_at,
            now,
        )
        .with_identity(
            window_key,
            Some(window_key.to_string()),
            None,
            contract_duration,
        ),
    ))
}

#[cfg(test)]
fn snapshot_window(
    label: &str,
    raw: Option<&RawValue>,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<UsageWindow> {
    let window_key = match label {
        "Premium" => "premium_interactions.v1",
        "Chat" => "chat.v1",
        _ => "row.copilot.unknown.v1",
    };
    match snapshot_window_with_identity(label, window_key, raw, resets_at, now) {
        CopilotRow::Usable(window) => Some(*window),
        CopilotRow::Placeholder | CopilotRow::Invalid | CopilotRow::Absent => None,
    }
}

/// Copilot reports `quota_reset_date` as a bare `YYYY-MM-DD`; treat it as UTC midnight.
fn parse_reset_date(value: &str) -> Option<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").ok()?;
    Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(value: &str) -> Box<RawValue> {
        RawValue::from_string(value.to_string()).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_751_328_000, 0).single().unwrap()
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ExpectedRow {
        Usable,
        Placeholder,
        Invalid,
        Absent,
    }

    struct QuotaRowCase {
        entitlement: Option<f64>,
        remaining: Option<f64>,
        percent: Option<f64>,
        expected: ExpectedRow,
    }

    #[rustfmt::skip]
    fn snapshot(entitlement: Option<f64>, remaining: Option<f64>, percent: Option<f64>) -> String {
        let mut fields = Vec::new();
        if let Some(v) = entitlement { fields.push(format!(r#""entitlement":{v}"#)); }
        if let Some(v) = remaining { fields.push(format!(r#""remaining":{v}"#)); }
        if let Some(v) = percent { fields.push(format!(r#""percent_remaining":{v}"#)); }
        format!("{{{}}}", fields.join(","))
    }

    #[rustfmt::skip]
    fn usage_body(plan: Option<&str>, reset: Option<&str>, premium: Option<&str>, chat: Option<&str>) -> String {
        let mut fields = Vec::new();
        if let Some(plan) = plan { fields.push(format!(r#""copilot_plan":{}"#, serde_json::json!(plan))); }
        if let Some(reset) = reset { fields.push(format!(r#""quota_reset_date":{reset}"#)); }
        let mut snaps = Vec::new();
        if let Some(premium) = premium { snaps.push(format!(r#""premium_interactions":{premium}"#)); }
        if let Some(chat) = chat { snaps.push(format!(r#""chat":{chat}"#)); }
        if !snaps.is_empty() { fields.push(format!(r#""quota_snapshots":{{{}}}"#, snaps.join(","))); }
        format!("{{{}}}", fields.join(","))
    }

    #[rustfmt::skip]
    fn classify(payload: Option<&str>) -> ExpectedRow {
        match snapshot_window_with_identity(
            "Premium", "premium_interactions.v1", payload.map(raw).as_deref(), None, now(),
        ) {
            CopilotRow::Usable(_) => ExpectedRow::Usable,
            CopilotRow::Placeholder => ExpectedRow::Placeholder,
            CopilotRow::Invalid => ExpectedRow::Invalid,
            CopilotRow::Absent => ExpectedRow::Absent,
        }
    }

    fn map_body(body: &str) -> (Option<String>, Vec<UsageWindow>, CopilotMapping) {
        map_user(serde_json::from_str(body).unwrap(), now())
    }

    #[test]
    #[rustfmt::skip]
    fn classifies_quota_rows() {
        let cases = [
            (Some(300.0), Some(90.0), Some(30.0), ExpectedRow::Usable),
            (Some(100.0), Some(75.0), None, ExpectedRow::Usable),
            (Some(3.0), Some(2.0), Some(66.0), ExpectedRow::Usable),
            (Some(3.0), Some(2.0), Some(67.0), ExpectedRow::Usable),
            (Some(0.0), Some(0.0), Some(0.0), ExpectedRow::Placeholder),
            (None, None, None, ExpectedRow::Invalid),
            (Some(10.0), None, None, ExpectedRow::Invalid),
            (None, Some(10.0), None, ExpectedRow::Invalid),
            (Some(0.0), Some(1.0), Some(100.0), ExpectedRow::Invalid),
            (Some(100.0), Some(101.0), Some(100.0), ExpectedRow::Invalid),
            (Some(-1.0), Some(0.0), Some(0.0), ExpectedRow::Invalid),
            (Some(100.0), Some(-1.0), Some(0.0), ExpectedRow::Invalid),
            (Some(300.0), Some(90.0), Some(101.0), ExpectedRow::Invalid),
            (Some(100.0), Some(0.0), Some(100.0), ExpectedRow::Invalid),
        ];
        for (entitlement, remaining, percent, expected) in cases {
            let case = QuotaRowCase { entitlement, remaining, percent, expected };
            let payload = snapshot(case.entitlement, case.remaining, case.percent);
            assert_eq!(classify(Some(&payload)), case.expected, "{payload}");
        }
        let usable = snapshot_window(
            "Premium", Some(raw(&snapshot(Some(300.0), Some(90.0), Some(30.0))).as_ref()), None, now(),
        ).unwrap();
        assert!((usable.remaining_for_test() - 30.0).abs() < 0.01);
        assert_eq!(classify(None), ExpectedRow::Absent);
        for malformed in [
            r#"{"entitlement":100,"remaining":"NaN"}"#,
            r#"{"entitlement":300,"remaining":90,"percent_remaining":1e400}"#,
            r#"{"entitlement":300,"remaining":90,"percent_remaining":"NaN"}"#,
        ] {
            assert_eq!(classify(Some(malformed)), ExpectedRow::Invalid, "{malformed}");
        }
    }

    #[test]
    #[rustfmt::skip]
    fn malformed_reset_dates_are_ignored_without_poisoning_rows() {
        let premium = snapshot(Some(100.0), Some(60.0), Some(60.0));
        let valid: CopilotUser = serde_json::from_str(&usage_body(None, Some(r#""2026-08-01""#), Some(&premium), None)).unwrap();
        assert_eq!(valid.quota_reset_date.as_deref(), Some("2026-08-01"));
        for reset in ["null", "42", r#"{"credential":"token-secret"}"#, r#"["token-secret"]"#, "true"] {
            let body = usage_body(None, Some(reset), Some(&premium), None);
            let usage: CopilotUser = serde_json::from_str(&body).unwrap();
            assert_eq!(usage.quota_reset_date, None, "{reset}");
            let (_, windows, mapping) = map_user(usage, now());
            assert_eq!((mapping, windows.len()), (CopilotMapping::Usable, 1), "{reset}");
            assert!(windows[0].resets_at_for_test().is_none(), "{reset}");
            assert!(!serde_json::to_string(&windows[0]).unwrap().contains("token-secret"), "{reset}");
        }
        let invalid = usage_body(None, Some(r#"{"ignored":"token-secret"}"#), Some(&snapshot(Some(100.0), Some(101.0), Some(100.0))), None);
        assert!(matches!(decode_usage_response(&invalid, now()), Err(ProviderFetchFailure::Terminal { .. })));
    }

    #[test]
    #[rustfmt::skip]
    fn sibling_rows_stay_isolated() {
        let valid = snapshot(Some(100.0), Some(60.0), Some(60.0));
        let placeholder = snapshot(Some(0.0), Some(0.0), Some(0.0));
        let invalid = snapshot(Some(0.0), Some(1.0), Some(100.0));
        let chat = snapshot(Some(100.0), Some(75.0), Some(75.0));
        let cases: &[(&str, Option<&str>, CopilotMapping, &[&str])] = &[
            (&placeholder, None, CopilotMapping::PlaceholderOnly, &[]),
            (&valid, Some("{}"), CopilotMapping::Usable, &["Premium"]),
            (&valid, Some(&placeholder), CopilotMapping::Usable, &["Premium"]),
            (&invalid, Some(&chat), CopilotMapping::Usable, &["Chat"]),
            (&invalid, None, CopilotMapping::Invalid, &[]),
        ];
        for (premium, chat_row, expected, labels) in cases {
            let body = usage_body(None, None, Some(premium), *chat_row);
            let (_, windows, mapping) = map_body(&body);
            assert_eq!(mapping, *expected, "{body}");
            let got: Vec<_> = windows.iter().map(|w| w.label_for_test()).collect();
            assert_eq!(got, *labels, "{body}");
        }
        let placeholder_body = usage_body(None, None, Some(&placeholder), None);
        assert!(decode_usage_response(&placeholder_body, now()).unwrap().1.is_empty());
        let invalid_body = usage_body(None, None, Some(&invalid), None);
        assert!(matches!(decode_usage_response(&invalid_body, now()), Err(ProviderFetchFailure::Terminal { .. })));
        for percent in ["1e400", r#""NaN""#, "100"] {
            let premium = format!(r#"{{"entitlement":300,"remaining":90,"percent_remaining":{percent}}}"#);
            let (_, windows, mapping) = map_body(&usage_body(None, None, Some(&premium), Some(&chat)));
            assert_eq!(mapping, CopilotMapping::Usable, "{percent}");
            assert_eq!(windows[0].label_for_test(), "Chat");
            assert!((windows[0].remaining_for_test() - 75.0).abs() < 0.01);
        }
    }

    #[test]
    #[rustfmt::skip]
    fn shared_first_of_month_reset_applies_to_both_canonical_cards() {
        let body = usage_body(
            Some("individual"), Some(r#""2026-08-01""#),
            Some(&snapshot(Some(300.0), Some(90.0), Some(30.0))),
            Some(&snapshot(Some(100.0), Some(75.0), None)),
        );
        let (plan, windows, mapping) = map_body(&body);
        assert_eq!(mapping, CopilotMapping::Usable);
        assert_eq!(plan.as_deref(), Some("Individual"));
        assert_eq!(windows.len(), 2);
        let (premium, chat) = (&windows[0], &windows[1]);
        assert_eq!(premium.label_for_test(), "Premium");
        assert_eq!(chat.label_for_test(), "Chat");
        assert_eq!(premium.resets_at_for_test(), Some("2026-08-01T00:00:00.000Z"));
        assert_eq!(chat.resets_at_for_test(), premium.resets_at_for_test());
        assert_eq!(premium.window_minutes_for_test(), Some(44_640), "first-of-month reset uses the exact preceding calendar month");
        assert_eq!(chat.window_minutes_for_test(), Some(44_640));
        assert_eq!(premium.pace_window_key_for_test(), Some("premium_interactions.v1"));
        assert_eq!(chat.pace_window_key_for_test(), Some("chat.v1"));
        for window in &windows {
            let wire = serde_json::to_value(window).unwrap();
            assert_eq!(wire["paceStatus"]["durationSource"], "contract");
            assert_eq!(wire["paceStatus"]["durationSeconds"], 2_678_400);
        }
        let observed = snapshot_window(
            "Premium",
            Some(raw(&snapshot(Some(300.0), Some(90.0), Some(30.0))).as_ref()),
            Some(parse_reset_date("2026-08-15").unwrap()),
            now(),
        ).unwrap();
        assert_eq!(observed.window_minutes_for_test(), None, "copilot.premium.observed-fallback");
        assert!(parse_reset_date("2026-07-01").is_some());
        assert!(parse_reset_date("not-a-date").is_none());
    }
}
