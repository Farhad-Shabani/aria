"""Serves parakeet-mlx transcriptions to the Aria daemon, one request per line.

Requests and replies are newline-delimited JSON. The daemon owns this process
and kills it to cancel, so there is no shutdown handshake.
"""

import base64
import json
import os
import sys
from contextlib import contextmanager

PROTOCOL = 1
SAMPLE_RATE = 16000
CHUNK_DURATION = 120.0
OVERLAP_DURATION = 15.0
STREAM_CONTEXT = (256, 256)
STREAM_DEPTH = 1
READER_PROBE_SAMPLES = [0, 1, -1, 0]
WARMUP_SAMPLES = SAMPLE_RATE
# Names the staged samples to upstream `transcribe`, which wants a path.
STAGED_PATH = "staged-request-audio"


class Channel:
    """The framed link to the daemon.

    Replies leave on the real stdout. Everything else in the process is pushed
    to stderr, so a library's warning cannot be mistaken for a frame.
    """

    def __init__(self, frames):
        self._frames = frames

    @classmethod
    def claim_stdout(cls):
        frames = os.fdopen(os.dup(1), "w", buffering=1)
        os.dup2(2, 1)
        sys.stdout = sys.stderr
        return cls(frames)

    def ready(self):
        self._send(ok=True, ready=True, protocol=PROTOCOL)

    def transcript(self, text):
        self._send(ok=True, text=text)

    def failure(self, detail):
        self._send(ok=False, error=detail)

    def requests(self):
        for line in sys.stdin:
            line = line.strip()
            if line:
                yield line

    def _send(self, **fields):
        self._frames.write(json.dumps(fields, separators=(",", ":")) + "\n")


class StagedAudio:
    """The samples of the request being served, for parakeet-mlx's loader.

    Upstream `transcribe` takes only a path and reads it through `load_audio`.
    Recordings never touch disk, so the replacement loader ignores the path
    and hands over the staged samples instead. It skips ffmpeg too.
    """

    def __init__(self):
        self._samples = None

    def load(self, path, sampling_rate, dtype=None):
        # The loader this replaces accepts a dtype and ignores it, always
        # returning float32. Honouring it gives the encoder the wrong precision.
        del path, sampling_rate, dtype
        samples, self._samples = self._samples, None
        if samples is None:
            raise RuntimeError("no request audio is staged")
        return samples

    @contextmanager
    def staged(self, samples):
        self._samples = samples
        try:
            yield STAGED_PATH
        finally:
            self._samples = None


STAGED = StagedAudio()


def samples_from(frames):
    """Little-endian 16-bit PCM as float32 samples in -1.0..1.0."""
    import mlx.core as mx
    import numpy

    return mx.array(numpy.frombuffer(frames, "<i2")).astype(mx.float32) / 32768.0


def install_audio_loader():
    from parakeet_mlx import parakeet

    if not hasattr(parakeet, "load_audio"):
        raise RuntimeError("parakeet_mlx.parakeet.load_audio is gone; this patch is stale")
    parakeet.load_audio = STAGED.load
    _confirm_loader_is_reachable(parakeet)


def _confirm_loader_is_reachable(parakeet):
    """Checks the replacement loader's output shape and dtype with probe samples."""
    import mlx.core as mx

    probe = samples_from(_pcm(READER_PROBE_SAMPLES))
    with STAGED.staged(probe) as path:
        samples = parakeet.load_audio(path, SAMPLE_RATE, mx.bfloat16)

    if samples.shape != (len(READER_PROBE_SAMPLES),) or samples.dtype != mx.float32:
        raise RuntimeError(
            f"the audio loader returned {samples.shape} {samples.dtype}; "
            f"expected ({len(READER_PROBE_SAMPLES)},) float32"
        )


def _pcm(samples):
    return b"".join(sample.to_bytes(2, "little", signed=True) for sample in samples)


def load_model(model_id):
    from parakeet_mlx import from_pretrained

    install_audio_loader()
    model = from_pretrained(model_id)
    warm(model)
    return model


def warm(model):
    """Warms the recognition path before reporting readiness."""
    transcribe(model, bytes(2 * WARMUP_SAMPLES))


def parse_request(request_line):
    request = json.loads(request_line)
    operation = request.get("op")
    if operation != "recognize":
        raise ValueError(f"unknown op {operation!r}")

    frames = base64.b64decode(request["audio"], validate=True)
    if len(frames) % 2:
        raise ValueError("audio must hold whole 16-bit samples")

    chunk_samples = request.get("chunk_samples")
    if chunk_samples is not None and chunk_samples <= 0:
        raise ValueError(f"chunk_samples must be positive, got {chunk_samples}")
    return frames, chunk_samples


def transcribe(model, frames, chunk_samples=None):
    samples = samples_from(frames)
    if chunk_samples is not None:
        return transcribe_streaming(model, samples, chunk_samples)
    with STAGED.staged(samples) as path:
        result = model.transcribe(
            path,
            chunk_duration=CHUNK_DURATION,
            overlap_duration=OVERLAP_DURATION,
        )
    return str(result.text).strip()


def transcribe_streaming(model, samples, chunk_samples):
    """Feeds a recording through the streaming decoder in capture-sized chunks."""
    hop_length = model.preprocessor_config.hop_length
    if chunk_samples < hop_length:
        raise ValueError(f"chunk_samples must cover one mel hop ({hop_length} samples)")
    if len(samples) < hop_length:
        return ""

    with model.transcribe_stream(context_size=STREAM_CONTEXT, depth=STREAM_DEPTH) as stream:
        for start, end in chunk_bounds(len(samples), chunk_samples, hop_length):
            stream.add_audio(samples[start:end])
        return str(stream.result.text).strip()


def chunk_bounds(total, chunk_samples, hop_length):
    """Chunk boundaries, with a tail shorter than one mel hop folded into the
    final chunk: a sub-hop chunk yields an empty spectrogram, which crashes
    MLX's encoder with a negative-size allocation."""
    cuts = list(range(chunk_samples, total, chunk_samples))
    if cuts and total - cuts[-1] < hop_length:
        cuts.pop()
    return zip([0, *cuts], [*cuts, total], strict=True)


def serve(channel, model):
    for request_line in channel.requests():
        try:
            channel.transcript(transcribe(model, *parse_request(request_line)))
        except Exception as error:
            channel.failure(describe(error))


def describe(error):
    return f"{type(error).__name__}: {error}"


def model_id_of(argv):
    if len(argv) != 2:
        raise ValueError("expected exactly one argument: the model id")
    return argv[1]


def main(argv):
    channel = Channel.claim_stdout()
    try:
        model = load_model(model_id_of(argv))
    except Exception as error:
        channel.failure(describe(error))
        return 1

    channel.ready()
    serve(channel, model)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
