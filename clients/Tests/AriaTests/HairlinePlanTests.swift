import XCTest

@testable import Aria

final class HairlinePlanTests: XCTestCase {
    private let sizes = [(width: 176.0, scale: 3.0), (width: 96.0, scale: 3.0), (width: 104.0, scale: 2.0)]
    /// Each preset at sizes it draws at, including the two-ring fallback's 1 px lines.
    private let marks: [(width: Double, scale: Double, lines: HairlinePlan.Lines)] = [
        (176, 3, .button), (96, 3, .button), (104, 2, .button), (176, 3, .large), (176, 2, .large),
        (536, 1, .large), (118, 1, .small), (26, 1, .small), (38, 1, .small),
    ]

    func testLargeMarksStopAtSixteenWaves() {
        let expected = [
            (width: 176.0, scale: 3.0, rings: 10, waves: 16),
            (width: 96.0, scale: 3.0, rings: 6, waves: 10),
            (width: 104.0, scale: 2.0, rings: 4, waves: 7),
        ]
        for size in expected {
            let plan = HairlinePlan(width: size.width, displayScale: size.scale)
            XCTAssertEqual(plan.rings.count, size.rings, "\(size)")
            XCTAssertEqual(plan.gaps + 1, size.waves, "\(size)")
        }
    }

    func testSmallLinesAreHeavierAndCloserThanTheButtons() {
        // The 180 px Home Screen icon's mark, drawn at one pixel per point.
        let icon = HairlinePlan(width: 118, displayScale: 1, lines: .small)
        let button = HairlinePlan(width: 118, displayScale: 1)
        let perUnit = 118 / MarkTuning.visible.width
        XCTAssertEqual(icon.hairline * perUnit, 2, accuracy: 1e-9)
        XCTAssertEqual(icon.step * perUnit, 3, accuracy: 1e-9)
        XCTAssertEqual(icon.rings.count, 3)
        XCTAssertEqual(button.step * perUnit, 4, accuracy: 1e-9)
    }

    func testRingsInsideStopBeforeRunningPastTheRing() {
        // A 666 px mark: 3 px lines on a 6 px step. Counted from where each ring starts, nine fit.
        let width = 666.0
        let plan = HairlinePlan(width: width, displayScale: 1, lines: .init(weight: 2, minGap: 3, ringFit: .inside))
        let perUnit = width / MarkTuning.visible.width
        XCTAssertEqual(plan.hairline * perUnit, 3, accuracy: 1e-9)
        XCTAssertEqual(plan.step * perUnit, 6, accuracy: 1e-9)
        XCTAssertEqual(plan.rings.count, 8)
        XCTAssertEqual(plan.gaps + 1, 14)
        let drawn = Double(plan.rings.count - 1) * plan.step + plan.hairline
        XCTAssertLessThanOrEqual(drawn, MarkTuning.ring)
    }

    func testLargeLinesDrawEightRingsAndFourteenWaves() {
        // A 176 pt mark at @3x, and the Mac icon's mark at 1024 px.
        for (width, scale) in [(176.0, 3.0), (536.0, 1.0)] {
            let plan = HairlinePlan(width: width, displayScale: scale, lines: .large)
            let perUnit = width / MarkTuning.visible.width * scale
            XCTAssertEqual(plan.hairline * perUnit, 3, accuracy: 1e-9)
            XCTAssertEqual(plan.step * perUnit, 5, accuracy: 1e-9)
            XCTAssertEqual(plan.rings.count, 8)
            XCTAssertEqual(plan.gaps + 1, 14)
        }
    }

    func testLargeLinesKeepTheButtonsRingsAtTwoX() {
        // At @2x the lines round to the button's, so the mark keeps the button's rings.
        let large = HairlinePlan(width: 176, displayScale: 2, lines: .large)
        let button = HairlinePlan(width: 176, displayScale: 2)
        XCTAssertEqual(large.rings.count, button.rings.count)
        XCTAssertEqual(large.gaps, button.gaps)
    }

    func testSmallMarksDrawTwoRingsOfThinnerLines() {
        // A 26 px mark: one 2 px ring would fit, so the lines drop to 1 px on a 2 px step.
        let width = 26.0
        let plan = HairlinePlan(width: width, displayScale: 1, lines: .small)
        let perUnit = width / MarkTuning.visible.width
        XCTAssertEqual(plan.rings.count, 2)
        XCTAssertEqual(plan.hairline * perUnit, 1, accuracy: 1e-9)
        XCTAssertEqual(plan.step * perUnit, 2, accuracy: 1e-9)
        for width in stride(from: 26.0, through: 120, by: 2) {
            for lines in [HairlinePlan.Lines.small, .button] {
                XCTAssertGreaterThanOrEqual(HairlinePlan(width: width, displayScale: 1, lines: lines).rings.count, 2)
            }
        }
    }

    func testRingEdgesLandOnWholeDevicePixels() {
        for size in marks {
            let plan = HairlinePlan(width: size.width, displayScale: size.scale, lines: size.lines)
            let perUnit = size.width / MarkTuning.visible.width * size.scale
            let center = (MarkTuning.center - MarkTuning.visible.minX) * perUnit
            for radius in plan.rings {
                for edge in [radius - plan.hairline / 2, radius + plan.hairline / 2] {
                    let pixels = center + edge * perUnit
                    XCTAssertEqual(pixels, pixels.rounded(), accuracy: 1e-6, "\(size)")
                }
            }
        }
    }

    func testLinesAreAtLeastTwoDevicePixelsWide() {
        for size in sizes {
            let plan = HairlinePlan(width: size.width, displayScale: size.scale)
            let perUnit = size.width / MarkTuning.visible.width * size.scale
            XCTAssertGreaterThanOrEqual(plan.hairline * perUnit, 2 - 1e-9, "\(size)")
        }
    }

    func testBandLinesNeverComeCloserThanAStep() {
        let loud = RibbonMotion.currents(phase: 1.3, share: { _ in 1 }, geometry: .mark)
        guard case .ribbon(let upper, let lower) = MarkMotion.resting else { return XCTFail("a resting ribbon") }
        for size in marks {
            let plan = HairlinePlan(width: size.width, displayScale: size.scale, lines: size.lines)
            for (top, bottom) in [(upper, lower), (loud.upper, loud.lower)] {
                let lines = plan.bandLines(upper: top, lower: bottom)
                for (above, below) in zip(lines, lines.dropFirst()) {
                    for (a, b) in zip(above, below) {
                        XCTAssertGreaterThanOrEqual(b.y - a.y, plan.step - 1e-9, "\(size)")
                    }
                }
            }
        }
    }

    func testFlatLineIsEngravedAsTheFlatRibbon() {
        let flat = RibbonMotion.currents(phase: 0, share: { _ in 0 }, geometry: .mark)
        let y = MarkTuning.line[0].y
        for size in marks {
            let plan = HairlinePlan(width: size.width, displayScale: size.scale, lines: size.lines)
            let ribbon = plan.bandLines(upper: flat.upper, lower: flat.lower).map { $0[0].y }
            let rows = plan.rows(from: y - MarkTuning.lineWidth / 2, through: y + MarkTuning.lineWidth / 2)
            XCTAssertEqual(rows.count, ribbon.count, "\(size)")
            for (row, line) in zip(rows, ribbon) {
                XCTAssertEqual(row, line, accuracy: 1e-9, "\(size)")
            }
        }
    }
}
