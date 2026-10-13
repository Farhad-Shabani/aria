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
    uvx ruff check {{ sidecar }} {{ sidecar_tests }}
    uvx ruff format --check {{ sidecar }} {{ sidecar_tests }}
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

# Rust: full and cross-target gates

rust_native_features := if os() == "macos" { "--all-features" } else { "" }

# Check the complete Rust workspace for the native host
rust-native: rust-format rust-deps rust-lint-all rust-test-all

# Check dependency usage and unlinked Rust source files
rust-deps:
    cargo shear --locked --deny-warnings

# Check Rust and TOML formatting
rust-format:
    taplo format --check --config taplo.toml
    cargo fmt --all --check

# Clippy already compiles the front-end; a separate check would compile it twice.
# Check compilation and lints across the complete Rust workspace
rust-lint-all:
    cargo clippy --locked --workspace --all-targets {{ rust_native_features }} -- -D warnings

# Run all Rust tests
rust-test-all:
    cargo test --locked --workspace --all-targets {{ rust_native_features }}

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
swift-check: swift-format swift-test

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
app: swift-project
    cargo build --locked --release --bin aria
    cd clients && xcodebuild -project Aria.xcodeproj -scheme Aria \
        -configuration Release -derivedDataPath build/mac -onlyUsePackageVersionsFromResolvedFile \
        CODE_SIGNING_ALLOWED=NO MARKETING_VERSION={{ version }} -quiet build
    scripts/bundle.sh clients/build/mac/Build/Products/Release/Aria.app

# Regenerate the Swift client stubs from the daemon's proto. Health comes
# along so clients can probe the daemon; the Rust side gets its own copy from
# tonic-health.
# Generate Swift client stubs from the daemon protocol
proto-swift:
    protoc --proto_path=proto \
        --swift_out=clients/Generated/Messages \
        --swift_opt=Visibility=Public,FileNaming=DropPath \
        --grpc-swift-2_out=clients/Generated/Rpc \
        --grpc-swift-2_opt=Visibility=Public,Client=true,Server=false,FileNaming=DropPath \
        proto/aria/daemon/v1/daemon.proto \
        proto/aria/daemon/v1/errors.proto \
        proto/aria/daemon/v1/relay.proto \
        proto/aria/daemon/v1/speech.proto \
        proto/aria/setup/v1/setup.proto \
        proto/grpc/health/v1/health.proto

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
    scripts/release-macos.sh {{ version }}

# Package the aria binary and its licenses into a tarball in dist/
release-linux: notices
    scripts/release-linux.sh {{ version }}

# Write dist/SHA256SUMS, the manifest install.sh reads
release-checksums:
    cp install.sh dist/install.sh
    cd dist && shasum -a 256 install.sh Aria-{{ version }}-macos-arm64.dmg aria-{{ version }}-linux-x86_64.tar.gz > SHA256SUMS

# Evaluation and benchmarks

# Speech corpus for `just accuracy`, into a gitignored directory
corpus utterances="100":
    crates/aria-bench/scripts/corpus.sh {{ justfile_directory() }}/build/corpus {{ utterances }}

# Long-form corpus (90-115 s recordings) for streaming/agreement runs
corpus-long recordings="12":
    crates/aria-bench/scripts/corpus-long.sh {{ justfile_directory() }}/build/corpus-long \
        {{ recordings }} {{ justfile_directory() }}/build/corpus/librispeech

# The environment's ARIA_TEST_*_MODEL variables select the backends.
# Measure word error rate against `just corpus`
accuracy *flags:
    cargo run --locked --release -p aria-bench -- stt \
        --corpus {{ justfile_directory() }}/build/corpus/manifest.tsv \
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

# Recompare saved latency samples without daemon startup or model inference
bench-latency-compare baseline current margin="0":
    ARIA_BENCH_BASELINE="{{ absolute_path(baseline) }}" \
        ARIA_BENCH_COMPARE="{{ absolute_path(current) }}" \
        ARIA_BENCH_MAX_REGRESSION_MS="{{ margin }}" cargo bench --locked --bench dictation_latency

# Six existing speech cases; pass --baseline for a required quality comparison
bench-smoke corpus=(justfile_directory() + "/build/corpus/smoke/manifest.tsv") *flags:
    cargo run --locked --release -p aria-bench -- stt \
        --corpus "{{ absolute_path(corpus) }}" --mode streaming \
        --model whisper={{ home_directory() }}/models/whisper/ggml-large-v3-turbo-q8_0.bin \
        --model parakeet={{ home_directory() }}/models/parakeet/parakeet-tdt-0.6b-v3 \
        --model nemotron={{ home_directory() }}/models/nemotron/nemotron-speech-streaming-en-0.6b \
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
