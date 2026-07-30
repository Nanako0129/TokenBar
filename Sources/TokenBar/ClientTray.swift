import Foundation
import TokenBarCore

/// Process-lifetime navigation memory shared by the main item, individual
/// items, and the popover tab bar. It deliberately stays out of UserDefaults:
/// M2 adds only the two item preferences. The main item retains its own full
/// route, while every client independently retains its last lens.
final class StatusItemRouteMemory {
    struct Route: Equatable {
        let clientId: String
        let view: String
    }

    private enum Owner {
        case main
        case client
    }

    private var owner: Owner = .main
    private var mainClient: String
    /// Lens the MAIN item last showed for each client tab.
    private var mainViews: [String: String] = [:]
    /// Lens each INDIVIDUAL client item last showed. Deliberately a second map:
    /// a shared one lets whichever surface moved last decide what the other one
    /// opens on — including a first click on a client item inheriting the lens
    /// the previous session persisted for the main item.
    private var clientViews: [String: String] = [:]

    init(mainClient: String, mainView: String) {
        self.mainClient = mainClient
        mainViews[mainClient] = Self.normalizedView(mainView)
    }

    private var mainRoute: Route {
        Route(
            clientId: mainClient,
            view: mainViews[mainClient] ?? AppView.overview.rawValue)
    }

    /// Records into whichever surface currently owns the popover, never both.
    func record(clientId: String, view: String) {
        let view = Self.normalizedView(view)
        switch owner {
        case .main:
            mainClient = clientId
            mainViews[clientId] = view
        case .client:
            clientViews[clientId] = view
        }
    }

    func activateMain(currentClient: String, currentView: String) -> Route {
        record(clientId: currentClient, view: currentView)
        owner = .main
        return mainRoute
    }

    func activateClient(
        _ clientId: String, currentClient: String, currentView: String
    ) -> Route {
        record(clientId: currentClient, view: currentView)
        owner = .client
        return Route(
            clientId: clientId,
            view: clientViews[clientId] ?? AppView.overview.rawValue)
    }

    func switchClient(
        from currentClient: String, currentView: String, to nextClient: String
    ) -> Route {
        record(clientId: currentClient, view: currentView)
        let views = owner == .main ? mainViews : clientViews
        let route = Route(
            clientId: nextClient,
            view: views[nextClient] ?? AppView.overview.rawValue)
        record(clientId: route.clientId, view: route.view)
        return route
    }

    private static func normalizedView(_ view: String) -> String {
        AppView(rawValue: view)?.rawValue ?? AppView.overview.rawValue
    }
}

/// Pure contracts shared by individual status items and the Settings surface.
enum ClientTray {
    static let enabledKey = "tokenbar.tray.clients.enabled"
    static let selectionsKey = "tokenbar.tray.clients.quotaSelections"
    static let activeTabKey = "tokenbar.activeTab"
    static let activeViewKey = "tokenbar.view"
    static let overviewTab = "overview"
    static let autoSelection = QuotaResolver.auto

    static let maxEnabledRawBytes = 8 * 1024
    static let maxSelectionRawBytes = 64 * 1024
    static let maxEntries = 128
    static let maxClientIDBytes = 128
    static let maxCardIDBytes = 4096

    enum Status: String, Equatable, Sendable {
        case available
        case suppressed
        case unavailable
        case errorAuto
        case errorExplicit
        case missingSelection

        var hint: String? {
            switch self {
            case .available: return nil
            case .suppressed: return "Hidden client tab; the item is suppressed."
            case .unavailable: return "Quota unavailable."
            case .errorAuto: return "Quota unavailable."
            case .errorExplicit: return "Using the last good quota window."
            case .missingSelection: return "Unavailable selection"
            }
        }
    }

    /// A Picker option; selection is decided by `tag` matching the row's stored
    /// selection, so no separate selected flag exists to drift out of sync.
    struct WindowOption: Identifiable, Equatable, Sendable {
        let tag: String
        let label: String
        let isEnabled: Bool

        var id: String { tag }
    }

    struct SettingsRow: Identifiable, Equatable, Sendable {
        let clientId: String
        let displayName: String
        let isEnabled: Bool
        let selection: String
        let remainingPercent: Double?
        let status: Status
        let options: [WindowOption]

        var id: String { clientId }

        var statusHint: String? { status.hint }

        var valueText: String { ClientTray.percentText(remainingPercent) }

