import SwiftUI

/// The total to download, if any, over notes on engines that fetch their own models.
struct DownloadFooter: View {
    let total: String?
    let notes: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: SetupMetrics.footerSpacing) {
            if let total {
                Text(total).fontWeight(.medium)
            }
            ForEach(notes, id: \.self) { Text($0) }
        }
    }
}
