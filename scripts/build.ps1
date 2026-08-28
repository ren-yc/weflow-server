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
& cargo @args
exit $LASTEXITCODE
