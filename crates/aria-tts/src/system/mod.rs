//! Speech through the operating system's own voice program.
//!
//! `say` on macOS and `espeak-ng` on Linux own the whole utterance: they
//! synthesize and play in one child process, so this backend produces no PCM
//! and needs no model download.
//!
//! The text is personal content. It reaches the child on stdin, never on the
//! command line, where every user on the machine could read it.
//!
//! ```no_run
//! use aria_tts::{system::SystemVoice, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let speech = SystemVoice::new()?;
//!
//! // Blocks until the platform program has finished speaking.
//! speech.speak(
//!     &SpeechText::new("Ready when you are.")?,
//!     None,
//!     SpeakingRate::standard(),
//!     &CancelToken::never(),
//! )?;
//! # Ok(())
//! # }
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aria_kit::backend::{BackendError, ModelDownload};
use aria_kit::cancel::CancelToken;
use aria_kit::primitives::process::{ChildProcess, ChildProcessError, WaitOutcome};
use aria_kit::synthesis::{
    Engine, SpeakingRate, SpeechText, SpeechWriter, SynthesisError, Synthesizer, Voice,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("no system voice program exists for this target");

/// Names this engine under `tts.backend` and in its own errors.
pub const NAME: &str = "system";

/// The pace the platform programs speak at when nothing is asked of them.
const NATURAL_WORDS_PER_MINUTE: f32 = 175.0;

/// A rough bytes-per-word guess, only to bound how long an utterance may run.
const BYTES_PER_WORD: usize = 6;

/// Slack on top of the estimated speaking time before the child is killed.
const CEILING_GRACE: Duration = Duration::from_secs(30);

impl Engine for SystemVoice {
    const NAME: &'static str = NAME;
    const MODEL: &'static [ModelDownload] = &[];

    /// The platform program is discovered, not configured.
    type Config = ();

    fn check_from(_config: &()) -> Result<PathBuf, SynthesisError> {
        Ok(Self::new()?.program)
    }

    fn load_from(_config: ()) -> Result<Self, SynthesisError> {
        Self::new()
    }
}

impl Synthesizer for SystemVoice {
    /// No PCM is written — the child owns playback — so this rate is nominal
    /// and only fills the contract.
    fn sample_rate(&self) -> u32 {
        24_000
    }

    fn plays_audio_itself(&self) -> bool {
        true
    }

    fn synthesize_with(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        out: SpeechWriter,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError> {
        let result = self.speak(text, voice, rate, cancel);
        match &result {
            Ok(()) => out.finish(),
            // The writer's reader is gone in practice — the child owns
            // playback — but it still closes with the cause. A program error
            // cannot be cloned, so the reader gets its message as an
            // inference failure.
            Err(error) => out.fail(unheard(error)),
        }
        result
    }
}

/// The platform voice program, resolved once.
pub struct SystemVoice {
    program: PathBuf,
}

impl SystemVoice {
    /// Finds the voice program. Nothing runs until an utterance.
    pub fn new() -> Result<Self, SynthesisError> {
        let program = platform::program().map_err(|missing| SynthesisError::Program {
            backend: NAME,
            program: missing.to_string(),
            source: BackendError::engine(missing),
        })?;
        Ok(Self { program })
    }

    /// Speaks and returns once the utterance ends. Cancelling kills the child,
    /// which is also what silences it.
    pub fn speak(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError> {
        self.speak_with_started(text, voice, rate, cancel, || {})
    }

    /// Speaks and reports once the text has been handed to the platform voice
    /// process.
    pub fn speak_with_started(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        cancel: &CancelToken,
        started: impl FnOnce(),
    ) -> Result<(), SynthesisError> {
        if cancel.is_cancelled() {
            return Err(SynthesisError::Cancelled);
        }
        let mut command = command(&self.program, voice, rate);
        let mut child = ChildProcess::spawn(&mut command)
            .map_err(|source| self.failure(SystemVoiceError::Spawn(source)))?;

        let mut stdin = child
            .take_stdin()
            .ok_or_else(|| self.failure(SystemVoiceError::StdinGone))?;
        // The text fits the pipe buffer, so the write cannot block on a child
        // that never reads. A child that died before reading closes the pipe;
        // its exit status says why, so the broken pipe is not the fault to
        // report.
        let written = stdin
            .write_all(text.as_str().as_bytes())
            .and_then(|()| stdin.flush());
        drop(stdin);
        let handed_off = written.is_ok();
        if let Err(source) = written
            && source.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(self.failure(SystemVoiceError::WriteText(source)));
        }
        if handed_off {
            started();
        }

        let deadline = Instant::now() + ceiling(text, rate);
        let outcome = child
            .wait_until(deadline, || cancel.is_cancelled())
            .map_err(|source| self.failure(SystemVoiceError::Child(source)))?;
        match outcome {
            WaitOutcome::Exited(status) if status.success() => Ok(()),
            // espeak-ng exits nonzero on a voice it does not have. Its
            // reason goes to stderr, which also carries the spoken text, so
            // the exit status is all this reads. macOS `say` falls back to
            // its default voice and succeeds, so this never fires there.
            WaitOutcome::Exited(_status) if voice.is_some() => {
                Err(SynthesisError::UnsupportedVoice {
                    backend: NAME,
                    voice: voice.expect("the arm tested it").as_str().to_owned(),
                })
            }
            WaitOutcome::Exited(status) => Err(self.failure(SystemVoiceError::Failed {
                exit_code: status.code(),
            })),
            WaitOutcome::Cancelled => Err(SynthesisError::Cancelled),
            WaitOutcome::TimedOut => Err(SynthesisError::TimedOut { backend: NAME }),
        }
    }

    fn failure(&self, error: SystemVoiceError) -> SynthesisError {
        SynthesisError::Program {
            backend: NAME,
            program: self.program.display().to_string(),
            source: BackendError::engine(error),
        }
    }
}

fn unheard(error: &SynthesisError) -> SynthesisError {
    SynthesisError::Inference {
        backend: NAME,
        source: BackendError::Unusable(error.to_string()),
    }
}

/// The child's command line: program, pace, and voice, never the text.
fn command(program: &Path, voice: Option<&Voice>, rate: SpeakingRate) -> Command {
    let words_per_minute = (NATURAL_WORDS_PER_MINUTE * rate.get()).round() as u32;
    let mut command = platform::command(program, voice, words_per_minute);
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// How long the utterance may run before the child is killed: twice the
/// estimated speaking time, plus grace for the program to start and settle.
///
/// A backstop, not the deadline of record: the caller holds a tighter
/// deadline on the utterance and reports an overrun as a timeout. The
/// doubling keeps this ceiling behind it.
fn ceiling(text: &SpeechText, rate: SpeakingRate) -> Duration {
    let words = (text.as_str().len() / BYTES_PER_WORD + 1) as f32;
    let minutes = words / (NATURAL_WORDS_PER_MINUTE * rate.get());
    Duration::from_secs_f32(minutes * 60.0 * 2.0) + CEILING_GRACE
}

#[derive(Debug, thiserror::Error)]
enum SystemVoiceError {
    #[error("failed to start the voice program")]
    Spawn(#[source] std::io::Error),

    #[error("the voice program's stdin was not piped")]
    StdinGone,

    #[error("failed to hand the text to the voice program")]
    WriteText(#[source] std::io::Error),

    #[error(transparent)]
    Child(ChildProcessError),

    #[error("the voice program exited with {exit_code:?}")]
    Failed { exit_code: Option<i32> },
}

#[derive(Debug, thiserror::Error)]
#[error("none of the voice programs exist: {}", candidates.join(", "))]
struct MissingProgram {
    candidates: Vec<String>,
}

impl MissingProgram {
    fn new(candidates: &[&Path]) -> Self {
        Self {
            candidates: candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(words: &str) -> SpeechText {
        SpeechText::new(words).expect("test text is valid")
    }

    #[test]
    fn the_command_line_never_carries_the_text() {
        let command = command(
            Path::new("/usr/bin/say"),
            Some(&Voice::new("Samantha").expect("a plain voice name")),
            SpeakingRate::standard(),
        );

        let arguments: Vec<_> = command.get_args().map(|arg| arg.to_os_string()).collect();
        assert!(
            !arguments.iter().any(|arg| arg == "hello"),
            "the text must travel on stdin"
        );
        assert!(arguments.iter().any(|arg| arg == "Samantha"));
        assert_eq!(
            command.get_envs().count(),
            0,
            "the child gets no environment"
        );
    }

    #[test]
    fn the_pace_scales_with_the_speaking_rate() {
        let command = command(
            Path::new("/usr/bin/say"),
            None,
            SpeakingRate::new(2.0).expect("2.0 is in range"),
        );

        let arguments: Vec<_> = command.get_args().map(|arg| arg.to_os_string()).collect();
        assert!(arguments.iter().any(|arg| arg == "350"));
    }

    #[test]
    fn the_ceiling_grows_with_the_text_and_shrinks_with_the_rate() {
        let short = ceiling(&text("hi"), SpeakingRate::standard());
        let long = ceiling(&text(&"word ".repeat(500)), SpeakingRate::standard());
        let fast = ceiling(
            &text(&"word ".repeat(500)),
            SpeakingRate::new(2.0).expect("2.0 is in range"),
        );

        assert!(long > short);
        assert!(fast < long);
        assert!(short >= CEILING_GRACE);
    }

    #[test]
    fn speaking_when_already_cancelled_does_not_start_a_child() {
        let voice = SystemVoice {
            program: PathBuf::from("/nonexistent/voice-program"),
        };
        let cancel = CancelToken::from_fn(|| true);

        let error = voice
            .speak(&text("hello"), None, SpeakingRate::standard(), &cancel)
            .expect_err("a cancelled request must not speak");

        assert!(matches!(error, SynthesisError::Cancelled));
    }
}
