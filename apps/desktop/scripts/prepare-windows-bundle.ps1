$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = Resolve-Path (Join-Path $ScriptDir "..\..\..")
$Out = Join-Path $Root "apps\desktop\src-tauri\windows"
$Cargo = $env:CARGO
if ([string]::IsNullOrWhiteSpace($Cargo)) {
    $CargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
    if ($CargoCommand) {
        $Cargo = $CargoCommand.Source
    }
}
if ([string]::IsNullOrWhiteSpace($Cargo)) {
    $CargoCandidate = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
    if (Test-Path -LiteralPath $CargoCandidate) {
        $Cargo = $CargoCandidate
    }
}
if ([string]::IsNullOrWhiteSpace($Cargo)) {
    throw "Could not find cargo. Install Rust or set the CARGO environment variable."
}

Push-Location $Root
try {
    & $Cargo build -p afs-cli -p afsd --release
} finally {
    Pop-Location
}

New-Item -ItemType Directory -Force -Path $Out | Out-Null
Copy-Item -LiteralPath (Join-Path $Root "target\release\afs.exe") -Destination (Join-Path $Out "afs.exe") -Force
Copy-Item -LiteralPath (Join-Path $Root "target\release\afsd.exe") -Destination (Join-Path $Out "afsd.exe") -Force

Write-Host "Prepared Windows CLI in $(Join-Path $Out 'afs.exe')"
Write-Host "Prepared Windows daemon in $(Join-Path $Out 'afsd.exe')"
