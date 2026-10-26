import SwiftUI

private enum Layout {
    static let rowPadding: CGFloat = 10
    static let cornerRadius: CGFloat = 12
    static let barButtonWidth: CGFloat = 26
    static let barButtonHeight: CGFloat = 22
    static let barDividerHeight: CGFloat = 14
}

struct ItemList<Item, Row: View>: View {
    @State private var rowHeights: [String: CGFloat] = [:]
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
        VStack(alignment: .leading, spacing: 12) {
            Group {
                if items.isEmpty {
                    Text(empty)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, minHeight: Layout.barButtonHeight)
                        .padding(.vertical, Layout.rowPadding)
                } else {
                    List(selection: $selected) {
                        ForEach(items, id: id) { item in
                            line(item[keyPath: id], row(item))
                                .onGeometryChange(for: CGFloat.self) {
                                    $0.size.height
                                } action: { height in
                                    rowHeights[item[keyPath: id]] = height
                                }
                                .tag(item[keyPath: id])
                                .listRowInsets(EdgeInsets())
                        }
                    }
                    .listStyle(.plain)
                    .scrollContentBackground(.hidden)
                    .frame(maxHeight: contentHeight)
                }
            }
            .background(.quaternary.opacity(0.3))
            .clipShape(.rect(cornerRadius: Layout.cornerRadius))
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
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .scenePadding()
        .background {
            Color.clear
                .contentShape(Rectangle())
                .onTapGesture { selected = nil }
        }
        .onChange(of: items.map { $0[keyPath: id] }) { _, ids in
            rowHeights = rowHeights.filter { ids.contains($0.key) }
        }
    }

    private var contentHeight: CGFloat? {
        let heights = items.compactMap { rowHeights[$0[keyPath: id]] }
        return heights.count == items.count ? heights.reduce(0, +) : nil
    }

    private var target: String? {
        items.contains { $0[keyPath: id] == selected } ? selected : nil
    }

    private func line(_ itemID: String, _ content: Row) -> some View {
        HStack {
            content.frame(maxWidth: .infinity, alignment: .leading)
            Button("Open \(noun)", systemImage: "chevron.right") { open(itemID) }
                .labelStyle(.iconOnly)
                .font(.footnote.weight(.semibold))
                .foregroundStyle(.tertiary)
                .frame(width: Layout.barButtonWidth, height: Layout.barButtonHeight)
                .contentShape(Rectangle())
                .buttonStyle(.plain)
        }
        .padding(.vertical, Layout.rowPadding)
        .alignmentGuide(.listRowSeparatorLeading) { $0[.leading] }
        .alignmentGuide(.listRowSeparatorTrailing) { $0[.trailing] }
        .contextMenu {
            Button("Remove \(noun)", role: .destructive) {
                remove(itemID)
                if selected == itemID { selected = nil }
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
