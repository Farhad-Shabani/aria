# Read text aloud

Use `aria say` to play text through your configured voice. Complete
[setup](./installation.md#setup) first. On Linux, keep the registered
background service running.

## Try it

```sh
aria say "Hello from Aria."
```

You should hear the sentence from your speakers. The command finishes when
playback ends. A new utterance replaces the one playing.

To stop speech, run this in another terminal:

```sh
aria say \
  --stop
```

## Speed and voice

For a faster voice on one request:

```sh
aria say \
  --rate 1.2 \
  "A little faster."
```

The rate is a multiplier between 0.25 and 4. Use `--voice` for a voice supported
by the selected engine. To list installed system voices on macOS:

```sh
say \
  -v '?'
```

Use a name from that list. For example, if Samantha is available:

```sh
aria say \
  --voice Samantha \
  "Hello."
```

These options affect one utterance. To change the default rate, use
[Settings](./configuration.md#voice-speed).

## System voice

The system engine uses macOS's installed voices, or `/usr/bin/espeak-ng` or
`/usr/bin/espeak` on Linux. It needs no downloaded model. If a Linux system
voice is unavailable, setup can offer Kokoro instead.

## Kokoro

Kokoro generates speech locally from a model and voices file. Run `aria setup`
and select Kokoro to download both. It uses English text processing.

If you already have the model and voices file, edit the existing `tts` section
in `~/.aria/config.yaml`, using their actual paths. Setup's default paths are:

```yaml
tts:
  backend: kokoro
  model: ~/.aria/models/kokoro/kokoro.onnx
  voices: ~/.aria/models/kokoro/voices.bin
  voice: af_heart
  rate: 1.0
```

[Restart the service](./configuration.md#apply-changes), then try the first
sentence again. Voice names must exist in the downloaded voices file.

If you want the system voice when Kokoro cannot initialize, add
`fallback: system` to that `tts` section. Fallback is off by default and applies
when the engine or output cannot start.

If nothing plays, follow [speech troubleshooting](./troubleshooting.md#no-speech-output).
