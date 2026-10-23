<h1 align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="assets/brand/aria-lockup-inline-animated-dark.svg"><img src="assets/brand/aria-lockup-inline-animated.svg" width="280" alt="Aria"></picture></h1>

<p align="center"><b>Talk to your AI agents without sending them your audio.</b></p>

<p align="center">
  <strong><a href="https://github.com/Farhad-Shabani/aria/releases/latest/download/Aria.dmg" title="Download the macOS desktop app">Download</a></strong> |
  <a href="https://ariacue.pages.dev/">Website</a> |
  <a href="https://ariacue.pages.dev/user-guide/">Users</a> |
  <a href="https://ariacue.pages.dev/dev-guide/">Developers</a>
</p>

<p align="center">
  <a href="https://github.com/Farhad-Shabani/aria/actions/workflows/check.yml"><img src="https://github.com/Farhad-Shabani/aria/actions/workflows/check.yml/badge.svg?branch=main" alt="Checks"></a>
  <a href="https://github.com/Farhad-Shabani/aria/releases/latest"><img src="https://img.shields.io/github/v/release/Farhad-Shabani/aria" alt="Latest release"></a>
  <img src="https://img.shields.io/badge/dynamic/yaml?url=https%3A%2F%2Fraw.githubusercontent.com%2FFarhad-Shabani%2Faria%2Fmain%2Fclients%2Fproject.yml&query=%24.options.deploymentTarget.macOS&label=macOS&suffix=%2B%20%28Apple%20silicon%29&color=blue&logo=apple" alt="Minimum macOS on Apple silicon">
  <img src="https://img.shields.io/badge/Linux-x86__64-blue?logo=linux" alt="Linux x86_64">
</p>

Aria hears you, writes what you say, and reads text aloud. It's a voice
assistant for macOS and Linux, with all speech processing on your machine. It
also talks to your AI agents and keeps the conversation going. They get your
words, never your audio.

The macOS app brings setup, settings, history, and keyboard shortcuts to your menu bar.

```sh
aria dictate          # speak; type into a Mac app or copy the text
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

Download the [macOS desktop app](https://github.com/Farhad-Shabani/aria/releases/latest/download/Aria.dmg).
Drag Aria to Applications, then open it for guided setup.

Or use Homebrew to install the app and the `aria` command:

```sh
brew install --cask farhad-shabani/tap/aria
```

On Linux x86_64, the installer checks the download and starts setup:

```sh
curl \
  --fail \
  --location \
  https://github.com/Farhad-Shabani/aria/releases/latest/download/install.sh | bash
```

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

## Libraries

You can use these Rust crates in your own projects, without the Aria daemon.

- `aria-kit`: audio capture, streaming transcription, text cleanup, speaker
  tracking, and shared engine interfaces
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
