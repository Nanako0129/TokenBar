import SwiftUI
import TokenBarCore

/// Live-session card: tokens/min per (client, agent, model) over the trailing
/// window, or collapsed to one row per client. Port of UsageTraceCard.tsx.
struct UsageTraceCard: View {
    let buckets: [TraceBucket]
    let windowSecs: Int
    var title = "Live session"
    /// Restrict rows to these client ids (nil = every client). Callers pass
    /// the displayed client set so hidden clients drop out of the total rate.
    var clientIds: [String]?

    /// When true, rows split by (client, agent, model); off collapses per
    /// client. The settings panel edits the same key.
    @AppStorage("tokenbar.trace.detailed") private var detailed = false

    private static let maxRows = 5

    var body: some View {
        let visible = clientIds.map { ids in
            let allowed = Set(ids)
            return buckets.filter { allowed.contains($0.client) }
        } ?? buckets
        let rows = detailed ? visible : TraceBucket.collapseByClient(visible)
        let top = Array(rows.prefix(Self.maxRows))
        let maxRate = top.map(\.tokensPerMin).max() ?? 0
        let totalRate = rows.reduce(0) { $0 + $1.tokensPerMin }
        let windowMin = max(1, Int((Double(windowSecs) / 60).rounded()))

        DashCard(title, trailing: {
            Text("last \(windowMin)m · \(Format.compactTokens(Int64(totalRate.rounded())))/m total")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }) {
            if top.isEmpty {
                Text("No activity in this window")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
                    .frame(maxWidth: .infinity, alignment: .center)
                    .padding(.vertical, 8)
            } else {
                VStack(spacing: 6) {
                    ForEach(top, id: \.self.key) { bucket in
                        row(bucket, maxRate: maxRate)
                    }
                }
            }
        }
    }

    private func row(_ bucket: TraceBucket, maxRate: Double) -> some View {
        let pct = maxRate > 0 ? max(4, bucket.tokensPerMin / maxRate * 100) : 0
        return VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(Self.clientLabel(bucket.client))
                    .font(.caption2.weight(.semibold))
                Text(bucket.agent)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                Text(bucket.model)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                Spacer()
                Text("\(Format.compactTokens(Int64(bucket.tokensPerMin.rounded())))/m")
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    Capsule().fill(.quaternary.opacity(0.6))
                    Capsule()
                        .fill(Color.accentColor.opacity(0.8))
                        .frame(width: geo.size.width * pct / 100)
                }
            }
            .frame(height: 4)
        }
    }

    private static func clientLabel(_ id: String) -> String {
        id == "claude-code" ? "Claude Code" : id
    }
}

extension TraceBucket {
    /// Stable row identity across refreshes.
    fileprivate var key: String { "\(client)|\(agent)|\(model)" }
}
