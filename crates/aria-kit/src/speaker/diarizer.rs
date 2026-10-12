//! Grouping voice embeddings into speakers, live and at the end of a
//! session. Any [`Embedder`] supplies the embeddings; the thresholds and the
//! clustering policy live here.

use super::{Embedder, Engine, Profile, Role, SpeakerName};

/// Minimum similarity for claiming an enrolled name.
pub const ENROLLED_THRESHOLD: f32 = 0.5;
/// Similarity threshold for anonymous clusters; short audio windows need a lower gate.
pub const CLUSTER_THRESHOLD: f32 = 0.21;
/// Similarity advantage required to switch anonymous speakers.
/// Synthetic tests do not calibrate this margin for real overlapping speech.
const STICKY_MARGIN: f32 = 0.10;

/// An embedding engine with enrolled profiles and anonymous voice clusters.
pub struct Diarizer<E> {
    embedder: E,
    enrolled: Vec<Profile>,
    clusters: Vec<Cluster>,
}

struct Cluster {
    name: String,
    sum: Vec<f32>,
}

/// Loads a diarizer with caller-supplied profiles. Logs model errors and returns
/// `None`, allowing the caller to continue without labels.
pub fn open<E: Engine>(config: E::Config, enrolled: Vec<Profile>) -> Option<Diarizer<E>> {
    // Never enrolling is ordinary, so an absent model is not a warning.
    if let Err(error) = E::check_from(&config) {
        tracing::info!(%error, "no speaker model; the transcript stays unlabeled");
        return None;
    }
    match E::load_from(config) {
        Ok(embedder) => Some(Diarizer::new(embedder, enrolled)),
        Err(error) => {
            tracing::warn!(%error, "speaker model unusable; the transcript stays unlabeled");
            None
        }
    }
}

impl<E: Embedder> Diarizer<E> {
    /// A diarizer over an engine the caller already loaded.
    pub fn new(embedder: E, enrolled: Vec<Profile>) -> Self {
        Self {
            embedder,
            enrolled,
            clusters: Vec::new(),
        }
    }

    /// A unit-norm embedding for one segment's audio window, or `None` when
    /// none can be trusted. A broken model degrades the same way a quiet
    /// window does, one warning apart.
    pub fn embed(&mut self, window: &[f32]) -> Option<Vec<f32>> {
        match self.embedder.embed(window) {
            Ok(embedding) => embedding,
            Err(error) => {
                tracing::warn!(%error, "speaker embedding failed");
                None
            }
        }
    }

    /// The live, tentative label for an embedding. `speaking` is the label
    /// the previous segment was given, which holds unless this one clearly
    /// belongs elsewhere.
    pub fn label(&mut self, embedding: &[f32], speaking: Option<&str>) -> String {
        assign(&self.enrolled, &mut self.clusters, embedding, speaking)
    }

    /// Replaces the enrolled profiles, keeping the loaded model. A voice
    /// enrolled since this diarizer opened is invisible until this is called.
    pub fn set_enrolled(&mut self, enrolled: Vec<Profile>) {
        self.enrolled = enrolled;
    }

    /// How many enrolled profiles belong to the machine's owner. Zero means
    /// nothing can answer "was that you?".
    pub fn owners(&self) -> usize {
        self.enrolled
            .iter()
            .filter(|profile| profile.role == Role::Owner)
            .count()
    }

    /// The enrolled name this embedding matches. Unlike [`Diarizer::label`],
    /// it never opens or grows an anonymous cluster.
    pub fn enrolled_name(&self, embedding: &[f32]) -> Option<String> {
        enrolled_name(&self.enrolled, embedding).map(SpeakerName::to_string)
    }

    /// How close this voice comes to the nearest profile the machine's owner
    /// enrolled, as a cosine. `None` when nobody owns a profile — a guest is
    /// recognized by name, but is not the person the machine answers to.
    #[cfg(not(target_family = "wasm"))]
    #[cfg(all(feature = "silero", not(target_family = "wasm")))]
    pub(super) fn owner_similarity(&self, embedding: &[f32]) -> Option<f32> {
        closest(
            self.enrolled
                .iter()
                .filter(|profile| profile.role == Role::Owner)
                .map(|profile| ((), dot(&profile.vector, embedding))),
        )
        .map(|(_, similarity)| similarity)
    }

    /// The final labels for a whole session's paragraphs. `None` embeddings
    /// come back `None` — the caller owns the timing that says which neighbor
    /// they belong to.
    pub fn relabel(&self, paragraphs: &[Option<Vec<f32>>]) -> Vec<Option<String>> {
        recluster(&self.enrolled, paragraphs)
    }
}

