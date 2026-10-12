# Vocabulary evaluation

Run from the repository root. These commands require local Rust dependencies, no API key.
Output artifacts are private and cannot overwrite an existing file.

```sh
cargo run --release --locked -p aria-bench \
  --example vocabulary -- \
  fixtures /absolute/path/fixtures.json \
  /absolute/path/frozen-memory-report.json

cargo run --release --locked -p aria-bench \
  --example vocabulary -- \
  microbench /absolute/path/microbench.json

cargo test --locked -p aria-bench vocabulary
```

The frozen memory report is optional. With it, every personal lexicon case
compares against its saved legacy output after input/rule identity checks.
Without it, the baseline is labeled uncorrected input. The fixture set is a
deterministic contract check, not held-out audio or a population accuracy estimate.

`evaluate SUITE OUTPUT` accepts the schema in `src/stt/vocabulary-cases.json`.
`whole_input` and `segments` are independent observations; set an unavailable
one to `null`. Do not manufacture a whole-recording ASR result by joining segments.
Target `written_words` ranges index exact whitespace tokens in the intended output.
`spoken_words` ranges index the existing scorer's normalized spoken reference.
Ranges are zero-based, end-exclusive, ordered and non-overlapping. `group` identifies
the independent source recording/project; overlapping clips and repetitions share it.

`replay TRACES ANNOTATIONS BACKEND OUTPUT` validates a completed STT artifact of
the current schema and completion manifest before replaying its frozen hypotheses/segments. Annotations
use the same suite schema, with one case per utterance and references matching the
artifact. All cases must be covered. Dataset conversion must preserve the published
split and source groups; vocabulary supplied from references is labeled oracle context.
No public dataset is downloaded by this tool.

Reports retain raw before/after text, failures, counts, denominators and grouped WER
intervals. Case-level exact precision is deliberately separate from edit-level
precision, which this evaluation does not estimate. Canonical target recall and recovery
use exact aligned target spans. Spoken-reference WER may penalize desired written
formatting such as `aria-stt`; do not use it as the identifier spelling score.
Schema-2 replay reports require contiguous canonical spans. `introduced_errors` counts
candidate error occurrences absent from the baseline at the same reference position;
`introduced_error_cases` counts affected cases. `introduced_incorrect_vocabulary_spans`
also counts new incorrect canonical occurrences, including distractors, by name and
aligned reference location. Repeated occurrences retain their multiplicity; overlapping
names use the longest span. These use exact whitespace-token alignment, including
punctuation, and its deterministic attribution for repeated words. `harmed_cases`
retains the narrower count of previously exact whole cases that became incorrect.
Target/background insertion errors attach to the following reference word, or the last
word at EOF. This local convention is not a claim of matching an external scorer.

Microbenchmarks report compilation cost, estimated snapshot bytes and warmed matcher
calls at 100–100,000 names, retaining failed compilations. These timings exclude capture,
ASR, delivery and refresh contention. The daemon's paired real-time benchmark is still
required before claiming no responsiveness regression.

## Daemon delivery baseline

The `dictation_latency` bench drives a chosen frozen daemon through RPC. Both
revisions use the same measurement executable. It records progress timestamps,
complete output and clipboard readback, and reports `release->delivered` and
`capture->delivered` timing. This is complete clipboard delivery;
it does not measure streaming insertion into another application.

From the repository root, collect the frozen baseline, then repeat with the candidate daemon:

```sh
ARIA_BENCH_DAEMON=/absolute/path/frozen-baseline-aria \
ARIA_TEST_MOONSHINE_MODEL=/absolute/path/moonshine/base \
ARIA_TEST_DICTATION_WAV=/absolute/path/fixture.wav \
ARIA_BENCH_BACKENDS=moonshine \
ARIA_BENCH_RUNS=30 \
ARIA_BENCH_WARMUPS=2 \
ARIA_BENCH_RESULTS=/absolute/path/baseline \
cargo bench --locked --bench dictation_latency

ARIA_BENCH_DAEMON=/absolute/path/candidate-aria \
ARIA_TEST_MOONSHINE_MODEL=/absolute/path/moonshine/base \
ARIA_TEST_DICTATION_WAV=/absolute/path/fixture.wav \
ARIA_BENCH_BACKENDS=moonshine \
ARIA_BENCH_RUNS=30 \
ARIA_BENCH_WARMUPS=2 \
ARIA_BENCH_RESULTS=/absolute/path/candidate \
ARIA_BENCH_BASELINE=/absolute/path/baseline/dictation-latency.json \
ARIA_BENCH_MAX_REGRESSION_MS=5 \
ARIA_BENCH_REQUIRE_SAME_TEXT=1 \
cargo bench --locked --bench dictation_latency
```

