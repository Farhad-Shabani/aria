import XCTest

@testable import Aria

@MainActor
final class SettingsEditorTests: XCTestCase {
    func testBindingPushesOnlyTheEditedField() async throws {
        let transport = FakeSettingsTransport()
        var loaded = Aria_Daemon_V1_SettingsSnapshot()
        loaded.revision = 1
        loaded.settings.sttLanguage = "en"
        var saved = loaded
        saved.revision = 2
        saved.settings.muteSound = true
        transport.snapshots = [loaded]
        var accepted = Aria_Daemon_V1_UpdateSettingsResponse()
        accepted.snapshot = saved
        transport.updates = [accepted]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        let editor = SettingsEditor(snapshot: try XCTUnwrap(settings.snapshot), settings: settings)
        let pushed = expectation(description: "pushed")
        transport.duringUpdate = { pushed.fulfill() }

        let mute = editor.binding(\.muteSound, \.muteSound)
        XCTAssertFalse(mute.wrappedValue)
        mute.wrappedValue = true
        await fulfillment(of: [pushed], timeout: 1)

        var expected = Aria_Daemon_V1_SettingsPatch()
        expected.muteSound = true
        XCTAssertEqual(transport.sentUpdates.map(\.patch), [expected])
    }

    func testEditLatestKeepsTwoQuickEdits() async throws {
        let transport = FakeSettingsTransport()
        var loaded = Aria_Daemon_V1_SettingsSnapshot()
        loaded.revision = 1
        transport.snapshots = [loaded]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        let drawn = SettingsEditor(snapshot: try XCTUnwrap(settings.snapshot), settings: settings)

        for app in ["com.a", "com.b"] {
            drawn.editLatest(\.applicationOverrides, \.applicationOverrides) {
                ApplicationOverrideEdits.add(app, to: &$0)
            }
        }
        let deadline = Date().addingTimeInterval(1)
        while settings.snapshot?.applicationOverrides.entries.count != 2, Date() < deadline {
            await Task.yield()
        }

        XCTAssertEqual(settings.snapshot?.applicationOverrides.entries.map(\.applicationID), ["com.a", "com.b"])
    }
}
