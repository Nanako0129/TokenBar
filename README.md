<h1 align="center">TokenBar</h1>

<p align="center">
  <strong>AI token usage &amp; quota monitor for the macOS menu bar — native Swift, Liquid Glass.</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Nanako0129/TokenBar/badges/install-count.json&style=flat-square" alt="Total installs">
  <img src="https://img.shields.io/badge/License-MIT-yellow.svg?style=flat-square" alt="MIT Licence">
  <img src="https://img.shields.io/badge/macOS-14%2B-black?style=flat-square&logo=apple" alt="macOS 14+">
  <img src="https://img.shields.io/badge/Apple%20Silicon-arm64-success?style=flat-square" alt="Apple Silicon">
  <img src="https://img.shields.io/badge/Swift-6-F05138?style=flat-square&logo=swift&logoColor=white" alt="Swift 6">
  <img src="https://img.shields.io/badge/Liquid%20Glass-macOS%2026-7c6cff?style=flat-square" alt="Liquid Glass">
</p>

<br>

**TokenBar** sits in your menu bar and shows what you're spending across
**25+ AI coding agents** — Claude Code, Codex, Cursor, OpenCode, Gemini CLI and
more — read on-device from your local session logs. No Dock icon, no telemetry,
no account.

<p align="center">
  <img src="docs/screenshots/popover-dark.png" alt="TokenBar popover in Liquid Glass over the desktop wallpaper" width="400">
</p>

The menu-bar title shows today's tokens, cost, live tokens/min, or **how much
subscription quota is left** — as signal bars, a ring, or a popsicle that melts
as your window drains. And the **cat sprints faster the more you burn**, tracing
back to [RunCat](https://kyome.io/runcat/) by Takuto Nakamura.

---

## The dashboard

Click the icon and a Liquid Glass popover opens. A row of **app tabs** filters
_which_ agents you're looking at; a **view switch** picks _how_ that data is
broken down — six lenses, plus the same year of usage as an orbitable 3D graph.

<p align="center">
  <img src="docs/screenshots/graph-3d.png" alt="Interactive 3D contribution graph" width="400">
</p>

<table>
  <tr>
    <td align="center" width="50%"><img src="docs/screenshots/models.png" alt="Models view" width="300"><br><sub><b>Models</b> — every model ranked by cost</sub></td>
    <td align="center" width="50%"><img src="docs/screenshots/daily.png" alt="Daily view" width="300"><br><sub><b>Daily</b> — active days, with day drill-down</sub></td>
  </tr>
  <tr>
    <td align="center" width="50%"><img src="docs/screenshots/hourly.png" alt="Hourly view" width="300"><br><sub><b>Hourly</b> — when in the day you burn tokens</sub></td>
    <td align="center" width="50%"><img src="docs/screenshots/stats.png" alt="Stats view" width="300"><br><sub><b>Stats</b> — headline summary &amp; streaks</sub></td>
  </tr>
  <tr>
    <td align="center" width="50%"><img src="docs/screenshots/agents.png" alt="Agents view" width="300"><br><sub><b>Agents</b> — sub-agents ranked by cost</sub></td>
    <td align="center" width="50%"><img src="docs/screenshots/settings.png" alt="Settings" width="300"><br><sub><b>Settings</b> — menu-bar title, icon &amp; quota source</sub></td>
  </tr>
</table>

Plus **OAuth quota cards** with pace projections, a live session trace, streaks,
and full keyboard control (⌘1–9 tabs, ⌘G chart toggle, ⌘, settings). A failed
refresh never blanks a reading — the last known value stays until a fresh one
lands.

## Install

```sh
brew install --cask nanako0129/tokenbar/tokenbar
```

In-app updates arrive via Sparkle; betas ride an opt-in channel
(Settings → "Receive beta updates"). The app is ad-hoc signed (not notarized) —
the cask clears the quarantine attribute on install, as disclosed. Requires an
Apple Silicon Mac on macOS 14+ (Liquid Glass needs macOS 26; earlier systems get
a vibrancy fallback). Still on macOS 11–13? The final Tauri build stays as
[`tokenbar@legacy`](https://github.com/Nanako0129/TokenBar-Tauri).

## How it works

Rust owns the data — session parsing, aggregation, pricing, quota fetching — via
the vendored [tokscale-core](https://github.com/junhoyeo/tokscale), exposed to
Swift as a C-ABI staticlib (`crates/tb_core_ffi`). Swift owns the rest: SwiftUI
views, the `NSStatusItem` shell, Sparkle updates.

```sh
make        # cargo build --release, then swift build
make run    # build + run the smoke binary
```

For maintainers and coding agents, the [project knowledge base](docs/knowledge/README.md) is the canonical guide to the Rust-to-Swift architecture, verification gates, vendor boundary, release chain, and maintenance state.

> Run `swift build` from the repo root — the linker's `-L target/release` path
> in `Package.swift` is relative.

## Credits

TokenBar is built on **[tokscale](https://github.com/junhoyeo/tokscale)** by
Junho Yeo. Its vendored `tokscale-core` crate does the session parsing, dedup,
and pricing across 25+ agents — and its interactive TUI is the blueprint for the
whole dashboard: the six lenses (Overview, Models, Daily, Hourly, Stats, Agents)
and their `In · Out · CR · CW` column breakdown are modeled on it.

The product line began as a fork of
[tokcat](https://github.com/handlecusion/tokcat) by handlecusion — the original
Tauri menu-bar monitor (itself built on tokscale). This native app is a
ground-up Swift rewrite that carries no tokcat code, but the menu-bar form and
the spinning-cat signature are theirs; the cat traces back to
[RunCat](https://kyome.io/runcat/) by Takuto Nakamura. The quota-pace cards
reference [CodexBar](https://github.com/steipete/CodexBar) by Peter Steinberger.

All MIT. Licensed under [MIT](LICENSE).
