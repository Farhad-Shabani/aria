# Mac dictation

Use Aria's hotkey to type speech into an app. Complete
[installation and setup](./installation.md), then open Aria from Applications.
It appears in the menu bar. **Set Up Aria…** reopens setup if you need it.

## Permissions

Aria needs two permissions:

- **Microphone** to hear you. Allow access when macOS asks. If you previously
  denied it, check **System Settings → Privacy & Security → Microphone** for
  the app or executable named in the request.
- **Accessibility** to read the hotkey and type into other apps. In
  **System Settings → Privacy & Security → Accessibility**, click **+** and
  choose `/Applications/Aria.app/Contents/Helpers/aria`. Use **⌘⇧G** in the
  file picker to enter that path.

After granting Accessibility, [restart Aria's service](./troubleshooting.md#restart)
if insertion still fails.

## Try it

1. Open an empty document in a text editor and click where you want the text.
2. Tap **right Option (⌥)**, say a short sentence, then tap it again to stop.
3. Keep that app and text field focused until processing finishes.

Recognized words appear as segments become ready.

## Tap or hold

Tap the key to start and tap again to stop. You can also hold it to record;
recording starts half a second into the hold, and releasing the key stops it.
Change the key to **Fn** under **Settings → Shortcuts** if you prefer.

Other keyboard or mouse input while you hold the key can cancel the recording.

## Captions

Enable **Show Captions** in Aria's menu to display recognized text while
recording. With this option on, Aria types the completed transcript after you
stop. With it off, Aria types finalized segments while you speak.

If output is set to copy, paste the transcript after recording; copying always
uses the completed text. Output choices are under **Settings → Dictation**.

## Keep Aria running

Dictation needs Aria's background service. Opening the app starts it; choosing
**Quit Aria** stops it. Reopen the app to restore dictation, or use
`aria service start` for the registered service.

You can enable **Launch at login** under **Settings → General**.

Once dictation works, you can [correct project names](./project-spellings.md)
or [review its history](./history.md).
