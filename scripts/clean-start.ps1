param(
    [switch] $Yes,
    [switch] $KeepCredentials,
    [string] $StateDir,
    [string] $InstallDir,
    [switch] $Help
)

$ErrorActionPreference = "Stop"

function Show-Usage {
    @"
Usage: scripts/clean-start.ps1 [-Yes] [-KeepCredentials] [-StateDir PATH] [-InstallDir PATH]

Reset this Windows machine to a clean Locality testing state.

By default this is a dry run. Pass -Yes to stop processes, run Locality's
uninstall-preparation hook when available, remove local Locality state, remove
the installed app directory, remove the default user-visible Locality mount
folder, remove the Windows login item, and delete Locality-managed terminal
command shims.

Options:
  -Yes              Execute the cleanup. Without this flag, only print actions.
  -KeepCredentials  Do not run Locality's credential-clearing uninstall hook.
  -StateDir PATH    State directory to delete. Defaults to LOCALITY_STATE_DIR or
                    the Locality state directory under LOCALAPPDATA.
  -InstallDir PATH  Installed app directory to delete. Defaults to
                    LOCALAPPDATA\Locality.
  -Help             Show this help.
"@
}

if ($Help) {
    Show-Usage
    return
}

$DryRun = -not $Yes
$TerminalShimMarker = "LOCALITY_TERMINAL_CLI_SHIM"
$RunKeyPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$RunValueName = "Locality"

function Write-Log {
    param([string] $Message)
    Write-Host $Message
}

function Write-Warn {
    param([string] $Message)
    Write-Warning "clean-start: $Message"
}

function Convert-ToFullPath {
    param([string] $Path)
    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $null
    }
    return [System.IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($Path))
}

function Get-LocalAppData {
    if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        return Convert-ToFullPath $env:LOCALAPPDATA
    }
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        return Convert-ToFullPath (Join-Path $env:USERPROFILE "AppData\Local")
    }
    return $null
}

function Get-DefaultStateDir {
    if (-not [string]::IsNullOrWhiteSpace($env:LOCALITY_STATE_DIR)) {
        return Convert-ToFullPath $env:LOCALITY_STATE_DIR
    }

    $localAppData = Get-LocalAppData
    $candidates = @()
    if ($localAppData) {
        $candidates += Join-Path $localAppData "Locality"
        $candidates += Join-Path $localAppData "AgentFS"
        $candidates += Join-Path $localAppData "AFS"
    }
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        $candidates += Join-Path $env:USERPROFILE "AppData\Local\Locality"
        $candidates += Join-Path $env:USERPROFILE "AppData\Local\AgentFS"
    }

    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate)) {
            return Convert-ToFullPath $candidate
        }
    }

    if ($localAppData) {
        return Convert-ToFullPath (Join-Path $localAppData "Locality")
    }
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        return Convert-ToFullPath (Join-Path $env:USERPROFILE "AppData\Local\Locality")
    }
    return Convert-ToFullPath ".loc"
}

function Get-DefaultInstallDir {
    $localAppData = Get-LocalAppData
    if ($localAppData) {
        return Convert-ToFullPath (Join-Path $localAppData "Locality")
    }
    return Get-DefaultStateDir
}

function Get-UserHome {
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        return Convert-ToFullPath $env:USERPROFILE
    }
    if (-not [string]::IsNullOrWhiteSpace($env:HOME)) {
        return Convert-ToFullPath $env:HOME
    }
    if (
        -not [string]::IsNullOrWhiteSpace($env:HOMEDRIVE) -and
        -not [string]::IsNullOrWhiteSpace($env:HOMEPATH)
    ) {
        return Convert-ToFullPath ("{0}{1}" -f $env:HOMEDRIVE, $env:HOMEPATH)
    }
    return $null
}

function Get-DefaultMountRoots {
    $userHome = Get-UserHome
    if (-not $userHome) {
        return @()
    }
    return @(Convert-ToFullPath (Join-Path $userHome "Locality"))
}

function Format-Command {
    param([string[]] $Parts)
    $escaped = @()
    foreach ($part in $Parts) {
        if ($null -eq $part) {
            continue
        }
        if ($part -match '[\s"`$&|<>]') {
            $escaped += '"' + ($part -replace '"', '\"') + '"'
        } else {
            $escaped += $part
        }
    }
    return "+ " + ($escaped -join " ")
}

function Invoke-Step {
    param(
        [string[]] $Command,
        [scriptblock] $Script,
        [switch] $AllowFailure
    )

    Write-Log (Format-Command $Command)
    if ($DryRun) {
        return
    }

    try {
        & $Script
    } catch {
        if ($AllowFailure) {
            Write-Warn $_.Exception.Message
        } else {
            throw
        }
    }
}

