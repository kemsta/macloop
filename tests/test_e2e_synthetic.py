from __future__ import annotations

import json
import queue
import struct
import subprocess
import sys
import textwrap
import threading
from pathlib import Path

import pytest

pytest.importorskip("numpy")
pytest.importorskip("macloop._macloop")

import numpy as np

import macloop


def _read_float_wav(path: Path) -> tuple[tuple[int, int, int], list[float]]:
    data = path.read_bytes()
    assert data[:4] == b"RIFF"
    assert data[8:12] == b"WAVE"

    fmt_info = None
    samples = None
    offset = 12
    while offset + 8 <= len(data):
        chunk_id = data[offset : offset + 4]
        size = struct.unpack_from("<I", data, offset + 4)[0]
        body_start = offset + 8
        body_end = body_start + size
        body = data[body_start:body_end]

        if chunk_id == b"fmt ":
            audio_format, channels, sample_rate, _, _, bits_per_sample = struct.unpack_from(
                "<HHIIHH", body, 0
            )
            fmt_info = (audio_format, channels, sample_rate, bits_per_sample)
        elif chunk_id == b"data":
            samples = [value for (value,) in struct.iter_unpack("<f", body)]

        offset = body_end + (size & 1)

    assert fmt_info is not None
    assert samples is not None
    return (fmt_info[0], fmt_info[1], fmt_info[2]), samples


def _collect_chunks(sink: macloop.AsrSink, count: int) -> list[macloop.AudioChunk]:
    collected: "queue.Queue[macloop.AudioChunk]" = queue.Queue()

    def _collect() -> None:
        for chunk in sink.chunks():
            collected.put(chunk)
            if collected.qsize() >= count:
                return

    collector = threading.Thread(target=_collect, daemon=True)
    collector.start()
    try:
        return [collected.get(timeout=2.0) for _ in range(count)]
    finally:
        collector.join(timeout=1.0)


def test_synthetic_source_reaches_asr_and_wav(tmp_path: Path) -> None:
    output_path = tmp_path / "synthetic.wav"

    with macloop.AudioEngine() as engine:
        stream = engine.create_stream(
            macloop.SyntheticSource,
            frames_per_callback=4,
            callback_count=3,
            start_value=1.0,
            step_value=1.0,
            start_delay_ms=100,
        )
        engine.add_processor(stream=stream, processor=macloop.GainProcessor(gain=2.0))

        asr_route = engine.route("synthetic_for_asr", stream=stream)
        wav_route = engine.route("synthetic_for_wav", stream=stream)

        asr_sink = macloop.AsrSink(
            routes=[asr_route],
            chunk_frames=4,
            sample_rate=48_000,
            channels=1,
            sample_format="f32",
        )
        wav_sink = macloop.WavSink(route=wav_route, file=output_path)

        chunks = _collect_chunks(asr_sink, 3)

        asr_sink.close()
        wav_sink.close()

    expected_chunks = [
        np.array([2.0, 4.0, 6.0, 8.0], dtype=np.float32),
        np.array([10.0, 12.0, 14.0, 16.0], dtype=np.float32),
        np.array([18.0, 20.0, 22.0, 24.0], dtype=np.float32),
    ]

    for chunk, expected in zip(chunks, expected_chunks):
        assert chunk.route_id == "synthetic_for_asr"
        assert chunk.frames == 4
        assert chunk.samples.dtype == np.float32
        assert np.array_equal(chunk.samples, expected)

    fmt_info, wav_samples = _read_float_wav(output_path)
    assert fmt_info[0] in {3, 65534}
    assert fmt_info[1:] == (2, 48_000)
    assert wav_samples == [
        2.0,
        2.0,
        4.0,
        4.0,
        6.0,
        6.0,
        8.0,
        8.0,
        10.0,
        10.0,
        12.0,
        12.0,
        14.0,
        14.0,
        16.0,
        16.0,
        18.0,
        18.0,
        20.0,
        20.0,
        22.0,
        22.0,
        24.0,
        24.0,
    ]


