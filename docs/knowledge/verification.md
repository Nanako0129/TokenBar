---
status: active
id: kb-verification
kind: canonical
scope: repository
read_when: changing runtime code, running a local build or UX acceptance, parser output, cache behavior, FFI contracts, or this knowledge tree
last_verified: 2026-07-31
sources: [".github/workflows/ci.yml", "Makefile", "Package.swift", "scripts/bundle.sh", "Sources/TokenBar/ClientTray.swift", "Sources/TokenBar/StatusItemController.swift", "Sources/TokenBar/Views/AgentIconView.swift", "Sources/TokenBar/Views/SettingsPanel.swift", "Sources/TokenBar/SelfTest.swift", "crates/tb_core_ffi/src/agent_account_scope.rs", "crates/tb_core_ffi/src/agent_quota_history.rs", "crates/tb_core_ffi/src/agent_storage_windows.rs", "docs/knowledge/plans/provider-quota-pace.md", "docs/knowledge/plans/codex-historical-pace-v2.md", "public TokenBar-Windows PR #7", "public TokenBar PR #114", "public TokenBar-Windows PR #12", "AGENTS.md", "memory-derived hermetic verification practice", "memory-derived local build indexing incident"]
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

PT0 的 hermetic authorities are Rust last-good and binding decisions, refresh status-before-body ordering, Grok monthly additive behavior, Copilot loader precedence, and Antigravity precedence; the FFI A/B publication-generation ordering and Swift diagnostic-candidate plus isolated UserDefaults scalar and local publication-state tests are required at the cross-language seam. The Rust fixture must pause A after its gate helper returns, let B obtain generation 2 and record its return first, then release A; it checks both return order and payload generations/content. A separate exhaustion fixture starts at `u64::MAX - 1` and proves the next call fails closed without invoking the publication body or repeating a generation. Swift fixtures distinguish bridge failure from malformed/missing successful data, prove Settings identities change across generations, legacy resolved values, selections, and exclusions, drive the tray apply seam with generation 2 terminal followed by generation 1 success to prove the late result resolves to generation 2 and cannot revive its scalar, and prove a generation 3 Dashboard publication replaces both an older tray payload and scalar before the tray's own poll returns while changing the gauge signature that gates immediate rendering. Live smoke requires authorization for that run and cannot replace these fixtures.

