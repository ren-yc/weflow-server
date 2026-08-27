#!/usr/bin/env bash
# Build wrapper for Linux/macOS: system perl + C toolchain (gcc/clang/make)
# are used directly by cc (libsqlite3-sys) and perl+make (openssl-src).
# No vcvars equivalent exists on these platforms.
# Usage: bash scripts/build.sh [cargo args...]
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v perl >/dev/null 2>&1; then
  echo "warning: perl not found (openssl-src needs it for Configure)." >&2
  echo "         Linux: apt install perl | macOS: ships with the system." >&2
fi
if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
  echo "warning: no C compiler found (libsqlite3-sys needs one)." >&2
  echo "         Linux: apt install build-essential | macOS: xcode-select --install" >&2
fi
if [[ "$(uname -s)" == "Darwin" ]] && ! command -v make >/dev/null 2>&1; then
  echo "warning: make not found (openssl-src needs it). macOS: xcode-select --install" >&2
fi

exec cargo "$@"