import Foundation

extension Bundle {
    var displayName: String {
        infoDictionary?["CFBundleDisplayName"] as? String
            ?? infoDictionary?["CFBundleName"] as? String
            ?? ProcessInfo.processInfo.processName
    }

    var shortVersion: String? {
        infoDictionary?["CFBundleShortVersionString"] as? String
    }

    var copyright: String? {
        infoDictionary?["NSHumanReadableCopyright"] as? String
    }
}
