#!/usr/bin/env bash
# 从调用方自备的本地配置注册一个账号（重启后恢复用）。
# 只读配置、不写盘、不回显 token 或密钥；token 放请求头，正文走 stdin，
# 两者都不进 curl 的命令行参数（进程列表看不到）。
set -euo pipefail

cfg=${1:?usage: register-accounts.sh <config.json>}
[ -r "$cfg" ] || { echo "配置不可读: $cfg" >&2; exit 2; }

py=${PYTHON:-python3}
base=$("$py" -c 'import json,sys; print(json.load(open(sys.argv[1]))["base_url"].rstrip("/"))' "$cfg")
token=$("$py" -c 'import json,sys; print(json.load(open(sys.argv[1]))["token"])' "$cfg")
body=$("$py" - "$cfg" <<'PY'
import json
import sys

cfg = json.load(open(sys.argv[1]))
out = {"wxid": cfg["wxid"]}
for key in ("db_path", "keys", "img_aes_key", "img_xor_key"):
    if cfg.get(key):
        out[key] = cfg[key]
print(json.dumps(out, ensure_ascii=False))
PY
)

# token 与正文都经进程替换交给 curl：不进命令行参数，也不落盘。
printf '%s' "$body" | curl -sS --fail-with-body \
    --config <(printf 'header = "Authorization: Bearer %s"\n' "$token") \
    -H 'Content-Type: application/json' --data @- "$base/api/v1/accounts"
