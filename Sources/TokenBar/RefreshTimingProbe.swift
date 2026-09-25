import Foundation
import TokenBarCore

/// The measurement lane behind `--refresh-timing`: how long the synchronous
/// quota refresh holds the main actor on real local data.
///
/// `refreshWindowQuotaHalves()` runs on the main actor every time a quota
/// window is switched, every poll and every reopen. This drives the production
/// `DashboardModel` through the same sequence the popover does (graph, quota
/// payload, the open tab's usage scan) and then times the call itself, so the
/// number it prints is the stall a window switch costs, not a model of it.
///
/// Not reachable from the shipping UI: `main.swift` runs it only for the flag
/// and exits. It reads real local data and fetches the real quota payload,
/// like `--window-probe`. The window-switch pass writes the window selection
/// (`WindowCardLoader.selectionKey`) of whichever defaults domain it runs in
/// and restores the previous value before exiting.
///
/// `--client <id>` picks the tab; the default is `claude`.
@MainActor
enum RefreshTimingProbe {
    static func start() -> Never {
        Task { @MainActor in
            await run()
            exit(0)
        }
        dispatchMain()
    }

    private static func ms(_ start: DispatchTime) -> Double {
        Double(DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1_000_000
    }

    private static func run() async {
        setvbuf(stdout, nil, _IONBF, 0)  // interleave with stderr in order
        let counting = CountingSource()
        let model = DashboardModel(source: counting)
        let d = UserDefaults.standard
        model.configureQuotaVisibility(
            tabHidden: ClientRegistry.parseIdSet(d.string(forKey: ClientRegistry.tabHiddenKey) ?? ""),
            limitsHidden: ClientRegistry.parseIdSet(d.string(forKey: ClientRegistry.limitsHiddenKey) ?? ""),
            orderRaw: d.string(forKey: ClientRegistry.tabOrderKey) ?? "")

        var t = DispatchTime.now()
        await model.load()
        print(String(format: "load (graph)                 %8.1f ms", ms(t)))

        t = DispatchTime.now()
        let poll = Task { await model.pollAgentUsage() }
        while model.agentUsage == nil { try? await Task.sleep(nanoseconds: 50_000_000) }
        poll.cancel()
        print(String(format: "first quota payload          %8.1f ms", ms(t)))

        let args = CommandLine.arguments
        let client = args.firstIndex(of: "--client").map { args[$0 + 1] } ?? "claude"
        model.windowUsageClient = client
        print("client                       \(client)")

        // Cold: no usage scan held yet, which is what a first open pays.
        t = DispatchTime.now()
        model.refreshWindowQuotaHalves()
        print(String(format: "refresh, no scan held        %8.1f ms", ms(t)))

        t = DispatchTime.now()
        await model.refreshWindowUsage()
        print(String(format: "usage scan (async)           %8.1f ms", ms(t)))

        // Warm: the scan is held, which is the state every window switch after
        // the first one runs in.
        var samples: [Double] = []
        for _ in 0..<5 {
            t = DispatchTime.now()
            model.refreshWindowQuotaHalves()
            samples.append(ms(t))
        }
        print("refresh, scan held (x5)      "
              + samples.map { String(format: "%.1f", $0) }.joined(separator: "  ") + " ms")

        // Window switches: each selects another of this client's windows, the
        // way the picker does, and runs the same two stages the popover runs.
        // Reports whether the switch paid for a new message scan.
        let windows = model.agentUsage?.agents.first { $0.clientId == client }?.uniqueCardWindows ?? []
        let saved = d.string(forKey: WindowCardLoader.selectionKey)
        defer { d.set(saved, forKey: WindowCardLoader.selectionKey) }
        for window in windows + windows {
            d.set("\(client)|\(window.cardId)", forKey: WindowCardLoader.selectionKey)
            let before = counting.scans
            t = DispatchTime.now()
            model.refreshWindowQuotaHalves()
            let sync = ms(t)
            t = DispatchTime.now()
            await model.refreshWindowUsage()
            print(String(format: "switch → %-24@ sync %6.1f ms  scan %@ %8.1f ms",
                         window.cardId, sync, counting.scans > before ? "yes" : "no ", ms(t)))
        }
    }
}

/// Forwards to the live source and counts message scans, so a window switch
/// can be seen paying for one or not.
private final class CountingSource: UsageDataSource, @unchecked Sendable {
    let live = LiveUsageDataSource()
    var scans = 0
    var allowsQuotaCachePersistence: Bool { live.allowsQuotaCachePersistence }
    func windowUsage(accountKey: String?, from: Int64, until: Int64) async throws -> WindowUsage {
        scans += 1
        return try await live.windowUsage(accountKey: accountKey, from: from, until: until)
    }
    func quotaCurveSync(clientId: String, accountKey: String?, windowKey: String, generation: UInt64) throws -> QuotaCurve? {
        try live.quotaCurveSync(clientId: clientId, accountKey: accountKey, windowKey: windowKey, generation: generation)
    }
    func quotaCurve(clientId: String, accountKey: String?, windowKey: String, generation: UInt64) async throws -> QuotaCurve? {
        try await live.quotaCurve(clientId: clientId, accountKey: accountKey, windowKey: windowKey, generation: generation)
    }
    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload { try await live.graph(year: year, priority: priority) }
    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload { try await live.refreshGraph(year: year, priority: priority) }
    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport { try await live.modelReport(year: year, priority: priority) }
    func hourlyReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> HourlyReport { try await live.hourlyReport(year: year, clients: clients, priority: priority) }
    func agentsReport(year: String?, clients: [String]?, priority: TaskPriority) async throws -> AgentsReport { try await live.agentsReport(year: year, clients: clients, priority: priority) }
    func agentUsage() async throws -> AgentUsagePayload { try await live.agentUsage() }
    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] { try await live.usageTrace(windowSecs: windowSecs) }
    func tokensPerMin() async throws -> Double { try await live.tokensPerMin() }
}
