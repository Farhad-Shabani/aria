//! Frames to samples and back.
//!
//! A segmentation engine answers on its own grid: one activity per
//! `frame_shift` samples, each speaking for a `frame_size` window around
//! itself. Every question the track asks — where a frame sits, which samples
//! it owns, how long a run of them lasts — is one of these conversions.

use std::ops::Range;

use crate::segmentation::ChunkShape;

use crate::audio::CANONICAL_SAMPLE_RATE;

/// The frame grid one engine answers on.
#[derive(Debug, Clone, Copy)]
pub(super) struct FrameGrid {
    shift: usize,
    size: usize,
}

impl From<&ChunkShape> for FrameGrid {
    fn from(shape: &ChunkShape) -> Self {
        Self {
            shift: shape.frame_shift,
            size: shape.frame_size,
        }
    }
}

impl FrameGrid {
    /// The frame covering a stream position, to the nearest.
    pub(super) fn at(&self, position: u64) -> usize {
        ((position + self.shift as u64 / 2) / self.shift as u64) as usize
    }

    /// Where a frame begins, in stream positions.
    pub(super) fn start(&self, frame: usize) -> u64 {
        frame as u64 * self.shift as u64
    }

    /// The samples one frame owns inside a chunk that starts at its frame 0.
    ///
    /// Frames overlap — `size` is wider than `shift` — so a frame's own
    /// audio is the middle of its window, not its leading edge.
    pub(super) fn window(&self, frame: usize) -> Range<usize> {
        let lead = self.size.saturating_sub(self.shift) / 2;
        let from = frame * self.shift + lead;
        from..from + self.shift
    }

    /// How many frames span `seconds`, rounded down.
    pub(super) fn frames(&self, seconds: f64) -> usize {
        (seconds * f64::from(CANONICAL_SAMPLE_RATE) / self.shift as f64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pyannote's own grid: frames step 270 samples and answer for 991.
    fn pyannote() -> FrameGrid {
        FrameGrid {
            shift: 270,
            size: 991,
        }
    }

    #[test]
    fn a_position_lands_on_its_nearest_frame() {
        let grid = pyannote();
        assert_eq!(grid.at(0), 0);
        assert_eq!(grid.at(134), 0);
        assert_eq!(grid.at(135), 1);
        assert_eq!(grid.at(270), 1);
    }

    #[test]
    fn a_frames_window_is_the_middle_of_what_it_answers_for() {
        // 991 wide against a 270 shift leaves 360 of context on each side.
        let heard = pyannote().window(0);
        assert_eq!(heard, 360..630);
        assert_eq!(heard.len(), 270, "a frame owns one shift of audio");
    }

    #[test]
    fn windows_of_neighbouring_frames_touch_and_do_not_overlap() {
        let grid = pyannote();
        assert_eq!(grid.window(4).end, grid.window(5).start);
    }

    #[test]
    fn a_run_of_frames_converts_to_seconds_and_back() {
        let grid = pyannote();
        // 0.3 s is pyannote's shortest turn: 17 frames at this shift.
        assert_eq!(grid.frames(0.3), 17);
        assert_eq!(grid.start(17), 17 * 270);
    }
}
