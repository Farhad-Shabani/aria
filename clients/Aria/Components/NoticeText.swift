import SwiftUI

extension ShapeStyle where Self == Color {
    /// Text that says something went wrong.
    static var failure: Color { .red }
    /// A mark on something the person can put right.
    static var attention: Color { .orange }
}

struct NoticeText: View {
    let notice: String?

    var body: some View {
        if let notice {
            Text(markdown: notice).foregroundStyle(.failure)
        }
    }
}

extension Text {
    /// Shows `**bold**` button names and `code` commands in a message
    /// built at run time, which a plain string would print literally.
    init(markdown: String) {
        self.init((try? AttributedString(markdown: markdown)) ?? AttributedString(markdown))
    }
}
