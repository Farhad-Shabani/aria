import SwiftUI

struct RecognitionSettings: View {
    let settings: DaemonSettings
    @ObservedObject var voices: VoiceProfiles
    let serverStt: ServerStt
    let setup: SetupStatus

    var body: some View {
        SettingsForm(settings: settings) { editor in
            Section {
                language(editor)
                    .settingsRow(.language)
            }
            VoiceSection(voices: voices, setup: setup, editor: editor)
            Section("Vocabulary") {
                Toggle(
                    SettingsRow.useProjectVocabulary.title,
                    isOn: editor.binding(\.vocabularyEnabled, \.vocabularyEnabled)
                )
                .settingsRow(.useProjectVocabulary)
                vocabularyFolder(editor)
                    .disabled(!editor.snapshot.vocabularyEnabled)
                    .settingsRow(.vocabularyFolder)
            }
            ServerSttSection(server: serverStt, setup: setup)
        }
    }

    private func language(_ editor: SettingsEditor) -> some View {
        Picker(SettingsRow.language.title, selection: editor.binding(\.sttLanguage, \.sttLanguage)) {
            ForEach(DictationEdits.languageChoices(including: editor.snapshot.sttLanguage), id: \.self) {
                Text(DictationEdits.languageTitle($0)).tag($0)
            }
        }
    }

    /// The daemon clears the folder when sent an empty path.
    private func vocabularyFolder(_ editor: SettingsEditor) -> some View {
        let chosen = editor.snapshot.hasVocabularyDir ? editor.snapshot.vocabularyDir : nil
        return LabeledContent {
            HStack {
                Button("Choose…") {
                    if let path = Self.chooseFolder() { editor.set(\.vocabularyDir, to: path) }
                }
                Button("Clear") { editor.set(\.vocabularyDir, to: "") }
                    .disabled(chosen == nil)
            }
        } label: {
            Text(SettingsRow.vocabularyFolder.title)
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
