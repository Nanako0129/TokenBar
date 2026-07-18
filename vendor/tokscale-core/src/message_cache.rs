use crate::sessions::codex::CodexParseState;
use crate::UnifiedMessage;
use bincode::Options;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::UNIX_EPOCH;

// 19: Codex fork-replay parsing now skips replayed parent usage, scopes the
// token_count dedup key to the fork parent, and keeps user-fork turns after
// repeated child session_meta rows. Cached messages store their dedup_key and
// older entries can be empty, so they must be reparsed.
// 20 (#760 vendoring): session parsers now canonicalize provider ids
// (fireworks->fireworks_ai, vertex/gemini/azure aliases), recover
// missing-provider messages (gjc/pi), infer codex provider from the model, and
// emit stable dedup keys (qwen/mux) — all of which change cached parser output,
// so stale entries must be reparsed. (Our own schema counter; do not mirror
// upstream's number.)
// 21 (M5b: vendoring our own upstream-merged fixes): codex same-millisecond
// fork-replay gate (#735), micode row-id-fallback dedup namespaced by db (#742),
// and gjc header-less session fallback by file name (#743) all change cached
// dedup keys / codex token output, so stale entries must be reparsed. (#741
// roo-family sibling fingerprinting also invalidates naturally, and #737's
// antigravity-cli timestamps converge with our dropped local patch.)
// 22 (M6: follow-up correctness + attribution): jcode journal-wins over stale
// snapshot + tz-less timestamps (#754), micode epoch seconds/ms normalization
// (#747), fable->anthropic provider inference (#762), and copilot per-message
// agent attribution (#724/#751) all change cached parser output, so stale
// entries must be reparsed. (Our own schema counter; do not mirror upstream's.)
// 23 (PR #30: drop synthetic claude rows): claudecode parsing now discards
// assistant turns whose model is the `<synthetic>` placeholder (locally
// fabricated by Claude Code, zero-token, no real cost), so a session cached
// before this change still carries the phantom zero-token row until reparsed.
// (Our own schema counter; do not mirror upstream's number.)
// 24 (M8-B1: cost provenance contract): UnifiedMessage now serializes
// cost_source, so schema-23 bincode entries must be rebuilt with the new field.
// 25 (M10-B: Jcode journal corrections that only replace a snapshotted message
// are now turn-neutral, so a following brand-new journal turn is no longer robbed
// of its is_turn_start; schema-24 caches carry the under-counted turn flags, so
// invalidate them.)
// 26 (M10-C: Pi session_info now supplies subagent attribution. Non-empty
// schema-25 caches replay Pi messages with no agent metadata, so invalidate them.
// This is TokenBar's own counter, replacing upstream #856's per-client
// parser_version rather than importing it.)
// 27 (M10-D: Claude workflow journals are now excluded, and deep nested workflow
// transcripts recover Tier-2 parent-session attribution. Schema-26 caches may
// replay usage-shaped workflow journals or stale/generic deep Tier-2 agent
// attribution, so invalidate them. The #856 parser_version/shard-cache
// architecture remains excluded.)
// 28 (M10-E: Copilot trace fallback now resolves the first root invoke_agent
// span by walking the parentSpanId hierarchy, so nested sub-agent invokes
// exported first no longer become the trace default. Schema-27 caches can carry
// the wrong nested fallback and must be rebuilt. The final #834 resolver covers
// #821's reversed-export-order semantic.)
// 29 (M14: Codex token durations now advance from the last accepted token
// snapshot instead of repeatedly measuring from the turn start. Schema-28
// caches can replay overlapping duration_ms values or resume an incremental
// parse without the new cursor, so unchanged sources must be rebuilt.)
// 30 (M20: OpenCode now reads v2 session_message rows. A hybrid database can
// already have a non-empty schema-29 cache containing only its v1 message rows;
// because the database fingerprint is unchanged, that entry must be rejected so
// the same source is rebuilt with both v1 and v2 output.)
const CACHE_SCHEMA_VERSION: u32 = 30;
const CACHE_FILENAME: &str = "source-message-cache.bin";
const CACHE_LOCK_FILENAME: &str = "source-message-cache.lock";
const MAX_CACHE_FILE_BYTES: u64 = 256 * 1024 * 1024;
const FINGERPRINT_SAMPLE_BYTES: usize = 4096;
const FINGERPRINT_SAMPLE_POINTS: usize = 5;
const HASH_BUFFER_BYTES: usize = 64 * 1024;

/// Process-level memo keyed by (path, size, modified_ns). On a cache hit the
/// caller skips `compute_sample_hashes` + `hash_prefix` (the expensive I/O +
/// SHA-256 work). On lock poison or stat failure the caller falls back to the
/// full recompute path — FAIL-LOUD, never returns stale/empty data.
struct HashMemoEntry {
    size: u64,
    modified_ns: u64,
    sample_hashes: Vec<FileSampleHash>,
    content_hash: [u8; 32],
}

static HASH_MEMO: LazyLock<Mutex<HashMap<PathBuf, HashMemoEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Process-level memo for the deserialized store. On a cache hit (`path +
/// size + modified_ns` all match) `load()` skips the bincode deserialize.
/// Path is compared so a test-env config-dir switch never returns data from
/// a different store file.
struct StoreMemo {
    cache_file: PathBuf,
    file_size: u64,
    file_modified_ns: u64,
    entries: HashMap<CachedPath, Arc<CachedSourceEntry>>,
}

static STORE_MEMO: LazyLock<Mutex<Option<StoreMemo>>> =
    LazyLock::new(|| Mutex::new(None));

/// Convert a `Duration` (typically elapsed since UNIX_EPOCH) to nanoseconds
/// as `u64`.
/// u64 holds ~584 years of ns since epoch — truncation is theoretical.
fn duration_to_nanos(d: std::time::Duration) -> u64 {
    d.as_nanos() as u64
}

fn cache_dir() -> Option<PathBuf> {
    if crate::paths::is_config_dir_overridden()
        || dirs::config_dir().is_some()
        || cfg!(target_os = "macos") && dirs::home_dir().is_some()
    {
        Some(crate::paths::get_cache_dir())
    } else {
        fallback_cache_dir()
    }
}

fn cache_path() -> Option<PathBuf> {
    Some(cache_dir()?.join(CACHE_FILENAME))
}

fn cache_lock_path() -> Option<PathBuf> {
    Some(cache_dir()?.join(CACHE_LOCK_FILENAME))
}

fn legacy_cache_paths() -> Vec<PathBuf> {
    if crate::paths::is_config_dir_overridden() {
        return Vec::new();
    }

    [
        crate::paths::legacy_dirs_cache_dir().map(|d| d.join(CACHE_FILENAME)),
        crate::paths::legacy_dot_cache_tokscale_dir().map(|d| d.join(CACHE_FILENAME)),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn fallback_cache_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|path| path.join("tokscale"))
        .or_else(user_scoped_temp_dir)
}

#[cfg(unix)]
fn user_scoped_temp_dir() -> Option<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    Some(std::env::temp_dir().join(format!("tokscale-uid-{uid}")))
}

#[cfg(not(unix))]
fn user_scoped_temp_dir() -> Option<PathBuf> {
    std::env::var_os("USERNAME")
        .or_else(|| std::env::var_os("USER"))
        .map(|user| {
            let mut path = std::env::temp_dir();
            path.push(format!("tokscale-user-{}", user.to_string_lossy()));
            path
        })
}

