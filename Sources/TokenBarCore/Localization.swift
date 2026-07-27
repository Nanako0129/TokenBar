import Foundation

extension String {
    /// Looks this string up in the main bundle's `Localizable.strings`.
    ///
    /// The English source text *is* the key, so a missing entry renders exactly
    /// as it did before i18n — there is no half-translated state.
    ///
    /// Lives in TokenBarCore because pace copy (`UsagePace`) is assembled here,
    /// and because `CrossCheckHarness` links this module: the harness compares
    /// shipping output byte-for-byte against the C# port, so it must resolve the
    /// same strings the app does — under a pinned `en` it gets the English
    /// fallback, which is identical to the pre-i18n text.
    public var localized: String { NSLocalizedString(self, comment: "") }

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
            format: NSLocalizedString(self, value: fallback, comment: ""),
            arguments: arguments)
    }
}