Run the candidate again with `ARIA_BENCH_VOCABULARY_DIR` naming a prepared project.
Preparation enables the feature and waits for a nonempty ready snapshot before
measurement. `ARIA_BENCH_PERSONAL_TERMS` optionally supplies the same lexicon file
to both isolated daemons. Preserve model/audio/personal identities and alternate
baseline/candidate ordering across process pairs. Thirty repetitions are an initial
collection budget, not evidence that a 5 ms margin is statistically resolved.

The same-text gate suits ordinary-speech negatives where project context should
change nothing. Positive correction cases need their frozen intended-output references
and the vocabulary scorer. The gate's tests inject latency and deleted/duplicated
words; both cause rejection.

The production streaming pipeline also has an ignored benchmark using an owned sink:

```sh
ARIA_VOCABULARY_PIPELINE_RESULTS=/absolute/path/streaming-pipeline.json \
cargo test --release --locked -p aria --lib \
  vocabulary_streaming_delivery_benchmark -- \
  --ignored \
  --nocapture
```

It alternates enabled/disabled matching, records first sink receipt and gaps, and checks
that concatenated delivered segments equal the final transcript. Its deterministic
transcriber and immediate WAV replay isolate pipeline behavior; they do not establish
real-ASR streaming latency or platform insertion latency.

For real macOS streaming insertion, run `dictation_latency` with the existing model,
WAV, daemon and result settings plus `ARIA_BENCH_STREAMING=1`. The runner uses
`xcrun swiftc` in Swift 6 mode to compile its AppKit target in a private temporary
directory. It launches the target, resets its text and cursor before each trial,
waits for keyboard focus, and closes it afterward, including on failure. Leave the
Mac's desktop undisturbed during the run. The daemon needs Accessibility permission;
WAV EOF ends capture. The runner refuses to start while `ARIA_BENCH_TARGET_PID`
or `ARIA_BENCH_TARGET_TEXT_FILE` is set.

The target timestamps text-change callbacks with `CLOCK_UPTIME_RAW`, the same clock
used by the Rust runner. It buffers at most 4,096 observations and 64 KiB of text per
trial, with no per-change file writes. Replies travel over an inherited local socket;
only the benchmark result artifact retains the text and timings. Trial IDs, exact
final text, receipt order and focus-loss checks must pass. Focus checks do not make
OS event routing atomic: the daemon still binds an application, not an exact control.

`request->first-receipt` and `request->final-receipt` measure request start to the
first and last target text-change callbacks. They measure application receipt, not
screen rendering or microphone-to-text latency. The `target` trace includes receipt
lengths, gaps and exact final text. Daemon segment timings remain separate in
`capture->first-segment` and `corrected_segments`. Streaming comparisons check
both receipt metrics; `rpc-delivery-v2` prevents comparison against older manual
target runs. Complete-mode delivery-log metrics remain omitted in streaming mode.

The native insertion check needs no speech model. It opens two test windows and
requires Accessibility permission on the test executable. Keep the desktop idle;
switching apps or typing invalidates the trial:

```sh
just rust-test aria \
  --lib \
  native_streaming_target_receives_unicode_selection_and_rejects_focus_loss \
  -- \
  --ignored \
  --test-threads=1
```

It exercises Unicode insertion across event chunks, replacement of a UTF-16
selection, refusal after focus changes, and resetting the target for another trial.
Protocol and receipt-validation tests run with `just rust-test aria --test latency_benches`.

## Generic collector diagnostics

The generic collector has an ignored release diagnostic covering extraction, selection,
SQLite reuse, and compilation. Create a private output directory first. The optional
scan directory is read locally; the report contains aggregate counts and timings, not
its filenames or extracted names.

```sh
ARIA_VOCABULARY_COLLECTION_RESULTS=/absolute/private/directory/collection.json \
ARIA_VOCABULARY_SCAN_DIR=/absolute/path/to/selected/folder \
cargo test --release --locked -p aria --lib \
  vocabulary_collection_benchmark -- \
  --ignored \
  --nocapture
```
