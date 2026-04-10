#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys
from typing import Iterable


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Resolve Swift runtime paths for the active toolchain and export them for CI."
    )
    parser.add_argument("--rust-target", help="Rust target triple, e.g. aarch64-apple-darwin")
    parser.add_argument(
        "--deployment-target",
        help="macOS deployment target, e.g. 11.0. Required with --rust-target.",
    )
    parser.add_argument(
        "--github-env",
        help="Path to $GITHUB_ENV. If set, resolved variables are appended to this file.",
    )
    return parser.parse_args()


def rust_target_to_swift_target(rust_target: str, deployment_target: str) -> str:
    mapping = {
        "aarch64-apple-darwin": "arm64-apple-macosx{deployment}",
        "x86_64-apple-darwin": "x86_64-apple-macosx{deployment}",
    }
    template = mapping.get(rust_target)
    if template is None:
        raise SystemExit(f"Unsupported Rust target for Swift runtime resolution: {rust_target}")
    return template.format(deployment=deployment_target)


def run_swift_target_info(swift_target: str | None) -> dict:
    cmd = ["swiftc"]
    if swift_target:
        cmd.extend(["-target", swift_target])
    cmd.append("-print-target-info")
    output = subprocess.check_output(cmd, text=True)
    print(output.rstrip())
    return json.loads(output)


def version_key(path: pathlib.Path) -> tuple:
    match = re.search(r"swift-(.+?)(?:/|$)", str(path))
    if not match:
        return ()
    parts = []
    for piece in re.split(r"[.-]", match.group(1)):
        try:
            parts.append((0, int(piece)))
        except ValueError:
            parts.append((1, piece))
    return tuple(parts)


def dedupe_paths(paths: Iterable[str]) -> list[str]:
    result: list[str] = []
    seen: set[str] = set()
    for raw in paths:
        if not raw:
            continue
        normalized = str(pathlib.Path(raw))
        if normalized in seen:
            continue
        seen.add(normalized)
        result.append(normalized)
    return result


def collect_search_paths(target_info: dict) -> tuple[list[str], str]:
    paths = target_info.get("paths", {})
    runtime_library_paths = [p for p in paths.get("runtimeLibraryPaths", []) if p]
    runtime_resource_path = paths.get("runtimeResourcePath", "") or ""

    search_paths = list(runtime_library_paths)
    if runtime_resource_path:
        search_paths.append(runtime_resource_path)
        parent = pathlib.Path(runtime_resource_path).parent
        if parent.exists():
            versioned = [
                str(child)
                for child in parent.glob("swift*/macosx")
                if (child / "libswiftCore.dylib").exists()
            ]
            search_paths.extend(sorted(versioned, key=lambda p: version_key(pathlib.Path(p)), reverse=True))

    return dedupe_paths(search_paths), runtime_resource_path


def resolve_stdlib_dir(search_paths: Iterable[str]) -> str:
    for candidate in search_paths:
        if (pathlib.Path(candidate) / "libswiftCore.dylib").is_file():
            return candidate
    raise SystemExit("swiftc target info did not yield a directory containing libswiftCore.dylib")


def append_github_env(path: str, values: dict[str, str]) -> None:
    with open(path, "a", encoding="utf-8") as fh:
        for key, value in values.items():
            fh.write(f"{key}={value}\n")


def main() -> int:
    args = parse_args()

    swift_target = None
    if args.rust_target:
        deployment_target = args.deployment_target or os.environ.get("MACOSX_DEPLOYMENT_TARGET")
        if not deployment_target:
            raise SystemExit("--deployment-target (or MACOSX_DEPLOYMENT_TARGET) is required with --rust-target")
        swift_target = rust_target_to_swift_target(args.rust_target, deployment_target)
        print(f"Resolved SWIFT_TARGET={swift_target}")

    target_info = run_swift_target_info(swift_target)
    search_paths, runtime_resource_path = collect_search_paths(target_info)
    stdlib_dir = resolve_stdlib_dir(search_paths)

    print(f"Resolved SWIFT_STDLIB_DIR={stdlib_dir}")
    print(f"Resolved SWIFT_RUNTIME_RESOURCE_DIR={runtime_resource_path}")
    print("Resolved Swift runtime search paths:")
    for path in search_paths:
        print(f"  {path}")

    dyld_paths = ":".join(search_paths)
    rustflags_parts = []
    for path in search_paths:
        rustflags_parts.append(f"-C link-arg=-L{path}")
        rustflags_parts.append(f"-C link-arg=-Wl,-rpath,{path}")
    existing_rustflags = os.environ.get("RUSTFLAGS", "").strip()
    if existing_rustflags:
        rustflags_parts.append(existing_rustflags)
    rustflags = " ".join(rustflags_parts)

    existing_dyld = os.environ.get("DYLD_FALLBACK_LIBRARY_PATH", "").strip()
    if existing_dyld:
        dyld_paths = f"{dyld_paths}:{existing_dyld}"

    if args.github_env:
        values = {
            "SWIFT_STDLIB_DIR": stdlib_dir,
            "SWIFT_RUNTIME_RESOURCE_DIR": runtime_resource_path,
            "SWIFT_RUNTIME_SEARCH_PATHS": dyld_paths,
            "DYLD_FALLBACK_LIBRARY_PATH": dyld_paths,
            "RUSTFLAGS": rustflags,
        }
        if swift_target:
            values["SWIFT_TARGET"] = swift_target
        append_github_env(args.github_env, values)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
