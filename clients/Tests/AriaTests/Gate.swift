/// Holds a call until the test opens the gate, so its reply can arrive after
/// a later call's reply.
@MainActor
final class Gate {
    private var waiter: CheckedContinuation<Void, Never>?

    func wait() async {
        await withCheckedContinuation { waiter = $0 }
    }

    /// Returns once a call is waiting.
    func reached() async {
        while waiter == nil { await Task.yield() }
    }

    func open() {
        waiter?.resume()
        waiter = nil
    }
}
