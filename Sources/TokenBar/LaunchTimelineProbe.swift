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

        let source = LiveUsageDataSource()
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
        if wants("window") {
            tasks.append(Task {
                model.refreshWindowQuotaHalves()
                await model.refreshWindowUsage()
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
        var milestones: [String] = []
        if wants("graph") || wants("pollgraph") { milestones.append("graph") }
        if wants("quota") {
            milestones += ["quota payload", "\(client) window card: quota half",
                           "\(client) window card: ready"]
        }
        if wants("series") { milestones.append("attributed series") }
        let deadline = 90_000.0
        while seen.count < milestones.count, ms() < deadline {
            mark("graph", model.stats != nil)
            mark("quota payload", model.agentUsage != nil)
            switch model.windowCards[client] {
            case .quotaOnly?, .ready?, .noQuotaHistory?, .blocked?:
                mark("\(client) window card: quota half", true)
            default: break
            }
            if case .ready? = model.windowCards[client] {
                mark("\(client) window card: ready", true)
            }
            mark("attributed series", series.points != nil)
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        for name in milestones where seen[name] == nil {
            print("   never  \(name) (within \(Int(deadline / 1000)) s)")
        }
        tasks.forEach { $0.cancel() }
    }
}
