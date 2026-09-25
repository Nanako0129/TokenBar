// Every visible string on the landing page, in both locales. Values are
// copied verbatim from `.agent-local/syrtis/landing/copy3.json` (source of
// truth for wording) into this structurally-aligned shape; components take a
// `locale` prop (default 'en') and read their slice via t(locale).
//
// `meta.title`/`meta.description` have no copy3 equivalent (copy3 has no
// `meta.*` keys) — they are built by combining copy3 strings verbatim
// (hero.h1 + "Syrtis" for the title; hero.lede as the description) rather
// than inventing new wording, following the English title/description
// already present in the prototype's own <title>/<meta description>.

export type Locale = 'en' | 'zh-tw'

export const LENS_ORDER = ['overview', 'quota', 'models', 'monthly', 'daily', 'hourly', 'stats', 'agents'] as const
export type LensId = (typeof LENS_ORDER)[number]

// The five `.stratum` sections' depth-ruler readings (10^n tokens), in
// document order — dashboard, limits, resets, privacy, name. Not locale
// text (same figures on both pages), so it lives outside the dictionary;
// the unit and stratum name next to each figure are. `exp` renders inside
// a real <sup>, not a Unicode superscript character — the prototype
// dropped ¹⁰'s mixed-height ¹ and ⁰ for the same reason (2026-09-26).
export const DEPTH_READINGS = [
  { base: '10', exp: '3' },
  { base: '10', exp: '6' },
  { base: '10', exp: '9' },
  { base: '10', exp: '10' },
  { base: '3.86×10', exp: '10' },
] as const

