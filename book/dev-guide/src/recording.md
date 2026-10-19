# Microphone transcription

With a Whisper model ready, you can transcribe microphone input using the
same engine. Enable the [`whisper` and `microphone` features](./workflow.md#transcription)
of `aria-stt`.
If you still need a model, follow [Model and audio](./transcription.md#model-and-audio).

On macOS, grant microphone access to the application running the example.
On Linux, install ALSA development headers and pkg-config before building.
Start with a short utterance and a working default input device.

## Stop with Enter

Start with a recording you stop yourself. This program records until you
press Enter, then transcribes the recording and prints the completed text.

Replace `src/main.rs` with this program, using the absolute path to your
Whisper model:

```rust,no_run
use aria_stt::{prelude::*, whisper::Whisper};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Whisper::load("/absolute/path/ggml-base.en.bin")?;
    let transcription = Transcription::streaming(engine);
    let session = Session::microphone(&transcription, &Language::Automatic)?;

    println!("Speak, then press Enter to finish.");
    std::io::stdin().read_line(&mut String::new())?;

    println!("{}", session.stop()?.as_str());
    Ok(())
}
```

Run with `cargo run --release`, speak a sentence, and press Enter within a
minute. The default microphone limit is 60 seconds; exceeding it produces an
error rather than a successful transcript.

`stop()` ends capture, processes the remaining audio, and returns the transcript.
Dropping the session without calling `stop()` requests a stop without returning
a transcript.

## Live text

The first example prints the transcript after you stop recording. To show
text as it becomes available, read finalized segments: pieces of recognized
text that will not change. This version also stops recording automatically
after silence. Replace `src/main.rs` with the following, keeping your model path:

```rust,no_run
use aria_stt::{prelude::*, whisper::Whisper};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Whisper::load("/absolute/path/ggml-base.en.bin")?;
    let transcription = Transcription::streaming(engine);
    let mut microphone = Microphone::open()?;
    let (recording, _stopper) = microphone.record(RecordingMode::StopOnSilence {
        trailing_silence: Default::default(),
    })?;
    let mut session = Session::over(&transcription, &Language::Automatic, recording)?;

    while let Some(segment) = session.next_finalized()? {
        println!("{}", segment.as_str());
    }

    println!("Complete: {}", session.stop()?.as_str());
    Ok(())
}
```

`record` also returns a `Stopper`. Call its `stop()` from another thread or
a UI control to end the recording early; here, silence ends it.

Speak soon after starting the program, then pause. The default stop condition
is 1.5 seconds of silence after speech. If no speech starts within ten seconds,
capture returns an error. The 60-second recording limit still applies.

Append finalized segments to your displayed text. The complete transcript
contains those segments too; replace the display with it, or use it elsewhere,
without appending it a second time. A short utterance may finish before any
intermediate text appears.

Here, silence ends the recording. Calling `stop()` on `Session::over` waits
for that end before completing transcription; it does not stop capture itself.

## Continuous dictation

`StopOnSilence` records one utterance. To print every utterance from one long
recording, record `UntilStopped` and cut it yourself with an `Endpointer`.
Add `aria-kit` for the endpointer types with `cargo add aria-kit`, then
replace `src/main.rs` with:

```rust,no_run
use std::time::Duration;

use aria_kit::audio::{SilenceDuration, pcm16_from_f32};
use aria_kit::voice_activity::{
    Boundary, Endpointer, LEAD_IN_FRAMES, SileroVad, VoiceActivityDetector,
};
use aria_stt::{prelude::*, whisper::Whisper};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Whisper::load("/absolute/path/ggml-base.en.bin")?;
    let transcription = Transcription::streaming(engine);
    let mut microphone = Microphone::builder()
        .maximum_duration(Duration::from_secs(60 * 60))
        .open()?;
    let (mut recording, _stopper) = microphone.record(RecordingMode::UntilStopped)?;
    let mut endpointer = Endpointer::new(SileroVad::load()?, SilenceDuration::default());
    let frame = SileroVad::FRAME_SAMPLES as u64;
    let lead_in = LEAD_IN_FRAMES as u64 * frame;
    let (mut heard, mut opened) = (0, None);

    while recording.fill_through(heard + 1)? {
        let batch: Vec<i16> = recording
            .span(heard..recording.held_to())
            .iter()
            .copied()
            .map(pcm16_from_f32)
            .collect();
        heard = recording.held_to();
        for boundary in endpointer.push(&batch)? {
            match boundary {
                Boundary::Start(at) => opened = Some(at.saturating_sub(lead_in)),
                Boundary::End(at) => {
                    let from = opened.take().expect("an utterance ends after it starts");
                    let mut utterance = RecordingReader::from_samples(recording.span(from..at));
                    match transcription.transcribe(&mut utterance, &Language::Automatic) {
                        Ok(text) => println!("{}", text.as_str()),
                        Err(TranscriptionError::NoSpeech) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        // Keep the open utterance, and enough audio for one that starts next.
        recording.release_before(opened.unwrap_or(heard.saturating_sub(lead_in + frame)));
    }
    Ok(())
}
```

Positions count from the endpointer's first push, which here is the start of
the recording. Each utterance opens `LEAD_IN_FRAMES` frames before its
`Start`, the same lead-in `StopOnSilence` keeps. An utterance still open when
capture ends gets no `End`: transcribe `opened..held_to()` yourself if you
need it. Streaming transcription decodes a long utterance in 20-second
windows. Sound that never pauses, such as music, keeps one utterance open and
growing; cut it at a fixed length if your input can do that.

The loop runs until the maximum duration, which ends capture with an error.
Call `stop()` on the `Stopper` to end it sooner.

## Recording lifetime

`Microphone::open` keeps capture open while the microphone object lives.
A recording uses a private temporary WAV that can contain background speech.
It starts with up to about one second of audio from before `record()`.
The file is removed when the recording objects holding it are dropped.

For how to reuse the model and manage sessions, read
[Engines and sessions](./architecture.md).
