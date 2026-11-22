"""Tests for the parakeet-mlx sidecar's protocol and chunking logic.

Load by path under a distinct name to avoid shadowing the parakeet_mlx package.
"""

from __future__ import annotations

import base64
import binascii
import importlib.util
import io
import itertools
import json
import sys
from importlib.util import find_spec
from pathlib import Path
from types import SimpleNamespace

import pytest

SIDECAR_PATH = Path(__file__).parent.parent / "src/parakeet_mlx/parakeet_mlx.py"


def _load_sidecar():
    spec = importlib.util.spec_from_file_location("parakeet_mlx_sidecar", SIDECAR_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


sidecar = _load_sidecar()

HOP = 160


def _request(n_samples: int = 8000, **fields) -> str:
    audio = base64.b64encode(bytes(2 * n_samples)).decode()
    return json.dumps({"op": "recognize", "audio": audio, **fields})


class TestChunkBounds:
    @pytest.mark.parametrize(
        ("total", "chunk"),
        [
            (48000, 16000),  # exact multiple
            (128001, 16000),  # 1-sample tail: the MLX negative-malloc regression
            (128100, 16000),  # tail below one hop, folded
            (128160, 16000),  # tail of exactly one hop, kept separate
            (8000, 16000),  # shorter than one chunk
            (200, 16000),  # between one hop and one chunk
        ],
    )
    def test_bounds_are_contiguous_cover_everything_and_never_go_sub_hop(self, total, chunk):
        bounds = list(sidecar.chunk_bounds(total, chunk, HOP))

        assert bounds[0][0] == 0
        assert bounds[-1][1] == total
        assert all(end == next_start for (_, end), (next_start, _) in itertools.pairwise(bounds))
        assert all(end - start >= HOP for start, end in bounds)

    def test_a_sub_hop_tail_lands_in_the_final_chunk(self):
        bounds = list(sidecar.chunk_bounds(128001, 16000, HOP))

        assert bounds[-1] == (112000, 128001)

    def test_a_tail_of_one_hop_stays_its_own_chunk(self):
        bounds = list(sidecar.chunk_bounds(128160, 16000, HOP))

        assert bounds[-1] == (128000, 128160)


class TestParseRequest:
    def test_a_request_yields_its_samples_and_chunk(self):
        frames, chunk = sidecar.parse_request(_request(4, chunk_samples=12000))

        assert (frames, chunk) == (bytes(8), 12000)

    def test_the_chunk_is_optional(self):
        assert sidecar.parse_request(_request(4)) == (bytes(8), None)

    def test_an_unknown_op_is_refused(self):
        with pytest.raises(ValueError, match="unknown op"):
            sidecar.parse_request(json.dumps({"op": "reticulate"}))

    def test_a_request_without_audio_is_refused(self):
        with pytest.raises(KeyError):
            sidecar.parse_request(json.dumps({"op": "recognize"}))

    def test_audio_that_is_not_base64_is_refused(self):
        line = json.dumps({"op": "recognize", "audio": "not base64!"})

        with pytest.raises(binascii.Error):
            sidecar.parse_request(line)

    def test_a_half_sample_is_refused(self):
        line = json.dumps({"op": "recognize", "audio": base64.b64encode(b"\x00").decode()})

        with pytest.raises(ValueError, match="whole 16-bit samples"):
            sidecar.parse_request(line)

    @pytest.mark.parametrize("chunk_samples", [0, -12000])
    def test_a_non_positive_chunk_is_refused(self, chunk_samples):
        with pytest.raises(ValueError, match="chunk_samples"):
            sidecar.parse_request(_request(chunk_samples=chunk_samples))

    @pytest.mark.parametrize("line", ["[]", "5", '"recognize"'])
    def test_json_that_is_not_an_object_is_refused(self, line):
        with pytest.raises((ValueError, AttributeError, TypeError)):
            sidecar.parse_request(line)


class TestStagedAudio:
    def test_the_loader_hands_over_the_staged_samples_once(self):
        staged = sidecar.StagedAudio()

        with staged.staged("samples") as path:
            assert staged.load(path, 16000) == "samples"
            with pytest.raises(RuntimeError, match="no request audio"):
                staged.load(path, 16000)

    def test_leaving_the_stage_drops_unread_samples(self):
        staged = sidecar.StagedAudio()

        with staged.staged("samples") as path:
            pass

        with pytest.raises(RuntimeError, match="no request audio"):
            staged.load(path, 16000)


class TestWireFormat:
    """Pins the exact frames the Rust side's Reply enum decodes."""

    def test_readiness_matches_the_handshake_the_daemon_expects(self):
        frames = io.StringIO()

        sidecar.Channel(frames).ready()

        assert frames.getvalue() == '{"ok":true,"ready":true,"protocol":1}\n'

    def test_a_transcript_frame_carries_ok_and_text(self):
        frames = io.StringIO()

        sidecar.Channel(frames).transcript("hello there")

        assert frames.getvalue() == '{"ok":true,"text":"hello there"}\n'

    def test_a_failure_frame_carries_ok_false_and_the_error(self):
        frames = io.StringIO()

        sidecar.Channel(frames).failure("boom")

        assert frames.getvalue() == '{"ok":false,"error":"boom"}\n'

    def test_multi_line_error_text_stays_one_frame(self):
        frames = io.StringIO()

        sidecar.Channel(frames).failure("line one\nline two\n")

        wire = frames.getvalue()
        assert wire.endswith("\n")
        assert wire.count("\n") == 1
        assert json.loads(wire) == {"ok": False, "error": "line one\nline two\n"}


class TestServe:
    def test_one_bad_request_does_not_stop_the_next(self, monkeypatch):
        monkeypatch.setattr(sys, "stdin", io.StringIO(f'{{"op":"reticulate"}}\n{_request()}\n'))
        monkeypatch.setattr(sidecar, "samples_from", lambda frames: frames)
        frames = io.StringIO()
        model = SimpleNamespace(
            transcribe=lambda path, chunk_duration, overlap_duration: SimpleNamespace(text=" hi ")
        )

        sidecar.serve(sidecar.Channel(frames), model)

        first, second = frames.getvalue().splitlines()
        assert json.loads(first)["ok"] is False
        assert json.loads(second) == {"ok": True, "text": "hi"}


needs_mlx = pytest.mark.skipif(find_spec("mlx") is None, reason="requires mlx")


@needs_mlx
class TestSamplesFrom:
    def test_little_endian_pcm_scales_to_unit_floats(self):
        samples = sidecar.samples_from((32767).to_bytes(2, "little", signed=True))

        assert samples.shape == (1,)
        assert abs(float(samples[0]) - 32767 / 32768) < 1e-6


class TestTranscribeStreaming:
    """Guards that must fire before the model is ever asked to stream."""

    class RefusesToStream:
        preprocessor_config = SimpleNamespace(hop_length=HOP)

        def transcribe_stream(self, **kwargs):
            raise AssertionError("the guard should have fired first")

    def test_a_chunk_below_one_mel_hop_is_refused(self):
        with pytest.raises(ValueError, match="mel hop"):
            sidecar.transcribe_streaming(self.RefusesToStream(), [0.0] * 8000, HOP - 1)

    def test_a_recording_below_one_mel_hop_is_silence(self):
        samples = [0.0] * (HOP - 1)

        assert sidecar.transcribe_streaming(self.RefusesToStream(), samples, 12000) == ""
