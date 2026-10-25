import GRPCCore
import XCTest

@testable import Aria

@MainActor
final class PreferencesLoadFailureTests: XCTestCase {
    func testLoadFailureWaitsOutADaemonThatIsNotListening() async {
        let transport = FakePreferencesTransport()
        transport.snapshotError = RPCError(code: .unavailable, message: "")
        let preferences = DaemonPreferences(transport: transport)

        await preferences.sync()

        XCTAssertNil(preferences.loadFailure)
    }

    func testLoadFailureReportsARefusal() async {
        struct Refused: LocalizedError {
            var errorDescription: String? { "refused" }
        }
        let transport = FakePreferencesTransport()
        transport.snapshotError = Refused()
        let preferences = DaemonPreferences(transport: transport)

        await preferences.sync()

        XCTAssertEqual(
            preferences.loadFailure, "Aria couldn't load its settings. Run aria doctor in Terminal to see why.")
    }
}
