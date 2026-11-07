import SwiftUI

struct PromptSettings: View {
    let id: String
    let settings: DaemonSettings
    @ObservedObject var hotkeys: PromptHotkeys
    @State private var failure: PromptDraft.Failure?

    var body: some View {
        SettingsForm(settings: settings) { editor in
            if let entry = PromptEdits.entry(id, in: editor.snapshot.prompts) {
                ItemPage(title: entry.name) {
                    SettingsIcon(pane: .prompts, side: SettingsMetrics.headerIconSide)
                } accessory: {
                    Toggle(entry.name, isOn: binding(\.enabled, of: entry, editor))
                        .labelsHidden()
                        .toggleStyle(.switch)
                } content: {
                    promptSection(entry, editor)
                    shortcutSection(entry, editor)
                }
            }
        }
        .alert(
            "Couldn’t Save Prompt",
            isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } }),
            presenting: failure
        ) { _ in
            Button("OK") {}
        } message: { failure in
            Text(failure.message)
        }
    }

    /// The daemon writes the prompt list. Syncing after the save lets the page show the change.
    private func save(
        _ change: (inout Aria_Daemon_V1_SavePromptRequest) -> Void, to entry: Aria_Daemon_V1_Prompt
    ) async -> PromptDraft.Failure? {
        let settings = settings
        let changes = PromptChanges(refresh: { await settings.sync() })
        guard case .failure(let failure) = await changes.save(PromptDraft.saveRequest(editing: entry, change)) else {
            return nil
        }
        return failure
    }

    /// For a choice made once, such as a kind or a file. The name field saves at each typing pause,
    /// so it shows a failure under the field instead.
    private func saveOrAlert(
        _ change: (inout Aria_Daemon_V1_SavePromptRequest) -> Void, to entry: Aria_Daemon_V1_Prompt
    ) async -> Bool {
        failure = await save(change, to: entry)
        return failure == nil
    }

    private func binding<Value>(
        _ field: WritableKeyPath<Aria_Daemon_V1_Prompt, Value> & Sendable,
        of entry: Aria_Daemon_V1_Prompt,
        _ editor: SettingsEditor
    ) -> Binding<Value> {
        let id = id
        return Binding(
            get: { entry[keyPath: field] },
            set: { value in editor.editPrompt(id) { $0[keyPath: field] = value } }
        )
    }

    private func promptSection(_ entry: Aria_Daemon_V1_Prompt, _ editor: SettingsEditor) -> some View {
        Section {
            PromptNameField(name: entry.name) { name in
                await save({ $0.name = name }, to: entry)
            }
            PromptKindPicker(kind: entry.kind) { kind in
                await saveOrAlert({ $0.kind = kind }, to: entry)
            }
            LabeledContent {
                HStack {
                    Button("Choose…") {
                        guard let file = PromptFile.choose() else { return }
                        let path = file.standardizedFileURL.path
                        Task { _ = await saveOrAlert({ $0.file = path }, to: entry) }
                    }
                    Button("Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: entry.file)])
                    }
                    .disabled(!PromptFile.exists(entry))
                }
            } label: {
                Text("File")
                if PromptFile.exists(entry) {
                    Text((entry.file as NSString).abbreviatingWithTildeInPath)
                        .lineLimit(1)
                        .truncationMode(.middle)
                } else {
                    Text("File not found").foregroundStyle(.failure)
                }
            }
        }
    }

    private func shortcutSection(_ entry: Aria_Daemon_V1_Prompt, _ editor: SettingsEditor) -> some View {
        let id = id
        return Section {
            // The prompt's own switch turns its chord on and off with it.
            ChordRecorder(
                title: "Shortcut",
                chord: entry.storedChord,
                isInUse: hotkeys.refused.contains(id),
                check: { chord in
                    var prompts = editor.snapshot.prompts
                    PromptEdits.update(id, in: &prompts) { $0.chord = chord.wire }
                    try await editor.check { $0.prompts = prompts }
                }
            ) { chord in
                editor.editPrompt(id) { $0.chord = chord.wire }
            } clear: {
                editor.editPrompt(id) { $0.clearChord() }
            }
        }
    }
}

/// Shows a choice at once. A refused choice goes back to the stored kind, which the prompt still
/// has.
private struct PromptKindPicker: View {
    let kind: Aria_Daemon_V1_PromptKind
    let save: (Aria_Daemon_V1_PromptKind) async -> Bool
    @State private var chosen: Aria_Daemon_V1_PromptKind

    init(kind: Aria_Daemon_V1_PromptKind, save: @escaping (Aria_Daemon_V1_PromptKind) async -> Bool) {
        self.kind = kind
        self.save = save
        _chosen = State(initialValue: kind)
    }

    var body: some View {
        Picker("Your words", selection: Binding(get: { chosen }, set: choose)) {
            ForEach(PromptEdits.kinds, id: \.self) { kind in
                Text(kind.title).tag(kind)
            }
        }
        .onChange(of: kind) { _, latest in chosen = latest }
    }

    private func choose(_ choice: Aria_Daemon_V1_PromptKind) {
        guard choice != chosen else { return }
        chosen = choice
        let stored = kind
        Task {
            if await !save(choice) { chosen = stored }
        }
    }
}

/// Saved a moment after typing stops, on Return, and on leaving the page.
/// Saving each keystroke would send a blank name, which the daemon refuses.
private struct PromptNameField: View {
    private static let typingPause = Duration.milliseconds(500)

    let name: String
    let rename: (String) async -> PromptDraft.Failure?
    @State private var draft: String
    /// Why the last rename wasn't saved, until one is.
    @State private var failure: PromptDraft.Failure?
    @FocusState private var isFocused: Bool

    init(name: String, rename: @escaping (String) async -> PromptDraft.Failure?) {
        self.name = name
        self.rename = rename
        _draft = State(initialValue: name)
    }

    var body: some View {
        TextField(text: $draft) {
            Text("Name")
            if let message = PromptDraft.nameProblem(draft)?.message ?? failure?.message {
                Text(message).foregroundStyle(.failure)
            }
        }
        .focused($isFocused)
        .onSubmit { Task { await commit() } }
        .task(id: draft) {
            try? await Task.sleep(for: Self.typingPause)
            guard !Task.isCancelled else { return }
            await commit()
        }
        // A save echoing back mid-typing would undo the keys after it.
        .onChange(of: name) { _, latest in
            if !isFocused { draft = latest }
        }
        .onDisappear { Task { await commit() } }
    }

    /// A name the daemon would refuse is left for the person to fix, and
    /// dropped only when they leave it.
    private func commit() async {
        let trimmed = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard PromptDraft.nameProblem(trimmed) == nil else {
            if !isFocused { draft = name }
            return
        }
        guard trimmed != name else {
            failure = nil
            return
        }
        failure = await rename(trimmed)
    }
}
