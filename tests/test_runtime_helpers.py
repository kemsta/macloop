from __future__ import annotations


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
