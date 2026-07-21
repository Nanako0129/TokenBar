#![deny(clippy::all)]

mod aggregator;
mod cc_mirror;
pub mod clients;
pub mod fs_atomic;
mod message_cache;
pub mod model_alias;
mod parser;
pub mod paths;
pub mod pricing;
mod provider_identity;
pub mod scanner;
pub mod sessionize;
pub mod sessions;

pub use aggregator::*;
pub use clients::{ClientCounts, ClientDef, ClientId, PathRoot};
pub use model_alias::{
    clear_model_aliases, model_alias_generation, model_aliases,
    register_usage_data_invalidation_hook, set_model_aliases, snapshot_grouping_aliases,
    GroupingAliasSnapshot, ModelAliasMap,
};
pub use parser::*;
pub use scanner::*;
pub use sessionize::{
    compute_daily_active_time, compute_time_metrics, sessionize, SessionizeAccumulator,
    SessionInterval, TimeMetrics, DEFAULT_IDLE_GAP_MS,
};
pub use sessions::{CostSource, UnifiedMessage};

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

/// Canonical model identity — the model id that leaves the machine.
///
/// This is [`normalize_syntactic`] with **no alias folding**: purely structural
/// canonicalization (lowercase, strip a `(reasoning-tier)` suffix, strip a
/// trailing `-YYYYMMDD` date, rewrite `.`→`-` inside claude version numbers, and
/// fold an `anthropic/claude-…` prefix). It never consults the user's
/// machine-local model aliases.
///
/// Every path that submits, uploads, exports as raw data, or persists a model id
/// MUST use this, not [`normalize_model_for_grouping`]. A machine-local alias
/// config must never rewrite the model identity persisted server-side, or usage
/// history would fragment and fork across a user's devices. Graph
/// `ClientContribution` keys also use this so export/raw identity stays stable.
pub fn canonical_model_id(model_id: &str) -> String {
    normalize_syntactic(model_id)
}

/// Local display/grouping model name: [`canonical_model_id`] plus the user's
/// configured model-alias fold. Every local report-grouping surface — the models
/// report, every `GroupBy`, monthly, and hourly — routes through this so name
/// variants fold uniformly for presentation.
///
/// The alias fold is **presentation only** and must never reach the
/// submit/upload/export/persist path (those use [`canonical_model_id`]), pricing
/// (which resolves the raw message `model_id`), or the message-cache key space.
/// An empty/unset alias config makes this identical to [`canonical_model_id`].
pub fn normalize_model_for_grouping(model_id: &str) -> String {
    model_alias::apply_global(normalize_syntactic(model_id))
}

/// Structural-only model-name normalization: lowercase, strip a
/// `(reasoning-tier)` suffix, strip a trailing `-YYYYMMDD` date, rewrite `.`→`-`
/// inside claude version numbers, and fold an `anthropic/claude-…` prefix.
///
/// This is the syntactic half of [`normalize_model_for_grouping`] /
/// [`canonical_model_id`]. It is also used by [`model_alias`] to normalize
/// configured alias keys and values into the same space, so a configured alias
/// matches its model regardless of case, dated suffix, or `.`-vs-`-` spelling.
pub(crate) fn normalize_syntactic(model_id: &str) -> String {
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
        // saturating so clamped (i64::MAX) buckets from a corrupt source can't
        // overflow the sum.
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.reasoning)
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

