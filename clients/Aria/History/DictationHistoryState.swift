import Foundation

@MainActor
final class DictationHistoryState: ObservableObject {
    struct Effects {
        var list: (String, UInt64?) async throws -> HistoryPage
        var get: (UInt64) async throws -> HistoryItem
        var delete: (UInt64) async throws -> Void
        var clear: () async throws -> HistoryClearResult
        var regenerate: (UInt64) async throws -> HistoryItem
        var copy: (String) -> Bool
        var waitForRegenerate: () async throws -> Void = { try await Task.sleep(for: .seconds(1)) }
    }

    @Published private(set) var items: [HistoryItem] = [] {
        didSet { days = HistoryDay.grouping(items) }
    }
    @Published private(set) var days: [HistoryDay] = []
    @Published private(set) var selectedID: UInt64?
    @Published private(set) var detail: HistoryItem?
    @Published private(set) var isLoading = false
    @Published private(set) var isLoadingDetail = false
    @Published private(set) var error: HistoryFailure?
    @Published private(set) var notice: String?
    @Published private(set) var regeneratingID: UInt64?
    @Published private(set) var deletingID: UInt64?
    @Published private(set) var isClearing = false
    @Published private(set) var query = ""
    /// Whether the window shows, so a dictation ending refreshes only a list
    /// someone is looking at.
    private(set) var isOpen = false
    private var next: UInt64?
    private var listGeneration = 0
    private var detailGeneration = 0
    private var listTask: Task<Void, Never>?
    private var detailTask: Task<Void, Never>?
    private var regenerateTask: Task<Void, Never>?
    private var deleteTask: Task<Void, Never>?
    private var clearTask: Task<Void, Never>?
    private var clearNotice: String?
    private let effects: Effects

    init(effects: Effects) { self.effects = effects }

    var canClear: Bool {
        !isClearing && deletingID == nil && regeneratingID == nil
            && detail?.isRegenerating != true && !items.contains(where: \.isRegenerating)
    }

    func open() {
        isOpen = true
        refresh()
    }

    func dictationEnded() {
        if isOpen { refresh() }
    }

    func search(_ value: String) {
        guard !isClearing else { return }
        query = value
        refresh(debounce: true)
    }

    func refresh(debounce: Bool = false, preservingError: Bool = false) {
        guard !isClearing else { return }
        listTask?.cancel()
        listGeneration += 1
        let generation = listGeneration
        let requestedQuery = query
        guard requestedQuery.utf8.count <= 256 else {
            isLoading = false
            error = .queryTooLong
            return
        }
        isLoading = true
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
                isLoading = false
                let id = selectedID.flatMap { id in items.contains { $0.id == id } ? id : nil }
                select(id ?? items.first?.id)
            } catch {
                guard generation == listGeneration, !Task.isCancelled else { return }
                isLoading = false
                self.error = error as? HistoryFailure ?? .failed
            }
        }
    }

    func loadMore(after id: UInt64) {
        guard !isClearing, id == items.last?.id, let cursor = next, !isLoading else { return }
        let generation = listGeneration
        isLoading = true
        listTask = Task {
            do {
                let page = try await effects.list(query, cursor)
                guard generation == listGeneration, !Task.isCancelled else { return }
                let existing = Set(items.map(\.id))
                items.append(contentsOf: page.items.filter { !existing.contains($0.id) })
                next = page.next
                notice = page.warning
                isLoading = false
            } catch {
                guard generation == listGeneration, !Task.isCancelled else { return }
                isLoading = false
                self.error = error as? HistoryFailure ?? .failed
            }
        }
    }

    func select(_ id: UInt64?) {
        guard !isClearing else { return }
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
        isLoadingDetail = id != nil && detail == nil
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
                    isLoadingDetail = false
                    guard entry.isRegenerating else { return }
                    try await effects.waitForRegenerate()
                }
            } catch {
                guard generation == detailGeneration, !Task.isCancelled else { return }
                isLoadingDetail = false
                self.error = error as? HistoryFailure ?? .failed
            }
        }
    }

    func copy() {
        guard !isClearing, let detail, !detail.text.isEmpty else { return }
        notice = effects.copy(detail.text) ? "Copied" : "Aria couldn’t copy the text. Try again."
    }

    func regenerate() {
        guard !isClearing, let entry = detail, entry.canRegenerate, regeneratingID == nil,
            deletingID != entry.id
        else { return }
        regeneratingID = entry.id
        notice = nil
        regenerateTask = Task {
            defer { regeneratingID = nil }
            do {
                let recovered = try await effects.regenerate(entry.id)
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
        guard !isClearing, deletingID == nil, regeneratingID != id else { return }
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
        isClearing = true
        listGeneration += 1
        detailGeneration += 1
        listTask?.cancel()
        detailTask?.cancel()
        isLoading = false
        isLoadingDetail = false
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
                    result.isCleanupPending
                    ? "Dictation text cleared. Aria will finish removing some recordings later." : nil
                notice = clearNotice
                isClearing = false
                refresh()
            } catch {
                guard !Task.isCancelled else { return }
                isClearing = false
                self.error = error as? HistoryFailure ?? .clearUncertain
                refresh(preservingError: true)
            }
        }
    }

    func close() {
        isOpen = false
        listGeneration += 1
        detailGeneration += 1
        listTask?.cancel()
        detailTask?.cancel()
        regenerateTask?.cancel()
        deleteTask?.cancel()
        clearTask?.cancel()
        if isClearing { error = .clearUncertain }
        isClearing = false
        isLoading = false
        isLoadingDetail = false
    }
}
