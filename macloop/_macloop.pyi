from __future__ import annotations

from typing import Any, Callable, Optional, Union

import numpy as np
import numpy.typing as npt

AudioSamples = Union[npt.NDArray[np.int16], npt.NDArray[np.float32]]


class LatencyStats:
    last_us: int
    max_us: int
    count: int
    bucket_bounds_us: list[int]
    buckets: list[int]
    p50_us: int
    p90_us: int
    p95_us: int
    p99_us: int


class PipelineStats:
    total_callback_time_us: int
    dropped_frames: int
    buffer_size: int
    latency: LatencyStats


class ProcessorStats:
    processing_time_us: int
    max_processing_time_us: int
    latency: LatencyStats


class StreamStats:
    pipeline: PipelineStats
    processors: dict[str, ProcessorStats]


class _AsrSinkBackend:
    def stats(self) -> dict[str, AsrInputStats]: ...
    def close(self) -> None: ...


class _WavSinkBackend:
    def stats(self) -> WavSinkStats: ...
    def close(self) -> None: ...


class AsrInputStats:
    chunks_emitted: int
    frames_emitted: int
    pending_frames: int
    poll: LatencyStats
    callback: LatencyStats


class WavSinkStats:
    write_calls: int
    samples_written: int
    frames_written: int
    write: LatencyStats
    finalize: LatencyStats


class _AudioEngineBackend:
    def __init__(self) -> None: ...
    def create_stream(
        self,
        stream_id: str,
        source_kind: str,
        config: dict[str, Any],
    ) -> None: ...
    def add_processor(
        self,
        stream_id: str,
        processor_id: str,
        processor_kind: str,
        config: dict[str, Any],
    ) -> None: ...
    def route(self, route_id: str, stream_id: str) -> None: ...
    def get_stats(self) -> dict[str, StreamStats]: ...
    def close(self) -> None: ...


def list_microphones() -> list[dict[str, Any]]: ...
def list_displays() -> list[dict[str, Any]]: ...
def list_applications() -> list[dict[str, Any]]: ...
def _create_asr_sink(
    engine: _AudioEngineBackend,
    sink_id: str,
    route_ids: list[str],
    sample_rate: int,
    channels: int,
    sample_format: str,
    chunk_frames: int,
    callback: Callable[[str, int, AudioSamples], None],
) -> _AsrSinkBackend: ...
def _create_wav_sink(
    engine: _AudioEngineBackend,
    sink_id: str,
    route_ids: list[str],
    fd: int,
    mix_gain: float,
) -> _WavSinkBackend: ...
