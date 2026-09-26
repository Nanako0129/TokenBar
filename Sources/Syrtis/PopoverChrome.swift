import AppKit
import SwiftUI

/// Shared model for the popover's size. The status-item controller owns one
/// instance; the SwiftUI popover reads `height` for its frame and the bottom
/// drag handle writes it. Width is fixed — only the height is user-adjustable,
/// from either the drag handle or the settings slider, both routed through the
/// same `tokenbar.popover.height` default so they stay in sync.
///
/// The size is resolved against the *status item's* screen on every open
/// (`resolve(visibleHeight:)`) rather than once at launch from `NSScreen.main`:
/// an accessory app has no key window at launch, so `NSScreen.main` there
/// resolves to whatever app was frontmost (often the wrong/smaller display),
/// which is why auto-updated installs kept the wrong size while fresh launches
/// looked fine.
@MainActor
final class PopoverChrome: ObservableObject {
    static let heightKey = "tokenbar.popover.height"

    /// Fixed popover width — bumped from the original 360 for a roomier read.
    static let width: CGFloat = 400
    /// Smallest height that still shows a usable card stack.
    static let minHeight: CGFloat = 480

    /// Auto height when the user hasn't picked one: ~60% of the screen's
    /// visible height, with a roomier floor than the old fixed 480.
    static func autoHeight(visibleHeight: CGFloat) -> CGFloat {
        max(560, (visibleHeight * 0.6).rounded())
    }

    let width = PopoverChrome.width
    let minHeight = PopoverChrome.minHeight

    /// Tallest height the current screen can host — set per-open by the
    /// controller from the status item's actual screen.
    @Published var maxHeight: CGFloat = 1200
    /// The user's explicit choice; 0 means "auto" (track the screen). Only the
    /// drag handle and the settings slider ever set this above 0.
    @Published private(set) var rawHeight: CGFloat
    /// Auto height for the current screen, recomputed on each resolve — used
    /// while `rawHeight` is 0 so the default stays adaptive across screens.
    @Published private var autoValue: CGFloat = 560

    /// Pushes the live height to the AppKit popover. `live` is true mid-drag
    /// so the controller can suppress the per-step resize animation.
    var onResize: ((_ height: CGFloat, _ live: Bool) -> Void)?

    /// The applied height: the user's choice or the screen auto, clamped.
    var height: CGFloat {
        min(max(minHeight, rawHeight > 0 ? rawHeight : autoValue), maxHeight)
    }

    init() {
        rawHeight = CGFloat(UserDefaults.standard.double(forKey: Self.heightKey))
    }

    /// Called before each open with the real screen bounds: refreshes the
    /// screen-derived auto height and clamps to the screen. Persists nothing —
    /// an unset height stays auto and keeps adapting.
    func resolve(visibleHeight: CGFloat) {
        maxHeight = max(minHeight, visibleHeight - 24)
        autoValue = Self.autoHeight(visibleHeight: visibleHeight)
        onResize?(height, false)
    }

    /// Live drag / slider updates. `persist` writes the value back so it
    /// survives relaunch and syncs the settings slider.
    func setHeight(_ value: CGFloat, persist persistValue: Bool, live: Bool) {
        let next = min(max(minHeight, value), maxHeight)
        if live {
            onResize?(next, true)
            return
        }
        rawHeight = next
        if persistValue { UserDefaults.standard.set(Double(rawHeight), forKey: Self.heightKey) }
        onResize?(height, false)
    }

    /// Re-read the value the settings slider wrote (from the other window) and
    /// apply it to the live popover if it actually changed. A stored 0 means
    /// the user reset to Auto.
    func reloadFromDefaults() {
        let stored = CGFloat(UserDefaults.standard.double(forKey: Self.heightKey))
        let next = stored > 0 ? min(max(minHeight, stored), maxHeight) : 0
        guard next != rawHeight else { return }
        rawHeight = next
        onResize?(height, false)
    }
}
