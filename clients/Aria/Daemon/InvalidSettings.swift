import Foundation

/// What `aria settings check --json` found wrong with the file.
struct InvalidSettings: Equatable, Sendable {
    /// Nil when the fault has no place in the file.
    let line: Int?

    init(line: Int?) {
        self.line = line
    }

    /// Nil for a valid file, or for output this build can't read.
    init?(checkOutput stdout: Data) {
        struct Report: Decodable {
            struct Fault: Decodable { let line: Int? }
            let valid: Bool
            let error: Fault?
        }
        guard let report = try? JSONDecoder().decode(Report.self, from: stdout), !report.valid else {
            return nil
        }
        line = report.error?.line
    }
}
