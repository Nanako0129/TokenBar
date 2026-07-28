---
status: active
id: kb-vendor-tokscale
kind: canonical
scope: repository
read_when: assessing upstream commits, changing shared-engine code or its consumer pin, or changing parser output
last_verified: 2026-07-29
sources: [".gitmodules", "vendor/README.md", "public tokscale-core UPSTREAM at b31e394", "docs/knowledge/architecture.md", "docs/knowledge/verification.md", "public issue #45", "public TokenBar PR #114", "public TokenBar-Windows PR #12"]
---

# Shared tokscale engine alignment

## 文件目的

TokenBar consumes the public [`tokscale-core`](https://github.com/Nanako0129/tokscale-core) engine through the pinned `vendor/tokscale-core` submodule. This document explains the consumer boundary and the method for safely aligning the shared engine. The exact upstream baseline, commit table, local patch table, and upstream report numbers live exclusively in the engine's immutable [`UPSTREAM.md`](https://github.com/Nanako0129/tokscale-core/blob/b31e39425859393504a2d56cb5af7c93e6461c7d/UPSTREAM.md); [`vendor/README.md`](../../vendor/README.md) records TokenBar's source and pin.

## 目錄

- [Current boundary](#current-boundary)
- [Selective-port method](#selective-port-method)
- [Shared adaptation families](#shared-adaptation-families)
- [Schema and parser output](#schema-and-parser-output)
- [Sibling-source rule](#sibling-source-rule)
- [Upstream alignment](#upstream-alignment)
- [Handoff checklist](#handoff-checklist)

---

## Current boundary

The engine's true baseline is recorded in `tokscale-core/UPSTREAM.md`; the Cargo package version is not a reliable baseline marker. The shared tree contains upstream cherry-picks plus streaming, cache, report, pricing, and defensive adaptations extracted from TokenBar. TokenBar keeps its application-specific FFI, C ABI, Swift, and build wiring outside the submodule.

> **不要在 consumer branch 直接改 submodule source。** Shared Rust changes first land and pass review in `tokscale-core`; TokenBar then advances only the reviewed gitlink and runs its consumer gates. A clean build alone cannot prove that streaming or cache semantics were preserved.

Native and Windows now pin the same reviewed engine commit through separate consumer migrations. That proves shared-source equality, not complete cross-repository behavior parity：Native and Windows still own separate FFI、C header、Swift／C# bridge and build surfaces. Shared Rust changes land in the engine first；each consumer then advances its gitlink and runs its own app gates, while app-owned ABI changes are ported and independently cross-checked. See [`architecture.md`](architecture.md#windows-downstream-consumer) and the completed [`shared-rust-engine-extraction.md`](plans/shared-rust-engine-extraction.md).

## Selective-port method

```mermaid
flowchart TD
    HEAD[Refresh tokscale upstream] --> DIFF[Read each real diff]
    DIFF --> CLASSIFY{Classify each part}
    CLASSIFY -->|already present| RECORD[Record no action]
    CLASSIFY -->|take| PORT[Apply narrow hunk in shared engine]
    CLASSIFY -->|adapt| ADAPT[Preserve shared streaming or cache seam]
    CLASSIFY -->|defer or skip| EXPLAIN[Record rationale]
    PORT --> FIXTURE[Add old-fail/new-pass fixture]
    ADAPT --> FIXTURE
    FIXTURE --> ENGINE[Run engine gates and review]
    ENGINE --> LEDGER[Update engine UPSTREAM ledger]
    LEDGER --> PIN[Advance reviewed TokenBar gitlink]
    PIN --> GATES[Run FFI, Swift, and smoke gates]
```

| Step | Rule |
|---|---|
| Reference | Re-fetch and record the upstream commit being assessed; do not use a stale plan line number as evidence |
| Diff | Read the actual diff, including multipart commits whose title understates runtime changes |
| Port | Apply only the selected hunk in the shared engine repository; use context-aware patching and fail loudly on mismatch |
| Adapt | Keep shared streaming lanes, report filters, and cache identity explicit; keep TokenBar-only FFI mapping in `crates/tb_core_ffi` |
| Verify | Test parser output, cache rebuild, streaming behavior, and materialized parity in the engine before advancing a consumer |
| Record | Update the exact engine ledger, then pin the reviewed engine commit and run TokenBar's consumer gates |

## Shared adaptation families

| Family | Contract |
|---|---|
| Streaming reports | `scan_messages_streaming`, per-client dedup sets, cross-source authority selectors, `StreamingAggregator`, `SessionizeAccumulator`, and Agents report parity remain local seams |
| Cache | Fingerprints, mtime probes, topology-sensitive in-process report tokens, sibling dependencies, pruning exceptions, schema decisions, and cached attribution rebuilds are local until upstream has the same architecture |
| Pricing | Cache-rate backfill and refreshable pricing are local behavior; upstream cost-provenance ports must not erase them |
| FFI | Report client slices, hourly/Agents filtering, bounded totals, and thin mappers are TokenBar-specific consumers |
| Discovery | Cowork, local client lanes, and platform-specific scanner roots may be local even when the parser originates upstream |
| Defensive fixes | Saturating folds, placeholder-row removal, trace-scoped identity, malformed-input handling, and bounded Windows atomic-replacement retries require their own regression evidence |

## Schema and parser output

The shared engine owns its cache-schema counter. It is schema **32** after the Grok Build `turn_completed.usage` primary path (parser output for existing Grok sources changes under the same fingerprint, so schema-31 context-only rows must rebuild). Historical trail: M20 advanced 29 → 30 for OpenCode v2 hybrid databases, M15-B kept 30 for a new Kiro source, M16 advanced 30 → 31 because existing Codex, Claude, Copilot, Jcode, provider, and Antigravity outputs changed under unchanged source fingerprints, M19-A kept 31 because bounded Windows atomic replacement changes only write transport, and M17 kept 31 because its independently fingerprinted unified source selects authority after raw cache retrieval without changing legacy serialized output. M18 also kept 31: routed and long-context pricing is applied only after raw source-message cache retrieval, so model IDs, fingerprints, parser output, and serialized layout remain unchanged. M21/M25 kept 31 for new clients and post-cache grouping aliases. Do not mirror an upstream schema number merely because the same upstream commit is being ported. Bump the shared schema when serialized message fields, parser output, dedup keys, attribution, or parser-resume state changes make old cached values semantically stale; do not bump for a new independently fingerprinted source, post-cache pricing/report arithmetic, or filesystem retry changes.

A parser-output change must include a same-fingerprint stale-cache regression. A test that only parses a fresh source does not prove that existing users receive the correction.

## Sibling-source rule

When a parser reads a primary file plus metadata, journal, history, or WAL sibling, treat the sibling as part of the source identity. The four required sites are:

| Site | Required behavior |
|---|---|
| Fingerprint | Include every sibling whose content can change parsed meaning |
| Active lane | Streaming and materialized consumers use the same fingerprint function |
| Mtime probe | Live tail sees sibling-only writes |
| Pruning | Modified-after scans retain sessions when a sibling is newer |

This rule applies to JSONL journals, Roo-family history, SQLite WAL files, Claude parent/workflow transcripts, and other secondary sources. A local adaptation is incomplete when only the parser or only the cache loader changes.

## Upstream alignment

The public rolling inventory is tracked in [issue #45](https://github.com/Nanako0129/TokenBar/issues/45). It is an inventory and decision surface, not a promise to clear every deferred capability. Correctness work is prioritized over new client breadth during maintenance. Every selected item must be re-evaluated against the current tokscale upstream head and current shared-engine tree before implementation.

The Copilot nested-agent bookkeeping in the engine's `UPSTREAM.md` records upstream issue [#879](https://github.com/junhoyeo/tokscale/issues/879) as closed and pull request [#880](https://github.com/junhoyeo/tokscale/pull/880) as merged (upstream commit `20d9096a68a40d4a4e83581b0e0dd308aadc5ab7`; GitHub PR merge commit `b7277d49a14ae905c17195be214d632e365b3ca6`). The exact merged diff has been compared with the M10-E hardening: its trace-scoped hierarchy and stale-cache rebuild semantics are equivalent, so no additional production or cache-schema port is needed. The assessment therefore closes as bookkeeping-only. This is no longer an external-upstream wait state; do not resurrect the superseded intermediate report when describing that status.

## Handoff checklist

| Question | Evidence |
|---|---|
| Is the selected upstream hunk present in the shared engine? | Exact file-level diff or stable patch comparison |
| Did a shared adaptation get overwritten? | Engine `UPSTREAM.md` local-patch table and targeted diff |
| Did parser output or attribution change? | Local schema decision plus stale-cache regression |
| Does a sibling source reach all consumers? | Fingerprint, lane, mtime, and prune tests |
| Does FFI expose a pre-aggregation filter? | C header, Rust mapper, Swift decoder, report parity fixture |
| Is the result still selective? | Focused engine fidelity note explaining included and excluded hunks, plus an exact reviewed consumer pin |
