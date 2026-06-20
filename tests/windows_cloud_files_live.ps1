param()

$ErrorActionPreference = "Stop"

if ($env:AFS_WINDOWS_CLOUD_FILES_LIVE -ne "1") {
    Write-Host "skip: set AFS_WINDOWS_CLOUD_FILES_LIVE=1 to run the live Windows Cloud Files Notion test"
    exit 0
}

$runningOnWindows = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows
)
if (-not $runningOnWindows) {
    $message = "skip: Windows Cloud Files live test requires Windows"
    if ($env:AFS_WINDOWS_CLOUD_FILES_LIVE_REQUIRED -eq "1") {
        Write-Error $message
        exit 1
    }
    Write-Host $message
    exit 0
}

$notionToken = if ($env:NOTION_TOKEN) { $env:NOTION_TOKEN } else { $env:NOTION_AT }
$parentPageId = if ($env:AFS_NOTION_LIVE_PARENT_PAGE) { $env:AFS_NOTION_LIVE_PARENT_PAGE } else { $env:AFS_NOTION_PAGE_ID }

if (-not $notionToken) {
    Write-Error "missing NOTION_TOKEN or NOTION_AT"
    exit 1
}

if (-not $parentPageId) {
    Write-Error "missing AFS_NOTION_LIVE_PARENT_PAGE or AFS_NOTION_PAGE_ID"
    exit 1
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$afsBin = if ($env:AFS_BIN) { $env:AFS_BIN } else { Join-Path $repoRoot "target\debug\afs.exe" }
$afsdBin = if ($env:AFSD_BIN) { $env:AFSD_BIN } else { Join-Path $repoRoot "target\debug\afsd.exe" }
$cloudFilesBin = if ($env:AFS_CLOUD_FILES_BIN) { $env:AFS_CLOUD_FILES_BIN } else { Join-Path $repoRoot "target\debug\afs-cloud-files.exe" }

$unique = "{0}-{1}" -f (Get-Date).ToUniversalTime().ToString("yyyyMMdd-HHmmss"), ([Guid]::NewGuid().ToString("N").Substring(0, 8))
$tmpRoot = Join-Path ([System.IO.Path]::GetTempPath()) "afs-windows-cloud-files-live-$unique"
$stateRoot = if ($env:AFS_WINDOWS_CLOUD_FILES_LIVE_STATE) { $env:AFS_WINDOWS_CLOUD_FILES_LIVE_STATE } else { Join-Path $tmpRoot "state" }
$syncRoot = if ($env:AFS_WINDOWS_CLOUD_FILES_LIVE_ROOT) { $env:AFS_WINDOWS_CLOUD_FILES_LIVE_ROOT } else { Join-Path $tmpRoot "AFS" }
$mountId = if ($env:AFS_WINDOWS_CLOUD_FILES_LIVE_MOUNT_ID) { $env:AFS_WINDOWS_CLOUD_FILES_LIVE_MOUNT_ID } else { "notion-windows-cloud-live-$unique" }
$daemonOut = Join-Path $tmpRoot "afsd.out.log"
$daemonErr = Join-Path $tmpRoot "afsd.err.log"
$providerOut = Join-Path $tmpRoot "afs-cloud-files.out.log"
$providerErr = Join-Path $tmpRoot "afs-cloud-files.err.log"
$scratchPageId = $null
$createdChildPageId = $null
$daemonProcess = $null
$providerProcess = $null
$failed = $false

function Normalize-NotionId {
    param([string] $InputId)
    $trimmed = $InputId.Trim().TrimEnd("/")
    $candidate = (($trimmed -split "[?#]")[0] -split "/")[-1]
    $hex = -join (($candidate.ToCharArray() | Where-Object { $_ -match "[0-9a-fA-F]" }))
    if ($hex.Length -ge 32) {
        return $hex.Substring($hex.Length - 32).ToLowerInvariant()
    }
    return $candidate
}

function ConvertTo-AfsSlug {
    param([string] $Title)
    $builder = [System.Text.StringBuilder]::new()
    foreach ($char in $Title.ToCharArray()) {
        if ($char -match "[A-Za-z0-9]") {
            [void] $builder.Append([char]::ToLowerInvariant($char))
        } elseif ([char]::IsWhiteSpace($char) -or $char -eq "-" -or $char -eq "_") {
            [void] $builder.Append("-")
        }
    }
    return $builder.ToString().Trim("-")
}

function Write-Utf8NoBom {
    param(
        [string] $Path,
        [string] $Contents
    )
    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($Path, $Contents, $encoding)
}

function Invoke-Native {
    param(
        [string] $FilePath,
        [string[]] $Arguments,
        [string] $Step
    )
    $output = & $FilePath @Arguments
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "$Step failed with exit code $exitCode"
    }
    return ($output -join "`n")
}

