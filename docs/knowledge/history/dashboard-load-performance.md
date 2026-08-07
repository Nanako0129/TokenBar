---
status: historical
id: kb-history-load-performance
kind: canonical
scope: repository
read_when: investigating dashboard load latency, planning a scan or cache optimization, or designing a before/after measurement for this project
last_verified: 2026-08-08
sources: ["sanitized local benchmark output", "macOS sample profiles", "PR #187", "PR #192", "tokscale-core feat-batched-parallel-parse"]
---

# Dashboard load performance

## 文件目的

保留 2026-08 那輪載入延遲調查的**方法、被推翻的假設與實測數字**，讓後續的效能工作不必重跑已經有答案的實驗，也不必重蹈同樣的量測錯誤。

被推翻的假設與成立的結論同樣重要，所以本文一併保留。多數在這裡花掉的時間不是花在修正上，是花在**證明某條路不值得走**。

---

## 症狀與它實際上是什麼

回報是三件事：

| 回報 | 實際 |
|---|---|
| 第一層 loading 很慢（新機、Pro 機型也是） | 三份報表同時發出，互相爭用 |
| turns 要等 20 分鐘以上 | Daily／Monthly 為了 turns 另外發一次 hourly 全量掃描 |
| 重開沒有變快，感覺沒吃到快取 | 快取有效，但 process 重啟會丟掉記憶體內的重開快照 |

第三點最容易誤判。磁碟快取是有效的——暖啟動引擎只要約 2 秒。慢的是**那 2–5 秒完全空白**，而不是資料要重算。

---

## 量測方法（可重用的部分）

### 隔離語料，不要動正式快取

每次量測用 `cp -Rc` 對 `~/.config/tokscale` 做 APFS clone，剝除 `codex-credentials.json`，只清 clone 內的 `cache/source-message-cache-v2`。正式設定與快取永不作為刪除或寫入目標。

`TOKSCALE_CONFIG_DIR` 同時控制**快取根目錄與 `PathRoot::Config` 的 scanner roots**，所以整份 profile clone 才能保住 config-root 來源；一個空目錄不是 production-equivalent 語料。

### 配對交錯，不要連續同一組

同組態的兩次冷跑在這台機器上曾相差 **9.4 秒**。任何單次比較都不成立。做法：

- 每組 7 對，**交錯**執行（NEW/OLD、OLD/NEW…）以抵銷漂移
- 固定 `RAYON_NUM_THREADS=2`（對齊出貨上限）與 `TOKSCALE_PRICING_CACHE_ONLY=1`（避開網路）
- **前後檢查語料 token**——這個 repo 的開發過程本身就在寫 log，語料會在量測期間長大

### 熱節流會偽造結論

連續跑 12 次 40 秒的冷掃描之後，中位數比機器閒置時單跑高約 **6.3 秒**（40,944ms vs 34,596ms）。批次量測要把這件事算進去，或改以配對比較讓它抵銷。

### 對拍 oracle 必須先在「零改動」上證明

**這是本文最重要的一條。**

graph 報表帶 volatile 的 `generated_at` 與 `processing_time_ms`，每日 client 列來自未排序的 `HashMap::into_values()`。所以：

> 天真的位元組或雜湊比對，**在兩次完全未改動的執行上就會失敗**。

正確順序是先跑未改動的引擎兩次，要求 oracle 通過，**之後**才拿它去驗改動。實務上這一步抓到的第一個東西是 oracle 自己：第一版 digest 直接依迭代順序雜湊 client 列，`OLD-a` 與 `OLD-b` 立刻不同。修法是雜湊前遞迴正規化每一個無序容器。

---

## 被推翻的假設

以下每一條都量過，**都不是原因**。不要重跑。

