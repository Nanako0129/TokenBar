---
status: active
id: kb-communication
kind: canonical
scope: repository
read_when: writing GitHub issues, PRs, review replies, release notes, or bilingual product copy
last_verified: 2026-07-14
sources: ["README.md", "AGENTS.md", "public GitHub contribution history", "canonical release and workflow documents"]
---

# Communication contract

## 文件目的

這份文件整理 TokenBar 對外協作的語言、排版、credit 與 review 回覆規則。它服務 GitHub、release notes、landing copy 與繁中說明；技術事實仍以程式碼、workflow 與 canonical knowledge 為準。

## 目錄

- [Audience and language](#audience-and-language)
- [GitHub prose](#github-prose)
- [Review replies](#review-replies)
- [Contributor credit](#contributor-credit)
- [Release-note accuracy](#release-note-accuracy)
- [zh-TW documents](#zh-tw-documents)
- [Evidence before claims](#evidence-before-claims)

---

## Audience and language

| Surface | Default language | Style |
|---|---|---|
| GitHub issue or PR | English | Concrete root cause, reproduction, expected result, and scope |
| GitHub review reply | English | Direct technical disposition; no social affirmation to AI reviewers |
| Release notes | English | User-visible impact, accurate scope, and contribution credit |
| Repository knowledge | Traditional Chinese with code identifiers preserved | Human-readable synthesis with stable links |
| Landing site | English plus `zh-tw` | Original product copy with aligned structure and metadata |

## GitHub prose

GitHub prose uses soft wrapping. Write each paragraph and list item as one logical line; do not impose commit-message hard-wrap columns. Use tables for mappings, blockquotes for important constraints, fenced blocks for commands, and Mermaid for relationships.

公開 issue 或 PR 的 repo 內連結一律用相對路徑，外部專案用公開 URL。避免帶有個人環境、私有主機、credential、machine-specific tooling 或 unpublished security work 的描述。

## Review replies

AI reviewer 的回覆只寫可驗證的技術事實：哪個 finding confirmed 或 refuted、根因在哪個資料流、改了什麼、哪個測試或 commit 證明。不要寫「Good catch」、「Thanks」或其他對 bot 的社交肯定。真人 maintainer 或 contributor 則維持清楚而正常的禮貌。

| Reply type | Required content |
|---|---|
| Confirmed finding | Root cause, narrow fix, regression evidence, and affected boundary |
| Refuted finding | Current code path, why the alleged path is unreachable or intentional, and any retained test |
| Deferred finding | Why it is outside the selected scope, what evidence is retained, and the future decision surface |
| Contributor follow-up | `@handle`、具體修正、測試結果、是否需要使用者授權 |

## Contributor credit

Release-note credit follows actual contribution, not the number of people who reported a similar symptom. A report that supplied reproducible environment data, logs, or root-cause evidence can be credited; a duplicate with no new diagnostic content is not automatically credited.

Every named contributor or reporter in a public reply or release note must use their GitHub handle, such as `@example`. Do not replace an explicit mention with “you” or an unnamed collective.

## Release-note accuracy

The release workflow may generate separate text and Markdown forms, and the output can drift between local preview and CI. After every release, compare the GitHub Release body, Sparkle appcast description, and legacy update notes against the actual diff.

> **準確性規則：** 不要重複宣稱上一版已完成的修正，也不要把 Agents report、quota provider、pricing、UI 或 release infrastructure 混寫成另一種功能。

When a published sentence is wrong, correct the affected public surface directly when the artifact signature does not cover that text. Do not create a new release merely to repair wording.

## zh-TW documents

中文文件使用台灣繁體、全形標點與自然的中英空格。雙語內容以 `---` 分隔，不額外加「中文」或「English」標題。中文技術文件可使用單個 `##` section title，但不要把每個結論拆成碎片化 bullet；有多個屬性的資訊改成表格。

## Evidence before claims

| Claim | Minimum support |
|---|---|
| Correctness fix | Hermetic old-fail/new-pass test or a source-traced proof with a bounded limitation |
| Performance change | Before/after measurement with workload and lifecycle stated |
| Release behavior | Published artifact or workflow run, not only local preview |
| Upstream fidelity | Exact upstream diff plus an explanation of every local adaptation |
| User report resolution | Reproduction, root cause, shipped path, and issue/PR status |

Writing should state what is known, what is inferred, and what remains private or deferred. Precision is more useful than marketing language.
