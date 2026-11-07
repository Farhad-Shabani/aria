import AppKit

extension NSAccessibility {
    /// Speaks `text` through VoiceOver, for a change that leaves focus where it is.
    @MainActor static func announce(_ text: String) {
        post(element: NSApp as Any, notification: .announcementRequested, userInfo: [.announcement: text])
    }
}
