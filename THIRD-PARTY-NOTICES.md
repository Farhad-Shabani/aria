# Third-party notices

This file covers third-party material copied into this repository. Licenses of
Cargo dependencies are not listed here.

## livekit-wakeword

`crates/aria-wake/tests/fixtures/negative.wav` is a verbatim copy of
`tests/fixtures/negative.wav` from livekit-wakeword by LiveKit, Inc., under the
Apache License 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE)).
`crates/aria-wake/THIRD-PARTY-NOTICES.md` carries this notice in the published
crate.

Source: <https://github.com/livekit/livekit-wakeword>

## gRPC health protocol

`proto/grpc/health/v1/health.proto` comes from
[grpc/grpc-proto](https://github.com/grpc/grpc-proto) by The gRPC Authors,
under the Apache License 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE)). The file
keeps its copyright notice.

## OpenAI Whisper

- `crates/aria-bench/src/stt/normalize/` ports Whisper's English text
  normalizer. `british-american-spelling.json` there is a verbatim copy of
  `whisper/normalizers/english.json`.
- `crates/aria-stt/src/whisper_candle/melfilters.bytes` is Whisper's 80-band
  mel filter bank. It is a verbatim copy of the file in
  [Candle's Whisper example](https://github.com/huggingface/candle/tree/main/candle-examples/examples/whisper),
  which Candle distributes under `MIT OR Apache-2.0`.
  `crates/aria-stt/THIRD-PARTY-NOTICES.md` carries this notice in the
  published crate.

Source: <https://github.com/openai/whisper>

```text
MIT License

Copyright (c) 2022 OpenAI

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