        var accessibilityLabel: String {
            ClientTray.quotaAccessibilityLabel(displayName, remainingPercent)
        }
    }

    /// Display-ready only: selection card IDs and provider diagnostics never
    /// reach a status item, and never participate in item reconciliation.
    struct Presentation: Equatable, Sendable {
        let clientId: String
        let displayName: String
        let remainingPercent: Double?
        let status: Status

        var processIdentity: String { ClientTray.processIdentity(for: clientId) }
        var autosaveName: String { ClientTray.autosaveName(for: clientId) }

        var valueText: String { ClientTray.percentText(remainingPercent) }

        var toolTip: String {
            if status == .missingSelection {
                return "%@ — selected quota window unavailable".localized(displayName)
            }
            if let percent = ClientTray.percentInt(remainingPercent) {
                if status == .errorExplicit {
                    return "%@ — %lld%% last known quota remaining".localized(
                        displayName, percent)
                }
                return "%@ — %lld%% quota remaining".localized(displayName, percent)
            }
            return "%@ — quota unavailable".localized(displayName)
        }

        var accessibilityLabel: String {
            if status == .missingSelection {
                return "%@, selected quota window unavailable".localized(displayName)
            }
            if status == .errorExplicit,
               let percent = ClientTray.percentInt(remainingPercent)
            {
                return "%@, %lld percent last known quota remaining".localized(
                    displayName, percent)
            }
            return ClientTray.quotaAccessibilityLabel(displayName, remainingPercent)
        }
    }

    static func enabled(defaults: UserDefaults = .standard) -> Set<String> {
        parseEnabledRaw(defaults.object(forKey: enabledKey) as? String)
    }

    /// Settings owns both preferences through `@AppStorage`, so mutation is a
    /// pure raw-to-raw transform; nil means the stored raw is out of bounds and
    /// must not be repaired by write-back.
    static func enabledRaw(
        updating raw: String?, clientId: String, enabled: Bool
    ) -> String? {
        guard isValidClientID(clientId), enabledInputIsWritable(raw) else { return nil }
        var ids = parseEnabledRaw(raw)
        if enabled {
            ids.insert(clientId)
        } else {
            ids.remove(clientId)
        }
        return canonicalEnabledRaw(ids)
    }

    static func selections(defaults: UserDefaults = .standard) -> [String: String] {
        parseSelectionsRaw(defaults.object(forKey: selectionsKey) as? String)
    }

    static func selectionsRaw(
        updating raw: String?, clientId: String, selection: String
    ) -> String? {
        guard isValidClientID(clientId), isValidSelection(selection) else { return nil }
        let state = selectionParseState(raw)
        guard state.isWritable else { return nil }
        var values = state.values
        values[clientId] = selection
        return canonicalSelectionsRaw(values)
    }

    static func parseEnabledRaw(_ raw: String?) -> Set<String> {
        guard let raw else { return [] }
        guard enabledInputIsWritable(raw) else { return [] }
        let ids = ClientRegistry.parseIdList(raw)
        guard ids.count <= maxEntries else { return [] }
        return Set(ids.filter(isValidClientID))
    }

    static func parseSelectionsRaw(_ raw: String?) -> [String: String] {
        selectionParseState(raw).values
    }

    static func canonicalEnabledRaw(_ ids: Set<String>) -> String? {
        let valid = ids.filter(isValidClientID)
        guard valid.count <= maxEntries else { return nil }
        let raw = valid.sorted().joined(separator: ",")
        return raw.utf8.count <= maxEnabledRawBytes ? raw : nil
    }