/// Fills missing labels from neighboring speakers. Utterance openings prefer
/// the following label; other gaps prefer the preceding one. All-None input stays unchanged.
pub fn settle(labels: &mut [Option<String>], opens_utterance: impl Fn(usize) -> bool) {
    for index in (0..labels.len()).rev() {
        if labels[index].is_none() && opens_utterance(index) {
            let next = labels.get(index + 1).cloned().flatten();
            labels[index] = next;
        }
    }
    for index in 1..labels.len() {
        if labels[index].is_none() {
            labels[index] = labels[index - 1].clone();
        }
    }
    let opening = labels.iter().position(Option::is_some);
    if let Some(first) = opening {
        let voice = labels[first].clone();
        labels[..first].fill(voice);
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Enrolled cosine at the gate wins the name — enrolled centroids never
/// update. Otherwise the nearest running cluster absorbs the embedding, or a
/// new `Speaker {n}` opens.
///
/// `speaking` identifies the previous anonymous speaker. It keeps the label
/// while above the cluster threshold and within [`STICKY_MARGIN`] of the best match.
fn assign(
    enrolled: &[Profile],
    clusters: &mut Vec<Cluster>,
    embedding: &[f32],
    speaking: Option<&str>,
) -> String {
    if let Some(name) = enrolled_name(enrolled, embedding) {
        return name.to_string();
    }
    let similarities: Vec<f32> = clusters
        .iter()
        .map(|cluster| dot(&cluster.sum, embedding) / dot(&cluster.sum, &cluster.sum).sqrt())
        .collect();
    let best = closest(similarities.iter().copied().enumerate());

    let held = speaking
        .and_then(|name| clusters.iter().position(|cluster| cluster.name == name))
        .filter(|&at| similarities[at] >= CLUSTER_THRESHOLD)
        .filter(|&at| best.is_none_or(|(_, top)| similarities[at] + STICKY_MARGIN >= top));
    let taking = held.or_else(|| {
        best.filter(|(_, similarity)| *similarity >= CLUSTER_THRESHOLD)
            .map(|(at, _)| at)
    });

    if let Some(at) = taking {
        return absorb(&mut clusters[at], embedding).to_owned();
    }
    let name = format!("Speaker {}", clusters.len() + 1);
    clusters.push(Cluster {
        name: name.clone(),
        sum: embedding.to_vec(),
    });
    name
}

/// Adds an embedding to a cluster and answers with its name.
fn absorb<'a>(cluster: &'a mut Cluster, embedding: &[f32]) -> &'a str {
    for (sum, value) in cluster.sum.iter_mut().zip(embedding) {
        *sum += value;
    }
    &cluster.name
}

/// The enrolled profile this embedding is close enough to claim.
fn enrolled_name<'a>(enrolled: &'a [Profile], embedding: &[f32]) -> Option<&'a SpeakerName> {
    closest(
        enrolled
            .iter()
            .map(|profile| (&profile.name, dot(&profile.vector, embedding))),
    )
    .filter(|(_, similarity)| *similarity >= ENROLLED_THRESHOLD)
    .map(|(name, _)| name)
}

fn closest<T>(candidates: impl Iterator<Item = (T, f32)>) -> Option<(T, f32)> {
    candidates.reduce(|best, candidate| {
        if candidate.1 > best.1 {
            candidate
        } else {
            best
        }
    })
}

