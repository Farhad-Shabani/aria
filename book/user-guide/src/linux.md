{{#title Dictate into Linux apps with a hotkey - Aria User Guide}}

# Linux dictation

Use Aria's hotkey to type speech into an app on GNOME, KDE Plasma, or an X11
desktop. Complete [installation and setup](./installation.md#linux) first. The
background service starts and stops with your desktop session.

## Permissions

Aria needs two permissions:

- **Keyboard access** to read the hotkey. The installer offers a udev rule
  that gives the logged-in user access to keyboards, mice, and touchpads. Aria
  only reads them. Every program you run could read every key and inject
  input too. To add the rule later, run `aria doctor` and follow its steps.
- **Keyboard control** to paste into other apps on Wayland. Aria asks the
  desktop for permission and saves the grant when the desktop supports it.
  If you decline, Aria copies instead; restarting the service asks again.

X11 desktops need only keyboard access.

## Try it

1. Open an empty document in a text editor and click where you want the text.
2. Tap **right Alt**, say a short sentence, then tap it again to stop.
3. Keep that app focused until the text appears.

On Wayland, Aria copies the text if keyboard permission is still pending
when recording ends. Once permission is granted, later dictations can paste.

## Tap or hold

Tap the key to start and tap again to stop. You can also hold it to record;
recording starts half a second into the hold, and releasing the key stops it.

Other keyboard or mouse input while you hold the key can cancel the recording.
Where right Alt is AltGr, typing characters with it still works. Only a press
on its own starts Aria.

## How text arrives

Aria puts the transcript on the clipboard and on the selection, then presses
**Shift+Insert** to paste. The transcript stays on the clipboard afterwards.

Text arrives once you stop recording, not while you speak.

When keyboard control is unavailable, Aria copies the text for you to paste.
This includes Wayland desktops without a remote desktop portal.

Aria can't tell which app is in front on Linux. Text goes to whatever has focus
when Aria pastes, and `application_overrides`, including `excluded`, have no
effect.
