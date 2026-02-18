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

## Notes

- One capture target per stream: either `display_id` or `pid`.
- For app capture use `pid=...`.

## License

MIT
