import XCTest

@testable import Aria

@MainActor
final class PreferencesEditorTests: XCTestCase {
    func test_binding_pushesOnlyTheEditedField() async throws {
        let transport = FakePreferencesTransport()
        var loaded = Aria_Daemon_V1_PreferenceSnapshot()
        loaded.revision = 1
        loaded.preferences.recognitionLanguage = "en"
        var saved = loaded
        saved.revision = 2
        saved.preferences.muteOutput = true
        transport.snapshots = [loaded]
        var accepted = Aria_Daemon_V1_UpdatePreferencesResponse()
        accepted.snapshot = saved
        transport.updates = [accepted]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        let editor = PreferencesEditor(snapshot: try XCTUnwrap(preferences.snapshot), preferences: preferences)
        let pushed = expectation(description: "pushed")
        transport.duringUpdate = { pushed.fulfill() }

        let mute = editor.binding(\.muteOutput, \.muteOutput)
        XCTAssertFalse(mute.wrappedValue)
        mute.wrappedValue = true
        await fulfillment(of: [pushed], timeout: 1)

        var expected = Aria_Daemon_V1_PreferencePatch()
        expected.muteOutput = true
        XCTAssertEqual(transport.sentUpdates.map(\.patch), [expected])
    }
}
