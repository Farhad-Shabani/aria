# Speech benchmarks

These recipes measure how well Aria's speech engines hear, and how fast they
respond. Run them from the repository root. They build in release mode.

## Quick start

```sh
just accuracy-check
```

This runs Whisper tiny.en on 100 recordings. It fails if accuracy dropped
against the committed reference. CI runs it on macOS.

## The test audio

`just corpus` builds 100 short recordings from LibriSpeech, a free set of
read audiobooks, into `build/corpus`. It tests whether an engine hears the
words, not how it handles dictation.

[`corpus/librispeech-test-clean.tsv`](corpus/librispeech-test-clean.tsv)
lists each recording, what was said, and a hash of its audio. The first
build downloads 346 MB and needs ffmpeg. Audio that doesn't match its hash
fails the build, so every machine tests the same audio.

## Word error rate

Word error rate is the share of words an engine gets wrong.

```sh
just accuracy --model whisper=/absolute/path/ggml-base.en.bin
```

Each `--model <engine>=<path>` adds one engine. Without it, the
`ARIA_TEST_*_MODEL` variables choose. The run prints a rate per engine. It
also saves one JSON row per recording, plus a `.manifest.json` of the run's
settings. Keep the two files together.

Two runs compare only if they used the same audio, models, device, settings,
and build. Otherwise the comparison refuses.

| Recipe | Measures |
|---|---|
| `just bench-quality` | Accuracy while streaming |
| `just bench-realtime` | Accuracy while streaming at real-time speed |
| `just bench-first-text` | Time to the first text |
| `just bench-latency` | Time from the end of speech to delivered text |

## Updating the reference

`just accuracy-check` compares against
[`baselines/stt-whisper-tiny-en.jsonl`](baselines/stt-whisper-tiny-en.jsonl).
It lists every recording whose transcript changed. It fails unless it is
confident accuracy did not drop.

When a change is meant to alter results, replace the reference. Then review
the listed transcripts in the pull request.

```sh
just accuracy-rebaseline /absolute/path/results.jsonl
```

Use the `results.jsonl` from the `accuracy-check` artifact CI uploads. A
reference from CI's own machine avoids GPU differences. Without a path, the
recipe records a local run instead. Either way, a run with failures or
different settings is refused, and the old reference stays.

## Comparing two saved runs

```sh
just bench-compare /absolute/path/candidate.jsonl \
    --baseline /absolute/path/baseline.jsonl \
    --max-wer-increase 0
```

No model loads. It passes only if it is 95% confident the error rate rose by
at most `--max-wer-increase`, a fraction: `0.001` is 0.1 percentage points.
A failed recording, or too few recordings, never passes.

## Latency

Latency varies by machine, so none is committed. Compare two runs from the
same machine, back to back, with nothing else heavy running. Fix the run
count first. Rerunning until a comparison passes proves nothing.

`ARIA_BENCH_BASELINE` names a saved latency JSON to compare against.
`ARIA_BENCH_MAX_REGRESSION` allows a slowdown, as a fraction of the
baseline's median. It defaults to 0. To recompare saved runs without the
daemon:

```sh
just bench-latency-compare \
    /absolute/path/baseline.json \
    /absolute/path/candidate.json \
    0.1
```

Here `0.1` allows a 10% slower median.
