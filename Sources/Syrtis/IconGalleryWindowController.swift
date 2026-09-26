import AppKit
import SwiftUI

/// Debug-only standalone window for `--icon-gallery` (see IconGalleryView).
/// Throwaway screenshot aid, not a shipped feature — no state persistence,
/// no close-teardown handling needed.
@MainActor
enum IconGalleryWindowController {
    private static var window: NSWindow?

    static func show() {
        let host = NSHostingController(rootView: IconGalleryView())
        let window = NSWindow(contentViewController: host)
        window.title = "Agent Icon Gallery (debug)"
        window.styleMask = [.titled, .closable, .miniaturizable]
        window.isReleasedWhenClosed = false
        Self.window = window
        NSApp.activate(ignoringOtherApps: true)
        window.center()
        window.makeKeyAndOrderFront(nil)
    }
}