function Assert-SafeRemovalPath {
    param([string] $Path)

    $full = Convert-ToFullPath $Path
    if ([string]::IsNullOrWhiteSpace($full)) {
        throw "refusing to remove an empty path"
    }

    $root = [System.IO.Path]::GetPathRoot($full)
    if ($full.TrimEnd('\') -ieq $root.TrimEnd('\')) {
        throw "refusing to remove filesystem root: $full"
    }

    $blocked = @()
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        $blocked += Convert-ToFullPath $env:USERPROFILE
    }
    $localAppData = Get-LocalAppData
    if ($localAppData) {
        $blocked += $localAppData
    }

    foreach ($pathToBlock in $blocked) {
        if ($pathToBlock -and ($full.TrimEnd('\') -ieq $pathToBlock.TrimEnd('\'))) {
            throw "refusing to remove broad user directory: $full"
        }
    }
}

function Remove-PathIfExists {
    param([string] $Path)

    $full = Convert-ToFullPath $Path
    if (-not $full) {
        return
    }
    if (-not (Test-Path -LiteralPath $full)) {
        return
    }

    Assert-SafeRemovalPath $full
    Invoke-Step -Command @("Remove-Item", "-LiteralPath", $full, "-Recurse", "-Force") -Script {
        Remove-Item -LiteralPath $full -Recurse -Force
    }
}

function Stop-ProcessImage {
    param([string] $ImageName)

    $taskkill = Join-Path $env:SystemRoot "System32\taskkill.exe"
    if (-not (Test-Path -LiteralPath $taskkill)) {
        $taskkill = "taskkill.exe"
    }

    Invoke-Step -Command @($taskkill, "/F", "/T", "/IM", $ImageName) -AllowFailure -Script {
        $output = & $taskkill /F /T /IM $ImageName 2>&1
        if ($LASTEXITCODE -ne 0) {
            $text = ($output -join "`n")
            if ($text -notmatch "not found" -and $text -notmatch "not running") {
                throw $text
            }
        }
    }
}

function Stop-LocalityProcesses {
    foreach ($image in @(
        "locality-desktop.exe",
        "Locality.exe",
        "locality-cloud-files.exe",
        "localityd.exe",
        "loc.exe"
    )) {
        Stop-ProcessImage $image
    }
}

function Get-DesktopBinary {
    param([string] $Directory)

    foreach ($name in @("Locality.exe", "locality-desktop.exe")) {
        $candidate = Join-Path $Directory $name
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return Convert-ToFullPath $candidate
        }
    }
    return $null
}

function Invoke-PrepareUninstall {
    param([string] $Directory)

    if ($KeepCredentials) {
        Write-Warn "skipping Locality --prepare-uninstall because -KeepCredentials was passed"
        return
    }

    $binary = Get-DesktopBinary $Directory
    if (-not $binary) {
        Write-Warn "Locality desktop binary not found under $Directory; credentials and agent integrations may remain"
        return
    }

    Invoke-Step -Command @($binary, "--prepare-uninstall") -AllowFailure -Script {
        & $binary --prepare-uninstall
        if ($LASTEXITCODE -ne 0) {
            throw "Locality --prepare-uninstall exited with code $LASTEXITCODE"
        }
    }
}

function Remove-LoginItem {
    Invoke-Step -Command @("Remove-ItemProperty", "-Path", $RunKeyPath, "-Name", $RunValueName) -AllowFailure -Script {
        if (Test-Path -LiteralPath $RunKeyPath) {
            Remove-ItemProperty -Path $RunKeyPath -Name $RunValueName -ErrorAction SilentlyContinue
        }
    }
}

function Remove-TerminalShimIfManaged {
    param([string] $Path)

    $full = Convert-ToFullPath $Path
    if (-not $full -or -not (Test-Path -LiteralPath $full -PathType Leaf)) {
        return
    }

    $contents = Get-Content -LiteralPath $full -Raw -ErrorAction SilentlyContinue
    if ($contents -notlike "*$TerminalShimMarker*") {
        return
    }

    Invoke-Step -Command @("Remove-Item", "-LiteralPath", $full, "-Force") -Script {
        Remove-Item -LiteralPath $full -Force
    }
}

function Remove-KnownTerminalShims {
    $localAppData = Get-LocalAppData
    if (-not $localAppData) {
        return
    }

    Remove-TerminalShimIfManaged (Join-Path $localAppData "Microsoft\WindowsApps\loc.cmd")
    Remove-TerminalShimIfManaged (Join-Path $localAppData "Locality\bin\loc.cmd")

    $binDir = Join-Path $localAppData "Locality\bin"
    if (Test-Path -LiteralPath $binDir -PathType Container) {
        $children = @(Get-ChildItem -LiteralPath $binDir -Force -ErrorAction SilentlyContinue)
        if ($children.Count -eq 0) {
            Invoke-Step -Command @("Remove-Item", "-LiteralPath", $binDir, "-Force") -Script {
                Remove-Item -LiteralPath $binDir -Force
            }
        }
    }
}

function Remove-MountRoots {
    foreach ($root in Get-DefaultMountRoots) {
        Remove-PathIfExists $root
    }
}

$resolvedStateDir = if ($StateDir) { Convert-ToFullPath $StateDir } else { Get-DefaultStateDir }
$resolvedInstallDir = if ($InstallDir) { Convert-ToFullPath $InstallDir } else { Get-DefaultInstallDir }

Write-Log "Locality Windows clean-start"
if ($DryRun) {
    Write-Log "Mode: dry run."
} else {
Write-Log "Mode: executing cleanup."
}
Write-Log "State directory: $resolvedStateDir"
Write-Log "Install directory: $resolvedInstallDir"
foreach ($root in Get-DefaultMountRoots) {
    Write-Log "Mount directory: $root"
}

Stop-LocalityProcesses
Invoke-PrepareUninstall $resolvedInstallDir
Stop-LocalityProcesses
Remove-LoginItem
Remove-KnownTerminalShims
Remove-MountRoots
Remove-PathIfExists $resolvedStateDir
if ($resolvedInstallDir.TrimEnd('\') -ine $resolvedStateDir.TrimEnd('\')) {
    Remove-PathIfExists $resolvedInstallDir
}

if ($DryRun) {
    Write-Log "Dry run complete. Re-run with -Yes to execute."
} else {
    Write-Log "Clean-start complete."
}
