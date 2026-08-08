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

每次量測用 `cp -Rc` 對 `~/.config/tokscale` 做 APFS clone，**剝除 clone 內的 provider 憑證檔**，只清 clone 內的 `cache/source-message-cache-v2`。正式設定與快取永不作為刪除或寫入目標。

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

而穩定只是必要條件，不是充分條件。第二版 digest 通過了零改動證明，卻在外部審查時被指出**只雜湊摘要欄位**——每列取 `client`／`model_id`／訊息數／成本，每日取合併後的 token 總量。兩個重複候選只要在這些欄位一致、而在 `provider_id` 或 token 分桶組成上不同，換誰勝出就換了輸出，digest 卻一動也不動。也就是說：它對自己唯一存在的目的是瞎的。同一次修正還發現排序鍵 `(client, model_id)` 不是全序（同 client 同 model 可以跨 provider），相等鍵的順序不保證，digest 本身就會跑動。

所以 oracle 要問兩個問題，不是一個：**同一份輸入跑兩次會不會一樣**（穩定），以及**輸出真的變了的時候它會不會變**（敏感）。第一個問題靠零改動對拍回答。第二個問題**不能靠讀程式碼或逐欄位檢查覆蓋面回答**——這份 digest 前四版都通過了穩定性證明，也都各自附了一套聽起來成立的覆蓋面論證，而四版全是瞎的。敏感度只能拿變異證：餵它一個已知不同的輸出，要求數字必須變。做法與結果見後面的〈敏感度〉。

---

## 操作手冊

以下指令可直接照抄。路徑一律用變數，不把私有路徑寫進 tracked tree。

### 一、建立隔離語料

```bash
WORK=$(mktemp -d)            # 不可預測、絕對路徑
chmod 700 "$WORK"
# 中斷、失敗、正常結束都要清掉。整套量測必須在同一個 shell session 內跑完。
# 刪除也要驗證。$WORK 裡有憑證副本，而 `rm -rf` 可能因為權限或不可變旗標
# 部分失敗；沒有檢查的話，每一條離開路徑都會安靜地留下它們。
cleanup() {
  rm -rf "$WORK" 2>/dev/null
  [ -e "$WORK" ] || return 0
  chmod -R u+rwX "$WORK" 2>/dev/null; rm -rf "$WORK" 2>/dev/null
  [ -e "$WORK" ] && echo "WARNING: $WORK 刪除失敗，內含憑證副本，請手動處理" >&2
  return 0
}
# 訊號 handler 裡的 exit 會**再觸發一次** EXIT handler。清理是冪等的所以無害，
# 但下一節的變異 handler 不是——那裡第二次執行會找不到已刪掉的備份而印出假警告。
# 兩處統一先 `trap - EXIT` 再 exit，形狀一致，抄過去不會抄到壞的那個。
on_signal() { cleanup; trap - EXIT; exit "$1"; }
trap cleanup EXIT
trap 'on_signal 129' HUP          # 129 = 128 + SIGHUP（終端斷線）
trap 'on_signal 130' INT          # 130 = 128 + SIGINT
trap 'on_signal 143' TERM         # 143 = 128 + SIGTERM

# 憑證檔名與清單位置都不進公開文件（見 AGENTS.md「Public repository」）。
# 由操作者指向自己的私有覆蓋層，一行一個 glob；沒設就拒絕開始——沒有清單時
# 「什麼都沒刪」和「沒有東西該刪」長得一模一樣。
: "${CRED_GLOBS:?先指向一份憑證 glob 清單（一行一個），內容不進 repo}"

strip_credentials() {   # $1 = 要清理的根目錄；失敗回非零
  # 每個讀清單的迴圈都用 `read -r g || [ -n "$g" ]`：清單若沒有結尾換行，
  # `read` 會把最後一行放進 $g 卻回傳失敗，單純的 `while read` 會整條跳過。
  # 三個迴圈共用這個條件，所以一條沒換行的單行清單會同時讓「刪除」與「重掃
  # 驗證」都靜靜地零命中，然後回報成功。實測：plain=0、guarded=1。
  [ -s "$CRED_GLOBS" ] || { echo "REFUSE: 憑證清單不存在"; return 1; }
  local left=0
  while IFS= read -r g || [ -n "$g" ]; do
    g=${g%$'\r'}                       # CRLF 清單會在樣式尾巴留一個 CR，
    [ -n "$g" ] || continue             # 於是刪除與驗證一起零命中並回報成功
    # -type f 會漏掉「憑證被搬走、原地留一條 symlink」的情況：cp -R 保留那條
    # 連結，clone 裡看起來沒有憑證，實際上一讀就讀到活的那份。
    # -name 只比 basename，所以帶 / 的樣式（`<子目錄>/<檔名>`）永遠不會命中；
    # 刪除與驗證共用同一個 predicate，兩邊會一起靜靜地零命中然後回報成功。
    # 樣式一律是 clone-relative。絕對路徑接在 $1 後面會變成 `$1//abs/path`，
    # 永遠不命中——而刪除與驗證共用它，於是兩邊一起零命中並回報成功。與其猜
    # 怎麼正規化，不如把契約講明並拒絕。
    case "$g" in /*) echo "REFUSE: 憑證樣式必須是 clone-relative：$g"; return 1;; esac
    case "$g" in */*) pred=-path; pat="$1/$g";; *) pred=-name; pat="$g";; esac
    find "$1" \( -type f -o -type l \) "$pred" "$pat" -delete || return 1
  done < "$CRED_GLOBS"
  # 刪完再掃一次。這一步才是保證，前一步只是意圖。
  while IFS= read -r g || [ -n "$g" ]; do
    g=${g%$'\r'}
    [ -n "$g" ] || continue
    case "$g" in */*) pred=-path; pat="$1/$g";; *) pred=-name; pat="$g";; esac
    left=$(( left + $(find "$1" \( -type f -o -type l \) "$pred" "$pat" | wc -l) ))
  done < "$CRED_GLOBS"
  [ "$left" -eq 0 ] || { echo "REFUSE: clone 內仍有 $left 個憑證檔"; return 1; }

  # find 不會走進 symlink 目錄，所以「憑證目錄整個被搬走、原地留一條連結」
  # 這種安排下，上面的 glob 一個都不會命中、重掃也是 0——回報成功，而 clone
  # 仍然是一條通往活憑證的路。這裡不刪（穿過連結刪會刪掉使用者真正的憑證），
  # 只拒絕。
  local via
  via=$(find "$1" -type l | while IFS= read -r l; do
    [ -d "$l" ] || continue
    while IFS= read -r g || [ -n "$g" ]; do
      [ -n "$g" ] || continue
      g=${g%$'\r'}
      case "$g" in */*) gp=$(basename "$g");; *) gp="$g";; esac
      [ -n "$(find -L "$l" -name "$gp" -print -quit 2>/dev/null)" ] && { echo "  $l"; break; }
    done < "$CRED_GLOBS"
  done)
  [ -z "$via" ] || { printf 'REFUSE: 憑證可經由這些 symlink 目錄讀到：\n%s\n' "$via"; return 1; }
}

