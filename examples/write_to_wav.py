from __future__ import annotations

import argparse
import time
import wave
from pathlib import Path

import numpy as np

import macloop


def open_wav_writer(path: Path, sample_rate: int, channels: int) -> wave.Wave_write:
    path.parent.mkdir(parents=True, exist_ok=True)
    wf = wave.open(str(path), "wb")
    wf.setnchannels(channels)
    wf.setsampwidth(2)  # int16
    wf.setframerate(sample_rate)
    return wf


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Capture display system audio and microphone into separate WAV files (sync API)."
    )
    parser.add_argument("--display-id", type=int, default=1, help="Display ID to capture system audio from.")
    parser.add_argument("--seconds", type=float, default=8.0, help="How long to capture.")
    parser.add_argument("--sample-rate", type=int, default=48_000, help="Output sample rate.")
    parser.add_argument("--channels", type=int, default=1, choices=[1, 2], help="Output channel count.")
    parser.add_argument("--system-out", default="out/display1_system.wav", help="Output WAV for system audio.")
    parser.add_argument("--mic-out", default="out/display1_mic.wav", help="Output WAV for microphone audio.")
    args = parser.parse_args()

    sources = macloop.list_audio_sources()
    display_ids = {s["display_id"] for s in sources if s.get("type") == "display" and "display_id" in s}
    if args.display_id not in display_ids:
        raise RuntimeError(
            f"Display {args.display_id} not found. Available display IDs: {sorted(display_ids)}"
        )

    cfg = macloop.AudioProcessingConfig(
        sample_rate=args.sample_rate,
        channels=args.channels,
        sample_format="i16",
        enable_aec=False,
        enable_ns=False,
    )

    sys_count = 0
    mic_count = 0

    system_out = Path(args.system_out)
    mic_out = Path(args.mic_out)

    start = time.monotonic()
    with (
        open_wav_writer(system_out, args.sample_rate, args.channels) as system_wf,
        open_wav_writer(mic_out, args.sample_rate, args.channels) as mic_wf,
        macloop.Capture(
            display_id=args.display_id,
            config=cfg,
            capture_system=True,
            capture_mic=True,
        ) as stream,
    ):
        for chunk in stream:
            arr = np.asarray(chunk.samples, dtype=np.int16)
            if arr.size == 0:
                if time.monotonic() - start >= args.seconds:
                    break
                continue

            if chunk.source == "system":
                system_wf.writeframes(arr.tobytes())
                sys_count += 1
            elif chunk.source == "mic":
                mic_wf.writeframes(arr.tobytes())
                mic_count += 1

            if time.monotonic() - start >= args.seconds:
                break

    print(f"Done. system_chunks={sys_count}, mic_chunks={mic_count}")
    print(f"System WAV: {system_out.resolve()}")
    print(f"Mic WAV:    {mic_out.resolve()}")


if __name__ == "__main__":
    main()
