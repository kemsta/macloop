#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os

from swift_runtime import get_swift_runtime_layout, rust_target_to_swift_target


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

    print(f"Resolved SWIFT_RUNTIME_RESOURCE_DIR={layout.runtime_resource_path}")
    print(f"Resolved SWIFT_TOOLCHAIN_LIB_ROOT={layout.toolchain_lib_root}")
    print("Resolved Swift link paths:")
    for path in layout.link_paths:
        print(f"  {path}")
    print("Resolved Swift bundle search paths:")
    for path in layout.bundle_search_paths:
        print(f"  {path}")

    dyld_paths = ":".join(layout.link_paths)
    rustflags_parts = []
    for path in layout.link_paths:
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
            "SWIFT_RUNTIME_RESOURCE_DIR": layout.runtime_resource_path,
            "SWIFT_TOOLCHAIN_LIB_ROOT": layout.toolchain_lib_root,
            "SWIFT_LINK_PATHS": ":".join(layout.link_paths),
            "SWIFT_BUNDLE_SEARCH_PATHS": ":".join(layout.bundle_search_paths),
            "DYLD_FALLBACK_LIBRARY_PATH": dyld_paths,
            "RUSTFLAGS": rustflags,
        }
        if swift_target:
            values["SWIFT_TARGET"] = swift_target
        append_github_env(args.github_env, values)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
