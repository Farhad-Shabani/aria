# Prompts (Mac)

A prompt turns what you say into finished text. You speak, and Aria types
the result where you were. For example:

- Polish a rambling thought into a clean sentence.
- Turn "tell Sam I'm running late" into a friendly message.
- Translate what you said into French.

Each prompt is a Markdown file of instructions you write.

**What you say and the prompt file go to Claude Code or Codex, which may send
them to cloud services.** Aria transcribes your speech before sending the text.
Read [Privacy](./privacy.md#prompts) before using a prompt.

## Before you start

Set up Claude Code or Codex by following
[Ask a question](./voice-chat.md#before-you-start). Prompts use the same
assistant and model as Ask. You can change both in
[Assistant settings](./settings.md#assistant).

Keep Aria.app running while you use prompt shortcuts.

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
3. Tap the Aria key (right Option, or Fn if you chose it) to stop. Aria
   transcribes, runs the prompt, and types the result.

To cancel while Aria works, tap the Aria key again. Pressing the shortcut
again does nothing until the run ends.

Aria types only into the app you were in when you started. Nothing is typed
in these cases:

- You switched apps before stopping. Aria sends nothing to the assistant.
- You switched apps while the assistant worked. Aria drops the result.
- You said nothing. Aria sends nothing to the assistant.
- Aria is switched off for the app in **Settings → Apps**.

Some apps don't accept typed text. Then the result goes to the clipboard,
and the status overlay says **copied**.

**In a terminal, take care.** Each line break in the result types as Return,
so a terminal runs each line as a command.

## Pick a prompt from a list

1. Hold the Aria key (right Option, or Fn if you chose it), tap **⌘** (or
   the list's own shortcut from **Settings → Shortcuts**), then let go. A
   list shows up to nine prompts that are on.
2. Press a prompt's number to start it, then speak.
3. Tap the Aria key to stop. Tap it once more to cancel.

To open the list with a shortcut of your own, or to turn it off, change
**Prompt list shortcut** in **Settings → Shortcuts**.

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

The prompt file supplies the instructions. Aria asks the assistant to treat
your spoken words as the text to rewrite or build on.

Aria turns off the assistant's tools for Prompts, so a prompt can't read files,
search the web, or run commands. Use [Ask](./voice-chat.md) for questions
about a project.

## When it fails

If a prompt fails, the status overlay shows an error and Aria types nothing.
Common causes:

- The prompt file moved. Its row in Settings shows **File not found**. Choose
  the file again.
- The prompt file is empty, larger than 16 KB, or not text.
- The assistant isn't installed or signed in. Check it with a typed
  [Ask](./voice-chat.md#try-a-typed-question).

Dictation history doesn't keep failed runs, so say it again.
