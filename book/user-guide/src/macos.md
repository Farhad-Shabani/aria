{{#title Dictate into Mac apps with a hotkey - Aria User Guide}}

# Mac dictation

Use Aria's hotkey to type speech into an app. Complete
[installation and setup](./installation.md), then open Aria from Applications.
It appears in the menu bar. To run setup again, open **Aria Setup** in
**Settings → General**.

## Permissions

Aria needs two permissions:

- **Microphone** to hear you. Allow access when macOS asks. If you previously
  denied it, turn on **Aria** in
  **System Settings → Privacy & Security → Microphone**.
- **Accessibility** to read the hotkey and type into other apps. When macOS
  asks, choose **Open System Settings** and turn on **Aria**. The switch is
  under **System Settings → Privacy & Security → Device Control and Data
  Access** (**Accessibility** before macOS 27). If Aria isn't listed, click **+**
  and choose
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

If you forget the second tap, **Auto-stop** ends the recording once you stop
speaking. It waits 10 seconds by default. Change or turn it off under
**Settings → Dictation**.
Change the key to **Fn** under **Settings → Shortcuts** if you prefer.

Other keyboard or mouse input while you hold the key can cancel the recording.

## Ask or pick a prompt

Settings calls the key you tap the **Aria key**: right Option, or Fn if you
chose it. Hold it, tap a second key, then let go:

- **⇧ Shift** asks your agent. Speak, then tap the Aria key to send. Tap it
  again while Aria thinks to cancel.
- **⌘ Command** shows the [prompt list](./prompts.md#pick-a-prompt-from-a-list).

Each is a setting under **Settings → Shortcuts**: keep the combo, record a
shortcut of your own instead, or turn it off. **Restore Defaults** brings
the combos back.

While an ask or prompt runs, pressing its combo again does nothing. The Aria
key alone sends and cancels it.

Any other key pressed with the Aria key cancels the press. Option shortcuts
in other apps keep working. These presses need Accessibility access.

## Captions

Turn on **Show captions** in **Settings → General** to see what Aria hears in
a panel while you record. Captions work with either delivery choice.

**Settings → Dictation → Deliver** decides when Aria types. With
**While speaking**, Aria types finalized segments as you speak. With
**After recording stops**, it types the completed transcript when you stop.

While Aria types segments, words it has heard but not typed yet show faintly
at the cursor. Aria may still change them before it types them. Apps that do
not report their cursor show none, and neither does **Only type my voice**.
Turn them off with **Show inline preview** under **Settings → General → On
screen**.

If output is set to copy, paste the transcript after recording; copying always
uses the completed text. Output choices are under **Settings → Dictation**.

## Per-app settings

**Settings → Apps** gives an app its own output and delivery. Click **+**, pick
the app, then choose its settings. Aria uses them when that app is in front as
dictation starts. Switching apps mid-recording changes nothing. Other apps use
**Settings → Dictation**. Flags such as `aria dictate --mode copy` still win.

Switch off the switch at the top of an app's page, and Aria starts no
dictation or Ask and types or copies nothing while that app is in front, even
from the phone.

## Dictate from iPhone and Apple Watch

Switch on **Phone pairing** under **Settings → Phone**. Aria restarts and shows
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
