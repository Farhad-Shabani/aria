{{#title Set up Aria's Rust speech libraries - Using Aria Libraries}}

# Setup

Start with a small Cargo project: add the dependencies for your task, check
that they build, then run an example. You can use the same dependencies in an
existing project. No Aria service or settings file is required.

## Build tools

Install stable Rust and Cargo with [rustup](https://rustup.rs).
These examples target macOS and Linux.

Whisper and Kokoro need a C/C++ compiler, CMake, and Clang/libclang. On macOS,
install the Xcode command-line tools.

On Linux, microphone capture and library playback also need ALSA development
headers and pkg-config. For the system voice, install `espeak-ng` or `espeak`
under `/usr/bin`. macOS provides the system voice through `say`.

## Create a project

```sh
cargo new speech-demo
cd speech-demo
```

Run the remaining commands from `speech-demo`, unless a page says otherwise.
`cargo add` adds the latest release of each crate to `Cargo.toml`.

### Transcription

```sh
cargo add aria-stt --features whisper
```

`aria-stt` has no engine enabled by default. The `whisper` feature selects the
native Whisper implementation.

For microphone input, also enable the `microphone` feature:

```sh
cargo add aria-stt --features microphone
```

### Speech output

For the system voice, run:

```sh
cargo add aria-tts --features system
```

For Kokoro audio generation, run this instead:

```sh
cargo add aria-tts --features kokoro
```

If your application uses both Whisper and Kokoro, add both `aria-stt` and `aria-tts`.
To use both the system voice and Kokoro, run
`cargo add aria-tts --features system,kokoro`.

## Check the build

```sh
cargo check
```

The first build downloads the crates. Microphone capture and Kokoro also need
ONNX Runtime, which the build downloads and verifies when needed. Download
speech models separately, as shown in each example.

If you already have compatible ONNX Runtime libraries, you can select them
with `ORT_LIB_LOCATION`:

```sh
ORT_LIB_LOCATION=/opt/onnxruntime/lib \
  cargo check
```

After the check passes, you're ready to [transcribe a file](./transcription.md),
[record from a microphone](./recording.md), or [generate speech](./synthesis.md).
