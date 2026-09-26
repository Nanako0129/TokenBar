import AppKit
import SwiftUI

/// Lets a plain vertical mouse wheel scroll a horizontal `ScrollView`. The
/// client-tab row scrolls horizontally, but a mouse without a horizontal-scroll
/// device only emits vertical wheel deltas, so those users otherwise can't move
/// the row. A trackpad (which sends horizontal/precise deltas) is left
/// untouched, and the redirect only fires while the cursor is over this row.
///
/// Place inside the horizontal ScrollView's content so `enclosingScrollView`
/// resolves to that row's NSScrollView (the OverlayScroller pattern).
struct HorizontalWheelScroll: NSViewRepresentable {
    func makeNSView(context: Context) -> WheelRedirectView { WheelRedirectView() }
    func updateNSView(_ view: WheelRedirectView, context: Context) {}

    /// The clamped horizontal origin after stepping by `step`, and whether it
    /// actually moved. Pure so tests can exercise the boundary case (already
    /// scrolled all the way to an edge) without simulating an `NSEvent`.
    ///
    /// FLAT-HEATMAP round 4, FIX 2: at either scroll edge (e.g. the heatmap's
    /// default trailing-scrolled position) the old code clamped the origin
    /// and unconditionally reported the event as consumed, even when the
    /// clamped value was identical to the current one — swallowing the
    /// dashboard's *vertical* scroll on the very first wheel tick over a
    /// heatmap already parked at its right edge.
    static func clampedScroll(originX: CGFloat, step: CGFloat, maxX: CGFloat) -> (newOriginX: CGFloat, moved: Bool) {
        let newOriginX = min(max(0, originX - step), maxX)
        return (newOriginX, newOriginX != originX)
    }

    @MainActor
    final class WheelRedirectView: NSView {
        private var monitor: Any?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if window == nil {
                removeMonitor()
            } else if monitor == nil {
                monitor = NSEvent.addLocalMonitorForEvents(matching: .scrollWheel) {
                    [weak self] event in
                    guard let self else { return event }
                    return self.redirect(event) ? nil : event
                }
            }
        }

        /// Returns true when the event was consumed as a horizontal scroll.
        private func redirect(_ event: NSEvent) -> Bool {
            guard let scroll = enclosingScrollView,
                let window, event.window === window
            else { return false }
            // A trackpad scroll carries gesture phases; a mouse wheel — even a
            // high-res one like the MX Master — does not. Restrict the redirect
            // to phase-less mouse wheels so trackpad swipes keep full native
            // behavior. (hasPreciseScrollingDeltas can't tell them apart: a
            // high-res mouse reports precise deltas for smooth scrolling too.)
            guard event.phase.isEmpty, event.momentumPhase.isEmpty else { return false }
            // The vertical wheel reports a clean vertical-only delta; the
            // horizontal thumb wheel (deltaX != 0) is left to scroll natively.
            guard event.scrollingDeltaX == 0 else { return false }
            let dy = event.scrollingDeltaY
            guard dy != 0 else { return false }
            // Only redirect while the pointer is over this row's scroll view.
            let point = scroll.convert(event.locationInWindow, from: nil)
            guard scroll.bounds.contains(point) else { return false }

            let clip = scroll.contentView
            let maxX = max(0, (scroll.documentView?.frame.width ?? 0) - clip.bounds.width)
            guard maxX > 0 else { return false }
            // Precise (smooth) deltas are already in points; coarse wheel deltas
            // are in lines and need scaling for a comfortable step.
            let step = event.hasPreciseScrollingDeltas ? dy : dy * 16
            let (newOriginX, moved) = HorizontalWheelScroll.clampedScroll(
                originX: clip.bounds.origin.x, step: step, maxX: maxX)
            // Already at this edge — let the event fall through to the parent
            // (vertical) scroll view instead of silently eating it.
            guard moved else { return false }
            var origin = clip.bounds.origin
            origin.x = newOriginX
            clip.setBoundsOrigin(origin)
            scroll.reflectScrolledClipView(clip)
            return true
        }

        private func removeMonitor() {
            if let monitor { NSEvent.removeMonitor(monitor) }
            monitor = nil
        }
        // The monitor is torn down in viewDidMoveToWindow when the row leaves
        // its window (popover close), so no deinit cleanup is needed — and a
        // nonisolated deinit cannot touch the non-Sendable monitor handle.
    }
}
