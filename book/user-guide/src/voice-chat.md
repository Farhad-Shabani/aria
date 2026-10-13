# Ask a question

Ask sends a typed or spoken question to Claude Code or Codex and prints the
reply in your terminal. It does not read replies aloud.

**Your question and project content read by the agent may go to cloud services.**
Spoken questions are transcribed locally first. The agent keeps its own
conversation records. Read [Privacy](./privacy.md#questions) before using
private project content.

## Before you start

Complete [setup](./installation.md#setup), then install and sign in to the
`claude` or `codex` command you selected. Follow the
[Claude Code quickstart](https://code.claude.com/docs/en/quickstart) or
[Codex CLI setup](https://learn.chatgpt.com/docs/codex/cli), then check that the
command works with your account. Keep Aria's background service running.

If you installed the agent after Aria setup, [refresh the service registration](./configuration.md#apply-changes)
so it can find the command.

## Try a typed question

Open a terminal in a project you are comfortable sharing, then run:

```sh
aria ask \
  --project "$PWD" \
  "What does this project do?"
```

Wait for the reply. `--project` tells the agent which folder to read. Aria
uses read-only agent settings; this limits changes to files but does not
prevent the agent from reading them or sending content to its service.

To continue the conversation, ask again from the same folder with the same
agent:

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

Speak, then pause. With the default stop setting, silence ends the recording.
The answer appears in the terminal. If a spoken question fails, try the typed
example again to distinguish an agent problem from a recording problem.

## Cancel

Press **Esc** when the terminal shows its cancel prompt to cancel the question.

## Start fresh

Repeated questions continue the conversation for that folder and agent.
Use `--new` to start another:

```sh
aria ask \
  --project "$PWD" \
  --new \
  "Explain the tests."
```

Starting fresh does not delete the agent's earlier conversation records.
Use `--session` with a name if you want separate conversations in one folder.
For a different agent on one request, add `--agent codex` or
`--agent claude-code`.

If Ask cannot start its agent, check [Troubleshooting](./troubleshooting.md#ask-cannot-start).
For names in spoken questions, [Project vocabulary](./project-spellings.md) explains
how to prepare the folder's spellings.
