{{#title Dictate from the terminal on macOS and Linux - Aria User Guide}}

# Terminal dictation

Use `aria dictate` to record one utterance and copy the recognized words.
Complete [setup](./installation.md#setup) and keep the background service
running. On Linux, the service also keeps copied text available for pasting.
Run it in your graphical desktop session so Aria can access the clipboard.

## Copy a sentence

```sh
aria dictate \
  --mode copy
```

Say a short sentence, then pause. By default, 1.5 seconds of silence ends the
recording. After processing finishes, paste into an empty text field and check
the words.

Copying replaces the clipboard.

Plain `aria dictate` uses your saved output preference. `--mode copy` chooses
the clipboard for this recording without changing that preference.

## Stop by hand

For a longer pause or a recording you want to end yourself, run:

```sh
aria dictate \
  --mode copy \
  --toggle
```

Speak, then press **Esc** when the terminal shows the stop prompt. Aria stops
recording, finishes transcription, and copies the result.

Without `--toggle`, silence ends the recording. With it, **Auto-stop** still
ends the recording once you stop speaking, after 10 seconds by default. Change
it under **Settings → Dictation**, or from the terminal:

```sh
aria dictate auto-stop       # show the setting
aria dictate auto-stop 20    # stop after 20 seconds of silence
aria dictate auto-stop off   # stop on Esc or a second run
```

Recordings stop after ten minutes at most.

## Mac insertion

On a Mac, [hotkey dictation](./macos.md#try-it) is the easiest way to speak into
another app. A command launched in Terminal starts with Terminal focused.
Insertion requires Accessibility access, and the intended app and field must
stay focused.

`--mode insert` types the completed transcript. `--stream` types finalized
segments while you speak. Switching apps stops it. So does clicking or typing,
in apps that report their text cursor. What you said so far stays in
[history](./history.md#saving) when saving is on. Use `--mode insert-or-copy`
if you want copying when insertion cannot begin. Linux uses the clipboard and
does not support insertion-only modes.

Aria adds a space where the text needs one. Dictating “Hello.” and then
“How are you” types “Hello. How are you”.

## Spoken corrections

To correct a simple change of mind in the completed text, add this setting to
`~/.aria/config.yaml` and [restart the service](./configuration.md#apply-changes):

```yaml
dictation_backtrack: true
```

For example, “meet me at 3, actually 4” can become “meet me at 4”.
“Send it to Ali, scratch that, to Sara” can become “Send it to Sara”.
This handles only simple, unambiguous corrections within a clause. More complex
corrections stay as spoken.

Corrections apply to the completed transcript. For Mac hotkey dictation, set
**Settings → Dictation → Deliver** to **After recording stops**. Live insertion
cannot retract earlier text, so it keeps the original words and correction cue.

For names Aria regularly spells differently, try [Project vocabulary](./project-spellings.md).
If recording fails, use [Troubleshooting](./troubleshooting.md).
