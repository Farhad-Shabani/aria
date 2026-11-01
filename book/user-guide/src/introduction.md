{{#title Dictation, speech, and voice assistants - Aria User Guide}}

# Welcome to Aria

Aria lets you type by speaking, hear text read aloud, and ask questions about a
project. On a Mac or a Linux desktop, you can dictate into other apps with a
hotkey.

## Start here

[Install Aria](./installation.md), then try [Mac dictation](./macos.md),
[Linux dictation](./linux.md), or [terminal dictation](./transcription.md). Start with one short sentence so you
can check the microphone, recognition, and output together.

Once that works, choose what you need next:

- [Review or delete saved dictations](./history.md).
- [Read text aloud](./tts.md).
- [Ask a question](./voice-chat.md) through Claude Code or Codex.
- [Correct project names](./project-spellings.md).
- [Create voice profiles](./speaker-verification.md).
- [Transcribe audio playing on your Mac](./listen.md).

If setup or recording fails, start with [Troubleshooting](./troubleshooting.md).

## Your data

Dictation and speech generation run locally by default. You can move
transcription to a [personal server](./settings.md#server-inference).
Downloading models needs network access. Ask uses an external assistant, which
may send your question and project content to cloud services.

Dictation history is enabled by default. Read [Privacy](./privacy.md) before
recording sensitive material; [history controls](./history.md#saving) explain
what you can change.

For adding speech to your own Rust application, read
[Using Aria Libraries](../dev-guide/introduction.html).
