{{#title Transcribe system audio on your Mac - Aria User Guide}}

# Transcribe system audio (Mac)

Transcription saves a transcript of audio playing on your Mac, such as a
meeting or video. It records what your Mac plays, not your microphone, and
writes to a file.

Transcription needs System Audio Recording permission. First set up
[Mac dictation](./macos.md) or [terminal dictation](./terminal-dictation.md),
depending on how you installed Aria.

## Record

You need two terminals: one shows the transcript, the other stops it.

1. Start the audio you want to transcribe, then run:

   ```sh
   aria transcribe start --follow
   ```

2. Allow **System Audio Recording** if macOS asks. The terminal prints
   `transcribing ->` and the file's path. You should see text appear as the
   audio plays.
3. When you're done, stop it in a second terminal:

   ```sh
   aria transcribe stop
   aria transcribe status
   ```

4. Check that status says `not transcribing`, then open the file.

**Ctrl-C stops the display, not the recording.** Always end with
`aria transcribe stop`. To show live text again, run `aria transcribe follow`.

To start from the app, [open Aria](./prompts.md#pick-a-prompt-in-aria) and
choose **Transcribe System Audio**. Choose **Live Transcript** to see the text.
While a transcription runs, the Aria menu shows **Transcribing your Mac's
audio**. Click the stop button beside it to end it.

## Transcript files

Aria saves each transcript in `~/.aria/transcripts`, a folder only Aria
writes to. With history on, a transcript appears in [history](./history.md)
as soon as it starts, and stays until you delete it. Deleting that entry, or
clearing history, deletes its file.

To find the folder, choose **Settings → Privacy**, then go to **Transcripts
folder**, and click **Open**. Don't move or edit the files there. To keep a
copy of a saved transcript elsewhere, choose **Show in Finder** on its history
entry, then copy the file. A transcript an older Aria saved elsewhere stays
where it is, and history keeps it.

With history off, or with history full, a transcript waits unsaved in
`~/.aria/unsaved`. When it ends, Aria lists it as **Unsaved Transcript**, with
these choices:

- **Save…** (**⌘S**) saves it where you choose, outside Aria's folders, as
  your own file. History doesn't list it.
- **Discard…** deletes it, once you confirm. While history can't open,
  Discard waits until it can.

If Aria quits before you choose, it lists the transcript again the next time it
opens. While history can't open, an unsaved transcript keeps the speaker labels
Aria gave while listening. A transcript that heard nothing leaves no file.

To save to a file of your own instead:

```sh
aria transcribe start \
  --output "$PWD/meeting.txt"
```

If the file exists, Transcription appends to it. Transcription only ever
appends to this file, so it keeps the speaker labels it gave while listening,
and text another app adds stays. Don't run this while another transcription
is running. A file you name with `--output` never appears in
history, and Aria never moves or deletes it.

Transcript files are plain text. Aria doesn't encrypt them as it does history,
so delete them when you no longer need them.

## Model

Transcription uses your speech model. If it can't run that model, it uses
Parakeet. If Parakeet isn't on your Mac either, add the Moonshine Base model
for English audio. Setup doesn't download it for you.

1. Download the model's three files:

   ```sh
   mkdir -p ~/.aria/models/moonshine/base
   cd ~/.aria/models/moonshine/base
   base=https://huggingface.co/onnx-community/moonshine-base-ONNX/resolve/b1e9b6aae3c3c7298f10c3798393fdf38e8fbbad
   curl -fLO "$base/onnx/encoder_model.onnx"
   curl -fLO "$base/onnx/decoder_model_merged.onnx"
   curl -fLO "$base/tokenizer.json"
   ```

2. Add this section to `~/.aria/settings.toml`, or edit the `transcription`
   section there:

   ```toml
   [transcription]
   backend = "moonshine"
   model = "~/.aria/models/moonshine/base"
   ```

3. Run `aria settings check`. The next transcription uses the model, with no
   restart.

[Voice profiles](./speaker-verification.md) can name the speakers. For
permission or model failures, see
[Troubleshooting](./troubleshooting.md#transcription-fails).
