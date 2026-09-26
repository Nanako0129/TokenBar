import SwiftUI

/// The seven-lens tab row under the header, port of ViewSwitch.tsx.
struct ViewSwitch: View {
    @Binding var active: AppView
    let views: [AppView]

    @Namespace private var thumb

    var body: some View {
        HStack(spacing: 2) {
            ForEach(views, id: \.self) { view in
                Button {
                    active = view
                } label: {
                    Text(view.label)
                        .font(.caption.weight(active == view ? .semibold : .regular))
                        .foregroundStyle(active == view ? .primary : .secondary)
                        .lineLimit(1)
                        .minimumScaleFactor(0.75)
                        .padding(.horizontal, 4)
                        .padding(.vertical, 4)
                        .frame(maxWidth: .infinity)
                        .modifier(SelectionBackground(
                            isSelected: active == view,
                            shape: RoundedRectangle(cornerRadius: 6), namespace: thumb))
                        .contentShape(RoundedRectangle(cornerRadius: 6))
                }
                .buttonStyle(.plain)
            }
        }
        .padding(2)
        .glassCard(cornerRadius: 8)
        .panelSelectionSlide(active)
    }
}
