{{#title Aria settings and speech models - Aria User Guide}}

# Settings

Aria keeps your settings in `~/.aria/settings.toml`. Change them in the Mac
app's Settings window, with `aria settings set`, or in a text editor. While an
Aria window is open, choose **File → Open Settings File** to edit the file.

## Mac app

Open **Settings…** from Aria's menu. The sidebar lists these pages:

- **General** controls launch at login, the status pill, the inline preview,
  captions, and the speaking rate. Open **Assistant** here to choose the app
  and model for Chat and Prompts. **Aria Setup** reopens setup.
- **Dictation** sets output, delivery, text cleanup, auto-stop, and muting
  sound while recording.
- **Recognition** sets the language, voice filtering, project vocabulary,
  and where speech recognition runs.
- **Shortcuts** chooses right Option or Fn for the Aria key. It also sets
  what opens Aria and what starts a chat: an Aria key combo, a shortcut of your
  own, or nothing.
- **Prompts** lists Markdown files that say what to make of what you say,
  each with its own shortcut. See [Prompts](./prompts.md).
- **Privacy** controls history, retained audio, and memory.
- **Phone** pairs Aria for iPhone.
- **About** shows the version and checks for updates.

The language picker offers English, the default, and automatic detection.
These choices still depend on the selected model's supported languages.

## Assistant

In **Settings → General → Assistant**, choose **App** (Claude Code, Codex, or
Local model) and **Model**. [Install and sign in](./chat.md#before-you-start)
first, or [set up a local model](./chat.md#use-a-local-model).
Changes apply to the next chat or prompt in both the Mac app and terminal.

Each assistant remembers its own model. **Default** lets the assistant choose.
Local model has no default, so it needs a model.
**Other…** accepts a model ID. An unavailable model causes an error, not a
switch to another model.

```sh
aria settings show assistant.id                     # show the assistant
aria settings set assistant.id codex                # choose claude-code, codex, or local
aria assistant models                               # list model IDs
aria settings set assistant.models.codex "MODEL_ID"  # choose a model from the list
aria settings reset assistant.models.codex          # let the assistant choose
```

Add `--assistant codex` or `--assistant claude-code` to list the other
assistant's models. For a single request, use
[Chat's options](./chat.md#use-a-different-assistant-or-model).

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

Run the speech model on a personal server while your desktop records and
delivers the text. The desktop needs no local speech model.
See [Privacy](./privacy.md#server-inference) for what gets sent.

On the server, set a local speech model in `~/.aria/settings.toml`, then run
the command below. The server reads the setting once, so restart `aria serve`
after changing it.

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
  **Paste Invitation**. Check the address, click **Pair**, and turn on
  **Recognize speech on your server**.
- **From a terminal:** run `aria remote pair`, paste it, and press Ctrl-D.
  Then run `aria setup --server`.

Either way, the next dictation uses the server, with no restart. Dictation,
recorded requests, phone relay and history retries then use the server. Streaming and
previews depend on its engine. If the connection fails, the request stops
instead of switching to a local engine.

Speech output, wake words and voice profiles stay on the device.
Transcription needs its own local `transcription.backend`. To go back, turn off
**Recognize speech on your server**, or run `aria setup --local`. Your
local engine keeps its settings while the server recognizes speech. If its model
isn't on disk, Aria downloads it before switching back.

To unpair, turn off **Recognize speech on your server** and click **Forget…**, or run
`aria remote forget`. The server keeps trusting the device until you block it.
Pairing again also waits until speech recognition runs on your device.

To block a device, run `aria remote revoke FINGERPRINT` on the server.
`aria remote pair` prints the fingerprint. In Settings, **Forget…** can copy
the whole command. If revoking reports a
storage error, fix it and revoke again before restarting the server.

## Apply changes

Most settings apply to the next dictation or request, including the speech
model. Speech output engines, their model paths, the phone port, and other keys
marked `restart` apply after a restart. Check the file and see each value:

```sh
aria settings check
aria settings show
aria settings show tts.rate
```

For `dictation.delivery_mode`, `show` also says when dictation delivers after
recording stops anyway, such as with copy output or a model that can't type
while you speak.

If the file isn't valid, Aria keeps your last good settings. On a Mac,
**Finish Setting Up Aria** in Settings names the line when there is one. A
service that's starting won't run until you fix it.

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
  stt.backend parakeet-mlx \
  stt.interpreter /usr/bin/python3
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
