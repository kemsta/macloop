from __future__ import annotations

import argparse
import time
from pathlib import Path

try:
    import macloop
except ModuleNotFoundError:
    from _bootstrap import bootstrap_repo_root

    bootstrap_repo_root()
    import macloop


def main() -> None:
    parser = argparse.ArgumentParser(description="Capture microphone audio into a WAV file.")
    parser.add_argument("--seconds", type=float, default=5.0, help="How long to record.")
    parser.add_argument("--device-id", type=int, default=None, help="Optional microphone device id.")
    parser.add_argument("--output", default="out/mic.wav", help="Output WAV path.")
    parser.add_argument("--list-mics", action="store_true", help="List available microphones and exit.")
    args = parser.parse_args()

    if args.list_mics:
        for mic in macloop.MicrophoneSource.list_devices():
            print(mic["id"], mic["name"], "(default)" if mic["is_default"] else "")
        return

    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    vpio_enabled = args.device_id is None

    with macloop.AudioEngine() as engine:
        mic = engine.create_stream(
            macloop.MicrophoneSource,
            device_id=args.device_id,
            vpio_enabled=vpio_enabled,
        )
        mic_for_wav = engine.route(stream=mic)
        wav_sink = macloop.WavSink(route=mic_for_wav, file=output)

        print(f"Recording microphone to {output.resolve()} for {args.seconds:.1f}s...")
        time.sleep(args.seconds)
        wav_sink.close()

    print(f"Done: {output.resolve()}")


if __name__ == "__main__":
    main()
