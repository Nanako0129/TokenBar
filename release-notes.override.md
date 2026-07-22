## Highlights

- **Copilot Desktop usage is now included.** TokenBar reads token-bearing local sessions from Copilot Desktop, preserves model, workspace, and agent attribution, and lets Copilot OTEL remain authoritative when both sources contain the same session. Totals may increase because previously invisible Desktop usage is now counted, not because the update created new spend. [#83](https://github.com/Nanako0129/TokenBar/pull/83)
- **Model usage is easier to inspect.** Hovered Token Usage and Models bars now gain adaptive outlines and glow in both light and dark appearances. Model markers glow with their rows, and Daily/Monthly drill-downs show detailed Input, Output, Cache read, Cache write, and Reasoning tooltips above neighboring rows. [#88](https://github.com/Nanako0129/TokenBar/pull/88)

## Changes

- **Hermes discovery supports native Windows layouts** for the downstream Windows build while leaving the macOS app unchanged. [#82](https://github.com/Nanako0129/TokenBar/pull/82)
