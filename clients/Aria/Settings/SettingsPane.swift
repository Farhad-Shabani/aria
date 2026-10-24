import SwiftUI

enum SettingsPane: String, CaseIterable, Identifiable {
    case general
    case dictation
    case recognition
    case shortcuts
    case prompts
    case apps
    case privacy
    case phone
    case about

    static let sidebarGroups: [[SettingsPane]] = [
        [.general],
        [.dictation, .recognition, .shortcuts, .prompts, .apps],
        [.privacy, .phone],
        [.about],
    ]

    var id: Self { self }

    var title: String {
        switch self {
        case .general: "General"
        case .dictation: "Dictation"
        case .recognition: "Recognition"
        case .shortcuts: "Shortcuts"
        case .prompts: "Prompts"
        case .apps: "Apps"
        case .privacy: "Privacy"
        case .phone: "Phone"
        case .about: "About"
        }
    }

    var symbol: String {
        switch self {
        case .general: "gearshape.fill"
        case .dictation: "mic.fill"
        case .recognition: "waveform"
        case .shortcuts: "keyboard.fill"
        case .prompts: "text.badge.star"
        case .apps: "square.grid.2x2.fill"
        case .privacy: "hand.raised.fill"
        case .phone: "iphone"
        case .about: "info"
        }
    }

    var tint: Color {
        switch self {
        case .general, .about: .gray
        case .dictation: .red
        case .recognition: .purple
        case .shortcuts: .indigo
        case .prompts: .orange
        case .privacy, .apps: .blue
        case .phone: .green
        }
    }
}
