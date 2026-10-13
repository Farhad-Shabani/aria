# Welcome to Aria

Aria's Rust libraries turn speech into text and text into spoken audio. Use
them to add microphone input, transcription, or speech playback to your
application. They also power the Aria voice assistant.

This guide assumes you know Rust and Cargo.

## Start here

First, [add Aria to your project](./workflow.md). Then choose a task:

- [Transcribe a WAV file](./transcription.md) with Whisper.
- [Record and transcribe microphone input](./recording.md).
- [Read text aloud](./synthesis.md#system-voice) with the system voice.
- [Generate audio samples](./synthesis.md#kokoro-audio) with Kokoro.

For a first transcription, start with a short WAV file. That lets you check
the model and its output before adding microphone capture.

Whisper and Kokoro run on your machine. Downloading models and build dependencies
needs network access. Read [Privacy](./privacy.md) before
processing personal recordings.

For everyday use of the voice assistant, read the
[User Guide](https://github.com/Farhad-Shabani/aria/blob/main/book/user-guide/src/introduction.md).

## Licensing

The library crates used here are Apache-2.0. The Aria application, CLI, and
macOS app are GPL-3.0-only. Depending on `aria-stt`, `aria-tts`, or `aria-kit`
does not require a dependency on the application crate, `aria`.

Engine dependencies and model weights have their own licenses. Review those
before distributing your application. The repository records bundled third-party material in
[THIRD-PARTY-NOTICES.md](https://github.com/Farhad-Shabani/aria/blob/main/THIRD-PARTY-NOTICES.md).
