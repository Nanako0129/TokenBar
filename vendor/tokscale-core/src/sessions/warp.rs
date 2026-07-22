use super::UnifiedMessage;
use crate::TokenBreakdown;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const WARP_NORMALIZED_USAGE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WarpNormalizedUsage {
    pub version: u32,
    pub synced_at_ms: i64,
    pub account_scope: String,
    pub usage: WarpAggregateUsage,
    #[serde(default)]
    pub workspaces: Vec<WarpWorkspaceUsage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WarpAggregateUsage {
    pub requests_used: Option<i64>,
    pub request_limit: Option<i64>,
    pub spend_cents: Option<i64>,
    pub credits_purchased_cents: Option<i64>,
    pub next_refresh_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WarpWorkspaceUsage {
    pub workspace_scope: String,
    pub requests_used: Option<i64>,
    pub spend_cents: Option<i64>,
}

/// Point-in-time Warp source supplied by TokenBar's credential boundary.
///
/// The source contains only normalized usage and installation-key HMAC identities.
/// Raw bearer tokens, workspace ids/names, response ids, and external file bytes never
/// enter tokscale-core, its message cache, or any report. The source path is used only
/// as the existing cache identity; parsing consumes the in-memory normalized payload.
#[derive(Clone)]
pub struct WarpUsageSource {
    path: PathBuf,
    usage: Arc<WarpNormalizedUsage>,
    fingerprint: [u8; 32],
    observed_mtime_ms: u64,
}

impl std::fmt::Debug for WarpUsageSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WarpUsageSource")
            .field("path", &"<redacted>")
            .field("account_scope", &"<opaque>")
            .field("workspace_count", &self.usage.workspaces.len())
            .field("observed_mtime_ms", &self.observed_mtime_ms)
            .finish()
    }
}

impl WarpUsageSource {
    pub fn new(
        path: PathBuf,
        usage: WarpNormalizedUsage,
        fingerprint: [u8; 32],
        observed_mtime_ms: u64,
    ) -> Result<Self, &'static str> {
        validate_normalized_usage(&usage)?;
        if path.as_os_str().is_empty() {
            return Err("warp source path is empty");
        }
        Ok(Self {
            path,
            usage: Arc::new(usage),
            fingerprint,
            observed_mtime_ms,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn observed_mtime_ms(&self) -> u64 {
        self.observed_mtime_ms
    }

    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub(crate) fn usage(&self) -> &WarpNormalizedUsage {
        &self.usage
    }
}

pub fn parse_warp_source(source: &WarpUsageSource) -> Vec<UnifiedMessage> {
    let usage = source.usage();
    if validate_normalized_usage(usage).is_err() {
        return Vec::new();
    }

    let workspace_messages: Vec<UnifiedMessage> = usage
        .workspaces
        .iter()
        .filter_map(|workspace| workspace_to_message(workspace, usage.synced_at_ms))
        .collect();
    if !workspace_messages.is_empty() {
        return workspace_messages;
    }

    usage_to_message(usage).into_iter().collect()
}

fn usage_to_message(usage: &WarpNormalizedUsage) -> Option<UnifiedMessage> {
    let requests = non_negative_i32(usage.usage.requests_used);
    let spend_cents = non_negative_i64(usage.usage.spend_cents);
    if requests == 0 && spend_cents == 0 {
        return None;
    }

    let mut message = UnifiedMessage::new(
        "warp",
        "aggregate-requests",
        "warp",
        format!("warp-aggregate-account-{}", usage.account_scope),
        usage.synced_at_ms,
        TokenBreakdown::default(),
        cents_to_dollars(spend_cents),
    );
    message.message_count = requests;
    message.mark_provider_reported_cost();
    Some(message)
}

fn workspace_to_message(workspace: &WarpWorkspaceUsage, timestamp: i64) -> Option<UnifiedMessage> {
    let requests = non_negative_i32(workspace.requests_used);
    let spend_cents = non_negative_i64(workspace.spend_cents);
    if requests == 0 && spend_cents == 0 {
        return None;
    }

    let mut message = UnifiedMessage::new(
        "warp",
        "aggregate-requests",
        "warp",
        format!("warp-aggregate-workspace-{}", workspace.workspace_scope),
        timestamp,
        TokenBreakdown::default(),
        cents_to_dollars(spend_cents),
    );
    message.message_count = requests;
    message.set_workspace(Some(workspace.workspace_scope.clone()), None);
    message.mark_provider_reported_cost();
    Some(message)
}

fn validate_normalized_usage(usage: &WarpNormalizedUsage) -> Result<(), &'static str> {
    if usage.version != WARP_NORMALIZED_USAGE_VERSION || usage.synced_at_ms <= 0 {
        return Err("invalid warp normalized usage header");
    }
    if !is_opaque_hmac(&usage.account_scope) {
        return Err("invalid warp account scope");
    }
    validate_non_negative(usage.usage.requests_used)?;
    validate_non_negative(usage.usage.request_limit)?;
    validate_non_negative(usage.usage.spend_cents)?;
    validate_non_negative(usage.usage.credits_purchased_cents)?;
    if usage
        .usage
        .next_refresh_at_ms
        .is_some_and(|value| value <= 0)
    {
        return Err("invalid warp refresh time");
    }

    let mut scopes = HashSet::new();
    for workspace in &usage.workspaces {
        if !is_opaque_hmac(&workspace.workspace_scope)
            || !scopes.insert(workspace.workspace_scope.as_str())
        {
            return Err("invalid warp workspace scope");
        }
        validate_non_negative(workspace.requests_used)?;
        validate_non_negative(workspace.spend_cents)?;
    }
    Ok(())
}

fn validate_non_negative(value: Option<i64>) -> Result<(), &'static str> {
    if value.is_some_and(|value| value < 0) {
        Err("negative warp usage value")
    } else {
        Ok(())
    }
}

