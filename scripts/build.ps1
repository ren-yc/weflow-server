# Build wrapper: locates the MSVC environment via vswhere (falling back to
# the legacy path), verifies perl/nasm/cargo, then runs cargo.
#
# Why: rusqlite bundles SQLCipher + vendored OpenSSL (openssl-src), whose
# perl Configure + nmake flow calls cl.exe/link.exe directly and needs
# INCLUDE/LIB/PATH from vcvars64.bat — it bypasses the cc crate's automatic
# MSVC detection. perl must be a native Windows Perl (Strawberry).
#
# Usage: powershell -File scripts\build.ps1 [cargo args...]
# Override the VS environment script with: $env:WEFLOW_VCVARS = "...vcvars64.bat"
$ErrorActionPreference = "Stop"

# --- 1. Locate vcvars64.bat ---
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
if (-not $vcvars) { throw "vcvars64.bat not found. Set WEFLOW_VCVARS or install VS with the C++ workload." }
if (-not (Test-Path $vcvars)) { throw "vcvars64.bat does not exist: $vcvars" }
Write-Host "MSVC env script: $vcvars"

# --- 2. Toolchain prerequisites ---
$strawPerl = "C:\Strawberry\perl\bin"
if (Test-Path "$strawPerl\perl.exe") {
    $env:PATH = "$strawPerl;$env:PATH"
} else {
    $perlCmd = Get-Command perl -ErrorAction SilentlyContinue
    if ($perlCmd) {
        if ($perlCmd.Source -match "Git") {
            throw "Found Git's MSYS perl; openssl-src requires native Windows Perl (Strawberry): https://strawberryperl.com"
        }
        Write-Warning "Using non-Strawberry perl: $($perlCmd.Source)"
    } else {
        throw "perl not found (openssl-src needs it). Install Strawberry Perl."
    }
}
if (-not (Get-Command nasm -ErrorAction SilentlyContinue)) {
    if (Test-Path "C:\Strawberry\c\bin\nasm.exe") {
        $env:PATH = "C:\Strawberry\c\bin;$env:PATH"
    } else {
        throw "nasm not found (OpenSSL x64 assembly). It ships with Strawberry Perl."
    }
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoPath = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
    if (Test-Path $cargoPath) {
        $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    } else {
        throw "cargo not found. Install via rustup."
    }
}

# --- 3. Capture vcvars environment ---
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