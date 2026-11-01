/// Holds one call until the test lets it through, so a reply can be made
/// to land after a later one.
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
