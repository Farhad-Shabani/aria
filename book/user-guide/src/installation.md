# Install Aria

Download Aria from the
[latest release](https://github.com/Farhad-Shabani/aria/releases/latest).
Releases run on Apple silicon Macs with macOS 15 or later, and on x86_64 Linux.

## Mac

With [Homebrew](https://brew.sh), this installs Aria and links the `aria`
command:

```sh
brew install --cask farhad-shabani/tap/aria
```

If Aria is already in Applications, add `--force` to replace it. Then open Aria
to finish setup, as in step 3 below.

Without Homebrew:

1. Download [Aria.dmg](https://github.com/Farhad-Shabani/aria/releases/latest/download/Aria.dmg).
2. Open it and drag **Aria** to **Applications**.
3. Open Aria from Applications. It appears in the menu bar and opens its setup
   window. **Express Setup** uses the defaults; **Customize…** lets you choose
   engines and models.

When setup finishes, Aria asks for your password to add the `aria` command to
the terminal. Skip it if you only use the menu bar app. **Set Up Aria…** in
the menu asks again later.

If you can't enter an administrator password, add this line to `~/.zshrc`
instead:

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

Keep the default **Parakeet** engine. It types text as you speak. Setup
downloads the model for the engine you choose and shows its size first.
Parakeet (MLX) is the exception: it needs its own Python and downloads its
model on first use.

The system voice needs no model. You can leave optional features at their
defaults. Choosing a chat agent does not contact it. You need an agent
account only for [Ask](./voice-chat.md).

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

### Free model space

A model stays on disk after you switch engines. To see each model and remove one
you no longer use:

```sh
aria models list
aria models remove parakeet
```

Aria keeps a model your configuration still uses. Setup downloads a removed
model again if you choose its engine later.

## Update or remove

On a Mac, Aria offers new releases itself. Choose **Check for Updates…** from
its menu to check now, or set automatic checks and installs in
**Settings → General**. With Homebrew, run `brew upgrade --cask --greedy aria`,
because Aria updates itself and a plain `brew upgrade` skips it.

To remove Aria from a Mac, quit it and move it to the Trash, or run
`brew uninstall --cask aria`. The `aria` command stays behind. Remove it with
`sudo rm /usr/local/bin/aria`.

On Linux, rerun the install command to update. It keeps your setup answers
and downloads nothing new; run `aria setup` to change them. To remove Aria:

```sh
curl -fsSL https://github.com/Farhad-Shabani/aria/releases/latest/download/install.sh | bash -s -- --uninstall
```

Removing Aria leaves `~/.aria`, models included, in place. To delete saved
dictations, use the [history controls](./history.md#delete) first.

## Build from source

To build Aria yourself, follow
[Get started](https://github.com/Farhad-Shabani/aria#get-started) in the README.
`just app` builds the Mac app into `clients/build/mac/Build/Products/Release/`.
A source build runs from its checkout, so keep that folder in place.
