import GRPCCore
import XCTest

@testable import Aria

@MainActor
final class SettingsLoadFailureTests: XCTestCase {
    func testLoadFailureWaitsOutADaemonThatIsNotListening() async {
        let transport = FakeSettingsTransport()
        transport.snapshotError = RPCError(code: .unavailable, message: "")
        let settings = DaemonSettings(transport: transport)

        await settings.sync()

        XCTAssertNil(settings.loadFailure)
    }

    func testLoadFailureReportsARefusal() async {
        struct Refused: LocalizedError {
            var errorDescription: String? { "refused" }
        }
        let transport = FakeSettingsTransport()
        transport.snapshotError = Refused()
        let settings = DaemonSettings(transport: transport)

        await settings.sync()

        XCTAssertEqual(
            settings.loadFailure, "Aria couldn’t load its settings. Run `aria doctor` in Terminal to see why.")
    }
}
