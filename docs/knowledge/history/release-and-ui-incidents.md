---
status: historical
id: kb-history-release-ui-incidents
kind: canonical
scope: repository
read_when: diagnosing a release regression, update failure, stale UI, or background CPU
last_verified: 2026-07-14
sources: ["public release history", "sanitized native progress memory", "release and workflow canonical documents"]
---

# Release and UI incidents

## 文件目的

這份文件保留幾個已驗證、容易重演的事故模式。它不是目前 workflow 的替代品；每個結論都應回到 [`release.md`](../release.md)、[`workflow.md`](../workflow.md) 或 runtime source。

## Incident map

| Incident | Root cause | Durable lesson | Current status |
|---|---|---|---|
| Beta bridge “improperly signed” | Sparkle could not find a bundle matching the old filename and bundle identity | A validation-looking error can be an install-target lookup failure; use the explicit stable migration path | Bridge population only shrinks |
| Single-item appcast | A writer replaced the feed instead of preserving stable and prerelease items | Use `generate_appcast` with a multi-item feed and verify channel semantics | Fixed |
| Release-note drift | Separate generated surfaces and non-deterministic generation repeated old fixes or changed wording | Compare published surfaces after every release; repair text without rebuilding when possible | Process rule |
| Install badge skipped cask | A non-critical workflow dispatch failed before the cask step | Give independent release steps explicit permissions and failure policy | Fixed in workflow |
| Popover stale after crossing a day | Initial load ran once while the hosting view stayed alive | Poll the graph through the mtime-aware FFI cache and keep last good data | Fixed; empty-today edge remains parked |
| Background CPU | Rayon workers and hidden SwiftUI polling/render trees stayed active | Profile before guessing; bound worker pools and cancel hidden-window tasks | Fixed in shipped maintenance line |
| Loading flash after lifecycle reset | Rebuilding a view for CPU cancellation also rebuilt its loading state | Cache only safe snapshots, exclude lazy reports, and guard year/lifecycle races | Fixed |
| Hidden client counted in totals | Tab-level hiding did not reach pre-aggregated report consumers | Push client filters to Rust before mixed folds and sweep every display consumer | Fixed in the hidden-client workstream |
| Agent icon audit | Incorrect or copied icon paths, or unsupported SVG effects, produced wrong icons | Compare each official app asset, title/path, and renderer support; do not substitute favicons or approximate assets | Fixed |

Setup-token quota fallback shipped: when profile usage is unavailable, provider rate-limit responses can still present quota to the user.

## Release incidents

Release chain incidents shared a pattern: one workflow step was treated as if it owned every downstream artifact. The durable fix is to make appcast, GitHub notes, cask, badge, legacy metadata, and Pages deployment independently observable. A successful app bundle does not prove a successful cask update or accurate release prose.

## UI lifecycle incidents

The menu-bar shell can remain alive while a popover or settings window is hidden. A SwiftUI `.task` or `TimelineView` tied only to view identity may therefore continue running. Any lifecycle change must measure both visible and hidden states, verify cancellation, and preserve the last known data instead of replacing it with an empty loading state.

## Correctness incident

The hidden-client work exposed a general failure mode: a final Swift filter cannot subtract a client from a Rust bucket that already combines multiple clients. The repair required a source-to-consumer matrix, ID vocabulary audit, FFI option propagation, and hermetic parity fixtures. That method is now canonical in [`architecture.md`](../architecture.md) and [`verification.md`](../verification.md).
