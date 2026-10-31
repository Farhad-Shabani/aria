import GRPCCore
import XCTest

@testable import Aria

@MainActor
final class SettingsHealthTests: XCTestCase {
    private struct Refused: Error {}

    private func health(
        status: Aria_Daemon_V1_SettingsStatus = .init(),
        reload: @escaping () async -> Void = {},
        file: SettingsFile.Effects? = nil
    ) -> SettingsHealth {
        SettingsHealth(
            effects: .init(
                status: { status },
                reloadSettings: reload,
                settingsFile: file ?? Self.file(exists: true) { _ in }
            ))
    }

    private static func file(
        exists: Bool, created: @escaping () -> Void = {}, opened: @escaping (URL) throws -> Void
    ) -> SettingsFile.Effects {
        .init(
            path: { URL(fileURLWithPath: "/home/.aria/settings.toml") },
            exists: { _ in exists },
            create: { created() },
            openInTextEditor: { try opened($0) }
        )
    }

    private func invalidStatus(line: UInt32) -> Aria_Daemon_V1_SettingsStatus {
        var status = Aria_Daemon_V1_SettingsStatus()
        status.fault.message = "unknown field `secret`"
        status.fault.position.line = line
        status.fault.position.column = 1
        return status
    }

    // MARK: Opening the file

    func testAMissingFileIsCreatedThroughTheCLIBeforeItOpens() async {
        var steps: [String] = []
        let failure = await SettingsFile.open(
            Self.file(exists: false, created: { steps.append("create") }) { _ in steps.append("open") })

        XCTAssertNil(failure)
        XCTAssertEqual(steps, ["create", "open"])
    }

    func testAFileThatExistsIsNeverCreated() async {
        var steps: [String] = []
        _ = await SettingsFile.open(
            Self.file(exists: true, created: { steps.append("create") }) { _ in steps.append("open") })

        XCTAssertEqual(steps, ["open"])
    }

    func testAFailedOpenSaysWhatToDoWithoutTheSystemsText() async {
        let failure = await SettingsFile.open(Self.file(exists: true) { _ in throw Refused() })

        XCTAssertEqual(failure, "Aria couldn’t open your settings file. Try again.")
    }

    // MARK: What the person sees

    func testAnInvalidFileNamesItsLineAndNeverTheParserText() {
        let items = SettingsAttention.all(
            status: invalidStatus(line: 3), startFailure: nil, isReadOnly: false)

        XCTAssertEqual(items, [.invalid(line: 3)])
        XCTAssertEqual(items[0].message, "Your settings file has a mistake on line 3. Fix it and save the file.")
        XCTAssertFalse(items[0].message.contains("secret"))
        XCTAssertTrue(items[0].opensFile)
    }

    func testARestartKeyAsksForARestartAndARevertNamesItsSetting() {
        var status = Aria_Daemon_V1_SettingsStatus()
        status.restartRequired = ["phone.port"]
        status.reverted = ["memory.enabled"]

        let items = SettingsAttention.all(status: status, startFailure: nil, isReadOnly: false)

        XCTAssertEqual(items, [.restartNeeded, .reverted])
        XCTAssertEqual(items[1].message, "A save in another app undid a change you made in Aria.")
    }

    func testAReadOnlyFileSaysItCantSave() {
        let items = SettingsAttention.all(status: nil, startFailure: nil, isReadOnly: true)

        XCTAssertEqual(
            items[0].message,
            "Aria can’t save settings because your settings file is read-only. Change it where it’s managed.")
    }

    func testAReadOnlyRefusalIsTheOnlyErrorThatReadsAsReadOnly() async {
        let transport = FakeSettingsTransport()
        var loaded = Aria_Daemon_V1_SettingsSnapshot()
        loaded.revision = 1
        transport.snapshots = [loaded, loaded]
        transport.updateError = RPCError(code: .permissionDenied, message: "settings.toml is read-only")
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        XCTAssertFalse(settings.isReadOnlyFailure)

        await settings.set { $0.muteSound = true }

        XCTAssertTrue(settings.isReadOnlyFailure)
    }

    func testAStartBlockedByTheFileShowsThatCause() {
        let items = SettingsAttention.all(
            status: nil, startFailure: .settingsInvalid(InvalidSettings(line: 2)), isReadOnly: false)

        XCTAssertEqual(items, [.startBlocked(line: 2)])
        XCTAssertTrue(items[0].opensFile)
    }

    func testTheCheckReportReadsItsLineAndAValidFileIsNil() {
        let invalid = Data(
            #"{"valid":false,"error":{"message":"x","line":4,"column":2}}"#.utf8)
        let placeless = Data(#"{"valid":false,"error":{"message":"x","line":null,"column":null}}"#.utf8)
        let valid = Data(#"{"valid":true,"error":null}"#.utf8)

        XCTAssertEqual(InvalidSettings(checkOutput: invalid), InvalidSettings(line: 4))
        XCTAssertEqual(InvalidSettings(checkOutput: placeless), InvalidSettings(line: nil))
        XCTAssertNil(InvalidSettings(checkOutput: valid))
        XCTAssertNil(InvalidSettings(checkOutput: Data("nonsense".utf8)))
    }

    // MARK: Following the daemon

    func testAStatusEventReadsAHandEditIntoTheSnapshot() async {
        let transport = FakeSettingsTransport()
        var before = Aria_Daemon_V1_SettingsSnapshot()
        before.revision = 900
        var edited = before
        edited.revision = 12
        edited.settings.memoryEnabled = true
        transport.snapshots = [before, edited]
        let settings = DaemonSettings(transport: transport)
        await settings.sync()
        let health = health(reload: { await settings.sync() })

        var status = Aria_Daemon_V1_SettingsStatus()
        status.revision = 12
        await health.receive(status)

        XCTAssertEqual(settings.snapshot?.memoryEnabled, true)
    }

    func testAStreamThatReopensReadsTheStatusAndTheValuesAgain() async {
        var reloads = 0
        let health = health(status: invalidStatus(line: 7), reload: { reloads += 1 })
        await health.receive(nil)

        await health.streamOpened()

        XCTAssertEqual(health.status?.fault.position.line, 7)
        XCTAssertEqual(reloads, 1)
    }

    func testAnUnsavedChangeIsAProblemToShow() {
        let items = SettingsAttention.all(
            status: nil, startFailure: nil, isReadOnly: false, hasUnsavedChange: true)

        XCTAssertEqual(items, [.unsaved])
        XCTAssertTrue(items[0].isProblem)
    }

    func testALostStreamClearsTheStatusAndReadsNothing() async {
        var reloads = 0
        let health = health(reload: { reloads += 1 })
        await health.receive(invalidStatus(line: 1))

        await health.receive(nil)

        XCTAssertNil(health.status)
        XCTAssertEqual(reloads, 1)
    }
}