| Fixture property | Required assertion |
|---|---|
| Duplicate or replay | 舊路徑的 total 與對照路徑分歧；新路徑與對照收斂 |
| Sibling-only write | 預設 fingerprint 不失效；完整 fingerprint、mtime probe、prune 都失效 |
| Provider cost | 缺失成本可估算；明確 provider-reported 成本不可被 stale pricing 覆蓋 |
| Hidden client | non-empty partial selection 在 Rust fold 前排除未選 client；`nil`／empty clients 依 C ABI contract 代表 all clients；all-hidden 由 Swift lens strict membership 阻擋；對外發布面另以 hermetic payload-builder fixture 證明隱藏 client 的 token／cost 與 top-client 標籤都不出現在送出的 payload，且未註冊 id 在 producer 端就被正向 allowlist 濾掉——既不進數字也不進標籤，因此中性常數自 `payload()` 已不可達，改以「整張圖只有未註冊 id 時發布 nothing」證明其為死路 |
| Quota account scope | 以temporary application-data root驗證exact 32-byte key、每次reload、cross-process winner、key-loss orphan recovery、atomic failure與raw-value scan；macOS／Unix驗證directory `0700`／file `0600`、symlink／non-regular／inode swap fail-closed。Windows另驗證system-preferred CNG、current-user owner、protected current-user／LocalSystem exact allow DACL、foreign／deny／inherited ACE拒絕、final symlink／junction／device／wrong type拒絕、volume plus 128-bit file identity replacement、no-delete-share lock、concurrent first-key winner、replace pre／post-commit failure、collision-safe quarantine與identity-bound rollback、sticky secure-root fallback、insecure legacy v2留原位不匯入，以及錯誤不洩漏path／SID／raw value；不得呼叫真實Keychain或provider credential |
| Quota history | Reset jitter、floating zero、duration lifecycle、partial／future-reset cycles、active-series capacity、account isolation、corrupt recovery與current-actual shift都以temporary v3 store驗證；Codex schema-2只驗current-account-only、byte-exact read-only migration，live provider refresh只作smoke |
| Overflow input | old arithmetic fails or wraps in the targeted site；new saturating path remains bounded |
| Individual client tray | UI-free SelfTest鎖定兩個defaults的parse前byte cap、entry／ID cap、deterministic serialization與超限no-writeback；client-scoped Auto／explicit／missing、error-only quota provider仍可配置、`antigravity-cli`只在quota lookup映射到`antigravity`且identity保持獨立、main完整route與per-client lens記憶互不污染、Settings row／picker狀態、official icon 1x＋2x reps、newer accepted publication與visible／AX／tooltip privacy都由synthetic graph／quota payload驗證，不建立真實system status items |
| Cache schema | 舊版本 cache 不被當成新 layout 靜默接受；新 layout 可重建並 reload |
| Provider transport fallback | last-good binding、refresh status-before-body、terminal/absent/4xx/schema/required-meter clearing、Grok additive monthly、Copilot loader、以及 diagnostic allowlist 都以 hermetic responses 驗證 |
| FFI publication | Provider run、JSON serialize、envelope、raw C-pointer publication 的 single-flight、gate-assigned checked `publicationGeneration`、exhaustion fail-closed、可反轉的 C return order，以及單次 run 內 provider 並行分別驗證 |
| Source-aware filter parity | `tb_filter_parity_probe` uses one context, fresh graph, `token0…token5`, exact integer comparison, diagnostic-only cost deltas because pricing refreshes independently from source generations, and short-circuits invalid later scans. Its synchronous callback seam tests stable match/mismatch, price-only refresh, tokenUnavailable versus graph failure, every source-change boundary, Agents-only late changes, call ordering, and a size-changing append token. The dedicated vendor fixture covers canonical, cc-mirror exact gating, synthetic gateway, duplicate canonical paths, inherited scanner-root isolation, unattributed `Main`, and cold/warm cache parity. |

> Historical runtime retirement checkpoint（2026-07-31）：`agent_history.rs` 的 v2 writer／evaluator 與專用 evaluator tests 已刪除；現行 gate 是 `agent_quota_history.rs` 的 schema-3 v3 evaluator 加上 current-account-only schema-2 importer regressions。舊 Historical pace v2 cross-port checkpoint 仍保留於下方，僅作歷史 parity 證據，不是 current gate。

## Runtime and FFI gates

The current CI runtime source is [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml). CI builds the Rust static library, builds Swift, runs the core selftest, and runs the FFI smoke binary. Those are CI build and smoke checks, not the complete local code-change gate. The local build order comes from [`Makefile`](../../Makefile) and the linker contract comes from [`Package.swift`](../../Package.swift).

```bash
cargo build --release
swift build
make selftest          # = swift run TokenBar --selftest -AppleLanguages "(en)"
swift run TokenBar --smoke
```

### Local full code-change gates

For Rust or cross-language code changes, the local full gate adds formatting, the Rust test suite, the all-targets Clippy pass, and the repository build:

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --workspace --all-targets
make build
make selftest          # = swift run TokenBar --selftest -AppleLanguages "(en)"
swift run TokenBar --smoke
```

`cargo test` and `cargo clippy --workspace --all-targets` are local full code-change gates; this document does not claim that the current CI workflow runs them. The `--all-targets` flag is required because `vendor/tokscale-core/src/lib.rs` declares `#![deny(clippy::all)]`, so a test-only lint can fail the gate even when the library target itself is clean.

