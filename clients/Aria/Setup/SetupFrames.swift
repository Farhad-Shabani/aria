import Foundation
import SwiftProtobuf

/// Length-delimited protobuf frames, as `aria setup` reads and writes them
/// on its stdio: a varint byte count, then the message.
enum SetupFrames {
    enum FrameError: LocalizedError, Equatable {
        case malformedLength
        case messageTooLarge
        case truncated

        var errorDescription: String? {
            switch self {
            case .malformedLength: "Setup returned an invalid message length."
            case .messageTooLarge: "Setup returned a message that is too large."
            case .truncated: "Setup output ended before a message was complete."
            }
        }
    }

    static func encode(_ message: some Message) throws -> Data {
        let payload: Data = try message.serializedData()
        var frame = Data()
        var length = UInt64(payload.count)
        repeat {
            var byte = UInt8(length & 0x7f)
            length >>= 7
            if length != 0 { byte |= 0x80 }
            frame.append(byte)
        } while length != 0
        frame.append(payload)
        return frame
    }

    /// Cuts a byte stream into frames as it arrives; one split across two
    /// pushes comes out whole on the second.
    struct Reader {
        private var buffer = Data()

        mutating func push(_ bytes: Data) throws(FrameError) -> [Data] {
            buffer.append(bytes)
            var frames = [Data]()
            while let (length, prefix) = try Self.lengthPrefix(of: buffer) {
                guard length <= buffer.count - prefix else { break }
                let start = buffer.startIndex + prefix
                frames.append(Data(buffer[start..<(start + length)]))
                buffer = Data(buffer[(start + length)...])
            }
            return frames
        }

        func finish() throws(FrameError) {
            guard buffer.isEmpty else { throw .truncated }
        }

        /// The frame length and how many bytes spell it, once all of them
        /// have arrived.
        private static func lengthPrefix(of data: Data) throws(FrameError) -> (Int, Int)? {
            var value: UInt64 = 0
            for (offset, byte) in data.enumerated() {
                guard offset < 9 || byte <= 1 else { throw .malformedLength }
                value |= UInt64(byte & 0x7f) << (offset * 7)
                if byte & 0x80 == 0 {
                    // SwiftProtobuf limits delimited messages to 2 GiB minus one byte.
                    guard value <= UInt64(Int32.max) else { throw .messageTooLarge }
                    return (Int(value), offset + 1)
                }
            }
            return nil
        }
    }
}
