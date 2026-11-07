import Foundation

@testable import Aria

extension Aria_Daemon_V1_Prompt {
    static func stub(file: String, id: String = UUID().uuidString) -> Self {
        var entry = Self()
        entry.id = id
        entry.name = URL(fileURLWithPath: file).deletingPathExtension().lastPathComponent
        entry.kind = .rewrite
        entry.file = file
        entry.enabled = true
        return entry
    }
}
