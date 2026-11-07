import AppKit

struct PaletteKey: Equatable {
    static let copy = PaletteKey(character: "c", modifiers: .command, label: "⌘C")
    static let rename = PaletteKey(character: "e", modifiers: .command, label: "⌘E")
    static let editPrompt = PaletteKey(character: "e", modifiers: .command, label: "⌘E")
    /// Return types a row into the app in front. ⌘↩ opens it instead.
    static let open = PaletteKey(character: "\r", modifiers: .command, label: "⌘↩")
    /// Return alone types a new line in the instructions.
    static let save = PaletteKey(character: "\r", modifiers: .command, label: "⌘↩")
    static let saveAsPrompt = PaletteKey(character: "s", modifiers: .command, label: "⌘S")
    static let delete = PaletteKey(character: "\u{7F}", modifiers: .command, label: "⌘⌫")
    static let transcribeAgain = PaletteKey(character: "r", modifiers: .command, label: "⌘R")
    static let runPrompt = PaletteKey(character: "\t", modifiers: [], label: "⇥")
    static let actions = PaletteKey(character: "k", modifiers: .command, label: "⌘K")
    static let stop = PaletteKey(character: ".", modifiers: .command, label: "⌘.")
    static let retry = PaletteKey(character: "r", modifiers: .command, label: "⌘R")
    static let settings = PaletteKey(character: ",", modifiers: .command, label: "⌘,")
    /// ⌃N and ⌃P move through a list, as in other pickers on macOS.
    static let next = PaletteKey(character: "n", modifiers: .control, label: "⌃N")
    static let previous = PaletteKey(character: "p", modifiers: .control, label: "⌃P")
    /// ⌃Tab and ⌃⇧Tab step through the section pills, as through tabs.
    static let nextSection = PaletteKey(character: "\t", modifiers: .control, label: "⌃⇥")
    static let previousSection = PaletteKey(character: "\u{19}", modifiers: [.control, .shift], label: "⌃⇧⇥")
    static let escape = PaletteKey(character: "\u{1B}", modifiers: [], label: "⎋")
    /// Labels only: the search and title fields take Return themselves.
    static let `return` = PaletteKey(character: "\r", modifiers: [], label: "↩")

    let character: Character
    let modifiers: NSEvent.ModifierFlags
    let label: String

    func matches(_ event: NSEvent) -> Bool {
        // Caps Lock is held state, not part of the chord.
        event.modifierFlags.intersection([.command, .option, .control, .shift]) == modifiers
            && event.charactersIgnoringModifiers?.lowercased().first == character
    }
}
