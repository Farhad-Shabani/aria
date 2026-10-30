<h1 align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Farhad-Shabani/aria/main/assets/brand/aria-lockup-inline-animated-dark.svg"><img src="https://raw.githubusercontent.com/Farhad-Shabani/aria/main/assets/brand/aria-lockup-inline-animated.svg" width="280" alt="Aria"></picture></h1>

<p align="center"><b>Talk to your AI assistants without sending them your audio.</b></p>

<p align="center">
  <strong><a href="https://github.com/Farhad-Shabani/aria/releases/latest/download/Aria.dmg" title="Download the macOS desktop app">Download</a></strong> |
  <a href="https://ariacue.pages.dev/">Website</a> |
  <a href="https://ariacue.pages.dev/user-guide/">Manual</a> |
  <a href="https://ariacue.pages.dev/dev-guide/">Libraries</a>
</p>

<p align="center">
  <a href="https://github.com/Farhad-Shabani/aria/actions/workflows/check.yml"><img src="https://github.com/Farhad-Shabani/aria/actions/workflows/check.yml/badge.svg?branch=main" alt="Checks"></a>
  <a href="https://github.com/Farhad-Shabani/aria/releases/latest"><img src="https://img.shields.io/github/v/release/Farhad-Shabani/aria" alt="Latest release"></a>
  <img src="https://img.shields.io/badge/dynamic/yaml?url=https%3A%2F%2Fraw.githubusercontent.com%2FFarhad-Shabani%2Faria%2Fmain%2Fclients%2Fproject.yml&query=%24.options.deploymentTarget.macOS&label=macOS&suffix=%2B%20%28Apple%20silicon%29&color=blue&logo=apple" alt="Minimum macOS on Apple silicon">
  <img src="https://img.shields.io/badge/Linux-x86__64-blue?logo=linux" alt="Linux x86_64">
</p>

Aria is a voice assistant for macOS and Linux. Press a hotkey and speak, and
Aria types your words into the Mac app you're using. Ask Claude Code or Codex a
question, or run one of your prompts, and the answer lands there too. Speech
runs on your computer by default, so your assistant gets your words as text,
never your audio.

A menu bar app for macOS brings setup, settings, history, and hotkeys together.
The `aria` command does most of this in a terminal, on macOS and Linux. [Rust
libraries](#libraries), licensed Apache-2.0, bring the speech pipeline to your
own projects, even in the browser through WebAssembly.

## What it does

- **Dictate** into the Mac app you're working in. On Linux, Aria copies the
  text.
- **Ask** Claude Code or Codex by voice, and keep the conversation going.
- **Rewrite** what you say with your own prompts, such as polishing or
  translating it.
- **Read** text aloud.
- **Transcribe** audio playing on your Mac, labeled by speaker.
- **Personalize** dictation to type only your voice, spell your project's
  names, and follow rules for each app.

Prompts, app rules, phone pairing, and the voice filter need the Mac app.

## Get started

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

Then try it from a terminal:

```sh
aria dictate          # speak; type into a Mac app or copy the text
aria ask              # speak a question; your assistant answers
aria say "Done for today."
aria listen start     # Mac only: system audio, by speaker
```

## What leaves your computer

Speech runs on your computer. Aria sends no audio to any cloud service, and
has no telemetry or crash reporting.

**Audio**

- **iPhone and Apple Watch (coming soon):** audio goes to your Mac on your
  local network. It uses mutual TLS, so both devices check each other's
  certificate. Off until you turn on phone pairing.
- **Your own server (optional):** audio goes to a machine you set up with
  `aria serve` and pair with, over mutual TLS too. Off until you set it up.
  Whoever runs that server can hear the audio.

**Text and files**, only when you use these features

- **Ask and prompts:** your words go to Claude Code or Codex, with the prompt
  if you run one. For Ask, the assistant may also read files in your project
  and send them to its provider. Prompts read no files.
- **Memory (off by default):** spoken Ask conversations go to Claude Code,
  which picks out facts you mention. Aria saves them in `~/.aria/memory`.

**Network**

- **Updates:** the Mac app checks GitHub daily. GitHub sees your IP address
  and app version. Turn it off under **Settings → About**.
- **Setup:** downloads models over HTTPS from Hugging Face and GitHub, and
  checks each file's checksum. One engine, Parakeet MLX, fetches its own model
  on first use.

History stays on your computer, readable only by your account. By default it
keeps text up to 30 days, and audio only from failed transcriptions, for 24
hours. Turn saving off under **Settings → Privacy**.
[Privacy](https://ariacue.pages.dev/user-guide/privacy.html) covers every case.

## Engines

- **Speech recognition:** Whisper (two builds), Parakeet, Moonshine, Nemotron,
  Parakeet MLX
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

To transcribe a WAV file with Whisper, run
`cargo add aria-stt --features whisper`, then:

```rust,no_run
use aria_stt::{prelude::*, whisper::Whisper};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Whisper::load("/absolute/path/ggml-base.en.bin")?;
    let transcription = Transcription::streaming(engine);
    let transcript = transcription.transcribe_file("speech.wav")?;
    println!("{}", transcript.as_str());
    Ok(())
}
```

The [library guide](https://ariacue.pages.dev/dev-guide/) covers models,
audio formats, and the other crates.

## Build from source

```sh
cargo build --release --bin aria
target/release/aria setup       # choose engines and fetch their models
target/release/aria doctor
```

A source build runs as `target/release/aria` in the examples above.

`just app` builds the macOS menu bar app with the daemon inside, into
`clients/build/mac/Build/Products/Release/`. It needs Xcode and XcodeGen.

## Contributing

See [CONTRIBUTING.md](https://github.com/Farhad-Shabani/aria/blob/main/CONTRIBUTING.md). Contributors sign a
[CLA](https://cla-assistant.io/Farhad-Shabani/aria) on their first pull
request.

## License

- The daemon, CLI, macOS app, and `aria-bench`:
  [GPL-3.0-only](https://github.com/Farhad-Shabani/aria/blob/main/LICENSE-GPL).
- The library crates, `aria-harness`, `aria-bench-speaker`, and the protocol in
  `proto/`: [Apache-2.0](https://github.com/Farhad-Shabani/aria/blob/main/LICENSE-APACHE).
- Copied third-party material: [THIRD-PARTY-NOTICES.md](https://github.com/Farhad-Shabani/aria/blob/main/THIRD-PARTY-NOTICES.md).
