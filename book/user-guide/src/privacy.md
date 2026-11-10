{{#title Privacy, local speech, and saved data - Aria User Guide}}

# Privacy

Aria processes dictation and speech generation locally by default. Local processing does
not mean nothing is saved: history is on by default, and text
can also reach your clipboard, other apps, and files.

## Dictations

[History](./history.md#saving) explains saving and retention controls. Turning
saving off affects future dictations, not existing entries. Retained original
audio can include background voices, even when a voice filter excludes them
from the text.

Recording also uses temporary plaintext audio files. Aria deletes them when
it no longer needs them. File deletion is not secure erasure and
cannot remove backups or snapshots.

On a Mac, **Mute while recording** asks macOS to silence other apps. Aria
doesn't record, save, or send their sound.

## Text cleanup with a local model

When you [set a local model](./terminal-dictation.md#use-a-local-model),
Aria sends the completed text of each dictation to that server. Aria connects
only to an address on your own Mac. The server decides where the text goes
next. A proxy or a cloud model, such as Ollama's cloud models, sends it off
your Mac. Any program running under your account could listen on that port and
read the text. What the server keeps is up to the server.

## Server inference

When you [use a server](./settings.md#server-inference), your audio goes
to it for recognition. Whoever runs that server can hear the audio and read
the text. Pairing alone sends nothing; uploads start only when you switch
recognition to the server.

Your voice profile and corrections never leave your computer. **Only type my
voice** checks the speaker before upload when you deliver **After recording
stops** and when you retry from history. With **While speaking**,
audio leaves before Aria knows who spoke. The check helps recognition; it
cannot promise that other voices are left out.

The server keeps no history. It deletes its working copies when each
recognition ends, though crashes and backups can keep them. Encryption
protects audio on the way, not from the person running the server. Pairing
keys sit unencrypted in `~/.aria/inference/`, readable only by your account.

To stop uploads, turn off **Recognize speech on your server** in **Settings → Recognition**,
or run `aria setup --local`. Cancelling a dictation or revoking
your device stops what comes next. Neither can take back audio the server
already has.

## Clipboard and apps

Copying exposes text to apps and clipboard managers that can read your
clipboard. Typing inserts it into the focused app, which controls its own
saving and sharing. Cancelling Aria cannot recall text already delivered.

On Linux, Aria pastes through the clipboard and the selection, so text it
inserts stays on both. Aria asks clipboard managers to skip it,
but they may not. The udev rule that lets the hotkey read your keyboard lets
every program you run read every key and inject input too. Aria reads key presses only to tell its
hotkey apart from typing, and never records what you type.

On a Mac, Aria sees which keys you press, to notice the Aria key. It never
records what you type.

To place spaces, Aria reads two characters before the cursor and one after
it. It skips password fields and never saves or logs them.

## Chats

[Chat](./chat.md) sends your request to the assistant you choose. Claude Code
and Codex may send the request and project content to cloud services. They
also keep their own conversation records. Aria uses read-only settings to
limit file changes, but those settings don't prevent network access.

**Local Provider** sends your request to the server that `provider.endpoint`
names. The server decides where it goes next, as with
[text cleanup](#text-cleanup-with-a-local-model). With history on, each
request also carries the chat so far.

With history on, Aria also saves each request and its reply in
[history](./history.md) for up to 30 days. Starting fresh doesn't delete
earlier records. Clearing Aria's history, or deleting a chat from it,
doesn't delete the assistant's records. Deleting a chat also makes
Aria forget the ID it uses to continue it with the assistant.

## Prompts

[Prompts](./prompts.md) send your spoken words as text, along with the prompt
file's instructions, to Claude Code or Codex. A prompt you run on a history
entry sends that entry's text instead: a chat's reply, or a transcript's
whole text. The assistant may send that text to cloud
services.

With history on, Aria saves the result of a prompt you run on a history entry
under that entry. Aria deletes it with the entry.

Each run starts without a project, earlier conversation, or Aria memory.
Aria also disables the assistant's tools and leaves out its saved instructions
and settings. Prompt recordings are not kept in dictation history.

## Memory

**Memory** is off by default. Turn it on in **Settings → Privacy**. On a Mac,
Aria asks you to confirm first.

With **Memory** on, Aria saves the text of each request you speak in your own
voice. It sends that text to Claude to find facts about you and words Aria
mishears. Aria adds those facts to later chats. This includes
**Local Provider** chats, so their text reaches Claude too. Memory needs your
voice profile. Without one, Aria saves nothing.

Memory lives in `~/.aria/memory/`, or the folder `memory.dir` names. Only your
user account and administrators can read it. Aria deletes a saved request
once it's 30 days old and Aria has checked it for facts. Facts and learned
substitutions stay until you delete them.

Turning **Memory** off keeps what's stored. To delete it, run
`aria memory clear`. This keeps `lexicon.txt` and assistant conversations.
Deletion isn't secure erasure and can't remove backups.

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

[Transcripts](./transcribe-system-audio.md#transcript-files) are separate files
with no automatic expiry. History lists transcripts saved under the default
folder, and deleting one there deletes its file. Choose a folder with suitable permissions
and sharing settings. Delete the files when you no longer need them.

[Project vocabulary](./project-spellings.md) scans the folder you choose. Its cache
contains collected names and source locations. Disabling corrections keeps
the cache; forgetting a folder removes its cached names.

Voice profiles store a numeric representation of enrolled speech locally.
They are recognition aids, not a way to authenticate a user or guarantee that
other voices are excluded. A profile can still identify your voice, so treat
it as personal data. Backups of your home folder, such as Time Machine,
include it.
