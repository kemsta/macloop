from __future__ import annotations

from typing import Any, AsyncIterator, Iterator, Optional, Sequence, Union

import numpy as np
import numpy.typing as npt

from ._macloop import (
    AsrInputStats,
    LatencyStats,
    PipelineStats,
    ProcessorStats,
    StreamStats,
    WavSinkStats,
)

AudioSamples = Union[npt.NDArray[np.int16], npt.NDArray[np.float32]]


class AudioChunk:
    route_id: str
    frames: int
    samples: AudioSamples


class MicrophoneSource:
    id: Optional[str]
    device_id: Optional[int]
    vpio_enabled: bool
    def __init__(
        self,
        id: Optional[str] = None,
        *,
        device_id: Optional[int] = None,
        vpio_enabled: bool = True,
    ) -> None: ...
    @staticmethod
    def list_devices() -> list[dict[str, Any]]: ...


class SystemAudioSource:
    id: Optional[str]
    display_id: Optional[int]
    def __init__(self, id: Optional[str] = None, *, display_id: Optional[int] = None) -> None: ...
    @staticmethod
    def list_displays() -> list[dict[str, Any]]: ...


class AppAudioSource:
    id: Optional[str]
    pids: Optional[Sequence[int]]
    display_id: Optional[int]
    def __init__(
        self,
        id: Optional[str] = None,
        *,
        pids: Optional[Sequence[int]] = None,
        display_id: Optional[int] = None,
    ) -> None: ...
    @staticmethod
    def list_applications() -> list[dict[str, Any]]: ...


class SyntheticSource:
    id: Optional[str]
    frames_per_callback: int
    callback_count: int
    start_value: float
    step_value: float
    interval_ms: int
    start_delay_ms: int
    def __init__(
        self,
        id: Optional[str] = None,
        *,
        frames_per_callback: int = 160,
        callback_count: int = 4,
        start_value: float = 0.0,
        step_value: float = 1.0,
        interval_ms: int = 0,
        start_delay_ms: int = 20,
    ) -> None: ...


class GainProcessor:
    id: Optional[str]
    gain: float
    def __init__(self, id: Optional[str] = None, *, gain: float) -> None: ...


class StreamHandle:
    id: str


class RouteHandle:
    id: str
    stream_id: str


class ProcessorHandle:
    id: str
    stream_id: str


class AudioEngine:
    def __init__(self) -> None: ...
    def create_stream(
        self,
        source_type: type[MicrophoneSource] | type[AppAudioSource] | type[SystemAudioSource] | type[SyntheticSource],
        id: Optional[str] = None,
        /,
        **config: Any,
    ) -> StreamHandle: ...
    def add_processor(
        self,
        *,
        stream: Union[StreamHandle, str],
        processor: GainProcessor,
    ) -> ProcessorHandle: ...
    def route(
        self,
        id: Optional[str] = None,
        *,
        stream: Union[StreamHandle, str],
    ) -> RouteHandle: ...
    def stats(self) -> dict[str, StreamStats]: ...
    def close(self) -> None: ...
    def __enter__(self) -> AudioEngine: ...
    def __exit__(self, exc_type: object, exc: object, tb: object) -> None: ...


class AsrSink:
    id: str
    def __init__(
        self,
        id: Optional[str] = None,
        *,
        routes: Sequence[RouteHandle],
        chunk_frames: int,
        sample_rate: int,
        channels: int,
        sample_format: str,
        max_queue_size: int = 256,
    ) -> None: ...
    def chunks(self) -> Iterator[AudioChunk]: ...
    def chunks_async(self) -> AsyncIterator[AudioChunk]: ...
    def stats(self) -> dict[str, AsrInputStats]: ...
    def close(self) -> None: ...
    def __iter__(self) -> Iterator[AudioChunk]: ...
    def __aiter__(self) -> AsyncIterator[AudioChunk]: ...
    def __enter__(self) -> AsrSink: ...
    def __exit__(self, exc_type: object, exc: object, tb: object) -> None: ...


class WavSink:
    id: str
    def __init__(
        self,
        id: Optional[str] = None,
        *,
        route: Optional[RouteHandle] = None,
        routes: Optional[Sequence[RouteHandle]] = None,
        file: Any,
        mix_gain: Optional[float] = None,
    ) -> None: ...
    def stats(self) -> WavSinkStats: ...
    def close(self) -> None: ...
    def __enter__(self) -> WavSink: ...
    def __exit__(self, exc_type: object, exc: object, tb: object) -> None: ...
