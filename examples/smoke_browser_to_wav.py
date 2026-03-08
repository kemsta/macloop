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

DEFAULT_MATCH_TERMS = ("chrome",)


def find_matching_apps(match_terms: list[str]) -> list[dict[str, object]]:
    normalized_terms = [term.lower() for term in match_terms]
    matched = []
    for app in macloop.AppAudioSource.list_applications():
        haystack = f'{app["name"]} {app["bundle_id"]}'.lower()
        if any(term in haystack for term in normalized_terms):
            matched.append(app)
    return matched


def find_apps_by_pid(pids: list[int]) -> list[dict[str, object]]:
    by_pid = {int(app["pid"]): app for app in macloop.AppAudioSource.list_applications()}
    matched = []
    missing = []
    for pid in pids:
        app = by_pid.get(pid)
        if app is None:
            missing.append(str(pid))
        else:
            matched.append(app)

    if missing:
        joined = ", ".join(missing)
        raise RuntimeError(f"Application pid(s) not found: {joined}. Run with --list-apps first.")

    return matched


def main() -> None:
    parser = argparse.ArgumentParser(
        description=(
            "Manual smoke test: capture browser application audio into a WAV file. "
            "All matching apps are captured and mixed into one output."
        )
    )
    parser.add_argument("--seconds", type=float, default=5.0, help="How long to record.")
    parser.add_argument("--output", default="out/smoke_browser.wav", help="Output WAV path.")
    parser.add_argument(
        "--match",
        action="append",
        default=[],
        help=(
            "Substring to match against application name or bundle id. "
            "Can be passed multiple times. Defaults to 'chrome'."
        ),
    )
    parser.add_argument(
        "--pid",
        action="append",
        type=int,
        default=[],
        help=(
            "Capture an exact application pid. Can be passed multiple times. "
            "If provided, --match is ignored."
        ),
    )
    parser.add_argument("--list-apps", action="store_true", help="List applications and exit.")
    args = parser.parse_args()

    if args.list_apps:
        for app in macloop.AppAudioSource.list_applications():
            default = " (default)" if app["is_default"] else ""
            print(f'{app["pid"]} {app["name"]} {app["bundle_id"]}{default}')
        return

    if args.pid:
        matched_apps = find_apps_by_pid(args.pid)
    else:
        match_terms = args.match or list(DEFAULT_MATCH_TERMS)
        matched_apps = find_matching_apps(match_terms)

    if not matched_apps:
        joined = ", ".join(args.match or list(DEFAULT_MATCH_TERMS))
        raise RuntimeError(
            f"No applications matched: {joined}. Run with --list-apps to inspect candidates."
        )

    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)

    with macloop.AudioEngine() as engine:
        routes = []
        print("Capturing application audio from:")
        for app in matched_apps:
            pid = int(app["pid"])
            print(f'  pid={pid} name={app["name"]} bundle_id={app["bundle_id"]}')
            stream = engine.create_stream(
                macloop.AppAudioSource,
                f"app_{pid}",
                pid=pid,
            )
            routes.append(engine.route(f"app_route_{pid}", stream=stream))

        wav_sink = macloop.WavSink(routes=routes, file=output)
        print(
            f"Recording browser audio to {output.resolve()} for {args.seconds:.1f}s "
            f"from {len(routes)} stream(s)"
        )
        time.sleep(args.seconds)
        wav_sink.close()

    print(f"Done: {output.resolve()}")


if __name__ == "__main__":
    main()
