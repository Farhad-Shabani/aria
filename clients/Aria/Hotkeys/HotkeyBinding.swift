/// What starts the ask or the prompt list: the Aria key held with a
/// modifier, or a chord of the user's own. Nil is off.
enum HotkeyBinding: Equatable {
    case combo(Aria_Daemon_V1_HotkeyCombo)
    case chord(Chord)
}

extension Aria_Daemon_V1_Hotkey {
    init(_ binding: HotkeyBinding?) {
        self.init()
        switch binding {
        case .combo(let combo): self.combo = combo
        case .chord(let chord): self.chord = chord.wire
        case nil: break
        }
    }

    /// Nil when off, or when the binding is one this build cannot use.
    var storedBinding: HotkeyBinding? {
        switch binding {
        case .combo(let combo): return combo.isKnown ? .combo(combo) : nil
        case .chord(let chord): return Chord(wire: chord).map(HotkeyBinding.chord)
        case nil: return nil
        }
    }

    var storedChord: Chord? {
        if case .chord(let chord) = storedBinding { return chord }
        return nil
    }
}

extension Aria_Daemon_V1_HotkeyCombo {
    var isKnown: Bool {
        switch self {
        case .shift, .command: return true
        case .unspecified, .UNRECOGNIZED: return false
        }
    }

    /// The modifier held with the Aria key, as a menu writes it. Empty for
    /// a combo this build does not know.
    var symbol: String {
        switch self {
        case .shift: return "⇧"
        case .command: return "⌘"
        case .unspecified, .UNRECOGNIZED: return ""
        }
    }
}
