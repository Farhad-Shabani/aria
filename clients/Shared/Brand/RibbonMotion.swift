import SwiftUI

/// Every number that shapes and moves the ribbon. The shape constants draw
/// the Mac pill's small slot; the motion constants also move the logo on the
/// iPhone and Watch (`MarkMotion`), so retuning them changes all three.
enum RibbonTuning {
    // Shape: the logo's two currents, drawn small.

    /// Slot the ribbon draws in, in points.
    static let width: CGFloat = 26
    static let height: CGFloat = 12
    /// Baseline between the two currents, in points from the top.
    static let middle = 6.0
    /// Gap between the currents' baselines, in points. The second current's
    /// `phaseGap` makes the drawn gap swing around it; too large a
    /// `maxHeight` lets the currents cross.
    static let spread = 3.2
    /// Wave height at full share, in points.
    static let maxHeight = 3.4
    /// Waves across the slot.
    static let periods = 1.5
    /// How far the second current runs behind the first, in radians.
    static let phaseGap = 0.9
    /// Segments per current; more is smoother and costs more.
    static let points = 28
    /// Outline stroke, in points. Softens the ribbon's edge.
    static let lineWidth: CGFloat = 1.6

    // Voice: how the ribbon follows the microphone while recording.

    /// Seconds for the level to rise toward a louder voice.
    static let attack = 0.015
    /// Seconds for the level to fall back toward a quieter one.
    static let release = 0.12
    /// Spring pull toward the level. Higher is snappier.
    static let stiffness = 550.0
    /// Spring damping. Below 1 overshoots a little; 1 never does.
    static let dampingRatio = 0.72
    /// Most of the full height the spring may draw, overshoot included.
    static let heightCap = 1.08
    /// Share the ribbon keeps in silence, so it never lies dead flat.
    static let breathFloor = 0.06
    /// Extra share the silent ribbon breathes up to.
    static let breathSwing = 0.06
    /// Seconds per breath in silence.
    static let breathPeriod = 4.0
    /// Flow speed at rest, in radians per second.
    static let baseSpeed = 3.2
    /// Extra flow speed at full height, in radians per second.
    static let energySpeed = 3.2
    /// Seconds per integration step. Smaller is steadier.
    static let simStep = 1.0 / 240
    /// Longest frame gap integrated, in seconds, so a stall never leaps.
    static let maxFrameGap = 0.1

    // Sweep: transcribing flattens the wave into a line, left to right.

    /// Seconds for the flat edge to cross the ribbon once.
    static let sweepPeriod = 1.05
    /// Half-width of the soft edge, as a share of the ribbon's length.
    static let sweepSoftness = 0.18
    /// Share of the full height still waving ahead of the edge.
    static let sweepHeight = 0.8
    /// Share of the energy speed the ribbon keeps while transcribing.
    static let sweepEnergy = 0.6
    /// Seconds to blend from the recording's last height into the sweep.
    static let handOff = 0.35
    /// Share of each lap the wave takes to grow back after the line, so a
    /// new lap never snaps from flat to full.
    static let sweepRegrow = 0.25

    // Reduce Motion: the ribbon holds still.

    /// Share of the full height the still ribbon is drawn at.
    static let stillShare = 0.5
}

/// Where a ribbon is drawn: the pill's small slot, or the logo's ring.
/// The motion is the same in both; only the drawing scales.
struct RibbonGeometry: Equatable, Sendable {
    /// Left end and length of the ribbon, in points.
    var left: Double
    var width: Double
    /// Baseline between the two currents, in points from the top.
    var middle: Double
    /// Gap between the currents' baselines, in points.
    var spread: Double
    /// Wave height at full share, in points.
    var maxHeight: Double
    var points: Int

    static let pill = RibbonGeometry(
        left: 0,
        width: Double(RibbonTuning.width),
        middle: RibbonTuning.middle,
        spread: RibbonTuning.spread,
        maxHeight: RibbonTuning.maxHeight,
        points: RibbonTuning.points
    )
}

/// The ribbon's motion, as plain numbers: no view, no clock, no audio.
struct RibbonMotion: Equatable, Sendable {
    /// The voice after the attack and release follower, 0 to 1.
    private(set) var level = 0.0
    /// The spring's height, as a share of `maxHeight`.
    private(set) var height = 0.0
    private(set) var velocity = 0.0
    /// Where the waves are along their travel, in radians.
    private(set) var phase = 0.0
    /// Seconds since the press began, for the breath.
    private(set) var clock = 0.0
    /// The height transcribing starts from.
    private(set) var handOffHeight = 0.0
    /// Seconds spent transcribing, or nil while recording.
    private(set) var sweptFor: Double?

    /// A ribbon still at `height`, a share of `maxHeight`. The pill starts
    /// flat; the logo starts from its resting wave.
    init(height: Double = 0) {
        self.height = height
    }

