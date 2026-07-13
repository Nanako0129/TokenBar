---
status: active
id: kb-migration-ledger
kind: ledger
scope: repository
read_when: auditing knowledge coverage, adding a memory/plan source, or deciding what stays private
last_verified: 2026-07-14
sources: ["memory index", "project memory set", "plan set", "sanitized local guidance source", "sanitized local visual-experiment source"]
source_total: 58
memory_count: 37
plan_count: 19
local_count: 2
boundary_counts: {memory: 37, plan: 19, local: 2}
---

# Migration ledger

## 文件目的

這份 ledger 是本次 canonical knowledge migration 的 no-gaps inventory。它盤點 37 個 memory source、19 個 plan source 與 2 個 local source；每個實體只出現一次，以 opaque ID 表示。Private 與 other-project source 只記分類、sanitized topic、處置與保留邊界，不暴露原始檔名、credential 位置、private environment、machine-specific tooling 或 private draft 內容。

> **安全邊界：** 這份表是分類證據，不是 private source 的副本。Private row 的 destination 會回到本 ledger 的 retention section；它不代表 private material 已經公開遷移。

## 目錄

- [Coverage](#coverage)
- [Treatment vocabulary](#treatment-vocabulary)
- [Source rows](#source-rows)
- [Private retention](#private-retention)
- [Other-project sources](#other-project-sources)
- [No-gaps verification](#no-gaps-verification)

---

## Coverage

| Source kind | Rows required | Rows present |
|---|---:|---:|
| Memory | 37 | 37 |
| Plan or plan artifact | 19 | 19 |
| Local migration source | 2 | 2 |
| **Total** | **58** | **58** |

## Treatment vocabulary

| Treatment | Meaning |
|---|---|
| summarize | Project-owned conclusion or durable rationale was rewritten into canonical documents; transient session detail was removed |
| park | Decision remains useful, but this migration authorizes no implementation |
| retain-private | Source stays outside this repository, including user-private and other-project material |
| supersede | A newer plan or canonical source replaces the old planning snapshot; the old source remains available for history |
| split | One mixed source was divided into public routing/facts and private retained material |

## Source rows

| source | kind | topic | status | privacy | treatment | destination | verification |
|---|---|---|---|---|---|---|---|
| `SRC-001` | memory | Project memory index and source catalog | active | repo-public | summarize | `docs/knowledge/README.md` and this ledger / canonical tree | Index count reconciled with the 37 memory rows |
| `SRC-002` | memory | Tauri lineage, vendor origin, and native product history | historical | repo-public | summarize | `history/native-rewrite.md`, `architecture.md`, `current-state.md` / public README and source tree | Cross-checked against current native targets and public product README |
| `SRC-003` | memory | Native rewrite execution history and maintenance status | active | repo-public | summarize | `current-state.md`, `history/native-rewrite.md`, `history/release-and-ui-incidents.md` / current repository history | Current baseline and maintenance posture checked against public main history |
| `SRC-004` | memory | Liquid Glass investigation and parked decision | parked | repo-public | summarize | `history/liquid-glass-experiments.md` / current glass source and sanitized local experiment source | Verdict and winning spike structure compared with the read-only local source |
| `SRC-005` | memory | Checkpoint discipline for long work items | active | repo-public | summarize | `workflow.md`, `verification.md` / canonical workflow | Rule reduced to project-owned checkpoint behavior; private orchestration removed |
| `SRC-006` | memory | Release-note generation drift and post-release check | active | repo-public | summarize | `release.md`, `communication.md` / release workflow and published artifacts | Three published note surfaces and override behavior checked against release source |
| `SRC-007` | memory | Credit by diagnostic contribution | active | repo-public | summarize | `communication.md` / public release history | Rule retained without private reporter details |
| `SRC-008` | memory | Refreshable vendored pricing cache | historical | repo-public | summarize | `architecture.md`, `vendor-tokscale.md` / vendor README | Pricing refresh behavior checked against vendor patch ledger |
| `SRC-009` | memory | Cache-rate backfill for provider-hinted pricing | historical | repo-public | summarize | `architecture.md`, `vendor-tokscale.md` / vendor README | Local patch remains listed in exact vendor source |
| `SRC-010` | memory | Private project notes | active | project-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private project material excluded |
| `SRC-011` | memory | Private user preferences | active | user-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private preference material excluded |
| `SRC-012` | memory | Private writing preferences | active | user-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private writing material excluded |
| `SRC-013` | memory | Private draft material | active | user-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private draft material excluded |
| `SRC-014` | memory | Private editorial notes | historical | user-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private editorial material excluded |
| `SRC-015` | memory | Hermetic fixture verification method | active | repo-public | summarize | `verification.md`, `decisions/0002-streaming-and-preaggregation.md` / canonical verification | Fixture properties and limitations checked against current test conventions |
| `SRC-016` | memory | Explicit merge, push, and release authorization | active | repo-public | summarize | `workflow.md`, `AGENTS.md` / canonical workflow | Authorization boundary appears once in canonical workflow and adapter guardrails |
| `SRC-017` | memory | External issue and PR review workflow | active | repo-public | summarize | `workflow.md`, `verification.md`, `communication.md` / public contribution process | No private account or credential mechanics copied |
| `SRC-018` | memory | Public PR and issue writing style | active | repo-public | summarize | `communication.md` / public GitHub surfaces | Hard-wrap, Unicode, and technical-reply rules represented without private tool paths |
| `SRC-019` | memory | tokscale sync history and selective alignment lessons | active | repo-public | summarize | `vendor-tokscale.md`, `plans/tokscale-alignment.md`, `decisions/0003-selective-upstream-alignment.md` / vendor README and issue #45 | Current schema, selective boundary, and upstream bookkeeping checked against vendor source |
| `SRC-020` | memory | Branch naming and PR authorization workflow | active | repo-public | summarize | `workflow.md`, `AGENTS.md` / canonical workflow | Rule matches adapter authorization and current public contribution history |
| `SRC-021` | memory | Traditional Chinese punctuation and bilingual layout | active | repo-public | summarize | `communication.md`, `docs/knowledge/README.md` / canonical style contract | Chinese punctuation and bilingual separator rules reviewed in new docs |
| `SRC-022` | memory | Sparkle multi-item appcast correction | historical | repo-public | summarize | `release.md`, `history/release-and-ui-incidents.md` / release script and appcast | Multi-item, channel, and preservation semantics checked against release source |
| `SRC-023` | memory | Agent icon audit and shipped correction | historical | repo-public | summarize | `history/release-and-ui-incidents.md` / current resources and public release history | Official-asset and renderer-support lessons retained without private asset URLs or scratchpad details |
| `SRC-024` | memory | Retired beta bridge bundle-identity incident | historical | repo-public | summarize | `release.md`, `history/release-and-ui-incidents.md` / `BetaMigration.swift` and release workflow | Bundle-selection root cause and Switch path verified against current source |
| `SRC-025` | memory | Homebrew tap rename decision | parked | repo-public | park | `release.md`, `current-state.md` / public tap and release workflow | Current tap naming and deferred migration boundary kept without operational secrets |
| `SRC-026` | memory | Zero-action UX principle | active | repo-public | summarize | `release.md` / canonical delivery rules | Principle retained without user identity or private environment |
| `SRC-027` | memory | Shipped outcome with retained private implementation details | historical | project-private | split | `history/release-and-ui-incidents.md`, `current-state.md` / private retention | Public conclusion is split into canonical docs; private implementation mechanics remain outside tracked docs |
| `SRC-028` | memory | Private follow-up notes | parked | user-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private follow-up content excluded |
| `SRC-029` | memory | Contributor mention rule | active | repo-public | summarize | `communication.md` / public communication contract | Rule appears once with release-credit guidance |
| `SRC-030` | memory | Technical response style for AI reviewers | active | repo-public | summarize | `communication.md` / public review workflow | No model or tool routing details copied |
| `SRC-031` | memory | Cross-cutting invariant audit method | active | repo-public | summarize | `architecture.md`, `verification.md`, `decisions/0002-streaming-and-preaggregation.md` / canonical data-flow rules | Pre-aggregation and sibling-site coverage appear in architecture and gates |
| `SRC-032` | memory | GitHub soft-wrap rule | active | repo-public | summarize | `communication.md` / GitHub writing contract | No private issue draft or URL copied |
| `SRC-033` | memory | Private tooling guidance | active | user-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private tooling material excluded |
| `SRC-034` | memory | Private workflow coordination notes | active | user-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private workflow material excluded |
| `SRC-035` | memory | Commit subject and credit convention | active | repo-public | summarize | `workflow.md` / public commit history and authorization rules | Private model credit instruction excluded |
| `SRC-036` | memory | Project knowledge separation decision | active | repo-public | summarize | `decisions/0001-canonical-knowledge-base.md`, `docs/knowledge/README.md` / canonical KB governance | Decision cross-checked against all new adapters and ledger rules |
| `SRC-037` | memory | Private integration notes | active | user-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private integration material excluded |
| `SRC-038` | plan | Other-project planning | active | other-project | retain-private | `migration-ledger.md#other-project-sources` / separate project boundary | Other-project lifecycle is classified only; source content excluded |
| `SRC-039` | plan | Other-project planning | active | other-project | retain-private | `migration-ledger.md#other-project-sources` / separate project boundary | Other-project lifecycle is classified only; source content excluded |
| `SRC-040` | plan | tokscale upstream axis ledger | historical | repo-public | summarize | `vendor-tokscale.md`, `plans/tokscale-alignment.md` / vendor README and issue #45 | Commit-classification purpose retained without stale line numbers |
| `SRC-041` | plan | tokscale no-gaps alignment snapshot | superseded | repo-public | supersede | `plans/tokscale-alignment.md`, `decisions/0003-selective-upstream-alignment.md` / current rolling plan | Current issue #45 and vendor tree take precedence |
| `SRC-042` | plan | Other-project planning | active | other-project | retain-private | `migration-ledger.md#other-project-sources` / separate project boundary | Other-project lifecycle is classified only; source content excluded |
| `SRC-043` | plan | Other-project planning | active | other-project | retain-private | `migration-ledger.md#other-project-sources` / separate project boundary | Other-project lifecycle is classified only; source content excluded |
| `SRC-044` | plan | Agents report streaming migration | historical | repo-public | summarize | `decisions/0002-streaming-and-preaggregation.md`, `verification.md`, `history/release-and-ui-incidents.md` / current core and FFI source | Old materialized-vs-streaming root cause checked against current architecture |
| `SRC-045` | plan | Cost-provenance selective port | historical | repo-public | summarize | `vendor-tokscale.md`, `plans/tokscale-alignment.md`, `architecture.md` / current vendor schema and README | Implemented schema and cost boundary checked against vendor source |
| `SRC-046` | plan | Initial tokscale M1-M4 sync | historical | repo-public | summarize | `vendor-tokscale.md`, `history/native-rewrite.md` / vendor README | Baseline and local-patch boundary retained without copying commit ledger |
| `SRC-047` | plan | Other-project planning | active | other-project | retain-private | `migration-ledger.md#other-project-sources` / separate project boundary | Other-project lifecycle is classified only; source content excluded |
| `SRC-048` | plan | CPU and lifecycle remediation | historical | repo-public | summarize | `history/release-and-ui-incidents.md`, `verification.md`, `current-state.md` / current Swift and FFI lifecycle | Root cause and cancellation lesson retained without local profiler details |
| `SRC-049` | plan | Private planning notes | active | user-private | retain-private | `migration-ledger.md#private-retention` / private retention | Private planning material excluded |
| `SRC-050` | plan | Other-project planning | active | other-project | retain-private | `migration-ledger.md#other-project-sources` / separate project boundary | Other-project lifecycle is classified only; source content excluded |
| `SRC-051` | plan | Implementation plan with retained private details | historical | project-private | split | `current-state.md`, `history/release-and-ui-incidents.md` / private retention | Public conclusion and private implementation mechanics are split; private material remains outside tracked docs |
| `SRC-052` | plan | Rolling tokscale catch-up | active | repo-public | summarize | `plans/tokscale-alignment.md`, `vendor-tokscale.md` / vendor README and issue #45 | Current upstream status and ordering reconciled with current vendor tree |
| `SRC-053` | plan | Upstream issue and PR reports | historical | repo-public | summarize | `vendor-tokscale.md`, `workflow.md`, `current-state.md` / vendor README and public issue/PR | Counted once; no duplicate rows for its four report subjects |
| `SRC-054` | plan | v4.0.7 to v4.0.10 assessment | superseded | repo-public | supersede | `plans/tokscale-alignment.md`, `decisions/0003-selective-upstream-alignment.md` / current rolling plan | Supersession checked against current public alignment status |
| `SRC-055` | plan | Canonical knowledge-base implementation plan | active | repo-public | summarize | `decisions/0001-canonical-knowledge-base.md`, `docs/knowledge/README.md` / current tracked tree | Required file set and task boundary reconciled with this worktree |
| `SRC-056` | plan | Deferred private decision | parked | project-private | park | `migration-ledger.md#private-retention` / private retention | Only the private lifecycle classification is retained |
| `SRC-057` | local | Private local guidance | active | project-private | split | `AGENTS.md`, `CLAUDE.md`, `vendor/AGENTS.md`, `landing/AGENTS.md`, `docs/knowledge/README.md`, `docs/knowledge/workflow.md` / private retention | Public facts are mapped to canonical adapters and knowledge docs; private local guidance remains outside the repository |
| `SRC-058` | local | Private visual experiment | parked | project-private | summarize | `history/liquid-glass-experiments.md` / canonical history | Durable findings were fully migrated to canonical history; the source may be removed after validation; private and transient artifacts were not added to the repository |

## Private retention

The following source classes deliberately remain outside the repository: credential acquisition or storage details, user identity and writing preferences, private draft material, machine-specific tooling, unpublished runtime follow-ups, and private plan artifacts. Their opaque rows prove that they were reviewed and classified; they do not authorize later publication.

A future client may import private material through a local `.agent-local/` overlay, but the overlay must not override the public architecture, verification, authorization, or release contract.

## Other-project sources

Rows marked `other-project` are retained only to prove that sources outside TokenBar were classified and excluded. They identify no project, product, domain, infrastructure type, toolchain, or work item; lifecycle remains in the status and treatment fields, while all source content stays outside this repository.

## No-gaps verification

| Check | Result |
|---|---|
| Memory inventory | 37 sources: the public index plus project and private sources reconciled outside the repository |
| Plan inventory | 19 sources: public, private, superseded, and other-project plans reconciled outside the repository |
| Local migration sources | 2 sanitized local sources, each represented once |
| Duplicate handling | One row for the upstream-report plan and one row for the project-private follow-up; subjects are split in treatment, not duplicated as sources |
| Destination coverage | Every row has a non-empty canonical destination or an explicit private/other-project retention section |
| Privacy scan | No source filename, absolute local path, credential value/location, private environment, machine-specific tooling, or unpublished material is copied |
| Source hierarchy | Exact vendor facts remain in `vendor/README.md`; runtime gates remain in workflow YAML; this ledger records migration treatment only |
| Validator boundary | The tracked validator checks the exact ledger structure; 58-source reconciliation remains a local external audit |
