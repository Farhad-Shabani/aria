use std::ops::Range;

use crate::backend::BackendError;
use crate::segmentation::{Activity, ChunkShape, SegmentationError, Segmenter};

pub(super) enum Evidence {
    Reassess(Range<u64>),
    Replaced {
        anchor: Range<u64>,
        candidate: Range<u64>,
    },
    Unresolved,
}

pub(super) struct Continuity<S> {
    segmenter: S,
    shape: ChunkShape,
    from: u64,
    audio: Vec<f32>,
}

impl<S: Segmenter> Continuity<S> {
    pub(super) fn new(segmenter: S) -> Result<Self, SegmentationError> {
        let shape = segmenter.shape();
        if shape.frame_shift == 0
            || shape.frame_size < shape.frame_shift
            || shape.samples < shape.frame_size
            || !(1..=Activity::MAX_SPEAKERS).contains(&shape.speakers)
        {
            return Err(invalid("invalid segmentation geometry"));
        }
        Ok(Self {
            segmenter,
            shape,
            from: 0,
            audio: Vec::new(),
        })
    }

    pub(super) fn restart(&mut self, from: u64) {
        self.from = from;
        self.audio.clear();
    }

    pub(super) fn push(&mut self, from: u64, audio: &[f32]) -> Result<(), SegmentationError> {
        if self.from.checked_add(self.audio.len() as u64) != Some(from)
            || from.checked_add(audio.len() as u64).is_none()
            || audio.iter().any(|sample| !sample.is_finite())
        {
            return Err(invalid("discontinuous or invalid voice audio"));
        }
        self.audio.extend_from_slice(audio);
        let excess = self.audio.len().saturating_sub(self.shape.samples);
        self.audio.drain(..excess);
        self.from = self
            .from
            .checked_add(excess as u64)
            .ok_or_else(|| invalid("voice audio position overflow"))?;
        Ok(())
    }

    pub(super) fn audio(&self, span: &Range<u64>) -> Result<&[f32], SegmentationError> {
        let start = span.start.checked_sub(self.from);
        let end = span.end.checked_sub(self.from);
        match (start, end) {
            (Some(start), Some(end)) if start <= end && end <= self.audio.len() as u64 => {
                Ok(&self.audio[start as usize..end as usize])
            }
            _ => Err(invalid("voice evidence lies outside retained audio")),
        }
    }

    pub(super) fn verify(
        &mut self,
        anchor: Option<&Range<u64>>,
        candidate: &Range<u64>,
        fresh_samples: usize,
    ) -> Result<Evidence, SegmentationError> {
        if candidate.start >= candidate.end
            || anchor.is_some_and(|anchor| anchor.start >= anchor.end)
        {
            return Err(invalid("empty or reversed voice evidence"));
        }
        let observed = self.audio(candidate)?;
        if observed.is_empty() || anchor.is_some_and(|anchor| anchor.end > candidate.end) {
            return Err(invalid("voice evidence is out of order"));
        }
        let through = (candidate.end - self.from) as usize;
        let mut input = self.audio[..through].to_vec();
        input.resize(self.shape.samples, 0.0);
        let frames = self.segmenter.segment(&input)?;
        let expected = (self.shape.samples - self.shape.frame_size) / self.shape.frame_shift + 1;
        if frames.len() != expected
            || frames.iter().any(|frame| {
                (self.shape.speakers..Activity::MAX_SPEAKERS).any(|local| frame.speaks(local))
            })
        {
            return Err(invalid("segmentation returned an invalid activity grid"));
        }
        let snapshot = Snapshot {
            from: self.from,
            through: candidate.end,
            shape: self.shape,
            frames: &frames,
        };
        let Some(current) = snapshot.single(candidate) else {
            return Ok(Evidence::Unresolved);
        };
        let retained_anchor = anchor.filter(|anchor| self.audio(anchor).is_ok());
        let Some(anchor) = retained_anchor else {
            return Ok(snapshot
                .fresh(candidate)
                .filter(|range| range.end - range.start >= fresh_samples as u64)
                .map_or(Evidence::Unresolved, Evidence::Reassess));
        };
        let reassess = anchor.start..candidate.end;
        if snapshot.single(&reassess).is_some() {
            return Ok(Evidence::Reassess(reassess));
        }
        let Some((previous, exclusive)) = snapshot.latest_exclusive(anchor) else {
            return Ok(Evidence::Unresolved);
        };
        if previous == current || exclusive.end > candidate.start {
            return Ok(Evidence::Unresolved);
        }
        Ok(Evidence::Replaced {
            anchor: exclusive,
            candidate: candidate.clone(),
        })
    }
}

