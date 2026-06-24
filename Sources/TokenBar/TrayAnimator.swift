import AppKit
import TokenBarCore

/// RunCat-style menu-bar animation, port of src-tauri's animation.rs: the
/// cat (or parrot) spins faster as the live token rate climbs. Frame sets
/// come in dark/light pairs and follow the menu bar's effective appearance —
/// `anim-*` are white glyphs for a dark menu bar, `anim-*-light` black ones
/// for a light menu bar.
@MainActor
final class TrayAnimator {
    static let animateKey = "tokenbar.tray.animate"
    static let styleKey = "tokenbar.tray.animationStyle"

    static let quotaSourceKey = "tokenbar.quota.source"

    private weak var controller: StatusItemController?
    /// Frame sets keyed by "<style>|<dark|light>".
    private let frames: [String: [NSImage]]
    private var animationTask: Task<Void, Never>?
    private var loadTask: Task<Void, Never>?
    private var quotaTask: Task<Void, Never>?
    /// RunCat load signal in [0, 100]: tokens/min ÷ 10K, so 1M tok/min = 100.
    private var load: Double = 0
    /// Latest OAuth quota snapshot — feeds the gauge icon styles and the
    /// quota title mode (AppDelegate reads it through `quotaRemaining`).
    private(set) var quota: AgentUsagePayload?
    /// Fired after every successful quota fetch (title refresh hook).
    var onQuotaUpdated: (() -> Void)?

    init(controller: StatusItemController) {
        self.controller = controller
        var sets: [String: [NSImage]] = [:]
        for (style, dir) in [("cat", "anim-cat2"), ("parrot", "anim-parrot")] {
            sets["\(style)|dark"] = Self.loadFrames(directory: dir)
            sets["\(style)|light"] = Self.loadFrames(directory: "\(dir)-light")
        }
        frames = sets
    }

    /// PNG frames sorted by name (frame-00 … frame-NN), sized for the bar.
    /// Internal so the settings window's menu-bar mock can render the same
    /// frame sets.
    static func loadFrames(directory: String) -> [NSImage] {
        let urls = Bundle.tokenBarResources.urls(
            forResourcesWithExtension: "png", subdirectory: directory) ?? []
        return urls
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
            .compactMap { url in
                guard let image = NSImage(contentsOf: url) else { return nil }
                image.size = NSSize(width: 18, height: 18)
                return image
            }
    }

    private var defaultsObserver: NSObjectProtocol?
    private var appearanceObserver: NSKeyValueObservation?

