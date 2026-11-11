import AppKit

struct PaletteKey: Equatable {
    /// Text selected on the page takes ⌘C first.
    static let copy = PaletteKey(character: "c", modifiers: .command, label: "⌘C")
    /// Return sends the follow-up typed on the chat page.
    static let typeText = PaletteKey(character: "\r", modifiers: .command, label: "⌘↩")
    /// Selects every question and reply on the chat page. A typed follow-up takes ⌘A first.
    static let selectAll = PaletteKey(character: "a", modifiers: .command, label: "⌘A")
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
    /// The search, title, and follow-up fields handle Return themselves. The entry and result
    /// pages match it too, so Return works wherever focus is.
    static let `return` = PaletteKey(character: "\r", modifiers: [], label: "↩")

    /// Caps Lock is held state, not part of the chord.
    static let chordModifiers: NSEvent.ModifierFlags = [.command, .option, .control, .shift]

    let character: Character
    let modifiers: NSEvent.ModifierFlags
    let label: String

    func matches(_ event: NSEvent) -> Bool {
        event.modifierFlags.intersection(Self.chordModifiers) == modifiers
            && event.charactersIgnoringModifiers?.lowercased().first == character
    }
}
