{{#title Privacy, local speech, and saved data - Aria User Guide}}

# Privacy

Aria processes dictation and speech generation locally by default. Local processing does
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

## Server inference

When you [use a server](./settings.md#server-inference), your audio goes
to it for transcription. Whoever runs that server can hear the audio and read
the text. Pairing alone sends nothing; uploads start only when you switch
transcription to the server.

Your voice profile and corrections never leave your computer. **Only type my
voice** checks the speaker before upload when you deliver **After recording
stops** and when you retry from history. With **While speaking**,
audio leaves before Aria knows who spoke. The check helps recognition; it
cannot promise that other voices are left out.

The server keeps no history. It deletes its working copies when each
transcription ends, though crashes and backups can keep them. Encryption
protects audio on the way, not from the person running the server. Pairing
keys sit unencrypted in `~/.aria/inference/`, readable only by your account.

To stop uploads, set **Runs on** to **This Mac** in **Settings → Recognition**,
or choose a local engine in `aria setup`. Cancelling a dictation or revoking
your device stops what comes next. Neither can take back audio the server
already has.

## Clipboard and apps

Copying exposes text to apps and clipboard managers that can read your
clipboard. Typing inserts it into the focused app, which controls its own
saving and sharing. Cancelling Aria cannot recall text already delivered.

On a Mac, Aria reads two characters before the cursor and one after it, to
decide where spaces go. It skips password fields, and never saves or logs
these characters.

## Questions

[Ask](./voice-chat.md) sends your question to Claude Code or Codex. That
assistant may send the question and project content to cloud services. It
also keeps its own conversation records. Aria uses read-only settings to
limit file changes, but those settings do not prevent network access.

Starting a fresh Ask conversation does not delete earlier records. Clearing
Aria's dictation history does not delete assistant conversations.

## Prompts

[Prompts](./prompts.md) send your spoken words as text, along with the prompt
file's instructions, to Claude Code or Codex. The assistant may send that
text to cloud services.

Each run starts without a project, earlier conversation, or Aria memory.
Aria also disables the assistant's tools and leaves out its saved instructions
and settings. Prompt recordings are not kept in dictation history.

## Updates

The Mac app checks GitHub for a new release about once a day. The check sends
the request GitHub sees from any download, such as your IP address and the app
version. Turn it off in **Settings → About** with
**Automatically check for updates**.

## Files and folders

`~/.aria/settings.toml` holds engine and model choices, shortcuts, prompt names
and paths, and per-app choices. It holds no recordings, transcripts, or
credentials. Only your user account and administrators can read it. A paired
phone can change only language, output, voice filtering, project vocabulary,
and history settings.

[System-audio transcripts](./listen.md#transcript-files) are separate files with
no automatic history expiry. Choose a folder with suitable permissions and
sharing settings; delete the files when you no longer need them.

[Project vocabulary](./project-spellings.md) scans the folder you choose. Its cache
contains collected names and source locations. Disabling corrections keeps
the cache; forgetting a folder removes its cached names.

Voice profiles store a numeric representation of enrolled speech locally.
They are recognition aids, not a way to authenticate a user or guarantee that
other voices are excluded. A profile can still identify your voice, so treat
it as personal data. Backups of your home folder, such as Time Machine,
include it.
