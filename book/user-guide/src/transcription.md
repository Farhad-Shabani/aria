{{#title Dictate from the terminal on macOS and Linux - Aria User Guide}}

# Terminal dictation

Use `aria dictate` to record one utterance and copy the recognized words.
Complete [setup](./installation.md#setup) and keep the background service
running. On Linux, the service also keeps copied text available for pasting.
It runs with your graphical desktop session so Aria can reach the clipboard.

## Copy a sentence

```sh
aria dictate \
  --mode copy
```

Say a short sentence, then pause. By default, 1.5 seconds of silence ends the
recording. After processing finishes, paste into an empty text field and check
the words.

Copying replaces the clipboard.

Plain `aria dictate` uses your saved output setting. `--mode copy` chooses
the clipboard for this recording without changing that setting.

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
aria settings show dictation.auto_stop_ms       # show the setting
aria settings set dictation.auto_stop_ms 20000  # stop after 20 seconds of silence
aria settings set dictation.auto_stop_ms 0      # stop on Esc or a second run
```

Recordings stop after ten minutes at most.

## Insert into an app

On a Mac, [hotkey dictation](./macos.md#try-it) is the easiest way to speak into
another app. A command launched in Terminal starts with Terminal focused.
Insertion requires Accessibility access, and the intended app and field must
stay focused.

`--mode insert` types the completed transcript. `--stream` types finalized
segments while you speak. With Copy output or a speech model that can't
stream, Aria delivers the whole text once you stop instead. If you switch to
another app or field, Aria keeps recording. It types what you said once you're back in that field. Clicking or
typing in that field stops the dictation, in apps that report their text
cursor.

`--mode insert-or-copy` copies the transcript when insertion can't begin. If
you stop while away, it copies the words Aria hasn't typed yet. `--mode insert`
keeps those words only in [history](./history.md#saving), when saving is on.

On Linux, [hotkey dictation](./linux.md#try-it) pastes into the focused app.
`--mode insert` pastes the completed transcript. `--stream` waits too, since
Aria pastes once recording stops.

To save the transcript to [history](./history.md#dictate-into-history) without
typing it, add `--history-only`.

On a Mac, Aria adds a space where the text needs one. Dictating “Hello.” and then
“How are you” types “Hello. How are you”.

## Clean up text

To tidy what you say, turn on **Settings → Dictation → Clean up text**, or
run:

```sh
aria settings set dictation.cleanup.enabled on
```

Aria then drops “um”, “uh”, and repeated words, with either delivery
setting.

With **Deliver** set to **After recording stops**, Aria also applies spoken
corrections to the completed text. “Meet me at 3, actually 4” becomes “meet
me at 4”. “Send it to Ali, scratch that, to Sara” becomes “Send it to Sara”.
This handles only simple, unambiguous corrections within a clause, in
English. More complex corrections stay as spoken.

**Clean up text** turns all of these on or off. To turn off one of them, set it
in your settings file:

```toml
[dictation.cleanup]
enabled = true
fillers = true      # drops "um", "uh", and repeated words
backtrack = false   # keeps spoken corrections as you said them
```

### Use a local model

A local language model can also fix punctuation, capitals, and spoken
corrections. You run the model in a server on your own Mac, such as
[Ollama](https://ollama.com). Aria sends the text only to that server.

**A cloud model sends your text off your Mac.** Pick a model that runs on
your Mac, such as `qwen2.5:3b`:

```sh
ollama pull qwen2.5:3b
```

Then add these lines to your settings file, and restart Aria:

```toml
[dictation.cleanup.llm]
endpoint = "http://127.0.0.1:11434/v1/chat/completions"
model = "qwen2.5:3b"
```

The model runs only when both of these are true:

- **Clean up text** is on.
- **Deliver** is set to **After recording stops**.

To turn the model off and keep these lines, add `enabled = false` under
`[dictation.cleanup.llm]`.

A smaller model, such as `qwen2.5:1.5b`, misses most spoken corrections.

If the model doesn't answer within 3 seconds, or adds words you didn't say,
Aria types the text without it. To check the setup, run `aria doctor` and
look for the `cleanup model` line.

For names Aria regularly spells differently, try [Project vocabulary](./project-spellings.md).
If recording fails, use [Troubleshooting](./troubleshooting.md).
