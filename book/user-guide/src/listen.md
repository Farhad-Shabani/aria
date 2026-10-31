{{#title Transcribe system audio on Mac - Aria User Guide}}

# System audio (Mac)

Listen saves a transcript of audio playing on your Mac, such as a meeting or
video. It captures system output, not microphone dictation, and writes to a
file rather than typing into an app.

Listen needs System Audio Recording permission. It uses your speech-to-text
engine, so the default setup needs no extra model. First check
[Mac dictation](./macos.md) or [terminal dictation](./transcription.md), depending
on how you installed Aria.

## Model

Listen runs on Parakeet, the default, and on Moonshine and Nemotron. With any
other engine, add the Moonshine Base model below for English audio. Setup
does not download it for you.

1. Download the model's three files:

   ```sh
   mkdir -p ~/.aria/models/moonshine/base
   cd ~/.aria/models/moonshine/base
   base=https://huggingface.co/onnx-community/moonshine-base-ONNX/resolve/b1e9b6aae3c3c7298f10c3798393fdf38e8fbbad
   curl -fLO "$base/onnx/encoder_model.onnx"
   curl -fLO "$base/onnx/decoder_model_merged.onnx"
   curl -fLO "$base/tokenizer.json"
   ```

2. Add this section to `~/.aria/settings.toml`, or edit its existing `listen`
   section:

```toml
[listen]
backend = "moonshine"
model = "~/.aria/models/moonshine/base"
transcripts_dir = "~/.aria/transcripts"
```

Check it with `aria settings check`. The next `aria listen start` reads it; no
restart is needed. Keep Aria's background service running.

## Record

You need two terminals: one to show the transcript and one to stop recording.

1. Start the audio you want to transcribe, then run:

   ```sh
   aria listen start \
     --follow
   ```

2. Allow **System Audio Recording** if macOS asks. The terminal prints
   `listening ->` and an output path when the request is accepted. Confirm
   that recognized text appears as the audio plays.
3. When finished, stop capture in a **second terminal**:

   ```sh
   aria listen stop
   aria listen status
   ```

4. Check that status says `not listening`, then open the file at the path
   printed when the session started.

**Ctrl-C stops the terminal display, not the recording.** Always use
`aria listen stop` to end capture. Use `aria listen follow` to show live text
again during a session.

## Transcript files

By default, files are saved under `~/.aria/transcripts`. To choose another path:

```sh
aria listen start \
  --output "$PWD/meeting.txt"
```

If the file exists, Listen appends to it. This command starts a session; do not
run it while another session is recording.

These files have no history expiry. Turning off dictation history does not
stop Listen from saving them, and clearing history does not delete them.
They do not get history's private-file protections, so delete them yourself
when you no longer need them.

[Voice profiles](./speaker-verification.md) can give speakers names. For
permission or model failures, see [Troubleshooting](./troubleshooting.md#system-audio-fails).