Live account-scope smoke必須在hermetic security suite通過後才執行，且每次重新執行都需要當次明確授權。若出現任何Keychain或credential授權視窗，立即停止process並把smoke判為失敗；不得要求輸入登入密碼、讀取secret或用真實credential診斷。

| Gate | Expected evidence |
|---|---|
| Rust | Release static library builds from the current source |
| Swift | SwiftPM links against the freshly built library from repository root |
| Selftest | UI-free TokenBarCore assertions pass。部分斷言逐字比對英文 UI 文案，因此語系必須鎖定 `en`（用 `make selftest`，或自行帶 `-AppleLanguages "(en)"`）；在中文系統上直接跑 `swift run TokenBar --selftest` 會因 `Format` 輸出中文而假性失敗，入口會先印出提示 |
| Smoke | Every C ABI entry point decodes or reports an intentional error envelope；account-scope path不得存取Keychain或顯示credential authorization UI |
| Account-scope storage | Hermetic security tests先證明permission、path、locking、atomicity與recovery；live smoke只驗證shipping data flow不彈授權UI，不取代fixture correctness |
| Windows secure storage | M19-B0證明Native candidate與核准Windows security/storage semantic source等價；M19-B1又在hosted Windows x64 runtime執行CNG、owner／exact protected DACL、final-component reparse、file identity、exclusive no-delete-share lock、replace、quarantine、legacy upgrade與error-privacy tests。合併後的exact Windows source另在real ARM64 Windows通過351項Rust tests、12-case provider-v3 CrossCheck、ARM64 PE checks與synthetic WinUI startup；macOS tests與GitHub ARM64 cross-package都不取代這些runtime assertions |
| Relink safety | If Rust changed without Swift source changes, the stale executable is removed before linking |
| Rust format | For Rust changes, run `cargo fmt --all -- --check` on the touched scope; vendor formatting policy may be intentionally separate |
| Local Rust tests | `cargo test` passes across workspace crates and test targets |
| Local Clippy | `cargo clippy --workspace --all-targets` passes, including test-only targets |

## Local build and UX acceptance

不需要 `.app` bundle 語意的人工 UI 檢查，優先從 repository root 執行 `swift run TokenBar --open-popover`。只有 icon、`Info.plist`、`LSUIElement`、Sparkle、autostart 或安裝路徑等 bundle-only 行為，才以 `make bundle` 產生的 `dist/TokenBar.app` 驗收。

Provider quota pace 以 `swift run TokenBar --demo --open-popover` 提供 deterministic 人工驗收面；snapshot badge 明示 `FIXTURE`，且 `DemoUsageDataSource` 不呼叫 live FFI、不讀寫 quota cache。Historical／Linear／Off 都要實際呈現；驗收時必須區分低 remaining 觸發的 quota 長條黃／紅健康色，與只有 `available` historical deficit 才可使用的 pace marker／footer 橘色。

Individual client items的deterministic Settings檢查以Argument Domain注入初始偏好；這只驗visual state與initial routing，不在同一process宣稱toggle persistence：

```bash
swift run TokenBar --demo --settings \
  -tokenbar.tray.clients.enabled claude,codex \
  -tokenbar.tray.clients.quotaSelections '{"claude":"auto","codex":"weekly.v1"}'

swift run TokenBar --demo --settings \
  -tokenbar.tray.clients.enabled claude,codex \
  -tokenbar.tray.clients.quotaSelections '{"claude":"auto","codex":"missing.v1"}'

swift run TokenBar --demo --settings \
  -tokenbar.tray.clients.enabled claude,codex \
  -tokenbar.tray.clients.quotaSelections '{"claude":"auto","codex":"weekly.v1"}' \
  -tokenbar.tabs.hidden codex
```

> **本機 bundle 邊界：** `dist/TokenBar.app` 是暫時的驗收產物，不是第二份安裝。日常使用與正式更新的 source of truth 仍是 `/Applications/TokenBar.app`。

