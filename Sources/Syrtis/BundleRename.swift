import Foundation

/// Renames the installed bundle from `TokenBar.app` to `Syrtis.app` on the
/// first launch after the Syrtis rename, then relaunches from the new path.
///
/// Sparkle names the installed bundle after the bundle that ran the update
/// (its `SUBundleName`, else `CFBundleName`), so an update from a pre-rename
/// build lands v2.0 at `…/TokenBar.app`, and only a later update would fix
/// the name. Doing it here renames every user on their first v2.0 launch,
/// whichever version they came from.
///
/// Runs from main.swift before `NSApplication` exists, so nothing has loaded
/// a resource through the old path yet. Every failure leaves the bundle where
/// it was and the launch continues there: the worst case is the old file
/// name, which is what the app had before this existed.
///
/// Measured on macOS 27 with a stand-in bundle (spike in `.agent-local`,
/// 2026-09-26): the move plus `open -n` relaunch works, an SMAppService login
/// item registered at the old path follows the move (one BTM record, URL
/// updated, still launched at the next login), `open -b` resolves to the new
/// path, and an unwritable parent is skipped. The Dock tile is repointed by
/// `DockPinRepair` in the relaunched process.
///
/// Not `#if DEBUG`: `make selftest-bundled` runs the release binary.
enum BundleRename {
    static let oldLeaf = "TokenBar.app"
    static let newLeaf = "Syrtis.app"

    /// Pure decision: the URL to move the bundle to, or nil to launch in place.
    static func destination(
        bundleURL: URL,
        arguments: [String],
        exists: (String) -> Bool,
        writable: (String) -> Bool
    ) -> URL? {
        // A demo/selftest launch of an installed bundle is not a user session.
        guard !BuildIdentity.isNonUserRuntime(arguments),
              bundleURL.lastPathComponent == oldLeaf,
              // A translocated copy is a read-only mirror; moving it is meaningless.
              !bundleURL.path.contains("/AppTranslocation/")
        else { return nil }
        let parent = bundleURL.deletingLastPathComponent()
        let target = parent.appendingPathComponent(newLeaf, isDirectory: true)
        // Never replace an existing Syrtis.app; a standard user cannot write
        // /Applications and keeps the old name.
        guard !exists(target.path), writable(parent.path) else { return nil }
        return target
    }

    /// Returns only if the app should keep launching from where it is.
    static func runIfNeeded() {
        let fm = FileManager.default
        let old = Bundle.main.bundleURL
        guard let target = destination(
            bundleURL: old, arguments: CommandLine.arguments,
            exists: { fm.fileExists(atPath: $0) }, writable: { fm.isWritableFile(atPath: $0) })
        else { return }
        do {
            try fm.moveItem(at: old, to: target)
        } catch {
            NSLog("Syrtis: bundle rename skipped: \(error)")
            return
        }
        // -n: this process holds the same bundle id, so without it Launch
        // Services may hand back this instance instead of starting a new one.
        let open = Process()
        open.executableURL = URL(fileURLWithPath: "/usr/bin/open")
        open.arguments = ["-n", target.path]
        var relaunched = false
        do {
            try open.run()
            open.waitUntilExit()
            relaunched = open.terminationStatus == 0
        } catch {
            NSLog("Syrtis: relaunch after bundle rename failed: \(error)")
        }
        if relaunched { exit(0) }
        // Put the bundle back so this process's resource paths resolve again.
        do {
            try fm.moveItem(at: target, to: old)
        } catch {
            NSLog("Syrtis: could not restore bundle after failed relaunch: \(error)")
        }
    }

    static func selfTest(_ expect: (Bool, String) -> Void) {
        let old = URL(fileURLWithPath: "/Applications/TokenBar.app", isDirectory: true)
        func decide(
            _ url: URL = old, args: [String] = ["/Applications/TokenBar.app/Contents/MacOS/Syrtis"],
            exists: Bool = false, writable: Bool = true
        ) -> URL? {
            destination(bundleURL: url, arguments: args,
                        exists: { _ in exists }, writable: { _ in writable })
        }
        expect(decide()?.path == "/Applications/Syrtis.app",
               "BUNDLE-RENAME control: TokenBar.app in a writable folder moves to Syrtis.app beside it")
        expect(decide(URL(fileURLWithPath: "/Users/a/Apps/TokenBar.app"))?.path == "/Users/a/Apps/Syrtis.app",
               "BUNDLE-RENAME the new bundle stays in the folder the user installed to")
        expect(decide(URL(fileURLWithPath: "/Applications/Syrtis.app")) == nil,
               "BUNDLE-RENAME an already renamed bundle launches in place")
        expect(decide(URL(fileURLWithPath: "/Applications/TokenBar Dev.app")) == nil,
               "BUNDLE-RENAME only the exact old file name is renamed")
        expect(decide(exists: true) == nil,
               "BUNDLE-RENAME an existing Syrtis.app is never replaced")
        expect(decide(writable: false) == nil,
               "BUNDLE-RENAME an unwritable folder (standard user) keeps the old name")
        expect(decide(URL(fileURLWithPath: "/private/var/folders/x/AppTranslocation/1/d/TokenBar.app")) == nil,
               "BUNDLE-RENAME a translocated copy is not moved")
        expect(decide(args: ["/Applications/TokenBar.app/Contents/MacOS/Syrtis", "--demo"]) == nil,
               "BUNDLE-RENAME a non-user runtime (--demo) never moves the installed bundle")
    }
}
