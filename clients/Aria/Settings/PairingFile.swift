import Foundation

/// The daemon's pairing material. The path is the daemon's contract, pinned
/// by tests in crates/aria/src/daemon/relay/pairing.rs.
enum PairingFile {
    private static let url = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent(".aria/phone/pairing.json")

    /// Revokes every phone: the daemon mints new material on its next start.
    static func remove() throws {
        do {
            try FileManager.default.removeItem(at: url)
        } catch let error as CocoaError where error.code == .fileNoSuchFile {
            return
        }
    }
}