    static func canonicalSelectionsRaw(_ values: [String: String]) -> String? {
        guard values.count <= maxEntries else { return nil }
        var valid: [String: String] = [:]
        for (clientId, selection) in values {
            guard isValidClientID(clientId), isValidSelection(selection) else { continue }
            valid[clientId] = selection
        }
        guard valid.count <= maxEntries,
              JSONSerialization.isValidJSONObject(valid)
        else { return nil }
        guard let data = try? JSONSerialization.data(
            withJSONObject: valid, options: [.sortedKeys]),
              data.count <= maxSelectionRawBytes
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func resolveWindow(
        payload: AgentUsagePayload?, clientId: String, selection: String
    ) -> UsageWindow? {
        let quotaClientId = quotaClientID(for: clientId)
        guard let agent = payload?.agents.first(where: { $0.clientId == quotaClientId }) else {
            return nil
        }
        let windows = agent.uniqueCardWindows
        if selection == autoSelection {
            guard agent.error == nil else { return nil }
            return windows
                .filter { $0.remainingPercent.isFinite }
                .min { $0.remainingPercent < $1.remainingPercent }
        }
        return windows.first { $0.cardId == selection }
    }

    static func settingsRows(
        presentClients: [String],
        payload: AgentUsagePayload?,
        enabled: Set<String>,
        selections: [String: String],
        hidden: Set<String>,
        orderRaw: String,
        officialClients: Set<String>
    ) -> [SettingsRow] {
        let present = canonicalIDs(presentClients)
        // Agent presence in the quota payload is the stable capability signal.
        // A supported provider can temporarily return only an error and zero
        // windows (for example while its local OAuth client is unavailable);
        // hiding that row would make first-time configuration impossible during
        // the outage. The row remains selectable and shows an unavailable value.
        let quotaCapable = Set(payload?.agents.map(\.clientId) ?? [])
        let newlyConfigurable = present.filter {
            quotaCapable.contains(quotaClientID(for: $0)) && officialClients.contains($0)
        }
        // Derived from the ordered `present` array, not by iterating the enabled
        // Set: `orderedClients` returns its input untouched when no tab order is
        // saved, so hash order would leak into the UI and then visibly reshuffle
        // once the payload lands and `newlyConfigurable` supplies graph order.
        let preserved = present.filter {
            enabled.contains($0) && officialClients.contains($0)
        }
        var ids = newlyConfigurable
        for id in preserved where !ids.contains(id) { ids.append(id) }
        ids = ClientRegistry.orderedClients(ids, orderRaw: orderRaw)

        return ids.map { clientId in
            let selection = selections[clientId] ?? autoSelection
            let quotaClientId = quotaClientID(for: clientId)
            let snapshot = payload?.agents.first { $0.clientId == quotaClientId }
            let windows = snapshot?.uniqueCardWindows ?? []
            let resolved = resolveWindow(
                payload: payload, clientId: clientId, selection: selection)
            var options = [
                WindowOption(
                    tag: autoSelection,
                    // Short on purpose: the row is already scoped to one client
                    // and labeled "Window", so both qualifiers would be padding
                    // in a picker that truncates at 210pt. The section hint
                    // spells out what Auto picks.
                    label: "Auto (tightest)",
                    isEnabled: true)
            ]
            options.append(contentsOf: windows.enumerated().map { index, window in
                WindowOption(
                    tag: window.cardId,
                    label: safeWindowLabel(window.label, index: index),
                    isEnabled: true)
            })
            let selectionIsMissing = selection != autoSelection
                && !windows.contains(where: { $0.cardId == selection })
            if selectionIsMissing {
                // Tagged with the stored selection so the Picker still resolves a
                // tag (SwiftUI logs and misbehaves otherwise) without ever
                // rendering the raw card ID.
                options.append(
                    WindowOption(
                        tag: selection,
                        label: "Unavailable selection",
                        isEnabled: false))
            }

            let status: Status
            if hidden.contains(clientId) {
                status = .suppressed
            } else if selectionIsMissing {
                status = .missingSelection
            } else if let snapshot, snapshot.error != nil {
                status = selection == autoSelection ? .errorAuto : .errorExplicit
            } else if resolved == nil {
                status = .unavailable
            } else {
                status = .available
            }

            return SettingsRow(
                clientId: clientId,
                displayName: ClientRegistry.style(clientId).displayName,
                isEnabled: enabled.contains(clientId),
                selection: selection,
                remainingPercent: resolved?.remainingPercent,
                status: status,
                options: options)
        }
    }

    static func runtimePresentations(
        graph: UsagePayload?,
        payload: AgentUsagePayload?,
        enabled: Set<String>,
        selections: [String: String],
        hidden: Set<String>,
        officialClients: Set<String>
    ) -> [Presentation] {
        let present = canonicalIDs(graph?.summary.clients ?? [])
        return present
            .filter { enabled.contains($0) && officialClients.contains($0) && !hidden.contains($0) }
            .sorted()
            .map { clientId in
                let selection = selections[clientId] ?? autoSelection
                let resolved = resolveWindow(
                    payload: payload, clientId: clientId, selection: selection)
                // Distinguish "this provider has no value right now" from "the
                // window you picked no longer exists", which needs a Settings
                // change rather than waiting. The message is fixed text; the raw
                // cardId is never surfaced.
                let snapshot = payload?.agents
                    .first { $0.clientId == quotaClientID(for: clientId) }
                let windows = snapshot?.uniqueCardWindows ?? []
                let selectionIsMissing = selection != autoSelection
                    && !windows.contains { $0.cardId == selection }
                let status: Status
                if selectionIsMissing {
                    status = .missingSelection
                } else if snapshot?.error != nil {
                    status = selection == autoSelection ? .errorAuto : .errorExplicit
                } else {
                    status = resolved == nil ? .unavailable : .available
                }
                return Presentation(
                    clientId: clientId,
                    displayName: ClientRegistry.style(clientId).displayName,
                    remainingPercent: resolved?.remainingPercent,
                    status: status)
            }
    }

    /// One rounding site: every displayed percent, tooltip, and accessibility
    /// label derives from this, so they can never disagree.
    static func percentInt(_ value: Double?) -> Int? {
        guard let value, value.isFinite else { return nil }
        return Int(min(100, max(0, value)).rounded())
    }

    static func percentText(_ value: Double?) -> String {
        percentInt(value).map { "\($0)%" } ?? "—%"
    }

    static func quotaAccessibilityLabel(_ displayName: String, _ value: Double?) -> String {
        if let percent = percentInt(value) {
            return "%@, %lld percent quota remaining".localized(displayName, percent)
        }
        return "%@, quota unavailable".localized(displayName)
    }

    /// Maps only the quota lookup boundary. Antigravity CLI usage is a distinct
    /// tab and status-item identity, but its quota is published by the shared
    /// Antigravity provider snapshot.
    static func quotaClientID(for clientId: String) -> String {
        clientId == "antigravity-cli" ? "antigravity" : clientId
    }

    static func processIdentity(for clientId: String) -> String {
        "client:\(clientId)"
    }

    static func autosaveName(for clientId: String) -> String {
        "tokenbar-client-\(clientId)"
    }

    static func canonicalIDs(_ ids: [String]) -> [String] {
        var seen = Set<String>()
        return ids.map(ClientRegistry.canonicalClient).filter { seen.insert($0).inserted }
    }

    static func isValidClientID(_ id: String) -> Bool {
        guard id.utf8.count <= maxClientIDBytes,
              let first = id.utf8.first,
              isLowerASCII(first) || isDigitASCII(first)
        else { return false }
        return id.utf8.dropFirst().allSatisfy {
            isLowerASCII($0) || isDigitASCII($0) || $0 == 46 || $0 == 95 || $0 == 45
        }
    }

    private static func isValidSelection(_ selection: String) -> Bool {
        if selection == autoSelection { return true }
        return selection.utf8.count <= maxCardIDBytes
            && !selection.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private static let safeWindowLabels: Set<String> = [
        "Session", "Weekly", "Monthly", "Daily", "Pro", "Flash", "Extra usage"
    ]

    private static func safeWindowLabel(_ label: String, index: Int) -> String {
        if let known = safeWindowLabels.first(where: {
            label == $0 || label.hasPrefix("\($0) ·")
        }) {
            return known
        }
        return "Quota window %lld".localized(index + 1)
    }

    private static func isLowerASCII(_ byte: UInt8) -> Bool { (97...122).contains(byte) }
    private static func isDigitASCII(_ byte: UInt8) -> Bool { (48...57).contains(byte) }

    private static func enabledInputIsWritable(_ raw: String?) -> Bool {
        guard let raw else { return true }
        guard raw.utf8.count <= maxEnabledRawBytes else { return false }
        return ClientRegistry.parseIdList(raw).count <= maxEntries
    }

    private struct SelectionParseState {
        let values: [String: String]
        let isWritable: Bool
    }

    private static func selectionParseState(_ raw: String?) -> SelectionParseState {
        guard let raw else { return SelectionParseState(values: [:], isWritable: true) }
        guard raw.utf8.count <= maxSelectionRawBytes else {
            return SelectionParseState(values: [:], isWritable: false)
        }
        guard let data = raw.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data),
              let dictionary = object as? [String: Any]
        else {
            return SelectionParseState(values: [:], isWritable: true)
        }
        guard dictionary.count <= maxEntries else {
            return SelectionParseState(values: [:], isWritable: false)
        }
        var values: [String: String] = [:]
        for (clientId, value) in dictionary {
            guard let selection = value as? String,
                  isValidClientID(clientId), isValidSelection(selection)
            else { continue }
            values[clientId] = selection
        }
        return SelectionParseState(values: values, isWritable: true)
    }
}
