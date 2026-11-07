import Foundation

struct PromptDraft: Equatable {
    /// What a prompt's file held when the draft read it.
    enum Contents: Equatable {
        case text(String)
        case missing
        case unreadable
    }

    /// Why the draft can't be saved yet.
    enum Problem: Equatable {
        case unreadable
        case blankName
        case longName
        case controlInName
        case blankInstructions
        case longInstructions

        /// `nil` for a blank field, which speaks for itself.
        var message: String? {
            switch self {
            case .unreadable: "Aria can’t read this prompt’s file."
            case .longName: "The name is too long."
            case .controlInName: "Names can’t have tabs or line breaks."
            case .longInstructions: "Instructions can be up to \(PromptFile.maximumKilobytes) KB."
            case .blankName, .blankInstructions: nil
            }
        }
    }

    /// Why the daemon didn't save a prompt or read its file.
    enum Failure: Error, Equatable {
        case full, missing, unreadable, unwritable, offline, failed

        init(_ failure: DaemonFailure) {
            self =
                switch failure {
                case _ where failure.isOffline: .offline
                case .refused(.promptsFull): .full
                case .refused(.promptMissing): .missing
                case .refused(.promptUnreadable): .unreadable
                case .refused(.promptWriteFailed): .unwritable
                case .refused, .unavailable, .unimplemented, .timeout, .cancelled, .invalidResponse, .other: .failed
                }
        }

        init(_ error: any Error) {
            self = (error as? DaemonFailure).map(Self.init) ?? .failed
        }

        var message: String {
            switch self {
            case .full:
                "You have \(PromptEdits.maximumPrompts) prompts, the most Aria keeps. Remove one, then try again."
            case .missing: "This prompt was removed. Copy your instructions into a new prompt."
            case .unreadable:
                "Aria can’t use this file. Choose a Markdown file with up to \(PromptFile.maximumKilobytes) KB of instructions."
            case .unwritable: "Aria couldn’t save the prompt file."
            case .offline: DaemonFailure.offlineMessage
            case .failed: "Something went wrong. Try again."
            }
        }
    }

    /// The daemon refuses a longer name.
    static let maximumNameBytes = 128

    /// Tells this draft from any other, saved or not. A new prompt has no
    /// `id` to do that.
    let identity = UUID()
    private(set) var id: String?
    var name: String
    var kind: Aria_Daemon_V1_PromptKind
    /// `nil` for a new file in the prompts folder, made on save.
    private(set) var file: URL?
    var instructions: String
    /// The file's text as read.
    private(set) var loaded: String?
    private(set) var isReadable = true
    private var original: Fields
    private var failed: Failed?

    private struct Failed: Equatable {
        let failure: Failure
        let fields: Fields
    }

    private struct Fields: Equatable {
        let name: String
        let kind: Aria_Daemon_V1_PromptKind
        let file: URL?
        let instructions: String
    }

    init() {
        id = nil
        name = ""
        kind = .rewrite
        instructions = ""
        original = Fields(name: "", kind: .rewrite, file: nil, instructions: "")
    }

    /// A new prompt holding `instructions`. It starts with unsaved changes.
    init(instructions: String, kind: Aria_Daemon_V1_PromptKind) {
        self.init()
        self.instructions = instructions
        self.kind = kind
    }

    init(editing entry: Aria_Daemon_V1_Prompt, contents: Contents) {
        id = entry.id
        name = entry.name
        kind = entry.kind
        let file = URL(fileURLWithPath: entry.file)
        self.file = file
        (instructions, loaded, isReadable) = Self.read(contents)
        original = Fields(name: entry.name, kind: entry.kind, file: file, instructions: instructions)
    }

    var isNew: Bool { id == nil }

    var hasChanges: Bool { fields != original }

    private var fields: Fields {
        Fields(name: name, kind: kind, file: file, instructions: instructions)
    }

    /// Why the last save or file read failed, until a field changes.
    var failure: Failure? {
        guard let failed, failed.fields == fields else { return nil }
        return failed.failure
    }

    mutating func fail(_ failure: Failure) {
        failed = Failed(failure: failure, fields: fields)
    }

    /// `saved` is what this draft was when its save began. Fields changed
    /// since stay changed, and a new prompt is now edited as the one saved.
    mutating func didSave(_ saved: PromptDraft, as response: Aria_Daemon_V1_SavePromptResponse) {
        id = response.id
        let file = URL(fileURLWithPath: response.file)
        if self.file == nil { self.file = file }
        loaded = saved.instructions
        original = Fields(name: saved.name, kind: saved.kind, file: file, instructions: saved.instructions)
        failed = nil
    }

    /// Uses `file` where it is, with its text as the instructions. A blank
    /// name takes the file's.
    mutating func choose(_ file: URL, contents: Contents) {
        self.file = file
        (instructions, loaded, isReadable) = Self.read(contents)
        if trimmedName.isEmpty { name = file.deletingPathExtension().lastPathComponent }
    }

    /// The instructions, the text as read, and whether it could be.
    private static func read(_ contents: Contents) -> (String, String?, Bool) {
        switch contents {
        case .text(let text): (text, text, true)
        case .missing: ("", nil, true)
        case .unreadable: ("", nil, false)
        }
    }

    private var trimmedName: String {
        name.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Mirrors what the daemon and the prompt run refuse.
    var problem: Problem? {
        if !isReadable { return .unreadable }
        if let problem = Self.nameProblem(name) { return problem }
        if instructions.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return .blankInstructions }
        if instructions.utf8.count > PromptFile.maximumBytes { return .longInstructions }
        return nil
    }

    /// The daemon saves a name trimmed, so the check is on the trimmed name.
    static func nameProblem(_ name: String) -> Problem? {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        if name.isEmpty { return .blankName }
        if name.utf8.count > maximumNameBytes { return .longName }
        if name.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }) {
            return .controlInName
        }
        return nil
    }

    var note: String? {
        failure?.message ?? problem?.message
    }

    /// Sends the instructions only when they differ from what was read, so the daemon rewrites the
    /// file only then.
    var saveRequest: Aria_Daemon_V1_SavePromptRequest {
        var request = Aria_Daemon_V1_SavePromptRequest()
        if let id { request.id = id }
        request.name = trimmedName
        request.kind = kind
        if let file { request.file = file.standardizedFileURL.path }
        if instructions != loaded { request.instructions = instructions }
        return request
    }

    /// One change to `entry`, such as its name. The daemon keeps what the change leaves out, and
    /// the file's text, as they are. The request still carries the kind, which the daemon needs.
    static func saveRequest(
        editing entry: Aria_Daemon_V1_Prompt, _ change: (inout Aria_Daemon_V1_SavePromptRequest) -> Void
    ) -> Aria_Daemon_V1_SavePromptRequest {
        var request = Aria_Daemon_V1_SavePromptRequest()
        request.id = entry.id
        request.kind = entry.kind
        change(&request)
        return request
    }

    /// A new rewrite that reads `file` as it is. With no name the daemon
    /// names it after the file, and with no instructions it leaves the file
    /// alone.
    static func saveRequest(adding file: URL) -> Aria_Daemon_V1_SavePromptRequest {
        var request = Aria_Daemon_V1_SavePromptRequest()
        request.kind = .rewrite
        request.file = file.standardizedFileURL.path
        return request
    }
}
