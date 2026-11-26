#!/usr/bin/env pwsh
# Builds the release binaries and packages them into a setup executable.
$ErrorActionPreference = 'Stop'

function Find-Iscc {
    $onPath = Get-Command iscc -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    # Inno installs per-user when winget runs unelevated and per-machine when
    # chocolatey runs elevated in CI, so both are checked rather than assumed.
    $candidates = @(
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )
    foreach ($c in $candidates) { if (Test-Path $c) { return $c } }
    return $null
}

$iscc = Find-Iscc
if (-not $iscc) {
    throw "Inno Setup is not installed. Install it with:`n  winget install --id JRSoftware.InnoSetup --exact"
}

# The single source of truth for the version is the workspace manifest.
$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Building Headset Tray $version with $iscc"

# Panic messages from dependencies embed their source paths, which live under
# the builder's profile (`C:\Users\<name>\.cargo\registry\...`). Rewrite those
# prefixes so a published binary does not carry the builder's account name.
# Passed with --config rather than RUSTFLAGS: Cargo merges a --config array
# with `.cargo/config.toml`'s, whereas RUSTFLAGS would replace it.
$hostTriple = ((rustc -vV) | Select-String '^host:\s*(\S+)').Matches[0].Groups[1].Value
$prefixes = @($env:USERPROFILE)
if ($env:CARGO_HOME -and -not $env:CARGO_HOME.StartsWith($env:USERPROFILE)) {
    $prefixes += $env:CARGO_HOME
}
# TOML literal strings ('...') keep the backslashes as they are.
$remap = ($prefixes | ForEach-Object { "'--remap-path-prefix=$_=~'" }) -join ', '

# --locked: the lockfile's pins are load-bearing (see Cargo.toml), so a stale
# lockfile must fail the build rather than be silently re-resolved.
cargo build --workspace --release --locked --config "target.$hostTriple.rustflags=[$remap]"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

New-Item -ItemType Directory -Force dist | Out-Null
& $iscc "/DAppVersion=$version" installer\headset-tray.iss
if ($LASTEXITCODE -ne 0) { throw "Inno Setup failed" }

Get-ChildItem dist\*-setup.exe | Select-Object Name, Length
