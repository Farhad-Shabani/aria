import XCTest

@testable import Aria

final class CommandLineToolTests: XCTestCase {
    func test_shellQuoted_keepsQuotesAndSpacesLiteral() {
        XCTAssertEqual(CommandLineTool.shellQuoted("/Apps/My Aria.app"), "'/Apps/My Aria.app'")
        XCTAssertEqual(CommandLineTool.shellQuoted("it's"), "'it'\\''s'")
    }

    func test_onlyAnInstalledCopyLinks() {
        XCTAssertTrue(CommandLineTool.isInstalledLocation(URL(fileURLWithPath: "/Applications/Aria.app")))
        XCTAssertTrue(CommandLineTool.isInstalledLocation(URL(fileURLWithPath: "/Users/me/Applications/Aria.app")))
        XCTAssertFalse(
            CommandLineTool.isInstalledLocation(URL(fileURLWithPath: "/Users/me/aria/clients/build/Aria.app")))
        XCTAssertFalse(
            CommandLineTool.isInstalledLocation(
                URL(fileURLWithPath: "/private/var/folders/x/AppTranslocation/1234/d/Aria.app")))
    }

    func test_appleScriptQuoted_escapesBackslashesAndDoubleQuotes() {
        XCTAssertEqual(CommandLineTool.appleScriptQuoted(#"a "b" \c"#), #""a \"b\" \\c""#)
    }
}
