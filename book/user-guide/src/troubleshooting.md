{{#title Troubleshoot Aria setup and dictation - Aria User Guide}}

# Troubleshooting

Start with the symptom you see.

## Command not found

On a Mac, finish **Aria Setup** in **Settings → General**; it installs the
command (see [installation](./installation.md#mac)). On Linux, make sure
`~/.local/bin` is on your `PATH`. Then open a new terminal and try `aria --help`.

## Service unavailable

```sh
aria service status
```

If registered but stopped, run `aria service start`. If not registered, complete
`aria setup`. For the Mac app, opening Aria starts its background service;
choosing **Quit Aria** stops it.

## No recording

Run `aria doctor` for model-path and input-device problems. It does not record
to prove microphone access. Make sure the intended microphone is connected
and selected as the system's default input.

On a Mac, check **System Settings → Privacy & Security → Microphone** for the
app or executable requesting access. Grant permission, then retry. A model
file that is missing or unreadable needs to be corrected in setup or
[Settings](./settings.md).

## No inserted text

If recording works but text does not appear, keep the target app and editable
field focused. Check [Accessibility permission](./macos.md#permissions). For a
terminal recording, try `aria dictate --mode copy` and paste to separate
insertion from recognition.

On Linux, run `aria doctor` if the hotkey does nothing; it shows how to grant
[keyboard access](./linux.md#permissions). If text is copied but not pasted,
your desktop may lack a remote desktop portal, or you declined its dialog;
restart the service to be asked again. Keep the background service running so
clipboard text remains available. If the service started outside your desktop,
such as over SSH, restart it from the desktop so it can reach the display.

## Restart

After changing engine settings,
[restart the service](./settings.md#apply-changes). Reopening the Mac app
also starts it.

## No speech output

Check your output device and volume. Then use `aria service status` and
`aria settings show` to inspect the voice and model paths. Engine and model
changes need a restart. `aria doctor` doesn't check speech-engine files.

Linux system speech needs `/usr/bin/espeak-ng` or `/usr/bin/espeak`. For Kokoro,
rerun setup to obtain its model and voices file, or correct their paths.

## Ask cannot start

Check that your chosen `claude` or `codex` command works directly with your
account. If you installed it after Aria started, [restart the service](#restart).
On Linux, if it lives outside the service's saved `PATH`, run
`aria service install` so the service can find it.

On a Mac, **Settings → Assistant** marks an assistant that is not installed or
not signed in, and shows how to fix it. After you sign in, switch back to
Settings and it checks again.
An Ask that fails for either reason says so in the status pill.

Try [a typed question](./voice-chat.md#try-a-typed-question) before a spoken one.
If typing works but speech does not, check the microphone and recognition model.

## System audio fails

In **System Settings → Privacy & Security → Screen & System Audio Recording**,
check permission for the requesting app or executable. Microphone permission
does not grant system-audio access.

Listen runs only on Parakeet, Moonshine, or Nemotron. With another engine,
set up the [Moonshine model](./listen.md#model). For a failure after Listen
starts, run `aria listen status`. Confirm that audio is playing through your
Mac.

## Read the error

`aria wake log` shows recent background-service errors, even when wake-word
monitoring is off. Look for the failure associated with the operation you tried.
After fixing a settings error, [apply the change](./settings.md#apply-changes).
