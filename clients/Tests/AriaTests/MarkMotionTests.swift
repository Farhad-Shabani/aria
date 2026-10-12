import XCTest

@testable import Aria

final class MarkMotionTests: XCTestCase {
    private let frame = 1.0 / 60

    private func advanced(_ motion: MarkMotion, _ stage: MarkStage, for seconds: Double, level: Double = 0)
        -> MarkMotion
    {
        var motion = motion
        for _ in 0..<Int((seconds / frame).rounded()) {
            motion.advance(stage, level: level, dt: frame)
        }
        return motion
    }

    func testRestingRibbonSpansTheRingsChord() {
        let (upper, lower) = RibbonMotion.currents(phase: 0, share: { _ in MarkMotion.restShare }, geometry: .mark)
        let inner = MarkTuning.radius - MarkTuning.ring / 2
        for end in [upper.first!, upper.last!, lower.first!, lower.last!] {
            let reach = hypot(end.x - MarkTuning.center, MarkTuning.center + MarkTuning.shift - MarkTuning.center)
            XCTAssertEqual(reach, inner, accuracy: 1e-9)
        }
    }

    func testRestingRibbonIsTheLogosWave() {
        let (upper, _) = RibbonMotion.currents(phase: 0, share: { _ in MarkMotion.restShare }, geometry: .mark)
        let baseline = MarkTuning.center + MarkTuning.shift - MarkTuning.spread / 2
        let deepest = upper.map { abs($0.y - baseline) }.max() ?? 0
        XCTAssertLessThanOrEqual(deepest, MarkTuning.restHeight + 1e-9)
        XCTAssertGreaterThan(deepest, MarkTuning.restHeight * 0.8)
    }

    func testPressStartsFromTheRestingWave() {
        var motion = MarkMotion()
        motion.advance(.listening, level: 0, dt: 0)
        XCTAssertEqual(motion.ribbon.height, MarkMotion.restShare, accuracy: 1e-9)
    }

    func testSilenceSettlesNearlyFlatAndVoiceRaisesIt() {
        let quiet = advanced(MarkMotion(), .listening, for: 1)
        let loud = advanced(MarkMotion(), .listening, for: 1, level: 1)
        XCTAssertLessThan(quiet.ribbon.height, MarkMotion.restShare)
        XCTAssertGreaterThan(loud.ribbon.height, 0.9)
    }

    func testDeliveryBendsTheLineIntoTheCheck() {
        let listened = advanced(MarkMotion(), .listening, for: 0.5, level: 0.6)
        let delivered = advanced(listened, .done, for: MarkTuning.settle + MarkTuning.bend + 0.1)
        guard case .line(let points, let width) = delivered.figure else {
            return XCTFail("a delivered mark draws a line")
        }
        for (point, check) in zip(points, MarkTuning.check) {
            XCTAssertEqual(point.x, check.x, accuracy: 1e-9)
            XCTAssertEqual(point.y, check.y, accuracy: 1e-9)
        }
        XCTAssertEqual(width, MarkTuning.checkWidth, accuracy: 1e-9)
    }

    func testReturnUnbendsThenRegrowsTheLogo() {
        let delivered = advanced(MarkMotion(), .done, for: 2)
        let unbent = advanced(delivered, .rest, for: MarkTuning.unbend - frame)
        guard case .line(let points, _) = unbent.figure else {
            return XCTFail("the check unbends as a line")
        }
        // Nearly unbent: much closer to the line than to the check.
        for ((point, flat), check) in zip(zip(points, MarkTuning.line), MarkTuning.check) {
            XCTAssertLessThan(abs(point.y - flat.y), abs(check.y - flat.y) / 4)
        }
        let rested = advanced(delivered, .rest, for: MarkTuning.unbend + MarkTuning.regrow + 0.1)
        guard case .ribbon(let band) = rested.figure, case .ribbon(let logo) = MarkMotion.resting else {
            return XCTFail("the mark returns to its ribbon")
        }
        XCTAssertEqual(band.boundingRect, logo.boundingRect)
    }

    func testRestWithoutDeliveryIsTheLogo() {
        let cancelled = advanced(advanced(MarkMotion(), .listening, for: 0.5, level: 1), .rest, for: frame)
        guard case .ribbon(let band) = cancelled.figure, case .ribbon(let logo) = MarkMotion.resting else {
            return XCTFail("a mark at rest draws the logo")
        }
        XCTAssertEqual(band.boundingRect, logo.boundingRect)
    }

    func testReduceMotionShowsTheLogoOrTheCheck() {
        guard case .line(let points, _) = MarkMotion.still(.done) else {
            return XCTFail("a delivered still mark is the check")
        }
        XCTAssertEqual(points, MarkTuning.check)
        guard case .ribbon = MarkMotion.still(.listening) else {
            return XCTFail("a still mark otherwise is the logo")
        }
    }
}
