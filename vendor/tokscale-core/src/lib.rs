#![deny(clippy::all)]

mod aggregator;
mod cc_mirror;
pub mod clients;
pub mod fs_atomic;
mod message_cache;
mod parser;
pub mod paths;
pub mod pricing;
mod provider_identity;
pub mod scanner;
pub mod sessionize;
pub mod sessions;

pub use aggregator::*;
pub use clients::{ClientCounts, ClientDef, ClientId, PathRoot};
pub use parser::*;
pub use scanner::*;
pub use sessionize::{
    compute_daily_active_time, compute_time_metrics, sessionize, SessionizeAccumulator,
    SessionInterval, TimeMetrics, DEFAULT_IDLE_GAP_MS,
};
pub use sessions::UnifiedMessage;

use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Strip a CLIProxyAPI-style `(level)` reasoning-effort suffix from a model id.
///
/// Mirrors <https://help.router-for.me/configuration/thinking>: the proxy
/// strips the parentheses before routing, so for pricing lookups we treat the
/// suffix as cosmetic and resolve to the base model. Accepts the level set the
/// proxy documents (case-insensitive — callers pass the lowercased id):
/// `minimal`, `low`, `medium`, `high`, `xhigh`, `auto`, `none`. Numeric
/// thinking budgets are intentionally not handled here.
pub(crate) fn strip_parenthesized_reasoning_tier(model_id: &str) -> Option<&str> {
    let without_closing_paren = model_id.strip_suffix(')')?;
    let (base_model, tier) = without_closing_paren.rsplit_once('(')?;

    if base_model.is_empty() || base_model.trim() != base_model {
        return None;
    }

    if !matches!(
        tier,
        "minimal" | "low" | "medium" | "high" | "xhigh" | "auto" | "none"
    ) {
        return None;
    }

    Some(base_model)
}

pub fn normalize_model_for_grouping(model_id: &str) -> String {
    let mut name = model_id.to_lowercase();

    if let Some(base_model) = strip_parenthesized_reasoning_tier(&name) {
        name = base_model.to_string();
    }
    if name.len() > 9 {
        let potential_date = &name[name.len() - 8..];
        if potential_date.chars().all(|c| c.is_ascii_digit())
            && name.as_bytes()[name.len() - 9] == b'-'
        {
            name = name[..name.len() - 9].to_string();
        }
    }

    if name.contains("claude") {
        let chars: Vec<char> = name.chars().collect();
        let mut result = String::with_capacity(name.len());
        for i in 0..chars.len() {
            if chars[i] == '.'
                && i > 0
                && i < chars.len() - 1
                && chars[i - 1].is_ascii_digit()
                && chars[i + 1].is_ascii_digit()
            {
                result.push('-');
            } else {
                result.push(chars[i]);
            }
        }
        name = result;
    }

    if let Some(canonical) = normalize_anthropic_prefixed_claude_model(&name) {
        name = canonical;
    }

    name
}

fn normalize_anthropic_prefixed_claude_model(model_id: &str) -> Option<String> {
    let rest = model_id.strip_prefix("anthropic/claude-")?;
    let mut parts = rest.split('-');
    let major = parts.next()?;
    let minor = parts.next()?;
    let family = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    if !matches!(family, "opus" | "sonnet" | "haiku") {
        return None;
    }

    Some(format!("claude-{family}-{major}-{minor}"))
}

fn retain_for_requested_clients(
    client: &str,
    model_id: &str,
    provider_id: &str,
    requested: &HashSet<&str>,
) -> bool {
    requested.contains(client)
        || (requested.contains("claude") && client.starts_with("cc-mirror/"))
        || (requested.contains("synthetic")
            && sessions::synthetic::matches_synthetic_filter(client, model_id, provider_id))
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub enum GroupBy {
    Model,
    #[default]
    ClientModel,
    ClientProviderModel,
    WorkspaceModel,
    Session,
    ClientSession,
}

impl std::fmt::Display for GroupBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupBy::Model => write!(f, "model"),
            GroupBy::ClientModel => write!(f, "client,model"),
            GroupBy::ClientProviderModel => write!(f, "client,provider,model"),
            GroupBy::WorkspaceModel => write!(f, "workspace,model"),
            GroupBy::Session => write!(f, "session,model"),
            GroupBy::ClientSession => write!(f, "client,session,model"),
        }
    }
}

impl std::str::FromStr for GroupBy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized: String = s.split(',').map(|p| p.trim()).collect::<Vec<_>>().join(",");
        match normalized.to_lowercase().as_str() {
            "model" => Ok(GroupBy::Model),
            "client,model" | "client-model" => Ok(GroupBy::ClientModel),
            "client,provider,model" | "client-provider-model" => Ok(GroupBy::ClientProviderModel),
            "workspace,model" | "workspace-model" => Ok(GroupBy::WorkspaceModel),
            "session" | "session,model" | "session-model" => Ok(GroupBy::Session),
            "client,session" | "client-session" | "client,session,model" | "client-session-model" => {
                Ok(GroupBy::ClientSession)
            }
            _ => Err(format!(
                "Invalid group-by value: '{}'. Valid options: model, client,model, client,provider,model, workspace,model, session,model, client,session,model",
                s
            )),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TokenBreakdown {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
}

impl TokenBreakdown {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_write + self.reasoning
    }
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPerformance {
    #[serde(rename = "msPer1KTokens")]
    pub ms_per_1k_tokens: Option<f64>,
    pub total_duration_ms: i64,
    pub timed_tokens: i64,
    pub sample_count: i32,
    pub token_coverage: f64,
}

impl ModelPerformance {
    pub fn record_message(&mut self, token_total: i64, duration_ms: Option<i64>) {
        let Some(duration_ms) = duration_ms else {
            return;
        };
        if duration_ms <= 0 || token_total <= 0 {
            return;
        }

        self.total_duration_ms = self.total_duration_ms.saturating_add(duration_ms);
        self.timed_tokens = self.timed_tokens.saturating_add(token_total);
        self.sample_count = self.sample_count.saturating_add(1);
    }

    pub fn finalize(&mut self, total_tokens: i64) {
        self.ms_per_1k_tokens = if self.timed_tokens > 0 && self.total_duration_ms > 0 {
            Some(self.total_duration_ms as f64 * 1000.0 / self.timed_tokens as f64)
        } else {
            None
        };

        self.token_coverage = if total_tokens > 0 {
            (self.timed_tokens as f64 / total_tokens as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
    }

    pub fn from_totals(total_duration_ms: i64, timed_tokens: i64, sample_count: i32) -> Self {
        let mut performance = Self {
            total_duration_ms,
            timed_tokens,
            sample_count,
            ..Self::default()
        };
        performance.finalize(timed_tokens);
        performance
    }
}

#[derive(Debug, Clone)]
pub struct ParsedMessage {
    pub client: String,
    pub model_id: String,
    pub provider_id: String,
    pub session_id: String,
    pub workspace_key: Option<String>,
    pub workspace_label: Option<String>,
    pub timestamp: i64,
    pub date: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub duration_ms: Option<i64>,
    pub message_count: i32,
    pub agent: Option<String>,
}

pub struct ParsedMessages {
    pub messages: Vec<ParsedMessage>,
    pub counts: ClientCounts,
    pub processing_time_ms: u32,
}

impl Clone for ParsedMessages {
    fn clone(&self) -> Self {
        let mut counts = ClientCounts::new();
        for client in ClientId::iter() {
            counts.set(client, self.counts.get(client));
        }

        Self {
            messages: self.messages.clone(),
            counts,
            processing_time_ms: self.processing_time_ms,
        }
    }
}

impl std::fmt::Debug for ParsedMessages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ParsedMessages");
        debug.field("messages", &self.messages);
        for client in ClientId::iter() {
            debug.field(client.as_str(), &self.counts.get(client));
        }
        debug.field("processing_time_ms", &self.processing_time_ms);
        debug.finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct LocalParseOptions {
    pub home_dir: Option<String>,
    pub use_env_roots: bool,
    pub clients: Option<Vec<String>>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub year: Option<String>,
    /// Persistent scanner config loaded from `~/.config/tokscale/settings.json`.
    /// Defaults to empty when callers don't care about user-configured paths.
    pub scanner_settings: scanner::ScannerSettings,
    /// Skip parsing file-backed session logs whose mtime (unix ms) is older
    /// than this. Lets high-frequency callers (live tails) avoid re-parsing
    /// an entire history when they only need recent messages — callers align
    /// it with `since`. Database-backed sources (SQLite) are always parsed:
    /// WAL writes may not touch the main db file's mtime.
    pub modified_after: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DailyTotals {
    pub tokens: i64,
    pub cost: f64,
    pub messages: i32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClientContribution {
    pub client: String,
    pub model_id: String,
    pub provider_id: String,
    pub tokens: TokenBreakdown,
    pub cost: f64,
    pub messages: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DailyContribution {
    pub date: String,
    pub totals: DailyTotals,
    pub intensity: u8,
    pub token_breakdown: TokenBreakdown,
    pub clients: Vec<ClientContribution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_time_ms: Option<i64>,
}

/// Per-session aggregate of token usage, cost, and timing — keyed on
/// `session_id` so downstream consumers can attribute cost to a specific
/// agent-CLI session rather than just a date or model rollup.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SessionContribution {
    pub session_id: String,
    pub client: String,
    pub provider: String,
    pub model: String,
    pub totals: DailyTotals,
    pub token_breakdown: TokenBreakdown,
    pub clients: Vec<ClientContribution>,
    /// Earliest message timestamp (unix seconds) in the session.
    pub first_seen: i64,
    /// Latest message timestamp (unix seconds) in the session.
    pub last_seen: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct YearSummary {
    pub year: String,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub range_start: String,
    pub range_end: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DataSummary {
    pub total_tokens: i64,
    pub total_cost: f64,
    pub total_days: i32,
    pub active_days: i32,
    pub average_per_day: f64,
    pub max_cost_in_single_day: f64,
    pub clients: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphMeta {
    pub generated_at: String,
    pub version: String,
    pub date_range_start: String,
    pub date_range_end: String,
    pub processing_time_ms: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphResult {
    pub meta: GraphMeta,
    pub summary: DataSummary,
    pub years: Vec<YearSummary>,
    pub contributions: Vec<DailyContribution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_metrics: Option<sessionize::TimeMetrics>,
}

#[derive(Debug, Clone, Default)]
pub struct ReportOptions {
    pub home_dir: Option<String>,
    pub use_env_roots: bool,
    pub clients: Option<Vec<String>>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub year: Option<String>,
    pub group_by: GroupBy,
    /// Persistent scanner config loaded from `~/.config/tokscale/settings.json`.
    /// Defaults to empty when callers don't care about user-configured paths.
    pub scanner_settings: scanner::ScannerSettings,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelUsage {
    pub client: String,
    pub merged_clients: Option<String>,
    pub workspace_key: Option<String>,
    pub workspace_label: Option<String>,
    pub session_id: Option<String>,
    pub model: String,
    pub provider: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub message_count: i32,
    pub cost: f64,
    pub performance: ModelPerformance,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MonthlyUsage {
    pub month: String,
    pub models: Vec<String>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub message_count: i32,
    pub cost: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelReport {
    pub entries: Vec<ModelUsage>,
    pub total_input: i64,
    pub total_output: i64,
    pub total_cache_read: i64,
    pub total_cache_write: i64,
    pub total_messages: i32,
    pub total_cost: f64,
    pub processing_time_ms: u32,
}

const UNKNOWN_WORKSPACE_LABEL: &str = "Unknown workspace";
const UNKNOWN_WORKSPACE_GROUP_KEY: &str = "\0unknown-workspace";

#[derive(Debug, Clone, serde::Serialize)]
pub struct MonthlyReport {
    pub entries: Vec<MonthlyUsage>,
    pub total_cost: f64,
    pub processing_time_ms: u32,
}

/// Hourly usage entry for a single hour slot (e.g. "2026-03-23 14:00")
#[derive(Debug, Clone, serde::Serialize)]
pub struct HourlyUsage {
    pub hour: String,
    pub clients: Vec<String>,
    pub models: Vec<String>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub message_count: i32,
    /// Number of user interaction turns (user→assistant boundaries).
    pub turn_count: i32,
    pub reasoning: i64,
    pub cost: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HourlyReport {
    pub entries: Vec<HourlyUsage>,
    pub total_cost: f64,
    pub processing_time_ms: u32,
}

pub fn get_home_dir_string(home_dir_option: &Option<String>) -> Result<String, String> {
    home_dir_option
        .clone()
        .or_else(|| std::env::var("HOME").ok())
        .or_else(|| dirs::home_dir().map(|p| p.to_string_lossy().into_owned()))
        .ok_or_else(|| {
            "HOME directory not specified and could not determine home directory".to_string()
        })
}

#[allow(dead_code)]
fn parse_all_messages_with_pricing(
    home_dir: &str,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
) -> Vec<UnifiedMessage> {
    parse_all_messages_with_pricing_with_env_strategy(
        home_dir,
        clients,
        pricing,
        true,
        &scanner::ScannerSettings::default(),
    )
}

// All report consumers (graph/model/monthly/hourly/agents) now fold over
// scan_messages_streaming. The materialized path below survives only behind the
// public `parse_local_unified_messages` (no in-repo callers — see its footgun
// doc) and the dead_code `parse_all_messages_with_pricing` wrapper.
fn parse_all_messages_with_pricing_with_env_strategy(
    home_dir: &str,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
    use_env_roots: bool,
    scanner_settings: &scanner::ScannerSettings,
) -> Vec<UnifiedMessage> {
    #[derive(Debug)]
    struct CachedParseOutcome {
        messages: Vec<UnifiedMessage>,
        cache_entry: Option<message_cache::CachedSourceEntry>,
        invalidate_cache: bool,
    }

    fn apply_pricing_to_messages(
        messages: &mut [UnifiedMessage],
        pricing: Option<&pricing::PricingService>,
    ) {
        for message in messages {
            message.refresh_derived_fields();
            apply_pricing_if_available(message, pricing);
        }
    }

    fn cached_messages(
        cached: &message_cache::CachedSourceEntry,
        pricing: Option<&pricing::PricingService>,
    ) -> Vec<UnifiedMessage> {
        let mut messages = cached.messages.clone();
        apply_pricing_to_messages(&mut messages, pricing);
        messages
    }

    fn parse_full_log_source(
        path: &Path,
        pricing: Option<&pricing::PricingService>,
        is_headless: bool,
    ) -> CachedParseOutcome {
        let fallback_timestamp = sessions::utils::file_modified_timestamp_ms(path);
        let parsed = sessions::codex::parse_codex_file_incremental(
            path,
            0,
            sessions::codex::CodexParseState::default(),
        );
        let messages = finalize_codex_messages(
            parsed.messages.clone(),
            pricing,
            is_headless,
            &parsed.fallback_timestamp_indices,
            fallback_timestamp,
        );
        if !parsed.parse_succeeded {
            return CachedParseOutcome {
                messages,
                cache_entry: None,
                invalidate_cache: false,
            };
        }

        if parsed.unresolved_model_events {
            return CachedParseOutcome {
                messages,
                cache_entry: None,
                invalidate_cache: false,
            };
        }

        let cache_entry = build_codex_cache_entry(
            path,
            parsed.messages,
            parsed.consumed_offset,
            parsed.state,
            parsed.fallback_timestamp_indices,
        );

        CachedParseOutcome {
            messages,
            cache_entry,
            invalidate_cache: false,
        }
    }

    fn finalize_codex_messages(
        mut messages: Vec<UnifiedMessage>,
        pricing: Option<&pricing::PricingService>,
        is_headless: bool,
        fallback_timestamp_indices: &[usize],
        fallback_timestamp: i64,
    ) -> Vec<UnifiedMessage> {
        for index in fallback_timestamp_indices {
            if let Some(message) = messages.get_mut(*index) {
                message.set_timestamp(fallback_timestamp);
            }
        }
        apply_pricing_to_messages(&mut messages, pricing);
        for message in &mut messages {
            apply_headless_agent(message, is_headless);
        }
        messages
    }

    fn build_codex_cache_entry(
        path: &Path,
        raw_messages: Vec<UnifiedMessage>,
        consumed_offset: u64,
        state: sessions::codex::CodexParseState,
        fallback_timestamp_indices: Vec<usize>,
    ) -> Option<message_cache::CachedSourceEntry> {
        let fingerprint = message_cache::SourceFingerprint::from_path(path)?;
        if fingerprint.size != consumed_offset {
            return None;
        }

        let codex_incremental =
            message_cache::build_codex_incremental_cache(path, consumed_offset, state)?;

        Some(message_cache::CachedSourceEntry::new(
            path,
            fingerprint,
            raw_messages,
            fallback_timestamp_indices,
            Some(codex_incremental),
        ))
    }

    fn load_or_parse_source_with_fingerprint_and_policy<F, FingerprintFn>(
        path: &Path,
        source_cache: &message_cache::SourceMessageCache,
        pricing: Option<&pricing::PricingService>,
        fingerprint_from_path: FingerprintFn,
        parse: F,
    ) -> CachedParseOutcome
    where
        F: Fn(&Path) -> (Vec<UnifiedMessage>, bool),
        FingerprintFn: Fn(&Path) -> Option<message_cache::SourceFingerprint>,
    {
        let Some(fingerprint) = fingerprint_from_path(path) else {
            let (mut messages, _) = parse(path);
            apply_pricing_to_messages(&mut messages, pricing);
            return CachedParseOutcome {
                messages,
                cache_entry: None,
                invalidate_cache: false,
            };
        };

        if let Some(cached) = source_cache.get(path) {
            if cached.fingerprint == fingerprint && !cached.messages.is_empty() {
                return CachedParseOutcome {
                    messages: cached_messages(cached, pricing),
                    cache_entry: None,
                    invalidate_cache: false,
                };
            }
        }

        let (mut messages, cacheable) = parse(path);
        let cache_entry = if messages.is_empty() || !cacheable {
            None
        } else {
            Some(message_cache::CachedSourceEntry::new(
                path,
                fingerprint,
                messages.clone(),
                Vec::new(),
                None,
            ))
        };
        apply_pricing_to_messages(&mut messages, pricing);

        CachedParseOutcome {
            messages,
            cache_entry,
            invalidate_cache: !cacheable,
        }
    }

    fn load_or_parse_source_with_fingerprint<F, FingerprintFn>(
        path: &Path,
        source_cache: &message_cache::SourceMessageCache,
        pricing: Option<&pricing::PricingService>,
        fingerprint_from_path: FingerprintFn,
        parse: F,
    ) -> CachedParseOutcome
    where
        F: Fn(&Path) -> Vec<UnifiedMessage>,
        FingerprintFn: Fn(&Path) -> Option<message_cache::SourceFingerprint>,
    {
        load_or_parse_source_with_fingerprint_and_policy(
            path,
            source_cache,
            pricing,
            fingerprint_from_path,
            |path| (parse(path), true),
        )
    }

    fn load_or_parse_source<F>(
        path: &Path,
        source_cache: &message_cache::SourceMessageCache,
        pricing: Option<&pricing::PricingService>,
        parse: F,
    ) -> CachedParseOutcome
    where
        F: Fn(&Path) -> Vec<UnifiedMessage>,
    {
        load_or_parse_source_with_fingerprint(
            path,
            source_cache,
            pricing,
            message_cache::SourceFingerprint::from_path,
            parse,
        )
    }

    fn load_or_parse_sqlite_source<F>(
        path: &Path,
        source_cache: &message_cache::SourceMessageCache,
        pricing: Option<&pricing::PricingService>,
        parse: F,
    ) -> CachedParseOutcome
    where
        F: Fn(&Path) -> Vec<UnifiedMessage>,
    {
        load_or_parse_source_with_fingerprint(
            path,
            source_cache,
            pricing,
            message_cache::SourceFingerprint::from_sqlite_path,
            parse,
        )
    }

    fn load_or_parse_codex_source(
        path: &Path,
        source_cache: &message_cache::SourceMessageCache,
        pricing: Option<&pricing::PricingService>,
        headless_roots: &[PathBuf],
    ) -> CachedParseOutcome {
        let is_headless = is_headless_path(path, headless_roots);
        let Some(fingerprint) = message_cache::SourceFingerprint::from_path(path) else {
            return parse_full_log_source(path, pricing, is_headless);
        };
        let fallback_timestamp = sessions::utils::file_modified_timestamp_ms(path);

        if let Some(cached) = source_cache.get(path) {
            let reparse_from_start = |invalidate_cache: bool| {
                let mut outcome = parse_full_log_source(path, pricing, is_headless);
                outcome.invalidate_cache = invalidate_cache && outcome.cache_entry.is_none();
                outcome
            };

            if cached.fingerprint == fingerprint {
                if message_cache::codex_cache_entry_matches_fingerprint(cached, &fingerprint) {
                    return CachedParseOutcome {
                        messages: finalize_codex_messages(
                            cached.messages.clone(),
                            pricing,
                            is_headless,
                            &cached.fallback_timestamp_indices,
                            fallback_timestamp,
                        ),
                        cache_entry: None,
                        invalidate_cache: false,
                    };
                }

                return reparse_from_start(true);
            }

            if let Some(codex_incremental) = cached.codex_incremental.as_ref() {
                if fingerprint.size > codex_incremental.consumed_offset
                    && message_cache::codex_prefix_matches(path, codex_incremental)
                {
                    let parsed = sessions::codex::parse_codex_file_incremental(
                        path,
                        codex_incremental.consumed_offset,
                        codex_incremental.state.clone(),
                    );
                    if parsed.parse_succeeded && !parsed.unresolved_model_events {
                        let mut raw_messages = cached.messages.clone();
                        let mut fallback_timestamp_indices =
                            cached.fallback_timestamp_indices.clone();
                        let existing_len = raw_messages.len();
                        fallback_timestamp_indices.extend(
                            parsed
                                .fallback_timestamp_indices
                                .iter()
                                .map(|index| existing_len + index),
                        );
                        raw_messages.extend(parsed.messages.clone());
                        let cache_entry = build_codex_cache_entry(
                            path,
                            raw_messages.clone(),
                            parsed.consumed_offset,
                            parsed.state,
                            fallback_timestamp_indices.clone(),
                        );
                        let Some(cache_entry) = cache_entry else {
                            return reparse_from_start(true);
                        };
                        let messages = finalize_codex_messages(
                            raw_messages,
                            pricing,
                            is_headless,
                            &fallback_timestamp_indices,
                            fallback_timestamp,
                        );
                        return CachedParseOutcome {
                            messages,
                            cache_entry: Some(cache_entry),
                            invalidate_cache: false,
                        };
                    }
                }
            }

            return reparse_from_start(true);
        }

        parse_full_log_source(path, pricing, is_headless)
    }

    let scan_result = scanner::scan_all_clients_with_scanner_settings(
        home_dir,
        clients,
        use_env_roots,
        scanner_settings,
    );
    let headless_roots = scanner::headless_roots_with_env_strategy(home_dir, use_env_roots);
    let mut source_cache = message_cache::SourceMessageCache::load();
    source_cache.prune_missing_files();
    let mut all_messages: Vec<UnifiedMessage> = Vec::new();
    let include_all = clients.is_empty();
    let include_synthetic = include_all || clients.iter().any(|c| c == "synthetic");

    // Parse OpenCode: prefer SQLite, collapse forked SQLite history there, then
    // suppress legacy JSON overlap by message identity.
    let mut opencode_seen: HashSet<String> = HashSet::new();

    for db_path in &scan_result.opencode_dbs {
        let CachedParseOutcome {
            messages,
            cache_entry,
            ..
        } = load_or_parse_sqlite_source(db_path, &source_cache, pricing, |path| {
            sessions::opencode::parse_opencode_sqlite(path)
        });

        // Dedup across channel-suffixed dbs: the same session can end up in
        // both `opencode.db` and `opencode-<channel>.db` if the user
        // switches channels mid-session. `discover_opencode_dbs` returns
        // paths in sorted order, so the first-seen copy is deterministic.
        all_messages.extend(messages.into_iter().filter(|message| {
            message
                .dedup_key
                .as_ref()
                .is_none_or(|key| opencode_seen.insert(key.clone()))
        }));

        if let Some(entry) = cache_entry {
            source_cache.insert(entry);
        }
    }

    let opencode_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::OpenCode)
        .par_iter()
        .filter_map(|path| {
            Some(load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::opencode::parse_opencode_file(path)
                    .into_iter()
                    .collect()
            }))
        })
        .collect();
    for outcome in opencode_outcomes {
        all_messages.extend(outcome.messages.into_iter().filter(|message| {
            message
                .dedup_key
                .as_ref()
                .is_none_or(|key| opencode_seen.insert(key.clone()))
        }));
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let claude_home = PathBuf::from(home_dir);
    let claude_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Claude)
        .par_iter()
        .map(|path| {
            load_or_parse_source_with_fingerprint(
                path,
                &source_cache,
                pricing,
                |path| {
                    message_cache::SourceFingerprint::from_claude_code_path_with_home(
                        path,
                        Some(&claude_home),
                    )
                },
                |path| sessions::claudecode::parse_claude_file_with_home(path, Some(&claude_home)),
            )
        })
        .collect();
    let mut claude_messages_raw: Vec<(String, UnifiedMessage)> = Vec::new();
    for outcome in claude_outcomes {
        claude_messages_raw.extend(outcome.messages.into_iter().map(|msg| {
            let dedup_key = msg.dedup_key.clone().unwrap_or_default();
            (dedup_key, msg)
        }));
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let mut seen_keys: HashSet<String> = HashSet::new();
    let claude_messages: Vec<UnifiedMessage> = claude_messages_raw
        .into_iter()
        .filter(|(key, _)| key.is_empty() || seen_keys.insert(key.clone()))
        .map(|(_, msg)| msg)
        .collect();
    all_messages.extend(claude_messages);

    let codex_outcomes: Vec<(PathBuf, CachedParseOutcome)> = scan_result
        .get(ClientId::Codex)
        .par_iter()
        .map(|path| {
            (
                path.clone(),
                load_or_parse_codex_source(path, &source_cache, pricing, &headless_roots),
            )
        })
        .collect();
    let mut codex_seen: HashSet<String> = HashSet::new();
    for (path, outcome) in codex_outcomes {
        all_messages.extend(
            outcome
                .messages
                .into_iter()
                .filter(|message| should_keep_deduped_message(&mut codex_seen, message)),
        );
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        } else if outcome.invalidate_cache {
            source_cache.remove(&path);
        }
    }

    let copilot_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Copilot)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::copilot::parse_copilot_file(path)
            })
        })
        .collect();
    for outcome in copilot_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let gemini_outcomes: Vec<(PathBuf, CachedParseOutcome)> = scan_result
        .get(ClientId::Gemini)
        .par_iter()
        .map(|path| {
            let outcome = load_or_parse_source_with_fingerprint_and_policy(
                path,
                &source_cache,
                pricing,
                message_cache::SourceFingerprint::from_path,
                |path| {
                    let parsed = sessions::gemini::parse_gemini_file_with_cache_status(path);
                    (parsed.messages, parsed.cacheable)
                },
            );
            (path.clone(), outcome)
        })
        .collect();
    for (path, outcome) in gemini_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        } else if outcome.invalidate_cache {
            source_cache.remove(&path);
        }
    }

    let cursor_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Cursor)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::cursor::parse_cursor_file(path)
            })
        })
        .collect();
    for outcome in cursor_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let warp_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Warp)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::warp::parse_warp_file(path)
            })
        })
        .collect();
    for outcome in warp_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let amp_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Amp)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::amp::parse_amp_file(path)
            })
        })
        .collect();
    for outcome in amp_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let codebuff_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Codebuff)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::codebuff::parse_codebuff_file(path)
            })
        })
        .collect();
    for outcome in codebuff_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let droid_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Droid)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::droid::parse_droid_file(path)
            })
        })
        .collect();
    for outcome in droid_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let openclaw_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::OpenClaw)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::openclaw::parse_openclaw_transcript(path)
            })
        })
        .collect();
    for outcome in openclaw_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let pi_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Pi)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::pi::parse_pi_file(path)
            })
        })
        .collect();
    for outcome in pi_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let kimi_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Kimi)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::kimi::parse_kimi_file(path)
            })
        })
        .collect();
    for outcome in kimi_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    // Parse Qwen files
    let qwen_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Qwen)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::qwen::parse_qwen_file(path)
            })
        })
        .collect();
    for outcome in qwen_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let roocode_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::RooCode)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::roocode::parse_roocode_file(path)
            })
        })
        .collect();
    for outcome in roocode_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let kilocode_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::KiloCode)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::kilocode::parse_kilocode_file(path)
            })
        })
        .collect();
    for outcome in kilocode_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let cline_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Cline)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::cline::parse_cline_file(path)
            })
        })
        .collect();
    for outcome in cline_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let jcode_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Jcode)
        .par_iter()
        .map(|path| {
            load_or_parse_source_with_fingerprint(
                path,
                &source_cache,
                pricing,
                message_cache::SourceFingerprint::from_jcode_path,
                sessions::jcode::parse_jcode_file,
            )
        })
        .collect();
    let mut jcode_seen: HashSet<String> = HashSet::new();
    for outcome in jcode_outcomes {
        all_messages.extend(
            outcome
                .messages
                .into_iter()
                .filter(|message| should_keep_deduped_message(&mut jcode_seen, message)),
        );
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    // micode: WAL-mode SQLite, cached via from_sqlite_path (-wal-aware).
    let micode_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::MiMoCode)
        .par_iter()
        .map(|path| {
            load_or_parse_sqlite_source(path, &source_cache, pricing, |path| {
                sessions::micode::parse_micode_sqlite(path)
            })
        })
        .collect();
    let mut micode_seen: HashSet<String> = HashSet::new();
    for outcome in micode_outcomes {
        all_messages.extend(
            outcome
                .messages
                .into_iter()
                .filter(|message| should_keep_deduped_message(&mut micode_seen, message)),
        );
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let mux_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Mux)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::mux::parse_mux_file(path)
            })
        })
        .collect();
    for outcome in mux_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    // Kilo CLI: SQLite database
    if let Some(db_path) = &scan_result.kilo_db {
        let kilo_messages: Vec<UnifiedMessage> = sessions::kilo::parse_kilo_sqlite(db_path)
            .into_iter()
            .map(|mut msg| {
                apply_pricing_if_available(&mut msg, pricing);
                msg
            })
            .collect();
        all_messages.extend(kilo_messages);
    }

    let mut hermes_seen: HashSet<String> = HashSet::new();
    for db_path in scan_result.hermes_db_paths() {
        let hermes_messages = parse_hermes_sqlite_with_pricing(&db_path, pricing);
        all_messages.extend(
            hermes_messages
                .into_iter()
                .filter(|message| should_keep_deduped_message(&mut hermes_seen, message)),
        );
    }

    if let Some(db_path) = &scan_result.goose_db {
        let goose_messages: Vec<UnifiedMessage> = sessions::goose::parse_goose_sqlite(db_path)
            .into_iter()
            .map(|mut msg| {
                apply_pricing_if_available(&mut msg, pricing);
                msg
            })
            .collect();
        all_messages.extend(goose_messages);
    }

    for db_path in scan_result.zed_db_paths() {
        let outcome = load_or_parse_sqlite_source(&db_path, &source_cache, pricing, |path| {
            sessions::zed::parse_zed_sqlite(path)
        });
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let kiro_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Kiro)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::kiro::parse_kiro_file(path)
            })
        })
        .collect();
    for outcome in kiro_outcomes {
        all_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    if let Some(db_path) = &scan_result.kiro_db {
        let kiro_db_messages: Vec<UnifiedMessage> = sessions::kiro::parse_kiro_sqlite(db_path)
            .into_iter()
            .map(|mut msg| {
                apply_pricing_if_available(&mut msg, pricing);
                msg
            })
            .collect();
        all_messages.extend(kiro_db_messages);
    }

    for source in &scan_result.crush_dbs {
        let crush_messages: Vec<UnifiedMessage> =
            sessions::crush::parse_crush_sqlite(&source.db_path)
                .into_iter()
                .map(|mut msg| {
                    msg.set_workspace(source.workspace_key.clone(), source.workspace_label.clone());
                    apply_pricing_if_available(&mut msg, pricing);
                    msg
                })
                .collect();
        all_messages.extend(crush_messages);
    }

    let antigravity_messages: Vec<UnifiedMessage> = scan_result
        .get(ClientId::Antigravity)
        .par_iter()
        .flat_map(|path| {
            sessions::antigravity::parse_antigravity_file(path)
                .into_iter()
                .map(|mut msg| {
                    apply_pricing_if_available(&mut msg, pricing);
                    msg
                })
                .collect::<Vec<_>>()
        })
        .collect();
    all_messages.extend(antigravity_messages);

    let antigravity_cli_messages: Vec<UnifiedMessage> = scan_result
        .get(ClientId::AntigravityCli)
        .par_iter()
        .flat_map(|path| {
            sessions::antigravity_cli::parse_antigravity_cli_file(path)
                .into_iter()
                .map(|mut msg| {
                    apply_pricing_if_available(&mut msg, pricing);
                    msg
                })
                .collect::<Vec<_>>()
        })
        .collect();
    all_messages.extend(antigravity_cli_messages);

    // Trae API dump uses exact dollar_float totals, so pricing lookup is not needed.
    let trae_messages: Vec<UnifiedMessage> = scan_result
        .get(ClientId::Trae)
        .par_iter()
        .flat_map(|path| sessions::trae::parse_trae_file("trae", path))
        .collect();
    let deduped_trae_messages = dedupe_latest_trae_messages(trae_messages);
    all_messages.extend(deduped_trae_messages);

    if include_synthetic {
        if let Some(db_path) = &scan_result.synthetic_db {
            let outcome = load_or_parse_sqlite_source(db_path, &source_cache, pricing, |path| {
                sessions::synthetic::parse_octofriend_sqlite(path)
            });
            all_messages.extend(outcome.messages);
            if let Some(entry) = outcome.cache_entry {
                source_cache.insert(entry);
            }
        }
    }

    // Filter BEFORE normalization so retain_for_requested_clients can see
    // original model/provider prefixes (e.g. "accounts/fireworks/models/…")
    // that is_synthetic_gateway relies on for gateway detection.
    if !include_all {
        let requested: HashSet<&str> = clients.iter().map(String::as_str).collect();
        all_messages.retain(|msg| {
            retain_for_requested_clients(&msg.client, &msg.model_id, &msg.provider_id, &requested)
        });
    }

    if include_synthetic {
        for msg in &mut all_messages {
            sessions::synthetic::normalize_synthetic_gateway_fields(
                &mut msg.model_id,
                &mut msg.provider_id,
            );
        }
    }

    source_cache.save_if_dirty();

    all_messages
}

