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
- **Recognition** sets the language, voice filtering, and project vocabulary.
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
