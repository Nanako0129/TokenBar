import AppKit
import SwiftUI

/// Forces the enclosing NSScrollView onto overlay-style scrollers: invisible
/// at rest, a translucent pill while scrolling, and a brief flash when the
/// popover opens so users learn the content scrolls. The system-wide "always
/// show scroll bars" preference — and "automatic" while a mouse is attached —
/// would otherwise pin the legacy track (the thick, permanently-visible strip).
///
/// Setting `scrollerStyle = .overlay` once is not enough: AppKit reapplies the
/// *legacy* preferred style a beat after layout (measured ~0.6s after open),
/// overriding the one-shot set, which is the "thick scroller stuck after a
/// relaunch" bug. So we KVO-watch `scrollerStyle` and flip any revert straight
/// back to overlay — event-driven, so it wins no matter when AppKit re-asserts.
struct OverlayScrollerEnforcer: NSViewRepresentable {
    func makeNSView(context: Context) -> EnforcerView { EnforcerView() }

    func updateNSView(_ view: EnforcerView, context: Context) { view.enforce() }

    @MainActor
    final class EnforcerView: NSView {
        nonisolated private static let styleKeyPath = "scrollerStyle"
        private weak var observedScroll: NSScrollView?
        private var registeredPrefObserver = false
        private var flashedInWindow = false

        override func viewWillMove(toWindow newWindow: NSWindow?) {
            super.viewWillMove(toWindow: newWindow)
            // Leaving the window arms the one-shot flash for next time and
            // drops the KVO watch (re-added on the next open).
            if newWindow == nil {
                flashedInWindow = false
                stopObservingScroll()
            }
        }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            enforce()
        }

        override func viewDidMoveToSuperview() {
            super.viewDidMoveToSuperview()
            enforce()
            guard !registeredPrefObserver else { return }
            registeredPrefObserver = true
            // The system flips styles when the global preference changes;
            // re-assert (and rewatch) whenever that happens.
            NotificationCenter.default.addObserver(
                forName: NSScroller.preferredScrollerStyleDidChangeNotification,
                object: nil, queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated { self?.enforce() }
            }
        }

        /// Pin overlay style on the enclosing scroll view and KVO-watch it so
        /// any later revert is undone immediately. Idempotent — every render
        /// pass may call it.
        func enforce() {
            guard let scroll = enclosingScrollView else { return }
            if observedScroll !== scroll {
                stopObservingScroll()
                scroll.addObserver(self, forKeyPath: Self.styleKeyPath, options: [], context: nil)
                observedScroll = scroll
            }
            pin(scroll)
            guard scroll.window != nil, !flashedInWindow else { return }
            flashedInWindow = true
            scroll.flashScrollers()
        }

        private func pin(_ scroll: NSScrollView) {
            if scroll.scrollerStyle != .overlay { scroll.scrollerStyle = .overlay }
            scroll.autohidesScrollers = true
        }

        private func stopObservingScroll() {
            observedScroll?.removeObserver(self, forKeyPath: Self.styleKeyPath)
            observedScroll = nil
        }

        // KVO fires on the thread that mutated the property — main for UI.
        // Nonisolated to match NSObject's declaration; hop back on with assume.
        nonisolated override func observeValue(
            forKeyPath keyPath: String?, of object: Any?,
            change: [NSKeyValueChangeKey: Any]?, context: UnsafeMutableRawPointer?
        ) {
            guard keyPath == Self.styleKeyPath else {
                super.observeValue(
                    forKeyPath: keyPath, of: object, change: change, context: context)
                return
            }
            // Re-pin via the watched scroll view we already hold (avoids sending
            // the non-Sendable `object` across the isolation boundary).
            MainActor.assumeIsolated {
                guard let scroll = self.observedScroll, scroll.scrollerStyle != .overlay
                else { return }
                scroll.scrollerStyle = .overlay
            }
        }
    }
}
