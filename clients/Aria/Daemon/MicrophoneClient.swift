import Foundation
import GRPCCore
import GRPCProtobuf

/// A connected input device, as the daemon lists it.
struct ListedMicrophone: Equatable, Sendable {
    /// What `dictation.microphone` takes to choose it.
    var id: String
    var label: String
}

/// The connected devices, and the one a dictation records on now.
struct MicrophoneList: Equatable, Sendable {
    var microphones: [ListedMicrophone]
    /// Nil when nothing can record.
    var currentID: String?

    init(microphones: [ListedMicrophone], currentID: String?) {
        self.microphones = microphones
        self.currentID = currentID
    }

    init(_ wire: Aria_Daemon_V1_ListMicrophonesResponse) {
        self.init(
            microphones: wire.microphones.map { ListedMicrophone(id: $0.id, label: $0.label) },
            currentID: wire.hasCurrentID ? wire.currentID : nil
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
