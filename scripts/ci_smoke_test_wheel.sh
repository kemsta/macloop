#!/usr/bin/env bash
set -euo pipefail

WHEEL_PATH="${1:?usage: ci_smoke_test_wheel.sh <wheel-path>}"
WHEEL_PATH="$(cd "$(dirname "$WHEEL_PATH")" && pwd)/$(basename "$WHEEL_PATH")"
PYTHON_BIN="${PYTHON_BIN:-$(command -v python || command -v python3)}"
VENV_DIR="$(mktemp -d)"
RUN_DIR="$VENV_DIR/run"
REPO_ROOT="$(pwd)"
cleanup() {
  rm -rf "$VENV_DIR"
}
trap cleanup EXIT

mkdir -p "$RUN_DIR"
"$PYTHON_BIN" -m venv "$VENV_DIR/venv"
"$VENV_DIR/venv/bin/python" -m pip install --upgrade pip
"$VENV_DIR/venv/bin/python" -m pip install "$WHEEL_PATH"
cd "$RUN_DIR"
export REPO_ROOT
"$VENV_DIR/venv/bin/python" - <<'PY'
import pathlib
import macloop
import macloop._macloop

repo_root = pathlib.Path(__import__('os').environ['REPO_ROOT']).resolve()
macloop_path = pathlib.Path(macloop.__file__).resolve()
ext_path = pathlib.Path(macloop._macloop.__file__).resolve()

print("macloop:", macloop_path)
print("macloop._macloop:", ext_path)

if repo_root in macloop_path.parents or repo_root in ext_path.parents:
    raise SystemExit("smoke test imported macloop from the repository checkout instead of the installed wheel")
PY