fn ensure_cache_dir(dir: &Path) -> std::io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(dir) {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(std::io::Error::other(
                "cache directory is not a real directory",
            ));
        }
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileSampleHash {
    pub offset: u64,
    pub len: u64,
    pub hash: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceFingerprint {
    pub size: u64,
    pub modified_ns: u64,
    pub sample_hashes: Vec<FileSampleHash>,
    pub content_hash: [u8; 32],
    pub related_files: Vec<RelatedFileFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RelatedFileFingerprint {
    pub suffix: String,
    pub size: u64,
    pub modified_ns: u64,
    pub sample_hashes: Vec<FileSampleHash>,
    pub content_hash: [u8; 32],
}

/// Metadata siblings a Grok `updates.jsonl` session's parse depends on, in the
/// same session directory. `parse_grok_updates_file` reconciles totals from
/// `signals.json` and `read_metadata` additionally reads `summary.json` and
/// `events.jsonl` for the model id. The fingerprint (`from_grok_path`) and every
/// mtime change probe in `lib.rs` (`latest_source_mtime_ms`, `grok_source_mtime_ms`)
/// must watch the *same* set, or a sibling-only write goes unnoticed and the
/// cache serves a stale (fallback-model) session.
pub(crate) const GROK_METADATA_SIBLINGS: [&str; 3] =
    ["signals.json", "summary.json", "events.jsonl"];

impl SourceFingerprint {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        Self::from_path_with_related(path, std::iter::empty())
    }

    pub(crate) fn from_sqlite_path(path: &Path) -> Option<Self> {
        let related_paths = ["-wal"]
            .into_iter()
            .map(|suffix| (suffix.to_string(), append_path_suffix(path, suffix)));
        Self::from_path_with_related(path, related_paths)
    }

    /// Fingerprint for a jcode session snapshot plus its sibling
    /// `<session>.journal.jsonl` append-log. jcode appends new turns to the
    /// journal between snapshot rewrites, so a journal-only write leaves the
    /// snapshot's size/mtime unchanged; including the journal in the fingerprint
    /// invalidates the cache instead of serving stale (missing-turn) data.
    pub(crate) fn from_jcode_path(path: &Path) -> Option<Self> {
        let related_paths =
            std::iter::once((".journal.jsonl".to_string(), jcode_journal_path(path)));
        Self::from_path_with_related(path, related_paths)
    }

    /// Fingerprint a Droid settings snapshot together with the fallback JSONL
    /// that supplies its model when the snapshot omits one.
    pub(crate) fn from_droid_path(path: &Path) -> Option<Self> {
        let Some(jsonl) = crate::sessions::droid::droid_jsonl_path(path) else {
            return Self::from_path(path);
        };
        let related_paths = std::iter::once(("session.jsonl".to_string(), jsonl));
        Self::from_path_with_related(path, related_paths)
    }

    /// Fingerprint a legacy Kimi wire log together with the shared config that
    /// supplies its model.
    pub(crate) fn from_kimi_path(path: &Path) -> Option<Self> {
        let Some(config) = crate::sessions::kimi::kimi_config_path(path) else {
            return Self::from_path(path);
        };
        let related_paths = std::iter::once(("config.json".to_string(), config));
        Self::from_path_with_related(path, related_paths)
    }

    /// Fingerprint a Kiro CLI session header together with its same-stem
    /// message sidecar.
    pub(crate) fn from_kiro_path(path: &Path) -> Option<Self> {
        let Some(messages) = crate::sessions::kiro::kiro_related_messages_path(path) else {
            return Self::from_path(path);
        };
        let related_paths = std::iter::once(("messages.jsonl".to_string(), messages));
        Self::from_path_with_related(path, related_paths)
    }

    /// Fingerprint for a Grok `updates.jsonl` session and every sibling
    /// `read_metadata` consults. `parse_grok_updates_file` reconciles session
    /// totals from `signals.json` (compaction), and `read_metadata` additionally
    /// reads `summary.json` (model id + `updated_at`/`created_at` timestamp) and
    /// `events.jsonl` (model id, session id, `ts` timestamp) — so a sibling that
    /// is written or rewritten after the last `updates.jsonl` write must still
    /// invalidate the cache. An `updates.jsonl`-only (or `signals.json`-only)
    /// fingerprint would leave a session pinned to its fallback model forever
    /// when a late-arriving `summary.json`/`events.jsonl` is the only carrier of
    /// the real model id.
    ///
    /// LOCAL DIVERGENCE from upstream junhoyeo/tokscale, which fingerprints only
    /// `signals.json` here even though its `read_metadata` reads all three — the
    /// same class of gap as our reported #741 (roo history sibling). Must be
    /// re-applied on any re-vendor of `message_cache.rs`; candidate to report
    /// upstream. See vendor/README.md.
    pub(crate) fn from_grok_path(path: &Path) -> Option<Self> {
        let session_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let related_paths = GROK_METADATA_SIBLINGS
            .into_iter()
            .map(|name| (name.to_string(), session_dir.join(name)));
        Self::from_path_with_related(path, related_paths)
    }

    /// Fingerprint for a Roo-family task (`ui_messages.json`) and its sibling
    /// `api_conversation_history.json`. `parse_roo_kilo_file` reads the history
    /// sibling for the model and agent, so a history-only rewrite (the UI file
    /// unchanged) must still invalidate the cache or reports keep stale
    /// model/agent/pricing.
    pub(crate) fn from_roo_path(path: &Path) -> Option<Self> {
        let history = crate::sessions::roocode::history_path_for_ui_messages(path);
        let related_paths = std::iter::once(("api_conversation_history.json".to_string(), history));
        Self::from_path_with_related(path, related_paths)
    }

    /// Fingerprint for a Claude Code JSONL file that may have a sibling `.meta.json`
    /// sidecar. When the sidecar appears or changes (e.g. after a Claude Code upgrade),
    /// the fingerprint changes and the cache invalidates.
    pub(crate) fn from_claude_code_path_with_home(
        path: &Path,
        home_dir: Option<&Path>,
    ) -> Option<Self> {
        let mut related = Vec::new();

        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            let meta_filename = format!("{}.meta.json", stem);
            related.push((".meta.json".to_string(), path.with_file_name(meta_filename)));
        }

        if let Some(variant_path) = crate::cc_mirror::variant_file_for_session_path(path, home_dir)
        {
            related.push(("cc-mirror/variant.json".to_string(), variant_path));
        }
        for (index, parent_path) in
            crate::sessions::claudecode::parent_session_paths_for_cache(path)
                .into_iter()
                .enumerate()
        {
            related.push((format!("parent-session-{index}.jsonl"), parent_path));
        }

        Self::from_path_with_related(path, related)
    }

    fn from_path_with_related<I>(path: &Path, related_paths: I) -> Option<Self>
    where
        I: IntoIterator<Item = (String, PathBuf)>,
    {
        let (size, modified_ns, sample_hashes, content_hash) = file_fingerprint_parts(path)?;
        let mut related_files: Vec<RelatedFileFingerprint> = related_paths
            .into_iter()
            .filter_map(|(suffix, related_path)| {
                RelatedFileFingerprint::from_path(suffix, &related_path)
            })
            .collect();
        related_files.sort_by(|left, right| left.suffix.cmp(&right.suffix));

        Some(Self {
            size,
            modified_ns,
            sample_hashes,
            content_hash,
            related_files,
        })
    }
}

