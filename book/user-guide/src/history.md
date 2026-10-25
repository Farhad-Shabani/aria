{{#title Manage dictation history and recordings - Aria User Guide}}

# Dictation history

Aria saves completed dictations so you can review their text. These controls
apply to both Mac and terminal dictation. Ask conversations and system-audio
transcripts are separate.

## Saving

Saving is on by default. In the Mac app, turn it off under
**Settings → Privacy → Save dictation history**. This stops future saves and
leaves existing entries until you delete them or they expire. The CLI has no
control for this preference.

Text is kept for up to 30 days, with limits of 1,000 entries and 8 MiB total.
Older entries can disappear sooner when those limits are reached.

By default, only failed transcriptions keep their original audio. Retained
recordings last up to 24 hours, with a 128 MiB total limit. Cancelled recordings
and recordings with no accepted speech are not saved in history.

## Review

On a Mac, choose **Dictation History…** from Aria's menu. Search for an entry,
then select it to read the text. **Copy text** copies it for use elsewhere.

From a terminal, run:

```sh
aria history list
```

The list shows each entry's ID, status, recording availability (available,
expired, or unavailable), and its full text.
For scripts, add `--json` to print one JSON array. Each entry also carries
`created_at_ms` and `application`.

## Retry

If an entry still has its recording, choose **Retry transcription** in the
Mac app. From a terminal, use the entry's ID; for example:

```sh
aria history retry 42
```

Retry replaces the entry's text without keeping the previous version. It does
not type or copy the result into another app. A failed retry leaves the
previous text in place.

Retries use the same [local or server setting](./configuration.md#server-inference)
as dictation. Enabled voice filtering runs locally before upload. The
server must provide the requested engine. A failed retry leaves the saved
text unchanged and never falls back to a local model.

## Keep successful recordings

To make future successful dictations available for retry, enable
**Settings → Privacy → Keep recordings for retry**,
or run:

```sh
aria history retain-audio true
```

The same 24-hour and 128 MiB limits apply. Originals can include background
speech and voices excluded from the transcript.

To stop keeping future successful recordings:

```sh
aria history retain-audio false
```

Existing recordings continue to expire.

## Delete

In the Mac app, select **Delete…** for one entry or **Clear History…** for all
entries and retained recordings. Clear includes entries outside the current
search.

From a terminal:

```sh
aria history clear
```

Answer `y` or `yes` to confirm; any other answer cancels. Clearing cannot be
undone, and it leaves saving settings unchanged.

Clearing history does not remove clipboard copies, Ask conversations, or
system-audio transcript files. Deletion is not secure erasure and cannot
remove backups. Read [Privacy](./privacy.md) for the other places text can go.
