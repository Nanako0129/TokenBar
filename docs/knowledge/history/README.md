---
status: historical
id: kb-history-index
kind: index
scope: repository
read_when: investigating why a current rule exists or reconstructing an earlier decision
last_verified: 2026-07-14
sources: ["public project history", "sanitized project memories", "sanitized local experiment source"]
---

# History index

## 文件目的

History 文件保留已驗證、仍能影響維護決策的結論，不是完整 session transcript。每篇都把舊背景壓縮成可接手的根因、取捨與目前狀態；不把私有路徑、credential 細節或個人工具設定帶進 tracked tree。

| History | 用途 | 狀態 |
|---|---|---|
| [`native-rewrite.md`](native-rewrite.md) | Tauri 到 native SwiftUI、Rust FFI 與出貨遷移 | historical baseline |
| [`liquid-glass-experiments.md`](liquid-glass-experiments.md) | Liquid Glass 調查、否決的 spike 路線與 parked 結論 | parked |
| [`release-and-ui-incidents.md`](release-and-ui-incidents.md) | 發版、更新、UI lifecycle 與效能事故的根因 | historical runbook context |

> 新程式碼的現況以 current source、workflow 與 vendor README 為準；history 只提供 rationale 與已知陷阱。