const en = {
  meta: {
    title: 'Syrtis — Where your tokens settle',
    description:
      'A free, open-source menu-bar app for macOS, with a tray version for Windows, that shows what your AI coding tools cost.',
    ogLocale: 'en_US',
    htmlLang: 'en',
  },
  nav: {
    dash: 'Dashboard',
    limits: 'Limits',
    resets: 'Resets',
    privacy: 'Privacy',
    install: 'Install',
  },
  hero: {
    gloss: 'From σύρω, to drag: sand carried along until it settles into shoals.',
    h1: 'Where your tokens settle.',
    lede: 'Syrtis is a free, open-source macOS menu-bar app, with a tray version for Windows, that reads the session logs already on your computer to show what your AI coding tools cost. It tallies tokens and spend across 25+ agents on-device, with no account, no telemetry, and no cloud sync.',
    works: 'Reads session logs from 25+ tools, including Claude Code, Codex, Cursor, Gemini CLI, OpenCode, Copilot, Kiro, and Antigravity.',
  },
  dashboard: {
    h2: 'Tokens settling into shape',
    lede: 'A single prompt barely registers on a bill, but thousands of small completions accumulate over weeks of work. Syrtis scans the local logs left by your coding agents to reveal what has settled.',
    figcaption: 'A year of token usage rendered as an orbitable 3D terrain, stacked by model or agent in tokens or dollars.',
    explorerH3: 'The dashboard, one lens at a time',
  },
  lens: {
    overview: 'Contribution chart, agent limits with pace, live session activity, model breakdown, and streaks.',
    quota: 'For each subscription, the active quota window, earlier windows with allowance consumed and cost, and a weekday-by-hour usage grid.',
    models: 'Every model ranked by cost, with an In · Out · CR · CW token split.',
    monthly: 'Active months of activity, each opening into per-model spend.',
    daily: 'Active days, each opening into an itemized breakdown of per-model spend.',
    hourly: 'A 24-hour distribution showing when in the day tokens get spent.',
    stats: 'Total spend, active day counts, streaks, favorite model, and highest-volume day.',
    agents: 'Sub-agents ranked by cost, with source applications and message counts.',
  },
  lensName: {
    overview: 'Overview',
    quota: 'Quota',
    models: 'Models',
    monthly: 'Monthly',
    daily: 'Daily',
    hourly: 'Hourly',
    stats: 'Stats',
    agents: 'Agents',
  },
  limits: {
    h2: 'Shoals beneath calm water',
    lede: 'In Acts 27, sailors caught in a storm moved first to avoid running aground on the hidden shoals of Syrtis. Provider usage limits pose the same hazard: invisible beneath normal work until requests abruptly stop.',
    body: 'The menu-bar item keeps that limit visible as signal bars, a ring, or a popsicle that melts as the quota drains. The gauge shifts to amber under 25% remaining and red under 10%. Limit cards show whether consumption is ahead of or behind pace, projecting when you will run out at this rate.',
    glassNote: 'On macOS 27, the dashboard opens over the wallpaper as a Liquid Glass panel.',
    figcaption: 'The menu-bar item at actual size.',
    gaugeOk: '25% or more',
    gaugeWarn: 'under 25%',
    gaugeLow: 'under 10%',
  },
  resets: {
    h2: 'Measuring the day',
    quote: "In 1659, Christiaan Huygens sketched a recurring dark patch on Mars, tracking its daily return to determine that Mars rotates in roughly 24 hours. That patch is now called Syrtis Major: the first surface feature recorded on another planet, and the first used to measure a planet's day.",
    cite: 'Christiaan Huygens, Mars observations, 1659',
    lede: 'AI allowances run on windows of their own: a few hours for a live session, and weekly resets for larger pools. Syrtis counts down to each reset, preserves the history and cost of past windows, and maps out which hours of the week the allowance goes.',
    figcaption: 'The Quota lens: past windows and weekly usage patterns.',
  },
  privacy: {
    h2: 'Contained on your machine',
    p1t: 'On-device parsing',
    p1b: 'Syrtis extracts tokens and costs directly from the log files your tools already store on disk, powered by the vendored tokscale-core library. No session data ever leaves your computer.',
    p2t: 'Zero accounts, zero telemetry',
    p2b: 'There are no accounts, no analytics beacons, and no tracking IDs. Syrtis operates without a cloud backend and performs no synchronization.',
    p3t: 'Explicit network boundaries',
    p3b: 'Outbound network calls are limited to the signed update feed, public pricing tables, and vendor quota endpoints, each contacted only when your own credentials for that vendor are present locally, such as Claude, Codex, Copilot, or Grok. Each endpoint receives only its own credentials; nothing is sent anywhere else.',
  },
  name: {
    h2: 'Previously TokenBar',
    body: 'Until version 2.0, the macOS app was called TokenBar. It is now named after the ancient Greek shoals formed by settling currents, and the Martian landmark that measured a day. Upgrading to v2.0 preserves your settings, history, and login item unchanged.',
    link: 'Why Syrtis: an essay on the name',
  },
  credit: {
    tokscale: 'by Junho Yeo — its tokscale-core parses, deduplicates, and prices usage; its terminal interface served as the model for the lenses.',
    tokcat: 'by handlecusion — where the product line began, and the source of the spinning oiiai cat in the menu bar.',
    RunCat: 'by Takuto Nakamura — the original menu-bar pet that runs faster the busier you are.',
    CodexBar: 'by Peter Steinberger — the reference for how quota pace is shown.',
  },
  install: {
    h2: 'Install Syrtis',
    lede: 'Pick your platform. Both versions are free and MIT-licensed, sharing the same Rust parsing core for on-device logs.',
    macNote: 'Requires Apple Silicon and macOS 14 or later. The Homebrew tap command is provisional until v2.0 ships. The app is not notarized, so install it through Homebrew: the cask clears the quarantine flag that would otherwise block it.',
    winX64: 'Download for x64',
    winArm64: 'Download for ARM64',
    winNote: 'The installers bundle the .NET runtime and update themselves. Or install via Scoop:',
    copy: 'Copy',
    copied: 'Copied',
    selected: 'Selected. Press Ctrl+C or ⌘C to copy.',
  },
  footer: {
    text: 'Syrtis is free and open-source software released under the MIT license.',
    platforms: 'macOS 14+ on Apple Silicon · Windows on x64 and ARM64',
    mac: 'Source (macOS)',
    win: 'Source (Windows)',
    essay: 'Why Syrtis',
  },
  depth: {
    unit: 'tokens',
    1: 'stratum I',
    2: 'stratum II',
    3: 'stratum III',
    4: 'bedrock',
    5: 'surfaced',
  },
  pending: 'Screenshot pending: to be captured from the v2.0 build.',
  counter: (n: string) => `≈ ${n} tokens settled · 1 grain = 1,000`,
}

