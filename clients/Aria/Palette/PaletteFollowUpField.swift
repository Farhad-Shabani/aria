import AppKit
import SwiftUI

/// Built on AppKit, as the search field is, so it takes the keys back after another page.
struct PaletteFollowUpField: NSViewRepresentable {
    @ObservedObject var controller: PaletteController
    let isEnabled: Bool
    let placeholder: String

    func makeNSView(context: Context) -> FocusingTextField {
        let field = FocusingTextField()
        field.cell?.wraps = false
        field.font = .preferredFont(forTextStyle: .body)
        field.setAccessibilityLabel("Follow-up")
        field.delegate = context.coordinator
        return field
    }

    func updateNSView(_ field: FocusingTextField, context: Context) {
        context.coordinator.controller = controller
        field.isEnabled = isEnabled
        field.placeholderString = placeholder
        if field.stringValue != controller.model.followUp { field.stringValue = controller.model.followUp }
        guard isEnabled, field.currentEditor() == nil, let window = field.window else { return }
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
            controller.model.followUp = field.stringValue
        }

        func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            guard selector == #selector(NSResponder.insertNewline(_:)) else { return false }
            controller.returnOnChat()
            return true
        }
    }
}
