import Foundation
import Observation
import TokenBarCore

/// The set of analysis lenses, echoing tokscale's TUI tabs. The client tab
/// (Overview/Claude/Codex…, later phase) filters *which* data; this picks
/// *how* it is broken down. The two compose.
enum AppView: String, CaseIterable {
    case overview, models, daily, hourly, stats, agents

    var label: String { rawValue.prefix(1).uppercased() + rawValue.dropFirst() }
}

/// Shared dashboard data for every lens. Base data (graph + model report)
/// loads when the popover opens; the hourly/agents reports load lazily the
/// first time their lens becomes active, mirroring the Tauri app's
/// empty-year short-circuit hooks.
@MainActor @Observable final class DashboardModel {
    enum Phase {
        case loading
        case ready
        case failed(String)
    }

    private(set) var phase: Phase = .loading
    private static let yearKey = "tokenbar.dashboard.year"

    /// Year filter for every lens (HeaderBar's year select in the Tauri app);
    /// nil = all time. Persisted so the selection survives the popover's
    /// rootView teardown/rebuild cycle.
    /// `--year=<yyyy>` preselects a year (debug/screenshot aid).
    private(set) var year: String? =
        CommandLine.arguments
            .first(where: { $0.hasPrefix("--year=") })
            .map { String($0.dropFirst("--year=".count)) }
        ?? UserDefaults.standard.string(forKey: yearKey)
    /// Union of `payload.years` across loads — a year-filtered payload only
    /// reports the selected year, so remember the rest for the picker.
    private(set) var knownYears: [String] = []
    private(set) var payload: UsagePayload?
    private(set) var stats: UsageStats?
    private(set) var modelReport: ModelReport?
    private(set) var colors = ModelColorMap(report: nil)
    private(set) var hourly: HourlyReport?
    private(set) var agents: AgentsReport?
    private(set) var agentUsage: AgentUsagePayload?
    private(set) var trace: [TraceBucket] = []

    /// TBCore is blocking — every fetch hops off the main actor.
    func load() async {
        do {
            let year = self.year
            async let payloadTask = Task.detached(priority: .userInitiated) {
                try TBCore.graph(year: year)
            }.value
            async let reportTask = Task.detached(priority: .userInitiated) {
                try? TBCore.modelReport(year: year)
            }.value
            let payload = try await payloadTask
            let report = await reportTask
            apply(payload: payload, report: report)
        } catch {
            // Keep showing stale data over an error screen when a previous
            // load succeeded — a transient failure must not blank the UI.
            if payload == nil {
                phase = .failed("Failed to load usage: \(error)")
            }
        }
    }

    private(set) var refreshing = false

    /// Manual refresh: force a full log re-read (bypassing the staticlib's
    /// 30s cache) and drop the lazy per-lens reports so they re-fetch.
    func refresh() async {
        guard !refreshing else { return }
        refreshing = true
        defer { refreshing = false }
        await reload(force: true)
    }

    /// Switch the year filter and re-fetch every lens for the new slice.
    /// Served from the staticlib's per-year cache when fresh, so flipping
    /// back to a recent year is instant.
    func setYear(_ newYear: String?) async {
        guard newYear != year, !refreshing else { return }
        year = newYear
        UserDefaults.standard.set(newYear, forKey: Self.yearKey)
        refreshing = true
        defer { refreshing = false }
        await reload(force: false)
    }

    private func reload(force: Bool) async {
        let year = self.year
        async let payloadTask = Task.detached(priority: .userInitiated) {
            force ? try TBCore.refreshGraph(year: year) : try TBCore.graph(year: year)
        }.value
        async let reportTask = Task.detached(priority: .userInitiated) {
            try? TBCore.modelReport(year: year)
        }.value
        guard let payload = try? await payloadTask else { return }
        let report = await reportTask
        apply(payload: payload, report: report)
        // Re-fetch the lazy lenses that were already loaded.
        if hourly != nil {
            hourly = await Task.detached(priority: .userInitiated) {
                try? TBCore.hourlyReport(year: year)
            }.value
        }
        if agents != nil {
            agents = await Task.detached(priority: .userInitiated) {
                try? TBCore.agentsReport(year: year)
            }.value
        }
    }

