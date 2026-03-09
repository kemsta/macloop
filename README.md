# macloop

[![CI](https://github.com/kemsta/macloop/actions/workflows/publish.yml/badge.svg?branch=main)](https://github.com/kemsta/macloop/actions/workflows/publish.yml)
[![PyPI](https://img.shields.io/pypi/v/macloop.svg)](https://pypi.org/project/macloop/)
[![TestPyPI](https://img.shields.io/badge/TestPyPI-macloop-blue)](https://test.pypi.org/project/macloop/)
[![codecov](https://codecov.io/gh/kemsta/macloop/branch/main/graph/badge.svg)](https://codecov.io/gh/kemsta/macloop)
[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)

> 🎙️ Build programmable macOS audio pipelines in Python without routing your whole machine through a virtual driver.

`macloop` is a Python-first audio capture toolkit backed by a real-time Rust engine. It lets you capture microphones, system audio, or individual applications, route one stream into multiple consumers, apply processors, and feed the results into sinks such as ASR and WAV recording, all from one in-process API.

---

## ✨ Why `macloop`?

Virtual devices such as BlackHole are useful when you need a system-wide loopback device. `macloop` targets a different workflow: **programmable capture pipelines inside an application**.

### `macloop` vs virtual-driver workflows

| Capability | `macloop` | BlackHole-style virtual driver |
| --- | --- | --- |
| Capture microphone audio | ✅ | ✅ |
| Capture system audio | ✅ | ✅ |
| Capture a single app (for example Zoom) | ✅ | ❌ typically not directly |
| Route one stream into several consumers | ✅ | ❌ external wiring needed |
| Run processors in the capture pipeline | ✅ | ❌ outside the driver |
| Voice-processed microphone path | ✅ via `vpio_enabled=True` | ❌ not provided by the driver itself |
| Noise suppression / echo cancellation as part of the pipeline | ⚠️ pipeline-ready, but not exposed as built-in public nodes yet | ❌ external tooling required |
| Feed Python ASR chunks directly | ✅ | ❌ extra bridge required |
| Record and transcribe the same meeting at once | ✅ | ⚠️ possible, but usually with extra routing glue |
| Requires changing your default output device | ❌ | often ✅ |
| Requires a virtual audio device to be installed | ❌ | ✅ |

**Why this matters:** if your goal is “capture, transform, split, and consume audio in Python”, `macloop` removes a lot of the manual patch-bay work.

---

## 🧱 Tech Stack

| Layer | Technology |
| --- | --- |
| Public API | Python |
| Native bindings | PyO3 |
| Audio engine | Rust |
| macOS capture backends | CoreAudio, ScreenCaptureKit |
| Array transport to Python | NumPy |

---

## 🧩 What You Can Build

`macloop` is designed as a modular pipeline:

```text
Source -> Processor(s) -> Route(s) -> Sink(s)
```

Examples:

- Record a meeting to WAV while streaming microphone chunks to an ASR engine.
- Capture only Zoom audio instead of the entire system mix.
- Split one microphone stream into separate routes for transcription, monitoring, and archival recording.
- Build deterministic tests with a synthetic source before touching real devices.

### Current building blocks

| Category | Available today |
| --- | --- |
| Sources | `MicrophoneSource`, `SystemAudioSource`, `AppAudioSource`, `SyntheticSource` |
| Processors | `GainProcessor` |
| Sinks | `AsrSink`, `WavSink` |
| ASR delivery | sync iteration and `asyncio` iteration |
| Output formats | `AsrSink`: `f32` / `i16`, mono or stereo |
| Metrics | `engine.stats()`, `asr_sink.stats()`, `wav_sink.stats()` |

---

## 🚀 Installation

### 1. Create a virtual environment

```bash
python -m venv .venv
source .venv/bin/activate
```

### 2. Upgrade packaging tools

```bash
python -m pip install --upgrade pip
```

### 3. Install `macloop`

```bash
pip install macloop
```

### Requirements

- macOS
- Python 3.9+

---

## ▶️ Quick Start

The example below creates a small audio graph:

- capture the microphone
- apply a gain processor
- split the stream into two routes
- record one route to WAV
- send the other route to an ASR sink

```python
import macloop


with macloop.AudioEngine() as engine:
    mic = engine.create_stream(
        macloop.MicrophoneSource,
        device_id=None,
        vpio_enabled=True,
    )

    engine.add_processor(
        stream=mic,
        processor=macloop.GainProcessor(gain=1.2),
    )

    mic_for_asr = engine.route("mic_for_asr", stream=mic)
    mic_for_wav = engine.route("mic_for_wav", stream=mic)

    wav_sink = macloop.WavSink(route=mic_for_wav, file="out/mic.wav")
    asr_sink = macloop.AsrSink(
        routes=[mic_for_asr],
        chunk_frames=320,
        sample_rate=16_000,
        channels=1,
        sample_format="f32",
    )

    for chunk in asr_sink.chunks():
        print(chunk.route_id, chunk.frames, chunk.samples.dtype)
        break

    asr_sink.close()
    wav_sink.close()
```

`AudioChunk.samples` is a NumPy array:

- `np.float32` for `sample_format="f32"`
- `np.int16` for `sample_format="i16"`

---

## 🎧 Real Example: Record And Transcribe A Meeting

This is the workflow `macloop` is built for: **one pipeline, multiple outputs**.

```python
import macloop


def find_zoom_pids() -> list[int]:
    pids = []
    for app in macloop.AppAudioSource.list_applications():
        if "zoom" in app["name"].lower():
            pids.append(int(app["pid"]))
    if not pids:
        raise RuntimeError("Zoom is not running")
    return pids


with macloop.AudioEngine() as engine:
    mic = engine.create_stream(macloop.MicrophoneSource, vpio_enabled=True)
    zoom = engine.create_stream(macloop.AppAudioSource, pids=find_zoom_pids())

    mic_for_asr = engine.route("mic_for_asr", stream=mic)
    zoom_for_asr = engine.route("zoom_for_asr", stream=zoom)
    mic_for_wav = engine.route("mic_for_wav", stream=mic)
    zoom_for_wav = engine.route("zoom_for_wav", stream=zoom)

    wav_sink = macloop.WavSink(
        routes=[mic_for_wav, zoom_for_wav],
        file="out/meeting.wav",
    )

    asr_sink = macloop.AsrSink(
        routes=[mic_for_asr, zoom_for_asr],
        chunk_frames=320,
        sample_rate=16_000,
        channels=1,
        sample_format="f32",
    )

    # Long-running pipeline: keep consuming until your app decides to stop.
    for chunk in asr_sink.chunks():
        print(chunk.route_id, chunk.frames)
        # Send chunk.samples into your ASR engine here.
```

Notes:

- `AsrSink` emits **independent chunks per route**.
- `WavSink` can mix several routes into one file.
- If `mix_gain` is not provided, `WavSink` uses `1 / N` by default.

---

## ⚡ Asyncio

`AsrSink` also supports async consumption:

```python
import asyncio
import macloop


async def main() -> None:
    with macloop.AudioEngine() as engine:
        mic = engine.create_stream(macloop.MicrophoneSource, vpio_enabled=True)
        mic_for_asr = engine.route(stream=mic)

        with macloop.AsrSink(
            routes=[mic_for_asr],
            chunk_frames=320,
            sample_rate=16_000,
            channels=1,
            sample_format="f32",
        ) as asr_sink:
            async for chunk in asr_sink.chunks_async():
                print(chunk.route_id, chunk.frames)
                break


asyncio.run(main())
```

---

## 🔎 Device Discovery

### Microphones

```python
import macloop

for mic in macloop.MicrophoneSource.list_devices():
    print(mic["id"], mic["name"], mic["is_default"])
```

### Displays

```python
import macloop

for display in macloop.SystemAudioSource.list_displays():
    print(display["id"], display["name"], display["width"], display["height"])
```

### Applications

```python
import macloop

for app in macloop.AppAudioSource.list_applications():
    print(app["pid"], app["name"], app["bundle_id"])
```

If `engine.create_stream(macloop.SystemAudioSource, ...)` is called without an explicit `display_id`, `macloop` uses the first available display.

`engine.create_stream(macloop.AppAudioSource, ...)` requires explicit `pids`. Use `AppAudioSource.list_applications()` to choose one or more target applications first.

---

## 🛠️ Example Scripts

The scripts below live in this repository, so run them from a source checkout.

### Record microphone audio to WAV

```bash
python examples/write_to_wav.py --seconds 5 --output out/mic.wav
```

### Stream microphone audio into Sherpa ONNX

```bash
uv run --with sherpa-onnx --with huggingface_hub --reinstall-package macloop \
  python examples/sherpa_asr_demo.py --seconds 5
```

---

## 📊 Telemetry

`macloop` exposes metrics at different levels of the pipeline:

- `engine.stats()` for per-stream real-time pipeline and processor metrics
- `asr_sink.stats()` for per-route ASR sink metrics
- `wav_sink.stats()` for WAV writer metrics

This makes it possible to inspect latency and drops at the node level instead of relying only on a single average number.

---

## 🗺️ Roadmap

- [ ] Add more built-in processors beyond `GainProcessor`
- [ ] Add zero-copy / lease-release delivery for Python
- [ ] Add richer pipeline examples for meeting bots and voice agents
- [ ] Add WebRTC AEC in a future iteration, with a routing model that can handle capture and reference streams cleanly

---

## 📄 License

MIT
