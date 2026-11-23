# Vocabulary evaluation

Use text replay to check what dictation writes with your dictionary and project
vocabulary. You can replay the built-in cases without a model or an API key.
Run the commands from the repository root.

## Run the fixtures

Choose a fresh output path. The report contains transcripts and vocabulary.

```sh
cargo run \
  --release \
  --locked \
  -p aria-bench \
  --example vocabulary \
  -- fixtures fixtures.json
```

Open `fixtures.json` and find `dash_name`. With project vocabulary, “look in
aria dash stt” becomes `look in aria-stt`. The report has file mode `0600`.

To compare personal dictionary cases against a saved memory evaluation, append
the report path after `fixtures.json`. Replay checks every case's input,
expected text, terms and learned pairs before using its saved output.
Otherwise the baseline is the uncorrected input.

The fixtures check specific behavior. To measure accuracy on real speech, use
recordings you kept separate from development.

## Replay your recordings

Use the same Cargo command with one of these argument lists after `--`.

| Arguments | Input |
|---|---|
| `evaluate SUITE OUTPUT` | A JSON suite in the format of `crates/aria-bench/src/stt/vocabulary-cases.json`. |
| `replay TRACES ANNOTATIONS BACKEND OUTPUT` | A completed speech-to-text JSONL file and a suite covering every utterance for the chosen backend. Keep `TRACES.manifest.json` beside `TRACES`. Each spoken reference must match the trace. |

Keep each dataset's published split. Give overlapping clips and repetitions
from the same source the same `group`. If you take vocabulary from reference
transcripts, disclose that the matcher had access to the answers.

- Record `whole_input` and finalized `segments` separately. Set a missing
  observation to `null`. Joining segments can't reproduce whole-recording
  recognition.
- For streaming, you can supply one `sentence_ended_before` value per segment.
  Use `true` for a sentence break, `false` for a continuation and `null` for
  unknown. Trace replay reads these values from the recording's results.
- Mark each target in both references. `written_words` counts exact tokens
  separated by whitespace. `spoken_words` counts normalized spoken words.
  Start at zero, exclude the end index and keep ranges in order without overlap.

## Read the results

Compare `personal` with `project` to see what adding project terms changes.
Both use dictation's rules for spoken numbers and symbols. For example, the
spoken reference can contain “twelve boxes” while the written reference expects
`12 boxes`. A learned pair applies only while its written form appears in
`personal_terms`.

Streaming replay treats consecutive segments as uninterrupted speech.
`output_segments` shows each release, including a closing mark held until the
end. `output` shows the text with insertion spacing. The streaming baseline
joins the original segments, or saved legacy segments, with spaces.

| Report field | What it measures |
|---|---|
| `canonical_target_recall`, `overall_recovery` | Share of written targets spelled exactly, and share of baseline target errors corrected. Target tokens must match consecutively, including punctuation. |
| `introduced_errors`, `introduced_error_cases` | New error occurrences at aligned reference positions, and cases with any such error. |
| `introduced_incorrect_vocabulary_spans` | New incorrect occurrences of vocabulary names, including distractors. Repetitions count separately. The longest overlapping name wins. |
| `harmed_cases` | Previously exact cases that now contain an error. |
| `changed_case_exact_precision` | Share of changed cases with entirely correct output. |
| `spoken_reference_wer` | Word error rate against the spoken reference. Correct written forms such as `aria-stt` can count as errors here. |

The report includes counts, denominators, failures and a grouped interval for
the change in spoken word error rate when the data permit one. The scorer
assigns insertions to the following reference word, or the last word at the end.

## Measure delivery latency

Use the same benchmark executable, model, WAV and personal dictionary for both
daemon builds. For Moonshine, set `ARIA_TEST_MOONSHINE_MODEL`,
`ARIA_TEST_DICTATION_WAV` and `ARIA_BENCH_BACKENDS=moonshine`. Set the number
of trials before measuring, for example `ARIA_BENCH_RUNS=30` and
`ARIA_BENCH_WARMUPS=2`. Replace the uppercase paths below with absolute paths.

```sh
ARIA_BENCH_DAEMON=BASELINE_BINARY \
ARIA_BENCH_RESULTS=BASELINE_DIR \
just bench-latency

ARIA_BENCH_DAEMON=CANDIDATE_BINARY \
ARIA_BENCH_RESULTS=CANDIDATE_DIR \
ARIA_BENCH_BASELINE=BASELINE_DIR/dictation-latency.json \
ARIA_BENCH_MAX_REGRESSION=0.05 \
ARIA_BENCH_REQUIRE_SAME_TEXT=1 \
just bench-latency
```

Alternate which build runs first across pairs. The example allows a 5% median
slowdown. Use the same-text check for speech you expect to stay unchanged.
For intended corrections, compare against the written reference instead.

Set `ARIA_BENCH_PERSONAL_TERMS` to use the same dictionary file in both runs.
Set `ARIA_BENCH_VOCABULARY_DIR` on the candidate run to measure project
vocabulary. The benchmark waits for the daemon to load project terms.

By default, `release->delivered` and `capture->delivered` measure time to
clipboard delivery. On a Mac, set `ARIA_BENCH_STREAMING=1` to measure
insertion in a test app. Give the daemon
Accessibility permission and keep the desktop idle during the run.
Unset `ARIA_BENCH_TARGET_PID` and `ARIA_BENCH_TARGET_TEXT_FILE` so the benchmark
can manage its own app. `request->first-receipt` and `request->final-receipt`
measure when the app observes its first and last text changes.

## Run focused diagnostics

For matcher timings, use `microbench OUTPUT` after `--` in the fixture command.
It reports compilation time, estimated memory and warmed calls with 100 to
100,000 names. It records failed compilations.

The streaming pipeline test uses a deterministic recognizer to check the
delivered text and time gaps between releases.

```sh
ARIA_VOCABULARY_PIPELINE_RESULTS="$PWD/streaming-pipeline.json" \
just rust-test aria-voice \
  --release \
  --lib \
  vocabulary_streaming_delivery_benchmark \
  -- \
  --ignored
```

For project vocabulary collection, run the same test command with
`vocabulary_collection_benchmark` as the test name. Set
`ARIA_VOCABULARY_COLLECTION_RESULTS` to a fresh path in a directory with mode
`0700`. Set `ARIA_VOCABULARY_SCAN_DIR` to include a local folder. The report
contains counts and timings and omits filenames and extracted names.

On a Mac, test Unicode insertion, selection replacement and focus loss on an
idle desktop. Give the test executable Accessibility permission.

```sh
just rust-test aria \
  --test macos_delivery \
  native_streaming_target_receives_unicode_selection_and_rejects_focus_loss \
  -- \
  --ignored \
  --test-threads=1
```

See [Speech benchmarks](README.md#latency) for paired latency comparisons.
