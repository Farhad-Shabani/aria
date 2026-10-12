import XCTest

@testable import Aria

final class RibbonTests: XCTestCase {
    private let frame = 1.0 / 60

    private func listening(to input: Double, for seconds: Double, from start: RibbonMotion = RibbonMotion())
        -> RibbonMotion
    {
        var motion = start
        var elapsed = 0.0
        while elapsed < seconds {
            motion.listen(to: input, for: frame)
            elapsed += frame
        }
        return motion
    }

    func test_voiceRisesFastAndFallsSlower() {
        let risen = listening(to: 1, for: 0.05)
        XCTAssertGreaterThan(risen.level, 0.9)

        let settled = listening(to: 1, for: 1)
        let falling = listening(to: 0, for: 0.05, from: settled)
        XCTAssertGreaterThan(falling.level, 0.5)
    }

    func test_silenceBreathesWithinItsBand() {
        var motion = listening(to: 0, for: 2)
        var lowest = Double.infinity
        var highest = -Double.infinity
        for _ in 0..<Int(RibbonTuning.breathPeriod / frame) {
            motion.listen(to: 0, for: frame)
            lowest = min(lowest, motion.height)
            highest = max(highest, motion.height)
        }
        XCTAssertGreaterThanOrEqual(lowest, RibbonTuning.breathFloor - 0.01)
        XCTAssertLessThanOrEqual(highest, RibbonTuning.breathFloor + RibbonTuning.breathSwing + 0.01)
        XCTAssertGreaterThan(highest - lowest, RibbonTuning.breathSwing / 2)
    }

    func test_louderFlowsFasterAndAlwaysLeftToRight() {
        let quiet = listening(to: 0, for: 1)
        let loud = listening(to: 1, for: 1)
        XCTAssertLessThan(quiet.phase, 0)
        XCTAssertLessThan(loud.phase, quiet.phase)
    }

    func test_sweepIsFlatBehindTheEdgeAndWavyAhead() {
        let halfway = RibbonTuning.sweepPeriod / 2
        XCTAssertEqual(RibbonMotion.sweepShare(0.1, elapsed: halfway), 0, accuracy: 1e-9)
        XCTAssertEqual(RibbonMotion.sweepShare(0.9, elapsed: halfway), RibbonTuning.sweepHeight, accuracy: 1e-9)
    }

    func test_bothCurrentsShareOneHeight() {
        let flat = RibbonMotion.currents(phase: 0, share: { _ in 0 })
        let upperLine = RibbonTuning.middle - RibbonTuning.spread / 2
        let lowerLine = RibbonTuning.middle + RibbonTuning.spread / 2
        XCTAssertTrue(flat.upper.allSatisfy { abs($0.y - upperLine) < 1e-9 })
        XCTAssertTrue(flat.lower.allSatisfy { abs($0.y - lowerLine) < 1e-9 })

        // Over a full turn of phase, each current reaches the same peak.
        var upperPeak = 0.0
        var lowerPeak = 0.0
        let point = RibbonTuning.points / 4
        for turn in 0..<360 {
            let phase = Double(turn) * .pi / 180
            let currents = RibbonMotion.currents(phase: phase, share: { _ in 1 })
            upperPeak = max(upperPeak, abs(currents.upper[point].y - upperLine))
            lowerPeak = max(lowerPeak, abs(currents.lower[point].y - lowerLine))
        }
        XCTAssertEqual(upperPeak, lowerPeak, accuracy: 1e-3)
    }

    func test_transcribingTakesOverWithoutAJump() {
        var motion = listening(to: 0.9, for: 1)
        let spoken = motion.share(at: 0.5)
        let phase = motion.phase

        motion.transcribe(for: 0)
        for p in stride(from: 0.0, through: 1.0, by: 0.1) {
            XCTAssertEqual(motion.share(at: p), spoken, accuracy: 1e-9)
        }

        motion.transcribe(for: frame)
        XCTAssertLessThan(abs(motion.share(at: 0.5) - spoken), 0.05)
        let sweepSpeed = RibbonTuning.baseSpeed + RibbonTuning.energySpeed * RibbonTuning.sweepEnergy
        XCTAssertEqual(motion.phase, phase - sweepSpeed * frame, accuracy: 1e-9)

        var elapsed = frame
        while elapsed < RibbonTuning.handOff {
            motion.transcribe(for: frame)
            elapsed += frame
        }
        let swept = motion.sweptFor ?? 0
        for p in stride(from: 0.0, through: 1.0, by: 0.1) {
            XCTAssertEqual(motion.share(at: p), RibbonMotion.sweepShare(p, elapsed: swept), accuracy: 1e-9)
        }
    }

    func test_sweepNeverSnapsBetweenLaps() {
        // Step across several laps; no frame may move any point far.
        var elapsed = 0.0
        while elapsed < RibbonTuning.sweepPeriod * 3 {
            for p in stride(from: 0.0, through: 1.0, by: 0.05) {
                let now = RibbonMotion.sweepShare(p, elapsed: elapsed)
                let next = RibbonMotion.sweepShare(p, elapsed: elapsed + frame)
                XCTAssertLessThan(abs(next - now), 0.15, "p \(p) at \(elapsed)s")
            }
            elapsed += frame
        }
    }

    @MainActor
    func test_aNewPressStartsFromSilence() {
        let model = RibbonModel()
        let first = Date(timeIntervalSince1970: 1_000)
        var date = first
        for _ in 0..<60 {
            _ = model.outline(at: date, state: .recording(since: first), level: 1)
            date += frame
        }
        XCTAssertGreaterThan(model.motion.height, 0.8)

        _ = model.outline(at: date, state: .recording(since: date), level: 0)
        XCTAssertEqual(model.motion, RibbonMotion())
    }

    func test_aLongStallMovesOnlyOneCappedFrame() {
        var motion = RibbonMotion()
        motion.listen(to: 1, for: 5)
        XCTAssertEqual(motion.clock, RibbonTuning.maxFrameGap, accuracy: 1e-9)
    }
}
