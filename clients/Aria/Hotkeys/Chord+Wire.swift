extension Chord {
    /// A chord this build cannot use reads as none.
    init?(stored wire: Aria_Daemon_V1_Chord, isSet: Bool) {
        guard isSet else { return nil }
        self.init(wire: wire)
    }
}

extension Aria_Daemon_V1_Hotkey {
    init(_ chord: Chord?) {
        self.init()
        if let chord { self.chord = chord.wire }
    }

    var storedChord: Chord? {
        Chord(stored: chord, isSet: hasChord)
    }
}

extension Aria_Daemon_V1_Prompt {
    var storedChord: Chord? {
        Chord(stored: chord, isSet: hasChord)
    }
}