def test_synthetic_source_reaches_i16_asr_output() -> None:
    with macloop.AudioEngine() as engine:
        stream = engine.create_stream(
            macloop.SyntheticSource,
            frames_per_callback=4,
            callback_count=1,
            start_value=-1.0,
            step_value=0.5,
            start_delay_ms=100,
        )
        route = engine.route("synthetic_i16", stream=stream)

        asr_sink = macloop.AsrSink(
            routes=[route],
            chunk_frames=4,
            sample_rate=48_000,
            channels=1,
            sample_format="i16",
        )

        [chunk] = _collect_chunks(asr_sink, 1)
        asr_sink.close()

    assert chunk.route_id == "synthetic_i16"
    assert chunk.frames == 4
    assert chunk.samples.dtype == np.int16
    assert np.array_equal(
        chunk.samples,
        np.array([-32767, -16384, 0, 16384], dtype=np.int16),
    )


def test_synthetic_source_reaches_resampled_asr_output() -> None:
    with macloop.AudioEngine() as engine:
        stream = engine.create_stream(
            macloop.SyntheticSource,
            frames_per_callback=160,
            callback_count=40,
            start_value=0.5,
            step_value=0.0,
            interval_ms=2,
            start_delay_ms=250,
        )
        route = engine.route("synthetic_resampled", stream=stream)

        asr_sink = macloop.AsrSink(
            routes=[route],
            chunk_frames=320,
            sample_rate=16_000,
            channels=1,
            sample_format="f32",
        )

        chunks = _collect_chunks(asr_sink, 3)
        asr_sink.close()

    chunk = chunks[-1]
    assert chunk.route_id == "synthetic_resampled"
    assert chunk.frames == 320
    assert chunk.samples.dtype == np.float32
    assert chunk.samples.shape == (320,)
    assert np.allclose(chunk.samples, np.full(320, 0.5, dtype=np.float32), atol=1e-3)


def test_two_synthetic_sources_mix_into_aligned_wav(tmp_path: Path) -> None:
    output_path = tmp_path / "synthetic_mix.wav"

    with macloop.AudioEngine() as engine:
        stream_a = engine.create_stream(
            macloop.SyntheticSource,
            "synthetic_a",
            frames_per_callback=4,
            callback_count=6,
            start_value=1.0,
            step_value=0.0,
            interval_ms=0,
            start_delay_ms=100,
        )
        stream_b = engine.create_stream(
            macloop.SyntheticSource,
            "synthetic_b",
            frames_per_callback=4,
            callback_count=6,
            start_value=3.0,
            step_value=0.0,
            interval_ms=3,
            start_delay_ms=180,
        )

        route_a = engine.route("synthetic_mix_a", stream=stream_a)
        route_b = engine.route("synthetic_mix_b", stream=stream_b)

        wav_sink = macloop.WavSink(routes=[route_a, route_b], file=output_path)

        # Let both synthetic sources finish and give the writer thread time to flush.
        threading.Event().wait(0.5)
        wav_sink.close()

    fmt_info, wav_samples = _read_float_wav(output_path)
    assert fmt_info[0] in {3, 65534}
    assert fmt_info[1:] == (2, 48_000)

    # Each source is constant stereo and the default mix gain is 0.5, so the result
    # should stay aligned at (1.0 + 3.0) * 0.5 == 2.0 rather than stretching in time.
    assert len(wav_samples) == 48
    assert wav_samples == [2.0] * 48


