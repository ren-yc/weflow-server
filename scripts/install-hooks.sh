#!/usr/bin/env bash
# 安装 weflow-server 预提交隐私检查钩子（幂等，可重复执行）
# 用法: bash scripts/install-hooks.sh
#
# .git/hooks/ 不受版本控制，所以钩子需要每份 clone 各自安装一次。
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
if [ -z "$root" ]; then
  echo "当前目录不在 git 仓库内" >&2
  exit 1
fi

hook_dir="$root/.git/hooks"
hook_path="$hook_dir/pre-commit"
mkdir -p "$hook_dir"

cat > "$hook_path" <<'HOOK'
#!/bin/sh
# weflow-server 预提交隐私检查（由 scripts/install-hooks.sh 安装，可重复安装覆盖）
#
# check-privacy.sh 用到 bash 数组，必须用 bash 而非 sh 执行。找不到 bash 时
# 阻止提交而非静默跳过：隐私检查失败开放（fail-open）等于没有检查，而 bash
# 本来就是本仓库的硬依赖（scripts/build.sh 同样需要）。
if command -v bash >/dev/null 2>&1; then
    exec bash scripts/check-privacy.sh
fi
echo "[隐私检查] 未找到 bash，无法执行 scripts/check-privacy.sh；提交已阻止。" >&2
echo "  安装 bash 后重试，或人工复核后用 git commit --no-verify 跳过（谨慎）。" >&2
exit 1
HOOK

chmod +x "$hook_path"
echo "已安装 pre-commit 钩子: $hook_path"
