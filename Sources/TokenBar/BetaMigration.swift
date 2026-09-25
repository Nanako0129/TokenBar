import AppKit

/// One-shot import of settings from the retired beta identity
/// (com.nyanako.tokenbar.beta, "TokenBar Beta.app"). Runs before anything
/// reads defaults so the first launch of the stable app keeps the user's
/// tray mode, icon style, quota source, chart view, orbit camera, etc.
///
/// Only `tokenbar.*` keys are copied (everything we own is under that
/// prefix), existing values in the stable domain are never overwritten,
/// and a marker key makes the whole thing run at most once. The shared
/// pace-history file lives at data_dir/com.nyanako.tokenbar for both
/// identities, so it needs no migration.
enum BetaMigration {
    private static let markerKey = "tokenbar.migratedFromBeta"
    private static let betaDomain = "com.nyanako.tokenbar.beta"

    static func runIfNeeded() {
        let defaults = UserDefaults.standard
        guard !defaults.bool(forKey: markerKey) else { return }
        defaults.set(true, forKey: markerKey)

        guard let beta = UserDefaults(suiteName: betaDomain) else { return }
        var copied = 0
        for (key, value) in beta.dictionaryRepresentation()
        where key.hasPrefix("tokenbar.") && defaults.object(forKey: key) == nil {
            defaults.set(value, forKey: key)
            copied += 1
        }
        if copied > 0 {
            NSLog("TokenBar: imported \(copied) settings from the beta app")
        }
    }
}

/// The retired beta app (com.nyanako.tokenbar.beta) can't auto-update across
/// to the stable identity (com.nyanako.tokenbar) — Sparkle refuses to install
/// over a different bundle id. So the final beta build (a 1.0+ version still
/// carrying the .beta id) ships this bridge: it's the full 1.0 app, plus a
/// one-tap "switch to the release build" that runs the Homebrew cask install
/// (which lays down the stable app, handles Gatekeeper, and whose first launch
/// imports these very settings via BetaMigration) and quits the beta.
enum BridgeBuild {
    static let installCommand = "brew install --cask nanako0129/tokenbar/tokenbar"

    /// True when running as a 1.0+ build that still carries the beta id —
    /// i.e. a beta-channel install that should graduate to the release app.
    static var isActive: Bool {
        guard Bundle.main.bundleIdentifier == "com.nyanako.tokenbar.beta" else { return false }
        let v = AppInfo.version
        // Any 1.x or higher on the beta id is the graduation build; betas are
        // "1.0.0-beta.N", which sort below "1.0.0" but still start with "1.".
        return !v.hasPrefix("0.") && !v.contains("-beta")
    }

    /// Graduate to the release app. If it's already installed (e.g. the user
    /// also came over via the Tauri updater), just launch it and quit —
    /// running `brew install` would fail "already installed". Otherwise run
    /// the cask install in Terminal (beta installs always have Homebrew —
    /// that's how they got here), then quit so the freshly-installed release
    /// app (same data dir, settings imported on first launch) takes over.
    @MainActor static func switchToRelease() {
        let releasePath = "/Applications/TokenBar.app"
        if FileManager.default.fileExists(atPath: releasePath) {
            NSWorkspace.shared.open(URL(fileURLWithPath: releasePath))
            NSApp.terminate(nil)
            return
        }
        // Write a .command script and open it: Terminal runs a double-clicked
        // .command without the Automation permission that an ad-hoc app needs
        // to script Terminal directly (which macOS silently denies — the old
        // AppleScript path just no-op'd for everyone).
        let script = """
        #!/bin/bash
        echo "Installing TokenBar 1.0…"
        \(installCommand)
        open -a TokenBar 2>/dev/null
        pkill -f "TokenBar Beta.app/Contents/MacOS" 2>/dev/null
        echo
        echo "Done — TokenBar 1.0 is installed and launched."
        echo "You can close this window and drag TokenBar Beta to the Trash."
        """
        let path = (NSTemporaryDirectory() as NSString)
            .appendingPathComponent("tokenbar-switch.command")
        do {
            try script.write(toFile: path, atomically: true, encoding: .utf8)
            try FileManager.default.setAttributes(
                [.posixPermissions: 0o755], ofItemAtPath: path)
            NSWorkspace.shared.open(URL(fileURLWithPath: path))
        } catch {
            NSLog("TokenBar bridge: could not write switch script: \(error)")
        }
    }
}
