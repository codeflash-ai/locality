$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = Resolve-Path (Join-Path $ScriptDir "..\..\..")
$Out = Join-Path $Root "apps\desktop\src-tauri\windows"
. (Join-Path $Root "scripts\windows-codesign.ps1")
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Sidecars = @(
    (Join-Path $Out "afs.exe"),
    (Join-Path $Out "afsd.exe"),
    (Join-Path $Out "afs-cloud-files.exe")
)

if ($env:AFS_WINDOWS_BUNDLE_PREPARED -ne "1") {
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
        & $Cargo build -p afs-cli -p afsd -p afs-cloud-files --release
    } finally {
        Pop-Location
    }

    Copy-Item -LiteralPath (Join-Path $Root "target\release\afs.exe") -Destination (Join-Path $Out "afs.exe") -Force
    Copy-Item -LiteralPath (Join-Path $Root "target\release\afsd.exe") -Destination (Join-Path $Out "afsd.exe") -Force
    Copy-Item -LiteralPath (Join-Path $Root "target\release\afs-cloud-files.exe") -Destination (Join-Path $Out "afs-cloud-files.exe") -Force
} else {
    foreach ($Sidecar in $Sidecars) {
        if (-not (Test-Path -LiteralPath $Sidecar)) {
            throw "AFS_WINDOWS_BUNDLE_PREPARED=1 but missing prepared sidecar: $Sidecar"
        }
    }
}

if (Test-AfsWindowsCodeSigningRequested) {
    foreach ($Sidecar in $Sidecars) {
        [void] (Invoke-AfsWindowsCodeSign -Path $Sidecar)
        Assert-AfsWindowsSigned -Path $Sidecar
    }
}

Write-Host "Prepared Windows CLI in $(Join-Path $Out 'afs.exe')"
Write-Host "Prepared Windows daemon in $(Join-Path $Out 'afsd.exe')"
Write-Host "Prepared Windows Cloud Files helper in $(Join-Path $Out 'afs-cloud-files.exe')"
