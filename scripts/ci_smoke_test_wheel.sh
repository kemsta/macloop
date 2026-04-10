#!/usr/bin/env bash
set -euo pipefail

WHEEL_PATH="${1:?usage: ci_smoke_test_wheel.sh <wheel-path>}"
PYTHON_BIN="${PYTHON_BIN:-$(command -v python || command -v python3)}"
VENV_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$VENV_DIR"
}
trap cleanup EXIT

"$PYTHON_BIN" -m venv "$VENV_DIR/venv"
"$VENV_DIR/venv/bin/python" -m pip install --upgrade pip
"$VENV_DIR/venv/bin/python" -m pip install "$WHEEL_PATH"
"$VENV_DIR/venv/bin/python" - <<'PY'
import pathlib
import macloop
import macloop._macloop

print("macloop:", pathlib.Path(macloop.__file__).resolve())
print("macloop._macloop:", pathlib.Path(macloop._macloop.__file__).resolve())
PY
