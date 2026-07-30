import Foundation

/// User-declared billing attribution for provider-split model report rows.
/// The confirmed and suggestion stores intentionally share a codec but never a
/// read path: consulting suggestions here would silently turn a proposal into
/// billing truth.
public enum UsageAttribution {
    public static let confirmedKey = "tokenbar.usage.attribution.confirmed"
    public static let suggestionsKey = "tokenbar.usage.attribution.suggestions"
    public static let maxEntries = 128
    public static let maxRawBytes = 64 * 1024

    public enum State: Equatable, Hashable, Sendable {
        case assigned(String)
        case excluded
        case unassigned
    }

    public struct Record: Equatable, Hashable, Sendable {
        public let client: String
        public let provider: String
        public let model: String?
        public let state: State

        public init(client: String, provider: String, model: String? = nil, state: State) {
            self.client = client
            self.provider = provider
            self.model = model
            self.state = state
        }
    }

    public struct Table: Equatable, Sendable {
        public let records: [Record]
        /// False means the stored value was present but foreign to this codec;
        /// keeping that bit with the table prevents a rejected read from being
        /// written back as a new empty declaration set.
        public let isWritable: Bool

        public init(records: [Record] = []) {
            self.records = records
            self.isWritable = true
        }

        init(records: [Record], isWritable: Bool) {
            self.records = records
            self.isWritable = isWritable
        }

        public func state(for entry: ModelReportEntry) -> State {
            UsageAttribution.resolve(
                client: entry.client, provider: entry.provider, model: entry.model,
                records: records)
        }
    }

    public static func confirmed(defaults: UserDefaults = .standard) -> Table {
        parseState(defaults.object(forKey: confirmedKey))
    }

    public static func suggestions(defaults: UserDefaults = .standard) -> Table {
        parseState(defaults.object(forKey: suggestionsKey))
    }

    /// Effective state reads only confirmed declarations. Suggestions remain
    /// available to a later acceptance UI but cannot affect report totals.
    public static func effectiveState(
        for entry: ModelReportEntry, defaults: UserDefaults = .standard
    ) -> State {
        confirmed(defaults: defaults).state(for: entry)
    }

    public static func resolve(
        client: String, provider: String, model: String?, records: [Record]
    ) -> State {
        if let model,
           let override = records.first(where: {
               $0.client == client && $0.provider == provider && $0.model == model
           })
        {
            return override.state
        }
        return records.first {
            $0.client == client && $0.provider == provider && $0.model == nil
        }?.state ?? .unassigned
    }

    public static func resolve(_ entry: ModelReportEntry, records: [Record]) -> State {
        resolve(
            client: entry.client, provider: entry.provider, model: entry.model,
            records: records)
    }

    /// Parse a raw @AppStorage string. A missing value is an empty writable
    /// table; malformed or semantically invalid data is an empty read-only
    /// table so a later mutation cannot overwrite data this codec does not
    /// understand.
    public static func parseRaw(_ raw: String?) -> Table {
        parseState(raw)
    }

