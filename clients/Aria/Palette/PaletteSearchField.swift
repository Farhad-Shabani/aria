import AppKit
import SwiftUI

/// ↑ and ↓ move the selection. With nothing typed the caret has nowhere to go, so → and ← open and
/// fold a thread, as in Mail's list.
///
/// AppKit, since SwiftUI drops a focus request made while the field isn't in the window yet. Coming
/// back from another page also leaves the field without keys.
struct PaletteSearchField: NSViewRepresentable {
    @ObservedObject var controller: PaletteController
    let placeholder: String

    func makeNSView(context: Context) -> FocusingTextField {
        let field = FocusingTextField()
        field.cell?.wraps = false
        field.font = .preferredFont(forTextStyle: .title3)
        field.stringValue = controller.model.query
        field.delegate = context.coordinator
        return field
    }

    func updateNSView(_ field: FocusingTextField, context: Context) {
        context.coordinator.controller = controller
        field.placeholderString = placeholder
        field.setAccessibilityLabel(placeholder)
        if field.stringValue != controller.model.query { field.stringValue = controller.model.query }
        // The keys come back here whenever no title is being edited, such as
        // after a rename.
        guard !controller.model.isRenaming, field.currentEditor() == nil, let window = field.window else {
            return
        }
        window.makeFirstResponder(field)
        let end = (field.stringValue as NSString).length
        field.currentEditor()?.selectedRange = NSRange(location: end, length: 0)
    }

    func makeCoordinator() -> Coordinator { Coordinator(controller: controller) }

    @MainActor
    final class Coordinator: NSObject, NSTextFieldDelegate {
        var controller: PaletteController

        init(controller: PaletteController) { self.controller = controller }

        func controlTextDidChange(_ notification: Notification) {
            guard let field = notification.object as? NSTextField else { return }
            controller.model.query = field.stringValue
        }

        func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            switch selector {
            case #selector(NSResponder.moveUp(_:)):
                controller.model.moveSelection(by: -1)
                return true
            case #selector(NSResponder.moveDown(_:)):
                controller.model.moveSelection(by: 1)
                return true
            case #selector(NSResponder.moveRight(_:)):
                return controller.model.query.isEmpty && controller.expandThread()
            case #selector(NSResponder.moveLeft(_:)):
                return controller.model.query.isEmpty && controller.model.foldThread()
            case #selector(NSResponder.insertNewline(_:)):
                controller.chooseSelected()
                return true
            default:
                return false
            }
        }
    }
}