fn dedupe_latest_trae_messages(mut messages: Vec<UnifiedMessage>) -> Vec<UnifiedMessage> {
    let mut latest_by_session: HashMap<String, UnifiedMessage> = HashMap::new();

    for message in messages.drain(..) {
        let session_id = message.session_id.clone();
        match latest_by_session.get_mut(&session_id) {
            Some(existing) => {
                let should_replace = message.timestamp > existing.timestamp
                    || (message.timestamp == existing.timestamp
                        && message.dedup_key.as_ref().is_some_and(|key| {
                            existing
                                .dedup_key
                                .as_ref()
                                .is_none_or(|existing_key| key > existing_key)
                        }));
                if should_replace {
                    *existing = message;
                }
            }
            None => {
                let _ = latest_by_session.insert(session_id, message);
            }
        }
    }

    let mut deduped: Vec<UnifiedMessage> = latest_by_session.into_values().collect();
    deduped.sort_unstable_by(|a, b| {
        a.session_id
            .cmp(&b.session_id)
            .then_with(|| a.timestamp.cmp(&b.timestamp))
    });
    deduped
}

fn filter_unified_messages(
    messages: Vec<UnifiedMessage>,
    options: &LocalParseOptions,
) -> Vec<UnifiedMessage> {
    let mut filtered = messages;

    if let Some(year) = &options.year {
        let year_prefix = format!("{}-", year);
        filtered.retain(|m| m.date.starts_with(&year_prefix));
    }

    if let Some(since) = &options.since {
        filtered.retain(|m| m.date.as_str() >= since.as_str());
    }

    if let Some(until) = &options.until {
        filtered.retain(|m| m.date.as_str() <= until.as_str());
    }

    filtered
}

fn workspace_bucket(msg: &UnifiedMessage) -> (String, Option<String>, String) {
    match (&msg.workspace_key, &msg.workspace_label) {
        (Some(key), Some(label)) => (key.clone(), Some(key.clone()), label.clone()),
        (Some(key), None) => (
            key.clone(),
            Some(key.clone()),
            sessions::workspace_label_from_key(key)
                .unwrap_or_else(|| UNKNOWN_WORKSPACE_LABEL.to_string()),
        ),
        _ => (
            UNKNOWN_WORKSPACE_GROUP_KEY.to_string(),
            None,
            UNKNOWN_WORKSPACE_LABEL.to_string(),
        ),
    }
}

fn aggregate_model_usage_entries(
    messages: Vec<UnifiedMessage>,
    group_by: &GroupBy,
) -> Vec<ModelUsage> {
    let mut model_map: HashMap<String, ModelUsage> = HashMap::new();

    for msg in messages {
        let normalized = normalize_model_for_grouping(&msg.model_id);
        let (workspace_group_key, workspace_key, workspace_label) = workspace_bucket(&msg);
        let key = match group_by {
            GroupBy::Model => normalized.clone(),
            GroupBy::ClientModel => format!("{}:{}", msg.client, normalized),
            GroupBy::ClientProviderModel => {
                format!("{}:{}:{}", msg.client, msg.provider_id, normalized)
            }
            GroupBy::WorkspaceModel => format!("{}:{}", workspace_group_key, normalized),
            GroupBy::Session => format!("{}:{}", msg.session_id, normalized),
            GroupBy::ClientSession => {
                format!("{}:{}:{}", msg.client, msg.session_id, normalized)
            }
        };
        let merge_clients = matches!(group_by, GroupBy::Model | GroupBy::WorkspaceModel);
        let session_grouped = matches!(group_by, GroupBy::Session | GroupBy::ClientSession);
        let entry = model_map.entry(key).or_insert_with(|| ModelUsage {
            client: msg.client.clone(),
            merged_clients: if merge_clients {
                Some(msg.client.clone())
            } else {
                None
            },
            workspace_key: if matches!(group_by, GroupBy::WorkspaceModel) {
                workspace_key.clone()
            } else {
                None
            },
            workspace_label: if matches!(group_by, GroupBy::WorkspaceModel) {
                Some(workspace_label.clone())
            } else {
                None
            },
            session_id: if session_grouped {
                Some(msg.session_id.clone())
            } else {
                None
            },
            model: normalized.clone(),
            provider: msg.provider_id.clone(),
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            message_count: 0,
            cost: 0.0,
            performance: ModelPerformance::default(),
        });

        if merge_clients {
            if !entry.client.split(", ").any(|s| s == msg.client) {
                entry.client = format!("{}, {}", entry.client, msg.client);
            }

            if let Some(merged_clients) = &mut entry.merged_clients {
                if !merged_clients.split(", ").any(|s| s == msg.client) {
                    *merged_clients = format!("{}, {}", merged_clients, msg.client);
                }
            }
        }

        if *group_by != GroupBy::ClientProviderModel
            && !entry.provider.split(", ").any(|p| p == msg.provider_id)
        {
            entry.provider = format!("{}, {}", entry.provider, msg.provider_id);
        }

        entry.input += msg.tokens.input;
        entry.output += msg.tokens.output;
        entry.cache_read += msg.tokens.cache_read;
        entry.cache_write += msg.tokens.cache_write;
        entry.reasoning += msg.tokens.reasoning;
        entry.message_count += msg.message_count.max(0);
        entry.cost += msg.cost;
        entry
            .performance
            .record_message(positive_token_total(&msg.tokens), msg.duration_ms);
    }

    let mut entries: Vec<ModelUsage> = model_map
        .into_values()
        .map(|mut entry| {
            let total_tokens = entry.input.max(0)
                + entry.output.max(0)
                + entry.cache_read.max(0)
                + entry.cache_write.max(0)
                + entry.reasoning.max(0);
            entry.performance.finalize(total_tokens);
            let mut providers: Vec<&str> = entry.provider.split(", ").collect();
            providers.sort_unstable();
            providers.dedup();
            entry.provider = providers.join(", ");
            entry
        })
        .collect();
    entries.sort_by(|a, b| match (a.cost.is_nan(), b.cost.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => b
            .cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal),
    });

    entries
}

fn positive_token_total(tokens: &TokenBreakdown) -> i64 {
    tokens.input.max(0)
        + tokens.output.max(0)
        + tokens.cache_read.max(0)
        + tokens.cache_write.max(0)
        + tokens.reasoning.max(0)
}

/// Returns the effective client list for a report: uses the caller-supplied
/// list when present, or falls back to all known clients + "synthetic".
fn resolve_report_clients(options: &ReportOptions) -> Vec<String> {
    options.clients.clone().unwrap_or_else(|| {
        let mut clients: Vec<String> = ClientId::ALL
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        clients.push("synthetic".to_string());
        clients
    })
}

/// Returns `true` when the message should pass the cross-file dedup gate for
/// lanes that track per-client seen keys.
///
/// Uses `contains` before `insert` to avoid cloning the key on the hot path
/// when the key is already present (i.e. the message is a duplicate).
fn dedup_gate_passes(key: &str, seen: &mut HashSet<String>) -> bool {
    if seen.contains(key) {
        return false;
    }
    seen.insert(key.to_owned());
    true
}

