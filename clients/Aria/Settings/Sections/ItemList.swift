import SwiftUI

private enum Layout {
    static let barButtonWidth: CGFloat = 26
    static let barButtonHeight: CGFloat = 22
    static let barDividerHeight: CGFloat = 14
    /// Pulls the bar left by the + glyph's side margin, so the glyph lines up
    /// with the rows' leading edge.
    static let barOutdent: CGFloat = 8
}

/// A pane's list of items. A click opens the item's page; the buttons under
/// the list add, and remove the item whose page Back just left, which the
/// list marks. A row's context menu removes any item.
struct ItemList<Item, Row: View>: View {
    let items: [Item]
    let id: KeyPath<Item, String>
    /// Names one item to assistive tech: "App" gives "Add App".
    let noun: String
    let empty: String
    /// The item to mark and to remove with −.
    let marked: String?
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
                    if let target { remove(target) }
                }
                .disabled(target == nil)
            }
            .fixedSize()
            .padding(.leading, -Layout.barOutdent)
        }
    }

    /// A mark whose item is gone, removed here or elsewhere, is none.
    private var target: String? {
        items.contains { $0[keyPath: id] == marked } ? marked : nil
    }

    private func line(_ itemID: String, _ content: Row) -> some View {
        ItemLink {
            open(itemID)
        } label: {
            content
        }
        .contextMenu {
            Button("Remove \(noun)", role: .destructive) { remove(itemID) }
        }
        .listRowBackground(target == itemID ? Color.accentColor.opacity(0.15) : nil)
        .accessibilityAddTraits(target == itemID ? .isSelected : [])
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