def test_hot_synthetic_engine_close_completes_with_wav_and_asr_sinks(tmp_path: Path) -> None:
    child_code = textwrap.dedent(
        """
        import json
        import sys
        import time
        from pathlib import Path

        import macloop

        base = Path(sys.argv[1])
        wav_path = base / "hot.wav"
        engine = macloop.AudioEngine()
        stream = engine.create_stream(
            macloop.SyntheticSource,
            frames_per_callback=160,
            callback_count=1_000_000,
            start_value=0.1,
            step_value=0.0,
            interval_ms=0,
            start_delay_ms=0,
        )
        route_wav = engine.route("hot_wav", stream=stream)
        route_asr = engine.route("hot_asr", stream=stream)
        wav_sink = macloop.WavSink(route=route_wav, file=wav_path)
        asr_sink = macloop.AsrSink(
            routes=[route_asr],
            chunk_frames=320,
            sample_rate=16_000,
            channels=1,
            sample_format="f32",
        )

        time.sleep(0.05)
        started = time.monotonic()
        engine.close()
        elapsed = time.monotonic() - started
        print(
            json.dumps(
                {
                    "close_elapsed_s": round(elapsed, 6),
                    "wav_exists": wav_path.exists(),
                    "wav_size": wav_path.stat().st_size if wav_path.exists() else 0,
                    "asr_closed": asr_sink._closed,
                    "wav_closed": wav_sink._closed,
                }
            ),
            flush=True,
        )
        """
    )

    try:
        completed = subprocess.run(
            [sys.executable, "-c", child_code, str(tmp_path)],
            capture_output=True,
            text=True,
            timeout=6.0,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        pytest.fail(
            "engine.close() hung for hot SyntheticSource with WavSink + AsrSink; "
            f"stdout={((exc.stdout or '')[:400])!r} stderr={((exc.stderr or '')[:400])!r}"
        )

    assert completed.returncode == 0, completed.stderr
    payload = completed.stdout.strip()
    assert payload, completed.stderr
    stats = json.loads(payload)
    assert stats["close_elapsed_s"] < 6.0
    assert stats["wav_exists"] is True
    assert stats["wav_size"] > 44
    assert stats["asr_closed"] is True
    assert stats["wav_closed"] is True


def test_hot_synthetic_restart_on_single_engine_does_not_hang(tmp_path: Path) -> None:
    child_code = textwrap.dedent(
        """
        import json
        import sys
        import time
        from pathlib import Path

        import macloop

        base = Path(sys.argv[1])
        engine = macloop.AudioEngine()
        stream = engine.create_stream(
            macloop.SyntheticSource,
            frames_per_callback=160,
            callback_count=1_000_000,
            start_value=0.1,
            step_value=0.0,
            interval_ms=0,
            start_delay_ms=0,
        )
        route_wav = engine.route("restart_wav", stream=stream)
        route_asr = engine.route("restart_asr", stream=stream)

        wav_sizes = []
        first_close_elapsed = None
        final_close_elapsed = None
        final_asr_closed = None
        final_wav_closed = None

        for cycle in range(2):
            wav_path = base / f"restart_{cycle}.wav"
            wav_sink = macloop.WavSink(route=route_wav, file=wav_path)
            asr_sink = macloop.AsrSink(
                routes=[route_asr],
                chunk_frames=320,
                sample_rate=16_000,
                channels=1,
                sample_format="f32",
            )
            time.sleep(0.05)

            if cycle == 0:
                started = time.monotonic()
                wav_sink.close()
                asr_sink.close()
                first_close_elapsed = time.monotonic() - started
            else:
                started = time.monotonic()
                engine.close()
                final_close_elapsed = time.monotonic() - started
                final_asr_closed = asr_sink._closed
                final_wav_closed = wav_sink._closed

            wav_sizes.append(wav_path.stat().st_size if wav_path.exists() else 0)

        print(
            json.dumps(
                {
                    "first_close_elapsed_s": round(first_close_elapsed, 6),
                    "final_close_elapsed_s": round(final_close_elapsed, 6),
                    "wav_sizes": wav_sizes,
                    "final_asr_closed": final_asr_closed,
                    "final_wav_closed": final_wav_closed,
                }
            ),
            flush=True,
        )
        """
    )

    try:
        completed = subprocess.run(
            [sys.executable, "-c", child_code, str(tmp_path)],
            capture_output=True,
            text=True,
            timeout=10.0,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        pytest.fail(
            "single-engine restart hung for hot SyntheticSource with WavSink + AsrSink; "
            f"stdout={((exc.stdout or '')[:400])!r} stderr={((exc.stderr or '')[:400])!r}"
        )

    assert completed.returncode == 0, completed.stderr
    payload = completed.stdout.strip()
    assert payload, completed.stderr
    stats = json.loads(payload)
    assert stats["first_close_elapsed_s"] < 2.0
    assert stats["final_close_elapsed_s"] < 2.0
    assert stats["wav_sizes"] == [size for size in stats["wav_sizes"] if size > 44]
    assert len(stats["wav_sizes"]) == 2
    assert stats["final_asr_closed"] is True
    assert stats["final_wav_closed"] is True
