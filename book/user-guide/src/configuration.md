{{#title Configure Aria settings and speech engines - Aria User Guide}}

# Settings

Use the Mac app's Settings window for everyday choices. Use
`~/.aria/config.yaml` for engines, model paths, and service options.
App preference changes apply immediately; file changes require a restart.

## Mac app

Open **Settings…** from Aria's menu. The sidebar lists these pages:

- **General** controls launch at login, the status pill, the inline preview,
  and captions. It also reopens setup.
- **Dictation** sets output, delivery, muting while recording, and the
  silence that ends dictation.
- **Recognition** sets where transcription runs, the language, voice
  filtering, and project vocabulary.
- **Shortcuts** chooses right Option or Fn for the dictation hotkey, and sets
  the Ask shortcut.
- **Prompts** lists Markdown files that say what to make of what you say,
  each with its own shortcut. See [Prompts](./prompts.md).
- **Privacy** controls dictation history and retained audio.
- **Phone** pairs Aria for iPhone.
- **About** shows the version and checks for updates.

The language picker offers English, the default, and automatic detection.
These choices still depend on the selected model's supported languages.

## Voice speed

For a default speaking rate, open `~/.aria/config.yaml` in a text editor.
If it already has a `tts` section, edit that section rather than adding another:

```yaml
tts:
  rate: 1.2
```

This makes speech 1.2 times the normal rate.
For a change to just one utterance, use [`aria say --rate`](./tts.md#speed-and-voice).

## Server inference

Run the transcription model on a personal server while your desktop records
and delivers the text. The desktop needs no local transcription model.
See [Privacy](./privacy.md#server-inference) for what gets sent.

On the server, configure a local transcription engine and its model in
`~/.aria/config.yaml`, then run:

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

Keep the invitation private; it works once, within 60 seconds. Copy it,
then pair:

- **On a Mac:** in **Settings → Recognition**, click **Pair…**, then
  **Paste and Pair**. Set **Runs on** to **My server**.
- **From a terminal:** run `aria remote pair`, paste it, and press Ctrl-D.
  Then run `aria setup` and choose **Paired inference server**.

Either way, Aria restarts its service to apply the change. Dictation, recorded
questions, phone relay and history retries then use the server. Streaming and
previews depend on its engine. If the connection fails, the request stops
rather than switching to a local engine.

Speech output, wake words and voice profiles stay on the device. System-audio
listening needs its own local `listen.backend`. To go back, set **Runs on** to
**This Mac**, or choose a local engine in `aria setup`.

To block a device, run `aria remote revoke FINGERPRINT` on the server.
`aria remote pair` prints the fingerprint; in Settings, Control-click the
**This Mac:** line and choose **Copy Fingerprint**. If revoking reports a
storage error, fix it and revoke again before restarting the server.

## Apply changes

After editing the file, inspect what your terminal would load:

```sh
aria config show
```

For the example above, find `tts.rate` and check that it is `1.2`. The output
shows each setting's source. It describes this shell's configuration, not a
snapshot of the running service.

Restart the registered service to apply file changes:

```sh
aria service stop
aria service start
aria service status
```

Starting a service that is already running does not reload it.

Restart it too after installing a tool such as `claude`. On a Mac, Aria reads
your shell's `PATH` each time it starts. On Linux, the service keeps the `PATH`
it was installed with, so run `aria service install` again. Do the same if the
`aria` binary moves.

## Other settings

Use [the annotated configuration file](https://github.com/Farhad-Shabani/aria/blob/main/config.example.yaml)
for the full reference. Unknown fields are rejected. App preferences such as
recognition language and history saving do not belong in `config.yaml`.

Environment variables affect only your terminal, not the background service.
