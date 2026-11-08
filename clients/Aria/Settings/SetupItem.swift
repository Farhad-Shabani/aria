import Foundation

/// One thing Aria still needs from the person, listed in Finish Setting Up
/// Aria. An optional feature's note stays beside its setting instead.
enum SetupItem: Hashable {
    case backgroundService
    case settingsFileStopsStart(line: Int?)
    case settingsFileIgnored(line: Int?)
    case server
    case speechModel(id: String)
    case ariaKey
    case ariaKeyRemapped

    /// Unset choices still run as their defaults, so a settings gap matters
    /// only for recognition on a server with no pairing.
    nonisolated static func items(in inspection: Aria_Setup_V1_Inspection) -> [SetupItem] {
        let recognition = Set(
            inspection.models.filter { $0.engineID == inspection.defaults.sttEngineID }.map(\.id))
        let isUnpairedServer = inspection.sttRunsOn == .server && !inspection.hasServer
        return inspection.missing.compactMap { missing in
            switch missing.kind {
            case .model: recognition.contains(missing.modelID) ? .speechModel(id: missing.modelID) : nil
            case .grant: .ariaKey
            case .keyRemapped: .ariaKeyRemapped
            case .settings: isUnpairedServer ? .server : nil
            case .wakeword, .service, .unspecified, .UNRECOGNIZED: nil
            }
        }
    }

    static func all(
        inspected: [SetupItem],
        settingsStatus: Aria_Daemon_V1_SettingsStatus?,
        startFailure: DaemonControlFailure?
    ) -> [SetupItem] {
        var items: [SetupItem] = []
        switch startFailure {
        case .backgroundItemOff: items.append(.backgroundService)
        case .settingsInvalid(let invalid): items.append(.settingsFileStopsStart(line: invalid.line))
        case .start, .stop, .restart, .stillRunning, .unresponsive, .sttModelMissing, nil:
            if let settingsStatus, settingsStatus.hasFault {
                let fault = settingsStatus.fault
                items.append(.settingsFileIgnored(line: fault.hasPosition ? Int(fault.position.line) : nil))
            }
        }
        return items + inspected
    }

    @MainActor
    static func all(setup: SetupStatus, health: SettingsHealth, poller: StatusPoller) -> [SetupItem] {
        all(inspected: setup.items, settingsStatus: health.status, startFailure: poller.failure)
    }

    var title: String {
        switch self {
        case .backgroundService: "Background service"
        case .settingsFileStopsStart, .settingsFileIgnored: "Settings file"
        case .server: "Server"
        case .speechModel: "Speech model"
        case .ariaKey: "Aria key"
        case .ariaKeyRemapped: "Aria key"
        }
    }

    var detail: String {
        switch self {
        case .backgroundService:
            "Turn on Aria in Login Items & Extensions so it can start."
        case .settingsFileStopsStart(let line):
            line.map { "Aria can’t start because line \($0) isn’t valid." }
                ?? "Aria can’t start because your settings file isn’t valid."
        case .settingsFileIgnored(let line):
            line.map { "Line \($0) isn’t valid, so Aria is using your last saved settings." }
                ?? "Your settings file isn’t valid, so Aria is using your last saved settings."
        case .server:
            "Aria can’t start until your Mac is paired with your server."
        case .speechModel:
            "Download it so Aria can turn your voice into text."
        case .ariaKey:
            "Lets Aria hear the Aria key and type for you."
        case .ariaKeyRemapped:
            "Modifier Keys turns the Aria key into another key, so Aria can’t hear it. Restore it in Keyboard settings."
        }
    }
}
