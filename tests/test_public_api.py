from __future__ import annotations

import asyncio
import tempfile

import numpy as np
import pytest


def test_public_api_surface(macloop_module) -> None:
    expected = {
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
    }
    assert expected.issubset(set(macloop_module.__all__))
    assert "Capture" not in macloop_module.__all__
    assert "AudioProcessingConfig" not in macloop_module.__all__
    assert not hasattr(macloop_module, "list_microphones")
    assert not hasattr(macloop_module, "list_displays")


def test_audio_engine_builder_autogenerates_ids(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(
            macloop_module.MicrophoneSource,
            device_id=7,
            vpio_enabled=False,
        )
        route = engine.route(stream=stream)
        processor = engine.add_processor(stream=stream, processor=macloop_module.GainProcessor(gain=1.2))

        assert stream.id.startswith("stream_")
        assert route.id.startswith("route_")
        assert processor.id.startswith("processor_")

        assert engine._backend.calls[:3] == [
            (
                "create_stream",
                stream.id,
                "microphone",
                {"device_id": 7, "vpio_enabled": False},
            ),
            ("route", route.id, stream.id),
            (
                "add_processor",
                stream.id,
                processor.id,
                "gain",
                {"gain": 1.2},
            ),
        ]


def test_audio_engine_supports_synthetic_source(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(
            macloop_module.SyntheticSource,
            frames_per_callback=4,
            callback_count=3,
            start_value=1.0,
            step_value=0.5,
            interval_ms=2,
            start_delay_ms=10,
        )
        assert engine._backend.calls[0] == (
            "create_stream",
            stream.id,
            "synthetic",
            {
                "frames_per_callback": 4,
                "callback_count": 3,
                "start_value": 1.0,
                "step_value": 0.5,
                "interval_ms": 2,
                "start_delay_ms": 10,
            },
        )


def test_audio_engine_defaults_system_audio_to_first_display(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(macloop_module.SystemAudioSource)

        assert engine._backend.calls[0] == (
            "create_stream",
            stream.id,
            "system_audio",
            {"display_id": 101},
        )


def test_audio_engine_requires_explicit_app_audio_pids(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        with pytest.raises(ValueError, match="requires explicit pids"):
            engine.create_stream(macloop_module.AppAudioSource)

        assert engine._backend.calls == []


def test_audio_engine_supports_multi_app_audio_source(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(
            macloop_module.AppAudioSource,
            pids=[111, 222, 111],
        )

        assert engine._backend.calls[0] == (
            "create_stream",
            stream.id,
            "application_audio",
            {"pids": [111, 222], "display_id": None},
        )


def test_audio_engine_rejects_unknown_source_kwargs(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        with pytest.raises(TypeError, match="unexpected keyword arguments: nope"):
            engine.create_stream(macloop_module.MicrophoneSource, nope=True)


def test_system_audio_source_get_displays_returns_backend_data(macloop_module) -> None:
    displays = macloop_module.SystemAudioSource.list_displays()

    assert displays[0]["id"] == 101
    assert displays[0]["is_default"] is True
    assert displays[1]["width"] == 1920


def test_microphone_source_list_devices_returns_backend_data(macloop_module) -> None:
    microphones = macloop_module.MicrophoneSource.list_devices()

    assert microphones[0]["id"] == 11
    assert microphones[0]["is_default"] is True
    assert microphones[1]["name"] == "USB Mic"


def test_app_audio_source_list_applications_returns_backend_data(macloop_module) -> None:
    applications = macloop_module.AppAudioSource.list_applications()

    assert applications[0]["pid"] == 111
    assert applications[0]["is_default"] is True
    assert applications[1]["bundle_id"] == "com.apple.Music"


def test_asr_sink_yields_chunks_and_claims_routes(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(macloop_module.MicrophoneSource)
        route = engine.route(stream=stream)

        sink = macloop_module.AsrSink(
            routes=[route],
            chunk_frames=2,
            sample_rate=16000,
            channels=1,
            sample_format="f32",
            max_queue_size=1,
        )

        with pytest.raises(ValueError, match="already in use"):
            macloop_module.AsrSink(
                routes=[route],
                chunk_frames=2,
                sample_rate=16000,
                channels=1,
                sample_format="f32",
            )

        chunk = next(sink.chunks())
        assert chunk.route_id == route.id
        assert chunk.frames == 2
        assert np.array_equal(chunk.samples, np.array([0.3, 0.4], dtype=np.float32))
        stats = sink.stats()
        assert stats[route.id].chunks_emitted == 2
        assert stats[route.id].frames_emitted == 4
        assert stats[route.id].pending_frames == 1
        assert stats[route.id].poll.p95_us == 16
        assert stats[route.id].callback.p99_us == 64

        sink.close()
        final_stats = sink.stats()
        assert final_stats[route.id].chunks_emitted == 2

        second = macloop_module.AsrSink(
            routes=[route],
            chunk_frames=2,
            sample_rate=16000,
            channels=1,
            sample_format="f32",
        )
        second.close()


def test_asr_sink_supports_async_iteration(macloop_module) -> None:
    async def consume() -> tuple[object, object]:
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

            chunk_from_method = await anext(sink.chunks_async())

            chunk_from_aiter = None
            async for chunk in sink:
                chunk_from_aiter = chunk
                break

            sink.close()
            return chunk_from_method, chunk_from_aiter

    first, second = asyncio.run(consume())
    assert np.array_equal(first.samples, np.array([0.1, 0.2], dtype=np.float32))
    assert np.array_equal(second.samples, np.array([0.3, 0.4], dtype=np.float32))


def test_wav_sink_accepts_file_objects_and_engine_closes_sinks(macloop_module) -> None:
    engine = macloop_module.AudioEngine()
    stream = engine.create_stream(macloop_module.MicrophoneSource, "mic")
    route = engine.route("wav_route", stream=stream)

    with tempfile.TemporaryFile() as fileobj:
        sink = macloop_module.WavSink(route=route, file=fileobj)
        assert sink.id.startswith("wav_sink_")
        assert engine._backend.calls[-1] == ("create_wav_sink", sink.id, (route.id,), fileobj.fileno(), 1.0)
        assert sink._backend.closed is False
        stats = sink.stats()
        assert stats.write_calls == 3
        assert stats.samples_written == 12
        assert stats.frames_written == 6
        assert stats.write.p95_us == 32
        assert stats.finalize.p99_us == 64

    engine.close()
    final_stats = sink.stats()
    assert final_stats.frames_written == 6
    assert sink._backend.closed is True
    assert engine._backend.closed is True


def test_wav_sink_accepts_multiple_routes(macloop_module) -> None:
    with macloop_module.AudioEngine() as engine:
        stream = engine.create_stream(macloop_module.MicrophoneSource, "mic")
        route_a = engine.route("wav_a", stream=stream)
        route_b = engine.route("wav_b", stream=stream)

        with tempfile.TemporaryFile() as fileobj:
            sink = macloop_module.WavSink(routes=[route_a, route_b], file=fileobj)

            assert engine._backend.calls[-1] == (
                "create_wav_sink",
                sink.id,
                (route_a.id, route_b.id),
                fileobj.fileno(),
                0.5,
            )

            with pytest.raises(ValueError, match="already in use"):
                macloop_module.WavSink(route=route_a, file=fileobj)

            sink.close()

        with tempfile.TemporaryFile() as fileobj:
            second = macloop_module.WavSink(routes=[route_a, route_b], file=fileobj, mix_gain=0.25)
            assert engine._backend.calls[-1] == (
                "create_wav_sink",
                second.id,
                (route_a.id, route_b.id),
                fileobj.fileno(),
                0.25,
            )
            second.close()
