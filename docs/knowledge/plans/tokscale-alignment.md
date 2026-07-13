---
status: active
id: kb-plan-tokscale-alignment
kind: plan
scope: repository
read_when: planning the next vendor sync or reviewing issue #45 inventory
last_verified: 2026-07-14
sources: ["vendor/README.md", "public issue #45", "public tokscale history", "docs/knowledge/decisions/0003-selective-upstream-alignment.md"]
---

# Rolling tokscale alignment plan

## 文件目的

TokenBar follows upstream `tokscale` as a rolling source and selects correctness work one bounded milestone at a time. This plan is the concise handoff surface; exact commit rows and local patch details stay in [`vendor/README.md`](../../../vendor/README.md).

## Current ordering

| Workstream | Status | Rule |
|---|---|---|
| Public inventory | active | Keep issue #45 current; inventory does not imply an implementation commitment |
| Vendored correctness | priority | Prefer user-reported wrong/missing data, cache invalidation, and cross-language contract fixes |
| Copilot upstream follow-up | external pending | Track issue #879 and PR #880; keep vendor bookkeeping explicit until upstream status changes |
| New client breadth | deferred | Requires an explicit product decision and a complete streaming/cache/FFI adaptation |
| Pricing pipeline expansion | deferred | Do not port a partial routed-pricing behavior without a complete precedence and safety model |

## Milestone protocol

```mermaid
flowchart TD
    REF[Refresh upstream main] --> INVENTORY[Record new core commits]
    INVENTORY --> DIFF[Read actual diffs]
    DIFF --> MATRIX[Classify each part]
    MATRIX --> SELECT[Choose one narrow milestone]
    SELECT --> FIX[Port and adapt local seams]
    FIX --> TEST[Hermetic, cache, FFI, Swift, and smoke gates]
    TEST --> REVIEW[Self-review and user authorization]
    REVIEW --> LEDGER[Update vendor README and public tracking]
```

A milestone is complete only when the selected diff is explained, excluded hunks are named, stale-cache behavior is addressed, and the exact vendor ledger is updated. A plan entry never authorizes remote integration by itself.

## Required evidence

| Area | Evidence |
|---|---|
| Baseline | Current upstream ref and current vendor tree, not a stale plan snapshot |
| Fidelity | File-level diff showing the selected hunk and every local adaptation |
| Correctness | Hermetic fixture with a failure mode before the change |
| Cache | Schema decision and same-fingerprint rebuild test when output changes |
| Streaming | Fingerprint, lane, mtime, prune, and materialized/streaming parity checks |
| Boundary | C ABI and Swift decoder updates when report options or payloads change |
| Delivery | No push, PR, merge, tag, or release until separately authorized |
