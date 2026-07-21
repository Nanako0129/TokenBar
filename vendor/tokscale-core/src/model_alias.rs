//! Config-driven model-name aliasing for **grouping only**.
//!
//! Different supply channels report the same physical model under different
//! name-strings (for example `claude-opus-4-8`, `claude-opus-4-8-cc`, and
//! `anthropic/claude-opus-4-8` are all one model), so usage stats split across
//! multiple rows. A user-configured `{alias: canonical}` map folds those
//! variants into one canonical **display/group** key.
//!
//! The fold runs as the terminal step of [`crate::normalize_model_for_grouping`],
//! so it applies uniformly to local model/monthly/hourly reports and every
//! `GroupBy`. It is **presentation only**: the submit/upload/export/persist path
//! and graph `ClientContribution` keys use [`crate::canonical_model_id`] (the
//! same syntactic normalization *without* the alias fold), so a machine-local
//! alias config can never rewrite the model identity that leaves the machine or
//! fragment history. It is deliberately **not** applied before pricing
//! (per-message cost is computed on the raw model id upstream), so folding can
//! only relabel and merge already-costed buckets and can never change a cost
//! total. It is orthogonal to the static pricing alias table
//! ([`crate::pricing::aliases`]) and to `provider_identity` — it touches only
//! the model dimension for local grouping.
//!
//! TokenBar adaptation vs upstream `9a5aeb65`: the process-wide map is
//! **reloadable** (not load-once). Changing aliases bumps
//! [`model_alias_generation`] and runs every registered
//! [`register_usage_data_invalidation_hook`] so usage-data consumers (reports,
//! later M24 Warp) can refresh. Message-cache schema stays 31 because aliases
//! are report-time only.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

/// Upper bound on the number of configured aliases retained. Oversized configs
/// are truncated rather than rejected, mirroring the capacity guard in
/// [`crate::pricing`]'s custom-pricing loader.
const MAX_MODEL_ALIASES: usize = 4096;

/// On-disk / settings shape of the flat `modelAliases` object
/// (`{ "alias": "canonical" }`). `#[serde(transparent)]` keeps the serialized
/// form a bare map. Deserialization is lossy: a malformed value (a non-object,
/// or an entry whose value is not a string) is skipped instead of failing the
/// whole settings load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ModelAliasMap {
    /// Raw `alias -> canonical` pairs exactly as written in the config.
    pub entries: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for ModelAliasMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Read the node as a generic value first so a malformed `modelAliases`
        // (e.g. an array or scalar) degrades to an empty map instead of
        // misaligning the parent settings deserializer. Keep only string-valued
        // entries; skip anything else.
        let value = serde_json::Value::deserialize(deserializer)?;
        let entries = match value {
            serde_json::Value::Object(object) => object
                .into_iter()
                .filter_map(|(key, value)| match value {
                    serde_json::Value::String(canonical) => Some((key, canonical)),
                    _ => None,
                })
                .collect(),
            _ => BTreeMap::new(),
        };
        Ok(Self { entries })
    }
}

/// Runtime resolver built from [`ModelAliasMap`]: keys and values are normalized
/// through [`crate::normalize_syntactic`] so lookups match regardless of case,
/// dated suffix, or `.`-vs-`-` spelling, and canonical values land in the same
/// space the grouping key uses. Empty keys/values and self-maps are dropped; the
/// number of entries is capped.
#[derive(Debug, Default)]
struct ModelAliasResolver {
    map: HashMap<String, String>,
}

impl ModelAliasResolver {
    /// Build a resolver from configured aliases. Both sides of each pair are run
    /// through [`crate::normalize_syntactic`] exactly once: keys are placed in the
    /// same space as incoming (already-normalized) model names, and canonical
    /// values are stored pre-normalized. `apply` returns a canonical value
    /// verbatim — it is never re-resolved or re-normalized — so the value written
    /// here is exactly the label shown in reports. Empty keys/values and
    /// self-maps are dropped, and the number of entries is capped.
    fn from_config(config: &ModelAliasMap) -> Self {
        let mut map = HashMap::new();
        for (raw_alias, raw_canonical) in &config.entries {
            if map.len() >= MAX_MODEL_ALIASES {
                break;
            }
            // Store keys under a separator-insensitive match key so matching is
            // provider-agnostic (not claude-only): `gpt-5-5` and `gpt-5.5` share
            // the key `gpt-5-5`. The stored canonical value keeps its
            // `normalize_syntactic` spelling — it is the label shown verbatim.
            let alias_norm = crate::normalize_syntactic(raw_alias);
            let canonical = crate::normalize_syntactic(raw_canonical);
            // Self-map drop compares the *exact* normalized forms, not the match
            // keys: `{gpt-5-5: gpt-5.5}` is a real separator relabel that must be
            // kept, whereas `{gpt-5.5: gpt-5.5}` is a genuine no-op to drop.
            if alias_norm.is_empty() || canonical.is_empty() || alias_norm == canonical {
                continue;
            }
            map.insert(match_key(&alias_norm), canonical);
        }
        Self { map }
    }

