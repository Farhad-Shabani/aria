import SwiftUI

let captionWidth: CGFloat = 720
/// High enough to clear the status pill, so both can show at once.
let captionBottomMargin: CGFloat = 90

/// Bottom-centre caption strip, in the shape of a video-call caption.
struct CaptionView: View {
    let committed: String
    let provisional: String

    private var separator: String {
        committed.isEmpty || provisional.isEmpty ? "" : " "
    }

    var body: some View {
        let tail = Text(provisional).italic().foregroundColor(.white.opacity(0.85))
        Text("\(committed)\(separator)\(tail)")
            .font(.system(size: 13, weight: .medium))
            .foregroundStyle(.white)
            .lineLimit(2)
            .truncationMode(.head)  // the newest words matter most
            .multilineTextAlignment(.center)
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(.black.opacity(0.75), in: RoundedRectangle(cornerRadius: 10))
            .frame(maxWidth: captionWidth)
    }
}