struct Snapshot<'a> {
    from: u64,
    through: u64,
    shape: ChunkShape,
    frames: &'a [Activity],
}

impl Snapshot<'_> {
    fn within(&self, span: &Range<u64>) -> impl Iterator<Item = (Range<u64>, Activity)> + '_ {
        let span = span.clone();
        self.frames
            .iter()
            .copied()
            .enumerate()
            .filter_map(move |(index, activity)| {
                let start = self.from + (index * self.shape.frame_shift) as u64;
                let end = start + self.shape.frame_size as u64;
                (start >= span.start && end <= span.end && end <= self.through)
                    .then_some((start..end, activity))
            })
    }

    fn single(&self, span: &Range<u64>) -> Option<usize> {
        let mut heard = None;
        for (_, activity) in self.within(span) {
            if activity.voices() == 0 {
                continue;
            }
            if activity.voices() != 1 {
                return None;
            }
            let local = (0..self.shape.speakers).find(|local| activity.speaks(*local))?;
            if heard.is_some_and(|heard| heard != local) {
                return None;
            }
            heard = Some(local);
        }
        heard
    }

    fn fresh(&self, candidate: &Range<u64>) -> Option<Range<u64>> {
        let current = self.single(candidate)?;
        let mut start = candidate.start;
        let frames: Vec<_> = self.within(&(self.from..candidate.end)).collect();
        for (support, activity) in frames.into_iter().rev() {
            if activity.voices() > 0 && (activity.voices() != 1 || !activity.speaks(current)) {
                break;
            }
            start = support.start;
        }
        Some(start..candidate.end)
    }

    fn latest_exclusive(&self, span: &Range<u64>) -> Option<(usize, Range<u64>)> {
        let mut latest: Option<(usize, Range<u64>)> = None;
        let mut interrupted = false;
        for (support, activity) in self.within(span) {
            if activity.voices() == 0 {
                continue;
            }
            if activity.voices() != 1 {
                interrupted = true;
                continue;
            }
            let local = (0..self.shape.speakers).find(|local| activity.speaks(*local))?;
            match &mut latest {
                Some((previous, run)) if *previous == local && !interrupted => {
                    run.end = support.end;
                }
                _ => latest = Some((local, support)),
            }
            interrupted = false;
        }
        latest
    }
}

fn invalid(reason: &str) -> SegmentationError {
    SegmentationError::Inference {
        backend: "speaker continuity",
        source: BackendError::Unusable(reason.into()),
    }
}

#[cfg(any(test, feature = "test-support"))]
pub struct LevelSegmenter;

#[cfg(any(test, feature = "test-support"))]
impl Segmenter for LevelSegmenter {
    fn shape(&self) -> ChunkShape {
        ChunkShape {
            samples: 160_000,
            frame_shift: 160,
            frame_size: 160,
            speakers: 2,
        }
    }

