#!/usr/bin/env python3
from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import zipfile
from typing import Iterable

ABSOLUTE_SWIFT_PATH_PREFIXES = (
    "/Applications/Xcode",
    "/Library/Developer/CommandLineTools",
)

from swift_runtime import (
    build_swift_library_index,
    get_swift_runtime_layout,
    resolve_swift_library,
    rust_target_to_arch,
    rust_target_to_swift_target,
)


SWIFT_DYLIB_PREFIX = "libswift"


class RepairError(RuntimeError):
    pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Swift-aware repair for macOS wheels")
    parser.add_argument("wheel", help="Path to the wheel to repair")
    parser.add_argument(
        "--output",
        help="Output wheel path. Defaults to repairing the input wheel in place.",
    )
    parser.add_argument("--rust-target", help="Rust target triple, e.g. aarch64-apple-darwin")
    parser.add_argument(
        "--deployment-target",
        help="macOS deployment target, e.g. 11.0. Required with --rust-target.",
    )
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        help="Keep the unpacked temporary directory for debugging.",
    )
    return parser.parse_args()


def run(cmd: list[str], *, capture_output: bool = False) -> str:
    if capture_output:
        return subprocess.check_output(cmd, text=True)
    subprocess.check_call(cmd)
    return ""


def parse_otool_dependencies(binary_path: pathlib.Path) -> list[str]:
    output = run(["otool", "-L", str(binary_path)], capture_output=True)
    dependencies: list[str] = []
    for line in output.splitlines()[1:]:
        stripped = line.strip()
        if not stripped:
            continue
        dependencies.append(stripped.split(" ", 1)[0])
    return dependencies


def parse_otool_rpaths(binary_path: pathlib.Path) -> list[str]:
    output = run(["otool", "-l", str(binary_path)], capture_output=True)
    lines = output.splitlines()
    rpaths: list[str] = []
    for index, line in enumerate(lines):
        if line.strip() != "cmd LC_RPATH":
            continue
        for candidate in lines[index + 1 : index + 8]:
            stripped = candidate.strip()
            if not stripped.startswith("path "):
                continue
            rpaths.append(stripped.split(" (offset ", 1)[0].split(" ", 1)[1])
            break
    return rpaths


def is_swift_dylib_install_name(install_name: str) -> bool:
    basename = pathlib.Path(install_name).name
    return basename.startswith(SWIFT_DYLIB_PREFIX) and basename.endswith(".dylib")


def is_system_swift_install_name(install_name: str) -> bool:
    return install_name.startswith("/usr/lib/swift/")


def is_toolchain_swift_install_name(install_name: str) -> bool:
    return install_name.startswith(ABSOLUTE_SWIFT_PATH_PREFIXES)


def find_root_binaries(staging_dir: pathlib.Path) -> list[pathlib.Path]:
    binaries: list[pathlib.Path] = []
    for suffix in ("*.so", "*.dylib"):
        for path in staging_dir.rglob(suffix):
            if ".dist-info/" in path.as_posix() or any(part.endswith(".dSYM") for part in path.parts) or "/.dylibs/" in path.as_posix():
                continue
            binaries.append(path)
    binaries.sort()
    return binaries


def select_bundle_dir(root_binaries: list[pathlib.Path]) -> pathlib.Path:
    for binary in root_binaries:
        if binary.suffix == ".so":
            return binary.parent / ".dylibs"
    if root_binaries:
        return root_binaries[0].parent / ".dylibs"
    raise RepairError("No root binaries found in wheel")


def make_loader_path(from_binary: pathlib.Path, target: pathlib.Path) -> str:
    relative = os.path.relpath(target, from_binary.parent)
    return "@loader_path/" + pathlib.PurePosixPath(relative).as_posix()


def copy_and_thin_library(source: pathlib.Path, destination: pathlib.Path, target_arch: str | None) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    if target_arch:
        result = subprocess.run(
            ["lipo", str(source), "-thin", target_arch, "-output", str(destination)],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            stderr = result.stderr.strip() or result.stdout.strip() or "unknown lipo error"
            raise RepairError(f"Unable to thin {source} to architecture {target_arch}: {stderr}")
    else:
        shutil.copy2(source, destination)
    shutil.copymode(source, destination)


def set_dylib_id(binary_path: pathlib.Path, new_id: str) -> None:
    subprocess.run(["install_name_tool", "-id", new_id, str(binary_path)], check=True)


def rewrite_dependency(binary_path: pathlib.Path, old: str, new: str) -> None:
    subprocess.run(["install_name_tool", "-change", old, new, str(binary_path)], check=True)


def delete_rpath(binary_path: pathlib.Path, old_rpath: str) -> None:
    subprocess.run(["install_name_tool", "-delete_rpath", old_rpath, str(binary_path)], check=True)


def ad_hoc_codesign(binary_paths: Iterable[pathlib.Path]) -> None:
    codesign = shutil.which("codesign")
    if not codesign:
        return
    for binary_path in binary_paths:
        subprocess.run(
            [codesign, "--force", "--sign", "-", "--timestamp=none", str(binary_path)],
            check=True,
        )


def update_record(staging_dir: pathlib.Path) -> None:
    record_files = list(staging_dir.rglob("*.dist-info/RECORD"))
    if len(record_files) != 1:
        raise RepairError(f"Expected exactly one RECORD file, found {len(record_files)}")
    record_path = record_files[0]

    rows: list[list[str]] = []
    for path in sorted(staging_dir.rglob("*")):
        if path.is_dir():
            continue
        relative = path.relative_to(staging_dir).as_posix()
        if path == record_path:
            rows.append([relative, "", ""])
            continue
        data = path.read_bytes()
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).decode("ascii").rstrip("=")
        rows.append([relative, f"sha256={digest}", str(len(data))])

    with record_path.open("w", encoding="utf-8", newline="") as fh:
        writer = csv.writer(fh)
        writer.writerows(rows)


