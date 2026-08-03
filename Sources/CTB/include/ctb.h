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
char *tb_quota_curve(const char *client_id, const char *window_key, uint64_t generation);

// Release a string returned by any tb_* entry point.
void tb_free(char *p);

#endif
