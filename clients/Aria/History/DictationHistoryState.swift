import Foundation

extension Notification.Name {
    static let dictationHistoryChanged = Notification.Name("dictationHistoryChanged")
}

struct HistoryItem: Identifiable, Equatable, Sendable {
    enum Status: Equatable, Sendable {
        case success, transcriptionFailed, deliveryFailed, recovered, deletionPending

        var title: String {
            switch self {
            case .success: "Success"
            case .transcriptionFailed: "Transcription failed"
            case .deliveryFailed: "Delivery failed"
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
    var complete = true
    var retryable = false
    var audioExpires: Date?
    var audioAvailable = false
    var audioExpired = false
    var retrying = false
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
                (complete ? "Words" : "Words (incomplete)", "\(text.split(whereSeparator: \.isWhitespace).count)"))
        }
        if status == .transcriptionFailed {
            let audio: String
            if audioAvailable, let audioExpires {
                audio = "Available until \(audioExpires.formatted(date: .abbreviated, time: .shortened))"
            } else {
                audio = audioExpired ? "Expired" : "Unavailable"
            }
            rows.append(("Audio", audio))
        }
        return rows
    }
}

struct HistoryPage: Sendable {
    var items: [HistoryItem]
    var next: UInt64?
    var warning: String?
}

struct HistoryClearResult: Sendable {
    var deletedEntries: UInt64
    var cleanupPending: Bool
}

enum HistoryFailure: Error, Equatable {
    case offline, storage, missing, audioUnavailable, projectUnavailable, tooLarge, busy, cancelled,
        timeout, queryTooLong, unsupported, clearUncertain, failed

    var message: String {
        switch self {
        case .offline: "Aria is unavailable. Start the daemon and try again."
        case .storage: "Dictation history storage is unavailable."
        case .missing: "This dictation is no longer available. Refresh history."
        case .audioUnavailable: "The recording is no longer available for retry."
        case .projectUnavailable: "The original project directory is unavailable. Restore it before retrying."
        case .tooLarge: "The transcript is too large for history to store."
        case .busy: "Aria is busy. Try again when the current operation finishes."
        case .cancelled: "Retry was cancelled."
        case .timeout: "The operation took too long. Try again."
        case .queryTooLong: "Use a shorter search (up to 256 bytes)."
        case .unsupported: "Update and restart Aria to clear dictation history."
        case .clearUncertain: "Aria could not confirm whether history was cleared. Refresh history, then try again."
        case .failed: "The operation failed. Please try again."
        }
    }
}

@MainActor
final class DictationHistoryState: ObservableObject {
    struct Effects {
        var list: (String, UInt64?) async throws -> HistoryPage
        var get: (UInt64) async throws -> HistoryItem
        var delete: (UInt64) async throws -> Void
        var clear: () async throws -> HistoryClearResult
        var retry: (UInt64) async throws -> HistoryItem
        var copy: (String) -> Bool
        var waitForRetry: () async throws -> Void = { try await Task.sleep(for: .seconds(1)) }
    }

    @Published private(set) var items: [HistoryItem] = []
    @Published private(set) var selectedID: UInt64?
    @Published private(set) var detail: HistoryItem?
    @Published private(set) var loading = false
    @Published private(set) var detailLoading = false
    @Published private(set) var error: HistoryFailure?
    @Published private(set) var notice: String?
    @Published private(set) var retryingID: UInt64?
    @Published private(set) var deletingID: UInt64?
    @Published private(set) var clearing = false
    @Published private(set) var query = ""
    private var next: UInt64?
    private var listGeneration = 0
    private var detailGeneration = 0
    private var listTask: Task<Void, Never>?
    private var detailTask: Task<Void, Never>?
    private var retryTask: Task<Void, Never>?
    private var deleteTask: Task<Void, Never>?
    private var clearTask: Task<Void, Never>?
    private var clearNotice: String?
    private let effects: Effects

    init(effects: Effects) { self.effects = effects }

    var canClear: Bool {
        !clearing && deletingID == nil && retryingID == nil
            && detail?.retrying != true && !items.contains(where: \.retrying)
    }

    func search(_ value: String) {
        guard !clearing else { return }
        query = value
        refresh(debounce: true)
    }

    func refresh(debounce: Bool = false, preservingError: Bool = false) {
        guard !clearing else { return }
        listTask?.cancel()
        listGeneration += 1
        let generation = listGeneration
        let requestedQuery = query
        guard requestedQuery.utf8.count <= 256 else {
            loading = false
            error = .queryTooLong
            return
        }
        loading = true
        if !preservingError && error != .clearUncertain { error = nil }
        listTask = Task {
            do {
                if debounce { try await Task.sleep(for: .milliseconds(250)) }
                var page = try await effects.list(requestedQuery, nil)
                while let selectedID, !page.items.contains(where: { $0.id == selectedID }), let cursor = page.next {
                    guard generation == listGeneration, !Task.isCancelled else { return }
                    let following = try await effects.list(requestedQuery, cursor)
                    page.items.append(contentsOf: following.items)
                    page.next = following.next
                    page.warning = following.warning ?? page.warning
                }
                guard generation == listGeneration, !Task.isCancelled else { return }
                items = page.items
                next = page.next
                notice = clearNotice ?? page.warning
                clearNotice = nil
                loading = false
                let id = selectedID.flatMap { id in items.contains { $0.id == id } ? id : nil }
                select(id ?? items.first?.id)
            } catch {
                guard generation == listGeneration, !Task.isCancelled else { return }
                loading = false
                self.error = error as? HistoryFailure ?? .failed
            }
        }
    }

