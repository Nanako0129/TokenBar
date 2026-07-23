//! Detect which subscription-type providers opencode is authenticated against.
//!
//! opencode can sign in to providers via OAuth (a shared subscription, e.g.
//! "Sign in with ChatGPT" = the Codex/ChatGPT plan) or via API keys (metered).
//! Its `~/.local/share/opencode/auth.json` records each provider with a `type`.
//! We surface the `type: "oauth"` providers so the user can see which agent
//! subscriptions opencode also draws on (its usage counts against those plans).

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct AuthEntry {
    #[serde(default)]
    r#type: Option<String>,
}

/// Friendly subscription labels for opencode's OAuth providers, in a stable order.
pub fn detect_subscriptions() -> Vec<String> {
    let Some(path) = auth_path() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<BTreeMap<String, AuthEntry>>(&raw) else {
        return Vec::new();
    };
    let mut labels: Vec<String> = entries
        .into_iter()
        .filter(|(_, entry)| {
            entry
                .r#type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("oauth"))
        })
        .map(|(provider, _)| subscription_label(&provider))
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

fn subscription_label(provider: &str) -> String {
    match provider.to_lowercase().as_str() {
        "openai" => "Codex".to_string(),
        "anthropic" => "Claude".to_string(),
        "github-copilot" | "copilot" => "Copilot".to_string(),
        "google" | "gemini" => "Gemini".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => provider.to_string(),
            }
        }
    }
}

fn auth_path() -> Option<PathBuf> {
    crate::user_home_dir().map(|home| home.join(".local/share/opencode/auth.json"))
}

pub(crate) struct GitHubCopilotCredential {
    pub(crate) request_token: String,
    pub(crate) marker: Vec<u8>,
    pub(crate) semantic_source: &'static str,
    pub(crate) canonical_location: String,
}

/// The durable GitHub OAuth credential opencode stored for its Copilot login.
/// A non-empty refresh string is both the request token and lineage marker;
/// missing or invalid refresh values fall back to a non-empty access string.
pub(crate) fn github_copilot_credential() -> Option<GitHubCopilotCredential> {
    let path = auth_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let json = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    github_copilot_credential_from(&path, &json)
}

fn github_copilot_credential_from(
    path: &std::path::Path,
    json: &serde_json::Value,
) -> Option<GitHubCopilotCredential> {
    let entry = json.get("github-copilot")?;
    if entry.get("type").and_then(|t| t.as_str()) != Some("oauth") {
        return None;
    }
    let token = ["refresh", "access"]
        .into_iter()
        .filter_map(|key| entry.get(key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .find(|token| !token.is_empty())?
        .to_string();
    Some(GitHubCopilotCredential {
        request_token: token.clone(),
        marker: token.into_bytes(),
        semantic_source: "opencode-auth-json",
        canonical_location: crate::agent_account_scope::canonical_file_location(
            path,
            Some("github-copilot"),
        )
        .ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_oauth_providers_only() {
        assert_eq!(subscription_label("openai"), "Codex");
        assert_eq!(subscription_label("github-copilot"), "Copilot");
        assert_eq!(subscription_label("anthropic"), "Claude");
        assert_eq!(
            subscription_label("minimax-coding-plan"),
            "Minimax-coding-plan"
        );
    }

    #[test]
    fn copilot_lineage_marker_uses_first_valid_refresh_or_access() {
        let path = std::env::temp_dir().join("fixture-opencode-auth.json");
        let cases = [
            (
                "refresh preferred",
                serde_json::json!({
                    "github-copilot": {
                        "type": "oauth",
                        "refresh": " refresh-marker ",
                        "access": "access-marker"
                    }
                }),
                Some("refresh-marker"),
            ),
            (
                "refresh missing",
                serde_json::json!({
                    "github-copilot": { "type": "oauth", "access": " access-marker " }
                }),
                Some("access-marker"),
            ),
            (
                "refresh null",
                serde_json::json!({
                    "github-copilot": {
                        "type": "oauth",
                        "refresh": null,
                        "access": "access-marker"
                    }
                }),
                Some("access-marker"),
            ),
            (
                "refresh empty",
                serde_json::json!({
                    "github-copilot": {
                        "type": "oauth",
                        "refresh": "",
                        "access": "access-marker"
                    }
                }),
                Some("access-marker"),
            ),
            (
                "refresh whitespace",
                serde_json::json!({
                    "github-copilot": {
                        "type": "oauth",
                        "refresh": " \t\n ",
                        "access": "access-marker"
                    }
                }),
                Some("access-marker"),
            ),
            (
                "refresh non-string",
                serde_json::json!({
                    "github-copilot": {
                        "type": "oauth",
                        "refresh": { "unexpected": true },
                        "access": "access-marker"
                    }
                }),
                Some("access-marker"),
            ),
            (
                "both invalid",
                serde_json::json!({
                    "github-copilot": {
                        "type": "oauth",
                        "refresh": false,
                        "access": "   "
                    }
                }),
                None,
            ),
        ];

        for (label, json, expected) in cases {
            let credential = github_copilot_credential_from(&path, &json);
            match expected {
                Some(expected) => {
                    let credential = credential.unwrap_or_else(|| panic!("{label}"));
                    assert_eq!(credential.request_token, expected, "{label}");
                    assert_eq!(credential.marker, expected.as_bytes(), "{label}");
                }
                None => assert!(credential.is_none(), "{label}"),
            }
        }
    }
}
