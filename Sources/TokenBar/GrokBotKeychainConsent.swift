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

    /// Record the user's answer and make it take effect now — BOTH answers.
    ///
    /// An earlier version installed only grants and returned early on a
    /// decline, on the reasoning that the registry is already empty. That is
    /// true exactly once. After a grant it is wrong twice over: declining left
    /// the process still reading the Keychain until the next launch, and —
    /// because the install is asynchronous — clicking Allow then Not now let
    /// the queued grant land after the refusal was stored, so `UserDefaults`
    /// said no while the registry said yes. Routing both answers through the
    /// same serial queue makes the last click win, which is the only rule a
    /// user can predict.
    ///
    /// What the persisted write buys separately is that the answer STICKS
    /// across launches: a terminal provider failure does not suppress retries,
    /// so without a remembered "no" every poll would put the dialog back (60s
    /// with the popover open, 5 min from the tray).
    /// `setConsent` is injectable here and not only on `apply` because this is
    /// the production entry point — the buttons call this, nothing calls
    /// `apply` directly. A test that drove `apply` instead could not observe
    /// this function's own decisions at all: reverting it to install grants
    /// only left such a test green while the defect was fully present.
    static func answer(
        _ granted: Bool,
        defaults: UserDefaults = .standard,
        setConsent: (@Sendable (String) -> Void)? = nil
    ) {
        defaults.set(granted, forKey: storageKey)
        if let setConsent {
            apply(granted: granted, setConsent: setConsent)
        } else {
            apply(granted: granted)
        }
    }

    /// Re-install a previously granted answer into the core registry. Called
    /// at launch, where only the granted case does anything: the registry
    /// starts empty, so denying it again would be a call and a wake that
    /// change nothing.
    static func applyIfGranted(defaults: UserDefaults = .standard) {
        guard answer(defaults: defaults) == true else { return }
        apply(granted: true)
    }

    /// Withdraw a grant that did not produce access.
    ///
    /// The core reports `source == "keychain-denied"` when the user gave
    /// permission in the app but macOS did not grant it — they pressed Deny,
    /// or left the dialog unanswered past the adapter's 25s bound. Leaving the
    /// grant in place would be the original bug wearing a new hat: a terminal
    /// provider failure does not suppress retries, so the next poll would read
    /// the Keychain again and reopen the dialog, every 60s with the popover
    /// open and every 5 minutes from the tray.
    ///
    /// Reverting to `false` rather than to "never asked" is deliberate. The
    /// user HAS answered, and the card says so; what they have not done is
    /// complete the OS half, which the prompt now offers to retry. Never asked
    /// would show them the full explanation again as if nothing had happened.
    ///
    /// Idempotent: every payload passes through here, and only the first one
    /// carrying the marker does any work.
    static func revokeIfAccessWasDenied(
        _ payload: AgentUsagePayload,
        defaults: UserDefaults = .standard
    ) {
        let denied = payload.agents.contains {
            $0.clientId == "grok-bot" && $0.source == "keychain-denied"
        }
        guard denied, answer(defaults: defaults) == true else { return }
        answer(false, defaults: defaults)
    }

    /// Payloads for the one wired client. Exposed for the selftest, which
    /// asserts the exact JSON rather than that "a call happened" — the core
    /// rejects an unknown client id, so a typo here would produce a grant that
    /// is never honoured and a card that never fills.
    static let grantedPayload = #"{"grok-bot":true}"#
    /// Full-replace with nothing, which is how the core spells "revoked".
    static let deniedPayload = "{}"

    /// Install the answer and make the quota cards notice.
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
        granted: Bool = true,
        setConsent: @escaping @Sendable (String) -> Void = {
            _ = try? TBCore.setKeychainConsent(json: $0)
        }
    ) {
        let payload = granted ? grantedPayload : deniedPayload
        applyQueue.async {
            // Compared on the queue, not before it, or two fast clicks both
            // read the same stale value and the later one is dropped. The
            // registry starts empty every launch, so the initial `nil` means
            // "denied" and a decline from a process that never granted
            // correctly does nothing at all — no FFI call, no wake.
            guard lastInstalledPayload != payload else { return }
            let hadInstalled = lastInstalledPayload != nil
            lastInstalledPayload = payload
            guard granted || hadInstalled else { return }
            setConsent(payload)
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
    ///
    /// Serial is also what makes the last click win: `answer` writes
    /// `UserDefaults` synchronously but installs asynchronously, so two clicks
    /// in quick succession must reach the core in the order they were made or
    /// the persisted answer and the registry disagree.
    private static let applyQueue = DispatchQueue(
        label: "com.nyanako.tokenbar.grok-bot-keychain-consent", qos: .userInitiated)

    /// What this process last handed the core, or `nil` if it never called the
    /// setter. Touched only from `applyQueue`, which is what makes a plain
    /// `static var` safe here and why the comparison lives inside the block.
    nonisolated(unsafe) private static var lastInstalledPayload: String?

    /// Lets the selftest drive the grant-then-decline sequence from a known
    /// starting point, since the registry is process-wide.
    ///
    /// Not behind `#if DEBUG`: the suite runs from the release binary too —
    /// that is what `make selftest-bundled` is — so a debug-only helper makes
    /// `SelfTest.swift` fail to compile in the release configuration, which is
    /// how this reached a pushed tag. The other `ForTesting` helpers in
    /// `DashboardModel`, `AttributedSeriesModel`, `ClaudeExtraRoots` and
    /// `DiscordIPC` carry no such guard for the same reason.
    static func resetInstalledPayloadForTesting() {
        applyQueue.sync { lastInstalledPayload = nil }
    }
}
