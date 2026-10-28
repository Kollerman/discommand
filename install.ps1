# Builds discommand and its WhatsApp helper and installs both into ~/.cargo/bin
# (already on PATH when Rust is installed). Re-run after changes to update.
# Works on Windows x64 and ARM64.
#
#   .\install.ps1                          asks which integrations to build
#   .\install.ps1 -With discord,calendar   only these: discord, whatsapp, calendar, todo, twos
#   .\install.ps1 -With all                back to all of them
param([string[]]$With)
$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    # The choice is remembered in .integrations (not in git) for later updates.
    $all = [ordered]@{ discord = 'Discord'; whatsapp = 'WhatsApp (needs Go)'; calendar = 'Google Calendar'; todo = 'Microsoft To Do'; twos = 'Twos' }
    $last = if (Test-Path .integrations) { (Get-Content .integrations -Raw).Trim() -split ',' }
    function Ask($question) {
        $a = (Read-Host "$question [Y/n]").Trim().ToLower()
        return $a -eq '' -or $a -eq 'y' -or $a -eq 'yes' -or $a -eq 'j' -or $a -eq 'ja'
    }
    if ($With) { $With = $With -split ',' | ForEach-Object { $_.Trim().ToLower() } | Where-Object { $_ } }
    elseif ([Console]::IsInputRedirected) { $With = $last }
    elseif ($last -and (Ask "Build with the same integrations as last time ($($last -join ', '))?")) { $With = $last }
    else {
        do {
            $With = @($all.Keys | Where-Object { Ask "  $($all[$_])" })
            if (-not $With) { Write-Host 'Choose at least one.' }
        } while (-not $With)
        if ($With.Count -eq $all.Count) { $With = @('all') }
    }
    if ($With -contains 'all') { $With = $null; Remove-Item .integrations -ErrorAction SilentlyContinue }
    $whatsapp = -not $With -or $With -contains 'whatsapp'

    $tools = @('cargo') + @(if ($whatsapp) { 'go' })
    foreach ($tool in $tools) {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
            throw "$tool not found. Install Rust (https://rustup.rs) and Go (https://go.dev/dl), then open a new terminal."
        }
    }
    $bin = Join-Path $env:USERPROFILE '.cargo\bin'
    $cargoArgs = @()
    # Windows on ARM without Visual Studio: use llvm-mingw + the gnullvm toolchain.
    if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -and (Test-Path "$env:LOCALAPPDATA\llvm-mingw\bin")) {
        $env:PATH = "$env:LOCALAPPDATA\llvm-mingw\bin;$env:PATH"
        $cargoArgs += '+stable-aarch64-pc-windows-gnullvm'
    }
    $cargoArgs += 'install', '--path', '.', '--force'
    if ($With) {
        $cargoArgs += '--no-default-features', '--features', ((@('native-tls') + $With) -join ',')
        Write-Host "Integrations: $($With -join ', ')"
    } else {
        Write-Host 'Integrations: all (choose with -With discord,whatsapp,calendar,todo,twos)'
    }

    if (Test-Path 'keys\keys.env') { Write-Host 'Building with keys\keys.env (one-step Google/Microsoft sign-in).' }
    else { Write-Host 'No keys\keys.env: building without built-in Google/Microsoft app IDs (see keys\README.md).' }

    cargo @cargoArgs
    if ($LASTEXITCODE) { throw 'cargo install failed' }
    if ($With) { Set-Content .integrations ($With -join ',') } elseif (Test-Path .integrations) { Remove-Item .integrations }

    if ($whatsapp) {
        Push-Location wa
        $env:CGO_ENABLED = '0'
        go build -trimpath -ldflags '-s -w' -o (Join-Path $bin 'discommand-wa.exe') .
        if ($LASTEXITCODE) { throw 'go build failed' }
        Pop-Location
        Write-Host "Installed discommand and discommand-wa to $bin"
    } else {
        Write-Host "Installed discommand to $bin"
    }
} finally {
    Pop-Location
}
