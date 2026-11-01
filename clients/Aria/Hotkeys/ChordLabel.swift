import SwiftUI

/// A chord as menus write it, opened up a little: run tight, the system font
/// draws ⌥ close enough to touch the key after it.
struct ChordLabel: View {
    private static let tracking: CGFloat = 1

    let chord: Chord

    var body: some View {
        Text(chord.title).tracking(Self.tracking)
    }
}
