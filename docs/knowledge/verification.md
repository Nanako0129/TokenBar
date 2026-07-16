---
status: active
id: kb-verification
kind: canonical
scope: repository
read_when: changing runtime code, running a local build or UX acceptance, parser output, cache behavior, FFI contracts, or this knowledge tree
last_verified: 2026-07-16
sources: [".github/workflows/ci.yml", "Makefile", "Package.swift", "scripts/bundle.sh", "crates/tb_core_ffi/src/agent_history.rs", "docs/knowledge/plans/codex-historical-pace-v2.md", "AGENTS.md", "memory-derived hermetic verification practice", "memory-derived local build indexing incident"]
---

# Verification contract

## 文件目的

這份文件把 TokenBar 的驗證分成 deterministic fixture、跨語言契約、runtime smoke、cache invalidation 與 repository hygiene。目標不是堆命令，而是讓每個修正都證明「舊行為會失敗、新行為正確、常見資料不回歸」。

## 目錄

- [Evidence model](#evidence-model)
- [Hermetic fixtures](#hermetic-fixtures)
- [Runtime and FFI gates](#runtime-and-ffi-gates)
- [Local build and UX acceptance](#local-build-and-ux-acceptance)
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
| Quota history | Reset jitter、floating zero、partial／future-reset weeks、account isolation、corrupt recovery 與 current-actual shift 都以 temporary v2 store 驗證；live provider refresh 只作 smoke |
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

## Local build and UX acceptance

不需要 `.app` bundle 語意的人工 UI 檢查，優先從 repository root 執行 `swift run TokenBar --open-popover`。只有 icon、`Info.plist`、`LSUIElement`、Sparkle、autostart 或安裝路徑等 bundle-only 行為，才以 `make bundle` 產生的 `dist/TokenBar.app` 驗收。

> **本機 bundle 邊界：** `dist/TokenBar.app` 是暫時的驗收產物，不是第二份安裝。日常使用與正式更新的 source of truth 仍是 `/Applications/TokenBar.app`。

[`scripts/bundle.sh`](../../scripts/bundle.sh) 會在組裝 app 前建立 `dist/.metadata_never_index`，避免 Spotlight 主動索引本機 bundle。但這個 marker 不會回溯刪除既有 Spotlight metadata；實際啟動 `dist/TokenBar.app` 也可能讓 LaunchServices 註冊它。因此本機 UX 驗收完成、且不再需要該 bundle 作為 release artifact 時，應撤銷這個特定 app 的註冊並刪除生成物，不要以重設整個 Launchpad database 作為第一步。

| UX surface | Preferred path | Completion evidence |
|---|---|---|
| Popover、lens、keyboard、scroll、appearance | `swift run TokenBar --open-popover` | 實際操作與必要截圖；結束測試 process |
| Icon、bundle identity、Sparkle、autostart | `make bundle` 後啟動 `dist/TokenBar.app` | 記錄 bundle-only 行為；完成後 unregister 並移除本機 bundle |
| Homebrew、Sparkle stable update、正式安裝路徑 | `/Applications/TokenBar.app` | 不以 `dist/TokenBar.app` 代替 installed-app 驗收 |

從 repository root 清理已完成驗收的本機 bundle：

```bash
ROOT="$(git rev-parse --show-toplevel)"
LOCAL_APP="$ROOT/dist/TokenBar.app"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

test -e "$ROOT/dist/.metadata_never_index"
"$LSREGISTER" -u "$LOCAL_APP" 2>/dev/null || true
rm -rf -- "$LOCAL_APP"
```

清理後，Spotlight 與 LaunchServices 查詢都不應再列出 repository 的 `dist/TokenBar.app`；正常情況只保留 `/Applications/TokenBar.app`：

```bash
mdfind "kMDItemContentType == 'com.apple.application-bundle' && kMDItemFSName == 'TokenBar.app'"
"$LSREGISTER" -dump | grep -F 'TokenBar.app'
```

若仍有 stale 結果，先等待 metadata service 收斂並重查；不要直接清空整台機器的 Spotlight index 或重設 Launchpad，因為那會波及其他 app 與使用者排列。

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
| Historical pace | Rust 的 optional nested result 同時擁有 expected、ETA、will-last 與 risk；Swift 只能導出 stage／文字，result 缺席時才使用 Linear |
| Lifecycle | Closing a popover or settings window cancels its tasks and stops background rendering |

## Cross-port fixture cross-check

Windows port（[Nanako0129/TokenBar-Windows](https://github.com/Nanako0129/TokenBar-Windows)）的 C# `TokenBar.Core` 是 `Sources/TokenBarCore` 的逐檔移植。單元測試的期望值由移植者撰寫，因此對「一致地誤讀 Swift 語意」的移植錯誤沒有偵測力；對拍（cross-check）以同一份 fixture JSON 餵 Swift 與 C# 兩邊、逐欄位 diff 輸出，才是移植忠實度的判準。

| 項目 | 內容 |
|---|---|
| Swift harness | [`Sources/CrossCheckHarness/main.swift`](../../Sources/CrossCheckHarness/main.swift)，`TZ=Asia/Taipei swift run crosscheck-harness <fixtures> <out>`；經 symlink 編入 app target 的 `Format.swift`，測的是 shipping 程式碼 |
| 契約與 fixture | Windows repo 的 `crosscheck/`（README＝schema 契約；fixture 以 FFI wire 編碼，兩邊都用 production decoder，無自製映射） |
| 比對 | Windows repo 的 `crosscheck/diff.py`：字串逐 byte、數字 epsilon 1e-9、缺鍵視同 null |
| 執行時機 | `Sources/TokenBarCore` 邏輯或 `Format` 語意變更後；Windows repo 每次 re-sync 或 delta 移植後 |

> 首輪實績（2026-07-16）：首跑 115 案例抓到 4 條 printf 捨入 seam 的真實漂移——C# 側以 `Math.Round` 預捨入模擬 `%.nf` 會把非 midpoint 的近半值重新量化；printf 對二進位真值做正確捨入。教訓：**模擬 printf 的中介捨入層一律可疑**。修正與後續 comparator 強化（整數精確比對、bool 嚴格比對、Int64 邊界案例——fixture 現為 116 案）都記錄在 Windows repo。

> Historical pace v2 checkpoint（2026-07-16）：116-case legacy baseline 已重跑，非 historical cases 全數一致。27 個 field differences 只分布在 9 個使用舊 top-level historical scalars 的 cases：`historical-expected-clamped`、`historical-runout-exact-half`、`historical-runout-high-keeps-eta`、`historical-runout-low-forces-lasts`、`historical-with-expected`、`runout-risk-certain`、`runout-risk-clamped-above-one`、`runout-risk-half-percent-rounds-up`、`runout-risk-thirty`。這些是 nested contract 取代 scalar contract 的 intended mismatch；Windows 新增 nested fixture／DTO 並完成 semantic port 前，不得宣稱 historical parity。

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
