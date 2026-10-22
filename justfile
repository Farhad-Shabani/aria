# Aria development task runner
# Optional inference sidecar owned by the Rust STT crate
sidecar := "crates/aria-stt/src/parakeet_mlx/parakeet_mlx.py"
sidecar_tests := "crates/aria-stt/tests/test_parakeet_mlx_sidecar.py"

# Everyday

# Run Rust and supporting-tool checks
check: rust-native support-check

# Run workspace tests
test: rust-test-all

# Check Rust formatting and lints
lint: rust-format rust-deps rust-lint-all

# Format Rust and Cargo manifests
fix:
    cargo fmt --all
    taplo format --config taplo.toml

# Check integration scripts and the optional MLX sidecar
support-check:
    bash integrations/test.sh
    uvx ruff@0.17.0 check {{ sidecar }} {{ sidecar_tests }}
    uvx ruff@0.17.0 format --check {{ sidecar }} {{ sidecar_tests }}
    uv run --no-project --with pytest pytest -q {{ sidecar_tests }}

# Rust: fast feedback

# Quickly check one Rust package
rust-check package="aria":
    cargo check --locked -p {{ package }}

# Quickly lint one Rust package
rust-lint package="aria":
    cargo clippy --locked -p {{ package }} -- -D warnings

# Quickly test one Rust package, optionally with a test filter
rust-test package="aria" *args:
    cargo test --locked -p {{ package }} {{ args }}

# Run the acceptance cases, serving ones included
rust-acceptance *args:
    #!/usr/bin/env bash
    set -euo pipefail
    model="$(just test-model)"
    ARIA_TEST_WHISPER_MODEL="$model" cargo test --locked -p aria --test acceptance {{ args }}

# Rust: full and cross-target gates

rust_native_features := if os() == "macos" { "--all-features" } else { "" }

# Check the complete Rust workspace for the native host
rust-native: rust-format rust-deps rust-lint-all rust-test-all

# Check dependency usage, unlinked Rust files, advisories, and licenses
rust-deps:
    cargo shear --locked --deny-warnings
    cargo deny --locked check

# Check Rust and TOML formatting
rust-format:
    taplo format --check --config taplo.toml
    cargo fmt --all --check

# Clippy already compiles the front-end; a separate check would compile it twice.
# Check compilation and lints across the complete Rust workspace
rust-lint-all:
    cargo clippy --locked --workspace --all-targets {{ rust_native_features }} -- -D warnings

# Run all Rust tests. `--all-targets` leaves out doctests, so they run on their own.
rust-test-all:
    cargo test --locked --workspace --all-targets {{ rust_native_features }}
    cargo test --locked --workspace --doc {{ rust_native_features }}

# Check every optional feature on its own
rust-features: rust-kit-features rust-stt-features rust-tts-features rust-wake-features

# Check the toolkit's hardware and model features one at a time
rust-kit-features:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo check --locked -p aria-kit --no-default-features --all-targets
    for feature in microphone playback silero; do
        cargo check --locked -p aria-kit --no-default-features --all-targets --features "$feature"
    done

# Check STT engines without relying on another engine's features
rust-stt-features:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo check --locked -p aria-stt --no-default-features --all-targets
    for engine in parakeet moonshine nemotron whisper whisper-candle parakeet-mlx; do
        cargo check --locked -p aria-stt --no-default-features --all-targets --features "$engine"
    done
    if [ "$(uname -s)" = Darwin ]; then
        cargo check --locked -p aria-stt --no-default-features --all-targets --features parakeet-gpu
    fi

# Check TTS engines without relying on another engine's features
rust-tts-features:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo check --locked -p aria-tts --no-default-features --all-targets
    for engine in kokoro qwen system; do
        cargo check --locked -p aria-tts --no-default-features --all-targets --features "$engine"
    done
    if [ "$(uname -s)" = Darwin ]; then
        for acceleration in qwen-accelerate qwen-metal; do
            cargo check --locked -p aria-tts --no-default-features --all-targets --features "$acceleration"
        done
    fi

# Check wake engines without relying on another engine's features
rust-wake-features:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo check --locked -p aria-wake --no-default-features --all-targets
    for engine in livekit openwakeword; do
        cargo check --locked -p aria-wake --no-default-features --all-targets --features "$engine"
    done

# Prove the browser pipeline still compiles for wasm
rust-wasm:
    cargo check --locked -p aria-kit -p aria-stt -p aria-web --features aria-stt/whisper-candle --target wasm32-unknown-unknown

# Rosetta loses spawn errors. Native ARM needs Clang and FP16 for its kernels.
rust_linux_platform := if arch() == "aarch64" { "linux/arm64" } else { "linux/amd64" }

