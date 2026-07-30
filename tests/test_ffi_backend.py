from __future__ import annotations

import os
import queue
import struct
import threading
from pathlib import Path

import pytest

pytest.importorskip("numpy")
ffi = pytest.importorskip("macloop._macloop")

import numpy as np


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


def _wait_for_chunks(
    collected: "queue.Queue[tuple[str, int, np.ndarray]]",
    count: int,
) -> list[tuple[str, int, np.ndarray]]:
    return [collected.get(timeout=2.0) for _ in range(count)]


def _open_file_descriptors() -> set[int]:
    descriptors = set()
    for name in os.listdir("/dev/fd"):
        try:
            fd = int(name)
            os.fstat(fd)
        except (OSError, ValueError):
            continue
        descriptors.add(fd)
    return descriptors


def test_low_level_ffi_asr_sink_round_trip() -> None:
    engine = ffi._AudioEngineBackend()
    engine.create_stream(
        "synthetic_stream",
        "synthetic",
        {
            "frames_per_callback": 4,
            "callback_count": 2,
            "start_value": 1.0,
            "step_value": 1.0,
            "start_delay_ms": 100,
        },
    )
    engine.add_processor("synthetic_stream", "gain_processor", "gain", {"gain": 2.0})
    engine.route("synthetic_route", "synthetic_stream")

    collected: "queue.Queue[tuple[str, int, np.ndarray]]" = queue.Queue()

    def callback(route_id: str, frames: int, samples) -> None:
        collected.put((route_id, frames, np.asarray(samples).copy()))

    sink = ffi._create_asr_sink(
        engine,
        "ffi_asr_sink",
        ["synthetic_route"],
        48_000,
        1,
        "f32",
        4,
        callback,
    )
    try:
        chunks = _wait_for_chunks(collected, 2)
        stats = sink.stats()
    finally:
        sink.close()
        engine.close()

    assert chunks[0][0] == "synthetic_route"
    assert chunks[0][1] == 4
    assert np.array_equal(chunks[0][2], np.array([2.0, 4.0, 6.0, 8.0], dtype=np.float32))
    assert np.array_equal(chunks[1][2], np.array([10.0, 12.0, 14.0, 16.0], dtype=np.float32))

    stream_stats = engine.get_stats()["synthetic_stream"]
    assert stream_stats.pipeline.buffer_size == 8
    assert "gain_processor" in stream_stats.processors
    assert stream_stats.processors["gain_processor"].processing_time_us >= 0

    sink_stats = stats["synthetic_route"]
    assert sink_stats.chunks_emitted == 2
    assert sink_stats.frames_emitted == 8
    assert sink_stats.callback.count == 2


def test_low_level_ffi_wav_sink_old_five_arg_api_writes_default_format(
    tmp_path: Path,
) -> None:
    output_path = tmp_path / "ffi_synthetic.wav"

    engine = ffi._AudioEngineBackend()
    engine.create_stream(
        "synthetic_stream",
        "synthetic",
        {
            "frames_per_callback": 4,
            "callback_count": 3,
            "start_value": 1.0,
            "step_value": 0.0,
            "start_delay_ms": 100,
        },
    )
    engine.route("wav_route", "synthetic_stream")

    with output_path.open("w+b") as fileobj:
        sink = ffi._create_wav_sink(
            engine,
            "ffi_wav_sink",
            ["wav_route"],
            fileobj.fileno(),
            1.0,
        )
        try:
            threading.Event().wait(0.5)
            stats = sink.stats()
        finally:
            sink.close()
            engine.close()

    fmt_info, samples = _read_float_wav(output_path)
    assert fmt_info[0] in {3, 65534}
    assert fmt_info[1:] == (2, 48_000)
    assert samples == [1.0] * 24
    assert stats.write_calls >= 1
    assert stats.samples_written == 24
    assert stats.frames_written == 12


def test_low_level_ffi_wav_sink_converts_to_mono_pcm16(tmp_path: Path) -> None:
    output_path = tmp_path / "ffi_synthetic_pcm16.wav"

    engine = ffi._AudioEngineBackend()
    engine.create_stream(
        "synthetic_stream",
        "synthetic",
        {
            "frames_per_callback": 160,
            "callback_count": 60,
            "start_value": 0.25,
            "step_value": 0.0,
            "interval_ms": 3,
            "start_delay_ms": 100,
        },
    )
    engine.route("wav_route", "synthetic_stream")

    with output_path.open("w+b") as fileobj:
        sink = ffi._create_wav_sink_with_config(
            engine,
            "ffi_wav_sink_pcm16",
            ["wav_route"],
            fileobj.fileno(),
            16_000,
            1,
            "i16",
            1.0,
        )
        try:
            threading.Event().wait(0.5)
        finally:
            sink.close()
            stats = sink.stats()
            engine.close()

    data = output_path.read_bytes()
    fmt_offset = data.index(b"fmt ") + 8
    audio_format, channels, sample_rate, _, _, bits_per_sample = struct.unpack_from(
        "<HHIIHH", data, fmt_offset
    )
    data_offset = data.index(b"data")
    data_size = struct.unpack_from("<I", data, data_offset + 4)[0]
    samples = [
        value
        for (value,) in struct.iter_unpack(
            "<h", data[data_offset + 8 : data_offset + 8 + data_size]
        )
    ]

    assert audio_format == 1
    assert (channels, sample_rate, bits_per_sample) == (1, 16_000, 16)
    assert len(samples) == 3_200
    assert all(abs(sample - 8192) <= 2 for sample in samples[500:-500])
    assert stats.samples_written == len(samples)
    assert stats.frames_written == len(samples)


