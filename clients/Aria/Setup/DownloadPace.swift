import Foundation

/// How long a download has left, from its pace over the last few seconds.
/// A rolling window follows a change of speed without jumping at every report.
struct DownloadPace {
    private static let window: TimeInterval = 10
    /// A shorter span swings too much to put a time on.
    private static let shortestSpan: TimeInterval = 2

    private var readings: [(at: Date, received: UInt64)] = []

    mutating func record(_ received: UInt64, at now: Date) {
        readings.append((now, received))
        readings.removeAll { now.timeIntervalSince($0.at) > Self.window }
    }

    /// In Software Update's words. Nil while the pace is still unknown.
    func timeLeft(of total: UInt64) -> String? {
        guard let first = readings.first, let last = readings.last else { return nil }
        let span = last.at.timeIntervalSince(first.at)
        guard span >= Self.shortestSpan, last.received > first.received else { return nil }
        let rate = Double(last.received - first.received) / span
        let seconds = Double(total > last.received ? total - last.received : 0) / rate
        return Self.phrase(for: seconds)
    }

    private static func phrase(for seconds: TimeInterval) -> String? {
        if seconds < 60 { return "Less than a minute remaining" }
        let formatter = DateComponentsFormatter()
        formatter.unitsStyle = .full
        formatter.allowedUnits = seconds < 3600 ? [.minute] : [.hour, .minute]
        formatter.maximumUnitCount = 1
        formatter.includesApproximationPhrase = true
        formatter.includesTimeRemainingPhrase = true
        return formatter.string(from: (seconds / 60).rounded() * 60)
    }
}
