// Every visible string on the landing page, in both locales. Components take
// a `locale` prop (default 'en') and read their slice via t(locale). Fields
// that carry inline markup (<br/>, <strong>, links) are rendered with
// set:html — keep them HTML-safe when editing.

export type Locale = 'en' | 'zh-tw'

const en = {
  meta: {
    title: 'Syrtis — track Claude Code & Codex token usage from the macOS menu bar',
    description:
      'Free, open-source menu-bar app that reads local logs to track AI coding spend across 25+ agents — Claude Code, Codex, Cursor, OpenCode and more. Live throughput, quota gauges, 3D usage graph. Native Swift, Liquid Glass, zero telemetry.',
    ogLocale: 'en_US',
    htmlLang: 'en',
  },
  menubar: {
    items: [
      { href: '#native', label: 'About' },
      { href: '#views', label: 'Views' },
      { href: '#quota', label: 'Menu Bar' },
      { href: '#privacy', label: 'Privacy' },
      { href: '#install', label: 'Install' },
    ],
    statusTitle: 'Syrtis lives right here',
  },
  hero: {
    eyebrow: 'Local-first · Native macOS menu bar',
    h1: 'Know every AI<br />token you <span class="hot">burn</span>.',
    lede: 'Syrtis reads your AI coding logs on-device and lays your spend bare across <strong>25+ agents</strong> — Claude Code, Codex, Cursor, OpenCode and more. Native Swift, dressed in Liquid Glass. No telemetry, no account. Just the numbers.',
    copy: 'copy',
    copied: 'copied',
    github: 'View on GitHub',
    statline: '25+ agents · 7 lenses · 160 fps 3D · 0 telemetry',
    popAlt: 'The Syrtis popover: token usage dashboard with agent limits and live pace',
  },
  native: {
    eyebrow: 'Native Swift · Liquid Glass',
    h2: 'Rebuilt in Swift.<br />Dressed in Liquid Glass.',
    intro:
      'This <em>is</em> Syrtis — since v1.0.0 the app is a ground-up native rewrite. Same dashboard, same numbers as the original, now with the system’s own glass, springs, and speed.',
    shotAlt: 'The native Syrtis popover rendered in Liquid Glass over the desktop wallpaper',
    points: [
      {
        title: '100% native Swift shell',
        body: 'SwiftUI + AppKit around a Rust core — no webview. The popover opens instantly and feels like part of the OS, because it is.',
      },
      {
        title: 'Liquid Glass, for real',
        body: 'On macOS 26 the dashboard renders as Liquid Glass cards floating over your wallpaper — the real system material, not a CSS imitation. Earlier systems get a vibrancy fallback.',
      },
      {
        title: 'SceneKit 3D at 160 fps',
        body: 'The year-at-a-glance contribution terrain is a real 3D scene you orbit and zoom — camera position remembered across opens.',
      },
      {
        title: 'Sparkle updates, AI-written notes',
        body: 'In-app updates arrive over a signed Sparkle feed, with release notes summarized from the actual commits. Beta channel is one Settings toggle away.',
      },
    ],
  },
  cat: {
    eyebrow: 'A gauge, not a mascot',
    h2: 'The cat spins faster<br />the more you burn.',
    body: "Your token throughput as a single glanceable critter: idle when nothing's flowing, whirling when Claude Code is mid-refactor. One look at the menu bar tells you how fast the meter is running — no window required.",
    credit:
      'The menu-bar pet, a critter that animates faster the harder you work it, is the invention of <a href="https://kyome.io/runcat/">RunCat</a> by <strong>Takuto Nakamura</strong>, and Party Parrot comes from there too. <a href="https://github.com/handlecusion/tokcat">tokcat</a> by <strong>handlecusion</strong> brought the idea to token tracking and made the oiiai spinning cat its signature. Syrtis began as a tokcat fork and reuses both, kept with gratitude.',
    crittersNote: 'oiiai cat from tokcat, Party Parrot from RunCat. Hover to feed them tokens.',
    gifAlt: "The oiiai cat spinning in the menu bar beside today's cost",
  },
  quota: {
    eyebrow: 'The menu bar is the dashboard',
    h2: 'Your quota,<br />melting in real time.',
    body: "The status item can gauge what's left of your subscription window — as signal bars, a ring, or a popsicle that melts as your five-hour window drains. Battery-style colors kick in when it matters: amber under 25%, red under 10%. Right-click picks which subscription it tracks.",
    note: 'drawn live on a 16px grid · same geometry as the app',
    gaugeBars: 'signal bars',
    gaugePop: 'melting popsicle',
    gaugeRing: 'ring gauge',
    legendOk: 'on pace',
    legendWarn: '< 25% left',
    legendLow: '< 10% left',
  },
  views: {
    eyebrow: 'One popover, seven lenses',
    h2: 'App tabs filter who. The view switch picks how.',
    intro:
      'Pick which agents you’re looking at, then choose how to break them down — a multi-view dashboard modeled on <a class="ilink" href="https://github.com/junhoyeo/tokscale">tokscale</a>’s TUI, lenses and columns and all.',
    altPrefix: (name: string) => `Syrtis ${name} view`,
    items: [
      { name: 'Overview', desc: 'The whole picture — contribution chart, agent limits with pace, live session, model breakdown, streaks.' },
      { name: 'Models', desc: 'Every model ranked by cost, with its share and a dim In · Out · CR · CW split.' },
      { name: 'Monthly', desc: 'Active months newest-first — select one to drill into that month’s per-model spend.' },
      { name: 'Daily', desc: 'Active days newest-first — select one to drill into that day’s per-model spend.' },
      { name: 'Hourly', desc: 'A 24-hour-of-day rhythm: when in the day your tokens actually get spent.' },
      { name: 'Stats', desc: 'Headline summary — total spend, active days, streaks, favorite model, best day.' },
      { name: 'Agents', desc: 'Sub-agents ranked by cost, with their source apps, message count, and tokens.' },
    ],
    wideTitle: '3D contribution graph',
    wideDesc:
      'The same year of usage as an interactive GitHub-style tile terrain — orbit it, and the stacked 2D bars are one toggle away. Tokens stack by <strong>model</strong> (provider shades) or by <strong>agent</strong> (brand colors), in tokens or dollars.',
    wideAlt: 'Syrtis interactive 3D contribution graph',
    lightboxLabel: 'Screenshot preview',
    lightboxClose: 'Close preview',
  },
  privacy: {
    eyebrow: 'Local-first by construction',
    h2: 'Your spend stays on your Mac.',
    intro: 'Syrtis is built so the honest answer to “where does my data go?” is: nowhere.',
    pillars: [
      {
        title: 'Reads on-device',
        body: 'Usage comes from the session logs already on your disk. Nothing is uploaded, nothing is mined — the parse happens locally via the vendored tokscale-core.',
      },
      {
        title: 'No telemetry, no account',
        body: 'No analytics, no sign-up, no cloud sync. Syrtis never phones home with what you do or what you spend.',
      },
      {
        title: 'Every request, disclosed',
        body: 'Network access: the signed updater feed, public model-pricing data, and — only when you have local Codex/Claude OAuth credentials — direct vendor quota lookups for the limit cards. No third parties, ever.',
      },
    ],
  },
  install: {
    eyebrow: 'Two minutes, Apple Silicon',
    h2: 'One command. The cat moves in.',
    intro:
      'Syrtis installs through Homebrew for <strong>Apple Silicon Macs</strong> on macOS 14+. The fully-qualified cask auto-taps, so there is no separate <code>brew tap</code> step.',
    pill: 'Native Swift · macOS 14+',
    desc: 'The shipping app. In-app updates arrive over a signed Sparkle feed and are verified before install; beta builds are an in-app <strong>Settings toggle</strong>, not a separate cask.',
    copyBtn: 'Copy command',
    copied: 'Copied ✓',
    legacy:
      'Still on an older Mac? <code>tokenbar@legacy</code> pins the final Tauri build (v0.4.5, macOS 11+) — the <a href="https://github.com/Nanako0129/TokenBar-Tauri">legacy repo</a> is archived but the cask stays. Building from source? <a href="https://github.com/Nanako0129/syrtis">Swift 6 + Rust</a>, Command Line Tools are enough.',
  },
  faq: {
    eyebrow: 'Questions',
    h2: 'Good to know.',
    intro: 'The short answers most people want before installing.',
    items: [
      {
        q: 'What exactly is Syrtis?',
        a: 'A free, open-source native macOS menu-bar app that reads your local AI coding session logs and shows what you’re spending — across a contribution graph, per-model and per-agent breakdowns, monthly, daily, and hourly views, and live throughput. No CLI to run, no account.',
      },
      {
        q: 'Which AI coding tools does it track?',
        a: '25+ agents from their local logs — Claude Code, Codex, Cursor, OpenCode, Gemini CLI, Copilot, Amp, Droid, Hermes, Goose, Kilo/KiloCode, Roo Code, Qwen, Kimi, Crush, Zed, Kiro, Trae, Warp and more. Parsing is handled by the vendored tokscale-core, so coverage tracks tokscale.',
      },
      {
        q: 'Does it cost anything?',
        a: 'No. Syrtis is MIT-licensed and free — no subscription, no paid tier, no telemetry.',
      },
      {
        q: 'What happened to the original (Tauri) app?',
        a: 'Syrtis began as a Tauri fork of tokcat. With v1.0.0 (June 2026) it was replaced by a ground-up Swift rewrite around the same Rust parsing core — the `tokenbar` cask now installs the native app, and the Tauri repository is archived. A `tokenbar@legacy` cask pins the final Tauri build (v0.4.5) for macOS 11+.',
      },
      {
        q: 'How do updates work?',
        a: 'In-app, over a signed Sparkle feed — each release is signature-verified before install, with release notes summarized from the actual commits. Want beta builds? It’s a toggle in Settings, not a separate install.',
      },
      {
        q: 'Where does my data go?',
        a: 'Nowhere. Usage history is read locally from disk. Network requests are limited to the signed updater feed, public model-pricing data, and optional Codex/Claude quota lookups using the OAuth credentials already on your machine — that’s what powers the menu-bar quota gauge and the agent limit cards.',
      },
      {
        q: 'Intel Mac or Windows?',
        a: 'Syrtis targets Apple Silicon (arm64) on macOS 14+; Liquid Glass needs macOS 26, earlier systems get a vibrancy fallback. The legacy Tauri build covers macOS 11+. There’s no Intel, Windows, or Linux build.',
      },
      {
        q: 'How do I uninstall it?',
        a: 'brew uninstall --cask tokenbar.',
      },
    ],
  },
  credits: {
    eyebrow: 'Standing on shoulders',
    h2: 'Built on great open source.',
    intro: 'Syrtis wouldn’t exist without these projects — thank you to their maintainers.',
    go: 'View on GitHub ↗',
    items: [
      {
        name: 'tokscale',
        who: 'by Junho Yeo',
        href: 'https://github.com/junhoyeo/tokscale',
        body: 'The foundation. Its vendored tokscale-core crate drives Syrtis’s session parsing, dedup, and pricing across 25+ agents — and its interactive TUI is the blueprint for the whole dashboard: the seven lenses and their In · Out · CR · CW column breakdown.',
        accent: '--p-deepseek',
      },
      {
        name: 'tokcat',
        who: 'by handlecusion',
        href: 'https://github.com/handlecusion/tokcat',
        body: 'Where Syrtis’s product line began — the original Tauri menu-bar monitor (itself built on tokscale). The native app is a ground-up Swift rewrite that carries no tokcat code, but the menu-bar pet form and the oiiai spinning-cat signature are theirs.',
        accent: '--p-anthropic',
      },
      {
        name: 'RunCat',
        who: 'by Takuto Nakamura',
        href: 'https://kyome.io/runcat/',
        body: 'The original menu-bar pet, the creature that runs faster the busier you are. Party Parrot, and every critter that has sprinted across a Mac menu bar (tokcat’s and Syrtis’s included), traces back here.',
        accent: '--p-cursor',
      },
      {
        name: 'CodexBar',
        who: 'by Peter Steinberger',
        href: 'https://github.com/steipete/CodexBar',
        body: 'Syrtis’s quota-pace presentation — ahead of or behind your window, projected run-dry time — references CodexBar’s approach.',
        accent: '--p-openai',
      },
    ],
  },
  footer: {
    tagline: 'AI token usage monitor for the macOS menu bar. MIT licensed.',
    stamp: 'macOS 14+ · Apple Silicon · Swift + Rust · Liquid Glass · MIT',
    copyright: '© Syrtis contributors',
    langSwitch: { href: '/zh-tw/', label: '繁體中文' },
  },
}

