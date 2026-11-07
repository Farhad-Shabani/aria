/// How far a model download has come, as setup reports it.
struct ModelTransfer: Equatable {
    let received: UInt64
    let total: UInt64

    /// Clamped, since `total` is the size setup expects, not the size served.
    var fraction: Double { Double(min(received, total)) / Double(max(total, 1)) }

    init(received: UInt64, total: UInt64) {
        self.received = received
        self.total = total
    }

    /// Nil until a report gives the size.
    init?(_ progress: Aria_Setup_V1_DownloadProgress) {
        guard progress.totalBytes > 0 else { return nil }
        self.init(received: progress.receivedBytes, total: progress.totalBytes)
    }
}
