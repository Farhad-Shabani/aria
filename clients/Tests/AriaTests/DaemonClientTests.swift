import XCTest

@testable import Aria

final class DaemonClientTests: XCTestCase {
    func testCLIFailureKeepsTheFirstLineAsASentence() {
        let cli = Data("error: remote invitation is invalid: eof\n".utf8)
        XCTAssertEqual(DaemonClient.cliFailure(stderr: cli, fallback: "Failed."), "Remote invitation is invalid: eof.")
        XCTAssertEqual(
            DaemonClient.cliFailure(stderr: Data("Already a sentence.".utf8), fallback: "Failed."),
            "Already a sentence.")
        XCTAssertEqual(DaemonClient.cliFailure(stderr: Data(), fallback: "Failed."), "Failed.")
    }

    func testLockedPidParsesPlainPid() {
        XCTAssertEqual(DaemonClient.lockedPid("4258"), 4258)
    }

    func testLockedPidToleratesSurroundingWhitespace() {
        XCTAssertEqual(DaemonClient.lockedPid("4258\n"), 4258)
    }

    func testLockedPidRejectsMissingEmptyOrGarbage() {
        XCTAssertNil(DaemonClient.lockedPid(nil))
        XCTAssertNil(DaemonClient.lockedPid(""))
        XCTAssertNil(DaemonClient.lockedPid("not a pid"))
    }

    func testDaemonBinaryCandidatesOverrideWinsAndExpandsTilde() {
        let defaults = scratchDefaults()
        defaults.set("~/bin/aria", forKey: "ariaDaemonBinary")
        let candidates = DaemonClient.daemonBinaryCandidates(defaults: defaults)
        XCTAssertEqual(candidates.count, 1)
        XCTAssertTrue(candidates[0].path.hasSuffix("/bin/aria"))
        XCTAssertFalse(candidates[0].path.contains("~"))
    }

    func testDaemonBinaryCandidatesFindTheBundledHelper() {
        let candidates = DaemonClient.daemonBinaryCandidates(
            bundledExecutable: URL(fileURLWithPath: "/Applications/Aria.app/Contents/MacOS/Aria"),
            defaults: scratchDefaults()
        )
        XCTAssertEqual(
            candidates.map(\.path),
            ["/Applications/Aria.app/Contents/Helpers/Aria.app/Contents/MacOS/aria"]
        )
    }

    private func scratchDefaults() -> UserDefaults {
        let suite = "AriaTests.\(name)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        addTeardownBlock { UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite) }
        return defaults
    }

    func testStartMethodFollowsTheAgent() throws {
        XCTAssertEqual(try DaemonClient.startMethod(agentStatus: .enabled, jobLoaded: true, override: nil), .kickstart)
        XCTAssertEqual(try DaemonClient.startMethod(agentStatus: .enabled, jobLoaded: false, override: nil), .register)
        XCTAssertEqual(
            try DaemonClient.startMethod(agentStatus: .notRegistered, jobLoaded: false, override: nil), .register)
        XCTAssertEqual(try DaemonClient.startMethod(agentStatus: .notFound, jobLoaded: false, override: nil), .register)
    }

    func testStartMethodLeavesATurnedOffAgentOff() {
        XCTAssertThrowsError(
            try DaemonClient.startMethod(agentStatus: .requiresApproval, jobLoaded: false, override: nil)
        ) { error in
            guard case DaemonClientError.agentDisabled = error else {
                return XCTFail("expected agentDisabled, got \(error)")
            }
        }
    }

    func testStartMethodSpawnsAnOverriddenBinaryWhateverTheAgent() throws {
        let bin = URL(fileURLWithPath: "/opt/aria/target/release/aria")
        XCTAssertEqual(try DaemonClient.startMethod(agentStatus: .enabled, jobLoaded: true, override: bin), .spawn(bin))
        XCTAssertEqual(
            try DaemonClient.startMethod(agentStatus: .requiresApproval, jobLoaded: false, override: bin), .spawn(bin))
    }
}
