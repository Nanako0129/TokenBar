import Foundation
import TokenBarCore

enum WarpSourcePreference {
    static let externalPathKey = "warpExternalUsagePath"
}

private actor WarpSourceBootstrap {
    static let shared = WarpSourceBootstrap()
    private var restored = false

    func restoreExternalIfNeeded() {
        guard !restored else { return }
        restored = true
        guard let path = UserDefaults.standard.string(forKey: WarpSourcePreference.externalPathKey),
              !path.isEmpty
        else { return }
        _ = try? TBCore.warpRestoreExternalUsage(path: path)
    }
}

/// App-level source of usage data. Every usage consumer depends on this
/// contract so `--demo` can replace the complete usage surface without
/// allowing live FFI calls to leak into the demo path.
protocol UsageDataSource: Sendable {
    /// Whether this source may read/write the persistent last-good quota cache.
    var allowsQuotaCachePersistence: Bool { get }

    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload
    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload
    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport
    func hourlyReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> HourlyReport
    func agentsReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> AgentsReport
    func agentUsage() async throws -> AgentUsagePayload
    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket]
    func tokensPerMin() async throws -> Double
}

/// The only normal-runtime owner of usage calls into `TBCore`.
struct LiveUsageDataSource: UsageDataSource {
    let allowsQuotaCachePersistence = true

    private func restoreWarpSource() async {
        await WarpSourceBootstrap.shared.restoreExternalIfNeeded()
    }

    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        await restoreWarpSource()
        return try await Task.detached(priority: priority) {
            try TBCore.graph(year: year)
        }.value
    }

    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        await restoreWarpSource()
        return try await Task.detached(priority: priority) {
            if let status = try? TBCore.warpStatus(), status.supported,
               status.mode == "app"
            {
                _ = try? TBCore.warpRefresh()
            }
            return try TBCore.refreshGraph(year: year)
        }.value
    }

    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport {
        await restoreWarpSource()
        return try await Task.detached(priority: priority) {
            try TBCore.modelReport(year: year)
        }.value
    }

    func hourlyReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> HourlyReport {
        await restoreWarpSource()
        return try await Task.detached(priority: priority) {
            try TBCore.hourlyReport(year: year, clients: clients)
        }.value
    }

    func agentsReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> AgentsReport {
        await restoreWarpSource()
        return try await Task.detached(priority: priority) {
            try TBCore.agentsReport(year: year, clients: clients)
        }.value
    }

    func agentUsage() async throws -> AgentUsagePayload {
        try await Task.detached(priority: .utility) {
            try TBCore.agentUsage()
        }.value
    }

    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] {
        await restoreWarpSource()
        return try await Task.detached(priority: .utility) {
            try TBCore.usageTrace(windowSecs: windowSecs)
        }.value
    }

    func tokensPerMin() async throws -> Double {
        await restoreWarpSource()
        return try await Task.detached(priority: .utility) {
            try TBCore.tokensPerMin()
        }.value
    }
}

/// Synthetic source used by the hidden `--demo` mode. It only exposes the
/// deterministic fixtures in `DemoData`; it has no dependency on `TBCore`.
struct DemoUsageDataSource: UsageDataSource {
    let allowsQuotaCachePersistence = false

    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        _ = priority
        return DemoData.payload(for: year)
    }

    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        _ = priority
        // Rebuild rather than cache the value so manual refresh follows the same
        // source boundary as live refresh and keeps the rolling date current.
        return DemoData.payload(for: year)
    }

    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport {
        _ = priority
        return DemoData.modelReport(for: year)
    }

    func hourlyReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> HourlyReport {
        _ = priority
        return DemoData.hourlyReport(for: year, clients: clients)
    }

    func agentsReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> AgentsReport {
        _ = priority
        return DemoData.agentsReport(for: year, clients: clients)
    }

    func agentUsage() async throws -> AgentUsagePayload {
        DemoData.agentUsage
    }

    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] {
        DemoData.trace(windowSecs: windowSecs)
    }

    func tokensPerMin() async throws -> Double {
        DemoData.tokensPerMin
    }
}

/// Selects one source for the process lifetime. The mode is intentionally
/// launch-time only; changing the flag requires relaunching the app.
enum UsageDataSources {
    static let current: any UsageDataSource = make(arguments: CommandLine.arguments)

    static func make(arguments: [String]) -> any UsageDataSource {
        arguments.contains("--demo") ? DemoUsageDataSource() : LiveUsageDataSource()
    }
}