# APFS copy-on-write clone：92 MB 的快取複製幾乎零時間、零額外磁碟，
# 未寫入前與原檔共用區塊。-R 遞迴，-c 走 clonefile(2)。
cp -Rc ~/.config/tokscale "$WORK/tokscale" || exit 1
strip_credentials "$WORK/tokscale" || exit 1
```

`INT`／`TERM` 的 handler **必須自己 `exit`**。bash 跑完 trap handler 會**繼續執行後面的指令**，不會終止 shell——所以只寫 `trap 'rm -rf "$WORK"' EXIT INT TERM` 的話，Ctrl-C 會刪掉語料然後讓剩下的量測繼續跑，而且下一節的變異驗證把原始碼備份放在 `$WORK` 裡，那個 handler 會在還原之前把備份刪掉、把變異過的 `lib.rs` 留在樹上。這行為在本機驗過：handler 印完之後 `STILL RUNNING AFTER SIGNAL` 照樣印出來。

不限制掃描深度是刻意的。憑證目前都在頂層，但**深度上限只在「憑證永遠不會被放進子目錄」成立時才安全**，而那不是這份 runbook 能保證的事。多走幾秒的 metadata 換掉一個假設。

`cp -R`（不加 `-c`）會真的複製 92 MB；`-c` 才是 clone。大語料（凍結 `~/.claude` 與 `~/.codex` 約 7.9 GB）時差別是「瞬間」與「數十秒＋佔滿磁碟」。

**清除前務必做圍堵檢查**，這是唯一一道防止打到正式資料的守衛：

```bash
CACHE="$WORK/tokscale/cache/source-message-cache-v2"

# $WORK 自己要先解析。macOS 的 mktemp -d 給 /var/folders/...，而 /var 是指向
# private/var 的 symlink，拿 pwd -P 的輸出去比對未解析的 $WORK 會永遠不相等。
WORK_P=$(cd "$WORK" && pwd -P) || exit 1

# 解析的是「父目錄」不是葉節點：cd + pwd -P 會攤平路徑上的每一層 symlink。
# 只檢查葉節點是不是 symlink 擋不住 cache 這一層被搬走的情況。
PARENT_P=$(cd "$(dirname "$CACHE")" 2>/dev/null && pwd -P) || {
  echo "REFUSE: cache parent unreachable"; exit 1; }
