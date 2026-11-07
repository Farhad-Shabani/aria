import XCTest

@testable import Aria

@MainActor
final class PromptDraftTests: XCTestCase {
    private static func entry(file: String = "/p/polish.md") -> Aria_Daemon_V1_Prompt {
        var entry = Aria_Daemon_V1_Prompt.stub(file: file)
        entry.name = "Polish"
        return entry
    }

    func testANewPromptAsksForAFileOfItsOwnWithItsTrimmedNameKindAndText() {
        var draft = PromptDraft()
        draft.name = "  Summary "
        draft.kind = .compose
        draft.instructions = "Three bullets."

        let request = draft.saveRequest

        XCTAssertFalse(request.hasID)
        XCTAssertFalse(request.hasFile)
        XCTAssertEqual(request.name, "Summary")
        XCTAssertEqual(request.kind, .compose)
        XCTAssertEqual(request.instructions, "Three bullets.")
    }

    func testAddingAFileAsksForANewRewriteTheDaemonNamesThatLeavesItsText() {
        let request = PromptDraft.saveRequest(adding: URL(fileURLWithPath: "/notes/../prompts/Email reply.md"))

        XCTAssertFalse(request.hasID)
        XCTAssertFalse(request.hasName)
        XCTAssertEqual(request.kind, .rewrite)
        XCTAssertEqual(request.file, "/prompts/Email reply.md")
        XCTAssertFalse(request.hasInstructions)
    }

    func testASettingsEditSendsOnlyItsChangeAndTheKindTheDaemonNeeds() {
        var entry = Self.entry()
        entry.kind = .compose

        let rename = PromptDraft.saveRequest(editing: entry) { $0.name = "Polish more" }
        let file = PromptDraft.saveRequest(editing: entry) { $0.file = "/p/other.md" }

        XCTAssertEqual(rename.id, entry.id)
        XCTAssertEqual(rename.name, "Polish more")
        XCTAssertEqual(rename.kind, .compose)
        XCTAssertFalse(rename.hasFile)
        XCTAssertFalse(rename.hasInstructions)
        XCTAssertFalse(file.hasName)
        XCTAssertEqual(file.file, "/p/other.md")
        XCTAssertFalse(file.hasInstructions)
    }

    func testAnEditNamesItsPromptAndSendsTextOnlyWhenItChanged() {
        let entry = Self.entry(file: "/p/polish.md")
        var draft = PromptDraft(editing: entry, contents: .text("Fix it."))
        draft.name = "Polish more"
        XCTAssertEqual(draft.saveRequest.id, entry.id)
        XCTAssertEqual(draft.saveRequest.file, "/p/polish.md")
        XCTAssertFalse(draft.saveRequest.hasInstructions)

        draft.instructions = "Fix it well."
        XCTAssertEqual(draft.saveRequest.instructions, "Fix it well.")
    }

    func testAChosenFileIsUsedInPlaceAndNamesABlankPrompt() {
        var draft = PromptDraft()
        draft.choose(URL(fileURLWithPath: "/notes/Email reply.md"), contents: .text("Reply kindly."))

        let request = draft.saveRequest

        XCTAssertEqual(request.name, "Email reply")
        XCTAssertEqual(request.file, "/notes/Email reply.md")
        XCTAssertFalse(request.hasInstructions)
    }

    func testEditingAMissingFileWritesItAgain() {
        var draft = PromptDraft(editing: Self.entry(), contents: .missing)
        draft.instructions = "Fix it."

        XCTAssertEqual(draft.saveRequest.instructions, "Fix it.")
    }

    func testSaveWaitsForANameAndInstructionsTheDaemonAndRunWouldTake() {
        var draft = PromptDraft()
        XCTAssertEqual(draft.problem, .blankName)
        draft.name = String(repeating: "é", count: 65)
        XCTAssertEqual(draft.problem, .longName)
        draft.name = "Tab\there"
        XCTAssertEqual(draft.problem, .controlInName)
        draft.name = "Summary"
        XCTAssertEqual(draft.problem, .blankInstructions)
        draft.instructions = String(repeating: "a", count: PromptFile.maximumBytes + 1)
        XCTAssertEqual(draft.problem, .longInstructions)
        draft.instructions = "Three bullets."
        XCTAssertNil(draft.problem)
        draft.name = "👩\u{200D}💻 Review \u{200F}"
        XCTAssertNil(draft.problem, "the daemon refuses Cc only, not a joiner or direction mark")
    }

