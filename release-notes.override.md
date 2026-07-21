## Highlights

- **Popover tooltips stay readable.** Token Usage (2D) and Models rich tooltips clamp inside the visible popover scroll area so they no longer sit under the footer or liquid-glass cards, restore cursor dodge (prefer above the pointer in the lower half of the source card), and stop inventing a fake dodge position right after you resize the popover height then hover again. [#78](https://github.com/Nanako0129/TokenBar/pull/78)

## Fixes

- **Tooltip layering and hit testing.** Open tooltips raise above neighboring glass cards; Models overlay geometry no longer steals hover from the row underneath.
- **Resize then hover.** Live scroll-viewport coordinates (with anchor-based freshness) replace a frozen pre-drag rect so post-resize hover placement matches the new height.
