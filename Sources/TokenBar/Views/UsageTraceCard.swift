import SwiftUI
import TokenBarCore

/// Live-session card: tokens/min per (client, agent, model) over the trailing
/// window, or collapsed to one row per client. Port of UsageTraceCard.tsx.
struct UsageTraceCard: View {
    let buckets: [TraceBucket]
    let windowSecs: Int
    var title = "Live session"
    /// Client ids to exclude (the user's hidden set). A deny-list, matching the
    /// menu-bar rate (LiveRate) — an allow-list blanked the card whenever the
    /// caller's client list had not loaded yet, and the two bases could
    /// transiently disagree on a client live in the tail but not yet in the
    /// graph's presentClients. Empty set = show every client (pre-#35 behavior).
    var hidden: Set<String> = []

    /// When true, rows split by (client, agent, model); off collapses per
    /// client. The settings panel edits the same key.
    @AppStorage("tokenbar.trace.detailed") private var detailed = false

    private static let maxRows = 5

    var body: some View {
        let visible = buckets.filter { !hidden.contains($0.client) }
        let rows = detailed ? visible : TraceBucket.collapseByClient(visible)
        let top = Array(rows.prefix(Self.maxRows))
        let maxRate = top.map(\.tokensPerMin).max() ?? 0
        // Single source of truth for the sum, shared with LiveRate.
        let totalRate = TraceBucket.totalRate(buckets, hidden: hidden)
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
