$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = Resolve-Path (Join-Path $ScriptDir "..\..\..")
$Out = Join-Path $Root "apps\desktop\src-tauri\windows"
$MountLogoSource = Join-Path $Root "apps\desktop\src-tauri\icons\locality-mount-logo.ico"
$MountLogoOut = Join-Path $Out "locality-mount-logo.ico"
. (Join-Path $Root "scripts\windows-codesign.ps1")
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Sidecars = @(
    (Join-Path $Out "loc.exe"),
    (Join-Path $Out "localityd.exe"),
    (Join-Path $Out "locality-cloud-files.exe")
)

if ($env:LOCALITY_WINDOWS_BUNDLE_PREPARED -ne "1") {
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
        & $Cargo build -p loc-cli -p localityd -p locality-cloud-files --release
    } finally {
        Pop-Location
    }

    Copy-Item -LiteralPath (Join-Path $Root "target\release\loc.exe") -Destination (Join-Path $Out "loc.exe") -Force
    Copy-Item -LiteralPath (Join-Path $Root "target\release\localityd.exe") -Destination (Join-Path $Out "localityd.exe") -Force
    Copy-Item -LiteralPath (Join-Path $Root "target\release\locality-cloud-files.exe") -Destination (Join-Path $Out "locality-cloud-files.exe") -Force
    Copy-Item -LiteralPath $MountLogoSource -Destination $MountLogoOut -Force
} else {
    foreach ($Sidecar in $Sidecars) {
        if (-not (Test-Path -LiteralPath $Sidecar)) {
            throw "LOCALITY_WINDOWS_BUNDLE_PREPARED=1 but missing prepared sidecar: $Sidecar"
        }
    }
    if (-not (Test-Path -LiteralPath $MountLogoOut)) {
        throw "LOCALITY_WINDOWS_BUNDLE_PREPARED=1 but missing prepared mount logo: $MountLogoOut"
    }
}

if (Test-LocalityWindowsCodeSigningRequested) {
    foreach ($Sidecar in $Sidecars) {
        [void] (Invoke-LocalityWindowsCodeSign -Path $Sidecar)
        Assert-LocalityWindowsSigned -Path $Sidecar
    }
}

Write-Host "Prepared Windows CLI in $(Join-Path $Out 'loc.exe')"
Write-Host "Prepared Windows daemon in $(Join-Path $Out 'localityd.exe')"
Write-Host "Prepared Windows Cloud Files helper in $(Join-Path $Out 'locality-cloud-files.exe')"
Write-Host "Prepared Windows mount logo in $MountLogoOut"
