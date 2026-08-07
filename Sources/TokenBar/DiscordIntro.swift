import AppKit
import SwiftUI
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
    /// Whether to present — and it CONSUMES the one-time flag either way.
    ///
    /// Folding the decision and the marking together is what makes "once,
    /// ever" true on the upgrade path. Someone who already had the feature on
    /// when this card shipped has nothing to be introduced to, so they do not
    /// see it; but if the flag were left unwritten, switching Discord off later
    /// would introduce them to a feature they had used and deliberately turned
    /// off. Skipping is a consumption, not a deferral.
    static func consume(defaults: UserDefaults = .standard) -> Bool {
        guard defaults.object(forKey: shownKey) as? Bool != true else { return false }
        markShown(defaults: defaults)
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

    /// A mock of the activity itself. Text alone made the card read as a
    /// warning about a feature rather than an introduction to one, and a
    /// picture of the actual thing is also the more honest surface: seeing the
    /// four lines that would appear says more than a sentence describing them.
    ///
    /// Representative values, labelled as a preview. Not the user's real
    /// figures — the graph has not necessarily loaded 1.5s after launch, and a
    /// card that showed a number and then published a different one would be
    /// worse than one that showed none.
    @MainActor
    private static func previewView() -> NSView {
        let preview = DiscordPresencePreview(
            title: "TokenBar".localized,
            details: "1.2M tokens today".localized,
            state: "Claude Code · $10-50".localized,
            button: DiscordIPC.buttonLabel.localized)
        let host = NSHostingView(rootView: preview)
        host.frame = NSRect(x: 0, y: 0, width: 300, height: 104)
        return host
    }

    @MainActor
    static func presentIfNeeded() {
        // Consumed here, before the modal runs: quitting while the card is up
        // counts as shown, and so does skipping it for an existing user.
        guard consume() else { return }

        let alert = NSAlert()
        alert.messageText = "Show today's usage on Discord".localized
        alert.informativeText = "Your Discord profile can show what you have been building today. Pick exactly what appears — or nothing at all — in Settings.".localized
        alert.accessoryView = previewView()
        let settings = alert.addButton(withTitle: "Open Settings".localized)
        let notNow = alert.addButton(withTitle: "Not now".localized)
        // Neither button is the default. A filled, Return-bound button next to
        // a plain one is a thumb on the scale, and the whole point of this card
        // is that it does not have one. Esc closes, as it must.
        settings.keyEquivalent = ""
        notNow.keyEquivalent = "\u{1b}"

        let choice: Choice = alert.runModal() == .alertFirstButtonReturn ? .openSettings : .notNow
        perform(choice) { SettingsWindowController.shared.show(scrollingTo: .discord) }
    }
}

/// What the presence looks like on a profile, laid out the way Discord lays it
/// out: art on the left, the app name, then `details` and `state`, with the
/// button underneath.
private struct DiscordPresencePreview: View {
    let title: String
    let details: String
    let state: String
    let button: String

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            // The app's own icon, because that is literally what Discord
            // shows: `largeImageKey` resolves to the TokenBar art uploaded to
            // the Developer Portal. A stand-in symbol would make the preview
            // decorative rather than accurate.
            Image(nsImage: NSApp.applicationIconImage)
                .resizable()
                .frame(width: 56, height: 56)
                .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.system(size: 12, weight: .bold))
                Text(details).font(.system(size: 11))
                Text(state).font(.system(size: 11)).foregroundStyle(.secondary)
                Text(button)
                    .font(.system(size: 10, weight: .medium))
                    .padding(.horizontal, 8)
                    .padding(.vertical, 3)
                    .frame(maxWidth: .infinity)
                    .background(
                        RoundedRectangle(cornerRadius: 4).fill(Color.secondary.opacity(0.18)))
                    .padding(.top, 3)
            }
            Spacer(minLength: 0)
        }
        .padding(10)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(Color.secondary.opacity(0.10)))
        .frame(width: 300, alignment: .leading)
    }
}
