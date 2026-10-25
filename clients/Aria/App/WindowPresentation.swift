import Foundation

/// Opens a window that may be asked for before its scene exists; the
/// request waits until the scene registers.
@MainActor
final class WindowPresentation {
    private var open: (() -> Void)?
    private var isPending = false

    func didLaunch(atLogin: Bool) {
        if !atLogin { request() }
    }

    func request() {
        guard let open else {
            isPending = true
            return
        }
        open()
    }

    func register(_ open: @escaping () -> Void) {
        self.open = open
        if isPending {
            isPending = false
            open()
        }
    }
}