function Invoke-Notion {
    param(
        [ValidateSet("GET", "POST", "PATCH")]
        [string] $Method,
        [string] $Path,
        [object] $Body = $null
    )
    $headers = @{
        Authorization = "Bearer $notionToken"
        "Notion-Version" = "2022-06-28"
    }
    $uri = "https://api.notion.com/v1/$Path"
    if ($null -eq $Body) {
        return Invoke-RestMethod -Method $Method -Uri $uri -Headers $headers
    }
    $json = $Body | ConvertTo-Json -Depth 32
    return Invoke-RestMethod -Method $Method -Uri $uri -Headers $headers -ContentType "application/json" -Body $json
}

function Get-NotionBlockText {
    param([string] $PageId)
    $response = Invoke-Notion -Method GET -Path ("blocks/{0}/children?page_size=100" -f (Normalize-NotionId $PageId))
    $parts = New-Object System.Collections.Generic.List[string]
    foreach ($block in $response.results) {
        $type = $block.type
        if ($type -and $block.$type -and $block.$type.rich_text) {
            foreach ($text in $block.$type.rich_text) {
                if ($text.plain_text) {
                    $parts.Add($text.plain_text)
                }
            }
        }
        if ($type -eq "child_page" -and $block.child_page.title) {
            $parts.Add($block.child_page.title)
        }
    }
    return ($parts -join "`n")
}

function Archive-NotionPage {
    param([string] $PageId)
    if (-not $PageId) {
        return
    }
    try {
        [void] (Invoke-Notion -Method PATCH -Path ("pages/{0}" -f (Normalize-NotionId $PageId)) -Body @{ archived = $true })
    } catch {
        Write-Warning "failed to archive Notion page ${PageId}: $($_.Exception.Message)"
    }
}

function Assert-NotionPageArchived {
    param([string] $PageId)
    $page = Invoke-Notion -Method GET -Path ("pages/{0}" -f (Normalize-NotionId $PageId))
    if (-not $page.archived) {
        throw "Notion page $PageId was not archived"
    }
}

function Get-FreeTcpAddr {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Parse("127.0.0.1"), 0)
    $listener.Start()
    try {
        $port = $listener.LocalEndpoint.Port
    } finally {
        $listener.Stop()
    }
    return "127.0.0.1:$port"
}

function Wait-ForCondition {
    param(
        [string] $Name,
        [scriptblock] $Condition,
        [int] $Attempts = 120,
        [int] $DelayMilliseconds = 500
    )
    $lastError = $null
    for ($index = 0; $index -lt $Attempts; $index++) {
        try {
            if (& $Condition) {
                return
            }
        } catch {
            $lastError = $_.Exception.Message
        }
        Start-Sleep -Milliseconds $DelayMilliseconds
    }
    if ($lastError) {
        throw "timed out waiting for $Name; last error: $lastError"
    }
    throw "timed out waiting for $Name"
}

function Wait-ForStatusContains {
    param(
        [string] $Path,
        [string] $Needle
    )
    Wait-ForCondition -Name "status $Path contains $Needle" -Condition {
        $output = Invoke-Native -FilePath $afsBin -Arguments @("status", $Path, "--json") -Step "afs status $Path"
        return $output.Contains($Needle)
    }
}

function Test-ProcessRunning {
    param([System.Diagnostics.Process] $Process)
    return $Process -and -not $Process.HasExited
}

function Stop-ChildProcess {
    param([System.Diagnostics.Process] $Process)
    if (Test-ProcessRunning $Process) {
        Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue
        try {
            $Process.WaitForExit(5000)
        } catch {
        }
    }
}

