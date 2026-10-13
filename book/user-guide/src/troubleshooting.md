# Troubleshooting

Start with the symptom you see.

## Command not found

On a Mac, add the app's command folder to your `PATH` as shown in
[installation](./installation.md#mac). On Linux, make sure `~/.local/bin` is on
your `PATH`. Then open a new terminal and try `aria --help`.

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
[Settings](./configuration.md).

## No inserted text

If recording works but text does not appear, keep the target app and editable
field focused. Check [Accessibility permission](./macos.md#permissions) for the
actual Aria executable, then restart its service. For a terminal recording,
try `aria dictate --mode copy` and paste to separate insertion from recognition.

On Linux, use copying rather than insertion-only modes. Keep the background
service running so clipboard text remains available.

## Restart

After changing engine settings or granting Accessibility access,
[restart the service](./configuration.md#apply-changes). Reopening the Mac app
also starts it.

## No speech output

Check your output device and volume. Then use `aria service status` and
`aria config show` to inspect the voice and model paths. File changes require
a restart. `aria doctor` does not check speech-engine files.

Linux system speech needs `/usr/bin/espeak-ng` or `/usr/bin/espeak`. For Kokoro,
rerun setup to obtain its model and voices file, or correct their paths.

## Ask cannot start

Check that your chosen `claude` or `codex` command works directly with your
account. If installed after Aria setup or in a different command directory,
run `aria service install` so the service can find it.

Try [a typed question](./voice-chat.md#try-a-typed-question) before a spoken one.
If typing works but speech does not, check the microphone and recognition model.

## System audio fails

In **System Settings → Privacy & Security → Screen & System Audio Recording**,
check permission for the requesting app or executable. Microphone permission
does not grant system-audio access.

The default Whisper model cannot run Listen. Check the
[Moonshine files and configuration](./listen.md#model),
and use `aria listen status` for a failure reported after starting. Confirm
that audio is actually playing through your Mac.

## Read the error

`aria wake log` shows recent background-service errors, even when wake-word
monitoring is off. Look for the failure associated with the operation you tried.
After fixing a configuration error, [apply the change](./configuration.md#apply-changes).
