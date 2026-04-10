#!/usr/bin/env bash
set -euo pipefail

TARGET_PATH="${1:-}"
TMPDIR_CREATED=""

cleanup() {
  if [[ -n "$TMPDIR_CREATED" && -d "$TMPDIR_CREATED" ]]; then
    rm -rf "$TMPDIR_CREATED"
  fi
}
trap cleanup EXIT

section() {
  printf '\n===== %s =====\n' "$1"
}

print_cmd() {
  printf '+ %s\n' "$*"
  "$@"
}

print_if_exists() {
  local path="$1"
  if [[ -e "$path" ]]; then
    print_cmd ls -ld "$path"
  else
    echo "missing: $path"
  fi
}

print_rpaths() {
  local bin="$1"
  section "LC_RPATH for $bin"
  otool -l "$bin" | awk '
    /LC_RPATH/ {in_rpath=1; print; next}
    in_rpath && /^Load command/ {in_rpath=0}
    in_rpath {print}
  '
}

inspect_binary() {
  local bin="$1"
  section "Inspect binary: $bin"
  print_cmd file "$bin"
  if command -v lipo >/dev/null 2>&1; then
    print_cmd lipo -info "$bin" || true
  fi
  print_cmd otool -L "$bin"
  print_rpaths "$bin"
}

inspect_swift_dylib() {
  local dylib="$1"
  if [[ ! -f "$dylib" ]]; then
    echo "missing dylib: $dylib"
    return
  fi
  section "Inspect Swift dylib: $dylib"
  print_cmd file "$dylib"
  if command -v lipo >/dev/null 2>&1; then
    print_cmd lipo -info "$dylib" || true
  fi
  print_cmd otool -L "$dylib"
}

inspect_swift_dir() {
  local dir="$1"
  section "Inspect Swift dir: $dir"
  if [[ ! -d "$dir" ]]; then
    echo "missing directory"
    return
  fi

  print_cmd ls -ld "$dir"
  echo "sample contents:"
  ls -1 "$dir" | head -n 30

  section "All Swift dylibs in $dir"
  find "$dir" -maxdepth 1 -name 'libswift*.dylib' | sort || true

  inspect_swift_dylib "$dir/libswift_Concurrency.dylib"
  inspect_swift_dylib "$dir/libswiftCore.dylib"
  inspect_swift_dylib "$dir/libswiftFoundation.dylib"
}

resolve_target_binary() {
  local path="$1"
  if [[ "$path" == *.whl ]]; then
    TMPDIR_CREATED="$(mktemp -d)"
    section "Extract wheel: $path" >&2
    python3 - <<'PY' "$path" "$TMPDIR_CREATED"
import sys, zipfile, pathlib
wheel = pathlib.Path(sys.argv[1])
out = pathlib.Path(sys.argv[2])
with zipfile.ZipFile(wheel) as zf:
    zf.extractall(out)
PY
    echo "extracted to: $TMPDIR_CREATED" >&2
    find "$TMPDIR_CREATED" \( -name '*.so' -o -name '*.dylib' \) | head -n 20 >&2
    find "$TMPDIR_CREATED" -name '*.so' | head -n 1
  else
    printf '%s\n' "$path"
  fi
}

section "Environment"
printf 'PWD=%s\n' "$PWD"
printf 'xcode-select -p: '
xcode-select -p || true
printf 'DEVELOPER_DIR=%s\n' "${DEVELOPER_DIR:-}"
printf 'SDKROOT=%s\n' "${SDKROOT:-}"
printf 'DYLD_FALLBACK_LIBRARY_PATH=%s\n' "${DYLD_FALLBACK_LIBRARY_PATH:-}"
printf 'RUSTFLAGS=%s\n' "${RUSTFLAGS:-}"

section "Candidate Swift stdlib dirs"
CANDIDATES=(
  "/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx"
  "/Library/Developer/CommandLineTools/usr/lib/swift/macosx"
  "$(xcode-select -p)/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx"
  "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx"
)

for dir in "${CANDIDATES[@]}"; do
  print_if_exists "$dir"
done

section "find libswift_Concurrency.dylib"
find /Library/Developer/CommandLineTools /Applications \
  -path '*/usr/lib/swift*/macosx/libswift_Concurrency.dylib' \
  2>/dev/null | sort -u || true

section "find Swift dylibs"
find /Library/Developer/CommandLineTools /Applications \
  -path '*/usr/lib/swift*/macosx/libswift*.dylib' \
  2>/dev/null | sort -u || true

for dir in "${CANDIDATES[@]}"; do
  inspect_swift_dir "$dir"
done

if [[ -n "$TARGET_PATH" ]]; then
  section "Target artifact"
  print_if_exists "$TARGET_PATH"
  RESOLVED_BIN="$(resolve_target_binary "$TARGET_PATH")"
  if [[ -n "$RESOLVED_BIN" && -e "$RESOLVED_BIN" ]]; then
    inspect_binary "$RESOLVED_BIN"
  else
    echo "No binary resolved from target path: $TARGET_PATH"
  fi
fi