[`scripts/bundle.sh`](../../scripts/bundle.sh) 會在組裝 app 前建立 `dist/.metadata_never_index`，避免 Spotlight 主動索引本機 bundle。但這個 marker 不會回溯刪除既有 Spotlight metadata；實際啟動 `dist/TokenBar.app` 也可能讓 LaunchServices 註冊它。因此本機 UX 驗收完成、且不再需要該 bundle 作為 release artifact 時，應撤銷這個特定 app 的註冊並刪除生成物，不要以重設整個 Launchpad database 作為第一步。

| UX surface | Preferred path | Completion evidence |
|---|---|---|
| Popover、lens、keyboard、scroll、appearance | `swift run TokenBar --open-popover` | 實際操作與必要截圖；結束測試 process |
| Individual client status items | `--demo --settings`配合兩個M2 Argument Domain keys驗initial visual state；本機資料、placement／right-click／跨螢幕則用同一`dist/TokenBar.app` | 預設只有主item；switch點擊後立即以`.mini`原生狀態更新，client shell可稍後於同一defaults reconciliation建立但不得阻塞control setter；Settings body重建不得同步呼叫`SMAppService.status`（本機量測單次約0.5～0.9秒），關閉／重開與連續toggle都須維持可互動；enable／disable／hide／restore保留selection與`tokenbar-client-<id>` placement；`antigravity-cli`在本機error-only provider狀態仍可配置；dashboard選定單一年份時，Settings仍以all-time graph保留所有live client item rows並可停用；client A→B沿用同一popover並各自恢復本次app session停留的lens；主item恢復自己的client-plus-lens route，不被individual item覆寫；主item right-click仍只改global source；1x／2x雙向移動圖示清晰；VoiceOver label不含raw card／error，explicit error fallback固定讀作last-known而非current quota；0與8 items的idle profile沒有per-client loop |
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
| Envelope shape | `ok` and `data`/`err` fields match `ctb.h` and Swift decoders; agent usage classifies `ok:false` as fixed bridge failure and malformed or `ok:true` missing data as fixed decode failure without public associated text |
| Publication order | Rust `publicationGeneration` is additive, gate-owned, and checked against exhaustion; one shared Swift `@MainActor` coordinator rejects lower generated payloads across DashboardModel, Settings, tray polling, and snapshot restore before payload or scalar apply, while missing generations pass through without changing state |
| Windows compatibility | M19-B1's production C# decoder ignored additive `publicationGeneration` while preserving existing fields; no DTO, C signature, or ownership change was required |
| Windows storage ownership | Native `agent_storage_windows` is the cfg-gated source of truth. Production account scope and v3 history must use only its CNG／DACL／secure-open／identity／lock／replace／quarantine primitives, preserve the trusted per-user anchor and same-SID threat boundary, and never be patched independently in the Windows consumer |
| Client filter | Non-empty selected IDs reach Rust before mixed buckets are folded; `nil`／empty client lists mean all clients per `ctb.h`; the Swift lens strict-membership check blocks all-hidden views |
| Arithmetic | Rust report totals, FFI mappers, Swift models, and live-rate consumers use bounded arithmetic where required |
| Stale-data policy | A failed refresh retains the last good value instead of blanking a working card；fallback must preserve display-ready fields only for a structural same-binding request transient, mark account scope untrusted, and skip enrichment/history/re-cache. Claude、Codex、Grok與Antigravity refresh persistence must re-read and match the loaded target before write-back and preserve unrelated siblings；account switch/logout is rejected before stale usage continues, while only unchanged-target persistence failure may follow the provider's existing uncacheable／terminal policy. Codex save failure must leave auth and metadata bytes unchanged；credentials persist before lineage transfer, transfer failure attempts rollback only after the exact just-written root still matches, and the post-binding reuses the already-verified corroborating scope plus the scope returned by transfer instead of adding a second fallible metadata write. Hermetic fixtures cover A→B、logout/removal、save/metadata failure、single pre-request resolution and sibling preservation；the compare-to-atomic-rename external-writer window and cross-resource crash interval are not claimed as filesystem CAS／atomic transaction |
| Diagnostic wire | Rust emits only `{category,status?,osCode?}` from the fixed category allowlist；`rateLimited`只允許status 429且不帶osCode，`serverError`只允許500...599且不帶osCode，非HTTP categories不得帶status，unknown category不得帶associated numerics；Swift lossy decoding逐欄丟棄malformed optional integers，非object或缺失／非字串category不建立candidate、unknown values正規化，且public logging維持bounded |
| Swift scalar authority | Successful finite outer payloads replace memory and persistent scalar; Dashboard polling reconciles accepted publications immediately and TrayAnimator reads the coordinator's latest generated payload before its own poll completes; selection／exclusion changes re-reconcile before render; Settings task identity uses `publicationGeneration` or a legacy timestamp-plus-resolved-scalar fingerprint; terminal／Absent／unresolved／hidden-all clear the scalar; only outer FFI failure or missing payload retains it. Isolated UserDefaults tests cover restart non-revival, healthy replacement, explicit fallback windows, Auto exclusion, reconciliation identity collisions, a newer terminal result followed by a late older tray success, and a newer Dashboard success replacing an older tray payload plus scalar while invalidating the gauge-render signature |
| Historical pace | Rust 的 typed `paceStatus` 擁有 lifecycle／duration，optional nested result 同時擁有 expected、ETA、will-last 與 risk；Swift 只能導出 mode policy、stage 與文字。`available + completeCycles == 0` 是合法的 validated current-window projection：Historical mode 必須使用 backend expected／ETA／will-last，partial risk 缺失時維持 nil、不顯示 risk copy，也不得退回 Linear。只有 `learningHistory` 可明示使用 exact-duration Linear estimate，`learningDuration`／`unavailable`／legacy 不得 silent fallback |
| Lifecycle | Closing a popover or settings window cancels its tasks and stops background rendering |