case "$PARENT_P/$(basename "$CACHE")" in
  "$WORK_P"/*) ;;
  *) echo "REFUSE: resolves outside the clone"; exit 1;;
esac

[ -L "$CACHE" ] && { echo "REFUSE: symlink"; exit 1; }
[ -d "$CACHE" ] || { echo "REFUSE: not a directory"; exit 1; }
```

這一段被外部審查抓過，值得記下來，因為兩個缺陷都不是讀程式碼看得出來的：

- **舊版把解析過的路徑拿去比對未解析的 `$WORK`**，在 macOS 上結果是**一律 REFUSE**——那份 runbook 照抄根本跑不起來。一道永遠拒絕的守衛在測試時看起來很安全，直到有人為了讓它動而把它拿掉。
- **舊版只檢查葉節點是不是 symlink。** 若使用者把 `~/.config/tokscale/cache` 設成 symlink（搬移大快取是常見作法），`cp -R` 依 `cp(1)` 會**保留 symlink 而不是跟隨**，於是 clone 內的 `cache` 仍指向外部；葉節點是真目錄，`[ -L ]` 為假，接著的 `rm -rf` 就穿過那一層把正式快取刪掉。

第二點不是推論。修正前後各跑三個 fixture（正常 clone／`cache` 是 symlink／葉節點是 symlink）：把第一個缺陷單獨修好之後，`cache` 是 symlink 的那個 fixture 被**放行**，隨後的 `rm -rf` 確實刪掉了放在外部的 sentinel 檔。新版三個 fixture 依序是 ACCEPT／REFUSE／REFUSE。

**破壞性步驟的守衛要自己證明會拒絕**，跟測試要證明能失敗是同一件事——只看到它在正常輸入下沒擋路，證明不了任何事。

### 二、冷啟動：清快取

```bash
rm -rf "$WORK/tokscale/cache/source-message-cache-v2"
```

只清 clone 內的 source-message cache，**保留 pricing cache**（否則會混入定價抓取成本）。正式路徑永不作為刪除目標。

> **陷阱**：若出現 `rm: Directory not empty`，代表當下還有東西在寫該目錄，那一次**不是純冷**。要記錄下來並在統計時同時給「含入」與「排除」兩種算法，不要擇一呈現。

### 三、單次執行

```bash
env HOME="$HOME" \
    TOKSCALE_CONFIG_DIR="$WORK/tokscale" \
    TOKSCALE_PRICING_CACHE_ONLY=1 \
    RAYON_NUM_THREADS=2 \
    ./target/release/examples/bench_scan "$HOME"
```

| 變數 | 為什麼 |
|---|---|
| `HOME` | scanner roots 來源。量測正式語料時用真實 HOME；做 digest 對拍時改指向凍結副本 |
| `TOKSCALE_CONFIG_DIR` | 同時控制快取根目錄**與** `PathRoot::Config` 的 scanner roots |
| `TOKSCALE_PRICING_CACHE_ONLY` | 避免網路抓取污染計時 |
| `RAYON_NUM_THREADS=2` | 對齊出貨的 `RAYON_INIT` 上限；不設會量到不會出貨的組態 |

### 四、配對交錯迴圈

```bash
run() { C="$WORK/tokscale/cache/source-message-cache-v2"
        # 計時容得下污染但容不下隱形：清除失敗時這一次**不是純冷**，標記出來，
        # 統計時同時給含入與排除兩種算法。正確性對拍沒有這個選項，見 digest_arm。
        rm -rf "$C" 2>/dev/null
        [ -e "$C" ] && printf 'CONTAMINATED '
        # 用命令替換而不是管線：管線的結束狀態是 tail 的（幾乎永遠 0），
        # 被殺掉或 panic 的量測會被當成完成的樣本記錄下來。
        out=$(env HOME="$HOME" TOKSCALE_CONFIG_DIR="$WORK/tokscale" \
                  TOKSCALE_PRICING_CACHE_ONLY=1 RAYON_NUM_THREADS=2 \
                  "$2" "$HOME" 2>&1); st=$?
        if [ $st -ne 0 ]; then
          printf '%-8s DISCARD status=%s\n' "$1" "$st"
          printf '%s\n' "$out" | tail -3
          return 1
        fi
        printf '%-8s %s\n' "$1" "$(printf '%s\n' "$out" | tail -1)"; }

for i in 1 2 3 4 5 6 7; do
  if [ $((i % 2)) -eq 1 ]; then a=NEW A=$NEW_BIN; b=OLD B=$OLD_BIN
  else                          a=OLD A=$OLD_BIN; b=NEW B=$NEW_BIN; fi
  # 配對的意義就是抵銷漂移，所以**任一 arm 失敗，整對都不採計**——
  # 留下落單的那次觀測會直接偏移比較結果。
  run "$a-$i" "$A" && run "$b-$i" "$B" || echo "PAIR-$i DISCARDED（兩個 arm 都不採計）"
done
```

**兩個 arm 用同一個 binary 名稱、不同 worktree**：舊版用 `git worktree add <dir> <base-sha>` 檢出基準點，兩邊各自 `cargo build --release --example bench_scan`。

> **陷阱**：這個迴圈要**當成背景任務的前景指令**執行。若寫成 `( … ) &` 再放進背景任務，外層指令一結束整個程序群會被收掉——實際發生過，只留下第一行輸出。
>
> **陷阱**：不要 `2>/dev/null`。程序被殺掉會看起來像「跑完但沒輸出」。

### 五、Digest 對拍（正確性）

計時可以容忍語料漂移，**digest 不行**。本專案的開發過程本身在寫 log，所以要先凍結語料：

```bash
mkdir -p "$WORK/corpus" && chmod 700 "$WORK/corpus"
# 每個 clone 都要檢查狀態。少掉一個來源不會讓後面的執行失敗，只會讓那條
# parser lane 從對拍裡無聲消失——digest 仍然算得出來，只是在比一份殘缺的語料。
cp -Rc ~/.claude "$WORK/corpus/.claude" || { echo "REFUSE: claude 語料複製失敗"; exit 1; }
cp -Rc ~/.codex  "$WORK/corpus/.codex"  || { echo "REFUSE: codex 語料複製失敗";  exit 1; }
for d in "$WORK/corpus/.claude/projects" "$WORK/corpus/.codex/sessions"; do
  [ -d "$d" ] && [ -n "$(ls -A "$d" 2>/dev/null)" ] || { echo "REFUSE: $d 空的或不存在"; exit 1; }
done

# cp -R 保留 symlink 而不跟隨（同上），所以「凍結」的語料裡可能有一條路徑仍然
# 指向會變動的活資料——那樣的話 OLD 與 NEW 讀到的根本不是同一份輸入。指向
# clone 內部的無害，指向外部的必須先處理。
# 掃整份 corpus，不只那兩個 root——本文後面量到掃描器讀的東西比那兩個 root
# 定義多，所以按 root 圈範圍的守衛，範圍必定不足。
# 而且 corpus 不是唯一的輸入：`TOKSCALE_CONFIG_DIR` 同時提供 `PathRoot::Config`
# 的 scanner roots（見前面「隔離語料」一節），所以 config clone 要一起檢查。
WORK_P=$(cd "$WORK" && pwd -P)
assert_frozen() {   # $1 = 要檢查的根目錄
external=$(find "$1" -type l | while IFS= read -r l; do
  # 整條鏈都要解析。只解析父目錄、信任未解析的最後一段是不夠的：一條指向
  # $WORK 內部的連結，其目標本身可以再指向外面，看起來「內部」但讀下去是活的。
  # 解析不出來就拒絕，不要跳過。「現在是斷的」不等於「量測期間是斷的」——
  # 目標的生產者還活著，可以在檢查之後、在兩個 arm 之間把它建出來，於是
  # OLD 與 NEW 讀到不同的活位元組。無法判定的連結一律當成不凍結。
  tgt=$(realpath "$l" 2>/dev/null) || { echo "  $l -> (無法解析)"; continue; }
  case "$tgt" in "$WORK_P"/*) continue;; esac          # 整條鏈都在 clone 內，無害
  # 只擋掃描器讀得到的：連結自己是 *.jsonl，或它通往一棵含 *.jsonl 的樹。
  case "$l" in *.jsonl) echo "  $l -> $tgt"; continue;; esac
  [ -d "$tgt" ] && [ -n "$(find "$tgt" -name '*.jsonl' -print -quit 2>/dev/null)" ] && echo "  $l -> $tgt"
done)
[ -z "$external" ] || { printf 'REFUSE: 以下 symlink 通往語料之外的 *.jsonl，凍結不成立：\n%s\n' "$external"; return 1; }
}
# 光靠「檢查當下裡面有沒有 *.jsonl」分類是不夠的——那跟上一版把斷鏈跳過犯的是
# 同一個錯：**現在的內容不是量測期間的內容**，活著的生產者可以在兩個 arm 之間
# 在那個目錄裡放一個新的 jsonl。所以指向外部的連結一律**具現化**成快照，而不是
# 分類。動手前量過：外部目標的總量相對於整份語料可以忽略，所以具現化不是
# 昂貴的選項——但這個比例要自己在自己的機器上量，別照抄結論。
MAT_MAX_KB=${MAT_MAX_KB:-102400}     # 單一外部目標的上限，預設 100 MB
materialize_external() {   # $1 = 根目錄
  fail="$WORK/.materialize-failed"; rm -f "$fail"
  find "$1" -type l | while IFS= read -r l; do
    tgt=$(realpath "$l" 2>/dev/null) || { rm -f "$l" || : > "$fail"; continue; }
    case "$tgt" in "$WORK_P"/*) continue;; esac
    # 先複製到暫存名字，成功才換掉連結。順序反過來的話——先 rm 再 cp——一次
    # 中途失敗會留下「連結沒了、目錄只複製到一半」的狀態，而後面的凍結斷言
    # 看不到任何外部連結，就會把一份被靜靜截斷的語料當成通過。
    # 具現化前先判型與設上限。無條件 `cp -RL` 會跟著巢狀連結走進任意的樹：
    # 可能複製大量無關的私有資料、在裝置檔或 FIFO 上卡住、或塞爆磁碟——而這
    # 一切都發生在凍結斷言之前。特殊檔一律拒絕，目錄超過上限就交給操作者決定。
    if [ -f "$tgt" ]; then :
    elif [ -d "$tgt" ]; then
      sz=$(du -sk "$tgt" 2>/dev/null | cut -f1); sz=${sz:-0}
      [ "$sz" -le "$MAT_MAX_KB" ] || {
        echo "REFUSE: $l 的目標超過 ${MAT_MAX_KB}k（${sz}k），請自行決定要排除還是提高上限" >&2
        : > "$fail"; continue; }
    else
      echo "REFUSE: $l 的目標不是一般檔案或目錄" >&2; : > "$fail"; continue
    fi
    tmp="$l.materializing.$$"
    if cp -RL "$tgt" "$tmp" 2>/dev/null; then
      rm -f "$l" && mv "$tmp" "$l" || : > "$fail"
    else
      rm -rf "$tmp"; : > "$fail"        # 連結原封不動留著，讓斷言抓得到
    fi
  done
  # 迴圈在管線的 subshell 裡，狀態傳不出來；用檔案系統上的標記跨過那道邊界。
  [ ! -e "$fail" ] || { echo "REFUSE: 具現化失敗，語料可能不完整"; return 1; }
}
materialize_external "$WORK/corpus"   || exit 1
materialize_external "$WORK/tokscale" || exit 1

# 具現化會把外部檔案帶進 clone 內的**新路徑**，所以憑證剝除必須在它之後再跑
# 一次；也因為這樣，憑證樣式建議用 basename 形式，帶路徑的樣式只認得原本的
# 位置，認不得具現化之後的位置。
strip_credentials "$WORK/corpus"   || exit 1
strip_credentials "$WORK/tokscale" || exit 1

# 具現化之後**必須**重跑凍結檢查。上面的迴圈在管線的 subshell 裡，它的失敗傳不
# 出來；真正的保證是「跑完之後一條外部連結都不剩」，跟憑證剝除完要重掃一次同理。
assert_frozen "$WORK/corpus"   || exit 1
assert_frozen "$WORK/tokscale" || exit 1

# 這兩個目錄裡有活的 provider OAuth token，整份複製會一起帶進來。用上面那個
# 同一個函式剝除並驗證；失敗就中止，不要帶著憑證繼續量測。
strip_credentials "$WORK/corpus" || exit 1

# 之後所有執行都傳 "$WORK/corpus" 當 HOME
# （目錄別取名 home：文件 gate 會把 "/home/" 當成 Unix 根路徑擋下）
```

$WORK 的 `trap` 已經在第一步裝好，中斷也會把語料一起帶走。

> 這一段被外部審查抓過兩次，第二次抓的是**我第一次的「修正」只寫了註解沒寫指令**——一段描述剝除步驟的散文，讀起來像已經做了，實際上什麼都沒執行。憑證仍然留在工作區。
>
> 描述一個安全步驟不等於執行它。這條在教訓表裡單獨佔一列。

> 這條守衛的範圍與判準都是量出來的，不是想出來的。**範圍**必須是整份 corpus：本文後面量到掃描器讀的東西比 `.claude/projects` 與 `.codex/sessions` 兩個 root 定義多，所以按 root 圈的守衛範圍必定不足。**判準**不能是「任何指向外部的 symlink」：真實的 agent 設定目錄裡本來就有一批與掃描器無關的連結（共用的工具目錄、外部安裝的執行檔、快取…），一律拒絕會得到一個**永遠拒絕**的守衛——那正是前面圍堵檢查犯過的錯。實際數量因機器而異，動手前自己量。
>
> 所以判準是「掃描器讀得到嗎」：連結自己是 `*.jsonl`，或它通往一棵含 `*.jsonl` 的樹。這個判準在本機跑出零命中，包含那條 `.claude/projects/<某專案>/memory`——它指向 repo 外的共用記憶目錄，但裡面沒有 `*.jsonl`。**它今天無害是運氣不是設計**，而守衛的作用就是讓下一次不再靠運氣。而且注意上面用的是 `external=$(... | while ...)` 而不是 `find | while ... exit 1`：管線裡的 `while` 跑在 subshell，`exit` 只會結束那個 subshell，外層照樣往下跑。
>
> **不要改成「只複製掃描器讀的那兩個目錄」。** 這條捷徑看起來更安全——憑證從構造上就進不來——但**實測會改變結果**。只複製 `~/.claude/projects` 與 `~/.codex/sessions`，訊息數從 171,984 掉到 171,983，digest 也隨之改變（那次量測用的是已作廢的第三版 digest，兩個絕對值不必重現；重點是**它們不相等**）。掃描器實際讀的東西比那兩個 root 定義多，差一則訊息就足以讓 digest 失去對照價值。
>
> 記在這裡是因為這正是本文想防的那種錯誤：一個聽起來顯然更好的做法，沒跑過就寫進 runbook。它的代價不是「稍微不準」，而是往後每一次對拍都在跟一個不同的語料比。

### digest 的建構規則

這份規則走到第五版才收斂，前四版**每一版都在自己的 commit message 裡宣稱完整**，然後被下一輪推翻。所以規則本身不如「為什麼是這個形狀」重要。

**不要手動列舉要雜湊的欄位。** 序列化整份報表再正規化，新欄位會自動進 digest，不需要有人記得。手動清單會在有人加欄位的那一刻無聲停止覆蓋——這是 v2（漏 `provider_id` 與 token 分桶）和 v3（`GraphResult` 五個欄位只雜湊 `contributions`，漏掉獨立 fold 產出的 `time_metrics`）共同的根因。

**預設要選「會大聲失敗」的那一邊。** 兩種名單的錯誤方向不對稱：

| 名單 | 漏掉一項的後果 |
|---|---|
| 排除清單（volatile 鍵、無序陣列） | `OLD-a != OLD-b`，當場停住 |
| 包含清單（要雜湊的欄位、要排序的陣列） | 安靜地少看一塊，綠燈照亮 |

所以 volatile 鍵用排除清單，陣列順序**預設保留**、只對確知無序的路徑排序。v4 反過來做——為了修 v3 而把每個陣列都排序——結果讓 digest 對 `contributions`／`years` 的順序失明，那正是排序類改動最可能造成的回歸。修一種失明時製造了另一種。

**唯一該排序的是真正無序的容器。** 每日 client 列來自 `HashMap::into_values()`，同一個 binary 跑兩次順序就不同，v1 直接照迭代順序雜湊、零改動對拍立刻抓到。用 JSON path（`contributions[].clients`）而不是鍵名指定，否則 aggregator 已排序的 `summary.clients` 會被一起掃進去——保留它的順序反而讓 digest 對「那個排序壞掉」也保持敏感。

**別自己磨掉精度。** `format!("{:.6}", cost)` 讓半微元以下的差異對 digest 隱形，而那是一個程式碼從未宣告、文件從未論證過的容差。用 serde_json 的最短往返數字形式。

**排除的只有 volatile：**`generated_at`、`processing_time_ms`。

三個 arm 共用 `$WORK/tokscale`，所以**每個 arm 都要自己清掉 message cache**：

```bash
# stdout 只吐 digest，人看的那一行走 stderr——這樣呼叫端可以直接比對數值。
digest_arm() {   # $1 = 標籤  $2 = binary
  C="$WORK/tokscale/cache/source-message-cache-v2"
  # 清除失敗就中止這個 arm。本文下面記過 `rm` 會以 `Directory not empty` 失敗，
  # 而且那在實際量測裡發生過兩次；帶著殘留快取跑出來的相等是假的。
  rm -rf "$C" || { echo "DISCARD 快取清除失敗" >&2; return 1; }
  [ -e "$C" ] && { echo "DISCARD 快取清除後仍存在" >&2; return 1; }
  out=$(env -i HOME="$WORK/corpus" PATH=/usr/bin:/bin \
            TOKSCALE_CONFIG_DIR="$WORK/tokscale" \
            TOKSCALE_PRICING_CACHE_ONLY=1 RAYON_NUM_THREADS=2 \
            "$2" "$WORK/corpus" 2>&1); st=$?
  [ $st -eq 0 ] || { printf '%-8s DISCARD status=%s\n' "$1" "$st" >&2
                     printf '%s\n' "$out" | tail -3 >&2; return 1; }
  line=$(printf '%s\n' "$out" | tail -1)
  printf '%-8s %s\n' "$1" "$line" >&2
  d=$(printf '%s\n' "$line" | sed -n 's/.*digest=\([0-9][0-9]*\).*/\1/p')
  [ -n "$d" ] || { echo "DISCARD 輸出裡沒有 digest" >&2; return 1; }
  printf '%s\n' "$d"
}

