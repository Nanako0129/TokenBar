import Foundation

/// Pure derivation for the provider-level Usage Attribution settings page.
public enum UsageAttributionSettings {
    public enum Copy {
        public static let section = "Usage attribution"
        public static let classifyHint = "Classify each observed client/provider source against the subscription it should count toward. Nothing here is inferred as a billing event."
        public static let canonicalizationHint = "Provider IDs are canonicalized before they reach TokenBar: openai_codex becomes openai, and vertex / vertex_ai become anthropic. Routes that canonicalize to the same provider cannot be separated here."
        public static let declarationHint = "A declaration is your classification, not a billing fact."
        public static let noRows = "No provider-split usage in this range."
        public static let acceptSuggestions = "Accept all suggestions (%lld)"
        public static let suggestionsHint = "Suggestions are proposals; they do not change your classification until accepted."
        public static let source = "%@ · %@"
        public static let observed = "Observed %@ tokens · %@"
        public static let classification = "Classification"
        public static let unassigned = "Unassigned"
        public static let excluded = "Not a subscription"
        public static let assigned = "Counts toward %@"
        public static let suggested = "Suggested: counts toward %@"
        public static let unspecifiedProvider = "Unspecified provider"
        public static let classificationFor = "Classification for %@"

        public static var all: [String] {
            [
                section, classifyHint, canonicalizationHint, declarationHint, noRows,
                acceptSuggestions, suggestionsHint, source, observed, classification,
                unassigned, excluded, assigned, suggested, unspecifiedProvider,
                classificationFor,
                WriteFailure.invalidExistingValue.message,
                WriteFailure.entryLimit.message,
                WriteFailure.sizeOrInvalidRecord.message,
            ]
        }
    }

    public struct Row: Identifiable, Equatable, Sendable {
        public let client: String
        public let provider: String
        public let tokens: Int64
        public let cost: Double
        public let state: UsageAttribution.State
        public let suggestedTarget: String?

        public var id: String { "\(client)\u{0}\(provider)" }

        /// Empty provider is valid wire data. Keep it as its own source key and
        /// give it a readable label instead of hiding or merging the row.
        public var providerLabel: String {
            provider.isEmpty ? Copy.unspecifiedProvider : provider
        }
    }

    public enum WriteFailure: Equatable, Sendable {
        case invalidExistingValue
        case entryLimit
        case sizeOrInvalidRecord

        public var message: String {
            switch self {
            case .invalidExistingValue:
                return "Could not save this classification: existing attribution data is invalid or foreign."
            case .entryLimit:
                return "Could not save this classification: the attribution entry limit was reached."
            case .sizeOrInvalidRecord:
                return "Could not save this classification: the new value is too large or unsupported."
            }
        }
    }

    /// This is auditable product knowledge, not an inference from local usage
    /// or quota payloads. Suggestions are limited to a client's own
    /// subscription so cross-client routing cannot become a guessed billing
    /// declaration (for example, Claude traffic using an OpenAI model).
    public static let subscriptionProviderMap: [String: Set<String>] = [
        "claude": ["anthropic"],
        "codex": ["openai"],
        "copilot": ["openai", "anthropic"],
        "grok": ["xai"],
        "antigravity": ["google"],
    ]

    /// `agentUsage` is the capability source for assignment targets. Keep every
    /// registered snapshot, including an error-only snapshot, so a transient
    /// quota outage cannot remove an existing subscription from the picker.
    public static func subscriptionClients(from payload: AgentUsagePayload?) -> [String] {
        var seen = Set<String>()
        return (payload?.agents.map(\.clientId) ?? []).filter {
            ClientRegistry.allIds.contains($0) && seen.insert($0).inserted
        }
    }

    /// Consume raw provider-split rows. `modelLevelEntries` folds providers
    /// back together, which would erase the exact dimension this page classifies.
    public static func rows(
        entries: [ModelReportEntry],
        confirmed: [UsageAttribution.Record],
        suggestions: [UsageAttribution.Record]
    ) -> [Row] {
        var aggregate: [String: (client: String, provider: String, tokens: Int64, cost: Double)] = [:]
        var order: [String] = []
        for entry in entries {
            let key = sourceKey(client: entry.client, provider: entry.provider)
            if let current = aggregate[key] {
                aggregate[key] = (
                    current.client,
                    current.provider,
                    current.tokens.saturatingAdding(entry.total),
                    current.cost + entry.cost)
            } else {
                aggregate[key] = (entry.client, entry.provider, entry.total, entry.cost)
                order.append(key)
            }
        }

        return order.compactMap { key in
            guard let value = aggregate[key] else { return nil }
            let state = UsageAttribution.resolve(
                client: value.client, provider: value.provider, model: nil, records: confirmed)
            let suggestion: String?
            if case .unassigned = state {
                suggestion = suggestions.first {
                    $0.client == value.client && $0.provider == value.provider && $0.model == nil
                }.flatMap { record in
                    if case let .assigned(target) = record.state { return target }
                    return nil
                }
            } else {
                suggestion = nil
            }
            return Row(
                client: value.client,
                provider: value.provider,
                tokens: value.tokens,
                cost: value.cost,
                state: state,
                suggestedTarget: suggestion)
        }
    }

    public static func suggestionTarget(
        sourceClient: String, provider: String, subscriptionClients: [String]
    ) -> String? {
        guard subscriptionClients.contains(sourceClient),
              subscriptionProviderMap[sourceClient]?.contains(provider) == true
        else { return nil }
        return sourceClient
    }

    public static func suggestionRecords(
        entries: [ModelReportEntry],
        confirmed: [UsageAttribution.Record],
        subscriptionClients: [String]
    ) -> [UsageAttribution.Record] {
        rows(entries: entries, confirmed: confirmed, suggestions: []).compactMap { row in
            guard case .unassigned = row.state,
                  let target = suggestionTarget(
                    sourceClient: row.client,
                    provider: row.provider,
                    subscriptionClients: subscriptionClients)
            else { return nil }
            return UsageAttribution.Record(
                client: row.client, provider: row.provider, state: .assigned(target))
        }
    }

    /// Returns only provider-level proposals that are still unassigned. The
    /// caller stores them with `suggestionsRaw`; only explicit acceptance may
    /// pass the same records to `confirmedRaw`.
    public static func acceptanceRecords(rows: [Row]) -> [UsageAttribution.Record] {
        rows.compactMap { row in
            guard case .unassigned = row.state,
                  let target = row.suggestedTarget
            else { return nil }
            return UsageAttribution.Record(
                client: row.client, provider: row.provider, state: .assigned(target))
        }
    }

    public static func writeFailure(
        table: UsageAttribution.Table,
        record: UsageAttribution.Record,
        result: String?
    ) -> WriteFailure? {
        guard result == nil else { return nil }
        guard table.isWritable else { return .invalidExistingValue }
        let existingKey = table.records.contains {
            $0.client == record.client && $0.provider == record.provider && $0.model == record.model
        }
        if table.records.count >= UsageAttribution.maxEntries && !existingKey {
            return .entryLimit
        }
        return .sizeOrInvalidRecord
    }

    public static func signature(
        entries: [ModelReportEntry], subscriptionClients: [String]
    ) -> String {
        let entrySignature = entries.map {
            [
                $0.client, $0.provider, $0.model, String($0.total),
                String($0.cost.bitPattern),
            ].joined(separator: "\u{1f}")
        }.joined(separator: "\u{1e}")
        return entrySignature + "|" + subscriptionClients.joined(separator: ",")
    }

    private static func sourceKey(client: String, provider: String) -> String {
        "\(client)\u{0}\(provider)"
    }
}