    /// Resolve one model name. `name` must already be `normalize_syntactic`'d (it
    /// is, since the only caller is [`crate::normalize_model_for_grouping`]).
    /// Resolution is single-hop — the canonical value is never re-resolved — so
    /// alias chains collapse one step and cycles are structurally impossible.
    /// Returns `name` unchanged on a miss.
    fn apply(&self, name: String) -> String {
        match self.map.get(&match_key(&name)) {
            Some(canonical) => canonical.clone(),
            None => name,
        }
    }
}

/// Reduce an already-`normalize_syntactic`'d model name to a separator-
/// insensitive match key by rewriting every `.` to `-`. This generalizes alias
/// matching beyond claude: `normalize_syntactic` only rewrites `.`→`-` inside
/// *claude* version numbers, so without this a `gpt-5-5` alias would miss
/// `gpt-5.5`. Folding on the match key alone keeps the displayed canonical form
/// (e.g. `gpt-5.5`) untouched for models that were never aliased.
fn match_key(normalized: &str) -> String {
    normalized.replace('.', "-")
}

#[derive(Default)]
struct AliasState {
    config: ModelAliasMap,
    resolver: ModelAliasResolver,
}

fn state() -> &'static RwLock<AliasState> {
    static STATE: OnceLock<RwLock<AliasState>> = OnceLock::new();
    STATE.get_or_init(|| RwLock::new(AliasState::default()))
}

/// Monotonic generation bumped on every successful alias install/clear.
/// Usage-data consumers (and later M24 Warp) can poll this cheaply to detect
/// that report-time grouping input changed without re-reading the full map.
static GENERATION: AtomicU64 = AtomicU64::new(0);

type InvalidationHook = Box<dyn Fn() + Send + Sync + 'static>;

fn hooks() -> &'static RwLock<Vec<InvalidationHook>> {
    static HOOKS: OnceLock<RwLock<Vec<InvalidationHook>>> = OnceLock::new();
    HOOKS.get_or_init(|| RwLock::new(Vec::new()))
}

fn install(config: ModelAliasMap) {
    let resolver = ModelAliasResolver::from_config(&config);
    {
        let mut state = state().write().unwrap_or_else(|e| e.into_inner());
        state.config = config;
        state.resolver = resolver;
    }
    GENERATION.fetch_add(1, Ordering::SeqCst);
    notify_usage_data_invalidation();
}

fn notify_usage_data_invalidation() {
    let hooks = hooks().read().unwrap_or_else(|e| e.into_inner());
    for hook in hooks.iter() {
        hook();
    }
}

/// Install (or replace) the process-wide model-alias map used for grouping.
///
/// Always reloads — later calls replace earlier ones. Bumps
/// [`model_alias_generation`] and fires every registered usage-data invalidation
/// hook so the next report sees the new grouping. Until the first non-empty
/// install (and after [`clear_model_aliases`]), grouping is a strict identity
/// no-op relative to [`crate::canonical_model_id`].
pub fn set_model_aliases(config: &ModelAliasMap) {
    install(config.clone());
}

/// Clear all process-wide grouping aliases (identity no-op) and invalidate
/// usage-data consumers.
pub fn clear_model_aliases() {
    install(ModelAliasMap::default());
}

/// Snapshot of the currently installed raw alias map (config shape, not the
/// normalized resolver). Empty when unset or cleared.
pub fn model_aliases() -> ModelAliasMap {
    state()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .config
        .clone()
}

