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