A=$(digest_arm OLD-a "$OLD_BIN") || exit 1
B=$(digest_arm OLD-b "$OLD_BIN") || exit 1
# 基準不穩就到此為止：NEW 跑出什麼都沒有意義，連跑都不必跑。
[ "$A" = "$B" ] || { echo "REFUSE: OLD-a=$A != OLD-b=$B，oracle 不穩定"; exit 1; }
N=$(digest_arm NEW "$NEW_BIN") || exit 1
[ "$N" = "$A" ] || { echo "MISMATCH: NEW=$N OLD=$A"; exit 1; }
echo "PASS digest=$A"
```

**每個 arm 的結束狀態是那支 binary 的，不是「digest 相等」。** 三支都正常結束但吐出三個不同的數值時，`&&` 串起來仍然整串成功——一個把「三個數值必須相同」寫成散文、卻用 `&&` 串起來執行的程序，等於沒有比對。所以要把數值接出來自己比。

**不清的話這個對拍證明不了它被引用來證明的東西。** `OLD-a` 會把 cache 填滿，`NEW` 接著就可能重播舊引擎寫進去的條目，而不是走它自己的 miss 解析與排序路徑——那正是要驗的東西。digest 仍然會相等，只是相等的原因變成「兩邊讀的是同一份快取」。

執行順序**必須**是：

```
OLD-a  →  OLD-b  →  NEW
```

`OLD-a == OLD-b` 成立之前，`NEW` 的結果沒有意義。若兩者不同，要修的是 digest，不是宣布通過。

### 六、剖析

```bash
sample <pid> 25 1 -file "$WORK/sample.txt"
```

25 秒窗口、1 ms 取樣。取得 pid 的方式視 harness 而定；若目標是子程序，用 `pgrep -f "<child pattern>"` 輪詢等它出現。

暖階段太短（2–5 秒）抓不準時，**把暖階段重複 20 次**拉長窗口，再從固定時間點開始取樣。

> **陷阱**：父程序常把子程序輸出緩衝到最後才印，所以**不能**靠 log 出現與否判斷階段已開始。改用「偵測到子程序後等固定秒數再取樣」。

分析 `sample` 輸出時，先看每個 thread 的總量：工作執行緒若整段停在 `rayon_core::WorkerThread::wait_until`，代表工作沒有被分派，這時要看的是主執行緒的自身時間。

### 七、變異驗證

```bash
cp src/lib.rs "$WORK/lib.rs.bak" || { echo "REFUSE: 備份失敗"; exit 1; }
# 還原要驗證，而且失敗時**不能**讓 cleanup 把備份刪掉——那是唯一的副本。
restore() {
  cp "$WORK/lib.rs.bak" src/lib.rs || return 1
  cmp -s "$WORK/lib.rs.bak" src/lib.rs || return 1
}
keep_or_clean() {
  if restore; then cleanup
  else echo "還原失敗：備份保留在 $WORK/lib.rs.bak，src/lib.rs 仍是變異狀態"; fi
}
# 變異期間，**每一條**離開路徑都必須先還原再清理——清理會刪掉 $WORK，備份就
# 在裡面。只保護 INT/TERM 不夠：終端斷線或任何其他原因造成的離開會走 EXIT，
# 那條路徑一樣會在還原之前刪掉備份，把變異過的 lib.rs 留在樹上。
mut_signal() { keep_or_clean; trap - EXIT; exit "$1"; }
trap keep_or_clean EXIT
trap 'mut_signal 129' HUP
trap 'mut_signal 130' INT
trap 'mut_signal 143' TERM

