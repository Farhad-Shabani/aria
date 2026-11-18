use crate::segmentation::Activity;

/// Frames in which each listed local speaks.
pub(super) fn frames(pattern: &[&[usize]]) -> Vec<Activity> {
    pattern
        .iter()
        .map(|speaking| {
            speaking
                .iter()
                .fold(Activity::SILENT, |active, &voice| active.speaking(voice))
        })
        .collect()
}
