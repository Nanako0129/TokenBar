import SwiftUI
import TokenBarCore

enum ModelBarGeometry {
    static let gap: CGFloat = 1

    static func widths(values: [Int64], totalWidth: CGFloat) -> [CGFloat] {
        guard !values.isEmpty, totalWidth > 0 else { return [] }
        let available = max(0, totalWidth - gap * CGFloat(values.count - 1))
        let minimum = min(1, available / CGFloat(values.count))
        let remainder = available - minimum * CGFloat(values.count)
        let weights = values.map { Double(max(0, $0)).squareRoot() }
        let totalWeight = weights.reduce(0, +)
        guard totalWeight > 0 else { return Array(repeating: 0, count: values.count) }
        return weights.map { minimum + remainder * CGFloat($0 / totalWeight) }
    }
}

/// "Models" card: one row per model sorted by cost, with the token-category
/// split as a stacked bar. Port of ModelBreakdownCard.tsx — bar widths use a
/// square-root scale (cache reads dwarf everything linearly; log flattens
/// everything), while the hover help reports true counts and linear shares.
struct ModelBreakdownCard: View {
    let report: ModelReport?
    /// Restrict rows to these clients; empty = show everything.
    var clientIds: [String] = []
    let colors: ModelColorMap
    var title = "Models"

    @State private var expanded = false
    @State private var hover: HoverState?
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var popoverScrollViewport

    private struct HoverState {
        let entry: ModelReportEntry
        /// Cursor location in the rows container's coordinate space.
        let point: CGPoint
    }

    private static let maxRows = 8
    private static let rowsSpace = "model-rows"

    /// Token-category palette (matches the Tauri .model-seg-* CSS classes).
    private static let tokenKinds: [(label: String, color: String, pick: (ModelReportEntry) -> Int64)] = [
        ("Input", "#3b82f6", { $0.input }),
        ("Output", "#22c55e", { $0.output }),
        ("Cache read", "#f59e0b", { $0.cacheRead }),
        ("Cache write", "#a855f7", { $0.cacheWrite }),
        ("Reasoning", "#ec4899", { $0.reasoning }),
    ]

    var body: some View {
        let allow = Set(clientIds)
        let rows = (report?.entries ?? [])
            .filter { allow.contains($0.client) }
            .sorted { $0.cost != $1.cost ? $0.cost > $1.cost : $0.total > $1.total }
        let totalCost = rows.reduce(0) { $0 + $1.cost }

        DashCard(
            title,
            trailing: {
                VStack(alignment: .trailing, spacing: 1) {
                    Text("\(rows.count) model\(rows.count == 1 ? "" : "s") · \(Format.usd(totalCost))")
                        .foregroundStyle(.secondary)
                    if let updatedAt = report?.pricingUpdatedAt {
                        Text("Prices updated \(Format.relativeTime(updatedAt))")
                            .foregroundStyle(.tertiary)
                            .help("LiteLLM pricing data; refreshes automatically about once an hour")
                    }
                }
                .font(.caption2)
            }
        ) {
            if rows.isEmpty {
                Text("No model usage in this range")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                legend
                VStack(spacing: 8) {
                    ForEach(
                        expanded ? rows : Array(rows.prefix(Self.maxRows)),
                        id: \.self.rowID
                    ) { entry in
                        row(entry)
                    }
                }
                .coordinateSpace(name: Self.rowsSpace)
                .overlay(alignment: .topLeading) {
                    if let hover {
                        GeometryReader { geo in
                            let measuredSize = tooltipSize == .zero
                                ? CGSize(width: ModelUsageTooltip.width, height: 120)
                                : tooltipSize
                            let offset = PopoverTooltipPlacement.offset(
                                anchor: hover.point,
                                tooltipSize: measuredSize,
                                containerFrame: geo.frame(in: .global),
                                viewport: popoverScrollViewport)
                            tooltip(hover.entry)
                                .offset(offset ?? .zero)
                        }
                        // GeometryReader fills the rows; keep hits on the rows
                        // so continuous hover does not end while the tooltip
                        // is up.
                        .allowsHitTesting(false)
                    }
                }
                let hidden = rows.count - min(rows.count, Self.maxRows)
                if hidden > 0 {
                    Button(expanded ? "Show less" : "Show \(hidden) more") {
                        expanded.toggle()
                    }
                    .buttonStyle(.plain)
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(.secondary)
                }
            }
        }
        .zIndex(hover == nil ? 0 : 1)
    }

