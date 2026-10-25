import Synchronization
import XCTest

@testable import Aria

final class ChildProcessTests: XCTestCase {
    private let shell = URL(fileURLWithPath: "/bin/sh")

    func testRunReportsExitStatus() async throws {
        let output = try await ChildProcess.run(shell, ["-c", "exit 3"])
        XCTAssertEqual(output.status, 3)
    }

    func testRunCollectsBothPipes() async throws {
        let output = try await ChildProcess.run(shell, ["-c", "printf out; printf err >&2"])
        XCTAssertEqual(output.status, 0)
        XCTAssertEqual(String(decoding: output.stdout, as: UTF8.self), "out")
        XCTAssertEqual(String(decoding: output.stderr, as: UTF8.self), "err")
    }

    func testLaunchWritesInputAndStreamsStdout() async throws {
        let child = ChildProcess(shell, ["-c", "cat"])
        let streamed = Mutex(Data())
        try child.launch(input: Data("request".utf8)) { chunk in
            streamed.withLock { $0.append(chunk) }
        }
        let output = await child.exit()
        XCTAssertEqual(output.status, 0)
        XCTAssertEqual(streamed.withLock { String(decoding: $0, as: UTF8.self) }, "request")
        XCTAssertTrue(output.stdout.isEmpty)
    }

    /// Far more stderr than a pipe buffers, written before any stdout: a
    /// reader that took stdout to its end first would never see it close.
    func testRunDrainsLargeStderrBeforeStdout() async throws {
        let stderrBytes = 1 << 20
        let script = "head -c \(stderrBytes) /dev/zero >&2; printf done"
        let output = try await ChildProcess.run(shell, ["-c", script])
        XCTAssertEqual(output.status, 0)
        XCTAssertEqual(output.stderr.count, stderrBytes)
        XCTAssertEqual(String(decoding: output.stdout, as: UTF8.self), "done")
    }

    func testRunCancellationInterruptsChild() async throws {
        let shell = shell
        let run = Task { try await ChildProcess.run(shell, ["-c", "printf started; exec sleep 30"]) }
        try await Task.sleep(for: .milliseconds(200))
        run.cancel()
        let output = try await run.value
        XCTAssertEqual(output.status, SIGINT)
        XCTAssertEqual(String(decoding: output.stdout, as: UTF8.self), "started")
    }

    func testRunThrowsForMissingExecutable() async {
        do {
            _ = try await ChildProcess.run(URL(fileURLWithPath: "/nonexistent/aria"), [])
            XCTFail("expected a launch failure")
        } catch {}
    }
}
