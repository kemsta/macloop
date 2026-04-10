#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
from typing import Iterable

from swift_runtime import (
    build_swift_library_index,
    get_swift_runtime_layout,
    resolve_swift_library,
    rust_target_to_swift_target,
)


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


def append_github_env(path: str, values: dict[str, str]) -> None:
    with open(path, "a", encoding="utf-8") as fh:
        for key, value in values.items():
            fh.write(f"{key}={value}\n")


def collect_link_paths(candidate_dirs: Iterable[str], library_index: dict[str, list[str]]) -> list[str]:
    link_paths = [path for path in candidate_dirs if path]

    for library_name in ("libswiftCore.dylib", "libswift_Concurrency.dylib"):
        resolved = resolve_swift_library(library_name, library_index)
        if resolved:
            link_paths.append(os.path.dirname(resolved))

    deduped: list[str] = []
    seen: set[str] = set()
    for path in link_paths:
        if path in seen:
            continue
        seen.add(path)
        deduped.append(path)
    return deduped


def main() -> int:
    args = parse_args()

    swift_target = None
    if args.rust_target:
        deployment_target = args.deployment_target or os.environ.get("MACOSX_DEPLOYMENT_TARGET")
        if not deployment_target:
            raise SystemExit("--deployment-target (or MACOSX_DEPLOYMENT_TARGET) is required with --rust-target")
        swift_target = rust_target_to_swift_target(args.rust_target, deployment_target)
        print(f"Resolved SWIFT_TARGET={swift_target}")

    layout, target_info_output = get_swift_runtime_layout(swift_target)
    print(target_info_output)

    library_index = build_swift_library_index(layout.candidate_dirs)
    link_paths = collect_link_paths(layout.candidate_dirs, library_index)

    stdlib_dir = resolve_swift_library("libswiftCore.dylib", library_index)
    if not stdlib_dir:
        raise SystemExit("swiftc target info did not yield a directory containing libswiftCore.dylib")
    stdlib_dir = os.path.dirname(stdlib_dir)

    print(f"Resolved SWIFT_STDLIB_DIR={stdlib_dir}")
    print(f"Resolved SWIFT_RUNTIME_RESOURCE_DIR={layout.runtime_resource_path}")
    print(f"Resolved SWIFT_TOOLCHAIN_LIB_ROOT={layout.toolchain_lib_root}")
    print("Resolved Swift runtime search paths:")
    for path in link_paths:
        print(f"  {path}")

    dyld_paths = ":".join(link_paths)
    rustflags_parts = []
    for path in link_paths:
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
            "SWIFT_RUNTIME_RESOURCE_DIR": layout.runtime_resource_path,
            "SWIFT_TOOLCHAIN_LIB_ROOT": layout.toolchain_lib_root,
            "SWIFT_RUNTIME_SEARCH_PATHS": ":".join(link_paths),
            "DYLD_FALLBACK_LIBRARY_PATH": dyld_paths,
            "RUSTFLAGS": rustflags,
        }
        if swift_target:
            values["SWIFT_TARGET"] = swift_target
        append_github_env(args.github_env, values)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
