from __future__ import annotations

import importlib
import os
import sys
import types

import numpy as np
import pytest


def pytest_addoption(parser):
    parser.addoption(
        "--run-medium",
        action="store_true",
        default=False,
        help="run medium e2e tests that open real audio capture devices/apps/system capture",
    )


def pytest_configure(config):
    config.addinivalue_line(
        "markers",
        "medium: medium e2e tests that require explicit opt-in and real macOS capture devices",
    )


def pytest_collection_modifyitems(config, items):
    if config.getoption("--run-medium"):
        return

    skip_medium = pytest.mark.skip(
        reason="medium tests are disabled by default; pass --run-medium to enable",
    )
    for item in items:
        if "medium" in item.keywords:
            item.add_marker(skip_medium)


@pytest.fixture
def require_medium_run() -> None:
    if os.environ.get("CI"):
        pytest.skip("medium tests must not run in CI")
    if not os.environ.get("PYTEST_RUN_MEDIUM"):
        pytest.skip("set PYTEST_RUN_MEDIUM=1 for medium tests to confirm real-device execution")


class _FakePipelineStats:
    def __init__(
        self,
        total_callback_time_us: int,
        dropped_frames: int,
        buffer_size: int,
        latency,
    ) -> None:
        self.total_callback_time_us = total_callback_time_us
        self.dropped_frames = dropped_frames
        self.buffer_size = buffer_size
        self.latency = latency


class _FakeLatencyStats:
    def __init__(
        self,
        *,
        last_us: int,
        max_us: int,
        count: int,
        bucket_bounds_us: list[int],
        buckets: list[int],
        p50_us: int,
        p90_us: int,
        p95_us: int,
        p99_us: int,
    ) -> None:
        self.last_us = last_us
        self.max_us = max_us
        self.count = count
        self.bucket_bounds_us = bucket_bounds_us
        self.buckets = buckets
        self.p50_us = p50_us
        self.p90_us = p90_us
        self.p95_us = p95_us
        self.p99_us = p99_us


class _FakeProcessorStats:
    def __init__(self, processing_time_us: int, max_processing_time_us: int, latency) -> None:
        self.processing_time_us = processing_time_us
        self.max_processing_time_us = max_processing_time_us
        self.latency = latency


class _FakeStreamStats:
    def __init__(self, pipeline, processors) -> None:
        self.pipeline = pipeline
        self.processors = processors


class _FakeAsrInputStats:
    def __init__(self, chunks_emitted, frames_emitted, pending_frames, poll, callback) -> None:
        self.chunks_emitted = chunks_emitted
        self.frames_emitted = frames_emitted
        self.pending_frames = pending_frames
        self.poll = poll
        self.callback = callback


class _FakeWavSinkStats:
    def __init__(self, write_calls, samples_written, frames_written, write, finalize) -> None:
        self.write_calls = write_calls
        self.samples_written = samples_written
        self.frames_written = frames_written
        self.write = write
        self.finalize = finalize


class _FakeAsrSinkBackend:
    def __init__(self) -> None:
        self.closed = False
        self._stats = {}

    def stats(self):
        return self._stats

    def close(self) -> None:
        self.closed = True


class _FakeWavSinkBackend:
    def __init__(self) -> None:
        self.closed = False
        write_latency = _FakeLatencyStats(
            last_us=18,
            max_us=30,
            count=3,
            bucket_bounds_us=[1, 2, 4, 8, 16, 32],
            buckets=[0, 0, 0, 0, 1, 2],
            p50_us=32,
            p90_us=32,
            p95_us=32,
            p99_us=32,
        )
        finalize_latency = _FakeLatencyStats(
            last_us=50,
            max_us=50,
            count=1,
            bucket_bounds_us=[1, 2, 4, 8, 16, 32, 64],
            buckets=[0, 0, 0, 0, 0, 0, 1],
            p50_us=64,
            p90_us=64,
            p95_us=64,
            p99_us=64,
        )
        self._stats = _FakeWavSinkStats(
            write_calls=3,
            samples_written=12,
            frames_written=6,
            write=write_latency,
            finalize=finalize_latency,
        )

    def stats(self):
        return self._stats

    def close(self) -> None:
        self.closed = True


