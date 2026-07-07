param()

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$scriptPath = Join-Path $repoRoot "scripts\clean-start.ps1"

function Assert-True {
    param(
        [bool] $Condition,
        [string] $Message
    )
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-Contains {
    param(
        [string] $Haystack,
        [string] $Needle
    )
    if (-not $Haystack.Contains($Needle)) {
        throw "expected output to contain '$Needle' but got:`n$Haystack"
    }
}

function New-TestRoot {
    $name = "loc-windows-clean-start-test-{0}" -f ([Guid]::NewGuid().ToString("N"))
    $path = Join-Path ([System.IO.Path]::GetTempPath()) $name
    New-Item -ItemType Directory -Path $path | Out-Null
    return $path
}

function Invoke-CleanStart {
    param([string[]] $CleanStartArguments)

    $output = & pwsh -NoProfile -ExecutionPolicy Bypass -File $scriptPath @CleanStartArguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "clean-start.ps1 failed with exit code $LASTEXITCODE`n$output"
    }
    return ($output -join "`n")
}

function Test-DryRunPlansWindowsCleanupWithoutDeleting {
    $root = New-TestRoot
    try {
        $stateDir = Join-Path $root "state"
        $installDir = Join-Path $root "install"
        New-Item -ItemType Directory -Path $stateDir | Out-Null
        New-Item -ItemType Directory -Path $installDir | Out-Null
        New-Item -ItemType File -Path (Join-Path $stateDir "state.sqlite3") | Out-Null
        New-Item -ItemType File -Path (Join-Path $installDir "Locality.exe") | Out-Null

        $output = Invoke-CleanStart -CleanStartArguments @(
            "-StateDir", $stateDir,
            "-InstallDir", $installDir
        )

        Assert-Contains $output "Locality Windows clean-start"
        Assert-Contains $output "Mode: dry run."
        Assert-Contains $output "--prepare-uninstall"
        Assert-Contains $output $stateDir
        Assert-Contains $output $installDir
        Assert-True (Test-Path -LiteralPath $stateDir) "dry run removed the state directory"
        Assert-True (Test-Path -LiteralPath $installDir) "dry run removed the install directory"
    } finally {
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Test-YesRemovesStateInstallAndManagedShims {
    $root = New-TestRoot
    $oldLocalAppData = $env:LOCALAPPDATA
    $oldUserProfile = $env:USERPROFILE
    try {
        $stateDir = Join-Path $root "state"
        $installDir = Join-Path $root "install"
        $localAppData = Join-Path $root "local-app-data"
        $profile = Join-Path $root "profile"
        $mountRoot = Join-Path $profile "Locality"
        $windowsApps = Join-Path $localAppData "Microsoft\WindowsApps"
        $localityBin = Join-Path $localAppData "Locality\bin"

        foreach ($path in @($stateDir, $installDir, $mountRoot, $windowsApps, $localityBin)) {
            New-Item -ItemType Directory -Path $path -Force | Out-Null
        }
        New-Item -ItemType File -Path (Join-Path $stateDir "state.sqlite3") | Out-Null
        New-Item -ItemType File -Path (Join-Path $installDir "Locality.exe") | Out-Null
        Set-Content -Path (Join-Path $windowsApps "loc.cmd") -Value "@echo off`r`nrem LOCALITY_TERMINAL_CLI_SHIM`r`n" -NoNewline
        Set-Content -Path (Join-Path $localityBin "loc.cmd") -Value "@echo off`r`nrem LOCALITY_TERMINAL_CLI_SHIM`r`n" -NoNewline
        Set-Content -Path (Join-Path $root "user-loc.cmd") -Value "@echo off`r`necho user owned`r`n" -NoNewline

        $env:LOCALAPPDATA = $localAppData
        $env:USERPROFILE = $profile

        $output = Invoke-CleanStart -CleanStartArguments @(
            "-Yes",
            "-KeepCredentials",
            "-StateDir", $stateDir,
            "-InstallDir", $installDir
        )

        Assert-Contains $output "Mode: executing cleanup."
        Assert-True (-not (Test-Path -LiteralPath $stateDir)) "cleanup did not remove state directory"
        Assert-True (-not (Test-Path -LiteralPath $installDir)) "cleanup did not remove install directory"
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $windowsApps "loc.cmd"))) "cleanup did not remove WindowsApps shim"
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $localityBin "loc.cmd"))) "cleanup did not remove Locality bin shim"
        Assert-True (Test-Path -LiteralPath (Join-Path $root "user-loc.cmd")) "cleanup removed an unmanaged command file"
        Assert-True (-not (Test-Path -LiteralPath $mountRoot)) "cleanup did not remove the user-visible mount root"
    } finally {
        $env:LOCALAPPDATA = $oldLocalAppData
        $env:USERPROFILE = $oldUserProfile
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Test-PrepareUninstallWaitsForWindowsAppHook {
    $script = Get-Content -LiteralPath $scriptPath -Raw
    Assert-Contains $script "Start-Process"
    Assert-Contains $script "-Wait"
    Assert-Contains $script "-PassThru"
}

function Test-CloudFilesMountCleanupFallbackIsPresent {
    $script = Get-Content -LiteralPath $scriptPath -Raw
    Assert-Contains $script "Reset-WindowsCloudFilesRegistrations"
    Assert-Contains $script "locality-cloud-files.exe"
    Assert-Contains $script "CldFlt"
    Assert-Contains $script "fltmc"
    Assert-Contains $script "Remove-MountRootIfExists"
}

function Test-MakeTargetsDispatchToWindowsScript {
    $planOutput = & make --dry-run clean-start-plan 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "make --dry-run clean-start-plan failed with exit code $LASTEXITCODE`n$planOutput"
    }
    $planText = $planOutput -join "`n"
    Assert-Contains $planText "clean-start.ps1"

    $cleanOutput = & make --dry-run clean-start 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "make --dry-run clean-start failed with exit code $LASTEXITCODE`n$cleanOutput"
    }
    $cleanText = $cleanOutput -join "`n"
    Assert-Contains $cleanText "clean-start.ps1"
    Assert-Contains $cleanText "-Yes"
}

Test-DryRunPlansWindowsCleanupWithoutDeleting
Test-YesRemovesStateInstallAndManagedShims
Test-PrepareUninstallWaitsForWindowsAppHook
Test-CloudFilesMountCleanupFallbackIsPresent
Test-MakeTargetsDispatchToWindowsScript
Write-Host "windows_clean_start: ok"
