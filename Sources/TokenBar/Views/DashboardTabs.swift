import SwiftUI
import TokenBarCore

/// Client tab row (Overview + one tab per present client), port of
/// DashboardTabs.tsx: horizontal scroll, active tab kept in view. SVG agent
/// icons arrive in a later phase — tabs show a brand-color disc with the
/// client's initial, the registry fallback the web app uses for icon-less
/// agents.
struct DashboardTabs: View {
    let clients: [String]
    @Binding var active: String
    /// Show ⌘1…⌘9 pins while Cmd is held (the discoverability overlay).
    var kbdHints = false

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 4) {
                    tab(id: "overview", label: "Overview", color: nil, index: 1)
                    ForEach(Array(clients.enumerated()), id: \.element) { i, id in
                        let style = ClientRegistry.style(id)
                        tab(
                            id: id, label: ClientRegistry.shortName(id),
                            color: style.color, index: i + 2)
                    }
                }
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
        Button {
            active = id
        } label: {
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
        }
        .buttonStyle(.plain)
        .id(id)
    }
}
