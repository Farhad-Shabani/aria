import XCTest

@testable import Aria

final class DaemonClientTests: XCTestCase {
    // MARK: - lockedPid

    func test_lockedPid_parsesPlainPid() {
        XCTAssertEqual(DaemonClient.lockedPid("4258"), 4258)
    }

    func test_lockedPid_toleratesSurroundingWhitespace() {
        XCTAssertEqual(DaemonClient.lockedPid("4258\n"), 4258)
    }

    func test_lockedPid_rejectsMissingEmptyOrGarbage() {
        XCTAssertNil(DaemonClient.lockedPid(nil))
        XCTAssertNil(DaemonClient.lockedPid(""))
        XCTAssertNil(DaemonClient.lockedPid("not a pid"))
    }

    // MARK: - daemonBinaryCandidates

    func test_daemonBinaryCandidates_overrideWinsAndExpandsTilde() {
        let defaults = scratchDefaults()
        defaults.set("~/bin/aria", forKey: "ariaDaemonBinary")
        let candidates = DaemonClient.daemonBinaryCandidates(defaults: defaults)
        XCTAssertEqual(candidates.count, 1)
        XCTAssertTrue(candidates[0].path.hasSuffix("/bin/aria"))
        XCTAssertFalse(candidates[0].path.contains("~"))
    }

    func test_daemonBinaryCandidates_findTheBundledHelper() {
        let candidates = DaemonClient.daemonBinaryCandidates(
            bundledExecutable: URL(fileURLWithPath: "/Applications/Aria.app/Contents/MacOS/Aria"),
            defaults: scratchDefaults()
        )
        XCTAssertEqual(candidates.map(\.path), ["/Applications/Aria.app/Contents/Helpers/aria"])
    }

    // MARK: - Scratch suite (never touch UserDefaults.standard)

    private func scratchDefaults() -> UserDefaults {
        let suite = "AriaTests.\(name)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        addTeardownBlock { UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite) }
        return defaults
    }

    // MARK: - startMethod

    func test_startMethod_followsTheAgent() throws {
        XCTAssertEqual(try DaemonClient.startMethod(agentStatus: .enabled, override: nil), .kickstart)
        XCTAssertEqual(try DaemonClient.startMethod(agentStatus: .notRegistered, override: nil), .register)
        XCTAssertEqual(try DaemonClient.startMethod(agentStatus: .notFound, override: nil), .register)
    }

    func test_startMethod_leavesATurnedOffAgentOff() {
        XCTAssertThrowsError(try DaemonClient.startMethod(agentStatus: .requiresApproval, override: nil)) { error in
            guard case DaemonClientError.agentDisabled = error else {
                return XCTFail("expected agentDisabled, got \(error)")
            }
        }
    }

    func test_startMethod_spawnsAnOverriddenBinaryWhateverTheAgent() throws {
        let bin = URL(fileURLWithPath: "/opt/aria/target/release/aria")
        XCTAssertEqual(try DaemonClient.startMethod(agentStatus: .enabled, override: bin), .spawn(bin))
        XCTAssertEqual(try DaemonClient.startMethod(agentStatus: .requiresApproval, override: bin), .spawn(bin))
    }
}