fn parse_kimi_source(path: &Path) -> Vec<UnifiedMessage> {
    if sessions::kimi::is_kimi_code_path(path) {
        sessions::kimi::parse_kimi_code_file(path)
    } else {
        sessions::kimi::parse_kimi_file(path)
    }
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

    // Parse OpenCode from both stores before merging so a provider-reported
    // duplicate wins even when it appears after an estimated copy.
    let opencode_sqlite_outcomes: Vec<CachedParseOutcome> = scan_result
        .opencode_dbs
        .iter()
        .map(|db_path| {
            load_or_parse_sqlite_source(db_path, &source_cache, pricing, |path| {
                sessions::opencode::parse_opencode_sqlite(path)
            })
        })
        .collect();
    let opencode_json_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::OpenCode)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::opencode::parse_opencode_file(path)
                    .into_iter()
                    .collect()
            })
        })
        .collect();
    let opencode_authoritative = opencode_authoritative_sources(
        opencode_sqlite_outcomes
            .iter()
            .chain(opencode_json_outcomes.iter())
            .flat_map(|outcome| outcome.messages.iter())
            .map(opencode_identity_group),
    );
    let mut opencode_selection = OpenCodeSelection::new(opencode_authoritative);

    for outcome in opencode_sqlite_outcomes {
        all_messages.extend(
            outcome
                .messages
                .into_iter()
                .filter_map(|message| opencode_selection.select_sqlite(message)),
        );
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }
    for outcome in opencode_json_outcomes {
        all_messages.extend(
            outcome
                .messages
                .into_iter()
                .filter_map(|message| opencode_selection.select_json(message, true)),
        );
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }
    all_messages.extend(opencode_selection.finish());

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
            load_or_parse_source_with_fingerprint(
                path,
                &source_cache,
                pricing,
                message_cache::SourceFingerprint::from_droid_path,
                sessions::droid::parse_droid_file,
            )
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

    let kimi_outcomes: Vec<(bool, CachedParseOutcome)> = scan_result
        .get(ClientId::Kimi)
        .par_iter()
        .map(|path| {
            (
                sessions::kimi::is_kimi_code_path(path),
                load_or_parse_source_with_fingerprint(
                    path,
                    &source_cache,
                    pricing,
                    message_cache::SourceFingerprint::from_kimi_path,
                    parse_kimi_source,
                ),
            )
        })
        .collect();
    let mut kimi_code_seen: HashSet<String> = HashSet::new();
    for (is_kimi_code, outcome) in kimi_outcomes {
        if is_kimi_code {
            all_messages.extend(
                outcome
                    .messages
                    .into_iter()
                    .filter(|message| should_keep_deduped_message(&mut kimi_code_seen, message)),
            );
        } else {
            all_messages.extend(outcome.messages);
        }
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let junie_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Junie)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::junie::parse_junie_file(path)
            })
        })
        .collect();
    let mut junie_seen: HashSet<String> = HashSet::new();
    for outcome in junie_outcomes {
        all_messages.extend(
            outcome
                .messages
                .into_iter()
                .filter(|message| should_keep_deduped_message(&mut junie_seen, message)),
        );
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    let opencodereview_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::OpenCodeReview)
        .par_iter()
        .map(|path| {
            load_or_parse_source(path, &source_cache, pricing, |path| {
                sessions::opencodereview::parse_opencodereview_file(path)
            })
        })
        .collect();
    let mut opencodereview_seen: HashSet<String> = HashSet::new();
    for outcome in opencodereview_outcomes {
        all_messages.extend(
            outcome
                .messages
                .into_iter()
                .filter(|message| should_keep_deduped_message(&mut opencodereview_seen, message)),
        );
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
            // from_roo_path folds the sibling api_conversation_history.json into
            // the fingerprint (parse_roo_kilo_file reads model/agent from it), so
            // a history-only rewrite invalidates the cache (#741).
            load_or_parse_source_with_fingerprint(
                path,
                &source_cache,
                pricing,
                message_cache::SourceFingerprint::from_roo_path,
                sessions::roocode::parse_roocode_file,
            )
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
            load_or_parse_source_with_fingerprint(
                path,
                &source_cache,
                pricing,
                message_cache::SourceFingerprint::from_roo_path,
                sessions::kilocode::parse_kilocode_file,
            )
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
            load_or_parse_source_with_fingerprint(
                path,
                &source_cache,
                pricing,
                message_cache::SourceFingerprint::from_roo_path,
                sessions::cline::parse_cline_file,
            )
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

    // micode: WAL-mode SQLite, cached via from_sqlite_path (-wal-aware). Pass
    // pricing: None so the loader returns the raw embedded cost, then reprice
    // below only when it's absent (cost-guarded, #742 Part 2 — mirrors the
    // streaming lane and gjc so MiMo Code's authoritative cost is never
    // overwritten by a recomputed tokens*rate). This materialized path is dead
    // code today (public API only), guarded here for parity with the streaming
    // lane.
    let micode_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::MiMoCode)
        .par_iter()
        .map(|path| {
            load_or_parse_sqlite_source(path, &source_cache, None, |path| {
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
                .map(|mut m| {
                    if m.cost <= 0.0 {
                        apply_pricing_if_available(&mut m, pricing);
                    }
                    m
                })
                .filter(|message| should_keep_deduped_message(&mut micode_seen, message)),
        );
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }

    // gjc: non-cached. Every message enters the shared pricing policy, which
    // preserves provider-reported totals and estimates only unknown costs;
    // message-level dedup collapses depth-1/depth-2 replays.
    let mut gjc_seen: HashSet<String> = HashSet::new();
    let gjc_messages: Vec<UnifiedMessage> = scan_result
        .get(ClientId::Gjc)
        .par_iter()
        .flat_map(|path| {
            sessions::gjc::parse_gjc_file(path)
                .into_iter()
                .map(|mut msg| {
                    apply_pricing_if_available(&mut msg, pricing);
                    msg
                })
                .collect::<Vec<_>>()
        })
        .collect();
    all_messages.extend(
        gjc_messages
            .into_iter()
            .filter(|message| should_keep_deduped_message(&mut gjc_seen, message)),
    );

    // Grok Build has two representations of the same sessions: legacy
    // per-session updates and a global per-inference unified log. Collect both
    // raw sets without pricing and apply unified-over-legacy precedence exactly
    // once before pricing or any report fold; a downstream aggregate cannot
    // subtract covered legacy usage safely.
    let grok_outcomes: Vec<CachedParseOutcome> = scan_result
        .get(ClientId::Grok)
        .par_iter()
        .map(|path| {
            load_or_parse_source_with_fingerprint(
                path,
                &source_cache,
                None,
                message_cache::SourceFingerprint::from_grok_path,
                sessions::grok::parse_grok_file,
            )
        })
        .collect();
    let mut grok_messages = Vec::new();
    for outcome in grok_outcomes {
        grok_messages.extend(outcome.messages);
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }
    all_messages.extend(
        sessions::grok::prefer_unified_log_messages(grok_messages)
            .into_iter()
            .map(|mut message| {
                apply_pricing_if_available(&mut message, pricing);
                message
            }),
    );

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

    // Kiro globalStorage has a precedence relation between self-contained
    // snapshots and execution records. Cache each source's raw parser output,
    // then suppress only after every file has been collected. The suppression
    // result must never be written back into the per-source cache.
    let kiro_outcomes: Vec<(PathBuf, CachedParseOutcome)> = scan_result
        .get(ClientId::Kiro)
        .par_iter()
        .map(|path| {
            (
                path.clone(),
                load_or_parse_source_with_fingerprint(
                    path,
                    &source_cache,
                    None,
                    message_cache::SourceFingerprint::from_kiro_path,
                    sessions::kiro::parse_kiro_file,
                ),
            )
        })
        .collect();
    let mut kiro_sources = Vec::new();
    for (path, outcome) in kiro_outcomes {
        kiro_sources.push((path, outcome.messages));
        if let Some(entry) = outcome.cache_entry {
            source_cache.insert(entry);
        }
    }
    let mut kiro_messages = sessions::kiro::merge_kiro_source_messages(kiro_sources);
    apply_pricing_to_messages(&mut kiro_messages, pricing);
    all_messages.extend(kiro_messages);

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
    // One alias snapshot for the whole fold so a mid-fold set_model_aliases
    // cannot split messages across two grouping configs.
    let aliases = model_alias::snapshot_grouping_aliases();

    for msg in messages {
        let normalized = aliases.fold(normalize_syntactic(&msg.model_id));
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

        // saturating_add so clamped (i64::MAX) buckets from a corrupt source
        // can't overflow the fold (matches the grand-total sum below).
        entry.input = entry.input.saturating_add(msg.tokens.input);
        entry.output = entry.output.saturating_add(msg.tokens.output);
        entry.cache_read = entry.cache_read.saturating_add(msg.tokens.cache_read);
        entry.cache_write = entry.cache_write.saturating_add(msg.tokens.cache_write);
        entry.reasoning = entry.reasoning.saturating_add(msg.tokens.reasoning);
        entry.message_count += msg.message_count.max(0);
        entry.cost += msg.cost;
        entry
            .performance
            .record_message(positive_token_total(&msg.tokens), msg.duration_ms);
    }

    let mut entries: Vec<ModelUsage> = model_map
        .into_values()
        .map(|mut entry| {
            let total_tokens = entry
                .input
                .max(0)
                .saturating_add(entry.output.max(0))
                .saturating_add(entry.cache_read.max(0))
                .saturating_add(entry.cache_write.max(0))
                .saturating_add(entry.reasoning.max(0));
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
    // saturating so multiple clamped (i64::MAX) buckets can't overflow the sum.
    tokens
        .input
        .max(0)
        .saturating_add(tokens.output.max(0))
        .saturating_add(tokens.cache_read.max(0))
        .saturating_add(tokens.cache_write.max(0))
        .saturating_add(tokens.reasoning.max(0))
}

/// Sum the (input, output, cache_read, cache_write) token fields across model
/// usage entries with saturating_add, so clamped (i64::MAX) entry buckets from a
/// corrupt source can't overflow the report-level totals (the entries are
/// already saturated per-field by aggregate_model_usage_entries).
fn model_report_token_totals(entries: &[ModelUsage]) -> (i64, i64, i64, i64) {
    entries.iter().fold(
        (0, 0, 0, 0),
        |(input, output, cache_read, cache_write), entry| {
            (
                input.saturating_add(entry.input),
                output.saturating_add(entry.output),
                cache_read.saturating_add(entry.cache_read),
                cache_write.saturating_add(entry.cache_write),
            )
        },
    )
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

/// Two-level client filter for the streaming hourly/agents reports (TokenBar
/// local patch). `scan_messages_streaming` selects scanner LANES from the
/// client list, but some visible client ids are not lanes of their own: a
/// `cc-mirror/<variant>` id is produced *during Claude-lane parsing* (#659), so
/// requesting it alone would enable no lane (`ClientId::from_str` returns None)
/// and the report would come back empty for a client that Daily/Models still
/// show. Split the request into:
/// - the lanes to scan — each `cc-mirror/*` maps to its producing `claude`
///   lane (the Claude scanner discovers `.cc-mirror/*/config/projects`), every
///   other id maps to itself; and
/// - an optional EXACT client-id set the aggregation keeps. Unlike the scan's
///   built-in `retain_for_requested_clients`, requesting `claude` here does NOT
///   sweep in the distinct `cc-mirror/*` variants: the graph/model/daily
///   payloads fold by `msg.client`, surfacing each variant as its own client
///   id, so the hourly/agents slices must match that exact-id grouping. The
///   `synthetic` special-case is preserved (synthetic is a model/provider match,
///   not a literal client id). `None` = no client filter (every message),
///   preserving the all-clients behavior.
fn split_report_client_filter(options: &ReportOptions) -> (Vec<String>, Option<HashSet<String>>) {
    let Some(requested) = options.clients.as_ref() else {
        return (resolve_report_clients(options), None);
    };
    let mut lanes: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for id in requested {
        let lane = if id.starts_with("cc-mirror/") {
            "claude".to_string()
        } else {
            id.clone()
        };
        if seen.insert(lane.clone()) {
            lanes.push(lane);
        }
    }
    (lanes, Some(requested.iter().cloned().collect()))
}

/// Exact per-message client gate for `split_report_client_filter`'s aggregation
/// half: keep a message iff its exact client id was requested, or it is a
/// synthetic match when `synthetic` was requested. `None` keeps every message.
fn report_message_client_passes(exact: &Option<HashSet<String>>, m: &UnifiedMessage) -> bool {
    match exact {
        None => true,
        Some(req) => {
            req.contains(m.client.as_str())
                || (req.contains("synthetic")
                    && sessions::synthetic::matches_synthetic_filter(
                        &m.client,
                        &m.model_id,
                        &m.provider_id,
                    ))
        }
    }
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

/// Cross-store OpenCode source identity. A migrated SQLite message can carry
/// both an embedded id and a v1 row/file fallback, so callers compare every
/// primary and alternate key at the same creation timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OpenCodeSourceIdentity {
    key: String,
    timestamp: i64,
}

impl OpenCodeSourceIdentity {
    fn all_from_message(message: &UnifiedMessage) -> Vec<Self> {
        let mut identities = Vec::new();
        let mut seen = HashSet::new();
        for key in message
            .dedup_key
            .iter()
            .chain(message.dedup_aliases.iter())
        {
            if !key.is_empty() && seen.insert(key.clone()) {
                identities.push(Self {
                    key: key.clone(),
                    timestamp: message.timestamp,
                });
            }
        }
        identities
    }
}

/// Logical OpenCode payload identity used after parser-local fork collapse.
///
/// Source keys are indexed separately because one migrated request can have
/// both an embedded id and a row/file fallback. Session and workspace remain
/// excluded because true fork copies move between both. Cost is excluded so a
/// provider-reported copy can still replace an estimated one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OpenCodePayloadIdentity {
    timestamp: i64,
    duration_ms: Option<i64>,
    model_id: String,
    provider_id: String,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    agent: Option<String>,
}

impl OpenCodePayloadIdentity {
    fn from_message(message: &UnifiedMessage) -> Self {
        Self {
            timestamp: message.timestamp,
            duration_ms: message.duration_ms,
            model_id: message.model_id.clone(),
            provider_id: message.provider_id.clone(),
            input: message.tokens.input,
            output: message.tokens.output,
            cache_read: message.tokens.cache_read,
            cache_write: message.tokens.cache_write,
            reasoning: message.tokens.reasoning,
            agent: message.agent.clone(),
        }
    }
}

fn opencode_identity_group(
    message: &UnifiedMessage,
) -> (bool, Vec<OpenCodeSourceIdentity>) {
    (
        message.cost_source == CostSource::ProviderReported,
        OpenCodeSourceIdentity::all_from_message(message),
    )
}

fn opencode_authoritative_sources(
    groups: impl IntoIterator<Item = (bool, Vec<OpenCodeSourceIdentity>)>,
) -> HashSet<OpenCodeSourceIdentity> {
    let groups: Vec<_> = groups.into_iter().collect();
    let mut authoritative = HashSet::new();
    for (is_provider_reported, sources) in &groups {
        if *is_provider_reported {
            authoritative.extend(sources.iter().cloned());
        }
    }
    loop {
        let previous_len = authoritative.len();
        for (_, sources) in &groups {
            if sources
                .iter()
                .any(|source| authoritative.contains(source))
            {
                authoritative.extend(sources.iter().cloned());
            }
        }
        if authoritative.len() == previous_len {
            return authoritative;
        }
    }
}

/// Applies one OpenCode source-priority snapshot without requiring sink
/// retraction. SQLite fork copies deduplicate only when a source key (primary or
/// alias) and the full payload identity both match; distinct embedded ids remain
/// separate even when every payload field collides.
struct OpenCodeSelection {
    authoritative_snapshot: HashSet<OpenCodeSourceIdentity>,
    seen_sqlite: HashMap<OpenCodeSourceIdentity, HashSet<OpenCodePayloadIdentity>>,
    emitted_sources: HashSet<OpenCodeSourceIdentity>,
    deferred_sqlite: Vec<Option<UnifiedMessage>>,
    deferred_sources: Vec<Vec<OpenCodeSourceIdentity>>,
    deferred_by_identity:
        HashMap<OpenCodeSourceIdentity, HashMap<OpenCodePayloadIdentity, Vec<usize>>>,
    deferred_by_source: HashMap<OpenCodeSourceIdentity, Vec<usize>>,
}

impl OpenCodeSelection {
    fn new(authoritative_snapshot: HashSet<OpenCodeSourceIdentity>) -> Self {
        Self {
            authoritative_snapshot,
            seen_sqlite: HashMap::new(),
            emitted_sources: HashSet::new(),
            deferred_sqlite: Vec::new(),
            deferred_sources: Vec::new(),
            deferred_by_identity: HashMap::new(),
            deferred_by_source: HashMap::new(),
        }
    }

    fn has_authority(&self, sources: &[OpenCodeSourceIdentity]) -> bool {
        sources
            .iter()
            .any(|source| self.authoritative_snapshot.contains(source))
    }

    fn has_emitted(&self, sources: &[OpenCodeSourceIdentity]) -> bool {
        sources
            .iter()
            .any(|source| self.emitted_sources.contains(source))
    }

    fn connected_deferred_sources(
        &self,
        sources: &[OpenCodeSourceIdentity],
    ) -> Vec<OpenCodeSourceIdentity> {
        let mut connected: HashSet<_> = sources.iter().cloned().collect();
        let mut pending: Vec<_> = sources.to_vec();
        while let Some(source) = pending.pop() {
            let Some(indices) = self.deferred_by_source.get(&source) else {
                continue;
            };
            for &index in indices {
                for alias in &self.deferred_sources[index] {
                    if connected.insert(alias.clone()) {
                        pending.push(alias.clone());
                    }
                }
            }
        }
        connected.into_iter().collect()
    }

    fn mark_emitted(&mut self, sources: &[OpenCodeSourceIdentity]) {
        let connected = self.connected_deferred_sources(sources);
        self.emitted_sources.extend(connected);
    }

    fn mark_sqlite_seen(
        &mut self,
        sources: &[OpenCodeSourceIdentity],
        payload: &OpenCodePayloadIdentity,
    ) -> bool {
        let duplicate = sources.iter().any(|source| {
            self.seen_sqlite
                .get(source)
                .is_some_and(|payloads| payloads.contains(payload))
        });
        for source in sources {
            self.seen_sqlite
                .entry(source.clone())
                .or_default()
                .insert(payload.clone());
        }
        duplicate
    }

    fn index_deferred(
        &mut self,
        index: usize,
        sources: &[OpenCodeSourceIdentity],
        payload: &OpenCodePayloadIdentity,
    ) {
        for source in sources {
            if !self.deferred_sources[index].contains(source) {
                self.deferred_sources[index].push(source.clone());
            }
            let identity_indices = self
                .deferred_by_identity
                .entry(source.clone())
                .or_default()
                .entry(payload.clone())
                .or_default();
            if !identity_indices.contains(&index) {
                identity_indices.push(index);
            }
            let source_indices = self.deferred_by_source.entry(source.clone()).or_default();
            if !source_indices.contains(&index) {
                source_indices.push(index);
            }
        }
    }

    fn defer_sqlite(
        &mut self,
        sources: &[OpenCodeSourceIdentity],
        payload: &OpenCodePayloadIdentity,
        message: UnifiedMessage,
    ) {
        let index = self.deferred_sqlite.len();
        self.deferred_sqlite.push(Some(message));
        self.deferred_sources.push(Vec::new());
        self.index_deferred(index, sources, payload);
    }

    fn first_active(&self, indices: &[usize]) -> Option<usize> {
        indices
            .iter()
            .copied()
            .find(|&index| self.deferred_sqlite[index].is_some())
    }

    fn find_deferred_identity(
        &self,
        sources: &[OpenCodeSourceIdentity],
        payload: &OpenCodePayloadIdentity,
    ) -> Option<usize> {
        sources.iter().find_map(|source| {
            self.deferred_by_identity
                .get(source)
                .and_then(|payloads| payloads.get(payload))
                .and_then(|indices| self.first_active(indices))
        })
    }

    fn find_deferred_source(&self, sources: &[OpenCodeSourceIdentity]) -> Option<usize> {
        sources.iter().find_map(|source| {
            self.deferred_by_source
                .get(source)
                .and_then(|indices| self.first_active(indices))
        })
    }

    fn take_deferred(&mut self, index: usize) -> Vec<OpenCodeSourceIdentity> {
        self.deferred_sqlite[index] = None;
        self.deferred_sources[index].clone()
    }

    fn select_sqlite(&mut self, message: UnifiedMessage) -> Option<UnifiedMessage> {
        let sources = OpenCodeSourceIdentity::all_from_message(&message);
        if sources.is_empty() {
            return Some(message);
        }
        let payload = OpenCodePayloadIdentity::from_message(&message);
        if self.mark_sqlite_seen(&sources, &payload) {
            if let Some(index) = self.find_deferred_identity(&sources, &payload) {
                self.index_deferred(index, &sources, &payload);
                if message.cost_source == CostSource::ProviderReported {
                    let deferred_sources = self.take_deferred(index);
                    self.mark_emitted(&deferred_sources);
                    self.mark_emitted(&sources);
                    return Some(message);
                }
                return None;
            }
            if self.has_emitted(&sources) {
                self.mark_emitted(&sources);
            }
            return None;
        }
        if self.has_emitted(&sources) {
            self.mark_emitted(&sources);
        }
        if self.has_authority(&sources) && message.cost_source != CostSource::ProviderReported {
            self.defer_sqlite(&sources, &payload, message);
            return None;
        }
        self.mark_emitted(&sources);
        Some(message)
    }

    fn select_json(&mut self, message: UnifiedMessage, will_emit: bool) -> Option<UnifiedMessage> {
        let sources = OpenCodeSourceIdentity::all_from_message(&message);
        if sources.is_empty() {
            return will_emit.then_some(message);
        }
        if self.has_authority(&sources) {
            if message.cost_source != CostSource::ProviderReported || !will_emit {
                return None;
            }
            let payload = OpenCodePayloadIdentity::from_message(&message);
            let exact = self.find_deferred_identity(&sources, &payload);
            if exact.is_none() && self.has_emitted(&sources) {
                return None;
            }
            if let Some(index) = exact.or_else(|| self.find_deferred_source(&sources)) {
                let deferred_sources = self.take_deferred(index);
                self.mark_emitted(&deferred_sources);
            }
            self.mark_emitted(&sources);
            return Some(message);
        }
        if self.has_emitted(&sources) {
            return None;
        }
        self.mark_emitted(&sources);
        will_emit.then_some(message)
    }

    fn finish(self) -> impl Iterator<Item = UnifiedMessage> {
        self.deferred_sqlite.into_iter().flatten()
    }
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

    let (total_input, total_output, total_cache_read, total_cache_write) =
        model_report_token_totals(&entries);
    let total_messages: i32 = entries.iter().map(|e| e.message_count).sum();
    // f64's Sum identity is -0.0, so an empty report would serialize as
    // "totalCost": -0.0; adding +0.0 normalizes the sign without changing
    // any non-zero total.
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;

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
    // One alias snapshot for the whole fold so a mid-fold set_model_aliases
    // cannot split messages across two grouping configs.
    let aliases = model_alias::snapshot_grouping_aliases();

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
                .insert(aliases.fold(normalize_syntactic(&msg.model_id)));
            // saturating_add so clamped (i64::MAX) buckets from a corrupt source
            // can't overflow the fold.
            entry.input = entry.input.saturating_add(msg.tokens.input);
            entry.output = entry.output.saturating_add(msg.tokens.output);
            entry.cache_read = entry.cache_read.saturating_add(msg.tokens.cache_read);
            entry.cache_write = entry.cache_write.saturating_add(msg.tokens.cache_write);
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

    // f64's Sum identity is -0.0, so an empty report would serialize as
    // "totalCost": -0.0; adding +0.0 normalizes the sign without changing
    // any non-zero total.
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;

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
    // Folds tokens like the model report's `aggregate_model_usage_entries`
    // (saturating_add — see that fn for why): the per-client parity those two
    // reports must hold depends on identical arithmetic. `message_count.max(0)`
    // matches the model/monthly aggregators (not the sessionizer's `.max(1)`).
    fn add(&mut self, msg: &UnifiedMessage) {
        self.clients.insert(msg.client.clone());
        // saturating_add so clamped (i64::MAX) buckets from a corrupt source
        // can't overflow the fold.
        self.input = self.input.saturating_add(msg.tokens.input);
        self.output = self.output.saturating_add(msg.tokens.output);
        self.cache_read = self.cache_read.saturating_add(msg.tokens.cache_read);
        self.cache_write = self.cache_write.saturating_add(msg.tokens.cache_write);
        self.reasoning = self.reasoning.saturating_add(msg.tokens.reasoning);
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
        // Copilot emits raw OTEL agent ids (e.g. "github.copilot.default",
        // "Plugin:team:slug") via #724; upstream prettifies them in its CLI TUI
        // (which we do not vendor), so apply the copilot-specific normalization
        // here on our streaming agents-report path. Every other client keeps the
        // generic normalization (opencode already normalizes at parse time).
        Some(raw) if !raw.trim().is_empty() && msg.client == "copilot" => {
            sessions::normalize_copilot_agent_name(raw)
        }
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
    // Two-level filter: scan the producing lanes (cc-mirror variants ride the
    // claude lane), then keep only the exact requested client ids at fold time.
    let (clients, exact) = split_report_client_filter(&options);

    let pricing = load_pricing_for_local_parse().await;
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if !report_message_client_passes(&exact, m) { return false; }
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
    // saturating_add so #766's i64::MAX-clamped buckets from a corrupt
    // Antigravity DB can't overflow the sort key (matches the model report's
    // saturating total; for normal >=0 tokens the result is unchanged).
    entries.sort_by(|a, b| {
        let a_total = a
            .input
            .saturating_add(a.output)
            .saturating_add(a.cache_read)
            .saturating_add(a.cache_write)
            .saturating_add(a.reasoning);
        let b_total = b
            .input
            .saturating_add(b.output)
            .saturating_add(b.cache_read)
            .saturating_add(b.cache_write)
            .saturating_add(b.reasoning);
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b_total.cmp(&a_total))
    });

    // f64's Sum identity is -0.0, so an empty report would serialize as
    // "totalCost": -0.0; adding +0.0 normalizes the sign without changing
    // any non-zero total.
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;
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
    // Two-level filter: scan the producing lanes (cc-mirror variants ride the
    // claude lane), then keep only the exact requested client ids at fold time.
    let (clients, exact) = split_report_client_filter(&options);

    let pricing = load_pricing_for_local_parse().await;
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if !report_message_client_passes(&exact, m) { return false; }
        if let Some(ref yp) = year_prefix { if !m.date.starts_with(yp.as_str()) { return false; } }
        if let Some(ref s) = since_s { if m.date.as_str() < s.as_str() { return false; } }
        if let Some(ref u) = until_s { if m.date.as_str() > u.as_str() { return false; } }
        true
    };

    let mut hour_map: HashMap<String, HourAggregator> = HashMap::new();
    // One alias snapshot for the whole fold so a mid-fold set_model_aliases
    // cannot split messages across two grouping configs.
    let aliases = model_alias::snapshot_grouping_aliases();

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
                .insert(aliases.fold(normalize_syntactic(&msg.model_id)));
            // saturating_add so clamped (i64::MAX) buckets from a corrupt source
            // can't overflow the fold.
            entry.input = entry.input.saturating_add(msg.tokens.input);
            entry.output = entry.output.saturating_add(msg.tokens.output);
            entry.cache_read = entry.cache_read.saturating_add(msg.tokens.cache_read);
            entry.cache_write = entry.cache_write.saturating_add(msg.tokens.cache_write);
            entry.reasoning = entry.reasoning.saturating_add(msg.tokens.reasoning);
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

    // f64's Sum identity is -0.0, so an empty report would serialize as
    // "totalCost": -0.0; adding +0.0 normalizes the sign without changing
    // any non-zero total.
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;

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

    // ---- OpenCode SQLite + legacy JSON ----
    // The sink cannot retract an estimated duplicate, so pre-scan source-key
    // alias groups and expand provider authority across each connected group.
    // Legacy JSON is parsed again below; this pass retains identities, not bodies.
    let mut opencode_identity_groups: Vec<_> = scan_result
        .get(ClientId::OpenCode)
        .par_iter()
        .filter_map(|path| sessions::opencode::parse_opencode_file(path))
        .map(|message| opencode_identity_group(&message))
        .collect();
    for db_path in &scan_result.opencode_dbs {
        opencode_identity_groups.extend(
            sessions::opencode::parse_opencode_sqlite(db_path)
                .into_iter()
                .map(|message| opencode_identity_group(&message)),
        );
    }
    let opencode_authoritative = opencode_authoritative_sources(opencode_identity_groups);

    let mut opencode_selection = OpenCodeSelection::new(opencode_authoritative);
    for db_path in &scan_result.opencode_dbs {
        for mut message in sessions::opencode::parse_opencode_sqlite(db_path) {
            apply_pricing_if_available(&mut message, pricing);
            if let Some(message) = opencode_selection.select_sqlite(message) {
                if passes_client(&message) && filter(&message) { sink(&message); }
            }
        }
    }
    for path in scan_result.get(ClientId::OpenCode) {
        if let Some(mut message) = sessions::opencode::parse_opencode_file(path) {
            apply_pricing_if_available(&mut message, pricing);
            let will_emit = passes_client(&message) && filter(&message);
            if let Some(message) = opencode_selection.select_json(message, will_emit) {
                sink(&message);
            }
        }
    }
    for message in opencode_selection.finish() {
        if passes_client(&message) && filter(&message) { sink(&message); }
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
        ($client_id:expr, $parse_fn:expr, $fingerprint_fn:expr) => {
            simple_lane!($client_id, $parse_fn, $fingerprint_fn, false)
        };
        // 4-arg (cost-guarded reprice): clients that embed an authoritative
        // per-message cost (e.g. MiMo Code — #742 Part 2) pass `true`, so a
        // recomputed tokens*rate never clobbers the embedded cost; only a
        // missing cost (`<= 0.0`) is repriced. The cache still stores raw
        // (unpriced) messages, so the guard is applied on emit here — exactly
        // like the default unconditional path (which passes `false`).
        ($client_id:expr, $parse_fn:expr, $fingerprint_fn:expr, $guard_cost:expr) => {{
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
                        reprice_lane_message(&mut m, pricing, $guard_cost);
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
                    reprice_lane_message(&mut m, pricing, $guard_cost);
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
    simple_lane!(
        ClientId::Droid,
        sessions::droid::parse_droid_file,
        message_cache::SourceFingerprint::from_droid_path
    );
    simple_lane!(ClientId::OpenClaw,  sessions::openclaw::parse_openclaw_transcript);
    simple_lane!(ClientId::Pi,        sessions::pi::parse_pi_file);
    simple_lane!(
        ClientId::Kimi,
        parse_kimi_source,
        message_cache::SourceFingerprint::from_kimi_path
    );
    simple_lane!(ClientId::Junie, sessions::junie::parse_junie_file);
    simple_lane!(
        ClientId::OpenCodeReview,
        sessions::opencodereview::parse_opencodereview_file
    );
    simple_lane!(ClientId::Qwen,      sessions::qwen::parse_qwen_file);
    // roo family: fingerprint via from_roo_path so a history-only rewrite of the
    // sibling api_conversation_history.json (which parse_roo_kilo_file reads for
    // model/agent) invalidates the cached lane (#741).
    simple_lane!(
        ClientId::RooCode,
        sessions::roocode::parse_roocode_file,
        message_cache::SourceFingerprint::from_roo_path
    );
    simple_lane!(
        ClientId::KiloCode,
        sessions::kilocode::parse_kilocode_file,
        message_cache::SourceFingerprint::from_roo_path
    );
    simple_lane!(
        ClientId::Cline,
        sessions::cline::parse_cline_file,
        message_cache::SourceFingerprint::from_roo_path
    );
    simple_lane!(
        ClientId::Jcode,
        sessions::jcode::parse_jcode_file,
        message_cache::SourceFingerprint::from_jcode_path
    );
    // ---- Grok legacy updates + unified log (batch precedence) ----
    // Cache each raw source independently, but do not emit cache hits early: the
    // global unified log can suppress legacy rows from any session file.
    {
        let grok_paths = scan_result.get(ClientId::Grok);
        let mut raw_by_path: Vec<Option<Vec<UnifiedMessage>>> =
            (0..grok_paths.len()).map(|_| None).collect();
        let mut miss_paths: Vec<(usize, &PathBuf)> = Vec::new();

        for (index, path) in grok_paths.iter().enumerate() {
            let fingerprint = message_cache::SourceFingerprint::from_grok_path(path);
            let cache_hit = fingerprint.as_ref().and_then(|fingerprint| {
                source_cache.get(path).filter(|cached| {
                    cached.fingerprint == *fingerprint && !cached.messages.is_empty()
                })
            });
            if let Some(cached) = cache_hit {
                raw_by_path[index] = Some(cached.messages.clone());
            } else {
                miss_paths.push((index, path));
            }
        }

        let parsed_misses: Vec<(usize, &PathBuf, Vec<UnifiedMessage>)> = miss_paths
            .par_iter()
            .map(|(index, path)| (*index, *path, sessions::grok::parse_grok_file(path)))
            .collect();
        for (index, path, messages) in parsed_misses {
            if !messages.is_empty() {
                if let Some(fingerprint) =
                    message_cache::SourceFingerprint::from_grok_path(path)
                {
                    source_cache.insert(message_cache::CachedSourceEntry::new(
                        path,
                        fingerprint,
                        messages.clone(),
                        Vec::new(),
                        None,
                    ));
                }
            }
            raw_by_path[index] = Some(messages);
        }

        let raw_messages = raw_by_path
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<_>>();
        let mut seen_keys: HashSet<String> = HashSet::new();
        for mut message in sessions::grok::prefer_unified_log_messages(raw_messages) {
            message.refresh_derived_fields();
            reprice_lane_message(&mut message, pricing, false);
            if !passes_client(&message) {
                continue;
            }
            let keep = message
                .dedup_key
                .as_ref()
                .is_none_or(|key| key.is_empty() || dedup_gate_passes(key, &mut seen_keys));
            if keep && filter(&message) {
                sink(&message);
            }
        }
    }
    // micode is WAL-mode SQLite; fingerprint via from_sqlite_path so a `-wal`
    // write invalidates the cache. MiMo Code records an authoritative per-message
    // cost (usage.cost), so this lane is cost-guarded (`true`): apply_pricing
    // only runs when the embedded cost is absent (`<= 0.0`), never overwriting a
    // real embedded cost with a recomputed tokens*rate. Today MiMo models are
    // absent from the pricing dataset so unconditional repricing would be a
    // no-op, but the guard future-proofs against a priced provider routed
    // through MiMo Code / the model being added to the dataset. (#742 Part 2 —
    // upstream applies this guard in its materialized lane, which is dead code
    // for us; the streaming lane is where the app actually reprices.)
    simple_lane!(
        ClientId::MiMoCode,
        sessions::micode::parse_micode_sqlite,
        message_cache::SourceFingerprint::from_sqlite_path,
        true
    );
    simple_lane!(ClientId::Mux,       sessions::mux::parse_mux_file);

    // ---- Kiro globalStorage files (raw cache + batch suppression) ----
    // Snapshots and successful executions can describe the same conversation.
    // Collect every raw source first so suppression runs before pricing, client
    // gating, date/report filters, and the sink. Suppressed aggregates are never
    // written to the per-source cache, allowing a later execution removal to
    // restore the cached snapshot.
    {
        let kiro_paths = scan_result.get(ClientId::Kiro);
        let mut raw_by_path: Vec<Option<Vec<UnifiedMessage>>> =
            (0..kiro_paths.len()).map(|_| None).collect();
        let mut miss_paths: Vec<(usize, &PathBuf)> = Vec::new();

        for (index, path) in kiro_paths.iter().enumerate() {
            let fingerprint = message_cache::SourceFingerprint::from_kiro_path(path);
            let cache_hit = fingerprint.as_ref().and_then(|fingerprint| {
                source_cache.get(path).filter(|cached| {
                    cached.fingerprint == *fingerprint && !cached.messages.is_empty()
                })
            });
            if let Some(cached) = cache_hit {
                raw_by_path[index] = Some(cached.messages.clone());
            } else {
                miss_paths.push((index, path));
            }
        }

        let parsed_misses: Vec<(usize, &PathBuf, Vec<UnifiedMessage>)> = miss_paths
            .par_iter()
            .map(|(index, path)| (*index, *path, sessions::kiro::parse_kiro_file(path)))
            .collect();
        for (index, path, messages) in parsed_misses {
            if !messages.is_empty() {
                if let Some(fingerprint) = message_cache::SourceFingerprint::from_kiro_path(path) {
                    source_cache.insert(message_cache::CachedSourceEntry::new(
                        path,
                        fingerprint,
                        messages.clone(),
                        Vec::new(),
                        None,
                    ));
                }
            } else {
                // A changed source that now parses empty must not keep replaying
                // a stale non-empty entry through a later same-fingerprint hit.
                source_cache.remove(path);
            }
            raw_by_path[index] = Some(messages);
        }

        let raw_sources = kiro_paths
            .iter()
            .cloned()
            .zip(raw_by_path.into_iter().map(Option::unwrap_or_default))
            .collect();
        let messages = sessions::kiro::merge_kiro_source_messages(raw_sources);
        for mut message in messages {
            message.refresh_derived_fields();
            reprice_lane_message(&mut message, pricing, false);
            if !passes_client(&message) {
                continue;
            }
            if filter(&message) {
                sink(&message);
            }
        }
    }

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

    // ---- gjc (gajae-code) JSONL with cost provenance ----
    // Route every message through the shared pricing policy. Provider-reported
    // totals return unchanged; unknown totals receive an estimate when possible.
    {
        let mut gjc_seen: HashSet<String> = HashSet::new();
        for path in scan_result.get(ClientId::Gjc) {
            for mut m in sessions::gjc::parse_gjc_file(path) {
                apply_pricing_if_available(&mut m, pricing);
                if !passes_client(&m) { continue; }
                let keep = m.dedup_key.as_ref().is_none_or(|k| k.is_empty() || dedup_gate_passes(k, &mut gjc_seen));
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
    if message.has_authoritative_cost() {
        return;
    }

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
        message.mark_estimated_cost();
    }
}

/// Reprice a streaming-lane message, respecting an authoritative embedded cost.
///
/// Clients that record a real per-message cost of their own (e.g. MiMo Code,
/// which stores `usage.cost`) pass `guard_authoritative_cost = true`: the
/// message is repriced only when its embedded cost is absent (`<= 0.0`), so a
/// recomputed `tokens * rate` never clobbers the authoritative value if the
/// model later resolves to a price. Every other lane passes `false` and
/// reprices unconditionally (unchanged behaviour). `#742` Part 2.
fn reprice_lane_message(
    message: &mut UnifiedMessage,
    pricing: Option<&pricing::PricingService>,
    guard_authoritative_cost: bool,
) {
    if !guard_authoritative_cost || message.cost <= 0.0 {
        apply_pricing_if_available(message, pricing);
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
/// Max mtime (unix ms) across every file the local scan would read. This
/// remains the timestamp-shaped probe used by pruning and diagnostics;
/// in-process report caches that must observe source deletion use
/// `local_source_change_token` instead. Database-backed sources contribute both
/// the db file and its `-wal` sidecar. Stat failures contribute nothing, so a
/// vanished file alone does not change this max-mtime value.
pub fn latest_source_mtime_ms(options: &LocalParseOptions) -> Result<u64, String> {
    let scan_result = scan_local_sources(options)?;
    Ok(latest_source_mtime_ms_from_scan(&scan_result))
}

fn scan_local_sources(options: &LocalParseOptions) -> Result<scanner::ScanResult, String> {
    let (home_dir, clients) = resolve_local_parse_request(options)?;
    Ok(scanner::scan_all_clients_with_scanner_settings(
        &home_dir,
        &clients,
        options.use_env_roots,
        &options.scanner_settings,
    ))
}

fn latest_source_mtime_ms_from_scan(scan_result: &scanner::ScanResult) -> u64 {
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
    // Hermes/Zed dbs may also be auto-discovered or supplied through extra
    // scan roots (the `files` lanes) — use the plural helpers so every db gets
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
    // jcode snapshots (`session_*.json`) carry a sibling `.journal.jsonl`
    // append-log; jcode writes new turns there between snapshot rewrites,
    // leaving the snapshot's mtime untouched. The snapshot itself is already
    // covered by the `scan_result.files` loop above, but the journal is
    // deliberately excluded from the scan (the glob is `session_*.json`), so
    // probe it here — otherwise a journal-only append leaves the change token
    // unchanged and the live tail never re-parses the new usage.
    for snapshot in scan_result.get(ClientId::Jcode) {
        let journal = message_cache::jcode_journal_path(snapshot);
        latest = latest.max(file_mtime_ms(&journal).unwrap_or(0));
    }
    // Legacy Grok sessions reconcile sibling metadata that can change without
    // touching updates.jsonl. The self-contained unified log is already covered
    // by the primary-file scan above.
    for source in scan_result.get(ClientId::Grok) {
        if source.file_name().and_then(|name| name.to_str()) != Some("updates.jsonl") {
            continue;
        }
        if let Some(parent) = source.parent() {
            for sibling in message_cache::GROK_METADATA_SIBLINGS {
                latest = latest.max(file_mtime_ms(&parent.join(sibling)).unwrap_or(0));
            }
        }
    }
    // Roo-family task parsers read model and agent identity from the sibling
    // api_conversation_history.json. Probe it alongside ui_messages.json so a
    // history-only rewrite reaches the cache fingerprint and active lane.
    for client in [ClientId::RooCode, ClientId::KiloCode, ClientId::Cline] {
        for ui_messages in scan_result.get(client) {
            latest = latest.max(roo_source_mtime_ms(ui_messages).unwrap_or(0));
        }
    }
    // These file-backed parsers consult secondary sources whose writes do not
    // update the scanned primary: Droid's fallback transcript, legacy Kimi's
    // shared config, and Kiro's CLI/IDE message sidecars. Probe each dependency
    // so a sibling-only change reaches the specialized cache fingerprint.
    for settings in scan_result.get(ClientId::Droid) {
        latest = latest.max(droid_source_mtime_ms(settings).unwrap_or(0));
    }
    for wire in scan_result.get(ClientId::Kimi) {
        latest = latest.max(kimi_source_mtime_ms(wire).unwrap_or(0));
    }
    for session in scan_result.get(ClientId::Kiro) {
        latest = latest.max(kiro_source_mtime_ms(session).unwrap_or(0));
    }
    latest
}

/// Opaque change token for in-process report caches. Hash the current source
/// topology and each parser dependency's size and nanosecond mtime so creating,
/// deleting, or rewriting a non-max source still invalidates cached graphs and
/// the live tail.
pub fn local_source_change_token(options: &LocalParseOptions) -> Result<u64, String> {
    use std::hash::{Hash, Hasher};

    let scan_result = scan_local_sources(options)?;
    let mut paths: Vec<PathBuf> = scan_result.files.iter().flatten().cloned().collect();

    let mut dbs = scan_result.opencode_dbs.clone();
    let single_dbs = [
        &scan_result.synthetic_db,
        &scan_result.kilo_db,
        &scan_result.goose_db,
        &scan_result.kiro_db,
    ];
    dbs.extend(single_dbs.into_iter().flatten().cloned());
    dbs.extend(scan_result.hermes_db_paths());
    dbs.extend(scan_result.zed_db_paths());
    dbs.extend(
        scan_result
            .crush_dbs
            .iter()
            .map(|source| source.db_path.clone()),
    );
    dbs.extend(scan_result.get(ClientId::AntigravityCli).iter().cloned());
    dbs.extend(scan_result.get(ClientId::MiMoCode).iter().cloned());
    for db in dbs {
        paths.push(db.clone());
        let mut wal = db.into_os_string();
        wal.push("-wal");
        paths.push(PathBuf::from(wal));
    }

    for snapshot in scan_result.get(ClientId::Jcode) {
        paths.push(message_cache::jcode_journal_path(snapshot));
    }
    for source in scan_result.get(ClientId::Grok) {
        if source.file_name().and_then(|name| name.to_str()) == Some("updates.jsonl") {
            if let Some(parent) = source.parent() {
                paths.extend(
                    message_cache::GROK_METADATA_SIBLINGS
                        .into_iter()
                        .map(|name| parent.join(name)),
                );
            }
        }
    }
    for client in [ClientId::RooCode, ClientId::KiloCode, ClientId::Cline] {
        paths.extend(
            scan_result
                .get(client)
                .iter()
                .map(|path| sessions::roocode::history_path_for_ui_messages(path)),
        );
    }
    paths.extend(
        scan_result
            .get(ClientId::Droid)
            .iter()
            .filter_map(|path| sessions::droid::droid_jsonl_path(path)),
    );
    paths.extend(
        scan_result
            .get(ClientId::Kimi)
            .iter()
            .filter_map(|path| sessions::kimi::kimi_config_path(path)),
    );
    paths.extend(
        scan_result
            .get(ClientId::Kiro)
            .iter()
            .filter_map(|path| sessions::kiro::kiro_related_messages_path(path)),
    );

    paths.sort();
    paths.dedup();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in paths {
        path.hash(&mut hasher);
        let state = std::fs::metadata(&path).ok().map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| (duration.as_secs(), duration.subsec_nanos()));
            (metadata.len(), modified)
        });
        state.hash(&mut hasher);
    }
    Ok(hasher.finish())
}

/// File mtime as unix ms; `None` on any stat failure.
fn file_mtime_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as u64)
}

/// Newest mtime for a primary source and every related path its parser reads.
/// Missing optional paths are ignored. Any other metadata failure returns
/// `None`, allowing pruning callers to fail open rather than silently dropping
/// a source whose freshness cannot be established.
fn source_with_related_mtime_ms(
    source: &Path,
    related_paths: impl IntoIterator<Item = PathBuf>,
) -> Option<u64> {
    let mut latest = file_mtime_ms(source)?;
    for related in related_paths {
        let metadata = match std::fs::metadata(&related) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let modified = metadata.modified().ok()?;
        let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
        latest = latest.max(duration.as_millis() as u64);
    }
    Some(latest)
}

fn roo_source_mtime_ms(ui_messages_path: &Path) -> Option<u64> {
    source_with_related_mtime_ms(
        ui_messages_path,
        std::iter::once(sessions::roocode::history_path_for_ui_messages(
            ui_messages_path,
        )),
    )
}

fn droid_source_mtime_ms(settings_path: &Path) -> Option<u64> {
    source_with_related_mtime_ms(
        settings_path,
        sessions::droid::droid_jsonl_path(settings_path),
    )
}

fn kimi_source_mtime_ms(wire_path: &Path) -> Option<u64> {
    source_with_related_mtime_ms(wire_path, sessions::kimi::kimi_config_path(wire_path))
}

fn kiro_source_mtime_ms(session_path: &Path) -> Option<u64> {
    source_with_related_mtime_ms(
        session_path,
        sessions::kiro::kiro_related_messages_path(session_path),
    )
}

/// Newest mtime for a Grok source and every file that affects its parse. The
/// unified log is self-contained; legacy updates include metadata siblings kept
/// in lockstep with `SourceFingerprint::from_grok_path`. `None` keeps the source
/// during pruning because over-parsing is safer than silently dropping usage.
fn grok_source_mtime_ms(source_path: &Path) -> Option<u64> {
    let mut latest = file_mtime_ms(source_path)?;
    if source_path.file_name().and_then(|name| name.to_str()) == Some("unified.jsonl") {
        return Some(latest);
    }

    let Some(parent) = source_path.parent() else {
        return Some(latest);
    };
    for name in message_cache::GROK_METADATA_SIBLINGS {
        let sibling = parent.join(name);
        if sibling.exists() {
            latest = latest.max(file_mtime_ms(&sibling)?);
        }
    }
    Some(latest)
}

/// Drop file-backed session logs older than `threshold_ms` (unix ms, mtime)
/// from a scan. Sources whose freshness is not captured by their scanned
/// file's own mtime are left untouched, because a sibling can change without
/// touching it: the Hermes/Zed/Antigravity-CLI/micode lanes hold SQLite dbs
/// (WAL writes may not bump the main `.db` mtime), while the jcode lane holds a
/// `session_*.json` snapshot whose sibling `.journal.jsonl` is appended between
/// snapshot rewrites. Those lanes remain exempt. Roo-family, Droid, legacy Kimi,
/// Kiro file sources, and Grok sources can still be bounded by folding every
/// parser dependency into their newest mtime.
/// Any stat failure keeps the file — over-parsing is safe, silently skipping is
/// not.
fn prune_scan_result_by_mtime(scan_result: &mut scanner::ScanResult, threshold_ms: u64) {
    // Lanes whose scanned file's mtime does not reflect a sibling write
    // (SQLite `-wal` or jcode's `.journal.jsonl`); kept in lockstep with the
    // sibling probes in `latest_source_mtime_ms`.
    let db_lanes = [
        ClientId::Hermes as usize,
        ClientId::Zed as usize,
        ClientId::AntigravityCli as usize,
        ClientId::MiMoCode as usize,
        ClientId::Jcode as usize,
    ];
    let roo_lanes = [
        ClientId::RooCode as usize,
        ClientId::KiloCode as usize,
        ClientId::Cline as usize,
    ];
    for (lane, files) in scan_result.files.iter_mut().enumerate() {
        if db_lanes.contains(&lane) {
            continue;
        }
        if roo_lanes.contains(&lane) {
            files
                .retain(|path| roo_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms));
            continue;
        }
        if lane == ClientId::Droid as usize {
            files.retain(|path| {
                droid_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms)
            });
            continue;
        }
        if lane == ClientId::Kimi as usize {
            files.retain(|path| {
                kimi_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms)
            });
            continue;
        }
        if lane == ClientId::Kiro as usize {
            // globalStorage precedence crosses files. If any IDE source changed,
            // retain the complete IDE cohort so an older execution can still
            // suppress a newer snapshot; CLI sources remain independently prunable.
            let keep_global_storage_batch = files
                .iter()
                .filter(|path| sessions::kiro::is_kiro_global_storage_source(path))
                .any(|path| kiro_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms));
            files.retain(|path| {
                if sessions::kiro::is_kiro_global_storage_source(path) {
                    keep_global_storage_batch
                } else {
                    kiro_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms)
                }
            });
            continue;
        }
        if lane == ClientId::Grok as usize {
            let is_unified = |path: &Path| {
                path.file_name().and_then(|name| name.to_str()) == Some("unified.jsonl")
            };
            let is_fresh =
                |path: &Path| grok_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms);
            let unified_fresh = files.iter().any(|path| is_unified(path) && is_fresh(path));
            if unified_fresh {
                // A fresh global authority file can cover any legacy session and
                // also needs those rows for workspace attribution.
                continue;
            }

            let legacy_fresh = files.iter().any(|path| !is_unified(path) && is_fresh(path));
            files.retain(|path| {
                if is_unified(path) {
                    // An older authority file must remain available while a
                    // legacy session is fresh, or live-tail pruning can reopen
                    // rows that full reports correctly suppress.
                    legacy_fresh
                } else {
                    is_fresh(path)
                }
            });
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
        let sqlite_messages: Vec<UnifiedMessage> = scan_result
            .opencode_dbs
            .iter()
            .flat_map(|db_path| sessions::opencode::parse_opencode_sqlite(db_path))
            .collect();
        let json_messages: Vec<UnifiedMessage> = scan_result
            .get(ClientId::OpenCode)
            .par_iter()
            .filter_map(|path| sessions::opencode::parse_opencode_file(path))
            .collect();
        let authoritative = opencode_authoritative_sources(
            sqlite_messages
                .iter()
                .chain(json_messages.iter())
                .map(opencode_identity_group),
        );
        let mut selection = OpenCodeSelection::new(authoritative);
        let mut selected: Vec<UnifiedMessage> = sqlite_messages
            .into_iter()
            .filter_map(|message| selection.select_sqlite(message))
            .collect();
        selected.extend(
            json_messages
                .into_iter()
                .filter_map(|message| selection.select_json(message, true)),
        );
        selected.extend(selection.finish());

        let count = selected.len() as i32;
        messages.extend(selected.iter().map(unified_to_parsed));
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

    let kimi_outcomes: Vec<(bool, Vec<UnifiedMessage>)> = scan_result
        .get(ClientId::Kimi)
        .par_iter()
        .map(|path| {
            (
                sessions::kimi::is_kimi_code_path(path),
                parse_kimi_source(path),
            )
        })
        .collect();
    let mut kimi_code_seen: HashSet<String> = HashSet::new();
    let kimi_msgs: Vec<ParsedMessage> = kimi_outcomes
        .into_iter()
        .flat_map(|(is_kimi_code, messages)| {
            messages
                .into_iter()
                .filter(|message| {
                    !is_kimi_code || should_keep_deduped_message(&mut kimi_code_seen, message)
                })
                .map(|message| unified_to_parsed(&message))
                .collect::<Vec<_>>()
        })
        .collect();
    let kimi_count = summed_parsed_message_count(&kimi_msgs);
    counts.set(ClientId::Kimi, kimi_count);
    messages.extend(kimi_msgs);

    let junie_msgs_raw: Vec<UnifiedMessage> = scan_result
        .get(ClientId::Junie)
        .par_iter()
        .flat_map(|path| sessions::junie::parse_junie_file(path))
        .collect();
    let mut junie_seen: HashSet<String> = HashSet::new();
    let junie_msgs: Vec<ParsedMessage> = junie_msgs_raw
        .into_iter()
        .filter(|message| should_keep_deduped_message(&mut junie_seen, message))
        .map(|message| unified_to_parsed(&message))
        .collect();
    let junie_count = summed_parsed_message_count(&junie_msgs);
    counts.set(ClientId::Junie, junie_count);
    messages.extend(junie_msgs);

    let opencodereview_msgs_raw: Vec<UnifiedMessage> = scan_result
        .get(ClientId::OpenCodeReview)
        .par_iter()
        .flat_map(|path| sessions::opencodereview::parse_opencodereview_file(path))
        .collect();
    let mut opencodereview_seen: HashSet<String> = HashSet::new();
    let opencodereview_msgs: Vec<ParsedMessage> = opencodereview_msgs_raw
        .into_iter()
        .filter(|message| {
            should_keep_deduped_message(&mut opencodereview_seen, message)
        })
        .map(|message| unified_to_parsed(&message))
        .collect();
    let opencodereview_count = summed_parsed_message_count(&opencodereview_msgs);
    counts.set(ClientId::OpenCodeReview, opencodereview_count);
    messages.extend(opencodereview_msgs);

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

    // Count path does not reprice (it produces message counts, not costs), so
    // the A1 cost guard is unnecessary here. (Upstream counts gjc rows with
    // `.len()`; `summed_parsed_message_count` is identical because gjc emits
    // message_count = 1, and keeps gjc consistent with the other new clients'
    // count lanes.)
    let gjc_msgs_raw: Vec<UnifiedMessage> = scan_result
        .get(ClientId::Gjc)
        .par_iter()
        .flat_map(|path| sessions::gjc::parse_gjc_file(path))
        .collect();
    let mut gjc_seen: HashSet<String> = HashSet::new();
    let gjc_msgs: Vec<ParsedMessage> = gjc_msgs_raw
        .into_iter()
        .filter(|message| should_keep_deduped_message(&mut gjc_seen, message))
        .map(|m| unified_to_parsed(&m))
        .collect();
    let gjc_count = summed_parsed_message_count(&gjc_msgs);
    counts.set(ClientId::Gjc, gjc_count);
    messages.extend(gjc_msgs);

    let grok_messages: Vec<UnifiedMessage> = scan_result
        .get(ClientId::Grok)
        .par_iter()
        .flat_map(|path| sessions::grok::parse_grok_file(path))
        .collect();
    let grok_msgs: Vec<ParsedMessage> =
        sessions::grok::prefer_unified_log_messages(grok_messages)
            .into_iter()
            .map(|message| unified_to_parsed(&message))
            .collect();
    let grok_count = summed_parsed_message_count(&grok_msgs);
    counts.set(ClientId::Grok, grok_count);
    messages.extend(grok_msgs);

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

    let kiro_sources: Vec<(PathBuf, Vec<UnifiedMessage>)> = scan_result
        .get(ClientId::Kiro)
        .par_iter()
        .map(|path| (path.clone(), sessions::kiro::parse_kiro_file(path)))
        .collect();
    let kiro_msgs: Vec<ParsedMessage> = sessions::kiro::merge_kiro_source_messages(kiro_sources)
        .iter()
        .map(unified_to_parsed)
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
        cost_source: CostSource::Unknown,
        duration_ms: msg.duration_ms,
        message_count: msg.message_count,
        agent: msg.agent.clone(),
        dedup_key: None,
        dedup_aliases: Vec::new(),
        is_turn_start: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        agent_bucket_key, aggregate_model_usage_entries, apply_pricing_if_available,
        canonical_model_id, clear_model_aliases, dedupe_latest_trae_messages,
        fold_messages_streaming, get_agents_report, get_hourly_report, get_model_report,
        get_monthly_report, latest_source_mtime_ms, local_source_change_token, message_cache,
        model_alias_generation, normalize_model_for_grouping, normalize_syntactic,
        opencode_authoritative_sources, opencode_identity_group,
        parse_all_messages_with_pricing_with_env_strategy, parse_local_clients,
        parse_local_unified_messages, parsed_to_unified, pricing, prune_scan_result_by_mtime,
        register_usage_data_invalidation_hook, reprice_lane_message, retain_for_requested_clients,
        scan_messages_streaming, scanner, select_local_parse_pricing, sessions, set_model_aliases,
        snapshot_grouping_aliases, unified_to_parsed, AgentAccumulator, ClientId, CostSource,
        GroupBy, LocalParseOptions, ModelAliasMap, OpenCodeSelection, OpenCodeSourceIdentity,
        ReportOptions, TokenBreakdown, UnifiedMessage, UNKNOWN_WORKSPACE_LABEL,
    };
    use bincode::Options;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            Self(
                keys.iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            )
        }

        fn set(vars: &[(&'static str, &std::ffi::OsStr)]) -> Self {
            let keys: Vec<_> = vars.iter().map(|(key, _)| *key).collect();
            let guard = Self::capture(&keys);
            unsafe {
                for (key, value) in vars {
                    std::env::set_var(key, value);
                }
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (key, previous) in self.0.drain(..) {
                    match previous {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn scanner_fixture_path(home: &Path, relative: &str) -> PathBuf {
        #[cfg(windows)]
        {
            // The production scanner builds these roots with format!("{home}/{relative}").
            // Match that lexical path on Windows so cache assertions use the same key.
            PathBuf::from(format!("{}/{}", home.display(), relative))
        }
        #[cfg(not(windows))]
        {
            home.join(relative)
        }
    }

    fn opencode_test_env(cache_home: &Path, source_home: &Path) -> EnvGuard {
        let xdg_data_home = scanner_fixture_path(source_home, ".local/share");
        EnvGuard::set(&[
            ("HOME", cache_home.as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.as_os_str()),
            ("XDG_DATA_HOME", xdg_data_home.as_os_str()),
        ])
    }

    fn parse_all_messages_with_pricing(
        home_dir: &str,
        clients: &[String],
        pricing: Option<&pricing::PricingService>,
    ) -> Vec<UnifiedMessage> {
        parse_all_messages_with_pricing_with_env_strategy(
            home_dir,
            clients,
            pricing,
            false,
            &scanner::ScannerSettings::default(),
        )
    }

    #[test]
    #[serial_test::serial]
    fn test_env_guard_restores_some_and_none_after_panic() {
        const KEYS: [&str; 3] = ["HOME", "TOKSCALE_PRICING_CACHE_ONLY", "TOKSCALE_CONFIG_DIR"];
        let _original = EnvGuard::capture(&KEYS);

        unsafe {
            std::env::set_var("HOME", "/tmp/tokscale-env-guard-home-before");
            std::env::remove_var("TOKSCALE_PRICING_CACHE_ONLY");
            std::env::set_var(
                "TOKSCALE_CONFIG_DIR",
                "/tmp/tokscale-env-guard-config-before",
            );
        }
        let first = std::panic::catch_unwind(|| {
            let _guard = EnvGuard::set(&[
                (
                    "HOME",
                    std::ffi::OsStr::new("/tmp/tokscale-env-guard-home-during"),
                ),
                ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
                (
                    "TOKSCALE_CONFIG_DIR",
                    std::ffi::OsStr::new("/tmp/tokscale-env-guard-config-during"),
                ),
            ]);
            panic!("exercise EnvGuard unwinding");
        });
        assert!(first.is_err());
        assert_eq!(
            std::env::var_os("HOME"),
            Some(std::ffi::OsString::from(
                "/tmp/tokscale-env-guard-home-before"
            ))
        );
        assert_eq!(std::env::var_os("TOKSCALE_PRICING_CACHE_ONLY"), None);
        assert_eq!(
            std::env::var_os("TOKSCALE_CONFIG_DIR"),
            Some(std::ffi::OsString::from(
                "/tmp/tokscale-env-guard-config-before"
            ))
        );

        unsafe {
            std::env::remove_var("HOME");
            std::env::set_var("TOKSCALE_PRICING_CACHE_ONLY", "before");
            std::env::remove_var("TOKSCALE_CONFIG_DIR");
        }
        let second = std::panic::catch_unwind(|| {
            let _guard = EnvGuard::set(&[
                (
                    "HOME",
                    std::ffi::OsStr::new("/tmp/tokscale-env-guard-home-during"),
                ),
                ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
                (
                    "TOKSCALE_CONFIG_DIR",
                    std::ffi::OsStr::new("/tmp/tokscale-env-guard-config-during"),
                ),
            ]);
            panic!("exercise inverse EnvGuard unwinding");
        });
        assert!(second.is_err());
        assert_eq!(std::env::var_os("HOME"), None);
        assert_eq!(
            std::env::var_os("TOKSCALE_PRICING_CACHE_ONLY"),
            Some(std::ffi::OsString::from("before"))
        );
        assert_eq!(std::env::var_os("TOKSCALE_CONFIG_DIR"), None);
    }

    #[test]
    #[serial_test::serial]
    fn test_empty_reports_normalize_total_cost_to_positive_zero() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);

        let options = ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["opencode".to_string()]),
            ..Default::default()
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let totals = [
            runtime
                .block_on(get_model_report(options.clone()))
                .unwrap()
                .total_cost,
            runtime
                .block_on(get_monthly_report(options.clone()))
                .unwrap()
                .total_cost,
            runtime
                .block_on(get_hourly_report(options.clone()))
                .unwrap()
                .total_cost,
            runtime
                .block_on(get_agents_report(options))
                .unwrap()
                .total_cost,
        ];

        for total in totals {
            assert_eq!(total.to_bits(), 0.0f64.to_bits());
        }
    }

    fn make_opencode_selection_message(key: &str, cost: f64, source: CostSource) -> UnifiedMessage {
        let mut message = UnifiedMessage::new_with_dedup(
            "opencode", "gpt-4o", "openai", "oc-session", 1_733_011_200_000,
            TokenBreakdown { input: 10, output: 5, cache_read: 0, cache_write: 0, reasoning: 0 },
            cost, Some(key.to_string()),
        );
        match source {
            CostSource::ProviderReported => message.mark_provider_reported_cost(),
            CostSource::Estimated => message.mark_estimated_cost(),
            CostSource::Unknown => {}
        }
        message
    }

    fn opencode_authority_set(key: &str) -> HashSet<OpenCodeSourceIdentity> {
        let message = make_opencode_selection_message(key, 0.0, CostSource::ProviderReported);
        OpenCodeSourceIdentity::all_from_message(&message)
            .into_iter()
            .collect()
    }

    #[test]
    fn test_opencode_streaming_selection_flushes_snapshot_fallback_on_json_drift() {
        // A missing file and an invalid file both produce no second-pass message;
        // a downgraded file produces an estimated message. All must flush SQLite.
        for second_pass in [None, None, Some(CostSource::Estimated)] {
            let key = "snapshot-authoritative";
            let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
            assert!(selection.select_sqlite(make_opencode_selection_message(
                key, 0.25, CostSource::Estimated,
            )).is_none());
            if let Some(source) = second_pass {
                assert!(selection.select_json(make_opencode_selection_message(
                    key, 0.0, source,
                ), true).is_none());
            }
            let selected: Vec<_> = selection.finish().collect();
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].cost, 0.25);
            assert_eq!(selected[0].cost_source, CostSource::Estimated);
        }
    }

    #[test]
    fn test_opencode_streaming_selection_replaces_deferred_sqlite_estimate() {
        let key = "sqlite-authoritative-replacement";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key, 0.25, CostSource::Estimated,
            ))
            .is_none());

        let selected = selection
            .select_sqlite(make_opencode_selection_message(
                key, 0.50, CostSource::ProviderReported,
            ))
            .expect("a later authoritative SQLite message must replace the fallback");
        assert_eq!(selected.cost, 0.50);
        assert_eq!(selected.cost_source, CostSource::ProviderReported);
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_first_sqlite_estimate() {
        let key = "sqlite-estimated-first-wins";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key, 0.25, CostSource::Estimated,
            ))
            .is_none());
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key, 0.50, CostSource::Estimated,
            ))
            .is_none());

        let selected: Vec<_> = selection.finish().collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].cost, 0.25);
        assert_eq!(selected[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_first_sqlite_authority() {
        let key = "sqlite-authoritative-first-wins";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        let first = selection
            .select_sqlite(make_opencode_selection_message(
                key, 0.50, CostSource::ProviderReported,
            ))
            .expect("the first authoritative SQLite message must be selected");
        assert_eq!(first.cost, 0.50);
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key, 0.75, CostSource::ProviderReported,
            ))
            .is_none());
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_fallback_until_json_is_emitted() {
        let key = "filtered-authoritative";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        assert!(selection.select_sqlite(make_opencode_selection_message(
            key, 0.25, CostSource::Estimated,
        )).is_none());
        assert!(selection.select_json(make_opencode_selection_message(
            key, 0.50, CostSource::ProviderReported,
        ), false).is_none());
        let selected: Vec<_> = selection.finish().collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].cost, 0.25);
    }

    #[test]
    fn test_opencode_streaming_selection_does_not_double_count_new_authority() {
        let key = "newly-authoritative";
        let mut selection = OpenCodeSelection::new(HashSet::new());
        assert!(selection.select_sqlite(make_opencode_selection_message(
            key, 0.25, CostSource::Estimated,
        )).is_some());
        assert!(selection.select_json(make_opencode_selection_message(
            key, 0.50, CostSource::ProviderReported,
        ), true).is_none());
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_incompatible_same_id_rows() {
        let key = "reused-message-id";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        let first = make_opencode_selection_message(key, 0.25, CostSource::Estimated);
        let mut second = make_opencode_selection_message(key, 0.50, CostSource::Estimated);
        second.tokens.input = 20;
        let json = make_opencode_selection_message(key, 0.75, CostSource::ProviderReported);

        assert!(selection.select_sqlite(first).is_none());
        assert!(selection.select_sqlite(second).is_none());
        assert!(selection.select_json(json, true).is_some());
        let deferred: Vec<_> = selection.finish().collect();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].tokens.input, 20);
    }

    #[test]
    fn test_opencode_streaming_selection_replaces_deferred_identity_after_other_emission() {
        let key = "reused-authoritative-id";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        let first = make_opencode_selection_message(key, 0.25, CostSource::ProviderReported);
        let mut second = make_opencode_selection_message(key, 0.50, CostSource::Estimated);
        second.tokens.input = 20;
        let mut json = make_opencode_selection_message(key, 0.75, CostSource::ProviderReported);
        json.tokens.input = 20;

        assert!(selection.select_sqlite(first).is_some());
        assert!(selection.select_sqlite(second).is_none());
        let selected = selection
            .select_json(json, true)
            .expect("JSON must replace its exact deferred identity");
        assert_eq!(selected.tokens.input, 20);
        assert_eq!(selected.cost_source, CostSource::ProviderReported);
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_expands_authority_through_aliases() {
        let embedded = "shared-embedded";
        let fallback = "legacy-message";
        let pure = make_opencode_selection_message(embedded, 0.25, CostSource::Estimated);
        let mut hybrid = make_opencode_selection_message(embedded, 0.50, CostSource::Estimated);
        hybrid.dedup_aliases.push(fallback.to_string());
        let json = make_opencode_selection_message(fallback, 0.75, CostSource::ProviderReported);
        let duplicate =
            make_opencode_selection_message(embedded, 1.0, CostSource::ProviderReported);
        let authoritative = opencode_authoritative_sources(
            [&pure, &hybrid, &json]
                .into_iter()
                .map(opencode_identity_group),
        );
        let mut selection = OpenCodeSelection::new(authoritative);

        assert!(selection.select_sqlite(pure).is_none());
        assert!(selection.select_sqlite(hybrid).is_none());
        let selected = selection
            .select_json(json, true)
            .expect("JSON authority must replace the aliased SQLite identity");
        assert_eq!(selected.cost, 0.75);
        assert!(selection.select_json(duplicate, true).is_none());
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_aliases_on_sqlite_replacement() {
        let embedded = "shared-embedded";
        let fallback = "legacy-message";
        let mut hybrid = make_opencode_selection_message(embedded, 0.25, CostSource::Estimated);
        hybrid.dedup_aliases.push(fallback.to_string());
        let sqlite = make_opencode_selection_message(embedded, 0.50, CostSource::ProviderReported);
        let json = make_opencode_selection_message(fallback, 0.75, CostSource::ProviderReported);
        let authoritative = opencode_authoritative_sources(
            [&hybrid, &sqlite, &json]
                .into_iter()
                .map(opencode_identity_group),
        );
        let mut selection = OpenCodeSelection::new(authoritative);

        assert!(selection.select_sqlite(hybrid).is_none());
        assert!(selection.select_sqlite(sqlite).is_some());
        assert!(selection.select_json(json, true).is_none());
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_propagates_emitted_aliases_to_deferred_rows() {
        let embedded = "shared-embedded";
        let fallback = "legacy-message";
        let first = make_opencode_selection_message(embedded, 0.25, CostSource::ProviderReported);
        let mut deferred = make_opencode_selection_message(embedded, 0.50, CostSource::Estimated);
        deferred.tokens.input = 20;
        deferred.dedup_aliases.push(fallback.to_string());
        let mut json = make_opencode_selection_message(fallback, 0.75, CostSource::ProviderReported);
        json.tokens.input = 30;
        let authoritative = opencode_authoritative_sources(
            [&first, &deferred, &json]
                .into_iter()
                .map(opencode_identity_group),
        );
        let mut selection = OpenCodeSelection::new(authoritative);

        assert!(selection.select_sqlite(first).is_some());
        assert!(selection.select_sqlite(deferred).is_none());
        assert!(selection.select_json(json, true).is_none());
        let remaining: Vec<_> = selection.finish().collect();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].tokens.input, 20);
    }

    #[test]
    fn test_opencode_streaming_selection_propagates_emitted_aliases_from_deferred_graph() {
        let embedded = "shared-embedded";
        let fallback = "legacy-message";
        let mut deferred = make_opencode_selection_message(embedded, 0.25, CostSource::Estimated);
        deferred.tokens.input = 20;
        deferred.dedup_aliases.push(fallback.to_string());
        let sqlite = make_opencode_selection_message(embedded, 0.50, CostSource::ProviderReported);
        let mut json = make_opencode_selection_message(fallback, 0.75, CostSource::ProviderReported);
        json.tokens.input = 30;
        let authoritative = opencode_authoritative_sources(
            [&deferred, &sqlite, &json]
                .into_iter()
                .map(opencode_identity_group),
        );
        let mut selection = OpenCodeSelection::new(authoritative);

        assert!(selection.select_sqlite(deferred).is_none());
        assert!(selection.select_sqlite(sqlite).is_some());
        assert!(selection.select_json(json, true).is_none());
        let remaining: Vec<_> = selection.finish().collect();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].tokens.input, 20);
    }

    #[test]
    fn test_opencode_streaming_selection_prefers_snapshot_authority() {
        let key = "stable-authoritative";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        assert!(selection.select_sqlite(make_opencode_selection_message(
            key, 0.25, CostSource::Estimated,
        )).is_none());
        let json = selection.select_json(make_opencode_selection_message(
            key, 0.50, CostSource::ProviderReported,
        ), true).unwrap();
        assert_eq!(json.cost, 0.50);
        assert_eq!(json.cost_source, CostSource::ProviderReported);
        assert_eq!(selection.finish().count(), 0);
    }

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

    fn create_opencode_v2_sqlite_db(db_path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT NOT NULL
            );
            CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
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

    /// Serializes tests that mutate the process-wide model-alias map.
    static MODEL_ALIAS_GLOBAL_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_alias_map(pairs: &[(&str, &str)]) -> ModelAliasMap {
        ModelAliasMap {
            entries: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn model_aliases_fold_grouping_only_not_canonical_or_pricing() {
        let _guard = MODEL_ALIAS_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_model_aliases();

        // Alias a channel-specific spelling onto the display/group key.
        set_model_aliases(&test_alias_map(&[("claude-opus-4-8-cc", "claude-opus-4-8")]));

        // Grouping sees the folded label.
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-8-cc"),
            "claude-opus-4-8"
        );
        // Raw identity path stays alias-free (syntactic only).
        assert_eq!(
            canonical_model_id("claude-opus-4-8-cc"),
            "claude-opus-4-8-cc"
        );

        // Pricing resolves the *raw* message model id, never the grouping label.
        // Custom pricing is registered only under the raw channel spelling; the
        // display/group key has no rate. After the alias fold, cost still uses
        // the raw path and remains non-zero.
        let mut custom = HashMap::new();
        custom.insert(
            "claude-opus-4-8-cc".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let service = pricing::PricingService::new_with_custom(
            pricing::custom::CustomPricing::from_models(custom),
            HashMap::new(),
            HashMap::new(),
        );
        let tokens = TokenBreakdown {
            input: 1000,
            output: 500,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        };
        let raw_cost = service.calculate_cost_with_provider("claude-opus-4-8-cc", None, &tokens);
        let group_label_cost =
            service.calculate_cost_with_provider("claude-opus-4-8", None, &tokens);
        assert!(raw_cost > 0.0, "raw id must remain the pricing key");
        assert_eq!(
            group_label_cost, 0.0,
            "grouping label must not be used as the pricing key"
        );

        let mut msg = UnifiedMessage::new(
            "claude",
            "claude-opus-4-8-cc",
            "anthropic",
            "s1",
            1_733_011_200_000,
            tokens.clone(),
            0.0,
        );
        apply_pricing_if_available(&mut msg, Some(&service));
        assert!(
            (msg.cost - raw_cost).abs() < 1e-12,
            "apply_pricing must keep using the raw message model_id (got {}, expected {})",
            msg.cost,
            raw_cost
        );
        // Message identity is never rewritten by aliases.
        assert_eq!(msg.model_id, "claude-opus-4-8-cc");

        // Model-report grouping merges channel variants under the alias label,
        // while pre-computed costs simply sum (aliases never reprice).
        let mut other = UnifiedMessage::new(
            "claude",
            "claude-opus-4-8",
            "anthropic",
            "s2",
            1_733_011_200_001,
            tokens,
            1.5,
        );
        other.mark_estimated_cost();
        msg.cost = raw_cost;
        msg.mark_estimated_cost();
        let entries = aggregate_model_usage_entries(vec![msg, other], &GroupBy::Model);
        assert_eq!(
            entries.len(),
            1,
            "alias must fold both variants into one bucket"
        );
        assert_eq!(entries[0].model, "claude-opus-4-8");
        assert!(
            (entries[0].cost - (raw_cost + 1.5)).abs() < 1e-9,
            "merged cost is the sum of already-costed buckets, not a reprice"
        );

        // Graph/export client contribution keeps the alias-free raw identity.
        let graph = fold_messages_streaming(&[UnifiedMessage::new(
            "claude",
            "claude-opus-4-8-cc",
            "anthropic",
            "s1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            0.25,
        )]);
        assert_eq!(graph.len(), 1);
        assert_eq!(graph[0].clients.len(), 1);
        assert_eq!(graph[0].clients[0].model_id, "claude-opus-4-8-cc");

        clear_model_aliases();
    }

    #[test]
    fn model_alias_reload_invalidates_usage_data_consumers() {
        let _guard = MODEL_ALIAS_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_model_aliases();

        static FIRES: AtomicUsize = AtomicUsize::new(0);
        register_usage_data_invalidation_hook(|| {
            FIRES.fetch_add(1, Ordering::SeqCst);
        });
        let baseline_fires = FIRES.load(Ordering::SeqCst);
        let gen0 = model_alias_generation();

        set_model_aliases(&test_alias_map(&[("claude-opus-4-8-cc", "claude-opus-4-8")]));
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-8-cc"),
            "claude-opus-4-8"
        );
        let gen1 = model_alias_generation();
        assert!(gen1 > gen0);

        // Reload replaces the map; consumers see the new fold on the next report.
        set_model_aliases(&test_alias_map(&[("gpt-5.5-cc", "gpt-5.5")]));
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-8-cc"),
            "claude-opus-4-8-cc",
            "previous alias must not survive a reload"
        );
        assert_eq!(normalize_model_for_grouping("gpt-5.5-cc"), "gpt-5.5");
        let gen2 = model_alias_generation();
        assert!(gen2 > gen1);

        clear_model_aliases();
        assert_eq!(normalize_model_for_grouping("gpt-5.5-cc"), "gpt-5.5-cc");
        assert!(model_alias_generation() > gen2);

        let after_fires = FIRES.load(Ordering::SeqCst);
        assert!(
            after_fires >= baseline_fires + 3,
            "set/reload/clear must each fire the usage-data invalidation hook \
             (baseline={baseline_fires}, after={after_fires})"
        );
    }

    #[test]
    fn aggregate_model_usage_entries_uses_fold_start_alias_snapshot() {
        // Codex P2: multi-message report folds must keep one alias config for
        // the whole fold. `aggregate_model_usage_entries` snapshots at start;
        // prove the snapshot pattern it uses is stable under mid-fold reload,
        // then that the real aggregator still merges under the fold-start map.
        let _guard = MODEL_ALIAS_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_model_aliases();
        set_model_aliases(&test_alias_map(&[("alias-a", "canonical-b")]));

        // Same capture point as aggregate_model_usage_entries.
        let aliases = snapshot_grouping_aliases();
        assert_eq!(
            aliases.fold(normalize_syntactic("alias-a")),
            "canonical-b",
            "fold start sees A→B"
        );

        // Mid-fold mutation of the process-wide map.
        set_model_aliases(&test_alias_map(&[("alias-a", "canonical-other")]));
        assert_eq!(
            normalize_model_for_grouping("alias-a"),
            "canonical-other",
            "live per-call path sees the reloaded map"
        );
        // Every message in the fold keeps the snapshotted label.
        for _ in 0..3 {
            assert_eq!(
                aliases.fold(normalize_syntactic("alias-a")),
                "canonical-b",
                "snapshot must ignore mid-fold reload for the rest of the fold"
            );
        }

        // Re-install fold-start aliases and run the real aggregator: both
        // channel variants merge under the snapshotted canonical label.
        set_model_aliases(&test_alias_map(&[("alias-a", "canonical-b")]));
        let tokens = TokenBreakdown {
            input: 10,
            output: 5,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        };
        let msg_a = UnifiedMessage::new(
            "claude",
            "alias-a",
            "anthropic",
            "s1",
            1_733_011_200_000,
            tokens.clone(),
            1.0,
        );
        let msg_b = UnifiedMessage::new(
            "claude",
            "canonical-b",
            "anthropic",
            "s2",
            1_733_011_200_001,
            tokens,
            2.0,
        );
        let entries = aggregate_model_usage_entries(vec![msg_a, msg_b], &GroupBy::Model);
        assert_eq!(entries.len(), 1, "fold-start alias must merge both variants");
        assert_eq!(entries[0].model, "canonical-b");
        assert!((entries[0].cost - 3.0).abs() < 1e-12);

        clear_model_aliases();
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
    #[serial_test::serial]
    fn test_cursor_parse_path_reprices_zero_cost_composer_1_5_rows() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_kimi_deduplicates_repeated_status_updates() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        // Hermetic: cache-only pricing + temp HOME → no network, pricing None.
        let _pricing = EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let _pricing = EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

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
    }

    // Issue #36: the client selection must be applied at the STREAMING SCAN,
    // not by a downstream membership filter over the pre-aggregated buckets.
    // codebuff (200/80) and kimi (100/50) fold into ONE shared "Main" agent
    // bucket; the FFI/DashboardModel now thread the selection into
    // ReportOptions.clients, so filtering to codebuff yields ONLY its 200/80 —
    // NOT the mixed 300/130. The old approach (unfiltered report + a Swift
    // membership filter over whole buckets) kept the entire shared bucket and
    // would read 300/130 here, so this test is RED against it and GREEN now.
    #[test]
    #[serial_test::serial]
    fn test_agents_report_client_filter_scopes_shared_bucket_issue36() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let _pricing = EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

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
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let run = |clients: Option<Vec<String>>| {
                rt.block_on(get_agents_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients,
                    ..Default::default()
                }))
                .unwrap()
            };

            // All clients: one shared "Main" bucket carrying the mixed total.
            let all = run(None);
            assert_eq!(all.entries.len(), 1, "codebuff + kimi share one Main bucket");
            assert_eq!(all.entries[0].agent, "Main");
            assert_eq!(all.entries[0].input, 300, "mixed bucket = 200 + 100");
            assert_eq!(all.entries[0].output, 130, "mixed bucket = 80 + 50");
            assert_eq!(all.total_messages, 2);

            // Filtered to codebuff: the SAME shared bucket, scoped at the scan
            // to codebuff's contribution alone — proves the FFI-level filter,
            // not a whole-bucket membership keep (which would still read 300).
            let filtered = run(Some(vec!["codebuff".to_string()]));
            assert_eq!(filtered.entries.len(), 1);
            assert_eq!(filtered.entries[0].agent, "Main");
            assert_eq!(
                filtered.entries[0].input, 200,
                "filtered = codebuff only, not the mixed 300"
            );
            assert_eq!(filtered.entries[0].output, 80);
            assert_eq!(filtered.entries[0].clients, vec!["codebuff".to_string()]);
            assert_eq!(filtered.total_messages, 1, "kimi's message is gone");
        }
    }

    // Issue #36 (round 3): a cc-mirror variant id (`cc-mirror/kimi-code`) is
    // produced during CLAUDE-lane parsing, not by a scanner lane of its own —
    // `ClientId::from_str("cc-mirror/kimi-code")` is None. The two-level split
    // must (a) map the variant to its producing `claude` lane so the scan finds
    // it, and (b) keep ONLY the exact requested ids at fold time so requesting
    // the variant returns just the variant, and requesting `claude` returns
    // plain claude WITHOUT the variant (which the graph/daily/models surface as
    // its own client id). RED against the old lane-only filter: requesting the
    // variant returned empty, and requesting claude swept the variant in.
    #[test]
    #[serial_test::serial]
    fn test_agents_report_cc_mirror_variant_slice_issue36() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let _pricing = EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

        {
            // Plain claude session (client "claude"): 100 in / 50 out.
            let claude_dir = source_home.path().join(".claude/projects/myproject");
            std::fs::create_dir_all(&claude_dir).unwrap();
            std::fs::write(
                claude_dir.join("conversation.jsonl"),
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_plain","message":{"id":"msg_plain","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#,
            )
            .unwrap();
            // cc-mirror variant (client "cc-mirror/kimi-code"): 300 in / 70 out.
            let variant_dir = source_home.path().join(".cc-mirror/kimi-code");
            let config_dir = variant_dir.join("config");
            let project_dir = config_dir.join("projects/proj");
            std::fs::create_dir_all(&project_dir).unwrap();
            std::fs::write(
                variant_dir.join("variant.json"),
                serde_json::json!({
                    "name": "kimi-code",
                    "provider": "kimi",
                    "configDir": config_dir,
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                project_dir.join("session.jsonl"),
                r#"{"type":"assistant","timestamp":"2024-12-01T11:00:00.000Z","requestId":"req_variant","message":{"id":"msg_variant","model":"claude-3-5-sonnet","usage":{"input_tokens":300,"output_tokens":70}}}"#,
            )
            .unwrap();

            let home = source_home.path().to_str().unwrap().to_string();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let run = |clients: Option<Vec<String>>| {
                let report = rt
                    .block_on(get_agents_report(ReportOptions {
                        home_dir: Some(home.clone()),
                        use_env_roots: false,
                        clients,
                        ..Default::default()
                    }))
                    .unwrap();
                let input: i64 = report.entries.iter().map(|e| e.input).sum();
                (report.total_messages, input)
            };

            // All clients: both messages fold into "Main".
            assert_eq!(run(None), (2, 400), "all: plain(100) + variant(300)");

            // Variant slice: the claude lane is scanned (so the variant is
            // found), then narrowed to exactly the variant id.
            assert_eq!(
                run(Some(vec!["cc-mirror/kimi-code".to_string()])),
                (1, 300),
                "variant slice = just the variant (300), not empty"
            );

            // Claude slice: plain claude ONLY — the distinct variant is excluded.
            assert_eq!(
                run(Some(vec!["claude".to_string()])),
                (1, 100),
                "claude slice = plain claude (100), not the mixed 400"
            );
        }
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
    fn agent_accumulator_saturates_overflowing_token_folds() {
        // Vendor-local sibling sweep alongside #823: AgentAccumulator::add is
        // its own per-field CROSS-MESSAGE fold (agents streaming report), not
        // one of the 6 sites #823 covers. An antigravity-cli row can carry an
        // i64::MAX bucket after the untrusted-varint clamp, so two such rows
        // folded into one agent bucket with plain `+=` overflow (debug panic /
        // release wrap) before any saturating grand total runs.
        let make = || {
            UnifiedMessage::new(
                "antigravity-cli",
                "gemini-3-pro",
                "antigravity",
                "session-overflow",
                1_733_011_200_000,
                TokenBreakdown {
                    input: i64::MAX,
                    output: 0,
                    cache_read: i64::MAX,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0,
            )
        };

        let mut acc = AgentAccumulator::default();
        acc.add(&make());
        acc.add(&make());

        assert_eq!(acc.input, i64::MAX);
        assert_eq!(acc.cache_read, i64::MAX);
    }

    #[test]
    fn test_agent_bucket_key_copilot_uses_copilot_normalizer() {
        // #724/#751: copilot messages carry a raw OTEL agent id. Our agents
        // report must prettify it with the copilot-specific normalizer (the
        // prettification upstream does in its CLI), while other clients keep the
        // generic normalization.
        let msg = |client: &str, agent: &str| {
            UnifiedMessage::new_with_agent(
                client,
                "m",
                "p",
                "s",
                0,
                TokenBreakdown::default(),
                0.0,
                Some(agent.to_string()),
            )
        };

        // Copilot: raw OTEL ids resolve to their pretty display form.
        assert_eq!(
            agent_bucket_key(&msg("copilot", "github.copilot.default")),
            "GitHub Copilot"
        );
        assert_eq!(
            agent_bucket_key(&msg("copilot", "Plugin:code-review-team:api-reviewer")),
            "Code Review Team: API Reviewer"
        );

        // A non-copilot client with the same raw id must NOT get the
        // copilot-specific prettification (proves the branch is client-scoped).
        assert_ne!(
            agent_bucket_key(&msg("codebuff", "github.copilot.default")),
            "GitHub Copilot"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_refreshes_stale_date_on_cache_hit() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        {
            let message_dir = scanner_fixture_path(
                source_home.path(),
                ".local/share/opencode/storage/message/project-1",
            );
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
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn test_empty_parse_results_are_not_cached_for_optional_file_sources() {
        use std::os::unix::fs::PermissionsExt;

        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        {
            let message_dir = scanner_fixture_path(
                source_home.path(),
                ".local/share/opencode/storage/message/project-1",
            );
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
    }

    #[test]
    #[serial_test::serial]
    fn test_empty_cache_hits_are_reparsed_for_optional_file_sources() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        {
            let message_dir = scanner_fixture_path(
                source_home.path(),
                ".local/share/opencode/storage/message/project-1",
            );
            std::fs::create_dir_all(&message_dir).unwrap();
            let source_path = message_dir.join("msg_001.json");
            std::fs::write(
                &source_path,
                r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            )
            .unwrap();
            let cache_path = scanner::scan_all_clients_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                true,
            )
            .get(ClientId::OpenCode)
            .first()
            .cloned()
            .expect("scanner must find the OpenCode fixture");

            let fingerprint = message_cache::SourceFingerprint::from_path(&cache_path).unwrap();
            let mut cache = message_cache::SourceMessageCache::default();
            cache.insert(message_cache::CachedSourceEntry::new(
                &cache_path,
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
            let repaired_entry = loaded.get(&cache_path).unwrap();
            assert_eq!(repaired_entry.messages.len(), 1);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_opencode_aliases_preserve_cross_store_identity_across_lanes() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        let db_dir = source_home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("opencode-next.db");
        let conn = create_opencode_v2_sqlite_db(&db_path);
        let v1_payload = build_opencode_sqlite_payload(
            1_700_000_000_000.0,
            1_700_000_000_500.0,
            100,
            10,
            1,
            5,
            2,
            0.0,
        );
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["legacy-message", "session-overlap", &v1_payload],
        )
        .unwrap();
        let v2_payload = build_opencode_sqlite_payload(
            1_700_000_000_000.0,
            1_700_000_000_500.0,
            100,
            10,
            1,
            5,
            2,
            0.02,
        )
        .replacen('{', r#"{"id":"v2-embedded","#, 1);
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["v2-message", "session-overlap", "assistant", &v2_payload],
        )
        .unwrap();
        let deferred_payload = build_opencode_sqlite_payload(
            1_700_000_010_000.0,
            1_700_000_010_500.0,
            20,
            5,
            0,
            0,
            0,
            0.0,
        );
        let deferred_v2_payload =
            deferred_payload.replacen('{', r#"{"id":"shared-order","#, 1);
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["legacy-order", "session-order", &deferred_payload],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "v2-order",
                "session-order",
                "assistant",
                &deferred_v2_payload
            ],
        )
        .unwrap();
        drop(conn);

        let sibling_db = db_dir.join("opencode-stable.db");
        let sibling_conn = create_opencode_v2_sqlite_db(&sibling_db);
        sibling_conn
            .execute(
                "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "v2-sibling",
                    "session-overlap",
                    "assistant",
                    &v2_payload
                ],
            )
            .unwrap();
        let provider_payload = build_opencode_sqlite_payload(
            1_700_000_010_000.0,
            1_700_000_010_500.0,
            10,
            5,
            0,
            0,
            0,
            0.02,
        )
        .replacen('{', r#"{"id":"shared-order","#, 1);
        sibling_conn
            .execute(
                "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "v2-order-provider",
                    "session-order",
                    "assistant",
                    &provider_payload
                ],
            )
            .unwrap();
        drop(sibling_conn);

        let json_dir = db_dir.join("storage/message/project-overlap");
        std::fs::create_dir_all(&json_dir).unwrap();
        let migrated_json_payload = build_opencode_sqlite_payload(
            1_700_000_000_000.0,
            1_700_000_000_500.0,
            100,
            10,
            1,
            5,
            2,
            0.03,
        );
        std::fs::write(
            json_dir.join("legacy-message.json"),
            migrated_json_payload,
        )
        .unwrap();
        let json_payload = build_opencode_sqlite_payload(
            1_700_000_010_000.0,
            1_700_000_010_500.0,
            30,
            5,
            0,
            0,
            0,
            0.03,
        );
        std::fs::write(json_dir.join("legacy-order.json"), json_payload).unwrap();

        let clients = vec!["opencode".to_string()];
        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
        );
        assert_eq!(materialized.len(), 3);
        let mut materialized_inputs: Vec<_> = materialized
            .iter()
            .map(|message| message.tokens.input)
            .collect();
        materialized_inputs.sort_unstable();
        assert_eq!(materialized_inputs, vec![10, 20, 100]);
        let migrated = materialized
            .iter()
            .find(|message| message.tokens.input == 100)
            .unwrap();
        assert_eq!(migrated.dedup_key.as_deref(), Some("v2-embedded"));
        assert_eq!(migrated.dedup_aliases, vec!["legacy-message"]);
        assert_eq!(migrated.cost, 0.02);
        assert_eq!(migrated.cost_source, CostSource::ProviderReported);
        let warm = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
        );
        assert_eq!(warm, materialized, "cache hits must retain OpenCode aliases");

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 3);
        let mut streamed_inputs: Vec<_> = streamed
            .iter()
            .map(|message| message.tokens.input)
            .collect();
        streamed_inputs.sort_unstable();
        assert_eq!(streamed_inputs, vec![10, 20, 100]);
        let migrated = streamed
            .iter()
            .find(|message| message.tokens.input == 100)
            .unwrap();
        assert_eq!(migrated.dedup_key.as_deref(), Some("v2-embedded"));
        assert_eq!(migrated.dedup_aliases, vec!["legacy-message"]);
        assert_eq!(migrated.cost, 0.02);
        assert_eq!(migrated.cost_source, CostSource::ProviderReported);

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::OpenCode), 3);
        assert_eq!(counted.messages.len(), 3);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            130
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_schema_29_hybrid_opencode_cache_rebuilds_with_v2_rows_across_lanes() {
        #[derive(serde::Serialize)]
        struct Schema29Store {
            schema_version: u32,
            entries: Vec<message_cache::CachedSourceEntry>,
        }

        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let _pricing_env =
            EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

        let db_dir = source_home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("opencode-next.db");
        let conn = create_opencode_v2_sqlite_db(&db_path);
        conn.execute(
            "INSERT INTO session (id, directory) VALUES (?1, ?2), (?3, ?4)",
            rusqlite::params!["session-v1", "/workspace/v1", "session-v2", "/workspace/v2"],
        )
        .unwrap();
        let v1_data = r#"{
            "id": "message-v1",
            "role": "assistant",
            "modelID": "claude-sonnet-4",
            "providerID": "anthropic",
            "mode": "build",
            "cost": 0.0,
            "tokens": { "input": 100, "output": 10, "reasoning": 1, "cache": { "read": 5, "write": 2 } },
            "time": { "created": 1700000000000.0, "completed": 1700000000500.0 }
        }"#;
        let v2_data = r#"{
            "id": "message-v1",
            "model": { "id": "claude-sonnet-4", "providerID": "anthropic" },
            "agent": "build",
            "cost": 0.02,
            "tokens": { "input": 200, "output": 20, "reasoning": 2, "cache": { "read": 10, "write": 4 } },
            "time": { "created": 1700000001000.0, "completed": 1700000001750.0 }
        }"#;
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["row-v1", "session-v1", v1_data],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["row-v2", "session-v2", "assistant", v2_data],
        )
        .unwrap();
        drop(conn);

        let json_dir = db_dir.join("storage/message/project-v1");
        std::fs::create_dir_all(&json_dir).unwrap();
        std::fs::write(
            json_dir.join("message-v1.json"),
            r#"{
                "id": "message-v1",
                "sessionID": "session-v1",
                "role": "assistant",
                "modelID": "claude-sonnet-4",
                "providerID": "anthropic",
                "mode": "build",
                "cost": 0.01,
                "tokens": { "input": 100, "output": 10, "reasoning": 1, "cache": { "read": 5, "write": 2 } },
                "time": { "created": 1700000000000.0, "completed": 1700000000500.0 }
            }"#,
        )
        .unwrap();

        let fingerprint = message_cache::SourceFingerprint::from_sqlite_path(&db_path).unwrap();
        let mut stale_message = UnifiedMessage::new_with_dedup(
            "opencode",
            "claude-sonnet-4",
            "anthropic",
            "session-v1",
            1_700_000_000_000,
            TokenBreakdown {
                input: 100,
                output: 10,
                cache_read: 5,
                cache_write: 2,
                reasoning: 1,
            },
            0.0,
            Some("message-v1".to_string()),
        );
        stale_message.duration_ms = Some(500);
        let stale_store = Schema29Store {
            schema_version: 29,
            entries: vec![message_cache::CachedSourceEntry::new(
                &db_path,
                fingerprint.clone(),
                vec![stale_message],
                Vec::new(),
                None,
            )],
        };
        let cache_file = crate::paths::get_cache_dir().join("source-message-cache.bin");
        std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        let writer = std::io::BufWriter::new(std::fs::File::create(&cache_file).unwrap());
        bincode::options()
            .serialize_into(writer, &stale_store)
            .unwrap();

        assert_eq!(
            message_cache::SourceFingerprint::from_sqlite_path(&db_path).unwrap(),
            fingerprint,
            "the schema-29 v1-only entry must match the unchanged hybrid database"
        );
        assert!(
            message_cache::SourceMessageCache::load().entries.is_empty(),
            "schema-29 cache entries must be rejected before parsing v2 rows"
        );

        let clients = vec!["opencode".to_string()];
        let parse_materialized = || {
            parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
            )
        };
        let mut cold = parse_materialized();
        let mut warm = parse_materialized();
        cold.sort_by(|left, right| {
            (&left.dedup_key, left.tokens.input).cmp(&(&right.dedup_key, right.tokens.input))
        });
        warm.sort_by(|left, right| {
            (&left.dedup_key, left.tokens.input).cmp(&(&right.dedup_key, right.tokens.input))
        });
        assert_eq!(cold.len(), 2);
        assert_eq!(
            cold.iter()
                .map(|message| message.dedup_key.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("message-v1"), Some("message-v1")],
            "same embedded ids with incompatible timestamps and tokens must remain distinct"
        );
        assert_eq!(
            cold.iter()
                .map(|message| (&message.dedup_key, &message.tokens))
                .collect::<Vec<_>>(),
            warm.iter()
                .map(|message| (&message.dedup_key, &message.tokens))
                .collect::<Vec<_>>(),
            "the rebuilt current-schema entry must preserve v1+v2 output on a warm hit"
        );
        let rebuilt_cache = message_cache::SourceMessageCache::load();
        assert_eq!(rebuilt_cache.get(&db_path).unwrap().messages.len(), 2);

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        streamed.sort_by(|left, right| {
            (&left.dedup_key, left.tokens.input).cmp(&(&right.dedup_key, right.tokens.input))
        });
        assert_eq!(
            streamed
                .iter()
                .map(|message| (&message.dedup_key, &message.tokens))
                .collect::<Vec<_>>(),
            cold.iter()
                .map(|message| (&message.dedup_key, &message.tokens))
                .collect::<Vec<_>>()
        );

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::OpenCode), 2);
        assert_eq!(counted.messages.len(), 2);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            300
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let options = ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients),
            ..Default::default()
        };
        let model = runtime.block_on(get_model_report(options.clone())).unwrap();
        let monthly = runtime
            .block_on(get_monthly_report(options.clone()))
            .unwrap();
        let hourly = runtime
            .block_on(get_hourly_report(options.clone()))
            .unwrap();
        let agents = runtime.block_on(get_agents_report(options)).unwrap();
        assert_eq!(model.total_messages, 2);
        assert_eq!(
            (
                model.total_input,
                model.total_output,
                model.total_cache_read,
                model.total_cache_write,
                model
                    .entries
                    .iter()
                    .map(|entry| entry.reasoning)
                    .sum::<i64>(),
            ),
            (300, 30, 15, 6, 3)
        );
        assert_eq!(
            monthly.entries.iter().fold((0, 0, 0, 0), |totals, entry| (
                totals.0 + entry.input,
                totals.1 + entry.output,
                totals.2 + entry.cache_read,
                totals.3 + entry.cache_write,
            ),),
            (300, 30, 15, 6)
        );
        assert_eq!(
            hourly
                .entries
                .iter()
                .fold((0, 0, 0, 0, 0), |totals, entry| (
                    totals.0 + entry.input,
                    totals.1 + entry.output,
                    totals.2 + entry.cache_read,
                    totals.3 + entry.cache_write,
                    totals.4 + entry.reasoning,
                ),),
            (300, 30, 15, 6, 3)
        );
        assert_eq!(
            agents
                .entries
                .iter()
                .fold((0, 0, 0, 0, 0), |totals, entry| (
                    totals.0 + entry.input,
                    totals.1 + entry.output,
                    totals.2 + entry.cache_read,
                    totals.3 + entry.cache_write,
                    totals.4 + entry.reasoning,
                ),),
            (300, 30, 15, 6, 3)
        );
        assert_eq!(
            monthly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            model.total_messages
        );
        assert_eq!(
            hourly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            model.total_messages
        );
        assert_eq!(agents.total_messages, model.total_messages);
    }

    #[test]
    #[serial_test::serial]
    fn m16_schema_30_jcode_cache_rebuilds_start_anchor_across_lanes() {
        #[derive(serde::Serialize)]
        struct Schema30Store {
            schema_version: u32,
            entries: Vec<message_cache::CachedSourceEntry>,
        }

        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let _pricing_env =
            EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

        let sessions_dir = source_home.path().join(".jcode/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let source_path = sessions_dir.join("session_m16.json");
        std::fs::write(
            &source_path,
            r#"{"id":"session_m16","provider_key":"cliproxyapi","model":"claude-sonnet-4","working_dir":"/workspace/m16","messages":[{"id":"u1","role":"user","timestamp":"2026-06-16T12:00:00Z"},{"id":"a1","role":"assistant","timestamp":"2026-06-16T12:00:01Z","token_usage":{"input_tokens":1200,"output_tokens":300},"tool_duration_ms":1000}]}"#,
        )
        .unwrap();

        let start = sessions::utils::parse_timestamp_str("2026-06-16T12:00:00Z").unwrap();
        let end = sessions::utils::parse_timestamp_str("2026-06-16T12:00:01Z").unwrap();
        let fingerprint = message_cache::SourceFingerprint::from_jcode_path(&source_path).unwrap();
        let mut stale_message = UnifiedMessage::new_with_dedup(
            "jcode",
            "claude-sonnet-4",
            "cliproxyapi",
            "session_m16",
            end,
            TokenBreakdown {
                input: 1200,
                output: 300,
                ..Default::default()
            },
            0.0,
            Some("stale-schema-30".to_string()),
        );
        stale_message.duration_ms = Some(end - start);
        let stale_store = Schema30Store {
            schema_version: 30,
            entries: vec![message_cache::CachedSourceEntry::new(
                &source_path,
                fingerprint.clone(),
                vec![stale_message],
                Vec::new(),
                None,
            )],
        };
        let cache_file = crate::paths::get_cache_dir().join("source-message-cache.bin");
        std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        let writer = std::io::BufWriter::new(std::fs::File::create(&cache_file).unwrap());
        bincode::options()
            .serialize_into(writer, &stale_store)
            .unwrap();

        assert_eq!(
            message_cache::SourceFingerprint::from_jcode_path(&source_path).unwrap(),
            fingerprint,
            "the source fingerprint must stay unchanged across the schema-only rebuild"
        );
        assert!(
            message_cache::SourceMessageCache::load().entries.is_empty(),
            "schema-30 entries must be rejected before corrected Jcode output is loaded"
        );

        let clients = vec!["jcode".to_string()];
        let parse_materialized = || {
            parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
            )
        };
        let cold = parse_materialized();
        let warm = parse_materialized();
        assert_eq!(cold.len(), 1);
        assert_eq!(
            (
                cold[0].timestamp,
                cold[0].duration_ms,
                cold[0].tokens.input,
                cold[0].tokens.output,
            ),
            (start, Some(end - start), 1200, 300)
        );
        assert_eq!(
            warm.iter()
                .map(|message| (
                    message.timestamp,
                    message.duration_ms,
                    message.tokens.clone()
                ))
                .collect::<Vec<_>>(),
            cold.iter()
                .map(|message| (
                    message.timestamp,
                    message.duration_ms,
                    message.tokens.clone()
                ))
                .collect::<Vec<_>>()
        );
        let rebuilt_cache = message_cache::SourceMessageCache::load();
        assert_eq!(rebuilt_cache.get(&source_path).unwrap().messages.len(), 1);

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 1);
        assert_eq!(
            (
                streamed[0].timestamp,
                streamed[0].duration_ms,
                streamed[0].tokens.input,
                streamed[0].tokens.output,
            ),
            (start, Some(end - start), 1200, 300)
        );

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::Jcode), 1);
        assert_eq!(counted.messages.len(), 1);
        assert_eq!(
            (counted.messages[0].input, counted.messages[0].output),
            (1200, 300)
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let options = ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients),
            ..Default::default()
        };
        let model = runtime.block_on(get_model_report(options.clone())).unwrap();
        let monthly = runtime
            .block_on(get_monthly_report(options.clone()))
            .unwrap();
        let hourly = runtime
            .block_on(get_hourly_report(options.clone()))
            .unwrap();
        let agents = runtime.block_on(get_agents_report(options)).unwrap();
        assert_eq!(
            (model.total_messages, model.total_input, model.total_output),
            (1, 1200, 300)
        );
        assert_eq!(
            monthly.entries.iter().fold((0, 0, 0), |totals, entry| (
                totals.0 + entry.message_count,
                totals.1 + entry.input,
                totals.2 + entry.output,
            )),
            (1, 1200, 300)
        );
        assert_eq!(
            hourly.entries.iter().fold((0, 0, 0), |totals, entry| (
                totals.0 + entry.message_count,
                totals.1 + entry.input,
                totals.2 + entry.output,
            )),
            (1, 1200, 300)
        );
        assert_eq!(agents.total_messages, 1);
        assert_eq!(
            agents.entries.iter().fold((0, 0), |totals, entry| (
                totals.0 + entry.input,
                totals.1 + entry.output,
            )),
            (1200, 300)
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_sqlite_source_cache_invalidates_on_wal_change() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

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
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_dedups_across_channel_suffixed_opencode_dbs() {
        // Regression guard: a session that appears in both `opencode.db` and
        // `opencode-<channel>.db` (e.g. the user switches channels mid-session)
        // must only be counted once.
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

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
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_opencode_sqlite_deduplicates_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

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
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_opencode_sqlite_counts_deduplicated_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

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
    }

    /// Regression fixture for Codex sessions that are live-only, archive-only,
    /// or briefly present in both roots while the CLI moves a transcript.
    fn write_codex_sessions_and_archived_sessions_fixture(source_home: &std::path::Path) {
        let sessions_dir = source_home.join(".codex/sessions");
        let archived_dir = source_home.join(".codex/archived_sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&archived_dir).unwrap();

        std::fs::write(
            sessions_dir.join("live-only.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-25T10:00:00Z","type":"session_meta","payload":{"id":"33333333-3333-7333-8333-333333333333","source":"interactive","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"output_tokens":5,"total_tokens":55},"last_token_usage":{"input_tokens":50,"output_tokens":5,"total_tokens":55}}}}"#,
                "\n"
            ),
        )
        .unwrap();

        std::fs::write(
            archived_dir.join("archived-only.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-20T09:00:00Z","type":"session_meta","payload":{"id":"44444444-4444-7444-8444-444444444444","source":"interactive","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-20T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-20T09:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":70,"output_tokens":7,"total_tokens":77},"last_token_usage":{"input_tokens":70,"output_tokens":7,"total_tokens":77}}}}"#,
                "\n"
            ),
        )
        .unwrap();

        let shared_content = concat!(
            r#"{"timestamp":"2026-06-22T08:00:00Z","type":"session_meta","payload":{"id":"55555555-5555-7555-8555-555555555555","source":"interactive","model_provider":"openai","cwd":"/repo"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-22T08:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-22T08:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":30,"output_tokens":3,"total_tokens":33},"last_token_usage":{"input_tokens":30,"output_tokens":3,"total_tokens":33}}}}"#,
            "\n"
        );
        std::fs::write(
            sessions_dir.join("shared-in-sessions.jsonl"),
            shared_content,
        )
        .unwrap();
        std::fs::write(
            archived_dir.join("shared-in-archived.jsonl"),
            shared_content,
        )
        .unwrap();
    }

    fn with_isolated_tokscale_cache<T>(
        cache_home: &std::path::Path,
        action: impl FnOnce() -> T,
    ) -> T {
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        action()
    }

    fn write_codex_duration_prefix_fixture(
        source_home: &std::path::Path,
    ) -> (std::path::PathBuf, String) {
        let lines = include_str!("../tests/fixtures/codex_duration_timing.jsonl")
            .lines()
            .collect::<Vec<_>>();
        let sessions_dir = source_home.join(".codex/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let source = sessions_dir.join("codex-duration-timing.jsonl");
        std::fs::write(&source, format!("{}\n", lines[..5].join("\n"))).unwrap();
        (source, format!("{}\n", lines[5..].join("\n")))
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_non_overlapping_durations_survive_incremental_and_streaming_caches() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let (source, suffix) = write_codex_duration_prefix_fixture(source_home.path());

        let home = source_home.path().to_str().unwrap().to_string();
        let clients = vec!["codex".to_string()];
        let parse_materialized = || {
            parse_all_messages_with_pricing_with_env_strategy(
                &home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
            )
        };

        let prefix = with_isolated_tokscale_cache(materialized_cache.path(), parse_materialized);
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].duration_ms, Some(1_000));

        let mut source_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap();
        source_file.write_all(suffix.as_bytes()).unwrap();
        source_file.flush().unwrap();
        drop(source_file);

        let materialized_incremental =
            with_isolated_tokscale_cache(materialized_cache.path(), parse_materialized);
        let materialized_warm =
            with_isolated_tokscale_cache(materialized_cache.path(), parse_materialized);
        for (phase, messages) in [
            ("incremental", &materialized_incremental),
            ("warm", &materialized_warm),
        ] {
            assert_eq!(messages.len(), 3, "{phase} materialized messages");
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.duration_ms)
                    .collect::<Vec<_>>(),
                vec![Some(1_000), Some(4_000), Some(2_000)],
                "{phase} materialized durations"
            );
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report_options = || ReportOptions {
            home_dir: Some(home.clone()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        };
        let streaming_cold = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });

        // The cold Codex streaming lane does not persist SourceMessageCache.
        // Seed this isolated cache through the public materialized path so the
        // second report exercises the shipping cache-hit branch.
        let cache_seed = with_isolated_tokscale_cache(streaming_cache.path(), parse_materialized);
        assert_eq!(cache_seed.len(), 3);
        assert!(
            streaming_cache
                .path()
                .join("cache/source-message-cache.bin")
                .is_file(),
            "materialized seed must persist the cache used by the warm report"
        );
        let streaming_warm = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });

        for (phase, report) in [("cold", &streaming_cold), ("warm", &streaming_warm)] {
            assert_eq!(report.total_messages, 3, "{phase} streaming messages");
            assert_eq!(report.entries.len(), 1, "{phase} model groups");
            let performance = &report.entries[0].performance;
            assert_eq!(
                performance.total_duration_ms, 7_000,
                "{phase} total duration"
            );
            assert_eq!(performance.timed_tokens, 170, "{phase} timed tokens");
            assert_eq!(performance.sample_count, 3, "{phase} samples");
            assert_eq!(performance.token_coverage, 1.0, "{phase} coverage");
            let expected_ms_per_1k = 7_000.0 * 1_000.0 / 170.0;
            assert!(
                (performance.ms_per_1k_tokens.unwrap() - expected_ms_per_1k).abs() < f64::EPSILON,
                "{phase} milliseconds per 1K tokens"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_archive_roots_are_exact_once_across_all_consumers() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let count_cache = tempfile::TempDir::new().unwrap();

        write_codex_sessions_and_archived_sessions_fixture(source_home.path());

        let home = source_home.path().to_str().unwrap().to_string();
        let clients = vec!["codex".to_string()];
        let materialized = with_isolated_tokscale_cache(materialized_cache.path(), || {
            parse_all_messages_with_pricing_with_env_strategy(
                &home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
            )
        });

        assert_eq!(materialized.len(), 3);
        let session_ids: HashSet<_> = materialized
            .iter()
            .map(|message| message.session_id.as_str())
            .collect();
        assert!(session_ids.contains("live-only"));
        assert!(session_ids.contains("archived-only"));
        assert_eq!(
            materialized
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            150,
        );
        assert_eq!(
            materialized
                .iter()
                .map(|message| message.tokens.output)
                .sum::<i64>(),
            15,
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report_options = || ReportOptions {
            home_dir: Some(home.clone()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        };
        let streaming_cold = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });

        // Codex's cold streaming lane does not populate SourceMessageCache, so
        // seed the same isolated cache through the public materialized path
        // before exercising the streaming cache-hit branch.
        let cache_seed = with_isolated_tokscale_cache(streaming_cache.path(), || {
            parse_all_messages_with_pricing_with_env_strategy(
                &home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
            )
        });
        assert_eq!(cache_seed.len(), 3);
        assert!(
            streaming_cache
                .path()
                .join("cache/source-message-cache.bin")
                .is_file(),
            "materialized seed must persist the cache used by the warm pass",
        );
        let streaming_warm = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });
        for (phase, streaming) in [("cold", &streaming_cold), ("warm", &streaming_warm)] {
            assert_eq!(streaming.total_messages, 3, "{phase} streaming messages");
            assert_eq!(streaming.total_input, 150, "{phase} streaming input");
            assert_eq!(streaming.total_output, 15, "{phase} streaming output");
        }

        let counted = with_isolated_tokscale_cache(count_cache.path(), || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(home),
                use_env_roots: false,
                clients: Some(clients),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap()
        });
        assert_eq!(counted.counts.get(ClientId::Codex), 3);
        assert_eq!(counted.messages.len(), 3);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            150,
        );
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.output)
                .sum::<i64>(),
            15,
        );
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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_keeps_user_fork_own_turn() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_deduplicates_parent_replay_across_forks() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_codex_counts_deduplicated_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_reparses_from_zero_when_incremental_prefix_is_stale() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let codex_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
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
            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
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
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_keeps_untimestamped_rows_in_sync_after_append() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let codex_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
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
            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_matches_cold_parse_after_malformed_json_append() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let codex_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
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

            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_exact_hit_codex_cache_repairs_fallback_timestamps_without_incremental_state() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
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
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_repairs_fallback_timestamps_after_source_mtime_change() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
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

            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
            assert_ne!(warm_messages[0].timestamp, initial_messages[0].timestamp);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_full_log_parse_preserves_valid_messages_before_invalid_line_error() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
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
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_does_not_persist_unknown_before_later_turn_context() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
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

            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(resumed_messages, fresh_messages);
            assert_eq!(resumed_messages.len(), 1);
            assert_eq!(resumed_messages[0].model_id, "gpt-5.5");

            drop(_fresh_env);
            assert!(message_cache::SourceMessageCache::load()
                .get(&path)
                .is_some());
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_skips_non_newline_terminated_resume_prefix() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
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

            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
            assert_eq!(warm_messages.len(), 2);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_does_not_reuse_priced_cost_without_pricing_service() {
        let temp_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", temp_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", temp_home.path().as_os_str()),
        ]);
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
        assert_eq!(msg.cost_source, CostSource::Estimated);
    }

    #[test]
    fn test_apply_pricing_if_available_preserves_provider_reported_cost() {
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
            "opencode",
            "gpt-4o",
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
            0.42,
        );
        msg.mark_provider_reported_cost();

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.42);
        assert_eq!(msg.cost_source, CostSource::ProviderReported);
    }

    #[test]
    #[serial_test::serial]
    fn test_cost_provenance_matches_materialized_and_streaming_lanes() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        let opencode_data_dir = source_home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&opencode_data_dir).unwrap();
        let opencode_db =
            rusqlite::Connection::open(opencode_data_dir.join("opencode.db")).unwrap();
        opencode_db.execute_batch("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);").unwrap();
        let sqlite_rows = [
            (
                "sqlite-a",
                r#"{"id":"json-authoritative","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.0,"tokens":{"input":20,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            ),
            (
                "sqlite-b",
                r#"{"id":"sqlite-authoritative","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.06,"tokens":{"input":20,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011201000}}"#,
            ),
            (
                "sqlite-c",
                r#"{"id":"both-authoritative","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.07,"tokens":{"input":20,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011202000}}"#,
            ),
            (
                "sqlite-d",
                r#"{"id":"both-estimated","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.0,"tokens":{"input":40,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011203000}}"#,
            ),
        ];
        for (row_id, data) in sqlite_rows {
            opencode_db
                .execute(
                    "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![row_id, "oc-session", data],
                )
                .unwrap();
        }
        drop(opencode_db);

        let opencode_dir = opencode_data_dir.join("storage/message/project-1");
        std::fs::create_dir_all(&opencode_dir).unwrap();
        let json_rows = [
            (
                "a-json-authoritative.json",
                r#"{"id":"json-authoritative","sessionID":"oc-session","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.05,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            ),
            (
                "b-json-estimated.json",
                r#"{"id":"sqlite-authoritative","sessionID":"oc-session","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011201000}}"#,
            ),
            (
                "c-json-authoritative.json",
                r#"{"id":"both-authoritative","sessionID":"oc-session","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.08,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011202000}}"#,
            ),
            (
                "d-json-estimated.json",
                r#"{"id":"both-estimated","sessionID":"oc-session","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011203000}}"#,
            ),
        ];
        for (name, data) in json_rows {
            std::fs::write(opencode_dir.join(name), data).unwrap();
        }

        let mut litellm = HashMap::new();
        litellm.insert(
            "openai/gpt-4o".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let clients = vec!["opencode".to_string()];
        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &clients,
            Some(&pricing),
            false,
            &scanner::ScannerSettings::default(),
        );
        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            Some(&pricing),
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );

        let summarize = |messages: Vec<UnifiedMessage>| {
            let mut rows: Vec<(String, f64, CostSource)> = messages
                .into_iter()
                .map(|message| {
                    (
                        message.dedup_key.unwrap_or_default(),
                        message.cost,
                        message.cost_source,
                    )
                })
                .collect();
            rows.sort_by(|left, right| left.0.cmp(&right.0));
            rows
        };
        let materialized = summarize(materialized);
        let streamed = summarize(streamed);
        assert_eq!(materialized, streamed);
        assert_eq!(
            materialized,
            vec![
                (
                    "both-authoritative".to_string(),
                    0.07,
                    CostSource::ProviderReported
                ),
                ("both-estimated".to_string(), 0.5, CostSource::Estimated),
                (
                    "json-authoritative".to_string(),
                    0.05,
                    CostSource::ProviderReported
                ),
                (
                    "sqlite-authoritative".to_string(),
                    0.06,
                    CostSource::ProviderReported
                ),
            ]
        );
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
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_keeps_gateway_message_under_synthetic_filter() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), temp_dir.path());
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
        // opencode canonicalizes the raw "fireworks" gateway id to "fireworks_ai" (#760).
        assert_eq!(parsed.messages[0].provider_id, "fireworks_ai");
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_fireworks_provider_kept_under_synthetic_only_filter() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), temp_dir.path());
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
        // opencode canonicalizes the raw "fireworks" gateway id to "fireworks_ai" (#760).
        assert_eq!(messages[0].provider_id, "fireworks_ai");
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
        // opencode canonicalizes the raw "fireworks" gateway id to "fireworks_ai" (#760).
        assert_eq!(parsed.messages[0].provider_id, "fireworks_ai");
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
        let profile_dir = temp_dir.path().join("external-hermes/director_planning");
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
    #[serial_test::serial]
    fn test_auto_discovered_hermes_profile_reaches_all_consumers() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let count_cache = tempfile::TempDir::new().unwrap();
        let profile_dir = source_home.path().join(".hermes/profiles/research");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let profile_db = profile_dir.join("state.db");
        let conn = create_hermes_sqlite_db(&profile_db);
        insert_hermes_session(
            &conn,
            "hermes-auto-profile",
            "claude-sonnet-4",
            2,
            100,
            25,
            0.07,
        );
        drop(conn);

        let home = source_home.path().to_str().unwrap().to_string();
        let clients = vec!["hermes".to_string()];
        let materialized = with_isolated_tokscale_cache(materialized_cache.path(), || {
            parse_all_messages_with_pricing_with_env_strategy(
                &home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
            )
        });
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].session_id, "hermes-auto-profile");
        assert_eq!(materialized[0].tokens.input, 100);
        assert_eq!(materialized[0].tokens.output, 25);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let streaming = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: Some(clients.clone()),
                    ..Default::default()
                }))
                .unwrap()
        });
        assert_eq!(streaming.total_messages, 2);
        assert_eq!(streaming.total_input, 100);
        assert_eq!(streaming.total_output, 25);

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        let counted = with_isolated_tokscale_cache(count_cache.path(), || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(home),
                use_env_roots: false,
                clients: Some(clients),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: Some(future_ms),
            })
            .unwrap()
        });
        assert_eq!(counted.counts.get(ClientId::Hermes), 2);
        assert_eq!(counted.messages.len(), 1);
        assert_eq!(counted.messages[0].session_id, "hermes-auto-profile");
        assert_eq!(counted.messages[0].input, 100);
        assert_eq!(counted.messages[0].output, 25);
    }

    #[test]
    fn test_modified_after_never_prunes_hermes_dbs_from_extra_scan_paths() {
        // SQLite WAL writes may leave the main db file's mtime untouched, so
        // `modified_after` must not prune Hermes/Zed dbs even when they come
        // from user scan roots (the `files` lanes) rather than the default
        // single-db path. A threshold in the future would prune any mtime.
        let temp_dir = tempfile::TempDir::new().unwrap();
        let profile_dir = temp_dir.path().join("external-hermes/director_planning");
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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
    }

    #[test]
    #[serial_test::serial]
    fn m21_sources_keep_materialized_streaming_count_and_report_parity() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let report_cache = tempfile::TempDir::new().unwrap();
        let clients = vec![
            "kimi".to_string(),
            "junie".to_string(),
            "opencodereview".to_string(),
        ];

        let legacy_kimi = source_home
            .path()
            .join(".kimi/sessions/group/legacy-session/wire.jsonl");
        std::fs::create_dir_all(legacy_kimi.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy_kimi,
            r#"{"timestamp":1770983410.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":7,"output":3},"message_id":"legacy-1"}}}"#,
        )
        .unwrap();

        let kimi_code = source_home
            .path()
            .join(".kimi-code/sessions/workspace/code-session/agents/main/wire.jsonl");
        std::fs::create_dir_all(kimi_code.parent().unwrap()).unwrap();
        let code_a = r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":100,"output":50,"inputCacheRead":10,"inputCacheCreation":0},"usageScope":"turn","time":1770983420000,"turnId":"turn-a"}"#;
        let code_b = r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":100,"output":50,"inputCacheRead":10,"inputCacheCreation":0},"usageScope":"turn","time":1770983420001,"turnId":"turn-b"}"#;
        std::fs::write(&kimi_code, format!("{code_a}\n{code_a}\n{code_b}\n")).unwrap();
        let kimi_code_replay = source_home
            .path()
            .join(".kimi-code/sessions/workspace/code-session/agents/reviewer/wire.jsonl");
        std::fs::create_dir_all(kimi_code_replay.parent().unwrap()).unwrap();
        std::fs::write(&kimi_code_replay, format!("{code_a}\n")).unwrap();

        let junie = source_home
            .path()
            .join(".junie/sessions/session-260213-120000/events.jsonl");
        std::fs::create_dir_all(junie.parent().unwrap()).unwrap();
        let write_junie = |input: i64, cost: f64| {
            let usage = serde_json::json!({
                "timestampMs": 1_770_983_430_000_i64,
                "event": {
                    "agentEvent": {
                        "kind": "LlmResponseMetadataEvent",
                        "agent": { "name": "reviewer" },
                        "modelUsage": [{
                            "model": "gpt-5",
                            "provider": "openai",
                            "inputTokens": input,
                            "outputTokens": 50,
                            "time": 2_000,
                            "cost": cost
                        }]
                    }
                }
            });
            std::fs::write(
                &junie,
                format!("{}\n{usage}\n", r#"{"kind":"UserPromptEvent"}"#),
            )
            .unwrap();
        };
        write_junie(100, 0.125);

        let review = source_home
            .path()
            .join(".opencodereview/sessions/repo/review-session.jsonl");
        std::fs::create_dir_all(review.parent().unwrap()).unwrap();
        let review_contents = concat!(
            r#"{"type":"session_start","cwd":"/work/repo"}"#,
            "\n",
            r#"{"type":"llm_response","timestamp":"2026-02-13T12:00:40Z","model":"gpt-4o","duration_ms":1500,"usage":{"prompt_tokens":20,"completion_tokens":5,"cache_read_tokens":1,"cache_write_tokens":2}}"#,
            "\n"
        );
        std::fs::write(&review, review_contents).unwrap();

        let home = source_home.path().to_string_lossy().into_owned();
        let run_materialized = || {
            with_isolated_tokscale_cache(materialized_cache.path(), || {
                let mut messages = parse_all_messages_with_pricing_with_env_strategy(
                    &home,
                    &clients,
                    None,
                    false,
                    &scanner::ScannerSettings::default(),
                );
                messages.sort_by(|left, right| {
                    (&left.client, &left.session_id, &left.dedup_key).cmp(&(
                        &right.client,
                        &right.session_id,
                        &right.dedup_key,
                    ))
                });
                messages
            })
        };
        let run_streaming = || {
            with_isolated_tokscale_cache(streaming_cache.path(), || {
                let mut messages = Vec::new();
                scan_messages_streaming(
                    &home,
                    &clients,
                    None,
                    false,
                    &scanner::ScannerSettings::default(),
                    &|_| true,
                    &mut |message| messages.push(message.clone()),
                );
                messages.sort_by(|left, right| {
                    (&left.client, &left.session_id, &left.dedup_key).cmp(&(
                        &right.client,
                        &right.session_id,
                        &right.dedup_key,
                    ))
                });
                messages
            })
        };

        let cold_materialized = run_materialized();
        let cold_streaming = run_streaming();
        assert_eq!(cold_materialized, cold_streaming);
        assert_eq!(cold_materialized.len(), 5);
        assert_eq!(
            cold_materialized
                .iter()
                .filter(|message| message.client == "kimi")
                .count(),
            3
        );
        let junie_message = cold_materialized
            .iter()
            .find(|message| message.client == "junie")
            .unwrap();
        assert_eq!(junie_message.cost_source, CostSource::ProviderReported);
        assert!((junie_message.cost - 0.125).abs() < 1e-9);
        assert!(junie_message.is_turn_start);
        assert_eq!(junie_message.duration_ms, Some(2_000));
        let review_message = cold_materialized
            .iter()
            .find(|message| message.client == "opencodereview")
            .unwrap();
        assert_eq!(review_message.duration_ms, Some(1_500));
        assert_eq!(review_message.workspace_label.as_deref(), Some("repo"));
        assert_eq!(run_materialized(), cold_materialized);
        assert_eq!(run_streaming(), cold_streaming);

        write_junie(200, 0.25);
        let rewritten_materialized = run_materialized();
        let rewritten_streaming = run_streaming();
        assert_eq!(rewritten_materialized, rewritten_streaming);
        let rewritten_junie = rewritten_materialized
            .iter()
            .find(|message| message.client == "junie")
            .unwrap();
        assert_eq!(rewritten_junie.tokens.input, 200);
        assert!((rewritten_junie.cost - 0.25).abs() < 1e-9);

        std::fs::remove_file(&review).unwrap();
        assert_eq!(run_materialized().len(), 4);
        assert_eq!(run_streaming().len(), 4);
        std::fs::write(&review, review_contents).unwrap();

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(home.clone()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::Kimi), 3);
        assert_eq!(counted.counts.get(ClientId::Junie), 1);
        assert_eq!(counted.counts.get(ClientId::OpenCodeReview), 1);
        assert_eq!(counted.messages.len(), 5);

        with_isolated_tokscale_cache(report_cache.path(), || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let options = ReportOptions {
                home_dir: Some(home),
                use_env_roots: false,
                clients: Some(clients),
                ..Default::default()
            };
            let model = runtime.block_on(get_model_report(options.clone())).unwrap();
            let monthly = runtime
                .block_on(get_monthly_report(options.clone()))
                .unwrap();
            let hourly = runtime
                .block_on(get_hourly_report(options.clone()))
                .unwrap();
            let agents = runtime.block_on(get_agents_report(options)).unwrap();
            assert_eq!(model.total_messages, 5);
            assert_eq!(model.total_input, 427);
            assert_eq!(model.total_output, 158);
            assert_eq!(model.total_cache_read, 21);
            assert_eq!(model.total_cache_write, 2);
            assert!((model.total_cost - 0.25).abs() < 1e-9);
            assert_eq!(
                monthly
                    .entries
                    .iter()
                    .map(|entry| entry.message_count)
                    .sum::<i32>(),
                5
            );
            assert_eq!(
                hourly
                    .entries
                    .iter()
                    .map(|entry| entry.message_count)
                    .sum::<i32>(),
                5
            );
            assert_eq!(agents.total_messages, 5);
            assert_eq!(
                agents.entries.iter().map(|entry| entry.input).sum::<i64>(),
                427
            );
        });
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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let micode_dir = source_home.path().join(".local/share/mimocode");
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
    }

    // #742 Part 2: the micode lane is cost-guarded so MiMo Code's authoritative
    // embedded cost is never overwritten by a recomputed tokens*rate when the
    // model resolves to a price. reprice_lane_message(.., guard=true) reprices
    // only when the embedded cost is absent (<= 0.0); guard=false is the old
    // unconditional behavior that this fix replaces for micode.
    #[test]
    fn test_reprice_lane_message_guards_authoritative_micode_cost() {
        // A pricing service that WOULD recompute a large cost for the MiMo model
        // (1000*0.001 + 500*0.002 = 2.0, versus the embedded 0.05).
        let mut litellm = HashMap::new();
        litellm.insert(
            "mimo-v2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let recomputed = 1000.0 * 0.001 + 500.0 * 0.002; // 2.0

        let make = |embedded_cost: f64| {
            UnifiedMessage::new(
                "micode",
                "mimo-v2.5-pro",
                "mimo",
                "ses",
                1_700_000_000_000,
                TokenBreakdown {
                    input: 1000,
                    output: 500,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                },
                embedded_cost,
            )
        };

        // guard=true + embedded cost present -> authoritative cost survives.
        let mut guarded = make(0.05);
        reprice_lane_message(&mut guarded, Some(&pricing), true);
        assert!(
            (guarded.cost - 0.05).abs() < 1e-9,
            "cost-guarded reprice must keep the embedded 0.05, got {}",
            guarded.cost
        );

        // guard=false (old behavior) -> unconditionally overwritten by the recompute.
        let mut unguarded = make(0.05);
        reprice_lane_message(&mut unguarded, Some(&pricing), false);
        assert!(
            (unguarded.cost - recomputed).abs() < 1e-9,
            "unguarded reprice overwrites the embedded cost with {recomputed}, got {}",
            unguarded.cost
        );

        // guard=true + embedded cost absent (<= 0.0) -> still repriced (fallback).
        let mut absent = make(0.0);
        reprice_lane_message(&mut absent, Some(&pricing), true);
        assert!(
            (absent.cost - recomputed).abs() < 1e-9,
            "a missing embedded cost must still be priced, got {}",
            absent.cost
        );
    }

    fn roo_task_root(home: &Path, client: ClientId) -> PathBuf {
        let relative = match client {
            ClientId::RooCode => ".config/Code/User/globalStorage/rooveterinaryinc.roo-cline/tasks",
            ClientId::KiloCode => ".config/Code/User/globalStorage/kilocode.kilo-code/tasks",
            ClientId::Cline => ".config/Code/User/globalStorage/saoudrizwan.claude-dev/tasks",
            _ => panic!("not a Roo-family client: {client:?}"),
        };
        home.join(relative)
    }

    fn write_roo_history_fixture(history: &Path, model: &str, agent: &str) {
        std::fs::write(
            history,
            format!(
                "<environment_details><model>{model}</model><slug>{agent}</slug></environment_details>"
            ),
        )
        .unwrap();
    }

    fn write_roo_task_fixture(
        home: &Path,
        client: ClientId,
        task_id: &str,
        model: &str,
        agent: &str,
    ) -> (PathBuf, PathBuf) {
        let task_dir = roo_task_root(home, client).join(task_id);
        std::fs::create_dir_all(&task_dir).unwrap();
        let ui_messages = task_dir.join("ui_messages.json");
        std::fs::write(
            &ui_messages,
            r#"[{"type":"say","say":"api_req_started","ts":"2026-06-25T10:00:00Z","text":"{\"cost\":0.125,\"tokensIn\":100,\"tokensOut\":25,\"cacheReads\":10,\"cacheWrites\":5,\"apiProtocol\":\"anthropic\"}"}]"#,
        )
        .unwrap();
        let history = sessions::roocode::history_path_for_ui_messages(&ui_messages);
        write_roo_history_fixture(&history, model, agent);
        (ui_messages, history)
    }

    #[test]
    #[serial_test::serial]
    fn test_roo_family_history_rewrite_refreshes_materialized_and_streaming_caches() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let family = [ClientId::RooCode, ClientId::KiloCode, ClientId::Cline];
        let mut litellm = HashMap::new();
        litellm.insert(
            "old-model".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        litellm.insert(
            "new-model-with-longer-id".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let mut histories = Vec::new();
        for client in family {
            let task_id = format!("{}-task", client.as_str());
            let (_, history) = write_roo_task_fixture(
                source_home.path(),
                client,
                &task_id,
                "old-model",
                "old-agent",
            );
            histories.push(history);
        }

        let home = source_home.path().to_str().unwrap().to_string();
        let clients: Vec<String> = family
            .into_iter()
            .map(|client| client.as_str().to_string())
            .collect();
        let run_materialized = || {
            with_isolated_tokscale_cache(materialized_cache.path(), || {
                parse_all_messages_with_pricing_with_env_strategy(
                    &home,
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner::ScannerSettings::default(),
                )
            })
        };
        let run_streaming = || {
            with_isolated_tokscale_cache(streaming_cache.path(), || {
                let mut messages = Vec::new();
                scan_messages_streaming(
                    &home,
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner::ScannerSettings::default(),
                    &|_: &UnifiedMessage| true,
                    &mut |message: &UnifiedMessage| messages.push(message.clone()),
                );
                messages
            })
        };

        let materialized_before = run_materialized();
        let streaming_before = run_streaming();
        for (lane, messages) in [
            ("materialized", &materialized_before),
            ("streaming", &streaming_before),
        ] {
            assert_eq!(messages.len(), 3, "{lane} seed message count");
            assert!(messages.iter().all(|message| {
                message.model_id == "old-model" && message.agent.as_deref() == Some("old-agent")
            }));
        }

        for history in &histories {
            write_roo_history_fixture(history, "new-model-with-longer-id", "new-agent");
        }

        let materialized_after = run_materialized();
        let streaming_after = run_streaming();
        assert_eq!(materialized_after.len(), 3);
        assert_eq!(streaming_after.len(), 3);
        for client in family {
            let client_name = client.as_str();
            let materialized = materialized_after
                .iter()
                .find(|message| message.client == client_name)
                .unwrap();
            let streaming = streaming_after
                .iter()
                .find(|message| message.client == client_name)
                .unwrap();
            let materialized_before = materialized_before
                .iter()
                .find(|message| message.client == client_name)
                .unwrap();

            assert_eq!(materialized.session_id, format!("{client_name}-task"));
            assert_eq!(materialized.model_id, "new-model-with-longer-id");
            assert_eq!(materialized.agent.as_deref(), Some("new-agent"));
            assert_eq!(materialized.tokens.input, 100);
            assert_eq!(materialized.tokens.output, 25);
            assert_eq!(materialized.tokens.cache_read, 10);
            assert_eq!(materialized.tokens.cache_write, 5);
            assert!(
                materialized.cost > materialized_before.cost,
                "{client_name} history model rewrite must refresh derived pricing"
            );

            assert_eq!(streaming.client, materialized.client);
            assert_eq!(streaming.session_id, materialized.session_id);
            assert_eq!(streaming.model_id, materialized.model_id);
            assert_eq!(streaming.agent, materialized.agent);
            assert_eq!(streaming.tokens.input, materialized.tokens.input);
            assert_eq!(streaming.tokens.output, materialized.tokens.output);
            assert_eq!(streaming.tokens.cache_read, materialized.tokens.cache_read);
            assert_eq!(
                streaming.tokens.cache_write,
                materialized.tokens.cache_write
            );
            assert!((streaming.cost - materialized.cost).abs() < 1e-9);
        }
    }

    fn latest_source_mtime_probes_roo_history(client: ClientId) {
        let source_home = tempfile::TempDir::new().unwrap();
        let (ui_messages, history) = write_roo_task_fixture(
            source_home.path(),
            client,
            "mtime-task",
            "test-model",
            "test-agent",
        );
        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for (path, time) in [(&ui_messages, stale_time), (&history, fresh_time)] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let Ok(()) = file.set_modified(time) else {
                return;
            };
        }

        let token = latest_source_mtime_ms(&LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec![client.as_str().to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(
            token,
            1_700_086_400_000,
            "{} change token must include the history sibling",
            client.as_str()
        );
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_roocode_history() {
        latest_source_mtime_probes_roo_history(ClientId::RooCode);
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_kilocode_history() {
        latest_source_mtime_probes_roo_history(ClientId::KiloCode);
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_cline_history() {
        latest_source_mtime_probes_roo_history(ClientId::Cline);
    }

    #[test]
    #[serial_test::serial]
    fn test_modified_after_keeps_roo_family_with_fresh_history() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let family = [ClientId::RooCode, ClientId::KiloCode, ClientId::Cline];
        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);

        for client in family {
            let stale_id = format!("{}-stale", client.as_str());
            let active_id = format!("{}-active", client.as_str());
            let (stale_ui, stale_history) = write_roo_task_fixture(
                source_home.path(),
                client,
                &stale_id,
                "stale-model",
                "stale-agent",
            );
            let (active_ui, active_history) = write_roo_task_fixture(
                source_home.path(),
                client,
                &active_id,
                "active-model",
                "active-agent",
            );
            for path in [&stale_ui, &stale_history, &active_ui, &active_history] {
                let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                let Ok(()) = file.set_modified(stale_time) else {
                    return;
                };
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&active_history)
                .unwrap();
            let Ok(()) = file.set_modified(fresh_time) else {
                return;
            };
        }

        let clients: Vec<String> = family
            .into_iter()
            .map(|client| client.as_str().to_string())
            .collect();
        let parsed = with_isolated_tokscale_cache(cache_home.path(), || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(clients),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: Some(1_700_043_200_000),
            })
            .unwrap()
        });

        assert_eq!(parsed.messages.len(), 3);
        for client in family {
            assert_eq!(parsed.counts.get(client), 1);
            assert!(parsed.messages.iter().any(|message| {
                message.client == client.as_str()
                    && message.session_id == format!("{}-active", client.as_str())
                    && message.model_id == "active-model"
            }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_modified_after_roo_history_stat_failure_keeps_source() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let ui_messages = temp_dir.path().join("ui_messages.json");
        std::fs::write(&ui_messages, b"[]").unwrap();
        let history = sessions::roocode::history_path_for_ui_messages(&ui_messages);
        std::os::unix::fs::symlink("api_conversation_history.json", &history).unwrap();

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::RooCode)
            .push(ui_messages.clone());
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert_eq!(
            scan_result.get(ClientId::RooCode),
            std::slice::from_ref(&ui_messages),
            "an unreadable history sibling must fail open during pruning"
        );
    }

    struct M13RelatedFixture {
        droid_source: PathBuf,
        droid_related: PathBuf,
        kimi_source: PathBuf,
        kimi_related: PathBuf,
        kiro_source: PathBuf,
        kiro_related: PathBuf,
    }

    fn write_m13_primary_fixtures(home: &Path) -> M13RelatedFixture {
        let droid_dir = home.join(".factory/sessions");
        std::fs::create_dir_all(&droid_dir).unwrap();
        let droid_source = droid_dir.join("droid-session.settings.json");
        std::fs::write(
            &droid_source,
            r#"{"providerLock":"anthropic","providerLockTimestamp":"2026-01-01T00:00:00Z","tokenUsage":{"inputTokens":100,"outputTokens":20,"cacheCreationTokens":5,"cacheReadTokens":10,"thinkingTokens":2}}"#,
        )
        .unwrap();
        let droid_related = droid_dir.join("droid-session.jsonl");

        let kimi_session_dir = home.join(".kimi/sessions/group-1/kimi-session");
        std::fs::create_dir_all(&kimi_session_dir).unwrap();
        let kimi_source = kimi_session_dir.join("wire.jsonl");
        std::fs::write(
            &kimi_source,
            r#"{"timestamp":1767225600.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":50,"output":5,"input_cache_read":3,"input_cache_creation":2},"message_id":"kimi-turn"}}}"#,
        )
        .unwrap();
        let kimi_related = home.join(".kimi/config.json");

        let kiro_dir = home.join(".kiro/sessions/cli");
        std::fs::create_dir_all(&kiro_dir).unwrap();
        let kiro_source = kiro_dir.join("kiro-session.json");
        std::fs::write(
            &kiro_source,
            r#"{"session_id":"kiro-session","cwd":"/tmp/m13-project","session_state":{"rts_model_state":{"model_info":{"model_id":"kiro-model","context_window_tokens":1000}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":0,"output_token_count":0,"end_timestamp":1767225601,"total_request_count":1,"message_ids":["kiro-turn"],"context_usage_percentage":10.0}]}}}"#,
        )
        .unwrap();
        let kiro_related = kiro_source.with_extension("jsonl");

        M13RelatedFixture {
            droid_source,
            droid_related,
            kimi_source,
            kimi_related,
            kiro_source,
            kiro_related,
        }
    }

    fn write_m13_related_fixtures(paths: &M13RelatedFixture) {
        std::fs::write(
            &paths.droid_related,
            r#"{"message":"Model: Claude-Opus-4.5-[Anthropic]"}"#,
        )
        .unwrap();
        std::fs::write(&paths.kimi_related, r#"{"model":"kimi-new-model"}"#).unwrap();
        std::fs::write(
            &paths.kiro_related,
            concat!(
                r#"{"version":"v1","kind":"Prompt","data":{"message_id":"kiro-prompt","content":[{"kind":"text","data":"prompt body"}],"meta":{"timestamp":1767225600.0}}}"#,
                "\n",
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"kiro-turn","content":[{"kind":"text","data":"abcdefghijklmnop"}]}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    fn set_m13_fixture_mtimes(
        paths: &M13RelatedFixture,
        primary_time: std::time::SystemTime,
        related_time: std::time::SystemTime,
    ) -> bool {
        for path in [&paths.droid_source, &paths.kimi_source, &paths.kiro_source] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            if file.set_modified(primary_time).is_err() {
                return false;
            }
        }
        for path in [
            &paths.droid_related,
            &paths.kimi_related,
            &paths.kiro_related,
        ] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            if file.set_modified(related_time).is_err() {
                return false;
            }
        }
        true
    }

    #[test]
    #[serial_test::serial]
    fn test_m13_related_sources_refresh_materialized_and_streaming_caches() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        let clients = vec!["droid".to_string(), "kimi".to_string(), "kiro".to_string()];

        let mut litellm = HashMap::new();
        for (model, input_rate, output_rate) in [
            ("claude-unknown", 0.001, 0.002),
            ("claude-opus-4-5", 0.01, 0.02),
            ("kimi-for-coding", 0.001, 0.002),
            ("kimi-new-model", 0.01, 0.02),
            ("kiro-model", 0.003, 0.004),
        ] {
            litellm.insert(
                model.to_string(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(input_rate),
                    output_cost_per_token: Some(output_rate),
                    ..Default::default()
                },
            );
        }
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let home = source_home.path().to_str().unwrap().to_string();
        let run_materialized = || {
            with_isolated_tokscale_cache(materialized_cache.path(), || {
                parse_all_messages_with_pricing_with_env_strategy(
                    &home,
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner::ScannerSettings::default(),
                )
            })
        };
        let run_streaming = || {
            with_isolated_tokscale_cache(streaming_cache.path(), || {
                let mut messages = Vec::new();
                scan_messages_streaming(
                    &home,
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner::ScannerSettings::default(),
                    &|_: &UnifiedMessage| true,
                    &mut |message: &UnifiedMessage| messages.push(message.clone()),
                );
                messages
            })
        };

        let materialized_before = run_materialized();
        let streaming_before = run_streaming();
        assert_eq!(materialized_before.len(), 3);
        assert_eq!(streaming_before.len(), 3);
        for (client, model) in [
            ("droid", "claude-unknown"),
            ("kimi", "kimi-for-coding"),
            ("kiro", "kiro-model"),
        ] {
            assert_eq!(
                materialized_before
                    .iter()
                    .find(|message| message.client == client)
                    .unwrap()
                    .model_id,
                model
            );
            assert_eq!(
                streaming_before
                    .iter()
                    .find(|message| message.client == client)
                    .unwrap()
                    .model_id,
                model
            );
        }

        write_m13_related_fixtures(&paths);

        let materialized_after = run_materialized();
        let streaming_after = run_streaming();
        assert_eq!(materialized_after.len(), 3);
        assert_eq!(streaming_after.len(), 3);
        for client in ["droid", "kimi", "kiro"] {
            let before = materialized_before
                .iter()
                .find(|message| message.client == client)
                .unwrap();
            let materialized = materialized_after
                .iter()
                .find(|message| message.client == client)
                .unwrap();
            let streaming = streaming_after
                .iter()
                .find(|message| message.client == client)
                .unwrap();

            let expected_model = match client {
                "droid" => "claude-opus-4-5",
                "kimi" => "kimi-new-model",
                "kiro" => "kiro-model",
                _ => unreachable!(),
            };
            assert_eq!(materialized.model_id, expected_model);
            assert!(
                materialized.cost > before.cost,
                "{client} related-source creation must refresh derived cost"
            );
            if client == "kiro" {
                assert_eq!(before.tokens.output, 0);
                assert_eq!(materialized.tokens.output, 4);
                assert_eq!(materialized.timestamp, 1_767_225_600_000);
                assert_eq!(materialized.duration_ms, Some(1_000));
            }

            assert_eq!(streaming.client, materialized.client);
            assert_eq!(streaming.session_id, materialized.session_id);
            assert_eq!(streaming.model_id, materialized.model_id);
            assert_eq!(streaming.provider_id, materialized.provider_id);
            assert_eq!(streaming.workspace_key, materialized.workspace_key);
            assert_eq!(streaming.workspace_label, materialized.workspace_label);
            assert_eq!(streaming.timestamp, materialized.timestamp);
            assert_eq!(streaming.duration_ms, materialized.duration_ms);
            assert_eq!(streaming.message_count, materialized.message_count);
            assert_eq!(streaming.tokens.input, materialized.tokens.input);
            assert_eq!(streaming.tokens.output, materialized.tokens.output);
            assert_eq!(streaming.tokens.cache_read, materialized.tokens.cache_read);
            assert_eq!(
                streaming.tokens.cache_write,
                materialized.tokens.cache_write
            );
            assert_eq!(streaming.tokens.reasoning, materialized.tokens.reasoning);
            assert!((streaming.cost - materialized.cost).abs() < 1e-9);
        }
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_m13_related_sources() {
        let source_home = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        write_m13_related_fixtures(&paths);
        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        if !set_m13_fixture_mtimes(&paths, stale_time, fresh_time) {
            return;
        }

        for client in [ClientId::Droid, ClientId::Kimi, ClientId::Kiro] {
            let token = latest_source_mtime_ms(&LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec![client.as_str().to_string()]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap();
            assert_eq!(
                token,
                1_700_086_400_000,
                "{} change token must include its parser dependency",
                client.as_str()
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_modified_after_keeps_m13_sources_with_fresh_dependencies() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        write_m13_related_fixtures(&paths);
        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        if !set_m13_fixture_mtimes(&paths, stale_time, fresh_time) {
            return;
        }

        let parsed = with_isolated_tokscale_cache(cache_home.path(), || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec![
                    "droid".to_string(),
                    "kimi".to_string(),
                    "kiro".to_string(),
                ]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: Some(1_700_043_200_000),
            })
            .unwrap()
        });

        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.counts.get(ClientId::Droid), 1);
        assert_eq!(parsed.counts.get(ClientId::Kimi), 1);
        assert_eq!(parsed.counts.get(ClientId::Kiro), 1);
        assert!(parsed
            .messages
            .iter()
            .any(|message| { message.client == "droid" && message.model_id == "claude-opus-4-5" }));
        assert!(parsed
            .messages
            .iter()
            .any(|message| { message.client == "kimi" && message.model_id == "kimi-new-model" }));
        assert!(parsed.messages.iter().any(|message| {
            message.client == "kiro" && message.output == 4 && message.duration_ms == Some(1_000)
        }));
    }

    #[test]
    fn test_modified_after_prunes_stale_m13_sources_without_dependencies() {
        let source_home = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Droid)
            .push(paths.droid_source);
        scan_result.get_mut(ClientId::Kimi).push(paths.kimi_source);
        scan_result.get_mut(ClientId::Kiro).push(paths.kiro_source);

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert!(scan_result.get(ClientId::Droid).is_empty());
        assert!(scan_result.get(ClientId::Kimi).is_empty());
        assert!(scan_result.get(ClientId::Kiro).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_modified_after_m13_dependency_stat_failures_keep_sources() {
        let source_home = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        for related in [
            &paths.droid_related,
            &paths.kimi_related,
            &paths.kiro_related,
        ] {
            std::os::unix::fs::symlink(related.file_name().unwrap(), related).unwrap();
        }

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Droid)
            .push(paths.droid_source.clone());
        scan_result
            .get_mut(ClientId::Kimi)
            .push(paths.kimi_source.clone());
        scan_result
            .get_mut(ClientId::Kiro)
            .push(paths.kiro_source.clone());
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert_eq!(
            scan_result.get(ClientId::Droid),
            std::slice::from_ref(&paths.droid_source)
        );
        assert_eq!(
            scan_result.get(ClientId::Kimi),
            std::slice::from_ref(&paths.kimi_source)
        );
        assert_eq!(
            scan_result.get(ClientId::Kiro),
            std::slice::from_ref(&paths.kiro_source)
        );
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

    // gjc (`$GJC_CODING_AGENT_DIR/sessions/*.jsonl`) must be discovered via the
    // EnvVar fallback root (`.gjc/agent`) + `*.jsonl` glob and flow through the
    // streaming lane, keeping its authoritative embedded `usage.cost.total`
    // (A1). With pricing absent the guard's reprice branch is a no-op; the
    // materialized path mirrors upstream's proven reprice-when-absent guard.
    #[test]
    #[serial_test::serial]
    fn test_streaming_gjc_flows_with_authoritative_cost() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let gjc_dir = source_home.path().join(".gjc/agent/sessions");
            std::fs::create_dir_all(&gjc_dir).unwrap();
            std::fs::write(
                gjc_dir.join("test.jsonl"),
                "{\"type\":\"session\",\"id\":\"gjc_ses_001\",\"cwd\":\"/work/pi\"}\n{\"type\":\"message\",\"id\":\"msg_001\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-sonnet-4\",\"provider\":\"anthropic\",\"timestamp\":1767225601000,\"usage\":{\"input\":100,\"output\":50,\"cost\":{\"total\":0.3}}}}\n",
            )
            .unwrap();

            let mut cost_sum = 0.0f64;
            let mut count = 0usize;
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["gjc".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| {
                    cost_sum += m.cost;
                    count += 1;
                },
            );

            assert_eq!(count, 1, "the gjc assistant message must flow through the streaming lane");
            assert!(
                (cost_sum - 0.3).abs() < 1e-9,
                "authoritative gjc cost must reach the sink (got {cost_sum})"
            );
        }
    }

    // jcode's `session_*.json` snapshot is a file-lane source whose sibling
    // `.journal.jsonl` is appended between snapshot rewrites without touching
    // the snapshot mtime, so it must be exempt from mtime pruning like the WAL
    // db lanes — otherwise an active session with a stale snapshot is dropped
    // and its recent journal turns vanish from the live tail.
    #[test]
    fn test_modified_after_never_prunes_jcode_sessions() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let jcode_snapshot = temp_dir.path().join("session_x.json");
        std::fs::File::create(&jcode_snapshot).unwrap();
        let claude_log = temp_dir.path().join("session.jsonl");
        std::fs::File::create(&claude_log).unwrap();

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Jcode)
            .push(jcode_snapshot.clone());
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
            scan_result.get(ClientId::Jcode),
            std::slice::from_ref(&jcode_snapshot),
            "jcode snapshot (its journal sibling can change without it) must survive mtime pruning"
        );
        assert!(
            scan_result.get(ClientId::Claude).is_empty(),
            "a plain-file client's stale log is still pruned"
        );
    }

    #[test]
    fn test_modified_after_prunes_grok_by_updates_or_signals_mtime() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let stale_dir = temp_dir.path().join("stale");
        let active_dir = temp_dir.path().join("active");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::create_dir_all(&active_dir).unwrap();

        let stale_updates = stale_dir.join("updates.jsonl");
        let stale_signals = stale_dir.join("signals.json");
        let active_updates = active_dir.join("updates.jsonl");
        let active_signals = active_dir.join("signals.json");
        for path in [&stale_updates, &stale_signals, &active_updates, &active_signals] {
            std::fs::File::create(path).unwrap();
        }

        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let active_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for path in [&stale_updates, &stale_signals, &active_updates] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let Ok(()) = file.set_modified(stale_time) else {
                return;
            };
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&active_signals)
            .unwrap();
        let Ok(()) = file.set_modified(active_time) else {
            return;
        };

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Grok)
            .extend([stale_updates.clone(), active_updates.clone()]);

        crate::prune_scan_result_by_mtime(&mut scan_result, 1_700_043_200_000);

        assert_eq!(
            scan_result.get(ClientId::Grok),
            std::slice::from_ref(&active_updates),
            "stale Grok sessions should be pruned while a fresh signals sibling keeps its session"
        );
    }

    #[test]
    fn test_modified_after_retains_grok_authority_sources() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let stale_dir = temp_dir.path().join("stale");
        let active_dir = temp_dir.path().join("active");
        let logs_dir = temp_dir.path().join("logs");
        for dir in [&stale_dir, &active_dir, &logs_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let stale_updates = stale_dir.join("updates.jsonl");
        let active_updates = active_dir.join("updates.jsonl");
        let unified = logs_dir.join("unified.jsonl");
        for path in [&stale_updates, &active_updates, &unified] {
            std::fs::File::create(path).unwrap();
        }

        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let active_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for path in [&stale_updates, &unified] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let Ok(()) = file.set_modified(stale_time) else {
                return;
            };
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&active_updates)
            .unwrap();
        let Ok(()) = file.set_modified(active_time) else {
            return;
        };

        let mut legacy_fresh = scanner::ScanResult::default();
        legacy_fresh.get_mut(ClientId::Grok).extend([
            stale_updates.clone(),
            active_updates.clone(),
            unified.clone(),
        ]);
        crate::prune_scan_result_by_mtime(&mut legacy_fresh, 1_700_043_200_000);
        assert_eq!(
            legacy_fresh.get(ClientId::Grok),
            &[active_updates.clone(), unified.clone()],
            "a stale unified authority source must survive while a legacy source is fresh"
        );

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&unified)
            .unwrap();
        let Ok(()) = file.set_modified(active_time) else {
            return;
        };
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&active_updates)
            .unwrap();
        let Ok(()) = file.set_modified(stale_time) else {
            return;
        };

        let mut unified_fresh = scanner::ScanResult::default();
        unified_fresh.get_mut(ClientId::Grok).extend([
            stale_updates.clone(),
            active_updates.clone(),
            unified.clone(),
        ]);
        crate::prune_scan_result_by_mtime(&mut unified_fresh, 1_700_043_200_000);
        assert_eq!(
            unified_fresh.get(ClientId::Grok),
            &[stale_updates, active_updates, unified],
            "a fresh unified source needs the legacy cohort for workspace attribution"
        );
    }

    // The pruning helper must treat a fresh write to *any* metadata sibling the
    // parser reads (not just signals.json) as source activity — otherwise a
    // summary.json- or events.jsonl-only write (a late-arriving model id) lets an
    // otherwise-stale session be pruned and its fresh model never re-parsed.
    fn prune_grok_keeps_session_with_fresh_sibling(fresh_sibling: &str) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let stale_dir = temp_dir.path().join("stale");
        let active_dir = temp_dir.path().join("active");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::create_dir_all(&active_dir).unwrap();

        let stale_updates = stale_dir.join("updates.jsonl");
        let active_updates = active_dir.join("updates.jsonl");
        let active_fresh = active_dir.join(fresh_sibling);
        // Every session file starts stale; only the one fresh sibling moves.
        let mut all_paths = vec![stale_updates.clone(), active_updates.clone()];
        for name in message_cache::GROK_METADATA_SIBLINGS {
            all_paths.push(stale_dir.join(name));
            all_paths.push(active_dir.join(name));
        }
        for path in &all_paths {
            std::fs::File::create(path).unwrap();
        }

        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let active_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for path in &all_paths {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let Ok(()) = file.set_modified(stale_time) else {
                return;
            };
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&active_fresh)
            .unwrap();
        let Ok(()) = file.set_modified(active_time) else {
            return;
        };

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Grok)
            .extend([stale_updates.clone(), active_updates.clone()]);

        crate::prune_scan_result_by_mtime(&mut scan_result, 1_700_043_200_000);

        assert_eq!(
            scan_result.get(ClientId::Grok),
            std::slice::from_ref(&active_updates),
            "a fresh {fresh_sibling} sibling must keep its otherwise-stale Grok session"
        );
    }

    #[test]
    fn test_modified_after_prunes_grok_keeps_session_with_fresh_summary() {
        prune_grok_keeps_session_with_fresh_sibling("summary.json");
    }

    #[test]
    fn test_modified_after_prunes_grok_keeps_session_with_fresh_events() {
        prune_grok_keeps_session_with_fresh_sibling("events.jsonl");
    }

    // The live-tail change token must move when Grok rewrites *any* metadata
    // sibling the parser reads, even though updates.jsonl is unchanged; otherwise
    // UsageTail short-circuits and the session stays pinned to its fallback model.
    fn latest_source_mtime_ms_probes_grok_sibling(fresh_sibling: &str) {
        let source_home = tempfile::TempDir::new().unwrap();
        let session_dir = source_home
            .path()
            .join(".grok/sessions/%2Ftmp%2Fproject/session-uuid-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        let updates = session_dir.join("updates.jsonl");
        std::fs::write(&updates, b"{\"totalTokens\":1}\n").unwrap();
        // Every sibling exists and is stale; only the target sibling is newer.
        for name in message_cache::GROK_METADATA_SIBLINGS {
            std::fs::write(session_dir.join(name), b"{}").unwrap();
        }

        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for name in std::iter::once("updates.jsonl").chain(message_cache::GROK_METADATA_SIBLINGS) {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(session_dir.join(name))
                .unwrap();
            let Ok(()) = f.set_modified(stale_time) else {
                return;
            };
        }
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(session_dir.join(fresh_sibling))
            .unwrap();
        let Ok(()) = f.set_modified(fresh_time) else {
            return;
        };

        let options = LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["grok".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        };
        let token = crate::latest_source_mtime_ms(&options).unwrap();

        assert_eq!(
            token, 1_700_086_400_000,
            "the change token must reflect the fresh {fresh_sibling} mtime, not just updates.jsonl"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_latest_source_mtime_ms_probes_grok_summary() {
        latest_source_mtime_ms_probes_grok_sibling("summary.json");
    }

    #[test]
    #[serial_test::serial]
    fn test_latest_source_mtime_ms_probes_grok_events() {
        latest_source_mtime_ms_probes_grok_sibling("events.jsonl");
    }

    #[test]
    #[serial_test::serial]
    fn grok_streaming_cache_hits_refresh_derived_date_before_filtering() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let logs_dir = source_home.path().join(".grok/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let unified = logs_dir.join("unified.jsonl");
        std::fs::write(
            &unified,
            "{\"ts\":\"2023-11-14T22:13:20Z\",\"pid\":7,\"sid\":\"cached\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":10,\"completion_tokens\":2}}\n",
        )
        .unwrap();

        let mut cached_messages = sessions::grok::parse_grok_unified_log_file(&unified);
        assert_eq!(cached_messages.len(), 1);
        let expected_date = cached_messages[0].date.clone();
        cached_messages[0].date = "stale-cached-date".to_string();
        let fingerprint = message_cache::SourceFingerprint::from_grok_path(&unified).unwrap();
        let mut cache = message_cache::SourceMessageCache::load();
        cache.insert(message_cache::CachedSourceEntry::new(
            &unified,
            fingerprint,
            cached_messages,
            Vec::new(),
            None,
        ));
        cache.save_if_dirty();

        let clients = vec!["grok".to_string()];
        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|message| message.date == expected_date,
            &mut |message| streamed.push(message.clone()),
        );

        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].date, expected_date);
    }

    #[test]
    #[serial_test::serial]
    fn grok_materialized_reprices_after_legacy_model_carry_over() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let covered_dir = source_home
            .path()
            .join(".grok/sessions/%2Ftmp%2Fproject/covered");
        std::fs::create_dir_all(&covered_dir).unwrap();
        std::fs::write(
            covered_dir.join("updates.jsonl"),
            concat!(
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"covered\",\"update\":{\"sessionUpdate\":\"user_message_chunk\",\"_meta\":{\"modelId\":\"grok-build\"}},\"_meta\":{\"agentTimestampMs\":1700000000000}}}\n",
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"covered\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\"},\"_meta\":{\"totalTokens\":999,\"agentTimestampMs\":1700000001000}}}\n",
            ),
        )
        .unwrap();
        let logs_dir = source_home.path().join(".grok/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(
            logs_dir.join("unified.jsonl"),
            "{\"ts\":\"2023-11-14T22:13:20Z\",\"pid\":7,\"sid\":\"covered\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":100,\"cached_prompt_tokens\":60,\"completion_tokens\":25,\"reasoning_tokens\":5}}\n",
        )
        .unwrap();

        let mut litellm = HashMap::new();
        litellm.insert(
            "grok-build".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                cache_read_input_token_cost: Some(0.0005),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let clients = vec!["grok".to_string()];
        let scanner_settings = scanner::ScannerSettings::default();
        let materialized = || {
            parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                Some(&pricing),
                false,
                &scanner_settings,
            )
        };
        let streamed = || {
            let mut messages = Vec::new();
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &clients,
                Some(&pricing),
                false,
                &scanner_settings,
                &|_| true,
                &mut |message| messages.push(message.clone()),
            );
            messages
        };

        let cold = materialized();
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].model_id, "grok-build");
        let expected_cost = pricing.calculate_cost_with_provider(
            &cold[0].model_id,
            Some(&cold[0].provider_id),
            &cold[0].tokens,
        );
        assert!(expected_cost > 0.0);
        assert_eq!(cold[0].cost, expected_cost);
        assert_eq!(cold[0].cost_source, CostSource::Estimated);
        assert_eq!(streamed(), cold);
        assert_eq!(materialized(), cold, "warm materialized cache must reprice");
        assert_eq!(streamed(), cold, "warm streaming cache must stay in parity");
    }

    #[test]
    #[serial_test::serial]
    fn m17_grok_unified_precedence_tracks_source_lifecycle_across_all_lanes() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let clients = vec!["grok".to_string()];
        let scanner_settings = scanner::ScannerSettings::default();

        let covered_dir = source_home
            .path()
            .join(".grok/sessions/%2Ftmp%2Fproject/covered");
        let legacy_only_dir = source_home
            .path()
            .join(".grok/sessions/%2Ftmp%2Fproject/legacy-only");
        std::fs::create_dir_all(&covered_dir).unwrap();
        std::fs::create_dir_all(&legacy_only_dir).unwrap();
        let covered_updates = covered_dir.join("updates.jsonl");
        let legacy_only_updates = legacy_only_dir.join("updates.jsonl");
        std::fs::write(
            &covered_updates,
            concat!(
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"covered\",\"update\":{\"sessionUpdate\":\"user_message_chunk\",\"_meta\":{\"modelId\":\"grok-build\"}},\"_meta\":{\"agentTimestampMs\":1700000000000}}}\n",
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"covered\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\"},\"_meta\":{\"totalTokens\":999,\"agentTimestampMs\":1700000001000}}}\n",
            ),
        )
        .unwrap();
        std::fs::write(
            &legacy_only_updates,
            concat!(
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"legacy-only\",\"update\":{\"sessionUpdate\":\"user_message_chunk\",\"_meta\":{\"modelId\":\"grok-build\"}},\"_meta\":{\"agentTimestampMs\":1700000010000}}}\n",
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"legacy-only\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\"},\"_meta\":{\"totalTokens\":17,\"agentTimestampMs\":1700000011000}}}\n",
            ),
        )
        .unwrap();
        let legacy_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for path in [&covered_updates, &legacy_only_updates] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            if file.set_modified(legacy_time).is_err() {
                return;
            }
        }

        let local_options = || LocalParseOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner_settings.clone(),
            modified_after: None,
        };
        let report_options = || ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            scanner_settings: scanner_settings.clone(),
            ..Default::default()
        };
        let sorted = |mut messages: Vec<UnifiedMessage>| {
            messages.sort_by(|left, right| left.dedup_key.cmp(&right.dedup_key));
            messages
        };
        let materialized = || {
            sorted(parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner_settings,
            ))
        };
        let streamed = || {
            let mut messages = Vec::new();
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner_settings,
                &|_| true,
                &mut |message| messages.push(message.clone()),
            );
            sorted(messages)
        };

        let legacy = materialized();
        assert_eq!(legacy.len(), 2);
        assert_eq!(
            legacy
                .iter()
                .map(|message| message.tokens.total())
                .sum::<i64>(),
            1_016
        );
        assert_eq!(materialized(), legacy, "legacy cache hits must stay stable");
        let before_unified = latest_source_mtime_ms(&local_options()).unwrap();
        let legacy_change_token = local_source_change_token(&local_options()).unwrap();
        assert_eq!(before_unified, 1_700_086_400_000);

        let logs_dir = source_home.path().join(".grok/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let unified = logs_dir.join("unified.jsonl");
        std::fs::write(
            &unified,
            "{\"ts\":\"2023-11-14T22:13:20Z\",\"pid\":7,\"sid\":\"covered\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":100,\"cached_prompt_tokens\":60,\"completion_tokens\":25,\"reasoning_tokens\":5}}\n",
        )
        .unwrap();
        let unified_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let unified_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&unified)
            .unwrap();
        if unified_file.set_modified(unified_time).is_err() {
            return;
        }
        assert_eq!(
            latest_source_mtime_ms(&local_options()).unwrap(),
            before_unified,
            "the newer legacy mtime deliberately masks unified topology changes"
        );
        let selected_change_token = local_source_change_token(&local_options()).unwrap();
        assert_ne!(selected_change_token, legacy_change_token);

        let selected = materialized();
        assert_eq!(selected.len(), 2);
        assert_eq!(
            selected
                .iter()
                .find(|message| message.session_id == "covered")
                .unwrap()
                .model_id,
            "grok-build"
        );
        let selected_tokens =
            selected
                .iter()
                .fold(TokenBreakdown::default(), |mut total, message| {
                    total.input += message.tokens.input;
                    total.output += message.tokens.output;
                    total.cache_read += message.tokens.cache_read;
                    total.cache_write += message.tokens.cache_write;
                    total.reasoning += message.tokens.reasoning;
                    total
                });
        assert_eq!(
            selected_tokens,
            TokenBreakdown {
                input: 57,
                output: 20,
                cache_read: 60,
                cache_write: 0,
                reasoning: 5,
            }
        );
        assert!(selected.iter().any(|message| {
            message.session_id == "covered"
                && message
                    .dedup_key
                    .as_deref()
                    .is_some_and(|key| key.starts_with("grok-unified:"))
        }));
        assert!(selected
            .iter()
            .any(|message| message.session_id == "legacy-only"));
        assert_eq!(
            materialized(),
            selected,
            "selected cache hits must stay stable"
        );
        assert_eq!(streamed(), selected);

        let counted = parse_local_clients(local_options()).unwrap();
        assert_eq!(counted.counts.get(ClientId::Grok), 2);
        assert_eq!(counted.messages.len(), 2);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            57
        );
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.cache_read)
                .sum::<i64>(),
            60
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let assert_reports = |expected: (i32, i64, i64, i64, i64, i64)| {
            let (messages, input, output, cache_read, cache_write, reasoning) = expected;
            let model = runtime
                .block_on(get_model_report(report_options()))
                .unwrap();
            let monthly = runtime
                .block_on(get_monthly_report(report_options()))
                .unwrap();
            let hourly = runtime
                .block_on(get_hourly_report(report_options()))
                .unwrap();
            let agents = runtime
                .block_on(get_agents_report(report_options()))
                .unwrap();

            assert_eq!(
                (
                    model.total_messages,
                    model.total_input,
                    model.total_output,
                    model.total_cache_read,
                    model.total_cache_write,
                    model
                        .entries
                        .iter()
                        .map(|entry| entry.reasoning)
                        .sum::<i64>(),
                ),
                expected
            );
            assert_eq!(
                monthly
                    .entries
                    .iter()
                    .fold((0, 0, 0, 0, 0), |totals, entry| (
                        totals.0 + entry.message_count,
                        totals.1 + entry.input,
                        totals.2 + entry.output,
                        totals.3 + entry.cache_read,
                        totals.4 + entry.cache_write,
                    ),),
                (messages, input, output, cache_read, cache_write)
            );
            assert_eq!(
                hourly
                    .entries
                    .iter()
                    .fold((0, 0, 0, 0, 0, 0), |totals, entry| (
                        totals.0 + entry.message_count,
                        totals.1 + entry.input,
                        totals.2 + entry.output,
                        totals.3 + entry.cache_read,
                        totals.4 + entry.cache_write,
                        totals.5 + entry.reasoning,
                    ),),
                expected
            );
            assert_eq!(
                agents
                    .entries
                    .iter()
                    .fold((0, 0, 0, 0, 0), |totals, entry| (
                        totals.0 + entry.input,
                        totals.1 + entry.output,
                        totals.2 + entry.cache_read,
                        totals.3 + entry.cache_write,
                        totals.4 + entry.reasoning,
                    ),),
                (input, output, cache_read, cache_write, reasoning)
            );
            assert_eq!(agents.total_messages, messages);
        };
        assert_reports((2, 57, 20, 60, 0, 5));

        std::fs::remove_file(&unified).unwrap();
        assert_eq!(
            latest_source_mtime_ms(&local_options()).unwrap(),
            before_unified
        );
        assert_eq!(
            local_source_change_token(&local_options()).unwrap(),
            legacy_change_token,
            "deleting a non-max authority source must still invalidate consumers"
        );
        let restored = materialized();
        assert_eq!(restored, legacy);
        assert_eq!(streamed(), legacy);
        let restored_count = parse_local_clients(local_options()).unwrap();
        assert_eq!(restored_count.counts.get(ClientId::Grok), 2);
        assert_reports((2, 1_016, 0, 0, 0, 0));
        assert!(message_cache::SourceMessageCache::load()
            .get(&unified)
            .is_none());
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_auto_discovered_hermes_profile_wal() {
        let source_home = tempfile::TempDir::new().unwrap();
        let profile_dir = source_home.path().join(".hermes/profiles/research");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let db = profile_dir.join("state.db");
        let wal = profile_dir.join("state.db-wal");
        std::fs::File::create(&db).unwrap();
        std::fs::File::create(&wal).unwrap();

        let db_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let wal_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        let db_file = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        let Ok(()) = db_file.set_modified(db_time) else {
            return;
        };
        drop(db_file);
        let wal_file = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
        let Ok(()) = wal_file.set_modified(wal_time) else {
            return;
        };
        drop(wal_file);

        let options = LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        };
        let token = crate::latest_source_mtime_ms(&options).unwrap();

        assert_eq!(
            token, 1_700_086_400_000,
            "the change token must include an auto-discovered profile WAL"
        );
    }

    // The live-tail change token must move when jcode appends to the sibling
    // `.journal.jsonl` even though the snapshot mtime is unchanged; otherwise
    // UsageTail short-circuits and never reflects the new turn.
    #[test]
    #[serial_test::serial]
    fn test_latest_source_mtime_ms_probes_jcode_journal() {
        let source_home = tempfile::TempDir::new().unwrap();
        let sessions_dir = source_home.path().join(".jcode/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let snapshot = sessions_dir.join("session_x.json");
        std::fs::write(&snapshot, br#"{"id":"session_x","messages":[]}"#).unwrap();
        let journal = sessions_dir.join("session_x.journal.jsonl");
        std::fs::write(&journal, b"{\"append_messages\":[]}\n").unwrap();

        // Snapshot old, journal strictly newer — the journal-only append the
        // probe must catch. Skip gracefully if the FS rejects set_modified.
        let snapshot_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let journal_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        let sf = std::fs::OpenOptions::new().write(true).open(&snapshot).unwrap();
        let Ok(()) = sf.set_modified(snapshot_time) else {
            return;
        };
        drop(sf);
        let jf = std::fs::OpenOptions::new().write(true).open(&journal).unwrap();
        let Ok(()) = jf.set_modified(journal_time) else {
            return;
        };
        drop(jf);

        let options = LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["jcode".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        };
        let token = crate::latest_source_mtime_ms(&options).unwrap();

        // The newer journal mtime must dominate; without the journal probe the
        // token would stop at the older snapshot mtime (1_700_000_000_000).
        assert_eq!(
            token, 1_700_086_400_000,
            "the change token must reflect the jcode journal mtime, not just the snapshot"
        );
    }

    #[test]
    fn test_parse_local_clients_honors_scanner_extra_scan_paths_for_zed_threads_db() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let extra_threads_dir = temp_dir.path().join("custom-zed/threads");
        std::fs::create_dir_all(&extra_threads_dir).unwrap();
        let threads_db = extra_threads_dir.join("threads.db");
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
        extra_scan_paths.insert("zed".to_string(), vec![extra_threads_dir]);
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
    fn test_parse_local_clients_dedups_default_and_auto_discovered_hermes_profile() {
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
        insert_hermes_session(
            &profile_conn,
            "profile-only-session",
            "claude-sonnet-4",
            1,
            30,
            3,
            0.02,
        );
        drop(profile_conn);

        let parsed = parse_local_clients(LocalParseOptions {
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

        assert_eq!(parsed.counts.get(ClientId::Hermes), 3);
        assert_eq!(parsed.messages.len(), 2);
        let shared = parsed
            .messages
            .iter()
            .find(|message| message.session_id == "shared-hermes-session")
            .unwrap();
        assert_eq!(shared.input, 100);
        assert_eq!(shared.output, 25);
        assert!(parsed
            .messages
            .iter()
            .any(|message| message.session_id == "profile-only-session"));
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
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let variant_dir = source_home.path().join(".cc-mirror/kimi-code");
            let config_dir = source_home.path().join("mirror-configs/kimi-code");
            let project_dir = config_dir.join("projects/project-one");
            std::fs::create_dir_all(&project_dir).unwrap();
            std::fs::create_dir_all(&variant_dir).unwrap();
            let variant_path = variant_dir.join("variant.json");
            std::fs::write(
                &variant_path,
                serde_json::json!({
                    "name": "kimi-code",
                    "provider": "kimi",
                    "configDir": config_dir,
                })
                .to_string(),
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
                serde_json::json!({
                    "name": "kimi-code",
                    "provider": "minimax",
                    "configDir": config_dir,
                })
                .to_string(),
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
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_keeps_normal_claude_when_cc_mirror_points_at_claude_config() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

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
                serde_json::json!({
                    "name": "plain-mirror",
                    "provider": "mirror",
                    "configDir": claude_dir,
                })
                .to_string(),
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
            cost_source: crate::CostSource::Unknown,
            duration_ms: None,
            message_count: 1,
            agent: None,
            dedup_key: dedup_key.map(|s| s.to_string()),
            dedup_aliases: Vec::new(),
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

    #[test]
    fn model_aggregation_saturates_overflowing_token_folds() {
        // token_total_saturates_on_overlarge_buckets (see positive_token_total's
        // callers) covers a single message's grand total; the per-field
        // CROSS-MESSAGE fold in aggregate_model_usage_entries must saturate too.
        // An antigravity-cli row can carry an i64::MAX bucket after the
        // untrusted-varint clamp (sessions/antigravity_cli.rs to_i64), so two
        // such rows folded into one model group with plain `+=` overflow (debug
        // panic / release wrap) before the already-saturating grand total runs.
        let make = || {
            UnifiedMessage::new(
                "antigravity-cli",
                "gemini-3-pro",
                "antigravity",
                "session-overflow",
                1_733_011_200_000,
                TokenBreakdown {
                    input: i64::MAX,
                    output: 0,
                    cache_read: i64::MAX,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0,
            )
        };

        let entries = aggregate_model_usage_entries(vec![make(), make()], &GroupBy::Model);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input, i64::MAX);
        assert_eq!(entries[0].cache_read, i64::MAX);
    }

    #[test]
    fn model_report_totals_saturate_across_groups() {
        // aggregate_model_usage_entries saturates each entry's fields, so an
        // entry can be i64::MAX. get_model_report sums the entries into the
        // report-level totals via model_report_token_totals; two saturated
        // entries (two distinct models) must not overflow that sum either.
        let make = |model: &str| {
            UnifiedMessage::new(
                "antigravity-cli",
                model,
                "antigravity",
                "session-overflow",
                1_733_011_200_000,
                TokenBreakdown {
                    input: i64::MAX,
                    output: 0,
                    cache_read: i64::MAX,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0,
            )
        };

        let entries = aggregate_model_usage_entries(
            vec![make("gemini-3-pro"), make("claude-opus-4-6")],
            &GroupBy::Model,
        );
        assert_eq!(entries.len(), 2);
        let (total_input, _total_output, total_cache_read, _total_cache_write) =
            super::model_report_token_totals(&entries);
        assert_eq!(total_input, i64::MAX);
        assert_eq!(total_cache_read, i64::MAX);
    }

    fn m15a_global_root(home: &Path) -> PathBuf {
        home.join("Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent")
    }

    fn write_m15a_snapshot(home: &Path, body: &str) -> PathBuf {
        let path = m15a_global_root(home).join("workspace-a/conversation.chat");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        m15a_global_root(home).join("workspace-a/conversation.chat")
    }

    fn write_m15a_execution(home: &Path, status: &str, start_time: &str) -> PathBuf {
        let path = m15a_global_root(home).join("workspace-a/execution-store/execution");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"{{
                    "executionId": "exec-1",
                    "chatSessionId": "chat-1",
                    "status": "{status}",
                    "startTime": {start_time},
                    "endTime": 1770983427500,
                    "completionOptions": {{"modelId": "claude-sonnet-4-5"}},
                    "context": {{"messages": [{{"entries": [{{"type": "text", "text": "execution input"}}]}}]}},
                    "actions": [{{"actionType": "say", "output": "execution output"}}]
                }}"#
            ),
        )
        .unwrap();
        path
    }

    fn m15a_snapshot_body(execution_id: &str, prompt: &str, response: &str) -> String {
        format!(
            r#"{{
                "executionId": "{execution_id}",
                "model": "claude-sonnet-4-5",
                "messages": [
                    {{"role": "user", "content": "{prompt}"}},
                    {{"role": "assistant", "content": "{response}"}}
                ]
            }}"#
        )
    }

    fn m15a_local_options(home: &Path) -> LocalParseOptions {
        LocalParseOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["kiro".to_string()]),
            ..Default::default()
        }
    }

    fn m15a_report_options(home: &Path) -> ReportOptions {
        ReportOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["kiro".to_string()]),
            ..Default::default()
        }
    }

    fn m15b_ide_paths(home: &Path) -> (PathBuf, PathBuf) {
        let dir = home.join(".kiro/sessions/workspace-a/sess_m15b");
        (dir.join("session.json"), dir.join("messages.jsonl"))
    }

    #[test]
    #[serial_test::serial]
    fn m15b_ide_sibling_changes_reach_cache_all_lanes_and_reports() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let (session, messages) = m15b_ide_paths(source_home.path());
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(
            &session,
            r#"{
                "id":"sess_m15b",
                "modelId":"claude-opus-4.6",
                "workspacePaths":["/tmp/m15b-project"]
            }"#,
        )
        .unwrap();

        let clients = ["kiro".to_string()];
        let scanner_settings = scanner::ScannerSettings::default();
        let parse_materialized = || {
            let mut parsed = parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner_settings,
            );
            parsed.sort_by(|left, right| left.dedup_key.cmp(&right.dedup_key));
            parsed
        };

        let source_fingerprint = message_cache::SourceFingerprint::from_path(&session).unwrap();
        let missing_sidecar_fingerprint =
            message_cache::SourceFingerprint::from_kiro_path(&session).unwrap();
        let before = latest_source_mtime_ms(&m15a_local_options(source_home.path())).unwrap();
        assert!(parse_materialized().is_empty());

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &messages,
            concat!(
                "{\"timestamp\":\"2026-06-20T10:00:00Z\",\"payload\":{\"type\":\"user\",\"content\":\"hello\"}}\n",
                "{\"payload\":{\"type\":\"session_metadata\",\"key\":\"contextUsage\",\"value\":{\"usagePercentage\":10.0}}}\n",
                "{\"payload\":{\"type\":\"assistant\",\"content\":\"answer\"}}\n",
                "{\"payload\":{\"type\":\"usage_summary\",\"elapsedTime\":1000}}\n",
                "{\"timestamp\":\"2026-06-20T10:00:01Z\",\"payload\":{\"type\":\"turn_end\"}}\n",
            ),
        )
        .unwrap();
        let first_sidecar_fingerprint =
            message_cache::SourceFingerprint::from_kiro_path(&session).unwrap();
        assert_ne!(missing_sidecar_fingerprint, first_sidecar_fingerprint);
        assert_eq!(
            source_fingerprint,
            message_cache::SourceFingerprint::from_path(&session).unwrap(),
            "messages.jsonl must invalidate through the related-file fingerprint"
        );
        let after_first = latest_source_mtime_ms(&m15a_local_options(source_home.path())).unwrap();
        assert!(after_first > before);

        let first = parse_materialized();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].tokens.input, 20_000);
        assert_eq!(first[0].dedup_key.as_deref(), Some("sess_m15b:ide:0"));
        assert_eq!(first[0].model_id, "claude-opus-4.6");
        assert_eq!(first[0].workspace_key.as_deref(), Some("/tmp/m15b-project"));
        assert_eq!(
            parse_materialized(),
            first,
            "the first warm hit must be stable"
        );

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &messages,
            concat!(
                "{\"timestamp\":\"2026-06-20T10:00:00Z\",\"payload\":{\"type\":\"user\",\"content\":\"hello\"}}\n",
                "{\"payload\":{\"type\":\"session_metadata\",\"key\":\"contextUsage\",\"value\":{\"usagePercentage\":10.0}}}\n",
                "{\"payload\":{\"type\":\"assistant\",\"content\":\"answer\"}}\n",
                "{\"payload\":{\"type\":\"usage_summary\",\"elapsedTime\":1000}}\n",
                "{\"timestamp\":\"2026-06-20T10:00:01Z\",\"payload\":{\"type\":\"turn_end\"}}\n",
                "{\"timestamp\":\"2026-06-20T10:01:00Z\",\"payload\":{\"type\":\"user\",\"content\":\"next\"}}\n",
                "{\"payload\":{\"type\":\"session_metadata\",\"key\":\"contextUsage\",\"value\":{\"usagePercentage\":20.0}}}\n",
                "{\"payload\":{\"type\":\"assistant\",\"content\":\"done\"}}\n",
                "{\"payload\":{\"type\":\"usage_summary\",\"elapsedTime\":1000}}\n",
                "{\"timestamp\":\"2026-06-20T10:01:01Z\",\"payload\":{\"type\":\"turn_end\"}}\n",
            ),
        )
        .unwrap();
        let updated_sidecar_fingerprint =
            message_cache::SourceFingerprint::from_kiro_path(&session).unwrap();
        assert_ne!(first_sidecar_fingerprint, updated_sidecar_fingerprint);
        let after_second = latest_source_mtime_ms(&m15a_local_options(source_home.path())).unwrap();
        assert!(after_second > after_first);

        let updated = parse_materialized();
        assert_eq!(updated.len(), 2);
        assert_eq!(
            updated
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            60_000
        );
        assert_eq!(
            parse_materialized(),
            updated,
            "the rebuilt cache must stay warm-complete"
        );
        assert_eq!(
            message_cache::SourceMessageCache::load()
                .get(&session)
                .unwrap()
                .messages
                .len(),
            2
        );

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner_settings,
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        streamed.sort_by(|left, right| left.dedup_key.cmp(&right.dedup_key));
        assert_eq!(streamed, updated);

        let counted = parse_local_clients(m15a_local_options(source_home.path())).unwrap();
        assert_eq!(counted.counts.get(ClientId::Kiro), 2);
        assert_eq!(counted.messages.len(), 2);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            60_000
        );
        let pruned = parse_local_clients(LocalParseOptions {
            modified_after: Some(after_first + 1),
            ..m15a_local_options(source_home.path())
        })
        .unwrap();
        assert_eq!(pruned.counts.get(ClientId::Kiro), 2);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report_options = m15a_report_options(source_home.path());
        let model = runtime
            .block_on(get_model_report(report_options.clone()))
            .unwrap();
        let monthly = runtime
            .block_on(get_monthly_report(report_options.clone()))
            .unwrap();
        let hourly = runtime
            .block_on(get_hourly_report(report_options.clone()))
            .unwrap();
        let agents = runtime.block_on(get_agents_report(report_options)).unwrap();
        assert_eq!(model.total_messages, 2);
        assert_eq!(model.total_input, 60_000);
        assert_eq!(
            monthly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            2
        );
        assert_eq!(
            hourly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            2
        );
        assert_eq!(agents.total_messages, 2);
        assert_eq!(
            agents.entries.iter().map(|entry| entry.input).sum::<i64>(),
            60_000
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_cli_keys_cannot_seed_or_collide_with_globalstorage() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let cli_dir = source_home.path().join(".kiro/sessions/cli");
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(
            cli_dir.join("cli.json"),
            r#"{"session_id":"execution","cwd":"workspace-a","session_state":{"rts_model_state":{"model_info":{"model_id":"cli-model"}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":1}]}}}"#,
        )
        .unwrap();
        std::fs::write(cli_dir.join("cli.jsonl"), "").unwrap();
        std::fs::write(
            cli_dir.join("cli-collision.json"),
            r#"{"session_id":"workspace-a/conversation:globalstorage:exec","cwd":"workspace-a","session_state":{"rts_model_state":{"model_info":{"model_id":"cli-model"}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":1}]}}}"#,
        )
        .unwrap();
        std::fs::write(cli_dir.join("cli-collision.jsonl"), "").unwrap();
        write_m15a_snapshot(source_home.path(), &m15a_snapshot_body("0", "ABCD", ""));
        let execution_zero =
            m15a_global_root(source_home.path()).join("workspace-a/execution-store/execution-zero");
        std::fs::create_dir_all(execution_zero.parent().unwrap()).unwrap();
        std::fs::write(
            execution_zero,
            r#"{
                "executionId": "0",
                "chatSessionId": "conversation",
                "status": "succeed",
                "startTime": 1770983426,
                "endTime": 1770983427500,
                "completionOptions": {"modelId": "claude-sonnet-4-5"},
                "context": {"messages": [{"entries": [{"type": "text", "text": "execution input"}]}]},
                "actions": [{"actionType": "say", "output": "execution output"}]
            }"#,
        )
        .unwrap();

        let expected = vec![
            "conversation",
            "execution",
            "workspace-a/conversation:globalstorage:exec",
        ];
        let mut materialized: Vec<_> = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
        )
        .into_iter()
        .map(|message| message.session_id)
        .collect();
        materialized.sort();
        assert_eq!(materialized, expected);

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.session_id.clone()),
        );
        streamed.sort();
        assert_eq!(streamed, expected);

        let mut counted: Vec<_> = parse_local_clients(m15a_local_options(source_home.path()))
            .unwrap()
            .messages
            .into_iter()
            .map(|message| message.session_id)
            .collect();
        counted.sort();
        assert_eq!(counted, expected);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_duplicate_snapshot_extensions_are_exact_once() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let body = m15a_snapshot_body("unused", "ABCD", "WXYZ");
        let chat = write_m15a_snapshot(source_home.path(), &body);
        std::fs::write(chat.with_extension("json"), body).unwrap();

        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
        );
        assert_eq!(materialized.len(), 1);

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 1);

        let counted = parse_local_clients(m15a_local_options(source_home.path())).unwrap();
        assert_eq!(counted.counts.get(ClientId::Kiro), 1);
        assert_eq!(counted.messages.len(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_materialized_streaming_count_and_report_parity() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let snapshot = write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "snapshot input", "snapshot output"),
        );
        let execution = write_m15a_execution(source_home.path(), "succeed", "1770983426");
        let mut pricing_data = HashMap::new();
        pricing_data.insert(
            "claude-sonnet-4-5".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(1.0),
                output_cost_per_token: Some(1.0),
                ..Default::default()
            },
        );
        let pricing_service = pricing::PricingService::new(pricing_data, HashMap::new());

        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            Some(&pricing_service),
            false,
            &scanner::ScannerSettings::default(),
        );
        assert_eq!(materialized.len(), 1);
        assert_eq!(
            materialized[0].dedup_key.as_deref(),
            Some("execution:exec-1")
        );
        assert!(materialized[0].cost > 0.0);

        // Both source entries are raw and independently cached even though the
        // snapshot is suppressed in the merged result.
        let cache = message_cache::SourceMessageCache::load();
        assert!(cache.get(&snapshot).is_some_and(|entry| entry.messages[0]
            .dedup_key
            .as_deref()
            .is_some_and(|key| key.ends_with(":globalstorage:exec:exec-1"))));
        assert!(cache.get(&execution).is_some_and(
            |entry| entry.messages[0].dedup_key.as_deref() == Some("execution:exec-1")
        ));
        assert!(cache
            .get(&snapshot)
            .is_some_and(|entry| entry.messages[0].cost == 0.0));
        assert!(cache
            .get(&execution)
            .is_some_and(|entry| entry.messages[0].cost == 0.0));

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            Some(&pricing_service),
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].dedup_key.as_deref(), Some("execution:exec-1"));
        assert!(streamed[0].cost > 0.0);
        assert_eq!(materialized[0].cost, streamed[0].cost);
        assert_eq!(
            (materialized[0].tokens.input, materialized[0].tokens.output),
            (streamed[0].tokens.input, streamed[0].tokens.output)
        );
        assert_eq!(materialized[0].model_id, "claude-sonnet-4-5");
        assert_eq!(materialized[0].session_id, "chat-1");
        assert_eq!(
            materialized[0].workspace_key.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(
            materialized[0].workspace_label.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(materialized[0].timestamp, 1_770_983_426_000);
        assert_eq!(materialized[0].duration_ms, Some(1_500));
        assert_eq!(materialized[0].message_count, 1);
        assert_eq!(
            (
                streamed[0].model_id.as_str(),
                streamed[0].session_id.as_str(),
                streamed[0].workspace_key.as_deref(),
                streamed[0].workspace_label.as_deref(),
                streamed[0].timestamp,
                streamed[0].duration_ms,
                streamed[0].message_count,
            ),
            (
                materialized[0].model_id.as_str(),
                materialized[0].session_id.as_str(),
                materialized[0].workspace_key.as_deref(),
                materialized[0].workspace_label.as_deref(),
                materialized[0].timestamp,
                materialized[0].duration_ms,
                materialized[0].message_count,
            )
        );

        let counted = parse_local_clients(m15a_local_options(source_home.path())).unwrap();
        assert_eq!(counted.counts.get(ClientId::Kiro), 1);
        assert_eq!(counted.messages.len(), 1);
        assert_eq!(counted.messages[0].model_id, materialized[0].model_id);
        assert_eq!(counted.messages[0].session_id, materialized[0].session_id);
        assert_eq!(
            counted.messages[0].workspace_key,
            materialized[0].workspace_key
        );
        assert_eq!(
            counted.messages[0].workspace_label,
            materialized[0].workspace_label
        );
        assert_eq!(counted.messages[0].timestamp, materialized[0].timestamp);
        assert_eq!(counted.messages[0].duration_ms, materialized[0].duration_ms);
        assert_eq!(
            counted.messages[0].message_count,
            materialized[0].message_count
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let options = m15a_report_options(source_home.path());
        let model = runtime.block_on(get_model_report(options.clone())).unwrap();
        let monthly = runtime
            .block_on(get_monthly_report(options.clone()))
            .unwrap();
        let hourly = runtime
            .block_on(get_hourly_report(options.clone()))
            .unwrap();
        let agents = runtime
            .block_on(get_agents_report(options.clone()))
            .unwrap();
        let mut session_options = options.clone();
        session_options.group_by = GroupBy::Session;
        let session_model = runtime.block_on(get_model_report(session_options)).unwrap();
        let mut workspace_options = options;
        workspace_options.group_by = GroupBy::WorkspaceModel;
        let workspace_model = runtime
            .block_on(get_model_report(workspace_options))
            .unwrap();

        assert_eq!(model.total_messages, 1);
        assert_eq!(model.entries.len(), 1);
        assert_eq!(model.entries[0].model, materialized[0].model_id);
        assert_eq!(
            model.entries[0].message_count,
            materialized[0].message_count
        );
        assert_eq!(session_model.entries.len(), 1);
        assert_eq!(
            session_model.entries[0].session_id.as_deref(),
            Some("chat-1")
        );
        assert_eq!(workspace_model.entries.len(), 1);
        assert_eq!(
            workspace_model.entries[0].workspace_key.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(
            workspace_model.entries[0].workspace_label.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(
            monthly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            1
        );
        assert_eq!(
            hourly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            1
        );
        assert_eq!(agents.total_messages, 1);
        assert_eq!(model.total_input, monthly.entries[0].input);
        assert_eq!(model.total_input, hourly.entries[0].input);
        assert_eq!(model.total_input, agents.entries[0].input);
        assert_eq!(model.total_output, monthly.entries[0].output);
        assert_eq!(model.total_output, hourly.entries[0].output);
        assert_eq!(model.total_output, agents.entries[0].output);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_warm_cache_mixed_hits_reapply_suppression_and_restore_snapshot() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let snapshot = write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "snapshot input", "snapshot output"),
        );
        let execution = write_m15a_execution(source_home.path(), "succeed", "1770983426");
        let home = source_home.path().to_str().unwrap();
        let clients = ["kiro".to_string()];

        let first = parse_all_messages_with_pricing_with_env_strategy(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].dedup_key.as_deref(), Some("execution:exec-1"));

        // Snapshot hit + execution miss: only the newly parsed execution wins.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_m15a_execution(source_home.path(), "succeed", "1770983426.5");
        let mut streamed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].dedup_key.as_deref(), Some("execution:exec-1"));

        // Execution hit + snapshot miss: the snapshot change is still suppressed.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body(
                "exec-1",
                "changed snapshot input",
                "changed snapshot output",
            ),
        );
        let second = parse_all_messages_with_pricing_with_env_strategy(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
        );
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].dedup_key.as_deref(), Some("execution:exec-1"));

        // A successful execution rewritten as failed removes its stale cache
        // entry and exposes the raw cached snapshot.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_m15a_execution(source_home.path(), "failed", "1770983426");
        let mut failed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| failed.push(message.clone()),
        );
        assert_eq!(failed.len(), 1);
        assert!(failed[0]
            .dedup_key
            .as_deref()
            .is_some_and(|key| key.ends_with(":globalstorage:exec:exec-1")));

        // Restore a successful execution after the failed rewrite. This proves
        // a cached successful execution can become authoritative again.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_m15a_execution(source_home.path(), "succeed", "1770983426");
        let restored_execution = parse_all_messages_with_pricing_with_env_strategy(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
        );
        assert_eq!(restored_execution.len(), 1);
        assert_eq!(
            restored_execution[0].dedup_key.as_deref(),
            Some("execution:exec-1")
        );
        assert!(message_cache::SourceMessageCache::load()
            .get(&execution)
            .is_some());

        // Removing that cached successful execution must expose the raw cached
        // snapshot on the other (streaming) lane.
        std::fs::remove_file(&execution).unwrap();
        let mut restored = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| restored.push(message.clone()),
        );
        assert_eq!(restored.len(), 1);
        assert!(restored[0]
            .dedup_key
            .as_deref()
            .is_some_and(|key| key.ends_with(":globalstorage:exec:exec-1")));
        assert!(message_cache::SourceMessageCache::load()
            .get(&snapshot)
            .is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_suppression_precedes_report_date_filter() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let snapshot = write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-future", "snapshot input", "snapshot output"),
        );
        let execution = m15a_global_root(source_home.path())
            .join("workspace-a/execution-store/execution-future");
        std::fs::create_dir_all(execution.parent().unwrap()).unwrap();
        std::fs::write(
            &execution,
            r#"{"executionId":"exec-future","chatSessionId":"chat-future","status":"succeed","startTime":4102444800000,"endTime":4102444801000,"actions":[{"actionType":"say","output":"future answer"}],"input":{"data":{"messages":[{"content":"future question"}]}}}"#,
        )
        .unwrap();
        let snapshot_date = sessions::kiro::parse_kiro_file(&snapshot)[0].date.clone();

        let mut options = m15a_report_options(source_home.path());
        options.until = Some(snapshot_date);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report = runtime.block_on(get_model_report(options)).unwrap();
        assert!(report.entries.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_globalstorage_mtime_pruning_and_stat_failure_fail_open() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let snapshot = write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "initial input", "initial output"),
        );
        let options = m15a_local_options(source_home.path());
        let before = latest_source_mtime_ms(&options).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "rewritten input", "rewritten output"),
        );
        let after = latest_source_mtime_ms(&options).unwrap();
        assert!(after > before, "globalStorage primary mtime must advance");

        let parsed = parse_local_clients(LocalParseOptions {
            modified_after: Some(before + 1),
            ..options.clone()
        })
        .unwrap();
        assert_eq!(parsed.counts.get(ClientId::Kiro), 1);

        let execution = write_m15a_execution(source_home.path(), "succeed", "1770983426");
        let execution_mtime = super::kiro_source_mtime_ms(&execution).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "newer snapshot", "must stay suppressed"),
        );
        let snapshot_mtime = super::kiro_source_mtime_ms(&snapshot).unwrap();
        assert!(snapshot_mtime > execution_mtime);
        let snapshot_date = sessions::kiro::parse_kiro_file(&snapshot)[0].date.clone();
        let parsed = parse_local_clients(LocalParseOptions {
            modified_after: Some(execution_mtime + 1),
            since: Some(snapshot_date),
            ..options.clone()
        })
        .unwrap();
        assert_eq!(parsed.counts.get(ClientId::Kiro), 1);
        assert!(
            parsed.messages.is_empty(),
            "mtime pruning must retain the older execution until suppression"
        );

        let missing = source_home.path().join("missing.chat");
        let mut scan = scanner::ScanResult::default();
        scan.get_mut(ClientId::Kiro).push(missing);
        prune_scan_result_by_mtime(&mut scan, u64::MAX);
        assert_eq!(scan.get(ClientId::Kiro).len(), 1);

        // Keep the fixture path live for the cache/source identity assertion.
        assert!(message_cache::SourceFingerprint::from_kiro_path(&snapshot).is_some());
    }
}
