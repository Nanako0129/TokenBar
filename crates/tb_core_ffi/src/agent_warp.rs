//! Warp aggregate usage producer.
//!
//! The bearer is process-memory only. Remote and external payloads are normalized
//! at this boundary before tokscale-core sees them; only installation-key HMAC
//! account/workspace identities and aggregate counters cross into reporting.

use crate::agent_account_scope::{
    begin_refresh, read_external_regular_bounded, RefreshTransaction, SecureFileRead,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
#[cfg(test)]
use reqwest::header::AUTHORIZATION;
use reqwest::header::{CONTENT_LENGTH, RETRY_AFTER};
use reqwest::{redirect, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokscale_core::sessions::warp::{
    WarpAggregateUsage, WarpNormalizedUsage, WarpUsageSource, WarpWorkspaceUsage,
    WARP_NORMALIZED_USAGE_VERSION,
};

const ENDPOINT: &str = "https://app.warp.dev/graphql/v2";
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BEARER_BYTES: usize = 16 * 1024;
const DEFAULT_RETRY_MS: i64 = 5 * 60 * 1000;
const CACHE_SCHEMA_VERSION: u32 = 1;

const REQUEST_LIMIT_QUERY: &str = r#"query GetRequestLimitInfo { requestLimitInfo { requestLimit requestsUsedSinceLastRefresh nextRefreshTime bonusGrantsInfo { spendingInfo { currentMonthSpendCents currentMonthCreditsPurchased } } } }"#;
const WORKSPACES_QUERY: &str = r#"query GetWorkspacesMetadataForUser { workspacesMetadataForUser { id totalRequestsUsedSinceLastRefresh aiOverages { currentMonthlyRequestCostCents currentMonthlyRequestsUsed } usageInfo { requestsUsedSinceLastRefresh } } }"#;

const ACCOUNT_DOMAIN: &[u8] = b"tokenbar-warp-account-v1";
const EXTERNAL_ACCOUNT_DOMAIN: &[u8] = b"tokenbar-warp-external-account-v1";
const WORKSPACE_DOMAIN: &[u8] = b"tokenbar-warp-workspace-v1";
const CACHE_MAC_DOMAIN: &[u8] = b"tokenbar-warp-cache-mac-v1";
const SOURCE_FINGERPRINT_DOMAIN: &[u8] = b"tokenbar-warp-source-fingerprint-v1";

static STATE: LazyLock<RwLock<WarpState>> = LazyLock::new(|| RwLock::new(WarpState::default()));
static GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WarpError {
    UnsupportedPlatform,
    InvalidBearer,
    NoActiveCredential,
    Cooldown,
    Unauthorized,
    Forbidden,
    RateLimited,
    RemoteRejected,
    RemoteServer,
    Timeout,
    Transport,
    Oversize,
    Decode,
    Graphql,
    InvalidUsage,
    InvalidExternalPath,
    ScopeMismatch,
    Storage,
    SourceChanged,
    Internal,
}

impl WarpError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "warp_unsupported_platform",
            Self::InvalidBearer => "warp_invalid_bearer",
            Self::NoActiveCredential => "warp_no_active_credential",
            Self::Cooldown => "warp_retry_cooldown",
            Self::Unauthorized => "warp_unauthorized",
            Self::Forbidden => "warp_forbidden",
            Self::RateLimited => "warp_rate_limited",
            Self::RemoteRejected => "warp_remote_rejected",
            Self::RemoteServer => "warp_remote_server",
            Self::Timeout => "warp_timeout",
            Self::Transport => "warp_transport",
            Self::Oversize => "warp_response_too_large",
            Self::Decode => "warp_decode_failed",
            Self::Graphql => "warp_graphql_error",
            Self::InvalidUsage => "warp_invalid_usage",
            Self::InvalidExternalPath => "warp_invalid_external_path",
            Self::ScopeMismatch => "warp_scope_mismatch",
            Self::Storage => "warp_storage_failed",
            Self::SourceChanged => "warp_source_changed",
            Self::Internal => "warp_internal",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WarpCapability {
    supported: bool,
    bearer_in_process_only: bool,
    external_exact_usage_json: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WarpStatus {
    supported: bool,
    mode: &'static str,
    active: bool,
    stale: bool,
    synced_at_ms: Option<i64>,
    retry_at_ms: Option<i64>,
    error_code: Option<&'static str>,
    requests_used: Option<i64>,
    request_limit: Option<i64>,
    spend_cents: Option<i64>,
    workspace_count: usize,
}

struct PreparedSource {
    source: WarpUsageSource,
    usage: WarpNormalizedUsage,
}

struct AppMode {
    bearer: String,
    account_scope: String,
    prepared: PreparedSource,
    stale: bool,
    retry_at_ms: Option<i64>,
    error: Option<WarpError>,
}

struct ExternalMode {
    prepared: PreparedSource,
    stale: bool,
    error: Option<WarpError>,
}

enum Mode {
    App(AppMode),
    External(ExternalMode),
}

#[derive(Default)]
struct WarpState {
    mode: Option<Mode>,
    last_error: Option<WarpError>,
    revoked_scopes: HashSet<String>,
}

pub(crate) struct SourceSnapshot {
    pub(crate) generation: u64,
    pub(crate) scanner_settings: tokscale_core::scanner::ScannerSettings,
}

pub(crate) fn capability() -> WarpCapability {
    WarpCapability {
        supported: cfg!(target_os = "macos"),
        bearer_in_process_only: cfg!(target_os = "macos"),
        external_exact_usage_json: cfg!(target_os = "macos"),
    }
}

pub(crate) fn status() -> WarpStatus {
    let state = STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match state.mode.as_ref() {
        Some(Mode::App(app)) => status_for_usage(
            "app",
            &app.prepared.usage,
            app.stale,
            app.retry_at_ms,
            app.error,
        ),
        Some(Mode::External(external)) => status_for_usage(
            "external",
            &external.prepared.usage,
            external.stale,
            None,
            external.error,
        ),
        None => WarpStatus {
            supported: cfg!(target_os = "macos"),
            mode: "none",
            active: false,
            stale: false,
            synced_at_ms: None,
            retry_at_ms: None,
            error_code: state.last_error.map(WarpError::code),
            requests_used: None,
            request_limit: None,
            spend_cents: None,
            workspace_count: 0,
        },
    }
}

fn status_for_usage(
    mode: &'static str,
    usage: &WarpNormalizedUsage,
    stale: bool,
    retry_at_ms: Option<i64>,
    error: Option<WarpError>,
) -> WarpStatus {
    WarpStatus {
        supported: cfg!(target_os = "macos"),
        mode,
        active: true,
        stale,
        synced_at_ms: Some(usage.synced_at_ms),
        retry_at_ms,
        error_code: error.map(WarpError::code),
        requests_used: usage.usage.requests_used,
        request_limit: usage.usage.request_limit,
        spend_cents: usage.usage.spend_cents,
        workspace_count: usage.workspaces.len(),
    }
}

pub(crate) fn source_snapshot() -> SourceSnapshot {
    let generation = GENERATION.load(Ordering::SeqCst);
    let source = {
        let state = STATE
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.mode.as_ref() {
            Some(Mode::App(app)) => Some(app.prepared.source.clone()),
            Some(Mode::External(external)) => Some(external.prepared.source.clone()),
            None => None,
        }
    };
    SourceSnapshot {
        generation,
        scanner_settings: tokscale_core::scanner::ScannerSettings {
            warp_usage_source: source,
            ..Default::default()
        },
    }
}

pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

pub(crate) async fn set_bearer(raw: &str) -> Result<WarpStatus, WarpError> {
    require_macos()?;
    let bearer = validate_bearer(raw)?;
    let transaction = begin_refresh("warp").map_err(|_| WarpError::Storage)?;
    let account_scope = transaction
        .opaque_hmac(ACCOUNT_DOMAIN, &[bearer.as_bytes()])
        .map_err(|_| WarpError::Storage)?;
    let prior = prior_identity();

    if prior
        .as_ref()
        .is_some_and(|identity| identity.account_scope.as_deref() == Some(account_scope.as_str()))
    {
        if let Some(retry_at_ms) = prior.as_ref().and_then(|identity| identity.retry_at_ms) {
            if retry_at_ms > now_ms() {
                return Err(WarpError::Cooldown);
            }
        }
    }

    let cache_candidate = if prior.is_none() {
        load_cache_candidate_with(&account_scope, || load_cache(&transaction, &account_scope))?
    } else {
        None
    };
    match fetch_usage(&transaction, &bearer, &account_scope).await {
        Ok(usage) => {
            let prepared = persist_app_source(&transaction, usage)?;
            install_authenticated_app_mode(AppMode {
                bearer,
                account_scope,
                prepared,
                stale: false,
                retry_at_ms: None,
                error: None,
            });
            Ok(status())
        }
        Err(failure) => {
            if failure.error == WarpError::Unauthorized {
                revoke_scope(&account_scope);
            }
            let returned_error = match set_failure_action(
                prior.as_ref(),
                &account_scope,
                cache_candidate.is_some(),
                failure.error,
            ) {
                SetFailureAction::ApplySameScope => {
                    apply_same_scope_failure(&transaction, &account_scope, failure).err()
                }
                SetFailureAction::InstallCached => {
                    let prepared = cache_candidate.ok_or(WarpError::Internal)?;
                    install_mode(Mode::App(AppMode {
                        bearer,
                        account_scope,
                        prepared,
                        stale: true,
                        retry_at_ms: failure.retry_at_ms,
                        error: Some(failure.error),
                    }));
                    None
                }
                SetFailureAction::StayInactive => {
                    apply_inactive_failure(&transaction, &account_scope, failure).err()
                }
                SetFailureAction::PreservePrior => None,
            };
            Err(returned_error.unwrap_or(failure.error))
        }
    }
}

pub(crate) async fn refresh() -> Result<WarpStatus, WarpError> {
    require_macos()?;
    let snapshot = app_snapshot()?;
    if snapshot.retry_at_ms.is_some_and(|retry| retry > now_ms()) {
        return Err(WarpError::Cooldown);
    }
    let transaction = begin_refresh("warp").map_err(|_| WarpError::Storage)?;
    if generation() != snapshot.generation {
        return Err(WarpError::SourceChanged);
    }
    match fetch_usage(&transaction, &snapshot.bearer, &snapshot.account_scope).await {
        Ok(usage) => {
            let prepared = persist_app_source(&transaction, usage)?;
            if generation() != snapshot.generation {
                return Err(WarpError::SourceChanged);
            }
            install_authenticated_app_mode(AppMode {
                bearer: snapshot.bearer,
                account_scope: snapshot.account_scope,
                prepared,
                stale: false,
                retry_at_ms: None,
                error: None,
            });
            Ok(status())
        }
        Err(failure) => {
            if failure.error == WarpError::Unauthorized {
                revoke_scope(&snapshot.account_scope);
            }
            apply_same_scope_failure(&transaction, &snapshot.account_scope, failure)?;
            Err(failure.error)
        }
    }
}

pub(crate) fn set_external(path: &Path) -> Result<WarpStatus, WarpError> {
    require_macos()?;
    validate_external_filename(path)?;
    let transaction = begin_refresh("warp").map_err(|_| WarpError::Storage)?;
    let prepared = prepare_external_source(&transaction, path)?;
    if clear_external_error_if_current(&prepared) {
        return Ok(status());
    }

    if matches!(
        prior_identity().as_ref().and_then(|identity| identity.mode),
        Some("app")
    ) {
        transaction
            .remove_warp_cache()
            .map_err(|_| WarpError::Storage)?;
    }
    install_mode(Mode::External(ExternalMode {
        prepared,
        stale: false,
        error: None,
    }));
    Ok(status())
}

pub(crate) fn restore_external_if_inactive(path: &Path) -> Result<WarpStatus, WarpError> {
    require_macos()?;
    validate_external_filename(path)?;
    let expected_generation = generation();
    if prior_identity().is_some() {
        return Ok(status());
    }
    let transaction = begin_refresh("warp").map_err(|_| WarpError::Storage)?;
    if generation() != expected_generation {
        return Ok(status());
    }
    let prepared = prepare_external_source(&transaction, path)?;
    install_external_if_inactive(expected_generation, prepared);
    Ok(status())
}

fn install_external_if_inactive(expected_generation: u64, prepared: PreparedSource) -> bool {
    {
        let mut state = STATE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if generation() != expected_generation || state.mode.is_some() {
            return false;
        }
        state.mode = Some(Mode::External(ExternalMode {
            prepared,
            stale: false,
            error: None,
        }));
        state.last_error = None;
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
    tokscale_core::invalidate_usage_data();
    true
}

pub(crate) fn refresh_external_if_active() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let snapshot = {
        let generation = generation();
        let state = STATE
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.mode.as_ref() {
            Some(Mode::External(external)) => {
                Some((generation, external.prepared.source.path().to_path_buf()))
            }
            _ => None,
        }
    };
    let Some((expected_generation, path)) = snapshot else {
        return;
    };
    let result = match begin_refresh("warp") {
        Ok(transaction) => {
            if generation() != expected_generation {
                return;
            }
            prepare_external_source(&transaction, &path)
        }
        Err(_) => Err(WarpError::Storage),
    };
    finish_external_refresh(expected_generation, &path, result);
}

fn finish_external_refresh(
    expected_generation: u64,
    expected_path: &Path,
    result: Result<PreparedSource, WarpError>,
) -> bool {
    match result {
        Ok(prepared) => install_refreshed_external(expected_generation, expected_path, prepared),
        Err(error) => mark_external_stale(expected_generation, expected_path, error),
    }
}

fn install_refreshed_external(
    expected_generation: u64,
    expected_path: &Path,
    prepared: PreparedSource,
) -> bool {
    let mut invalidate = false;
    {
        let mut state = STATE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if generation() != expected_generation {
            return false;
        }
        let Some(Mode::External(current)) = state.mode.as_mut() else {
            return false;
        };
        if current.prepared.source.path() != expected_path {
            return false;
        }
        if !same_external_source(&current.prepared, &prepared) {
            current.prepared = prepared;
            GENERATION.fetch_add(1, Ordering::SeqCst);
            invalidate = true;
        }
        current.stale = false;
        current.error = None;
        state.last_error = None;
    }
    if invalidate {
        tokscale_core::invalidate_usage_data();
    }
    true
}

fn mark_external_stale(expected_generation: u64, expected_path: &Path, error: WarpError) -> bool {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if generation() != expected_generation {
        return false;
    }
    let Some(Mode::External(current)) = state.mode.as_mut() else {
        return false;
    };
    if current.prepared.source.path() != expected_path {
        return false;
    }
    current.stale = true;
    current.error = Some(error);
    state.last_error = None;
    true
}

fn validate_external_filename(path: &Path) -> Result<(), WarpError> {
    if path.file_name().and_then(|name| name.to_str()) == Some("usage.json") {
        Ok(())
    } else {
        Err(WarpError::InvalidExternalPath)
    }
}

fn prepare_external_source(
    transaction: &RefreshTransaction,
    path: &Path,
) -> Result<PreparedSource, WarpError> {
    let read = read_external_regular_bounded(path, MAX_RESPONSE_BYTES)
        .map_err(|_| WarpError::InvalidExternalPath)?;
    validate_external_filename(&read.path)?;
    let path_identity = path_identity_bytes(&read.path);
    let account_scope = transaction
        .opaque_hmac(EXTERNAL_ACCOUNT_DOMAIN, &[path_identity.as_slice()])
        .map_err(|_| WarpError::Storage)?;
    let raw: ExternalUsage = serde_json::from_slice(&read.bytes).map_err(|_| WarpError::Decode)?;
    let usage = normalize_external(transaction, raw, account_scope)?;
    source_from_usage(transaction, read.path, read.modified_ms, usage)
}

pub(crate) fn clear() -> Result<WarpStatus, WarpError> {
    require_macos()?;
    let transaction = match begin_refresh("warp") {
        Ok(transaction) => transaction,
        Err(_) => {
            clear_mode();
            return Err(WarpError::Storage);
        }
    };
    let was_app = matches!(
        prior_identity().as_ref().and_then(|identity| identity.mode),
        Some("app")
    );
    clear_mode();
    if was_app {
        transaction
            .remove_warp_cache()
            .map_err(|_| WarpError::Storage)?;
    }
    Ok(status())
}

fn require_macos() -> Result<(), WarpError> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        Err(WarpError::UnsupportedPlatform)
    }
}

fn validate_bearer(raw: &str) -> Result<String, WarpError> {
    let bearer = raw.trim();
    if bearer.is_empty()
        || bearer.len() > MAX_BEARER_BYTES
        || bearer.bytes().any(|byte| byte.is_ascii_control())
        || bearer
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        return Err(WarpError::InvalidBearer);
    }
    Ok(bearer.to_string())
}

