import SwiftUI

struct PaletteView: View {
    @ObservedObject var controller: PaletteController
    let listen: ListenStatus
    let history: HistoryState

    var body: some View {
        Group {
            switch controller.model.page {
            case .commands: PaletteCommandsPage(controller: controller, listen: listen, history: history)
            case .liveTranscript: PaletteLivePage(controller: controller, listen: listen, history: history)
            case .entry: PaletteEntryPage(controller: controller, history: history)
            case .result: PaletteResultPage(controller: controller, run: controller.promptRun, history: history)
            case .prompt:
                if let draft = Binding($controller.model.draft) {
                    PalettePromptPage(controller: controller, draft: draft)
                }
            }
        }
        .frame(width: PalettePanel.size.width, height: PalettePanel.size.height)
    }
}
