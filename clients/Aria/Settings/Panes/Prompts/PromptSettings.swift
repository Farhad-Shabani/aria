import SwiftUI

struct PromptSettings: View {
    let id: String
    let preferences: DaemonPreferences
    @ObservedObject var hotkeys: PromptHotkeys

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
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
    }

    private func binding<Value>(
        _ field: WritableKeyPath<Aria_Daemon_V1_Prompt, Value> & Sendable,
        of entry: Aria_Daemon_V1_Prompt,
        _ editor: PreferencesEditor
    ) -> Binding<Value> {
        let id = id
        return Binding(
            get: { entry[keyPath: field] },
            set: { value in editor.editPrompt(id) { $0[keyPath: field] = value } }
        )
    }

    private func promptSection(_ entry: Aria_Daemon_V1_Prompt, _ editor: PreferencesEditor) -> some View {
        Section {
            PromptNameField(name: entry.name) { name in
                editor.editPrompt(id) { $0.name = name }
            }
            Picker("Your words", selection: binding(\.kind, of: entry, editor)) {
                ForEach(PromptEdits.kinds, id: \.self) { kind in
                    Text(kind.title).tag(kind)
                }
            }
            LabeledContent {
                HStack {
                    Button("Choose…") {
                        guard let file = PromptFile.choose() else { return }
                        editor.editPrompt(id) { $0.file = file.standardizedFileURL.path }
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
                    Text("File not found").foregroundStyle(.red)
                }
            }
        }
    }

    private func shortcutSection(_ entry: Aria_Daemon_V1_Prompt, _ editor: PreferencesEditor) -> some View {
        let id = id
        return Section {
            // The prompt's own switch turns its chord on and off with it.
            ChordRecorder(
                title: "Shortcut",
                chord: entry.storedChord,
                inUse: hotkeys.refused.contains(id)
            ) { chord in
                editor.editPrompt(id) { $0.chord = chord.wire }
            } clear: {
                editor.editPrompt(id) { $0.clearChord() }
            }
        }
    }
}

/// Saved a moment after typing stops, on Return, and on leaving the page.
/// Saving each keystroke would send a blank name, which the daemon refuses.
private struct PromptNameField: View {
    private static let typingPause = Duration.milliseconds(500)

    let name: String
    let rename: (String) -> Void
    @State private var draft: String
    @FocusState private var focused: Bool

    init(name: String, rename: @escaping (String) -> Void) {
        self.name = name
        self.rename = rename
        _draft = State(initialValue: name)
    }

    var body: some View {
        TextField("Name", text: $draft)
            .focused($focused)
            .onSubmit(commit)
            .task(id: draft) {
                try? await Task.sleep(for: Self.typingPause)
                guard !Task.isCancelled else { return }
                commit()
            }
            // A save echoing back mid-typing would undo the keys after it.
            .onChange(of: name) { _, latest in
                if !focused { draft = latest }
            }
            .onDisappear(perform: commit)
    }

    /// A blank name is left for the person to finish, and dropped only
    /// when they leave it.
    private func commit() {
        let trimmed = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            if !focused { draft = name }
            return
        }
        if trimmed != name { rename(trimmed) }
    }
}
