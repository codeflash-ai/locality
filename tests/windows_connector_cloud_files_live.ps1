param(
    [ValidateSet("gmail", "slack", "linear", "granola")]
    [string] $Connector = $env:LOCALITY_LIVE_CONNECTOR
)

$ErrorActionPreference = "Stop"

if ($env:LOCALITY_WINDOWS_CONNECTOR_CLOUD_FILES_LIVE -ne "1") {
    Write-Host "skip: set LOCALITY_WINDOWS_CONNECTOR_CLOUD_FILES_LIVE=1 to run connector Cloud Files scenarios"
    exit 0
}
if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "Windows connector Cloud Files live test requires PowerShell 7+"
}
$isWindowsHost = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows
)
if (-not $isWindowsHost) {
    if ($env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE_REQUIRED -eq "1") {
        throw "Windows is required"
    }
    Write-Host "skip: Windows is required"
    exit 0
}
if (-not $Connector) {
    throw "LOCALITY_LIVE_CONNECTOR or -Connector is required"
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$locBin = if ($env:LOCALITY_BIN) { $env:LOCALITY_BIN } else { Join-Path $repoRoot "target\debug\loc.exe" }
$localitydBin = if ($env:LOCALITYD_BIN) { $env:LOCALITYD_BIN } else { Join-Path $repoRoot "target\debug\localityd.exe" }
$cloudFilesBin = if ($env:LOCALITY_CLOUD_FILES_BIN) { $env:LOCALITY_CLOUD_FILES_BIN } else { Join-Path $repoRoot "target\debug\locality-cloud-files.exe" }
$python = if ($env:PYTHON) { $env:PYTHON } else { "python" }
if (-not ((Test-Path -LiteralPath $locBin) -and (Test-Path -LiteralPath $localitydBin) -and (Test-Path -LiteralPath $cloudFilesBin))) {
    Push-Location $repoRoot
    try {
        cargo build -p localityd -p loc-cli -p locality-cloud-files
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build for Windows Cloud Files live binaries failed"
        }
    } finally {
        Pop-Location
    }
}

$unique = "{0}-{1}" -f (Get-Date).ToUniversalTime().ToString("yyyyMMdd-HHmmss"), ([Guid]::NewGuid().ToString("N").Substring(0, 8))
$providerRoot = if ($env:LOCALITY_WINDOWS_CONNECTOR_CLOUD_FILES_ROOT) {
    $env:LOCALITY_WINDOWS_CONNECTOR_CLOUD_FILES_ROOT
} else {
    Join-Path ([System.IO.Path]::GetTempPath()) "locality-connector-cloud-files-$Connector-$unique"
}
$createdRoot = -not $env:LOCALITY_WINDOWS_CONNECTOR_CLOUD_FILES_ROOT
New-Item -ItemType Directory -Path $providerRoot -Force | Out-Null
$env:LOCALITY_CLOUD_FILES_BIN = $cloudFilesBin

try {
    & $python (Join-Path $PSScriptRoot "live_connector_matrix.py") validate
    if ($LASTEXITCODE -ne 0) {
        throw "live connector matrix validation failed"
    }
    & $python (Join-Path $PSScriptRoot "live_provider_connector_scenario.py") `
        --connector $Connector `
        --projection windows-cloud-files `
        --provider-root $providerRoot `
        --loc $locBin `
        --localityd $localitydBin
    if ($LASTEXITCODE -ne 0) {
        throw "$Connector Windows Cloud Files scenario failed"
    }
} finally {
    if ($createdRoot -and (Test-Path -LiteralPath $providerRoot)) {
        $tempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
        $resolvedRoot = [System.IO.Path]::GetFullPath($providerRoot)
        if (-not $resolvedRoot.StartsWith($tempRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "refusing to clean provider root outside the temporary directory"
        }
        Remove-Item -LiteralPath $providerRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
