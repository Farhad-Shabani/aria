{{#title Transcribe system audio on your Mac - Aria User Guide}}

# Transcribe system audio (Mac)

Transcription saves a transcript of audio playing on your Mac, such as a
meeting or video. It captures what your Mac plays, not your microphone, and
writes to a file instead of typing into an app.

Transcription needs System Audio Recording permission. It uses your
speech model, so the default setup needs no extra model. First check
[Mac dictation](./macos.md) or [terminal dictation](./terminal-dictation.md),
depending on how you installed Aria.

## Model

Transcription uses your speech model. If Transcription can't run that model,
it uses Parakeet. If Parakeet isn't on your Mac, add the Moonshine Base model
below for English audio. Setup doesn't download it for you.

1. Download the model's three files:

   ```sh
   mkdir -p ~/.aria/models/moonshine/base
   cd ~/.aria/models/moonshine/base
   base=https://huggingface.co/onnx-community/moonshine-base-ONNX/resolve/b1e9b6aae3c3c7298f10c3798393fdf38e8fbbad
   curl -fLO "$base/onnx/encoder_model.onnx"
   curl -fLO "$base/onnx/decoder_model_merged.onnx"
   curl -fLO "$base/tokenizer.json"
   ```

2. Add this section to `~/.aria/settings.toml`, or edit its existing
   `transcription` section:

```toml
[transcription]
backend = "moonshine"
model = "~/.aria/models/moonshine/base"
transcripts_dir = "~/.aria/transcripts"
```

Check it with `aria settings check`. The next `aria transcribe start` reads it,
with no restart. Keep Aria's background service running. Aria also reads
`listen.*` keys and renames them to `transcription.*` the next time it saves
the file.

## Record

You need two terminals: one to show the transcript and one to stop recording.

1. Start the audio you want to transcribe, then run:

   ```sh
   aria transcribe start \
     --follow
   ```

2. Allow **System Audio Recording** if macOS asks. The terminal prints
   `transcribing ->` and an output path when the request is accepted. Confirm
   that recognized text appears as the audio plays.
3. When finished, stop capture in a **second terminal**:

   ```sh
   aria transcribe stop
   aria transcribe status
   ```

4. Check that status says `not transcribing`, then open the file at the path
   printed when the transcription started.

**Ctrl-C stops the terminal display, not the recording.** Always use
`aria transcribe stop` to end capture. Use `aria transcribe follow` to show
live text again while a transcription runs.

On a Mac, you can also open Aria and choose **Transcribe System Audio**. While a
transcription runs, choose **Live Transcript** to see its text. The Aria menu
shows **Transcribing your Mac's audio** while a transcription runs, whatever
started it. Click the stop button beside it to end capture.

## Transcript files

By default, Aria saves files under `~/.aria/transcripts`. To choose another
path:

```sh
aria transcribe start \
  --output "$PWD/meeting.txt"
```

If the file exists, Transcription appends to it. This command starts a
transcription. Don't run it while another transcription is recording.

With history on, a transcript saved under the default folder also appears
in [history](./history.md) for up to 30 days. Deleting that entry, or clearing
history, deletes its file. When the entry expires, the file stays. A file you
name with `--output` never appears in history. Turning off history doesn't stop
Transcription from saving files. They don't get history's private-file
protections, so delete them when you no longer need them.

[Voice profiles](./speaker-verification.md) can give speakers names. For
permission or model failures, see
[Troubleshooting](./troubleshooting.md#transcription-fails).
