import AppKit

/// A borderless one-line field that takes the keys, with its text selected, as soon as it is in a
/// window.
final class FocusingTextField: NSTextField {
    override init(frame: NSRect) {
        super.init(frame: frame)
        isBordered = false
        drawsBackground = false
        focusRingType = .none
        lineBreakMode = .byTruncatingTail
        cell?.isScrollable = true
        // Else ⌃N and ⌃P scroll the typed line up out of the field.
        usesSingleLineMode = true
        setContentHuggingPriority(.defaultLow, for: .horizontal)
        setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        window?.makeFirstResponder(self)
    }
}
