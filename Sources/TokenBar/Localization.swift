import AppKit
import Foundation
import TokenBarCore

/// UI language override. `system` defers to macOS; the others pin
/// `AppleLanguages` so the choice survives a relaunch.
///
/// Cocoa reads `AppleLanguages` once during startup, so a change only takes
/// effect on the next launch. Live switching would mean rebuilding every view
/// against a swapped bundle — not worth it for a setting flipped once.
///
/// The `String.localized` lookup helpers live in `TokenBarCore`, since pace
/// copy is assembled there and the cross-check harness links that module.
enum AppLanguage: String, CaseIterable {
    case system
    case english = "en"
    case traditionalChinese = "zh-Hant"

    static let storageKey = "tokenbar.language"
    private static let appleLanguagesKey = "AppleLanguages"

    var label: String {
        switch self {
        case .system: return "System".localized
        case .english: return "English"
        case .traditionalChinese: return "繁體中文"
        }
    }

    /// A language change needs a relaunch, but selecting the current value
    /// again (or receiving a malformed raw value) must not prompt.
    static func requiresRelaunch(from currentRawValue: String, to nextRawValue: String) -> Bool {
        currentRawValue != nextRawValue && Self(rawValue: nextRawValue) != nil
    }

    /// Writes (or clears) the `AppleLanguages` override.
    func apply(to defaults: UserDefaults = .standard) {
        switch self {
        case .system:
            defaults.removeObject(forKey: Self.appleLanguagesKey)
        default:
            defaults.set([rawValue], forKey: Self.appleLanguagesKey)
        }
    }

    /// Bare SwiftPM executables use Bundle.main for SwiftUI's implicit
    /// `LocalizedStringKey` lookup, while SwiftPM stores target resources in a
    /// sibling bundle. Stage those .lproj directories beside the executable
    /// before any views are created. Packaged .app runs already have them in
    /// the main bundle and are left untouched.
    static func prepareDirectRunResources() {
        guard Bundle.main.bundleURL.pathExtension != "app",
              let resourceURL = Bundle.main.resourceURL,
              let packageBundle = Bundle(
                url: resourceURL.appendingPathComponent("TokenBar_TokenBar.bundle")),
              let packageResourceURL = packageBundle.resourceURL
        else { return }

        let fileManager = FileManager.default
        for locale in ["en", "zh-Hant"] {
            let source = packageResourceURL.appendingPathComponent("\(locale).lproj")
            let destination = resourceURL.appendingPathComponent("\(locale).lproj")
            guard fileManager.fileExists(atPath: source.path) else { continue }
            if fileManager.fileExists(atPath: destination.path) {
                try? fileManager.removeItem(at: destination)
            }
            try? fileManager.copyItem(at: source, to: destination)
        }
    }
}

@MainActor
enum AppRelauncher {
    /// Starts a new app instance before terminating this one. Waiting for
    /// NSWorkspace's completion keeps a failed launch from quitting the
    /// currently working app.
    static func relaunch() {
        guard Bundle.main.bundleURL.pathExtension == "app" else {
            NSSound.beep()
            return
        }

        let configuration = NSWorkspace.OpenConfiguration()
        configuration.createsNewApplicationInstance = true
        NSWorkspace.shared.openApplication(
            at: Bundle.main.bundleURL,
            configuration: configuration
        ) { application, error in
            let openedReplacement =
                error == nil
                && application?.processIdentifier != nil
                && application?.processIdentifier != ProcessInfo.processInfo.processIdentifier
            Task { @MainActor in
                guard openedReplacement else {
                    NSSound.beep()
                    return
                }
                NSApp.terminate(nil)
            }
        }
    }
}