    func testANameIsCheckedTrimmedAndOnlyARefusalOtherThanBlankSaysWhy() {
        XCTAssertEqual(PromptDraft.nameProblem(" \n"), .blankName)
        XCTAssertEqual(PromptDraft.nameProblem(String(repeating: "é", count: 65)), .longName)
        XCTAssertEqual(PromptDraft.nameProblem("Tab\there"), .controlInName)
        XCTAssertNil(PromptDraft.nameProblem("  Summary\n"))

        XCTAssertNil(PromptDraft.Problem.blankName.message)
        XCTAssertNotNil(PromptDraft.Problem.longName.message)
        XCTAssertNotNil(PromptDraft.Problem.controlInName.message)
    }

    func testAnUnreadableFileCannotBeSavedOver() {
        let draft = PromptDraft(editing: Self.entry(), contents: .unreadable)

        XCTAssertEqual(draft.problem, .unreadable)
        XCTAssertNotNil(draft.note)
    }

    func testAOneOffSavedAsAPromptIsANewComposeDraftThatWantsAName() {
        let draft = PromptDraft(instructions: "List the action items.", kind: .compose)

        XCTAssertTrue(draft.isNew)
        XCTAssertEqual(draft.kind, .compose)
        XCTAssertTrue(draft.hasChanges)
        XCTAssertEqual(draft.problem, .blankName)
    }

    func testReadContentsTellBlankInstructionsFromAMissingFile() throws {
        let blank = Aria_Daemon_V1_GetPromptResponse.with { $0.instructions = "" }
        let missing = Aria_Daemon_V1_GetPromptResponse.with { $0.missing = true }

        XCTAssertEqual(try PromptDraft.Contents(blank), .text(""))
        XCTAssertEqual(try PromptDraft.Contents(missing), .missing)
        XCTAssertThrowsError(try PromptDraft.Contents(Aria_Daemon_V1_GetPromptResponse()))
    }

    func testASaveFailureSaysWhyUntilAFieldChanges() {
        var draft = PromptDraft(instructions: "Three bullets.", kind: .compose)
        draft.name = "Summary"

        draft.fail(.init(.refused(.promptsFull)))
        XCTAssertEqual(draft.failure, .full)
        XCTAssertEqual(draft.note, PromptDraft.Failure.full.message)
        XCTAssertEqual(PromptDraft.Failure(.unavailable), .offline)
        XCTAssertEqual(PromptDraft.Failure(.refused(.promptMissing)), .missing)
        XCTAssertEqual(PromptDraft.Failure(.refused(.promptUnreadable)), .unreadable)

        draft.name = String(repeating: "é", count: 65)
        XCTAssertNil(draft.failure)
        XCTAssertEqual(draft.note, "The name is too long.")
    }

    func testANewDraftChangedWhileSavingEditsThePromptItMade() {
        var draft = PromptDraft(instructions: "Three bullets.", kind: .compose)
        draft.name = "Summary"
        let saved = draft
        draft.instructions = "Three short bullets."
        var response = Aria_Daemon_V1_SavePromptResponse()
        response.id = "made"
        response.file = "/p/Summary.md"

        draft.didSave(saved, as: response)

        XCTAssertEqual(draft.saveRequest.id, "made")
        XCTAssertEqual(draft.saveRequest.file, "/p/Summary.md")
        XCTAssertEqual(draft.saveRequest.instructions, "Three short bullets.")
        XCTAssertTrue(draft.hasChanges)
        draft.instructions = "Three bullets."
        XCTAssertFalse(draft.hasChanges)
        XCTAssertFalse(draft.saveRequest.hasInstructions)
    }

    func testOnlyAChangedFieldCountsAsAChange() {
        var draft = PromptDraft(editing: Self.entry(), contents: .text("Fix it."))
        XCTAssertFalse(draft.hasChanges)
        draft.instructions = "Fix it well."
        XCTAssertTrue(draft.hasChanges)
        draft.instructions = "Fix it."
        XCTAssertFalse(draft.hasChanges)
    }
}
