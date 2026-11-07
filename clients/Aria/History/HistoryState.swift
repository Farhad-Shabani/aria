import Foundation

struct HistoryWork: Equatable {
    var isClearing = false
    var deletingID: UInt64?
    var regeneratingID: UInt64?

    func canRename(_ id: UInt64) -> Bool {
        !isClearing && deletingID != id
    }

    func canDelete(_ entry: HistoryItem) -> Bool {
        !isClearing && deletingID == nil && !entry.isRegenerating && regeneratingID != entry.id
    }

    func canRegenerate(_ entry: HistoryItem) -> Bool {
        entry.canRegenerate && !entry.isRegenerating && !isClearing && regeneratingID == nil
            && deletingID != entry.id
    }
}

@MainActor
final class HistoryState: ObservableObject {
    static let copiedNotice = "Copied"
    /// The daemon refuses a longer search (`ListHistoryRequest.query`).
    static let maximumQueryBytes = 256

    struct Effects {
        /// A query, the section to keep to (`nil` for all), and a cursor.
        var list: (String, HistorySection?, UInt64?) async throws -> HistoryPage
        var get: (UInt64) async throws -> HistoryItem
        /// An entry's prompt results, newest first.
        var thread: (UInt64) async throws -> [PromptResult]
        /// `nil` removes the title.
        var rename: (UInt64, String?) async throws -> HistoryItem
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
    private(set) var query = ""
    /// Whether history shows, so a dictation ending refreshes only a list
    /// someone is looking at.
    private(set) var isOpen = false
    private var next: UInt64?
    private var listGeneration = 0
    private var detailGeneration = 0
    private var listTask: Task<Void, Never>?
    private var detailTask: Task<Void, Never>?
    private var regenerateTask: Task<Void, Never>?
    private var deleteTask: Task<Void, Never>?
    private var copyTask: Task<Void, Never>?
    private var renameTask: Task<Void, Never>?
    private var clearTask: Task<Void, Never>?
    private var clearNotice: String?
    private let effects: Effects

    init(effects: Effects) { self.effects = effects }

    var canClear: Bool {
        !isClearing && deletingID == nil && regeneratingID == nil
            && detail?.isRegenerating != true && !items.contains(where: \.isRegenerating)
    }

    /// `nil` lists every section.
    private(set) var section: HistorySection?

    func open() {
        isOpen = true
        query = ""
        section = nil
        refresh()
    }

    func dictationEnded() {
        if isOpen { refresh() }
    }

    /// A filter set during a clear waits for the refresh that ends it.
    func filter(query: String, section: HistorySection?) {
        guard query != self.query || section != self.section else { return }
        let isTyping = section == self.section
        self.query = query
        self.section = section
        guard isOpen, !isClearing else { return }
        refresh(debounce: isTyping)
    }

    func refresh(debounce: Bool = false, preservingError: Bool = false) {
        guard !isClearing else { return }
        listTask?.cancel()
        listGeneration += 1
        let generation = listGeneration
        let requestedQuery = query
        let requestedSection = section
        guard requestedQuery.utf8.count <= Self.maximumQueryBytes else {
            isLoading = false
            error = .queryTooLong
            return
        }
        isLoading = true
        if !preservingError && error != .clearUncertain { error = nil }
        listTask = Task {
            do {
                if debounce { try await Task.sleep(for: .milliseconds(250)) }
                var page = try await effects.list(requestedQuery, requestedSection, nil)
                while let selectedID, !page.items.contains(where: { $0.id == selectedID }), let cursor = page.next {
                    guard generation == listGeneration, !Task.isCancelled else { return }
                    let following = try await effects.list(requestedQuery, requestedSection, cursor)
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
                let page = try await effects.list(query, section, cursor)
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
        if selectedID != id { forgetCopied() }
        // A refresh re-selects what is already open. Holding the loaded detail
        // there keeps the pane from blanking under someone mid-read.
        if selectedID != id { detail = nil }
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
        guard !isClearing, let detail else { return }
        put(detail.copyText)
    }

    /// `nil` when the results couldn't be read. `error` says why.
    func thread(of entry: UInt64) async -> [PromptResult]? {
        do {
            return try await effects.thread(entry)
        } catch {
            self.error = error as? HistoryFailure ?? .failed
            return nil
        }
    }

    /// Copies text the caller already holds, such as a prompt result.
    func copy(text: String) {
        guard !isClearing else { return }
        put(text)
    }

    /// `nil` when the entry couldn't be read. `error` says why.
    func typingText(of id: UInt64) async -> String? {
        do {
            return try await effects.get(id).copyText
        } catch {
            self.error = error as? HistoryFailure ?? .failed
            return nil
        }
    }

    func refuseTyping() {
        error = .tooLongToType
    }

    /// Copies an entry from its row, which holds only the start of its text.
    func copy(_ id: UInt64) {
        guard !isClearing else { return }
        copyTask?.cancel()
        copyTask = Task {
            do {
                let entry = try await effects.get(id)
                guard !Task.isCancelled, !isClearing else { return }
                put(entry.copyText)
            } catch {
                guard !Task.isCancelled else { return }
                self.error = error as? HistoryFailure ?? .failed
            }
        }
    }

    func rename(_ id: UInt64, to name: String) {
        guard canRename(id) else { return }
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.utf8.count <= HistoryItem.titleLimit else {
            error = .nameTooLong
            return
        }
        error = nil
        let title = trimmed.isEmpty ? nil : trimmed
        guard (items.first { $0.id == id } ?? detail)?.title != title else { return }
        renameTask = Task {
            do {
                let renamed = try await effects.rename(id, title)
                guard !Task.isCancelled else { return }
                if let index = items.firstIndex(where: { $0.id == id }) { items[index] = renamed }
                if detail?.id == id { detail = renamed }
            } catch {
                guard !Task.isCancelled else { return }
                self.error = error as? HistoryFailure ?? .failed
            }
        }
    }

    var work: HistoryWork {
        HistoryWork(isClearing: isClearing, deletingID: deletingID, regeneratingID: regeneratingID)
    }

    func canRename(_ id: UInt64) -> Bool {
        work.canRename(id)
    }

    /// A copy's notice or failure belongs to the row it was shown on.
    func forgetCopied() {
        if notice == Self.copiedNotice { notice = nil }
        if error == .copyFailed { error = nil }
    }

    private func put(_ text: String) {
        guard !text.isEmpty else { return }
        forgetCopied()
        if effects.copy(text) { notice = Self.copiedNotice } else { error = .copyFailed }
    }

    func canDelete(_ entry: HistoryItem) -> Bool {
        work.canDelete(entry)
    }

    func canRegenerate(_ entry: HistoryItem) -> Bool {
        work.canRegenerate(entry)
    }

    func regenerate() {
        guard let entry = detail, canRegenerate(entry) else { return }
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
        copyTask?.cancel()
        renameTask?.cancel()
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
        copyTask?.cancel()
        renameTask?.cancel()
        clearTask?.cancel()
        if isClearing { error = .clearUncertain }
        isClearing = false
        isLoading = false
        isLoadingDetail = false
    }
}
