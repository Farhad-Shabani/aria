import Combine
import XCTest

@testable import Aria

@MainActor
final class PromptHotkeysTests: XCTestCase {
    func testAPromptWhoseFileCannotBeReadIsRefusedBeforeRecording() async {
        var draft = Aria_Daemon_V1_Prompt()
        draft.id = "draft"
        draft.name = "Draft"
        draft.kind = .rewrite
        draft.enabled = true
        draft.instructionsPath =
            FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString).appendingPathExtension("md").path
        var loaded = Aria_Daemon_V1_PreferenceSnapshot()
        loaded.preferences.prompts.entries = [draft]
        let transport = FakePreferencesTransport()
        transport.snapshots = [loaded]
        let preferences = DaemonPreferences(transport: transport)
        await preferences.sync()
        let hotkeys = PromptHotkeys(events: DaemonEvents())
        hotkeys.track(preferences)
        let deadline = Date().addingTimeInterval(1)
        while hotkeys.runnable.isEmpty, Date() < deadline {
            await Task.yield()
        }
        let refused = expectation(description: "refused")
        var names: [String] = []
        let refusals = hotkeys.refusals.sink { name in
            names.append(name)
            refused.fulfill()
        }

        hotkeys.press("draft")
        await fulfillment(of: [refused], timeout: 1)
        while hotkeys.isBusy("draft"), Date() < deadline {
            await Task.yield()
        }

        XCTAssertEqual(names, ["Draft"])
        XCTAssertFalse(hotkeys.isBusy("draft"))
        refusals.cancel()
    }
}