    private var legend: some View {
        FlowLayout(hSpacing: 8, vSpacing: 3) {
            ForEach(Self.tokenKinds, id: \.label) { kind in
                HStack(spacing: 4) {
                    RoundedRectangle(cornerRadius: 1.5)
                        .fill(Color(hex: kind.color))
                        .frame(width: 7, height: 7)
                    Text(kind.label)
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
            }
        }
    }

    private func row(_ entry: ModelReportEntry) -> some View {
        let isHovered = hover?.entry.rowID == entry.rowID
        return HStack(spacing: 8) {
            Circle()
                .fill(Color(hex: colors.color(entry.provider, entry.model)))
                .frame(width: 8, height: 8)
                .overlay {
                    Circle().stroke(
                        Color.primary.opacity(isHovered ? 0.85 : 0),
                        lineWidth: 1)
                }
                .shadow(
                    color: Color.primary.opacity(isHovered ? 0.65 : 0),
                    radius: isHovered ? 3 : 0)
            VStack(alignment: .leading, spacing: 3) {
                Text(entry.model)
                    .font(.caption)
                    .lineLimit(1)
                    .truncationMode(.middle)
                segmentBar(entry, isHovered: isHovered)
            }
            Spacer(minLength: 8)
            VStack(alignment: .trailing, spacing: 2) {
                Text(Format.compactTokens(entry.total))
                    .font(.caption.monospacedDigit())
                Text(Format.usd(entry.cost))
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(Color(hex: "#22c55e"))
            }
        }
        // Float the rich tooltip near the cursor anywhere on the row — the
        // web card tracks just the bar, but a 6pt-tall hover target is too
        // fiddly with a real pointer.
        .contentShape(Rectangle())
        .onContinuousHover(coordinateSpace: .named(Self.rowsSpace)) { phase in
            switch phase {
            case let .active(point):
                hover = HoverState(entry: entry, point: point)
            case .ended:
                hover = nil
            }
        }
    }

    /// sqrt-scaled stacked category bar.
    private func segmentBar(_ entry: ModelReportEntry, isHovered: Bool) -> some View {
        let segments = Self.tokenKinds
            .map { (color: $0.color, value: $0.pick(entry)) }
            .filter { $0.value > 0 }
        return GeometryReader { geo in
            let widths = ModelBarGeometry.widths(
                values: segments.map(\.value), totalWidth: geo.size.width)
            HStack(spacing: ModelBarGeometry.gap) {
                ForEach(segments.indices, id: \.self) { i in
                    RoundedRectangle(cornerRadius: 1.5)
                        .fill(Color(hex: segments[i].color))
                        .frame(width: widths[i])
                }
            }
        }
        .frame(height: 6)
        .overlay {
            RoundedRectangle(cornerRadius: 1.5)
                .stroke(Color.primary.opacity(isHovered ? 0.85 : 0), lineWidth: 1)
        }
        .shadow(
            color: Color.primary.opacity(isHovered ? 0.65 : 0),
            radius: isHovered ? 3 : 0)
    }

    // MARK: - Hover tooltip

    /// True token counts and linear shares per category — the bar itself is
    /// sqrt-scaled, so this is where the real numbers live.
    private func tooltip(_ entry: ModelReportEntry) -> some View {
        ModelUsageTooltip(
            model: entry.model,
            provider: entry.provider,
            context: ClientRegistry.style(entry.client).displayName,
            color: colors.color(entry.provider, entry.model),
            input: entry.input,
            output: entry.output,
            cacheRead: entry.cacheRead,
            cacheWrite: entry.cacheWrite,
            reasoning: entry.reasoning,
            total: entry.total,
            cost: entry.cost,
            measuredSize: $tooltipSize)
    }
}

struct ModelUsageTooltip: View {
    static let width: CGFloat = 210

    let model: String
    let provider: String
    let context: String?
    let color: String
    let input: Int64
    let output: Int64
    let cacheRead: Int64
    let cacheWrite: Int64
    let reasoning: Int64
    let total: Int64
    let cost: Double
    @Binding var measuredSize: CGSize

    private var kinds: [(label: String, color: String, value: Int64)] {
        [
            ("Input", "#3b82f6", input),
            ("Output", "#22c55e", output),
            ("Cache read", "#f59e0b", cacheRead),
            ("Cache write", "#a855f7", cacheWrite),
            ("Reasoning", "#ec4899", reasoning),
        ]
        .filter { $0.value > 0 }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 5) {
                Circle()
                    .fill(Color(hex: color))
                    .frame(width: 6, height: 6)
                Text(model)
                    .font(.caption.weight(.semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Text([context, provider].compactMap { $0 }.joined(separator: " · "))
                .font(.caption2)
                .foregroundStyle(.tertiary)
            HStack {
                Text("\(Format.compactTokens(total)) tokens")
                Spacer()
                Text(Format.usd(cost))
            }
            .font(.caption2)
            .foregroundStyle(.secondary)
            ForEach(kinds, id: \.label) { kind in
                HStack(spacing: 4) {
                    RoundedRectangle(cornerRadius: 1.5)
                        .fill(Color(hex: kind.color))
                        .frame(width: 6, height: 6)
                    Text(kind.label)
                    Spacer()
                    Text("\(Format.compactTokens(kind.value)) · \(Int((Double(kind.value) / Double(max(1, total)) * 100).rounded()))%")
                        .foregroundStyle(.secondary)
                }
                .font(.caption2)
            }
        }
        .padding(8)
        .frame(width: Self.width, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.quaternary))
        .onGeometryChange(for: CGSize.self) { $0.size } action: { measuredSize = $0 }
        .allowsHitTesting(false)
    }
}

extension ModelReportEntry {
    /// Stable row identity across re-sorts (client+model+provider triple).
    var rowID: String { "\(client)|\(model)|\(provider)" }
}
