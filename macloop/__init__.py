from __future__ import annotations

import asyncio
import os
import queue
import uuid
import weakref
from dataclasses import dataclass
from pathlib import Path
from typing import Any, AsyncIterator, Iterator, Optional, Protocol, Sequence, Tuple, Type, Union

import numpy as np
import numpy.typing as npt

from ._macloop import (
    AsrInputStats,
    LatencyStats,
    PipelineStats,
    ProcessorStats,
    StreamStats,
    WavSinkStats,
    _AsrSinkBackend,
    _AudioEngineBackend,
    _WavSinkBackend,
    _create_asr_sink,
    _create_wav_sink,
)
from ._macloop import list_applications as _list_applications
from ._macloop import list_displays as _list_displays
from ._macloop import list_microphones as _list_microphones


AudioSamples = Union[npt.NDArray[np.int16], npt.NDArray[np.float32]]

_STOP = object()


def _generate_id(prefix: str) -> str:
    return f"{prefix}_{uuid.uuid4().hex}"


def _drop_oldest_put(q: "queue.Queue[object]", item: object) -> None:
    try:
        q.put_nowait(item)
    except queue.Full:
        try:
            q.get_nowait()
        except queue.Empty:
            pass
        q.put_nowait(item)


def _drop_oldest_put_async(q: "asyncio.Queue[object]", item: object) -> None:
    try:
        q.put_nowait(item)
    except asyncio.QueueFull:
        try:
            q.get_nowait()
        except asyncio.QueueEmpty:
            pass
        q.put_nowait(item)


def _raise_on_unexpected_kwargs(name: str, kwargs: dict[str, Any]) -> None:
    if not kwargs:
        return
    unexpected = ", ".join(sorted(kwargs))
    raise TypeError(f"{name} got unexpected keyword arguments: {unexpected}")


@dataclass(frozen=True, slots=True)
class AudioChunk:
    route_id: str
    frames: int
    samples: AudioSamples


class _StreamSourceType(Protocol):
    @classmethod
    def _resolve_backend_spec_kwargs(cls, **kwargs: Any) -> tuple[str, dict[str, Any]]: ...


class _ProcessorSpec(Protocol):
    id: Optional[str]

    def _backend_spec(self) -> tuple[str, dict[str, Any]]: ...


class MicrophoneSource:
    def __init__(
        self,
        id: Optional[str] = None,
        *,
        device_id: Optional[int] = None,
        vpio_enabled: bool = True,
    ) -> None:
        self.id = id
        self.device_id = device_id
        self.vpio_enabled = vpio_enabled

    @staticmethod
    def list_devices() -> list[dict[str, Any]]:
        return list(_list_microphones())

    @classmethod
    def _resolve_backend_spec_kwargs(
        cls,
        *,
        device_id: Optional[int] = None,
        vpio_enabled: bool = True,
        **kwargs: Any,
    ) -> tuple[str, dict[str, Any]]:
        _raise_on_unexpected_kwargs("MicrophoneSource", kwargs)
        return (
            "microphone",
            {
                "device_id": device_id,
                "vpio_enabled": vpio_enabled,
            },
        )


class SystemAudioSource:
    def __init__(self, id: Optional[str] = None, *, display_id: Optional[int] = None) -> None:
        self.id = id
        self.display_id = display_id

    @staticmethod
    def list_displays() -> list[dict[str, Any]]:
        return list(_list_displays())

    @classmethod
    def _resolve_backend_spec_kwargs(
        cls,
        *,
        display_id: Optional[int] = None,
        **kwargs: Any,
    ) -> tuple[str, dict[str, Any]]:
        _raise_on_unexpected_kwargs("SystemAudioSource", kwargs)
        if display_id is None:
            displays = cls.list_displays()
            if not displays:
                raise RuntimeError("no displays are available for system audio capture")
            display_id = int(displays[0]["id"])

        return ("system_audio", {"display_id": display_id})


