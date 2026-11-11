import SwiftUI

struct PaletteResultPage: View {
    @ObservedObject var controller: PaletteController
    @ObservedObject var run: PaletteRun
    @ObservedObject var history: HistoryState

    var body: some View {
        PaletteBars {
            header
        } content: {
            content
        } bottom: {
            VStack(spacing: 0) {
                PaletteTextFollowUp(controller: controller)
                PaletteFooter {
                    if let error = history.error {
                        NoticeText(notice: error.message)
                    } else if let notice = history.notice {
                        Text(notice)
                    }
                } actions: {
                    actions
                }
            }
        }
        .announcingNotes(of: history)
        .onChange(of: run.result?.state) { _, state in
            switch state {
            case .replied?: NSAccessibility.announce("Result ready")
            case .failed(let failure)?: NSAccessibility.announce(failure.message)
            case .running?, nil: break
            }
        }
    }

    private var header: some View {
        PaletteHeader(back: controller.escape) {
            Text(run.result?.promptName ?? "Prompt")
        } detail: {
            if let input = run.result?.input {
                Label(input.name, systemImage: input.kind.symbol)
            }
        }
    }

    @ViewBuilder
    private var content: some View {
        switch run.result?.state {
        case .running(let step)?:
            PalettePageProgress(step)
        case .replied(let text)?:
            ScrollView {
                Text(text)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, PageLayout.textInset)
                    .padding(.vertical, PageLayout.verticalInset)
            }
        case .failed(let failure)?:
            NoticeText(notice: failure.message)
                .multilineTextAlignment(.center)
                .padding(.horizontal, PageLayout.textInset)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case nil:
            EmptyView()
        }
    }

    @ViewBuilder
    private var actions: some View {
        PaletteFooterActions(
            actions: controller.actions, perform: controller.perform, toggleMenu: controller.toggleMenu)
        switch run.result?.state {
        case .running?:
            PaletteFooterButton(title: "Cancel", key: .escape) { controller.escape() }
        case .replied?:
            // Return works here wherever focus is. With nothing typed, it types the result.
            if let title = controller.returnTitle {
                BarDivider()
                PaletteFooterButton(title: title, key: .return) { controller.returnOnFollowUp() }
                    .disabled(!controller.canReturnOnFollowUp)
            }
        case .failed?, nil:
            EmptyView()
        }
    }
}
