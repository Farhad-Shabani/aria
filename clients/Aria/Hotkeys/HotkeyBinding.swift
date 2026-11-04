/// What starts the ask or the prompt list: the Aria key held with a
/// modifier, or a chord of the user's own. Nil is off.
enum ShortcutBinding: Equatable {
    case combo(ShortcutCombo)
    case chord(Chord)
}

/// The modifier held with the Aria key.
enum ShortcutCombo: Equatable {
    case shift
    case command

    /// Nil for `unspecified` and values this build does not know.
    init?(wire: Aria_Daemon_V1_ShortcutCombo) {
        switch wire {
        case .shift: self = .shift
        case .command: self = .command
        case .unspecified, .UNRECOGNIZED: return nil
        }
    }

    var wire: Aria_Daemon_V1_ShortcutCombo {
        switch self {
        case .shift: .shift
        case .command: .command
        }
    }

    /// The modifier as a menu writes it.
    var symbol: String {
        switch self {
        case .shift: "⇧"
        case .command: "⌘"
        }
    }
}

extension Aria_Daemon_V1_Shortcut {
    init(_ binding: ShortcutBinding?) {
        self.init()
        switch binding {
        case .combo(let combo): self.combo = combo.wire
        case .chord(let chord): self.chord = chord.wire
        case nil: break
        }
    }

    /// Nil when off, or when the binding is one this build cannot use.
    var storedBinding: ShortcutBinding? {
        switch binding {
        case .combo(let combo): return ShortcutCombo(wire: combo).map(ShortcutBinding.combo)
        case .chord(let chord): return Chord(wire: chord).map(ShortcutBinding.chord)
        case nil: return nil
        }
    }

    var storedChord: Chord? {
        if case .chord(let chord) = storedBinding { return chord }
        return nil
    }
}