# Check the complete Rust workspace on Linux
rust-linux:
    docker run --rm --init --platform {{ rust_linux_platform }} \
        -v {{ justfile_directory() }}:/src \
        -v aria-linux-cargo:/usr/local/cargo/registry \
        -v aria-linux-native-target:/target \
        -e CARGO_TARGET_DIR=/target \
        -e CARGO_INCREMENTAL=0 \
        -e CARGO_PROFILE_DEV_DEBUG=0 \
        -e CARGO_PROFILE_TEST_DEBUG=0 \
        -e 'CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS=-C target-feature=+fp16' \
        -e CC=clang -e CXX=clang++ \
        -w /src rust:1-trixie \
        bash -c "apt-get update -qq \
            && apt-get install -y -qq cmake clang libasound2-dev pkg-config espeak-ng xvfb xauth \
            && rustup component add clippy \
            && cargo clippy --locked --workspace --all-targets -- -D warnings \
            && setpriv --bounding-set=-dac_override,-dac_read_search \
                xvfb-run -a cargo test --locked --workspace --all-targets"

# Rust: build artifacts

# Remove Rust build artifacts; pass Cargo selectors or --dry-run as needed
rust-clean *args:
    cargo clean {{ args }}

# Swift: the macOS app

swift_srcs := "clients/Aria clients/Shared clients/Tests"

# Run the macOS app checks
swift-check: swift-format swift-stubs swift-test

# Check Swift formatting and lints (swift-format ships in the Xcode toolchain)
swift-format:
    swift format lint --strict --parallel --recursive {{ swift_srcs }}

# Auto-fix Swift formatting
swift-fix:
    swift format --in-place --parallel --recursive {{ swift_srcs }}

# Regenerate the gitignored Xcode project from project.yml. XcodeGen has no
# optional includes, so seed an empty signing overlay when none exists.
# Builds use only the committed Package.resolved, as Cargo builds --locked.
# XcodeGen leaves that file in place.
# Generate the Xcode project
swift-project:
    test -f clients/project.local.yml || printf '{}\n' > clients/project.local.yml
    cd clients && xcodegen generate

# Run the macOS app tests
swift-test: swift-project
    cd clients && xcodebuild test -project Aria.xcodeproj -scheme Aria \
        -destination "platform=macOS,arch=$(uname -m)" -onlyUsePackageVersionsFromResolvedFile \
        CODE_SIGNING_ALLOWED=NO -quiet

version := `sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1`

# Build Aria.app with the daemon inside, into clients/build/mac
app profile="dist": swift-project
    cargo build --locked --profile {{ profile }} --bin aria
    cd clients && xcodebuild -project Aria.xcodeproj -scheme Aria \
        -configuration Release -derivedDataPath build/mac -onlyUsePackageVersionsFromResolvedFile \
        ARCHS=$(uname -m) \
        CODE_SIGNING_ALLOWED=NO MARKETING_VERSION={{ version }} CURRENT_PROJECT_VERSION={{ version }} -quiet build
    scripts/macos/bundle.sh clients/build/mac/Build/Products/Release/Aria.app {{ profile }}
    scripts/macos/check-minos.sh clients/build/mac/Build/Products/Release/Aria.app

swift_resolved := "clients/Aria.xcodeproj/project.xcworkspace/xcshareddata/swiftpm/Package.resolved"
swift_plugins := "clients/build/protoc-plugins"

# Generate Swift client stubs from the daemon protocol
proto-swift: (_swift-stubs "clients/Generated")

# Check the committed Swift stubs match the daemon protocol
swift-stubs: (_swift-stubs "clients/build/stubs")
    diff -r clients/build/stubs clients/Generated

# Health comes along so clients can probe the daemon. The Rust side gets its
# own copy from tonic-health.
_swift-stubs out: _swift-plugins
    rm -rf {{ out }}/Messages {{ out }}/Rpc
    mkdir -p {{ out }}/Messages {{ out }}/Rpc
    protoc --proto_path=proto \
        --plugin={{ swift_plugins }}/build/release/protoc-gen-swift \
        --plugin={{ swift_plugins }}/build/release/protoc-gen-grpc-swift-2 \
        --swift_out={{ out }}/Messages \
        --swift_opt=Visibility=Public,FileNaming=DropPath \
        --grpc-swift-2_out={{ out }}/Rpc \
        --grpc-swift-2_opt=Visibility=Public,Client=true,Server=false,FileNaming=DropPath \
        proto/aria/daemon/v1/daemon.proto \
        proto/aria/daemon/v1/errors.proto \
        proto/aria/daemon/v1/relay.proto \
        proto/aria/daemon/v1/speech.proto \
        proto/aria/setup/v1/setup.proto \
        proto/grpc/health/v1/health.proto

