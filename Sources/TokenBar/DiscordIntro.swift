import AppKit
import TokenBarCore

/// The one-time card that makes the Discord Rich Presence feature findable.
///
/// It exists because the feature is default-off and lives inside a Settings
/// section nobody has a reason to open. It does **not** enable anything, and
/// there is deliberately no path from here to on: the only route is the
/// Settings toggle, because that is where the full disclosure is. A card with
/// an enable button collects consent against the card's three sentences
/// instead of against the disclosure — which is the dark pattern this exists
/// to avoid, not a shortcut around it.
///
/// The motivation for building this feature and the user's privacy interest
/// point in opposite directions, and this card is where that conflict lands.
/// So it describes what the feature does and never what the project gets from
/// it, and it is not shorter or friendlier than the disclosure in a way the
/// disclosure would then have to argue against.
enum DiscordIntro {
    /// Set when the card is PRESENTED, not when it is acted on.
    ///
    /// Writing it only on the preferred action means the card returns until
    /// the user complies. Dismiss, Esc, and quitting while it is on screen all
    /// count as shown.
    ///
    /// Deliberately **not** versioned. A versioned flag turns every release
    /// into a fresh consent-adjacent interruption — a reusable notification
    /// channel pointed at the user, which is a different product than a
    /// one-time introduction.
    static let shownKey = "tokenbar.discord.introShown"

    /// Once, ever, and never to someone already using the feature: there is
    /// nothing to introduce, and interrupting them would be pure noise.
    ///
    /// `object(forKey:) as? Bool` rather than `bool(forKey:)` for the same
    /// reason the switch itself uses it — the two should not read their own
    /// preferences by different rules.
    static func shouldPresent(defaults: UserDefaults = .standard) -> Bool {
        guard defaults.object(forKey: shownKey) as? Bool != true else { return false }
        return !DiscordPresence.enabled(defaults: defaults)
    }

    static func markShown(defaults: UserDefaults = .standard) {
        defaults.set(true, forKey: shownKey)
    }

    /// What the two buttons do. Neither writes `DiscordPresence.enabledKey`,
    /// and that is the contract the assertion pins: opening Settings is a
    /// navigation, not a consent.
    enum Choice {
        case openSettings
        case notNow
    }

    /// Separated from the AppKit presentation so the contract is reachable
    /// without an app lifecycle — `SelfTest.run()` returns `Never` before
    /// `NSApplication.shared` exists, and a rule that lives only inside a
    /// modal callback cannot be asserted at all.
    static func perform(_ choice: Choice, openSettings: () -> Void) {
        switch choice {
        case .openSettings: openSettings()
        case .notNow: break
        }
    }

    @MainActor
    static func presentIfNeeded() {
        guard shouldPresent() else { return }
        // Before the modal runs, not after: quitting while it is up counts.
        markShown()

        let alert = NSAlert()
        alert.messageText = "Show today's usage on Discord".localized
        alert.informativeText = "TokenBar can publish today's tokens, a client name and a cost range to your Discord profile. It stays off until you turn it on in Settings, where the full disclosure is. Anyone who can see your profile can read and keep every update, and switching it off later cannot unshare what already went out.".localized
        let settings = alert.addButton(withTitle: "Open Settings".localized)
        let notNow = alert.addButton(withTitle: "Not now".localized)
        // Neither button is the default. A filled, Return-bound button next to
        // a plain one is a thumb on the scale, and the whole point of this card
        // is that it does not have one. Esc closes, as it must.
        settings.keyEquivalent = ""
        notNow.keyEquivalent = "\u{1b}"

        let choice: Choice = alert.runModal() == .alertFirstButtonReturn ? .openSettings : .notNow
        perform(choice) { SettingsWindowController.shared.show() }
    }
}
