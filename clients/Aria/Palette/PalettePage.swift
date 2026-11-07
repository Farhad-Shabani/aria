/// Which page the palette shows. Every page but the commands page goes back to it.
enum PalettePage: Equatable {
    case commands
    case liveTranscript
    /// The history entry selected.
    case entry
    /// A prompt's result on a history entry.
    case result
    /// A prompt being written or edited: `PaletteModel.draft`.
    case prompt
}
