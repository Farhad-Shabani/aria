import GRPCCore
import GRPCProtobuf

/// Subscribes to the daemon's event stream over its Unix socket.
///
/// The daemon comes and goes, so a dropped stream is normal rather than an
/// error: report the gap, wait, and subscribe again.
@MainActor
final class DaemonEvents {
    /// One stream, many readers: every overlay watches the same subscription.
    private var dictationHandlers: [(Aria_Daemon_V1_DictationEvent?) -> Void] = []
    private var captionHandlers: [(Aria_Daemon_V1_CaptionSnapshot?) -> Void] = []
    private var ghostHandlers: [(Aria_Daemon_V1_GhostSnapshot?) -> Void] = []
    private var chatHandlers: [(Aria_Daemon_V1_ChatEvent?) -> Void] = []
    private var speakerHandlers: [(Aria_Daemon_V1_SpeakerEvent?) -> Void] = []
    private var levelHandlers: [(Float) -> Void] = []
    private var hotkeyHandlers: [(ShortcutCombo) -> Void] = []
    private var settingsHandlers: [(Aria_Daemon_V1_SettingsStatus?) -> Void] = []
    private var systemTranscriptionHandlers: [(Aria_Daemon_V1_SystemTranscriptionEvent?) -> Void] = []
    private var streamOpenedHandlers: [() -> Void] = []

    /// Called with `nil` whenever the stream is not carrying events.
    func onDictation(_ handle: @escaping (Aria_Daemon_V1_DictationEvent?) -> Void) {
        dictationHandlers.append(handle)
    }

    func onCaption(_ handle: @escaping (Aria_Daemon_V1_CaptionSnapshot?) -> Void) {
        captionHandlers.append(handle)
    }

    /// Called for every ghost, and with `nil` whenever the stream is not
    /// carrying events. Lossy at the source: the next ghost replaces a lost one.
    func onGhost(_ handle: @escaping (Aria_Daemon_V1_GhostSnapshot?) -> Void) {
        ghostHandlers.append(handle)
    }

    /// Called for every microphone level while a press records. Lossy at the
    /// source: a level that did not arrive is a frame, not a gap.
    func onLevel(_ handle: @escaping (Float) -> Void) {
        levelHandlers.append(handle)
    }

    /// Called for every chat stage, and with `nil` whenever the stream is
    /// not carrying events.
    func onChat(_ handle: @escaping (Aria_Daemon_V1_ChatEvent?) -> Void) {
        chatHandlers.append(handle)
    }

    /// Called for every enrollment stage, and with `nil` whenever the
    /// stream is not carrying events.
    func onSpeaker(_ handle: @escaping (Aria_Daemon_V1_SpeakerEvent?) -> Void) {
        speakerHandlers.append(handle)
    }

    /// Called for every Aria key combo the daemon reads that this build
    /// knows. A combo is over the moment it arrives, so a lost stream leaves
    /// nothing to clear.
    func onHotkey(_ handle: @escaping (ShortcutCombo) -> Void) {
        hotkeyHandlers.append(handle)
    }

    /// Called whenever the settings status moves, and with `nil` whenever
    /// the stream is not carrying events.
    func onSettings(_ handle: @escaping (Aria_Daemon_V1_SettingsStatus?) -> Void) {
        settingsHandlers.append(handle)
    }

    /// Called for each system transcription stage that arrives, and with `nil` whenever the
    /// stream is not carrying events. Lossy at the source: a stage can go missing.
    func onSystemTranscription(_ handle: @escaping (Aria_Daemon_V1_SystemTranscriptionEvent?) -> Void) {
        systemTranscriptionHandlers.append(handle)
    }

    /// Called each time the stream connects. It doesn't replay what it missed.
    func onStreamOpened(_ handle: @escaping () -> Void) {
        streamOpenedHandlers.append(handle)
    }

    private static let retryDelay = Duration.seconds(2)

    private var task: Task<Void, Never>?

    func start() {
        guard task == nil else { return }
        task = Task { [weak self] in
            while !Task.isCancelled {
                await self?.subscribeOnce()
                self?.publish(dictation: nil)
                self?.publish(chat: nil)
                self?.publish(speaker: nil)
                self?.publish(caption: nil)
                self?.publish(ghost: nil)
                self?.publish(settings: nil)
                self?.publish(systemTranscription: nil)
                try? await Task.sleep(for: Self.retryDelay)
            }
        }
    }

    private func publish(dictation: Aria_Daemon_V1_DictationEvent?) {
        for handler in dictationHandlers { handler(dictation) }
    }

    private func publish(chat: Aria_Daemon_V1_ChatEvent?) {
        for handler in chatHandlers { handler(chat) }
    }

    private func publish(speaker: Aria_Daemon_V1_SpeakerEvent?) {
        for handler in speakerHandlers { handler(speaker) }
    }

    private func publish(caption: Aria_Daemon_V1_CaptionSnapshot?) {
        for handler in captionHandlers { handler(caption) }
    }

    private func publish(ghost: Aria_Daemon_V1_GhostSnapshot?) {
        for handler in ghostHandlers { handler(ghost) }
    }

    private func publish(settings: Aria_Daemon_V1_SettingsStatus?) {
        for handler in settingsHandlers { handler(settings) }
    }

    private func publish(systemTranscription: Aria_Daemon_V1_SystemTranscriptionEvent?) {
        for handler in systemTranscriptionHandlers { handler(systemTranscription) }
    }

    private func publish(level: Float) {
        for handler in levelHandlers { handler(level) }
    }

    private func publish(hotkey: ShortcutCombo) {
        for handler in hotkeyHandlers { handler(hotkey) }
    }

    /// A stream has no deadline; it ends when the daemon does. How it ended
    /// is logged by the transport, and `start` subscribes again.
    private func subscribeOnce() async {
        do {
            try await DaemonTransport.call(timeout: nil) { client, options in
                let events = Aria_Daemon_V1_EventsService.Client(wrapping: client)
                try await events.subscribe(request: ClientRequest(message: .init()), options: options) { response in
                    await MainActor.run {
                        for handler in self.streamOpenedHandlers { handler() }
                    }
                    for try await event in response.messages {
                        switch event.payload {
                        case .dictation(let dictation):
                            await MainActor.run { self.publish(dictation: dictation) }
                        case .caption(let caption):
                            await MainActor.run { self.publish(caption: caption) }
                        case .ghost(let ghost):
                            await MainActor.run { self.publish(ghost: ghost) }
                        case .level(let level):
                            await MainActor.run { self.publish(level: level.level) }
                        case .chat(let chat):
                            await MainActor.run { self.publish(chat: chat) }
                        case .speaker(let speaker):
                            await MainActor.run { self.publish(speaker: speaker) }
                        case .hotkey(let hotkey):
                            guard let combo = ShortcutCombo(wire: hotkey.combo) else { continue }
                            await MainActor.run { self.publish(hotkey: combo) }
                        case .systemTranscription(let systemTranscription):
                            await MainActor.run { self.publish(systemTranscription: systemTranscription) }
                        case .settings(let settings):
                            await MainActor.run { self.publish(settings: settings) }
                        default:
                            continue
                        }
                    }
                }
            }
        } catch {}
    }
}
