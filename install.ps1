# Builds discommand and its WhatsApp helper and installs both into ~/.cargo/bin
# (already on PATH when Rust is installed). Re-run after changes to update.
$ErrorActionPreference = 'Stop'
$bin = Join-Path $env:USERPROFILE '.cargo\bin'
$cargoArgs = @()
# Windows on ARM without Visual Studio: use llvm-mingw + the gnullvm toolchain.
if (Test-Path "$env:LOCALAPPDATA\llvm-mingw\bin") {
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
