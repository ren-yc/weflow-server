# 安装 weflow-server 预提交隐私检查钩子（幂等，可重复执行）
# 用法: powershell -ExecutionPolicy Bypass -File scripts/install-hooks.ps1
#
# .git/hooks/ 不受版本控制，所以钩子需要每份 clone 各自安装一次。
$ErrorActionPreference = "Stop"

$root = git rev-parse --show-toplevel
if (-not $root) {
    Write-Error "当前目录不在 git 仓库内"
    exit 1
}

$hookDir = Join-Path $root ".git\hooks"
$hookPath = Join-Path $hookDir "pre-commit"
New-Item -ItemType Directory -Force -Path $hookDir | Out-Null

$hook = @'
#!/bin/sh
# weflow-server 预提交隐私检查（由 scripts/install-hooks.ps1 安装，可重复安装覆盖）
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
'@

# LF + 无 BOM：钩子由 sh 执行，CRLF 或 BOM 会导致 shebang 解析失败
$hook = $hook -replace "`r`n", "`n"
[System.IO.File]::WriteAllText($hookPath, $hook, (New-Object System.Text.UTF8Encoding $false))
Write-Host "已安装 pre-commit 钩子: $hookPath"
