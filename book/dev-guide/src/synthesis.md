# Generate speech

Use the system voice when you want text read aloud without downloading a model.
Use Kokoro when your application needs audio samples for its own player or file
writer. Start with the example that matches the output you need.
Both examples use the [synthesis dependencies](./workflow.md#speech-output).

## System voice

Enable the `system` feature of `aria-tts`, as shown in setup.
On macOS, this uses `say`. On Linux, install `espeak-ng` or `espeak` at its
standard `/usr/bin` location.

Replace `src/main.rs` with:

```rust,no_run
use aria_tts::{prelude::*, system::SystemVoice};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let voice = SystemVoice::new()?;
    voice.speak(
        &SpeechText::new("Hello from Aria.")?,
        None,
        SpeakingRate::standard(),
        &CancelToken::never(),
    )?;
    Ok(())
}
```

Run `cargo run --release`. You should hear the sentence through the system
voice. The call returns after playback finishes or fails. `None` selects the
platform's default voice; `SpeakingRate::standard()` speaks at 175 words per
minute.
`CancelToken::never()` lets this example run without a cancellation signal.

The system voice runs in a child process. It needs no `playback` feature and
provides no raw audio samples (PCM), so it cannot generate an audio file.

## Kokoro audio

Enable `kokoro` instead of `system` in the `aria-tts` dependency. Download the
model and voices from
[Kokoro's model-files release](https://github.com/thewh1teagle/kokoro-onnx/releases/tag/model-files-v1.0):

```sh
curl -fLO https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/kokoro-v1.0.onnx
curl -fLO https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/voices-v1.0.bin
```

The voices file is a NumPy archive despite its `.bin` extension.

This example collects generated samples and prints their count. Replace
`src/main.rs` with the following, using the absolute paths to your two files:

```rust,no_run
use aria_tts::{kokoro::Kokoro, prelude::*};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Kokoro::load(
        "/absolute/path/kokoro-v1.0.onnx",
        "/absolute/path/voices-v1.0.bin",
    )?;
    let speech = Synthesis::new(engine);
    let mut session = speech.start(&SpeechText::new("Hello from Aria.")?)?;
    let sample_rate = session.sample_rate();
    let mut samples = Vec::new();
    let mut buffer = [0.0; 1024];

    loop {
        let count = session.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        samples.extend_from_slice(&buffer[..count]);
    }

    println!("Generated {} samples at {sample_rate} Hz", samples.len());
    Ok(())
}
```

Run `cargo run --release`. This prints the sample count and sample rate; it
does not play the audio. Kokoro supplies mono `f32` PCM at 24 kHz and uses
US English text processing. Its standard voices archive includes the default
`af_heart` voice.

`read()` waits for samples, completion, or an error. A zero count
means successful completion; an error means generation failed. Consume only
`buffer[..count]`. To avoid storing the whole utterance, send each block to
your player or file writer.

Dropping the session requests cancellation and waits for its thread to stop.
The engine may take time to stop, so keep that cleanup off the UI thread.

## Kokoro playback

Run `cargo add aria-kit --features playback`. It adds `aria-kit`, or enables
the feature on an existing entry.
Keep the Kokoro-loading and `Synthesis::new` lines from the previous example.
Replace the sample-reading code, from `let mut session` through `println!`, with:

```rust,no_run
# use aria_tts::{kokoro::Kokoro, prelude::*};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let engine = Kokoro::load("/absolute/path/kokoro-v1.0.onnx", "/absolute/path/voices-v1.0.bin")?;
# let speech = Synthesis::new(engine);
speech.speak(&SpeechText::new("Hello from Aria.")?)?;
# Ok(())
# }
```

This plays through the default output device and returns when the utterance
finishes. Linux playback also needs ALSA headers and pkg-config at build time.

## Text and voices

`SpeechText::new` trims input and rejects empty text or text over 8 KiB.
`Voice::new` validates a name's form; whether it exists depends on the engine.
`SpeakingRate::new` accepts a finite multiplier from 0.25 to 4.0.

For Kokoro, `start_with` lets you choose a voice and speaking rate and supply a
cancellation token. Use names from your voices archive. Keep model loading
separate from each utterance, as explained in [Engines and sessions](./architecture.md).
