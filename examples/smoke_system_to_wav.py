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
        description="Manual smoke test: capture system audio into a WAV file."
    )
    parser.add_argument("--seconds", type=float, default=5.0, help="How long to record.")
    parser.add_argument(
        "--display-id",
        type=int,
        default=None,
        help="Optional display id. If omitted, the first display is used.",
    )
    parser.add_argument("--output", default="out/smoke_system.wav", help="Output WAV path.")
    parser.add_argument("--list-displays", action="store_true", help="List displays and exit.")
    args = parser.parse_args()

    if args.list_displays:
        for display in macloop.SystemAudioSource.list_displays():
            default = " (default)" if display["is_default"] else ""
            print(
                f'{display["id"]} {display["name"]} '
                f'{display["width"]}x{display["height"]}{default}'
            )
        return

    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)

    with macloop.AudioEngine() as engine:
        system_stream = engine.create_stream(
            macloop.SystemAudioSource,
            display_id=args.display_id,
        )
        route = engine.route("system_smoke_wav", stream=system_stream)
        wav_sink = macloop.WavSink(route=route, file=output)

        print(f"Recording system audio to {output.resolve()} for {args.seconds:.1f}s")
        time.sleep(args.seconds)
        wav_sink.close()

    print(f"Done: {output.resolve()}")


if __name__ == "__main__":
    main()
