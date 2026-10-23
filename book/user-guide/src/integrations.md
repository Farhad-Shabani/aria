{{#title Use Aria with Raycast and tmux - Aria User Guide}}

# Raycast and tmux

These optional integrations use scripts from the Aria repository. If you
installed a release, clone it once and keep that folder:

```sh
git clone https://github.com/Farhad-Shabani/aria.git ~/aria
```

The scripts use an installed `aria` when found, otherwise the checkout's
`target/release/aria`.

## Raycast

In Raycast's Script Commands settings, add the checkout's
`integrations/raycast/` folder as a script directory. Then search for one of
these commands:

- **Aria Dictate** starts manual dictation with live insertion. Set output
  to an insertion mode under **Settings → Dictation** first; it cannot use
  Copy output. Run the command again to stop. For copying, use the next command.
- **Aria Dictate to Clipboard** copies the completed transcript. It stops on
  silence.
- **Aria Ask** opens Terminal, records a question, and prints the reply. It
  requires an authenticated agent as described in [Ask](./voice-chat.md).

For insertion, keep an editable field focused in the intended app when
opening Raycast and grant [Accessibility access](./macos.md#permissions).
For copying, paste after processing finishes. Dictation follows the same
[history settings](./history.md) as other recordings.

The helpers start an Aria daemon if none is running. If a command is missing
from Raycast, check the script-directory path. If it runs but fails, check
`aria doctor` and which installed or checkout command the helper found.

## tmux

The tmux integration shows recording and processing activity in its status bar.
It needs both a configuration line and a running watcher.

For a checkout at `~/aria`, add this to `~/.tmux.conf`. Replace the path if yours
is elsewhere:

```tmux
source-file ~/aria/integrations/tmux/tmux.conf
```

Reload it from a tmux terminal:

```sh
tmux source-file ~/.tmux.conf
```

If you use the Raycast helpers, they start the watcher. Otherwise run it in
another terminal while Aria's service is running, replacing `~/aria` with
your checkout path:

```sh
~/aria/integrations/tmux/status.sh
```

Leave that terminal open. The status bar shows a red circle during recording
and a processing indicator while Aria finishes the text. It clears when the
operation ends.

If the bar stays empty, check the watcher and `aria service status`. Ctrl-C in
the watcher's terminal stops the display, not dictation. After the service has
been stopped for a while, restart the watcher when you start Aria again.