pub async fn get_model_report(options: ReportOptions) -> Result<ModelReport, String> {
    let start = Instant::now();

    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = resolve_report_clients(&options);

    let pricing = load_pricing_for_local_parse().await;
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix { if !m.date.starts_with(yp.as_str()) { return false; } }
        if let Some(ref s) = since_s { if m.date.as_str() < s.as_str() { return false; } }
        if let Some(ref u) = until_s { if m.date.as_str() > u.as_str() { return false; } }
        true
    };
    let mut model_msgs: Vec<UnifiedMessage> = Vec::new();
    scan_messages_streaming(
        &home_dir, &clients, pricing.as_deref(), options.use_env_roots, &options.scanner_settings,
        &msg_filter,
        &mut |m: &UnifiedMessage| { model_msgs.push(m.clone()); },
    );
    let entries = aggregate_model_usage_entries(model_msgs, &options.group_by);

    let total_input: i64 = entries.iter().map(|e| e.input).sum();
    let total_output: i64 = entries.iter().map(|e| e.output).sum();
    let total_cache_read: i64 = entries.iter().map(|e| e.cache_read).sum();
    let total_cache_write: i64 = entries.iter().map(|e| e.cache_write).sum();
    let total_messages: i32 = entries.iter().map(|e| e.message_count).sum();
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum();

    Ok(ModelReport {
        entries,
        total_input,
        total_output,
        total_cache_read,
        total_cache_write,
        total_messages,
        total_cost,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

#[derive(Default)]
struct MonthAggregator {
    models: HashSet<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    message_count: i32,
    cost: f64,
}

pub async fn get_monthly_report(options: ReportOptions) -> Result<MonthlyReport, String> {
    let start = Instant::now();

    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = resolve_report_clients(&options);

    let pricing = load_pricing_for_local_parse().await;
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix { if !m.date.starts_with(yp.as_str()) { return false; } }
        if let Some(ref s) = since_s { if m.date.as_str() < s.as_str() { return false; } }
        if let Some(ref u) = until_s { if m.date.as_str() > u.as_str() { return false; } }
        true
    };

    let mut month_map: HashMap<String, MonthAggregator> = HashMap::new();

    scan_messages_streaming(
        &home_dir, &clients, pricing.as_deref(), options.use_env_roots, &options.scanner_settings,
        &msg_filter,
        &mut |msg: &UnifiedMessage| {
            let month = if msg.date.len() >= 7 {
                msg.date[..7].to_string()
            } else {
                return;
            };

            let entry = month_map.entry(month).or_default();

            entry
                .models
                .insert(normalize_model_for_grouping(&msg.model_id));
            entry.input += msg.tokens.input;
            entry.output += msg.tokens.output;
            entry.cache_read += msg.tokens.cache_read;
            entry.cache_write += msg.tokens.cache_write;
            entry.message_count += msg.message_count.max(0);
            entry.cost += msg.cost;
        },
    );

    let mut entries: Vec<MonthlyUsage> = month_map
        .into_iter()
        .map(|(month, agg)| MonthlyUsage {
            month,
            models: agg.models.into_iter().collect(),
            input: agg.input,
            output: agg.output,
            cache_read: agg.cache_read,
            cache_write: agg.cache_write,
            message_count: agg.message_count,
            cost: agg.cost,
        })
        .collect();

    entries.sort_by(|a, b| a.month.cmp(&b.month));

    let total_cost: f64 = entries.iter().map(|e| e.cost).sum();

    Ok(MonthlyReport {
        entries,
        total_cost,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

#[derive(Debug, Default, Clone)]
struct AgentAccumulator {
    clients: std::collections::BTreeSet<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    cost: f64,
    messages: i32,
}

impl AgentAccumulator {
    // Plain `+=` (not saturating_add) folds tokens EXACTLY like the model
    // report's `aggregate_model_usage_entries`; the per-client parity those two
    // reports must hold depends on identical arithmetic. `message_count.max(0)`
    // matches the model/monthly aggregators (not the sessionizer's `.max(1)`).
    fn add(&mut self, msg: &UnifiedMessage) {
        self.clients.insert(msg.client.clone());
        self.input += msg.tokens.input;
        self.output += msg.tokens.output;
        self.cache_read += msg.tokens.cache_read;
        self.cache_write += msg.tokens.cache_write;
        self.reasoning += msg.tokens.reasoning;
        self.cost += msg.cost;
        self.messages += msg.message_count.max(0);
    }
}

/// Agent bucket key for a message: the normalized sub-agent name, or "Main"
/// when the message carries no agent attribution. Mirrors the old FFI
/// `agents_report.rs` bucketing so the report stays byte-stable across the
/// streaming migration.
fn agent_bucket_key(msg: &UnifiedMessage) -> String {
    match msg.agent.as_deref() {
        Some(raw) if !raw.trim().is_empty() => sessions::normalize_agent_name(raw),
        _ => "Main".to_string(),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentReportEntry {
    pub agent: String,
    pub clients: Vec<String>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub cost: f64,
    pub messages: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentReport {
    pub entries: Vec<AgentReportEntry>,
    pub total_cost: f64,
    pub total_messages: i32,
    pub processing_time_ms: u32,
}

/// Per-sub-agent usage breakdown, ranked by cost then total tokens, with
/// unattributed messages folded into a single "Main" bucket.
///
/// Folds the SAME deduped, per-client-gated, priced message stream that the
/// model/graph/hourly/monthly reports consume (`scan_messages_streaming`), so
/// the agents report now agrees with them on copilot/codebuff/kimi/cursor/warp
/// /amp/droid/etc. totals (issue #6 — previously the agents report alone rode
/// the materialized path and skipped per-client cross-file dedup). Mirrors
/// `get_monthly_report`'s fold-in-sink shape.
pub async fn get_agents_report(options: ReportOptions) -> Result<AgentReport, String> {
    let start = Instant::now();

    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = resolve_report_clients(&options);

    let pricing = load_pricing_for_local_parse().await;
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix { if !m.date.starts_with(yp.as_str()) { return false; } }
        if let Some(ref s) = since_s { if m.date.as_str() < s.as_str() { return false; } }
        if let Some(ref u) = until_s { if m.date.as_str() > u.as_str() { return false; } }
        true
    };

    let mut by_agent: HashMap<String, AgentAccumulator> = HashMap::new();

    scan_messages_streaming(
        &home_dir, &clients, pricing.as_deref(), options.use_env_roots, &options.scanner_settings,
        &msg_filter,
        &mut |msg: &UnifiedMessage| {
            by_agent.entry(agent_bucket_key(msg)).or_default().add(msg);
        },
    );

    let mut entries: Vec<AgentReportEntry> = by_agent
        .into_iter()
        .map(|(agent, agg)| AgentReportEntry {
            agent,
            clients: agg.clients.into_iter().collect(),
            input: agg.input,
            output: agg.output,
            cache_read: agg.cache_read,
            cache_write: agg.cache_write,
            reasoning: agg.reasoning,
            cost: agg.cost,
            messages: agg.messages,
        })
        .collect();

    // Cost desc, then total-tokens desc — matches the old FFI agents report
    // ordering. This token-total formula MUST stay identical to the `total`
    // computed in the FFI mapper (crates/tb_core_ffi/src/agents_report.rs).
    entries.sort_by(|a, b| {
        let a_total = a.input + a.output + a.cache_read + a.cache_write + a.reasoning;
        let b_total = b.input + b.output + b.cache_read + b.cache_write + b.reasoning;
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b_total.cmp(&a_total))
    });

    let total_cost: f64 = entries.iter().map(|e| e.cost).sum();
    let total_messages: i32 = entries.iter().map(|e| e.messages).sum();

    Ok(AgentReport {
        entries,
        total_cost,
        total_messages,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

#[derive(Default)]
struct HourAggregator {
    clients: HashSet<String>,
    models: HashSet<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    message_count: i32,
    turn_count: i32,
    cost: f64,
}

/// Generate hourly usage report, keyed by "YYYY-MM-DD HH:00".
///
/// Derives the hour slot from `UnifiedMessage.timestamp` (Unix ms).
/// Falls back to date + "00:00" when timestamp is zero or missing.
pub async fn get_hourly_report(options: ReportOptions) -> Result<HourlyReport, String> {
    use chrono::{Local, TimeZone};

    let start = Instant::now();

    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = resolve_report_clients(&options);

    let pricing = load_pricing_for_local_parse().await;
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix { if !m.date.starts_with(yp.as_str()) { return false; } }
        if let Some(ref s) = since_s { if m.date.as_str() < s.as_str() { return false; } }
        if let Some(ref u) = until_s { if m.date.as_str() > u.as_str() { return false; } }
        true
    };

    let mut hour_map: HashMap<String, HourAggregator> = HashMap::new();

    scan_messages_streaming(
        &home_dir, &clients, pricing.as_deref(), options.use_env_roots, &options.scanner_settings,
        &msg_filter,
        &mut |msg: &UnifiedMessage| {
            let hour_key = if msg.timestamp > 0 {
                let ts_secs = msg.timestamp / 1000;
                match Local.timestamp_opt(ts_secs, 0) {
                    chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:00").to_string(),
                    _ => format!("{} 00:00", msg.date),
                }
            } else {
                format!("{} 00:00", msg.date)
            };

            let entry = hour_map.entry(hour_key).or_default();

            entry.clients.insert(msg.client.clone());
            entry
                .models
                .insert(normalize_model_for_grouping(&msg.model_id));
            entry.input += msg.tokens.input;
            entry.output += msg.tokens.output;
            entry.cache_read += msg.tokens.cache_read;
            entry.cache_write += msg.tokens.cache_write;
            entry.reasoning += msg.tokens.reasoning;
            entry.message_count += msg.message_count.max(0);
            if msg.is_turn_start {
                entry.turn_count += 1;
            }
            entry.cost += msg.cost;
        },
    );

    let mut entries: Vec<HourlyUsage> = hour_map
        .into_iter()
        .map(|(hour, agg)| HourlyUsage {
            hour,
            clients: {
                let mut v: Vec<String> = agg.clients.into_iter().collect();
                v.sort();
                v
            },
            models: {
                let mut v: Vec<String> = agg.models.into_iter().collect();
                v.sort();
                v
            },
            input: agg.input,
            output: agg.output,
            cache_read: agg.cache_read,
            cache_write: agg.cache_write,
            message_count: agg.message_count,
            turn_count: agg.turn_count,
            reasoning: agg.reasoning,
            cost: agg.cost,
        })
        .collect();

    entries.sort_by(|a, b| a.hour.cmp(&b.hour));

    let total_cost: f64 = entries.iter().map(|e| e.cost).sum();

    Ok(HourlyReport {
        entries,
        total_cost,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

/// Streaming scan driver — mirrors `parse_all_messages_with_pricing_with_env_strategy`
/// but never materialises a full-history `Vec<UnifiedMessage>`.
///
/// For file-backed lanes with a cache hit: iterates `cached.messages` by
/// reference (one clone per message, not one clone of the whole Vec), applies
/// pricing on the temporary copy, and immediately calls `sink`.  Peak memory
/// per lane is O(messages_in_that_file), not O(sum_of_all_files).
///
/// Cross-file dedup_key gate and trae keep-latest buffer both live here so
/// both the day-aggregator and the sessionize accumulator see a consistent,
/// de-duplicated stream.  `filter` is applied after dedup gate.
///
/// `sink` receives each final message exactly once.  Trae winners are flushed
/// at the very end (after all other lanes), matching `StreamingAggregator`
/// semantics.
fn scan_messages_streaming<F, S>(
    home_dir: &str,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
    use_env_roots: bool,
    scanner_settings: &scanner::ScannerSettings,
    filter: &F,
    sink: &mut S,
)
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage),
{
    let scan_result = scanner::scan_all_clients_with_scanner_settings(
        home_dir,
        clients,
        use_env_roots,
        scanner_settings,
    );
    let headless_roots = scanner::headless_roots_with_env_strategy(home_dir, use_env_roots);
    let mut source_cache = message_cache::SourceMessageCache::load();
    source_cache.prune_missing_files();

    let include_all = clients.is_empty();
    let include_synthetic = include_all || clients.iter().any(|c| c == "synthetic");
    let requested: HashSet<&str> = clients.iter().map(String::as_str).collect();

    // Inline helper: should this message pass the client filter?
    let passes_client = |m: &UnifiedMessage| -> bool {
        include_all
            || retain_for_requested_clients(&m.client, &m.model_id, &m.provider_id, &requested)
    };

    // Each client lane owns its dedup set (see `simple_lane!` / the Gemini
    // block below). Sharing one set across clients would let a dedup_key from
    // one client suppress an identical key from another — copilot uses
    // `trace:span` keys but codebuff/kimi use raw upstream message ids with no
    // client namespace, so a cross-client collision is possible. Per-client
    // sets match the claude/codex/hermes/opencode lanes above.

    // Trae keep-latest buffer — flushed after all other lanes.
    let mut trae_latest: HashMap<String, UnifiedMessage> = HashMap::new();

    // ---- OpenCode SQLite ----
    let mut opencode_seen: HashSet<String> = HashSet::new();
    for db_path in &scan_result.opencode_dbs {
        for mut m in sessions::opencode::parse_opencode_sqlite(db_path) {
            apply_pricing_if_available(&mut m, pricing);
            let keep = m.dedup_key.as_ref().is_none_or(|k| dedup_gate_passes(k, &mut opencode_seen));
            if keep && passes_client(&m) && filter(&m) { sink(&m); }
        }
    }
    // OpenCode JSON legacy
    let opencode_parsed: Vec<Vec<UnifiedMessage>> = scan_result
        .get(ClientId::OpenCode)
        .par_iter()
        .map(|path| sessions::opencode::parse_opencode_file(path).into_iter().collect::<Vec<_>>())
        .collect();
    for msgs in opencode_parsed {
        for mut m in msgs {
            apply_pricing_if_available(&mut m, pricing);
            let keep = m.dedup_key.as_ref().is_none_or(|k| dedup_gate_passes(k, &mut opencode_seen));
            if keep && passes_client(&m) && filter(&m) { sink(&m); }
        }
    }

    // ---- Claude Code JSONL (cache-aware, reference-iterate on hit) ----
    let claude_home = PathBuf::from(home_dir);
    let mut claude_seen: HashSet<String> = HashSet::new();
    for path in scan_result.get(ClientId::Claude) {
        let fp = message_cache::SourceFingerprint::from_claude_code_path_with_home(path, Some(&claude_home));
        let cache_hit = fp.as_ref().and_then(|fp| source_cache.get(path).filter(|c| &c.fingerprint == fp && !c.messages.is_empty()));
        if let Some(cached) = cache_hit {
            for msg in cached.messages.iter() {
                let mut m = msg.clone();
                m.refresh_derived_fields();
                apply_pricing_if_available(&mut m, pricing);
                if !passes_client(&m) { continue; }
                let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut claude_seen));
                if keep && filter(&m) { sink(&m); }
            }
        } else {
            let msgs = sessions::claudecode::parse_claude_file_with_home(path, Some(&claude_home));
            for mut m in msgs {
                apply_pricing_if_available(&mut m, pricing);
                if !passes_client(&m) { continue; }
                let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut claude_seen));
                if keep && filter(&m) { sink(&m); }
            }
        }
    }

    // ---- Codex JSONL (cache-aware, headless-aware) ----
    let mut codex_seen: HashSet<String> = HashSet::new();
    for path in scan_result.get(ClientId::Codex) {
        let fp = message_cache::SourceFingerprint::from_path(path);
        let cache_hit = fp.as_ref().and_then(|fp| source_cache.get(path).filter(|c| &c.fingerprint == fp));
        if let Some(cached) = cache_hit {
            let is_headless = is_headless_path(path, &headless_roots);
            let fallback_ts = sessions::utils::file_modified_timestamp_ms(path);
            let fti = &cached.fallback_timestamp_indices;
            for (idx, msg) in cached.messages.iter().enumerate() {
                let mut m = msg.clone();
                if fti.contains(&idx) { m.set_timestamp(fallback_ts); } else { m.refresh_derived_fields(); }
                apply_pricing_if_available(&mut m, pricing);
                apply_headless_agent(&mut m, is_headless);
                if !passes_client(&m) { continue; }
                let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut codex_seen));
                if keep && filter(&m) { sink(&m); }
            }
        } else {
            let is_headless = is_headless_path(path, &headless_roots);
            let fallback_ts = sessions::utils::file_modified_timestamp_ms(path);
            let parsed = sessions::codex::parse_codex_file_incremental(
                path, 0, sessions::codex::CodexParseState::default(),
            );
            let mut msgs = parsed.messages;
            for idx in &parsed.fallback_timestamp_indices {
                if let Some(m) = msgs.get_mut(*idx) { m.set_timestamp(fallback_ts); }
            }
            for mut m in msgs {
                apply_pricing_if_available(&mut m, pricing);
                apply_headless_agent(&mut m, is_headless);
                if !passes_client(&m) { continue; }
                let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut codex_seen));
                if keep && filter(&m) { sink(&m); }
            }
        }
    }

    // ---- Simple file-backed lanes (per-lane dedup set, cache-aware) ----
    // Cache hit  → iterate cached.messages by reference (one clone per message),
    //              refresh_derived_fields, apply pricing, dedup, filter, sink.
    // Cache miss → par-collect parse results, then sequential: writeback + emit.
    // Mirrors load_or_parse_source semantics from parse_all_messages_with_pricing_with_env_strategy.
    macro_rules! simple_lane {
        // Default: fingerprint the source file itself.
        ($client_id:expr, $parse_fn:expr) => {
            simple_lane!(
                $client_id,
                $parse_fn,
                message_cache::SourceFingerprint::from_path
            )
        };
        // Custom fingerprint fn — for sources whose cache validity depends on a
        // sibling file (e.g. jcode's `.journal.jsonl`), so a sibling-only write
        // still invalidates the cache instead of serving stale data.
        ($client_id:expr, $parse_fn:expr, $fingerprint_fn:expr) => {{
            // Per-lane dedup set: persists across this client's files, never
            // shared with other clients (see the note above the trae buffer).
            let mut seen_keys: HashSet<String> = HashSet::new();
            // Separate paths into cache-hit (emit immediately) vs cache-miss (par-parse).
            let mut miss_paths: Vec<&PathBuf> = Vec::new();
            for path in scan_result.get($client_id) {
                let fp = $fingerprint_fn(path);
                let cache_hit = fp.as_ref().and_then(|fp| {
                    source_cache.get(path).filter(|c| c.fingerprint == *fp && !c.messages.is_empty())
                });
                if let Some(cached) = cache_hit {
                    for msg in cached.messages.iter() {
                        let mut m = msg.clone();
                        m.refresh_derived_fields();
                        apply_pricing_if_available(&mut m, pricing);
                        if !passes_client(&m) { continue; }
                        let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut seen_keys));
                        if keep && filter(&m) { sink(&m); }
                    }
                } else {
                    miss_paths.push(path);
                }
            }
            // Par-parse all cache-miss files, then sequential writeback + emit.
            let parsed_misses: Vec<(&PathBuf, Vec<UnifiedMessage>)> = miss_paths
                .par_iter()
                .map(|path| (*path, $parse_fn(*path)))
                .collect();
            for (path, msgs) in parsed_misses {
                if !msgs.is_empty() {
                    if let Some(fp) = $fingerprint_fn(path) {
                        let entry = message_cache::CachedSourceEntry::new(
                            path, fp, msgs.clone(), Vec::new(), None,
                        );
                        source_cache.insert(entry);
                    }
                }
                for mut m in msgs {
                    apply_pricing_if_available(&mut m, pricing);
                    if !passes_client(&m) { continue; }
                    let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut seen_keys));
                    if keep && filter(&m) { sink(&m); }
                }
            }
        }};
    }
    simple_lane!(ClientId::Copilot,   sessions::copilot::parse_copilot_file);
    simple_lane!(ClientId::Cursor,    sessions::cursor::parse_cursor_file);
    simple_lane!(ClientId::Warp,      sessions::warp::parse_warp_file);
    simple_lane!(ClientId::Amp,       sessions::amp::parse_amp_file);
    simple_lane!(ClientId::Codebuff,  sessions::codebuff::parse_codebuff_file);
    simple_lane!(ClientId::Droid,     sessions::droid::parse_droid_file);
    simple_lane!(ClientId::OpenClaw,  sessions::openclaw::parse_openclaw_transcript);
    simple_lane!(ClientId::Pi,        sessions::pi::parse_pi_file);
    simple_lane!(ClientId::Kimi,      sessions::kimi::parse_kimi_file);
    simple_lane!(ClientId::Qwen,      sessions::qwen::parse_qwen_file);
    simple_lane!(ClientId::RooCode,   sessions::roocode::parse_roocode_file);
    simple_lane!(ClientId::KiloCode,  sessions::kilocode::parse_kilocode_file);
    simple_lane!(ClientId::Cline,     sessions::cline::parse_cline_file);
    simple_lane!(
        ClientId::Jcode,
        sessions::jcode::parse_jcode_file,
        message_cache::SourceFingerprint::from_jcode_path
    );
    // micode is WAL-mode SQLite; fingerprint via from_sqlite_path so a `-wal`
    // write invalidates the cache. Its authoritative per-message cost survives
    // because apply_pricing only overwrites when the model resolves to a
    // non-zero price (MiMo models are not in the pricing dataset) — this
    // mirrors upstream, which passes pricing through the same loader.
    simple_lane!(
        ClientId::MiMoCode,
        sessions::micode::parse_micode_sqlite,
        message_cache::SourceFingerprint::from_sqlite_path
    );
    simple_lane!(ClientId::Mux,       sessions::mux::parse_mux_file);
    simple_lane!(ClientId::Kiro,      sessions::kiro::parse_kiro_file);

    // ---- Gemini (cache-aware with invalidate_cache semantics) ----
    // Uses load_or_parse_source_with_fingerprint_and_policy equivalent:
    // cacheable=false → remove stale cache entry (invalidate_cache).
    {
        // Per-lane dedup set (Gemini currently emits no dedup_key, so this is a
        // no-op today, but keeps the lane consistent and collision-proof).
        let mut seen_keys: HashSet<String> = HashSet::new();
        let mut gemini_miss_paths: Vec<&PathBuf> = Vec::new();
        for path in scan_result.get(ClientId::Gemini) {
            let fp = message_cache::SourceFingerprint::from_path(path);
            let cache_hit = fp.as_ref().and_then(|fp| {
                source_cache.get(path).filter(|c| c.fingerprint == *fp && !c.messages.is_empty())
            });
            if let Some(cached) = cache_hit {
                for msg in cached.messages.iter() {
                    let mut m = msg.clone();
                    m.refresh_derived_fields();
                    apply_pricing_if_available(&mut m, pricing);
                    if !passes_client(&m) { continue; }
                    let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut seen_keys));
                    if keep && filter(&m) { sink(&m); }
                }
            } else {
                gemini_miss_paths.push(path);
            }
        }
        let gemini_parsed: Vec<(&PathBuf, sessions::gemini::GeminiParseResult)> = gemini_miss_paths
            .par_iter()
            .map(|path| (*path, sessions::gemini::parse_gemini_file_with_cache_status(path)))
            .collect();
        for (path, parsed) in gemini_parsed {
            if parsed.cacheable && !parsed.messages.is_empty() {
                if let Some(fp) = message_cache::SourceFingerprint::from_path(path) {
                    let entry = message_cache::CachedSourceEntry::new(
                        path, fp, parsed.messages.clone(), Vec::new(), None,
                    );
                    source_cache.insert(entry);
                }
            } else if !parsed.cacheable {
                source_cache.remove(path);
            }
            for mut m in parsed.messages {
                apply_pricing_if_available(&mut m, pricing);
                if !passes_client(&m) { continue; }
                let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut seen_keys));
                if keep && filter(&m) { sink(&m); }
            }
        }
    }

    // ---- Kilo SQLite ----
    if let Some(db_path) = &scan_result.kilo_db {
        for mut m in sessions::kilo::parse_kilo_sqlite(db_path) {
            apply_pricing_if_available(&mut m, pricing);
            if passes_client(&m) && filter(&m) { sink(&m); }
        }
    }

    // ---- Hermes SQLite (own dedup set) ----
    {
        let mut hermes_seen: HashSet<String> = HashSet::new();
        for db_path in scan_result.hermes_db_paths() {
            for m in parse_hermes_sqlite_with_pricing(&db_path, pricing) {
                if !passes_client(&m) { continue; }
                let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut hermes_seen));
                if keep && filter(&m) { sink(&m); }
            }
        }
    }

    // ---- Antigravity CLI (.db protobuf, own dedup set on responseId) ----
    {
        let mut antigravity_cli_seen: HashSet<String> = HashSet::new();
        for path in scan_result.get(ClientId::AntigravityCli) {
            for mut m in sessions::antigravity_cli::parse_antigravity_cli_file(path) {
                apply_pricing_if_available(&mut m, pricing);
                if !passes_client(&m) { continue; }
                // A responseId is unique only within a conversation DB (the
                // parser already drops repeats per-file), so namespace the
                // cross-file gate by session to avoid collapsing two independent
                // conversations that happen to reuse a responseId. Upstream has
                // no cross-file gate here at all; this keeps the streaming lane's
                // numbers identical to it while staying collision-proof.
                let keep = m.dedup_key.as_ref().is_none_or(|k| {
                    k.is_empty()
                        || dedup_gate_passes(
                            &format!("{}:{}", m.session_id, k),
                            &mut antigravity_cli_seen,
                        )
                });
                if keep && filter(&m) { sink(&m); }
            }
        }
    }

    // ---- Goose SQLite ----
    if let Some(db_path) = &scan_result.goose_db {
        for mut m in sessions::goose::parse_goose_sqlite(db_path) {
            apply_pricing_if_available(&mut m, pricing);
            if passes_client(&m) && filter(&m) { sink(&m); }
        }
    }

    // ---- Zed SQLite (cache-aware reference-iterate) ----
    for db_path in scan_result.zed_db_paths() {
        let fp = message_cache::SourceFingerprint::from_sqlite_path(&db_path);
        let cache_hit = fp.as_ref().and_then(|fp| source_cache.get(&db_path).filter(|c| &c.fingerprint == fp));
        if let Some(cached) = cache_hit {
            for msg in cached.messages.iter() {
                let mut m = msg.clone();
                m.refresh_derived_fields();
                apply_pricing_if_available(&mut m, pricing);
                if passes_client(&m) && filter(&m) { sink(&m); }
            }
        } else {
            for mut m in sessions::zed::parse_zed_sqlite(&db_path) {
                apply_pricing_if_available(&mut m, pricing);
                if passes_client(&m) && filter(&m) { sink(&m); }
            }
        }
    }

    // ---- Kiro SQLite ----
    if let Some(db_path) = &scan_result.kiro_db {
        for mut m in sessions::kiro::parse_kiro_sqlite(db_path) {
            apply_pricing_if_available(&mut m, pricing);
            if passes_client(&m) && filter(&m) { sink(&m); }
        }
    }

    // ---- Crush SQLite ----
    for source in &scan_result.crush_dbs {
        for mut m in sessions::crush::parse_crush_sqlite(&source.db_path) {
            m.set_workspace(source.workspace_key.clone(), source.workspace_label.clone());
            apply_pricing_if_available(&mut m, pricing);
            if passes_client(&m) && filter(&m) { sink(&m); }
        }
    }

    // ---- Antigravity ----
    {
        let parsed: Vec<Vec<UnifiedMessage>> = scan_result
            .get(ClientId::Antigravity)
            .par_iter()
            .map(|path| sessions::antigravity::parse_antigravity_file(path))
            .collect();
        for msgs in parsed {
            for mut m in msgs {
                apply_pricing_if_available(&mut m, pricing);
                if passes_client(&m) && filter(&m) { sink(&m); }
            }
        }
    }

    // ---- Trae (keep-latest per session_id, buffer — flushed below) ----
    {
        let trae_raw: Vec<UnifiedMessage> = scan_result
            .get(ClientId::Trae)
            .par_iter()
            .flat_map(|path| sessions::trae::parse_trae_file("trae", path))
            .collect();
        for m in trae_raw {
            let entry = trae_latest.entry(m.session_id.clone());
            match entry {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let existing = slot.get();
                    let replace = m.timestamp > existing.timestamp
                        || (m.timestamp == existing.timestamp
                            && m.dedup_key.as_ref().is_some_and(|k| {
                                existing.dedup_key.as_ref().is_none_or(|ek| k.as_str() > ek.as_str())
                            }));
                    if replace { *slot.get_mut() = m; }
                }
                std::collections::hash_map::Entry::Vacant(slot) => { slot.insert(m); }
            }
        }
    }

    // ---- Synthetic ----
    if let Some(db_path) = scan_result.synthetic_db.as_ref().filter(|_| include_synthetic) {
        let fp = message_cache::SourceFingerprint::from_sqlite_path(db_path);
        let cache_hit = fp.as_ref().and_then(|fp| source_cache.get(db_path).filter(|c| &c.fingerprint == fp));
        if let Some(cached) = cache_hit {
            for msg in cached.messages.iter() {
                let mut m = msg.clone();
                m.refresh_derived_fields();
                apply_pricing_if_available(&mut m, pricing);
                sessions::synthetic::normalize_synthetic_gateway_fields(&mut m.model_id, &mut m.provider_id);
                if passes_client(&m) && filter(&m) { sink(&m); }
            }
        } else {
            for mut m in sessions::synthetic::parse_octofriend_sqlite(db_path) {
                apply_pricing_if_available(&mut m, pricing);
                sessions::synthetic::normalize_synthetic_gateway_fields(&mut m.model_id, &mut m.provider_id);
                if passes_client(&m) && filter(&m) { sink(&m); }
            }
        }
    }

    // ---- Flush trae keep-latest (after all other lanes) ----
    for m in trae_latest.into_values() {
        if passes_client(&m) && filter(&m) { sink(&m); }
    }

    source_cache.save_if_dirty();
}


async fn generate_graph_with_loaded_pricing(
    options: ReportOptions,
    pricing: Option<&pricing::PricingService>,
) -> Result<GraphResult, String> {
    let start = Instant::now();

    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = resolve_report_clients(&options);

    // Build filter closure from report options (year/since/until).
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix {
            if !m.date.starts_with(yp.as_str()) { return false; }
        }
        if let Some(ref s) = since_s {
            if m.date.as_str() < s.as_str() { return false; }
        }
        if let Some(ref u) = until_s {
            if m.date.as_str() > u.as_str() { return false; }
        }
        true
    };

    // Dual-sink: day aggregator + sessionize accumulator fed in one pass.
    // scan_messages_streaming handles all dedup (trae keep-latest + dedup_key gate).
    // StreamingAggregator.feed_pre_deduped() bypasses its internal dedup gate
    // since the driver already guarantees uniqueness.
    let mut day_agg = aggregator::StreamingAggregator::new();
    let mut sess_agg = sessionize::SessionizeAccumulator::new();

    scan_messages_streaming(
        &home_dir,
        &clients,
        pricing,
        options.use_env_roots,
        &options.scanner_settings,
        &msg_filter,
        &mut |m: &UnifiedMessage| {
            day_agg.feed_pre_deduped(m);
            sess_agg.feed(m);
        },
    );

    let contributions = day_agg.finalize();
    let intervals = sess_agg.finalize(sessionize::DEFAULT_IDLE_GAP_MS);
    let time_metrics =
        sessionize::compute_time_metrics(&intervals, sessionize::DEFAULT_IDLE_GAP_MS);
    let daily_active_time = sessionize::compute_daily_active_time(&intervals);

    let processing_time_ms = start.elapsed().as_millis() as u32;
    let mut result = aggregator::generate_graph_result(contributions, processing_time_ms);
    result.time_metrics = Some(time_metrics);

    for contribution in &mut result.contributions {
        if let Some(&ms) = daily_active_time.get(&contribution.date) {
            contribution.active_time_ms = Some(ms);
        }
    }

    Ok(result)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TimeMetricsReport {
    pub metrics: sessionize::TimeMetrics,
    pub processing_time_ms: u32,
}

pub async fn get_time_metrics_report(options: ReportOptions) -> Result<TimeMetricsReport, String> {
    let start = Instant::now();

    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = resolve_report_clients(&options);

    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix { if !m.date.starts_with(yp.as_str()) { return false; } }
        if let Some(ref s) = since_s { if m.date.as_str() < s.as_str() { return false; } }
        if let Some(ref u) = until_s { if m.date.as_str() > u.as_str() { return false; } }
        true
    };
    let mut sess_agg = sessionize::SessionizeAccumulator::new();
    scan_messages_streaming(
        &home_dir, &clients, None, options.use_env_roots, &options.scanner_settings,
        &msg_filter,
        &mut |m: &UnifiedMessage| { sess_agg.feed(m); },
    );
    let intervals = sess_agg.finalize(sessionize::DEFAULT_IDLE_GAP_MS);
    let metrics = sessionize::compute_time_metrics(&intervals, sessionize::DEFAULT_IDLE_GAP_MS);

    Ok(TimeMetricsReport {
        metrics,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

pub async fn generate_graph(options: ReportOptions) -> Result<GraphResult, String> {
    let pricing = pricing::PricingService::get_or_init().await?;
    generate_graph_with_loaded_pricing(options, Some(&pricing)).await
}

pub async fn generate_local_graph_report(options: ReportOptions) -> Result<GraphResult, String> {
    let pricing = load_pricing_for_local_parse().await;
    generate_graph_with_loaded_pricing(options, pricing.as_deref()).await
}

/// Streaming graph entry-point.
///
/// Accepts an already-parsed message slice, applies an optional `since`
/// date prefix filter (`msg.date.as_str() >= since`), then folds through
/// `StreamingAggregator` and wraps the result via
/// `aggregator::generate_graph_result`.
pub fn build_graph_result_from_messages(
    messages: &[UnifiedMessage],
    since: Option<&str>,
) -> GraphResult {
    let iter = messages.iter().filter(|msg| {
        since.is_none_or(|s| msg.date.as_str() >= s)
    });
    let contributions = aggregator::fold_messages_iter(iter);
    aggregator::generate_graph_result(contributions, 0)
}

fn is_headless_path(path: &Path, headless_roots: &[PathBuf]) -> bool {
    headless_roots.iter().any(|root| path.starts_with(root))
}

fn apply_headless_agent(message: &mut UnifiedMessage, is_headless: bool) {
    if is_headless && message.agent.is_none() {
        message.agent = Some("headless".to_string());
    }
}

fn pricing_multiplier(message: &UnifiedMessage) -> f64 {
    // Zed bills hosted models at provider list price + 10%.
    // Source: https://zed.dev/docs/ai/plans-and-usage and https://zed.dev/docs/ai/models
    //
    // The multiplier is keyed on the message's `provider_id`, not on the
    // provenance of the matched LiteLLM pricing row. Today this is safe because
    // tokscale's bundled LiteLLM dataset only carries upstream-provider rows
    // (anthropic, openai, google) for the underlying models. If a future
    // LiteLLM update adds rows under provider `zed.dev` that already include
    // Zed's markup, this function would double-bill — revisit by threading
    // the matched-price provenance through `apply_pricing_if_available`.
    if message.client == "zed"
        && message
            .provider_id
            .eq_ignore_ascii_case(sessions::zed::ZED_HOSTED_PROVIDER)
    {
        1.1
    } else {
        1.0
    }
}

fn apply_pricing_if_available(
    message: &mut UnifiedMessage,
    pricing: Option<&pricing::PricingService>,
) {
    let Some(pricing) = pricing else {
        return;
    };

    let calculated_cost = pricing.calculate_cost_with_provider(
        &message.model_id,
        Some(&message.provider_id),
        &message.tokens,
    ) * pricing_multiplier(message);

    if calculated_cost > 0.0 {
        message.cost = calculated_cost;
    }
}

fn parse_hermes_sqlite_with_pricing(
    db_path: &Path,
    pricing: Option<&pricing::PricingService>,
) -> Vec<UnifiedMessage> {
    sessions::hermes::parse_hermes_sqlite(db_path)
        .into_iter()
        .map(|mut msg| {
            if msg.cost <= 0.0 {
                apply_pricing_if_available(&mut msg, pricing);
            }
            msg
        })
        .collect()
}

fn select_local_parse_pricing<F>(
    fresh: Result<Arc<pricing::PricingService>, String>,
    stale: F,
) -> Option<Arc<pricing::PricingService>>
where
    F: FnOnce() -> Option<pricing::PricingService>,
{
    fresh.ok().or_else(|| stale().map(Arc::new))
}

async fn load_pricing_for_local_parse() -> Option<Arc<pricing::PricingService>> {
    if std::env::var("TOKSCALE_PRICING_CACHE_ONLY")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
    {
        return pricing::PricingService::load_cached_any_age().map(Arc::new);
    }

    // Interactive/local views should pick up newly released model pricing as soon
    // as a fresh fetch succeeds, but still remain usable offline by falling back
    // to any cached dataset when the network path fails.
    select_local_parse_pricing(
        pricing::PricingService::get_or_init().await,
        pricing::PricingService::load_cached_any_age,
    )
}

fn resolve_local_parse_request(
    options: &LocalParseOptions,
) -> Result<(String, Vec<String>), String> {
    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = options.clients.clone().unwrap_or_else(|| {
        let mut clients: Vec<String> = ClientId::iter()
            .filter(|c| c.parse_local())
            .map(|c| c.as_str().to_string())
            .collect();
        clients.push("synthetic".to_string());
        clients
    });
    Ok((home_dir, clients))
}

fn parse_local_unified_messages_resolved(
    options: LocalParseOptions,
    home_dir: &str,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
) -> Result<Vec<UnifiedMessage>, String> {
    let messages = parse_all_messages_with_pricing_with_env_strategy(
        home_dir,
        clients,
        pricing,
        options.use_env_roots,
        &options.scanner_settings,
    );
    Ok(filter_unified_messages(messages, &options))
}
/// Max mtime (unix ms) across every file the local scan would read — the
/// cheapest "did anything change" probe for callers that cache reports
/// derived from `parse_local_clients` / the unified-message parsers.
/// Database-backed sources contribute both the db file and its `-wal`
/// sidecar (WAL writes may leave the main db file's mtime untouched).
/// Stat failures contribute nothing, so a vanished file alone never
/// invalidates a caller's cache — its replacement or sibling will.
pub fn latest_source_mtime_ms(options: &LocalParseOptions) -> Result<u64, String> {
    let (home_dir, clients) = resolve_local_parse_request(options)?;
    let scan_result = scanner::scan_all_clients_with_scanner_settings(
        &home_dir,
        &clients,
        options.use_env_roots,
        &options.scanner_settings,
    );
    let mut latest: u64 = 0;
    for files in scan_result.files.iter() {
        for path in files {
            latest = latest.max(file_mtime_ms(path).unwrap_or(0));
        }
    }
    let mut dbs: Vec<PathBuf> = scan_result.opencode_dbs.clone();
    let single_dbs = [
        &scan_result.synthetic_db,
        &scan_result.kilo_db,
        &scan_result.goose_db,
        &scan_result.kiro_db,
    ];
    dbs.extend(single_dbs.into_iter().flatten().cloned());
    // Hermes/Zed dbs may also be discovered via user-provided extra scan
    // roots (the `files` lanes) — use the plural helpers so every db gets
    // its `-wal` sidecar probed, not just the default-path single.
    dbs.extend(scan_result.hermes_db_paths());
    dbs.extend(scan_result.zed_db_paths());
    dbs.extend(scan_result.crush_dbs.iter().map(|c| c.db_path.clone()));
    // Antigravity CLI conversation `.db` files arrive via the generic `files`
    // lane (a `*.db` glob, no dedicated ScanResult field), so probe their `-wal`
    // sidecars here too — a WAL-only write would otherwise leave the change
    // token unchanged and the live tail would never re-parse the new usage.
    dbs.extend(scan_result.get(ClientId::AntigravityCli).iter().cloned());
    // micode `.db` files likewise arrive via the generic `*.db` glob and are
    // WAL-mode SQLite, so probe their `-wal` sidecars for the live-tail change
    // token too.
    dbs.extend(scan_result.get(ClientId::MiMoCode).iter().cloned());
    for db in dbs {
        latest = latest.max(file_mtime_ms(&db).unwrap_or(0));
        let mut wal = db.into_os_string();
        wal.push("-wal");
        latest = latest.max(file_mtime_ms(Path::new(&wal)).unwrap_or(0));
    }
    Ok(latest)
}

/// File mtime as unix ms; `None` on any stat failure.
fn file_mtime_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as u64)
}

