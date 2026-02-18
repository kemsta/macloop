from __future__ import annotations

import asyncio
import queue
import threading
from dataclasses import dataclass
from typing import Any, AsyncIterator, Iterator, Optional

from ._macloop import AudioEngine as _AudioEngine
from ._macloop import AudioProcessingConfig, list_audio_sources


@dataclass(frozen=True)
class AudioChunk:
    source: str
    samples: Any


_STOP = object()


class Capture:
    def __init__(
        self,
        display_id: Optional[int] = None,
        pid: Optional[int] = None,
        config: Optional[AudioProcessingConfig] = None,
        *,
        capture_system: bool = True,
        capture_mic: bool = False,
        max_queue_size: int = 256,
    ) -> None:
        self._engine = _AudioEngine(display_id=display_id, pid=pid, config=config)
        self._capture_system = capture_system
        self._capture_mic = capture_mic
        self._max_queue_size = max_queue_size
        self._started = False
        self._lock = threading.Lock()
        self._sync_queue: Optional[queue.Queue[object]] = None
        self._async_queue: Optional[asyncio.Queue[object]] = None
        self._loop: Optional[asyncio.AbstractEventLoop] = None

    def _enqueue_sync(self, item: object) -> None:
        q = self._sync_queue
        if q is None:
            return
        try:
            q.put_nowait(item)
        except queue.Full:
            try:
                q.get_nowait()
            except queue.Empty:
                pass
            q.put_nowait(item)

    def _enqueue_async(self, item: object) -> None:
        q = self._async_queue
        loop = self._loop
        if q is None or loop is None:
            return

        def _put() -> None:
            try:
                q.put_nowait(item)
            except asyncio.QueueFull:
                try:
                    _ = q.get_nowait()
                except asyncio.QueueEmpty:
                    pass
                q.put_nowait(item)

        loop.call_soon_threadsafe(_put)

    def _build_sync_callback(self):
        def _callback(source: str, samples: Any) -> None:
            self._enqueue_sync(AudioChunk(source=source, samples=samples))

        return _callback

    def _build_async_callback(self):
        def _callback(source: str, samples: Any) -> None:
            self._enqueue_async(AudioChunk(source=source, samples=samples))

        return _callback

    def start(self) -> None:
        with self._lock:
            if self._started:
                return
            if self._sync_queue is not None:
                callback = self._build_sync_callback()
            elif self._async_queue is not None:
                callback = self._build_async_callback()
            else:
                self._sync_queue = queue.Queue(maxsize=self._max_queue_size)
                callback = self._build_sync_callback()

            self._engine.start(
                callback,
                capture_system=self._capture_system,
                capture_mic=self._capture_mic,
            )
            self._started = True

    def stop(self) -> None:
        with self._lock:
            if not self._started:
                return
            self._engine.stop()
            self._started = False

        self._enqueue_sync(_STOP)
        self._enqueue_async(_STOP)

    def __enter__(self) -> Iterator[AudioChunk]:
        self._sync_queue = queue.Queue(maxsize=self._max_queue_size)
        self._async_queue = None
        self._loop = None
        self.start()
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.stop()
        self._sync_queue = None

    def __iter__(self) -> Iterator[AudioChunk]:
        return self

    def __next__(self) -> AudioChunk:
        q = self._sync_queue
        if q is None:
            raise RuntimeError("Use `with Capture(...) as stream:` for sync iteration.")

        item = q.get()
        if item is _STOP:
            raise StopIteration
        return item  # type: ignore[return-value]

    async def __aenter__(self) -> AsyncIterator[AudioChunk]:
        self._loop = asyncio.get_running_loop()
        self._async_queue = asyncio.Queue(maxsize=self._max_queue_size)
        self._sync_queue = None
        self.start()
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        self.stop()
        self._async_queue = None
        self._loop = None

    def __aiter__(self) -> AsyncIterator[AudioChunk]:
        return self

    async def __anext__(self) -> AudioChunk:
        q = self._async_queue
        if q is None:
            raise RuntimeError("Use `async with Capture(...) as stream:` for async iteration.")
        item = await q.get()
        if item is _STOP:
            raise StopAsyncIteration
        return item  # type: ignore[return-value]

    def get_stats(self):
        return self._engine.get_stats()


__all__ = ["AudioProcessingConfig", "AudioChunk", "Capture", "list_audio_sources"]
