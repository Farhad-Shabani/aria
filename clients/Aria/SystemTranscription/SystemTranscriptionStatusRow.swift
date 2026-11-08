import SwiftUI

struct SystemTranscriptionStatusRow: View {
    private static let iconSide: CGFloat = 26
    private static let stopSide: CGFloat = 24

    @ObservedObject var status: SystemTranscriptionStatus

    var body: some View {
        if status.isRunning {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 8) {
                    Image(systemName: "waveform")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.white)
                        .frame(width: Self.iconSide, height: Self.iconSide)
                        .background(.tint, in: Circle())
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 0) {
                        Text("Transcribing your Mac’s audio")
                        Text(SystemTranscriptionStatus.savingNote)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 8)
                    stopControl.frame(width: Self.stopSide, height: Self.stopSide)
                }
                if status.didFailToStop {
                    NoticeText(notice: SystemTranscriptionStatus.stopFailure)
                        .font(.caption)
                }
            }
            .padding(.horizontal, 8)
            .announcingStopFailure(of: status)
        }
    }

    @ViewBuilder
    private var stopControl: some View {
        if status.isStopping {
            ProgressView().controlSize(.small)
        } else {
            Button {
                Task { await status.stop() }
            } label: {
                Image(systemName: SystemTranscriptionStatus.stopSymbol).font(.caption)
            }
            .buttonStyle(MenuIconButtonStyle())
            .help("Stop Transcribing")
            .accessibilityLabel("Stop Transcribing")
        }
    }
}

extension View {
    func announcingStopFailure(of status: SystemTranscriptionStatus) -> some View {
        onChange(of: status.didFailToStop) { _, didFail in
            if didFail { NSAccessibility.announce(SystemTranscriptionStatus.stopFailure) }
        }
    }
}

private struct MenuIconButtonStyle: ButtonStyle {
    @State private var isHovered = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .contentShape(Circle())
            .background {
                Circle().fill(.primary.opacity(configuration.isPressed ? 0.12 : isHovered ? 0.06 : 0))
            }
            .onHover { isHovered = $0 }
    }
}