class AppAudioSource:
    def __init__(
        self,
        id: Optional[str] = None,
        *,
        pids: Optional[Sequence[int]] = None,
        display_id: Optional[int] = None,
    ) -> None:
        self.id = id
        self.pids = list(pids) if pids is not None else None
        self.display_id = display_id

    @staticmethod
    def list_applications() -> list[dict[str, Any]]:
        return list(_list_applications())

    @classmethod
    def _resolve_backend_spec_kwargs(
        cls,
        *,
        pids: Optional[Sequence[int]] = None,
        display_id: Optional[int] = None,
        **kwargs: Any,
    ) -> tuple[str, dict[str, Any]]:
        _raise_on_unexpected_kwargs("AppAudioSource", kwargs)
        if pids is None:
            raise ValueError(
                "AppAudioSource requires explicit pids; use AppAudioSource.list_applications() to choose them"
            )

        normalized_pids = list(dict.fromkeys(int(pid) for pid in pids))
        if not normalized_pids:
            raise ValueError("AppAudioSource requires at least one pid in pids")

        return (
            "application_audio",
            {
                "pids": normalized_pids,
                "display_id": display_id,
            },
        )


class SyntheticSource:
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
    ) -> None:
        self.id = id
        self.frames_per_callback = frames_per_callback
        self.callback_count = callback_count
        self.start_value = start_value
        self.step_value = step_value
        self.interval_ms = interval_ms
        self.start_delay_ms = start_delay_ms

    @classmethod
    def _resolve_backend_spec_kwargs(
        cls,
        *,
        frames_per_callback: int = 160,
        callback_count: int = 4,
        start_value: float = 0.0,
        step_value: float = 1.0,
        interval_ms: int = 0,
        start_delay_ms: int = 20,
        **kwargs: Any,
    ) -> tuple[str, dict[str, Any]]:
        _raise_on_unexpected_kwargs("SyntheticSource", kwargs)
        return (
            "synthetic",
            {
                "frames_per_callback": frames_per_callback,
                "callback_count": callback_count,
                "start_value": start_value,
                "step_value": step_value,
                "interval_ms": interval_ms,
                "start_delay_ms": start_delay_ms,
            },
        )


class GainProcessor:
    def __init__(self, id: Optional[str] = None, *, gain: float) -> None:
        self.id = id
        self.gain = gain

    def _backend_spec(self) -> tuple[str, dict[str, Any]]:
        return ("gain", {"gain": self.gain})


class StreamHandle:
    def __init__(self, id: str, engine: "AudioEngine") -> None:
        self.id = id
        self._engine_ref = weakref.ref(engine)


class RouteHandle:
    def __init__(self, id: str, stream_id: str, engine: "AudioEngine") -> None:
        self.id = id
        self.stream_id = stream_id
        self._engine_ref = weakref.ref(engine)


class ProcessorHandle:
    def __init__(self, id: str, stream_id: str, engine: "AudioEngine") -> None:
        self.id = id
        self.stream_id = stream_id
        self._engine_ref = weakref.ref(engine)


class _Closable(Protocol):
    def close(self) -> None: ...


