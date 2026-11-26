{{#title Reuse speech engines and sessions in Rust - Using Aria Libraries}}

# Engines and sessions

Load the model once and reuse the `Transcription` or `Synthesis` that owns it.
A session lasts for one live recording or one Kokoro utterance.

## Libraries

Use `aria-stt` for transcription and `aria-tts` for speech synthesis.
Both use `aria-kit` for audio. The [setup guide](./workflow.md#create-a-project)
shows which Cargo features to enable for your input or output.

## Transcription

Choose the call by where your audio comes from and who ends the recording.

| Input | Call | How you get the transcript |
|---|---|---|
| [A WAV file](./transcription.md#print-the-transcript) | `transcription.transcribe_file(path)` | The call reads the file and returns a `Transcript` directly. |
| [The default microphone](./recording.md#stop-with-enter) | `Session::microphone` | Call `session.finish()` to stop capture and return the complete transcript. |
| [A recording you supply](./recording.md#live-text) | `Session::over` | The audio source must end the recording. `session.finish()` waits for that end and returns the complete transcript. |

To show text while someone speaks, use `Transcription::streaming` and read
finalized segments with `session.next_finalized()`. Those words stay unchanged.
The transcript from `finish()` includes them, so appending it to the displayed
segments would repeat them.

## Synthesis

For [Kokoro audio](./synthesis.md#kokoro-audio), load the model and voices
files, then pass the engine to `Synthesis::new`. Each call to `start` returns
a session you can read for audio samples. To play those samples through Aria,
enable the [playback feature](./synthesis.md#kokoro-playback). For the
[system voice](./synthesis.md#system-voice), call `SystemVoice::speak`
directly. It handles playback and returns when speech ends.

## Blocking calls

In a UI or async application, load models, transcribe, read samples, and run
playback on a worker thread. Drop a live Kokoro session there too. Dropping it
requests cancellation and waits for its synthesis worker to stop.
