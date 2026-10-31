# Acceptance cases

Each `*.yaml` file here is one test of the `aria` binary, named after the
file. Run them with:

```sh
just rust-acceptance                    # every case, with the model CI uses
just rust-acceptance -- history-fresh   # only cases whose name contains this
```

Each case starts from scratch. It gets a temporary home directory holding
only its settings, and its own runtime directory. Your `ARIA_*` variables are
cleared, sounds and banners are off, and the Aria key is off. A command
that would ask for input reads nothing, so it fails instead of waiting.

## Writing a case

```yaml
name: what the case shows, as a sentence
platform: macos            # optional; other systems skip the case
settings: |                # written to ~/.aria/settings.toml
  [stt]
  backend = "whisper"
  model = "${WHISPER_MODEL}"

  [capture]
  backend = "wav_file"
  path = "${FIXTURES}/silence.wav"
expect:
  daemon:                  # optional; leave it out to start no daemon
    startup: success       # or failure
    stderr_contains: ["text"]   # every entry must appear
  before_daemon:           # optional; `aria` commands run before any daemon starts
    - run: [settings, set, assistant.id, codex]
  client:                  # `aria` commands, run in order
    - run: [history, list]
      exit: success        # the default; or failure
      stdout_contains: ["text"]
      stderr_contains: ["text"]
```

A case runs one of three ways:

- **`startup: failure`** runs `aria daemon` and passes if it exits with an
  error. A case like this can't have client commands, because no daemon is
  left to answer them.
- **`startup: success`** waits until the daemon's log says it is ready, then
  runs the client commands. It ends with SIGTERM and passes only if the
  daemon exits cleanly. A daemon that exits before it is ready fails the case.
- **No `daemon`** runs the client commands with no daemon at all. Use it for
  commands that never need one, and for the error a command gives when it
  needs one and none is running.

A running daemon opens its audio input. Give it a `wav_file` capture backend, as
above, or it opens your microphone.

## Placeholders

`${FIXTURES}` points to a copy of this package's `tests/fixtures` that only
you can write. The daemon refuses an audio file that other users can write.

Any other placeholder comes from an environment variable with `ARIA_TEST_` in
front. `${WHISPER_MODEL}` reads `ARIA_TEST_WHISPER_MODEL`, for example. If the
variable is unset, the case is skipped and the output shows which variable it
needs. `just rust-acceptance` sets `ARIA_TEST_WHISPER_MODEL` for you.

## When a case fails

The failure names the command and what it expected. It then shows the exit
status, stdout, stderr, and the last lines of the daemon log.
