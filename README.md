# macloop

Low-latency macOS audio capture and processing for Python, powered by Rust.

- System audio and microphone capture via ScreenCaptureKit
- WebRTC-based AEC (echo cancellation) and NS (noise suppression)
- Sync and async streaming APIs

## Requirements

- macOS (Apple Silicon or Intel)
- Python 3.9+

## Installation

```bash
pip install macloop
```

## Quick Start (sync)

```python
import macloop

sources = macloop.list_audio_sources()
display = next(s for s in sources if s["type"] == "display")

cfg = macloop.AudioProcessingConfig(
    sample_rate=16000,
    channels=1,
    sample_format="i16",
    enable_aec=True,
    enable_ns=False,
)

with macloop.Capture(
    display_id=display["display_id"],
    config=cfg,
    capture_system=True,
    capture_mic=True,
) as stream:
    for chunk in stream:
        print(chunk.source, len(chunk.samples))
        # chunk.samples is numpy.ndarray (int16 or float32)
```

## Quick Start (asyncio)

```python
import asyncio
import macloop

async def main():
    sources = macloop.list_audio_sources()
    display = next(s for s in sources if s["type"] == "display")
    cfg = macloop.AudioProcessingConfig()

    async with macloop.Capture(
        display_id=display["display_id"],
        config=cfg,
        capture_system=True,
        capture_mic=True,
    ) as stream:
        async for chunk in stream:
            print(chunk.source, len(chunk.samples))

asyncio.run(main())
```

## Data Format

`AudioChunk.samples` is a `numpy.ndarray`:

- `np.int16` when `sample_format="i16"`
- `np.float32` when `sample_format="f32"`

This makes it convenient to pass chunks directly to model inference pipelines
without extra conversion.

```python
# Example: direct handoff to inference-friendly float32
import numpy as np
import macloop

sources = macloop.list_audio_sources()
display = next(s for s in sources if s["type"] == "display")
cfg = macloop.AudioProcessingConfig(sample_format="f32", sample_rate=16000, channels=1)

with macloop.Capture(display_id=display["display_id"], config=cfg) as stream:
    for chunk in stream:
        x = chunk.samples.astype(np.float32, copy=False)
        # model(x)
```

## Sherpa ASR Demo

For speech-to-text (ASR), use this short example:

```bash
uv run --with sherpa-onnx --with huggingface_hub --with macloop \
  python examples/sherpa_asr_demo.py --seconds 5
```

It captures microphone audio with `macloop`, downloads a Sherpa model from Hugging Face
(or uses `--model-dir`), and prints transcript text.

## Notes

- One capture target per stream: either `display_id` or `pid`.
- For app capture use `pid=...`.

## License

MIT