def repack_wheel(staging_dir: pathlib.Path, output_wheel: pathlib.Path) -> None:
    with zipfile.ZipFile(output_wheel, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        for path in sorted(staging_dir.rglob("*")):
            if path.is_dir():
                continue
            zf.write(path, path.relative_to(staging_dir).as_posix())


def main() -> int:
    args = parse_args()
    input_wheel = pathlib.Path(args.wheel).resolve()
    output_wheel = pathlib.Path(args.output).resolve() if args.output else input_wheel

    swift_target = None
    target_arch = None
    if args.rust_target:
        deployment_target = args.deployment_target or os.environ.get("MACOSX_DEPLOYMENT_TARGET")
        if not deployment_target:
            raise SystemExit("--deployment-target (or MACOSX_DEPLOYMENT_TARGET) is required with --rust-target")
        swift_target = rust_target_to_swift_target(args.rust_target, deployment_target)
        target_arch = rust_target_to_arch(args.rust_target)
        print(f"Resolved SWIFT_TARGET={swift_target}")
        print(f"Resolved target architecture={target_arch}")

    layout, target_info_output = get_swift_runtime_layout(swift_target)
    print(target_info_output)
    print(f"Resolved SWIFT_TOOLCHAIN_LIB_ROOT={layout.toolchain_lib_root}")
    print("Resolved Swift bundle search paths:")
    for path in layout.bundle_search_paths:
        print(f"  {path}")

    library_index = build_swift_library_index(layout.bundle_search_paths)

    temp_dir_obj = tempfile.TemporaryDirectory(prefix="repair-swift-wheel-")
    try:
        staging_dir = pathlib.Path(temp_dir_obj.name)
        with zipfile.ZipFile(input_wheel) as zf:
            zf.extractall(staging_dir)

        root_binaries = find_root_binaries(staging_dir)
        if not root_binaries:
            raise RepairError("No root binaries found in wheel")
        print("Root binaries:")
        for binary in root_binaries:
            print(f"  {binary.relative_to(staging_dir).as_posix()}")

        bundle_dir = select_bundle_dir(root_binaries)
        bundle_dir.mkdir(parents=True, exist_ok=True)
        print(f"Bundling Swift dylibs into {bundle_dir.relative_to(staging_dir).as_posix()}")

        copied_libraries: dict[str, pathlib.Path] = {}
        queued: list[pathlib.Path] = list(root_binaries)
        processed: set[pathlib.Path] = set()
        modified_binaries: set[pathlib.Path] = set()
        required_rpath_swift: set[str] = set()
        satisfied_required_rpath_swift: set[str] = set()
        system_swift_basenames: set[str] = set()

        for binary_path in root_binaries:
            for install_name in parse_otool_dependencies(binary_path):
                if is_system_swift_install_name(install_name) and is_swift_dylib_install_name(install_name):
                    system_swift_basenames.add(pathlib.Path(install_name).name)

        prefer_system_swift_runtime = bool(system_swift_basenames)
        if system_swift_basenames:
            print("System Swift dylibs already referenced by root binaries:")
            for basename in sorted(system_swift_basenames):
                print(f"  /usr/lib/swift/{basename}")
            print("Preferring system Swift runtime over wheel-local bundling to avoid duplicate runtime loading.")

        while queued:
            binary_path = queued.pop(0)
            if binary_path in processed:
                continue
            processed.add(binary_path)

            for install_name in parse_otool_dependencies(binary_path):
                if not is_swift_dylib_install_name(install_name):
                    continue

                basename = pathlib.Path(install_name).name
                must_bundle = install_name.startswith("@rpath/")
                uses_system_swift = is_system_swift_install_name(install_name)
                uses_toolchain_swift = is_toolchain_swift_install_name(install_name)

                if uses_system_swift:
                    system_swift_basenames.add(basename)
                    continue

                if must_bundle:
                    required_rpath_swift.add(basename)

                system_install_name = f"/usr/lib/swift/{basename}"
                if (must_bundle or uses_toolchain_swift) and prefer_system_swift_runtime:
                    if install_name != system_install_name:
                        rewrite_dependency(binary_path, install_name, system_install_name)
                        modified_binaries.add(binary_path)
                        print(
                            f"Rewrote {binary_path.relative_to(staging_dir).as_posix()}: {install_name} -> {system_install_name}"
                        )
                    if must_bundle:
                        satisfied_required_rpath_swift.add(basename)
                    continue

                if not (must_bundle or uses_toolchain_swift):
                    continue

                resolved = resolve_swift_library(basename, library_index)
                if not resolved:
                    if must_bundle:
                        raise RepairError(
                            f"Unable to resolve required Swift library {basename} inside active toolchain bundle search paths"
                        )
                    print(f"Leaving unresolved system Swift dependency in place: {install_name}")
                    continue

                destination = copied_libraries.get(basename)
                if destination is None:
                    source_path = pathlib.Path(resolved)
                    destination = bundle_dir / basename
                    copy_and_thin_library(source_path, destination, target_arch)
                    copied_libraries[basename] = destination
                    queued.append(destination)
                    modified_binaries.add(destination)
                    set_dylib_id(destination, make_loader_path(destination, destination))
                    print(f"Bundled {basename} from {source_path} -> {destination.relative_to(staging_dir).as_posix()}")

                new_install_name = make_loader_path(binary_path, destination)
                if install_name != new_install_name:
                    rewrite_dependency(binary_path, install_name, new_install_name)
                    modified_binaries.add(binary_path)
                    print(
                        f"Rewrote {binary_path.relative_to(staging_dir).as_posix()}: {install_name} -> {new_install_name}"
                    )

                if must_bundle:
                    satisfied_required_rpath_swift.add(basename)

        for binary_path in sorted(processed):
            for rpath in parse_otool_rpaths(binary_path):
                if rpath.startswith(ABSOLUTE_SWIFT_PATH_PREFIXES):
                    delete_rpath(binary_path, rpath)
                    modified_binaries.add(binary_path)
                    print(f"Deleted RPATH from {binary_path.relative_to(staging_dir).as_posix()}: {rpath}")

        ad_hoc_codesign(sorted(modified_binaries))

        unresolved_after_repair: list[tuple[pathlib.Path, str]] = []
        absolute_toolchain_swift_paths: list[tuple[pathlib.Path, str]] = []
        absolute_toolchain_rpaths: list[tuple[pathlib.Path, str]] = []
        for binary_path in sorted(processed):
            for install_name in parse_otool_dependencies(binary_path):
                if not is_swift_dylib_install_name(install_name):
                    continue
                if install_name.startswith("@rpath/"):
                    unresolved_after_repair.append((binary_path, install_name))
                if install_name.startswith(ABSOLUTE_SWIFT_PATH_PREFIXES):
                    absolute_toolchain_swift_paths.append((binary_path, install_name))
            for rpath in parse_otool_rpaths(binary_path):
                if rpath.startswith(ABSOLUTE_SWIFT_PATH_PREFIXES):
                    absolute_toolchain_rpaths.append((binary_path, rpath))

        if unresolved_after_repair:
            lines = [
                f"{binary.relative_to(staging_dir).as_posix()}: {install_name}"
                for binary, install_name in unresolved_after_repair
            ]
            raise RepairError("Unresolved Swift @rpath dependencies remain after repair:\n" + "\n".join(lines))

        if absolute_toolchain_swift_paths:
            lines = [
                f"{binary.relative_to(staging_dir).as_posix()}: {install_name}"
                for binary, install_name in absolute_toolchain_swift_paths
            ]
            raise RepairError(
                "Absolute Xcode/CommandLineTools Swift paths remain after repair:\n" + "\n".join(lines)
            )

        if absolute_toolchain_rpaths:
            lines = [
                f"{binary.relative_to(staging_dir).as_posix()}: {rpath}"
                for binary, rpath in absolute_toolchain_rpaths
            ]
            raise RepairError(
                "Absolute Xcode/CommandLineTools RPATH entries remain after repair:\n" + "\n".join(lines)
            )

        missing_required = [
            name
            for name in sorted(required_rpath_swift)
            if name not in copied_libraries and name not in satisfied_required_rpath_swift
        ]
        if missing_required:
            raise RepairError(
                "Required Swift @rpath libraries were not bundled: " + ", ".join(missing_required)
            )

        update_record(staging_dir)

        output_wheel.parent.mkdir(parents=True, exist_ok=True)
        temp_output = output_wheel.with_suffix(output_wheel.suffix + ".tmp")
        repack_wheel(staging_dir, temp_output)
        temp_output.replace(output_wheel)
        print(f"Repaired wheel written to {output_wheel}")

        if args.keep_temp:
            print(f"Keeping temporary directory: {staging_dir}")
            temp_dir_obj.cleanup = lambda: None  # type: ignore[attr-defined]
        return 0
    finally:
        if not args.keep_temp:
            temp_dir_obj.cleanup()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RepairError as exc:
        print(exc, file=sys.stderr)
        raise SystemExit(1)
