import Foundation

/// Rust-owned classification for one source-generation-aware report pair.
public enum FilterParityStatus: String, Decodable, Sendable {
    case match
    case mismatch
    case sourceChanged
    case tokenUnavailable

    /// Bounded labels used by the CLI smoke output. The wire status remains
    /// lower camel case for the Rust/Swift contract.
    var smokeLabel: String {
        switch self {
        case .match:
            "MATCH"
        case .mismatch:
            "MISMATCH"
        case .sourceChanged:
            "SOURCE_CHANGED / SKIP"
        case .tokenUnavailable:
            "TOKEN_UNAVAILABLE / SKIP"
        }
    }
}

/// Aggregate fields intentionally contain no client, model, agent, path, or
/// message identity. Rust computes them before this FFI payload is serialized.
public struct FilterParityAggregate: Decodable, Sendable {
    public let entryCount: Int64
    public let input: Int64
    public let output: Int64
    public let cacheRead: Int64
    public let cacheWrite: Int64
    public let reasoning: Int64
    public let totalTokens: Int64
    public let messageCount: Int64
    public let totalCost: Double
}

public struct FilterParityReport: Decodable, Sendable {
    public let status: FilterParityStatus
    public let unfiltered: FilterParityAggregate?
    public let full: FilterParityAggregate?
    public let delta: FilterParityAggregate?

    var smokeSummary: String {
        switch status {
        case .match, .mismatch:
            guard let delta else { return status.smokeLabel }
            return "\(status.smokeLabel) entriesΔ=\(delta.entryCount) "
                + "tokensΔ=\(delta.totalTokens) messagesΔ=\(delta.messageCount) "
                + "costΔ=\(Self.formatCost(delta.totalCost))"
        case .sourceChanged, .tokenUnavailable:
            return status.smokeLabel
        }
    }

    private static func formatCost(_ value: Double) -> String {
        String(format: "%.2f", locale: Locale(identifier: "en_US_POSIX"), value)
    }
}

public struct FilterParityProbe: Decodable, Sendable {
    public let hourly: FilterParityReport
    public let agents: FilterParityReport
    public let presentClientCount: Int64

    /// One bounded line with no local source text. Aggregate deltas remain in
    /// the decoded DTO for callers that need the diagnostic detail.
    public var smokeSummary: String {
        "hourly=\(hourly.smokeSummary); agents=\(agents.smokeSummary)"
    }
}