const zhTw: typeof en = {
  meta: {
    title: 'Syrtis｜在 macOS 選單列追蹤 Claude Code／Codex 的 token 用量',
    description:
      '免費開源的選單列 App，直接讀本機紀錄追蹤 25+ 個 AI 編碼 agent 的花費——Claude Code、Codex、Cursor、OpenCode 等。即時吞吐、額度儀錶、3D 用量圖。原生 Swift、Liquid Glass、零遙測。',
    ogLocale: 'zh_TW',
    htmlLang: 'zh-Hant-TW',
  },
  menubar: {
    items: [
      { href: '#native', label: '關於' },
      { href: '#views', label: '視圖' },
      { href: '#quota', label: '選單列' },
      { href: '#privacy', label: '隱私' },
      { href: '#install', label: '安裝' },
    ],
    statusTitle: 'Syrtis 就住在這裡',
  },
  hero: {
    eyebrow: 'Local-first · 原生 macOS 選單列',
    h1: '每顆 AI token<br />怎麼<span class="hot">燒</span>的，都知道。',
    lede: 'Syrtis 在你的 Mac 上直接讀取 AI 編碼工具的本機紀錄，攤開 <strong>25+ 個 agent</strong> 的花費——Claude Code、Codex、Cursor、OpenCode 等等。原生 Swift、披上 Liquid Glass。零遙測、免帳號，只給你數字。',
    copy: '複製',
    copied: '已複製',
    github: '在 GitHub 上看',
    statline: '25+ agents · 7 種視圖 · 160 fps 3D · 0 遙測',
    popAlt: 'Syrtis popover：token 用量儀表板，含 agent 額度與即時進度',
  },
  native: {
    eyebrow: 'Native Swift · Liquid Glass',
    h2: '用 Swift 重寫。<br />披上 Liquid Glass。',
    intro:
      '這<em>就是</em>現在的 Syrtis——自 v1.0.0 起，整個 app 都是從零重寫的原生程式。同一套儀表板、同樣的數字，多了系統原生的玻璃、彈簧動畫與速度。',
    shotAlt: '原生 Syrtis popover 以 Liquid Glass 浮在桌布上',
    points: [
      {
        title: '100% 原生 Swift 外殼',
        body: 'SwiftUI + AppKit 包著 Rust 核心——沒有 webview。popover 一點即開，感覺像系統的一部分，因為它就是。',
      },
      {
        title: '真正的 Liquid Glass',
        body: '在 macOS 26 上，儀表板是漂在桌布上的 Liquid Glass 玻璃卡——系統原生的真材質，不是 CSS 仿的。較舊的系統自動退回 vibrancy。',
      },
      {
        title: 'SceneKit 3D，160 fps',
        body: '年度貢獻地形是貨真價實的 3D 場景，可旋轉、可縮放——鏡頭位置每次開啟都記得。',
      },
      {
        title: 'Sparkle 更新＋AI 寫的更新說明',
        body: 'App 內更新走簽章驗證的 Sparkle feed，更新說明由實際 commit 摘要而成。Beta 通道在設定裡一鍵切換。',
      },
    ],
  },
  cat: {
    eyebrow: '儀錶，不是吉祥物',
    h2: '燒得越快，<br />貓轉得越快。',
    body: '你的 token 吞吐量濃縮成選單列上一隻一眼可讀的小生物：沒事時發呆，Claude Code 重構到一半時狂轉。瞄一眼選單列就知道錶轉多快——不用開任何視窗。',
    credit:
      '選單列養寵物（動得多快反映你操得多兇）這個設計，源自 <strong>Takuto Nakamura</strong> 的 <a href="https://kyome.io/runcat/">RunCat</a>，party parrot 也是從這裡來的。<a href="https://github.com/handlecusion/tokcat">tokcat</a>（<strong>handlecusion</strong>）把它帶進 token 用量監視，並以 oiiai 旋轉貓當招牌。Syrtis 最初是 tokcat 的 fork，oiiai 貓與 party parrot 都心懷感激地沿用。',
    crittersNote: 'oiiai 旋轉貓來自 tokcat，party parrot 來自 RunCat。滑過去餵牠們一點 token。',
    gifAlt: 'oiiai 旋轉貓在選單列今日花費旁打轉',
  },
  quota: {
    eyebrow: '選單列就是儀表板',
    h2: '你的額度，<br />即時融化中。',
    body: 'Status item 可以顯示訂閱額度視窗還剩多少——訊號格、圓環，或一支隨五小時視窗流逝慢慢融化的冰棒。電池式警示色在要緊時亮起：剩 25% 以下轉琥珀、10% 以下轉紅。右鍵選擇要追蹤哪個訂閱。',
    note: '16px 網格即時繪製 · 與 app 同一套幾何',
    gaugeBars: '訊號格',
    gaugePop: '融化冰棒',
    gaugeRing: '圓環',
    legendOk: '進度正常',
    legendWarn: '剩不到 25%',
    legendLow: '剩不到 10%',
  },
  views: {
    eyebrow: '一個 popover，七種透鏡',
    h2: 'App 分頁選「看誰」，視圖切換選「怎麼看」。',
    intro:
      '先挑要看的 agent，再選拆解方式——多視圖儀表板的版面，連透鏡與欄位都照著 <a class="ilink" href="https://github.com/junhoyeo/tokscale">tokscale</a> 的 TUI 來設計。',
    altPrefix: (name: string) => `Syrtis ${name} 視圖`,
    items: [
      { name: 'Overview', desc: '全貌——貢獻圖、agent 額度與進度、即時 session、模型佔比、連續紀錄。' },
      { name: 'Models', desc: '每個模型按花費排名，附佔比與 In · Out · CR · CW 細目。' },
      { name: 'Monthly', desc: '活躍月份由新到舊排列——點選一個月即可下鑽該月各模型的花費。' },
      { name: 'Daily', desc: '活躍日由新到舊排列——點選一天即可下鑽當日各模型的花費。' },
      { name: 'Hourly', desc: '24 小時節奏圖：你的 token 實際都燒在一天的哪些時段。' },
      { name: 'Stats', desc: '重點摘要——總花費、活躍天數、連續紀錄、最愛模型、最高一日。' },
      { name: 'Agents', desc: '子 agent 按花費排名，附來源 app、訊息數與 token 量。' },
    ],
    wideTitle: '3D 貢獻圖',
    wideDesc:
      '同一年的用量化成可互動的 GitHub 式方塊地形——旋轉它，2D 堆疊長條一鍵切換。Token 可按<strong>模型</strong>（供應商色階）或按 <strong>agent</strong>（品牌色）堆疊，單位可選 token 或美元。',
    wideAlt: 'Syrtis 可互動的 3D 貢獻圖',
    lightboxLabel: '截圖預覽',
    lightboxClose: '關閉預覽',
  },
  privacy: {
    eyebrow: '骨子裡 local-first',
    h2: '你的花費，留在你的 Mac 上。',
    intro: 'Syrtis 的設計，讓「我的資料去了哪？」可以誠實回答：哪都沒去。',
    pillars: [
      {
        title: '本機讀取',
        body: '用量來自磁碟上既有的 session 紀錄。不上傳、不挖掘——解析由 vendored tokscale-core 在本機完成。',
      },
      {
        title: '零遙測、免帳號',
        body: '沒有分析、不用註冊、沒有雲端同步。Syrtis 不會回報你做了什麼、花了多少。',
      },
      {
        title: '每個請求，攤開講',
        body: '網路存取僅限：簽章的更新 feed、公開的模型價格資料，以及——僅當本機已有 Codex/Claude OAuth 憑證時——直連官方的額度查詢（供額度卡使用）。永遠沒有第三方。',
      },
    ],
  },
  install: {
    eyebrow: '兩分鐘，Apple Silicon',
    h2: '一行指令，貓就搬進來。',
    intro:
      'Syrtis 透過 Homebrew 安裝，需要 <strong>Apple Silicon Mac</strong> 與 macOS 14+。cask 全名會自動 tap，不用另外 <code>brew tap</code>。',
    pill: 'Native Swift · macOS 14+',
    desc: '正式出貨版。App 內更新走簽章驗證的 Sparkle feed，安裝前都會驗證；beta 版只是<strong>設定裡的開關</strong>，不是另一個 cask。',
    copyBtn: '複製指令',
    copied: '已複製 ✓',
    legacy:
      '還在用舊一點的 Mac？<code>tokenbar@legacy</code> 釘住最後一版 Tauri build（v0.4.5，macOS 11+）——<a href="https://github.com/Nanako0129/TokenBar-Tauri">legacy repo</a> 已封存，但 cask 會留著。想自己編譯？<a href="https://github.com/Nanako0129/syrtis">Swift 6 + Rust</a>，Command Line Tools 就夠。',
  },
  faq: {
    eyebrow: '常見問題',
    h2: '先說清楚。',
    intro: '安裝前大家最想知道的幾個簡答。',
    items: [
      {
        q: 'Syrtis 到底是什麼？',
        a: '免費開源的原生 macOS 選單列 app，讀取本機的 AI 編碼 session 紀錄，把你的花費攤開來看——貢獻圖、各模型與各 agent 細目、每月、每日與每時視圖、即時吞吐量。不用跑 CLI、免帳號。',
      },
      {
        q: '追蹤哪些 AI 編碼工具？',
        a: '25+ 個 agent，直接讀本機紀錄——Claude Code、Codex、Cursor、OpenCode、Gemini CLI、Copilot、Amp、Droid、Hermes、Goose、Kilo/KiloCode、Roo Code、Qwen、Kimi、Crush、Zed、Kiro、Trae、Warp 等。解析交給 vendored tokscale-core，涵蓋範圍跟著 tokscale 走。',
      },
      {
        q: '要錢嗎？',
        a: '不用。Syrtis 採 MIT 授權、完全免費——沒有訂閱、沒有付費版、沒有遙測。',
      },
      {
        q: '原本的（Tauri）版本怎麼了？',
        a: 'Syrtis 最初是 tokcat 的 Tauri fork。v1.0.0（2026 年 6 月）起換成從零重寫的原生 Swift app，沿用同一顆 Rust 解析核心——`tokenbar` cask 現在裝的就是原生版，Tauri repo 已封存。`tokenbar@legacy` cask 釘住最後一版 Tauri build（v0.4.5），支援 macOS 11+。',
      },
      {
        q: '更新怎麼跑？',
        a: 'App 內更新，走簽章的 Sparkle feed——每個版本安裝前都會驗證簽章，更新說明由實際 commit 摘要而成。想吃 beta？設定裡一個開關，不用另外安裝。',
      },
      {
        q: '我的資料去了哪？',
        a: '哪都沒去。用量歷史直接從磁碟讀。網路請求僅限：簽章更新 feed、公開模型價格資料，以及使用你機器上既有 OAuth 憑證的選擇性 Codex/Claude 額度查詢——選單列額度儀錶和 agent 額度卡就是靠這個。',
      },
      {
        q: 'Intel Mac 或 Windows 呢？',
        a: 'Syrtis 鎖定 Apple Silicon（arm64）、macOS 14+；Liquid Glass 需要 macOS 26，較舊系統退回 vibrancy。Legacy Tauri 版涵蓋 macOS 11+。沒有 Intel、Windows 或 Linux 版。',
      },
      {
        q: '怎麼移除？',
        a: 'brew uninstall --cask tokenbar。',
      },
    ],
  },
  credits: {
    eyebrow: '站在肩膀上',
    h2: '建立在出色的開源之上。',
    intro: '沒有這些專案就沒有 Syrtis——感謝每一位維護者。',
    go: '到 GitHub 看 ↗',
    items: [
      {
        name: 'tokscale',
        who: 'by Junho Yeo',
        href: 'https://github.com/junhoyeo/tokscale',
        body: '整個專案的地基。vendored 的 tokscale-core crate 驅動 Syrtis 跨 25+ agent 的 session 解析、去重與計價——而它的互動式 TUI 更是整套儀表板的藍本：七種透鏡，連同 In · Out · CR · CW 的欄位拆解，都照著它來。',
        accent: '--p-deepseek',
      },
      {
        name: 'tokcat',
        who: 'by handlecusion',
        href: 'https://github.com/handlecusion/tokcat',
        body: 'Syrtis 產品線的起點——原版的 Tauri 選單列監視器（它自己也建立在 tokscale 之上）。原生版是從零的 Swift 重寫、不含任何 tokcat 程式碼，但選單列寵物的形態與 oiiai 旋轉貓的招牌創意都來自這裡。',
        accent: '--p-anthropic',
      },
      {
        name: 'RunCat',
        who: 'by Takuto Nakamura',
        href: 'https://kyome.io/runcat/',
        body: '選單列養寵物（動得多快反映你多忙）這個設計的始祖。party parrot、以及每一隻在 Mac 選單列上衝刺過的貓（tokcat 的和 Syrtis 的都算），都可以追溯到這裡。',
        accent: '--p-cursor',
      },
      {
        name: 'CodexBar',
        who: 'by Peter Steinberger',
        href: 'https://github.com/steipete/CodexBar',
        body: 'Syrtis 的額度進度呈現——超前或落後視窗、預計耗盡時間——參考了 CodexBar 的做法。',
        accent: '--p-openai',
      },
    ],
  },
  footer: {
    tagline: 'macOS 選單列的 AI token 用量監視器。MIT 授權。',
    stamp: 'macOS 14+ · Apple Silicon · Swift + Rust · Liquid Glass · MIT',
    copyright: '© Syrtis contributors',
    langSwitch: { href: '/', label: 'English' },
  },
}

const dict: Record<Locale, typeof en> = { en, 'zh-tw': zhTw }

export const t = (locale: Locale) => dict[locale]
