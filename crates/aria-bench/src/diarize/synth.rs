//! Ground-truth conversations: kokoro turns rendered to one canonical WAV.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::diarize::{BenchError, ScriptError};
use aria_kit::audio::{Clip, ClipError, canonical_samples, write_wav};
use aria_kit::cancel::CancelToken;
use aria_kit::synthesis::{SpeakingRate, SpeechText, SynthesisError, Synthesizer, Voice, stream};

/// Silence between turns, long enough for a paragraph break.
const TURN_GAP: Duration = Duration::from_millis(1_500);

pub(crate) struct Scenario {
    pub(crate) name: &'static str,
    pub(crate) turns: Vec<Turn>,
}

pub(crate) struct Turn {
    /// Ground-truth speaker identity — the kokoro voice name.
    pub(crate) speaker: &'static str,
    pub(crate) text: &'static str,
}

impl Scenario {
    pub(crate) fn all() -> Vec<Self> {
        let long = vec![
            turn(
                "am_adam",
                "Good morning everyone, let us walk through the quarterly numbers before we make any decisions about the budget.",
            ),
            turn(
                "af_heart",
                "Before you start, I want to flag that the marketing spend from last month has not been reconciled yet.",
            ),
            turn(
                "am_adam",
                "That is a fair point, so let us treat the marketing figures as provisional until the reconciliation lands.",
            ),
            turn(
                "bm_george",
                "From the engineering side, the migration finished ahead of schedule and the cost savings are already visible.",
            ),
            turn(
                "af_heart",
                "That is great news, and it should offset most of the overrun we saw in the first quarter.",
            ),
            turn(
                "bm_george",
                "Exactly, and if we keep the current pace we will be under budget by the end of the year.",
            ),
            turn(
                "am_adam",
                "Then let us lock the plan as it stands and revisit the marketing line once the numbers are final.",
            ),
            turn(
                "af_heart",
                "Agreed, I will circulate the reconciled figures by Friday so everyone can review them before the next meeting.",
            ),
        ];
        let choppy = vec![
            turn("am_adam", "Did you see the report?"),
            turn("af_heart", "I did, last night."),
            turn("am_adam", "And what did you think?"),
            turn("af_heart", "Honestly, it needs work."),
            turn("bm_george", "Which section bothered you?"),
            turn("af_heart", "The forecast, mostly."),
            turn("am_adam", "The forecast looked fine to me."),
            turn("bm_george", "The assumptions are stale though."),
            turn("af_heart", "Exactly my point."),
            turn(
                "am_adam",
                "Fine, let us redo the assumptions together tomorrow.",
            ),
        ];
        // Four voices of one gender and accent: the hardest case for
        // embeddings, where cross-voice cosines sit closest.
        let alike = vec![
            turn(
                "am_adam",
                "The quarterly budget forecast is finished and the finance committee has approved every line of it.",
            ),
            turn(
                "am_michael",
                "Deployment of the kubernetes cluster finished overnight and every rollout stayed inside its error budget.",
            ),
            turn(
                "am_eric",
                "My typography revision changes the heading scale, so the layout grid needs redrawing before launch.",
            ),
            turn(
                "am_liam",
                "Hiring closed three offers this week and the interview loop is now shorter by two full rounds.",
            ),
            turn(
                "am_adam",
                "Finance also wants the depreciation schedule restated before the quarterly filing goes out.",
            ),
            turn(
                "am_michael",
                "Kubernetes autoscaling absorbed the traffic spike without a single rollout being paused.",
            ),
            turn(
                "am_eric",
                "The heading scale interacts badly with our narrowest layout, so typography stays provisional.",
            ),
            turn(
                "am_liam",
                "Candidates keep asking about the interview loop, so hiring published the whole rubric.",
            ),
        ];
        // Five voices, one of whom barely speaks — the case a small-cluster
        // rule is most likely to get wrong.
        let crowd = vec![
            turn(
                "af_bella",
                "Our research cohort finished recruitment and the longitudinal study begins sampling next week.",
            ),
            turn(
                "bm_daniel",
                "Legal reviewed the indemnity clause and the counterparty accepted our liability cap unchanged.",
            ),
            turn(
                "am_fenrir",
                "Security rotated every credential after the audit and intrusion detection now covers the staging network.",
            ),
            turn(
                "bf_emma",
                "Support ticket volume dropped once the onboarding walkthrough shipped to new customers.",
            ),
            turn(
                "af_nova",
                "Procurement signed the vendor renewal yesterday.",
            ),
            turn(
                "af_bella",
                "Recruitment for the second research cohort depends on whether sampling holds its schedule.",
            ),
            turn(
                "bm_daniel",
                "The indemnity language still needs a counterparty signature before legal can close it.",
            ),
            turn(
                "am_fenrir",
                "Intrusion detection flagged one credential replay, so security tightened the audit window.",
            ),
            turn(
                "bf_emma",
                "Onboarding walkthrough feedback suggests support should rewrite the billing article next.",
            ),
        ];
        vec![
            Self {
                name: "long",
                turns: long,
            },
            Self {
                name: "choppy",
                turns: choppy,
            },
            Self {
                name: "alike",
                turns: alike,
            },
            Self {
                name: "crowd",
                turns: crowd,
            },
        ]
    }

