import asyncio
import importlib
import sys
import types

import pytest


class _FakeAudioProcessingConfig:
    def __init__(self, **kwargs):
        self.__dict__.update(kwargs)

    def calibrate_delay(self, measured_system_latency_ms: float, measured_mic_latency_ms: float) -> None:
        self.aec_stream_delay_ms = int(measured_system_latency_ms - measured_mic_latency_ms)


class _FakeAudioEngine:
    def __init__(self, display_id=None, pid=None, config=None):
        self.display_id = display_id
        self.pid = pid
        self.config = config
        self.started = False
        self._stats = {"ok": True}

    def start(self, callback, capture_system=True, capture_mic=False):
        self.started = True
        # Emit a few chunks immediately to exercise queue/iterator paths.
        callback("system", b"s0")
        callback("mic", b"m1")
        callback("system", b"s2")

    def stop(self):
        self.started = False

    def get_stats(self):
        return self._stats


@pytest.fixture
def macloop_module(monkeypatch):
    fake_ext = types.ModuleType("macloop._macloop")
    fake_ext.AudioEngine = _FakeAudioEngine
    fake_ext.AudioProcessingConfig = _FakeAudioProcessingConfig
    fake_ext.list_audio_sources = lambda: [
        {"type": "app", "name": "Music", "pid": 42, "bundle_id": "com.apple.Music"},
        {"type": "display", "name": "Display 1", "display_id": 1},
    ]

    monkeypatch.setitem(sys.modules, "macloop._macloop", fake_ext)
    if "macloop" in sys.modules:
        del sys.modules["macloop"]
    mod = importlib.import_module("macloop")
    return importlib.reload(mod)


def test_public_api_surface(macloop_module):
    assert "Capture" in macloop_module.__all__
    assert "AudioProcessingConfig" in macloop_module.__all__
    assert "AudioEngine" not in macloop_module.__all__


def test_list_audio_sources_passthrough(macloop_module):
    sources = macloop_module.list_audio_sources()
    assert len(sources) == 2
    assert {s["type"] for s in sources} == {"app", "display"}


def test_capture_sync_iterator(macloop_module):
    with macloop_module.Capture(display_id=1, capture_system=True, capture_mic=True) as stream:
        first = next(stream)
        second = next(stream)
        assert first.source == "system"
        assert second.source == "mic"


def test_capture_max_queue_drops_oldest(macloop_module):
    with macloop_module.Capture(display_id=1, max_queue_size=1) as stream:
        # Fake engine emits: s0, m1, s2. With queue size 1 only the latest survives.
        latest = next(stream)
        assert latest.source == "system"
        assert latest.samples == b"s2"


def test_capture_get_stats(macloop_module):
    with macloop_module.Capture(display_id=1) as stream:
        stats = stream.get_stats()
        assert stats == {"ok": True}


@pytest.mark.asyncio
async def test_capture_async_iterator(macloop_module):
    async with macloop_module.Capture(display_id=1, capture_system=True, capture_mic=True) as stream:
        first = await stream.__anext__()
        second = await stream.__anext__()
        assert first.source == "system"
        assert second.source == "mic"
