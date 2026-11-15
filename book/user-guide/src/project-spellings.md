{{#title Correct dictation with project vocabulary - Aria User Guide}}

# Project vocabulary

Aria can correct names from a folder you choose. For example, “aria dash stt”
can become `aria-stt`. This is optional and off by default.

## Choose a folder

Aria reads file and folder names, never file contents. It skips hidden
entries and anything a `.gitignore` inside the folder excludes. It keeps a
name only when the name contains `.`, `-` or `_`, such as `Cargo.toml`. The
name must also be short to say: six words at most, and one number of up to two
digits. So `vex16.rs` stays, and `icon-1024-dark.png` doesn't.
The names it keeps, and where they are, stay in a private local cache.

Complete [setup](./installation.md#setup) and keep the background service running.
In the Mac app, enable **Settings → Recognition → Use project vocabulary**
and choose the folder. Or open a terminal in that folder:

```sh
aria settings set dictation.vocabulary.enabled on
aria settings set dictation.vocabulary.dir "$PWD"
aria vocabulary status
```

Wait until status says its vocabulary is ready. If it is still preparing,
check again. Then dictate a project name and inspect its spelling. Ambiguous
names stay unchanged.

## Refresh

After adding or renaming files, refresh the names:

```sh
aria vocabulary refresh
aria vocabulary status
```

Refresh runs in the background. Aria does not watch the folder for changes.

## Which folder applies?

Terminal dictation uses the directory you run it from. Hotkey dictation uses
the folder in `dictation.vocabulary.dir`. A folder includes its descendants.
Aria doesn't search upward for a project root.

For a spoken request, `aria ask --project "$PWD"` looks up spellings in that
folder and gives the assistant the same folder to work in. The assistant may
send project content to cloud services. Read
[Chat with an assistant](./chat.md) first.

Transcription doesn't use project vocabulary. Live dictation corrects
each finalized segment separately, so a name split across segments may stay
unchanged.

## Disable or forget

Run `aria settings set dictation.vocabulary.enabled off` to stop corrections
while keeping cached names.
Use `aria dictate --no-project` to skip corrections for just one dictation.

To remove names cached for a folder:

```sh
aria vocabulary forget "$PWD"
```

This also removes overlapping parent and child caches. Disable corrections
first if you do not want a later refresh to recreate them.

If status says `partial`, Aria stopped at a limit or skipped a folder it
couldn't read. It kept the names nearest the top of the folder. To include
deeper names, choose a smaller folder. If status reports `TooLarge`, choose a
smaller folder too. For how Aria stores other content, read
[Privacy](./privacy.md).
