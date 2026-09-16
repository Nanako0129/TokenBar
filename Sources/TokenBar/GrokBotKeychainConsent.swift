import Foundation
import TokenBarCore

/// Whether the user has agreed to let TokenBar read the Grok Bot login from
/// the macOS Keychain, and the wiring that carries that answer into the core.
///
/// The core registry (`tb_set_keychain_consent`) is an in-memory `RwLock` that
/// starts empty every launch, so `UserDefaults` is the source of truth Swift
/// owns — the same split `ClaudeExtraRoots` uses, for the same reason.
///
/// **Three states here, two in the core.** "Never asked" and "declined" are
/// the same instruction to the adapter — do not touch the Keychain — so the
/// core stores only grants. Swift keeps the distinction because it chooses the
/// copy: a full explanation for someone who has not been asked, a one-line row
/// with a way back for someone who said no.
///
/// Deliberately NOT `DiscordIntro`'s "write the flag when PRESENTED" rule.
/// That flag suppresses a one-time interruption, so presenting it is the event
/// worth recording. This one records an ANSWER to a question; writing it on
/// presentation would record a decision the user never made, and the wrong one
/// in both directions — a granted-by-default read they never agreed to, or a
/// permanent decline from someone who closed the popover to think about it.
/// Not `@MainActor`: everything here is either an immutable constant or a
/// `UserDefaults` read/write, both of which are safe anywhere, and the smoke
/// path calls it off the main actor. Only the wake-up needs the main actor,
/// and it hops there explicitly.
enum GrokBotKeychainConsent {
    static let storageKey = "tokenbar.grokBot.keychainConsent"

    /// `object(forKey:) as? Bool` rather than `bool(forKey:)`, which cannot
    /// tell "no" from "not asked" — it answers `false` for both, and those two
    /// need different copy. Same convention as `DiscordIntro`.
    static func answer(defaults: UserDefaults = .standard) -> Bool? {
        defaults.object(forKey: storageKey) as? Bool
    }

    /// Record the user's answer and, if it is yes, make it take effect now.
    ///
    /// Declining makes no core call: the registry is already empty for this
    /// client, and there is nothing to wake because nothing will change. What
    /// the write buys is that the answer STICKS — a terminal provider failure
    /// does not suppress retries, so without a remembered "no" every poll
    /// would put the dialog back (60s with the popover open, 5 min from the
    /// tray).
    static func answer(_ granted: Bool, defaults: UserDefaults = .standard) {
        defaults.set(granted, forKey: storageKey)
        guard granted else { return }
        apply()
    }

    /// Re-install a previously granted answer into the core registry. Called
    /// at launch, where only the granted case does anything: an empty registry
    /// already denies, which is the correct behaviour for everyone else.
    static func applyIfGranted(defaults: UserDefaults = .standard) {
        guard answer(defaults: defaults) == true else { return }
        apply()
    }

    /// Payload for the one wired client. Exposed for the selftest, which
    /// asserts the exact JSON rather than that "a call happened" — the core
    /// rejects an unknown client id silently as far as this path is concerned,
    /// so a typo here would produce a grant that is never honoured and a card
    /// that never fills.
    static let grantedPayload = #"{"grok-bot":true}"#

    /// Install the grant and make the quota cards notice.
    ///
    /// The setter is a parameter with the real one as its default, the same
    /// seam `ClaudeExtraRoots.install(setConfigDirs:setScanPaths:)` uses: the
    /// property worth testing is an ORDER between the FFI call and the two
    /// wake-ups, and an order can only be observed by holding one side still.
    ///
    /// Never on the calling actor. The setter itself is cheap, but this runs
    /// from a button on the MainActor and the queue hop costs nothing; sharing
    /// the shape with its sibling is worth more than saving it.
    static func apply(
        setConsent: @escaping @Sendable (String) -> Void = {
            _ = try? TBCore.setKeychainConsent(json: $0)
        }
    ) {
        applyQueue.async {
            setConsent(grantedPayload)
            Task { @MainActor in
                // Invalidate BEFORE signalling, the same order and for the
                // same reason as `ClaudeExtraRoots.install`: a poll woken
                // while the throttle still holds a payload is answered from
                // the one built before consent existed, and the user watches
                // the card sit unchanged for up to the throttle floor after
                // clicking Allow.
                await AgentUsageThrottle.shared.invalidate()
                // Wakes both poll loops now rather than at the next 60s/5min
                // tick. Each captured the epoch before fetching, so a fetch
                // already in flight discards its own payload and refetches
                // instead of publishing one built without the grant.
                ClaudeExtraRoots.RegistryChange.signal()
            }
        }
    }

    /// Serial and separate from `ClaudeExtraRoots.applyQueue`: two registries
    /// with two consumers, and a consent install must not queue behind a scan
    /// path probe that can block for a mount timeout — the user just pressed a
    /// button and is waiting for a system dialog.
    private static let applyQueue = DispatchQueue(
        label: "com.nyanako.tokenbar.grok-bot-keychain-consent", qos: .userInitiated)
}
