/// Apps that share which settings their page shows. An app in no group
/// shows every setting, so the table only narrows pages.
enum AppGroup {
    case chat
    case terminal
    case document
    case browser

    static func of(bundleID: String) -> AppGroup? {
        members[bundleID]
    }

    var settings: [AppSetting] {
        switch self {
        case .chat, .terminal, .document, .browser: AppSetting.allCases
        }
    }

    private static let members: [String: AppGroup] = [
        "com.tinyspeck.slackmacgap": .chat,
        "com.apple.MobileSMS": .chat,
        "com.hnc.Discord": .chat,
        "net.whatsapp.WhatsApp": .chat,
        "ru.keepcoder.Telegram": .chat,
        "com.microsoft.teams2": .chat,

        "com.apple.Terminal": .terminal,
        "com.googlecode.iterm2": .terminal,
        "com.mitchellh.ghostty": .terminal,
        "dev.warp.Warp-Stable": .terminal,
        "com.microsoft.VSCode": .terminal,
        "com.apple.dt.Xcode": .terminal,
        "dev.zed.Zed": .terminal,

        "com.apple.Notes": .document,
        "com.apple.TextEdit": .document,
        "com.apple.iWork.Pages": .document,
        "com.microsoft.Word": .document,
        "md.obsidian": .document,
        "notion.id": .document,

        "com.apple.Safari": .browser,
        "com.google.Chrome": .browser,
        "company.thebrowser.Browser": .browser,
        "org.mozilla.firefox": .browser,
    ]
}
