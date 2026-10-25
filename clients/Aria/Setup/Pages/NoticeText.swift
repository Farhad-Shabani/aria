import SwiftUI

struct NoticeText: View {
    let notice: String?

    var body: some View {
        if let notice {
            Text(notice).foregroundStyle(.red)
        }
    }
}
