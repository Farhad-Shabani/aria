{{#title Correct dictation with project vocabulary - Aria User Guide}}

# Project vocabulary

Aria can correct names from a folder you choose. For example, “aria dash stt”
can become `aria-stt`. This is optional and off by default.

## Choose a folder

Aria reads file and folder names in that folder, never file contents. It
skips hidden files and folders, and anything a `.gitignore` inside the folder
excludes. It keeps a name only when it contains `.`, `-` or `_` and is short
to say. So Aria keeps `Cargo.toml` and `vex16.rs`, and skips `README` and
`icon-1024-dark.png`. It stores the names it keeps, and their paths, in
`~/.aria/vocabulary` on your computer.

Complete [setup](./installation.md#setup) and keep the background service
running. Then choose the folder in the Mac app or in a terminal.

On a Mac:

1. Open **Settings → Recognition**.
2. Turn on **Use project vocabulary**.
3. Next to **Folder**, click **Choose…** and choose the folder.

In a terminal in that folder, run:

```sh
aria settings set dictation.vocabulary.enabled on
aria settings set dictation.vocabulary.dir "$PWD"
aria vocabulary status
```

You should see a line that starts with `Ready`, such as
`Ready: 784 of 784 names`. For other results, see
[Read the status](#read-the-status). Then dictate a name from the folder and
check its spelling. For example, “cargo dot toml” types `Cargo.toml`.

If two names sound the same, such as `aria-stt` and `aria_stt`, Aria corrects
neither.

## Refresh the names

After you add or rename files, refresh the names:

```sh
aria vocabulary refresh
aria vocabulary status
```

Refresh runs in the background. Aria doesn't watch the folder for changes.

## Which folder applies

`aria dictate` uses the directory you run it from. Dictation with the Aria key
uses the folder in `dictation.vocabulary.dir`. A folder includes the folders
inside it. Aria doesn't search upward for a project root.

For a spoken request, `aria ask --project "$PWD"` looks up names in that
folder. It also gives the assistant that folder to work in. The assistant may
send project content to cloud services. Read
[Chat with an assistant](./chat.md) first.

Transcription doesn't use project vocabulary. Live dictation types what you
say in parts and corrects each part on its own. A name split across two parts
may stay unchanged.

## Read the status

`aria vocabulary status` prints how many names Aria uses, out of the names it
found.

| Status shows | Meaning | What to do |
|---|---|---|
| `Ready` | Aria corrects names from the folder. It uses at most 2,048, nearest the top of the folder first. | Nothing. |
| `Refreshing` | Aria is reading the folder. | Run `aria vocabulary status` again. |
| `partial` | Aria stopped at a limit, or skipped a folder it couldn't read. It kept the names nearest the top. | To include deeper names, choose a smaller folder. |
| `TooLarge` | The folder's names don't fit Aria's limits. | Choose a smaller folder. |

## Turn off or forget

To stop corrections but keep the stored names, run
`aria settings set dictation.vocabulary.enabled off`. To skip corrections for
one dictation, run `aria dictate --no-project`.

To remove the names stored for a folder:

```sh
aria vocabulary forget "$PWD"
```

This also removes names stored for folders inside it and for folders that
contain it. A later refresh stores them again, so turn off project vocabulary
first if you don't want that. Removing names isn't secure erasure. For how Aria
stores other content, read [Privacy](./privacy.md).