# The plugins build at the app's pinned versions, so the stubs match the
# libraries the app links.
_swift-plugins:
    #!/usr/bin/env bash
    set -euo pipefail
    version=$(jq -er '.pins[] | select(.identity == "grpc-swift-protobuf") | .state.version' {{ swift_resolved }})
    source={{ swift_plugins }}/grpc-swift-protobuf-$version
    if [ ! -d "$source" ]; then
        git clone --quiet --depth 1 --branch "$version" \
            https://github.com/grpc/grpc-swift-protobuf.git "$source"
    fi
    cp {{ swift_resolved }} "$source/Package.resolved"
    for product in protoc-gen-swift protoc-gen-grpc-swift-2; do
        swift build --package-path "$source" --scratch-path {{ swift_plugins }}/build \
            -c release --only-use-versions-from-resolved-file --product "$product" --quiet
    done

# Release

# Write the aria binary's third-party licenses to target/THIRD-PARTY-LICENSES.md
notices:
    #!/usr/bin/env bash
    set -euo pipefail
    out=target/THIRD-PARTY-LICENSES.md
    mkdir -p target
    cargo about generate --locked -m crates/aria/Cargo.toml \
        -c licenses/about.toml -o "$out" licenses/about.hbs
    full_text() { printf '\n### %s\n\n```text\n' "$1"; cat "$2"; printf '```\n'; }
    {
        cat licenses/native.md
        printf '\n## Full license texts\n'
        full_text "ONNX Runtime third-party notices" licenses/vendor/onnxruntime-notices.txt
    } >> "$out"

# Package Aria.app into a DMG in dist/, with its licenses
release-macos: app notices
    scripts/macos/release.sh

# Package the aria binary and its licenses into a tarball in dist/
release-linux: notices
    scripts/linux/release.sh {{ version }}

# Write dist/SHA256SUMS, the manifest install.sh reads
release-checksums:
    cp install.sh dist/install.sh
    cd dist && shasum -a 256 install.sh Aria.dmg aria-linux-x86_64.tar.gz > SHA256SUMS

# Evaluation and benchmarks

# Speech corpus for `just accuracy`, into a gitignored directory
corpus utterances="100":
    cargo run --locked --release -p aria-bench -- corpus \
        {{ justfile_directory() }}/build/corpus --utterances {{ utterances }}

# Long-form corpus (90-115 s recordings) for streaming/agreement runs
corpus-long recordings="12":
    crates/aria-bench/scripts/corpus-long.sh {{ justfile_directory() }}/build/corpus-long \
        {{ recordings }} {{ justfile_directory() }}/build/corpus/librispeech

# Room noise recordings (DEMAND) for `just corpus-noisy`
noise:
    cargo run --locked --release -p aria-bench -- noise {{ justfile_directory() }}/build/noise

# Copy of a corpus under room noise at a signal-to-noise ratio in dB
corpus-noisy snr="10" source=(justfile_directory() + "/build/corpus-long/manifest.tsv"):
    cargo run --locked --release -p aria-bench -- noisy \
        --corpus "{{ absolute_path(source) }}" \
        --noise {{ justfile_directory() }}/build/noise \
        --snr {{ snr }} \
        --output {{ justfile_directory() }}/build/corpus-noisy-{{ snr }}db

test_model_url := "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-tiny.en.bin"
test_model_sha256 := "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f"

# Fetch the Whisper model the acceptance cases and accuracy check run, and print its path
test-model directory=(cache_directory() / "aria-test-models"):
    #!/usr/bin/env bash
    set -euo pipefail
    model="{{ directory }}/ggml-tiny.en.bin"
    if [ ! -f "$model" ]; then
        mkdir -p "{{ directory }}"
        curl -sSfL --retry 3 -o "$model.part" "{{ test_model_url }}"
        mv "$model.part" "$model"
    fi
    echo "{{ test_model_sha256 }}  $model" | shasum -a 256 -c >&2
    chmod 644 "$model"
    echo "$model"

# The environment's ARIA_TEST_*_MODEL variables select the backends.
# Measure word error rate against `just corpus`
accuracy *flags:
    cargo run --locked --release -p aria-bench -- stt \
        --corpus {{ justfile_directory() }}/build/corpus/manifest.tsv \
        {{ flags }}

stt_reference := "crates/aria-bench/baselines/stt-whisper-tiny-en.jsonl"

# Fail when word error rate regresses against the committed reference
accuracy-check models=(cache_directory() / "aria-test-models"):
    just _accuracy-run build/accuracy-check "{{ models }}" --baseline {{ stt_reference }} \
        || { echo "if the change is intended, rebaseline from CI's accuracy-check artifact: just accuracy-rebaseline <results.jsonl>" >&2; exit 1; }

