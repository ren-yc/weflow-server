#!/usr/bin/env bash
# Build wrapper for Linux/macOS (mirror of scripts/build.ps1).
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo "$@"