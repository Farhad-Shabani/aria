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
        draft.file =
            FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString).appendingPathExtension("md").path
        var loaded = Aria_Daemon_V1_SettingsSnapshot()
        loaded.settings.prompts.entries = [draft]
        let transport = FakeSettingsTransport()
        transport.snapshots = [loaded]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        let hotkeys = PromptHotkeys(events: DaemonEvents())
        hotkeys.track(settings)
        let refused = expectation(description: "refused")
        var names: [String?] = []
        var failures: [DaemonFailure] = []
        let refusals = hotkeys.refusals.sink { refusal in
            names.append(refusal.prompt)
            failures.append(refusal.failure)
            refused.fulfill()
        }

        // A press does nothing until the hotkeys have taken in the settings.
        let deadline = Date().addingTimeInterval(1)
        while Date() < deadline {
            hotkeys.press("draft")
            if hotkeys.isBusy("draft") { break }
            await Task.yield()
        }
        await fulfillment(of: [refused], timeout: 1)
        while hotkeys.isBusy("draft"), Date() < deadline {
            await Task.yield()
        }

        XCTAssertEqual(names, ["Draft"])
        XCTAssertEqual(failures, [.refused(.promptUnreadable)])
        XCTAssertFalse(hotkeys.isBusy("draft"))
        refusals.cancel()
    }
}
