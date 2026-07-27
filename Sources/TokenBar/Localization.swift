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

    /// Writes (or clears) the `AppleLanguages` override.
    func apply(to defaults: UserDefaults = .standard) {
        switch self {
        case .system:
            defaults.removeObject(forKey: Self.appleLanguagesKey)
        default:
            defaults.set([rawValue], forKey: Self.appleLanguagesKey)
        }
    }
}
