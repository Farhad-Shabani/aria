import XCTest

@testable import Aria

final class SetupItemTests: XCTestCase {
    private static func invalidStatus(line: UInt32?) -> Aria_Daemon_V1_SettingsStatus {
        var status = Aria_Daemon_V1_SettingsStatus()
        status.fault.message = "unknown field `secret`"
        if let line { status.fault.position.line = line }
        return status
    }

    func testWhatStopsAriaFromStartingComesFirst() {
        let items = SetupItem.all(
            inspected: [.ariaKey, .speechModel(id: "parakeet")], settingsStatus: nil,
            startFailure: .backgroundItemOff)

        XCTAssertEqual(items, [.backgroundService, .ariaKey, .speechModel(id: "parakeet")])
    }

    func testAnInvalidFileNamesItsLineAndNeverTheParserText() {
        let items = SetupItem.all(inspected: [], settingsStatus: Self.invalidStatus(line: 3), startFailure: nil)

        XCTAssertEqual(items, [.settingsFileIgnored(line: 3)])
        XCTAssertEqual(items[0].detail, "Line 3 isn’t valid, so Aria is using your last saved settings.")
        XCTAssertFalse(items[0].detail.contains("secret"))
    }

    func testAFileThatStopsTheStartIsListedOnce() {
        let items = SetupItem.all(
            inspected: [], settingsStatus: Self.invalidStatus(line: 2),
            startFailure: .settingsInvalid(InvalidSettings(line: 2)))

        XCTAssertEqual(items, [.settingsFileStopsStart(line: 2)])
        XCTAssertEqual(items[0].detail, "Aria can’t start because line 2 isn’t valid.")
    }

    func testAFaultWithoutALineNamesTheFile() {
        XCTAssertEqual(
            SetupItem.settingsFileStopsStart(line: nil).detail,
            "Aria can’t start because your settings file isn’t valid.")
    }

    func testAGenericStartFailureIsNoItem() {
        XCTAssertEqual(SetupItem.all(inspected: [], settingsStatus: nil, startFailure: .start), [])
    }
}
