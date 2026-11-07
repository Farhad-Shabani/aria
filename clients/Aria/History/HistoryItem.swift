import Foundation

/// One kind of saved item, which history can be narrowed to.
enum HistorySection: Equatable, Sendable, CaseIterable {
    case dictation, listen, ask

    var historyTitle: String {
        switch self {
        case .dictation: "Dictation History"
        case .listen: "Mac Audio History"
        case .ask: "Ask History"
        }
    }

    var symbol: String {
        switch self {
        case .dictation: "mic"
        case .listen: "waveform"
        case .ask: "bubble.left"
        }
    }
}

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

    enum Kind: Equatable, Sendable {
        case dictation
        /// A Mac Audio session. `text` is its transcript, or only its start when `read` isn't
        /// `.whole`.
        case listen(transcript: String, read: TranscriptRead?)
        /// `text` is the question.
        case ask(reply: String)

        var title: String {
            switch self {
            case .dictation: "Dictation"
            case .listen: "Mac Audio"
            case .ask: "Ask"
            }
        }

        var section: HistorySection {
            switch self {
            case .dictation: .dictation
            case .listen: .listen
            case .ask: .ask
            }
        }

        var symbol: String { section.symbol }
    }

    enum TranscriptRead: Equatable, Sendable {
        case whole, truncated, unavailable
    }

    enum Source: Equatable, Sendable {
        case local, relay

        var title: String {
            switch self {
            case .local: "Dictation"
            case .relay: "iPhone or Apple Watch"
            }
        }
    }

    let id: UInt64
    /// A name someone gave it, shown in place of its text.
    var title: String?
    var kind: Kind = .dictation
    var created: Date
    var duration: Double?
    var source: Source = .local
    var application: String?
    /// The folder it was made in, such as where an ask's assistant worked.
    var project: String?
    var text: String
    var isComplete = true
    /// How many prompt results its thread holds.
    var resultCount = 0
    var status: Status
    var message: String?
    var warning: String?
    var audio: Audio = .unavailable
    var isRegenerating = false
    var canRegenerate = false

    var preview: String { text.isEmpty ? status.title : text }
    var name: String { title ?? preview }
    /// The daemon's limit on a title, in UTF-8 bytes.
    static let titleLimit = 256

    var facts: String {
        var parts: [String] = []
        if let place { parts.append(place) }
        if let elapsed { parts.append(elapsed) }
        let countsWords =
            switch kind {
            case .dictation: isComplete
            case .listen(_, let read): read == .whole
            case .ask: false
            }
        if countsWords, !text.isEmpty {
            parts.append("\(text.split(whereSeparator: \.isWhitespace).count) words")
        }
        return parts.joined(separator: " · ")
    }

    var copyText: String {
        if case .ask(let reply) = kind { reply } else { text }
    }

    var canCopy: Bool { !copyText.isEmpty && status != .deletionPending }
    var canRename: Bool { status != .deletionPending }

    var origin: String {
        switch kind {
        case .dictation, .ask: place ?? kind.title
        case .listen: elapsed ?? kind.title
        }
    }

    private var place: String? {
        switch kind {
        case .dictation: application ?? source.title
        case .ask: project.map { URL(fileURLWithPath: $0).lastPathComponent }
        case .listen: nil
        }
    }

    private var elapsed: String? {
        duration.map { Duration.seconds($0.rounded()).lengthLabel }
    }

    var audioNote: String? {
        guard status == .transcriptionFailed else { return nil }
        return switch audio {
        case .available(let expiry):
            "Its recording is kept until \(expiry.formatted(date: .abbreviated, time: .shortened))."
        case .unavailable: "Its recording wasn’t kept."
        case .expired: "Its recording has expired."
        }
    }
}

struct HistoryDay: Equatable {
    let start: Date
    let items: [HistoryItem]

    static func title(for start: Date, calendar: Calendar = .current) -> String {
        if calendar.isDateInToday(start) { return "Today" }
        if calendar.isDateInYesterday(start) { return "Yesterday" }
        return start.formatted(date: .abbreviated, time: .omitted)
    }

    static func time(of date: Date) -> String {
        date.formatted(date: .omitted, time: .shortened)
    }

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