try {
    New-Item -ItemType Directory -Force -Path $stateRoot, $syncRoot | Out-Null

    if (-not ((Test-Path -LiteralPath $afsBin) -and (Test-Path -LiteralPath $afsdBin) -and (Test-Path -LiteralPath $cloudFilesBin))) {
        Push-Location $repoRoot
        try {
            Invoke-Native -FilePath "cargo" -Arguments @("build", "-p", "afsd", "-p", "afs-cli", "-p", "afs-cloud-files") -Step "cargo build"
        } finally {
            Pop-Location
        }
    }

    $tcpAddr = Get-FreeTcpAddr
    $env:AFS_STATE_DIR = $stateRoot
    $env:NOTION_TOKEN = $notionToken
    $env:AFS_CLOUD_FILES_BIN = $cloudFilesBin
    $parentPageId = Normalize-NotionId $parentPageId
    $scratchTitle = "AFS Cloud Files live $unique"
    $initialBody = "Initial paragraph created by the Windows Cloud Files live e2e."
    $scratch = Invoke-Notion -Method POST -Path "pages" -Body @{
        parent = @{
            type = "page_id"
            page_id = $parentPageId
        }
        properties = @{
            title = @{
                title = @(
                    @{
                        text = @{
                            content = $scratchTitle
                        }
                    }
                )
            }
        }
        children = @(
            @{
                object = "block"
                type = "paragraph"
                paragraph = @{
                    rich_text = @(
                        @{
                            type = "text"
                            text = @{
                                content = $initialBody
                            }
                        }
                    )
                }
            }
        )
    }
    $scratchPageId = Normalize-NotionId $scratch.id

    $previousDisable = $env:AFS_DAEMON_DISABLE
    $env:AFS_DAEMON_DISABLE = "1"
    try {
        Invoke-Native -FilePath $afsBin -Arguments @(
            "mount", "notion", $syncRoot,
            "--root-page", $scratchPageId,
            "--mount-id", $mountId,
            "--projection", "windows-cloud-files",
            "--json"
        ) -Step "afs mount" | Out-Null
    } finally {
        if ($null -eq $previousDisable) {
            Remove-Item Env:\AFS_DAEMON_DISABLE -ErrorAction SilentlyContinue
        } else {
            $env:AFS_DAEMON_DISABLE = $previousDisable
        }
    }

    $env:AFS_DAEMON_TCP_ADDR = $tcpAddr
    if (-not $env:AFS_CLOUD_FILES_TRACE) {
        $env:AFS_CLOUD_FILES_TRACE = "1"
    }

    Invoke-Native -FilePath $cloudFilesBin -Arguments @(
        "register",
        "--mount-id", $mountId,
        "--display-name", "AFS Live Cloud Files",
        "--sync-root", $syncRoot,
        "--state-dir", $stateRoot,
        "--json"
    ) -Step "afs-cloud-files register" | Out-Null

    $daemonProcess = Start-Process -FilePath $afsdBin -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $daemonOut -RedirectStandardError $daemonErr
    Wait-ForCondition -Name "afsd TCP endpoint" -Condition {
        if (-not (Test-ProcessRunning $daemonProcess)) {
            throw "afsd exited with code $($daemonProcess.ExitCode)"
        }
        $output = Invoke-Native -FilePath $afsBin -Arguments @("daemon", "status", "--state-dir", $stateRoot, "--tcp-addr", $tcpAddr, "--json") -Step "afs daemon status"
        return $output.Contains('"state": "running"')
    }

    $providerProcess = Start-Process -FilePath $cloudFilesBin -ArgumentList @(
        "run",
        "--mount-id", $mountId,
        "--sync-root", $syncRoot,
        "--state-dir", $stateRoot,
        "--json"
    ) -PassThru -WindowStyle Hidden -RedirectStandardOutput $providerOut -RedirectStandardError $providerErr

    $sourceRoot = Join-Path $syncRoot "notion"
    $pageDir = Join-Path $sourceRoot (ConvertTo-AfsSlug $scratchTitle)
    $pageFile = Join-Path $pageDir "page.md"
    Wait-ForCondition -Name "Cloud Files source root" -Condition {
        if (-not (Test-ProcessRunning $providerProcess)) {
            throw "afs-cloud-files exited with code $($providerProcess.ExitCode)"
        }
        Test-Path -LiteralPath $sourceRoot
    }
    Wait-ForCondition -Name "scratch page placeholder" -Condition {
        Get-ChildItem -LiteralPath $sourceRoot -Force | Out-Null
        Test-Path -LiteralPath $pageDir
    }
    Wait-ForCondition -Name "scratch page.md placeholder" -Condition {
        Get-ChildItem -LiteralPath $pageDir -Force | Out-Null
        Test-Path -LiteralPath $pageFile
    }

    $hydrated = Get-Content -LiteralPath $pageFile -Raw
    if (-not $hydrated.Contains($initialBody)) {
        throw "hydrated page.md did not contain the initial Notion paragraph"
    }

    $editMarker = "Windows Cloud Files live edit $unique"
    Write-Utf8NoBom -Path $pageFile -Contents ($hydrated.TrimEnd() + "`n`n$editMarker`n")
    Wait-ForStatusContains -Path $pageFile -Needle '"local_body_changed"'
    Invoke-Native -FilePath $afsBin -Arguments @("push", $pageFile, "-y", "--json") -Step "push edited page" | Out-Null
    Wait-ForStatusContains -Path $pageFile -Needle '"state": "clean"'
    $remoteText = Get-NotionBlockText -PageId $scratchPageId
    if (-not $remoteText.Contains($editMarker)) {
        throw "Notion page did not contain the pushed edit marker"
    }

    $draftFile = Join-Path $pageDir ("draft-$unique.md")
    $renamedDraftFile = Join-Path $pageDir ("draft-renamed-$unique.md")
    Write-Utf8NoBom -Path $draftFile -Contents "---`ntitle: `"Draft $unique`"`n---`n# Draft`n`nCreated through Cloud Files and not pushed.`n"
    Rename-Item -LiteralPath $draftFile -NewName (Split-Path -Leaf $renamedDraftFile)
    Wait-ForStatusContains -Path $renamedDraftFile -Needle '"pending_virtual_create"'
    Remove-Item -LiteralPath $renamedDraftFile -Force
    Wait-ForStatusContains -Path $pageDir -Needle '"clean": true'

    $childTitle = "AFS Cloud Files child $unique"
    $childDir = Join-Path $pageDir (ConvertTo-AfsSlug $childTitle)
    $childPage = Join-Path $childDir "page.md"
    $childMarker = "Windows Cloud Files created child $unique"
    New-Item -ItemType Directory -Force -Path $childDir | Out-Null
    Write-Utf8NoBom -Path $childPage -Contents "---`ntitle: `"$childTitle`"`n---`n# Created child`n`n$childMarker`n"
    Wait-ForStatusContains -Path $childPage -Needle '"pending_virtual_create"'
    $pushChildOutput = Invoke-Native -FilePath $afsBin -Arguments @("push", $childPage, "-y", "--json") -Step "push created child page"
    $pushChild = $pushChildOutput | ConvertFrom-Json
    $createdChildPageId = @($pushChild.changed_remote_ids | Where-Object { (Normalize-NotionId $_) -ne $scratchPageId } | Select-Object -First 1)[0]
    if (-not $createdChildPageId) {
        throw "child page push did not report a created remote id"
    }
    $createdChildPageId = Normalize-NotionId $createdChildPageId
    $childRemoteText = Get-NotionBlockText -PageId $createdChildPageId
    if (-not $childRemoteText.Contains($childMarker)) {
        throw "created child Notion page did not contain the pushed marker"
    }

    Remove-Item -LiteralPath $childDir -Recurse -Force
    Wait-ForStatusContains -Path $pageDir -Needle '"pending_virtual_delete"'
    Invoke-Native -FilePath $afsBin -Arguments @("push", $pageDir, "-y", "--json") -Step "push deleted child page" | Out-Null
    Assert-NotionPageArchived -PageId $createdChildPageId

    Write-Host "ok: Windows Cloud Files live Notion e2e completed"
} catch {
    $failed = $true
    Write-Error $_ -ErrorAction Continue
} finally {
    Stop-ChildProcess $providerProcess
    Stop-ChildProcess $daemonProcess
    if (Test-Path -LiteralPath $cloudFilesBin) {
        try {
            Invoke-Native -FilePath $cloudFilesBin -Arguments @("unregister", "--mount-id", $mountId, "--state-dir", $stateRoot, "--json") -Step "afs-cloud-files unregister" | Out-Null
        } catch {
            Write-Warning "Cloud Files unregister failed: $($_.Exception.Message)"
        }
    }
    Archive-NotionPage -PageId $createdChildPageId
    Archive-NotionPage -PageId $scratchPageId

    if ($failed) {
        foreach ($log in @($daemonOut, $daemonErr, $providerOut, $providerErr)) {
            if (Test-Path -LiteralPath $log) {
                Write-Host "----- $log -----"
                Get-Content -LiteralPath $log -ErrorAction SilentlyContinue
            }
        }
    }

    if ($env:AFS_WINDOWS_CLOUD_FILES_LIVE_KEEP_TMP -eq "1") {
        Write-Host "kept Windows Cloud Files live temp root: $tmpRoot"
    } elseif ((Test-Path -LiteralPath $tmpRoot) -and $tmpRoot.StartsWith([System.IO.Path]::GetTempPath(), [System.StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $tmpRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($failed) {
    exit 1
}