def test_wav_close_error_restores_route_and_closes_duplicated_fd(tmp_path: Path) -> None:
    read_only_path = tmp_path / "read_only.wav"
    read_only_path.touch()
    recovered_path = tmp_path / "recovered.wav"
    engine = ffi._AudioEngineBackend()
    engine.create_stream(
        "synthetic_stream",
        "synthetic",
        {
            "frames_per_callback": 4,
            "callback_count": 1,
            "start_value": 0.25,
            "start_delay_ms": 100,
        },
    )
    engine.route("wav_route", "synthetic_stream")

    try:
        with read_only_path.open("rb") as fileobj:
            descriptors_before = _open_file_descriptors()
            sink = ffi._create_wav_sink(
                engine,
                "failing_wav_sink",
                ["wav_route"],
                fileobj.fileno(),
                1.0,
            )
            with pytest.raises(
                RuntimeError,
                match="failed to stop wav sink: wav writer error",
            ):
                sink.close(engine)

            stats = sink.stats()
            assert stats.finalize.count == 1
            assert _open_file_descriptors() == descriptors_before

        with recovered_path.open("w+b") as fileobj:
            replacement = ffi._create_wav_sink(
                engine,
                "replacement_wav_sink",
                ["wav_route"],
                fileobj.fileno(),
                1.0,
            )
            replacement.close(engine)
    finally:
        engine.close()


def test_wav_close_combines_write_and_restoration_errors(tmp_path: Path) -> None:
    read_only_path = tmp_path / "read_only_combined.wav"
    read_only_path.touch()
    engine = ffi._AudioEngineBackend()
    wrong_engine = ffi._AudioEngineBackend()
    engine.create_stream("synthetic_stream", "synthetic", {"start_delay_ms": 100})
    engine.route("wav_route", "synthetic_stream")
    wrong_engine.create_stream("other_stream", "synthetic", {})
    wrong_engine.route("wav_route", "other_stream")

    try:
        with read_only_path.open("rb") as fileobj:
            sink = ffi._create_wav_sink(
                engine,
                "failing_wav_sink",
                ["wav_route"],
                fileobj.fileno(),
                1.0,
            )
            with pytest.raises(RuntimeError) as exc_info:
                sink.close(wrong_engine)

        message = str(exc_info.value)
        assert "failed to stop wav sink: wav writer error" in message
        assert "additionally failed to restore wav sink routes" in message
        assert sink.stats().finalize.count == 1
    finally:
        engine.close()
        wrong_engine.close()


def test_low_level_ffi_validates_invalid_configs(tmp_path: Path) -> None:
    engine = ffi._AudioEngineBackend()

    with pytest.raises(ValueError, match="unsupported source_kind"):
        engine.create_stream("bad_stream", "nope", {})

    with pytest.raises(ValueError, match="requires at least one pid"):
        engine.create_stream("bad_app", "application_audio", {})

    engine.create_stream("synthetic_stream", "synthetic", {})

    with pytest.raises(ValueError, match="unsupported processor_kind"):
        engine.add_processor("synthetic_stream", "bad_processor", "nope", {})

    engine.route("synthetic_route", "synthetic_stream")

    with pytest.raises(ValueError, match="unsupported sample_format"):
        ffi._create_asr_sink(
            engine,
            "bad_sink",
            ["synthetic_route"],
            16_000,
            1,
            "u8",
            320,
            lambda *_args: None,
        )

    output_path = tmp_path / "invalid.wav"
    with output_path.open("w+b") as fileobj:
        with pytest.raises(ValueError, match="unsupported sample_format"):
            ffi._create_wav_sink_with_config(
                engine,
                "bad_wav_format",
                ["synthetic_route"],
                fileobj.fileno(),
                16_000,
                1,
                "u8",
                1.0,
            )

        with pytest.raises(ValueError, match="unsupported output channels"):
            ffi._create_wav_sink_with_config(
                engine,
                "bad_wav_channels",
                ["synthetic_route"],
                fileobj.fileno(),
                16_000,
                3,
                "i16",
                1.0,
            )

        sink = ffi._create_wav_sink_with_config(
            engine,
            "valid_wav_after_errors",
            ["synthetic_route"],
            fileobj.fileno(),
            48_000,
            2,
            "f32",
            1.0,
        )
        sink.close(engine)

    engine.close()
