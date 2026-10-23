# Privacy

Aria processes dictation and speech generation locally. Local processing does
not mean nothing is saved: dictation history is on by default, and transcripts
can also reach your clipboard, other apps, and files.

## Dictations

[History](./history.md#saving) explains saving and retention controls. Turning
saving off affects future dictations, not existing entries. Retained original
audio can include background voices, even when a voice filter excludes them
from the text.

Recording also uses temporary plaintext audio files. Aria deletes them when
it no longer needs them. File deletion is not secure erasure and
cannot remove backups or snapshots.

## Clipboard and apps

Copying exposes text to apps and clipboard managers that can read your
clipboard. Typing inserts it into the focused app, which controls its own
saving and sharing. Cancelling Aria cannot recall text already delivered.

On a Mac, Aria reads two characters before the cursor and one after it, to
decide where spaces go. It skips password fields, and never saves or logs
these characters.

## Questions

[Ask](./voice-chat.md) uses Claude Code or Codex. The agent may send the question
and project content to cloud services and retain its own conversations. Aria's
read-only agent settings restrict edits; they do not make the agent offline.

Starting a fresh Ask conversation does not delete earlier records. Clearing
Aria's dictation history does not delete agent conversations.

## Updates

The Mac app checks GitHub for a new release about once a day. The check sends
the request GitHub sees from any download, such as your IP address and the app
version. Turn it off in **Settings → General** with
**Automatically check for updates**.

## Files and folders

[System-audio transcripts](./listen.md#transcript-files) are separate files with
no automatic history expiry. Choose a folder with suitable permissions and
sharing settings; delete the files when you no longer need them.

[Project vocabulary](./project-spellings.md) scans the folder you choose. Its cache
contains collected names and source locations. Disabling corrections keeps
the cache; forgetting a folder removes its cached names.

Voice profiles store a numeric representation of enrolled speech locally.
They are recognition aids, not a way to authenticate a user or guarantee that
other voices are excluded.
