import Foundation

/// Pure aggregation for the Stats usage-attribution card.
public enum UsageAttributionBreakdown {
    public enum Copy {
        public static let title = "API-list-price equivalent"
        public static let hint =
            "Nothing is classified yet. Classify sources in Settings → Usage attribution."
        public static let noUsage = "No attributed usage in this range."

        public static var all: [String] { [title, hint, noUsage] }
    }

    public struct Row: Identifiable, Equatable, Hashable, Sendable {
        public let state: UsageAttribution.State
        public let tokens: Int64
        public let cost: Double

        public var id: String {
            switch state {
            case let .assigned(target): return "assigned:\(target)"
            case .excluded: return "excluded"
            case .unassigned: return "unassigned"
            }
        }
    }

    /// Classify and merge raw provider rows for the selected clients.
    public static func rows(
        entries: [ModelReportEntry],
        clientIds: [String],
        confirmed: [UsageAttribution.Record]
    ) -> [Row] {
        let allowed = Set(clientIds)
        var assigned: [String: (tokens: Int64, cost: Double)] = [:]
        var excluded: (tokens: Int64, cost: Double) = (0, 0)
        var unassigned: (tokens: Int64, cost: Double) = (0, 0)

        for entry in entries where allowed.contains(entry.client) {
            let state = UsageAttribution.resolve(entry, records: confirmed)
            guard entry.total != 0 || entry.cost != 0 else { continue }
            switch state {
            case let .assigned(target):
                let current = assigned[target] ?? (0, 0)
                assigned[target] = (
                    current.tokens.saturatingAdding(entry.total), current.cost + entry.cost)
            case .excluded:
                excluded.tokens = excluded.tokens.saturatingAdding(entry.total)
                excluded.cost += entry.cost
            case .unassigned:
                unassigned.tokens = unassigned.tokens.saturatingAdding(entry.total)
                unassigned.cost += entry.cost
            }
        }

        var result = assigned.keys.sorted().compactMap { target -> Row? in
            guard let value = assigned[target], value.tokens != 0 || value.cost != 0 else {
                return nil
            }
            return Row(state: .assigned(target), tokens: value.tokens, cost: value.cost)
        }
        if excluded.tokens != 0 || excluded.cost != 0 {
            result.append(Row(state: .excluded, tokens: excluded.tokens, cost: excluded.cost))
        }
        if unassigned.tokens != 0 || unassigned.cost != 0 {
            result.append(Row(state: .unassigned, tokens: unassigned.tokens, cost: unassigned.cost))
        }
        return result
    }
}