class AudioEngine:
    def __init__(self) -> None:
        self._backend = _AudioEngineBackend()
        self._streams: dict[str, StreamHandle] = {}
        self._routes: dict[str, RouteHandle] = {}
        self._processors: dict[str, ProcessorHandle] = {}
        self._claimed_routes: set[str] = set()
        self._sink_refs: list[weakref.ReferenceType[_Closable]] = []
        self._closed = False

    def create_stream(
        self,
        source_type: Type[_StreamSourceType],
        id: Optional[str] = None,
        /,
        **config: Any,
    ) -> StreamHandle:
        self._ensure_open()

        stream_id = id or _generate_id("stream")
        if stream_id in self._streams:
            raise ValueError(f"stream '{stream_id}' already exists")

        try:
            source_kind, backend_config = source_type._resolve_backend_spec_kwargs(**config)
        except AttributeError as exc:
            raise NotImplementedError(
                "only MicrophoneSource, AppAudioSource, SystemAudioSource, and SyntheticSource are supported in this version"
            ) from exc

        self._backend.create_stream(stream_id, source_kind, backend_config)

        handle = StreamHandle(stream_id, self)
        self._streams[stream_id] = handle
        return handle

    def add_processor(
        self,
        *,
        stream: Union[StreamHandle, str],
        processor: GainProcessor,
    ) -> ProcessorHandle:
        self._ensure_open()

        stream_id = self._resolve_stream_id(stream)
        processor_id = processor.id or _generate_id("processor")
        if processor_id in self._processors:
            raise ValueError(f"processor '{processor_id}' already exists")

        try:
            processor_kind, config = processor._backend_spec()
        except AttributeError as exc:
            raise NotImplementedError("only GainProcessor is supported in this version") from exc

        self._backend.add_processor(stream_id, processor_id, processor_kind, config)
        processor.id = processor_id

        handle = ProcessorHandle(processor_id, stream_id, self)
        self._processors[processor_id] = handle
        return handle

    def route(self, id: Optional[str] = None, *, stream: Union[StreamHandle, str]) -> RouteHandle:
        self._ensure_open()

        stream_id = self._resolve_stream_id(stream)
        route_id = id or _generate_id("route")
        if route_id in self._routes:
            raise ValueError(f"route '{route_id}' already exists")

        self._backend.route(route_id, stream_id)

        handle = RouteHandle(route_id, stream_id, self)
        self._routes[route_id] = handle
        return handle

    def stats(self) -> dict[str, StreamStats]:
        self._ensure_open()
        return dict(self._backend.get_stats())

    def close(self) -> None:
        if self._closed:
            return

        self._closed = True
        backend_err: Optional[Exception] = None
        sink_err: Optional[Exception] = None
        try:
            self._backend.close()
        except Exception as exc:
            backend_err = exc
        finally:
            for sink_ref in list(self._sink_refs):
                sink = sink_ref()
                if sink is None:
                    continue
                try:
                    sink.close()
                except Exception as exc:
                    if sink_err is None:
                        sink_err = exc

            self._sink_refs.clear()
            self._claimed_routes.clear()

        if backend_err is not None:
            raise backend_err
        if sink_err is not None:
            raise sink_err

    def __enter__(self) -> "AudioEngine":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass

    def _resolve_stream_id(self, stream: Union[StreamHandle, str]) -> str:
        if isinstance(stream, StreamHandle):
            self._ensure_stream_handle(stream)
            return stream.id

        if not isinstance(stream, str):
            raise TypeError("stream must be a StreamHandle or stream id string")

        if stream not in self._streams:
            raise ValueError(f"unknown stream '{stream}'")

        return stream

    def _ensure_stream_handle(self, stream: StreamHandle) -> None:
        engine = stream._engine_ref()
        if engine is not self:
            raise ValueError("stream handle belongs to a different audio engine")

    def _ensure_route_handle(self, route: RouteHandle) -> None:
        engine = route._engine_ref()
        if engine is not self:
            raise ValueError("route handle belongs to a different audio engine")

    def _ensure_routes_available(self, routes: Sequence[RouteHandle]) -> None:
        if not routes:
            raise ValueError("routes must not be empty")

        for route in routes:
            self._ensure_route_handle(route)
            if route.id in self._claimed_routes:
                raise ValueError(f"route '{route.id}' is already in use")

    def _claim_routes(self, route_ids: Sequence[str]) -> None:
        self._claimed_routes.update(route_ids)

    def _release_routes(self, route_ids: Sequence[str]) -> None:
        for route_id in route_ids:
            self._claimed_routes.discard(route_id)

    def _register_sink(self, sink: _Closable) -> None:
        self._sink_refs.append(weakref.ref(sink))

    def _ensure_open(self) -> None:
        if self._closed:
            raise RuntimeError("audio engine is closed")