    func start() {
        startAnimationLoop()
        startLoadPolling()
        startQuotaPolling()
        // Re-render the gauge the moment a setting changes (style, coloring,
        // quota source) — the 30s gauge loop alone is too slow.
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.renderGaugeIcon() }
        }
        // Re-render on dark/light mode flip so gauge icons don't show the
        // wrong color scheme for up to 30s (the gauge loop's sleep interval).
        appearanceObserver = NSApp.observe(
            \.effectiveAppearance, options: [.new]
        ) { [weak self] _, _ in
            MainActor.assumeIsolated { self?.renderGaugeIcon() }
        }
    }

    func stop() {
        animationTask?.cancel()
        loadTask?.cancel()
        quotaTask?.cancel()
        if let defaultsObserver { NotificationCenter.default.removeObserver(defaultsObserver) }
        appearanceObserver?.invalidate()
    }

    /// Draws the current gauge style immediately (no-op for cat/parrot,
    /// whose frames the animation loop owns).
    private func renderGaugeIcon() {
        let style = UserDefaults.standard.string(forKey: Self.styleKey) ?? "cat"
        guard let gaugeStyle = QuotaIconStyle(rawValue: style) else { return }
        let coloring = IconColoring(
            rawValue: UserDefaults.standard.string(forKey: IconColoring.storageKey) ?? ""
        ) ?? .warningOnly
        controller?.setFrame(
            TrayIcons.image(
                style: gaugeStyle, remaining: quotaRemaining,
                dark: controller?.isDarkAppearance ?? true,
                coloring: coloring))
    }

    /// Internal so the settings window's preview can fall back to the same
    /// last-good reading before its own quota fetch lands.
    static let lastRemainingKey = "tokenbar.quota.lastRemaining"

    /// Last successfully resolved remaining percent — a transient fetch
    /// failure (or a provider erroring) must never zero/blank the display.
    /// Persisted so a relaunch shows the last reading immediately instead of
    /// a blank gauge while the first (network) fetch runs.
    private var cachedQuotaRemaining: Double? =
        UserDefaults.standard.object(forKey: lastRemainingKey) as? Double

    /// The selected quota window's remaining percent, holding the last good
    /// value across failed refreshes (nil only before any data ever arrived).
    var quotaRemaining: Double? {
        let selection = UserDefaults.standard.string(forKey: Self.quotaSourceKey)
            ?? QuotaResolver.auto
        if let value = QuotaResolver.resolve(payload: quota, selection: selection)?
            .window.remainingPercent
        {
            if value != cachedQuotaRemaining {
                cachedQuotaRemaining = value
                UserDefaults.standard.set(value, forKey: Self.lastRemainingKey)
            }
            return value
        }
        return cachedQuotaRemaining
    }

    private func currentFrames() -> [NSImage] {
        let style = UserDefaults.standard.string(forKey: Self.styleKey) ?? "cat"
        let dark = controller?.isDarkAppearance ?? true
        return frames["\(style)|\(dark ? "dark" : "light")"]
            ?? frames["cat|dark"] ?? []
    }

    private var animateEnabled: Bool {
        UserDefaults.standard.object(forKey: Self.animateKey) == nil
            || UserDefaults.standard.bool(forKey: Self.animateKey)
    }

    /// animation.rs: `speed = max(1, load/5)`, `interval = 500ms / speed` —
    /// idle 2 fps, full load 40 fps.
    private var frameInterval: Duration {
        .milliseconds(Int(500.0 / max(1.0, load / 5.0)))
    }

    private func startAnimationLoop() {
        animationTask = Task { [weak self] in
            var index = 0
            var lastKey = ""
            while !Task.isCancelled {
                guard let self else { break }
                let style = UserDefaults.standard.string(forKey: Self.styleKey) ?? "cat"
                // Gauge styles: event-driven renders happen on quota
                // changes (onQuotaUpdated) and settings changes (defaults
                // observer). This loop only catches appearance flips (light/
                // dark mode), so a long sleep is fine.
                if QuotaIconStyle(rawValue: style) != nil {
                    self.renderGaugeIcon()
                    try? await Task.sleep(for: .seconds(30))
                    continue
                }
                let set = self.currentFrames()
                if style != lastKey {
                    index = 0
                    lastKey = style
                }
                guard !set.isEmpty else {
                    try? await Task.sleep(for: .seconds(2))
                    continue
                }
                if !self.animateEnabled {
                    index = 0
                    self.controller?.setFrame(set[0])
                    try? await Task.sleep(for: .seconds(2))
                    continue
                }
                self.controller?.setFrame(set[index % set.count])
                index = (index + 1) % set.count
                try? await Task.sleep(for: self.frameInterval)
            }
        }
    }

    /// OAuth quota fetch is network-bound (~30s worst case across four
    /// providers), so refresh on a 5-minute cadence — quota windows move
    /// slowly and the popover has its own faster loop while open.
    private func startQuotaPolling() {
        quotaTask = Task { [weak self] in
            while !Task.isCancelled {
                let payload = try? await Task.detached(priority: .utility) {
                    try TBCore.agentUsage()
                }.value
                guard let self, !Task.isCancelled else { break }
                if let payload {
                    self.quota = payload
                    self.renderGaugeIcon()
                    self.onQuotaUpdated?()
                }
                try? await Task.sleep(for: .seconds(300))
            }
        }
    }

    /// The raw tokens/min value from the last load poll — exposed so the
    /// tray title can display it without its own FFI call.
    private(set) var tokensPerMinRate: Double?

    /// Poll the live rate to feed the spin speed. 30s cadence balances
    /// animation responsiveness against the rayon wakeup cost of each FFI
    /// call (the staticlib's mtime check wakes the entire rayon pool).
    private func startLoadPolling() {
        loadTask = Task { [weak self] in
            while !Task.isCancelled {
                let rate = try? await Task.detached(priority: .utility) {
                    try TBCore.tokensPerMin()
                }.value
                guard let self, !Task.isCancelled else { break }
                if let rate {
                    self.load = min(rate / 10_000.0, 100.0)
                    self.tokensPerMinRate = rate
                    self.onQuotaUpdated?()
                }
                try? await Task.sleep(for: .seconds(30))
            }
        }
    }
}