    /// Follows the microphone for `dt` seconds.
    mutating func listen(to input: Double, for dt: Double) {
        sweptFor = nil
        var remaining = min(max(dt, 0), RibbonTuning.maxFrameGap)
        let damping = 2 * RibbonTuning.dampingRatio * RibbonTuning.stiffness.squareRoot()
        while remaining > 0 {
            let step = min(RibbonTuning.simStep, remaining)
            let rising = input > level
            let settle = rising ? RibbonTuning.attack : RibbonTuning.release
            level += (input - level) * (1 - exp(-step / settle))
            let goal = max(level, Self.breath(at: clock))
            velocity += (RibbonTuning.stiffness * (goal - height) - damping * velocity) * step
            height += velocity * step
            // Decreasing, so the crests travel left to right.
            phase -= (RibbonTuning.baseSpeed + RibbonTuning.energySpeed * clamp(height, 0, 1)) * step
            clock += step
            remaining -= step
        }
    }

    /// Runs the transcribing sweep for `dt` seconds, picking up where recording left off.
    mutating func transcribe(for dt: Double) {
        let elapsed = min(max(dt, 0), RibbonTuning.maxFrameGap)
        if sweptFor == nil {
            handOffHeight = clamp(height, 0, RibbonTuning.heightCap)
            sweptFor = 0
        }
        sweptFor = (sweptFor ?? 0) + elapsed
        phase -= (RibbonTuning.baseSpeed + RibbonTuning.energySpeed * RibbonTuning.sweepEnergy) * elapsed
    }

    /// The wave's height at `p` along the ribbon, as a share of `maxHeight`:
    /// 0 to `heightCap`, since the spring may overshoot.
    func share(at p: Double) -> Double {
        guard let sweptFor else { return clamp(height, 0, RibbonTuning.heightCap) }
        let blend = smoothstep(0, RibbonTuning.handOff, sweptFor)
        return mix(handOffHeight, Self.sweepShare(p, elapsed: sweptFor), blend)
    }

    /// The silent ribbon's share at `t` seconds into a press.
    static func breath(at t: Double) -> Double {
        let wave = 0.5 + 0.5 * sin(2 * .pi * t / RibbonTuning.breathPeriod)
        return RibbonTuning.breathFloor + RibbonTuning.breathSwing * wave
    }

    /// The sweep's share at `p`: flat behind the moving edge, waving ahead of
    /// it. Each lap ends flat, so the wave grows back before the edge returns.
    static func sweepShare(_ p: Double, elapsed: Double) -> Double {
        let softness = RibbonTuning.sweepSoftness
        let lap = (elapsed / RibbonTuning.sweepPeriod).truncatingRemainder(dividingBy: 1)
        let edge = lap * (1 + 2 * softness) - softness
        let regrown = smoothstep(0, RibbonTuning.sweepRegrow, lap)
        return RibbonTuning.sweepHeight * regrown * smoothstep(edge - softness, edge + softness, p)
    }

    /// Both currents' points. They read one share, so they rise and fall together.
    static func currents(
        phase: Double,
        share: (Double) -> Double,
        geometry: RibbonGeometry = .pill
    ) -> (upper: [CGPoint], lower: [CGPoint]) {
        func current(_ index: Int) -> [CGPoint] {
            let side = index == 0 ? -1.0 : 1.0
            let baseline = geometry.middle + side * geometry.spread / 2
            return (0...geometry.points).map { step in
                let p = Double(step) / Double(geometry.points)
                let wave = sin(2 * .pi * RibbonTuning.periods * p + phase + Double(index) * RibbonTuning.phaseGap)
                let y = baseline - geometry.maxHeight * share(p) * sin(.pi * p) * wave
                return CGPoint(x: geometry.left + geometry.width * p, y: y)
            }
        }
        return (current(0), current(1))
    }

    var outline: Path {
        Self.outline(phase: phase, share: share(at:))
    }

    /// The ribbon as one closed band between its two currents.
    static func outline(phase: Double, share: (Double) -> Double, geometry: RibbonGeometry = .pill) -> Path {
        let (upper, lower) = currents(phase: phase, share: share, geometry: geometry)
        return Path { path in
            path.addLines(upper + lower.reversed())
            path.closeSubpath()
        }
    }
}

func clamp(_ value: Double, _ low: Double, _ high: Double) -> Double {
    min(max(value, low), high)
}

func smoothstep(_ edge0: Double, _ edge1: Double, _ x: Double) -> Double {
    let k = clamp((x - edge0) / (edge1 - edge0), 0, 1)
    return k * k * (3 - 2 * k)
}

func mix(_ a: Double, _ b: Double, _ k: Double) -> Double {
    a + (b - a) * k
}
