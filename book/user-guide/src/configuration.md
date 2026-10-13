# Settings

Use the Mac app's Settings window for everyday choices. Use
`~/.aria/config.yaml` for engines, model paths, and service options.
App preference changes apply immediately; file changes require a restart.

## Mac app

Open **Settings…** from Aria's menu:

- **General** controls launch at login, the status pill, and dictation-history
  saving.
- **Shortcuts** chooses right Option or Fn for the dictation hotkey, and sets
  the Ask shortcut.
- **Dictation** selects language and output, and can mute your speakers while
  recording. **Show Advanced Options** contains delivery, voice filtering,
  project vocabulary, stopping, and retained audio.

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

Starting a service that is already running does not reload it. If the binary
moved, or an agent was installed outside the service's saved `PATH`, run
`aria service install` to refresh registration.

## Other settings

Use [the annotated configuration file](https://github.com/Farhad-Shabani/aria/blob/main/config.example.yaml)
for the full reference. Unknown fields are rejected. App preferences such as
recognition language and history saving do not belong in `config.yaml`.

Environment variables affect only your terminal, not the background service.
