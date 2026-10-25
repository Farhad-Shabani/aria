import SwiftUI

struct HotkeyRecorder: View {
    let title: String
    let chord: Chord?
    @ObservedObject var binding: ChordBinding
    let set: (Chord?) -> Void

    var body: some View {
        ChordRecorder(title: title, chord: chord, isInUse: binding.wasRefused, record: set) {
            set(nil)
        }
    }
}
