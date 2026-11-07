import AppKit
import SwiftUI

/// An entry's title, edited in place. Return or clicking away saves, as in
/// Finder, and Esc cancels. The placeholder is what shows if left blank.
///
/// AppKit, since SwiftUI drops a focus request for a field inside the
/// palette's scrolling list.
struct PaletteTitleField: NSViewRepresentable {
    @ObservedObject var controller: PaletteController
    let placeholder: String
    var style: NSFont.TextStyle = .body

    func makeNSView(context: Context) -> FocusingTextField {
        let field = FocusingTextField()
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.lineBreakMode = .byTruncatingTail
        field.cell?.isScrollable = true
        field.usesSingleLineMode = true
        field.setContentHuggingPriority(.defaultLow, for: .horizontal)
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        field.setAccessibilityLabel("Title")
        field.stringValue = controller.model.renamingTitle
        field.delegate = context.coordinator
        return field
    }

    func updateNSView(_ field: FocusingTextField, context: Context) {
        context.coordinator.controller = controller
        field.placeholderString = placeholder
        field.font = .preferredFont(forTextStyle: style)
    }

    func makeCoordinator() -> Coordinator { Coordinator(controller: controller) }

    @MainActor
    final class Coordinator: NSObject, NSTextFieldDelegate {
        var controller: PaletteController

        init(controller: PaletteController) { self.controller = controller }

        func controlTextDidChange(_ notification: Notification) {
            guard let field = notification.object as? NSTextField else { return }
            controller.model.renamingTitle = field.stringValue
        }

        func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            switch selector {
            case #selector(NSResponder.insertNewline(_:)):
                controller.saveTitle()
            case #selector(NSResponder.cancelOperation(_:)):
                controller.escape()
            default:
                return false
            }
            return true
        }

        /// Clicking away saves. After Return or Esc the rename has already ended, so nothing is
        /// saved.
        func controlTextDidEndEditing(_ notification: Notification) {
            controller.saveTitle()
        }
    }
}

struct PaletteTitleActions: View {
    @ObservedObject var controller: PaletteController

    var body: some View {
        PaletteFooterButton(title: "Cancel", key: .escape) { controller.escape() }
        BarDivider()
        PaletteFooterButton(title: "Save", key: .return) { controller.saveTitle() }
    }
}
