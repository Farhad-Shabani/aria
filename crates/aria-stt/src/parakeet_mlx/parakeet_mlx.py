"""Serves parakeet-mlx transcriptions to the Aria daemon, one request per line.

Requests and replies are newline-delimited JSON. The daemon owns this process
and kills it to cancel, so there is no shutdown handshake.
"""

import json
import os
import sys
import tempfile
import wave
from contextlib import contextmanager

PROTOCOL = 1
SAMPLE_RATE = 16000
CHUNK_DURATION = 120.0
OVERLAP_DURATION = 15.0
STREAM_CONTEXT = (256, 256)
STREAM_DEPTH = 1
READER_PROBE_SAMPLES = [0, 1, -1, 0]
WARMUP_SAMPLES = SAMPLE_RATE


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


def read_wav(path, sampling_rate, dtype=None):
    """Decodes one canonical recording: 16 kHz, mono, 16-bit.

    Replaces parakeet-mlx's own reader, which shells out to ffmpeg. The reader
    it stands in for accepts a dtype and ignores it, always returning float32;
    honouring it instead gives the encoder the wrong precision.
    """
    import mlx.core as mx
    import numpy

    del dtype

    with wave.open(str(path), "rb") as source:
        if source.getnchannels() != 1 or source.getsampwidth() != 2:
            raise ValueError("recording must be 16-bit mono")
        if source.getframerate() != sampling_rate:
            raise ValueError(
                f"recording is {source.getframerate()} Hz; expected {sampling_rate} Hz"
            )
        frames = source.readframes(source.getnframes())

    samples = numpy.frombuffer(frames, numpy.int16)
    return mx.array(samples).astype(mx.float32) / 32768.0


def install_wav_reader():
    from parakeet_mlx import parakeet

    if not hasattr(parakeet, "load_audio"):
        raise RuntimeError("parakeet_mlx.parakeet.load_audio is gone; this patch is stale")
    parakeet.load_audio = read_wav
    _confirm_reader_is_reachable(parakeet)


def _confirm_reader_is_reachable(parakeet):
    """Checks the replacement reader's output shape and dtype with a probe WAV."""
    import mlx.core as mx

    with _probe_recording(READER_PROBE_SAMPLES) as probe:
        samples = parakeet.load_audio(probe, SAMPLE_RATE, mx.bfloat16)

    if samples.shape != (len(READER_PROBE_SAMPLES),) or samples.dtype != mx.float32:
        raise RuntimeError(
            f"the audio reader returned {samples.shape} {samples.dtype}; "
            f"expected ({len(READER_PROBE_SAMPLES)},) float32"
        )


@contextmanager
def _probe_recording(samples):
    import numpy

    with tempfile.NamedTemporaryFile(suffix=".wav") as probe:
        with wave.open(probe.name, "wb") as writer:
            writer.setnchannels(1)
            writer.setsampwidth(2)
            writer.setframerate(SAMPLE_RATE)
            writer.writeframes(numpy.array(samples, numpy.int16).tobytes())
        yield probe.name


def load_model(model_id):
    from parakeet_mlx import from_pretrained

    install_wav_reader()
    model = from_pretrained(model_id)
    warm(model)
    return model


def warm(model):
    """Warms the transcription path before reporting readiness."""
    with _probe_recording([0] * WARMUP_SAMPLES) as probe:
        transcribe(model, probe)


def parse_request(request_line):
    request = json.loads(request_line)
    operation = request.get("op")
    if operation != "transcribe":
        raise ValueError(f"unknown op {operation!r}")

    audio_path = request["audio_path"]
    if not os.path.isfile(audio_path):
        raise FileNotFoundError(audio_path)

    chunk_samples = request.get("chunk_samples")
    if chunk_samples is not None and chunk_samples <= 0:
        raise ValueError(f"chunk_samples must be positive, got {chunk_samples}")
    return audio_path, chunk_samples


def transcribe(model, audio_path, chunk_samples=None):
    if chunk_samples is not None:
        return transcribe_streaming(model, audio_path, chunk_samples)
    result = model.transcribe(
        audio_path,
        chunk_duration=CHUNK_DURATION,
        overlap_duration=OVERLAP_DURATION,
    )
    return str(result.text).strip()


def transcribe_streaming(model, audio_path, chunk_samples):
    """Feeds a recording through the streaming decoder in capture-sized chunks."""
    samples = read_wav(audio_path, SAMPLE_RATE)
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
