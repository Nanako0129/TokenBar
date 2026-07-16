import Foundation

// OAuth quota cards (`AgentUsagePayload` in the Tauri frontend's
// src/lib/agentUsage.ts).

public struct AgentIdentity: Decodable, Sendable {
    public let email: String?
    public let plan: String?
}

/// A backend-owned historical projection for one quota window.
///
/// The values are produced together by the Rust evaluator. Swift may use the
/// expected usage to classify the current pace, but must preserve the backend's
/// projection (ETA, lasts-to-reset decision, and optional risk) as one result.
public struct HistoricalPace: Decodable, Sendable {
    public let expectedUsedPercent: Double
    public let etaSeconds: Double?
    public let willLastToReset: Bool
    public let runOutProbability: Double?

    public init(
        expectedUsedPercent: Double,
        etaSeconds: Double? = nil,
        willLastToReset: Bool,
        runOutProbability: Double? = nil
    ) {
        self.expectedUsedPercent = expectedUsedPercent
        self.etaSeconds = etaSeconds
        self.willLastToReset = willLastToReset
        self.runOutProbability = runOutProbability
    }
}

public struct UsageWindow: Decodable, Sendable {
    public let label: String
    public let usedPercent: Double
    public let remainingPercent: Double
    public let resetsAt: String?
    public let resetText: String?
    /// Total window length in minutes; enables pace (expected vs actual).
    public let windowMinutes: Int64?
    /// Backend-owned historical projection, present only when enough complete
    /// weeks exist. Missing or null means Swift uses its linear calculation.
    public let historicalPace: HistoricalPace?

    // Memberwise init so --selftest can build fixture windows.
    public init(
        label: String, usedPercent: Double, remainingPercent: Double,
        resetsAt: String? = nil, resetText: String? = nil,
        windowMinutes: Int64? = nil, historicalPace: HistoricalPace? = nil
    ) {
        self.label = label
        self.usedPercent = usedPercent
        self.remainingPercent = remainingPercent
        self.resetsAt = resetsAt
        self.resetText = resetText
        self.windowMinutes = windowMinutes
        self.historicalPace = historicalPace
    }
}

public struct CreditsSnapshot: Decodable, Sendable {
    public let remaining: Double?
    public let unlimited: Bool
}

public struct AgentUsageSnapshot: Decodable, Sendable {
    public let clientId: String
    public let source: String
    public let updatedAt: String
    public let identity: AgentIdentity?
    public let windows: [UsageWindow]
    public let credits: CreditsSnapshot?
    public let error: String?
}

public struct AgentUsagePayload: Decodable, Sendable {
    public let generatedAt: String
    public let agents: [AgentUsageSnapshot]
    /// Subscription-type providers opencode is authed against (e.g. ["Codex"]).
    /// Omitted from the JSON entirely when empty.
    public let opencodeSubscriptions: [String]?
}
