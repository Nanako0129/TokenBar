import Foundation

private enum TokenBarLocalization {
    /// SwiftPM puts executable-target resources in a sibling bundle. A
    /// packaged app gets the same .lproj directories copied into its main
    /// bundle, so check both locations and keep the lookup independent of the
    /// caller module.
    private static let bundles: [Bundle] = {
        var result = [Bundle.main]
        if let resourceURL = Bundle.main.resourceURL?
            .appendingPathComponent("TokenBar_TokenBar.bundle"),
           let resourceBundle = Bundle(url: resourceURL)
        {
            result.append(resourceBundle)
        }
        return result
    }()

    static func string(forKey key: String) -> String {
        for bundle in bundles {
            let value = bundle.localizedString(forKey: key, value: nil, table: nil)
            if value != key { return value }
        }
        return key
    }

    static func string(forKey key: String, fallback: String) -> String {
        for bundle in bundles {
            let value = bundle.localizedString(forKey: key, value: nil, table: nil)
            if value != key { return value }
        }
        return fallback
    }
}

extension String {
    /// Looks this string up in the main bundle or SwiftPM resource bundle's
    /// `Localizable.strings`.
    ///
    /// The English source text *is* the key, so a missing entry renders exactly
    /// as it did before i18n — there is no half-translated state.
    ///
    /// Lives in TokenBarCore because pace copy (`UsagePace`) is assembled here,
    /// and because `CrossCheckHarness` links this module: the harness compares
    /// shipping output byte-for-byte against the C# port, so it must resolve the
    /// same strings the app does — under a pinned `en` it gets the English
    /// fallback, which is identical to the pre-i18n text.
    public var localized: String { TokenBarLocalization.string(forKey: self) }

    /// `localized` plus `String(format:)`, for entries carrying `%@` / `%lld`.
    public func localized(_ arguments: any CVarArg...) -> String {
        String(format: localized, arguments: arguments)
    }

    /// Lookup under a semantic key with an explicit English fallback.
    ///
    /// For the cases where English source text cannot serve as the key because
    /// two different strings render identically — `Format.monthDay` and
    /// `monthYear` are both `%1$@ %2$lld` in English but reorder in Chinese.
    /// `value:` is what keeps a missing entry rendering as English instead of
    /// leaking the key.
    public func localized(default fallback: String, _ arguments: any CVarArg...) -> String {
        String(
            format: TokenBarLocalization.string(forKey: self, fallback: fallback),
            arguments: arguments)
    }
}
