{{#title Aria settings and speech engines - Aria User Guide}}

# Settings

Aria keeps your settings in `~/.aria/settings.toml`. Change them in the Mac
app's Settings window, with `aria settings set`, or in a text editor. While an
Aria window is open, choose **File → Open Settings File** to edit the file.

## Mac app

Open **Settings…** from Aria's menu. The sidebar lists these pages:

- **General** controls launch at login, the status pill, the inline preview,
  captions, and the speaking rate. Open **Assistant** here to choose the app
  and model for Ask and Prompts. **Aria Setup** reopens setup.
- **Dictation** sets output, delivery, spoken corrections, auto-stop, the
  wake-word pause, and muting sound while recording.
- **Recognition** sets where transcription runs, the language, voice
  filtering, and project vocabulary.
- **Shortcuts** chooses right Option, Fn, or Off for the Aria key, and what
  starts Ask and the prompt list: an Aria key combo, a shortcut of your own, or
  nothing.
- **Prompts** lists Markdown files that say what to make of what you say,
  each with its own shortcut. See [Prompts](./prompts.md).
- **Privacy** controls dictation history, retained audio, and memory.
- **Phone** pairs Aria for iPhone.
- **About** shows the version and checks for updates.

The language picker offers English, the default, and automatic detection.
These choices still depend on the selected model's supported languages.

## Assistant

In **Settings → General → Assistant**, choose **App** (Claude Code or Codex)
and **Model**. [Install and sign in](./voice-chat.md#before-you-start) first.
Changes apply to the next Ask or Prompt in both the Mac app and terminal.

Each assistant remembers its own model. **Default** lets the assistant choose.
**Other…** accepts a model ID. An unavailable model causes an error, not a
switch to another model.

```sh
aria settings show assistant.id                     # show the assistant
aria settings set assistant.id codex                # choose codex or claude-code
aria assistant models                               # list model IDs
aria settings set assistant.models.codex "MODEL_ID"  # choose a model from the list
aria settings reset assistant.models.codex          # let the assistant choose
```

Add `--assistant codex` or `--assistant claude-code` to list the other
assistant's models. For a single question, use
[Ask's options](./voice-chat.md#use-a-different-assistant-or-model).

Without a choice, Aria uses Claude Code.

## Voice speed

For a default speaking rate, use **Settings → General → Spoken Content**, or
run:

```sh
aria settings set tts.rate 1.2
```

This makes speech 1.2 times the normal rate. To change one utterance, use
[`aria say --rate`](./tts.md#speed-and-voice).

## Server inference

Run the transcription model on a personal server while your desktop records
and delivers the text. The desktop needs no local transcription model.
See [Privacy](./privacy.md#server-inference) for what gets sent.

On the server, set a local transcription engine and its model in
`~/.aria/settings.toml`, then run:

```sh
aria serve \
  --bind 0.0.0.0:9851
```

In another server terminal, create an invitation. Replace `your-server`
with an address your device can reach:

```sh
aria remote invite \
  --endpoint https://your-server:9851
```

Keep the invitation private. It works once, within 60 seconds. Copy it, then
pair:

- **On a Mac:** in **Settings → Recognition**, click **Pair…**, then
  **Paste Invitation**. Check the address, click **Pair**, and set **Runs on**
  to **My server**.
- **From a terminal:** run `aria remote pair`, paste it, and press Ctrl-D.
  Then run `aria setup` and choose **Paired inference server**.

Either way, Aria restarts its service to apply the change. Dictation, recorded
questions, phone relay and history retries then use the server. Streaming and
previews depend on its engine. If the connection fails, the request stops
instead of switching to a local engine.

Speech output, wake words and voice profiles stay on the device. System-audio
listening needs its own local `listen.backend`. To go back, set **Runs on** to
**This Mac**, or choose a local engine in `aria setup`.

To unpair, set **Runs on** to **This Mac** and click **Forget…**, or run
`aria remote forget`. The server keeps trusting the device until you block it.

To block a device, run `aria remote revoke FINGERPRINT` on the server.
`aria remote pair` prints the fingerprint. In Settings, Control-click the
**This Mac:** line and choose **Copy Fingerprint**. If revoking reports a
storage error, fix it and revoke again before restarting the server.

## Apply changes

Most settings apply to the next dictation or question. Engines, model paths,
the phone port, and other keys marked `restart` apply after a restart. Check
the file and see each value:

```sh
aria settings check
aria settings show
aria settings show tts.rate
```

If the file has a mistake, Aria keeps your last good settings and names the
line. A service that's starting won't run until you fix it.

To change one key without an editor, or return it to its default:

```sh
aria settings set memory.enabled on
aria settings reset memory.enabled
```

A switch takes `on`, `off`, `true`, or `false`. Name a table's entry with a
dotted path, and quote a bundle ID:
`aria settings set 'application_overrides."com.apple.Terminal".excluded' on`.

Some keys only work together, so set them in one command:

```sh
aria settings set \
  stt.backend server \
  stt.server https://your-server:9851
```

In the Mac app, click **Restart to Apply**. In a terminal, restart the service:

```sh
aria service stop
aria service start
aria service status
```

Restart it too after installing a tool such as `claude`. On a Mac, Aria reads
your shell's `PATH` each time it starts. On Linux, the service keeps the `PATH`
it was installed with, so run `aria service install` again. Do the same if the
`aria` binary moves.

## Other settings

Use [the annotated settings file](https://github.com/Farhad-Shabani/aria/blob/main/settings.example.toml)
for the full reference. `aria settings schema` prints a JSON Schema for your
editor.
