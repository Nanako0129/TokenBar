import AppKit
import Observation
import ServiceManagement
import Sparkle

/// Sparkle auto-updates. Only armed when running from an assembled .app
/// bundle — the bare SwiftPM executable (dev runs, --selftest, CI) has no
/// Info.plist feed/key and Sparkle would refuse to start.
///
/// Besides the interactive check, a silent probe runs at launch and every
/// six hours; when it finds a newer version the popover footer grows a
/// one-click update button bound to `availableVersion`.
@MainActor
@Observable
final class UpdaterService: NSObject, SPUUpdaterDelegate {
    static let shared = UpdaterService()

    static var isAvailable: Bool { Bundle.main.bundleURL.pathExtension == "app" }

    /// Display version of an available update (nil = up to date / unknown).
    private(set) var availableVersion: String?

    @ObservationIgnored private var controller: SPUStandardUpdaterController?
    @ObservationIgnored private var probeTask: Task<Void, Never>?

    private override init() {
        super.init()
        guard Self.isAvailable else { return }
        controller = SPUStandardUpdaterController(
            startingUpdater: true, updaterDelegate: self, userDriverDelegate: nil)
        probeTask = Task { [weak self] in
            // Let the updater session finish starting before the first probe
            // (a check fired straight from init gets swallowed), retry once
            // shortly after, then settle into a slow cadence.
            try? await Task.sleep(for: .seconds(5))
            while !Task.isCancelled {
                self?.controller?.updater.checkForUpdateInformation()
                try? await Task.sleep(for: .seconds(60))
                if self?.availableVersion != nil { break }
            }
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(6 * 3600))
                self?.controller?.updater.checkForUpdateInformation()
            }
        }
    }

    /// The interactive Sparkle flow (update dialog with install button).
    func checkForUpdates() {
        controller?.checkForUpdates(nil)
    }

    // MARK: - SPUUpdaterDelegate

    /// Prerelease appcast items are tagged <sparkle:channel>beta</sparkle:channel>;
    /// only installs that opt in (Settings → "Receive beta updates") see them.
    nonisolated func allowedChannels(for updater: SPUUpdater) -> Set<String> {
        UserDefaults.standard.bool(forKey: "tokenbar.updates.beta") ? ["beta"] : []
    }

    nonisolated func updater(_ updater: SPUUpdater, didFindValidUpdate item: SUAppcastItem) {
        let version = item.displayVersionString
        Task { @MainActor in self.availableVersion = version }
    }

    nonisolated func updaterDidNotFindUpdate(_ updater: SPUUpdater) {
        Task { @MainActor in self.availableVersion = nil }
    }
}

/// Launch-at-login via SMAppService — also bundle-only (the bare executable
/// has no main-app service identity to register).
@MainActor
enum AutostartService {
    static var isAvailable: Bool { UpdaterService.isAvailable }

    nonisolated static func readEnabled() async -> Bool {
        await Task.detached(priority: .utility) {
            SMAppService.mainApp.status == .enabled
        }.value
    }

    @discardableResult
    static func setEnabled(_ enabled: Bool) -> Bool {
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            return true
        } catch {
            return false
        }
    }
}
