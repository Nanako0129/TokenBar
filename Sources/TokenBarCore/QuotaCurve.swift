import Foundation

// Read-only quota curve snapshot (`tb_quota_curve`). Keys match the Rust serde
// camelCase serialization exactly.

/// One admitted quota reading. `sampledAt` is the real observation time, never
/// a grid position: phase-bucket admission decides *whether* a reading is kept,
/// not when it was taken, so the points are unevenly spaced by construction.
public struct QuotaCurvePoint: Decodable, Sendable, Equatable {
    public let sampledAt: Int64
    public let usedPercent: Double
    public let resetAt: Int64
    /// Per point, not per cycle: one reset group can legitimately carry
    /// readings recorded under different window lengths and sources.
    public let durationSeconds: Int64
    public let durationSource: String
    public let origin: String
}

/// What the snapshot covers. Deliberately not a completeness claim — whether a
/// requested range is fully covered is decided when drawing, against the
/// series' actual window length and the samples actually held.
public struct QuotaCurveCoverage: Decodable, Sendable, Equatable {
    public let oldestSampledAt: Int64
    public let newestSampledAt: Int64
    public let sampleCount: Int
}

public struct QuotaCurve: Decodable, Sendable, Equatable {
    public let points: [QuotaCurvePoint]
    public let coverage: QuotaCurveCoverage
    public let activeResetAt: Int64?
    /// The publication generation whose binding resolved this series' identity.
    /// It is not a data cutoff: the samples are the history as of the read.
    public let generation: UInt64
}
