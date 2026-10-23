import SwiftUI

struct RecognitionSettings: View {
    let preferences: DaemonPreferences

    var body: some View {
        PreferencesForm(preferences: preferences) { editor in
            Section {
                language(editor)
            }
            Section("Voice") {
                Toggle(isOn: editor.binding(\.ownVoiceOnly, \.ownVoiceOnly)) {
                    Text("Only type my own voice")
                    Text("Needs an enrolled owner voice.")
                }
                .help("Enroll your voice in Terminal: aria speaker enroll <name> --role owner")
            }
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
        return LabeledContent("Folder") {
            HStack {
                Text(chosen ?? "No folder chosen")
                    .foregroundStyle(chosen == nil ? .secondary : .primary)
                    .truncationMode(.middle)
                    .lineLimit(1)
                Button("Choose…") {
                    if let path = Self.chooseFolder() { editor.set(\.vocabularyDirectory, to: path) }
                }
                Button("Clear") { editor.set(\.vocabularyDirectory, to: "") }
                    .disabled(chosen == nil)
            }
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
