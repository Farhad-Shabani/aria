# Mac dictation

Use Aria's hotkey to type speech into an app. Complete
[installation and setup](./installation.md), then open Aria from Applications.
It appears in the menu bar. **Set Up Aria…** reopens setup if you need it.

## Permissions

Aria needs two permissions:

- **Microphone** to hear you. Allow access when macOS asks. If you previously
  denied it, switch on **Aria** in
  **System Settings → Privacy & Security → Microphone**.
- **Accessibility** to read the hotkey and type into other apps. In
  **System Settings → Privacy & Security → Accessibility**, switch on **Aria**.
  If it isn't listed, click **+** and choose
  `/Applications/Aria.app/Contents/Helpers/Aria.app`. Use **⌘⇧G** in the
  file picker to enter that path.

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

While Aria types segments, words it has heard but not typed yet show faintly
at the cursor. Aria may still change them before it types them. Apps that do
not report their cursor show none, and neither does **Only type my own voice**.
Turn them off with **Show words before they're typed** under **Settings →
General → On screen**.

If output is set to copy, paste the transcript after recording; copying always
uses the completed text. Output choices are under **Settings → Dictation**.

## Dictate from iPhone and Apple Watch

Switch on **Phone Pairing** under **Settings → Phone**. Aria restarts and shows
a code for Aria for iPhone to scan. Switching it off keeps paired phones for
next time.

**Copy Code** puts the code on the clipboard, marked hidden from clipboard
managers and wiped when the code expires. Universal Clipboard may still carry
it to your other devices meanwhile. **Revoke All…** unpairs every phone: Aria
restarts with a new code, and each phone scans again.

## Keep Aria running

Dictation needs Aria's background service. Opening the app starts it; choosing
**Quit Aria** stops it until you open the app again. Keep Aria allowed under
**System Settings → General → Login Items → Allow in the Background**.

You can enable **Launch at login** under **Settings → General**.

Once dictation works, you can [correct project names](./project-spellings.md)
or [review its history](./history.md).