    /// Renders the scenario to a 16 kHz mono 16-bit WAV, turns separated by
    /// silence, and returns its path.
    pub(crate) fn synthesize<S: Synthesizer>(
        &self,
        tts: &S,
        directory: &Path,
    ) -> Result<PathBuf, BenchError> {
        let scenario = self.name;
        let gap = vec![0.0_f32; canonical_samples(TURN_GAP) as usize];
        let mut conversation: Vec<f32> = gap.clone();
        for turn in &self.turns {
            let spoken = render(tts, turn).map_err(|error| error.at(scenario))?;
            conversation.extend(spoken.into_canonical()?.into_samples());
            conversation.extend_from_slice(&gap);
        }

        let path = directory.join(format!("{}.wav", self.name));
        write_wav(&path, &Clip::canonical(conversation))
            .map_err(|source| BenchError::Wav { scenario, source })?;
        Ok(path)
    }
}

fn turn(speaker: &'static str, text: &'static str) -> Turn {
    Turn { speaker, text }
}

/// A turn's rendering failure, still missing the scenario that names it.
enum RenderError {
    Script(ScriptError),
    Synthesis(SynthesisError),
    SampleRate(ClipError),
}

impl RenderError {
    fn at(self, scenario: &'static str) -> BenchError {
        match self {
            Self::Script(source) => BenchError::Script { scenario, source },
            Self::Synthesis(source) => BenchError::Synthesis { scenario, source },
            Self::SampleRate(source) => BenchError::SampleRate { scenario, source },
        }
    }
}

fn render<S: Synthesizer>(tts: &S, turn: &Turn) -> Result<Clip, RenderError> {
    let script = |error: ScriptError| RenderError::Script(error);
    let text = SpeechText::new(turn.text).map_err(|error| script(error.into()))?;
    let voice = Voice::new(turn.speaker).map_err(|error| script(error.into()))?;
    let rate = SpeakingRate::standard();
    let (writer, mut speech) = stream(tts.sample_rate());
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            tts.synthesize_with(&text, Some(&voice), rate, writer, &CancelToken::never())
        });
        let mut samples = Vec::new();
        let mut buffer = vec![0.0_f32; 8_192];
        let ended = loop {
            match speech.read(&mut buffer) {
                Ok(0) => break Ok(()),
                Ok(copied) => samples.extend_from_slice(&buffer[..copied]),
                Err(error) => break Err(error),
            }
        };
        // The worker's own error outranks the echo it left on the stream.
        worker
            .join()
            .expect("synthesis does not panic")
            .and(ended)
            .map_err(RenderError::Synthesis)?;
        Clip::new(tts.sample_rate(), samples).map_err(RenderError::SampleRate)
    })
}
