<!-- DRAFT mirror of release-notes.override.md for v1.6.1 review.
     Target: v1.6.1 from origin/main @ 45c2b85d (post #78).
     Scope: patch — hover tooltip clamp / dodge / post-resize placement only.
     Excludes open M23 PR #74 unless merged before tag. -->

## Highlights

- **Popover tooltips stay readable.** Token Usage (2D) and Models rich tooltips clamp inside the visible popover scroll area so they no longer sit under the footer or liquid-glass cards, restore cursor dodge (prefer above the pointer in the lower half of the source card), and stop inventing a fake dodge position right after you resize the popover height then hover again. [#78](https://github.com/Nanako0129/TokenBar/pull/78)

## Fixes

- **Tooltip layering and hit testing.** Open tooltips raise above neighboring glass cards; Models overlay geometry no longer steals hover from the row underneath.
- **Resize then hover.** Live scroll-viewport coordinates (with anchor-based freshness) replace a frozen pre-drag rect so post-resize hover placement matches the new height.

## Not in this release

- Copilot Desktop SQLite, VS Code `chatSessions`, and Hermes Windows discovery (open M23 / PR #74)
- Zcode client (PR #72 closed unmerged; deferred)
- In-app settings UI for reloadable model aliases (core API only; PR #75)

**Full Changelog**: https://github.com/Nanako0129/TokenBar/compare/v1.6.0...v1.6.1