const zhTw: typeof en = {
  meta: {
    title: '積沙成洲 — Syrtis',
    description:
      'Syrtis 是一款免費開源的 macOS 選單列軟體，也有 Windows 系統匣版本，直接讀取電腦上既有的 AI 寫程式工具紀錄，計算累積的 token 與花費。在本機解析超過 25 個 agent 的使用量，不需註冊帳號、不收集遙測資料，亦不同步至雲端。',
    ogLocale: 'zh_TW',
    htmlLang: 'zh-Hant-TW',
  },
  nav: {
    dash: '儀表板',
    limits: '額度',
    resets: '重設',
    privacy: '隱私',
    install: '安裝',
  },
  hero: {
    gloss: '源自 σύρω（拖曳）：潮水挾帶細沙前行，水緩處沉積成洲。',
    h1: '積沙成洲',
    lede: 'Syrtis 是一款免費開源的 macOS 選單列軟體，也有 Windows 系統匣版本，直接讀取電腦上既有的 AI 寫程式工具紀錄，計算累積的 token 與花費。在本機解析超過 25 個 agent 的使用量，不需註冊帳號、不收集遙測資料，亦不同步至雲端。',
    works: '支援 25 款以上工具的紀錄讀取，涵蓋 Claude Code、Codex、Cursor、Gemini CLI、OpenCode、Copilot、Kiro 與 Antigravity。',
  },
  dashboard: {
    h2: '細沙沉積成形',
    lede: '單次呼叫模型的 token 微小得難以察覺，但數週積累下來，終究會顯出輪廓。Syrtis 梳理 Mac 上既有的歷程檔案，清楚呈現這些零散呼叫沉積出的總量與花費。',
    figcaption: '將一整年的使用量渲染為可自由旋轉視角的 3D 地形，能按模型或 agent 堆疊，切換檢視 token 數或花費金額。',
    explorerH3: '儀表板的八種視圖',
  },
  lens: {
    overview: '貢獻度圖表、agent 額度與步調推算、目前的 session、模型佔比與連續使用天數。',
    quota: '各訂閱項目的當前配額時間窗、過往週期的額度消耗與花費，以及一週各時段的使用分布網格。',
    models: '所有模型依花費排序，並清楚拆解 In、Out、CR 與 CW 用量。',
    monthly: '活躍月份總覽，逐月展開檢視各模型的花費明細。',
    daily: '每日使用紀錄，逐日展開各模型的消耗與支出。',
    hourly: '統整一天 24 小時中 token 集中消耗的時段分布。',
    stats: '總支出、活躍天數、連續紀錄、最常用的模型，以及單日最高花費。',
    agents: '依花費排序子 agent，標示其來源應用程式與訊息數量。',
  },
  lensName: {
    overview: '總覽',
    quota: '額度',
    models: '模型',
    monthly: '每月',
    daily: '每日',
    hourly: '每小時',
    stats: '統計',
    agents: 'Agent',
  },
  limits: {
    h2: '平靜水面下的暗沙',
    lede: '《使徒行傳》第二十七章記載，水手在暴風雨中最先採取的行動，便是竭力避免船隻擱淺在隱沒難辨的 Syrtis 暗沙上。AI 服務商的用量限制亦是如此：平時沉在日常工作底層毫無跡象，直到請求中斷受阻時才猝不及防。',
    body: 'Syrtis 將剩餘配額常駐於選單列，可顯示為訊號條、環形進度環，或隨消耗消融的冰棒圖示。剩餘額度低於 25% 時指標轉為琥珀色，低於 10% 則變為紅色。額度卡會分析當前使用節奏是否超前或落後，並推算照此速度用盡額度的確切時間。',
    glassNote: '在 macOS 27 上，儀表板會以 Liquid Glass 面板懸浮展開於桌面桌布之上。',
    figcaption: '選單列圖示，以實際大小呈現。',
    gaugeOk: '25% 以上',
    gaugeWarn: '低於 25%',
    gaugeLow: '低於 10%',
  },
  resets: {
    h2: '測量行星的一天',
    quote: '1659 年，克里斯蒂安．惠更斯在火星表面描摹出一塊暗色斑塊，見其每日重返相同位置，由此推導出火星自轉一週約為 24 小時。那塊斑塊如今被稱為 Syrtis Major，是人類在另一顆行星上記錄的第一個地表特徵，也是第一個用來度量行星晝夜的基準。',
    cite: '克里斯蒂安．惠更斯，火星觀測紀錄，1659 年',
    lede: '各家配額各有其週期節奏：短則數小時的單次對話視窗，長則每週重設的完整額度。Syrtis 能倒數每次重設時間，完整留存過往週期的紀錄與花費，並標示出一週當中額度消耗最密集的鐘點。',
    figcaption: '額度視圖：過往時間窗紀錄，以及額度在一週之中的消耗分布。',
  },
  privacy: {
    h2: '純本機運作',
    p1t: '本機直接解析',
    p1b: '透過內嵌的 tokscale-core 程式庫，Syrtis 直接讀取本機已有的對話紀錄以計算 token 與花費。解析完全在 Mac 上執行，歷史檔案絕不上傳。',
    p2t: '無帳號、零遙測',
    p2b: '無需註冊帳號，不設使用者身分追蹤，也不收集任何遙測資料。軟體沒有雲端後端，不同步任何資料。',
    p3t: '連線請求完全透明',
    p3b: '僅在必要時發起特定請求：簽署過的更新檢查、公開模型定價資料，以及僅在電腦上已有你自己的服務商憑證時（例如 Claude、Codex、Copilot 與 Grok），查詢該服務商的配額端點。每個端點只會收到它自己的憑證，除此之外不傳送至任何地方。',
  },
  name: {
    h2: '原名 TokenBar',
    body: 'macOS 版在 2.0 版本前名為 TokenBar。更名為 Syrtis，典故取自水流力竭處沉積成洲的暗沙，以及首度記錄行星自轉的火星地貌。升級至 2.0 版時，你原有的偏好設定、歷史紀錄與開機啟動項目皆會原樣保留。',
    link: '〈為什麼叫 Syrtis：關於命名的文章〉',
  },
  credit: {
    tokscale: '（作者 Junho Yeo）──其 tokscale-core 負責解析、去除重複紀錄與用量計價，其終端機介面亦是各個檢視模式的原型。',
    tokcat: '（作者 handlecusion）──本專案產品線的起點，也是選單列上旋轉 oiiai 貓咪的由來。',
    RunCat: '（作者 Takuto Nakamura）──原創的選單列寵物軟體，系統越忙碌時奔跑得越快。',
    CodexBar: '（作者 Peter Steinberger）──配額消耗速度計算的參考實作。',
  },
  install: {
    h2: '安裝 Syrtis',
    lede: '選擇你的作業系統平台。兩個版本皆為 MIT 授權的免費開源軟體，共用同一套 Rust 解析核心讀取本機紀錄。',
    macNote: '需使用 Apple Silicon Mac，系統版本為 macOS 14 以上。Homebrew 安裝指令在 2.0 正式發行前為過渡版本。軟體尚未經過 Apple 公證，請透過 Homebrew 安裝，cask 會清除原本會擋下它的隔離標記。',
    winX64: '下載 x64 版',
    winArm64: '下載 ARM64 版',
    winNote: '安裝程式內建 .NET 執行環境且具備自動更新功能，亦可使用 Scoop 安裝：',
    copy: '複製',
    copied: '已複製',
    selected: '已選取，請按 Ctrl+C 或 ⌘C 複製。',
  },
  footer: {
    text: 'Syrtis 為採用 MIT 授權條款釋出的自由與開源軟體。',
    platforms: 'macOS 14 以上（Apple Silicon）・Windows（x64 與 ARM64）',
    mac: '原始碼（macOS）',
    win: '原始碼（Windows）',
    essay: '命名由來',
  },
  depth: {
    unit: 'token',
    1: '第一層',
    2: '第二層',
    3: '第三層',
    4: '基岩',
    5: '出露',
  },
  pending: '截圖待補：將取自 2.0 正式版本。',
  counter: (n: string) => `約 ${n} token 沉積・1 粒 = 1,000`,
}

const dict: Record<Locale, typeof en> = { en, 'zh-tw': zhTw }

export const t = (locale: Locale) => dict[locale]
