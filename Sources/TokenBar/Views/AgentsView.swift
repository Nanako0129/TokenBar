import SwiftUI
import TokenBarCore

/// "Agents by cost" lens, port of AgentsView.tsx: named sub-agents (plus a
/// "Main" bucket for unattributed messages) ranked by cost, showing the
/// source clients they ran under, message count, total tokens, and cost.
struct AgentsView: View {
    let report: AgentsReport?
    /// Restrict to agents that ran under any of these clients; empty = all.
    var clientIds: [String] = []

    var body: some View {
        let allow = Set(clientIds)
        let rows = (report?.entries ?? [])
            .filter { $0.clients.contains { allow.contains($0) } }
        let totalCost = rows.reduce(0) { $0 + $1.cost }
        let maxCost = max(rows.map(\.cost).max() ?? 1, 0.000001)

        DashCard(
            "Agents by cost",
            trailing: {
                Text("\(rows.count) agent\(rows.count == 1 ? "" : "s") · \(Format.usd(totalCost))")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        ) {
            if report == nil {
                Text("Loading…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else if rows.isEmpty {
                Text("No agent activity in this range")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(spacing: 10) {
                    ForEach(rows, id: \.agent) { entry in
                        row(entry, totalCost: totalCost, maxCost: maxCost)
                    }
                }
            }
        }
    }

    private func row(_ entry: AgentReportEntry, totalCost: Double, maxCost: Double) -> some View {
        let share = totalCost > 0 ? entry.cost / totalCost * 100 : 0
        let sources = entry.clients.map { ClientRegistry.style($0).displayName }
            .joined(separator: " · ")
        return VStack(alignment: .leading, spacing: 3) {
            HStack {
                Text(entry.agent)
                    .font(.caption)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(entry.agent)
                Spacer()
                Text(String(format: "%.1f%%", share))
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.tertiary)
            }
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    RoundedRectangle(cornerRadius: 2)
                        .fill(.quaternary.opacity(0.5))
                    RoundedRectangle(cornerRadius: 2)
                        .fill(Color.accentColor.opacity(0.7))
                        .frame(width: geo.size.width * CGFloat(entry.cost / maxCost))
                }
            }
            .frame(height: 6)
            HStack {
                Text(sources)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                    .help(sources)
                Spacer()
                Text("\(entry.messages.formatted()) msgs · \(Format.compactTokens(entry.total)) · \(Format.usd(entry.cost))")
                    .foregroundStyle(.secondary)
                    .layoutPriority(1)
            }
            .font(.caption2)
        }
    }
}
