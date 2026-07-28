---
status: active
id: kb-plan-index
kind: index
scope: repository
read_when: selecting or resuming a planned work item
last_verified: 2026-07-28
sources: ["docs/knowledge/current-state.md", "docs/knowledge/vendor-tokscale.md", "docs/knowledge/plans/provider-quota-pace.md", "public issue #45", "public TokenBar-Windows PR #7"]
---

# Plan registry

## 文件目的

這個目錄只保存已整理、仍能讓新 session 接手的 project plan。Plan 的 `status` 是 registry metadata，不代表使用者已授權 push、merge 或 release；integration 仍遵守 [`workflow.md`](../workflow.md)。

| Plan | Status | Scope |
|---|---|---|
| [`provider-quota-pace.md`](provider-quota-pace.md) | active | Provider-wide contract、Stage 7 integration、Windows production port與cross-language parity are complete through M19-B1 |
| [`codex-historical-pace-v2.md`](codex-historical-pace-v2.md) | superseded | Implemented Codex Weekly v2 foundation retained as migration and evaluator evidence |
| [`tokscale-alignment.md`](tokscale-alignment.md) | historical | Completed M15–D2 selective-alignment cycle; current engine work follows `vendor-tokscale.md` |
| [`grok-turn-completed-usage.md`](grok-turn-completed-usage.md) | historical | Completed PR #77 implementation record; current engine work follows `vendor-tokscale.md` |

Historical or superseded private plans remain classified in [`../migration-ledger.md`](../migration-ledger.md); they are not copied wholesale into the public tree.
