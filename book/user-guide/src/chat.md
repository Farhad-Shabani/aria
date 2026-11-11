{{#title Chat with Claude Code, Codex, or a local model by voice - Aria User Guide}}

# Chat with an assistant

Chat sends what you type or say to Claude Code, Codex, or a model on your
Mac. The reply appears in your terminal, or in Aria on a Mac. To keep your
requests on your Mac, see [Use a local model](#use-a-local-model).

**Your request, and project files the assistant reads, may go to cloud
services.** Aria sends the assistant text, never audio. The assistant keeps
its own conversation records. Read [Privacy](./privacy.md#chats) before you
use private project content.

## Before you start

1. Complete [Aria setup](./installation.md#setup).
2. Install Claude Code or Codex and sign in. Follow the
   [Claude Code quickstart](https://code.claude.com/docs/en/quickstart) or
   [Codex CLI setup](https://learn.chatgpt.com/docs/codex/cli), then check that
   `claude` or `codex` works in your terminal.
3. Choose that assistant in **Settings → General → Assistant**, or run
   `aria settings set assistant.id claude-code` or
   `aria settings set assistant.id codex`.

Keep Aria's background service running. If you installed the assistant after
Aria setup, follow the [service restart steps](./settings.md#apply-changes)
so Aria can find its command.

## Type a request

Open a terminal in a project you are comfortable sharing, then run:

```sh
aria ask \
  --project "$PWD" \
  "What does this project do?"
```

The reply appears in your terminal. `--project` gives the assistant a folder
to work in. Aria runs it with read-only settings to limit file changes. The
assistant can still read files and send their content to its service.

To continue the chat, ask again from the same folder with the same
assistant:

```sh
aria ask \
  --project "$PWD" \
  "Where should I start reading?"
```

## Speak a request

Once a typed request works, leave out the text:

```sh
aria ask \
  --project "$PWD"
```

Speak, then pause. Silence ends the recording.
The reply appears in the terminal. If a spoken request fails, try a typed
one. If that works, the problem is the recording.

## Ask on a Mac

The Ask shortcut opens Aria on a chat with your assistant. To set it up,
follow [Ask or pick a prompt](./macos.md#ask-or-pick-a-prompt). To follow up
later, see [Continue a chat](./history.md#continue-a-chat). To ask about a
saved text, see
[Ask about an entry or a result](./history.md#ask-about-an-entry-or-a-result).
A chat started on a Mac is separate from the one `aria ask` continues in the
same folder.

## Cancel

To cancel, press **Esc** when the terminal shows its cancel prompt.

## Start fresh

Each request continues the chat for that folder and assistant.
Use `--new` to have the assistant start fresh:

```sh
aria ask \
  --project "$PWD" \
  --new \
  "Explain the tests."
```

Starting fresh does not delete the assistant's earlier conversation records.
Use `--chat` with a name to keep separate chats in one folder.
A name can't start with `/`, and each assistant's chats have different
names. A named chat runs in the folder where it started, even when you ask
from another folder or with `--project`.

## Use a different assistant or model

Add `--assistant codex`, `--assistant claude-code`, or `--assistant local-model` to
use a different assistant for one request. Add `--model "MODEL_ID"` to choose
a model for that request. Neither option changes your saved settings.

To see or change your usual assistant and model, follow
[Assistant settings](./settings.md#assistant).

## Use a local model

The **Local Provider** assistant sends your request to a model server on your
Mac, such as [Ollama](https://ollama.com). It can't read your project.

**A cloud model sends your request off your Mac.** Pick a model that runs on
your Mac.

1. Download a model:

   ```sh
   ollama pull qwen2.5:3b
   ```

2. Add these lines to your settings file:

   ```toml
   [provider]
   endpoint = "http://127.0.0.1:11434/v1/chat/completions"

   [assistant.models]
   local-model = "qwen2.5:3b"
   ```

3. Follow the [service restart steps](./settings.md#apply-changes).
4. Choose **Local Provider** in **Settings → General → Assistant**, or run
   `aria settings set assistant.id local-model`.

Send a request as in [Type a request](#type-a-request). The reply should
appear in your terminal.

The local model remembers the chat only while history is on.

If a chat can't start its assistant, check
[Troubleshooting](./troubleshooting.md#chat-cant-start).
For names in spoken requests, [Project vocabulary](./project-spellings.md)
explains how to prepare the folder's spellings.