/// Drop file-backed session logs older than `threshold_ms` (unix ms, mtime)
/// from a scan. Database-backed sources are left untouched: SQLite WAL writes
/// may not update the main db file's mtime, so they are always parsed. The
/// Hermes/Zed/Antigravity-CLI `files` lanes hold SQLite dbs (Hermes/Zed via
/// user scan roots, Antigravity CLI via the `*.db` glob), so they are exempt
/// too. Any stat failure keeps the file — over-parsing is safe, silently
/// skipping is not.
fn prune_scan_result_by_mtime(scan_result: &mut scanner::ScanResult, threshold_ms: u64) {
    let db_lanes = [
        ClientId::Hermes as usize,
        ClientId::Zed as usize,
        ClientId::AntigravityCli as usize,
        ClientId::MiMoCode as usize,
    ];
    for (lane, files) in scan_result.files.iter_mut().enumerate() {
        if db_lanes.contains(&lane) {
            continue;
        }
        files.retain(|path| {
            let Ok(meta) = std::fs::metadata(path) else {
                return true;
            };
            let Ok(modified) = meta.modified() else {
                return true;
            };
            let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) else {
                return true;
            };
            duration.as_millis() as u64 >= threshold_ms
        });
    }
}

pub fn parse_local_clients(options: LocalParseOptions) -> Result<ParsedMessages, String> {
    let start = Instant::now();

    let home_dir = get_home_dir_string(&options.home_dir)?;

    let clients: Vec<String> = options.clients.clone().unwrap_or_else(|| {
        let mut clients: Vec<String> = ClientId::iter()
            .filter(|c| c.parse_local())
            .map(|c| c.as_str().to_string())
            .collect();
        clients.push("synthetic".to_string());
        clients
    });
    let include_all = clients.is_empty();
    let include_synthetic = include_all || clients.iter().any(|c| c == "synthetic");

    let mut scan_result = scanner::scan_all_clients_with_scanner_settings(
        &home_dir,
        &clients,
        options.use_env_roots,
        &options.scanner_settings,
    );
    if let Some(threshold_ms) = options.modified_after {
        prune_scan_result_by_mtime(&mut scan_result, threshold_ms);
    }
    let headless_roots =
        scanner::headless_roots_with_env_strategy(&home_dir, options.use_env_roots);

    let mut messages: Vec<ParsedMessage> = Vec::new();

    // Parse OpenCode: prefer SQLite, collapse forked SQLite history there, then
    // suppress legacy JSON overlap by message identity.
    let mut counts = ClientCounts::new();

    let opencode_count: i32 = {
        let mut seen: HashSet<String> = HashSet::new();
        let mut count: i32 = 0;

        for db_path in &scan_result.opencode_dbs {
            let sqlite_msgs: Vec<(String, ParsedMessage)> =
                sessions::opencode::parse_opencode_sqlite(db_path)
                    .into_iter()
                    .filter_map(|msg| {
                        let key = msg.dedup_key.clone().unwrap_or_default();
                        // Dedup across multiple channel-suffixed dbs: the
                        // same session can end up in both `opencode.db` and
                        // `opencode-<channel>.db` if the user switches
                        // channels mid-session.
                        if !key.is_empty() && !seen.insert(key.clone()) {
                            return None;
                        }
                        Some((key, unified_to_parsed(&msg)))
                    })
                    .collect();
            count += sqlite_msgs.len() as i32;
            for (_key, parsed) in sqlite_msgs {
                messages.push(parsed);
            }
        }

        let json_msgs: Vec<(String, ParsedMessage)> = scan_result
            .get(ClientId::OpenCode)
            .par_iter()
            .filter_map(|path| {
                let msg = sessions::opencode::parse_opencode_file(path)?;
                let key = msg.dedup_key.clone().unwrap_or_default();
                Some((key, unified_to_parsed(&msg)))
            })
            .collect();
        let deduped: Vec<ParsedMessage> = json_msgs
            .into_iter()
            .filter(|(key, _)| key.is_empty() || seen.insert(key.clone()))
            .map(|(_, msg)| msg)
            .collect();
        count += deduped.len() as i32;
        messages.extend(deduped);

        count
    };
    counts.set(ClientId::OpenCode, opencode_count);

    let claude_home = PathBuf::from(&home_dir);
    let claude_msgs_raw: Vec<(String, ParsedMessage)> = scan_result
        .get(ClientId::Claude)
        .par_iter()
        .map_init(std::collections::HashMap::new, |parent_cache, path| {
            sessions::claudecode::parse_claude_file_with_cache_and_home(
                path,
                parent_cache,
                Some(&claude_home),
            )
            .into_iter()
            .map(|msg| {
                let dedup_key = msg.dedup_key.clone().unwrap_or_default();
                (dedup_key, unified_to_parsed(&msg))
            })
            .collect::<Vec<_>>()
        })
        .flatten()
        .collect();

    let mut seen_keys: HashSet<String> = HashSet::new();
    let claude_msgs: Vec<ParsedMessage> = claude_msgs_raw
        .into_iter()
        .filter(|(key, _)| key.is_empty() || seen_keys.insert(key.clone()))
        .map(|(_, msg)| msg)
        .collect();
    let claude_count = claude_msgs.len() as i32;
    counts.set(ClientId::Claude, claude_count);
    messages.extend(claude_msgs);

    let codex_msgs_raw: Vec<UnifiedMessage> = scan_result
        .get(ClientId::Codex)
        .par_iter()
        .flat_map(|path| {
            let is_headless = is_headless_path(path, &headless_roots);
            sessions::codex::parse_codex_file(path)
                .into_iter()
                .map(|mut msg| {
                    apply_headless_agent(&mut msg, is_headless);
                    msg
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let mut codex_seen: HashSet<String> = HashSet::new();
    let codex_msgs: Vec<ParsedMessage> = codex_msgs_raw
        .into_iter()
        .filter(|message| should_keep_deduped_message(&mut codex_seen, message))
        .map(|message| unified_to_parsed(&message))
        .collect();
    let codex_count = codex_msgs.len() as i32;
    counts.set(ClientId::Codex, codex_count);
    messages.extend(codex_msgs);

    let copilot_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Copilot)
        .par_iter()
        .flat_map(|path| {
            sessions::copilot::parse_copilot_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let copilot_count = copilot_msgs.len() as i32;
    counts.set(ClientId::Copilot, copilot_count);
    messages.extend(copilot_msgs);

    let gemini_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Gemini)
        .par_iter()
        .flat_map(|path| {
            sessions::gemini::parse_gemini_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let gemini_count = gemini_msgs.len() as i32;
    counts.set(ClientId::Gemini, gemini_count);
    messages.extend(gemini_msgs);

    let amp_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Amp)
        .par_iter()
        .flat_map(|path| {
            sessions::amp::parse_amp_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let amp_count = amp_msgs.len() as i32;
    counts.set(ClientId::Amp, amp_count);
    messages.extend(amp_msgs);

    let codebuff_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Codebuff)
        .par_iter()
        .flat_map(|path| {
            sessions::codebuff::parse_codebuff_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let codebuff_count = codebuff_msgs.len() as i32;
    counts.set(ClientId::Codebuff, codebuff_count);
    messages.extend(codebuff_msgs);

    let droid_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Droid)
        .par_iter()
        .flat_map(|path| {
            sessions::droid::parse_droid_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let droid_count = droid_msgs.len() as i32;
    counts.set(ClientId::Droid, droid_count);
    messages.extend(droid_msgs);

    let openclaw_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::OpenClaw)
        .par_iter()
        .flat_map(|path| {
            sessions::openclaw::parse_openclaw_transcript(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let openclaw_count = openclaw_msgs.len() as i32;
    counts.set(ClientId::OpenClaw, openclaw_count);
    messages.extend(openclaw_msgs);

    let pi_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Pi)
        .par_iter()
        .flat_map(|path| {
            sessions::pi::parse_pi_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let pi_count = pi_msgs.len() as i32;
    counts.set(ClientId::Pi, pi_count);
    messages.extend(pi_msgs);

    // Parse Kimi wire.jsonl files in parallel
    let kimi_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Kimi)
        .par_iter()
        .flat_map(|path| {
            sessions::kimi::parse_kimi_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let kimi_count = kimi_msgs.len() as i32;
    counts.set(ClientId::Kimi, kimi_count);
    messages.extend(kimi_msgs);

    // Parse Qwen JSONL files in parallel
    let qwen_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Qwen)
        .par_iter()
        .flat_map(|path| {
            sessions::qwen::parse_qwen_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let qwen_count = qwen_msgs.len() as i32;
    counts.set(ClientId::Qwen, qwen_count);
    messages.extend(qwen_msgs);

    let roocode_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::RooCode)
        .par_iter()
        .flat_map(|path| {
            sessions::roocode::parse_roocode_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let roocode_count = roocode_msgs.len() as i32;
    counts.set(ClientId::RooCode, roocode_count);
    messages.extend(roocode_msgs);

    let kilocode_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::KiloCode)
        .par_iter()
        .flat_map(|path| {
            sessions::kilocode::parse_kilocode_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let kilocode_count = summed_parsed_message_count(&kilocode_msgs);
    counts.set(ClientId::KiloCode, kilocode_count);
    messages.extend(kilocode_msgs);

    let cline_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Cline)
        .par_iter()
        .flat_map(|path| {
            sessions::cline::parse_cline_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let cline_count = summed_parsed_message_count(&cline_msgs);
    counts.set(ClientId::Cline, cline_count);
    messages.extend(cline_msgs);

    let jcode_msgs_raw: Vec<UnifiedMessage> = scan_result
        .get(ClientId::Jcode)
        .par_iter()
        .flat_map(|path| sessions::jcode::parse_jcode_file(path))
        .collect();
    let mut jcode_seen: HashSet<String> = HashSet::new();
    let jcode_msgs: Vec<ParsedMessage> = jcode_msgs_raw
        .into_iter()
        .filter(|message| should_keep_deduped_message(&mut jcode_seen, message))
        .map(|m| unified_to_parsed(&m))
        .collect();
    let jcode_count = summed_parsed_message_count(&jcode_msgs);
    counts.set(ClientId::Jcode, jcode_count);
    messages.extend(jcode_msgs);

    let micode_msgs_raw: Vec<UnifiedMessage> = scan_result
        .get(ClientId::MiMoCode)
        .par_iter()
        .flat_map(|path| sessions::micode::parse_micode_sqlite(path))
        .collect();
    let mut micode_seen: HashSet<String> = HashSet::new();
    let micode_msgs: Vec<ParsedMessage> = micode_msgs_raw
        .into_iter()
        .filter(|message| should_keep_deduped_message(&mut micode_seen, message))
        .map(|m| unified_to_parsed(&m))
        .collect();
    let micode_count = summed_parsed_message_count(&micode_msgs);
    counts.set(ClientId::MiMoCode, micode_count);
    messages.extend(micode_msgs);

    let mux_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Mux)
        .par_iter()
        .flat_map(|path| {
            sessions::mux::parse_mux_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let mux_count = summed_parsed_message_count(&mux_msgs);
    counts.set(ClientId::Mux, mux_count);
    messages.extend(mux_msgs);

    // Kilo CLI: SQLite database
    let _kilo_count: i32 = if let Some(db_path) = &scan_result.kilo_db {
        let kilo_msgs: Vec<ParsedMessage> = sessions::kilo::parse_kilo_sqlite(db_path)
            .into_iter()
            .map(|msg| unified_to_parsed(&msg))
            .collect();
        let count = summed_parsed_message_count(&kilo_msgs);
        counts.set(ClientId::Kilo, count);
        messages.extend(kilo_msgs);
        count
    } else {
        0
    };

    let hermes_db_paths = scan_result.hermes_db_paths();
    if !hermes_db_paths.is_empty() {
        let mut hermes_seen: HashSet<String> = HashSet::new();
        let hermes_msgs: Vec<ParsedMessage> = hermes_db_paths
            .iter()
            .flat_map(|db_path| sessions::hermes::parse_hermes_sqlite(db_path))
            .filter(|msg| should_keep_deduped_message(&mut hermes_seen, msg))
            .map(|msg| unified_to_parsed(&msg))
            .collect();
        let count = summed_parsed_message_count(&hermes_msgs);
        counts.set(ClientId::Hermes, count);
        messages.extend(hermes_msgs);
    }

    if let Some(db_path) = &scan_result.goose_db {
        let goose_msgs: Vec<ParsedMessage> = sessions::goose::parse_goose_sqlite(db_path)
            .into_iter()
            .map(|msg| unified_to_parsed(&msg))
            .collect();
        let count = summed_parsed_message_count(&goose_msgs);
        counts.set(ClientId::Goose, count);
        messages.extend(goose_msgs);
    }

    let zed_db_paths = scan_result.zed_db_paths();
    if !zed_db_paths.is_empty() {
        let zed_msgs: Vec<ParsedMessage> = zed_db_paths
            .iter()
            .flat_map(|db_path| sessions::zed::parse_zed_sqlite(db_path))
            .map(|msg| unified_to_parsed(&msg))
            .collect();
        let count = summed_parsed_message_count(&zed_msgs);
        counts.set(ClientId::Zed, count);
        messages.extend(zed_msgs);
    }

    let kiro_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Kiro)
        .par_iter()
        .flat_map(|path| {
            sessions::kiro::parse_kiro_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let kiro_count = summed_parsed_message_count(&kiro_msgs);
    counts.set(ClientId::Kiro, kiro_count);
    messages.extend(kiro_msgs);

    if let Some(db_path) = &scan_result.kiro_db {
        let kiro_db_msgs: Vec<ParsedMessage> = sessions::kiro::parse_kiro_sqlite(db_path)
            .into_iter()
            .map(|msg| unified_to_parsed(&msg))
            .collect();
        let kiro_db_count = summed_parsed_message_count(&kiro_db_msgs);
        counts.add(ClientId::Kiro, kiro_db_count);
        messages.extend(kiro_db_msgs);
    }

    let crush_msgs: Vec<ParsedMessage> = scan_result
        .crush_dbs
        .par_iter()
        .flat_map(|source| {
            sessions::crush::parse_crush_sqlite(&source.db_path)
                .into_iter()
                .map(|mut msg| {
                    msg.set_workspace(source.workspace_key.clone(), source.workspace_label.clone());
                    unified_to_parsed(&msg)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let crush_count = summed_parsed_message_count(&crush_msgs);
    counts.set(ClientId::Crush, crush_count);
    messages.extend(crush_msgs);

    let antigravity_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Antigravity)
        .par_iter()
        .flat_map(|path| {
            sessions::antigravity::parse_antigravity_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let antigravity_count = antigravity_msgs.len() as i32;
    counts.set(ClientId::Antigravity, antigravity_count);
    messages.extend(antigravity_msgs);

    let antigravity_cli_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::AntigravityCli)
        .par_iter()
        .flat_map(|path| {
            sessions::antigravity_cli::parse_antigravity_cli_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let antigravity_cli_count = summed_parsed_message_count(&antigravity_cli_msgs);
    counts.set(ClientId::AntigravityCli, antigravity_cli_count);
    messages.extend(antigravity_cli_msgs);

    let trae_msgs: Vec<ParsedMessage> = {
        let unique_trae_messages = dedupe_latest_trae_messages(
            scan_result
                .get(ClientId::Trae)
                .par_iter()
                .flat_map(|path| sessions::trae::parse_trae_file("trae", path))
                .collect(),
        );
        unique_trae_messages
            .into_iter()
            .map(|msg| unified_to_parsed(&msg))
            .collect()
    };
    let trae_count = trae_msgs.len() as i32;
    counts.set(ClientId::Trae, trae_count);
    messages.extend(trae_msgs);

    let warp_msgs: Vec<ParsedMessage> = scan_result
        .get(ClientId::Warp)
        .par_iter()
        .flat_map(|path| {
            sessions::warp::parse_warp_file(path)
                .into_iter()
                .map(|msg| unified_to_parsed(&msg))
                .collect::<Vec<_>>()
        })
        .collect();
    let warp_count = summed_parsed_message_count(&warp_msgs);
    counts.set(ClientId::Warp, warp_count);
    messages.extend(warp_msgs);

    if include_synthetic {
        if let Some(db_path) = &scan_result.synthetic_db {
            let synthetic_msgs: Vec<ParsedMessage> =
                sessions::synthetic::parse_octofriend_sqlite(db_path)
                    .into_iter()
                    .map(|msg| unified_to_parsed(&msg))
                    .collect();
            messages.extend(synthetic_msgs);
        }
    }

    // Filter BEFORE normalization (see parse_all_messages_with_pricing).
    if !include_all {
        let requested: HashSet<&str> = clients.iter().map(String::as_str).collect();
        messages.retain(|msg| {
            retain_for_requested_clients(&msg.client, &msg.model_id, &msg.provider_id, &requested)
        });
    }

    if include_synthetic {
        for msg in &mut messages {
            sessions::synthetic::normalize_synthetic_gateway_fields(
                &mut msg.model_id,
                &mut msg.provider_id,
            );
        }
    }

    let filtered = filter_parsed_messages(messages, &options);

    Ok(ParsedMessages {
        messages: filtered,
        counts,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

#[doc(hidden)]
pub async fn parse_local_unified_messages_with_pricing(
    options: LocalParseOptions,
    pricing: Option<&pricing::PricingService>,
) -> Result<Vec<UnifiedMessage>, String> {
    let (home_dir, clients) = resolve_local_parse_request(&options)?;
    parse_local_unified_messages_resolved(options, &home_dir, &clients, pricing)
}

/// Parse the local unified message stream into a fully materialized `Vec`.
///
/// **Footgun:** this rides the old materialized path
/// (`parse_all_messages_with_pricing_with_env_strategy`), which does NOT apply
/// per-client cross-file dedup to the `simple_lane!` clients (copilot, codebuff,
/// kimi, cursor, warp, amp, droid, …) and resolves a narrower client set via
/// `resolve_local_parse_request`. New report consumers MUST fold over
/// `scan_messages_streaming` instead (see `get_model_report` / `get_agents_report`)
/// or their totals will diverge from the other reports. Retained as public
/// vendored API surface; no in-repo callers after the issue #6 agents migration.
pub async fn parse_local_unified_messages(
    options: LocalParseOptions,
) -> Result<Vec<UnifiedMessage>, String> {
    let (home_dir, clients) = resolve_local_parse_request(&options)?;
    let pricing = load_pricing_for_local_parse().await;
    parse_local_unified_messages_resolved(options, &home_dir, &clients, pricing.as_deref())
}

fn unified_to_parsed(msg: &UnifiedMessage) -> ParsedMessage {
    ParsedMessage {
        client: msg.client.clone(),
        model_id: msg.model_id.clone(),
        provider_id: msg.provider_id.clone(),
        session_id: msg.session_id.clone(),
        workspace_key: msg.workspace_key.clone(),
        workspace_label: msg.workspace_label.clone(),
        timestamp: msg.timestamp,
        date: msg.date.clone(),
        input: msg.tokens.input,
        output: msg.tokens.output,
        cache_read: msg.tokens.cache_read,
        cache_write: msg.tokens.cache_write,
        reasoning: msg.tokens.reasoning,
        duration_ms: msg.duration_ms,
        message_count: msg.message_count,
        agent: msg.agent.clone(),
    }
}

fn should_keep_deduped_message(seen_keys: &mut HashSet<String>, message: &UnifiedMessage) -> bool {
    message
        .dedup_key
        .as_ref()
        .is_none_or(|key| seen_keys.insert(key.clone()))
}

fn summed_parsed_message_count(messages: &[ParsedMessage]) -> i32 {
    messages
        .iter()
        .map(|msg| msg.message_count.max(0))
        .sum::<i32>()
}

fn filter_parsed_messages(
    messages: Vec<ParsedMessage>,
    options: &LocalParseOptions,
) -> Vec<ParsedMessage> {
    let mut filtered = messages;

    if let Some(year) = &options.year {
        let year_prefix = format!("{}-", year);
        filtered.retain(|m| m.date.starts_with(&year_prefix));
    }

    if let Some(since) = &options.since {
        filtered.retain(|m| m.date.as_str() >= since.as_str());
    }

    if let Some(until) = &options.until {
        filtered.retain(|m| m.date.as_str() <= until.as_str());
    }

    filtered
}

pub fn parsed_to_unified(msg: &ParsedMessage, cost: f64) -> UnifiedMessage {
    UnifiedMessage {
        client: msg.client.clone(),
        model_id: msg.model_id.clone(),
        provider_id: msg.provider_id.clone(),
        session_id: msg.session_id.clone(),
        workspace_key: msg.workspace_key.clone(),
        workspace_label: msg.workspace_label.clone(),
        timestamp: msg.timestamp,
        date: msg.date.clone(),
        tokens: TokenBreakdown {
            input: msg.input,
            output: msg.output,
            cache_read: msg.cache_read,
            cache_write: msg.cache_write,
            reasoning: msg.reasoning,
        },
        cost,
        duration_ms: msg.duration_ms,
        message_count: msg.message_count,
        agent: msg.agent.clone(),
        dedup_key: None,
        is_turn_start: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        agent_bucket_key, aggregate_model_usage_entries, apply_pricing_if_available,
        dedupe_latest_trae_messages, fold_messages_streaming, get_agents_report, get_model_report,
        message_cache, normalize_model_for_grouping, parse_all_messages_with_pricing,
        parse_local_clients, parse_local_unified_messages, parsed_to_unified, pricing,
        retain_for_requested_clients, scan_messages_streaming, scanner, select_local_parse_pricing,
        unified_to_parsed,
        AgentAccumulator, ClientId, GroupBy, LocalParseOptions, ReportOptions, TokenBreakdown,
        UnifiedMessage, UNKNOWN_WORKSPACE_LABEL,
    };
    use std::collections::{HashMap, HashSet};
    use std::io::Write;
    use std::str::FromStr;
    use std::sync::Arc;

    fn make_workspace_message(
        client: &str,
        model_id: &str,
        provider_id: &str,
        session_id: &str,
        cost: f64,
        workspace_key: Option<&str>,
        workspace_label: Option<&str>,
    ) -> UnifiedMessage {
        let mut msg = UnifiedMessage::new(
            client,
            model_id,
            provider_id,
            session_id,
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            cost,
        );
        msg.set_workspace(
            workspace_key.map(str::to_string),
            workspace_label.map(str::to_string),
        );
        msg
    }

    fn make_trae_message(
        session_id: &str,
        timestamp: i64,
        dedup_key: Option<&str>,
        cost: f64,
    ) -> UnifiedMessage {
        UnifiedMessage::new_with_dedup(
            "trae",
            "gpt-5.2",
            "openai",
            session_id,
            timestamp,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            cost,
            dedup_key.map(str::to_string),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_opencode_sqlite_payload(
        created_ms: f64,
        completed_ms: f64,
        input: i64,
        output: i64,
        reasoning: i64,
        cache_read: i64,
        cache_write: i64,
        cost: f64,
    ) -> String {
        format!(
            r#"{{
                "role": "assistant",
                "modelID": "claude-sonnet-4",
                "providerID": "anthropic",
                "cost": {cost},
                "tokens": {{
                    "input": {input},
                    "output": {output},
                    "reasoning": {reasoning},
                    "cache": {{ "read": {cache_read}, "write": {cache_write} }}
                }},
                "time": {{ "created": {created_ms}, "completed": {completed_ms} }},
                "mode": "build"
            }}"#
        )
    }

    fn create_opencode_sqlite_db(db_path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn create_hermes_sqlite_db(db_path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                model TEXT,
                started_at REAL NOT NULL,
                message_count INTEGER DEFAULT 0,
                input_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0,
                cache_write_tokens INTEGER DEFAULT 0,
                reasoning_tokens INTEGER DEFAULT 0,
                billing_provider TEXT,
                estimated_cost_usd REAL,
                actual_cost_usd REAL
            );",
        )
        .unwrap();
        conn
    }

    fn create_zed_sqlite_db(db_path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                data_type TEXT NOT NULL,
                data BLOB NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_zed_thread(conn: &rusqlite::Connection, id: &str, model: &str) {
        let payload = format!(
            r#"{{
                "version": "0.3.0",
                "title": "Test thread",
                "updated_at": "2026-05-01T12:30:00Z",
                "request_token_usage": {{
                    "turn-1": {{
                        "input_tokens": 42,
                        "output_tokens": 7,
                        "cache_creation_input_tokens": 3,
                        "cache_read_input_tokens": 5
                    }}
                }},
                "model": {{
                    "provider": "zed.dev",
                    "model": "{model}"
                }},
                "imported": false
            }}"#
        );
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, "Test thread", "2026-05-01T12:30:00Z", "json", payload.as_bytes()],
        )
        .unwrap();
    }

    fn insert_hermes_session(
        conn: &rusqlite::Connection,
        id: &str,
        model: &str,
        message_count: i64,
        input_tokens: i64,
        output_tokens: i64,
        actual_cost_usd: f64,
    ) {
        conn.execute(
            "INSERT INTO sessions (
                id, source, model, started_at, message_count,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                billing_provider, estimated_cost_usd, actual_cost_usd
            ) VALUES (?1, 'cli', ?2, 1775001102.0, ?3, ?4, ?5, 0, 0, 0, 'anthropic', NULL, ?6)",
            rusqlite::params![
                id,
                model,
                message_count,
                input_tokens,
                output_tokens,
                actual_cost_usd
            ],
        )
        .unwrap();
    }

    #[test]
    fn test_normalize_model_for_grouping() {
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-5-20251101"),
            "claude-opus-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4-5-20250929"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4-20250514"),
            "claude-sonnet-4"
        );

        assert_eq!(
            normalize_model_for_grouping("claude-opus-4.5"),
            "claude-opus-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4.5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4.6"),
            "claude-opus-4-6"
        );
        assert_eq!(
            normalize_model_for_grouping("anthropic/claude-4-6-sonnet"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            normalize_model_for_grouping("anthropic/claude-4-5-haiku"),
            "claude-haiku-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("anthropic/claude-4-6-opus"),
            "claude-opus-4-6"
        );

        assert_eq!(normalize_model_for_grouping("gpt-5.2"), "gpt-5.2");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(xhigh)"), "gpt-5.4");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(high)"), "gpt-5.4");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(minimal)"), "gpt-5.4");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(auto)"), "gpt-5.4");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(none)"), "gpt-5.4");
        assert_eq!(
            normalize_model_for_grouping("gpt-5.4(weirdgarbage)"),
            "gpt-5.4(weirdgarbage)"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4.5(high)"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("gemini-3-pro(auto)"),
            "gemini-3-pro"
        );
        assert_eq!(
            normalize_model_for_grouping("gemini-2.5-pro"),
            "gemini-2.5-pro"
        );

        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-5-high"),
            "claude-opus-4-5-high"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-5-thinking-high"),
            "claude-opus-4-5-thinking-high"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4-5-high"),
            "claude-sonnet-4-5-high"
        );

        assert_eq!(
            normalize_model_for_grouping("claude-4-sonnet"),
            "claude-4-sonnet"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-4-opus-thinking"),
            "claude-4-opus-thinking"
        );

        assert_eq!(normalize_model_for_grouping("big-pickle"), "big-pickle");
        assert_eq!(normalize_model_for_grouping("grok-code"), "grok-code");

        assert_eq!(
            normalize_model_for_grouping("claude-opus-4.5-20251101"),
            "claude-opus-4-5"
        );
    }

    #[test]
    fn test_group_by_from_str_valid_values() {
        assert_eq!(GroupBy::from_str("model").unwrap(), GroupBy::Model);
        assert_eq!(
            GroupBy::from_str("client,model").unwrap(),
            GroupBy::ClientModel
        );
        assert_eq!(
            GroupBy::from_str("client-model").unwrap(),
            GroupBy::ClientModel
        );
        assert_eq!(
            GroupBy::from_str("client,provider,model").unwrap(),
            GroupBy::ClientProviderModel
        );
        assert_eq!(
            GroupBy::from_str("client-provider-model").unwrap(),
            GroupBy::ClientProviderModel
        );
        assert_eq!(
            GroupBy::from_str("workspace,model").unwrap(),
            GroupBy::WorkspaceModel
        );
        assert_eq!(
            GroupBy::from_str("workspace-model").unwrap(),
            GroupBy::WorkspaceModel
        );
        assert_eq!(GroupBy::from_str("session").unwrap(), GroupBy::Session);
        assert_eq!(
            GroupBy::from_str("session,model").unwrap(),
            GroupBy::Session
        );
        assert_eq!(
            GroupBy::from_str("session-model").unwrap(),
            GroupBy::Session
        );
        assert_eq!(
            GroupBy::from_str("client,session").unwrap(),
            GroupBy::ClientSession
        );
        assert_eq!(
            GroupBy::from_str("client,session,model").unwrap(),
            GroupBy::ClientSession
        );
        assert_eq!(
            GroupBy::from_str("client-session-model").unwrap(),
            GroupBy::ClientSession
        );
        assert!(GroupBy::from_str("unknown").is_err());
    }

    #[test]
    fn test_group_by_default_is_client_model() {
        assert_eq!(GroupBy::default(), GroupBy::ClientModel);
    }

    #[test]
    fn test_group_by_display_round_trips_with_from_str() {
        let variants = [
            GroupBy::Model,
            GroupBy::ClientModel,
            GroupBy::ClientProviderModel,
            GroupBy::WorkspaceModel,
            GroupBy::Session,
            GroupBy::ClientSession,
        ];

        for variant in variants {
            let rendered = variant.to_string();
            let parsed = GroupBy::from_str(&rendered).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn test_group_by_from_str_whitespace_handling() {
        assert_eq!(
            GroupBy::from_str("client, model").unwrap(),
            GroupBy::ClientModel
        );
        assert_eq!(GroupBy::from_str(" model ").unwrap(), GroupBy::Model);
        assert_eq!(
            GroupBy::from_str("client , provider , model").unwrap(),
            GroupBy::ClientProviderModel
        );
        assert_eq!(
            GroupBy::from_str("workspace, model").unwrap(),
            GroupBy::WorkspaceModel
        );
    }

    #[test]
    fn test_model_usage_performance_uses_only_timed_positive_token_messages() {
        let mut timed = make_workspace_message(
            "opencode",
            "gpt-5.4",
            "openai",
            "session-1",
            0.0,
            None,
            None,
        );
        timed.tokens = TokenBreakdown {
            input: 100,
            output: 50,
            cache_read: 25,
            cache_write: 0,
            reasoning: 25,
        };
        timed.duration_ms = Some(400);

        let mut untimed = make_workspace_message(
            "opencode",
            "gpt-5.4",
            "openai",
            "session-2",
            0.0,
            None,
            None,
        );
        untimed.tokens = TokenBreakdown {
            input: 300,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        };

        let entries = aggregate_model_usage_entries(vec![timed, untimed], &GroupBy::ClientModel);

        assert_eq!(entries.len(), 1);
        let performance = &entries[0].performance;
        assert_eq!(performance.total_duration_ms, 400);
        assert_eq!(performance.timed_tokens, 200);
        assert_eq!(performance.sample_count, 1);
        assert_eq!(performance.ms_per_1k_tokens, Some(2000.0));
        assert!((performance.token_coverage - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn test_model_usage_performance_is_null_without_duration_samples() {
        let entries = aggregate_model_usage_entries(
            vec![make_workspace_message(
                "claude",
                "claude-sonnet-4-5",
                "anthropic",
                "session-1",
                0.0,
                None,
                None,
            )],
            &GroupBy::ClientModel,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].performance.ms_per_1k_tokens, None);
        assert_eq!(entries[0].performance.total_duration_ms, 0);
        assert_eq!(entries[0].performance.timed_tokens, 0);
        assert_eq!(entries[0].performance.token_coverage, 0.0);
    }

    #[test]
    fn test_workspace_model_grouping_merges_same_workspace_and_model() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-1",
                    1.25,
                    Some("/repo-a"),
                    Some("repo-a"),
                ),
                make_workspace_message(
                    "qwen",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-2",
                    2.75,
                    Some("/repo-a"),
                    Some("repo-a"),
                ),
            ],
            &GroupBy::WorkspaceModel,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model, "claude-sonnet-4-5");
        assert_eq!(entries[0].workspace_key.as_deref(), Some("/repo-a"));
        assert_eq!(entries[0].workspace_label.as_deref(), Some("repo-a"));
        assert_eq!(entries[0].cost, 4.0);
        assert_eq!(entries[0].message_count, 2);
        assert_eq!(entries[0].merged_clients.as_deref(), Some("claude, qwen"));
    }

    #[test]
    fn test_model_grouping_merges_anthropic_prefixed_claude_variant_with_canonical_model() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "anthropic/claude-4-6-sonnet",
                    "anthropic",
                    "session-1",
                    1.25,
                    Some("/repo-a"),
                    Some("repo-a"),
                ),
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-6",
                    "anthropic",
                    "session-2",
                    2.75,
                    Some("/repo-b"),
                    Some("repo-b"),
                ),
            ],
            &GroupBy::ClientModel,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model, "claude-sonnet-4-6");
        assert_eq!(entries[0].input, 20);
        assert_eq!(entries[0].output, 10);
        assert_eq!(entries[0].cost, 4.0);
        assert_eq!(entries[0].message_count, 2);
    }

    #[test]
    fn test_workspace_model_grouping_separates_different_workspaces() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-1",
                    1.0,
                    Some("/repo-a"),
                    Some("repo-a"),
                ),
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-2",
                    2.0,
                    Some("/repo-b"),
                    Some("repo-b"),
                ),
            ],
            &GroupBy::WorkspaceModel,
        );

        assert_eq!(entries.len(), 2);
        let labels: HashSet<_> = entries
            .iter()
            .map(|entry| entry.workspace_label.as_deref().unwrap())
            .collect();
        assert_eq!(labels, HashSet::from(["repo-a", "repo-b"]));
    }

    #[test]
    fn test_workspace_model_grouping_uses_unknown_bucket_without_workspace_metadata() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-1",
                    1.0,
                    None,
                    None,
                ),
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-2",
                    "2.0".parse().unwrap(),
                    None,
                    None,
                ),
            ],
            &GroupBy::WorkspaceModel,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].workspace_key, None);
        assert_eq!(
            entries[0].workspace_label.as_deref(),
            Some(UNKNOWN_WORKSPACE_LABEL)
        );
        assert_eq!(entries[0].message_count, 2);
        assert_eq!(entries[0].cost, 3.0);
    }

    #[test]
    fn test_parsed_round_trip_preserves_workspace_metadata() {
        let mut unified = UnifiedMessage::new(
            "qwen",
            "qwen3.5-plus",
            "qwen",
            "session-1",
            1_742_390_400_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 2,
                cache_write: 0,
                reasoning: 1,
            },
            1.25,
        );
        unified.set_workspace(
            Some("//server/share/demo-workspace".to_string()),
            Some("demo-workspace".to_string()),
        );
        unified.duration_ms = Some(2500);

        let parsed = unified_to_parsed(&unified);
        let round_tripped = parsed_to_unified(&parsed, 2.5);

        assert_eq!(
            round_tripped.workspace_key.as_deref(),
            Some("//server/share/demo-workspace")
        );
        assert_eq!(
            round_tripped.workspace_label.as_deref(),
            Some("demo-workspace")
        );
        assert_eq!(round_tripped.cost, 2.5);
        assert_eq!(round_tripped.duration_ms, Some(2500));
    }

    #[test]
    fn test_workspace_model_grouping_keeps_real_unknown_workspace_separate() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-1",
                    1.0,
                    Some("unknown-workspace"),
                    Some("unknown-workspace"),
                ),
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-2",
                    2.0,
                    None,
                    None,
                ),
            ],
            &GroupBy::WorkspaceModel,
        );

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| {
            entry.workspace_key.as_deref() == Some("unknown-workspace")
                && entry.workspace_label.as_deref() == Some("unknown-workspace")
                && (entry.cost - 1.0).abs() < f64::EPSILON
        }));
        assert!(entries.iter().any(|entry| {
            entry.workspace_key.is_none()
                && entry.workspace_label.as_deref() == Some(UNKNOWN_WORKSPACE_LABEL)
                && (entry.cost - 2.0).abs() < f64::EPSILON
        }));
    }

    #[test]
    fn test_session_grouping_merges_same_session_and_model() {
        // Two messages with the same session_id + same model — should collapse
        // into one row regardless of the client that produced them, because
        // GroupBy::Session keys on (session_id, model) only.
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-shared",
                    1.25,
                    None,
                    None,
                ),
                make_workspace_message(
                    "amp",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-shared",
                    2.75,
                    None,
                    None,
                ),
            ],
            &GroupBy::Session,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_deref(), Some("session-shared"));
        assert_eq!(entries[0].model, "claude-sonnet-4-5");
        assert!((entries[0].cost - 4.0).abs() < f64::EPSILON);
        assert_eq!(entries[0].message_count, 2);
        assert!(entries[0].workspace_key.is_none());
        assert!(entries[0].workspace_label.is_none());
        // Session grouping does not merge_clients into a comma list.
        assert!(entries[0].merged_clients.is_none());
    }

    #[test]
    fn test_session_grouping_separates_different_sessions() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message("codex", "gpt-5", "openai", "session-a", 1.0, None, None),
                make_workspace_message("codex", "gpt-5", "openai", "session-b", 2.0, None, None),
            ],
            &GroupBy::Session,
        );

        assert_eq!(entries.len(), 2);
        let session_ids: HashSet<_> = entries
            .iter()
            .map(|e| e.session_id.as_deref().unwrap())
            .collect();
        assert_eq!(session_ids, HashSet::from(["session-a", "session-b"]));
    }

    #[test]
    fn test_client_session_grouping_keeps_clients_separate() {
        // Same session_id seen by two different clients (unusual in practice
        // but possible if parsers collide on an id space). ClientSession
        // must yield two rows; Session would yield one (covered above).
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-shared",
                    1.0,
                    None,
                    None,
                ),
                make_workspace_message(
                    "amp",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-shared",
                    3.0,
                    None,
                    None,
                ),
            ],
            &GroupBy::ClientSession,
        );

        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert_eq!(entry.session_id.as_deref(), Some("session-shared"));
            assert!(entry.merged_clients.is_none());
        }
        let by_client: HashSet<_> = entries.iter().map(|e| e.client.as_str()).collect();
        assert_eq!(by_client, HashSet::from(["claude", "amp"]));
    }

    #[test]
    fn test_non_session_grouping_does_not_populate_session_id() {
        // Defensive: only Session/ClientSession variants should set the
        // session_id field on ModelUsage — every other group_by must leave
        // it None so the camelCase JSON output omits it via
        // `skip_serializing_if = "Option::is_none"`.
        for group_by in &[
            GroupBy::Model,
            GroupBy::ClientModel,
            GroupBy::ClientProviderModel,
            GroupBy::WorkspaceModel,
        ] {
            let entries = aggregate_model_usage_entries(
                vec![make_workspace_message(
                    "codex",
                    "gpt-5",
                    "openai",
                    "session-x",
                    1.0,
                    None,
                    None,
                )],
                group_by,
            );
            assert_eq!(entries.len(), 1);
            assert!(
                entries[0].session_id.is_none(),
                "session_id leaked into {:?} grouping",
                group_by
            );
        }
    }

    #[test]
    fn test_retain_for_requested_clients_keeps_original_client_matches() {
        let requested: HashSet<&str> = HashSet::from(["opencode"]);
        assert!(retain_for_requested_clients(
            "opencode",
            "gpt-4o",
            "anthropic",
            &requested
        ));
        assert!(!retain_for_requested_clients(
            "claude",
            "gpt-4o",
            "anthropic",
            &requested
        ));
    }

    #[test]
    fn test_retain_for_requested_clients_accepts_synthetic_gateway_traffic() {
        let requested: HashSet<&str> = HashSet::from(["synthetic"]);
        assert!(retain_for_requested_clients(
            "opencode",
            "hf:deepseek-ai/DeepSeek-V3-0324",
            "unknown",
            &requested
        ));
        assert!(retain_for_requested_clients(
            "synthetic",
            "deepseek-v3-0324",
            "synthetic",
            &requested
        ));
        assert!(!retain_for_requested_clients(
            "opencode",
            "gpt-4o",
            "anthropic",
            &requested
        ));
    }

    #[test]
    fn test_retain_for_requested_clients_preserves_kilo_split() {
        let kilocode_only: HashSet<&str> = HashSet::from(["kilocode"]);
        assert!(retain_for_requested_clients(
            "kilocode",
            "gpt-5",
            "openai",
            &kilocode_only
        ));
        assert!(!retain_for_requested_clients(
            "kilo",
            "gpt-5",
            "openai",
            &kilocode_only
        ));

        let kilo_only: HashSet<&str> = HashSet::from(["kilo"]);
        assert!(retain_for_requested_clients(
            "kilo", "gpt-5", "openai", &kilo_only
        ));
        assert!(!retain_for_requested_clients(
            "kilocode", "gpt-5", "openai", &kilo_only
        ));
    }

    #[test]
    fn test_cursor_parse_path_reprices_zero_cost_composer_1_5_rows() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cursor_cache_dir = temp_dir.path().join(".config/tokscale/cursor-cache");
        std::fs::create_dir_all(&cursor_cache_dir).unwrap();

        let csv = r#"Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost
"2026-03-04T12:00:00.000Z","Included","Composer 1.5","No","1200","1000","5000","2000","8000","0""#;
        std::fs::write(cursor_cache_dir.join("usage.csv"), csv).unwrap();

        let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());
        let messages = parse_all_messages_with_pricing(
            temp_dir.path().to_str().unwrap(),
            &["cursor".to_string()],
            Some(&pricing),
        );

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "cursor");
        assert_eq!(messages[0].model_id, "Composer 1.5");
        assert!(messages[0].cost > 0.0);
    }

    fn write_kimi_repeated_status_fixture(source_home: &std::path::Path) {
        let session_dir = source_home.join(".kimi/sessions/group-1/session-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("wire.jsonl"),
            r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 10, "output": 1, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-progressive"}}}
{"timestamp": 1770983420.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 20, "output": 2, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-progressive"}}}
{"timestamp": 1770983430.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 5, "output": 1, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-distinct"}}}
{"timestamp": 1770983440.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 7, "output": 1, "input_cache_read": 0, "input_cache_creation": 0}}}}
{"timestamp": 1770983450.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 8, "output": 1, "input_cache_read": 0, "input_cache_creation": 0}}}}"#,
        )
        .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_kimi_deduplicates_repeated_status_updates() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            write_kimi_repeated_status_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["kimi".to_string()],
                None,
            );

            assert_eq!(messages.len(), 4);
            assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 40);
            assert_eq!(messages.iter().map(|m| m.tokens.output).sum::<i64>(), 5);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_kimi_deduplicates_repeated_status_updates() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            write_kimi_repeated_status_fixture(source_home.path());

            let parsed = parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["kimi".to_string()]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap();

            assert_eq!(parsed.counts.get(ClientId::Kimi), 4);
            assert_eq!(parsed.messages.len(), 4);
            assert_eq!(parsed.messages.iter().map(|m| m.input).sum::<i64>(), 40);
            assert_eq!(parsed.messages.iter().map(|m| m.output).sum::<i64>(), 5);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    // Regression: the streaming driver must NOT share one dedup set across
    // different clients. kimi and codebuff both emit raw upstream message ids
    // as dedup_key with no client namespace, so a shared set would let one
    // client's key suppress an identical key from the other. Here both a kimi
    // message and a codebuff message carry dedup_key "COLLIDE"; both must
    // survive. With a single shared `seen_keys` (the pre-fix behaviour) the
    // second lane's message is silently dropped and this fails.
    #[test]
    #[serial_test::serial]
    fn test_streaming_driver_does_not_dedup_across_clients() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            // kimi: one StatusUpdate carrying message_id "COLLIDE".
            let kimi_dir = source_home.path().join(".kimi/sessions/g/s");
            std::fs::create_dir_all(&kimi_dir).unwrap();
            std::fs::write(
                kimi_dir.join("wire.jsonl"),
                r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 100, "output": 50, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "COLLIDE"}}}"#,
            )
            .unwrap();

            // codebuff: one assistant message whose upstream id is "COLLIDE".
            let cb_dir = source_home.path().join(".config/manicode/projects/proj");
            std::fs::create_dir_all(&cb_dir).unwrap();
            std::fs::write(
                cb_dir.join("chat-messages.json"),
                r#"[{"role":"assistant","id":"COLLIDE","metadata":{"model":"claude-sonnet-4","usage":{"inputTokens":200,"outputTokens":80}},"credits":0.02}]"#,
            )
            .unwrap();

            let mut seen: Vec<String> = Vec::new();
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["kimi".to_string(), "codebuff".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| seen.push(m.client.clone()),
            );

            assert!(
                seen.iter().any(|c| c == "kimi"),
                "kimi message with shared dedup_key must survive: {seen:?}"
            );
            assert!(
                seen.iter().any(|c| c == "codebuff"),
                "codebuff message with shared dedup_key must survive: {seen:?}"
            );
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    // M2 (codex fork-replay): the parser-level fork dedup (#649/#681) must also
    // collapse replayed parent token_count rows through OUR streaming report
    // path (scan_messages_streaming), not just the materialized
    // parse_all_messages_with_pricing path the upstream tests exercise. Without
    // the fork-parent-scoped dedup key, each fork's replayed parent rows survive
    // per child and inflate codex totals.
    #[test]
    #[serial_test::serial]
    fn test_streaming_codex_collapses_parent_replay_across_forks() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            write_codex_parent_replay_fixture(source_home.path());

            let mut input_sum = 0i64;
            let mut output_sum = 0i64;
            let mut count = 0usize;
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| {
                    input_sum += m.tokens.input;
                    output_sum += m.tokens.output;
                    count += 1;
                },
            );

            // Same collapse as the materialized
            // test_parse_all_messages_with_pricing_codex_deduplicates_parent_replay_across_forks:
            // the parent's two turns plus the single own-turn shared (by identical
            // cumulative total) across the two forks. Without #649/#681 the
            // replayed parent rows would survive per fork and inflate this.
            assert_eq!(count, 3, "replayed parent rows must collapse to 3 messages");
            assert_eq!(input_sum, 140);
            assert_eq!(output_sum, 14);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    // Issue #6: the agents report must dedup the simple_lane! clients
    // (copilot/codebuff/kimi/…) like the model/graph/hourly reports. Here
    // codebuff emits the SAME upstream message id "DUP" in two different
    // project files. The OLD materialized path (parse_local_unified_messages)
    // never gated codebuff, so it counts both; the streaming-backed
    // get_agents_report keeps one — matching get_model_report. Repointing
    // get_agents_report at the old path makes the parity assertion FAIL (RED).
    #[test]
    #[serial_test::serial]
    fn test_agents_report_dedups_like_model_report_issue6() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());
        // Hermetic: cache-only pricing + temp HOME → no network, pricing None.
        std::env::set_var("TOKSCALE_PRICING_CACHE_ONLY", "1");

        {
            let write_codebuff = |proj: &str| {
                let dir = source_home
                    .path()
                    .join(format!(".config/manicode/projects/{proj}"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("chat-messages.json"),
                    r#"[{"role":"assistant","id":"DUP","metadata":{"model":"claude-sonnet-4","usage":{"inputTokens":200,"outputTokens":80}},"credits":0.02}]"#,
                )
                .unwrap();
            };
            write_codebuff("projA");
            write_codebuff("projB");

            let home = source_home.path().to_str().unwrap().to_string();
            let clients = Some(vec!["codebuff".to_string()]);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let agents = rt
                .block_on(get_agents_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: clients.clone(),
                    ..Default::default()
                }))
                .unwrap();
            let model = rt
                .block_on(get_model_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: clients.clone(),
                    ..Default::default()
                }))
                .unwrap();
            let old = rt
                .block_on(parse_local_unified_messages(LocalParseOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: clients.clone(),
                    since: None,
                    until: None,
                    year: None,
                    scanner_settings: scanner::ScannerSettings::default(),
                    modified_after: None,
                }))
                .unwrap();
            let old_total: i32 = old.iter().map(|m| m.message_count.max(0)).sum();

            assert_eq!(old_total, 2, "old materialized path must NOT dedup codebuff");
            assert_eq!(model.total_messages, 1, "model report dedups codebuff");
            assert_eq!(agents.total_messages, 1, "agents report must dedup codebuff");
            assert_eq!(
                agents.total_messages, model.total_messages,
                "issue #6: agents must agree with the model report"
            );
            assert_ne!(
                old_total, model.total_messages,
                "the old path diverged from the model report (the #6 bug)"
            );
            assert!(
                (agents.total_cost - model.total_cost).abs() < 1e-9,
                "agents/model cost parity (agents={}, model={})",
                agents.total_cost,
                model.total_cost
            );
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
        std::env::remove_var("TOKSCALE_PRICING_CACHE_ONLY");
    }

    // Preservation: with no duplicate dedup_keys (and only parse_local==true
    // clients), the streaming-backed agents report produces the SAME numbers the
    // old materialized path did. codebuff + kimi, distinct ids, no agent
    // attribution → a single "Main" bucket.
    #[test]
    #[serial_test::serial]
    fn test_agents_report_preserves_numbers_without_duplicates() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());
        std::env::set_var("TOKSCALE_PRICING_CACHE_ONLY", "1");

        {
            let cb_dir = source_home.path().join(".config/manicode/projects/proj");
            std::fs::create_dir_all(&cb_dir).unwrap();
            std::fs::write(
                cb_dir.join("chat-messages.json"),
                r#"[{"role":"assistant","id":"A","metadata":{"model":"claude-sonnet-4","usage":{"inputTokens":200,"outputTokens":80}},"credits":0.02}]"#,
            )
            .unwrap();
            let kimi_dir = source_home.path().join(".kimi/sessions/g/s");
            std::fs::create_dir_all(&kimi_dir).unwrap();
            std::fs::write(
                kimi_dir.join("wire.jsonl"),
                "{\"type\": \"metadata\", \"protocol_version\": \"1.3\"}\n{\"timestamp\": 1770983410.0, \"message\": {\"type\": \"StatusUpdate\", \"payload\": {\"token_usage\": {\"input_other\": 100, \"output\": 50, \"input_cache_read\": 0, \"input_cache_creation\": 0}, \"message_id\": \"K\"}}}",
            )
            .unwrap();

            let home = source_home.path().to_str().unwrap().to_string();
            let clients = Some(vec!["codebuff".to_string(), "kimi".to_string()]);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let agents = rt
                .block_on(get_agents_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: clients.clone(),
                    ..Default::default()
                }))
                .unwrap();

            assert_eq!(
                agents.entries.len(),
                1,
                "no agent attribution → a single Main bucket"
            );
            let main = &agents.entries[0];
            assert_eq!(main.agent, "Main");
            assert_eq!(main.messages, 2);
            // BTreeSet → sorted, both clients fold into Main.
            assert_eq!(main.clients, vec!["codebuff".to_string(), "kimi".to_string()]);

            // Byte-for-byte equivalence with the old materialized path for the
            // non-duplicate case (both parse identically; only dedup differs).
            let old = rt
                .block_on(parse_local_unified_messages(LocalParseOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients,
                    since: None,
                    until: None,
                    year: None,
                    scanner_settings: scanner::ScannerSettings::default(),
                    modified_after: None,
                }))
                .unwrap();
            let old_input: i64 = old.iter().map(|m| m.tokens.input).sum();
            let old_output: i64 = old.iter().map(|m| m.tokens.output).sum();
            let old_cache_read: i64 = old.iter().map(|m| m.tokens.cache_read).sum();
            let old_cache_write: i64 = old.iter().map(|m| m.tokens.cache_write).sum();
            let old_reasoning: i64 = old.iter().map(|m| m.tokens.reasoning).sum();
            let old_messages: i32 = old.iter().map(|m| m.message_count.max(0)).sum();
            let old_cost: f64 = old.iter().map(|m| m.cost).sum();

            assert_eq!(main.input, old_input);
            assert_eq!(main.output, old_output);
            assert_eq!(main.cache_read, old_cache_read);
            assert_eq!(main.cache_write, old_cache_write);
            assert_eq!(main.reasoning, old_reasoning);
            assert_eq!(main.messages, old_messages);
            assert!((agents.total_cost - old_cost).abs() < 1e-9);
            // Sanity: codebuff contributes its known tokens.
            assert!(main.input >= 200 && main.output >= 80);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
        std::env::remove_var("TOKSCALE_PRICING_CACHE_ONLY");
    }

    // Agent bucketing + fold arithmetic in isolation (no fixtures): normalized
    // names, the "Main" fallback, plain `+=` token sums, and message_count.max(0).
    #[test]
    fn test_agent_bucket_key_and_accumulator() {
        let msg = |agent: Option<&str>| {
            let mut m = UnifiedMessage::new_with_agent(
                "codebuff",
                "m",
                "p",
                "s",
                0,
                TokenBreakdown {
                    input: 10,
                    output: 5,
                    cache_read: 2,
                    cache_write: 1,
                    reasoning: 3,
                },
                0.5,
                agent.map(|a| a.to_string()),
            );
            m.message_count = 2;
            m
        };

        assert_eq!(agent_bucket_key(&msg(None)), "Main");
        assert_eq!(agent_bucket_key(&msg(Some("   "))), "Main");
        assert_eq!(agent_bucket_key(&msg(Some("OmO"))), "Sisyphus");

        let mut acc = AgentAccumulator::default();
        acc.add(&msg(None));
        let mut negative = msg(None);
        negative.message_count = -3; // .max(0) clamp → contributes 0 messages
        acc.add(&negative);

        assert_eq!(acc.input, 20);
        assert_eq!(acc.output, 10);
        assert_eq!(acc.cache_read, 4);
        assert_eq!(acc.cache_write, 2);
        assert_eq!(acc.reasoning, 6);
        assert!((acc.cost - 1.0).abs() < 1e-9);
        assert_eq!(acc.messages, 2, "message_count.max(0): 2 + 0");
        assert!(acc.clients.contains("codebuff"));
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_refreshes_stale_date_on_cache_hit() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let message_dir = source_home
                .path()
                .join(".local/share/opencode/storage/message/project-1");
            std::fs::create_dir_all(&message_dir).unwrap();
            let path = message_dir.join("msg_001.json");
            std::fs::write(
                &path,
                r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            )
            .unwrap();

            let fingerprint = message_cache::SourceFingerprint::from_path(&path).unwrap();
            let mut stale_message = UnifiedMessage::new(
                "opencode",
                "accounts/fireworks/models/deepseek-v3-0324",
                "fireworks",
                "session-1",
                1_733_011_200_000,
                TokenBreakdown {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0,
            );
            stale_message.date = "1900-01-01".to_string();

            let mut cache = message_cache::SourceMessageCache::default();
            cache.insert(message_cache::CachedSourceEntry::new(
                &path,
                fingerprint,
                vec![stale_message],
                Vec::new(),
                None,
            ));
            cache.save_if_dirty();

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );

            assert_eq!(messages.len(), 1);
            assert_ne!(messages[0].date, "1900-01-01");
            assert_eq!(
                messages[0].date,
                UnifiedMessage::new(
                    "opencode",
                    "accounts/fireworks/models/deepseek-v3-0324",
                    "fireworks",
                    "session-1",
                    1_733_011_200_000,
                    TokenBreakdown {
                        input: 10,
                        output: 5,
                        cache_read: 0,
                        cache_write: 0,
                        reasoning: 0,
                    },
                    0.0,
                )
                .date
            );
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn test_empty_parse_results_are_not_cached_for_optional_file_sources() {
        use std::os::unix::fs::PermissionsExt;

        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let message_dir = source_home
                .path()
                .join(".local/share/opencode/storage/message/project-1");
            std::fs::create_dir_all(&message_dir).unwrap();
            let path = message_dir.join("msg_001.json");
            std::fs::write(
                &path,
                r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            )
            .unwrap();

            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o000);
            std::fs::set_permissions(&path, permissions).unwrap();

            let first_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert!(first_messages.is_empty());

            let cache = message_cache::SourceMessageCache::load();
            assert!(cache.get(&path).is_none());

            let mut readable_permissions = std::fs::metadata(&path).unwrap().permissions();
            readable_permissions.set_mode(0o644);
            std::fs::set_permissions(&path, readable_permissions).unwrap();

            let second_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(second_messages.len(), 1);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_empty_cache_hits_are_reparsed_for_optional_file_sources() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let message_dir = source_home
                .path()
                .join(".local/share/opencode/storage/message/project-1");
            std::fs::create_dir_all(&message_dir).unwrap();
            let path = message_dir.join("msg_001.json");
            std::fs::write(
                &path,
                r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            )
            .unwrap();

            let fingerprint = message_cache::SourceFingerprint::from_path(&path).unwrap();
            let mut cache = message_cache::SourceMessageCache::default();
            cache.insert(message_cache::CachedSourceEntry::new(
                &path,
                fingerprint,
                Vec::new(),
                Vec::new(),
                None,
            ));
            cache.save_if_dirty();

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(messages.len(), 1);

            let loaded = message_cache::SourceMessageCache::load();
            let repaired_entry = loaded.get(&path).unwrap();
            assert_eq!(repaired_entry.messages.len(), 1);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_sqlite_source_cache_invalidates_on_wal_change() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let db_dir = source_home.path().join(".local/share/opencode");
            std::fs::create_dir_all(&db_dir).unwrap();
            let db_path = db_dir.join("opencode.db");

            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let journal_mode: String = conn
                .query_row("PRAGMA journal_mode=WAL;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(journal_mode.to_lowercase(), "wal");
            conn.execute_batch(
                "PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE message (
                     id TEXT PRIMARY KEY,
                     session_id TEXT NOT NULL,
                     data TEXT NOT NULL
                 );",
            )
            .unwrap();

            let row_one = r#"{
                "role": "assistant",
                "modelID": "claude-sonnet-4",
                "providerID": "anthropic",
                "tokens": { "input": 100, "output": 50, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                "time": { "created": 1700000000000.0 }
            }"#;
            let row_two = r#"{
                "role": "assistant",
                "modelID": "claude-sonnet-4",
                "providerID": "anthropic",
                "tokens": { "input": 120, "output": 60, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                "time": { "created": 1700000001000.0 }
            }"#;

            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params!["msg-1", "session-1", row_one],
            )
            .unwrap();

            let first_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(first_messages.len(), 1);

            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params!["msg-2", "session-1", row_two],
            )
            .unwrap();
            assert!(db_path.with_extension("db-wal").exists());

            let refreshed_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(refreshed_messages.len(), 2);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_dedups_across_channel_suffixed_opencode_dbs() {
        // Regression guard: a session that appears in both `opencode.db` and
        // `opencode-<channel>.db` (e.g. the user switches channels mid-session)
        // must only be counted once.
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let db_dir = source_home.path().join(".local/share/opencode");
            std::fs::create_dir_all(&db_dir).unwrap();

            let schema = "PRAGMA journal_mode=WAL;
                 PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE message (
                     id TEXT PRIMARY KEY,
                     session_id TEXT NOT NULL,
                     data TEXT NOT NULL
                 );";
            let row = |input: u64, ts: u64| {
                format!(
                    r#"{{
                        "role": "assistant",
                        "modelID": "claude-sonnet-4",
                        "providerID": "anthropic",
                        "tokens": {{ "input": {input}, "output": 10, "reasoning": 0, "cache": {{ "read": 0, "write": 0 }} }},
                        "time": {{ "created": {ts}.0 }}
                    }}"#
                )
            };

            let default_db = db_dir.join("opencode.db");
            let conn = rusqlite::Connection::open(&default_db).unwrap();
            conn.execute_batch(schema).unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "shared-msg",
                    "session-shared",
                    row(100, 1_700_000_000_000u64)
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "latest-only",
                    "session-latest",
                    row(200, 1_700_000_001_000u64)
                ],
            )
            .unwrap();
            drop(conn);

            let stable_db = db_dir.join("opencode-stable.db");
            let conn = rusqlite::Connection::open(&stable_db).unwrap();
            conn.execute_batch(schema).unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "shared-msg",
                    "session-shared",
                    row(100, 1_700_000_000_000u64)
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "stable-only",
                    "session-stable",
                    row(300, 1_700_000_002_000u64)
                ],
            )
            .unwrap();
            drop(conn);

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(
                messages.len(),
                3,
                "expected 3 unique messages (shared + latest-only + stable-only), got {}",
                messages.len()
            );
            let mut ids: Vec<String> = messages
                .iter()
                .filter_map(|m| m.dedup_key.clone())
                .collect();
            ids.sort();
            assert_eq!(ids, vec!["latest-only", "shared-msg", "stable-only"]);

            let messages_warm = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(
                messages_warm.len(),
                3,
                "warm cache must also dedup shared message across channel dbs"
            );
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_opencode_sqlite_deduplicates_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let db_dir = source_home.path().join(".local/share/opencode");
            std::fs::create_dir_all(&db_dir).unwrap();
            let db_path = db_dir.join("opencode.db");
            let conn = create_opencode_sqlite_db(&db_path);

            let msg_a = build_opencode_sqlite_payload(
                1_700_000_000_000.0,
                1_700_000_000_500.0,
                100,
                50,
                0,
                10,
                5,
                0.01,
            );
            let msg_b = build_opencode_sqlite_payload(
                1_700_000_001_000.0,
                1_700_000_001_500.0,
                200,
                80,
                10,
                20,
                0,
                0.02,
            );
            let msg_c = build_opencode_sqlite_payload(
                1_700_000_002_000.0,
                1_700_000_002_500.0,
                300,
                120,
                15,
                0,
                0,
                0.03,
            );

            for (id, session_id, payload) in [
                ("root_a", "root", msg_a.as_str()),
                ("root_b", "root", msg_b.as_str()),
                ("fork_a_copy", "fork", msg_a.as_str()),
                ("fork_b_copy", "fork", msg_b.as_str()),
                ("fork_c_new", "fork", msg_c.as_str()),
            ] {
                conn.execute(
                    "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, session_id, payload],
                )
                .unwrap();
            }
            drop(conn);

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );

            assert_eq!(messages.len(), 3);
            assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 600);
            assert_eq!(messages.iter().map(|m| m.tokens.output).sum::<i64>(), 250);
            assert_eq!(messages.iter().map(|m| m.cost).sum::<f64>(), 0.06);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_opencode_sqlite_counts_deduplicated_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let db_dir = source_home.path().join(".local/share/opencode");
            std::fs::create_dir_all(&db_dir).unwrap();
            let db_path = db_dir.join("opencode.db");
            let conn = create_opencode_sqlite_db(&db_path);

            let msg_a = build_opencode_sqlite_payload(
                1_700_000_000_000.0,
                1_700_000_000_500.0,
                100,
                50,
                0,
                10,
                5,
                0.01,
            );
            let msg_b = build_opencode_sqlite_payload(
                1_700_000_001_000.0,
                1_700_000_001_500.0,
                200,
                80,
                10,
                20,
                0,
                0.02,
            );
            let msg_c = build_opencode_sqlite_payload(
                1_700_000_002_000.0,
                1_700_000_002_500.0,
                300,
                120,
                15,
                0,
                0,
                0.03,
            );

            for (id, session_id, payload) in [
                ("root_a", "root", msg_a.as_str()),
                ("root_b", "root", msg_b.as_str()),
                ("fork_a_copy", "fork", msg_a.as_str()),
                ("fork_b_copy", "fork", msg_b.as_str()),
                ("fork_c_new", "fork", msg_c.as_str()),
            ] {
                conn.execute(
                    "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, session_id, payload],
                )
                .unwrap();
            }
            drop(conn);

            let parsed = parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["opencode".to_string()]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap();

            assert_eq!(parsed.counts.get(ClientId::OpenCode), 3);
            assert_eq!(parsed.messages.len(), 3);
            assert_eq!(parsed.messages.iter().map(|m| m.input).sum::<i64>(), 600);
            assert_eq!(parsed.messages.iter().map(|m| m.output).sum::<i64>(), 250);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    fn write_codex_forked_history_fixture(source_home: &std::path::Path) {
        let codex_dir = source_home.join(".codex/sessions");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("parent.jsonl"),
            concat!(
                r#"{"timestamp":"2026-04-30T10:00:00Z","type":"session_meta","payload":{"id":"parent-session","source":"interactive","model_provider":"openai","cwd":"/Users/alice/root"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65},"last_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65}}}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            codex_dir.join("fork.jsonl"),
            concat!(
                r#"{"timestamp":"2026-04-30T10:01:00Z","type":"session_meta","payload":{"id":"fork-session","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1}}},"model_provider":"openai","cwd":"/Users/alice/root-worktree"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"total_tokens":130}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:02Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65},"last_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":110,"cached_input_tokens":22,"output_tokens":33,"total_tokens":143},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"total_tokens":13}}}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    fn write_codex_parent_replay_fixture(source_home: &std::path::Path) {
        let codex_dir = source_home.join(".codex/sessions");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("parent.jsonl"),
            concat!(
                r#"{"timestamp":"2026-05-24T20:00:00Z","type":"session_meta","payload":{"id":"019e5b00-0000-7000-8000-000000000001","source":"vscode","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-05-24T20:00:01Z","type":"turn_context","payload":{"turn_id":"019e5b00-0001-7000-8000-000000000001","model":"gpt-5.5","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-05-24T20:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110},"last_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-05-24T20:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":130,"output_tokens":13,"total_tokens":143},"last_token_usage":{"input_tokens":30,"output_tokens":3,"total_tokens":33}}}}"#,
                "\n"
            ),
        )
        .unwrap();

        for (filename, child_id, child_turn_id, timestamp) in [
            (
                "child-a.jsonl",
                "019e5c03-1e99-7000-8000-000000000001",
                "019e5c03-6425-7000-8000-000000000001",
                "2026-05-24T21:00:00Z",
            ),
            (
                "child-b.jsonl",
                "019e5c04-1e99-7000-8000-000000000001",
                "019e5c04-6425-7000-8000-000000000001",
                "2026-05-24T22:00:00Z",
            ),
        ] {
            std::fs::write(
                codex_dir.join(filename),
                format!(
                    concat!(
                        r#"{{"timestamp":"{timestamp}","type":"session_meta","payload":{{"id":"{child_id}","forked_from_id":"019e5b00-0000-7000-8000-000000000001","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"019e5b00-0000-7000-8000-000000000001","depth":1}}}}}},"model_provider":"openai","agent_nickname":"worker","cwd":"/repo"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"session_meta","payload":{{"id":"019e5b00-0000-7000-8000-000000000001","source":"vscode","model_provider":"openai","cwd":"/repo"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"turn_context","payload":{{"turn_id":"019e5b00-0001-7000-8000-000000000001","model":"gpt-5.5","cwd":"/repo"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"output_tokens":10,"total_tokens":110}},"last_token_usage":{{"input_tokens":100,"output_tokens":10,"total_tokens":110}}}}}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":130,"output_tokens":13,"total_tokens":143}},"last_token_usage":{{"input_tokens":30,"output_tokens":3,"total_tokens":33}}}}}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"task_started","turn_id":"{child_turn_id}"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"turn_context","payload":{{"turn_id":"{child_turn_id}","model":"gpt-5.5","cwd":"/repo"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":140,"output_tokens":14,"total_tokens":154}},"last_token_usage":{{"input_tokens":10,"output_tokens":1,"total_tokens":11}}}}}}}}"#,
                        "\n",
                    ),
                    timestamp = timestamp,
                    child_id = child_id,
                    child_turn_id = child_turn_id,
                ),
            )
            .unwrap();
        }
    }

    fn write_codex_user_fork_replay_fixture(source_home: &std::path::Path) {
        let sessions_dir = source_home.join(".codex/sessions/2026/01/02");
        let archived_dir = source_home.join(".codex/archived_sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&archived_dir).unwrap();

        std::fs::write(
            archived_dir.join("rollout-2026-01-02T03-04-05-11111111-1111-7111-8111-111111111111.jsonl"),
            concat!(
                r#"{"timestamp":"2026-01-02T03:04:05Z","type":"session_meta","payload":{"id":"11111111-1111-7111-8111-111111111111","source":"vscode","thread_source":"user","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:04:06Z","type":"turn_context","payload":{"turn_id":"11111111-3333-7333-8333-333333333333","model":"gpt-5.5","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:04:07Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"total_tokens":1100},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"total_tokens":1100}}}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:04:08Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"cached_input_tokens":450,"output_tokens":120,"total_tokens":1320},"last_token_usage":{"input_tokens":200,"cached_input_tokens":50,"output_tokens":20,"total_tokens":220}}}}"#,
                "\n"
            ),
        )
        .unwrap();

        std::fs::write(
            sessions_dir.join("rollout-2026-01-02T03-10-00-22222222-2222-7222-8222-222222222222.jsonl"),
            concat!(
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"session_meta","payload":{"id":"22222222-2222-7222-8222-222222222222","forked_from_id":"11111111-1111-7111-8111-111111111111","source":"vscode","thread_source":"user","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"session_meta","payload":{"id":"11111111-1111-7111-8111-111111111111","source":"vscode","thread_source":"user","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"turn_context","payload":{"turn_id":"11111111-3333-7333-8333-333333333333","model":"gpt-5.5","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"total_tokens":1100},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"total_tokens":1100}}}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"cached_input_tokens":450,"output_tokens":120,"total_tokens":1320},"last_token_usage":{"input_tokens":200,"cached_input_tokens":50,"output_tokens":20,"total_tokens":220}}}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:30Z","type":"turn_context","payload":{"turn_id":"22222222-4444-7444-8444-444444444444","model":"gpt-5.5","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:30Z","type":"session_meta","payload":{"id":"22222222-2222-7222-8222-222222222222","forked_from_id":"11111111-1111-7111-8111-111111111111","source":"vscode","thread_source":"user","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:53Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1500,"cached_input_tokens":500,"output_tokens":150,"total_tokens":1650},"last_token_usage":{"input_tokens":300,"cached_input_tokens":50,"output_tokens":30,"total_tokens":330}}}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_deduplicates_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            write_codex_forked_history_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(messages.len(), 3);
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.input)
                    .sum::<i64>(),
                88
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.cache_read)
                    .sum::<i64>(),
                22
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.output)
                    .sum::<i64>(),
                33
            );
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_keeps_user_fork_own_turn() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            write_codex_user_fork_replay_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            let session_ids: HashSet<_> = messages
                .iter()
                .map(|message| message.session_id.as_str())
                .collect();
            assert!(session_ids.contains(
                "rollout-2026-01-02T03-10-00-22222222-2222-7222-8222-222222222222"
            ));
            assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 1000);
            assert_eq!(messages.iter().map(|m| m.tokens.cache_read).sum::<i64>(), 500);
            assert_eq!(messages.iter().map(|m| m.tokens.output).sum::<i64>(), 150);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_deduplicates_parent_replay_across_forks() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            write_codex_parent_replay_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            // Parent contributes its two turns. The two forks each replay the
            // parent history (skipped) and then emit one own turn that lands on
            // the identical cumulative total (140/14). Sibling forks sharing a
            // cumulative total is the signature of a replayed row, so the
            // fork-parent-scoped dedup key collapses them into one. Real fork
            // fan-out replays the same upstream totals into 10-100+ siblings;
            // two distinct turns reaching a byte-identical cumulative vector by
            // chance does not happen in practice because the cumulative encodes
            // each fork's divergent context size.
            assert_eq!(messages.len(), 3);
            assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 140);
            assert_eq!(messages.iter().map(|m| m.tokens.output).sum::<i64>(), 14);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    fn write_codex_twin_token_count_fixture(source_home: &std::path::Path) {
        // Single session with two turns whose `last_token_usage` deltas are
        // byte-identical but emitted at different timestamps. The fork-dedup
        // key includes the cumulative total, so both turns must survive even
        // when a user happens to send two turns producing the same per-turn
        // delta.
        let codex_dir = source_home.join(".codex/sessions");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("twin-deltas.jsonl"),
            concat!(
                r#"{"timestamp":"2026-04-30T11:00:00Z","type":"session_meta","payload":{"id":"twin-session","source":"interactive","model_provider":"openai","cwd":"/Users/alice/root"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T11:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T11:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T11:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20,"cached_input_tokens":4,"output_tokens":6},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_keeps_twin_token_counts_at_distinct_timestamps() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            write_codex_twin_token_count_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(
                messages.len(),
                2,
                "two turns with identical token deltas at distinct timestamps must both survive dedup",
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.input)
                    .sum::<i64>(),
                16,
                "input tokens normalize cache_read out of input: 2 turns × (10 - 2) = 16",
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.output)
                    .sum::<i64>(),
                6,
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.cache_read)
                    .sum::<i64>(),
                4,
            );
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_codex_counts_deduplicated_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            write_codex_forked_history_fixture(source_home.path());

            let parsed = parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["codex".to_string()]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap();

            assert_eq!(parsed.counts.get(ClientId::Codex), 3);
            assert_eq!(parsed.messages.len(), 3);
            assert_eq!(
                parsed
                    .messages
                    .iter()
                    .map(|message| message.input)
                    .sum::<i64>(),
                88
            );
            assert_eq!(
                parsed
                    .messages
                    .iter()
                    .map(|message| message.cache_read)
                    .sum::<i64>(),
                22
            );
            assert_eq!(
                parsed
                    .messages
                    .iter()
                    .map(|message| message.output)
                    .sum::<i64>(),
                33
            );
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_reparses_from_zero_when_incremental_prefix_is_stale() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let codex_dir = source_home.path().join(".codex/sessions");
            std::fs::create_dir_all(&codex_dir).unwrap();
            let path = codex_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);
            assert_eq!(initial_messages[0].model_id, "gpt-5.4");
            assert!(message_cache::SourceMessageCache::load()
                .get(&path)
                .and_then(|entry| entry.codex_incremental.as_ref())
                .is_some());

            std::thread::sleep(std::time::Duration::from_millis(5));
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"cached_input_tokens":3,"output_tokens":5},"last_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            std::env::set_var("HOME", fresh_cache_home.path());
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
            assert_eq!(warm_messages.len(), 2);
            assert!(warm_messages
                .iter()
                .all(|message| message.model_id == "gpt-5.5"));
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_keeps_untimestamped_rows_in_sync_after_append() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let codex_dir = source_home.path().join(".codex/sessions");
            std::fs::create_dir_all(&codex_dir).unwrap();
            let path = codex_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let first_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(first_messages.len(), 1);

            std::thread::sleep(std::time::Duration::from_millis(5));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(
                concat!(
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"cached_input_tokens":3,"output_tokens":5},"last_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2}}}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.flush().unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            std::env::set_var("HOME", fresh_cache_home.path());
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_matches_cold_parse_after_malformed_json_append() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let codex_dir = source_home.path().join(".codex/sessions");
            std::fs::create_dir_all(&codex_dir).unwrap();
            let path = codex_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":999""#,
                    "\n"
                ),
            )
            .unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);

            std::thread::sleep(std::time::Duration::from_millis(5));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(
                concat!(
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"cached_input_tokens":3,"output_tokens":5},"last_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2}}}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.flush().unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert!(message_cache::SourceMessageCache::load()
                .get(&path)
                .is_none());

            std::env::set_var("HOME", fresh_cache_home.path());
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_exact_hit_codex_cache_repairs_fallback_timestamps_without_incremental_state() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let session_dir = source_home.path().join(".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let expected = crate::sessions::codex::parse_codex_file(&path);
            assert_eq!(expected.len(), 1);

            let fingerprint = message_cache::SourceFingerprint::from_path(&path).unwrap();
            let mut stale_message = expected[0].clone();
            stale_message.timestamp = 0;
            stale_message.date = "1900-01-01".to_string();

            let mut cache = message_cache::SourceMessageCache::default();
            cache.insert(message_cache::CachedSourceEntry::new(
                &path,
                fingerprint,
                vec![stale_message],
                vec![0],
                None,
            ));
            cache.save_if_dirty();

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(messages, expected);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_repairs_fallback_timestamps_after_source_mtime_change() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let session_dir = source_home.path().join(".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");
            let contents = concat!(
                r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                "\n"
            );
            std::fs::write(&path, contents).unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);

            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(&path, contents).unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            std::env::set_var("HOME", fresh_cache_home.path());
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
            assert_ne!(warm_messages[0].timestamp, initial_messages[0].timestamp);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_full_log_parse_preserves_valid_messages_before_invalid_line_error() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let session_dir = source_home.path().join(".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");

            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.write_all(&[0xff, b'\n']).unwrap();
            file.flush().unwrap();

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].model_id, "gpt-5.4");

            let cache = message_cache::SourceMessageCache::load();
            assert!(cache.get(&path).is_none());
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_does_not_persist_unknown_before_later_turn_context() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let session_dir = source_home.path().join(".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"session_meta","payload":{"source":"interactive","model_provider":"openai"}}"#,
                    "\n",
                    r#"{"timestamp":"2026-04-27T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);
            assert_eq!(initial_messages[0].model_id, "unknown");
            assert!(message_cache::SourceMessageCache::load()
                .get(&path)
                .is_none());

            std::thread::sleep(std::time::Duration::from_millis(5));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(
                concat!(
                    r#"{"timestamp":"2026-04-27T10:00:04Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.flush().unwrap();

            let resumed_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            std::env::set_var("HOME", fresh_cache_home.path());
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(resumed_messages, fresh_messages);
            assert_eq!(resumed_messages.len(), 1);
            assert_eq!(resumed_messages[0].model_id, "gpt-5.5");

            std::env::set_var("HOME", cache_home.path());
            assert!(message_cache::SourceMessageCache::load()
                .get(&path)
                .is_some());
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_skips_non_newline_terminated_resume_prefix() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let session_dir = source_home.path().join(".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#
                ),
            )
            .unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);
            assert!(message_cache::SourceMessageCache::load()
                .get(&path)
                .is_none());

            std::thread::sleep(std::time::Duration::from_millis(5));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(
                concat!(
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"cached_input_tokens":3,"output_tokens":5},"last_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2}}}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.flush().unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            std::env::set_var("HOME", fresh_cache_home.path());
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
            assert_eq!(warm_messages.len(), 2);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_does_not_reuse_priced_cost_without_pricing_service() {
        let temp_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", temp_home.path());
        {
            let cursor_cache_dir = source_home.path().join(".config/tokscale/cursor-cache");
            std::fs::create_dir_all(&cursor_cache_dir).unwrap();

            let csv = r#"Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost
"2026-03-04T12:00:00.000Z","Included","Composer 1.5","No","1200","1000","5000","2000","8000","0""#;
            std::fs::write(cursor_cache_dir.join("usage.csv"), csv).unwrap();

            let mut litellm = HashMap::new();
            litellm.insert(
                "Composer 1.5".into(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(0.001),
                    output_cost_per_token: Some(0.002),
                    cache_read_input_token_cost: Some(0.0005),
                    ..Default::default()
                },
            );
            let pricing = pricing::PricingService::new(litellm, HashMap::new());

            let repriced_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["cursor".to_string()],
                Some(&pricing),
            );
            assert_eq!(repriced_messages.len(), 1);
            assert!(repriced_messages[0].cost > 0.0);

            let cached_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["cursor".to_string()],
                None,
            );

            assert_eq!(cached_messages.len(), 1);
            assert_eq!(cached_messages[0].cost, 0.0);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn test_apply_pricing_if_available_keeps_existing_cost_without_pricing() {
        let mut msg = UnifiedMessage::new_with_agent(
            "roocode",
            "gpt-4o",
            "provider",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.42,
            Some("planner".to_string()),
        );

        apply_pricing_if_available(&mut msg, None);

        assert_eq!(msg.cost, 0.42);
    }

    #[test]
    fn test_apply_pricing_if_available_overrides_cost_when_pricing_exists() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gpt-4o".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "codex",
            "gpt-4o",
            "provider",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.02);
    }

    #[test]
    fn test_apply_pricing_if_available_applies_zed_hosted_markup() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "claude-sonnet-4-5".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "zed",
            "claude-sonnet-4-5",
            crate::sessions::zed::ZED_HOSTED_PROVIDER,
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert!((msg.cost - 0.022).abs() < 1e-12);
    }

    #[test]
    fn test_apply_pricing_if_available_skips_zed_markup_for_non_zed_client() {
        // Non-zed client with provider_id "zed.dev" must not receive the +10%
        // markup. The multiplier is gated on (client == "zed" AND provider).
        let mut litellm = HashMap::new();
        litellm.insert(
            "claude-sonnet-4-5".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "claudecode",
            "claude-sonnet-4-5",
            crate::sessions::zed::ZED_HOSTED_PROVIDER,
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        // 10 * 0.001 + 5 * 0.002 = 0.020, no markup.
        assert!((msg.cost - 0.020).abs() < 1e-12);
    }

    #[test]
    fn test_apply_pricing_if_available_skips_zed_markup_for_byok_provider() {
        // A Zed message whose provider_id is the upstream provider directly
        // (BYOK / non-hosted path) must not be marked up — the user is paying
        // the upstream API directly, not through Zed.
        let mut litellm = HashMap::new();
        litellm.insert(
            "claude-sonnet-4-5".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "zed",
            "claude-sonnet-4-5",
            "anthropic",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert!((msg.cost - 0.020).abs() < 1e-12);
    }

    #[test]
    fn test_apply_pricing_if_available_uses_reasoning_for_gemini() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gemini-2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "gemini",
            "gemini-2.5-pro",
            "google",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 7,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.034);
    }

    #[test]
    fn test_apply_pricing_if_available_uses_cache_read_pricing_for_gemini() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gemini-2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                cache_read_input_token_cost: Some(0.0001),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "gemini",
            "gemini-2.5-pro",
            "google",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 7,
                cache_write: 0,
                reasoning: 3,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.0267);
    }

    #[test]
    fn test_apply_pricing_if_available_uses_market_rate_for_free_variant() {
        let mut openrouter = HashMap::new();
        openrouter.insert(
            "z-ai/glm-4.7".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(HashMap::new(), openrouter);

        let mut msg = UnifiedMessage::new(
            "opencode",
            "glm-4.7-free",
            "modal",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.02);
    }

    #[test]
    fn test_apply_pricing_if_available_prefers_provider_aware_match() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "xai/grok-code-fast-1-0825".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        litellm.insert(
            "azure_ai/grok-code-fast-1".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "opencode",
            "grok-code",
            "azure",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.2);
    }

    #[test]
    fn test_apply_pricing_if_available_uses_nested_reseller_exact_match() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gpt-4".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        litellm.insert(
            "azure/openai/gpt-4".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "opencode",
            "gpt-4",
            "azure",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.2);
    }

    #[test]
    fn test_apply_pricing_if_available_keeps_scoped_fireworks_cost_without_exact_pricing() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "fireworks_ai/accounts/fireworks/models/deepseek-r1-0528-distill-qwen3-8b".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.0000002),
                output_cost_per_token: Some(0.0000002),
                ..Default::default()
            },
        );

        let mut openrouter = HashMap::new();
        openrouter.insert(
            "deepseek/deepseek-v4-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.000001),
                output_cost_per_token: Some(0.000002),
                ..Default::default()
            },
        );

        let pricing = pricing::PricingService::new(litellm, openrouter);
        let mut msg = UnifiedMessage::new(
            "opencode",
            "accounts/fireworks/models/deepseek-v4-pro",
            "fireworks",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.123,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.123);
    }

    #[test]
    fn test_apply_pricing_if_available_prefers_provider_specific_exact_match_over_plain_exact() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gemini-2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                cache_creation_input_token_cost: None,
                ..Default::default()
            },
        );

        let mut openrouter = HashMap::new();
        openrouter.insert(
            "google/gemini-2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                cache_creation_input_token_cost: Some(0.01),
                ..Default::default()
            },
        );

        let pricing = pricing::PricingService::new(litellm, openrouter);

        let mut msg = UnifiedMessage::new(
            "opencode",
            "gemini-2.5-pro",
            "google",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 3,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.05);
    }

    #[test]
    fn test_apply_pricing_if_available_normalizes_openai_codex_provider() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "openai/gpt-5.2-preview".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        litellm.insert(
            "google/gpt-5.2-preview-max".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.1),
                output_cost_per_token: Some(0.2),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "openclaw",
            "gpt-5.2",
            "openai-codex",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.2);
    }

    #[test]
    fn test_apply_pricing_if_available_prices_claude_code_gpt_5_3_codex() {
        let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());

        let mut msg = UnifiedMessage::new(
            "claude",
            "gpt-5.3-codex",
            "openai",
            "session-1",
            1_776_000_000_000,
            TokenBreakdown {
                input: 1_000_000,
                output: 100_000,
                cache_read: 50_000,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        let expected = 1.75 + 1.4 + 0.00875;
        assert!((msg.cost - expected).abs() < 1e-12);
    }

    #[test]
    fn test_apply_pricing_if_available_prices_claude_code_minimax_model() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "minimax/minimax-m2.1".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "claude",
            "MiniMax-M2.1",
            "minimax",
            "session-1",
            1_776_000_000_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.2);
    }

    #[test]
    fn test_apply_pricing_if_available_prices_kimi_k2p6_alias() {
        let mut openrouter = HashMap::new();
        openrouter.insert(
            "moonshotai/kimi-k2.6".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(9.5e-7),
                output_cost_per_token: Some(0.000004),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(HashMap::new(), openrouter);

        let mut msg = UnifiedMessage::new(
            "kimi",
            "k2p6",
            "kimi-for-coding",
            "session-1",
            1_776_000_000_000,
            TokenBreakdown {
                input: 1_000_000,
                output: 250_000,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        let expected = 1_000_000.0 * 9.5e-7 + 250_000.0 * 0.000004;
        assert!((msg.cost - expected).abs() < 1e-12);
        assert!(msg.cost > 0.0);
    }

    #[test]
    fn test_select_local_parse_pricing_prefers_fresh_service_for_new_models() {
        let mut fresh_litellm = HashMap::new();
        fresh_litellm.insert(
            "gpt-5.4".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.000002),
                output_cost_per_token: Some(0.00001),
                ..Default::default()
            },
        );
        let fresh = Arc::new(pricing::PricingService::new(fresh_litellm, HashMap::new()));
        let stale = pricing::PricingService::new(HashMap::new(), HashMap::new());
        let selected = select_local_parse_pricing(Ok(Arc::clone(&fresh)), || Some(stale)).unwrap();

        let mut msg = UnifiedMessage::new(
            "opencode",
            "gpt-5.4",
            "openai",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(selected.as_ref()));

        assert!(msg.cost > 0.0);
    }

    #[test]
    fn test_select_local_parse_pricing_falls_back_to_stale_cache_on_fetch_error() {
        let mut stale_litellm = HashMap::new();
        stale_litellm.insert(
            "gpt-5.2".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.00000175),
                output_cost_per_token: Some(0.000014),
                ..Default::default()
            },
        );
        let stale = pricing::PricingService::new(stale_litellm, HashMap::new());

        let selected =
            select_local_parse_pricing(Err("network failed".to_string()), || Some(stale)).unwrap();

        assert!(selected.lookup_with_source("gpt-5.2", None).is_some());
    }

    #[test]
    fn test_select_local_parse_pricing_does_not_evaluate_stale_fallback_on_fresh_success() {
        let fresh = Arc::new(pricing::PricingService::new(HashMap::new(), HashMap::new()));
        let mut stale_called = false;

        let selected = select_local_parse_pricing(Ok(Arc::clone(&fresh)), || {
            stale_called = true;
            None
        })
        .unwrap();

        assert!(Arc::ptr_eq(&selected, &fresh));
        assert!(!stale_called);
    }

    #[test]
    fn test_dedupe_latest_trae_messages_keeps_latest_timestamp_for_session() {
        let messages = vec![
            make_trae_message(
                "session-stable",
                1_700_000_002_000,
                Some("trae:session-stable:1_700_000_002"),
                0.2,
            ),
            make_trae_message(
                "session-stable",
                1_700_000_003_000,
                Some("trae:session-stable:1_700_000_003"),
                0.3,
            ),
            make_trae_message(
                "session-other",
                1_700_000_001_000,
                Some("trae:session-other:1_700_000_001"),
                0.1,
            ),
        ];

        let deduped = dedupe_latest_trae_messages(messages);

        assert_eq!(deduped.len(), 2);
        let stable = deduped
            .iter()
            .find(|msg| msg.session_id == "session-stable")
            .expect("session-stable should remain after dedupe");
        assert_eq!(stable.timestamp, 1_700_000_003_000);
        assert_eq!(stable.cost, 0.3);
        assert_eq!(
            stable.dedup_key.as_deref(),
            Some("trae:session-stable:1_700_000_003")
        );
    }

    #[test]
    fn test_dedupe_latest_trae_messages_tiebreaks_by_dedup_key() {
        let messages = vec![
            make_trae_message(
                "session-stable",
                1_700_000_010_000,
                Some("dedupe-key-a"),
                0.2,
            ),
            make_trae_message(
                "session-stable",
                1_700_000_010_000,
                Some("dedupe-key-z"),
                0.4,
            ),
            make_trae_message(
                "session-stable",
                1_700_000_009_000,
                Some("dedupe-key-m"),
                0.1,
            ),
        ];

        let deduped = dedupe_latest_trae_messages(messages);

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].timestamp, 1_700_000_010_000);
        assert_eq!(deduped[0].dedup_key.as_deref(), Some("dedupe-key-z"));
        assert_eq!(deduped[0].cost, 0.4);
    }

    #[test]
    fn test_parse_all_messages_with_pricing_keeps_gateway_message_under_synthetic_filter() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let message_dir = temp_dir
            .path()
            .join(".local/share/opencode/storage/message/project-1");
        std::fs::create_dir_all(&message_dir).unwrap();
        std::fs::write(
            message_dir.join("msg_001.json"),
            r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"hf:deepseek-ai/DeepSeek-V3-0324","providerID":"unknown","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
        )
        .unwrap();

        let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());
        let messages = parse_all_messages_with_pricing(
            temp_dir.path().to_str().unwrap(),
            &["synthetic".to_string()],
            Some(&pricing),
        );

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "opencode");
        assert_eq!(messages[0].model_id, "deepseek-v3-0324");
        assert_eq!(messages[0].provider_id, "synthetic");
    }

    #[test]
    fn test_parse_local_clients_preserves_gateway_message_client_counts() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let message_dir = temp_dir
            .path()
            .join(".local/share/opencode/storage/message/project-1");
        std::fs::create_dir_all(&message_dir).unwrap();
        std::fs::write(
            message_dir.join("msg_001.json"),
            r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
        )
        .unwrap();

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["opencode".to_string(), "synthetic".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::OpenCode), 1);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].client, "opencode");
        assert_eq!(parsed.messages[0].model_id, "deepseek-v3-0324");
        assert_eq!(parsed.messages[0].provider_id, "fireworks");
    }

    #[test]
    fn test_parse_all_messages_fireworks_provider_kept_under_synthetic_only_filter() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let message_dir = temp_dir
            .path()
            .join(".local/share/opencode/storage/message/project-1");
        std::fs::create_dir_all(&message_dir).unwrap();
        std::fs::write(
            message_dir.join("msg_001.json"),
            r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0.1,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
        )
        .unwrap();

        let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());
        let messages = parse_all_messages_with_pricing(
            temp_dir.path().to_str().unwrap(),
            &["synthetic".to_string()],
            Some(&pricing),
        );

        assert_eq!(
            messages.len(),
            1,
            "fireworks gateway message must not be dropped when filtering for synthetic"
        );
        assert_eq!(messages[0].client, "opencode");
        assert_eq!(messages[0].model_id, "deepseek-v3-0324");
        assert_eq!(messages[0].provider_id, "fireworks");
    }

    #[test]
    fn test_parse_local_clients_fireworks_provider_kept_under_synthetic_only_filter() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let message_dir = temp_dir
            .path()
            .join(".local/share/opencode/storage/message/project-1");
        std::fs::create_dir_all(&message_dir).unwrap();
        std::fs::write(
            message_dir.join("msg_001.json"),
            r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0.1,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
        )
        .unwrap();

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["synthetic".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(
            parsed.messages.len(),
            1,
            "fireworks gateway message must not be dropped when filtering for synthetic only"
        );
        assert_eq!(parsed.messages[0].client, "opencode");
        assert_eq!(parsed.messages[0].model_id, "deepseek-v3-0324");
        assert_eq!(parsed.messages[0].provider_id, "fireworks");
    }

    #[test]
    fn test_parse_local_clients_honors_scanner_settings_opencode_db_paths() {
        // Regression guard: `parse_local_clients` used to call
        // `scan_all_clients_with_env_strategy`, which silently dropped
        // `options.scanner_settings`. Users with
        // `scanner.opencodeDbPaths` pointing at an OPENCODE_DB outside the
        // XDG data dir would see no rows through the clients/wrapped
        // command paths even though model/monthly/graph reports honored
        // the same config.
        let temp_dir = tempfile::TempDir::new().unwrap();
        // Deliberately do not create ~/.local/share/opencode so nothing
        // is auto-discoverable; the only db the scanner can find must
        // come from `scanner_settings`.
        let outside_dir = temp_dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let external_db = outside_dir.join("opencode.db");

        let conn = rusqlite::Connection::open(&external_db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 data TEXT NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "ext-msg-1",
                "ext-session",
                r#"{
                    "role": "assistant",
                    "modelID": "claude-sonnet-4",
                    "providerID": "anthropic",
                    "tokens": { "input": 42, "output": 7, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                    "time": { "created": 1700000000000.0 }
                }"#
            ],
        )
        .unwrap();
        drop(conn);

        // Without scanner_settings: no rows (nothing auto-discoverable).
        let parsed_default = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["opencode".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();
        assert_eq!(parsed_default.counts.get(ClientId::OpenCode), 0);
        assert!(parsed_default.messages.is_empty());

        // With scanner_settings pointing at the external db: the user
        // row must show up.
        let parsed_with_settings = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["opencode".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                opencode_db_paths: vec![external_db.clone()],
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();
        assert_eq!(
            parsed_with_settings.counts.get(ClientId::OpenCode),
            1,
            "scanner.opencodeDbPaths must reach the parse_local_clients path"
        );
        assert_eq!(parsed_with_settings.messages.len(), 1);
        assert_eq!(parsed_with_settings.messages[0].client, "opencode");
        assert_eq!(parsed_with_settings.messages[0].model_id, "claude-sonnet-4");
    }

    #[test]
    fn test_parse_local_clients_honors_scanner_extra_scan_paths_for_hermes_profile_db() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let profile_dir = temp_dir.path().join(".hermes/profiles/director_planning");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let profile_db = profile_dir.join("state.db");
        let conn = create_hermes_sqlite_db(&profile_db);
        insert_hermes_session(
            &conn,
            "hermes-extra-session",
            "claude-sonnet-4",
            2,
            100,
            25,
            0.07,
        );
        drop(conn);

        let parsed_default = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();
        assert_eq!(parsed_default.counts.get(ClientId::Hermes), 0);
        assert!(parsed_default.messages.is_empty());

        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert("hermes".to_string(), vec![profile_dir]);
        let parsed_with_settings = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed_with_settings.counts.get(ClientId::Hermes), 2);
        assert_eq!(parsed_with_settings.messages.len(), 1);
        assert_eq!(parsed_with_settings.messages[0].client, "hermes");
        assert_eq!(
            parsed_with_settings.messages[0].agent.as_deref(),
            Some("Hermes Agent")
        );
        assert_eq!(
            parsed_with_settings.messages[0].session_id,
            "hermes-extra-session"
        );
        assert_eq!(parsed_with_settings.messages[0].model_id, "claude-sonnet-4");
        assert_eq!(parsed_with_settings.messages[0].input, 100);
        assert_eq!(parsed_with_settings.messages[0].output, 25);
    }

    #[test]
    fn test_modified_after_never_prunes_hermes_dbs_from_extra_scan_paths() {
        // SQLite WAL writes may leave the main db file's mtime untouched, so
        // `modified_after` must not prune Hermes/Zed dbs even when they come
        // from user scan roots (the `files` lanes) rather than the default
        // single-db path. A threshold in the future would prune any mtime.
        let temp_dir = tempfile::TempDir::new().unwrap();
        let profile_dir = temp_dir.path().join(".hermes/profiles/director_planning");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let profile_db = profile_dir.join("state.db");
        let conn = create_hermes_sqlite_db(&profile_db);
        insert_hermes_session(
            &conn,
            "hermes-wal-session",
            "claude-sonnet-4",
            1,
            50,
            10,
            0.03,
        );
        drop(conn);

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert("hermes".to_string(), vec![profile_dir]);
        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: Some(future_ms),
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Hermes), 1);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].session_id, "hermes-wal-session");
    }

    #[test]
    fn test_modified_after_never_prunes_antigravity_cli_dbs() {
        // Antigravity CLI conversation `.db` files arrive via the generic
        // `files` lane (a `*.db` glob), but they are SQLite — WAL writes may
        // leave the main db mtime untouched, so they must be exempt from mtime
        // pruning like Hermes/Zed. A plain-file client with the same old mtime
        // is still pruned (control).
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cli_db = temp_dir.path().join("conv.db");
        std::fs::File::create(&cli_db).unwrap();
        let claude_log = temp_dir.path().join("session.jsonl");
        std::fs::File::create(&claude_log).unwrap();

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::AntigravityCli)
            .push(cli_db.clone());
        scan_result
            .get_mut(ClientId::Claude)
            .push(claude_log.clone());

        // A threshold in the future would prune any real on-disk mtime.
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        crate::prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert_eq!(
            scan_result.get(ClientId::AntigravityCli),
            std::slice::from_ref(&cli_db),
            "Antigravity CLI .db (a WAL-mode SQLite source) must survive mtime pruning"
        );
        assert!(
            scan_result.get(ClientId::Claude).is_empty(),
            "a plain-file client's stale log is still pruned"
        );
    }

    /// Write a minimal Antigravity CLI conversation DB (one priced
    /// `gen_metadata` row carrying `response_id`). The `trajectory_metadata_blob`
    /// table is omitted on purpose — the parser tolerates its absence and falls
    /// back to the file mtime for the timestamp.
    fn write_antigravity_cli_db(
        conversations_dir: &std::path::Path,
        file_stem: &str,
        response_id: &str,
    ) {
        fn encode_varint(mut value: u64) -> Vec<u8> {
            let mut out = Vec::new();
            loop {
                let mut byte = (value & 0x7f) as u8;
                value >>= 7;
                if value != 0 {
                    byte |= 0x80;
                }
                out.push(byte);
                if value == 0 {
                    break;
                }
            }
            out
        }
        fn enc_varint(field: u64, value: u64) -> Vec<u8> {
            let mut out = encode_varint(field << 3);
            out.extend(encode_varint(value));
            out
        }
        fn enc_len(field: u64, payload: &[u8]) -> Vec<u8> {
            let mut out = encode_varint((field << 3) | 2);
            out.extend(encode_varint(payload.len() as u64));
            out.extend_from_slice(payload);
            out
        }

        let mut usage = Vec::new();
        usage.extend(enc_varint(2, 500)); // new input
        usage.extend(enc_varint(9, 300)); // output
        usage.extend(enc_len(11, response_id.as_bytes())); // responseId
        let mut chat_model = Vec::new();
        chat_model.extend(enc_len(4, &usage));
        chat_model.extend(enc_len(19, b"gemini-3-flash-a"));
        let gen_blob = enc_len(1, &chat_model);

        std::fs::create_dir_all(conversations_dir).unwrap();
        let path = conversations_dir.join(format!("{file_stem}.db"));
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE gen_metadata (idx integer, data blob, size integer);")
            .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            rusqlite::params![gen_blob],
        )
        .unwrap();
    }

    // Two independent Antigravity CLI conversation DBs that reuse the same
    // responseId must both survive the streaming report path. responseIds are
    // unique only within a conversation, so the cross-file dedup gate is
    // namespaced by session; with a bare-responseId key (the pre-fix behaviour)
    // the second conversation is silently dropped and this fails (count == 1).
    #[test]
    #[serial_test::serial]
    fn test_streaming_antigravity_cli_keeps_colliding_response_ids_across_conversations() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let conversations_dir = source_home
                .path()
                .join(".gemini/antigravity-cli/conversations");
            write_antigravity_cli_db(&conversations_dir, "conv-aaa", "SHARED");
            write_antigravity_cli_db(&conversations_dir, "conv-bbb", "SHARED");

            let mut sessions: Vec<String> = Vec::new();
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["antigravity-cli".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| sessions.push(m.session_id.clone()),
            );

            sessions.sort();
            assert_eq!(
                sessions,
                vec!["conv-aaa".to_string(), "conv-bbb".to_string()],
                "both conversations reusing responseId \"SHARED\" must survive"
            );
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    // jcode (`~/.jcode/sessions/session_*.json`) must be discovered by the
    // generic scanner (EnvVar JCODE_HOME / .jcode root, `session_*.json` glob)
    // and flow through the streaming lane with its authoritative per-message
    // token_usage.
    #[test]
    #[serial_test::serial]
    fn test_streaming_jcode_flows_through_lane() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let sessions_dir = source_home.path().join(".jcode/sessions");
            std::fs::create_dir_all(&sessions_dir).unwrap();
            std::fs::write(
                sessions_dir.join("session_test.json"),
                r#"{"id":"session_test","provider_key":"cliproxyapi","model":"claude-sonnet-4","working_dir":"/x","messages":[{"id":"u1","role":"user","timestamp":"2026-06-16T12:00:00Z"},{"id":"a1","role":"assistant","timestamp":"2026-06-16T12:00:01Z","token_usage":{"input_tokens":1200,"output_tokens":300}}]}"#,
            )
            .unwrap();

            let mut input_sum = 0i64;
            let mut count = 0usize;
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["jcode".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| {
                    input_sum += m.tokens.input;
                    count += 1;
                },
            );

            assert_eq!(count, 1, "the jcode assistant message must flow through the streaming lane");
            assert_eq!(input_sum, 1200);
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    // micode (`$XDG_DATA_HOME/micode/*.db`, WAL-mode SQLite) must be discovered
    // via the generic `*.db` glob and flow through the streaming lane, keeping
    // its authoritative per-message cost intact (MiMo models are unpriced, so
    // apply_pricing leaves the embedded cost alone).
    #[test]
    #[serial_test::serial]
    fn test_streaming_micode_flows_with_authoritative_cost() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let micode_dir = source_home.path().join(".local/share/micode");
            std::fs::create_dir_all(&micode_dir).unwrap();
            let db_path = micode_dir.join("test.db");
            {
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                conn.execute_batch(
                    "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);",
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![
                        "msg_001",
                        "ses_001",
                        r#"{"role":"assistant","modelID":"mimo-v2.5-pro","providerID":"mimo","cost":0.05,"tokens":{"input":1000,"output":500},"time":{"created":1700000000000.0,"completed":1700000001000.0}}"#
                    ],
                )
                .unwrap();
            }

            let mut cost_sum = 0.0f64;
            let mut count = 0usize;
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["micode".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| {
                    cost_sum += m.cost;
                    count += 1;
                },
            );

            assert_eq!(count, 1, "the micode assistant message must flow through the streaming lane");
            assert!(
                (cost_sum - 0.05).abs() < 1e-9,
                "authoritative micode cost must survive pricing (got {cost_sum})"
            );
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    // micode `.db` is WAL-mode SQLite reached via the generic `*.db` glob, so it
    // must be exempt from mtime pruning (a WAL-only write leaves the main db's
    // mtime untouched) — same treatment as Antigravity CLI / Hermes / Zed.
    #[test]
    fn test_modified_after_never_prunes_micode_dbs() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let micode_db = temp_dir.path().join("micode.db");
        std::fs::File::create(&micode_db).unwrap();
        let claude_log = temp_dir.path().join("session.jsonl");
        std::fs::File::create(&claude_log).unwrap();

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::MiMoCode)
            .push(micode_db.clone());
        scan_result
            .get_mut(ClientId::Claude)
            .push(claude_log.clone());

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        crate::prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert_eq!(
            scan_result.get(ClientId::MiMoCode),
            std::slice::from_ref(&micode_db),
            "micode .db (a WAL-mode SQLite source) must survive mtime pruning"
        );
        assert!(
            scan_result.get(ClientId::Claude).is_empty(),
            "a plain-file client's stale log is still pruned"
        );
    }

    #[test]
    fn test_parse_local_clients_honors_scanner_extra_scan_paths_for_zed_threads_db() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let windows_threads_dir = temp_dir.path().join("AppData/Local/Zed/threads");
        std::fs::create_dir_all(&windows_threads_dir).unwrap();
        let threads_db = windows_threads_dir.join("threads.db");
        let conn = create_zed_sqlite_db(&threads_db);
        insert_zed_thread(&conn, "zed-extra-thread", "claude-sonnet-4-5");
        drop(conn);

        let parsed_default = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["zed".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();
        assert_eq!(parsed_default.counts.get(ClientId::Zed), 0);
        assert!(parsed_default.messages.is_empty());

        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert("zed".to_string(), vec![windows_threads_dir]);
        let parsed_with_settings = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["zed".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed_with_settings.counts.get(ClientId::Zed), 1);
        assert_eq!(parsed_with_settings.messages.len(), 1);
        assert_eq!(parsed_with_settings.messages[0].client, "zed");
        assert_eq!(
            parsed_with_settings.messages[0].session_id,
            "zed-extra-thread"
        );
        assert_eq!(
            parsed_with_settings.messages[0].model_id,
            "claude-sonnet-4-5"
        );
        assert_eq!(parsed_with_settings.messages[0].input, 42);
        assert_eq!(parsed_with_settings.messages[0].output, 7);
    }

    #[test]
    fn test_parse_local_clients_dedups_zed_threads_across_default_and_extra_dbs() {
        let temp_dir = tempfile::TempDir::new().unwrap();

        // Place threads.db at the default platform path so the scanner finds it
        // as `zed_db` AND we also pass it via extraScanPaths.
        let default_threads_dir = temp_dir.path().join(".local/share/zed/threads");
        std::fs::create_dir_all(&default_threads_dir).unwrap();
        let default_db = default_threads_dir.join("threads.db");
        let conn = create_zed_sqlite_db(&default_db);
        insert_zed_thread(&conn, "shared-zed-thread", "claude-sonnet-4-5");
        drop(conn);

        // Point extraScanPaths.zed at the same directory — dedup should prevent
        // the thread from appearing twice.
        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert("zed".to_string(), vec![default_threads_dir.clone()]);
        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["zed".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        // Should see exactly 1 message, not 2 (deduped by canonicalize).
        assert_eq!(parsed.counts.get(ClientId::Zed), 1);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].session_id, "shared-zed-thread");
    }

    #[test]
    fn test_parse_local_clients_zed_extra_scan_paths_nonexistent_dir_is_silent() {
        let temp_dir = tempfile::TempDir::new().unwrap();

        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert(
            "zed".to_string(),
            vec![temp_dir.path().join("does/not/exist")],
        );
        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["zed".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Zed), 0);
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn test_parse_local_clients_dedups_hermes_sessions_across_default_and_extra_dbs() {
        let temp_dir = tempfile::TempDir::new().unwrap();

        let default_dir = temp_dir.path().join(".hermes");
        std::fs::create_dir_all(&default_dir).unwrap();
        let default_db = default_dir.join("state.db");
        let default_conn = create_hermes_sqlite_db(&default_db);
        insert_hermes_session(
            &default_conn,
            "shared-hermes-session",
            "claude-sonnet-4",
            2,
            100,
            25,
            0.07,
        );
        drop(default_conn);

        let profile_dir = temp_dir.path().join(".hermes/profiles/director_planning");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let profile_db = profile_dir.join("state.db");
        let profile_conn = create_hermes_sqlite_db(&profile_db);
        insert_hermes_session(
            &profile_conn,
            "shared-hermes-session",
            "claude-sonnet-4",
            9,
            999,
            999,
            9.99,
        );
        drop(profile_conn);

        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert("hermes".to_string(), vec![profile_db]);
        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Hermes), 2);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].session_id, "shared-hermes-session");
        assert_eq!(parsed.messages[0].input, 100);
        assert_eq!(parsed.messages[0].output, 25);
    }

    #[test]
    fn test_parse_local_clients_claude_filter_ignores_scanner_settings_opencode_db_paths() {
        // Regression guard for the scanner client-filter bypass: even
        // when `scanner.opencodeDbPaths` pins an external opencode db,
        // a `--clients claude` request must NOT pull in OpenCode rows.
        // Before the fix, the merge ran outside the OpenCode-enabled
        // guard so user-pinned dbs leaked through both `messages` and
        // `counts` (the latter is computed before the message-level
        // client filter, so even the post-filter pipeline could not
        // hide a leaked count).
        let temp_dir = tempfile::TempDir::new().unwrap();

        // Claude session: one assistant message, the only thing the
        // filter should accept.
        let claude_dir = temp_dir.path().join(".claude/projects/myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("conversation.jsonl"),
            r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}
"#,
        )
        .unwrap();

        // External opencode.db that the user has pinned via
        // scanner.opencodeDbPaths. Without the fix, this would leak
        // into the Claude-only result.
        let outside_dir = temp_dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let external_db = outside_dir.join("opencode.db");
        let conn = rusqlite::Connection::open(&external_db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 data TEXT NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "leaked-opencode",
                "should-not-show-up",
                r#"{
                    "role": "assistant",
                    "modelID": "claude-sonnet-4",
                    "providerID": "anthropic",
                    "tokens": { "input": 9999, "output": 9999, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                    "time": { "created": 1700000000000.0 }
                }"#
            ],
        )
        .unwrap();
        drop(conn);

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                opencode_db_paths: vec![external_db.clone()],
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        assert_eq!(
            parsed.counts.get(ClientId::OpenCode),
            0,
            "OpenCode count must stay zero under a Claude-only filter even \
             when scanner.opencodeDbPaths is set"
        );
        assert_eq!(
            parsed.counts.get(ClientId::Claude),
            1,
            "Claude message must still be counted"
        );
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].client, "claude");
        assert!(
            parsed.messages.iter().all(|m| m.client != "opencode"),
            "no OpenCode messages may leak into a Claude-only result, got {:?}",
            parsed.messages
        );
    }

    #[test]
    fn test_parse_local_clients_claude_transcripts_count_only_usage_metadata() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let transcripts_dir = temp_dir.path().join(".claude/transcripts");
        std::fs::create_dir_all(&transcripts_dir).unwrap();
        std::fs::write(
            transcripts_dir.join("ses_123456789012345678901234567.jsonl"),
            r#"{"type":"user","timestamp":"2026-04-01T10:00:00.000Z","message":{"content":"Wrapped prompt"}}
{"type":"assistant","timestamp":"2026-04-01T10:00:01.000Z","requestId":"req_wrapper","message":{"id":"msg_wrapper","model":"claude-sonnet-4","usage":{"input_tokens":123,"output_tokens":45,"cache_read_input_tokens":67,"cache_creation_input_tokens":8}}}
"#,
        )
        .unwrap();
        std::fs::write(
            transcripts_dir.join("ses_765432109876543210987654321.jsonl"),
            r#"{"type":"user","timestamp":"2026-04-01T10:00:00.000Z","message":{"content":"Wrapped prompt"}}
{"type":"tool_use","timestamp":"2026-04-01T10:00:01.000Z","message":{"content":"Run tool"}}
{"type":"tool_result","timestamp":"2026-04-01T10:00:02.000Z","message":{"content":"Tool result"}}
"#,
        )
        .unwrap();

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Claude), 1);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].client, "claude");
        assert_eq!(
            parsed.messages[0].session_id,
            "ses_123456789012345678901234567"
        );
        assert_eq!(parsed.messages[0].model_id, "claude-sonnet-4");
        assert_eq!(parsed.messages[0].input, 123);
        assert_eq!(parsed.messages[0].output, 45);
        assert_eq!(parsed.messages[0].cache_read, 67);
        assert_eq!(parsed.messages[0].cache_write, 8);
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_refreshes_cc_mirror_provider_when_variant_metadata_changes() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let variant_dir = source_home.path().join(".cc-mirror/kimi-code");
            let config_dir = source_home.path().join("mirror-configs/kimi-code");
            let project_dir = config_dir.join("projects/project-one");
            std::fs::create_dir_all(&project_dir).unwrap();
            std::fs::create_dir_all(&variant_dir).unwrap();
            let variant_path = variant_dir.join("variant.json");
            std::fs::write(
                &variant_path,
                format!(
                    r#"{{"name":"kimi-code","provider":"kimi","configDir":"{}"}}"#,
                    config_dir.display()
                ),
            )
            .unwrap();
            let session_path = project_dir.join("session.jsonl");
            std::fs::write(
                &session_path,
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}
"#,
            )
            .unwrap();

            let first_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["claude".to_string()],
                None,
            );
            assert_eq!(first_messages.len(), 1);
            assert_eq!(first_messages[0].client, "cc-mirror/kimi-code");
            assert_eq!(first_messages[0].provider_id, "kimi");

            std::fs::write(
                &variant_path,
                format!(
                    r#"{{"name":"kimi-code","provider":"minimax","configDir":"{}"}}"#,
                    config_dir.display()
                ),
            )
            .unwrap();

            let refreshed_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["claude".to_string()],
                None,
            );
            assert_eq!(refreshed_messages.len(), 1);
            assert_eq!(refreshed_messages[0].client, "cc-mirror/kimi-code");
            assert_eq!(refreshed_messages[0].provider_id, "minimax");
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_keeps_normal_claude_when_cc_mirror_points_at_claude_config() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", cache_home.path());

        {
            let claude_dir = source_home.path().join(".claude");
            let project_dir = claude_dir.join("projects/project-one");
            std::fs::create_dir_all(&project_dir).unwrap();
            let session_path = project_dir.join("session.jsonl");
            std::fs::write(
                &session_path,
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}
"#,
            )
            .unwrap();

            let variant_dir = source_home.path().join(".cc-mirror/plain-mirror");
            std::fs::create_dir_all(&variant_dir).unwrap();
            std::fs::write(
                variant_dir.join("variant.json"),
                format!(
                    r#"{{"name":"plain-mirror","provider":"mirror","configDir":"{}"}}"#,
                    claude_dir.display()
                ),
            )
            .unwrap();

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["claude".to_string()],
                None,
            );
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].client, "claude");
        }

        match original_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn test_parse_local_clients_amp_partial_ledger_recovers_message_fallback_day() {
        use chrono::TimeZone;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let amp_dir = temp_dir.path().join(".local/share/amp/threads");
        std::fs::create_dir_all(&amp_dir).unwrap();

        let thread_created = chrono::DateTime::parse_from_rfc3339("2026-04-04T12:00:00Z")
            .unwrap()
            .timestamp_millis();
        let ledger_timestamp = chrono::DateTime::parse_from_rfc3339("2026-04-08T12:00:00Z")
            .unwrap()
            .timestamp_millis();

        let thread = format!(
            r#"{{
                "id": "thread-amp-gap",
                "created": {thread_created},
                "usageLedger": {{
                    "events": [
                        {{
                            "timestamp": "2026-04-08T12:00:00Z",
                            "model": "claude-sonnet-4-0",
                            "credits": 0.75,
                            "tokens": {{ "input": 100, "output": 20 }}
                        }}
                    ]
                }},
                "messages": [
                    {{
                        "role": "assistant",
                        "messageId": 1,
                        "usage": {{
                            "model": "claude-sonnet-4-0",
                            "inputTokens": 100,
                            "outputTokens": 20,
                            "credits": 0.75
                        }}
                    }},
                    {{
                        "role": "assistant",
                        "messageId": 2,
                        "usage": {{
                            "model": "claude-sonnet-4-0",
                            "inputTokens": 50,
                            "outputTokens": 10,
                            "credits": 0.40
                        }}
                    }}
                ]
            }}"#
        );
        std::fs::write(amp_dir.join("T-thread-amp-gap.json"), thread).unwrap();

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["amp".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Amp), 2);
        assert_eq!(parsed.messages.len(), 2);

        let dates: HashSet<String> = parsed.messages.iter().map(|msg| msg.date.clone()).collect();
        let local_date = |timestamp_ms: i64| {
            chrono::Local
                .timestamp_millis_opt(timestamp_ms)
                .single()
                .unwrap()
                .format("%Y-%m-%d")
                .to_string()
        };
        assert!(dates.contains(&local_date(thread_created + 2000)));
        assert!(dates.contains(&local_date(ledger_timestamp)));
    }

    // =========================================================================
    // fold_messages_streaming parity tests (RED — fold_messages_streaming not yet impl)
    // =========================================================================

    /// Deterministic UnifiedMessage fixture helper shared with parity tests.
    /// Uses no real JSONL files; all fields are constructed inline.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn parity_msg(
        date: &str,
        client: &str,
        model: &str,
        session_id: &str,
        dedup_key: Option<&str>,
        timestamp_ms: i64,
        input: i64,
        output: i64,
        cost: f64,
    ) -> crate::sessions::UnifiedMessage {
        use crate::TokenBreakdown;
        crate::sessions::UnifiedMessage {
            client: client.to_string(),
            model_id: model.to_string(),
            provider_id: "anthropic".to_string(),
            session_id: session_id.to_string(),
            workspace_key: None,
            workspace_label: None,
            timestamp: timestamp_ms,
            date: date.to_string(),
            tokens: TokenBreakdown {
                input,
                output,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            cost,
            duration_ms: None,
            message_count: 1,
            agent: None,
            dedup_key: dedup_key.map(|s| s.to_string()),
            is_turn_start: false,
        }
    }

    // A/B parity: fold_messages_streaming output == aggregate_by_date output
    // for the same deterministic fixture (no dedup_keys, no trae).
    #[test]
    fn test_fold_messages_streaming_parity_with_aggregate_by_date_no_dedup() {
        let messages = vec![
            parity_msg("2025-06-01", "claude", "claude-sonnet-4-5", "s1", None,
                1_748_000_000_000, 100, 50, 0.01),
            parity_msg("2025-06-01", "opencode", "gpt-4o", "s2", None,
                1_748_000_001_000, 200, 100, 0.02),
            parity_msg("2025-06-02", "codex", "gpt-5", "s3", None,
                1_748_086_400_000, 400, 200, 0.04),
        ];

        // Reference: existing aggregate_by_date (clone-based)
        let reference = crate::aggregator::aggregate_by_date(messages.clone());

        // Subject: new streaming path
        let streaming = fold_messages_streaming(&messages);

        assert_eq!(
            reference.len(), streaming.len(),
            "parity: day bucket count must match"
        );
        for (ref_day, stream_day) in reference.iter().zip(streaming.iter()) {
            assert_eq!(ref_day.date, stream_day.date, "parity: date must match");
            assert_eq!(
                ref_day.totals.tokens, stream_day.totals.tokens,
                "parity: tokens must match for date {}", ref_day.date
            );
            assert!(
                (ref_day.totals.cost - stream_day.totals.cost).abs() < 1e-9,
                "parity: cost must match for date {}", ref_day.date
            );
            assert_eq!(
                ref_day.totals.messages, stream_day.totals.messages,
                "parity: message_count must match for date {}", ref_day.date
            );
        }
    }

    // A/B parity with cross-file dedup: fold_messages_streaming must apply
    // the same dedup_key filtering as the existing pipeline does via seen_keys.
    #[test]
    fn test_fold_messages_streaming_parity_cross_file_dedup() {
        // Construct messages that include a duplicated dedup_key pair.
        // The existing pipeline filters duplicates via the seen_keys HashSet.
        // fold_messages_streaming must produce the same counts.
        let unique = parity_msg("2025-06-10", "claude", "claude-sonnet-4-5", "u1",
            Some("unique-key-1"), 1_749_000_000_000, 300, 150, 0.06);
        let dup_first = parity_msg("2025-06-10", "claude", "claude-sonnet-4-5", "d1",
            Some("dup-key-shared"), 1_749_000_001_000, 200, 100, 0.04);
        let dup_second = parity_msg("2025-06-10", "claude", "claude-haiku-4-5", "d2",
            Some("dup-key-shared"), 1_749_000_002_000, 200, 100, 0.04);

        // The reference pipeline keeps only the first occurrence of dup-key-shared
        // (seen_keys.insert returns false on second) — 2 messages total.
        let all_msgs = vec![unique.clone(), dup_first.clone(), dup_second.clone()];

        let streaming = fold_messages_streaming(&all_msgs);

        assert_eq!(streaming.len(), 1, "all on same date -> 1 bucket");
        assert_eq!(
            streaming[0].totals.messages, 2,
            "parity: duplicate dedup_key must reduce count from 3 to 2"
        );
        assert!(
            (streaming[0].totals.cost - 0.10).abs() < 1e-9,
            "parity: cost must exclude the duplicate message"
        );
    }

    // =========================================================================
    // Phase 2 RED tests: build_graph_result_from_messages streaming entry-point
    // =========================================================================
    //
    // `build_graph_result_from_messages` does NOT exist yet.  These tests
    // define the observable contract that the GREEN implementation must satisfy:
    //   - accept a `&[UnifiedMessage]` slice and an optional `since` date string
    //   - apply the `since` post-parse filter (date string prefix comparison,
    //     same semantics as `filter_messages_for_report`)
    //   - drive aggregation via `StreamingAggregator` (zero-clone fold path)
    //   - return a `GraphResult` whose per-day tokens/cost match the reference
    //     `aggregate_by_date` pipeline exactly (zero tolerance)
    //
    // All three tests will produce a **compile error** until the function is
    // declared in lib.rs, which is the required RED state.

    use crate::GraphResult;

    /// Multi-client, multi-day fixture: streaming path total tokens and cost
    /// per daily bucket must match hand-computed expected values.
    ///
    /// Hardcoded expected values — calculation:
    ///
    /// 2025-06-01 (no dedup):
    ///   s1: input=500, output=250 → tokens=750, cost=0.05
    ///   s2: input=300, output=150 → tokens=450, cost=0.03
    ///   TOTAL: tokens=1200, cost=0.08, messages=2
    ///
    /// 2025-06-02 (trae session dedup — same session_id="trae-sess"):
    ///   trae-k1: ts=1_748_822_400_000, input=100, output=50  → tokens=150, cost=0.01
    ///   trae-k2: ts=1_748_822_500_000, input=200, output=100 → tokens=300, cost=0.02
    ///   StreamingAggregator keeps latest timestamp → trae-k2 wins
    ///   TOTAL: tokens=300, cost=0.02, messages=1
    ///
    /// 2025-06-03 (cross-file dedup_key — both carry "dup-phase2"):
    ///   d1: input=400, output=200 → tokens=600, cost=0.04  (first seen — kept)
    ///   d2: input=400, output=200 → tokens=600, cost=0.04  (same dedup_key — dropped)
    ///   TOTAL: tokens=600, cost=0.04, messages=1
    #[test]
    fn test_build_graph_result_from_messages_matches_aggregate_by_date() {
        let messages = vec![
            // Day 2025-06-01: two clients, no dedup
            parity_msg("2025-06-01", "claude", "claude-sonnet-4-5", "s1", None,
                1_748_736_000_000, 500, 250, 0.05),
            parity_msg("2025-06-01", "opencode", "gpt-4o", "s2", None,
                1_748_736_001_000, 300, 150, 0.03),
            // Day 2025-06-02: trae dedup by session_id — two entries same session, keep latest
            parity_msg("2025-06-02", "trae", "gpt-5.2", "trae-sess", Some("trae-k1"),
                1_748_822_400_000, 100, 50, 0.01),
            parity_msg("2025-06-02", "trae", "gpt-5.2", "trae-sess", Some("trae-k2"),
                1_748_822_500_000, 200, 100, 0.02),   // newer timestamp -> wins
            // Day 2025-06-03: cross-file dedup pair — same dedup_key, second dropped
            parity_msg("2025-06-03", "claude", "claude-haiku-4-5", "d1",
                Some("dup-phase2"), 1_748_908_800_000, 400, 200, 0.04),
            parity_msg("2025-06-03", "claude", "claude-haiku-4-5", "d2",
                Some("dup-phase2"), 1_748_908_801_000, 400, 200, 0.04), // same dedup_key -> discarded
        ];

        // Subject: new streaming entry-point
        let result: GraphResult =
            crate::build_graph_result_from_messages(&messages, None);

        // Verify bucket count: 3 distinct dates
        assert_eq!(
            result.contributions.len(), 3,
            "phase2 streaming: must produce exactly 3 daily buckets"
        );

        // Locate each day bucket by date (sort order: ascending)
        let day1 = result.contributions.iter().find(|c| c.date == "2025-06-01")
            .expect("phase2: 2025-06-01 bucket must exist");
        let day2 = result.contributions.iter().find(|c| c.date == "2025-06-02")
            .expect("phase2: 2025-06-02 bucket must exist");
        let day3 = result.contributions.iter().find(|c| c.date == "2025-06-03")
            .expect("phase2: 2025-06-03 bucket must exist");

        // 2025-06-01: s1 (750) + s2 (450) = 1200 tokens, 0.05+0.03=0.08 cost, 2 messages
        assert_eq!(day1.totals.tokens, 1200,
            "2025-06-01: tokens must be 750+450=1200");
        assert!(
            (day1.totals.cost - 0.08).abs() < 1e-9,
            "2025-06-01: cost must be 0.05+0.03=0.08"
        );
        assert_eq!(day1.totals.messages, 2,
            "2025-06-01: both non-trae non-dedup messages must be counted");

        // 2025-06-02: trae session dedup — trae-k2 wins (larger timestamp)
        // trae-k2: input=200, output=100 -> tokens=300, cost=0.02
        assert_eq!(day2.totals.tokens, 300,
            "2025-06-02: trae dedup — only winner (trae-k2, tokens=300) counted");
        assert!(
            (day2.totals.cost - 0.02).abs() < 1e-9,
            "2025-06-02: trae dedup — cost must be 0.02 (trae-k2 only)"
        );
        assert_eq!(day2.totals.messages, 1,
            "2025-06-02: trae dedup collapses 2 entries to 1 per session_id");

        // 2025-06-03: cross-file dedup — d1 kept, d2 dropped (same dedup_key)
        // d1: input=400, output=200 -> tokens=600, cost=0.04
        assert_eq!(day3.totals.tokens, 600,
            "2025-06-03: cross-file dedup — only d1 (tokens=600) counted, d2 dropped");
        assert!(
            (day3.totals.cost - 0.04).abs() < 1e-9,
            "2025-06-03: cross-file dedup — cost must be 0.04 (d1 only)"
        );
        assert_eq!(day3.totals.messages, 1,
            "2025-06-03: duplicate dedup_key dropped, 1 message retained");
    }

    /// `since` filter semantics: same fixture with `since = "2025-06-02"` must
    /// produce only the 2025-06-02 and 2025-06-03 buckets, with their
    /// token/cost totals matching a manually filtered reference.
    #[test]
    fn test_build_graph_result_from_messages_since_filter_excludes_earlier_dates() {
        let messages = vec![
            parity_msg("2025-06-01", "claude", "claude-sonnet-4-5", "s1", None,
                1_748_736_000_000, 500, 250, 0.05),
            parity_msg("2025-06-01", "opencode", "gpt-4o", "s2", None,
                1_748_736_001_000, 300, 150, 0.03),
            parity_msg("2025-06-02", "codex", "gpt-5", "s3", None,
                1_748_822_400_000, 400, 200, 0.04),
            parity_msg("2025-06-03", "claude", "claude-haiku-4-5", "s4", None,
                1_748_908_800_000, 200, 100, 0.02),
        ];

        // Subject: streaming entry with since = "2025-06-02"
        // (function does not exist yet -> RED compile error)
        let result: GraphResult =
            crate::build_graph_result_from_messages(&messages, Some("2025-06-02"));

        // Only 2025-06-02 and 2025-06-03 must be present
        assert_eq!(
            result.contributions.len(), 2,
            "since filter: must exclude 2025-06-01, leaving 2 buckets"
        );

        let dates: Vec<&str> = result.contributions.iter().map(|c| c.date.as_str()).collect();
        assert!(dates.contains(&"2025-06-02"),
            "since filter: 2025-06-02 bucket must be present");
        assert!(dates.contains(&"2025-06-03"),
            "since filter: 2025-06-03 bucket must be present");
        assert!(!dates.contains(&"2025-06-01"),
            "since filter: 2025-06-01 bucket must be absent");

        // 2025-06-02 token total: input 400 + output 200 = 600
        let day2 = result.contributions.iter().find(|c| c.date == "2025-06-02").unwrap();
        assert_eq!(day2.totals.tokens, 600,
            "since filter: 2025-06-02 token total must be 600");
        assert!(
            (day2.totals.cost - 0.04).abs() < 1e-9,
            "since filter: 2025-06-02 cost must be 0.04"
        );
    }

    /// Trae dedup in streaming path: two messages for the same trae session
    /// (same `session_id`, different `dedup_key`, later timestamp wins) must
    /// produce exactly ONE message worth of tokens/cost in the daily bucket.
    #[test]
    fn test_build_graph_result_from_messages_trae_session_dedup_keeps_latest() {
        let messages = vec![
            // Earlier trae message (should be dropped)
            parity_msg("2025-06-10", "trae", "gpt-5.2", "trae-sess-a", Some("trae-early"),
                1_749_513_600_000, 100, 50, 0.01),
            // Later trae message for same session_id (should win)
            parity_msg("2025-06-10", "trae", "gpt-5.2", "trae-sess-a", Some("trae-late"),
                1_749_513_700_000, 300, 150, 0.03),
            // Non-trae message (should be included as-is)
            parity_msg("2025-06-10", "claude", "claude-sonnet-4-5", "c1", None,
                1_749_513_800_000, 200, 100, 0.02),
        ];

        // Subject: streaming entry (does not exist yet -> RED compile error)
        let result: GraphResult =
            crate::build_graph_result_from_messages(&messages, None);

        assert_eq!(result.contributions.len(), 1,
            "trae dedup: all messages on same date -> 1 bucket");

        let day = &result.contributions[0];
        // Kept messages: trae-late (tokens=450) + claude (tokens=300) = 750 total tokens
        assert_eq!(day.totals.tokens, 750,
            "trae dedup: token total must reflect only the winning trae entry (450) + claude (300)");
        assert!(
            (day.totals.cost - 0.05).abs() < 1e-9,
            "trae dedup: cost must be 0.03 (latest trae) + 0.02 (claude) = 0.05"
        );
        assert_eq!(day.totals.messages, 2,
            "trae dedup: message count must be 2 (1 trae winner + 1 claude)");
    }

}
