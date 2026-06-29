$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = Resolve-Path (Join-Path $ScriptDir "..")
$DesktopDir = Join-Path $Root "apps\desktop"
$NsisDir = Join-Path $Root "target\release\bundle\nsis"
$WindowsOutDir = Join-Path $Root "target\release\bundle\windows"
$UpdaterDir = Join-Path $Root "target\release\bundle\updater"
$ProductName = if ($env:PUBLISH_PRODUCT_NAME) { $env:PUBLISH_PRODUCT_NAME } else { "Locality" }
$Channel = if ($env:PUBLISH_CHANNEL) { $env:PUBLISH_CHANNEL } else { "beta" }
$DateStamp = if ($env:PUBLISH_DATE) { $env:PUBLISH_DATE } else { (Get-Date).ToUniversalTime().ToString("yyyyMMdd") }
$UpdaterEndpoint = if ($env:TAURI_UPDATER_ENDPOINT) {
    $env:TAURI_UPDATER_ENDPOINT
} else {
    "https://github.com/codeflash-ai/locality/releases/latest/download/latest-windows.json"
}

. (Join-Path $Root "scripts\windows-codesign.ps1")

function Write-Log {
    param([string] $Message)
    Write-Host "publish-windows: $Message"
}

function Fail {
    param([string] $Message)
    throw "publish-windows: error: $Message"
}

function Require-Command {
    param([string] $Name)
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        Fail "missing required command: $Name"
    }
}

function Assert-CleanTree {
    if ($env:PUBLISH_ALLOW_DIRTY -eq "1") {
        return
    }
    $status = & git -C $Root status --porcelain
    if ($LASTEXITCODE -ne 0) {
        Fail "git status failed"
    }
    if ($status) {
        Fail "working tree has uncommitted changes; commit them first or set PUBLISH_ALLOW_DIRTY=1"
    }
}

function Get-WindowsArch {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) {
        $arch = $env:PROCESSOR_ARCHITEW6432
    }
    switch -Regex ($arch) {
        "^(AMD64|x86_64)$" { return "x86_64" }
        "^(ARM64|AARCH64)$" { return "aarch64" }
        default { return $arch.ToLowerInvariant() }
    }
}

function Get-TauriBuildConfig {
    if ([string]::IsNullOrWhiteSpace($env:TAURI_UPDATER_PUBKEY)) {
        return "{}"
    }
    if ([string]::IsNullOrWhiteSpace($env:TAURI_SIGNING_PRIVATE_KEY)) {
        Fail "TAURI_UPDATER_PUBKEY is set but TAURI_SIGNING_PRIVATE_KEY is missing"
    }
    $config = @{
        bundle = @{
            createUpdaterArtifacts = $true
        }
        plugins = @{
            updater = @{
                pubkey = $env:TAURI_UPDATER_PUBKEY
                endpoints = @($UpdaterEndpoint)
            }
        }
    }
    return ($config | ConvertTo-Json -Depth 8 -Compress)
}

function Latest-Artifact {
    param(
        [string] $Directory,
        [scriptblock] $Predicate
    )
    Get-ChildItem -LiteralPath $Directory -File -ErrorAction SilentlyContinue |
        Where-Object $Predicate |
        Sort-Object LastWriteTimeUtc, FullName |
        Select-Object -Last 1
}

function Write-Sha256 {
    param([string] $Path)
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
    $line = "$hash  $(Split-Path -Leaf $Path)"
    Set-Content -LiteralPath "$Path.sha256" -Value $line -Encoding ascii
}

function Copy-ReleaseArtifact {
    param(
        [string] $Source,
        [string] $Destination
    )
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
    Write-Sha256 -Path $Destination
}

function Assert-SidecarsSigned {
    $sidecarDir = Join-Path $Root "apps\desktop\src-tauri\windows"
    foreach ($name in @("loc.exe", "localityd.exe", "locality-cloud-files.exe")) {
        Assert-LocalityWindowsSigned -Path (Join-Path $sidecarDir $name)
    }
}

