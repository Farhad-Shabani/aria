import Sparkle
import XCTest

@testable import Aria

final class UpdateFeedTests: XCTestCase {
    func testBundleNamesASignedHTTPSFeed() throws {
        let info = try XCTUnwrap(Bundle.main.infoDictionary)
        let feed = try XCTUnwrap((info["SUFeedURL"] as? String).flatMap(URL.init(string:)))
        XCTAssertEqual(feed.scheme, "https")
        let key = try XCTUnwrap(info["SUPublicEDKey"] as? String)
        XCTAssertFalse(key.isEmpty, "set SUPublicEDKey from Sparkle's generate_keys")
    }
}

// Sparkle builds an item outside a feed only through a deprecated initializer.
@available(*, deprecated)
final class ScheduledUpdateTests: XCTestCase {
    func testARoutineUpdateNeverOpensSparklesAlert() throws {
        let update = try appcastItem(critical: false)
        XCTAssertFalse(shows(update, immediateFocus: true))
        XCTAssertFalse(shows(update, immediateFocus: false))
    }

    func testACriticalUpdateOpensSparklesAlertOnlyNearLaunch() throws {
        let update = try appcastItem(critical: true)
        XCTAssertTrue(shows(update, immediateFocus: true))
        XCTAssertFalse(shows(update, immediateFocus: false))
    }

    private func shows(_ update: SUAppcastItem, immediateFocus: Bool) -> Bool {
        ReminderDriverDelegate().standardUserDriverShouldHandleShowingScheduledUpdate(
            update, andInImmediateFocus: immediateFocus)
    }
}

@available(*, deprecated)
private func appcastItem(critical: Bool) throws -> SUAppcastItem {
    var item: [String: Any] = [
        "sparkle:version": "2",
        "enclosure": ["url": "https://example.com/Aria.zip"],
    ]
    if critical { item["sparkle:criticalUpdate"] = [String: Any]() }
    return try XCTUnwrap(SUAppcastItem(dictionary: item))
}
