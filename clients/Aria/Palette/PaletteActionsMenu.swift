import SwiftUI

/// Every action on the page or its selected row, with its key.
struct PaletteActionsMenu: View {
    private static let width: CGFloat = 260
    private static let rowHeight: CGFloat = 28
    private static let inset: CGFloat = 6
    /// Keeps the menu's corners parallel to a selected row's.
    private static let cornerRadius = SelectionFill.cornerRadius + inset

    @ObservedObject var controller: PaletteController
    /// Observed because the actions read history state.
    @ObservedObject var history: HistoryState
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Group {
            if let selected = controller.model.menuIndex {
                list(selected: selected)
                    .transition(
                        reduceMotion ? .opacity : .opacity.combined(with: .scale(scale: 0.96, anchor: .bottomTrailing)))
            }
        }
        .animation(.paletteQuick, value: controller.model.menuIndex == nil)
    }

    private func list(selected: Int) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(controller.actions.enumerated()), id: \.element.id) { index, action in
                Button {
                    controller.runMenuAction(at: index)
                } label: {
                    HStack {
                        Text(action.title)
                        Spacer(minLength: 12)
                        PaletteKeyCap(action.key.label).accessibilityHidden(true)
                    }
                    .padding(.horizontal, 8)
                    .frame(minHeight: Self.rowHeight)
                    .selectionFill(index == selected)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .disabled(!action.isEnabled)
                .onHover { isHovering in
                    if isHovering { controller.selectMenuAction(at: index) }
                }
                .accessibilityAddTraits(index == selected ? .isSelected : [])
            }
            if controller.model.page == .commands {
                Text("⌘1–9 runs that prompt on this.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 8)
                    .padding(.top, 6)
                    .padding(.bottom, 2)
            }
        }
        .padding(Self.inset)
        .frame(width: Self.width)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: Self.cornerRadius))
        .overlay { RoundedRectangle(cornerRadius: Self.cornerRadius).strokeBorder(.quaternary) }
        .shadow(radius: 12, y: 4)
        .padding(.trailing, 10)
        .padding(.bottom, PaletteBarMetrics.footerHeight + 4)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Actions")
    }
}
