#!/usr/bin/env bash
set -euo pipefail

TARGET_TRIPLE="${1:?usage: ci_build_and_repair_wheel.sh <target-triple> <out-dir> [deployment-target]}"
OUT_DIR="${2:?usage: ci_build_and_repair_wheel.sh <target-triple> <out-dir> [deployment-target]}"
DEPLOYMENT_TARGET="${3:-${MACOSX_DEPLOYMENT_TARGET:-}}"
PYTHON_BIN="${PYTHON_BIN:-$(command -v python || command -v python3)}"

export PATH="$(dirname "$(rustup which cargo)"):$PATH"
hash -r

test -n "${SWIFT_RUNTIME_RESOURCE_DIR:-}"
test -d "$SWIFT_RUNTIME_RESOURCE_DIR"

which "$PYTHON_BIN"
"$PYTHON_BIN" --version
which cargo
cargo -vV
which rustc
rustc -vV

mkdir -p "$OUT_DIR"
"$PYTHON_BIN" -m maturin build --release --auditwheel skip --target "$TARGET_TRIPLE" --out "$OUT_DIR"

WHEEL="$(find "$OUT_DIR" -name '*.whl' | head -n 1)"
test -n "$WHEEL"
"$PYTHON_BIN" scripts/repair_swift_wheel.py \
  "$WHEEL" \
  --rust-target "$TARGET_TRIPLE" \
  --deployment-target "$DEPLOYMENT_TARGET"
