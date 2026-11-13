/// Which page the palette shows. Every page but the commands page goes back to it, or to the
/// chat it was opened from.
enum PalettePage: Equatable {
    case commands
    /// The chat `Chats.shown`.
    case chat
    case liveTranscript
    /// Pairs your iPhone.
    case phonePairing
    /// The history entry selected.
    case entry
    /// A prompt's result on a history entry.
    case result
    /// A prompt being written or edited: `PaletteModel.draft`.
    case prompt
}