function Copy-UpdaterArtifacts {
    param(
        [string] $CommitShort,
        [string] $Arch
    )
    if ([string]::IsNullOrWhiteSpace($env:TAURI_UPDATER_PUBKEY)) {
        Write-Log "Windows updater artifacts disabled; set TAURI_UPDATER_PUBKEY and TAURI_SIGNING_PRIVATE_KEY to enable"
        return
    }

    $artifact = Latest-Artifact -Directory $NsisDir -Predicate {
        $_.Name.EndsWith(".nsis.zip", [System.StringComparison]::OrdinalIgnoreCase) -or
        $_.Name.EndsWith(".msi.zip", [System.StringComparison]::OrdinalIgnoreCase)
    }
    if (-not $artifact) {
        Fail "Tauri did not produce a Windows updater zip artifact"
    }
    if (-not (Test-Path -LiteralPath "$($artifact.FullName).sig")) {
        Fail "Tauri did not produce $($artifact.FullName).sig"
    }

    $extension = if ($artifact.Name.EndsWith(".msi.zip", [System.StringComparison]::OrdinalIgnoreCase)) {
        "msi.zip"
    } else {
        "nsis.zip"
    }
    New-Item -ItemType Directory -Force -Path $UpdaterDir | Out-Null
    $versioned = Join-Path $UpdaterDir "$ProductName-$Channel-$DateStamp-$CommitShort-windows-$Arch.$extension"
    $alias = Join-Path $UpdaterDir "$ProductName-$Channel-windows-$Arch.$extension"
    Copy-Item -LiteralPath $artifact.FullName -Destination $versioned -Force
    Copy-Item -LiteralPath "$($artifact.FullName).sig" -Destination "$versioned.sig" -Force
    Copy-Item -LiteralPath $artifact.FullName -Destination $alias -Force
    Copy-Item -LiteralPath "$($artifact.FullName).sig" -Destination "$alias.sig" -Force
    Write-Log "published updater artifact $alias"
}

Require-Command git
Require-Command npm
Require-Command cargo
Assert-CleanTree

if ($env:PUBLISH_REQUIRE_SIGNING -eq "1" -and -not (Test-LocalityWindowsCodeSigningRequested) -and -not (Test-LocalityWindowsExternalCodeSigningRequested)) {
    Fail "PUBLISH_REQUIRE_SIGNING=1 requires either LOCALITY_WINDOWS_CODESIGN=1 with a signing certificate selector or LOCALITY_WINDOWS_EXTERNAL_CODESIGN=1"
}

$commitShort = (& git -C $Root rev-parse --short=7 HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($commitShort)) {
    Fail "could not read git commit"
}
$arch = Get-WindowsArch
$configJson = Get-TauriBuildConfig

Write-Log "commit $commitShort"
Write-Log "architecture $arch"
if (Test-LocalityWindowsCodeSigningRequested) {
    Write-Log "Windows Authenticode signing enabled"
} elseif (Test-LocalityWindowsExternalCodeSigningRequested) {
    Write-Log "Windows Authenticode signing delegated to external workflow steps"
} else {
    Write-Log "Windows Authenticode signing disabled"
}

if (-not (Test-Path -LiteralPath (Join-Path $DesktopDir "node_modules\.package-lock.json"))) {
    Write-Log "installing desktop npm dependencies"
    & npm --prefix $DesktopDir ci
    if ($LASTEXITCODE -ne 0) {
        Fail "npm ci failed"
    }
}

Remove-Item -LiteralPath $NsisDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $WindowsOutDir | Out-Null

Write-Log "building Tauri NSIS package"
Push-Location $Root
try {
    & npm --prefix $DesktopDir run tauri -- build --bundles nsis --config $configJson
    if ($LASTEXITCODE -ne 0) {
        Fail "Tauri Windows build failed"
    }
} finally {
    Pop-Location
}

if (Test-LocalityWindowsCodeSigningRequested -or Test-LocalityWindowsExternalCodeSigningRequested) {
    Assert-SidecarsSigned
}

$installer = Latest-Artifact -Directory $NsisDir -Predicate {
    $_.Extension -ieq ".exe" -and $_.Name -match "setup|installer|Locality"
}
if (-not $installer) {
    Fail "Tauri did not produce a Windows NSIS installer"
}

if (Test-LocalityWindowsCodeSigningRequested) {
    [void] (Invoke-LocalityWindowsCodeSign -Path $installer.FullName)
    Assert-LocalityWindowsSigned -Path $installer.FullName
} elseif (Test-LocalityWindowsExternalCodeSigningRequested) {
    Write-Log "Windows installer signing deferred to external workflow step"
} elseif ($env:PUBLISH_REQUIRE_SIGNING -eq "1") {
    Fail "Windows installer is unsigned"
}

$versionedInstaller = Join-Path $WindowsOutDir "$ProductName-$Channel-$DateStamp-$commitShort-windows-$arch-setup.exe"
$aliasInstaller = Join-Path $WindowsOutDir "$ProductName-$Channel-windows-$arch-setup.exe"
Copy-ReleaseArtifact -Source $installer.FullName -Destination $versionedInstaller
Copy-ReleaseArtifact -Source $installer.FullName -Destination $aliasInstaller
Copy-UpdaterArtifacts -CommitShort $commitShort -Arch $arch

Write-Host ""
Write-Host "Published Windows installer:"
Write-Host "  $versionedInstaller"
Write-Host "  $versionedInstaller.sha256"
Write-Host "  $aliasInstaller"
Write-Host "  $aliasInstaller.sha256"
