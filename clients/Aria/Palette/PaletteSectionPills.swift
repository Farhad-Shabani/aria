import SwiftUI

/// Picks the history section the list keeps to.
struct PaletteSectionPills: View {
    let selected: HistorySection?
    let choose: (HistorySection?) -> Void

    var body: some View {
        HStack(spacing: 6) {
            pill(nil)
            ForEach(HistorySection.allCases, id: \.self) { pill($0) }
            Spacer(minLength: 0)
        }
        // The first pill's fill starts where a row's highlight does.
        .padding(.horizontal, PaletteBarMetrics.listInset)
        .frame(height: PaletteBarMetrics.pillsHeight, alignment: .top)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("History Sections")
    }

    private func pill(_ section: HistorySection?) -> some View {
        let isSelected = section == selected
        return Button {
            choose(section)
        } label: {
            Group {
                if let section {
                    Image(systemName: section.symbol)
                } else {
                    Text("All")
                }
            }
            .frame(minWidth: 18)
            .foregroundStyle(isSelected ? AnyShapeStyle(.background) : AnyShapeStyle(.secondary))
            .paletteCapsule(isSelected ? AnyShapeStyle(.primary) : AnyShapeStyle(.quinary))
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .help(section?.historyTitle ?? "All History")
        .accessibilityLabel(section?.historyTitle ?? "All History")
        .accessibilityAddTraits(isSelected ? .isSelected : [])
    }
}

extension View {
    func paletteCapsule(_ fill: AnyShapeStyle = AnyShapeStyle(.quinary)) -> some View {
        font(.callout)
            .padding(.horizontal, 10)
            .padding(.vertical, 4)
            .background(fill, in: Capsule())
    }
}
