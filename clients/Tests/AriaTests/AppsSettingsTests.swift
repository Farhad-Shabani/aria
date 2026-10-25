import XCTest

@testable import Aria

final class AppsSettingsTests: XCTestCase {
    private let slack = "com.tinyspeck.slackmacgap"

    private func withSlack() -> Aria_Daemon_V1_ApplicationOverrides {
        var apps = Aria_Daemon_V1_ApplicationOverrides()
        ApplicationOverrideEdits.add(slack, to: &apps)
        return apps
    }

    func testAddingAnAppTwiceKeepsOneEntry() {
        var apps = withSlack()

        ApplicationOverrideEdits.add(slack, to: &apps)

        XCTAssertEqual(apps.entries.map(\.applicationID), [slack])
        XCTAssertEqual(ApplicationOverrideEdits.overrides(for: slack, in: apps), Aria_Daemon_V1_DictationOverrides())
    }

    func testRemovingDropsOnlyThatApp() {
        var apps = withSlack()
        ApplicationOverrideEdits.add("com.apple.Notes", to: &apps)

        ApplicationOverrideEdits.remove(slack, from: &apps)

        XCTAssertEqual(apps.entries.map(\.applicationID), ["com.apple.Notes"])
    }

    func testChoosingDefaultClearsTheSetting() {
        var apps = withSlack()
        ApplicationOverrideEdits.setOutput(.insertAndSubmit, for: slack, in: &apps)

        ApplicationOverrideEdits.setOutput(nil, for: slack, in: &apps)

        XCTAssertEqual(ApplicationOverrideEdits.overrides(for: slack, in: apps)?.hasOutputMode, false)
    }

    func testAChoiceMatchingDictationReadsAsDefault() {
        let dictation = Aria_Daemon_V1_OutputMode.insert

        XCTAssertNil(ApplicationOverrideEdits.choice(Optional(dictation), dictation: dictation))
        XCTAssertNil(ApplicationOverrideEdits.choice(nil, dictation: dictation))
        XCTAssertEqual(ApplicationOverrideEdits.choice(.copy, dictation: dictation), .copy)
    }

    func testSwitchingAriaOffKeepsTheAppsSettings() {
        var apps = withSlack()
        ApplicationOverrideEdits.setOutput(.copy, for: slack, in: &apps)

        ApplicationOverrideEdits.setExcluded(true, for: slack, in: &apps)

        XCTAssertTrue(ApplicationOverrideEdits.isExcluded(slack, in: apps))
        XCTAssertEqual(ApplicationOverrideEdits.overrides(for: slack, in: apps)?.outputMode, .copy)
        ApplicationOverrideEdits.setExcluded(false, for: slack, in: &apps)
        XCTAssertFalse(ApplicationOverrideEdits.isExcluded(slack, in: apps))
    }

    func testEditingAnAppNotInTheListChangesNothing() {
        let original = withSlack()
        var apps = original

        ApplicationOverrideEdits.setOutput(.copy, for: "com.apple.Notes", in: &apps)

        XCTAssertEqual(apps, original)
    }

    func testGroupsKnowCommonAppsAndLeaveOthersOut() {
        XCTAssertEqual(AppGroup.of(bundleID: slack), .chat)
        XCTAssertEqual(AppGroup.of(bundleID: "com.apple.Terminal"), .terminal)
        XCTAssertNil(AppGroup.of(bundleID: "com.example.unknown"))
    }

    func testAnAppInNoGroupShowsEverySetting() {
        let shown = AppSetting.shown(for: "com.example.unknown", overrides: Aria_Daemon_V1_DictationOverrides())

        XCTAssertEqual(shown, AppSetting.allCases)
    }

    func testAStoredSettingIsAlwaysShown() {
        var overrides = Aria_Daemon_V1_DictationOverrides()
        overrides.deliveryMode = .complete

        XCTAssertTrue(AppSetting.shown(for: slack, overrides: overrides).contains(.delivery))
    }

    func testAnAppPageBelongsToTheAppsPane() {
        XCTAssertEqual(SettingsPage.item(.apps, id: slack).pane, .apps)
        XCTAssertEqual(SettingsPage.pane(.privacy).pane, .privacy)
    }
}
