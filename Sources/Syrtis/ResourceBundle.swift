import Foundation

extension Bundle {
    /// The SwiftPM resource bundle, located the *app-bundle* way.
    ///
    /// SwiftPM's generated `Bundle.module` accessor for executable targets
    /// only checks the main bundle's root URL and the absolute build-time
    /// path — the latter resolves on the build machine (so dev runs work)
    /// but is /Users/runner/… garbage in CI artifacts, and the former isn't
    /// where a proper .app keeps resources. Check Contents/Resources first,
    /// fall back to Bundle.module for bare `swift run` / selftest runs.
    static let tokenBarResources: Bundle = {
        if let url = Bundle.main.resourceURL?
            .appendingPathComponent("Syrtis_Syrtis.bundle"),
            let bundle = Bundle(url: url)
        {
            return bundle
        }
        return Bundle.module
    }()
}
