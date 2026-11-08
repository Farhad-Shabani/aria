import XCTest

final class UpdateFeedTests: XCTestCase {
    func testBundleNamesASignedHTTPSFeed() throws {
        let info = try XCTUnwrap(Bundle.main.infoDictionary)
        let feed = try XCTUnwrap((info["SUFeedURL"] as? String).flatMap(URL.init(string:)))
        XCTAssertEqual(feed.scheme, "https")
        let key = try XCTUnwrap(info["SUPublicEDKey"] as? String)
        XCTAssertFalse(key.isEmpty, "set SUPublicEDKey from Sparkle's generate_keys")
    }
}
