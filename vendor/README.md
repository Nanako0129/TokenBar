# Vendored dependencies

| Crate | Source | Vendored from |
|---|---|---|
| `tokscale-core` | [junhoyeo/tokscale](https://github.com/junhoyeo/tokscale) (`crates/tokscale-core`, MIT) | [Nanako0129/TokenBar](https://github.com/Nanako0129/TokenBar) `vendor/tokscale-core` @ `606cae1` (v0.4.4: backfill missing cache rates from runner-up pricing source) |

> **Sync rule (historical):** the Tauri repo (`Nanako0129/TokenBar-Tauri`,
> archived 2026-06-12) used to be the single upstream-sync point. With it
> archived, this repo now owns the vendored copy; future syncs come straight
> from junhoyeo/tokscale and must re-apply the local patches below.

> **Baseline:** this vendor's true upstream baseline is ≈ junhoyeo/tokscale
> `0c820a5d` (cc-mirror #618) — **not** the `version = "3.0.0"` in `Cargo.toml`,
> which is stale. It sits between upstream v3.0.0 and v3.1.x, plus the local
> patches below, plus the cherry-picked upstream commits listed next.

## Cherry-picked upstream commits (ahead of baseline)

Specific later upstream fixes pulled in selectively (we do **not** re-vendor
wholesale, which would clobber the streaming/dedup/cowork patches below). The
next sync should treat these as already-present and not re-apply them.

| Commit | What | Files |
|---|---|---|
| `#614` (1a305f0f), `#658` (5c1fe659), `#634` (aebe4ea8) | Modern-Claude pricing: parse `claude-{family}-{major}-{minor}` from the id instead of a hardcoded opus-4.7/4.6/4.5 chain, never-degrade veto on the resolve path, and skip-unusable exact entries. Fixes ~3x overcharge on newer minors (e.g. `opus-4-8`). **models.dev (`#665`) deliberately excluded.** | `src/pricing/lookup.rs` |
| `#707` (5017eefb), *blocklist hunk only* | `FUZZY_BLOCKLIST` += `claude`/`anthropic`/`model`/`router`; `strips_claude_numeric_minor` hardened via new `is_version_segment`. Stops retired `claude-2.x` eroding to a bare brand token and fuzzy-matching an opus-fast key. The pass-reorder / `prefers_model_part_key` / models.dev parts of #707 were **not** taken. | `src/pricing/lookup.rs` |
| `#659` (7500b303) | cc-mirror `tool_result` keeps its variant client id + provider hint (was hardcoded to `claude`); dedup_key namespaced by client. | `src/sessions/claudecode.rs` |
| `#723` (cbbd0dff) | Copilot CLI OTEL underscore-format cache token attributes (`gen_ai.usage.cache_read_input_tokens`) read as fallbacks (were 0). | `src/sessions/copilot.rs` |
| `#649` (44055841), `#651` (2d90f41d), `#681` (b43dc5f8), `#719` (3a68cf52) | Codex fork-replay correctness: skip replayed parent `token_count` rows in fork/subagent logs, scope the dedup key to the fork parent so sibling replays collapse, and keep user-fork own-turns after a repeated child `session_meta`. Fixes codex token over-counting (nested-parent + sibling forks) and under-counting (user forks). `CACHE_SCHEMA_VERSION` bumped to 19 to reparse stale entries. `lib.rs` change is the ported upstream integration tests (plus a streaming-path regression); production logic is in `codex.rs`. `#646` (turn detection) was already vendored. | `src/sessions/codex.rs`, `src/message_cache.rs`, `src/lib.rs` (tests) |

## Local patches (diverged from upstream)

| Patch | Files | Status upstream |
|---|---|---|
| PR #2 (perf): `HASH_MEMO` + `STORE_MEMO` process-level memos; `LocalParseOptions.modified_after` mtime pruning; `latest_source_mtime_ms()` change probe | `src/message_cache.rs`, `src/lib.rs` | not yet forwarded to junhoyeo/tokscale |
| PR #3 (perf): streaming per-file aggregation replaces materialize-then-aggregate for the graph/model/monthly/hourly reports — `StreamingAggregator` + `SessionizeAccumulator` folded by `scan_messages_streaming` in one cache-aware pass (no full-history `Vec`). Each client lane owns its dedup set (follow-up `0752e35`: prevents cross-client `dedup_key` collisions). | `src/aggregator.rs`, `src/lib.rs`, `src/sessionize.rs`, `tests/streaming_snapshot.rs` | not yet forwarded to junhoyeo/tokscale |
| #6 (fix): the **agents report** now folds over `scan_messages_streaming` too — new `get_agents_report` (mirrors `get_model_report`, `resolve_report_clients` + a single streaming pass into `AgentAccumulator`), so it shares the one deduped/per-client-gated/priced stream as every other report (resolves the issue #6 divergence: agents no longer over-counts copilot/codebuff/kimi/cursor/warp/… duplicate `dedup_key`s, and scans the same client set). `parse_local_unified_messages` survives as public API only (footgun-documented, no in-repo callers). `crates/tb_core_ffi/src/agents_report.rs` is now a thin mapper like `model_report.rs` (no longer byte-identical to the archived Tauri original — accepted). | `src/lib.rs`, `crates/tb_core_ffi/src/agents_report.rs` | not yet forwarded to junhoyeo/tokscale |
| #5 (feat): discover Claude desktop "Cowork" (local-agent-mode) transcripts. `discover_cowork_project_roots()` recurses `~/Library/Application Support/Claude/local-agent-mode-sessions/**/.claude/projects` and feeds the roots into `built_in_extra_scan_paths_for` as `ClientId::Claude`. Returns the per-session `projects` roots only, so the sibling `audit.jsonl` (a mirror of the same `usage` records) is never scanned — scanning it would double-count. | `src/scanner.rs` | not yet forwarded to junhoyeo/tokscale |
| pricing (fix): **cache-rate backfill** — `choose_best_source_result` is wrapped so the chosen pricing source has any missing cache read/write rates grafted from the runner-up source (`backfill_cache_costs` + `prefer_litellm_over_openrouter`). Without it, a provider-hint that selects an entry lacking cache rates (e.g. an OpenRouter row) bills cache reads at $0 (fable-5 showed $11.50 instead of $45). Upstream `#658`/`#707` do **not** subsume this: `has_any_usable_pricing` is an `.any()` gate, so a row that prices everything *except* cache still passes through unfilled — the root cause stays ours to fix. | `src/pricing/lookup.rs` | not yet forwarded to junhoyeo/tokscale |
| pricing (perf): **in-memory auto-refresh** — `PRICING_SERVICE` is a `RwLock<Option<CachedService>>` with `IN_MEMORY_TTL = 3600s` (was a never-refreshing `OnceCell`), so prices re-read the file cache / network roughly hourly instead of being frozen for the process lifetime; adds `pricing_cached_at()` (Models card "Prices updated …"). Spans `mod.rs` + the additive `litellm::cached_at` / `cache::cache_timestamp` helpers. | `src/pricing/mod.rs`, `src/pricing/litellm.rs`, `src/pricing/cache.rs` | not yet forwarded to junhoyeo/tokscale |
