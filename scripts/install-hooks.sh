#!/usr/bin/env bash
# 安装 weflow-server 的提交钩子：pre-commit（隐私 + 编号引用）与 commit-msg（提交信息）
# 用法: bash scripts/install-hooks.sh
#
# .git/hooks/ 不受版本控制，所以钩子需要每份 clone 各自安装一次。幂等，可重复执行。
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
if [ -z "$root" ]; then
  echo "当前目录不在 git 仓库内" >&2
  exit 1
fi

hook_dir="$root/.git/hooks"
mkdir -p "$hook_dir"

cat > "$hook_dir/pre-commit" <<'HOOK'
#!/bin/sh
# weflow-server 预提交检查：隐私 + 编号引用（由 scripts/install-hooks.* 安装，可重复安装覆盖）
#
# 两个检查**都要跑**，任一失败即阻止提交。这是刻意的：
#   * 不能用 exec —— exec 会替换当前进程，其后的检查永远不会执行，等于静默失效；
#   * 不能写成「check; scan」后只看最后一条退出码 —— 前一个失败会被后一个的成功掩盖；
#   * 不能写成「check && scan」 —— 隐私失败时编号检查根本不跑。
# 因此逐个执行、累积状态，最后统一判决。
#
# 退出码：编号检查 0 无命中 / 1 命中 / 2 扫描未执行（空 diff 不等于干净）。
# 「未执行」与「命中」的诊断文案刻意不同，否则 2 会被读成「干净」。
#
# bash 与 Python 3 都是本仓库的硬依赖（scripts/build.sh 需要 bash；编号扫描器是 Python）。
# 找不到时**阻止提交**而不是跳过：失败开放的检查等于没有检查。

status=0

if command -v bash >/dev/null 2>&1; then
    bash scripts/check-privacy.sh || status=1
else
    echo "[隐私检查] 未找到 bash，无法执行 scripts/check-privacy.sh；提交已阻止。" >&2
    echo "  安装 bash 后重试，或人工复核后用 git commit --no-verify 跳过（谨慎）。" >&2
    status=1
fi

# 解释器解析：$PYTHON -> python -> python3，且**实际执行** --version 校验。
# 不能只靠 command -v：Windows 上 python3 常被应用执行别名占据，命令存在但不能用。
# python 优先于 python3 是刻意的：那个别名 stub 在环境变量缺失的进程里运行时，
# 会在当前目录下创建字面命名的垃圾目录树（把注册表里未展开的 %SystemDrive% 路径
# 当成相对路径写盘）；排在循环末尾可以让常规机器永远不触到它。
py=""
for candidate in "$PYTHON" python python3; do
    [ -n "$candidate" ] || continue
    command -v "$candidate" >/dev/null 2>&1 || continue
    if "$candidate" --version 2>&1 | grep -q "Python 3"; then
        py="$candidate"
        break
    fi
done

if [ -n "$py" ]; then
    "$py" scripts/forbidden_refs.py
    rc=$?
    if [ "$rc" = "2" ]; then
        echo "[编号检查] 扫描未执行，按拒绝放行处理；提交已阻止（诊断见上）。" >&2
        status=1
    elif [ "$rc" != "0" ]; then
        status=1
    fi
else
    echo "[编号检查] 未找到可用的 Python 3，无法执行 scripts/forbidden_refs.py；提交已阻止。" >&2
    echo "  安装 Python 3 后重试，或人工复核后用 git commit --no-verify 跳过（谨慎）。" >&2
    status=1
fi

exit $status
HOOK

cat > "$hook_dir/commit-msg" <<'HOOK'
#!/bin/sh
# weflow-server 提交信息检查：编号引用（由 scripts/install-hooks.* 安装，可重复安装覆盖）
#
# 参数 $1 = 提交信息文件路径（git 传入）。只扫会真正进入提交的部分：
# 注释行与 scissors 之后的 diff 由扫描器自行剥掉。
#
# Python 缺失时同样阻止提交（与 pre-commit 同规）。

py=""
for candidate in "$PYTHON" python python3; do
    [ -n "$candidate" ] || continue
    command -v "$candidate" >/dev/null 2>&1 || continue
    if "$candidate" --version 2>&1 | grep -q "Python 3"; then
        py="$candidate"
        break
    fi
done

if [ -z "$py" ]; then
    echo "[编号检查·提交信息] 未找到可用的 Python 3；提交已阻止。" >&2
    echo "  安装 Python 3 后重试，或人工复核后用 git commit --no-verify 跳过（谨慎）。" >&2
    exit 1
fi

"$py" scripts/forbidden_refs.py --message-file "$1"
exit $?
HOOK

chmod +x "$hook_dir/pre-commit" "$hook_dir/commit-msg"
echo "已安装 pre-commit 与 commit-msg 钩子: $hook_dir"
