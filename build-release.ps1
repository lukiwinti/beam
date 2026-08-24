[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
$cargoPath = if ($null -ne $cargoCommand) {
    $cargoCommand.Source
} else {
    Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
}
if (-not (Test-Path -LiteralPath $cargoPath)) {
    throw 'Cargo wurde nicht gefunden. Bitte zuerst Rust über https://rustup.rs installieren.'
}
$env:CARGO_TARGET_DIR = Join-Path $PSScriptRoot 'build'
$env:RUSTFLAGS = '-C target-feature=+crt-static'

& $cargoPath build --release
if ($LASTEXITCODE -ne 0) {
    throw "Der Release-Build ist mit Exitcode $LASTEXITCODE fehlgeschlagen."
}

$executable = Join-Path $env:CARGO_TARGET_DIR 'release\tesla-screen-sender.exe'
if (-not (Test-Path -LiteralPath $executable)) {
    throw "Die erwartete EXE wurde nicht erzeugt: $executable"
}

$file = Get-Item -LiteralPath $executable
$hash = Get-FileHash -LiteralPath $executable -Algorithm SHA256

Write-Host ''
Write-Host 'Release erfolgreich gebaut:' -ForegroundColor Green
Write-Host "  Datei:  $($file.FullName)"
Write-Host "  Größe:  $([Math]::Round($file.Length / 1MB, 2)) MB"
Write-Host "  SHA256: $($hash.Hash)"
