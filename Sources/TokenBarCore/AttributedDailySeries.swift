import Foundation

/// Daily token and cost points grouped by attribution bucket and model.
public enum AttributedDailySeries {
    public struct Point: Equatable, Sendable {
        public let date: String
        public let state: UsageAttribution.State
        public let model: String
        public let tokens: Int64
        public let cost: Double

        public init(
            date: String,
            state: UsageAttribution.State,
            model: String,
            tokens: Int64,
            cost: Double
        ) {
            self.date = date
            self.state = state
            self.model = model
            self.tokens = tokens
            self.cost = cost
        }
    }

    private struct Key: Hashable {
        let date: String
        let state: UsageAttribution.State
        let model: String

        var stateID: String { AttributedDailySeries.stateID(for: state) }
    }

    private struct Value {
        var tokens: Int64
        var cost: Double
    }

    private struct Row {
        let key: Key
        let tokens: Int64
        let cost: Double
    }

    /// Fold each graph client row into the current date, attribution state, and
    /// canonical model bucket. Confirmed records are the only declarations read.
    public static func points(
        contributions: [Contribution],
        confirmed: [UsageAttribution.Record]
    ) -> [Point] {
        var rows: [Row] = []

        for contribution in contributions {
            for client in contribution.clients {
                let tokens = client.tokens.total
                guard tokens != 0 || client.cost != 0 else { continue }

                let state: UsageAttribution.State
                if client.providerId.contains(",") {
                    state = .unassigned
                } else {
                    state = UsageAttribution.resolve(
                        client: client.client,
                        provider: client.providerId,
                        model: client.modelId,
                        records: confirmed)
                }
                let key = Key(
                    date: contribution.date,
                    state: state,
                    model: client.modelId)
                rows.append(Row(key: key, tokens: tokens, cost: client.cost))
            }
        }

        // Canonicalize the fold order so permuting graph rows cannot change
        // floating-point or saturating-integer accumulation.
        rows.sort {
            if $0.key.date != $1.key.date { return $0.key.date < $1.key.date }
            if $0.key.stateID != $1.key.stateID { return $0.key.stateID < $1.key.stateID }
            if $0.key.model != $1.key.model { return $0.key.model < $1.key.model }
            if $0.tokens != $1.tokens { return $0.tokens < $1.tokens }
            return $0.cost < $1.cost
        }

        var buckets: [Key: Value] = [:]
        for row in rows {
            var value = buckets[row.key] ?? Value(tokens: 0, cost: 0)
            value.tokens = value.tokens.saturatingAdding(row.tokens)
            value.cost += row.cost
            buckets[row.key] = value
        }

        return buckets.keys.sorted {
            if $0.date != $1.date { return $0.date < $1.date }
            if $0.stateID != $1.stateID { return $0.stateID < $1.stateID }
            return $0.model < $1.model
        }.compactMap { key in
            guard let value = buckets[key], value.tokens != 0 || value.cost != 0 else {
                return nil
            }
            return Point(
                date: key.date,
                state: key.state,
                model: key.model,
                tokens: value.tokens,
                cost: value.cost)
        }
    }

    private static func stateID(for state: UsageAttribution.State) -> String {
        switch state {
        case let .assigned(target): return "assigned:\(target)"
        case .excluded: return "excluded"
        case .unassigned: return "unassigned"
        }
    }
}
