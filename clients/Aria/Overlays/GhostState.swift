import Foundation

/// The words streaming has heard but not typed, and where to draw them. It
/// empties once they land, the caret is lost, the press ends, or five seconds
/// pass without an update.
struct GhostState {
    static let expiry: TimeInterval = 5
    private(set) var text = ""
    private(set) var placement: Aria_Daemon_V1_Placement?
    private var enabled = false
    private var updatedAt: TimeInterval?

    var isVisible: Bool { !text.isEmpty && placement != nil }

    mutating func setEnabled(_ enabled: Bool) {
        guard self.enabled != enabled else { return }
        self.enabled = enabled
        clear()
    }

    mutating func apply(_ snapshot: Aria_Daemon_V1_GhostSnapshot?, now: TimeInterval) {
        guard enabled, let snapshot, !snapshot.text.isEmpty, snapshot.hasPlacement else {
            clear()
            return
        }
        text = String(String.UnicodeScalarView(snapshot.text.unicodeScalars.suffix(2048)))
        placement = snapshot.placement
        updatedAt = now
    }

    mutating func expire(now: TimeInterval) {
        guard let updatedAt, now - updatedAt >= Self.expiry else { return }
        clear()
    }

    mutating func clear() {
        text = ""
        placement = nil
        updatedAt = nil
    }
}
