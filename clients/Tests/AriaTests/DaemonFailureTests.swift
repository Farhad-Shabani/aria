import GRPCCore
import GRPCProtobuf
import SwiftProtobuf
import XCTest

@testable import Aria

final class DaemonFailureTests: XCTestCase {
    /// A status as the daemon sends one: its verdict packed in the details,
    /// under whatever gRPC code it projected it to.
    private func refusal(_ code: Aria_Daemon_V1_ErrorCode, under status: RPCError.Code) throws -> RPCError {
        var error = Aria_Daemon_V1_RpcError()
        error.code = code
        error.message = "A differently worded explanation."
        let packed = GoogleRPCStatus(code: status, message: "", details: .any(try Google_Protobuf_Any(message: error)))
        var metadata = Metadata()
        metadata.addBinary(try packed.serializedBytes(), forKey: "grpc-status-details-bin")
        return RPCError(code: status, message: "", metadata: metadata)
    }

    func testADetailIsTheDaemonsVerdictWhateverTheStatusCode() throws {
        XCTAssertEqual(
            DaemonFailure(try refusal(.historyUnavailable, under: .unavailable)), .refused(.historyUnavailable))
        XCTAssertEqual(
            DaemonFailure(try refusal(.historyProjectUnavailable, under: .failedPrecondition)),
            .refused(.historyProjectUnavailable))
        XCTAssertEqual(DaemonFailure(try refusal(.cancelled, under: .cancelled)), .refused(.cancelled))
    }

    func testAStatusWithoutADetailIsTransport() {
        XCTAssertEqual(DaemonFailure(RPCError(code: .unavailable, message: "")), .unavailable)
        XCTAssertEqual(DaemonFailure(RPCError(code: .unimplemented, message: "")), .unimplemented)
        XCTAssertEqual(DaemonFailure(RPCError(code: .deadlineExceeded, message: "")), .timeout)
        XCTAssertEqual(DaemonFailure(RPCError(code: .cancelled, message: "")), .cancelled)
        XCTAssertEqual(DaemonFailure(RPCError(code: .internalError, message: "raw")), .transport(.internalError))
    }

    func testAnErrorOutsideGRPCIsUnknownTransport() {
        struct Broken: Error {}
        XCTAssertEqual(DaemonFailure(Broken()), .transport(.unknown))
    }

    func testHistoryReadsTheVerdictBeforeTheProjection() {
        XCTAssertEqual(HistoryFailure(.refused(.historyUnavailable)), .storage)
        XCTAssertEqual(HistoryFailure(.refused(.historyProjectUnavailable)), .projectUnavailable)
        XCTAssertEqual(HistoryFailure(.refused(.shuttingDown)), .offline)
        XCTAssertEqual(HistoryFailure(.unavailable), .offline)
        XCTAssertEqual(HistoryFailure(.timeout), .timeout)
        XCTAssertEqual(HistoryFailure(.transport(.resourceExhausted)), .busy)
        XCTAssertEqual(HistoryFailure(.transport(.notFound)), .missing)
        XCTAssertEqual(HistoryFailure(.invalidResponse), .failed)
    }
}