| 假設 | 實測 | 結論 |
|---|---|---|
| rayon 執行緒上限（2）綁住冷掃描 | 2 條 28,772ms／10 條 28,078ms／6 條 31,213ms；同為 2 條的兩次相差 9.4 秒 | **差 2.4%，在雜訊內**。工作根本沒被分派出去，提高上限沒有意義 |
| I/O 綁住 | 讀完整份 6.10 GB 語料單執行緒 **7.6 秒**（約 800 MB/s） | I/O 最多佔 28 秒掃描的四分之一 |
| 一開始就要全部年份，顆粒度太大 | 全部年份 28,734／29,526ms；只要 2026 年 28,127／34,522ms | **完全不省**。沒有日期索引，解析器必須讀完每則訊息才知道日期；年份只縮小折疊範圍 |
| 語料只多了約 1000 筆 turns，不該慢 13 秒 | turns +669，但新增位元組 **約 960 MB**（codex 792 MB、claude 168 MB），佔 8.2 GB 語料的 +12% | **turns 是錯的量尺**。turns 是「一輪對話的起點」，不是位元組 |
| 快取的 bincode 序列化＋fsync 是那段序列化成本 | 剖析中連前幾名都排不上；fold、快取寫入與其餘 20 個 client 合計 13.8% | 錯。真正的成本在兩個解析 lane |
| 暖啟動可以靠先比 size+mtime 再算取樣雜湊來省 | `message_cache.rs` 早就先比 size 與 mtime 才算 | **守衛已存在**。那段 read 花在「已比對通過」的檔案上，是刻意的二次確認 |

---

## 剖析結果

macOS 內建 `sample`，不需要改任何程式碼，也不需要上游變更。

### 冷掃描（主執行緒約 20,409 個取樣 ≈ 整段掃描）

| 佔比 | 內容 |
|---|---|
| 43.2% | claude lane — `parse_claude_file_with_home` |
| 43.0% | codex lane — `parse_full_codex_raw_source` |
| — 其中 20.9% | `sha2::compress256`（快取指紋） |
| 13.8% | 其他全部（fold、快取寫入、其餘約 20 個 client） |

**兩個 lane 依序跑在主執行緒上，rayon 工作執行緒整段停在 `WorkerThread::wait_until`。** 這一次解釋了先前所有矛盾：不是 I/O、不是平行 CPU，是序列化。

### 暖掃描（重複 20 次暖迭代以取得足夠取樣窗口）

| 佔比 | 內容 |
|---|---|
| ~24% | `compute_sample_hashes` 底下的 `read` syscall |
| | `SourceFingerprint::check_*` → `primary_fingerprint_matches` → `compute_sample_hashes` |

暖啟動的成本是**對約 8,362 個檔案逐一 `open` + `read` 取樣位元組**，不是雜湊也不是解析。

取樣時機的坑：父程序會緩衝子程序輸出到最後才印，所以不能靠 log 出現與否判斷階段。改用「等待固定時間後取樣」，並把暖階段重複多次拉長窗口。

---

## 出貨的三項改動

### 一、turns 折疊進 graph（PR #187）

`DailyContribution` 新增 `turns_by_client`，在 graph 的同一次掃描中依 exact client 累加 `is_turn_start`。Daily／Monthly 直接讀它，**移除第三次全量掃描**。

平行 reduction 會遺失計數，所以 `merge` 必須一併折疊該欄位——這是加欄位時最容易漏的一步。

### 二、graph 獨佔關鍵路徑（PR #187）

三份報表同時發出時，在 2 執行緒池上互相爭用，**總工作量加倍**（33 秒序列 → 67 秒並行）且延後首屏。改成 graph 先完成並 render，model 由需要它的 lens 事後請求。

| | 改動前 | 改動後 |
|---|---|---|
| 冷啟動首次繪製 | 42,953ms | **28,555ms** |
| Daily 的額外 turns 等待 | 有 | **0ms** |

真實資料驗收：94 天、12,849 turns，新舊路徑逐日逐 client 相等。

### 三、重啟還原快照（PR #192）

不同的軸——**不是縮短等待，是消除空白**。重啟後從 caches 目錄還原上一份 graph payload 立即渲染，背景刷新，右上角既有的 refresh 按鈕兼任新鮮度指示。

配額刻意不落盤（帶帳號身分），所以還原後額度卡片會短暫沒有值——那些列因此顯示「查詢中…」而不是「沒有資料」。**「沒有資料」是在宣稱問過了、沒有**，在第一次查詢落地前那是假答案。

