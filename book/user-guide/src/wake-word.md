{{#title Start dictation with a wake word - Aria User Guide}}

# Wake word

Wake-word monitoring starts dictation when a local model detects its trained
phrase. The recording stops on silence and delivers one complete transcript
using your output preference. It does not ask an agent a question.

This keeps microphone monitoring active while the daemon runs. Wake-triggered
dictation follows the same [history preference](./history.md)
as other dictation. Turning wake monitoring off does not close a microphone
stream already used by the daemon; stop the daemon to stop its capture.

## Setup

First make [ordinary dictation](./transcription.md) work. You need a persistent
daemon using microphone capture and a compatible wake-word model.
Aria ships no wake-word model. Place an openWakeWord classifier in
`~/.aria/models/wakeword` as `aria.onnx`, beside `melspectrogram.onnx` and
`embedding_model.onnx`.

Enable monitoring in `~/.aria/config.yaml`:

```yaml
wake:
  enabled: true
  backend: openwakeword
  model: ~/.aria/models/wakeword/aria.onnx
  threshold: 0.5
```

Restart the registered service and check monitoring:

```sh
aria service stop
aria service start
aria wake status
```

Status must report `listening` before you try the phrase. On macOS, leave an
editable field focused and ensure [Accessibility access](./macos.md#permissions)
is granted for insertion. On Linux, delivery uses the clipboard.

Speak the phrase trained into your classifier, then the sentence you want to
dictate, and pause. The resulting text should arrive after recording stops.
The model defines the phrase; changing the filename does not change what it
recognizes. If you received a custom model, obtain its exact phrase from its
builder.

## Troubleshooting and stopping

`aria wake status` distinguishes `listening`, `degraded`, `disabled`, and
`stopped`; only `listening` returns success. If it is degraded, inspect
`aria wake log` and use `aria doctor` to check the model files. If it is disabled,
check the configuration and restart after changing it.

The threshold must be greater than zero and at most one. A higher value
requires a stronger match and can reduce accidental triggers; it may also
miss intended ones. Adjust it only after checking the model and microphone.

To disable wake triggers, set `wake.enabled: false` and restart the service.
Ordinary dictation remains available, including after wake inference fails.
To stop the daemon itself, use `aria service stop`.

## Models

Supported backends are `openwakeword` and `livekit`. `wake.model` names one
classifier file. Its directory must also contain `melspectrogram.onnx` and
`embedding_model.onnx`.

The `livekit` backend needs its own classifier and an explicit `wake.model`
path. livekit-wakeword's `hey_livekit.onnx` model recognizes “hey livekit”.
A classifier must match its selected backend.

## Foreground operation

For troubleshooting, you can run the daemon in the terminal with monitoring
forced on. This is an alternative to the registered service:

```sh
aria service stop
aria wake run
```

Stop the foreground process with Ctrl-C before running `aria service start`
to restore the registered service.
