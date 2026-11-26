{{#title Reuse speech engines and sessions in Rust - Using Aria Libraries}}

# Engines and sessions

Speech work uses three objects. You load an engine once, hand it to a
`Transcription` or `Synthesis`, and keep that value. Each recording or text
then gets its own short-lived `Session`.

| Object | What it holds | How long to keep it |
|---|---|---|
| Engine: `Whisper`, `Kokoro` | The loaded model. Loading reads model files and is slow. | Until you pass it to `Transcription` or `Synthesis`, which take ownership |
| `Transcription`, `Synthesis` | The engine, plus the transcription mode and text layers | As long as your app needs the model. Reuse it for every recording or text |
| `Session` | One recording being transcribed, or one text being spoken | Until that recording or text is done |

## Libraries

`aria-stt` provides transcription and `aria-tts` provides synthesis. Each
exports the types you need in its `prelude`: `use aria_stt::prelude::*;` or
`use aria_tts::prelude::*;`. Import the engine from its own module, such as
`aria_stt::whisper::Whisper` or `aria_tts::kokoro::Kokoro`.

Both crates build on `aria-kit`, the audio library. Add `aria-kit` yourself only
to call it directly, for example to write a WAV file with
`aria_kit::audio::write_wav`.

Some uses need a Cargo feature:

| To | Enable |
|---|---|
| Record from the microphone | The [`microphone` feature](./workflow.md#transcription) of `aria-stt` |
| Play audio on the speakers | The [`playback` feature](./synthesis.md#kokoro-playback) of `aria-kit` |
| Split a long recording at each pause | The [`silero` feature](./recording.md#continuous-dictation) of `aria-kit` |

## Transcription

`Whisper::load(path)` loads a Whisper speech-recognition model from a file.
[Model and audio](./transcription.md#model-and-audio) shows where to download
one. Pass the engine to one of two constructors. Each takes ownership, so one
`Transcription` has one mode. To use both modes, load the model twice.

- `Transcription::streaming(engine)` gives you text in pieces while the
  recording runs.
- `Transcription::complete(engine)` transcribes the whole recording in one pass
  once it ends.

You can add text layers with `transcription.with(layer)`. A text layer edits
recognized text, for example to fix how a project name is spelled. See
[Project spellings](./transcription.md#project-spellings).

There are three ways to transcribe. In each, `language` is a `Language`, such
as `Language::Automatic`:

| Call | Audio | Recording ends when |
|---|---|---|
| `transcription.transcribe_file(path)` | A WAV file | The file ends. It returns a `Transcript` and needs no session. |
| `Session::microphone(&transcription, &language)` | The default microphone | You call `session.finish()`, which stops the microphone |
| `Session::over(&transcription, &language, reader)` | A `RecordingReader` you supply | The code writing that recording ends it, as described below. |

Two calls give you a `RecordingReader`, and each ends its recording
differently:

- `microphone.record(mode)` returns a reader and a `Stopper`. Get
  `microphone` from `Microphone::open()`, and keep it until the recording ends.
  With `RecordingMode::StopOnSilence`, the recording ends after a pause. To end
  it sooner, call `stop()` on the `Stopper`.
- `aria_kit::audio::Recording::open()` returns a reader and a writer. Push
  samples into the writer with `push`, then end the recording with the
  writer's `finish()`.

For a `Session::over` session, `session.finish()` waits for that end. If the
recording never ends, `finish` never returns.

Either way, `session.finish()` returns a `Transcript` with all the text, or
`TranscriptionError::NoSpeech` if nobody spoke. A microphone recording stops
with an error after 60 seconds, and the session then returns that error.

Only a session from `Transcription::streaming` has `session.next_finalized()`.
It returns `Some(segment)` for each finalized segment: a piece of text that
won't change. It returns `None` once the recording has ended and all text is
out. The `Transcript` from `finish`
repeats those segments, so don't append it to text you already showed.

The libraries don't type text into another app. Your code decides whether to
show, save, or send it.

## Synthesis

`Kokoro::load(model, voices)` loads the Kokoro speech model from its model
file and voices file. [Kokoro audio](./synthesis.md#kokoro-audio) shows where
to download both. Pass the engine to `Synthesis::new(engine)`, which takes
ownership, and keep the `Synthesis`.

With `speech` as your `Synthesis`, `speech.start(&text)` returns a `Session`
for one text. Build the text with `SpeechText::new("Hello.")?`. This `Session`
comes from `aria_tts` and is a different type from the transcription one. Each
synthesis session runs on its own worker thread.

Read the audio from the session in one of two ways:

- Call `session.read(&mut buffer)` until it returns 0. Each call fills a
  buffer of `f32` samples and returns how many. `session.sample_rate()` gives
  their rate, which a player or WAV writer needs.
- Call `session.into_clip()` to get all the audio at once.

To stop speech early, start it with `speech.start_with(&text, voice, rate,
&cancel)`, where `cancel` is a `CancelToken::new()`. Then call
`cancel.cancel()`. Dropping a synthesis session also cancels it, and waits for
its worker thread to stop.

The system voice needs no model file. It needs the `system` feature of
`aria-tts`. Create it with
`let system = aria_tts::system::SystemVoice::new()?;`. Then
`system.speak(&text, voice, rate, &cancel)` speaks through `say` on macOS, or
`espeak-ng` or `espeak` on Linux. It uses no `Synthesis` and no session. The
call returns when speech ends, and it gives you no audio samples.

## Blocking calls

These calls block the thread that makes them:

- Loading an engine: `Whisper::load`, `Kokoro::load`
- Transcribing: `transcribe_file`, `next_finalized`, `finish`, and the
  `Session` constructors, which wait for the engine to start
- Speaking: `read`, `into_clip`, `SystemVoice::speak`, and dropping a synthesis
  `Session`

In a UI or async app, run them on a worker thread.