/// Average-linkage agglomerative clustering at the gate. With unit vectors
/// the inter-cluster average cosine is `dot(sumA, sumB) / (nA * nB)`.
// Each merge rescans every pair, so this is O(n³) in a session's
// paragraphs. A linkage matrix would help if hours-long sessions drag at stop.
fn recluster(enrolled: &[Profile], paragraphs: &[Option<Vec<f32>>]) -> Vec<Option<String>> {
    struct Group {
        sum: Vec<f32>,
        count: usize,
        members: Vec<usize>,
    }
    let mut groups: Vec<Group> = paragraphs
        .iter()
        .enumerate()
        .filter_map(|(index, embedding)| {
            embedding.as_ref().map(|embedding| Group {
                sum: embedding.clone(),
                count: 1,
                members: vec![index],
            })
        })
        .collect();

    loop {
        let mut best: Option<(usize, usize, f32)> = None;
        for a in 0..groups.len() {
            for b in a + 1..groups.len() {
                let similarity = dot(&groups[a].sum, &groups[b].sum)
                    / (groups[a].count * groups[b].count) as f32;
                if best.is_none_or(|(_, _, top)| similarity > top) {
                    best = Some((a, b, similarity));
                }
            }
        }
        let Some((a, b, similarity)) = best else {
            break;
        };
        if similarity < CLUSTER_THRESHOLD {
            break;
        }
        let absorbed = groups.swap_remove(b);
        let keeper = &mut groups[a];
        for (sum, value) in keeper.sum.iter_mut().zip(&absorbed.sum) {
            *sum += value;
        }
        keeper.count += absorbed.count;
        keeper.members.extend(absorbed.members);
    }

    groups.sort_by_key(|group| group.members.iter().min().copied());
    let mut labels = vec![None; paragraphs.len()];
    let mut numbered = 0;
    for group in &groups {
        let name = closest(enrolled.iter().map(|profile| {
            (
                &profile.name,
                dot(&profile.vector, &group.sum) / group.count as f32,
            )
        }))
        .filter(|(_, similarity)| *similarity >= ENROLLED_THRESHOLD)
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| {
            numbered += 1;
            format!("Speaker {numbered}")
        });
        for &member in &group.members {
            labels[member] = Some(name.clone());
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speaker::normalized;

    const A: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
    const B: [f32; 4] = [0.0, 1.0, 0.0, 0.0];

    fn near_a() -> Vec<f32> {
        normalized(vec![0.9, 0.1, 0.0, 0.0]).expect("nonzero")
    }

    #[test]
    fn assign_joins_close_voices_and_splits_far_ones() {
        let mut clusters = Vec::new();
        assert_eq!(assign(&[], &mut clusters, &A, None), "Speaker 1");
        assert_eq!(assign(&[], &mut clusters, &near_a(), None), "Speaker 1");
        assert_eq!(assign(&[], &mut clusters, &B, None), "Speaker 2");
    }

    #[test]
    fn a_voice_already_talking_keeps_the_label_through_a_close_call() {
        // Two voices a hair apart. Without the margin the label would flip
        // between them segment by segment, which is what a reader sees as
        // one person becoming five.
        let mut clusters = Vec::new();
        assert_eq!(assign(&[], &mut clusters, &A, None), "Speaker 1");
        assert_eq!(assign(&[], &mut clusters, &B, None), "Speaker 2");

        let between = normalized(vec![0.55, 0.45, 0.0, 0.0]).expect("nonzero");
        assert_eq!(
            assign(&[], &mut clusters, &between, Some("Speaker 1")),
            "Speaker 1",
            "the voice talking holds it"
        );
    }

    #[test]
    fn a_voice_that_is_clearly_someone_else_still_takes_the_label() {
        // Stickiness must not weld the label on. A voice that wins outright
        // takes it however long the other has been talking.
        let mut clusters = Vec::new();
        assert_eq!(assign(&[], &mut clusters, &A, None), "Speaker 1");
        assert_eq!(assign(&[], &mut clusters, &B, None), "Speaker 2");
        assert_eq!(
            assign(&[], &mut clusters, &B, Some("Speaker 1")),
            "Speaker 2"
        );
    }

    #[test]
    fn assign_prefers_an_enrolled_name_and_leaves_its_profile_alone() {
        let enrolled = vec![super::super::enrolled_voice(
            "alex",
            Role::Owner,
            A.to_vec(),
        )];
        let mut clusters = Vec::new();
        assert_eq!(assign(&enrolled, &mut clusters, &near_a(), None), "alex");
        assert!(clusters.is_empty());
        assert_eq!(enrolled[0].vector, A.to_vec());
    }

    #[test]
    fn recluster_merges_an_online_split_and_leaves_unembedded_slots_open() {
        let paragraphs = vec![
            Some(A.to_vec()),
            None,
            Some(near_a()),
            Some(B.to_vec()),
            None,
        ];
        let labels = recluster(&[], &paragraphs);
        let expected = [
            Some("Speaker 1"),
            None,
            Some("Speaker 1"),
            Some("Speaker 2"),
            None,
        ];
        let actual: Vec<Option<&str>> = labels.iter().map(Option::as_deref).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn settle_names_an_opening_nobody_could_embed() {
        let named = |name: &str| Some(name.to_owned());
        // Nothing precedes these, so the backward pass is the only one that
        // can reach them. Unnamed, they open the transcript with no speaker.
        let mut labels = vec![None, None, named("Speaker 1")];
        settle(&mut labels, |_| false);
        let actual: Vec<Option<&str>> = labels.iter().map(Option::as_deref).collect();
        assert_eq!(
            actual,
            [Some("Speaker 1"), Some("Speaker 1"), Some("Speaker 1")]
        );
    }

    #[test]
    fn settle_leaves_a_session_nobody_named_alone() {
        let mut labels = vec![None, None];
        settle(&mut labels, |_| false);
        assert_eq!(labels, [None, None]);
    }

    #[test]
    fn settle_sends_an_opener_forward_and_a_tail_back() {
        let named = |name: &str| Some(name.to_owned());
        let mut labels = vec![
            named("Speaker 1"),
            None, // a tail: keeps the voice before it
            None, // an opener: takes the voice after it
            named("Speaker 2"),
        ];
        settle(&mut labels, |index| index == 2);
        let actual: Vec<Option<&str>> = labels.iter().map(Option::as_deref).collect();
        assert_eq!(
            actual,
            [
                Some("Speaker 1"),
                Some("Speaker 1"),
                Some("Speaker 2"),
                Some("Speaker 2"),
            ]
        );
    }

    #[test]
    fn recluster_gives_a_merged_group_its_enrolled_name() {
        let enrolled = vec![super::super::enrolled_voice(
            "alex",
            Role::Owner,
            A.to_vec(),
        )];
        let paragraphs = vec![Some(near_a()), Some(B.to_vec())];
        let labels = recluster(&enrolled, &paragraphs);
        assert_eq!(labels[0].as_deref(), Some("alex"));
        assert_eq!(labels[1].as_deref(), Some("Speaker 1"));
    }
}