#[cfg(unix)]
fn path_identity_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_identity_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

struct PriorIdentity {
    mode: Option<&'static str>,
    account_scope: Option<String>,
    retry_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetFailureAction {
    ApplySameScope,
    InstallCached,
    StayInactive,
    PreservePrior,
}

fn set_failure_action(
    prior: Option<&PriorIdentity>,
    new_scope: &str,
    has_cache: bool,
    error: WarpError,
) -> SetFailureAction {
    match prior {
        Some(prior) if prior.account_scope.as_deref() == Some(new_scope) => {
            SetFailureAction::ApplySameScope
        }
        Some(_) => SetFailureAction::PreservePrior,
        None if has_cache && error != WarpError::Unauthorized => SetFailureAction::InstallCached,
        None => SetFailureAction::StayInactive,
    }
}

fn load_cache_candidate_with<T>(
    account_scope: &str,
    load: impl FnOnce() -> Result<Option<T>, WarpError>,
) -> Result<Option<T>, WarpError> {
    let revoked = STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .revoked_scopes
        .contains(account_scope);
    if revoked {
        Ok(None)
    } else {
        load()
    }
}

fn revoke_scope(account_scope: &str) {
    STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .revoked_scopes
        .insert(account_scope.to_string());
}

fn prior_identity() -> Option<PriorIdentity> {
    let state = STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.mode.as_ref().map(|mode| match mode {
        Mode::App(app) => PriorIdentity {
            mode: Some("app"),
            account_scope: Some(app.account_scope.clone()),
            retry_at_ms: app.retry_at_ms,
        },
        Mode::External(_) => PriorIdentity {
            mode: Some("external"),
            account_scope: None,
            retry_at_ms: None,
        },
    })
}

#[cfg(test)]
fn external_source_is_current(prepared: &PreparedSource) -> bool {
    let state = STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    matches!(
        state.mode.as_ref(),
        Some(Mode::External(current)) if same_external_source(&current.prepared, prepared)
    )
}

fn clear_external_error_if_current(prepared: &PreparedSource) -> bool {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(Mode::External(current)) = state.mode.as_mut() else {
        return false;
    };
    if !same_external_source(&current.prepared, prepared) {
        return false;
    }
    current.stale = false;
    current.error = None;
    state.last_error = None;
    true
}

fn same_external_source(current: &PreparedSource, candidate: &PreparedSource) -> bool {
    current.usage == candidate.usage
        && current.source.path() == candidate.source.path()
        && current.source.observed_mtime_ms() == candidate.source.observed_mtime_ms()
}

struct AppSnapshot {
    generation: u64,
    bearer: String,
    account_scope: String,
    retry_at_ms: Option<i64>,
}

fn app_snapshot() -> Result<AppSnapshot, WarpError> {
    let generation = generation();
    let state = STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(Mode::App(app)) = state.mode.as_ref() else {
        return Err(WarpError::NoActiveCredential);
    };
    Ok(AppSnapshot {
        generation,
        bearer: app.bearer.clone(),
        account_scope: app.account_scope.clone(),
        retry_at_ms: app.retry_at_ms,
    })
}

fn install_mode(mode: Mode) {
    {
        let mut state = STATE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.mode = Some(mode);
        state.last_error = None;
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
    tokscale_core::invalidate_usage_data();
}

fn install_authenticated_app_mode(app: AppMode) {
    {
        let mut state = STATE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.revoked_scopes.remove(&app.account_scope);
        state.mode = Some(Mode::App(app));
        state.last_error = None;
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
    tokscale_core::invalidate_usage_data();
}

fn clear_mode() {
    {
        let mut state = STATE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.mode = None;
        state.last_error = None;
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
    tokscale_core::invalidate_usage_data();
}

fn clear_mode_with_error(error: WarpError) {
    {
        let mut state = STATE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.mode = None;
        state.last_error = Some(error);
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
    tokscale_core::invalidate_usage_data();
}

fn set_no_mode_error(error: WarpError) {
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.mode.is_none() {
        state.last_error = Some(error);
    }
}

#[derive(Clone, Copy)]
struct FetchFailure {
    error: WarpError,
    retry_at_ms: Option<i64>,
}

fn apply_same_scope_failure(
    transaction: &RefreshTransaction,
    account_scope: &str,
    failure: FetchFailure,
) -> Result<(), WarpError> {
    apply_same_scope_failure_with(account_scope, failure, || {
        transaction
            .remove_warp_cache()
            .map_err(|_| WarpError::Storage)
    })
}

fn apply_same_scope_failure_with(
    account_scope: &str,
    failure: FetchFailure,
    purge: impl FnOnce() -> Result<(), WarpError>,
) -> Result<(), WarpError> {
    if failure.error == WarpError::Unauthorized {
        revoke_scope(account_scope);
        clear_mode_with_error(WarpError::Unauthorized);
        if purge().is_err() {
            set_no_mode_error(WarpError::Storage);
            return Err(WarpError::Storage);
        }
        return Ok(());
    }
    let mut state = STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(Mode::App(app)) = state.mode.as_mut() {
        app.stale = true;
        app.retry_at_ms = failure.retry_at_ms;
        app.error = Some(failure.error);
    }
    Ok(())
}

fn apply_inactive_failure(
    transaction: &RefreshTransaction,
    account_scope: &str,
    failure: FetchFailure,
) -> Result<(), WarpError> {
    if failure.error == WarpError::Unauthorized {
        revoke_scope(account_scope);
        if transaction.remove_warp_cache().is_err() {
            set_no_mode_error(WarpError::Storage);
            return Err(WarpError::Storage);
        }
    }
    set_no_mode_error(failure.error);
    Ok(())
}

async fn fetch_usage(
    transaction: &RefreshTransaction,
    bearer: &str,
    account_scope: &str,
) -> Result<WarpNormalizedUsage, FetchFailure> {
    let client = build_client(true).map_err(|error| FetchFailure {
        error,
        retry_at_ms: Some(now_ms().saturating_add(DEFAULT_RETRY_MS)),
    })?;
    let (request_limit, workspaces) = fetch_remote(&client, ENDPOINT, bearer).await?;
    normalize_remote(
        transaction,
        request_limit,
        workspaces,
        account_scope.to_string(),
    )
    .map_err(|error| FetchFailure {
        error,
        retry_at_ms: Some(now_ms().saturating_add(DEFAULT_RETRY_MS)),
    })
}

async fn fetch_remote(
    client: &reqwest::Client,
    endpoint: &str,
    bearer: &str,
) -> Result<(RequestLimitData, WorkspaceData), FetchFailure> {
    let request_limit = send_graphql::<RequestLimitData>(
        client,
        endpoint,
        bearer,
        "GetRequestLimitInfo",
        REQUEST_LIMIT_QUERY,
    )
    .await?;
    let workspaces = send_graphql::<WorkspaceData>(
        client,
        endpoint,
        bearer,
        "GetWorkspacesMetadataForUser",
        WORKSPACES_QUERY,
    )
    .await?;
    Ok((request_limit, workspaces))
}

fn build_client(https_only: bool) -> Result<reqwest::Client, WarpError> {
    build_client_with_timeout(https_only, HTTP_TIMEOUT)
}

fn build_client_with_timeout(
    https_only: bool,
    timeout: Duration,
) -> Result<reqwest::Client, WarpError> {
    reqwest::Client::builder()
        .https_only(https_only)
        .redirect(redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|_| WarpError::Internal)
}

async fn send_graphql<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    endpoint: &str,
    bearer: &str,
    operation_name: &'static str,
    query: &'static str,
) -> Result<T, FetchFailure> {
    let request = client
        .post(endpoint)
        .bearer_auth(bearer)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&GraphqlRequest {
            operation_name,
            query,
            variables: EmptyVariables {},
        });
    let response = request.send().await.map_err(map_reqwest_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(classify_status(status, response.headers().get(RETRY_AFTER)));
    }
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err(transient_failure(WarpError::Oversize));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES as usize {
            return Err(transient_failure(WarpError::Oversize));
        }
        bytes.extend_from_slice(&chunk);
    }
    let envelope: GraphqlEnvelope<T> =
        serde_json::from_slice(&bytes).map_err(|_| transient_failure(WarpError::Decode))?;
    if !envelope.errors.is_empty() {
        return Err(transient_failure(WarpError::Graphql));
    }
    envelope
        .data
        .ok_or_else(|| transient_failure(WarpError::Decode))
}

fn map_reqwest_error(error: reqwest::Error) -> FetchFailure {
    transient_failure(if error.is_timeout() {
        WarpError::Timeout
    } else {
        WarpError::Transport
    })
}

fn transient_failure(error: WarpError) -> FetchFailure {
    FetchFailure {
        error,
        retry_at_ms: Some(now_ms().saturating_add(DEFAULT_RETRY_MS)),
    }
}

fn classify_status(
    status: StatusCode,
    retry_after: Option<&reqwest::header::HeaderValue>,
) -> FetchFailure {
    let error = match status {
        StatusCode::UNAUTHORIZED => WarpError::Unauthorized,
        StatusCode::FORBIDDEN => WarpError::Forbidden,
        StatusCode::TOO_MANY_REQUESTS => WarpError::RateLimited,
        status if status.is_server_error() => WarpError::RemoteServer,
        _ => WarpError::RemoteRejected,
    };
    let retry_at_ms = if error == WarpError::Unauthorized {
        None
    } else if error == WarpError::RateLimited {
        Some(parse_retry_after(retry_after).max(now_ms().saturating_add(DEFAULT_RETRY_MS)))
    } else {
        Some(now_ms().saturating_add(DEFAULT_RETRY_MS))
    };
    FetchFailure { error, retry_at_ms }
}

fn parse_retry_after(value: Option<&reqwest::header::HeaderValue>) -> i64 {
    let now = now_ms();
    let Some(value) = value.and_then(|value| value.to_str().ok()) else {
        return now.saturating_add(DEFAULT_RETRY_MS);
    };
    if let Ok(seconds) = value.trim().parse::<i64>() {
        return now.saturating_add(seconds.max(0).saturating_mul(1000));
    }
    DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.with_timezone(&Utc).timestamp_millis())
        .unwrap_or_else(|| now.saturating_add(DEFAULT_RETRY_MS))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlRequest {
    operation_name: &'static str,
    query: &'static str,
    variables: EmptyVariables,
}

#[derive(Serialize)]
struct EmptyVariables {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphqlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct GraphqlError {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestLimitData {
    request_limit_info: Option<RequestLimitInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestLimitInfo {
    request_limit: Option<i64>,
    requests_used_since_last_refresh: Option<i64>,
    next_refresh_time: Option<String>,
    bonus_grants_info: Option<BonusGrantsInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BonusGrantsInfo {
    spending_info: Option<SpendingInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpendingInfo {
    current_month_spend_cents: Option<i64>,
    current_month_credits_purchased: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceData {
    #[serde(default)]
    workspaces_metadata_for_user: Vec<WorkspaceMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceMetadata {
    id: String,
    total_requests_used_since_last_refresh: Option<i64>,
    ai_overages: Option<AiOverages>,
    usage_info: Option<WorkspaceUsageInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AiOverages {
    current_monthly_request_cost_cents: Option<i64>,
    current_monthly_requests_used: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceUsageInfo {
    requests_used_since_last_refresh: Option<i64>,
}

fn normalize_remote(
    transaction: &RefreshTransaction,
    request: RequestLimitData,
    workspaces: WorkspaceData,
    account_scope: String,
) -> Result<WarpNormalizedUsage, WarpError> {
    let scope_domain = account_scope.clone();
    normalize_remote_with(request, workspaces, account_scope, |raw_id| {
        transaction
            .opaque_hmac(
                WORKSPACE_DOMAIN,
                &[scope_domain.as_bytes(), raw_id.as_bytes()],
            )
            .map_err(|_| WarpError::Storage)
    })
}

fn normalize_remote_with(
    request: RequestLimitData,
    workspaces: WorkspaceData,
    account_scope: String,
    mut workspace_scope: impl FnMut(&str) -> Result<String, WarpError>,
) -> Result<WarpNormalizedUsage, WarpError> {
    let info = request.request_limit_info.ok_or(WarpError::InvalidUsage)?;
    let spending = info.bonus_grants_info.and_then(|bonus| bonus.spending_info);
    let usage = WarpAggregateUsage {
        requests_used: info.requests_used_since_last_refresh,
        request_limit: info.request_limit,
        spend_cents: spending
            .as_ref()
            .and_then(|spending| spending.current_month_spend_cents),
        credits_purchased_cents: spending
            .and_then(|spending| spending.current_month_credits_purchased),
        next_refresh_at_ms: parse_optional_time(info.next_refresh_time.as_deref())?,
    };
    let workspaces = workspaces
        .workspaces_metadata_for_user
        .into_iter()
        .map(|workspace| {
            if workspace.id.trim().is_empty() || workspace.id.len() > MAX_RESPONSE_BYTES as usize {
                return Err(WarpError::InvalidUsage);
            }
            let requests_used = workspace
                .ai_overages
                .as_ref()
                .and_then(|overages| overages.current_monthly_requests_used)
                .or(workspace.total_requests_used_since_last_refresh)
                .or_else(|| {
                    workspace
                        .usage_info
                        .as_ref()
                        .and_then(|info| info.requests_used_since_last_refresh)
                });
            let spend_cents = workspace
                .ai_overages
                .and_then(|overages| overages.current_monthly_request_cost_cents);
            Ok(WarpWorkspaceUsage {
                workspace_scope: workspace_scope(&workspace.id)?,
                requests_used,
                spend_cents,
            })
        })
        .collect::<Result<Vec<_>, WarpError>>()?;
    let normalized = WarpNormalizedUsage {
        version: WARP_NORMALIZED_USAGE_VERSION,
        synced_at_ms: now_ms().max(1),
        account_scope,
        usage,
        workspaces,
    };
    validate_normalized(&normalized)?;
    Ok(normalized)
}

fn parse_optional_time(value: Option<&str>) -> Result<Option<i64>, WarpError> {
    value
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|date| date.with_timezone(&Utc).timestamp_millis())
                .map_err(|_| WarpError::InvalidUsage)
        })
        .transpose()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExternalUsage {
    version: u32,
    synced_at: String,
    usage: ExternalAggregateUsage,
    #[serde(default)]
    workspaces: Vec<ExternalWorkspaceUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExternalAggregateUsage {
    requests_used: Option<i64>,
    request_limit: Option<i64>,
    spend_cents: Option<i64>,
    credits_purchased_cents: Option<i64>,
    next_refresh_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExternalWorkspaceUsage {
    id: String,
    #[serde(default)]
    name: Option<String>,
    requests_used: Option<i64>,
    spend_cents: Option<i64>,
}

fn normalize_external(
    transaction: &RefreshTransaction,
    raw: ExternalUsage,
    account_scope: String,
) -> Result<WarpNormalizedUsage, WarpError> {
    if raw.version != WARP_NORMALIZED_USAGE_VERSION {
        return Err(WarpError::InvalidUsage);
    }
    let synced_at_ms = DateTime::parse_from_rfc3339(&raw.synced_at)
        .map_err(|_| WarpError::InvalidUsage)?
        .with_timezone(&Utc)
        .timestamp_millis();
    let usage = WarpAggregateUsage {
        requests_used: raw.usage.requests_used,
        request_limit: raw.usage.request_limit,
        spend_cents: raw.usage.spend_cents,
        credits_purchased_cents: raw.usage.credits_purchased_cents,
        next_refresh_at_ms: parse_optional_time(raw.usage.next_refresh_time.as_deref())?,
    };
    let workspaces = raw
        .workspaces
        .into_iter()
        .map(|workspace| {
            let _ = workspace.name;
            if workspace.id.trim().is_empty() {
                return Err(WarpError::InvalidUsage);
            }
            let scope = transaction
                .opaque_hmac(
                    WORKSPACE_DOMAIN,
                    &[account_scope.as_bytes(), workspace.id.as_bytes()],
                )
                .map_err(|_| WarpError::Storage)?;
            Ok(WarpWorkspaceUsage {
                workspace_scope: scope,
                requests_used: workspace.requests_used,
                spend_cents: workspace.spend_cents,
            })
        })
        .collect::<Result<Vec<_>, WarpError>>()?;
    let normalized = WarpNormalizedUsage {
        version: WARP_NORMALIZED_USAGE_VERSION,
        synced_at_ms,
        account_scope,
        usage,
        workspaces,
    };
    validate_normalized(&normalized)?;
    Ok(normalized)
}

fn validate_normalized(usage: &WarpNormalizedUsage) -> Result<(), WarpError> {
    WarpUsageSource::new(PathBuf::from("usage.json"), usage.clone(), [0; 32], 0)
        .map(|_| ())
        .map_err(|_| WarpError::InvalidUsage)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CacheEnvelope {
    schema_version: u32,
    account_scope: String,
    payload_bytes_base64: String,
    payload_mac: String,
}

fn persist_app_source(
    transaction: &RefreshTransaction,
    usage: WarpNormalizedUsage,
) -> Result<PreparedSource, WarpError> {
    let bytes = encode_cache(transaction, &usage)?;
    let read = transaction
        .write_warp_cache(&bytes, MAX_RESPONSE_BYTES)
        .map_err(|_| WarpError::Storage)?;
    decode_cache(transaction, read, &usage.account_scope)?.ok_or(WarpError::ScopeMismatch)
}

fn load_cache(
    transaction: &RefreshTransaction,
    account_scope: &str,
) -> Result<Option<PreparedSource>, WarpError> {
    let Some(read) = transaction
        .read_warp_cache(MAX_RESPONSE_BYTES)
        .map_err(|_| WarpError::Storage)?
    else {
        return Ok(None);
    };
    match decode_cache(transaction, read, account_scope) {
        Ok(candidate) => Ok(candidate),
        Err(_) => {
            transaction
                .remove_warp_cache()
                .map_err(|_| WarpError::Storage)?;
            Ok(None)
        }
    }
}

fn encode_cache(
    transaction: &RefreshTransaction,
    usage: &WarpNormalizedUsage,
) -> Result<Vec<u8>, WarpError> {
    let payload = serde_json::to_vec(usage).map_err(|_| WarpError::Internal)?;
    let mac = transaction
        .hmac_digest(
            CACHE_MAC_DOMAIN,
            &[usage.account_scope.as_bytes(), payload.as_slice()],
        )
        .map_err(|_| WarpError::Storage)?;
    serde_json::to_vec(&CacheEnvelope {
        schema_version: CACHE_SCHEMA_VERSION,
        account_scope: usage.account_scope.clone(),
        payload_bytes_base64: URL_SAFE_NO_PAD.encode(&payload),
        payload_mac: URL_SAFE_NO_PAD.encode(mac),
    })
    .map_err(|_| WarpError::Internal)
}

fn decode_cache(
    transaction: &RefreshTransaction,
    read: SecureFileRead,
    expected_scope: &str,
) -> Result<Option<PreparedSource>, WarpError> {
    let envelope: CacheEnvelope =
        serde_json::from_slice(&read.bytes).map_err(|_| WarpError::Storage)?;
    if envelope.schema_version != CACHE_SCHEMA_VERSION || envelope.account_scope != expected_scope {
        return Ok(None);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(envelope.payload_bytes_base64.as_bytes())
        .map_err(|_| WarpError::Storage)?;
    if payload.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(WarpError::Storage);
    }
    let mac = URL_SAFE_NO_PAD
        .decode(envelope.payload_mac.as_bytes())
        .map_err(|_| WarpError::Storage)?;
    transaction
        .verify_hmac(
            CACHE_MAC_DOMAIN,
            &[expected_scope.as_bytes(), payload.as_slice()],
            &mac,
        )
        .map_err(|_| WarpError::Storage)?;
    let usage: WarpNormalizedUsage =
        serde_json::from_slice(&payload).map_err(|_| WarpError::Storage)?;
    if usage.account_scope != expected_scope {
        return Err(WarpError::ScopeMismatch);
    }
    source_from_usage(transaction, read.path, read.modified_ms, usage).map(Some)
}

fn source_from_usage(
    transaction: &RefreshTransaction,
    path: PathBuf,
    modified_ms: u64,
    usage: WarpNormalizedUsage,
) -> Result<PreparedSource, WarpError> {
    let payload = serde_json::to_vec(&usage).map_err(|_| WarpError::Internal)?;
    let fingerprint = transaction
        .hmac_digest(
            SOURCE_FINGERPRINT_DOMAIN,
            &[usage.account_scope.as_bytes(), payload.as_slice()],
        )
        .map_err(|_| WarpError::Storage)?;
    let source = WarpUsageSource::new(path, usage.clone(), fingerprint, modified_ms)
        .map_err(|_| WarpError::InvalidUsage)?;
    Ok(PreparedSource { source, usage })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::COOKIE;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::{mpsc, Mutex};

    static STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_state_for_test() {
        let mut state = STATE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.mode = None;
        state.last_error = None;
        state.revoked_scopes.clear();
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }

    fn prepared(account: char, fingerprint: u8) -> PreparedSource {
        prepared_with_requests(account, fingerprint, 1)
    }

    fn prepared_with_requests(
        account: char,
        fingerprint: u8,
        requests_used: i64,
    ) -> PreparedSource {
        prepared_at_path(
            PathBuf::from("usage.json"),
            account,
            fingerprint,
            requests_used,
            1,
        )
    }

    fn prepared_at_path(
        path: PathBuf,
        account: char,
        fingerprint: u8,
        requests_used: i64,
        modified_ms: u64,
    ) -> PreparedSource {
        let usage = WarpNormalizedUsage {
            version: WARP_NORMALIZED_USAGE_VERSION,
            synced_at_ms: 1,
            account_scope: account.to_string().repeat(43),
            usage: WarpAggregateUsage {
                requests_used: Some(requests_used),
                ..Default::default()
            },
            workspaces: Vec::new(),
        };
        PreparedSource {
            source: WarpUsageSource::new(path, usage.clone(), [fingerprint; 32], modified_ms)
                .unwrap(),
            usage,
        }
    }

    fn serve_once(response: Vec<u8>, delay: Duration) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = vec![0_u8; 16 * 1024];
            let read = stream.read(&mut request).unwrap_or(0);
            let _ = sender.send(String::from_utf8_lossy(&request[..read]).into_owned());
            std::thread::sleep(delay);
            let _ = stream.write_all(&response);
        });
        (format!("http://{address}"), receiver)
    }

    fn serve_sequence(responses: Vec<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0_u8; 16 * 1024];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(&response);
            }
        });
        format!("http://{address}")
    }

    fn json_response(status: &str, body: &[u8]) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect()
    }

    fn fetch_error<T>(result: Result<T, FetchFailure>) -> FetchFailure {
        match result {
            Err(error) => error,
            Ok(_) => panic!("expected fetch failure"),
        }
    }

    fn fetch_ok<T>(result: Result<T, FetchFailure>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("unexpected fetch failure: {}", error.error.code()),
        }
    }

    fn decode_request() -> RequestLimitData {
        serde_json::from_value(serde_json::json!({
            "requestLimitInfo": {
                "requestLimit": 100,
                "requestsUsedSinceLastRefresh": 42,
                "nextRefreshTime": "2026-08-01T00:00:00Z",
                "bonusGrantsInfo": {
                    "spendingInfo": {
                        "currentMonthSpendCents": 1234,
                        "currentMonthCreditsPurchased": 500
                    }
                }
            }
        }))
        .unwrap()
    }

    fn decode_workspaces() -> WorkspaceData {
        serde_json::from_value(serde_json::json!({
            "workspacesMetadataForUser": [{
                "id": "sentinel-workspace-id",
                "totalRequestsUsedSinceLastRefresh": 10,
                "aiOverages": {
                    "currentMonthlyRequestCostCents": 345,
                    "currentMonthlyRequestsUsed": 12
                },
                "usageInfo": { "requestsUsedSinceLastRefresh": 11 }
            }]
        }))
        .unwrap()
    }

    #[test]
    fn request_is_fixed_https_bearer_only_and_queries_exclude_member_identity() {
        let client = build_client(true).unwrap();
        let request = client
            .post(ENDPOINT)
            .bearer_auth("sentinel-secret")
            .json(&GraphqlRequest {
                operation_name: "GetWorkspacesMetadataForUser",
                query: WORKSPACES_QUERY,
                variables: EmptyVariables {},
            })
            .build()
            .unwrap();
        assert_eq!(request.url().as_str(), ENDPOINT);
        assert_eq!(request.url().scheme(), "https");
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer sentinel-secret"
        );
        assert!(request.headers().get(COOKIE).is_none());
        assert!(!WORKSPACES_QUERY.contains("members"));
        assert!(!WORKSPACES_QUERY.contains("userId"));
        assert!(!WORKSPACES_QUERY.contains(" name "));
    }

    #[tokio::test]
    async fn transport_refuses_redirects_caps_bodies_and_classifies_timeouts() {
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (url, _) = serve_once(redirect, Duration::ZERO);
        let error = fetch_error(
            send_graphql::<RequestLimitData>(
                &build_client(false).unwrap(),
                &url,
                "secret",
                "GetRequestLimitInfo",
                REQUEST_LIMIT_QUERY,
            )
            .await,
        );
        assert_eq!(error.error, WarpError::RemoteRejected);

        let oversized = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{{}}",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let (url, _) = serve_once(oversized, Duration::ZERO);
        let error = fetch_error(
            send_graphql::<RequestLimitData>(
                &build_client(false).unwrap(),
                &url,
                "secret",
                "GetRequestLimitInfo",
                REQUEST_LIMIT_QUERY,
            )
            .await,
        );
        assert_eq!(error.error, WarpError::Oversize);

        let mut streamed =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                .to_vec();
        streamed.resize(streamed.len() + MAX_RESPONSE_BYTES as usize + 1, b'x');
        let (url, _) = serve_once(streamed, Duration::ZERO);
        let error = fetch_error(
            send_graphql::<RequestLimitData>(
                &build_client(false).unwrap(),
                &url,
                "secret",
                "GetRequestLimitInfo",
                REQUEST_LIMIT_QUERY,
            )
            .await,
        );
        assert_eq!(error.error, WarpError::Oversize);

        let valid = br#"{"data":{"requestLimitInfo":null},"errors":[]}"#;
        let (url, _) = serve_once(json_response("200 OK", valid), Duration::from_millis(100));
        let error = fetch_error(
            send_graphql::<RequestLimitData>(
                &build_client_with_timeout(false, Duration::from_millis(20)).unwrap(),
                &url,
                "secret",
                "GetRequestLimitInfo",
                REQUEST_LIMIT_QUERY,
            )
            .await,
        );
        assert_eq!(error.error, WarpError::Timeout);
    }

    #[tokio::test]
    async fn two_queries_are_one_all_or_nothing_fetch() {
        let request_body = br#"{"data":{"requestLimitInfo":{"requestLimit":100,"requestsUsedSinceLastRefresh":42,"nextRefreshTime":null,"bonusGrantsInfo":null}},"errors":[]}"#;
        let server = serve_sequence(vec![
            json_response("200 OK", request_body),
            json_response("500 Internal Server Error", b"remote sentinel"),
        ]);
        let failure =
            fetch_error(fetch_remote(&build_client(false).unwrap(), &server, "secret").await);
        assert_eq!(failure.error, WarpError::RemoteServer);

        let workspace_body = br#"{"data":{"workspacesMetadataForUser":[]},"errors":[]}"#;
        let server = serve_sequence(vec![
            json_response("200 OK", request_body),
            json_response("200 OK", workspace_body),
        ]);
        let (request, workspaces) =
            fetch_ok(fetch_remote(&build_client(false).unwrap(), &server, "secret").await);
        assert_eq!(request.request_limit_info.unwrap().request_limit, Some(100));
        assert!(workspaces.workspaces_metadata_for_user.is_empty());
    }

    #[tokio::test]
    async fn transport_sends_bearer_without_cookie_and_sanitizes_graphql_errors() {
        let body = br#"{"data":null,"errors":[{"message":"sentinel remote body"}]}"#;
        let (url, request) = serve_once(json_response("200 OK", body), Duration::ZERO);
        let error = fetch_error(
            send_graphql::<RequestLimitData>(
                &build_client(false).unwrap(),
                &url,
                "sentinel-secret",
                "GetRequestLimitInfo",
                REQUEST_LIMIT_QUERY,
            )
            .await,
        );
        assert_eq!(error.error, WarpError::Graphql);
        let request = request
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .to_ascii_lowercase();
        assert!(request.contains("authorization: bearer sentinel-secret"));
        assert!(!request.contains("cookie:"));
        assert!(!error.error.code().contains("sentinel"));
    }

    #[test]
    fn two_typed_paths_normalize_without_raw_workspace_identity() {
        let normalized = normalize_remote_with(
            decode_request(),
            decode_workspaces(),
            "A".repeat(43),
            |_| Ok("B".repeat(43)),
        )
        .unwrap();
        let wire = serde_json::to_string(&normalized).unwrap();
        assert_eq!(normalized.usage.requests_used, Some(42));
        assert_eq!(normalized.usage.spend_cents, Some(1234));
        assert_eq!(normalized.workspaces[0].requests_used, Some(12));
        assert_eq!(normalized.workspaces[0].spend_cents, Some(345));
        assert!(!wire.contains("sentinel-workspace-id"));
        assert!(!wire.contains("Personal"));
    }

    #[test]
    fn status_classes_are_fixed_and_rate_limit_is_at_least_five_minutes() {
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED, None).error,
            WarpError::Unauthorized
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN, None).error,
            WarpError::Forbidden
        );
        let before = now_ms();
        let limited = classify_status(
            StatusCode::TOO_MANY_REQUESTS,
            Some(&reqwest::header::HeaderValue::from_static("1")),
        );
        assert_eq!(limited.error, WarpError::RateLimited);
        assert!(limited.retry_at_ms.unwrap() >= before + DEFAULT_RETRY_MS);
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR, None).error,
            WarpError::RemoteServer
        );
    }

    #[test]
    fn trust_boundary_rejects_graphql_errors_unknown_shapes_and_bad_bearers() {
        let graphql: Result<GraphqlEnvelope<RequestLimitData>, _> = serde_json::from_value(
            serde_json::json!({"data": null, "errors": [{"message": "sentinel remote"}]}),
        );
        assert!(!graphql.unwrap().errors.is_empty());
        let unknown: Result<RequestLimitData, _> = serde_json::from_value(serde_json::json!({
            "requestLimitInfo": null,
            "responseIdentity": "sentinel-response-id"
        }));
        assert!(unknown.is_err());
        assert_eq!(validate_bearer(""), Err(WarpError::InvalidBearer));
        assert_eq!(
            validate_bearer("Bearer token"),
            Err(WarpError::InvalidBearer)
        );
        assert_eq!(
            validate_bearer("token\nCookie: x"),
            Err(WarpError::InvalidBearer)
        );
        assert!(validate_external_filename(Path::new("/tmp/usage.json")).is_ok());
        for sibling in [
            "/tmp/usage-old.json",
            "/tmp/usage.json.backup",
            "/tmp/.usage.json.tmp",
            "/tmp/usage-archive.json",
        ] {
            assert_eq!(
                validate_external_filename(Path::new(sibling)),
                Err(WarpError::InvalidExternalPath)
            );
        }
    }

    #[test]
    fn external_shape_requires_exact_known_fields_but_discards_workspace_name() {
        let raw: ExternalUsage = serde_json::from_value(serde_json::json!({
            "version": 1,
            "syncedAt": "2026-07-22T00:00:00Z",
            "usage": {
                "requestsUsed": 1,
                "requestLimit": 2,
                "spendCents": 3,
                "creditsPurchasedCents": 4,
                "nextRefreshTime": "2026-08-01T00:00:00Z"
            },
            "workspaces": [{
                "id": "sentinel-workspace-id",
                "name": "sentinel-workspace-name",
                "requestsUsed": 1,
                "spendCents": 2
            }]
        }))
        .unwrap();
        assert_eq!(
            raw.workspaces[0].name.as_deref(),
            Some("sentinel-workspace-name")
        );
        let with_unknown: Result<ExternalUsage, _> = serde_json::from_value(serde_json::json!({
            "version": 1,
            "syncedAt": "2026-07-22T00:00:00Z",
            "usage": {},
            "workspaces": [],
            "responseIdentity": "sentinel-response-id"
        }));
        assert!(with_unknown.is_err());
    }

    #[test]
    fn app_external_rotation_and_logout_expose_exactly_one_active_source() {
        let _guard = STATE_TEST_LOCK.lock().unwrap();
        clear_mode();
        let before = generation();
        install_mode(Mode::App(AppMode {
            bearer: "process-memory-only".to_string(),
            account_scope: "A".repeat(43),
            prepared: prepared('A', 1),
            stale: false,
            retry_at_ms: None,
            error: None,
        }));
        let app = source_snapshot();
        assert!(app.scanner_settings.warp_usage_source.is_some());
        assert_eq!(status().mode, "app");
        assert!(app.generation > before);

        install_mode(Mode::External(ExternalMode {
            prepared: prepared('B', 2),
            stale: false,
            error: None,
        }));
        let external = source_snapshot();
        assert!(external.scanner_settings.warp_usage_source.is_some());
        assert_eq!(status().mode, "external");
        assert!(external.generation > app.generation);
        assert!(external_source_is_current(&prepared('B', 2)));
        assert!(!external_source_is_current(&prepared('C', 2)));
        let wire = serde_json::to_string(&status()).unwrap();
        assert!(!wire.contains("process-memory-only"));
        assert!(!wire.contains(&"A".repeat(43)));

        clear_mode();
        assert!(source_snapshot()
            .scanner_settings
            .warp_usage_source
            .is_none());
        assert_eq!(status().mode, "none");
    }

    #[test]
    fn external_reload_cannot_override_a_newer_source() {
        let _guard = STATE_TEST_LOCK.lock().unwrap();
        clear_mode();
        install_mode(Mode::External(ExternalMode {
            prepared: prepared('A', 1),
            stale: false,
            error: None,
        }));
        let external_generation = generation();
        assert!(install_refreshed_external(
            external_generation,
            Path::new("usage.json"),
            prepared_with_requests('A', 2, 2),
        ));
        assert!(external_source_is_current(&prepared_with_requests(
            'A', 2, 2
        )));
        assert!(!install_refreshed_external(
            external_generation,
            Path::new("usage.json"),
            prepared_with_requests('A', 3, 3),
        ));
        assert!(external_source_is_current(&prepared_with_requests(
            'A', 2, 2
        )));

        install_mode(Mode::App(AppMode {
            bearer: "replacement-bearer".to_string(),
            account_scope: "B".repeat(43),
            prepared: prepared('B', 4),
            stale: false,
            retry_at_ms: None,
            error: None,
        }));
        assert!(!install_refreshed_external(
            generation(),
            Path::new("usage.json"),
            prepared_with_requests('A', 5, 5),
        ));
        assert_eq!(status().mode, "app");
        clear_mode();
    }

    #[test]
    fn external_refresh_failures_keep_last_good_visible_and_success_clears_status() {
        let _guard = STATE_TEST_LOCK.lock().unwrap();
        reset_state_for_test();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "tokenbar-warp-external-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let selected = directory.join("usage.json");
        let valid = br#"{"version":1,"syncedAt":"2026-07-22T00:00:00Z","usage":{"requestsUsed":9,"requestLimit":10,"spendCents":0,"creditsPurchasedCents":0,"nextRefreshTime":null},"workspaces":[]}"#;
        std::fs::write(&selected, valid).unwrap();
        assert!(serde_json::from_slice::<ExternalUsage>(valid).is_ok());
        let canonical = std::fs::canonicalize(&selected).unwrap();
        let last_good = prepared_at_path(canonical.clone(), 'A', 7, 9, 1);
        install_mode(Mode::External(ExternalMode {
            prepared: last_good,
            stale: false,
            error: None,
        }));
        let expected_generation = generation();

        std::fs::remove_file(&selected).unwrap();
        let deleted_error = match read_external_regular_bounded(&canonical, MAX_RESPONSE_BYTES) {
            Err(_) => WarpError::InvalidExternalPath,
            Ok(_) => panic!("deleted external usage source unexpectedly read"),
        };
        assert!(finish_external_refresh(
            expected_generation,
            &canonical,
            Err(deleted_error),
        ));
        assert!(status().stale);
        assert_eq!(
            status().error_code,
            Some(WarpError::InvalidExternalPath.code())
        );
        let source = source_snapshot()
            .scanner_settings
            .warp_usage_source
            .expect("last-good source remains selectable");
        assert_eq!(
            tokscale_core::sessions::warp::parse_warp_source(&source)[0].message_count,
            9
        );

        std::fs::write(&selected, b"{").unwrap();
        let read = read_external_regular_bounded(&canonical, MAX_RESPONSE_BYTES).unwrap();
        assert!(serde_json::from_slice::<ExternalUsage>(&read.bytes).is_err());
        assert!(finish_external_refresh(
            expected_generation,
            &canonical,
            Err(WarpError::Decode),
        ));
        assert!(status().stale);
        assert_eq!(status().error_code, Some(WarpError::Decode.code()));

        for error in [WarpError::Storage, WarpError::InvalidUsage] {
            assert!(finish_external_refresh(
                expected_generation,
                &canonical,
                Err(error),
            ));
            assert!(status().stale);
            assert_eq!(status().error_code, Some(error.code()));
            assert!(source_snapshot()
                .scanner_settings
                .warp_usage_source
                .is_some());
        }

        assert!(finish_external_refresh(
            expected_generation,
            &canonical,
            Ok(prepared_at_path(canonical.clone(), 'A', 7, 9, 1)),
        ));
        let recovered = status();
        assert!(!recovered.stale);
        assert_eq!(recovered.error_code, None);
        assert_eq!(recovered.requests_used, Some(9));

        reset_state_for_test();
        std::fs::remove_file(&selected).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn external_bootstrap_cannot_override_a_newer_app_source() {
        let _guard = STATE_TEST_LOCK.lock().unwrap();
        clear_mode();
        let inactive_generation = generation();
        install_mode(Mode::App(AppMode {
            bearer: "newer-bearer".to_string(),
            account_scope: "B".repeat(43),
            prepared: prepared('B', 1),
            stale: false,
            retry_at_ms: None,
            error: None,
        }));
        assert!(!install_external_if_inactive(
            inactive_generation,
            prepared('A', 2),
        ));
        assert_eq!(status().mode, "app");

        clear_mode();
        let inactive_generation = generation();
        assert!(install_external_if_inactive(
            inactive_generation,
            prepared('A', 3),
        ));
        assert_eq!(status().mode, "external");
        clear_mode();
    }

    #[test]
    fn failed_replacement_preserves_prior_scope_and_restart_cache_is_scope_gated() {
        let old = PriorIdentity {
            mode: Some("app"),
            account_scope: Some("A".repeat(43)),
            retry_at_ms: None,
        };
        assert_eq!(
            set_failure_action(Some(&old), &"B".repeat(43), true, WarpError::Unauthorized,),
            SetFailureAction::PreservePrior
        );
        assert_eq!(
            set_failure_action(Some(&old), &"A".repeat(43), true, WarpError::Forbidden,),
            SetFailureAction::ApplySameScope
        );
        assert_eq!(
            set_failure_action(None, &"A".repeat(43), true, WarpError::Timeout),
            SetFailureAction::InstallCached
        );
        assert_eq!(
            set_failure_action(None, &"A".repeat(43), true, WarpError::Unauthorized),
            SetFailureAction::StayInactive
        );
        assert_eq!(
            set_failure_action(None, &"A".repeat(43), false, WarpError::Timeout),
            SetFailureAction::StayInactive
        );
    }

    #[test]
    fn unauthorized_purge_failure_blocks_residual_cache_on_transient_retry() {
        let _guard = STATE_TEST_LOCK.lock().unwrap();
        reset_state_for_test();
        let account_scope = "R".repeat(43);
        install_mode(Mode::App(AppMode {
            bearer: "revoked-bearer".to_string(),
            account_scope: account_scope.clone(),
            prepared: prepared('R', 1),
            stale: false,
            retry_at_ms: None,
            error: None,
        }));

        let failure = FetchFailure {
            error: WarpError::Unauthorized,
            retry_at_ms: None,
        };
        assert_eq!(
            apply_same_scope_failure_with(&account_scope, failure, || { Err(WarpError::Storage) }),
            Err(WarpError::Storage)
        );
        let failed_purge = status();
        assert_eq!(failed_purge.mode, "none");
        assert_eq!(failed_purge.error_code, Some(WarpError::Storage.code()));

        let residual =
            load_cache_candidate_with(&account_scope, || Ok(Some(prepared('R', 1)))).unwrap();
        assert!(residual.is_none());
        assert_eq!(
            set_failure_action(None, &account_scope, residual.is_some(), WarpError::Timeout,),
            SetFailureAction::StayInactive
        );
        assert!(source_snapshot()
            .scanner_settings
            .warp_usage_source
            .is_none());

        clear_mode();
        assert!(
            load_cache_candidate_with(&account_scope, || Ok(Some(prepared('R', 2))))
                .unwrap()
                .is_none()
        );

        install_authenticated_app_mode(AppMode {
            bearer: "fresh-authenticated-bearer".to_string(),
            account_scope: account_scope.clone(),
            prepared: prepared('R', 3),
            stale: false,
            retry_at_ms: None,
            error: None,
        });
        let mut loaded_after_success = false;
        assert!(load_cache_candidate_with(&account_scope, || {
            loaded_after_success = true;
            Ok(Some(()))
        })
        .unwrap()
        .is_some());
        assert!(loaded_after_success);

        reset_state_for_test();
    }

    #[cfg(unix)]
    #[test]
    fn external_path_identity_preserves_non_utf8_canonical_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let first = PathBuf::from(OsString::from_vec(b"/tmp/warp-\x80/usage.json".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"/tmp/warp-\x81/usage.json".to_vec()));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(path_identity_bytes(&first), path_identity_bytes(&second));
    }

    #[test]
    fn error_codes_never_embed_remote_or_local_details() {
        for error in [
            WarpError::Unauthorized,
            WarpError::Forbidden,
            WarpError::RateLimited,
            WarpError::RemoteServer,
            WarpError::Timeout,
            WarpError::Decode,
            WarpError::Graphql,
            WarpError::Oversize,
            WarpError::Storage,
        ] {
            let code = error.code();
            assert!(code.starts_with("warp_"));
            assert!(!code.contains('/'));
            assert!(!code.contains("sentinel"));
        }
    }
}
