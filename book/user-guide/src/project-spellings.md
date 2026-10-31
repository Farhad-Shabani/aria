{{#title Correct dictation with project vocabulary - Aria User Guide}}

# Project vocabulary

Aria can correct names from a folder you choose. For example, “aria dash stt”
can become `aria-stt`. This is optional and off by default.

## Choose a folder

Aria reads file and folder names and names found in local text. A `.gitignore`
file does not prevent Aria from reading those files. Aria skips hidden files
but does not check other files for secrets.
Choose a folder you are comfortable letting Aria read. Names and their source
locations stay in a private local cache.

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

For a spoken question, `aria ask --project "$PWD"` looks up spellings in that
folder and gives the assistant the same folder to work in. The assistant may
send project content to cloud services; read [Ask a question](./voice-chat.md)
first.

System-audio transcription does not use project vocabulary. Live dictation corrects
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

If status reports `TooLarge`, choose a smaller folder. For how Aria stores
other content, read [Privacy](./privacy.md).
