import SwiftUI
import TokenBarCore

/// Client tab row (Overview + one tab per present client), port of
/// DashboardTabs.tsx: horizontal scroll, active tab kept in view. SVG agent
/// icons arrive in a later phase — tabs show a brand-color disc with the
/// client's initial, the registry fallback the web app uses for icon-less
/// agents.
///
/// Client tabs are drag-reorderable in place; the drop writes the same
/// `tabOrderKey` the settings list and the quota cards share. Overview is
/// pinned first — it is neither draggable nor a drop target, so no drag can
/// move a client ahead of it.
struct DashboardTabs: View {
    let clients: [String]
    @Binding var active: String
    /// Show ⌘1…⌘9 pins while Cmd is held (the discoverability overlay).
    var kbdHints = false

    /// Shared client order (top tabs + quota cards). Written on drop, and
    /// observed so the row re-sorts the instant a reorder lands.
    @AppStorage(ClientRegistry.tabOrderKey) private var orderRaw = ""

    // Drag state, scoped to this row.
    @State private var dragId: String?
    @State private var overId: String?
    @State private var tabFrames: [String: CGRect] = [:]

    private static let dragSpace = "client-tabs-row"

    private struct TabFramesKey: PreferenceKey {
        static let defaultValue: [String: CGRect] = [:]
        static func reduce(value: inout [String: CGRect], nextValue: () -> [String: CGRect]) {
            value.merge(nextValue(), uniquingKeysWith: { $1 })
        }
    }

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 4) {
                    tab(id: "overview", label: "Overview".localized, color: nil, index: 1)
                    ForEach(Array(clients.enumerated()), id: \.element) { i, id in
                        let style = ClientRegistry.style(id)
                        tab(
                            id: id, label: ClientRegistry.shortName(id),
                            color: style.color, index: i + 2)
                            // Only client tabs publish a frame, so Overview can
                            // never be picked as a drop target.
                            .background(
                                GeometryReader { geo in
                                    Color.clear.preference(
                                        key: TabFramesKey.self,
                                        value: [id: geo.frame(in: .named(Self.dragSpace))])
                                })
                            .opacity(dragId == id ? 0.5 : 1)
                            .overlay(alignment: dropAlignment(for: id)) {
                                if let edge = dropEdge(for: id) {
                                    Capsule()
                                        .fill(Color.accentColor)
                                        .frame(width: 2)
                                        .padding(.vertical, 1)
                                        .offset(x: edge == .leading ? -3 : 3)
                                }
                            }
                            .help("Drag to reorder")
                            .gesture(dragGesture(for: id))
                    }
                }
                .coordinateSpace(name: Self.dragSpace)
                .onPreferenceChange(TabFramesKey.self) { tabFrames = $0 }
                // Let a plain vertical mouse wheel scroll this horizontal row.
                .background(HorizontalWheelScroll())
            }
            .onChange(of: active) { _, next in
                withAnimation(.easeOut(duration: 0.15)) {
                    proxy.scrollTo(next, anchor: nil)
                }
            }
        }
    }

    private func tab(id: String, label: String, color: String?, index: Int) -> some View {
        HStack(spacing: 5) {
            if color != nil {
                AgentIconView(clientId: id, size: 14)
            }
            Text(label)
                .font(.caption.weight(active == id ? .semibold : .regular))
                .lineLimit(1)
                .fixedSize()
            if kbdHints && index <= 9 {
                Text("⌘\(index)")
                    .font(.system(size: 8, weight: .semibold).monospacedDigit())
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 3)
                    .padding(.vertical, 1)
                    .background(.quaternary, in: RoundedRectangle(cornerRadius: 3))
            }
        }
        .foregroundStyle(active == id ? .primary : .secondary)
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(
            active == id ? AnyShapeStyle(.quaternary) : AnyShapeStyle(.clear),
            in: Capsule())
        .contentShape(Capsule())
        // A plain tap selects. Not a Button: the reorder DragGesture and a
        // Button's own press gesture would both claim the drag, and the button
        // action can still fire after a drop that ends inside its bounds.
        .onTapGesture { active = id }
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(active == id ? [.isButton, .isSelected] : .isButton)
        // A tap gesture is not an activation point for VoiceOver the way a
        // Button is, so restore one explicitly.
        .accessibilityAction { active = id }
        .id(id)
    }

    // MARK: - Drag reorder

    /// Which edge of the hovered tab the drop line sits on, matching
    /// `ClientRegistry.reorder`'s direction-aware insert (dragging right drops
    /// after the target, left drops before it). Static and pure so SelfTest can
    /// assert the direction mapping.
    static func dropEdge(
        dragId: String?, overId: String?, tabId: String, in clients: [String]
    ) -> HorizontalEdge? {
        guard let dragId, overId == tabId, dragId != tabId,
              let fromI = clients.firstIndex(of: dragId),
              let toI = clients.firstIndex(of: tabId)
        else { return nil }
        return fromI < toI ? .trailing : .leading
    }

    private func dropEdge(for id: String) -> HorizontalEdge? {
        Self.dropEdge(dragId: dragId, overId: overId, tabId: id, in: clients)
    }

    private func dropAlignment(for id: String) -> Alignment {
        dropEdge(for: id) == .leading ? .leading : .trailing
    }

    /// `minimumDistance` keeps a click a click: below the threshold the drag
    /// never starts and the tap gesture selects the tab instead.
    private func dragGesture(for id: String) -> some Gesture {
        DragGesture(minimumDistance: 4, coordinateSpace: .named(Self.dragSpace))
            .onChanged { value in
                dragId = id
                let over = tabFrames.first { $0.value.contains(value.location) }?.key
                overId = (over != nil && over != id) ? over : nil
            }
            .onEnded { _ in
                if let over = overId, over != id {
                    // `clients` is a subset of the shared order (it excludes
                    // hidden clients and quota-only ids). Merge the reordered
                    // subset back into the saved order so off-screen ids keep
                    // their slots instead of being dropped from the key.
                    let full = ClientRegistry.parseIdList(orderRaw)
                    let merged = ClientRegistry.mergeReorder(
                        full: full, visible: clients, from: id, to: over)
                    orderRaw = merged.joined(separator: ",")
                }
                dragId = nil
                overId = nil
            }
    }
}
