# Builds discommand and its WhatsApp helper and installs both into ~/.cargo/bin
# (already on PATH when Rust is installed). Re-run after changes to update.
# Works on Windows x64 and ARM64.
$ErrorActionPreference = 'Stop'
foreach ($tool in 'cargo', 'go') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "$tool not found. Install Rust (https://rustup.rs) and Go (https://go.dev/dl), then open a new terminal."
    }
}
$bin = Join-Path $env:USERPROFILE '.cargo\bin'
$cargoArgs = @()
# Windows on ARM without Visual Studio: use llvm-mingw + the gnullvm toolchain.
$arm64 = $env:PROCESSOR_ARCHITECTURE -eq 'ARM64'
if ($arm64 -and (Test-Path "$env:LOCALAPPDATA\llvm-mingw\bin")) {
    $env:PATH = "$env:LOCALAPPDATA\llvm-mingw\bin;$env:PATH"
    $cargoArgs += '+stable-aarch64-pc-windows-gnullvm'
}

Push-Location $PSScriptRoot
try {
    if (Test-Path 'keys\keys.env') { Write-Host 'Building with keys\keys.env (one-step Google/Microsoft sign-in).' }
    else { Write-Host 'No keys\keys.env: building without built-in Google/Microsoft app IDs (see keys\README.md).' }

    cargo @cargoArgs install --path . --force
    if ($LASTEXITCODE) { throw 'cargo install failed' }

    Push-Location wa
    $env:CGO_ENABLED = '0'
    go build -trimpath -ldflags '-s -w' -o (Join-Path $bin 'discommand-wa.exe') .
    if ($LASTEXITCODE) { throw 'go build failed' }
    Pop-Location

    Write-Host "Installed discommand and discommand-wa to $bin"
} finally {
    Pop-Location
}
