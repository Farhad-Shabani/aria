import Foundation
import GRPCCore
import XCTest

@testable import Aria

final class DaemonTransportTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        // A Unix socket path is length-limited, so a short temp dir.
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("aria-\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try FileManager.default.removeItem(at: directory)
    }

    func testNothingListeningIsUnavailable() async {
        let socket = directory.appendingPathComponent("absent.sock")
        do {
            _ = try await DaemonTransport.call(socket: socket) { client, options in
                try await Grpc_Health_V1_Health.Client(wrapping: client)
                    .check(request: ClientRequest(message: .init()), options: options) { try $0.message }
            }
            XCTFail("a call with nothing listening returned")
        } catch {
            XCTAssertEqual(error as? DaemonFailure, .unavailable)
        }
    }

    func testAServerThatNeverAnswersTimesOutClientSide() async throws {
        let socket = directory.appendingPathComponent("deaf.sock")
        let listener = try DeafListener(path: socket.path)
        defer { listener.close() }
        let started = ContinuousClock.now
        do {
            _ = try await DaemonTransport.call(timeout: .milliseconds(200), socket: socket) { client, options in
                try await Grpc_Health_V1_Health.Client(wrapping: client)
                    .check(request: ClientRequest(message: .init()), options: options) { try $0.message }
            }
            XCTFail("a call nobody answers returned")
        } catch {
            XCTAssertEqual(error as? DaemonFailure, .timeout)
        }
        let elapsed = ContinuousClock.now - started
        XCTAssertLessThan(elapsed, .seconds(2), "the deadline, not the connection, ended the call")
    }
}

/// A bound Unix socket that accepts connections into its backlog and never
/// reads them, like a daemon that hung.
private struct DeafListener {
    let descriptor: Int32

    init(path: String) throws {
        descriptor = socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8CString)
        precondition(bytes.count <= MemoryLayout.size(ofValue: address.sun_path), "socket path too long")
        withUnsafeMutableBytes(of: &address.sun_path) { $0.copyBytes(from: bytes.map { UInt8(bitPattern: $0) }) }
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(descriptor, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bound == 0, listen(descriptor, 1) == 0 else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }
    }

    func close() {
        Darwin.close(descriptor)
    }
}
