# Install Aria

Download Aria from the
[latest release](https://github.com/Farhad-Shabani/aria/releases/latest).
Releases run on Apple Silicon Macs with macOS 15 or later, and on x86_64 Linux.

## Mac

1. Download `Aria-<version>-macos-arm64.dmg`.
2. Open it and drag **Aria** to **Applications**.
3. Open Aria from Applications. It appears in the menu bar and opens its setup
   window. **Express Setup** uses the defaults; **Customize…** lets you choose
   engines and models.

To use the `aria` command in a terminal too, add this line to `~/.zshrc`, then
open a new terminal:

```sh
export PATH="/Applications/Aria.app/Contents/Helpers:$PATH"
```

Continue to [Mac dictation](./macos.md).

## Linux

Run:

```sh
curl -fsSL https://github.com/Farhad-Shabani/aria/releases/latest/download/install.sh | bash
```

The installer checks the download, installs the `aria` command in
`~/.local/bin`, and starts [setup](#setup). If it reports that `~/.local/bin`
is not on your `PATH`, add it in your shell's startup file.

Aria needs a systemd user session for its background service and ALSA
(`libasound2`) for audio. For copying text, run it in your graphical desktop
session. For speech output, install a system voice:

```sh
sudo apt-get install espeak-ng
```

Continue to [terminal dictation](./transcription.md).

## Setup

The Mac app and the Linux installer both start setup. To run it again:

```sh
aria setup
```

Choose **Whisper** for a first transcription. Setup can download its model.
For speech output, the system voice needs no model. You can leave optional
features at their defaults. Choosing a chat agent does not contact it; an
agent account is needed only for [Ask](./voice-chat.md).

Setup saves `~/.aria/config.yaml`, downloads the models you select, and starts
Aria's background service. It backs up a hand-written configuration as
`config.yaml.bak` before updating it. If a model download is interrupted,
rerun setup to resume it.

To check the result:

```sh
aria doctor
```

Doctor reports configuration, model-path, and service problems. It does not
record audio to test microphone permission.

## Update or remove

On a Mac, choose **Quit Aria** from its menu, replace Aria in Applications
with the copy from the new DMG, and open it. To remove Aria, quit it and move
it to the Trash.

On Linux, rerun the install command to update. To remove Aria:

```sh
curl -fsSL https://github.com/Farhad-Shabani/aria/releases/latest/download/install.sh | bash -s -- --uninstall
```

Removing Aria leaves your models and `~/.aria` in place. To delete saved
dictations, use the [history controls](./history.md#delete) first.

## Build from source

To build Aria yourself, follow
[Get started](https://github.com/Farhad-Shabani/aria#get-started) in the README.
`just app` builds the Mac app into `clients/build/mac/Build/Products/Release/`.
A source build runs from its checkout, so keep that folder in place.
