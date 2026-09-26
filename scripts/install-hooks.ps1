# 安装 weflow-server 的提交钩子：pre-commit（隐私 + 编号引用）与 commit-msg（提交信息）
# 用法: powershell -ExecutionPolicy Bypass -File scripts/install-hooks.ps1
#
# .git/hooks/ 不受版本控制，所以钩子需要每份 clone 各自安装一次。幂等，可重复执行。
$ErrorActionPreference = "Stop"

$root = git rev-parse --show-toplevel
if (-not $root) {
    Write-Error "当前目录不在 git 仓库内"
    exit 1
}

$hookDir = Join-Path $root ".git\hooks"
New-Item -ItemType Directory -Force -Path $hookDir | Out-Null

function Install-Hook([string]$name, [string]$body) {
    $path = Join-Path $hookDir $name
    # LF + 无 BOM：钩子由 sh 执行，CRLF 或 BOM 会导致 shebang 解析失败
    $body = $body -replace "`r`n", "`n"
    [System.IO.File]::WriteAllText($path, $body, (New-Object System.Text.UTF8Encoding $false))
    Write-Host "已安装 $name 钩子: $path"
}

$preCommit = @'
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

# 解释器解析：$PYTHON -> python3 -> python，且**实际执行** --version 校验。
# 不能只靠 command -v：Windows 上 python3 常被应用执行别名占据，命令存在但不能用。
py=""
for candidate in "$PYTHON" python3 python; do
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
'@

$commitMsg = @'
#!/bin/sh
# weflow-server 提交信息检查：编号引用（由 scripts/install-hooks.* 安装，可重复安装覆盖）
#
# 参数 $1 = 提交信息文件路径（git 传入）。只扫会真正进入提交的部分：
# 注释行与 scissors 之后的 diff 由扫描器自行剥掉。
#
# Python 缺失时同样阻止提交（与 pre-commit 同规）。

py=""
for candidate in "$PYTHON" python3 python; do
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
'@

Install-Hook "pre-commit" $preCommit
Install-Hook "commit-msg" $commitMsg
