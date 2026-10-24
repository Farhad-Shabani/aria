# Prompts (Mac)

A prompt turns what you say into finished text. You speak, and Aria types
the result where you were. For example:

- Polish a rambling thought into a clean sentence.
- Turn "tell Sam I'm running late" into a friendly message.
- Translate what you said into French.

Each prompt is a Markdown file of instructions you write.

**What you say and the prompt file go to Claude Code or Codex, which may send
them to cloud services.** Aria transcribes your speech on your Mac first.
Read [Privacy](./privacy.md#prompts) before using a prompt.

## Before you start

- Set up Claude Code or Codex as in
  [Ask a question](./voice-chat.md#before-you-start). Aria uses the agent
  that `chat.agent` names.
- Keep Aria.app running. Prompt shortcuts work only while it runs.

## Add a prompt

1. Open **Settings → Prompts**.
2. Click **+** and choose a Markdown file. The prompt takes the file's name.
3. On the prompt's page, set **Your words**:
   - **Rewrite them** keeps your words and meaning. It cleans them up or
     changes their style.
   - **Write from them** writes something new, such as a reply, a
     translation, or a summary.
4. Optional: click **Record** and press a shortcut, such as **⌃⌥P**.

A shortcut needs Control, Option, or Shift. ⌘ alone isn't enough, because
⌘ shortcuts belong to the app in front. To clear a shortcut, click **ⓧ**
beside it.

A prompt without a shortcut still runs from the
[prompt list](#pick-a-prompt-from-a-list).

A prompt file can be a few plain lines. For example, a `polish.md` set to
**Rewrite them**:

```markdown
Turn this into clean written text. Remove filler words and fix grammar.
Keep my meaning, names, and numbers.
```

## Use a prompt

1. Click into the app where you want the text.
2. Press the prompt's shortcut and speak.
3. Press it again to stop. Aria transcribes, runs the prompt, and types the
   result.

To cancel while Aria works, press the shortcut a third time.

Aria types only into the app you were in when you started. Nothing is typed
in these cases:

- You switched apps before stopping. Aria sends nothing to the agent.
- You switched apps while the agent worked. Aria drops the result.
- You said nothing. Aria sends nothing to the agent.
- The app has **Turn Off Aria** on in **Settings → Apps**.

Some apps don't accept typed text. Then the result goes to the clipboard,
and the status overlay says **copied**.

**In a terminal, take care.** Each line break in the result types as Return,
so a terminal runs each line as a command.

## Pick a prompt from a list

1. In **Settings → Shortcuts**, set a shortcut for **Show prompt list**.
2. Press it. A list shows up to nine prompts that are on.
3. Press a prompt's number to start it, then speak.
4. Press the list's shortcut again to stop. Press it once more to cancel.

**Esc** closes the list. It also closes by itself after eight seconds.
While the list shows, the number keys and Esc go to the list, not to the app.

## Change or remove a prompt

- **Edit it:** change the Markdown file in any editor. Aria reads the file on
  each run, so the next run uses your edit.
- **Turn it off:** use the switch on its page. Its shortcut stops working, and
  the prompt list hides it.
- **Remove it:** Control-click its row and choose **Remove Prompt**. Aria
  forgets the prompt but leaves the file alone.

## What a prompt can't do

Aria tells the agent to treat what you said as text to work on. Your words
are never instructions to the agent. The agent's tools are off during a run,
so a prompt can't read files, search the web, or run commands. For that, use
[Ask](./voice-chat.md).

## When it fails

On a failure, Aria types nothing, not even your raw words. The status overlay
says the agent failed. Common causes:

- The prompt file moved. Its row in Settings shows **File not found**. Choose
  the file again.
- The prompt file is empty, larger than 16 KB, or not text.
- The agent isn't installed or signed in. Check it with a typed
  [Ask](./voice-chat.md#try-a-typed-question).

Dictation history doesn't keep failed runs, so say it again.
