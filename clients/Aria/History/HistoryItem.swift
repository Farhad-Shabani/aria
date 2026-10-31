import Foundation

struct HistoryItem: Identifiable, Equatable, Sendable {
    enum Audio: Equatable, Sendable {
        case available(until: Date)
        case unavailable
        case expired
    }

    enum Status: Equatable, Sendable {
        case success, transcriptionFailed, deliveryFailed, recovered, deletionPending

        var title: String {
            switch self {
            case .success: "Success"
            case .transcriptionFailed: "Couldn’t transcribe"
            case .deliveryFailed: "Couldn’t type"
            case .recovered: "Recovered"
            case .deletionPending: "Deletion pending"
            }
        }
    }

    let id: UInt64
    var text: String
    var created: Date
    var status: Status
    var source: String = "Dictation"
    var application: String?
    var duration: Double?
    var isComplete = true
    var isRetryable = false
    var audio: Audio = .unavailable
    var isRetrying = false
    var warning: String?
    var message: String?

    var preview: String { text.isEmpty ? status.title : text }

    var metadata: [(String, String)] {
        var rows = [("Source", source)]
        if let application { rows.append(("Application", application)) }
        rows.append(("Status", status.title))
        rows.append(("Created", created.formatted(date: .abbreviated, time: .shortened)))
        if let duration {
            rows.append(("Duration", "\(duration.formatted(.number.precision(.fractionLength(0...1)))) seconds"))
        }
        if !text.isEmpty {
            rows.append(
                (isComplete ? "Words" : "Words (incomplete)", "\(text.split(whereSeparator: \.isWhitespace).count)"))
        }
        if status == .transcriptionFailed {
            let description: String
            switch audio {
            case .available(let expiry):
                description = "Available until \(expiry.formatted(date: .abbreviated, time: .shortened))"
            case .unavailable:
                description = "Unavailable"
            case .expired:
                description = "Expired"
            }
            rows.append(("Audio", description))
        }
        return rows
    }
}

struct HistoryDay: Equatable {
    let start: Date
    let items: [HistoryItem]

    /// Newest day first; items keep their order within a day.
    static func grouping(_ items: [HistoryItem], calendar: Calendar = .current) -> [HistoryDay] {
        Dictionary(grouping: items) { calendar.startOfDay(for: $0.created) }
            .sorted { $0.key > $1.key }
            .map { HistoryDay(start: $0.key, items: $0.value) }
    }
}

struct HistoryPage: Sendable {
    var items: [HistoryItem]
    var next: UInt64?
    var warning: String?
}

struct HistoryClearResult: Sendable {
    var deletedEntries: UInt64
    var isCleanupPending: Bool
}
