#ifndef CTB_H
#define CTB_H

#include <stdint.h>

// C-ABI surface of crates/tb_core_ffi. Every function returns a heap-allocated
// NUL-terminated JSON string that must be released with tb_free.
//
// Envelope: every entry point except tb_probe returns
//   {"ok":true,"data":<payload>}   on success
//   {"ok":false,"err":"..."}       on failure
// Payload fields use the Tauri frontend's camelCase contract. In particular,
// AgentUsagePayload is `{generatedAt, publicationGeneration?, agents,
// opencodeSubscriptions}` (the subscription array is omitted when empty).
// `publicationGeneration` is an additive optional Rust `u64` JSON integer for
// generated payloads; demo/legacy payloads omit it.
// Rust assigns it with a checked increment at process-wide publication-gate
// entry before the complete provider run; exhaustion returns an outer error
// rather than repeating a generation. The gate orders generations and pointer
// creation, but is released before `tb_agent_usage` returns and therefore does
// not promise C return order. Swift's shared MainActor publication coordinator
// rejects a lower generation for dashboard, Settings, tray, and snapshot
// consumers. AgentUsage snapshots may additionally carry the additive optional
// camelCase field
// `transportDiagnostic?: {category, status?, osCode?}`. `error` remains the
// user-visible provider status and may coexist with last-good windows;
// `transportDiagnostic` is the only provider failure detail permitted in the
// public Unified Log. Its `category` is limited to timeout/dns/tls/
// connectionRefused/connectionReset/connect/request/responseBody/rateLimited/
// serverError. `rateLimited` accepts only status 429; `serverError` accepts only
// 500...599. HTTP categories do not carry `osCode`; non-HTTP categories do not
// carry `status`. `osCode`, when present, is a 32-bit OS error integer. Neither
// field carries token, header,
// body, URL/query, email, account ID, credential path, or free-form cause data.
// Other report payloads retain their existing camelCase shapes from the Tauri
// contract. Adding these fields does not change C function signatures, ownership,
// ABI, or any other wire fields. Each v3 quota window uses
// `{cardId, label, usedPercent, remainingPercent, resetsAt, resetText,
// windowMinutes, paceStatus, historicalPace}`. `paceStatus` is required and
// carries `{state, windowKey, durationSeconds, durationSource, completeCycles,
// reason}`; positive durationSeconds is the pace calculation source of truth,
// while windowMinutes is compatibility output derived by integer division.
// historicalPace is present only for `available` and carries one coherent Rust
// result: `{expectedUsedPercent, etaSeconds, willLastToReset,
// runOutProbability}`. A legacy payload missing the entire paceStatus key is
// not eligible for an implicit Linear fallback. ETA/risk remain optional inside
// an available historical result. Other report payloads retain their existing
// camelCase shapes from the Tauri contract.
// tb_probe keeps its Phase 0 shape: {"ok":true,"messages":N} / {"ok":false,...}.
//
// `year` parameters may be NULL or "" for the all-time view, otherwise a
// 4-digit year string ("2026"). All calls are blocking; tb_agent_usage also
// performs network requests — invoke from a background thread.

// Smoke probe: total locally parsed messages.
char *tb_probe(void);

// Contribution graph (UsagePayload). Serves a <=30s-old cached payload.
char *tb_graph(const char *year);
// Contribution graph, always recomputed (cache refreshed as a side effect).
char *tb_refresh_graph(const char *year);

// Per-model report (ModelReport).
char *tb_model_report(const char *year);
// Per-hour report (HourlyReport). `clients` = comma-joined canonical ids to
// restrict to, or NULL/empty for all clients (filtered in the streaming scan).
char *tb_hourly_report(const char *year, const char *clients);
// Per-(sub-)agent report (AgentsReport). `clients` as in tb_hourly_report.
char *tb_agents_report(const char *year, const char *clients);

// Source-generation-aware hourly/Agents filter parity diagnostic. The graph
// client list is derived from a fresh graph and all reports are bracketed by
// one opaque local-source token sequence. The success payload contains only
// lower-camel status values (match/mismatch/sourceChanged/tokenUnavailable),
// bounded report aggregates, and presentClientCount; it never exposes source
// paths, raw messages, cache data, credentials, providers, models, agents, or
// workspaces. A token probe failure is a successful tokenUnavailable result;
// graph/report/mapping/serialization failures use the normal outer error
// envelope. All calls are blocking and must be made off the main thread.
char *tb_filter_parity_probe(void);

// Live trace buckets over the trailing window (array of TraceBucket;
// snake_case fields, e.g. tokens_per_min). Lazily re-parses at most every 10s.
char *tb_usage_trace(int64_t window_secs);
// Live rate: {"tokensPerMin": <number>} (10-minute-window average).
char *tb_tokens_per_min(void);

