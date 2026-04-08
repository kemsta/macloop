from __future__ import annotations

import asyncio
import gc
import os
import queue

import numpy as np
import pytest


async def _drain_async_queue(q: "asyncio.Queue[object]") -> list[object]:
    items = []
    while not q.empty():
        items.append(await q.get())
    return items


def test_generate_id_and_unexpected_kwargs_helpers(macloop_module) -> None:
    first = macloop_module._generate_id("sink")
    second = macloop_module._generate_id("sink")
    assert first.startswith("sink_")
    assert second.startswith("sink_")
    assert first != second

    macloop_module._raise_on_unexpected_kwargs("Thing", {})
    with pytest.raises(TypeError, match=r"Thing got unexpected keyword arguments: a, z"):
        macloop_module._raise_on_unexpected_kwargs("Thing", {"z": 1, "a": 2})


def test_drop_oldest_put_helpers(macloop_module) -> None:
    q: "queue.Queue[object]" = queue.Queue(maxsize=2)
    macloop_module._drop_oldest_put(q, "first")
    macloop_module._drop_oldest_put(q, "second")
    macloop_module._drop_oldest_put(q, "third")
    assert q.get_nowait() == "second"
    assert q.get_nowait() == "third"

    async def exercise_async() -> list[object]:
        q_async: "asyncio.Queue[object]" = asyncio.Queue(maxsize=2)
        macloop_module._drop_oldest_put_async(q_async, "first")
        macloop_module._drop_oldest_put_async(q_async, "second")
        macloop_module._drop_oldest_put_async(q_async, "third")
        return await _drain_async_queue(q_async)

    assert asyncio.run(exercise_async()) == ["second", "third"]


def test_microphone_source_list_devices_passthrough(macloop_module) -> None:
    microphones = macloop_module.MicrophoneSource.list_devices()
    assert microphones == [
        {"id": 11, "name": "Built-in Mic", "is_default": True},
        {"id": 22, "name": "USB Mic", "is_default": False},
    ]


