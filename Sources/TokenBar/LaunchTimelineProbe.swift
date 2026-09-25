import Foundation
import TokenBarCore

/// The measurement lane behind `--launch-timeline`: when each part of the
/// dashboard becomes drawable after a cold process start.
///
/// Starts the same tasks the popover starts when it opens (`PopoverView`'s
/// `.task` modifiers: graph load and poll, quota poll, trace poll, the open
/// tab's window stages, the attributed series) at the same moment, so they
/// contend the way they do in the app, and records the first time each
/// milestone holds. `--refresh-timing` times the stages one after another,
/// which says what each costs alone and nothing about the order a user sees.
///
/// Not reachable from the shipping UI: `main.swift` runs it only for the flag
/// and exits. It reads real local data and fetches the real quota payload.
/// No snapshot is restored or written (`cachesSnapshot: false`), so the graph
/// milestone is the refresh a relaunch waits for behind the restored picture.
/// `--client <id>` picks the open tab; the default is `claude`.
@MainActor
enum LaunchTimelineProbe {
    static func start() -> Never {
        Task { @MainActor in
            await run()
            exit(0)
        }
        dispatchMain()
    }

    private static func run() async {
        setvbuf(stdout, nil, _IONBF, 0)
        let args = CommandLine.arguments
        let client = args.firstIndex(of: "--client")
            .flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil } ?? "claude"
        let t0 = DispatchTime.now()
        func ms() -> Double { Double(DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds) / 1e6 }

        let source = TimedSource()
        let model = DashboardModel(source: source)
        let series = AttributedSeriesModel()
        let d = UserDefaults.standard
        model.configureQuotaVisibility(
            tabHidden: ClientRegistry.parseIdSet(d.string(forKey: ClientRegistry.tabHiddenKey) ?? ""),
            limitsHidden: ClientRegistry.parseIdSet(d.string(forKey: ClientRegistry.limitsHiddenKey) ?? ""),
            orderRaw: d.string(forKey: ClientRegistry.tabOrderKey) ?? "")
        model.windowUsageClient = client

        // `--only graph,quota,...` starts a subset, to find which tasks slow
        // which. Names: graph (load), quota (poll, which also starts the
        // window scan), window (the open tab's stages), trace, pollgraph,
        // series, and tray (opt-in, below). Default: the popover's set.
        let only = args.firstIndex(of: "--only")
            .flatMap { $0 + 1 < args.count ? Set(args[$0 + 1].split(separator: ",").map(String.init)) : nil }
        func wants(_ name: String) -> Bool { only?.contains(name) ?? true }
        var tasks: [Task<Void, Never>] = []
        if wants("graph") { tasks.append(Task { await model.load() }) }
        if wants("quota") { tasks.append(Task { await model.pollAgentUsage() }) }
        var windowStagesDone = false
        if wants("window") {
            tasks.append(Task {
                model.refreshWindowQuotaHalves()
                await model.refreshWindowUsage()
                windowStagesDone = true
            })
        }
        if wants("trace") { tasks.append(Task { await model.pollTrace() }) }
        if wants("pollgraph") { tasks.append(Task { await model.pollGraph() }) }
        // Not a popover task: the tray's first title refresh at app launch,
        // which forces a full re-read (`AppDelegate.startTitleRefresh`,
        // `lastFullRefresh` starts at `.distantPast`). Opt-in only.
        if only?.contains("tray") == true {
            tasks.append(Task { _ = try? await source.refreshGraph(year: nil, priority: .utility) })
        }
        if wants("series") {
            tasks.append(Task {
                await series.load(
                    source: source, confirmed: UsageAttribution.confirmed().records)
            })
        }

