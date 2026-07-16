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
// AgentUsagePayload is `{generatedAt, agents, opencodeSubscriptions}` (the
// subscription array is omitted when empty) and each quota window uses
// `{label, usedPercent, remainingPercent, resetsAt,
// resetText, windowMinutes, historicalPace}`. When historicalPace is present,
// it is one nested result with `{expectedUsedPercent, etaSeconds,
// willLastToReset, runOutProbability}`; missing/null historicalPace means the
// Swift presentation layer uses its linear pace fallback. The nested result is
// optional and its ETA/risk fields may be omitted or null. Other report
// payloads retain their existing camelCase shapes from the Tauri contract.
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

// Live trace buckets over the trailing window (array of TraceBucket;
// snake_case fields, e.g. tokens_per_min). Lazily re-parses at most every 10s.
char *tb_usage_trace(int64_t window_secs);
// Live rate: {"tokensPerMin": <number>} (10-minute-window average).
char *tb_tokens_per_min(void);

// OAuth quota cards (AgentUsagePayload) for codex/claude/antigravity/copilot.
// Network-bound; per-provider failures are reported inside each snapshot.
char *tb_agent_usage(void);

// Release a string returned by any tb_* entry point.
void tb_free(char *p);

#endif
