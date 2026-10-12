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

    func test_startMethod_kickstartsTheServiceWheneverInstalled() {
        let bin = URL(fileURLWithPath: "/opt/aria/target/release/aria")
        XCTAssertEqual(
            DaemonClient.startMethod(serviceInstalled: true, executableCandidates: [bin]),
            .kickstart)
        XCTAssertEqual(
            DaemonClient.startMethod(serviceInstalled: true, executableCandidates: []),
            .kickstart)
    }

    func test_startMethod_spawnsTheFirstExecutableWithoutTheService() {
        let release = URL(fileURLWithPath: "/opt/aria/target/release/aria")
        let debug = URL(fileURLWithPath: "/opt/aria/target/debug/aria")
        XCTAssertEqual(
            DaemonClient.startMethod(serviceInstalled: false, executableCandidates: [release, debug]),
            .spawn(release))
        XCTAssertNil(DaemonClient.startMethod(serviceInstalled: false, executableCandidates: []))
    }

    func test_servicePlist_mirrorsTheRustLabel() {
        XCTAssertEqual(DaemonClient.serviceLabel, "com.aria.daemon")
        XCTAssertTrue(
            DaemonClient.servicePlistURL.path.hasSuffix("/Library/LaunchAgents/com.aria.daemon.plist"))
    }
}
