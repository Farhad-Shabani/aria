import XCTest

@testable import Aria

final class HairlinePlanTests: XCTestCase {
    private let sizes = [(width: 176.0, scale: 3.0), (width: 96.0, scale: 3.0), (width: 104.0, scale: 2.0)]

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

    func testRingEdgesLandOnWholeDevicePixels() {
        for size in sizes {
            let plan = HairlinePlan(width: size.width, displayScale: size.scale)
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
        for size in sizes {
            let plan = HairlinePlan(width: size.width, displayScale: size.scale)
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
        for size in sizes {
            let plan = HairlinePlan(width: size.width, displayScale: size.scale)
            let ribbon = plan.bandLines(upper: flat.upper, lower: flat.lower).map { $0[0].y }
            let rows = plan.rows(from: y - MarkTuning.lineWidth / 2, through: y + MarkTuning.lineWidth / 2)
            XCTAssertEqual(rows.count, ribbon.count, "\(size)")
            for (row, line) in zip(rows, ribbon) {
                XCTAssertEqual(row, line, accuracy: 1e-9, "\(size)")
            }
        }
    }
}
