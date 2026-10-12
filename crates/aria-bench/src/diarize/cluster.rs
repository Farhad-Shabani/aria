//! Average-linkage agglomerative clustering over voiceprints.
//!
//! `group` repeatedly merges the closest pair across all voiceprints, costing O(n³).
//! `group_online` assigns incoming voiceprints and re-merges the resulting groups.
//! Both compare groups by mean pairwise cosine similarity.

/// When to stop joining groups.
pub(crate) enum Stop {
    /// Exactly this many voices remain.
    Count(usize),
    /// Nothing left to join is this alike.
    Similarity(f32),
}

/// Groups voiceprints, joining the two most alike at every step. A slot
/// nobody could embed stays `None` — it joins no group. Group numbers run in
/// order of first appearance, so the first voice heard is group 0.
pub(crate) fn group(voices: &[Option<Vec<f32>>], stop: Stop) -> Vec<Option<usize>> {
    let mut groups: Vec<Group> = voices
        .iter()
        .enumerate()
        .filter_map(|(index, voice)| voice.as_ref().map(|values| Group::opening(values, index)))
        .collect();

    let (floor, least) = match stop {
        Stop::Count(wanted) => (wanted.max(1), f32::NEG_INFINITY),
        Stop::Similarity(least) => (1, least),
    };
    while groups.len() > floor && merge_closest(&mut groups, least) {}
    numbered(groups, voices.len())
}

/// Groups voiceprints in arrival order: each joins the voice it is most alike
/// above `least`, or opens a new one.
///
/// Each addition changes a group's mean, so groups are re-merged after placement.
pub(crate) fn group_online(voices: &[Option<Vec<f32>>], least: f32) -> Vec<Option<usize>> {
    let mut groups: Vec<Group> = Vec::new();
    for (index, voice) in voices.iter().enumerate() {
        let Some(values) = voice else { continue };
        let nearest = groups
            .iter_mut()
            .map(|group| {
                let similarity = alike((&group.sum, group.count), (values, 1));
                (group, similarity)
            })
            .reduce(|best, candidate| {
                if candidate.1 > best.1 {
                    candidate
                } else {
                    best
                }
            });
        match nearest {
            Some((group, similarity)) if similarity >= least => group.take(values, index),
            _ => groups.push(Group::opening(values, index)),
        }
        while merge_closest(&mut groups, least) {}
    }
    numbered(groups, voices.len())
}

/// One voice: the sum of its voiceprints, how many it holds, and which slots
/// of the recording it answers for.
struct Group {
    sum: Vec<f32>,
    count: usize,
    members: Vec<usize>,
}

impl Group {
    fn opening(embedding: &[f32], member: usize) -> Self {
        Self {
            sum: embedding.to_vec(),
            count: 1,
            members: Vec::from([member]),
        }
    }

    fn take(&mut self, embedding: &[f32], member: usize) {
        for (sum, value) in self.sum.iter_mut().zip(embedding) {
            *sum += value;
        }
        self.count += 1;
        self.members.push(member);
    }

    fn absorb(&mut self, other: Self) {
        for (sum, value) in self.sum.iter_mut().zip(&other.sum) {
            *sum += value;
        }
        self.count += other.count;
        self.members.extend(other.members);
    }
}

/// Average cosine between every member of one group and every member of
/// another. With unit vectors that is the dot of the sums over the product of
/// the sizes — one multiply instead of a nested loop.
fn alike(a: (&[f32], usize), b: (&[f32], usize)) -> f32 {
    let dot: f32 = a.0.iter().zip(b.0).map(|(x, y)| x * y).sum();
    dot / (a.1 * b.1) as f32
}

/// Joins the two most alike groups when they clear `least`. False when
/// nothing is left worth joining.
fn merge_closest(groups: &mut Vec<Group>, least: f32) -> bool {
    let mut best: Option<(usize, usize, f32)> = None;
    for a in 0..groups.len() {
        for b in a + 1..groups.len() {
            let similarity = alike(
                (&groups[a].sum, groups[a].count),
                (&groups[b].sum, groups[b].count),
            );
            if best.is_none_or(|(_, _, top)| similarity > top) {
                best = Some((a, b, similarity));
            }
        }
    }
    match best {
        Some((a, b, similarity)) if similarity >= least => {
            let absorbed = groups.swap_remove(b);
            groups[a].absorb(absorbed);
            true
        }
        _ => false,
    }
}

/// Numbers the groups by first appearance, so the first voice heard is 0.
fn numbered(mut groups: Vec<Group>, slots: usize) -> Vec<Option<usize>> {
    groups.sort_by_key(|group| group.members.iter().min().copied());
    let mut numbers = vec![None; slots];
    for (number, group) in groups.iter().enumerate() {
        for &member in &group.members {
            numbers[member] = Some(number);
        }
    }
    numbers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(values: [f32; 2]) -> Option<Vec<f32>> {
        Some(Vec::from(values))
    }

    fn two_voices() -> Vec<Option<Vec<f32>>> {
        Vec::from([
            voice([1.0, 0.0]),
            voice([0.99, 0.14]),
            voice([0.0, 1.0]),
            voice([0.14, 0.99]),
        ])
    }

    #[test]
    fn a_count_joins_until_that_many_remain() {
        let numbers = group(&two_voices(), Stop::Count(2));
        assert_eq!(numbers, Vec::from([Some(0), Some(0), Some(1), Some(1)]));
    }

    #[test]
    fn a_similarity_leaves_unlike_voices_apart() {
        let voices = Vec::from([voice([1.0, 0.0]), voice([0.0, 1.0])]);
        assert_eq!(
            group(&voices, Stop::Similarity(0.5)),
            Vec::from([Some(0), Some(1)])
        );
    }

    #[test]
    fn a_voiceprint_nobody_could_take_joins_nothing() {
        let voices = Vec::from([voice([1.0, 0.0]), None, voice([0.99, 0.14])]);
        assert_eq!(
            group(&voices, Stop::Count(1)),
            Vec::from([Some(0), None, Some(0)])
        );
    }

    #[test]
    fn arriving_one_at_a_time_finds_the_same_two_voices() {
        assert_eq!(
            group_online(&two_voices(), 0.5),
            Vec::from([Some(0), Some(0), Some(1), Some(1)])
        );
    }

    #[test]
    fn a_voice_that_matches_nothing_opens_its_own() {
        let voices = Vec::from([voice([1.0, 0.0]), voice([0.0, 1.0]), voice([0.99, 0.14])]);
        assert_eq!(
            group_online(&voices, 0.5),
            Vec::from([Some(0), Some(1), Some(0)])
        );
    }

    #[test]
    fn two_voices_that_drift_together_are_merged_after_the_fact() {
        // Unit vectors at 0, 80, 35 and 45 degrees. The first two are far
        // enough apart to open two voices; the last two join the first and
        // drag it around until the pair is close enough to be one.
        let half_root_two = std::f32::consts::FRAC_1_SQRT_2;
        let voices = Vec::from([
            voice([1.0, 0.0]),
            voice([0.17365, 0.98481]),
            voice([0.81915, 0.57358]),
            voice([half_root_two, half_root_two]),
        ]);
        assert_eq!(group_online(&voices, 0.5), Vec::from([Some(0); 4]));
    }

    #[test]
    fn the_voice_heard_first_is_numbered_first() {
        let voices = Vec::from([voice([0.0, 1.0]), voice([1.0, 0.0]), voice([0.14, 0.99])]);
        assert_eq!(
            group_online(&voices, 0.5),
            Vec::from([Some(0), Some(1), Some(0)])
        );
    }
}
