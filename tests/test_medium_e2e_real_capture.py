from __future__ import annotations

import math
import shutil
import struct
import subprocess
import threading
import time
import wave
from pathlib import Path
from typing import Callable

import pytest

pytest.importorskip("numpy")
pytest.importorskip("macloop._macloop")

import numpy as np

import macloop

pytestmark = [
    pytest.mark.medium,
    pytest.mark.skipif(
        shutil.which("afplay") is None,
        reason="medium real-capture tests require /usr/bin/afplay",
    ),
    pytest.mark.usefixtures("require_medium_run"),
]

_CLOSE_TIMEOUT_S = 5.0
_PLAYBACK_DURATION_S = 3.0
_EXPECTED_TONE_HZ = 440.0


def _write_stereo_tone_wav(
    path: Path,
    *,
    frequency_hz: float = _EXPECTED_TONE_HZ,
    duration_s: float = _PLAYBACK_DURATION_S,
    sample_rate: int = 48_000,
    amplitude: float = 0.35,
) -> None:
    frame_count = int(sample_rate * duration_s)
    path.parent.mkdir(parents=True, exist_ok=True)

    with wave.open(str(path), "wb") as wav_file:
        wav_file.setnchannels(2)
        wav_file.setsampwidth(2)
        wav_file.setframerate(sample_rate)

        frames = bytearray()
        for index in range(frame_count):
            sample = amplitude * math.sin(2.0 * math.pi * frequency_hz * index / sample_rate)
            pcm = max(-32767, min(32767, int(sample * 32767)))
            frames.extend(struct.pack("<hh", pcm, pcm))

        wav_file.writeframes(bytes(frames))


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


def _invoke_with_timeout(fn: Callable[[], None], *, timeout_s: float, label: str) -> float:
    started_at = time.monotonic()
    fn()
    elapsed = time.monotonic() - started_at
    if elapsed > timeout_s:
        pytest.fail(f"{label} exceeded {timeout_s:.1f}s (took {elapsed:.3f}s)")
    return elapsed


def _spawn_afplay(path: Path) -> subprocess.Popen[bytes]:
    return subprocess.Popen(["/usr/bin/afplay", str(path)])


def _stop_process(proc: subprocess.Popen[bytes]) -> None:
    if proc.poll() is not None:
        return

    proc.terminate()
    try:
        proc.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=2.0)


def _wait_for_pid_in_app_capture_list(pid: int, *, timeout_s: float = 5.0) -> bool:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        apps = macloop.AppAudioSource.list_applications()
        if any(int(app["pid"]) == pid for app in apps):
            return True
        time.sleep(0.1)
    return False


def _assert_captured_tone_matches(path: Path, *, expected_hz: float = _EXPECTED_TONE_HZ) -> None:
    fmt_info, samples = _read_float_wav(path)
    assert fmt_info[0] in {3, 65534}
    assert fmt_info[1:] == (2, 48_000)
    assert samples, "expected captured WAV to contain sample data"

    interleaved = np.asarray(samples, dtype=np.float32)
    assert interleaved.size >= 2_048, "captured WAV is unexpectedly short"

    stereo = interleaved.reshape(-1, 2)
    mono = stereo.mean(axis=1)

    non_silent = np.flatnonzero(np.abs(mono) > 0.01)
    assert non_silent.size > 0, "captured WAV contains only silence"

    mono = mono[non_silent[0] : non_silent[-1] + 1]
    segment = mono[: min(len(mono), 48_000)]
    assert segment.size >= 4_096, "captured WAV does not contain enough signal to analyze"

    window = np.hanning(segment.size)
    spectrum = np.fft.rfft(segment * window)
    freqs = np.fft.rfftfreq(segment.size, d=1.0 / 48_000.0)
    magnitudes = np.abs(spectrum)
    magnitudes[0] = 0.0
    peak_hz = float(freqs[int(np.argmax(magnitudes))])

    assert abs(peak_hz - expected_hz) <= 35.0, (
        f"captured dominant frequency {peak_hz:.1f}Hz did not match expected tone "
        f"near {expected_hz:.1f}Hz"
    )


def _assert_finalized_wav(path: Path) -> None:
    data = path.read_bytes()
    assert data[:4] == b"RIFF"
    assert data[8:12] == b"WAVE"
    assert len(data) >= 44, "expected finalized WAV header"


