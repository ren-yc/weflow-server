# Build wrapper: locates the MSVC environment via vswhere (falling back to
# the legacy path), verifies perl/nasm/cargo, then runs cargo.
#
# Why: rusqlite bundles SQLCipher + vendored OpenSSL (openssl-src), whose
# perl Configure + nmake flow calls cl.exe/link.exe directly and needs
# INCLUDE/LIB/PATH from vcvars64.bat — it bypasses the cc crate's automatic
# MSVC detection. perl must be a native Windows Perl (Strawberry): Git's MSYS
# perl mangles Windows paths in Configure.
#
# Usage: powershell -File scripts\build.ps1 [cargo args...]
# Override the VS environment script with: $env:WEFLOW_VCVARS = "...vcvars64.bat"
$ErrorActionPreference = "Stop"

# --- 1. Locate vcvars64.bat: WEFLOW_VCVARS > vswhere > legacy fallback ---
$vcvars = $env:WEFLOW_VCVARS
if (-not $vcvars) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $vsPath = & $vswhere -latest -products "*" `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -property installationPath
        if ($vsPath) {
            $candidate = Join-Path $vsPath "VC\Auxiliary\Build\vcvars64.bat"
            if (Test-Path $candidate) { $vcvars = $candidate }
        }
    }
}
# Legacy fallback: vswhere is missing on some installs (and absent entirely
# for VS build-tools-only layouts), so probe the default install paths.
# Newest first, both Program Files roots, all editions.
if (-not $vcvars) {
    $roots = @($env:ProgramFiles, ${env:ProgramFiles(x86)}) | Where-Object { $_ }
    foreach ($root in $roots) {
        foreach ($ver in @("18", "2022", "17", "2019", "16")) {
            foreach ($ed in @("Community", "Professional", "Enterprise", "BuildTools", "Preview")) {
                $candidate = Join-Path $root "Microsoft Visual Studio\$ver\$ed\VC\Auxiliary\Build\vcvars64.bat"
                if (Test-Path $candidate) { $vcvars = $candidate; break }
            }
            if ($vcvars) { break }
        }
        if ($vcvars) { break }
    }
}
if (-not $vcvars) {
    throw "vcvars64.bat not found. Install Visual Studio with the 'Desktop development with C++' workload, or set WEFLOW_VCVARS to your vcvars64.bat."
}
if (-not (Test-Path $vcvars)) {
    throw "vcvars64.bat does not exist: $vcvars (check WEFLOW_VCVARS or your Visual Studio install)"
}
Write-Host "MSVC env script: $vcvars"

# --- 2. Toolchain prerequisites (prepend BEFORE capturing vcvars env) ---
$strawPerl = "C:\Strawberry\perl\bin"
if (Test-Path "$strawPerl\perl.exe") {
    $env:PATH = "$strawPerl;$env:PATH"
} else {
    $perlCmd = Get-Command perl -ErrorAction SilentlyContinue
    if ($perlCmd) {
        if ($perlCmd.Source -match "Git") {
            throw "Found Git's MSYS perl at $($perlCmd.Source). openssl-src requires native Windows Perl - install Strawberry Perl: https://strawberryperl.com"
        }
        Write-Warning "Using non-Strawberry perl: $($perlCmd.Source)"
    } else {
        throw "perl not found (openssl-src needs it to run Configure). Install Strawberry Perl: https://strawberryperl.com"
    }
}
if (-not (Get-Command nasm -ErrorAction SilentlyContinue)) {
    if (Test-Path "C:\Strawberry\c\bin\nasm.exe") {
        $env:PATH = "C:\Strawberry\c\bin;$env:PATH"
    } else {
        throw "nasm not found (OpenSSL x64 assembly). It ships with Strawberry Perl, or get it from https://nasm.us"
    }
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoPath = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
    if (Test-Path $cargoPath) {
        $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    } else {
        throw "cargo not found. Install via rustup: https://rustup.rs"
    }
}

# --- 3. Capture the vcvars environment into this process ---
$envBlock = cmd /c "`"$vcvars`" >nul 2>&1 && set"
if ($LASTEXITCODE -ne 0) { throw "vcvars64.bat failed (exit $LASTEXITCODE)" }
foreach ($line in $envBlock) {
    $i = $line.IndexOf('=')
    if ($i -gt 0) { [Environment]::SetEnvironmentVariable($line.Substring(0, $i), $line.Substring($i + 1), "Process") }
}

# --- 4. Passthrough ---
Set-Location $PSScriptRoot\..

