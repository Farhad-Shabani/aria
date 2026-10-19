# Acceptance cases

Each `*.yaml` file here is one test of the `aria` binary. The test is named
after the file:

```sh
cargo test -p aria --test acceptance                       # every case
cargo test -p aria --test acceptance -- history-fresh      # cases whose name matches
```

A case runs in a sandbox: a fresh temporary home holding the case's config,
its own runtime directory, no inherited `ARIA_*` variables, no sounds or
banners, and no global hotkey. Commands get no terminal input.

## Shape

```yaml
name: what the case shows, as a sentence
platform: macos            # optional; the case is ignored on other systems
config: |                  # written to ~/.aria/config.yaml
  transcription:
    backend: whisper
    model: ${WHISPER_MODEL}
  capture:
    backend: wav-file
    path: ${FIXTURES}/silence.wav
expect:
  daemon:                  # optional; leave it out to run no daemon
    startup: success       # or failure
    stderr_contains: [..]  # every entry must appear
  client:                  # `aria` commands run in order
    - run: [history, list]
      exit: success        # the default; or failure
      stdout_contains: [..]
      stderr_contains: [..]
```

- **`startup: failure`** runs `aria daemon` until it exits, and judges that
  it failed. Client steps are refused here, since nothing serves them.
- **`startup: success`** waits until the daemon logs that it is ready, runs
  the client steps against it, then stops it with SIGTERM and judges a clean
  exit. A daemon that exits before it is ready fails the case.
- **No `daemon`** runs the client steps with nothing serving. It covers
  commands that never need a daemon, and the error the rest give when none
  runs.

A serving daemon holds its capture open. Give it `capture: wav-file`, as
above, or it opens the microphone.

## Placeholders

`${FIXTURES}` is a private copy of this package's `tests/fixtures`
directory, since the daemon refuses an input file other users can write.

Any other `${NAME}` is read from the environment variable `ARIA_TEST_NAME`,
for inputs that differ per machine, such as a model file. When it is unset,
the case is ignored, and the reason shows beside its name:

```sh
ARIA_TEST_WHISPER_MODEL=~/.aria/models/whisper/ggml-base.en.bin \
  cargo test -p aria --test acceptance
```

## When a case fails

The failure names the command and what it lacked, then shows its exit
status, stdout, stderr, and the last lines of the daemon log.
