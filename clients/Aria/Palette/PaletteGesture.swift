/// What a press of the palette chord asks the caller to do.
enum PalettePress: Equatable {
    /// A prompt the palette started is still running. The Aria key drives it.
    case ignore
    case open
    case close
}

/// What the palette chord means, decided apart from Carbon and the panel.
struct PaletteGesture: Equatable {
    private var started: String?

    /// A run that has ended frees the chord to open the palette again.
    mutating func press(isOpen: Bool, isBusy: (String) -> Bool) -> PalettePress {
        if let started, isBusy(started) {
            return .ignore
        }
        started = nil
        return isOpen ? .close : .open
    }

    mutating func chose(_ id: String) {
        started = id
    }
}