    private func apply(payload: UsagePayload, report: ModelReport?) {
        // A year-filtered payload reports only the selected year (empty if that
        // year has no data). Validate the filter against THIS fresh payload —
        // not the knownYears union, which never drops a year once seen — so a
        // selected year whose logs were deleted/moved (even while the popover
        // stays open) clears instead of stranding the dashboard on an empty
        // slice. Re-fetch unfiltered so all data shows immediately.
        if let year, !payload.years.contains(where: { $0.year == year }) {
            self.year = nil
            UserDefaults.standard.removeObject(forKey: Self.yearKey)
            Task { [weak self] in await self?.reload(force: false) }
            return
        }
        self.payload = payload
        stats = UsageStats(payload: payload, selectedClients: Set(payload.summary.clients))
        modelReport = report
        colors = ModelColorMap(report: report)
        knownYears = Set(knownYears + payload.years.map(\.year)).sorted(by: >)
        phase = .ready
    }

    /// Periodically re-derive every loaded lens so the popover advances while
    /// it stays open. StatusItemController tears down and rebuilds PopoverView
    /// on each open/close cycle, so `.task { load() }` runs on every open and
    /// this loop is cancelled on close — but while open, without this loop the
    /// overview bars never pick up today's usage until a manual Refresh. Uses
    /// the non-forced graph() path: the staticlib's mtime-aware cache makes
    /// idle ticks cheap and only re-aggregates when logs actually change.
    /// Keeps stale data on error (only assigns on success).
    func pollGraph() async {
        while !Task.isCancelled {
            // Sleep first: load()'s initial fetch already covers t=0.
            try? await Task.sleep(for: .seconds(60))
            if Task.isCancelled { break }
            // Don't race an in-flight manual Refresh or year switch.
            guard !refreshing else { continue }
            let year = self.year
            async let payloadTask = Task.detached(priority: .utility) {
                try? TBCore.graph(year: year)
            }.value
            async let reportTask = Task.detached(priority: .utility) {
                try? TBCore.modelReport(year: year)
            }.value
            let fetched = await payloadTask
            let report = await reportTask
            if Task.isCancelled { break }
            // The year may have changed while we were off-actor; drop a stale
            // slice so the chart never flickers to the wrong year.
            guard self.year == year, let payload = fetched else { continue }
            apply(payload: payload, report: report)
            // Re-fetch the lazy lenses that were already loaded (mirrors reload).
            if hourly != nil {
                hourly = await Task.detached(priority: .utility) {
                    try? TBCore.hourlyReport(year: year)
                }.value
            }
            if agents != nil {
                agents = await Task.detached(priority: .utility) {
                    try? TBCore.agentsReport(year: year)
                }.value
            }
        }
    }

    /// Poll the OAuth quota snapshots while the popover is open. The fetch is
    /// network-bound (up to ~30s when a provider hangs), so failures keep the
    /// previous payload; per-provider errors live inside each snapshot.
    func pollAgentUsage() async {
        while !Task.isCancelled {
            let payload = try? await Task.detached(priority: .utility) {
                try TBCore.agentUsage()
            }.value
            if Task.isCancelled { break }
            if let payload { agentUsage = payload }
            try? await Task.sleep(for: .seconds(60))
        }
    }

    /// Poll the live tail (10-minute window) — drives the limits card's
    /// "Live" badge now and the trace card in a later phase. The staticlib
    /// re-parses at most every 10s, so this matches its cadence.
    func pollTrace() async {
        while !Task.isCancelled {
            let buckets = try? await Task.detached(priority: .utility) {
                try TBCore.usageTrace(windowSecs: 600)
            }.value
            if Task.isCancelled { break }
            if let buckets { trace = buckets }
            try? await Task.sleep(for: .seconds(10))
        }
    }

    /// Fetch the lazy per-lens reports on first activation.
    func ensureData(for view: AppView) async {
        let year = self.year
        switch view {
        case .hourly where hourly == nil:
            hourly = await Task.detached(priority: .userInitiated) {
                try? TBCore.hourlyReport(year: year)
            }.value
        case .agents where agents == nil:
            agents = await Task.detached(priority: .userInitiated) {
                try? TBCore.agentsReport(year: year)
            }.value
        default:
            break
        }
    }
}
