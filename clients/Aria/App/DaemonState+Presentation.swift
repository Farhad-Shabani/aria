import SwiftUI

extension DaemonState {
    var statusLine: String {
        switch self {
        case .running: return "Running"
        case .stopped: return "Stopped"
        case .working: return "Working…"
        }
    }

    var statusColor: Color {
        switch self {
        case .running: return .green
        case .stopped: return .gray
        case .working: return .orange
        }
    }
}
