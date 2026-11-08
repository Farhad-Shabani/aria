import SwiftUI

struct SystemTranscriptionClock: View {
    let startedAt: Date?
    let isRunning: Bool
    /// The last offset the session reported, for the ended label.
    let endedAfterSeconds: UInt64
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Group {
            if isRunning {
                TimelineView(.periodic(from: .now, by: 1)) { context in
                    let label = liveLabel(at: context.date)
                    Text(label)
                        .contentTransition(.numericText())
                        .animation(reduceMotion ? nil : .smooth, value: label)
                }
            } else {
                Text("Ended · \(Duration.seconds(endedAfterSeconds).lengthLabel)")
            }
        }
        .monospacedDigit()
        .foregroundStyle(.secondary)
    }

    private func liveLabel(at now: Date) -> String {
        guard let startedAt else { return "Live" }
        let seconds = UInt64(max(0, now.timeIntervalSince(startedAt)))
        let duration = Duration.seconds(seconds)
        return seconds < 3600
            ? duration.formatted(.time(pattern: .minuteSecond))
            : duration.formatted(.time(pattern: .hourMinuteSecond))
    }
}

extension SystemTranscriptionClock {
    init(_ systemTranscription: SystemTranscriptionStatus) {
        self.init(
            startedAt: systemTranscription.startedAt, isRunning: systemTranscription.isRunning,
            endedAfterSeconds: systemTranscription.transcript.offsetSeconds)
    }
}