class AsrSink:
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
    ) -> None:
        engine = _engine_from_routes(routes)
        engine._ensure_open()
        engine._ensure_routes_available(routes)

        sink_id = id or _generate_id("asr_sink")
        route_ids = [route.id for route in routes]
        out_queue: "queue.Queue[object]" = queue.Queue(maxsize=max_queue_size)

        self.id = sink_id
        self._engine_ref = weakref.ref(engine)
        self._route_ids = tuple(route_ids)
        self._queue = out_queue
        self._queue_maxsize = max_queue_size
        self._async_queue: Optional[asyncio.Queue[object]] = None
        self._async_loop: Optional[asyncio.AbstractEventLoop] = None
        self._consume_mode: Optional[str] = None
        self._closed = False

        def _callback(route_id: str, frames: int, samples: AudioSamples) -> None:
            self._enqueue_chunk(AudioChunk(route_id=route_id, frames=frames, samples=samples))

        backend = _create_asr_sink(
            engine._backend,
            sink_id,
            route_ids,
            sample_rate,
            channels,
            sample_format,
            chunk_frames,
            _callback,
        )

        self._backend = backend

        engine._claim_routes(route_ids)
        engine._register_sink(self)

    def chunks(self) -> Iterator[AudioChunk]:
        self._activate_sync_mode()
        while True:
            item = self._queue.get()
            if item is _STOP:
                return
            yield item  # type: ignore[misc]

    async def chunks_async(self) -> AsyncIterator[AudioChunk]:
        async_queue = self._activate_async_mode()
        while True:
            item = await async_queue.get()
            if item is _STOP:
                return
            yield item  # type: ignore[misc]

    def close(self) -> None:
        if self._closed:
            return

        err: Optional[Exception] = None
        try:
            self._backend.close()
        except Exception as exc:
            err = exc
        finally:
            self._closed = True
            engine = self._engine_ref()
            if engine is not None:
                engine._release_routes(self._route_ids)
            _drop_oldest_put(self._queue, _STOP)
            async_loop = self._async_loop
            async_queue = self._async_queue
            if async_loop is not None and async_queue is not None:
                async_loop.call_soon_threadsafe(_drop_oldest_put_async, async_queue, _STOP)

        if err is not None:
            raise err

    def stats(self) -> dict[str, AsrInputStats]:
        return dict(self._backend.stats())

    def __iter__(self) -> Iterator[AudioChunk]:
        return self.chunks()

    def __aiter__(self) -> AsyncIterator[AudioChunk]:
        return self.chunks_async()

    def __enter__(self) -> "AsrSink":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass

    def _enqueue_chunk(self, chunk: AudioChunk) -> None:
        async_loop = self._async_loop
        async_queue = self._async_queue
        if async_loop is not None and async_queue is not None:
            async_loop.call_soon_threadsafe(_drop_oldest_put_async, async_queue, chunk)
            return
        _drop_oldest_put(self._queue, chunk)

    def _activate_sync_mode(self) -> None:
        if self._consume_mode is None:
            self._consume_mode = "sync"
            return
        if self._consume_mode != "sync":
            raise RuntimeError("asr sink is already being consumed via asyncio")

    def _activate_async_mode(self) -> "asyncio.Queue[object]":
        loop = asyncio.get_running_loop()

        if self._consume_mode is None:
            self._consume_mode = "async"
        elif self._consume_mode != "async":
            raise RuntimeError("asr sink is already being consumed synchronously")

        if self._async_queue is None:
            self._async_loop = loop
            self._async_queue = asyncio.Queue(maxsize=self._queue_maxsize)
            self._drain_sync_queue_into_async()
        elif self._async_loop is not loop:
            raise RuntimeError("asr sink asyncio consumer is bound to a different event loop")

        return self._async_queue

    def _drain_sync_queue_into_async(self) -> None:
        if self._async_queue is None:
            return

        while True:
            try:
                item = self._queue.get_nowait()
            except queue.Empty:
                return
            _drop_oldest_put_async(self._async_queue, item)


