from __future__ import annotations

import argparse
import asyncio
import time

try:
    import macloop
except ModuleNotFoundError:
    from _bootstrap import bootstrap_repo_root

    bootstrap_repo_root()
    import macloop


def _ts() -> str:
    return time.strftime("%H:%M:%S")


async def main() -> None:
    parser = argparse.ArgumentParser(
        description=(
            "Manual smoke test: capture system audio into AsrSink via chunks_async(). "
            "Prints progress markers so it is easy to see whether creation hangs."
        )
    )
    parser.add_argument(
        "--display-id",
        type=int,
        default=None,
        help="Optional display id. If omitted, the first display is used.",
    )
    parser.add_argument("--list-displays", action="store_true", help="List displays and exit.")
    parser.add_argument("--chunk-frames", type=int, default=320, help="Frames per ASR chunk.")
    parser.add_argument("--sample-rate", type=int, default=16_000, help="ASR output sample rate.")
    parser.add_argument("--channels", type=int, default=1, choices=(1, 2), help="ASR output channels.")
    parser.add_argument(
        "--sample-format",
        default="f32",
        choices=("f32", "i16"),
        help="ASR output sample format.",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=8.0,
        help="Seconds to wait for the first chunk before failing.",
    )
    parser.add_argument(
        "--max-chunks",
        type=int,
        default=3,
        help="How many chunks to print before exiting successfully.",
    )
    args = parser.parse_args()

    if args.list_displays:
        for index, display in enumerate(macloop.SystemAudioSource.list_displays()):
            default = " (default)" if display["is_default"] else ""
            print(
                f'index={index} id={display["id"]} {display["name"]} '
                f'{display["width"]}x{display["height"]}{default}'
            )
        return

    print(f"[{_ts()}] creating engine")
    with macloop.AudioEngine() as engine:
        print(f"[{_ts()}] creating system stream display_id={args.display_id!r}")
        system_stream = engine.create_stream(
            macloop.SystemAudioSource,
            display_id=args.display_id,
        )
        route = engine.route("system_async_asr", stream=system_stream)
        print(f"[{_ts()}] route ready route_id={route.id}")

        print(f"[{_ts()}] creating AsrSink")
        with macloop.AsrSink(
            routes=[route],
            chunk_frames=args.chunk_frames,
            sample_rate=args.sample_rate,
            channels=args.channels,
            sample_format=args.sample_format,
        ) as asr_sink:
            print(f"[{_ts()}] AsrSink created")

            chunk_iter = asr_sink.chunks_async()
            for index in range(args.max_chunks):
                chunk = await asyncio.wait_for(anext(chunk_iter), timeout=args.timeout)
                peak = float(abs(chunk.samples).max()) if len(chunk.samples) else 0.0
                print(
                    f"[{_ts()}] chunk#{index + 1} route={chunk.route_id} "
                    f"frames={chunk.frames} samples={len(chunk.samples)} "
                    f"dtype={chunk.samples.dtype} peak={peak:.5f}"
                )

            stats = asr_sink.stats()[route.id]
            print(
                f"[{_ts()}] stats chunks_emitted={stats.chunks_emitted} "
                f"frames_emitted={stats.frames_emitted} pending_frames={stats.pending_frames}"
            )

    print(f"[{_ts()}] done")


if __name__ == "__main__":
    asyncio.run(main())