# `testing` feature 只影响**可见性**：默认构建下实现面是 `pub(crate)`（边界由编译器强制），
# 开了它才转成 `pub` —— 而集成测试在独立 crate 里，只能看见 `pub`。所以凡是编译测试的命令
# 都要带上它，否则报的是「模块是私有的」，与真正的问题无关。
#
# 在这里补而不是写进文档：忘记它得到的是一堆看不懂的隐私错误，而不是一个明确的提示。
# 已显式给过 feature 相关参数时不插手（尊重调用方的选择）。
# 判定只看 `--` **之前**的那段：`--` 之后是转发给测试二进制／rustc 的参数，那里出现同名词
# 不代表调用方给 cargo 指定过 feature（当成指定过就会漏注入，报错是一堆「模块是私有的」）。
$dash = [Array]::IndexOf($args, '--')
# 外层 @() 不可省：PowerShell 把 if 赋值的单元素数组摊平成标量，于是 `test`（恰好
# 一个参数，也就是最常用的门禁调用形态）会让 $cargoSide 变成字符串、$cargoSide[0] 取到
# 首字符 't'，判定为「不是 test」，testing 从此不再注入 —— 报的是一堆「模块是私有的」。
$cargoSide = @(if ($dash -lt 0) { $args } elseif ($dash -eq 0) { @() } else { $args[0..($dash - 1)] })
$restSide = @(if ($dash -lt 0) { @() } else { $args[$dash..($args.Count - 1)] })
# 子命令不假定在 $args[0]：--locked／--config 是 cargo 的全局选项，
# `build.ps1 --locked test` 合法且同样需要注入。这些全局选项的值紧跟其后，跳过值才轮得到子命令。
$valueOptions = @('-p', '--package', '--config', '-Z', '--target', '--target-dir', '--manifest-path', '--color', '--message-format', '--profile')
$subIndex = -1
for ($i = 0; $i -lt $cargoSide.Count; $i++) {
    $tok = [string]$cargoSide[$i]
    if ($tok.Length -eq 0) { continue }
    if ($tok[0] -eq '-') {
        if ($valueOptions -contains $tok) { $i += 1 }
        continue
    }
    $subIndex = $i
    break
}
$sub = if ($subIndex -ge 0) { [string]$cargoSide[$subIndex] } else { '' }
# 调用方自己指定过 feature 就不插手（尊重选择）；`--features=x` 与 `-Fxyz` 的合并形态
# 也算指定过——早先只比 `-contains '--features'`，这两种写法会被当成没指定，于是重复注入。
$alreadyHas = $false
foreach ($tok in $cargoSide) {
    $t = [string]$tok
    if ($t -eq '--all-features' -or $t -eq '--no-default-features' -or $t -eq '--features' -or $t -eq '-F') { $alreadyHas = $true; break }
    if ($t.StartsWith('--features=') -or ($t.Length -gt 2 -and $t.StartsWith('-F'))) { $alreadyHas = $true; break }
}
# `-p <crate>` 选中的是另一个 package：`testing` 只存在于根 package，注入了会让 cargo 直接报
# 「the package does not contain this feature」，而这不是调用方的本意（SDK 的测试用公开 API，不需要它）。
$selectsPackage = $false
foreach ($tok in $cargoSide) {
    $t = [string]$tok
    if ($t -eq '-p' -or $t -eq '--package' -or ($t.Length -gt 2 -and $t.StartsWith('-p'))) { $selectsPackage = $true; break }
}
$needsTesting = ($sub -eq 'test') -or ($sub -eq 'clippy' -and $cargoSide -contains '--all-targets')
if ($needsTesting -and -not $alreadyHas -and -not $selectsPackage -and $subIndex -ge 0) {
    # Select-Object / 切片而非下标区间：单参数调用会算出区间 1..0，PowerShell 把端点
    # 取整回绕成「再取一次首元素」，调用方的首参被注入第二遍——build.ps1 test
    # 变成 cargo test test，第二个 test 沦为过滤词，全量测试被静默换成零匹配。
    # 注入点紧跟子命令，而不是硬插在数组最前面：`--locked test` 要保持 --locked 在前。
    $head = @(if ($subIndex -eq 0) { @() } else { $cargoSide[0..($subIndex - 1)] })
    $tail = @(if ($subIndex + 1 -lt $cargoSide.Count) { $cargoSide[($subIndex + 1)..($cargoSide.Count - 1)] } else { @() })
    $args = $head + @($sub) + @('--features', 'testing') + $tail + $restSide
    Write-Host 'build.ps1: 已补 --features testing（集成测试需要它才看得见实现面）'
}
& cargo @args
exit $LASTEXITCODE
