@testable import Aria

extension Aria_Setup_V1_SetupEvent {
    static func started(_ step: Aria_Setup_V1_Step) -> Self {
        var event = Self()
        event.started = step
        return event
    }

    static func download(received: UInt64, of total: UInt64) -> Self {
        var event = Self()
        event.download.receivedBytes = received
        event.download.totalBytes = total
        return event
    }
}
