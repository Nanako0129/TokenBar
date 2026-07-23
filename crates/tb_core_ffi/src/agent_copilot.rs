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
    clean_plan, read_response_body, request_after_verified_binding, AgentIdentity,
    ProviderCacheBinding, ProviderFetchFailure, ResponseReadFailure, TransportErrorFacts,
    TransportPhase, UsageWindow,
};
use crate::opencode_integrations::GitHubCopilotCredential;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use serde_json::value::RawValue;

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
    #[serde(default)]
    quota_reset_date: Option<String>,
    #[serde(default)]
    quota_snapshots: Option<QuotaSnapshots>,
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
            let client = reqwest::Client::builder()
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
    let usage: CopilotUser = serde_json::from_str(&body).map_err(|_| {
        ProviderFetchFailure::terminal("Copilot usage response could not be decoded.")
    })?;

    let (plan, windows, mapping) = map_user(usage, now);
    if windows.is_empty() && mapping != CopilotMapping::PlaceholderOnly {
        return Err(ProviderFetchFailure::terminal(
            "Copilot usage API returned no usable quota windows.",
        ));
    }
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

    #[test]
    fn maps_premium_and_chat_snapshots() {
        let now = Utc::now();
        let body = r#"{
            "copilot_plan": "individual",
            "quota_reset_date": "2026-07-01",
            "quota_snapshots": {
                "premium_interactions": { "entitlement": 300, "remaining": 90, "percent_remaining": 30 },
                "chat": { "entitlement": 0, "remaining": 0 }
            }
        }"#;
        let usage: CopilotUser = serde_json::from_str(body).unwrap();
        let snaps = usage.quota_snapshots.unwrap();
        let premium =
            snapshot_window("Premium", snaps.premium_interactions.as_deref(), None, now).unwrap();
        assert!((premium.remaining_for_test() - 30.0).abs() < 0.01);
        // chat is a zero-entitlement placeholder → skipped
        assert!(snapshot_window("Chat", snaps.chat.as_deref(), None, now).is_none());
    }

    #[test]
    fn zero_entitlement_and_missing_fields_do_not_create_usable_windows() {
        let now = Utc::now();
        let placeholder: CopilotUser = serde_json::from_str(
            r#"{
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 0,
                        "remaining": 0,
                        "percent_remaining": 0
                    }
                }
            }"#,
        )
        .unwrap();
        let (_, windows, mapping) = map_user(placeholder, now);
        assert!(windows.is_empty());
        assert_eq!(mapping, CopilotMapping::PlaceholderOnly);

        for malformed in [r#"{}"#, r#"{"entitlement": 10}"#, r#"{"remaining": 10}"#] {
            let row = raw(malformed);
            assert!(matches!(
                snapshot_window_with_identity(
                    "Premium",
                    "premium_interactions.v1",
                    Some(row.as_ref()),
                    None,
                    now,
                ),
                CopilotRow::Invalid
            ));
        }

        let valid_with_malformed_sibling: CopilotUser = serde_json::from_str(
            r#"{
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 100,
                        "remaining": 60,
                        "percent_remaining": 60
                    },
                    "chat": {}
                }
            }"#,
        )
        .unwrap();
        let (_, windows, mapping) = map_user(valid_with_malformed_sibling, now);
        assert_eq!(mapping, CopilotMapping::Usable);
        assert_eq!(windows.len(), 1);

        for malformed in [
            r#"{"entitlement":0,"remaining":1,"percent_remaining":100}"#,
            r#"{"entitlement":100,"remaining":101,"percent_remaining":100}"#,
            r#"{"entitlement":-1,"remaining":0,"percent_remaining":0}"#,
            r#"{"entitlement":100,"remaining":-1,"percent_remaining":0}"#,
        ] {
            let row = raw(malformed);
            assert!(matches!(
                snapshot_window_with_identity(
                    "Premium",
                    "premium_interactions.v1",
                    Some(row.as_ref()),
                    None,
                    now,
                ),
                CopilotRow::Invalid
            ));
        }

        let invalid_with_valid_sibling: CopilotUser = serde_json::from_str(
            r#"{
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 0,
                        "remaining": 1,
                        "percent_remaining": 100
                    },
                    "chat": {
                        "entitlement": 100,
                        "remaining": 75,
                        "percent_remaining": 75
                    }
                }
            }"#,
        )
        .unwrap();
        let (_, windows, mapping) = map_user(invalid_with_valid_sibling, now);
        assert_eq!(mapping, CopilotMapping::Usable);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label_for_test(), "Chat");

        let invalid_only: CopilotUser = serde_json::from_str(
            r#"{
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 0,
                        "remaining": 1,
                        "percent_remaining": 100
                    }
                }
            }"#,
        )
        .unwrap();
        let (_, windows, mapping) = map_user(invalid_only, now);
        assert!(windows.is_empty());
        assert_eq!(mapping, CopilotMapping::Invalid);
    }

    #[test]
    fn stage4_copilot_maps_shared_reset_to_both_quota_cards() {
        let now = Utc.timestamp_opt(1_751_328_000, 0).single().unwrap();
        let usage: CopilotUser = serde_json::from_str(
            r#"{
                "copilot_plan": "individual",
                "quota_reset_date": "2026-08-01",
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 300,
                        "remaining": 90,
                        "percent_remaining": 30
                    },
                    "chat": {
                        "entitlement": 100,
                        "remaining": 75
                    }
                }
            }"#,
        )
        .unwrap();
        let (plan, windows, mapping) = map_user(usage, now);
        assert_eq!(mapping, CopilotMapping::Usable);
        assert_eq!(plan.as_deref(), Some("Individual"));
        assert_eq!(windows.len(), 2);
        let premium = &windows[0];
        let chat = &windows[1];

        assert_eq!(premium.label_for_test(), "Premium");
        assert_eq!(chat.label_for_test(), "Chat");
        assert_eq!(
            premium.resets_at_for_test(),
            Some("2026-08-01T00:00:00.000Z")
        );
        assert_eq!(chat.resets_at_for_test(), premium.resets_at_for_test());
        assert_eq!(
            premium.window_minutes_for_test(),
            Some(44_640),
            "first-of-month reset uses the exact preceding calendar month"
        );
        assert_eq!(chat.window_minutes_for_test(), Some(44_640));
        assert_eq!(
            premium.pace_window_key_for_test(),
            Some("premium_interactions.v1")
        );
        assert_eq!(chat.pace_window_key_for_test(), Some("chat.v1"));
        for window in &windows {
            let wire = serde_json::to_value(window).unwrap();
            assert_eq!(wire["paceStatus"]["durationSource"], "contract");
            assert_eq!(wire["paceStatus"]["durationSeconds"], 2_678_400);
        }

        let non_calendar_reset = parse_reset_date("2026-08-15").unwrap();
        let observed_raw =
            raw(r#"{ "entitlement": 300, "remaining": 90, "percent_remaining": 30 }"#);
        let observed = snapshot_window(
            "Premium",
            Some(observed_raw.as_ref()),
            Some(non_calendar_reset),
            now,
        )
        .unwrap();
        assert_eq!(
            observed.window_minutes_for_test(),
            None,
            "copilot.premium.observed-fallback"
        );
    }

    #[test]
    fn rejects_invalid_remaining_percentages_before_wire() {
        let now = Utc::now();
        let out_of_range =
            raw(r#"{ "entitlement": 300, "remaining": 90, "percent_remaining": 101 }"#);
        assert!(snapshot_window("Premium", Some(out_of_range.as_ref()), None, now,).is_none());
        let non_finite =
            raw(r#"{ "entitlement": 100, "remaining": "NaN", "percent_remaining": null }"#);
        assert!(snapshot_window("Chat", Some(non_finite.as_ref()), None, now).is_none());
    }

    #[test]
    fn rejects_percentages_that_contradict_absolute_quota() {
        let now = Utc::now();
        let contradictory =
            raw(r#"{ "entitlement": 100, "remaining": 0, "percent_remaining": 100 }"#);
        assert!(matches!(
            snapshot_window_with_identity(
                "Premium",
                "premium_interactions.v1",
                Some(contradictory.as_ref()),
                None,
                now,
            ),
            CopilotRow::Invalid
        ));

        for rounded in [66, 67] {
            let payload = raw(&format!(
                r#"{{ "entitlement": 3, "remaining": 2, "percent_remaining": {rounded} }}"#
            ));
            assert!(matches!(
                snapshot_window_with_identity(
                    "Premium",
                    "premium_interactions.v1",
                    Some(payload.as_ref()),
                    None,
                    now,
                ),
                CopilotRow::Usable(_)
            ));
        }
    }

    #[test]
    fn malformed_snapshot_percentage_does_not_poison_valid_sibling() {
        let now = Utc.timestamp_opt(1_751_328_000, 0).single().unwrap();
        for invalid in ["1e400", r#""NaN""#, "100"] {
            let usage: CopilotUser = serde_json::from_str(&format!(
                r#"{{
                    "quota_reset_date": "2026-08-01",
                    "quota_snapshots": {{
                        "premium_interactions": {{
                            "entitlement": 300,
                            "remaining": 90,
                            "percent_remaining": {invalid}
                        }},
                        "chat": {{
                            "entitlement": 100,
                            "remaining": 75,
                            "percent_remaining": 75
                        }}
                    }}
                }}"#
            ))
            .unwrap();
            let (_, windows, mapping) = map_user(usage, now);
            assert_eq!(mapping, CopilotMapping::Usable);
            assert_eq!(windows.len(), 1);
            assert_eq!(windows[0].label_for_test(), "Chat");
            assert!((windows[0].remaining_for_test() - 75.0).abs() < 0.01);
        }
    }

    #[test]
    fn parses_reset_date() {
        assert!(parse_reset_date("2026-07-01").is_some());
        assert!(parse_reset_date("not-a-date").is_none());
    }
}
