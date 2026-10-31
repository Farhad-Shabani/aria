{{#title Ask Claude Code and Codex by voice - Aria User Guide}}

# Ask a question

Ask sends a typed or spoken question to Claude Code or Codex and prints the
reply in your terminal. It does not read replies aloud.

**Your question and project content read by the assistant may go to cloud services.**
Aria transcribes spoken questions before sending the text. The assistant
keeps its own conversation records. Read [Privacy](./privacy.md#questions)
before using private project content.

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

## Try a typed question

Open a terminal in a project you are comfortable sharing, then run:

```sh
aria ask \
  --project "$PWD" \
  "What does this project do?"
```

The reply appears in your terminal. `--project` gives the assistant a folder
to work in. Aria runs it with read-only settings to limit file changes. The
assistant can still read files and send their content to its service.

To continue the conversation, ask again from the same folder with the same
assistant:

```sh
aria ask \
  --project "$PWD" \
  "Where should I start reading?"
```

## Speak a question

Once the typed question works, omit its text:

```sh
aria ask \
  --project "$PWD"
```

Speak, then pause. Silence ends the recording.
The answer appears in the terminal. If a spoken question fails, try the typed
example again to distinguish an assistant problem from a recording problem.

## Cancel

Press **Esc** when the terminal shows its cancel prompt to cancel the question.

## Start fresh

Repeated questions continue the conversation for that folder and assistant.
Use `--new` to start another:

```sh
aria ask \
  --project "$PWD" \
  --new \
  "Explain the tests."
```

Starting fresh does not delete the assistant's earlier conversation records.
Use `--session` with a name if you want separate conversations in one folder.

## Use a different assistant or model

Add `--assistant codex` or `--assistant claude-code` to use a different
assistant for one question. Add `--model "MODEL_ID"` to choose a model for
that question. Neither option changes your saved settings.

To see or change your usual assistant and model, follow
[Assistant settings](./settings.md#assistant).

If Ask cannot start its assistant, check [Troubleshooting](./troubleshooting.md#ask-cannot-start).
For names in spoken questions, [Project vocabulary](./project-spellings.md) explains
how to prepare the folder's spellings.