        var seen: [String: Double] = [:]
        func mark(_ name: String, _ holds: Bool) {
            if holds, seen[name] == nil {
                seen[name] = ms()
                print(String(format: "%8.0f ms  %@", seen[name]!, name))
            }
        }
        // One milestone per selected task, so a subset never ends with nothing
        // to report. Trace and the tray refresh are timed at the source (their
        // results can legitimately be empty, so the model's state cannot say
        // they have answered); the window stages by the task finishing.
        let quotaHalf = "\(client) window card: quota half"
        let cardReady = "\(client) window card: ready"
        let cardNoQuota = "\(client) window card: settled without quota (blocked / no history)"
        var milestones: [String] = []
        if wants("graph") || wants("pollgraph") { milestones.append("graph") }
        if wants("quota") { milestones += ["quota payload", quotaHalf, cardReady] }
        if wants("window") { milestones.append("window stages finished") }
        if wants("trace") { milestones.append("trace (first answer)") }
        if only?.contains("tray") == true { milestones.append("tray forced refresh") }
        if wants("series") { milestones.append("attributed series") }
        // A card that settles without a quota half will never be ready either;
        // it satisfies both instead of holding the run until the deadline.
        func pending(_ name: String) -> Bool {
            guard seen[name] == nil else { return false }
            if name == quotaHalf || name == cardReady { return seen[cardNoQuota] == nil }
            return true
        }
        let deadline = 90_000.0
        while milestones.contains(where: pending), ms() < deadline {
            mark("graph", model.stats != nil)
            mark("quota payload", model.agentUsage != nil)
            switch model.windowCards[client] {
            case .quotaOnly?:
                mark(quotaHalf, true)
            case .ready?:
                mark(quotaHalf, true)
                mark(cardReady, true)
            case .blocked?, .noQuotaHistory?:
                mark(cardNoQuota, true)
            default: break
            }
            mark("window stages finished", windowStagesDone)
            mark("trace (first answer)", source.answered("trace"))
            mark("tray forced refresh", source.answered("refreshGraph"))
            mark("attributed series", series.points != nil)
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        for name in milestones where pending(name) {
            print("   never  \(name) (within \(Int(deadline / 1000)) s)")
        }
        tasks.forEach { $0.cancel() }
    }
}

/// Forwards to the live source and records which calls have returned at least
/// once, for milestones the model's own state cannot show.
private final class TimedSource: UsageDataSource, @unchecked Sendable {
    private let live = LiveUsageDataSource()
    private let lock = NSLock()
    private var returned: Set<String> = []

    func answered(_ call: String) -> Bool {
        lock.lock(); defer { lock.unlock() }
        return returned.contains(call)
    }

    private func note(_ call: String) {
        lock.lock(); returned.insert(call); lock.unlock()
    }

    var allowsQuotaCachePersistence: Bool { live.allowsQuotaCachePersistence }
    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        try await live.graph(year: year, priority: priority)
    }
    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        defer { note("refreshGraph") }
        return try await live.refreshGraph(year: year, priority: priority)
    }
    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport {
        try await live.modelReport(year: year, priority: priority)
    }
    func hourlyReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> HourlyReport {
        try await live.hourlyReport(year: year, clients: clients, priority: priority)
    }
    func agentsReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> AgentsReport {
        try await live.agentsReport(year: year, clients: clients, priority: priority)
    }
    func agentUsage() async throws -> AgentUsagePayload { try await live.agentUsage() }
    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] {
        defer { note("trace") }
        return try await live.usageTrace(windowSecs: windowSecs)
    }
    func tokensPerMin() async throws -> Double { try await live.tokensPerMin() }
    func windowUsage(accountKey: String?, from: Int64, until: Int64) async throws -> WindowUsage {
        try await live.windowUsage(accountKey: accountKey, from: from, until: until)
    }
    func quotaCurve(clientId: String, accountKey: String?, windowKey: String, generation: UInt64) async throws -> QuotaCurve? {
        try await live.quotaCurve(clientId: clientId, accountKey: accountKey, windowKey: windowKey, generation: generation)
    }
    func quotaCurveSync(clientId: String, accountKey: String?, windowKey: String, generation: UInt64) throws -> QuotaCurve? {
        try live.quotaCurveSync(clientId: clientId, accountKey: accountKey, windowKey: windowKey, generation: generation)
    }
}
