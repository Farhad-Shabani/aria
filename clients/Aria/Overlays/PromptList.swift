import Carbon.HIToolbox
import Combine
import SwiftUI

/// One chord that lists the prompts that are on, so one runs without its own
/// chord: press it, then the prompt's number. While a run it started is
/// going, the chord stands in for that prompt's own: stop, then cancel.
///
/// The list takes no focus, so the app in front keeps its caret. Its keys
/// are chords of their own, taken only while it shows.
@MainActor
final class PromptList {
    /// A list left unanswered lets its keys go, so 1 to 9 are never held long.
    private static let openFor = Duration.seconds(8)

    private let prompts: PromptHotkeys
    private(set) lazy var chord = ChordBinding { [weak self] in self?.press() }
    private let panel = FloatingPanel(width: listWidth, bottomMargin: listBottomMargin)
    private var listKeys: [GlobalShortcut] = []
    private var gesture = PromptListGesture()
    private var expiry: Task<Void, Never>?
    private var chordChange: AnyCancellable?

    init(prompts: PromptHotkeys) {
        self.prompts = prompts
    }

    func track(_ preferences: DaemonPreferences) {
        chordChange = preferences.$snapshot
            .map { $0?.promptListHotkey.storedChord }
            .sink { [weak self] chord in
                Task { @MainActor in self?.apply(chord) }
            }
    }

    private func apply(_ chord: Chord?) {
        self.chord.bind(chord)
        if chord == nil { close() }
    }

    private func press() {
        switch gesture.press(isOpen: !listKeys.isEmpty, isBusy: { prompts.isBusy($0) }) {
        case .prompt(let id): prompts.press(id)
        case .open: open()
        case .close: close()
        }
    }

    private func open() {
        let keys = PromptListGesture.keys(for: prompts.runnable)
        guard !keys.isEmpty else { return }
        for (keyCode, entry) in keys {
            take(keyCode) { [weak self] in self?.choose(entry.id) }
        }
        take(kVK_Escape) { [weak self] in self?.close() }
        let shown = keys.map(\.entry)
        panel.show(PromptListView(entries: shown))
        let spoken = shown.enumerated().map { "\($0.offset + 1), \($0.element.name)" }.joined(separator: "; ")
        NSAccessibility.post(
            element: NSApp as Any,
            notification: .announcementRequested,
            userInfo: [.announcement: spoken]
        )
        expiry = Task { [weak self] in
            try? await Task.sleep(for: Self.openFor)
            guard !Task.isCancelled else { return }
            self?.close()
        }
    }

    /// With no modifier, so the bare key works while the list shows.
    private func take(_ keyCode: Int, action: @escaping @MainActor () -> Void) {
        let key = GlobalShortcut()
        if key.register(keyCode: UInt32(keyCode), modifiers: 0, action: action) {
            listKeys.append(key)
        }
    }

    private func choose(_ id: String) {
        close()
        gesture.chose(id)
        prompts.press(id)
    }

    private func close() {
        expiry?.cancel()
        expiry = nil
        for key in listKeys {
            key.unregister()
        }
        listKeys = []
        panel.hide()
    }
}

private let listWidth = pillWidth
/// Above the pill, so a pill still showing stays readable.
private let listBottomMargin = pillBottomMargin + 52

private enum Layout {
    static let maxNameWidth: CGFloat = 280
}

private struct PromptListView: View {
    let entries: [Aria_Daemon_V1_Prompt]

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(entries.enumerated()), id: \.element.id) { index, entry in
                HStack(spacing: 10) {
                    Text("\(index + 1)")
                        .foregroundStyle(.white.opacity(0.5))
                    Text(entry.name)
                        .lineLimit(1)
                        .frame(maxWidth: Layout.maxNameWidth, alignment: .leading)
                    Spacer(minLength: 16)
                    if let chord = entry.storedChord {
                        ChordLabel(chord: chord)
                            .foregroundStyle(.white.opacity(0.5))
                    }
                }
            }
        }
        .font(OverlayStyle.font)
        .monospacedDigit()
        .foregroundStyle(.white)
        .padding(12)
        .fixedSize()
        .background(OverlayStyle.fill, in: RoundedRectangle(cornerRadius: 12))
    }
}