class _FakeAudioEngineBackend:
    def __init__(self) -> None:
        self.calls: list[tuple[object, ...]] = []
        self.closed = False
        pipeline_latency = _FakeLatencyStats(
            last_us=10,
            max_us=20,
            count=4,
            bucket_bounds_us=[1, 2, 4, 8, 16, 32],
            buckets=[0, 0, 0, 1, 3, 0],
            p50_us=16,
            p90_us=16,
            p95_us=16,
            p99_us=16,
        )
        processor_latency = _FakeLatencyStats(
            last_us=6,
            max_us=12,
            count=4,
            bucket_bounds_us=[1, 2, 4, 8, 16, 32],
            buckets=[0, 0, 1, 3, 0, 0],
            p50_us=8,
            p90_us=8,
            p95_us=8,
            p99_us=8,
        )
        self.stats = {
            "stream_1": _FakeStreamStats(
                _FakePipelineStats(10, 1, 256, pipeline_latency),
                {"gain": _FakeProcessorStats(6, 12, processor_latency)},
            )
        }

    def create_stream(self, stream_id, source_kind, config) -> None:
        self.calls.append(("create_stream", stream_id, source_kind, dict(config)))

    def add_processor(self, stream_id, processor_id, processor_kind, config) -> None:
        self.calls.append(
            ("add_processor", stream_id, processor_id, processor_kind, dict(config))
        )

    def route(self, route_id, stream_id) -> None:
        self.calls.append(("route", route_id, stream_id))

    def get_stats(self):
        return self.stats

    def close(self) -> None:
        self.closed = True


def _fake_create_asr_sink(
    engine,
    sink_id,
    route_ids,
    sample_rate,
    channels,
    sample_format,
    chunk_frames,
    callback,
):
    engine.calls.append(
        (
            "create_asr_sink",
            sink_id,
            tuple(route_ids),
            sample_rate,
            channels,
            sample_format,
            chunk_frames,
        )
    )
    poll_latency = _FakeLatencyStats(
        last_us=12,
        max_us=24,
        count=2,
        bucket_bounds_us=[1, 2, 4, 8, 16, 32],
        buckets=[0, 0, 0, 0, 2, 0],
        p50_us=16,
        p90_us=16,
        p95_us=16,
        p99_us=16,
    )
    callback_latency = _FakeLatencyStats(
        last_us=30,
        max_us=40,
        count=2,
        bucket_bounds_us=[1, 2, 4, 8, 16, 32, 64],
        buckets=[0, 0, 0, 0, 0, 1, 1],
        p50_us=32,
        p90_us=64,
        p95_us=64,
        p99_us=64,
    )
    backend = _FakeAsrSinkBackend()
    backend._stats = {
        route_ids[0]: _FakeAsrInputStats(
            chunks_emitted=2,
            frames_emitted=4,
            pending_frames=1,
            poll=poll_latency,
            callback=callback_latency,
        )
    }
    callback(route_ids[0], chunk_frames, np.array([0.1, 0.2], dtype=np.float32))
    callback(route_ids[0], chunk_frames, np.array([0.3, 0.4], dtype=np.float32))
    return backend


def _fake_create_wav_sink(engine, sink_id, route_ids, fd, mix_gain):
    engine.calls.append(("create_wav_sink", sink_id, tuple(route_ids), fd, mix_gain))
    return _FakeWavSinkBackend()


@pytest.fixture
def macloop_module(monkeypatch):
    fake_ext = types.ModuleType("macloop._macloop")
    fake_ext._AudioEngineBackend = _FakeAudioEngineBackend
    fake_ext._AsrSinkBackend = _FakeAsrSinkBackend
    fake_ext._WavSinkBackend = _FakeWavSinkBackend
    fake_ext.LatencyStats = _FakeLatencyStats
    fake_ext.PipelineStats = _FakePipelineStats
    fake_ext.ProcessorStats = _FakeProcessorStats
    fake_ext.StreamStats = _FakeStreamStats
    fake_ext.AsrInputStats = _FakeAsrInputStats
    fake_ext.WavSinkStats = _FakeWavSinkStats
    fake_ext._create_asr_sink = _fake_create_asr_sink
    fake_ext._create_wav_sink = _fake_create_wav_sink
    fake_ext.list_microphones = lambda: [
        {"id": 11, "name": "Built-in Mic", "is_default": True},
        {"id": 22, "name": "USB Mic", "is_default": False},
    ]
    fake_ext.list_applications = lambda: [
        {"pid": 111, "name": "Safari", "bundle_id": "com.apple.Safari", "is_default": True},
        {"pid": 222, "name": "Music", "bundle_id": "com.apple.Music", "is_default": False},
    ]
    fake_ext.list_displays = lambda: [
        {"id": 101, "name": "Display 101", "width": 2560, "height": 1440, "is_default": True},
        {"id": 202, "name": "Display 202", "width": 1920, "height": 1080, "is_default": False},
    ]

    monkeypatch.setitem(sys.modules, "macloop._macloop", fake_ext)
    sys.modules.pop("macloop", None)

    module = importlib.import_module("macloop")
    return importlib.reload(module)