def _run_system_close_once(output_path: Path, fixture_path: Path) -> tuple[macloop.WavSink, float]:
    engine = macloop.AudioEngine()
    player = None
    sink = None
    close_attempted = False
    try:
        stream = engine.create_stream(macloop.SystemAudioSource)
        route = engine.route("system_route", stream=stream)
        sink = macloop.WavSink(route=route, file=output_path)

        player = _spawn_afplay(fixture_path)
        time.sleep(0.75)

        close_attempted = True
        elapsed = _invoke_with_timeout(
            engine.close,
            timeout_s=_CLOSE_TIMEOUT_S,
            label="system engine.close()",
        )

        return sink, elapsed
    finally:
        if player is not None:
            _stop_process(player)
        if not close_attempted:
            if sink is not None:
                sink.close()
            engine.close()


def test_medium_system_audio_engine_close_completes_within_timeout(tmp_path: Path) -> None:
    fixture_path = tmp_path / "system_fixture.wav"
    output_path = tmp_path / "system_capture.wav"
    _write_stereo_tone_wav(fixture_path)

    sink, elapsed = _run_system_close_once(output_path, fixture_path)

    assert elapsed < _CLOSE_TIMEOUT_S
    stats = sink.stats()
    assert stats.samples_written > 0
    assert stats.frames_written > 0
    _assert_finalized_wav(output_path)
    _assert_captured_tone_matches(output_path)


def test_medium_application_audio_engine_close_completes_within_timeout(tmp_path: Path) -> None:
    fixture_path = tmp_path / "app_fixture.wav"
    output_path = tmp_path / "app_capture.wav"
    _write_stereo_tone_wav(fixture_path, duration_s=4.0)

    player = _spawn_afplay(fixture_path)
    try:
        if not _wait_for_pid_in_app_capture_list(player.pid):
            pytest.skip(
                f"playback process pid {player.pid} is not visible to AppAudioSource.list_applications()"
            )

        engine = macloop.AudioEngine()
        sink = None
        close_attempted = False
        try:
            stream = engine.create_stream(macloop.AppAudioSource, pids=[player.pid])
            route = engine.route("app_route", stream=stream)
            sink = macloop.WavSink(route=route, file=output_path)
            time.sleep(0.75)

            close_attempted = True
            elapsed = _invoke_with_timeout(
                engine.close,
                timeout_s=_CLOSE_TIMEOUT_S,
                label="application engine.close()",
            )

            assert elapsed < _CLOSE_TIMEOUT_S
            stats = sink.stats()
            assert stats.samples_written > 0
            assert stats.frames_written > 0
            _assert_finalized_wav(output_path)
            _assert_captured_tone_matches(output_path)
        finally:
            if not close_attempted:
                if sink is not None:
                    sink.close()
                engine.close()
    finally:
        _stop_process(player)


def test_medium_microphone_engine_close_completes_within_timeout(tmp_path: Path) -> None:
    output_path = tmp_path / "microphone_capture.wav"

    engine = macloop.AudioEngine()
    sink = None
    close_attempted = False
    try:
        stream = engine.create_stream(macloop.MicrophoneSource)
        route = engine.route("microphone_route", stream=stream)
        sink = macloop.WavSink(route=route, file=output_path)
        time.sleep(0.5)

        close_attempted = True
        elapsed = _invoke_with_timeout(
            engine.close,
            timeout_s=_CLOSE_TIMEOUT_S,
            label="microphone engine.close()",
        )

        assert elapsed < _CLOSE_TIMEOUT_S
        _assert_finalized_wav(output_path)
        sink.close()
        engine.close()
    finally:
        if not close_attempted:
            if sink is not None:
                sink.close()
            engine.close()


def test_medium_system_audio_engine_close_remains_stable_across_repeated_runs(
    tmp_path: Path,
) -> None:
    fixture_path = tmp_path / "repeated_system_fixture.wav"
    _write_stereo_tone_wav(fixture_path, duration_s=2.0)

    elapsed_values = []
    for index in range(3):
        output_path = tmp_path / f"repeated_system_capture_{index}.wav"
        sink, elapsed = _run_system_close_once(output_path, fixture_path)
        elapsed_values.append(elapsed)
        assert sink.stats().samples_written > 0
        _assert_finalized_wav(output_path)

    assert len(elapsed_values) == 3
    assert max(elapsed_values) < _CLOSE_TIMEOUT_S
