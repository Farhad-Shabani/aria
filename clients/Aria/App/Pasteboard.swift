import AppKit

enum Pasteboard {
    /// Puts `text` on the general pasteboard, marked so clipboard managers
    /// don't record it. False when the pasteboard refused.
    static func copyConcealed(_ text: String) -> Bool {
        let pasteboard = NSPasteboard.general
        pasteboard.clearContents()
        guard pasteboard.setString("", forType: .init("org.nspasteboard.ConcealedType")) else {
            return false
        }
        return pasteboard.setString(text, forType: .string)
    }
}
