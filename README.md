<h1 align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="assets/brand/aria-lockup-inline-animated-dark.svg"><img src="assets/brand/aria-lockup-inline-animated.svg" width="280" alt="Aria"></picture></h1>

<p align="center"><b>Talk to your AI agents without sending them your audio.</b></p>

Aria hears you, writes what you say, and reads text aloud. It's a voice
assistant for macOS and Linux, with all speech processing on your machine. It
also talks to your AI agents and keeps the conversation going. They get your
words, never your audio.

```sh
aria dictate          # speak; the text lands in the focused app
aria ask              # speak a question; your agent answers
aria say "Done for today."
aria listen start     # transcribe system audio, with speakers
```

## What it does

- **Dictate** into any app on macOS, or to the clipboard on macOS and Linux.
- **Ask** your AI agent by voice, and keep the conversation going.
- **Speak** text aloud.
- **Wake** on a word you choose, with your own model.
- **Listen** to system audio on macOS and transcribe it, labeled by speaker.
- **Learn** your project's vocabulary, to help spell its names right.

## Install

Download the Mac app or the Linux binary from the
[latest release](https://github.com/Farhad-Shabani/aria/releases/latest).
The [user guide](https://farhad-shabani.github.io/aria/user-guide/) covers
installation, setup, and every feature.

## Build from source

```sh
cargo build --release --bin aria
target/release/aria setup       # choose engines and fetch their models
target/release/aria doctor
```

A source build runs as `target/release/aria` in the examples above.

`just app` builds the macOS menu bar app with the daemon inside, into
`clients/build/mac/Build/Products/Release/`. It needs Xcode and XcodeGen.

## Engines

- **Speech recognition:** Whisper, Parakeet, Moonshine, Nemotron, Parakeet MLX
- **Speech synthesis:** Kokoro, Qwen3-TTS, system voice
- **Wake word:** openWakeWord, livekit-wakeword

## Toolkit

You can use these Rust crates in your own projects, without the Aria daemon.
The [developer guide](https://farhad-shabani.github.io/aria/dev-guide/) shows
how.

- `aria-kit`: audio capture, and the traits every engine implements
- `aria-stt`: speech-recognition engines
- `aria-tts`: speech-synthesis engines
- `aria-wake`: wake-word detection
- `aria-speaker`: speaker embeddings
- `aria-segmentation`: speaker segmentation
- `aria-web`: the dictation pipeline in the browser, through WebAssembly
- `aria-primitives`: process, file, and socket building blocks

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Contributors sign a
[CLA](https://cla-assistant.io/Farhad-Shabani/aria) on their first pull
request.

## License

- The daemon, CLI, macOS app, `aria-bench`, and the rest of this repository:
  [GPL-3.0-only](LICENSE-GPL).
- The Aria name and logo: [reserved](assets/brand/LICENSE).
- The toolkit crates, `aria-bench-speaker`, and the protocol in `proto/`:
  [Apache-2.0](LICENSE-APACHE).
- Copied third-party material: [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
