import XCTest

@testable import Aria

final class PromptEditsTests: XCTestCase {
    func testEntryIsAnEnabledRewriteNamedAfterTheFileWithNoChord() {
        let id = UUID()

        let entry = PromptEdits.makeEntry(for: URL(fileURLWithPath: "/notes/../prompts/Email reply.md"), id: id)

        XCTAssertEqual(entry.id, id.uuidString)
        XCTAssertEqual(entry.name, "Email reply")
        XCTAssertEqual(entry.file, "/prompts/Email reply.md")
        XCTAssertEqual(entry.kind, .rewrite)
        XCTAssertTrue(entry.enabled)
        XCTAssertNil(entry.storedChord)
    }
}