# …套用變異…
cargo test --release; st=$?                # 先接住狀態：restore 會覆蓋 $?，
                                           # 而下面的判定紀律要看的就是這個值
restore || { trap - EXIT; echo "REFUSE: 還原失敗，備份不刪"; exit 1; }
grep -c MUT src/lib.rs                     # 確認還原乾淨
echo "mutation test status=$st"            # 判定看這個，不是最後一道指令的狀態

trap cleanup EXIT                          # 還原完畢，恢復原本的 handler
trap 'on_signal 129' HUP
trap 'on_signal 130' INT
trap 'on_signal 143' TERM
```

判定紀律：

- **建置失敗不算殺死**——那不是測試抓到的
- **崩潰不算存活**——trap 不會印出 FAIL 行，只數 FAIL 的 harness 會誤判為通過。要同時看 exit code 與斷言總數
- 存活代表**測試沒在測它宣稱的東西**。修測試，不要削弱變異
- 不要用 `git checkout --` 還原：它還原到 HEAD，會連未提交的工作一起帶走

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
| OLD-a（`5546bd5`，批次化之前） | `6031430621329286087` |
| OLD-b（同上，零改動對拍） | `6031430621329286087` |
| BATCH（`921412b`，批次化之後） | `6031430621329286087` |
| HEAD（`5b5f500`，含後續修正） | `6031430621329286087` |

前兩者相同才使第三者有意義。數值一律與 engine commit 綁定記錄；digest 定義每改一次，兩支 probe 都要重編、全部執行都要重跑，舊數值作廢不得沿用（本文之前記過 `796241964421401448`，那是被作廢的第三版）。

變異驗證：破壞批次保序 → 2 條測試轉紅（其中一條直指「batched streaming order must match the unbatched materialized order」）；dedup 集合改為每批重置 → 1 條轉紅。1,341 個測試全數通過。

#### 敏感度：證明 oracle 真的會失敗

**上面四行全部是「相等」，而一個永遠回傳常數的 digest 會百分之百通過它們。** 相等只證明穩定，不證明敏感——這兩件事要分開證。

做法是變異記憶體裡的 `GraphResult` 再算 digest（不是變異引擎），把問題隔離到 oracle 本身，一次掃描得到全部判定。每個變異對應一種**真實存在過**的盲點：

| 變異 | 期望 | 結果 | 對應的失明 |
|---|---|---|---|
| `provider_id` 改一個字元 | 變 | ✅ | v2 |
| `time_metrics.session_count +1` | 變 | ✅ | v3（只看 `contributions`） |
| 交換兩天 `contributions` | 變 | ✅ | v4（把有序陣列排序掉） |
| 某列 `cost += 1e-9` | 變 | ✅ | `{:.6}` 磨掉的精度 |
| **交換同一天的兩個 client 列** | **不變** | ✅ | 負對照 |

最後一項最容易被略過，但沒有它前四項也可能只是「這個 digest 對什麼都敏感」的假象。它證明每日 client 列的排序是**刻意的設計**，不是又一個沒被發現的盲點。

---

## 一條沒有被採納的路

回到上游 materialized 路徑以取回平行度——**不可行，而且不是記憶體的理由**。那條路徑的文件註解明載它不對 `simple_lane!` 的 client 做 per-client 跨檔 dedup，且解析出較窄的 client 集合，**總量會與其他報表分歧**。它是保留的 public API，不是可以直接切回去的實作。

---

## 方法上的教訓

| 教訓 | 來自 |
|---|---|
| oracle 必須先在零改動上證明，否則它的綠燈毫無意義 | 第一版 digest 在兩次未改動執行上就不同 |
| 穩定的 oracle 仍可能是瞎的：相等只證明穩定，敏感度要拿變異單獨證 | digest 前四版全部通過零改動對拍，全部是瞎的；一個回傳常數的 digest 也會通過那些對拍 |
| 變異驗證要帶負對照，否則「全部偵測到」可能只是對什麼都敏感 | 交換同一天的兩個 client 列必須**不**動 digest，那才證明排序是設計而非盲點 |
| 修一種失明時會製造另一種 | v4 為了補齊欄位涵蓋把每個陣列都排序，於是對 `contributions`／`years` 的順序失明 |
| 名單的預設方向要選「漏掉會大聲失敗」的那一邊 | 排除清單漏一項 → `OLD-a != OLD-b` 當場停住；包含清單漏一項 → 安靜少看一塊 |
| 破壞性步驟的守衛要拿 fixture 證明它**會拒絕**，不能只看它沒擋路 | 圍堵檢查同時有「永遠拒絕」與「穿過 symlink 父目錄刪到正式快取」兩個缺陷，兩個都是外部審查抓的 |
| 複製整份 profile 當語料＝把憑證一起複製；剝除與事後刪除都要寫進步驟 | 本輪就有兩份活的 codex token 被這份 recipe 放進 benchmark 工作區 |
| **描述一個安全步驟不等於執行它** | 第一次「修正」只加了一段承諾會剝除憑證的註解，沒有任何指令；審查第二輪才抓到，憑證整段期間都還在 |
| 刪除類的守衛要有「刪完再掃一次」的驗證步驟 | 只呼叫 `find -delete` 是意圖，重掃一次得到 0 才是保證 |
| `trap` 的 handler 跑完會**繼續**執行，不會終止 shell；`INT`／`TERM` 要自己 `exit` | 加上 trap 的那一版讓 Ctrl-C 會刪掉語料後繼續量測，並在還原前刪掉變異驗證的原始碼備份 |
| 修正引入的新機制本身也要當成新程式碼審 | trap 是為了修「語料不留過夜」而加的，它自己帶進兩個更嚴重的缺陷 |
| 保護「每一條離開路徑」，不是列舉想得到的訊號 | 只擋 `INT`／`TERM` 之後，終端斷線走 `EXIT` 依然會刪掉備份、留下變異過的原始碼 |
| 準備步驟失敗要中止，不能靠後續步驟自己失敗 | 少複製一個語料來源不會讓執行報錯，只會讓那條 parser lane 從對拍裡無聲消失 |
| 對拍的每個 arm 都要從**同一個**初始狀態開始，共用快取會讓相等變成假的 | 不清 message cache 的話 `NEW` 會重播 `OLD-a` 寫進去的條目，相等的原因變成「讀的是同一份快取」 |
| 「清乾淨」這個前提本身會失敗，而且本文記錄過它真的失敗過兩次 | `rm -rf` 遇上 `Directory not empty` 不會中止；正確性 arm 必須中止，計時 arm 至少要標記為污染 |
| 「凍結」也是一個會失敗的前提：`cp -R` 保留 symlink，快照裡可能還有一條路通往活資料 | 本機語料就有一條指向外部的 symlink，今天沒影響純屬它裡面沒有 `*.jsonl` |
| 管線裡的 `while` 在 subshell，`exit` 出不了外層 | 這類守衛寫成 `find \| while ...; exit 1` 會完全失效；改成先收集再判斷 |
| `-type f` 會放過「原地留一條 symlink」的檔案 | 憑證被搬走後 clone 看起來乾淨，一讀就讀到活的那份 |
| 守衛的**範圍**和**判準**都要量過：範圍太窄會漏，判準太寬會變成永遠拒絕 | 按兩個 root 圈範圍已被自己的量測證明不足；「任何外部 symlink 都拒絕」會命中一批與掃描器無關的既有連結 |
| symlink 要防的是**祖先**不只是檔案本身 | 憑證目錄整個被搬走時，`find` 不會走進去，glob 零命中、重掃零殘留、回報成功 |
| containment 檢查要解析**整條鏈**，不能信任未解析的最後一段 | 一條指向 clone 內部的連結，其目標可以再指向外面 |
| 訊號 handler 裡的 `exit` 會再觸發一次 `EXIT` handler | 第二次執行找不到已刪掉的備份，於是每次成功的訊號復原都印出「還原失敗」的假警告 |
| 要判定的那個狀態要**當場接住**，別讓後面的指令覆蓋 `$?` | 變異驗證的判定紀律要求看 exit code，而 recipe 自己在 `cargo test` 之後就把它蓋掉了 |
| `while read` 會吃掉沒有結尾換行的最後一行 | 一條單行、沒換行的憑證清單會讓刪除與驗證同時零命中並回報成功；用 `read -r g \|\| [ -n "$g" ]` |
| 輸入不只一個來源時，守衛要套在**每一個**來源上 | 凍結檢查只套 corpus，而 config clone 同樣提供 scanner roots |
| 「現在無害」不等於「量測期間無害」：無法判定的輸入要拒絕而不是跳過 | 斷掉的 `*.jsonl` 連結在檢查時給不出資料，但它的生產者可以在兩個 arm 之間把目標建出來 |
| 同一個理由也打死「一次性內容檢查」：外部連結要具現化，不是分類 | 「這個目錄現在沒有 `*.jsonl`」跟「斷鏈現在給不出資料」是同一種一次性判斷 |
| `-name` 只比 basename，帶 `/` 的樣式永遠不命中 | 一條 `<子目錄>/<檔名>` 形式的樣式會讓刪除與驗證一起零命中並回報成功 |
| 契約要講明並拒絕違反者，不要猜怎麼正規化 | 絕對路徑樣式接在根目錄後面變成 `$1//abs/path`，一樣是靜靜零命中 |
| 「具現化」不是無害的複製：要判型、設上限，並在之後重跑憑證剝除 | 無條件 `cp -RL` 會跟著巢狀連結複製任意私有資料、卡在裝置檔上，或把憑證帶到樣式認不得的新路徑 |
| 文字型設定會帶 CRLF，`read -r` 只吃掉 `\n` | 尾巴留下的 CR 讓刪除與驗證一起零命中並回報成功 |
| 清理本身也要驗證，而且失敗要大聲說 | `rm -rf` 可能因權限或不可變旗標部分失敗，而每一條離開路徑都會安靜地留下憑證副本 |
| 教學用的例子也算公開資訊：示範語法不需要真實的憑證路徑 | 移除檔名之後，又用一個真實路徑當 `-path` 的例子放了回去 |
| 量測結論可以寫，機器清單不行 | 「外部連結有幾條、長什麼樣」是本機盤點；可重用的是「這類連結存在，所以一律拒絕不可行，動手前自己量」 |
| 破壞性的取代要「先建好再換掉」，不是「先刪掉再建」 | 中途失敗會留下沒有外部連結、但內容被截斷的語料，而守衛正好只檢查外部連結 |
| 「必須相等」要寫成比較，不能寫成散文再用 `&&` 串起來 | 每個 arm 的狀態是 binary 的結束碼；三個 digest 全不同也照樣整串成功，而且基準不穩時還會繼續跑 NEW |
| 配對量測裡任一 arm 失敗就整對作廢 | 只讓失敗那次不列印，留下的落單觀測會偏移比較 |
| 還原失敗時不要刪掉唯一的副本 | trap 原本無條件 `cleanup`，還原一失敗就同時失去備份與乾淨的原始碼 |
| 「顯然更安全」的替代做法一樣要跑過才能寫進 runbook | 只複製兩個掃描器 root 的版本掉了一則訊息、換掉了 digest |
| 跨平台 fixture 不要預測路徑拼法，去問會做查詢的那一方 | 快取 key 用 `TempDir::join` 種下，Windows 上與 scanner 的拼法不符，每一個本該命中的檔案都靜默退化成重新解析；連兩次紅燈才改成經 `scanner_spelling` 取得拼法 |
| 一個失敗有多種成因時，讓測試自己講是哪一種 | 「發生了 fresh parse」同時代表 key 查不到與 fingerprint 不符，分辨它們燒掉兩次 CI；補上分項斷言後下一次紅燈會自己指認 |
| 「通過」不等於「能失敗」——加測試的速度容易快過驗證測試能失敗的速度 | 本輪兩次寫出不可能失敗的斷言，都只有靠變異跑才發現 |
| `git diff main <branch>` 會把 **main 自己的前進**顯示成分支的刪除 | 差點據此誤報一條分支倒退了引擎 pin |
| 用錯的量尺會讓真實的成長看起來不合理 | turns +669 vs 位元組 +960 MB |
| 把診斷資訊丟掉會讓失敗看起來像成功 | `2>/dev/null` 讓被殺掉的程序看起來像「跑完沒輸出」；`tail -5` 截掉了最大的測試 binary |
| 長時間程序要由主 session 持有 | 背景任務裡再 `&` 一層，外層一結束整個程序群被收掉 |
