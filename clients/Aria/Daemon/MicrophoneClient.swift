import Foundation
import GRPCCore
import GRPCProtobuf

/// A connected input device, as the daemon lists it.
struct ListedMicrophone: Equatable, Sendable {
    /// What `dictation.microphone` takes to choose it.
    var id: String
    var label: String
}

/// What a press records on, as the daemon decides it.
enum CurrentMicrophone: Equatable, Sendable {
    /// A listed microphone's `id`.
    case device(String)
    case phone
}

/// The connected devices, and what a press records on now.
struct MicrophoneList: Equatable, Sendable {
    var microphones: [ListedMicrophone]
    /// Nil when nothing can record.
    var current: CurrentMicrophone?
}

extension MicrophoneList {
    init(_ wire: Aria_Daemon_V1_ListMicrophonesResponse) {
        let current: CurrentMicrophone? =
            switch wire.current {
            case .deviceID(let id): .device(id)
            case .phone: .phone
            case nil: nil
            }
        self.init(
            microphones: wire.microphones.map { ListedMicrophone(id: $0.id, label: $0.label) },
            current: current
        )
    }
}

/// The daemon's MicrophoneService.
enum MicrophoneClient {
    static func list() async throws -> MicrophoneList {
        try await DaemonTransport.call { client, options in
            try await Aria_Daemon_V1_MicrophoneService.Client(wrapping: client)
                .list(request: ClientRequest(message: .init()), options: options) {
                    MicrophoneList(try $0.message)
                }
        }
    }
}
