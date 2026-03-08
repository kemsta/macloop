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
    parser = argparse.ArgumentParser(
        description="Manual smoke test: capture the default microphone into a WAV file."
    )
    parser.add_argument("--seconds", type=float, default=5.0, help="How long to record.")
    parser.add_argument(
        "--device-id",
        type=int,
        default=None,
        help="Optional microphone device id. If set, VPIO is disabled automatically.",
    )
    parser.add_argument("--output", default="out/smoke_mic.wav", help="Output WAV path.")
    parser.add_argument("--list-devices", action="store_true", help="List microphones and exit.")
    args = parser.parse_args()

    if args.list_devices:
        for mic in macloop.MicrophoneSource.list_devices():
            default = " (default)" if mic["is_default"] else ""
            print(f'{mic["id"]} {mic["name"]}{default}')
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
        route = engine.route("mic_smoke_wav", stream=mic)
        wav_sink = macloop.WavSink(route=route, file=output)

        print(
            f"Recording microphone to {output.resolve()} for {args.seconds:.1f}s "
            f"(vpio_enabled={vpio_enabled}, device_id={args.device_id})"
        )
        time.sleep(args.seconds)
        wav_sink.close()

    print(f"Done: {output.resolve()}")


if __name__ == "__main__":
    main()