    func loadMore(after id: UInt64) {
        guard !clearing, id == items.last?.id, let cursor = next, !loading else { return }
        let generation = listGeneration
        loading = true
        listTask = Task {
            do {
                let page = try await effects.list(query, cursor)
                guard generation == listGeneration, !Task.isCancelled else { return }
                let existing = Set(items.map(\.id))
                items.append(contentsOf: page.items.filter { !existing.contains($0.id) })
                next = page.next
                notice = page.warning
                loading = false
            } catch {
                guard generation == listGeneration, !Task.isCancelled else { return }
                loading = false
                self.error = error as? HistoryFailure ?? .failed
            }
        }
    }

    func select(_ id: UInt64?) {
        guard !clearing else { return }
        detailTask?.cancel()
        detailGeneration += 1
        let generation = detailGeneration
        if selectedID != id && notice == "Copied" { notice = nil }
        // A refresh re-selects what is already open. Holding the loaded detail
        // there keeps the pane from blanking under someone mid-read.
        if selectedID != id {
            detail = nil
        }
        selectedID = id
        detailLoading = id != nil && detail == nil
        guard let id else { return }
        detailTask = Task {
            do {
                while generation == detailGeneration, !Task.isCancelled {
                    let entry = try await effects.get(id)
                    guard generation == detailGeneration, !Task.isCancelled else { return }
                    detail = entry
                    if let index = items.firstIndex(where: { $0.id == id }) {
                        items[index] = entry
                    }
                    detailLoading = false
                    guard entry.retrying else { return }
                    try await effects.waitForRetry()
                }
            } catch {
                guard generation == detailGeneration, !Task.isCancelled else { return }
                detailLoading = false
                self.error = error as? HistoryFailure ?? .failed
            }
        }
    }

    func copy() {
        guard !clearing, let detail, !detail.text.isEmpty else { return }
        notice = effects.copy(detail.text) ? "Copied" : "Could not copy the text. Try again."
    }

    func retry() {
        guard !clearing, let entry = detail, entry.retryable, retryingID == nil,
            deletingID != entry.id
        else { return }
        retryingID = entry.id
        notice = nil
        retryTask = Task {
            defer { retryingID = nil }
            do {
                let recovered = try await effects.retry(entry.id)
                guard !Task.isCancelled else { return }
                if let index = items.firstIndex(where: { $0.id == entry.id }) {
                    items[index] = recovered
                }
                if selectedID == entry.id { detail = recovered }
                refresh()
            } catch {
                guard !Task.isCancelled else { return }
                self.error = error as? HistoryFailure ?? .failed
                if selectedID == entry.id { select(entry.id) }
            }
        }
    }

    func delete(_ id: UInt64) {
        guard !clearing, deletingID == nil, retryingID != id else { return }
        deletingID = id
        deleteTask = Task {
            defer { deletingID = nil }
            do {
                try await effects.delete(id)
                guard !Task.isCancelled else { return }
                let index = items.firstIndex { $0.id == id }
                items.removeAll { $0.id == id }
                if selectedID == id {
                    let neighbor = index.flatMap { items.isEmpty ? nil : items[min($0, items.count - 1)].id }
                    select(neighbor)
                }
                refresh()
            } catch {
                guard !Task.isCancelled else { return }
                self.error = error as? HistoryFailure ?? .failed
                if selectedID == id { select(id) }
            }
        }
    }

    func clear() {
        guard canClear else { return }
        clearing = true
        listGeneration += 1
        detailGeneration += 1
        listTask?.cancel()
        detailTask?.cancel()
        loading = false
        detailLoading = false
        error = nil
        notice = nil
        clearNotice = nil
        clearTask = Task {
            do {
                let result = try await effects.clear()
                guard !Task.isCancelled else { return }
                items = []
                selectedID = nil
                detail = nil
                next = nil
                query = ""
                clearNotice =
                    result.cleanupPending
                    ? "Dictation text cleared. Some recordings could not be removed; Aria will retry." : nil
                notice = clearNotice
                clearing = false
                refresh()
            } catch {
                guard !Task.isCancelled else { return }
                clearing = false
                self.error = error as? HistoryFailure ?? .clearUncertain
                refresh(preservingError: true)
            }
        }
    }

    func close() {
        listGeneration += 1
        detailGeneration += 1
        listTask?.cancel()
        detailTask?.cancel()
        retryTask?.cancel()
        deleteTask?.cancel()
        clearTask?.cancel()
        if clearing { error = .clearUncertain }
        clearing = false
        loading = false
        detailLoading = false
    }
}