fn is_opaque_hmac(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn non_negative_i64(value: Option<i64>) -> i64 {
    value.unwrap_or(0).max(0)
}

fn non_negative_i32(value: Option<i64>) -> i32 {
    non_negative_i64(value).min(i32::MAX as i64) as i32
}

fn cents_to_dollars(cents: i64) -> f64 {
    cents as f64 / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opaque(byte: u8) -> String {
        std::iter::repeat_n(byte as char, 43).collect()
    }

    fn source(usage: WarpNormalizedUsage) -> WarpUsageSource {
        WarpUsageSource::new(PathBuf::from("/private/usage.json"), usage, [7; 32], 1)
            .expect("valid source")
    }

    #[test]
    fn normalized_workspace_usage_emits_only_opaque_identity() {
        let account_scope = opaque(b'A');
        let workspace_scope = opaque(b'B');
        let messages = parse_warp_source(&source(WarpNormalizedUsage {
            version: WARP_NORMALIZED_USAGE_VERSION,
            synced_at_ms: 1_769_683_200_000,
            account_scope: account_scope.clone(),
            usage: WarpAggregateUsage {
                requests_used: Some(42),
                request_limit: Some(100),
                spend_cents: Some(1_234),
                credits_purchased_cents: Some(500),
                next_refresh_at_ms: Some(1_769_769_600_000),
            },
            workspaces: vec![WarpWorkspaceUsage {
                workspace_scope: workspace_scope.clone(),
                requests_used: Some(12),
                spend_cents: Some(345),
            }],
        }));

        assert_eq!(messages.len(), 1);
        let workspace = &messages[0];
        assert_eq!(workspace.client, "warp");
        assert_eq!(workspace.model_id, "aggregate-requests");
        assert_eq!(workspace.provider_id, "warp");
        assert_eq!(
            workspace.session_id,
            format!("warp-aggregate-workspace-{workspace_scope}")
        );
        assert_eq!(
            workspace.workspace_key.as_deref(),
            Some(workspace_scope.as_str())
        );
        assert_eq!(workspace.workspace_label, None);
        assert_eq!(workspace.message_count, 12);
        assert_eq!(workspace.tokens, TokenBreakdown::default());
        assert!((workspace.cost - 3.45).abs() < 1e-9);
        assert!(workspace.has_authoritative_cost());
        assert!(!workspace.session_id.contains(&account_scope));
    }

    #[test]
    fn empty_workspace_usage_falls_back_to_opaque_account_scope() {
        let account_scope = opaque(b'C');
        let messages = parse_warp_source(&source(WarpNormalizedUsage {
            version: WARP_NORMALIZED_USAGE_VERSION,
            synced_at_ms: 1_769_683_200_000,
            account_scope: account_scope.clone(),
            usage: WarpAggregateUsage {
                requests_used: Some(42),
                spend_cents: Some(1_234),
                ..Default::default()
            },
            workspaces: vec![],
        }));

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].session_id,
            format!("warp-aggregate-account-{account_scope}")
        );
        assert_eq!(messages[0].workspace_key, None);
        assert_eq!(messages[0].workspace_label, None);
        assert_eq!(messages[0].message_count, 42);
        assert!((messages[0].cost - 12.34).abs() < 1e-9);
    }

    #[test]
    fn raw_or_duplicate_scopes_are_rejected_before_parsing() {
        let raw = WarpNormalizedUsage {
            version: WARP_NORMALIZED_USAGE_VERSION,
            synced_at_ms: 1,
            account_scope: "raw-account-id".to_string(),
            usage: WarpAggregateUsage::default(),
            workspaces: vec![],
        };
        assert!(WarpUsageSource::new(PathBuf::from("usage.json"), raw, [0; 32], 1).is_err());

        let workspace_scope = opaque(b'D');
        let duplicate = WarpNormalizedUsage {
            version: WARP_NORMALIZED_USAGE_VERSION,
            synced_at_ms: 1,
            account_scope: opaque(b'E'),
            usage: WarpAggregateUsage::default(),
            workspaces: vec![
                WarpWorkspaceUsage {
                    workspace_scope: workspace_scope.clone(),
                    requests_used: Some(1),
                    spend_cents: None,
                },
                WarpWorkspaceUsage {
                    workspace_scope,
                    requests_used: Some(2),
                    spend_cents: None,
                },
            ],
        };
        assert!(WarpUsageSource::new(PathBuf::from("usage.json"), duplicate, [0; 32], 1).is_err());
    }
}