class WavSink:
    def __init__(
        self,
        id: Optional[str] = None,
        *,
        route: Optional[RouteHandle] = None,
        routes: Optional[Sequence[RouteHandle]] = None,
        file: Any,
        mix_gain: Optional[float] = None,
    ) -> None:
        route_list = _resolve_sink_routes(route=route, routes=routes)
        engine = _engine_from_routes(route_list)
        engine._ensure_open()
        engine._ensure_routes_available(route_list)

        sink_id = id or _generate_id("wav_sink")
        route_ids = [item.id for item in route_list]
        effective_mix_gain = mix_gain if mix_gain is not None else (1.0 / len(route_ids))
        fd, should_close = _resolve_wav_fd(file)
        try:
            backend = _create_wav_sink(
                engine._backend,
                sink_id,
                route_ids,
                fd,
                float(effective_mix_gain),
            )
        finally:
            if should_close:
                os.close(fd)

        self.id = sink_id
        self._backend = backend
        self._engine_ref = weakref.ref(engine)
        self._route_ids = tuple(route_ids)
        self._closed = False

        engine._claim_routes(route_ids)
        engine._register_sink(self)

    def close(self) -> None:
        if self._closed:
            return

        err: Optional[Exception] = None
        try:
            self._backend.close()
        except Exception as exc:
            err = exc
        finally:
            self._closed = True
            engine = self._engine_ref()
            if engine is not None:
                engine._release_routes(self._route_ids)

        if err is not None:
            raise err

    def stats(self) -> WavSinkStats:
        return self._backend.stats()

    def __enter__(self) -> "WavSink":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass
def _engine_from_routes(routes: Sequence[RouteHandle]) -> AudioEngine:
    if not routes:
        raise ValueError("routes must not be empty")

    engine = _engine_from_route(routes[0])
    for route in routes[1:]:
        other = _engine_from_route(route)
        if other is not engine:
            raise ValueError("all routes must belong to the same audio engine")
    return engine


def _engine_from_route(route: RouteHandle) -> AudioEngine:
    if not isinstance(route, RouteHandle):
        raise TypeError("route must be a RouteHandle")

    engine = route._engine_ref()
    if engine is None:
        raise RuntimeError("route handle is no longer attached to a live audio engine")
    return engine


def _resolve_sink_routes(
    *, route: Optional[RouteHandle], routes: Optional[Sequence[RouteHandle]]
) -> Sequence[RouteHandle]:
    if route is not None and routes is not None:
        raise ValueError("pass either route or routes, not both")
    if route is not None:
        return [route]
    if routes is None or not routes:
        raise ValueError("routes must not be empty")
    return routes


def _resolve_wav_fd(file: Any) -> Tuple[int, bool]:
    if isinstance(file, int):
        if file < 0:
            raise ValueError("file descriptor must be non-negative")
        return file, False

    if isinstance(file, (str, Path, os.PathLike)):
        path = Path(file)
        path.parent.mkdir(parents=True, exist_ok=True)
        flags = os.O_CREAT | os.O_TRUNC | os.O_WRONLY
        return os.open(os.fspath(path), flags, 0o666), True

    fileno = getattr(file, "fileno", None)
    if fileno is None:
        raise TypeError("file must be an fd, path, or file-like object with fileno()")

    fd = int(fileno())
    if fd < 0:
        raise ValueError("file descriptor must be non-negative")
    return fd, False


__all__ = [
    "AppAudioSource",
    "AsrSink",
    "AsrInputStats",
    "AudioChunk",
    "AudioEngine",
    "AudioSamples",
    "GainProcessor",
    "LatencyStats",
    "MicrophoneSource",
    "PipelineStats",
    "ProcessorStats",
    "ProcessorHandle",
    "RouteHandle",
    "StreamStats",
    "StreamHandle",
    "SystemAudioSource",
    "SyntheticSource",
    "WavSink",
    "WavSinkStats",
]
