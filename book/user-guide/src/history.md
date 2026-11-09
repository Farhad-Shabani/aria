{{#title Manage history and recordings - Aria User Guide}}

# History

Aria saves completed dictations so you can review their text. These controls
apply to both Mac and terminal dictation. History also lists each transcript
that [Transcription](./transcribe-system-audio.md) saves to the default folder,
and each request you send in a [chat](./chat.md), with its reply. The
assistant keeps its own conversation records.

## Saving

Saving is on by default. In the Mac app, turn it off under
**Settings → Privacy → Save history**, or run
`aria settings set history.enabled off`. This stops future saves and leaves
existing entries until you delete them or they expire.

Aria deletes old history on its own, so personal text doesn't pile up on
your Mac. It deletes text after 30 days. A transcript's entry keeps the first
KiB of its text, and the transcript file holds the rest. When the entry
expires, the transcript file stays.

By default, only recordings Aria couldn't transcribe keep their original
audio. Retained recordings last up to 24 hours, with a 128 MiB total limit.
Cancelled recordings and recordings with no accepted speech are not saved in
history.

## Review

On a Mac, [open Aria](./prompts.md#pick-a-prompt-in-aria). Your dictations,
transcripts, and chats show by day, each with its own icon. Type to search
them. Press **⌘↩** to read one. Click **Copy**, or press **⌘C**, to use the
text elsewhere. For a chat, Copy takes the reply. A long transcript shows its
first 1 MiB.

Press **Return** to type an entry's text into the app you came from. With no app
behind Aria, **Return** opens the entry instead. Text over 64 KiB is too long to
type, so copy it.

To see one section alone, click its icon under the search field, or press
**⌃Tab** to step through them. You can also type its name and choose
**Dictations**, **Transcripts**, or **Chats**. Search then covers only that
section. Click **All**, or press **Esc**, to see everything
again.

## Run a prompt on an entry

1. Select an entry, then press **Tab** or click **Run Prompt**.
2. Choose a prompt, or press its number. Aria shows the result.
3. Press **Return** to type the result into the app you came from, or press
   **⌘C** to copy it.

With an entry selected, press **⌘** and a prompt's number to run that
prompt on it at once.

To run instructions of your own once, type them in place of a prompt's name
and choose **Run “…”**. Aria writes from them, as **Write from them** does,
and names the result after their first line. To keep them as a prompt, press
**⌘S** on the result.

Press **Esc** on the result to cancel the run. If you close Aria instead, the
run goes on. While **Save history** is on, Aria still saves its result.

With history on, Aria saves each result under its entry. The entry's row
shows how many it has, such as **+3**. Click the count, or press **→**, to
show the results under the entry. Press **←** or **Esc** to hide them. Select
a result and press **Return** to type it, or **⌘↩** to read it.

An entry keeps its 20 newest results, and all results share 8 MiB of text.
Search doesn't cover results. Deleting an entry, or clearing history, deletes
its results too.

From a terminal, run:

```sh
aria history list
```

The list shows each entry's ID, section, status, recording availability
(available, expired, or unavailable), and the first 512 bytes of its text. To
list one section, add `--section dictation`, `--section transcription`, or
`--section chat`. To list only entries that mention something, add
`--query TEXT`. It matches the text, a chat's reply, the entry's name, and the
app's name, ignoring case. For scripts, add `--json` to print one JSON array.
Each entry's `section` is `dictation`, `transcription`, or `chat`. Each entry
also carries `created_at_ms` and `application`.

To print one entry's full text, or the prompt results saved under it, run:

```sh
aria history show 42
aria history results 42
```

## Dictate into history

To save a dictation without typing it anywhere, open Aria and choose
**New Dictation**. Speak, then tap the Aria key to stop. Aria saves the text
while **Save history** is on. From a terminal, run
`aria dictate --history-only`.

## Rename

From a terminal, give an entry a name by its ID:

```sh
aria history rename 42 "Weekly plan"
```

A name is one line of up to 256 bytes. Search matches it as well as the text.
To remove the name, run the command without one.

## Retry

If an entry still has its recording, open it in Aria and click **Transcribe
Again**. From a terminal, use the entry's ID. For example:

```sh
aria history retry 42
```

Retry replaces the entry's text without keeping the previous version. It does
not type or copy the result into another app. A failed retry leaves the
previous text in place.

Retries use the same [local or server setting](./settings.md#server-inference)
as dictation. Enabled voice filtering runs locally before upload. The
server must provide the requested engine. A failed retry leaves the saved
text unchanged and never falls back to a local model.

## Keep successful recordings

To make future successful dictations available for retry, enable
**Settings → Privacy → Keep audio for 24 hours**,
or run:

```sh
aria settings set history.keep_audio on
```

The same 24-hour and 128 MiB limits apply. Originals can include background
speech and voices excluded from the text.

To stop keeping future successful recordings:

```sh
aria settings set history.keep_audio off
```

Existing recordings continue to expire.

## Delete

In Aria, open an entry and click **Delete…** to remove it. To remove every
entry and retained recording, type “clear” and choose **Clear All History…**.
Clear includes entries outside the current search.

From a terminal, delete one entry by its ID, or clear everything:

```sh
aria history delete 42
aria history clear
```

Answer `y` or `yes` to confirm. Any other answer cancels. In a script, add
`--yes`. To clear one section, add `--section` as for `aria history list`.
Clearing can't be undone, and it leaves saving settings unchanged.

Deleting a transcript's entry, or clearing history, also deletes its
transcript file. Clearing history doesn't remove clipboard copies, the
assistant's conversation records, or transcript files that history doesn't
list. Deletion isn't secure erasure and can't remove backups. Read
[Privacy](./privacy.md) for the other places text can go.