impl RelatedFileFingerprint {
    fn from_path(suffix: String, path: &Path) -> Option<Self> {
        let (size, modified_ns, sample_hashes, content_hash) = file_fingerprint_parts(path)?;
        Some(Self {
            suffix,
            size,
            modified_ns,
            sample_hashes,
            content_hash,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CodexIncrementalCache {
    pub state: CodexParseState,
    pub consumed_offset: u64,
    pub ends_with_newline: bool,
    pub prefix_hash: [u8; 32],
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct CachedPath(Vec<u8>);

#[cfg(unix)]
impl CachedPath {
    pub(crate) fn from_path(path: &Path) -> Self {
        use std::os::unix::ffi::OsStrExt;

        Self(path.as_os_str().as_bytes().to_vec())
    }

    pub(crate) fn to_path_buf(&self) -> PathBuf {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        PathBuf::from(OsString::from_vec(self.0.clone()))
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct CachedPath(Vec<u16>);

#[cfg(windows)]
impl CachedPath {
    pub(crate) fn from_path(path: &Path) -> Self {
        use std::os::windows::ffi::OsStrExt;

        Self(path.as_os_str().encode_wide().collect())
    }

    pub(crate) fn to_path_buf(&self) -> PathBuf {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        PathBuf::from(OsString::from_wide(&self.0))
    }
}

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct CachedPath(String);

#[cfg(not(any(unix, windows)))]
impl CachedPath {
    pub(crate) fn from_path(path: &Path) -> Self {
        Self(path.to_string_lossy().into_owned())
    }

    pub(crate) fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedSourceEntry {
    pub path: CachedPath,
    pub fingerprint: SourceFingerprint,
    pub messages: Vec<UnifiedMessage>,
    pub fallback_timestamp_indices: Vec<usize>,
    pub codex_incremental: Option<CodexIncrementalCache>,
}

impl CachedSourceEntry {
    pub(crate) fn new(
        path: &Path,
        fingerprint: SourceFingerprint,
        messages: Vec<UnifiedMessage>,
        fallback_timestamp_indices: Vec<usize>,
        codex_incremental: Option<CodexIncrementalCache>,
    ) -> Self {
        Self {
            path: CachedPath::from_path(path),
            fingerprint,
            messages,
            fallback_timestamp_indices,
            codex_incremental,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CachedSourceStore {
    schema_version: u32,
    entries: Vec<CachedSourceEntry>,
}

#[derive(Default)]
pub(crate) struct SourceMessageCache {
    pub entries: HashMap<CachedPath, Arc<CachedSourceEntry>>,
    dirty: bool,
    dirty_keys: HashSet<CachedPath>,
    deleted_paths: HashSet<CachedPath>,
}

/// Return the memo's entry map if `path + size + mtime_ns` all match.
/// Returns `None` on lock poison, stat failure, or any mismatch.
fn store_memo_entries_if_current(
    path: &Path,
) -> Option<HashMap<CachedPath, Arc<CachedSourceEntry>>> {
    let guard = STORE_MEMO.lock().ok()?;
    let memo = guard.as_ref()?;
    if memo.cache_file != path {
        return None;
    }
    let meta = fs::metadata(path).ok()?;
    let size = meta.len();
    let modified_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(duration_to_nanos)?;
    if size != memo.file_size || modified_ns != memo.file_modified_ns {
        return None;
    }
    Some(memo.entries.clone())
}

/// Write or replace the STORE_MEMO entry. Silently skips on lock poison or
/// unreadable mtime — a miss on the next `load()` will just re-read disk.
fn store_memo_update(
    path: &Path,
    meta: &std::fs::Metadata,
    entries: &HashMap<CachedPath, Arc<CachedSourceEntry>>,
) {
    let size = meta.len();
    let Some(modified_ns) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(duration_to_nanos)
    else {
        return;
    };
    if let Ok(mut guard) = STORE_MEMO.lock() {
        *guard = Some(StoreMemo {
            cache_file: path.to_path_buf(),
            file_size: size,
            file_modified_ns: modified_ns,
            entries: entries.clone(),
        });
    }
}

/// Load from legacy cache paths without memoizing (path would differ from
/// the canonical path so the memo would never be a valid hit).
fn load_from_legacy_paths() -> SourceMessageCache {
    legacy_cache_paths()
        .into_iter()
        .find_map(|p| read_store_from_path(&p))
        .map(|store| SourceMessageCache {
            entries: store
                .entries
                .into_iter()
                .map(|e| (e.path.clone(), Arc::new(e)))
                .collect(),
            dirty: false,
            dirty_keys: HashSet::new(),
            deleted_paths: HashSet::new(),
        })
        .unwrap_or_default()
}

/// Build the `merged_entries` base for `save_if_dirty`. Tries STORE_MEMO
/// first to skip re-reading the on-disk store; falls back to disk on a miss.
fn save_merge_base(
    final_path: &Path,
) -> HashMap<CachedPath, Arc<CachedSourceEntry>> {
    if let Some(entries) = store_memo_entries_if_current(final_path) {
        return entries;
    }
    read_store_from_path(final_path)
        .map(|store| {
            store
                .entries
                .into_iter()
                .map(|e| (e.path.clone(), Arc::new(e)))
                .collect()
        })
        .unwrap_or_default()
}

impl SourceMessageCache {
    pub(crate) fn load() -> Self {
        let Some(path) = cache_path() else {
            return Self::default();
        };
        let Some(lock_path) = cache_lock_path() else {
            return Self::default();
        };
        if let Some(lock_dir) = lock_path.parent() {
            if ensure_cache_dir(lock_dir).is_err() {
                return Self::default();
            }
        }

        // Check STORE_MEMO before acquiring the file lock. A hit means the
        // on-disk store is almost certainly unchanged — the stat is taken
        // outside the file lock, so a concurrent atomic-rename writer can
        // theoretically race it; (path, size, mtime_ns) collision makes a
        // false hit vanishingly unlikely. Path is part of the key so a
        // test-env config-dir switch never returns data from a different file.
        if let Some(entries) = store_memo_entries_if_current(&path) {
            return Self {
                entries,
                dirty: false,
                dirty_keys: HashSet::new(),
                deleted_paths: HashSet::new(),
            };
        }

        let lock_file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
        {
            Ok(file) => file,
            Err(_) => return Self::default(),
        };
        if fs2::FileExt::lock_shared(&lock_file).is_err() {
            return Self::default();
        }

        // Stat inside the shared lock for a consistent size+mtime pair.
        let locked_meta = fs::metadata(&path).ok();

        let store = match read_store_from_path_status(&path) {
            CacheReadStatus::Loaded(store) => Some(store),
            CacheReadStatus::Missing => {
                // Legacy paths: load but do not memoize (path mismatch).
                return load_from_legacy_paths();
            }
            CacheReadStatus::Invalid => None,
        };
        let Some(store) = store else {
            return Self::default();
        };

        let entries: HashMap<CachedPath, Arc<CachedSourceEntry>> = store
            .entries
            .into_iter()
            .map(|entry| (entry.path.clone(), Arc::new(entry)))
            .collect();

        // Update STORE_MEMO so the next load() on an unchanged file is free.
        if let Some(meta) = locked_meta.as_ref() {
            store_memo_update(&path, meta, &entries);
        }

        Self {
            entries,
            dirty: false,
            dirty_keys: HashSet::new(),
            deleted_paths: HashSet::new(),
        }
    }

    pub(crate) fn insert(&mut self, entry: CachedSourceEntry) {
        let key = entry.path.clone();
        self.entries.insert(key.clone(), Arc::new(entry));
        self.deleted_paths.remove(&key);
        self.dirty_keys.insert(key);
        self.dirty = true;
    }

    pub(crate) fn get(&self, path: &Path) -> Option<&CachedSourceEntry> {
        let key = CachedPath::from_path(path);
        self.entries.get(&key).map(|a| a.as_ref())
    }

    pub(crate) fn remove(&mut self, path: &Path) {
        let key = CachedPath::from_path(path);
        if self.entries.remove(&key).is_some() {
            self.dirty_keys.remove(&key);
            self.deleted_paths.insert(key);
            self.dirty = true;
        }
        // Evict from HASH_MEMO so a deleted session file leaves no stale memo
        // entry. Lock poison → silently skip; next access recomputes.
        if let Ok(mut memo) = HASH_MEMO.lock() {
            memo.remove(path);
        }
    }

    pub(crate) fn prune_missing_files(&mut self) {
        let removed_paths: Vec<CachedPath> = self
            .entries
            .keys()
            .filter(|path| !path.to_path_buf().exists())
            .cloned()
            .collect();
        if removed_paths.is_empty() {
            return;
        }

        for path in &removed_paths {
            let path_buf = path.to_path_buf();
            self.entries.remove(path);
            self.dirty_keys.remove(path);
            self.deleted_paths.insert(path.clone());
            // Evict from HASH_MEMO so deleted session files leave no stale
            // memo entries. Lock poison → silently skip; next access
            // recomputes.
            if let Ok(mut memo) = HASH_MEMO.lock() {
                memo.remove(&path_buf);
            }
        }
        self.dirty = true;
    }

    pub(crate) fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }

        let Some(dir) = cache_dir() else {
            return;
        };
        if ensure_cache_dir(&dir).is_err() {
            return;
        }

        let Some(final_path) = cache_path() else {
            return;
        };
        let Some(lock_path) = cache_lock_path() else {
            return;
        };
        let lock_file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
        {
            Ok(file) => file,
            Err(_) => return,
        };
        if fs2::FileExt::lock_exclusive(&lock_file).is_err() {
            return;
        }

        // Build the merged entries. Try STORE_MEMO first to skip re-reading
        // the on-disk store; fall back to disk read on a miss.
        let mut merged_entries: HashMap<CachedPath, Arc<CachedSourceEntry>> =
            save_merge_base(&final_path);

        for path in &self.deleted_paths {
            if !path.to_path_buf().exists() {
                merged_entries.remove(path);
            }
        }
        for path in &self.dirty_keys {
            if let Some(arc) = self.entries.get(path) {
                merged_entries.insert(path.clone(), Arc::clone(arc));
            }
        }

        let store = CachedSourceStore {
            schema_version: CACHE_SCHEMA_VERSION,
            entries: merged_entries.values().map(|e| (**e).clone()).collect(),
        };

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(duration_to_nanos)
            .unwrap_or(0);
        let tmp_path = dir.join(format!(
            ".{}.{}.{:x}.tmp",
            CACHE_FILENAME,
            std::process::id(),
            nanos
        ));

        // INVARIANT: All cache writes use atomic temp-file rename. NEVER delete
        // the canonical cache file before writing — a partial save or process
        // crash between delete and rename would lose the cache. The temp-file
        // pattern makes corruption-on-crash impossible.
        let write_result = (|| -> std::io::Result<()> {
            let file = File::create(&tmp_path)?;
            let mut writer = BufWriter::new(file);
            bincode::options()
                .with_limit(MAX_CACHE_FILE_BYTES)
                .serialize_into(&mut writer, &store)
                .map_err(std::io::Error::other)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            drop(writer);
            crate::fs_atomic::replace_file(&tmp_path, &final_path)?;
            let final_file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&final_path)?;
            final_file.sync_all()?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&tmp_path);
            return;
        }

        // Stat final_path while exclusive lock is still held, then refresh
        // STORE_MEMO so the next load() skips the disk read (race-safe).
        if let Ok(meta) = fs::metadata(&final_path) {
            store_memo_update(&final_path, &meta, &merged_entries);
        }

        self.entries = merged_entries;
        self.dirty = false;
        self.dirty_keys.clear();
        self.deleted_paths.clear();
    }
}

fn read_store_from_path(path: &Path) -> Option<CachedSourceStore> {
    let file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.len() > MAX_CACHE_FILE_BYTES {
        return None;
    }

    let reader = BufReader::new(file);
    let store: CachedSourceStore = bincode::options()
        .with_limit(MAX_CACHE_FILE_BYTES)
        .deserialize_from(reader)
        .ok()?;
    if store.schema_version != CACHE_SCHEMA_VERSION {
        return None;
    }
    Some(store)
}

enum CacheReadStatus {
    Missing,
    Invalid,
    Loaded(CachedSourceStore),
}

fn read_store_from_path_status(path: &Path) -> CacheReadStatus {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return CacheReadStatus::Missing,
        Err(_) => return CacheReadStatus::Invalid,
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_) => return CacheReadStatus::Invalid,
    };
    if metadata.len() > MAX_CACHE_FILE_BYTES {
        return CacheReadStatus::Invalid;
    }

    let reader = BufReader::new(file);
    let store: CachedSourceStore = match bincode::options()
        .with_limit(MAX_CACHE_FILE_BYTES)
        .deserialize_from(reader)
    {
        Ok(store) => store,
        Err(_) => return CacheReadStatus::Invalid,
    };
    if store.schema_version != CACHE_SCHEMA_VERSION {
        return CacheReadStatus::Invalid;
    }
    CacheReadStatus::Loaded(store)
}

fn read_sample_hash(file: &mut File, offset: u64, len: usize) -> Option<FileSampleHash> {
    if len == 0 {
        return Some(FileSampleHash {
            offset,
            len: 0,
            hash: 0,
        });
    }

    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = vec![0_u8; len];
    file.read_exact(&mut buffer).ok()?;

    Some(FileSampleHash {
        offset,
        len: len as u64,
        hash: hash_bytes(&buffer),
    })
}

fn compute_sample_hashes(path: &Path, size: u64) -> Option<Vec<FileSampleHash>> {
    if size == 0 {
        return Some(Vec::new());
    }

    let mut file = File::open(path).ok()?;
    let offsets = sample_offsets(size);
    offsets
        .into_iter()
        .map(|(offset, len)| read_sample_hash(&mut file, offset, len))
        .collect()
}

fn sample_offsets(size: u64) -> Vec<(u64, usize)> {
    let sample_len = size.min(FINGERPRINT_SAMPLE_BYTES as u64) as usize;
    if sample_len == 0 {
        return Vec::new();
    }

    let max_offset = size.saturating_sub(sample_len as u64);
    let mut offsets = if max_offset == 0 {
        vec![0]
    } else {
        vec![
            0,
            max_offset / 4,
            max_offset / 2,
            max_offset.saturating_mul(3) / 4,
            max_offset,
        ]
    };
    offsets.sort_unstable();
    offsets.dedup();
    offsets.truncate(FINGERPRINT_SAMPLE_POINTS);
    offsets
        .into_iter()
        .map(|offset| (offset, sample_len))
        .collect()
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn file_fingerprint_parts(path: &Path) -> Option<(u64, u64, Vec<FileSampleHash>, [u8; 32])> {
    let metadata = path.metadata().ok()?;
    let size = metadata.len();
    let modified_ns = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(duration_to_nanos)?;

    // Check HASH_MEMO: if (path, size, modified_ns) all match, skip the
    // expensive compute_sample_hashes + hash_prefix (file I/O + SHA-256).
    // Lock poison or stat failure → fall through to full recompute (FAIL-LOUD).
    if let Ok(memo) = HASH_MEMO.lock() {
        if let Some(entry) = memo.get(path) {
            if entry.size == size && entry.modified_ns == modified_ns {
                return Some((size, modified_ns, entry.sample_hashes.clone(), entry.content_hash));
            }
        }
    }

    let sample_hashes = compute_sample_hashes(path, size)?;
    let content_hash = hash_prefix(path, size)?;

    // Update HASH_MEMO. Silently skip on lock poison — next call recomputes.
    if let Ok(mut memo) = HASH_MEMO.lock() {
        memo.insert(
            path.to_path_buf(),
            HashMemoEntry {
                size,
                modified_ns,
                sample_hashes: sample_hashes.clone(),
                content_hash,
            },
        );
    }

    Some((size, modified_ns, sample_hashes, content_hash))
}

fn append_path_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

/// Sibling journal path for a jcode session snapshot: `session_x.json` ->
/// `session_x.journal.jsonl` (replacing the `.json` suffix; falling back to a
/// plain append when the name has no `.json` suffix). `pub(crate)` so the
/// live-tail change-token probe can stat the journal alongside the snapshot.
pub(crate) fn jcode_journal_path(path: &Path) -> PathBuf {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return append_path_suffix(path, ".journal.jsonl");
    };
    let journal_name = file_name
        .strip_suffix(".json")
        .map(|stem| format!("{stem}.journal.jsonl"))
        .unwrap_or_else(|| format!("{file_name}.journal.jsonl"));
    path.with_file_name(journal_name)
}

fn hash_prefix(path: &Path, len: u64) -> Option<[u8; 32]> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];

    while remaining > 0 {
        let bytes_to_read = remaining.min(HASH_BUFFER_BYTES as u64) as usize;
        let read = file.read(&mut buffer[..bytes_to_read]).ok()?;
        if read == 0 {
            return None;
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }

    Some(hasher.finalize().into())
}

pub(crate) fn build_codex_incremental_cache(
    path: &Path,
    consumed_offset: u64,
    state: CodexParseState,
) -> Option<CodexIncrementalCache> {
    let ends_with_newline = consumed_offset == 0 || file_ends_with_newline(path, consumed_offset);
    if !ends_with_newline {
        return None;
    }

    Some(CodexIncrementalCache {
        state,
        consumed_offset,
        ends_with_newline,
        prefix_hash: hash_prefix(path, consumed_offset)?,
    })
}

fn file_ends_with_newline(path: &Path, size: u64) -> bool {
    if size == 0 {
        return true;
    }

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    if file.seek(SeekFrom::Start(size.saturating_sub(1))).is_err() {
        return false;
    }

    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).is_ok() && byte[0] == b'\n'
}

pub(crate) fn codex_prefix_matches(path: &Path, cached: &CodexIncrementalCache) -> bool {
    if cached.consumed_offset > 0 && !cached.ends_with_newline {
        return false;
    }

    match hash_prefix(path, cached.consumed_offset) {
        Some(prefix_hash) => prefix_hash == cached.prefix_hash,
        None => false,
    }
}

pub(crate) fn codex_cache_entry_matches_fingerprint(
    cached: &CachedSourceEntry,
    fingerprint: &SourceFingerprint,
) -> bool {
    let Some(codex_incremental) = cached.codex_incremental.as_ref() else {
        return false;
    };

    codex_incremental.consumed_offset == fingerprint.size
        && codex_incremental.ends_with_newline
        && codex_incremental.prefix_hash == fingerprint.content_hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TokenBreakdown;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn from_jcode_path_invalidates_on_journal_only_change() {
        // jcode appends new turns to `<session>.journal.jsonl` between snapshot
        // rewrites. The cache fingerprint must include the journal, or a
        // journal-only write (snapshot byte-identical) serves stale data.
        let dir = TempDir::new().unwrap();
        let snapshot = dir.path().join("session_test.json");
        std::fs::write(&snapshot, br#"{"id":"session_test","messages":[]}"#).unwrap();
        let journal = dir.path().join("session_test.journal.jsonl");
        std::fs::write(&journal, b"{\"append_messages\":[]}\n").unwrap();

        let jcode_before = SourceFingerprint::from_jcode_path(&snapshot).unwrap();
        let plain_before = SourceFingerprint::from_path(&snapshot).unwrap();

        // Append to the journal only; leave the snapshot byte-identical.
        std::fs::write(
            &journal,
            b"{\"append_messages\":[]}\n{\"append_messages\":[{\"id\":\"x\"}]}\n",
        )
        .unwrap();

        let jcode_after = SourceFingerprint::from_jcode_path(&snapshot).unwrap();
        let plain_after = SourceFingerprint::from_path(&snapshot).unwrap();

        assert_ne!(
            jcode_before, jcode_after,
            "a journal-only change must alter the jcode fingerprint"
        );
        assert_eq!(
            plain_before, plain_after,
            "from_path ignores the journal sibling (control)"
        );
    }

    #[test]
    fn from_roo_path_invalidates_on_history_only_change() {
        // parse_roo_kilo_file reads model/agent from the sibling
        // api_conversation_history.json, so a history-only rewrite (ui_messages
        // byte-identical) must change the fingerprint or the cache serves stale
        // model/agent/pricing.
        let dir = TempDir::new().unwrap();
        let ui = dir.path().join("ui_messages.json");
        std::fs::write(&ui, b"[]").unwrap();
        let history = dir.path().join("api_conversation_history.json");
        std::fs::write(&history, b"<model>claude-sonnet-4</model>").unwrap();

        let roo_before = SourceFingerprint::from_roo_path(&ui).unwrap();
        let plain_before = SourceFingerprint::from_path(&ui).unwrap();

        // Rewrite the history only; leave ui_messages.json byte-identical.
        std::fs::write(&history, b"<model>claude-opus-4</model>").unwrap();

        let roo_after = SourceFingerprint::from_roo_path(&ui).unwrap();
        let plain_after = SourceFingerprint::from_path(&ui).unwrap();

        assert_ne!(
            roo_before, roo_after,
            "a history-only change must alter the roo fingerprint"
        );
        assert_eq!(
            plain_before, plain_after,
            "from_path ignores the history sibling (control)"
        );
    }

    #[test]
    fn from_roo_path_invalidates_when_history_appears() {
        let dir = TempDir::new().unwrap();
        let ui = dir.path().join("ui_messages.json");
        std::fs::write(&ui, b"[]").unwrap();

        let before = SourceFingerprint::from_roo_path(&ui).unwrap();
        std::fs::write(
            crate::sessions::roocode::history_path_for_ui_messages(&ui),
            b"<environment_details><model>gpt-5</model></environment_details>",
        )
        .unwrap();
        let after = SourceFingerprint::from_roo_path(&ui).unwrap();

        assert_ne!(
            before, after,
            "creating the optional history sibling must invalidate the roo fingerprint"
        );
    }

    #[test]
    fn from_droid_path_invalidates_when_fallback_jsonl_appears() {
        let dir = TempDir::new().unwrap();
        let settings = dir.path().join("session.settings.json");
        std::fs::write(&settings, br#"{"tokenUsage":{"inputTokens":1}}"#).unwrap();

        let before = SourceFingerprint::from_droid_path(&settings).unwrap();
        let plain_before = SourceFingerprint::from_path(&settings).unwrap();
        let jsonl = crate::sessions::droid::droid_jsonl_path(&settings).unwrap();
        assert_eq!(jsonl, dir.path().join("session.jsonl"));
        assert!(before.related_files.is_empty());

        std::fs::write(&jsonl, b"Model: Claude Sonnet 4\n").unwrap();

        let after = SourceFingerprint::from_droid_path(&settings).unwrap();
        let plain_after = SourceFingerprint::from_path(&settings).unwrap();
        assert_ne!(before, after);
        assert_eq!(plain_before, plain_after);
        assert_eq!(after.related_files[0].suffix, "session.jsonl");

        std::fs::write(&jsonl, b"Model: Claude Opus 4.5 Thinking\n").unwrap();
        let rewritten = SourceFingerprint::from_droid_path(&settings).unwrap();
        assert_ne!(after, rewritten);
        assert_eq!(
            plain_after,
            SourceFingerprint::from_path(&settings).unwrap()
        );
    }

    #[test]
    fn from_kimi_path_invalidates_when_config_appears() {
        let dir = TempDir::new().unwrap();
        let wire = dir.path().join(".kimi/sessions/group/session/wire.jsonl");
        std::fs::create_dir_all(wire.parent().unwrap()).unwrap();
        std::fs::write(&wire, b"usage\n").unwrap();

        let before = SourceFingerprint::from_kimi_path(&wire).unwrap();
        let plain_before = SourceFingerprint::from_path(&wire).unwrap();
        let config = crate::sessions::kimi::kimi_config_path(&wire).unwrap();
        assert_eq!(config, dir.path().join(".kimi/config.json"));
        assert!(before.related_files.is_empty());

        std::fs::write(&config, br#"{"model":"kimi-k2"}"#).unwrap();

        let after = SourceFingerprint::from_kimi_path(&wire).unwrap();
        let plain_after = SourceFingerprint::from_path(&wire).unwrap();
        assert_ne!(before, after);
        assert_eq!(plain_before, plain_after);
        assert_eq!(after.related_files[0].suffix, "config.json");

        std::fs::write(&config, br#"{"model":"kimi-k2-thinking"}"#).unwrap();
        let rewritten = SourceFingerprint::from_kimi_path(&wire).unwrap();
        assert_ne!(after, rewritten);
        assert_eq!(plain_after, SourceFingerprint::from_path(&wire).unwrap());
    }

    #[test]
    fn from_kiro_path_invalidates_when_messages_sidecar_appears() {
        let dir = TempDir::new().unwrap();
        let session = dir.path().join("session.json");
        std::fs::write(&session, b"{}").unwrap();

        let before = SourceFingerprint::from_kiro_path(&session).unwrap();
        let plain_before = SourceFingerprint::from_path(&session).unwrap();
        let messages = crate::sessions::kiro::kiro_related_messages_path(&session).unwrap();
        assert_eq!(messages, dir.path().join("session.jsonl"));
        assert!(before.related_files.is_empty());

        std::fs::write(&messages, b"message\n").unwrap();

        let after = SourceFingerprint::from_kiro_path(&session).unwrap();
        let plain_after = SourceFingerprint::from_path(&session).unwrap();
        assert_ne!(before, after);
        assert_eq!(plain_before, plain_after);
        assert_eq!(after.related_files[0].suffix, "messages.jsonl");

        std::fs::write(&messages, b"rewritten message sidecar\n").unwrap();
        let rewritten = SourceFingerprint::from_kiro_path(&session).unwrap();
        assert_ne!(after, rewritten);
        assert_eq!(plain_after, SourceFingerprint::from_path(&session).unwrap());
    }

    #[test]
    fn from_grok_path_invalidates_on_summary_only_change() {
        // read_metadata reads the model id (and timestamp) from the sibling
        // summary.json when updates.jsonl carries no per-turn model, so a
        // summary-only rewrite (updates.jsonl + signals.json byte-identical) must
        // change the fingerprint or the cache keeps a session pinned to its
        // fallback model. Local divergence: upstream fingerprints only
        // signals.json here.
        let dir = TempDir::new().unwrap();
        let updates = dir.path().join("updates.jsonl");
        std::fs::write(&updates, b"{\"totalTokens\":10}\n").unwrap();
        let summary = dir.path().join("summary.json");
        std::fs::write(&summary, br#"{"current_model_id":"grok-4"}"#).unwrap();

        let grok_before = SourceFingerprint::from_grok_path(&updates).unwrap();
        let plain_before = SourceFingerprint::from_path(&updates).unwrap();

        // Rewrite summary.json only; leave updates.jsonl byte-identical.
        std::fs::write(&summary, br#"{"current_model_id":"grok-4-fast"}"#).unwrap();

        let grok_after = SourceFingerprint::from_grok_path(&updates).unwrap();
        let plain_after = SourceFingerprint::from_path(&updates).unwrap();

        assert_ne!(
            grok_before, grok_after,
            "a summary-only change must alter the grok fingerprint"
        );
        assert_eq!(
            plain_before, plain_after,
            "from_path ignores the summary sibling (control)"
        );
    }

    #[test]
    fn from_grok_path_invalidates_on_events_only_change() {
        // events.jsonl is the other metadata sibling read_metadata consults for
        // the model id / session id / timestamp, so an events-only rewrite must
        // invalidate the cache too.
        let dir = TempDir::new().unwrap();
        let updates = dir.path().join("updates.jsonl");
        std::fs::write(&updates, b"{\"totalTokens\":10}\n").unwrap();
        let events = dir.path().join("events.jsonl");
        std::fs::write(&events, b"{\"model_id\":\"grok-4\"}\n").unwrap();

        let before = SourceFingerprint::from_grok_path(&updates).unwrap();

        std::fs::write(&events, b"{\"model_id\":\"grok-4-fast\"}\n").unwrap();

        let after = SourceFingerprint::from_grok_path(&updates).unwrap();

        assert_ne!(
            before, after,
            "an events-only change must alter the grok fingerprint"
        );
    }

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            Self(
                keys.iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            )
        }

        fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) {
            unsafe { std::env::set_var(key, value) };
        }

        fn remove(&mut self, key: &'static str) {
            unsafe { std::env::remove_var(key) };
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

    /// Pin every env var the cache resolvers consult so the test stays
    /// inside `temp_home`, including on Windows where HOME/XDG do not control
    /// the platform known folders. The override also keeps canonical cache
    /// tests from reading or writing a real profile.
    fn sandbox_cache_env(temp_home: &std::path::Path) -> EnvGuard {
        let config_dir = temp_home.join(".config");
        let cache_dir = temp_home.join(".cache");
        let mut guard = EnvGuard::capture(&[
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "TOKSCALE_CONFIG_DIR",
        ]);
        guard.set("HOME", temp_home);
        guard.set("XDG_CONFIG_HOME", &config_dir);
        guard.set("XDG_CACHE_HOME", &cache_dir);
        guard.set("TOKSCALE_CONFIG_DIR", temp_home);
        guard
    }

    /// Set legacy roots without the override for the non-Windows migration
    /// tests. The returned guard restores every process environment variable
    /// on normal return and during unwinding.
    fn legacy_cache_env(
        temp_home: &std::path::Path,
        xdg_cache: Option<&std::path::Path>,
    ) -> EnvGuard {
        let config_dir = temp_home.join(".config");
        let mut guard = EnvGuard::capture(&[
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "TOKSCALE_CONFIG_DIR",
        ]);
        guard.set("HOME", temp_home);
        guard.set("XDG_CONFIG_HOME", &config_dir);
        match xdg_cache {
            Some(path) => guard.set("XDG_CACHE_HOME", path),
            None => guard.remove("XDG_CACHE_HOME"),
        }
        guard.remove("TOKSCALE_CONFIG_DIR");
        guard
    }

    fn restore_cache_env(guard: EnvGuard) {
        drop(guard);
    }

    #[test]
    #[serial_test::serial]
    fn test_env_guard_restores_after_unwind() {
        const KEY: &str = "TOKSCALE_MESSAGE_CACHE_ENV_GUARD_SELF_CHECK";
        let mut outer = EnvGuard::capture(&[KEY]);
        outer.set(KEY, "before");
        let result = std::panic::catch_unwind(|| {
            let mut inner = EnvGuard::capture(&[KEY]);
            inner.set(KEY, "during");
            panic!("exercise EnvGuard unwinding");
        });
        assert!(result.is_err());
        assert_eq!(std::env::var_os(KEY), Some("before".into()));
    }

    fn write_temp_file(content: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn test_codex_prefix_matches_appended_file() {
        let file = write_temp_file(b"line-1\nline-2\n");
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let incremental_cache = build_codex_incremental_cache(
            file.path(),
            fingerprint.size,
            CodexParseState::default(),
        )
        .unwrap();

        let mut reopened = file.reopen().unwrap();
        reopened.seek(SeekFrom::End(0)).unwrap();
        reopened.write_all(b"line-3\n").unwrap();
        reopened.flush().unwrap();

        assert!(codex_prefix_matches(file.path(), &incremental_cache,));
    }

    #[test]
    fn test_source_fingerprint_changes_for_same_size_rewrite() {
        let file = write_temp_file(b"aaaa\nbbbb\ncccc\n");
        let before = SourceFingerprint::from_path(file.path()).unwrap();

        // Windows can retain the same timestamp for a fast same-size rewrite;
        // use a changed-length fixture there instead of sleeping or changing
        // the production fingerprint memo semantics.
        #[cfg(not(target_os = "windows"))]
        let rewritten = b"aaaa\nzzzz\ncccc\n";
        #[cfg(target_os = "windows")]
        let rewritten = b"aaaa\nzzzz\ncccc\nchanged-length\n";
        std::fs::write(file.path(), rewritten).unwrap();

        let after = SourceFingerprint::from_path(file.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn test_source_fingerprint_changes_for_large_same_size_unsampled_rewrite() {
        let mut original = vec![b'a'; 128 * 1024];
        original.extend_from_slice(b"\n");
        let file = write_temp_file(&original);
        let before = SourceFingerprint::from_path(file.path()).unwrap();

        let mut rewritten = original.clone();
        rewritten[73 * 1024] = b'z';
        // Keep the unsampled same-size rewrite on Unix. Windows filesystem
        // timestamp precision can memoize a rapid rewrite, so make its
        // fixture length-changing without adding a sleep.
        #[cfg(target_os = "windows")]
        rewritten.extend_from_slice(b"changed-length\n");
        std::fs::write(file.path(), &rewritten).unwrap();

        let after = SourceFingerprint::from_path(file.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn test_sqlite_source_fingerprint_tracks_sidecar_changes() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("history.db");
        std::fs::write(&db_path, b"main-db").unwrap();

        let base = SourceFingerprint::from_sqlite_path(&db_path).unwrap();

        let wal_path = append_path_suffix(&db_path, "-wal");
        std::fs::write(&wal_path, b"wal-1").unwrap();
        let with_wal = SourceFingerprint::from_sqlite_path(&db_path).unwrap();
        assert_ne!(base, with_wal);

        std::fs::write(&wal_path, b"wal-2-changed-length").unwrap();
        let updated_wal = SourceFingerprint::from_sqlite_path(&db_path).unwrap();
        assert_ne!(with_wal, updated_wal);

        let before_shm = SourceFingerprint::from_sqlite_path(&db_path).unwrap();
        let shm_path = append_path_suffix(&db_path, "-shm");
        std::fs::write(&shm_path, b"shm-1").unwrap();
        let with_shm = SourceFingerprint::from_sqlite_path(&db_path).unwrap();
        assert_eq!(before_shm, with_shm);
    }

    #[test]
    fn test_claude_sidechain_fingerprint_tracks_nested_parent_session_changes() {
        let dir = TempDir::new().unwrap();
        let project_dir = dir.path().join("projects/project-one");
        let sidechain_path = project_dir
            .join("parent-session/subagents")
            .join("agent-child.jsonl");
        std::fs::create_dir_all(sidechain_path.parent().unwrap()).unwrap();
        std::fs::write(
            &sidechain_path,
            concat!(
                r#"{"type":"assistant","isSidechain":true,"sessionId":"parent-session","agentId":"child","timestamp":"2026-01-01T00:00:00Z","requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":1}}}"#,
                "\n"
            ),
        )
        .unwrap();

        let parent_path =
            crate::sessions::claudecode::parent_session_paths_for_cache(&sidechain_path)
                .into_iter()
                .next()
                .unwrap();
        assert_eq!(parent_path, project_dir.join("parent-session.jsonl"));
        let base =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();

        std::fs::write(&parent_path, b"parent transcript 1\n").unwrap();
        let with_parent =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();
        assert_ne!(base, with_parent);

        std::fs::write(&parent_path, b"parent transcript 2 with changed length\n").unwrap();
        let updated_parent =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();
        assert_ne!(with_parent, updated_parent);
    }

    #[test]
    fn test_claude_sidechain_fingerprint_tracks_flat_parent_session_changes() {
        let dir = TempDir::new().unwrap();
        let project_dir = dir.path().join("projects/project-one");
        std::fs::create_dir_all(&project_dir).unwrap();
        let sidechain_path = project_dir.join("agent-child.jsonl");
        let mut sidechain = format!("{}\n", "x".repeat(4096)).repeat(65);
        sidechain.push_str(concat!(
            r#"{"type":"assistant","isSidechain":true,"sessionId":"flat-parent","agentId":"child","timestamp":"2026-01-01T00:00:00Z","requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            "\n"
        ));
        std::fs::write(&sidechain_path, sidechain).unwrap();

        let parent_path =
            crate::sessions::claudecode::parent_session_paths_for_cache(&sidechain_path)
                .into_iter()
                .next()
                .unwrap();
        assert_eq!(parent_path, project_dir.join("flat-parent.jsonl"));
        let base =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();

        std::fs::write(&parent_path, b"flat parent 1\n").unwrap();
        let with_parent =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();
        assert_ne!(base, with_parent);

        std::fs::write(&parent_path, b"flat parent 2 with changed length\n").unwrap();
        let updated_parent =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();
        assert_ne!(with_parent, updated_parent);
    }

    #[test]
    fn test_claude_code_fingerprint_tracks_meta_sidecar_changes() {
        let dir = TempDir::new().unwrap();
        let jsonl_path = dir.path().join("agent-abc123.jsonl");
        std::fs::write(&jsonl_path, b"jsonl-content").unwrap();

        // No meta sidecar → baseline fingerprint
        let base = SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();

        // Add meta sidecar → fingerprint changes
        let meta_path = dir.path().join("agent-abc123.meta.json");
        std::fs::write(&meta_path, br#"{"agentType":"explore"}"#).unwrap();
        let with_meta =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();
        assert_ne!(
            base, with_meta,
            "Adding meta sidecar should change fingerprint"
        );

        // Update meta sidecar → fingerprint changes again
        std::fs::write(&meta_path, br#"{"agentType":"executor"}"#).unwrap();
        let updated_meta =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();
        assert_ne!(
            with_meta, updated_meta,
            "Updating meta sidecar should change fingerprint"
        );

        // Main session file (no agent- prefix) → unaffected by unrelated meta files
        let main_path = dir.path().join("session-uuid.jsonl");
        std::fs::write(&main_path, b"main-session").unwrap();
        let main_fp1 =
            SourceFingerprint::from_claude_code_path_with_home(&main_path, None).unwrap();
        // Create a meta file with the main session stem (unlikely in practice)
        let main_meta = dir.path().join("session-uuid.meta.json");
        std::fs::write(&main_meta, br#"{"agentType":"x"}"#).unwrap();
        let main_fp2 =
            SourceFingerprint::from_claude_code_path_with_home(&main_path, None).unwrap();
        assert_ne!(
            main_fp1, main_fp2,
            "Claude Code fingerprints always track .meta.json if it exists"
        );
    }

    #[test]
    fn test_claude_code_fingerprint_tracks_cc_mirror_variant_metadata_changes() {
        let dir = TempDir::new().unwrap();
        let variant_dir = dir.path().join(".cc-mirror/kimi-code");
        let config_dir = variant_dir.join("config");
        let project_dir = config_dir.join("projects/project-one");
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl_path = project_dir.join("session.jsonl");
        std::fs::write(&jsonl_path, b"jsonl-content").unwrap();

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
        let with_kimi =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();

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
        let with_minimax =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();

        assert_ne!(
            with_kimi, with_minimax,
            "Changing cc-mirror provider metadata should invalidate parsed Claude cache entries"
        );
    }

    #[test]
    fn test_claude_code_fingerprint_tracks_cc_mirror_custom_config_dir_metadata_changes() {
        let dir = TempDir::new().unwrap();
        let variant_dir = dir.path().join(".cc-mirror/kimi-code");
        let config_dir = dir.path().join("mirror-configs/kimi-code");
        let project_dir = config_dir.join("projects/project-one");
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl_path = project_dir.join("session.jsonl");
        std::fs::write(&jsonl_path, b"jsonl-content").unwrap();

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
        let with_kimi =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, Some(dir.path()))
                .unwrap();

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
        let with_minimax =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, Some(dir.path()))
                .unwrap();

        assert_ne!(
            with_kimi, with_minimax,
            "Changing cc-mirror metadata should invalidate cache entries for custom configDir layouts"
        );
    }

    #[test]
    fn test_codex_incremental_cache_requires_newline_boundary() {
        let file = write_temp_file(b"line-1\nline-2");

        assert!(build_codex_incremental_cache(
            file.path(),
            file.as_file().metadata().unwrap().len(),
            CodexParseState::default(),
        )
        .is_none());
    }

    #[test]
    fn test_codex_prefix_matches_rejects_middle_rewrite_with_same_tail() {
        let file = write_temp_file(b"aaaa\nbbbb\ncccc\n");
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let incremental_cache = build_codex_incremental_cache(
            file.path(),
            fingerprint.size,
            CodexParseState::default(),
        )
        .unwrap();

        std::fs::write(file.path(), b"aaaa\nzzzz\ncccc\nmore\n").unwrap();

        assert!(!codex_prefix_matches(file.path(), &incremental_cache));
    }

    #[test]
    fn test_codex_prefix_matches_rejects_large_unsampled_rewrite() {
        let mut original = vec![b'a'; 128 * 1024];
        original.extend_from_slice(b"\n");
        let file = write_temp_file(&original);
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let incremental_cache = build_codex_incremental_cache(
            file.path(),
            fingerprint.size,
            CodexParseState::default(),
        )
        .unwrap();

        let mut rewritten = original.clone();
        rewritten[73 * 1024] = b'z';
        rewritten.extend_from_slice(b"appended\n");
        std::fs::write(file.path(), rewritten).unwrap();

        assert!(!codex_prefix_matches(file.path(), &incremental_cache));
    }

    #[test]
    #[serial_test::serial]
    fn test_source_message_cache_round_trip() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        let file = write_temp_file(b"{}\n");
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let entry = CachedSourceEntry::new(
            file.path(),
            fingerprint,
            vec![UnifiedMessage::new(
                "client",
                "gpt-5",
                "provider",
                "session-1",
                1,
                TokenBreakdown {
                    input: 1,
                    output: 2,
                    cache_read: 3,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0,
            )],
            Vec::new(),
            None,
        );

        let mut cache = SourceMessageCache::default();
        cache.insert(entry);
        cache.save_if_dirty();

        let loaded = SourceMessageCache::load();
        assert_eq!(loaded.entries.len(), 1);
        assert!(loaded.get(file.path()).is_some());

        restore_cache_env(prev_env);
    }

    #[test]
    fn test_prune_missing_files_removes_deleted_entries() {
        let file = write_temp_file(b"{}\n");
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let path = file.path().to_path_buf();

        let mut cache = SourceMessageCache::default();
        cache.insert(CachedSourceEntry::new(
            &path,
            fingerprint,
            Vec::new(),
            Vec::new(),
            None,
        ));

        std::fs::remove_file(&path).unwrap();
        cache.prune_missing_files();

        assert!(cache.entries.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn test_load_ignores_oversized_cache_file() {
        let temp_home = TempDir::new().unwrap();
        let _env = sandbox_cache_env(temp_home.path());

        let cache_file = cache_path().unwrap();
        ensure_cache_dir(cache_file.parent().unwrap()).unwrap();
        let file = File::create(&cache_file).unwrap();
        file.set_len(MAX_CACHE_FILE_BYTES + 1).unwrap();

        let loaded = SourceMessageCache::load();
        assert!(loaded.entries.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn test_schema_24_cache_is_stale_and_rebuilt_as_current_schema() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source = write_temp_file(b"schema-migration\n");
            let source_fingerprint = SourceFingerprint::from_path(source.path()).unwrap();
            let mut stale_message = UnifiedMessage::new(
                "client",
                "model",
                "provider",
                "stale-session",
                1,
                TokenBreakdown::default(),
                0.0,
            );
            stale_message.is_turn_start = true;
            let stale_entry = CachedSourceEntry::new(
                source.path(),
                source_fingerprint.clone(),
                vec![stale_message],
                Vec::new(),
                None,
            );
            let cache_file = cache_path().unwrap();
            ensure_cache_dir(cache_file.parent().unwrap()).unwrap();
            let stale_store = CachedSourceStore {
                schema_version: 24,
                entries: vec![stale_entry],
            };

            let writer = BufWriter::new(File::create(&cache_file).unwrap());
            bincode::options()
                .serialize_into(writer, &stale_store)
                .unwrap();

            let mut loaded = SourceMessageCache::load();
            assert!(loaded.entries.is_empty(), "schema-24 entries must be stale");
            assert_eq!(
                SourceFingerprint::from_path(source.path()).unwrap(),
                source_fingerprint,
                "the stale cache entry and rebuilt parse have the same source fingerprint"
            );

            let rebuilt_message = UnifiedMessage::new(
                "client",
                "model",
                "provider",
                "rebuilt-session",
                2,
                TokenBreakdown::default(),
                0.0,
            );
            loaded.insert(CachedSourceEntry::new(
                source.path(),
                source_fingerprint,
                vec![rebuilt_message],
                Vec::new(),
                None,
            ));
            loaded.save_if_dirty();

            let rebuilt = read_store_from_path(&cache_file).unwrap();
            assert_eq!(rebuilt.schema_version, CACHE_SCHEMA_VERSION);
            assert_eq!(rebuilt.entries.len(), 1);
            assert_eq!(
                rebuilt.entries[0].messages[0].cost_source,
                crate::sessions::CostSource::Unknown
            );
            assert_eq!(rebuilt.entries[0].messages[0].session_id, "rebuilt-session");
            assert!(
                !rebuilt.entries[0].messages[0].is_turn_start,
                "stale schema-24 turn flags must not survive the rebuild"
            );
        }

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_schema_25_pi_cache_is_stale_and_rebuilt_with_agent() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source = write_temp_file(b"pi-schema-migration\n");
            let source_fingerprint = SourceFingerprint::from_path(source.path()).unwrap();
            let stale_message = UnifiedMessage::new(
                "pi",
                "gpt-5",
                "openai",
                "pi-session",
                1,
                TokenBreakdown::default(),
                0.0,
            );
            let stale_entry = CachedSourceEntry::new(
                source.path(),
                source_fingerprint.clone(),
                vec![stale_message],
                Vec::new(),
                None,
            );
            let cache_file = cache_path().unwrap();
            ensure_cache_dir(cache_file.parent().unwrap()).unwrap();
            let stale_store = CachedSourceStore {
                schema_version: 25,
                entries: vec![stale_entry],
            };

            let writer = BufWriter::new(File::create(&cache_file).unwrap());
            bincode::options()
                .serialize_into(writer, &stale_store)
                .unwrap();

            let mut loaded = SourceMessageCache::load();
            assert!(
                loaded.entries.is_empty(),
                "schema-25 Pi entries must be stale"
            );
            assert_eq!(
                SourceFingerprint::from_path(source.path()).unwrap(),
                source_fingerprint,
                "the stale cache entry and rebuilt parse have the same source fingerprint"
            );

            let rebuilt_message = UnifiedMessage::new_with_agent(
                "pi",
                "gpt-5",
                "openai",
                "pi-session",
                2,
                TokenBreakdown::default(),
                0.0,
                Some("go-reviewer".to_string()),
            );
            loaded.insert(CachedSourceEntry::new(
                source.path(),
                source_fingerprint,
                vec![rebuilt_message],
                Vec::new(),
                None,
            ));
            loaded.save_if_dirty();

            let rebuilt = read_store_from_path(&cache_file).unwrap();
            assert_eq!(rebuilt.schema_version, CACHE_SCHEMA_VERSION);
            assert_eq!(rebuilt.entries.len(), 1);

            let reloaded = SourceMessageCache::load();
            assert_eq!(reloaded.entries.len(), 1);
            assert_eq!(
                reloaded.entries.values().next().unwrap().messages[0]
                    .agent
                    .as_deref(),
                Some("go-reviewer")
            );
        }

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_schema_26_claude_workflow_cache_is_stale_and_rebuilt_with_parent_agent() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let project_dir = temp_home.path().join(".claude/projects/project-one");
            let parent_session_id = "claude-schema-parent";
            let parent_path = project_dir.join(format!("{parent_session_id}.jsonl"));
            let workflow_dir = project_dir
                .join(parent_session_id)
                .join("subagents")
                .join("workflows")
                .join("wf_schema");
            let sidechain_path = workflow_dir.join("agent-deepagent1.jsonl");
            std::fs::create_dir_all(&workflow_dir).unwrap();

            std::fs::write(
                &parent_path,
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","message":{"id":"msg_parent","model":"claude-3-5-sonnet","role":"assistant","content":[{"type":"tool_use","id":"toolu_schema","name":"Agent","input":{"subagent_type":"code-reviewer","prompt":"review"}}],"usage":{"input_tokens":80,"output_tokens":40}}}
{"type":"user","timestamp":"2024-12-01T10:00:01.000Z","message":{"role":"user","content":[{"tool_use_id":"toolu_schema","type":"tool_result","content":[{"type":"text","text":"agentId: deepagent1 (use SendMessage)"}]}]}}"#,
            )
            .unwrap();
            std::fs::write(
                &sidechain_path,
                r#"{"type":"user","isSidechain":true,"sessionId":"claude-schema-parent","agentId":"deepagent1","timestamp":"2024-12-01T10:00:00.500Z","message":{"content":"task"}}
{"type":"assistant","isSidechain":true,"sessionId":"claude-schema-parent","agentId":"deepagent1","timestamp":"2024-12-01T10:00:01.000Z","requestId":"req_schema","message":{"id":"msg_schema","model":"claude-3-5-sonnet","usage":{"input_tokens":300,"output_tokens":120}}}"#,
            )
            .unwrap();

            let source_fingerprint = SourceFingerprint::from_claude_code_path_with_home(
                &sidechain_path,
                Some(temp_home.path()),
            )
            .unwrap();
            let stale_message = UnifiedMessage::new(
                "claude",
                "claude-sonnet-4",
                "anthropic",
                parent_session_id,
                1,
                TokenBreakdown {
                    input: 300,
                    output: 120,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0,
            );
            assert!(stale_message.agent.is_none());
            let stale_entry = CachedSourceEntry::new(
                &sidechain_path,
                source_fingerprint.clone(),
                vec![stale_message],
                Vec::new(),
                None,
            );
            let cache_file = cache_path().unwrap();
            ensure_cache_dir(cache_file.parent().unwrap()).unwrap();
            let stale_store = CachedSourceStore {
                schema_version: 26,
                entries: vec![stale_entry],
            };

            let writer = BufWriter::new(File::create(&cache_file).unwrap());
            bincode::options()
                .serialize_into(writer, &stale_store)
                .unwrap();

            let mut loaded = SourceMessageCache::load();
            assert!(
                loaded.entries.is_empty(),
                "schema-26 Claude workflow entries must be stale"
            );
            assert_eq!(
                SourceFingerprint::from_claude_code_path_with_home(
                    &sidechain_path,
                    Some(temp_home.path())
                )
                .unwrap(),
                source_fingerprint,
                "the stale cache entry and rebuilt parse have the same source fingerprint"
            );

            let rebuilt_messages = crate::sessions::claudecode::parse_claude_file_with_home(
                &sidechain_path,
                Some(temp_home.path()),
            );
            assert_eq!(rebuilt_messages.len(), 1);
            assert_eq!(
                rebuilt_messages[0].agent.as_deref(),
                Some("Code Reviewer"),
                "rebuilding must recover Tier-2 attribution from the parent session"
            );
            loaded.insert(CachedSourceEntry::new(
                &sidechain_path,
                source_fingerprint,
                rebuilt_messages,
                Vec::new(),
                None,
            ));
            loaded.save_if_dirty();

            let rebuilt = read_store_from_path(&cache_file).unwrap();
            assert_eq!(rebuilt.schema_version, CACHE_SCHEMA_VERSION);
            assert_eq!(rebuilt.entries.len(), 1);
            assert_eq!(
                rebuilt.entries[0].messages[0].agent.as_deref(),
                Some("Code Reviewer")
            );

            let reloaded = SourceMessageCache::load();
            assert_eq!(reloaded.entries.len(), 1);
            assert_eq!(
                reloaded.entries.values().next().unwrap().messages[0]
                    .agent
                    .as_deref(),
                Some("Code Reviewer")
            );
        }

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_schema_27_copilot_cache_is_stale_and_rebuilt_with_root_agent() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            // The nested invoke_agent is exported before the root invoke_agent,
            // while the agentless chat turn is covered by the root span. The
            // intermediate tool-task span deliberately has no attributes: the
            // pre-hardening resolver dropped its tool-task -> invoke-root edge,
            // making the nested invoke look like a root and poisoning the
            // schema-27 fallback.
            let source = write_temp_file(
                br#"{"type":"span","traceId":"trace-nested","spanId":"invoke-sub","parentSpanId":"tool-task","name":"invoke_agent","endTime":[1775934261,0],"attributes":{"gen_ai.operation.name":"invoke_agent","gen_ai.provider.name":"github","gen_ai.request.model":"claude-sonnet-4.6","gen_ai.agent.id":"github.copilot.subagent"}}
{"type":"span","traceId":"trace-nested","spanId":"chat-sub","parentSpanId":"invoke-sub","name":"chat claude-sonnet-4.6","endTime":[1775934262,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.provider.name":"github","gen_ai.request.model":"claude-sonnet-4.6","gen_ai.response.model":"claude-sonnet-4.6","gen_ai.response.id":"resp-sub","gen_ai.agent.id":"github.copilot.subagent","gen_ai.usage.input_tokens":100,"gen_ai.usage.output_tokens":10}}
{"type":"span","traceId":"trace-nested","spanId":"tool-task","parentSpanId":"invoke-root","name":"execute_tool task","endTime":[1775934263,0]}
{"type":"span","traceId":"trace-nested","spanId":"invoke-root","name":"invoke_agent","endTime":[1775934260,0],"attributes":{"gen_ai.operation.name":"invoke_agent","gen_ai.provider.name":"github","gen_ai.request.model":"claude-sonnet-4.6","gen_ai.agent.id":"github.copilot.default"}}
{"type":"span","traceId":"trace-nested","spanId":"chat-plain","parentSpanId":"invoke-root","name":"chat gpt-5.4-mini","endTime":[1775934264,967317833],"attributes":{"gen_ai.operation.name":"chat","gen_ai.provider.name":"github","gen_ai.request.model":"gpt-5.4-mini","gen_ai.response.model":"gpt-5.4-mini","gen_ai.response.id":"resp-plain","gen_ai.usage.input_tokens":200,"gen_ai.usage.output_tokens":20}}"#,
            );
            let source_fingerprint = SourceFingerprint::from_path(source.path()).unwrap();
            let parsed_messages = crate::sessions::copilot::parse_copilot_file(source.path());
            assert_eq!(parsed_messages.len(), 2);
            assert_eq!(
                parsed_messages
                    .iter()
                    .find(|message| message.model_id == "gpt-5.4-mini")
                    .unwrap()
                    .agent
                    .as_deref(),
                Some("github.copilot.default")
            );
            assert_eq!(
                parsed_messages
                    .iter()
                    .find(|message| message.model_id == "claude-sonnet-4.6")
                    .unwrap()
                    .agent
                    .as_deref(),
                Some("github.copilot.subagent")
            );

            // Reproduce a schema-27 entry produced by the first-invoke resolver:
            // the agentless turn incorrectly inherits the nested sub-agent.
            let mut stale_messages = parsed_messages.clone();
            let stale_plain = stale_messages
                .iter_mut()
                .find(|message| message.model_id == "gpt-5.4-mini")
                .unwrap();
            stale_plain.agent = Some("github.copilot.subagent".to_string());
            assert_eq!(
                stale_plain.agent.as_deref(),
                Some("github.copilot.subagent")
            );
            let stale_entry = CachedSourceEntry::new(
                source.path(),
                source_fingerprint.clone(),
                stale_messages,
                Vec::new(),
                None,
            );
            let cache_file = cache_path().unwrap();
            ensure_cache_dir(cache_file.parent().unwrap()).unwrap();
            let stale_store = CachedSourceStore {
                schema_version: 27,
                entries: vec![stale_entry],
            };

            let writer = BufWriter::new(File::create(&cache_file).unwrap());
            bincode::options()
                .serialize_into(writer, &stale_store)
                .unwrap();

            let mut loaded = SourceMessageCache::load();
            assert!(
                loaded.entries.is_empty(),
                "schema-27 Copilot entries must be stale"
            );
            assert_eq!(
                SourceFingerprint::from_path(source.path()).unwrap(),
                source_fingerprint,
                "the stale cache entry and rebuilt parse have the same source fingerprint"
            );

            let rebuilt_messages = crate::sessions::copilot::parse_copilot_file(source.path());
            let rebuilt_plain = rebuilt_messages
                .iter()
                .find(|message| message.model_id == "gpt-5.4-mini")
                .unwrap();
            assert_eq!(
                rebuilt_plain.agent.as_deref(),
                Some("github.copilot.default"),
                "reparse must use the root invoke_agent for the agentless turn"
            );
            let rebuilt_sub = rebuilt_messages
                .iter()
                .find(|message| message.model_id == "claude-sonnet-4.6")
                .unwrap();
            assert_eq!(
                rebuilt_sub.agent.as_deref(),
                Some("github.copilot.subagent"),
                "per-record nested attribution must remain unchanged"
            );
            loaded.insert(CachedSourceEntry::new(
                source.path(),
                source_fingerprint,
                rebuilt_messages,
                Vec::new(),
                None,
            ));
            loaded.save_if_dirty();

            let rebuilt = read_store_from_path(&cache_file).unwrap();
            assert_eq!(rebuilt.schema_version, CACHE_SCHEMA_VERSION);
            assert_eq!(rebuilt.entries.len(), 1);
            let rebuilt_entry = &rebuilt.entries[0];
            assert_eq!(
                rebuilt_entry
                    .messages
                    .iter()
                    .find(|message| message.model_id == "gpt-5.4-mini")
                    .unwrap()
                    .agent
                    .as_deref(),
                Some("github.copilot.default")
            );
            assert_eq!(
                rebuilt_entry
                    .messages
                    .iter()
                    .find(|message| message.model_id == "claude-sonnet-4.6")
                    .unwrap()
                    .agent
                    .as_deref(),
                Some("github.copilot.subagent")
            );

            let reloaded = SourceMessageCache::load();
            let reloaded_entry = reloaded.get(source.path()).unwrap();
            assert_eq!(
                reloaded_entry
                    .messages
                    .iter()
                    .find(|message| message.model_id == "gpt-5.4-mini")
                    .unwrap()
                    .agent
                    .as_deref(),
                Some("github.copilot.default")
            );
            assert_eq!(
                reloaded_entry
                    .messages
                    .iter()
                    .find(|message| message.model_id == "claude-sonnet-4.6")
                    .unwrap()
                    .agent
                    .as_deref(),
                Some("github.copilot.subagent")
            );
        }

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_schema_28_codex_cache_is_stale_and_rebuilt_with_non_overlapping_durations() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source = write_temp_file(include_bytes!(
                "../tests/fixtures/codex_duration_timing.jsonl"
            ));
            let source_fingerprint = SourceFingerprint::from_path(source.path()).unwrap();
            let parsed = crate::sessions::codex::parse_codex_file_incremental(
                source.path(),
                0,
                CodexParseState::default(),
            );
            assert!(parsed.parse_succeeded);
            assert_eq!(
                parsed
                    .messages
                    .iter()
                    .map(|message| message.duration_ms)
                    .collect::<Vec<_>>(),
                vec![Some(1_000), Some(4_000), Some(2_000)]
            );

            let mut stale_messages = parsed.messages.clone();
            stale_messages[1].duration_ms = Some(5_000);
            let stale_incremental =
                build_codex_incremental_cache(source.path(), parsed.consumed_offset, parsed.state)
                    .unwrap();
            let stale_entry = CachedSourceEntry::new(
                source.path(),
                source_fingerprint.clone(),
                stale_messages,
                Vec::new(),
                Some(stale_incremental),
            );
            let cache_file = cache_path().unwrap();
            ensure_cache_dir(cache_file.parent().unwrap()).unwrap();
            let stale_store = CachedSourceStore {
                schema_version: 28,
                entries: vec![stale_entry],
            };

            let writer = BufWriter::new(File::create(&cache_file).unwrap());
            bincode::options()
                .serialize_into(writer, &stale_store)
                .unwrap();

            let mut loaded = SourceMessageCache::load();
            assert!(
                loaded.entries.is_empty(),
                "schema-28 Codex durations and incremental state must be stale"
            );
            assert_eq!(
                SourceFingerprint::from_path(source.path()).unwrap(),
                source_fingerprint,
                "the stale entry and rebuilt parse have the same source fingerprint"
            );

            let rebuilt = crate::sessions::codex::parse_codex_file_incremental(
                source.path(),
                0,
                CodexParseState::default(),
            );
            assert!(rebuilt.parse_succeeded);
            assert_eq!(
                rebuilt
                    .messages
                    .iter()
                    .map(|message| message.duration_ms)
                    .collect::<Vec<_>>(),
                vec![Some(1_000), Some(4_000), Some(2_000)]
            );
            assert!(rebuilt.state.last_accepted_token_timestamp_ms.is_some());
            let rebuilt_incremental = build_codex_incremental_cache(
                source.path(),
                rebuilt.consumed_offset,
                rebuilt.state,
            )
            .unwrap();
            loaded.insert(CachedSourceEntry::new(
                source.path(),
                source_fingerprint,
                rebuilt.messages,
                rebuilt.fallback_timestamp_indices,
                Some(rebuilt_incremental),
            ));
            loaded.save_if_dirty();

            let persisted = read_store_from_path(&cache_file).unwrap();
            assert_eq!(persisted.schema_version, CACHE_SCHEMA_VERSION);
            assert_eq!(persisted.entries.len(), 1);
            assert_eq!(
                persisted.entries[0]
                    .messages
                    .iter()
                    .map(|message| message.duration_ms)
                    .collect::<Vec<_>>(),
                vec![Some(1_000), Some(4_000), Some(2_000)]
            );
            assert!(persisted.entries[0]
                .codex_incremental
                .as_ref()
                .unwrap()
                .state
                .last_accepted_token_timestamp_ms
                .is_some());
        }

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_fallback_cache_dir_prefers_runtime_dir() {
        let runtime_dir = TempDir::new().unwrap();
        let mut _env = EnvGuard::capture(&["XDG_RUNTIME_DIR"]);
        _env.set("XDG_RUNTIME_DIR", runtime_dir.path());

        assert_eq!(
            fallback_cache_dir(),
            Some(runtime_dir.path().join("tokscale"))
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_save_if_dirty_marks_cache_clean() {
        let temp_home = TempDir::new().unwrap();
        let _env = sandbox_cache_env(temp_home.path());

        let mut cache = SourceMessageCache::default();
        assert!(!cache.dirty);

        let file = write_temp_file(b"{}\n");
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        cache.insert(CachedSourceEntry::new(
            file.path(),
            fingerprint,
            Vec::new(),
            Vec::new(),
            None,
        ));
        assert!(cache.dirty);

        cache.save_if_dirty();
        assert!(!cache.dirty);
    }

    #[test]
    #[serial_test::serial]
    fn test_save_if_dirty_merges_concurrent_writers() {
        let temp_home = TempDir::new().unwrap();
        let _env = sandbox_cache_env(temp_home.path());

        let file_one = write_temp_file(b"{\"id\":1}\n");
        let file_two = write_temp_file(b"{\"id\":2}\n");

        let mut writer_one = SourceMessageCache::load();
        let mut writer_two = SourceMessageCache::load();

        writer_one.insert(CachedSourceEntry::new(
            file_one.path(),
            SourceFingerprint::from_path(file_one.path()).unwrap(),
            Vec::new(),
            Vec::new(),
            None,
        ));
        writer_two.insert(CachedSourceEntry::new(
            file_two.path(),
            SourceFingerprint::from_path(file_two.path()).unwrap(),
            Vec::new(),
            Vec::new(),
            None,
        ));

        writer_one.save_if_dirty();
        writer_two.save_if_dirty();

        let loaded = SourceMessageCache::load();
        assert!(loaded.get(file_one.path()).is_some());
        assert!(loaded.get(file_two.path()).is_some());
    }

    #[test]
    #[serial_test::serial]
    fn test_save_if_dirty_preserves_recreated_path_from_concurrent_writer() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("session.jsonl");
            std::fs::write(&path, b"{\"id\":\"old\"}\n").unwrap();

            let mut seed = SourceMessageCache::default();
            seed.insert(CachedSourceEntry::new(
                &path,
                SourceFingerprint::from_path(&path).unwrap(),
                vec![UnifiedMessage::new(
                    "client",
                    "gpt-5",
                    "provider",
                    "old-session",
                    1,
                    TokenBreakdown {
                        input: 1,
                        output: 0,
                        cache_read: 0,
                        cache_write: 0,
                        reasoning: 0,
                    },
                    0.0,
                )],
                Vec::new(),
                None,
            ));
            seed.save_if_dirty();

            let mut stale_deleter = SourceMessageCache::load();
            std::fs::remove_file(&path).unwrap();
            stale_deleter.prune_missing_files();

            std::fs::write(&path, b"{\"id\":\"fresh\"}\n").unwrap();
            let mut fresh_writer = SourceMessageCache::load();
            fresh_writer.insert(CachedSourceEntry::new(
                &path,
                SourceFingerprint::from_path(&path).unwrap(),
                vec![UnifiedMessage::new(
                    "client",
                    "gpt-5",
                    "provider",
                    "fresh-session",
                    2,
                    TokenBreakdown {
                        input: 2,
                        output: 0,
                        cache_read: 0,
                        cache_write: 0,
                        reasoning: 0,
                    },
                    0.0,
                )],
                Vec::new(),
                None,
            ));
            fresh_writer.save_if_dirty();

            stale_deleter.save_if_dirty();

            let loaded = SourceMessageCache::load();
            let entry = loaded
                .get(&path)
                .expect("recreated source cache entry should survive stale delete");
            assert_eq!(entry.messages[0].session_id, "fresh-session");
        }

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    #[cfg(not(target_os = "windows"))]
    fn load_falls_back_to_legacy_dirs_cache_path() {
        let temp_home = TempDir::new().unwrap();
        let temp_xdg_cache = TempDir::new().unwrap();
        let _env = legacy_cache_env(temp_home.path(), Some(temp_xdg_cache.path()));

        let source = write_temp_file(b"legacy-dirs\n");
        let entry = CachedSourceEntry::new(
            source.path(),
            SourceFingerprint::from_path(source.path()).unwrap(),
            Vec::new(),
            Vec::new(),
            None,
        );

        let legacy_path = crate::paths::legacy_dirs_cache_dir()
            .unwrap()
            .join(CACHE_FILENAME);
        ensure_cache_dir(legacy_path.parent().unwrap()).unwrap();
        let store = CachedSourceStore {
            schema_version: CACHE_SCHEMA_VERSION,
            entries: vec![entry],
        };
        let writer = BufWriter::new(File::create(&legacy_path).unwrap());
        bincode::options().serialize_into(writer, &store).unwrap();

        let loaded = SourceMessageCache::load();
        assert!(loaded.get(source.path()).is_some());
    }

    #[test]
    #[serial_test::serial]
    #[cfg(not(target_os = "windows"))]
    fn load_falls_back_to_legacy_dot_cache_path() {
        let temp_home = TempDir::new().unwrap();
        let _env = legacy_cache_env(temp_home.path(), None);

        let source = write_temp_file(b"legacy-dot\n");
        let entry = CachedSourceEntry::new(
            source.path(),
            SourceFingerprint::from_path(source.path()).unwrap(),
            Vec::new(),
            Vec::new(),
            None,
        );

        let legacy_path = crate::paths::legacy_dot_cache_tokscale_dir()
            .unwrap()
            .join(CACHE_FILENAME);
        ensure_cache_dir(legacy_path.parent().unwrap()).unwrap();
        let store = CachedSourceStore {
            schema_version: CACHE_SCHEMA_VERSION,
            entries: vec![entry],
        };
        let writer = BufWriter::new(File::create(&legacy_path).unwrap());
        bincode::options().serialize_into(writer, &store).unwrap();

        let loaded = SourceMessageCache::load();
        assert!(loaded.get(source.path()).is_some());
    }

    #[cfg(windows)]
    #[test]
    #[serial_test::serial]
    fn legacy_cache_paths_are_ordered_and_override_gated_without_io() {
        let temp_home = TempDir::new().unwrap();
        let _env = legacy_cache_env(temp_home.path(), None);
        let candidates = legacy_cache_paths();
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0],
            dirs::cache_dir()
                .expect("Windows exposes a cache directory")
                .join("tokscale")
                .join(CACHE_FILENAME)
        );
        assert_eq!(
            candidates[1],
            dirs::home_dir()
                .expect("Windows exposes a home directory")
                .join(".cache")
                .join("tokscale")
                .join(CACHE_FILENAME)
        );

        let mut override_env = EnvGuard::capture(&["TOKSCALE_CONFIG_DIR"]);
        override_env.set("TOKSCALE_CONFIG_DIR", temp_home.path());
        assert!(legacy_cache_paths().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_cached_path_preserves_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(vec![0x66, 0x6f, 0x80, 0x6f]));
        let cached_path = CachedPath::from_path(&path);

        assert_eq!(cached_path.to_path_buf(), path);
    }
}
