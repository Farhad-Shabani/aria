# File transcription

Start by turning a short WAV recording into text with Whisper. Once that
works, you can choose a language or correct project terms in the result.
Add the [`aria-stt` dependency](./workflow.md#transcription) and check your
build tools before continuing.

## Model and audio

Whisper needs a GGML model from the
[whisper.cpp model collection](https://huggingface.co/ggerganov/whisper.cpp).
This example uses `ggml-base.en.bin`, a small English model:

```sh
curl -fL -o ggml-base.en.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin
```

Use the model's absolute path below.

The input must be a nonempty WAV containing **16 kHz, mono, signed 16-bit PCM**
(uncompressed audio), with at most 15 minutes of audio. MP3, stereo WAV, and
other sample rates are not converted automatically. To convert a recording
with `ffmpeg`:

```sh
ffmpeg -i input.m4a \
  -ar 16000 \
  -ac 1 \
  -c:a pcm_s16le \
  speech.wav
```

Use a short recording with a known sentence for your first test.

## Print the transcript

Replace `src/main.rs` with:

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

Replace the model path, put `speech.wav` in your project directory, and run:

```sh
cargo run \
  --release
```

The program prints the recognized sentence. `Whisper::load` loads the model;
`Transcription` uses it to process the recording. Reuse `transcription` for
later files to avoid loading the model for each one.

`transcribe_file` uses automatic language selection; an English-only model
still recognizes English only. `Transcription::streaming` also works with a
completed file and returns the complete result here.

If model loading fails, check that you downloaded a GGML file and supplied an
absolute path. Model files must be owned by your user or root and must not be
writable by group or others (for example, `chmod 644`). If input loading fails, check the WAV format above.
Silence or empty recognized output is reported as `TranscriptionError::NoSpeech`.

## Choose a language

If you know the recording's language, you can select it explicitly. Use a
`RecordingReader` to read the WAV and pass the language to
`transcribe`. Replace `src/main.rs` with:

```rust,no_run
use aria_stt::{prelude::*, whisper::Whisper};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Whisper::load("/absolute/path/ggml-base.en.bin")?;
    let transcription = Transcription::streaming(engine);
    let mut audio = RecordingReader::from_wav_file("speech.wav")?;
    let language: Language = "en".parse()?;
    let transcript = transcription.transcribe(&mut audio, &language)?;
    println!("{}", transcript.as_str());
    Ok(())
}
```

The selected language must be supported by the model.

## Project spellings

Recognition may spell a project name differently from how you write it.
Text layers let you correct those spellings after recognition.
Keep the model-loading line and replace `let transcription = ...` with:

```rust,no_run
# use aria_stt::{prelude::*, whisper::Whisper};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let engine = Whisper::load("/absolute/path/ggml-base.en.bin")?;
let transcription = Transcription::streaming(engine)
    .with(RemoveHesitations)
    .with(Vocabulary::new(["aria-stt", "Cargo.toml"])?);
# let _ = transcription;
# Ok(())
# }
```

Layers run in `.with(...)` order. `RemoveHesitations` removes a fixed set of
English fillers. `Vocabulary` can turn “aria dash stt” into `aria-stt` and
“cargo dot toml” into `Cargo.toml`. It corrects text after recognition; it does
not train or prompt the model, and it is not general fuzzy spelling correction.

With live transcription, layers process each finalized segment separately.
A phrase split across segments cannot be matched as one vocabulary term.

### Test your spellings

`Vocabulary` works without a model, so you can test it directly. Put this in
`tests/vocabulary.rs`:

```rust
use aria_stt::Vocabulary;

#[test]
fn vocabulary_formats_project_terms() {
    let vocabulary = Vocabulary::new(["aria-stt", "Cargo.toml"]).unwrap();

    assert_eq!(
        vocabulary
            .correct("aria dash stt and cargo dot toml")
            .as_deref(),
        Some("aria-stt and Cargo.toml")
    );
    assert_eq!(vocabulary.correct("unchanged"), None);
}
```

Run `cargo test`. `correct` returns `Some` when it changes text and `None`
when it leaves text alone. Add cases for your own terms and for ordinary text
that must stay unchanged.

To use the same transcription with microphone input, continue to
[Microphone transcription](./recording.md).