## Cross-port fixture cross-check

Windows port（[Nanako0129/TokenBar-Windows](https://github.com/Nanako0129/TokenBar-Windows)）的 C# `TokenBar.Core` 是 `Sources/TokenBarCore` 的逐檔移植。兩個 consumer 現在都 pin `84e0d66413d4e0d87b734f66f7a848b3bc323258`；same-pin 只證明 shared source 相同，不取代下述 cross-check 這道跨語言 gate；app-owned FFI、Swift 與 C# surfaces 仍可獨立漂移。單元測試的期望值由移植者撰寫，因此對「一致地誤讀 Swift 語意」的移植錯誤沒有偵測力；對拍（cross-check）以同一份 fixture JSON 餵 Swift 與 C# 兩邊、逐欄位 diff 輸出，才是移植忠實度的判準。

| 項目 | 內容 |
|---|---|
| Swift harness | [`Sources/CrossCheckHarness/main.swift`](../../Sources/CrossCheckHarness/main.swift)，`TZ=Asia/Taipei swift run crosscheck-harness <fixtures> <out> [usage-pace|format|provider-quota-pace-v3] -AppleLanguages "(en)"`；selector 省略時維持 legacy complete run，所有路徑使用 shipping 程式碼 |
| 語系鎖定 | `Format` 的日期／相對時間與 `UsagePace.durationText` 自 i18n 後具語系相依（查表工具在 `Sources/TokenBarCore/Localization.swift`，harness 透過連結 TokenBarCore 取得）。`make build` 會把 `Sources/TokenBar/Resources/Localizations/*.lproj` 複製到 `.build/debug/`；裸 `swift run TokenBar` 會在入口從 SwiftPM resource bundle 暫存同樣的目錄，harness 本身沒有該 target resource，沒有 `.lproj` 時解析為 `en`。對拍因此**必須**帶 `-AppleLanguages "(en)"`。harness 對此 **fail closed**：解析時只認 `-AppleLanguages <value>`（其餘 dash 開頭的 token 一律拒絕，避免吃掉 fixture 路徑後對錯誤目錄執行），並在 TZ guard 之後檢查 `preferredLocalizations` 必須為 `en`，否則 exit——寧可拒跑，也不要靜默產生對拍永遠對不上的在地化輸出。 |
| 契約與 fixture | Windows repo 的 legacy `crosscheck/` 保留既有 116-case reference；provider v3 handoff 由 Mac-owned [`provider-quota-pace-v3.json`](../../Fixtures/CrossCheck/provider-quota-pace-v3.json) 提供，Rust production serializer 鎖定 payload，Swift／Windows 都必須用 production decoder，無自製 wire mapping |
| 比對 | Windows repo 的 `crosscheck/diff.py`：字串逐 byte、數字 epsilon 1e-9、缺鍵視同 null |
| 執行時機 | `Sources/TokenBarCore` 邏輯或 `Format` 語意變更後；Windows app-owned delta 或 reviewed engine pin advance 後 |

> 首輪實績（2026-07-16）：首跑 115 案例抓到 4 條 printf 捨入 seam 的真實漂移——C# 側以 `Math.Round` 預捨入模擬 `%.nf` 會把非 midpoint 的近半值重新量化；printf 對二進位真值做正確捨入。教訓：**模擬 printf 的中介捨入層一律可疑**。修正與後續 comparator 強化（整數精確比對、bool 嚴格比對、Int64 邊界案例——fixture 現為 116 案）都記錄在 Windows repo。

> Historical pace v2 checkpoint（2026-07-16）：116-case legacy baseline 已重跑，非 historical cases 全數一致。27 個 field differences 只分布在 9 個使用舊 top-level historical scalars 的 cases：`historical-expected-clamped`、`historical-runout-exact-half`、`historical-runout-high-keeps-eta`、`historical-runout-low-forces-lasts`、`historical-with-expected`、`runout-risk-certain`、`runout-risk-clamped-above-one`、`runout-risk-half-percent-rounds-up`、`runout-risk-thirty`。這些是 nested contract 取代 scalar contract 的 intended mismatch；Windows 新增 nested fixture／DTO 並完成 semantic port 前，不得宣稱 historical parity。
>
> Provider-wide v3 checkpoint（2026-07-27）：no-selector Mac harness 以 production decoder 完整產生 42 pace＋74 format cases，不再因單一 malformed legacy row 中止。原本 28／42 pace cases 的 intended mismatch 來自 strict v3 decoder、typed lifecycle 與 no-silent-fallback contract。M19-B1 的 Windows port在 [Windows PR #7](https://github.com/Nanako0129/TokenBar-Windows/pull/7) 完成 production DTO／state machine／selection／presentation；完整 Swift／C# cross-check 119 cases零material difference。Mac-owned fixture 的7張 lifecycle windows與12個 projection／selection／legacy／malformed cases仍由Rust production serializer鎖定，real ARM64 CrossCheck也產生exact 12 cases。Provider-v3 Windows status因此為 **port/parity complete**。

> Shared-engine extraction checkpoint（2026-07-28）：Native [PR #114](https://github.com/Nanako0129/TokenBar/pull/114) 與 Windows [PR #12](https://github.com/Nanako0129/TokenBar-Windows/pull/12) 都 pin `b31e39425859393504a2d56cb5af7c93e6461c7d`。Windows current-head gates produced 119 cross-check cases with zero material difference；hosted x64／ARM64 jobs and a separate native ARM64 packaged-FFI run passed. Future same-pin assertions prove shared source equality but do not replace this cross-language gate.

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
