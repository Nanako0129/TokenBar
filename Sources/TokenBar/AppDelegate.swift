import AppKit
import TokenBarCore

/// App bootstrap: accessory activation policy (menu-bar only, no Dock icon),
/// the status-item controller, and the tray-title refresh loop.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private static let titleRefreshSecs: UInt64 = 300

    private var statusController: StatusItemController?
    private var trayAnimator: TrayAnimator?
    private var titleRefreshTask: Task<Void, Never>?
    private var defaultsObserver: NSObjectProtocol?
    // Last good fetches — a failed refresh keeps showing these.
    private var lastGraph: UsagePayload?
    private var lastRate: Double?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        BetaMigration.runIfNeeded() // before anything reads defaults
        _ = UpdaterService.shared // arm Sparkle when bundled

        let controller = StatusItemController()
        statusController = controller
        let animator = TrayAnimator(controller: controller)
        trayAnimator = animator
        controller.quotaPayloadProvider = { [weak animator] in animator?.quota }
        // A fresh quota or rate fetch re-renders the title right away.
        animator.onQuotaUpdated = { [weak self] in self?.applyTitle() }
        animator.start()
        startTitleRefresh()

        // Re-render the title the moment a setting changes (tray mode, quota
        // source from the right-click menu or the panel). Cheap: recomputes
        // from cached data only.
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { _ in
            MainActor.assumeIsolated {
                (NSApp.delegate as? AppDelegate)?.applyTitle()
            }
        }

        // Debug hooks: `--open-popover` shows the popover shortly after
        // launch, `--settings` the settings window — both screenshot aids.
        if CommandLine.arguments.contains("--open-popover") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                controller.showPopover()
            }
        }
        if CommandLine.arguments.contains("--settings") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                SettingsWindowController.shared.show()
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        titleRefreshTask?.cancel()
        trayAnimator?.stop()
        if let defaultsObserver { NotificationCenter.default.removeObserver(defaultsObserver) }
        // Remove the status item / close the popover so ControlCenter tears
        // the menu-bar item down cleanly (avoids the ~40s RunningBoard
        // "waiting on exit context" stall seen on the 2026-06-16 quit).
        statusController?.tearDown()
    }

    /// Compose the tray title from the cached data and the current settings.
    /// The rate prefers the animator's 30s-fresh value over lastRate (which
    /// is only updated on the 5-minute title-refresh cycle).
    private func applyTitle() {
        let mode = TrayMode.current
        let quotaRemaining = trayAnimator?.quotaRemaining
        let rate = trayAnimator?.tokensPerMinRate ?? lastRate
        statusController?.updateTitle(
            mode.title(graph: lastGraph, tokensPerMin: rate, quotaRemaining: quotaRemaining),
            color: mode.titleColor(quotaRemaining: quotaRemaining))
    }

    /// Background graph refresh: serves the graph-based title modes (today's
    /// tokens/cost, total tokens/cost). The rate and quota title modes are
    /// covered by TrayAnimator's load/quota polling via onQuotaUpdated.
    /// A full log re-read (tb_refresh_graph) is forced every "Data refresh"
    /// interval from settings; between forced refreshes the staticlib's
    /// mtime-aware cache makes ticks cheap.
    private func startTitleRefresh() {
        titleRefreshTask = Task { [weak self] in
            var lastFullRefresh = Date.distantPast
            while !Task.isCancelled {
                let mode = TrayMode.current
                let intervalMin = max(1, UserDefaults.standard.object(forKey: "tokenbar.refresh.intervalMin")
                    .flatMap { $0 as? Int } ?? 30)
                let forceRefresh = Date().timeIntervalSince(lastFullRefresh) >= Double(intervalMin) * 60
                let graph = try? await Task.detached(priority: .utility) {
                    forceRefresh ? try TBCore.refreshGraph() : try TBCore.graph()
                }.value
                if forceRefresh && graph != nil { lastFullRefresh = Date() }
                // Failed refreshes keep the last good numbers — the title
                // must never blank/zero out on a transient error.
                if let graph { self?.lastGraph = graph }
                if mode == .tokensPerMin {
                    let rate = try? await Task.detached(priority: .utility) {
                        try TBCore.tokensPerMin()
                    }.value
                    if let rate { self?.lastRate = rate }
                }
                guard !Task.isCancelled else { break }
                self?.applyTitle()
                try? await Task.sleep(for: .seconds(Double(Self.titleRefreshSecs)))
            }
        }
    }
}