### 四、引擎分批平行解析（tokscale-core，未合併）

兩個重 lane 的每檔工作天然分成兩半：

| 階段 | 內容 | 性質 |
|---|---|---|
| A（平行） | 指紋檢查（全部檔案）＋ 解析（僅 cache miss） | 貴、每檔獨立、無共享可變狀態 |
| B（序列、原始順序） | 命中就地重播、套用快取異動、逐訊息 dedup 與 sink | 便宜、順序相依 |

關鍵限制：

- **sink 契約不變**（`S: FnMut(&UnifiedMessage)`），六個生產呼叫點一行都不用改
- **dedup 集合維持單一序列**，看到的訊息順序與改動前相同。重複鍵的訊息**內容不保證相同**，所以保序是全部的安全論證
- **命中不平行化**：exact hit 會 clone 整個快取向量，一批 32 個保留的記憶體遠超過逐檔處理。階段 A 對命中只回傳 marker
- repo 內已有此形狀的前例：materialized 相容路徑本身就是「有序平行 map ＋ 序列快取異動與 dedup」

Amdahl 上限：86.2% 理想減半、13.8% 序列，兩個 worker 的理論上限約 **1.76×**。

實測（7 對交錯冷跑，`RAYON_NUM_THREADS=2`，隔離 clone，每次清快取）：

| | NEW 中位數 | OLD 中位數 | 門檻 | 結果 |
|---|---|---|---|---|
| 冷啟動 | 23,587ms | 32,302ms | ≥ 20% 改善 | **−27.0%** |
| 暖啟動 | 1,649ms | 1,813ms | ≤ +5% 退化 | 快 9% |
| 峰值 RSS | 289.0 MB | 290.9 MB | ≤ +25% 退化 | 低 0.7% |

冷啟動的兩個區間**完全不重疊**（NEW 最大 26,117 < OLD 最小 29,769）。其中兩次的清快取指令回報 `Directory not empty`，排除那兩次重算為 −27.8%，結論不變——兩種算法都記錄在此，不擇一呈現。

RSS 沒有上升是 hit-marker 設計的直接證據：分批本身會增加保留量，但命中不 clone 就不會。

正確性對拍（凍結語料副本，避免本專案開發過程自身寫入 log 造成漂移）：

| 執行 | digest |
|---|---|
| OLD-a | `7883380643849671025` |
| OLD-b | `7883380643849671025` |
| NEW | `7883380643849671025` |

前兩者相同才使第三者有意義。變異驗證：破壞批次保序 → 2 條測試轉紅（其中一條直指「batched streaming order must match the unbatched materialized order」）；dedup 集合改為每批重置 → 1 條轉紅。1,323 個測試全數通過。

---

## 一條沒有被採納的路

回到上游 materialized 路徑以取回平行度——**不可行，而且不是記憶體的理由**。那條路徑的文件註解明載它不對 `simple_lane!` 的 client 做 per-client 跨檔 dedup，且解析出較窄的 client 集合，**總量會與其他報表分歧**。它是保留的 public API，不是可以直接切回去的實作。

---

## 方法上的教訓

| 教訓 | 來自 |
|---|---|
| oracle 必須先在零改動上證明，否則它的綠燈毫無意義 | 第一版 digest 在兩次未改動執行上就不同 |
| 「通過」不等於「能失敗」——加測試的速度容易快過驗證測試能失敗的速度 | 本輪兩次寫出不可能失敗的斷言，都只有靠變異跑才發現 |
| `git diff main <branch>` 會把 **main 自己的前進**顯示成分支的刪除 | 差點據此誤報一條分支倒退了引擎 pin |
| 用錯的量尺會讓真實的成長看起來不合理 | turns +669 vs 位元組 +960 MB |
| 把診斷資訊丟掉會讓失敗看起來像成功 | `2>/dev/null` 讓被殺掉的程序看起來像「跑完沒輸出」；`tail -5` 截掉了最大的測試 binary |
| 長時間程序要由主 session 持有 | 背景任務裡再 `&` 一層，外層一結束整個程序群被收掉 |
