import SwiftUI

struct PaletteView: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var systemTranscription: SystemTranscriptionStatus
    let history: HistoryState

    var body: some View {
        Group {
            switch controller.model.page {
            case .commands:
                PaletteCommandsPage(
                    controller: controller, systemTranscription: systemTranscription, history: history,
                    chats: controller.chats, microphones: controller.microphones)
            case .liveTranscript:
                PaletteLivePage(controller: controller, systemTranscription: systemTranscription, history: history)
            case .phonePairing:
                if let pairing = controller.phonePairing {
                    PalettePhonePage(controller: controller, pairing: pairing)
                }
            case .chat:
                PaletteChatPage(
                    controller: controller, chats: controller.chats, history: history)
            case .entry: PaletteEntryPage(controller: controller, history: history)
            case .result: PaletteResultPage(controller: controller, run: controller.promptRun, history: history)
            case .prompt:
                if let draft = Binding($controller.model.draft) {
                    PalettePromptPage(controller: controller, draft: draft)
                }
            }
        }
        .background(alignment: .bottom) {
            ZStack {
                if systemTranscription.isRunning {
                    PaletteSandBand(level: systemTranscription.level)
                        .transition(.opacity)
                }
            }
            .animation(.easeInOut(duration: 0.4), value: systemTranscription.isRunning)
        }
        .environment(\.isOverSand, systemTranscription.isRunning)
        .frame(width: PalettePanel.size.width, height: PalettePanel.size.height)
    }
}
