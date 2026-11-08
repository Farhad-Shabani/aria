{{#title Turn speech into finished text with prompts - Aria User Guide}}

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

A prompt without a shortcut still runs from
[Aria's list](#pick-a-prompt-in-aria).

A prompt file can be a few plain lines. For example, a `polish.md` set to
**Rewrite them**:

```markdown
Turn this into clean written text. Remove filler words and fix grammar.
Keep my meaning, names, and numbers.
```

## Write a prompt in Aria

1. [Open Aria](#pick-a-prompt-in-aria), type “new prompt”, and choose
   **New Prompt…**.
2. Enter a **Name** and choose **Your words**.
3. Type the instructions under **Instructions**.
4. Press **⌘↩** to save.

Aria saves the instructions in a new file in `~/.aria/prompts`, named after
the prompt. It never replaces a file that's already there. To use a file you
already have, click **Choose…**. Aria reads that file where it is.

Press **Esc** to leave. If you changed anything, Aria asks before discarding
it. Aria keeps up to 16 prompts.

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

## Pick a prompt in Aria

1. Hold the Aria key, tap **⌘**, then let go. Aria opens over the app
   you're in.
2. Press a prompt's number to start it, then speak. Or type to find a
   prompt and press **Return**.
3. Tap the Aria key to stop. Tap it once more to cancel.

The first nine prompts that are on get a number. While **Save history** is on,
Aria lists your [history](./history.md) until you type. The numbers work
either way. To use a shortcut of your own, or to turn it off, change
**Open Aria** in **Settings → Shortcuts**. Opening Aria.app also opens Aria.

**Esc** clears what you typed, then closes Aria. Clicking elsewhere closes it
too. The app you were in stays in front, so the result types there.

## Change or remove a prompt

- **Edit it in Aria:** select the prompt in Aria and press **⌘E**. Change it,
  then press **⌘↩** to save.
- **Edit its file:** change the Markdown file in any editor. Aria reads the
  file on each run, so the next run uses your edit.
- **Turn it off:** use the switch on its page. Its shortcut stops working, and
  Aria's list hides it.
- **Remove it:** Control-click its row and choose **Remove Prompt**. Aria
  forgets the prompt but leaves the file alone.

## Manage prompts from a terminal

List your prompts with their IDs:

```sh
aria prompt list
```

Each line shows the prompt's ID, name, kind, whether it's on, its shortcut,
and its file. Add `--json` for one JSON array. Other `aria prompt` commands
take a prompt's ID or its name, ignoring case.

To add a prompt with its instructions:

```sh
aria prompt add \
  --name Polish \
  --instructions "Turn this into clean written text."
```

Aria prints the new prompt's ID. You should see the prompt in
`aria prompt list` and in Aria. As in Aria, the instructions go in a new file
in `~/.aria/prompts`. To use a file you already have, pass `--file FILE`
instead. Without `--name`, the prompt takes the file's name. With neither
`--instructions` nor `--file`, Aria reads the instructions you pipe in. Add
`--kind compose` for **Write from them**.

Other changes:

- **Change it:** `aria prompt edit Polish --kind compose`. It also takes
  `--name`, `--file`, and `--instructions`. The prompt keeps its shortcut and
  whether it's on.
- **Print its instructions:** `aria prompt show Polish`.
- **Turn it off:** `aria settings set prompts.ID.enabled off`, with the ID
  from `aria prompt list`.
- **Remove it:** `aria prompt remove Polish`. Its file stays where it is.

## Run a prompt from a terminal

Keep Aria's background service running, then run:

```sh
aria prompt run Polish --text "so um I think we ship on friday"
```

Aria prints the reply. Without `--text`, Aria reads the text you pipe in. To
run on a [history](./history.md) entry, pass `--entry ID` instead. While
**Save history** is on, Aria saves the reply under that entry. To run
instructions of your own once, pass `--instructions` in place of the prompt's
name. Press **Esc** or **Ctrl-C** to cancel.

To type or copy the reply as a dictation does, add `--mode insert-or-copy`.

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