/// Process-wide generation for the installed alias map. Starts at 0; increments
/// on every [`set_model_aliases`] / [`clear_model_aliases`]. Independent of
/// message-cache schema (stays 31).
pub fn model_alias_generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// Register a process-wide callback invoked whenever grouping aliases change.
///
/// Hooks are append-only for the process lifetime. Intended for TokenBar
/// usage-data layers (in-process report caches, later M24 Warp) so a settings
/// reload does not require a process restart. Hooks must be cheap and
/// non-reentrant with respect to alias install.
pub fn register_usage_data_invalidation_hook(hook: impl Fn() + Send + Sync + 'static) {
    hooks()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(hook));
}

/// Apply the installed resolver to an already-`normalize_syntactic`'d name.
pub(crate) fn apply_global(name: String) -> String {
    state()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .resolver
        .apply(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    /// Serializes tests that mutate the process-wide alias state.
    static GLOBAL_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn resolver(pairs: &[(&str, &str)]) -> ModelAliasResolver {
        let entries = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        ModelAliasResolver::from_config(&ModelAliasMap { entries })
    }

    fn alias_map(pairs: &[(&str, &str)]) -> ModelAliasMap {
        ModelAliasMap {
            entries: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    #[test]
    fn folds_three_variants_to_one_canonical() {
        let r = resolver(&[
            ("claude-opus-4-8-cc", "claude-opus-4-8"),
            ("anthropic/claude-opus-4-8", "claude-opus-4-8"),
        ]);
        // All three real-world spellings collapse to the canonical name. The
        // third needs no map entry: syntactic normalization already lowercases it.
        for input in [
            "claude-opus-4-8-cc",
            "anthropic/claude-opus-4-8",
            "Claude-Opus-4-8",
        ] {
            assert_eq!(
                r.apply(crate::normalize_syntactic(input)),
                "claude-opus-4-8",
                "input {input} should fold to claude-opus-4-8"
            );
        }
    }

    #[test]
    fn keys_match_case_and_dotted_insensitively() {
        // Config key written with upper case and a dotted version still matches
        // the normalized input, because both sides run through normalize_syntactic.
        let r = resolver(&[("Claude-Opus-4.8-CC", "claude-opus-4-8")]);
        assert_eq!(
            r.apply(crate::normalize_syntactic("claude-opus-4-8-cc")),
            "claude-opus-4-8"
        );
    }

    #[test]
    fn drops_empty_and_self_maps() {
        let r = resolver(&[
            ("", "claude-opus-4-8"),
            ("claude-opus-4-8-cc", ""),
            ("gpt-5.5", "gpt-5.5"),
        ]);
        assert!(r.map.is_empty());
    }

    #[test]
    fn resolution_is_single_hop() {
        // {a: b, b: c} resolves a -> b (not c) and never loops.
        let r = resolver(&[("model-a", "model-b"), ("model-b", "model-c")]);
        assert_eq!(r.apply("model-a".to_string()), "model-b");
        assert_eq!(r.apply("model-b".to_string()), "model-c");
    }

    #[test]
    fn separator_insensitive_match_is_provider_agnostic() {
        // Finding A: `normalize_syntactic` rewrites `.`→`-` only for claude, so
        // the resolver must fold separators itself for every other provider. The
        // regression is when the CONFIGURED alias key and the model string the
        // provider actually reports use different separators — the old exact
        // HashMap lookup missed and left the variant unfolded.

        // Dashed alias key (`gpt-5-5-cc`), dotted model spelling (`gpt-5.5-cc`):
        // must still fold to the canonical `gpt-5.5`.
        let dashed_key = resolver(&[("gpt-5-5-cc", "gpt-5.5")]);
        assert_eq!(
            dashed_key.apply(crate::normalize_syntactic("gpt-5.5-cc")),
            "gpt-5.5",
            "a dashed alias key must match the dotted model spelling (gpt-5-5 ↔ gpt-5.5)"
        );

        // Mirror: dotted alias key, dashed model spelling.
        let dotted_key = resolver(&[("gpt-5.5-cc", "gpt-5.5")]);
        assert_eq!(
            dotted_key.apply(crate::normalize_syntactic("gpt-5-5-cc")),
            "gpt-5.5",
            "a dotted alias key must match the dashed model spelling"
        );
    }

    #[test]
    fn miss_is_identity() {
        let r = resolver(&[("claude-opus-4-8-cc", "claude-opus-4-8")]);
        assert_eq!(r.apply("gpt-5.5".to_string()), "gpt-5.5");
    }

    #[test]
    fn empty_resolver_is_identity() {
        let r = ModelAliasResolver::default();
        assert_eq!(
            r.apply("claude-opus-4-8-cc".to_string()),
            "claude-opus-4-8-cc"
        );
    }

    #[test]
    fn respects_capacity_cap() {
        let entries: BTreeMap<String, String> = (0..MAX_MODEL_ALIASES + 100)
            .map(|i| (format!("alias-{i}"), format!("canonical-{i}")))
            .collect();
        let r = ModelAliasResolver::from_config(&ModelAliasMap { entries });
        assert_eq!(r.map.len(), MAX_MODEL_ALIASES);
    }

    #[test]
    fn deserialize_is_lossy_over_non_string_values() {
        // Non-string values are skipped; string entries survive.
        let parsed: ModelAliasMap =
            serde_json::from_str(r#"{"a": "b", "n": 5, "arr": ["x"]}"#).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries.get("a").map(String::as_str), Some("b"));
    }

    #[test]
    fn deserialize_of_non_object_is_empty() {
        // A misuse (array/scalar instead of an object) degrades to empty, not error.
        assert!(serde_json::from_str::<ModelAliasMap>("[]")
            .unwrap()
            .entries
            .is_empty());
        assert!(serde_json::from_str::<ModelAliasMap>("\"oops\"")
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn serialize_round_trips_as_flat_map() {
        let map = ModelAliasMap {
            entries: [(
                "claude-opus-4-8-cc".to_string(),
                "claude-opus-4-8".to_string(),
            )]
            .into_iter()
            .collect(),
        };
        let json = serde_json::to_string(&map).unwrap();
        assert_eq!(json, r#"{"claude-opus-4-8-cc":"claude-opus-4-8"}"#);
        assert_eq!(serde_json::from_str::<ModelAliasMap>(&json).unwrap(), map);
    }

    #[test]
    fn reloadable_global_install_and_clear() {
        let _guard = GLOBAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_model_aliases();
        let gen0 = model_alias_generation();

        set_model_aliases(&alias_map(&[("claude-opus-4-8-cc", "claude-opus-4-8")]));
        assert_eq!(
            apply_global(crate::normalize_syntactic("claude-opus-4-8-cc")),
            "claude-opus-4-8"
        );
        assert_eq!(
            model_aliases()
                .entries
                .get("claude-opus-4-8-cc")
                .map(String::as_str),
            Some("claude-opus-4-8")
        );
        let gen1 = model_alias_generation();
        assert!(gen1 > gen0);

        // Reload replaces rather than first-wins.
        set_model_aliases(&alias_map(&[("gpt-5.5-cc", "gpt-5.5")]));
        assert_eq!(
            apply_global(crate::normalize_syntactic("claude-opus-4-8-cc")),
            "claude-opus-4-8-cc",
            "previous alias must be gone after reload"
        );
        assert_eq!(
            apply_global(crate::normalize_syntactic("gpt-5.5-cc")),
            "gpt-5.5"
        );
        let gen2 = model_alias_generation();
        assert!(gen2 > gen1);

        clear_model_aliases();
        assert!(model_aliases().entries.is_empty());
        assert_eq!(
            apply_global(crate::normalize_syntactic("gpt-5.5-cc")),
            "gpt-5.5-cc"
        );
        assert!(model_alias_generation() > gen2);
    }

    #[test]
    fn invalidation_hook_fires_on_set_and_clear() {
        let _guard = GLOBAL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_model_aliases();

        static FIRES: AtomicUsize = AtomicUsize::new(0);
        // Register once per process; subtract baseline so the assertion is local.
        register_usage_data_invalidation_hook(|| {
            FIRES.fetch_add(1, Ordering::SeqCst);
        });
        let baseline = FIRES.load(Ordering::SeqCst);

        set_model_aliases(&alias_map(&[("a", "b")]));
        clear_model_aliases();
        let after = FIRES.load(Ordering::SeqCst);
        assert!(
            after >= baseline + 2,
            "set + clear must each fire the invalidation hook (baseline={baseline}, after={after})"
        );
    }
}
