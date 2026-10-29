import SwiftUI

struct RecognitionSettings: View {
    let preferences: DaemonPreferences
    @ObservedObject var voices: VoiceProfiles
    let serverTranscription: ServerTranscription
    let setup: SetupStatus

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            TranscriptionSection(server: serverTranscription, setup: setup)
            Section {
                language(editor)
            }
            VoiceSection(voices: voices, setup: setup, editor: editor)
            Section("Vocabulary") {
                Toggle("Use project vocabulary", isOn: editor.binding(\.projectVocabulary, \.projectVocabulary))
                vocabularyFolder(editor)
                    .disabled(!editor.snapshot.projectVocabulary)
            }
        }
    }

    private func language(_ editor: PreferencesEditor) -> some View {
        Picker("Language", selection: editor.binding(\.recognitionLanguage, \.recognitionLanguage)) {
            ForEach(DictationEdits.languageChoices(including: editor.snapshot.recognitionLanguage), id: \.self) {
                Text(DictationEdits.languageTitle($0)).tag($0)
            }
        }
    }

    /// The daemon clears the folder when sent an empty path.
    private func vocabularyFolder(_ editor: PreferencesEditor) -> some View {
        let chosen = editor.snapshot.hasVocabularyDirectory ? editor.snapshot.vocabularyDirectory : nil
        return LabeledContent {
            HStack {
                Button("Choose…") {
                    if let path = Self.chooseFolder() { editor.set(\.vocabularyDirectory, to: path) }
                }
                Button("Clear") { editor.set(\.vocabularyDirectory, to: "") }
                    .disabled(chosen == nil)
            }
        } label: {
            Text("Folder")
            Text(chosen.map { ($0 as NSString).abbreviatingWithTildeInPath } ?? "No folder chosen")
                .lineLimit(1)
                .truncationMode(.middle)
        }
    }

    private static func chooseFolder() -> String? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Use Folder"
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        return url.standardizedFileURL.path
    }
}