    /// Keep canonical encoding private: public callers must update the
    /// observed raw value so malformed or foreign data cannot be replaced by
    /// serializing the empty table returned by a rejected read.
    private static func canonicalRaw(_ records: [Record]) -> String? {
        guard records.count <= maxEntries,
              records.allSatisfy(isPersistable),
              Set(records.map(sourceKey)).count == records.count
        else { return nil }

        let objects = records.sorted(by: recordPrecedes).map { record in
            var object: [String: Any] = [
                "client": record.client,
                "provider": record.provider,
                "state": stateName(record.state),
            ]
            object["model"] = record.model ?? NSNull()
            if case let .assigned(target) = record.state {
                object["target"] = target
            }
            return object
        }
        guard let data = try? JSONSerialization.data(
            withJSONObject: objects, options: [.sortedKeys]),
            data.count <= maxRawBytes
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    /// Replace or remove one source-key declaration while preserving the
    /// observed raw value's fail-closed boundary. `.unassigned` removes the
    /// key; it is never serialized as a record. The `Any?` input lets callers
    /// pass `UserDefaults.object(forKey:)` without turning a present foreign
    /// value into the writable nil used for an absent key.
    public static func confirmedRaw(updating raw: Any?, record: Record) -> String? {
        updatedRaw(raw, record: record)
    }

    public static func suggestionsRaw(updating raw: Any?, record: Record) -> String? {
        updatedRaw(raw, record: record)
    }

    private struct SourceKey: Hashable {
        let client: String
        let provider: String
        let model: String?
    }

    private static func updatedRaw(_ raw: Any?, record: Record) -> String? {
        let parsed = parseState(raw)
        guard isValidSource(record), parsed.isWritable else { return nil }
        var records = parsed.records
        records.removeAll { sourceKey($0) == sourceKey(record) }
        if case .unassigned = record.state {
            return canonicalRaw(records)
        }
        records.append(record)
        return canonicalRaw(records)
    }

    private static func parseState(_ value: Any?) -> Table {
        guard let value else { return Table() }
        // UserDefaults nil means absent; every present non-String belongs to a
        // foreign writer and must not be replaced by this codec.
        guard let raw = value as? String else {
            return Table(records: [], isWritable: false)
        }
        guard raw.utf8.count <= maxRawBytes,
              let data = raw.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data),
              let array = object as? [Any], array.count <= maxEntries
        else {
            return Table(records: [], isWritable: false)
        }

        var records: [Record] = []
        for value in array {
            guard let dictionary = value as? [String: Any],
                  let record = parseRecord(dictionary)
            else {
                return Table(records: [], isWritable: false)
            }
            records.append(record)
        }
        guard Set(records.map(sourceKey)).count == records.count else {
            return Table(records: [], isWritable: false)
        }
        return Table(records: records.sorted(by: recordPrecedes), isWritable: true)
    }

    private static func parseRecord(_ dictionary: [String: Any]) -> Record? {
        guard let client = dictionary["client"] as? String,
              let provider = dictionary["provider"] as? String,
              let state = dictionary["state"] as? String,
              let modelValue = dictionary["model"]
        else { return nil }

        let model: String?
        if modelValue is NSNull {
            model = nil
        } else if let value = modelValue as? String {
            model = value
        } else {
            return nil
        }

        switch state {
        case "assigned":
            guard Set(dictionary.keys) == Set(["client", "model", "provider", "state", "target"]),
                  let target = dictionary["target"] as? String
            else { return nil }
            let record = Record(
                client: client, provider: provider, model: model, state: .assigned(target))
            return isPersistable(record) ? record : nil
        case "excluded":
            guard Set(dictionary.keys) == Set(["client", "model", "provider", "state"]) else {
                return nil
            }
            let record = Record(
                client: client, provider: provider, model: model, state: .excluded)
            return isPersistable(record) ? record : nil
        default:
            return nil
        }
    }

    private static func isPersistable(_ record: Record) -> Bool {
        // Keep both ends of an assignment in the registry: an arbitrary target
        // would create a billing bucket the quota layer cannot render.
        guard ClientRegistry.allIds.contains(record.client) else { return false }
        switch record.state {
        case let .assigned(target):
            return ClientRegistry.allIds.contains(target)
        case .excluded:
            return true
        case .unassigned:
            return false
        }
    }

    private static func isValidSource(_ record: Record) -> Bool {
        ClientRegistry.allIds.contains(record.client)
            && (record.state == .unassigned || isPersistable(record))
    }

    private static func sourceKey(_ record: Record) -> SourceKey {
        SourceKey(client: record.client, provider: record.provider, model: record.model)
    }

    private static func stateName(_ state: State) -> String {
        switch state {
        case .assigned: return "assigned"
        case .excluded: return "excluded"
        case .unassigned: return "unassigned"
        }
    }

    private static func recordPrecedes(_ lhs: Record, _ rhs: Record) -> Bool {
        if lhs.client != rhs.client { return lhs.client < rhs.client }
        if lhs.provider != rhs.provider { return lhs.provider < rhs.provider }
        switch (lhs.model, rhs.model) {
        case (nil, nil): return false
        case (nil, _): return true
        case (_, nil): return false
        case let (left?, right?): return left < right
        }
    }
}
