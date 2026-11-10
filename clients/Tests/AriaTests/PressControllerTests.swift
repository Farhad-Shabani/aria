import XCTest

@testable import Aria

@MainActor
final class PressControllerTests: XCTestCase {
    private static let key = ChatKey(assistantID: .claudeCode, name: "c1")
    private static let minted = ChatKey(assistantID: .claudeCode, name: "minted")
    private static let chat = ChatTarget(workdir: "/code/aria", assistantID: .claudeCode)

    private static func chats() -> Chats {
        Chats(
            effects: .init(
                start: { _, _, _ in StartedChat(ticket: 7, chat: minted) },
                ask: { _, _, _, _, _ in 7 },
                cancel: { _ in },
                read: { _, _ in ChatPage(asks: [], older: nil) },
                pending: { _ in [] }))
    }

    private static func effects(
        hearing heard: String?, during: @escaping @MainActor () -> Void = {},
        folders: @escaping @MainActor (String?) -> Void
    ) -> PressController.Effects {
        .init(
            transcribe: { folder, _ in
                folders(folder)
                during()
                return heard.map { DictationClient.Transcript(text: $0, ownVoice: true) }
            },
            resolve: { chat },
            runPrompt: { _, _, _ in nil })
    }

    private func waitUntil(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async {
        for _ in 0..<200 where !condition() { await Task.yield() }
        XCTAssertTrue(condition(), file: file, line: line)
    }

    func testAnAskPressWithNowhereToAskRecordsNothing() {
        var folders: [String?] = []
        let controller = PressController(
            events: DaemonEvents(),
            operation: .ask(Self.chats(), target: { nil }, show: { XCTFail("nothing shows") }),
            effects: Self.effects(hearing: "When is lunch?") { folders.append($0) })

        controller.press()

        XCTAssertFalse(controller.isBusy, "no recording starts")
        XCTAssertEqual(folders, [])
    }

    func testAFollowUpAsksInItsChatAndFreesTheNextPress() async {
        let chats = Self.chats()
        chats.open(Self.key, folder: "/code/aria")
        var tookFollowUpWhileRecording: Bool?
        var folders: [String?] = []
        var shown = 0
        let controller = PressController(
            events: DaemonEvents(),
            operation: .ask(
                chats, target: { .followUp(Self.key) }, show: { shown += 1 }),
            effects: Self.effects(
                hearing: "And dinner?",
                during: {
                    tookFollowUpWhileRecording = chats.takesFollowUp || chats.send("Typed")
                }
            ) { folders.append($0) })

        controller.press()

        await waitUntil { shown == 1 && !controller.isBusy }
        XCTAssertEqual(tookFollowUpWhileRecording, false, "nothing is typed in while it is recorded")
        XCTAssertNil(chats.recording)
        XCTAssertEqual(chats.shown?.key, Self.key)
        XCTAssertEqual(chats.shown?.asks.map(\.question), ["And dinner?"])
        XCTAssertEqual(folders, ["/code/aria"])
        controller.press()
        await waitUntil { folders.count == 2 }
    }

    func testAFollowUpGoesToTheChatsNewNameWhenRenamedWhileRecorded() async {
        var askedIn: [ChatKey] = []
        let chats = Chats(
            effects: .init(
                start: { _, _, _ in StartedChat(ticket: 7, chat: Self.minted) },
                ask: { _, chat, _, _, _ in
                    askedIn.append(chat)
                    return 7
                },
                cancel: { _ in },
                read: { _, _ in ChatPage(asks: [], older: nil) },
                pending: { _ in [] }))
        chats.open(Self.key, folder: "/code/aria")
        let renamed = ChatKey(assistantID: .claudeCode, name: "Lunch plans")
        var shown = 0
        let controller = PressController(
            events: DaemonEvents(),
            operation: .ask(chats, target: { .followUp(Self.key) }, show: { shown += 1 }),
            effects: Self.effects(hearing: "And dinner?", during: { chats.renamed(Self.key, to: renamed) }) { _ in })

        controller.press()

        await waitUntil { shown == 1 && askedIn.count == 1 }
        XCTAssertEqual(askedIn, [renamed])
        XCTAssertEqual(chats.shown?.key, renamed)
    }

    func testANewAskStartsAChatInTheFolderInFrontAndFreesTheNextPress() async {
        let chats = Self.chats()
        var folders: [String?] = []
        var shown = 0
        let controller = PressController(
            events: DaemonEvents(),
            operation: .ask(chats, target: { .new }, show: { shown += 1 }),
            effects: Self.effects(hearing: "When is lunch?") { folders.append($0) })

        controller.press()

        await waitUntil { shown == 1 && !controller.isBusy }
        await waitUntil { chats.shown?.key == Self.minted }
        XCTAssertEqual(chats.shown?.folder, "/code/aria")
        XCTAssertEqual(chats.shown?.asks.map(\.question), ["When is lunch?"])
        XCTAssertEqual(folders, ["/code/aria"])
        controller.press()
        await waitUntil { folders.count == 2 }
    }

    func testSilenceOpensNothingAndFreesTheNextPress() async {
        let chats = Self.chats()
        var folders: [String?] = []
        let controller = PressController(
            events: DaemonEvents(),
            operation: .ask(
                chats, target: { .followUp(Self.key) },
                show: { XCTFail("nothing shows") }),
            effects: Self.effects(hearing: nil) { folders.append($0) })

        controller.press()

        await waitUntil { folders.count == 1 && !controller.isBusy }
        XCTAssertNil(chats.recording)
        XCTAssertNil(chats.shown)
    }
}
