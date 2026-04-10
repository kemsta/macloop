#!/usr/bin/env python3
from __future__ import annotations

import json
import pathlib
import re
import subprocess
from dataclasses import dataclass
from typing import Iterable


@dataclass(frozen=True)
class SwiftRuntimeLayout:
    swift_target: str | None
    target_info: dict
    runtime_library_paths: list[str]
    runtime_resource_path: str
    toolchain_lib_root: str
    candidate_dirs: list[str]


def rust_target_to_swift_target(rust_target: str, deployment_target: str) -> str:
    mapping = {
        "aarch64-apple-darwin": "arm64-apple-macosx{deployment}",
        "x86_64-apple-darwin": "x86_64-apple-macosx{deployment}",
    }
    template = mapping.get(rust_target)
    if template is None:
        raise ValueError(f"Unsupported Rust target for Swift runtime resolution: {rust_target}")
    return template.format(deployment=deployment_target)


def rust_target_to_arch(rust_target: str) -> str:
    mapping = {
        "aarch64-apple-darwin": "arm64",
        "x86_64-apple-darwin": "x86_64",
    }
    arch = mapping.get(rust_target)
    if arch is None:
        raise ValueError(f"Unsupported Rust target for Swift runtime resolution: {rust_target}")
    return arch


def run_swift_target_info(swift_target: str | None) -> tuple[str, dict]:
    cmd = ["swiftc"]
    if swift_target:
        cmd.extend(["-target", swift_target])
    cmd.append("-print-target-info")
    output = subprocess.check_output(cmd, text=True)
    return output.rstrip(), json.loads(output)


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


def collect_candidate_dirs(target_info: dict) -> tuple[list[str], str, str, list[str]]:
    paths = target_info.get("paths", {})
    runtime_library_paths = [str(pathlib.Path(p)) for p in paths.get("runtimeLibraryPaths", []) if p]
    runtime_resource_path = str(pathlib.Path(paths.get("runtimeResourcePath", ""))) if paths.get("runtimeResourcePath") else ""

    if not runtime_resource_path:
        raise ValueError("swiftc target info did not provide runtimeResourcePath")

    toolchain_lib_root = str(pathlib.Path(runtime_resource_path).parent)
    lib_root_path = pathlib.Path(toolchain_lib_root)

    candidates: list[str] = []
    candidates.extend(runtime_library_paths)
    candidates.append(runtime_resource_path)

    if lib_root_path.exists():
        versioned_dirs: list[pathlib.Path] = []
        for child in lib_root_path.iterdir():
            if not child.is_dir() or not child.name.startswith("swift"):
                continue
            macosx_dir = child / "macosx"
            if macosx_dir.is_dir():
                versioned_dirs.append(macosx_dir)
            elif any(grandchild.suffix == ".dylib" for grandchild in child.glob("libswift*.dylib")):
                versioned_dirs.append(child)
        candidates.extend(str(path) for path in sorted(versioned_dirs, key=version_key))

    return dedupe_paths(candidates), runtime_resource_path, toolchain_lib_root, runtime_library_paths


def get_swift_runtime_layout(swift_target: str | None = None) -> tuple[SwiftRuntimeLayout, str]:
    target_info_output, target_info = run_swift_target_info(swift_target)
    candidate_dirs, runtime_resource_path, toolchain_lib_root, runtime_library_paths = collect_candidate_dirs(target_info)
    return (
        SwiftRuntimeLayout(
            swift_target=swift_target,
            target_info=target_info,
            runtime_library_paths=runtime_library_paths,
            runtime_resource_path=runtime_resource_path,
            toolchain_lib_root=toolchain_lib_root,
            candidate_dirs=candidate_dirs,
        ),
        target_info_output,
    )


def build_swift_library_index(candidate_dirs: Iterable[str]) -> dict[str, list[str]]:
    index: dict[str, list[str]] = {}
    for directory in candidate_dirs:
        dir_path = pathlib.Path(directory)
        if not dir_path.is_dir():
            continue
        for dylib_path in sorted(dir_path.glob("libswift*.dylib")):
            index.setdefault(dylib_path.name, []).append(str(dylib_path))
    return index


def resolve_swift_library(basename: str, library_index: dict[str, list[str]]) -> str | None:
    candidates = library_index.get(basename, [])
    return candidates[0] if candidates else None