# Replace the committed reference with CI's accuracy-check results, or a fresh local run
accuracy-rebaseline results="":
    #!/usr/bin/env bash
    set -euo pipefail
    results="{{ results }}"
    if [ -z "$results" ]; then
        just _accuracy-run build/accuracy-rebaseline
        results=build/accuracy-rebaseline/results.jsonl
    fi
    cargo run --locked --release -p aria-bench -- reference-stt "$results" {{ stt_reference }}

_accuracy-run directory models=(cache_directory() / "aria-test-models") *flags:
    #!/usr/bin/env bash
    set -euo pipefail
    just corpus
    model="$(just test-model "{{ models }}")"
    rm -rf "{{ directory }}"
    mkdir -p "{{ directory }}"
    cargo run --locked --release -p aria-bench -- stt \
        --corpus {{ justfile_directory() }}/build/corpus/manifest.tsv \
        --model whisper="$model" \
        --results "{{ directory }}/results.jsonl" \
        {{ flags }}

# Streaming quality without wall-clock replay; defaults to shipping cadences
bench-quality corpus=(justfile_directory() + "/build/corpus/manifest.tsv") *flags:
    cargo run --locked --release -p aria-bench -- stt \
        --corpus "{{ absolute_path(corpus) }}" --mode streaming {{ flags }}

# Real-time streaming quality and segment latency from the same inference
bench-realtime corpus=(justfile_directory() + "/build/corpus/manifest.tsv") *flags:
    cargo run --locked --release -p aria-bench -- stt \
        --corpus "{{ absolute_path(corpus) }}" --mode streaming --realtime {{ flags }}

# Offline report or paired WER comparison; no model is loaded
bench-compare results *flags:
    cargo run --locked --release -p aria-bench -- compare-stt \
        "{{ absolute_path(results) }}" {{ flags }}

# Speaker-label accuracy on synthesized conversations (realtime; ~4 min)
diarize-bench *flags:
    cargo run --locked --release -p aria-bench -- diarize {{ flags }}

# Speaker verification error rates over a trial list, and what the shipped
# cosine gates would cost on it. See crates/aria-bench-speaker/src/trials.rs for the
# list format; VoxCeleb's published lists work as they are.
# Measure speaker verification error rates
bench-speaker trials model="":
    cargo run --locked --release -p aria-bench-speaker -- {{ trials }} {{ model }}

# Check memory scoring and data adapters without model calls
test-memory:
    cargo test --release --locked -p aria --lib memory::eval
    cargo test --release --locked -p aria-harness

# Memory effectiveness; live mode invokes the Claude CLI
bench-memory mode="offline" model="sonnet" repeats="3":
    ARIA_MEMORY_EVAL_MODE="{{ mode }}" ARIA_MEMORY_EVAL_MODEL="{{ model }}" \
        ARIA_MEMORY_EVAL_REPEATS="{{ repeats }}" \
        cargo test --release --locked -p aria --lib memory::eval::effectiveness -- \
        --ignored --exact --nocapture

# Daemon latency only; ARIA_BENCH_BASELINE selects a saved run to compare
bench-latency:
    cargo bench --locked --bench dictation_latency

# Headless time to first finalized text; stops capture after the first segment
bench-first-text:
    cargo bench --locked --bench first_text

# Recompare saved latency samples without inference; margin 0.1 allows a 10% slower median
bench-latency-compare baseline current margin="0":
    ARIA_BENCH_BASELINE="{{ absolute_path(baseline) }}" \
        ARIA_BENCH_COMPARE="{{ absolute_path(current) }}" \
        ARIA_BENCH_MAX_REGRESSION="{{ margin }}" cargo bench --locked --bench dictation_latency

models_dir := home_directory() + "/.aria/models"

# Six existing speech cases; pass --baseline for a required quality comparison
bench-smoke corpus=(justfile_directory() + "/build/corpus/smoke/manifest.tsv") *flags:
    cargo run --locked --release -p aria-bench -- stt \
        --corpus "{{ absolute_path(corpus) }}" --mode streaming \
        --model whisper={{ models_dir }}/whisper/ggml-large-v3-turbo-q8_0.bin \
        --model parakeet={{ models_dir }}/parakeet/parakeet-tdt-0.6b-v3 \
        --model nemotron={{ models_dir }}/nemotron/nemotron-speech-streaming-en-0.6b \
        {{ flags }}

# Documentation

# Build both books
book: book-user book-dev

# Build user guide
book-user:
    mdbook build book/user-guide

# Build dev guide
book-dev:
    mdbook build book/dev-guide

# Serve user guide with live reload
book-serve guide="user-guide":
    mdbook serve book/{{ guide }}
