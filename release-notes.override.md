## Highlights

- **Add optional menu bar items for individual clients.** Enable eligible clients in Settings to show their brand icon and the remaining quota for Auto or a selected window; clicking an item opens TokenBar on that client. [#119](https://github.com/Nanako0129/TokenBar/pull/119)
- **Settings now uses native sidebar navigation.** Menu Bar, Dashboard, General, and About each get a focused page while the live preview remains visible, and quota source selection is now organized as Agent then Window. [#117](https://github.com/Nanako0129/TokenBar/pull/117)

## Changes

- **Reorder client tabs directly in the popover.** Drag tabs along the top bar; Overview stays first, and hidden clients keep their saved positions. [#116](https://github.com/Nanako0129/TokenBar/pull/116) — thanks @amikai

## Fixes

- **Historical quota pace now starts from validated evidence instead of a fixed completed-cycle requirement.** A stable current cycle can qualify earlier; insufficient or low-quality history stays in learning mode, and risk remains hidden until enough evidence exists. [#120](https://github.com/Nanako0129/TokenBar/pull/120)
- **Claude quota cards now reflect live Pro and Max subscription changes** instead of relying only on a login-time plan snapshot. The live lookup is bounded and cached, with stored plan data used as a fallback when needed. [#124](https://github.com/Nanako0129/TokenBar/pull/124)
- **Grok usage is attributed to models more safely across process restarts and subagents.** Ambiguous or conflicting evidence remains unattributed instead of being guessed. [#123](https://github.com/Nanako0129/TokenBar/pull/123)
- **Popover height dragging stays responsive,** and Settings and Quit continue responding normally after a resize. [#110](https://github.com/Nanako0129/TokenBar/pull/110)
