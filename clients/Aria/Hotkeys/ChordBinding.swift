import Combine

@MainActor
final class ChordBinding: ObservableObject {
    private let shortcut = GlobalShortcut()
    private let action: () -> Void
    /// What is registered, so an unchanged preference costs nothing.
    private(set) var bound: Chord?
    /// Carbon turned the chord down.
    @Published private(set) var refused = false

    init(action: @escaping () -> Void) {
        self.action = action
    }

    func bind(_ chord: Chord?) {
        guard let chord else {
            shortcut.unregister()
            bound = nil
            refused = false
            return
        }
        guard chord != bound else { return }
        let taken = shortcut.register(keyCode: UInt32(chord.keyCode), modifiers: chord.carbonModifiers, action: action)
        bound = taken ? chord : nil
        refused = !taken
    }
}