    fn segment(&mut self, audio: &[f32]) -> Result<Vec<Activity>, SegmentationError> {
        Ok(audio
            .chunks_exact(160)
            .map(|frame| {
                let mut active = Activity::SILENT;
                if frame.iter().any(|sample| *sample >= 0.4) {
                    active = active.speaking(0);
                }
                if frame.iter().any(|sample| (0.1..0.4).contains(sample)) {
                    active = active.speaking(1);
                }
                active
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        shape: ChunkShape,
        frames: Vec<Activity>,
    }

    impl Segmenter for Fixed {
        fn shape(&self) -> ChunkShape {
            self.shape
        }
        fn segment(&mut self, input: &[f32]) -> Result<Vec<Activity>, SegmentationError> {
            assert_eq!(input.len(), self.shape.samples);
            Ok(self.frames.clone())
        }
    }

    fn fixture(voices: &[&[usize]]) -> Continuity<Fixed> {
        let shape = ChunkShape {
            samples: voices.len() * 10,
            frame_shift: 10,
            frame_size: 10,
            speakers: 3,
        };
        let frames = voices
            .iter()
            .map(|voices| {
                voices
                    .iter()
                    .fold(Activity::SILENT, |active, local| active.speaking(*local))
            })
            .collect();
        let mut continuity = Continuity::new(Fixed { shape, frames }).unwrap();
        continuity.push(0, &vec![0.2; shape.samples]).unwrap();
        continuity
    }

    #[test]
    fn continued_identity_uses_all_contiguous_evidence() {
        let mut continuity = fixture(&[&[0], &[0], &[], &[0], &[0], &[0]]);
        assert!(
            matches!(continuity.verify(Some(&(0..20)), &(40..60), 30).unwrap(), Evidence::Reassess(range) if range == (0..60))
        );
    }

    #[test]
    fn equal_endpoint_voices_do_not_hide_an_intervening_voice() {
        for middle in [&[1][..], &[0, 1][..]] {
            let mut continuity = fixture(&[&[0], &[0], middle, &[0], &[0], &[0]]);
            assert!(matches!(
                continuity.verify(Some(&(0..20)), &(40..60), 30).unwrap(),
                Evidence::Unresolved
            ));
        }
    }

    #[test]
    fn local_permutations_preserve_a_real_replacement() {
        for (previous, current) in [(0, 1), (1, 0), (2, 1), (1, 2)] {
            let mut continuity = fixture(&[
                &[previous],
                &[previous],
                &[],
                &[current],
                &[current],
                &[current],
            ]);
            assert!(
                matches!(continuity.verify(Some(&(0..20)), &(40..60), 30).unwrap(), Evidence::Replaced { anchor, candidate } if anchor == (0..20) && candidate == (40..60))
            );
        }
    }

    #[test]
    fn mixed_anchor_uses_its_latest_exclusive_contiguous_run() {
        let mut continuity = fixture(&[&[0, 1], &[0], &[0], &[1], &[1], &[1]]);
        assert!(
            matches!(continuity.verify(Some(&(0..30)), &(40..60), 30).unwrap(), Evidence::Replaced { anchor, .. } if anchor == (10..30))
        );
        let mut continuity = fixture(&[&[0, 1], &[0, 1], &[0, 1], &[1], &[1], &[1]]);
        assert!(matches!(
            continuity.verify(Some(&(0..30)), &(40..60), 30).unwrap(),
            Evidence::Unresolved
        ));
    }

    #[test]
    fn an_overlap_tail_preserves_the_latest_completed_exclusive_anchor_run() {
        let mut continuity = fixture(&[&[0], &[0], &[0, 1], &[1], &[1], &[1]]);
        assert!(
            matches!(continuity.verify(Some(&(0..30)), &(40..60), 30).unwrap(), Evidence::Replaced { anchor, candidate } if anchor == (0..20) && candidate == (40..60))
        );
    }

    #[test]
    fn expired_anchor_requires_fresh_homogeneous_evidence() {
        let mut continuity = fixture(&[&[0], &[0], &[0], &[0], &[0], &[0]]);
        continuity.restart(100);
        continuity.push(100, &[0.2; 60]).unwrap();
        assert!(
            matches!(continuity.verify(Some(&(0..20)), &(140..160), 60).unwrap(), Evidence::Reassess(range) if range == (100..160))
        );
        assert!(matches!(
            continuity.verify(Some(&(0..20)), &(140..160), 61).unwrap(),
            Evidence::Unresolved
        ));
    }

    #[test]
    fn padding_cannot_supply_candidate_activity() {
        let mut continuity = fixture(&[&[0], &[0], &[1], &[1], &[1], &[1]]);
        assert!(continuity.verify(Some(&(0..20)), &(50..70), 30).is_err());
        assert!(matches!(
            continuity.verify(Some(&(0..20)), &(20..25), 30).unwrap(),
            Evidence::Unresolved
        ));
    }

    #[test]
    fn history_is_bounded_and_uses_absolute_origins() {
        let mut continuity = fixture(&[&[0], &[0], &[0], &[0], &[0], &[0]]);
        continuity.restart(100);
        continuity.push(100, &[0.2; 40]).unwrap();
        continuity.push(140, &[0.2; 40]).unwrap();
        assert_eq!(continuity.audio.len(), 60);
        assert_eq!(continuity.from, 120);
        assert!(continuity.audio(&(100..140)).is_err());
    }

    #[test]
    fn malformed_geometry_grid_and_audio_are_errors() {
        let shape = ChunkShape {
            samples: 60,
            frame_shift: 0,
            frame_size: 10,
            speakers: 3,
        };
        assert!(
            Continuity::new(Fixed {
                shape,
                frames: vec![]
            })
            .is_err()
        );
        let mut continuity = fixture(&[&[0], &[0], &[0], &[0], &[0], &[0]]);
        assert!(continuity.push(61, &[0.2]).is_err());
        assert!(continuity.push(60, &[f32::NAN]).is_err());
        let shape = ChunkShape {
            samples: 60,
            frame_shift: 10,
            frame_size: 10,
            speakers: 3,
        };
        let mut malformed = Continuity::new(Fixed {
            shape,
            frames: Vec::new(),
        })
        .unwrap();
        malformed.push(0, &[0.2; 60]).unwrap();
        assert!(malformed.verify(Some(&(0..20)), &(40..60), 30).is_err());
    }
}
