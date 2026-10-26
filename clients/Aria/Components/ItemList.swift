import SwiftUI

private enum Layout {
    static let barButtonWidth: CGFloat = 26
    static let barButtonHeight: CGFloat = 22
    static let barDividerHeight: CGFloat = 14
    /// Pulls the bar left by the + glyph's side margin, so the glyph lines up
    /// with the rows' leading edge.
    static let barOutdent: CGFloat = 8
}

struct ItemList<Item, Row: View>: View {
    let items: [Item]
    let id: KeyPath<Item, String>
    /// Names one item to assistive tech: "App" gives "Add App".
    let noun: String
    let empty: String
    @Binding var selected: String?
    let canAdd: Bool
    let add: () -> Void
    let remove: (String) -> Void
    let open: (String) -> Void
    @ViewBuilder let row: (Item) -> Row

    var body: some View {
        Section {
            if items.isEmpty {
                Text(empty)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity)
            }
            ForEach(items, id: id) { item in
                line(item[keyPath: id], row(item))
            }
        } footer: {
            HStack(spacing: 0) {
                barButton("Add \(noun)", symbol: "plus", action: add)
                    .disabled(!canAdd)
                Divider().frame(height: Layout.barDividerHeight)
                barButton("Remove \(noun)", symbol: "minus") {
                    if let target {
                        remove(target)
                        selected = nil
                    }
                }
                .disabled(target == nil)
            }
            .fixedSize()
            .padding(.leading, -Layout.barOutdent)
        }
    }

    private var target: String? {
        items.contains { $0[keyPath: id] == selected } ? selected : nil
    }

    private func line(_ itemID: String, _ content: Row) -> some View {
        HStack {
            Button {
                selected = itemID
            } label: {
                content
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityAddTraits(target == itemID ? .isSelected : [])
            Button("Open \(noun)", systemImage: "chevron.right") { open(itemID) }
                .labelStyle(.iconOnly)
                .font(.footnote.weight(.semibold))
                .foregroundStyle(.tertiary)
                .frame(width: Layout.barButtonWidth, height: Layout.barButtonHeight)
                .contentShape(Rectangle())
                .buttonStyle(.plain)
        }
        .contextMenu {
            Button("Remove \(noun)", role: .destructive) {
                remove(itemID)
                if selected == itemID { selected = nil }
            }
        }
        .background {
            if target == itemID {
                RoundedRectangle(cornerRadius: 6)
                    .fill(Color.accentColor.opacity(0.24))
                    .strokeBorder(Color.accentColor, lineWidth: 1)
            }
        }
    }

    private func barButton(_ title: String, symbol: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .frame(width: Layout.barButtonWidth, height: Layout.barButtonHeight)
                .contentShape(Rectangle())
        }
        .buttonStyle(.borderless)
        .accessibilityLabel(title)
        .help(title)
    }
}