// OAuth quota cards (AgentUsagePayload) for codex/claude/antigravity/copilot/grok.
// Network-bound; per-provider failures are reported inside each snapshot.
char *tb_agent_usage(void);

// Read-only quota curve snapshot for one series selected by the latest
// successful agent-usage publication generation. This call performs no network
// request; the returned JSON is released with tb_free.
// `account_key` selects which account's series to read: NULL or empty is the
// primary account, which is every account that exists today. Passing the wrong
// one returns another account's curve under a generation that validates, so it
// is a parameter rather than something inferred here.
char *tb_quota_curve(const char *client_id, const char *account_key, const char *window_key,
                     uint64_t generation);

/* PROTOTYPE - usage inside an absolute [from_ms, until_ms) window.
 *
 * `until_ms` is quantised to the minute for caching, so two calls sharing a
 * `from_ms` and landing in the same minute are answered from one scan: the
 * later call can be up to a minute short of its own end. Deliberate, and
 * tested (`window_usage::tests::quantised_window_calls_scan_once`) — the sole
 * consumer already serves its own scan for 30s before asking again, so exact
 * ends would buy precision nobody reads at the cost of a 0% hit rate.
 *
 * `account_key` selects whose transcripts are read: NULL for the primary
 * account, an extra Claude account's `CLAUDE_CONFIG_DIR` otherwise — the same
 * value `tb_quota_curve` takes, so a window's usage and the quota it is
 * divided against are scoped by one string. There is no "every account"
 * argument: a quota window belongs to an account, and a total spanning
 * accounts has no quota reading to divide by. */
char *tb_window_usage(const char *account_key, int64_t from_ms, int64_t until_ms);
// Replace the process-wide extra-scan-paths registry used by every
// subsequent report/parse call (no restart needed). `json` is an object of
// `{"<public-client-id>": ["<absolute-dir-path>", ...]}`, full-replace
// semantics ({} clears it). Success data is
// `{"registeredCount":N,"unreadable":[{"client","path","reason"}],"rejected":[{"client","path","reason"}]}`.
// A directory that merely can't be read right now (unmounted volume, config
// dir not yet created) is still registered: it is listed in `unreadable` and
// picked up automatically by the next scan, with no need to call this again.
// `rejected` paths are NOT registered and will never contribute — because the
// client id has no extra-root support here, or because the path cannot become
// a scan root at all (empty, relative, or an existing non-directory). The last
// case matters: the scanner walks any path that exists, so a transcript FILE
// passed as a root would otherwise be ingested while being reported as merely
// unreadable. Malformed JSON returns the normal error envelope and leaves the
// registry untouched.
char *tb_set_extra_scan_paths(const char *json);
// Replace the process-wide registry of extra Claude config directories — the
// `CLAUDE_CONFIG_DIR`-isolated accounts that each get their own quota card.
// `json` is an array of absolute directory paths, e.g.
// `["/Users/x/.claude-work"]`, full-replace semantics ([] clears it). Success
// data is `{"registeredCount":N,"rejected":[{"path","reason"}]}`; a path is
// rejected when it is empty, relative, the filesystem root, or a repeat.
// Existence is NOT probed: whether a directory is readable right now says
// nothing about which account its credential belongs to.
//
// This is not `tb_set_extra_scan_paths`. That one takes the expanded
// `<dir>/projects` and `<dir>/transcripts` sub-roots and decides which
// directories the usage scanner walks; this one takes the config directories
// themselves and decides whose credential each quota card is fetched with.
// Passing one where the other is expected fails silently in both directions.
char *tb_set_claude_config_dirs(const char *json);
// Replace the process-wide registry of macOS Keychain consent — which clients
// the user has agreed to let this process read a Keychain item for. `json` is
// `{"<public-client-id>": true|false}`, e.g. `{"grok-bot":true}`, full-replace
// semantics ({} clears every grant). Success data is
// `{"grantedCount":N,"rejected":[{"client","reason"}]}`; a client id this
// consumer does not wire Keychain consent for is rejected and never stored,
// while `false` is an ordinary answer rather than a rejection.
//
// The intent is that a client absent from the registry is never read from the
// Keychain at all, so the OS authorization dialog cannot appear before the app
// has asked the user itself. "grok-bot" is the only id this build accepts.
//
// !! As of this revision NO adapter consults the registry yet, so setting it
// does not change behaviour: the Grok Bot Keychain read still happens and the
// dialog still appears unannounced. This entry point lands ahead of its
// consumer because `ctb.h` is a cross-repo contract and the Windows port needs
// the symbol to port; the macOS-only gate follows in a separate change. Do not
// quote the guarantee above as something a caller can rely on until then.
//
// The registry is in-memory and starts empty every launch, so the caller owns
// re-applying the user's stored answer at startup; a process that never calls
// this leaves the registry empty.
char *tb_set_keychain_consent(const char *json);

// Release a string returned by any tb_* entry point.
void tb_free(char *p);

#endif
