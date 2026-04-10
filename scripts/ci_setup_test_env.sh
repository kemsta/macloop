#!/usr/bin/env bash
set -euo pipefail

uv sync --group dev --no-install-project --no-install-workspace --no-install-package macloop --no-install-local
.venv/bin/maturin develop --manifest-path python_ffi/Cargo.toml
