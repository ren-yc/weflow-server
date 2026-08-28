#!/usr/bin/env bash
# Privacy check: fail loudly if machine-specific secrets leaked into tracked
# repository files. Run by the git pre-commit hook (install once per clone
# with `bash scripts/install-hooks.sh`), and on demand:
# `bash scripts/check-privacy.sh`
#
# Scans (tracked files only — `git grep`, so gitignored scratch dirs like
# _fetch_wechat/ and reference/ are out of scope by construction):
#   1. exact real values from the gitignored weflow-server.json — wxid,
#      db_path, img_aes_key and every per-database enc_key (skipped when the
#      file is absent, e.g. CI)
#   2. generic 64-hex literals (the enc_key shape) outside the fixture
#      allowlist — catches leaks on machines without the local config
#   3. the local username and machine-specific absolute paths
#
# Exits non-zero (and lists every hit) when anything leaks, which aborts the
# commit; findings go to stderr, which git shows verbatim. Values are NEVER
# echoed — only labels and the files they were found in.
#
# Deliberately NOT scanned: img_xor_key. It is a single XOR byte (256
# possible values) and `src/keystore/mod.rs` legitimately documents the
# accepted *format* with a literal example, so matching it is pure noise
# with no secrecy value to protect.

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

CONFIG="weflow-server.json"

# Paths excluded from every scan:
#   *.lock        — Cargo.lock carries ~200 crate checksums in 64-hex shape
#   this script   — it contains the pattern strings below
PATHSPEC=(':!*.lock' ':!scripts/check-privacy.sh')

# Fixture 64-hex values that are allowed to appear in tracked files. Keep this
# list exact (full literals, not markers): allowlisting by nearby marker words
# lets a real key slide through any file that happens to say "example".
FIXTURE_HEX=(
  '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' # tests/common/mod.rs FAKE_KEY_HEX
)

hits=0

# report <label> <fixed-string-pattern>
# Findings go to STDERR so the git pre-commit hook surfaces them verbatim
# alongside the aborted commit. The pattern itself is never printed.
report() {
  local label="$1"
  local pattern="$2"
  local found
  # `-e` guards patterns that begin with `-`; pathspecs go after `--`.
  found=$(git grep -lF -e "$pattern" -- "${PATHSPEC[@]}" 2>/dev/null)
  if [ -n "$found" ]; then
    echo "[隐私检查] 检测到 $label:" >&2
    echo "$found" | sed 's/^/    /' >&2
    hits=$((hits + 1))
  fi
}

# JSON field reader for the config: tries the interpreters that may exist on a
# dev machine (python / python3 / the Windows `py` launcher). Prints one value
# per line for `keys` (a nested object), a single line otherwise. Returns 1
# when no interpreter works — callers must NOT treat that as "nothing to
# check" while the config exists (see the loud error below).
read_config_values() {
  local py
  for py in python python3 "py -3"; do
    $py - "$CONFIG" <<'PY' 2>/dev/null && return 0
import json, sys
# Force LF: on Windows Python defaults to \r\n, and a trailing \r would make
# every `git grep -F` search for "<secret>\r" and silently match nothing.
try:
    sys.stdout.reconfigure(newline="\n")
except AttributeError:  # Python < 3.7
    pass
with open(sys.argv[1], encoding="utf-8") as fh:
    cfg = json.load(fh)
# label<TAB>value; img_xor_key is deliberately omitted (see header comment)
for field in ("wxid", "db_path", "img_aes_key"):
    value = cfg.get(field)
    if isinstance(value, str) and value:
        print("%s\t%s" % (field, value))
keys = cfg.get("keys")
if isinstance(keys, dict):
    for name, value in sorted(keys.items()):
        if isinstance(value, str) and value:
            print("keys[%s]\t%s" % (name, value))
key = cfg.get("key")
if isinstance(key, str) and key:
    print("key\t%s" % key)
PY
  done
  return 1
}

# ---- 1. Real values from the gitignored local config -----------------------
if [ -f "$CONFIG" ]; then
  if ! CONFIG_VALUES="$(read_config_values)" || [ -z "$CONFIG_VALUES" ]; then
    # A machine holding weflow-server.json must actually run the most
    # sensitive checks. Failing silently (no python, the Windows Store stub,
    # broken JSON) would print 通过 while real wxid/key leaks go unchecked —
    # block until the environment can read the config.
    echo "[隐私检查] 错误：存在 $CONFIG 但无法读取其中的 wxid/keys（python/python3/py 不可用，或 JSON 损坏？）。最敏感的泄露检查无法运行，拒绝放行；请安装 Python 后重试。" >&2
    exit 1
  fi
  while IFS=$'\t' read -r label value; do
    # Belt and braces against CRLF from any interpreter: a trailing \r would
    # turn every search into "<secret>\r" and match nothing (silent pass).
    value="${value%$'\r'}"
    [ -n "${value:-}" ] || continue
    case "$label" in
      wxid)        report "真实 wxid（$CONFIG）" "$value" ;;
      db_path)     report "真实数据库路径（$CONFIG 的 db_path）" "$value" ;;
      img_aes_key) report "真实图片 AES 密钥（$CONFIG 的 img_aes_key）" "$value" ;;
      *)           report "真实数据库密钥（$CONFIG 的 $label）" "$value" ;;
    esac
  done <<< "$CONFIG_VALUES"
fi

# ---- 2. Generic 64-hex literals outside the fixture allowlist --------------
# The enc_key shape. This layer works even without the local config, so a
# leaked key from another machine still gets caught.
while IFS= read -r found_hex; do
  found_hex="${found_hex%$'\r'}"
  [ -n "$found_hex" ] || continue
  allowed=0
  for fixture in "${FIXTURE_HEX[@]}"; do
    if [ "$found_hex" = "$fixture" ]; then
      allowed=1
      break
    fi
  done
  [ "$allowed" -eq 1 ] && continue
  # Report by shape, never the value itself.
  report "疑似数据库密钥（64 位 hex 字面量，不在 fixture 白名单中）" "$found_hex"
done < <(git grep -ohE '[0-9a-fA-F]{64}' -- "${PATHSPEC[@]}" 2>/dev/null | sort -u)

# ---- 3. Machine-specific paths / usernames --------------------------------
# MSYS bash's `whoami` prints the bare username; Windows' `whoami.exe` prints
# `HOSTNAME\user`, and the UPN form is `user@domain`. Normalize to the bare
# name: `C:\Users\$USER_NAME` needs it, and the bare name is a substring of
# every other form.
#
# The scan is case-SENSITIVE (`grep -F`, no -i) on purpose: a short lowercase
# username can appear as a case-different substring inside CamelCase Windows
# API names (e.g. ReadDirectoryChangesW), which is not a leak.
USER_NAME="$(whoami 2>/dev/null | sed 's/.*[\\/@]//' || true)"
if [ -n "$USER_NAME" ]; then
  report "本机用户名" "$USER_NAME"
  report "用户目录绝对路径 C:\\Users\\<用户名>" "C:\\Users\\$USER_NAME"
fi

if [ "$hits" -gt 0 ]; then
  echo "[隐私检查] 发现 $hits 类敏感信息泄露，请清理后再继续。" >&2
  echo "  确认为误报时，请人工复核后用 git commit --no-verify 跳过（谨慎）。" >&2
  exit 1
fi
echo "[隐私检查] 通过：未发现本机特定信息（wxid / 密钥 / 路径 / 用户名）。"
exit 0
