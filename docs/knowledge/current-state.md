---
status: active
id: kb-current-state
kind: canonical
scope: repository
read_when: starting work, triaging an issue, or deciding whether an upstream item is urgent
last_verified: 2026-07-15
sources: ["public GitHub main history", "public issue #45", "vendor/README.md", "docs/knowledge/history/README.md", "docs/knowledge/plans/tokscale-alignment.md"]
---

# Current state

## 文件目的

TokenBar native 已完成從 Tauri 到 SwiftUI 的出貨重寫，現在是維護期。這份文件只保留接手時需要的狀態、優先級與公開追蹤入口；完整歷史放在 [`history/`](history/README.md)，vendor 細節放在 [`vendor/README.md`](../../vendor/README.md)。

## 目錄

- [Maintenance posture](#maintenance-posture)
- [Shipped baseline](#shipped-baseline)
- [Priority order](#priority-order)
- [Tracked work](#tracked-work)
- [Deferred and parked](#deferred-and-parked)
- [Handoff questions](#handoff-questions)

---

## Maintenance posture

維護期的排序是 user-reported correctness 優先，其次是可驗證的資料遺失、stale cache、跨語言契約與 release chain 問題；新 client breadth 或大型 UI 重構沒有自動優先權。公開 issue [#45](https://github.com/Nanako0129/TokenBar/issues/45) 是完整 upstream inventory 與決策面，不是「每一列都必須清掉」的 backlog。

> **接手原則：** 先確認問題是否影響實際使用者的數字或更新路徑，再決定要修、報上游、defer，或明確 parked。不要把 inventory 的數量當成工作量承諾。

## Shipped baseline

| Area | Current evidence |
|---|---|
| Product | Native SwiftUI menu-bar app is the shipping line; the predecessor Tauri repository is archived and remains a legacy migration source |
| Vendor | Current main tree includes selective tokscale alignment through Roo-family related-source freshness; local cache schema is 28 |
| Correctness | Cost provenance, Jcode correction turns, Pi metadata, Claude workflow transcripts, Copilot hierarchy, hidden-client filtering, and bounded folds have landed in staged releases or main history |
| Release | Stable Sparkle feed, Homebrew cask, legacy update metadata, and landing Pages workflows are maintained as separate delivery surfaces |
| Current repository baseline | Before each task, fetch and resolve the current `origin/main`; this document is not a commit pin |

Setup-token quota fallback is shipped: when profile usage is unavailable, provider rate-limit responses can still present quota to the user.

## Priority order

| Priority | Trigger | First reading |
|---|---|---|
| 1 | User-reported wrong or missing usage, cost, quota, or hidden-client data | [`architecture.md`](architecture.md), [`verification.md`](verification.md) |
| 2 | Regression at Rust -> C ABI -> Swift or cache invalidation seam | [`architecture.md`](architecture.md), [`vendor-tokscale.md`](vendor-tokscale.md) |
| 3 | Upstream correctness item with a narrow local adaptation | [`plans/tokscale-alignment.md`](plans/tokscale-alignment.md), [`vendor/README.md`](../../vendor/README.md) |
| 4 | Release, appcast, cask, or migration failure | [`release.md`](release.md), [`history/release-and-ui-incidents.md`](history/release-and-ui-incidents.md) |
| 5 | Cosmetic or broad product expansion | Requires an explicit product decision; do not infer from issue #45 inventory |

## Tracked work

| Workstream | Status | Public surface |
|---|---|---|
| Copilot upstream follow-up | Assessment complete: merged PR #880 is equivalent to the local M10-E trace-scoped hierarchy and cache invalidation; no additional code or schema port is needed | [issue #879](https://github.com/junhoyeo/tokscale/issues/879), [PR #880](https://github.com/junhoyeo/tokscale/pull/880) |
| Rolling tokscale alignment | Active inventory and decision process; work is selective, not wholesale | [issue #45](https://github.com/Nanako0129/TokenBar/issues/45), [`plans/tokscale-alignment.md`](plans/tokscale-alignment.md) |
| Day-bar empty-today behavior | Parked, because changing the right edge changes the visible chart and needs a focused fixture plus UI verification | No public commitment beyond the maintenance note |
| Liquid Glass parity | Parked; current glass recipe remains the shipped status quo | [`history/liquid-glass-experiments.md`](history/liquid-glass-experiments.md) |

## Deferred and parked

The following are intentionally not implied by a green build: a full transparent-panel Liquid Glass re-architecture, broad new-client adoption, project-private maintenance work, user-specific writing or tool preferences, and other-project plans. Those items remain in their private or other-project source and are classified in [`migration-ledger.md`](migration-ledger.md).

## Handoff questions

Before starting a new task, answer these questions from canonical sources:

| Question | Source |
|---|---|
| Where is the value computed, and is it already pre-aggregated? | [`architecture.md`](architecture.md) |
| What is the smallest deterministic fixture? | [`verification.md`](verification.md) |
| Could a vendor sync erase a local seam? | [`vendor-tokscale.md`](vendor-tokscale.md) and [`vendor/README.md`](../../vendor/README.md) |
| Is the proposed change authorized to reach main or a release channel? | [`workflow.md`](workflow.md) |
| Is this a must-fix, tracked follow-up, or inventory-only item? | This document plus public issue #45 |
