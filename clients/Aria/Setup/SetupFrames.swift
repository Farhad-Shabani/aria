import Foundation
import SwiftProtobuf

/// Length-delimited protobuf frames, as `aria setup` reads and writes them
/// on its stdio: a varint byte count, then the message.
enum SetupFrames {
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

        mutating func push(_ bytes: Data) -> [Data] {
            buffer.append(bytes)
            var frames = [Data]()
            while let (length, prefix) = Self.lengthPrefix(of: buffer), buffer.count >= prefix + length {
                let start = buffer.startIndex + prefix
                frames.append(Data(buffer[start..<(start + length)]))
                buffer = Data(buffer[(start + length)...])
            }
            return frames
        }

        /// The frame length and how many bytes spell it, once all of them
        /// have arrived.
        private static func lengthPrefix(of data: Data) -> (Int, Int)? {
            var value: UInt64 = 0
            var shift: UInt64 = 0
            for (offset, byte) in data.enumerated() {
                value |= UInt64(byte & 0x7f) << shift
                if byte & 0x80 == 0 { return (Int(value), offset + 1) }
                shift += 7
                if shift > 63 { return nil }
            }
            return nil
        }
    }
}
