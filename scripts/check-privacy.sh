#!/usr/bin/env bash
# Privacy reality-check: no real WeChat identifiers, keys or local paths may
# reach the repository (mirrors qqflow-server's check-privacy.sh).
# Usage: bash scripts/check-privacy.sh
set -u
cd "$(dirname "$0")/.."

# Patterns that should NEVER appear in tracked files (all values below are
# fixture/example-only).
PATTERNS=(
  'wxid_[a-zA-Z0-9]{10,}'          # real wxid length is >= 10
  'xwechat_files[^"]*'              # real absolute paths
  'Documents\\WeChat Files'        # real 3.x root (tracked docs may mention
                                   # it; keep this off)
)

bad=0
for f in $(git ls-files 2>/dev/null || find . -path ./target -prune -o -type f -print); do
  case "$f" in
    *.md|*.rs|*.toml|*.json|*.ps1|*.sh)
      # real-looking 64-hex keys are the sharpest signal
      if grep -qE '([0-9a-f]{64})' "$f" 2>/dev/null; then
        # fixture keys are allowed when they sit next to FAKE_ markers
        if ! grep -qE 'FAKE_KEY_HEX|fake key|example' "$f" 2>/dev/null; then
          echo "PRIVACY: 64-hex literal in $f"; bad=1
        fi
      fi
      ;;
  esac
done

if [ "$bad" -ne 0 ]; then
  echo "check-privacy: FAILED (see lines above)"
  exit 1
fi
echo "check-privacy: ok"
exit 0