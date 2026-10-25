import SwiftUI

extension ShapeStyle where Self == Color {
    /// Text that says something went wrong.
    static var failure: Color { .red }
}

struct NoticeText: View {
    let notice: String?

    var body: some View {
        if let notice {
            Text(notice).foregroundStyle(.failure)
        }
    }
}
