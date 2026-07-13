---
status: parked
id: kb-history-liquid-glass
kind: canonical
scope: repository
read_when: considering a glass, popover, panel, or transparency redesign
last_verified: 2026-07-14
sources: ["sanitized local experiment source", "sanitized Liquid Glass project memory", "Sources/TokenBar/GlassBackground.swift", "Sources/TokenBar/Views/Cards.swift"]
---

# Liquid Glass experiments

## 文件目的

這份文件把 Liquid Glass 調查中仍會影響維護決策的 durable technical evidence 遷入 canonical history。調查結論是 parked、維持現狀：卡片確實有真實折射，但系統面板級 parity 受兩個結構性缺口限制，沒有新的 panel re-architecture 計畫。Spike 的 app changes 已還原；下列 shipping recipe 是技術記錄，不是 runtime 變更。

> **結論：** 卡片 `.glassEffect` 確實會折射背後內容，但系統面板級的外觀需要透明視窗架構，且仍會撞上 public API 無法提供的 theme-independent material。現行卡片配方維持不變。

## 目錄

- [Goal](#goal)
- [Verified findings](#verified-findings)
- [Decisive evidence](#decisive-evidence)
- [Glass variants](#glass-variants)
- [Approaches tried](#approaches-tried)
- [Structural gaps](#structural-gaps)
- [Shipping recipe](#shipping-recipe)
- [Future resume plumbing](#future-resume-plumbing)
- [Parked boundary](#parked-boundary)

---

## Goal

The 2026-06/07 investigation asked why TokenBar's dashboard cards looked flat or smoked instead of like macOS Control Center or the Wi-Fi menu-bar dropdown, and whether a third-party menu-bar app could match those system panels.

## Verified findings

| Question | Verified answer |
|---|---|
| Is the card glass real Liquid Glass? | **Yes.** `.glassEffect` genuinely refracts contrasty content behind it. |
| What does the refraction look like? | Blur, dimming, and a thin rim-lens at the rounded edges—not dramatic geometric warping. |
| Why do the shipping cards look flat? | They sit over a spatially uniform `NSVisualEffectView.Material.hudWindow` backdrop, so the glass has little contrast to refract. |
| Does swapping `NSVisualEffectView.Material` help? | **No.** Every tested behind-window material is a uniform blur layer, with no spatial variation for the lens effect. |
| Is bundling or the SDK the gate? | **No.** A signed app built against the macOS 26.5 SDK using real `NSGlassEffectView` reproduced the same behavior. |
| `NSGlassEffectView` (AppKit) versus SwiftUI `.glassEffect`? | **Approximately equivalent.** AppKit's edge is marginally crisper, but there is no qualitative difference. |

## Decisive evidence

> **Decisive half-color/half-flat evidence:** The same card recipe placed half over colorful content and half over a flat dark area refracted the colorful half while the dark half stayed flat. The glass works; it needs contrasty content behind it.

A hover-tooltip z-order bug independently exposed the same behavior: the Streaks card's glass refracted the tooltip content beneath it. The observed effect is therefore content-dependent refraction, not a painted rim or a packaging artifact.

## Glass variants

| Variant | Behavior in this context | Decision |
|---|---|---|
| `.clear` | Ultra-transparent. Over a transparent window with no backing material it renders to almost nothing, so the card vanishes. It is too thin and lets the background read sharply without frost. | Not suitable as the panel base. |
| `.regular` | Frosts and blurs the background in the system-panel direction. It is the right visible base, although dark mode renders a dark frost. | Use as the base for the future panel structure. |

## Approaches tried

| # | Approach | Result and decision |
|---:|---|---|
| 1 | Hand-drawn white rim on `.clear` cards | Rejected. It creates fake glass without real refraction. |
| 2 | Synthetic aurora gradient behind the cards inside the window | Works technically because the glass refracts it, but it is painted content rather than the real background the product needs. Rejected. |
| 3 | Transparent `NSPanel` replacing `NSPopover`, with cards using `.glassEffect(.clear)` | Cards are invisible because `.clear` over a transparent window is nearly nothing. Rejected. |
| 4 | The same transparent panel, with cards using `.glassEffect(.regular)` | Cards become visible and refract the desktop, but the panel chrome—header, footer, and gaps—becomes transparent. Raw desktop bleeds through and the result is messy. Rejected. |
| 5 | One `.regular` glass surface filling the panel, with cards as plain fills | Closest to the Wi-Fi-dropdown model: a cohesive frosted panel, readable base, and edge refraction. Keep as the winning future structure, not shipping code. |
| 6 | Flat white overlay, tuned from `0.18` to `0.07` | A flat white veil makes the surface too white and foggy; it fogs rather than brightens. Rejected. |
| 7 | White `.plusLighter` glow layer, even at `0.05` | It still fogs. An overlay is the wrong tool for brightness. Rejected. |
| 8 | Force the light material with `.environment(\.colorScheme, .light)` | It becomes a light-theme white surface instead of the theme-independent look of system panels. Rejected. |

## Structural gaps

| Gap | Consequence |
|---|---|
| `NSPopover` blurs the desktop before the cards see it. | `NSPopover` always draws its own material between the content and the desktop, so cards refract a flat blurred tone rather than sharp wallpaper. Reaching the desktop requires a transparent `NSPanel` and a real re-architecture: hand-rolled transient dismissal, positioning, and the missing system arrow all have to be rebuilt. |
| System panels use a theme-independent material, while public third-party glass follows the app color scheme. | Control Center and the Wi-Fi dropdown keep consistent translucency in light and dark mode. `.glassEffect` and `NSGlassEffectView` render a dark frost in dark mode, and public APIs provide no knob for the system's theme-independent material. This private-material gap remains even after a panel rewrite. |

> Even a perfect transparent-panel version would still be blur, dimming, and a rim over whatever is behind the popover. It would be subtle over a typical dark or plain desktop and would remain tied to the app theme. The dramatic, consistent system-panel look is not reachable with public APIs.

## Shipping recipe

The shipping app keeps the current card recipe in `Sources/TokenBar/Views/Cards.swift` on the macOS 26 branch:

```swift
// GlassCardBackground (Views/Cards.swift), macOS 26 branch
content
    .background(colorScheme == .dark ? Color.black.opacity(0.32)
                                     : Color.white.opacity(0.10),
                in: RoundedRectangle(cornerRadius: cornerRadius))
    .glassEffect(.clear, in: .rect(cornerRadius: cornerRadius))
```

The backdrop remains `PopoverBackdrop` backed by `NSVisualEffectView(.hudWindow, .behindWindow)`. Do not use a white or `.plusLighter` overlay to brighten the glass: brightness has to come from the material, and the material follows the app theme—the second structural gap above.

## Future resume plumbing

The winning structure is approach 5: one `.regular` `GlassEffectContainer` filling a transparent `NSPanel`, with cards rendered as plain fills. It is a reference for a future investigation, not authorization to redesign the shipping popover.

| Panel concern | Working spike behavior | Edge case or constraint |
|---|---|---|
| Window shell | Borderless `NSPanel`, `isOpaque = false`, `backgroundColor = .clear`, `level = .popUpMenu`, `canBecomeKey = true`, and `hidesOnDeactivate = false` | The shell must supply the missing transparent-window behavior that `NSPopover` cannot provide. |
| Positioning | Position under the status-item button through `btnWindow.convertToScreen(...)`. | Positioning becomes hand-rolled once the system popover is replaced. |
| Transient dismissal | Dismiss from `windowDidResignKey`. | Menus or pickers inside the panel can resign key and cause premature dismissal; this needs explicit lifecycle handling. |
| Shape and chrome | The glass surface fills the panel while cards use plain fills. | A rounded panel surface must be reconciled with the rectangular window shadow, and chrome must remain readable over transparent areas. |

If the investigation resumes, reproduce the panel lifecycle and menu/picker key-resignation behavior first, then obtain a separate product decision before changing the shipping popover.

## Parked boundary

> This canonical history now retains the durable technical evidence from the experiment log. After the knowledge checks verify the migration, the local raw source may be removed. Private and transient experiment material remains outside the tracked knowledge base.

The current runtime remains unchanged. The document records the decisive evidence, eight tested approaches, the two structural gaps, the shipping recipe, and the panel plumbing needed to resume the investigation without treating the old raw log as the canonical source.