def test_stats_passthrough(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        stats = engine.stats()

    assert list(stats.keys()) == ["stream_1"]
    snapshot = stats["stream_1"]
    assert snapshot.pipeline.total_callback_time_us == 10
    assert snapshot.pipeline.dropped_frames == 1
    assert snapshot.pipeline.buffer_size == 256
    assert snapshot.pipeline.latency.count == 4
    assert snapshot.pipeline.latency.p95_us == 16
    assert snapshot.processors["gain"].processing_time_us == 6
    assert snapshot.processors["gain"].latency.max_us == 12


def test_audio_engine_rejects_duplicate_ids(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(macloop_module.MicrophoneSource, "stream_dup")
        with pytest.raises(ValueError, match="stream 'stream_dup' already exists"):
            engine.create_stream(macloop_module.MicrophoneSource, "stream_dup")

        route = engine.route(id="route_dup", stream=stream)
        with pytest.raises(ValueError, match="route 'route_dup' already exists"):
            engine.route(id="route_dup", stream=stream)

        engine.add_processor(
            stream=stream,
            processor=macloop_module.GainProcessor(id="processor_dup", gain=1.0),
        )
        with pytest.raises(ValueError, match="processor 'processor_dup' already exists"):
            engine.add_processor(
                stream=stream,
                processor=macloop_module.GainProcessor(id="processor_dup", gain=1.0),
            )

        assert route.id == "route_dup"


def test_audio_engine_rejects_unknown_types_and_invalid_stream_refs(macloop_module) -> None:
    class UnsupportedSource:
        pass

    class UnsupportedProcessor:
        id = None

    with macloop_module.AudioEngine() as engine:
        with pytest.raises(NotImplementedError, match="only MicrophoneSource"):
            engine.create_stream(UnsupportedSource)

        stream = engine.create_stream(macloop_module.MicrophoneSource)

        with pytest.raises(TypeError, match="stream must be a StreamHandle or stream id string"):
            engine.add_processor(stream=123, processor=macloop_module.GainProcessor(gain=1.0))

        with pytest.raises(ValueError, match="unknown stream 'missing'"):
            engine.route(stream="missing")

        with pytest.raises(NotImplementedError, match="only GainProcessor"):
            engine.add_processor(stream=stream, processor=UnsupportedProcessor())


def test_audio_engine_rejects_cross_engine_handles(macloop_module) -> None:
    with macloop_module.AudioEngine() as first, macloop_module.AudioEngine() as second:
        foreign_stream = first.create_stream(macloop_module.MicrophoneSource)
        with pytest.raises(ValueError, match="stream handle belongs to a different audio engine"):
            second.route(stream=foreign_stream)

        first_route = first.route(stream=foreign_stream)
        second_stream = second.create_stream(macloop_module.MicrophoneSource)
        second_route = second.route(stream=second_stream)
        with pytest.raises(ValueError, match="all routes must belong to the same audio engine"):
            macloop_module.AsrSink(
                routes=[first_route, second_route],
                chunk_frames=2,
                sample_rate=16000,
                channels=1,
                sample_format="f32",
            )


def test_audio_engine_close_is_idempotent(macloop_module) -> None:
    engine = macloop_module.AudioEngine()
    engine.close()
    engine.close()
    assert engine._backend.closed is True


def test_audio_engine_close_swallows_sink_errors(macloop_module) -> None:
    class FailingSink:
        def close(self) -> None:
            raise RuntimeError("boom")

    engine = macloop_module.AudioEngine()
    engine._register_sink(FailingSink())
    engine.close()

    assert engine._backend.closed is True
    with pytest.raises(RuntimeError, match="audio engine is closed"):
        engine.stats()


def test_asr_sink_close_releases_routes_on_backend_error(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(macloop_module.MicrophoneSource)
        route = engine.route(stream=stream)
        sink = macloop_module.AsrSink(
            routes=[route],
            chunk_frames=2,
            sample_rate=16000,
            channels=1,
            sample_format="f32",
        )
        sink._backend.close = lambda: (_ for _ in ()).throw(RuntimeError("boom"))

        with pytest.raises(RuntimeError, match="boom"):
            sink.close()

        assert route.id not in engine._claimed_routes
        assert sink._closed is True


def test_wav_sink_close_releases_routes_on_backend_error(macloop_module, tmp_path) -> None:
    path = tmp_path / "boom.wav"
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(macloop_module.MicrophoneSource)
        route = engine.route(stream=stream)
        sink = macloop_module.WavSink(route=route, file=path)
        sink._backend.close = lambda: (_ for _ in ()).throw(RuntimeError("boom"))

        with pytest.raises(RuntimeError, match="boom"):
            sink.close()

        assert route.id not in engine._claimed_routes
        assert sink._closed is True


def test_asr_sink_rejects_different_asyncio_event_loop(macloop_module) -> None:
    async def activate_async_mode(sink) -> None:
        sink._activate_async_mode()

    loop_a = asyncio.new_event_loop()
    loop_b = asyncio.new_event_loop()
    try:
        with macloop_module.AudioEngine() as engine:
            stream = engine.create_stream(macloop_module.MicrophoneSource)
            route = engine.route(stream=stream)
            sink = macloop_module.AsrSink(
                routes=[route],
                chunk_frames=2,
                sample_rate=16000,
                channels=1,
                sample_format="f32",
            )
            try:
                loop_a.run_until_complete(activate_async_mode(sink))
                with pytest.raises(RuntimeError, match="bound to a different event loop"):
                    loop_b.run_until_complete(activate_async_mode(sink))
            finally:
                sink.close()
    finally:
        loop_a.close()
        loop_b.close()


def test_asr_sink_rejects_mixed_sync_async_consumption(macloop_module) -> None:
    async def consume_async_after_sync(sink) -> None:
        with pytest.raises(RuntimeError, match="already being consumed synchronously"):
            agen = sink.chunks_async()
            await agen.__anext__()

    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(macloop_module.MicrophoneSource)
        route = engine.route(stream=stream)
        sink = macloop_module.AsrSink(
            routes=[route],
            chunk_frames=2,
            sample_rate=16000,
            channels=1,
            sample_format="f32",
        )
        try:
            chunk = next(sink.chunks())
            assert chunk.route_id == route.id
            asyncio.run(consume_async_after_sync(sink))
        finally:
            sink.close()


def test_engine_from_route_rejects_invalid_and_stale_handles(macloop_module) -> None:
    with pytest.raises(TypeError, match="route must be a RouteHandle"):
        macloop_module._engine_from_route("nope")

    engine = macloop_module.AudioEngine()
    stream = engine.create_stream(macloop_module.MicrophoneSource)
    route = engine.route(stream=stream)
    engine.close()
    del engine
    gc.collect()

    with pytest.raises(RuntimeError, match="no longer attached to a live audio engine"):
        macloop_module._engine_from_route(route)


def test_resolve_sink_routes_and_wav_fd_helpers(macloop_module, tmp_path) -> None:
    engine = macloop_module.AudioEngine()
    stream = engine.create_stream(macloop_module.MicrophoneSource)
    route = engine.route(stream=stream)

    with pytest.raises(ValueError, match="pass either route or routes, not both"):
        macloop_module._resolve_sink_routes(route=route, routes=[route])
    with pytest.raises(ValueError, match="routes must not be empty"):
        macloop_module._resolve_sink_routes(route=None, routes=[])

    path_fd, should_close = macloop_module._resolve_wav_fd(tmp_path / "nested" / "out.wav")
    assert should_close is True
    os.close(path_fd)

    with (tmp_path / "existing.wav").open("w+b") as fileobj:
        file_fd, should_close = macloop_module._resolve_wav_fd(fileobj)
        assert file_fd == fileobj.fileno()
        assert should_close is False

    with pytest.raises(ValueError, match="file descriptor must be non-negative"):
        macloop_module._resolve_wav_fd(-1)
    with pytest.raises(TypeError, match="file must be an fd, path, or file-like object with fileno"):
        macloop_module._resolve_wav_fd(object())

    engine.close()


def test_source_resolvers_validate_kwargs_and_defaults(macloop_module, monkeypatch) -> None:
    monkeypatch.setattr(macloop_module, "_list_displays", lambda: [])

    with pytest.raises(RuntimeError, match="no displays are available for system audio capture"):
        macloop_module.SystemAudioSource._resolve_backend_spec_kwargs()

    with pytest.raises(ValueError, match="requires at least one pid"):
        macloop_module.AppAudioSource._resolve_backend_spec_kwargs(pids=[])

    with pytest.raises(TypeError, match="unexpected keyword arguments: nope"):
        macloop_module.MicrophoneSource._resolve_backend_spec_kwargs(nope=True)

    with pytest.raises(TypeError, match="unexpected keyword arguments: nope"):
        macloop_module.SyntheticSource._resolve_backend_spec_kwargs(nope=True)


def test_wav_sink_route_shortcut_and_stats(macloop_module, tmp_path) -> None:
    path = tmp_path / "helper.wav"
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(macloop_module.MicrophoneSource)
        route = engine.route(stream=stream)
        sink = macloop_module.WavSink(route=route, file=path)
        try:
            stats = sink.stats()
        finally:
            sink.close()

    assert stats.write_calls == 3
    assert stats.frames_written == 6
