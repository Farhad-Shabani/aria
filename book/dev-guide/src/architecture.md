# Engines and sessions

The examples use two kinds of object: a loaded engine and a session for one
recording or utterance. Keeping them separate lets you load a model once and
reuse it for later requests.

## Libraries

`aria-stt` provides transcription, `aria-tts` provides synthesis, and `aria-kit`
provides microphone capture, audio playback, and shared speech types.

The engine crates re-export their common types in `prelude`. That is why
transcription imports `Transcription`, `RecordingReader`, and `Microphone` from
`aria_stt`, and synthesis imports `Synthesis` and `CancelToken` from `aria_tts`. Add
`aria-kit` directly for playback or for writing a clip to WAV.

## Transcription

```text
Load Whisper once
  → construct Transcription
  → provide a WAV or microphone recording
  → receive finalized text
```

`Whisper` loads and runs the recognition model. `Transcription` chooses how to
process the recording and applies any text layers. `Session` manages
transcription of one recording.

For a completed file, `transcribe_file` returns one `Transcript`. For live
input, `Session::next_finalized` returns segments as they become final, and
`stop` returns the complete transcript. Finalized segments stay unchanged.

The libraries do not insert text into another application. Your code decides
whether to display, save, or deliver it.

## Synthesis

```text
Load Kokoro once
  → construct Synthesis
  → start an utterance
  → read audio samples until completion
```

`Kokoro` supplies the model and voices. `Synthesis` starts one worker per
utterance. Its session gives you the sample rate and generated audio samples.
Dropping the session requests cancellation and waits for the thread to stop.

The system voice has a different output: it speaks through the platform's
voice program and provides no audio samples.

## Blocking calls

Model loading, transcription, audio reads, and speaking are synchronous calls.
Run them on a worker thread when integrating with a UI or an asynchronous
application. Streaming can produce output before an operation finishes, but
reading the next result can still block.
