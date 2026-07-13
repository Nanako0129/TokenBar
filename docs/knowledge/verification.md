---
status: active
id: kb-verification
kind: canonical
scope: repository
read_when: changing runtime code, parser output, cache behavior, FFI contracts, or this knowledge tree
last_verified: 2026-07-14
sources: [".github/workflows/ci.yml", "Makefile", "Package.swift", "AGENTS.md", "memory-derived hermetic verification practice"]
---

# Verification contract

## 文件目的

這份文件把 TokenBar 的驗證分成 deterministic fixture、跨語言契約、runtime smoke、cache invalidation 與 repository hygiene。目標不是堆命令，而是讓每個修正都證明「舊行為會失敗、新行為正確、常見資料不回歸」。

## 目錄

- [Evidence model](#evidence-model)
- [Hermetic fixtures](#hermetic-fixtures)
- [Runtime and FFI gates](#runtime-and-ffi-gates)
- [Cache and sibling checks](#cache-and-sibling-checks)
- [Cross-language invariants](#cross-language-invariants)
- [Documentation checks](#documentation-checks)
- [Failure interpretation](#failure-interpretation)

---

## Evidence model

| Evidence layer | Answers | Cannot prove alone |
|---|---|---|
| Hermetic fixture | 觸發條件下 old/new 是否分歧、修正是否收斂 | 真實 GUI lifecycle 或 provider 網路狀態 |
| Unit or core test | 純函式、parser、fold、schema contract 是否穩定 | Swift/AppKit integration |
| FFI smoke | Rust -> C ABI -> Swift decoder 是否能端到端運作 | 所有特殊資料條件的正確數字 |
| Live app check | 真實 session、視窗 lifecycle、外觀與更新流程是否不崩 | 沒有觸發資料時的 correctness fix |
| CI | 可重複的 build/selftest/smoke gate | 本機 private data 與人工 UX 判斷 |

## Hermetic fixtures

當修正效果取決於本機可能沒有的 session、duplicate key、cursor、WAL、sibling metadata 或 provider cost 時，優先建立合成 fixture。測試應同時保留 old-fail/new-pass 證據，並另加無觸發條件的保值 case。

> **Hermetic 原則：** Live app 在沒有觸發條件時顯示「沒有變化」，只證明常見資料不崩，不能證明修正有效。權威證據是可重跑、與本機資料無關的 fixture。

| Fixture property | Required assertion |
|---|---|
| Duplicate or replay | 舊路徑的 total 與對照路徑分歧；新路徑與對照收斂 |
| Sibling-only write | 預設 fingerprint 不失效；完整 fingerprint、mtime probe、prune 都失效 |
| Provider cost | 缺失成本可估算；明確 provider-reported 成本不可被 stale pricing 覆蓋 |
| Hidden client | non-empty partial selection 在 Rust fold 前排除未選 client；`nil`／empty clients 依 C ABI contract 代表 all clients；all-hidden 由 Swift lens strict membership 阻擋 |
| Overflow input | old arithmetic fails or wraps in the targeted site；new saturating path remains bounded |
| Cache schema | 舊版本 cache 不被當成新 layout 靜默接受；新 layout 可重建並 reload |

## Runtime and FFI gates

The current CI runtime source is [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml). CI builds the Rust static library, builds Swift, runs the core selftest, and runs the FFI smoke binary. Those are CI build and smoke checks, not the complete local code-change gate. The local build order comes from [`Makefile`](../../Makefile) and the linker contract comes from [`Package.swift`](../../Package.swift).

```bash
cargo build --release
swift build
swift run TokenBar --selftest
swift run TokenBar --smoke
```

### Local full code-change gates

For Rust or cross-language code changes, the local full gate adds formatting, the Rust test suite, the all-targets Clippy pass, and the repository build:

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --workspace --all-targets
make build
swift run TokenBar --selftest
swift run TokenBar --smoke
```

`cargo test` and `cargo clippy --workspace --all-targets` are local full code-change gates; this document does not claim that the current CI workflow runs them. The `--all-targets` flag is required because `vendor/tokscale-core/src/lib.rs` declares `#![deny(clippy::all)]`, so a test-only lint can fail the gate even when the library target itself is clean.

| Gate | Expected evidence |
|---|---|
| Rust | Release static library builds from the current source |
| Swift | SwiftPM links against the freshly built library from repository root |
| Selftest | UI-free TokenBarCore assertions pass |
| Smoke | Every C ABI entry point decodes or reports an intentional error envelope |
| Relink safety | If Rust changed without Swift source changes, the stale executable is removed before linking |
| Rust format | For Rust changes, run `cargo fmt --all -- --check` on the touched scope; vendor formatting policy may be intentionally separate |
| Local Rust tests | `cargo test` passes across workspace crates and test targets |
| Local Clippy | `cargo clippy --workspace --all-targets` passes, including test-only targets |

## Cache and sibling checks

A source reader that consumes secondary files must be verified as one unit. The regression matrix is deliberately broader than the parser function itself.

| Seam | Check |
|---|---|
| Fingerprint | Primary-only change and sibling-only change produce different fingerprints |
| Active lane | The source is reachable by the streaming and materialized consumers that claim support |
| Latest mtime | Live-tail change token observes every relevant sibling and WAL |
| Pruning | `modified_after` keeps a session when a relevant sibling is fresher than the primary |
| Cache rebuild | Same-fingerprint stale serialized data is rejected when parser output or attribution changes |
| Report parity | Materialized and streaming reports agree on the fixture's selected fields |

## Cross-language invariants

| Contract | Verification |
|---|---|
| Heap JSON ownership | Every successful FFI pointer is decoded and released through `tb_free`; errors do not leak a second ownership path |
| Envelope shape | `ok` and `data`/`err` fields match `ctb.h` and Swift decoders |
| Client filter | Non-empty selected IDs reach Rust before mixed buckets are folded; `nil`／empty client lists mean all clients per `ctb.h`; the Swift lens strict-membership check blocks all-hidden views |
| Arithmetic | Rust report totals, FFI mappers, Swift models, and live-rate consumers use bounded arithmetic where required |
| Stale-data policy | A failed refresh retains the last good value instead of blanking a working card |
| Lifecycle | Closing a popover or settings window cancels its tasks and stops background rendering |

## Documentation checks

The knowledge tree is validated by `scripts/check_knowledge.py`, the `make check-docs` target, and the CI knowledge-validation step. The final documentation gate is:

```bash
python3 scripts/check_knowledge.py --self-test
python3 -m py_compile scripts/check_knowledge.py
python3 scripts/check_knowledge.py
make check-docs
git diff --check origin/main...HEAD
```

These checks cover frontmatter, relative links, canonical reachability, migration-ledger counts and enums, privacy scans, and repository whitespace. Do not claim runtime PASS for a docs-only change.

## Failure interpretation

A failed smoke run caused by missing local credentials, an empty private session tree, or a provider network response is not evidence that the parser or docs are wrong. Record the environmental limitation separately, then rely on hermetic tests and the relevant source-level gate. Conversely, a green live smoke run without a fixture does not close a data-dependent correctness issue.
