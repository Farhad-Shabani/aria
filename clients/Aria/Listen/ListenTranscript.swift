import Foundation

/// The lines of the Mac Audio session Aria has followed, built from the event stream. The daemon
/// writes the whole transcript to a file.
struct ListenTranscript: Equatable {
    /// An hours-long session stays small in memory. Older lines stay in the
    /// transcript file.
    static let lineLimit = 2000
    /// A gap this long opens a new line, as `GAP_BREAK` does for the transcript file and `aria
    /// listen --follow`.
    static let paragraphGap: TimeInterval = 3

    struct Line: Equatable, Identifiable {
        let id: Int
        let offsetSeconds: UInt64
        var speakerLabel: String
        var text: String
    }

    private(set) var lines: [Line] = []
    /// The furthest offset any event reported, in seconds into the recording.
    private(set) var offsetSeconds: UInt64 = 0
    private var nextID = 0
    private var lastSegmentAt: Date?

    /// `now` is when the event arrived: a gap between arrivals ends a line.
    mutating func apply(_ event: Aria_Daemon_V1_ListenEvent, at now: Date) {
        offsetSeconds = max(offsetSeconds, event.offsetSeconds)
        switch event.stage {
        case .started:
            self = ListenTranscript()
        case .segmentTranscribed:
            append(event, at: now)
        case .speakerMerged:
            relabel(event.mergedFromLabel, as: event.speakerLabel)
        case .stopped, .failed, .unspecified, .UNRECOGNIZED:
            break
        }
    }

    var text: String {
        lines.map { line in
            let stamp = Self.stamp(line.offsetSeconds)
            return line.speakerLabel.isEmpty ? "\(stamp) \(line.text)" : "\(stamp) \(line.speakerLabel): \(line.text)"
        }
        .joined(separator: "\n\n")
    }

    /// `[HH:MM:SS]`, the transcript file's stamp.
    static func stamp(_ seconds: UInt64) -> String {
        String(format: "[%02llu:%02llu:%02llu]", seconds / 3600, seconds % 3600 / 60, seconds % 60)
    }

    private mutating func append(_ event: Aria_Daemon_V1_ListenEvent, at now: Date) {
        let text = event.text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        defer { lastSegmentAt = now }
        if let last = lines.indices.last, lines[last].speakerLabel == event.speakerLabel,
            let lastSegmentAt, now.timeIntervalSince(lastSegmentAt) < Self.paragraphGap
        {
            lines[last].text += " " + text
            return
        }
        lines.append(Line(id: nextID, offsetSeconds: event.offsetSeconds, speakerLabel: event.speakerLabel, text: text))
        nextID += 1
        if lines.count > Self.lineLimit {
            lines.removeFirst(lines.count - Self.lineLimit)
        }
    }

    private mutating func relabel(_ old: String, as new: String) {
        guard !old.isEmpty else { return }
        for index in lines.indices where lines[index].speakerLabel == old {
            lines[index].speakerLabel = new
        }
    }
}

extension Aria_Daemon_V1_ListenEvent {
    fileprivate var offsetSeconds: UInt64 { UInt64(max(0, offset.seconds)) }
}
