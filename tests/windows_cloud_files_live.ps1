param()

$ErrorActionPreference = "Stop"

if ($env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE -ne "1") {
    Write-Host "skip: set LOCALITY_WINDOWS_CLOUD_FILES_LIVE=1 to run the live Windows Cloud Files Notion test"
    exit 0
}

if ($PSVersionTable.PSVersion.Major -lt 7) {
    Write-Error "Windows Cloud Files live test requires PowerShell 7+. Run with pwsh."
    exit 1
}

$runningOnWindows = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows
)
if (-not $runningOnWindows) {
    $message = "skip: Windows Cloud Files live test requires Windows"
    if ($env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE_REQUIRED -eq "1") {
        Write-Error $message
        exit 1
    }
    Write-Host $message
    exit 0
}

$notionToken = if ($env:NOTION_TOKEN) { $env:NOTION_TOKEN } else { $env:NOTION_AT }
$parentPageId = if ($env:LOCALITY_NOTION_LIVE_PARENT_PAGE) { $env:LOCALITY_NOTION_LIVE_PARENT_PAGE } else { $env:LOCALITY_NOTION_PAGE_ID }

if (-not $notionToken) {
    Write-Error "missing NOTION_TOKEN or NOTION_AT"
    exit 1
}

if (-not $parentPageId) {
    Write-Error "missing LOCALITY_NOTION_LIVE_PARENT_PAGE or LOCALITY_NOTION_PAGE_ID"
    exit 1
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$locBin = if ($env:LOCALITY_BIN) { $env:LOCALITY_BIN } else { Join-Path $repoRoot "target\debug\loc.exe" }
$localitydBin = if ($env:LOCALITYD_BIN) { $env:LOCALITYD_BIN } else { Join-Path $repoRoot "target\debug\localityd.exe" }
$cloudFilesBin = if ($env:LOCALITY_CLOUD_FILES_BIN) { $env:LOCALITY_CLOUD_FILES_BIN } else { Join-Path $repoRoot "target\debug\locality-cloud-files.exe" }

$unique = "{0}-{1}" -f (Get-Date).ToUniversalTime().ToString("yyyyMMdd-HHmmss"), ([Guid]::NewGuid().ToString("N").Substring(0, 8))
$tmpRoot = Join-Path ([System.IO.Path]::GetTempPath()) "loc-windows-cloud-files-live-$unique"
$stateRoot = if ($env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE_STATE) { $env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE_STATE } else { Join-Path $tmpRoot "state" }
$syncRoot = if ($env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE_ROOT) { $env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE_ROOT } else { Join-Path $tmpRoot "Locality" }
$mountId = if ($env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE_MOUNT_ID) { $env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE_MOUNT_ID } else { "notion-windows-cloud-live-$unique" }
$scratchPageId = $null
$createdChildPageId = $null
$tcpAddr = $null
$failed = $false

function Write-Step {
    param([string] $Message)
    Write-Host "[loc-live] $Message"
}

function Invoke-WithTimeout {
    param(
        [string] $Name,
        [scriptblock] $Script,
        [object[]] $ArgumentList = @(),
        [int] $TimeoutSeconds = 60
    )
    $job = Start-Job -WorkingDirectory (Get-Location).Path -ScriptBlock $Script -ArgumentList $ArgumentList
    try {
        if (-not (Wait-Job -Job $job -Timeout $TimeoutSeconds)) {
            Stop-Job -Job $job -ErrorAction SilentlyContinue
            throw "$Name timed out after $TimeoutSeconds seconds"
        }
        Receive-Job -Job $job -ErrorAction Stop
    } finally {
        Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
    }
}

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

function ConvertTo-LocalitySlug {
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
    Invoke-WithTimeout -Name "write $Path" -TimeoutSeconds 60 -ArgumentList @($Path, $Contents) -Script {
        param([string] $Path, [string] $Contents)
        $encoding = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllText($Path, $Contents, $encoding)
    } | Out-Null
}

function Invoke-Native {
    param(
        [string] $FilePath,
        [string[]] $Arguments,
        [string] $Step,
        [int] $TimeoutSeconds = 120,
        [switch] $NoCapture
    )
    Write-Step $Step
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo.FileName = $FilePath
    $process.StartInfo.WorkingDirectory = (Get-Location).Path
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = -not $NoCapture
    $process.StartInfo.RedirectStandardError = -not $NoCapture
    foreach ($argument in $Arguments) {
        [void] $process.StartInfo.ArgumentList.Add($argument)
    }
    [void] $process.Start()
    $stdout = $null
    $stderr = $null
    if (-not $NoCapture) {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
    }
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        try {
            $process.Kill($true)
        } catch {
            $process.Kill()
        }
        throw "$Step timed out after $TimeoutSeconds seconds"
    }
    [void] $process.WaitForExit()
    $stdoutText = ""
    $stderrText = ""
    if (-not $NoCapture) {
        if (-not $stdout.Wait(30000)) {
            throw "$Step exited, but stdout did not close within 30 seconds"
        }
        if (-not $stderr.Wait(30000)) {
            throw "$Step exited, but stderr did not close within 30 seconds"
        }
        $stdoutText = $stdout.Result
        $stderrText = $stderr.Result
    }
    $output = @($stdoutText, $stderrText).Where({ -not [string]::IsNullOrWhiteSpace($_) }) -join "`n"
    if ($process.ExitCode -ne 0) {
        if ($output) {
            Write-Host $output
        }
        throw "$Step failed with exit code $($process.ExitCode)"
    }
    return $stdoutText
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
        return Invoke-RestMethod -Method $Method -Uri $uri -Headers $headers -TimeoutSec 30
    }
    $json = $Body | ConvertTo-Json -Depth 32
    return Invoke-RestMethod -Method $Method -Uri $uri -Headers $headers -ContentType "application/json" -Body $json -TimeoutSec 30
}

function Test-PathWithTimeout {
    param(
        [string] $Path,
        [string] $Name = $Path
    )
    $result = Invoke-WithTimeout -Name "test path $Name" -TimeoutSeconds 20 -ArgumentList @($Path) -Script {
        param([string] $Path)
        Test-Path -LiteralPath $Path
    }
    return [bool] $result
}

function Get-ChildItemWithTimeout {
    param(
        [string] $Path,
        [string] $Name = $Path
    )
    Invoke-WithTimeout -Name "enumerate $Name" -TimeoutSeconds 30 -ArgumentList @($Path) -Script {
        param([string] $Path)
        Get-ChildItem -LiteralPath $Path -Force | Select-Object -ExpandProperty FullName
    }
}

function Get-ContentWithTimeout {
    param(
        [string] $Path,
        [string] $Name = $Path,
        [int] $TimeoutSeconds = 90
    )
    Invoke-WithTimeout -Name "read $Name" -TimeoutSeconds $TimeoutSeconds -ArgumentList @($Path) -Script {
        param([string] $Path)
        Get-Content -LiteralPath $Path -Raw
    }
}

function Rename-ItemWithTimeout {
    param(
        [string] $Path,
        [string] $NewName
    )
    Invoke-WithTimeout -Name "rename $Path" -TimeoutSeconds 60 -ArgumentList @($Path, $NewName) -Script {
        param([string] $Path, [string] $NewName)
        Rename-Item -LiteralPath $Path -NewName $NewName
    } | Out-Null
}

function Remove-ItemWithTimeout {
    param(
        [string] $Path,
        [switch] $Recurse
    )
    Invoke-WithTimeout -Name "remove $Path" -TimeoutSeconds 60 -ArgumentList @($Path, [bool] $Recurse) -Script {
        param([string] $Path, [bool] $Recurse)
        if ($Recurse) {
            Remove-Item -LiteralPath $Path -Recurse -Force
        } else {
            Remove-Item -LiteralPath $Path -Force
        }
    } | Out-Null
}

function New-DirectoryWithTimeout {
    param([string] $Path)
    Invoke-WithTimeout -Name "create directory $Path" -TimeoutSeconds 60 -ArgumentList @($Path) -Script {
        param([string] $Path)
        New-Item -ItemType Directory -Force -Path $Path | Out-Null
    } | Out-Null
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
        $output = Invoke-Native -FilePath $locBin -Arguments @("status", $Path, "--json") -Step "loc status $Path"
        return $output.Contains($Needle)
    }
}

try {
    Write-Step "preparing temp state and sync root"
    New-Item -ItemType Directory -Force -Path $stateRoot, $syncRoot | Out-Null

    if (-not ((Test-Path -LiteralPath $locBin) -and (Test-Path -LiteralPath $localitydBin) -and (Test-Path -LiteralPath $cloudFilesBin))) {
        Push-Location $repoRoot
        try {
            Invoke-Native -FilePath "cargo" -Arguments @("build", "-p", "localityd", "-p", "loc-cli", "-p", "locality-cloud-files") -Step "cargo build" -TimeoutSeconds 600
        } finally {
            Pop-Location
        }
    }

    $tcpAddr = Get-FreeTcpAddr
    $env:LOCALITY_STATE_DIR = $stateRoot
    $env:NOTION_TOKEN = $notionToken
    $env:LOCALITY_CLOUD_FILES_BIN = $cloudFilesBin
    $parentPageId = Normalize-NotionId $parentPageId
    $scratchTitle = "Locality Cloud Files live $unique"
    $initialBody = "Initial paragraph created by the Windows Cloud Files live e2e."
    Write-Step "creating scratch Notion page"
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

    Write-Step "mounting Windows Cloud Files projection"
    $previousDisable = $env:LOCALITY_DAEMON_DISABLE
    $env:LOCALITY_DAEMON_DISABLE = "1"
    try {
        Invoke-Native -FilePath $locBin -Arguments @(
            "mount", "notion", $syncRoot,
            "--root-page", $scratchPageId,
            "--mount-id", $mountId,
            "--projection", "windows-cloud-files",
            "--json"
        ) -Step "loc mount" | Out-Null
    } finally {
        if ($null -eq $previousDisable) {
            Remove-Item Env:\LOCALITY_DAEMON_DISABLE -ErrorAction SilentlyContinue
        } else {
            $env:LOCALITY_DAEMON_DISABLE = $previousDisable
        }
    }

    $env:LOCALITY_DAEMON_TCP_ADDR = $tcpAddr
    if (-not $env:LOCALITY_CLOUD_FILES_TRACE) {
        $env:LOCALITY_CLOUD_FILES_TRACE = "1"
    }

    Write-Step "starting localityd"
    Invoke-Native -FilePath $locBin -Arguments @(
        "daemon", "start",
        "--session",
        "--state-dir", $stateRoot,
        "--tcp-addr", $tcpAddr,
        "--localityd-bin", $localitydBin,
        "--include-env", "NOTION_TOKEN",
        "--json"
    ) -Step "loc daemon start" -NoCapture | Out-Null
    Wait-ForCondition -Name "localityd TCP endpoint" -Condition {
        $output = Invoke-Native -FilePath $locBin -Arguments @("daemon", "status", "--state-dir", $stateRoot, "--tcp-addr", $tcpAddr, "--json") -Step "loc daemon status"
        return $output.Contains('"state": "running"')
    }

    Write-Step "starting Cloud Files provider"
    Invoke-Native -FilePath $locBin -Arguments @("file-provider", "start", $mountId, "--json") -Step "loc file-provider start" | Out-Null
    Wait-ForCondition -Name "Cloud Files provider lifecycle" -Condition {
        $output = Invoke-Native -FilePath $locBin -Arguments @("file-provider", "status", $mountId, "--json") -Step "loc file-provider status"
        return $output.Contains('"state": "running"')
    }
    Write-Step "running loc doctor"
    $doctorOutput = Invoke-Native -FilePath $locBin -Arguments @("doctor", "--json") -Step "loc doctor"
    $doctor = $doctorOutput | ConvertFrom-Json
    if (-not $doctor.ok) {
        throw "loc doctor reported $($doctor.status) during live Cloud Files e2e: $doctorOutput"
    }

    $sourceRoot = Join-Path $syncRoot "notion"
    $pageDir = Join-Path $sourceRoot (ConvertTo-LocalitySlug $scratchTitle)
    $pageFile = Join-Path $pageDir "page.md"
    Write-Step "waiting for source root"
    Wait-ForCondition -Name "Cloud Files source root" -Condition {
        Test-PathWithTimeout -Path $sourceRoot -Name "source root"
    }
    Write-Step "waiting for scratch page placeholder"
    Wait-ForCondition -Name "scratch page placeholder" -Condition {
        Get-ChildItemWithTimeout -Path $sourceRoot -Name "source root" | Out-Null
        Test-PathWithTimeout -Path $pageDir -Name "scratch page directory"
    }
    Write-Step "waiting for scratch page.md placeholder"
    Wait-ForCondition -Name "scratch page.md placeholder" -Condition {
        Get-ChildItemWithTimeout -Path $pageDir -Name "scratch page directory" | Out-Null
        Test-PathWithTimeout -Path $pageFile -Name "scratch page.md"
    }

    Write-Step "hydrating page.md"
    $hydrated = Get-ContentWithTimeout -Path $pageFile -Name "scratch page.md" -TimeoutSeconds 120
    if (-not $hydrated.Contains($initialBody)) {
        throw "hydrated page.md did not contain the initial Notion paragraph"
    }

    $editMarker = "Windows Cloud Files live edit $unique"
    Write-Step "editing hydrated page.md"
    Write-Utf8NoBom -Path $pageFile -Contents ($hydrated.TrimEnd() + "`n`n$editMarker`n")
    Wait-ForStatusContains -Path $pageFile -Needle '"local_body_changed"'
    Write-Step "pushing page edit"
    Invoke-Native -FilePath $locBin -Arguments @("push", $pageFile, "-y", "--json") -Step "push edited page" | Out-Null
    Wait-ForStatusContains -Path $pageFile -Needle '"state": "clean"'
    Write-Step "verifying page edit in Notion"
    $remoteText = Get-NotionBlockText -PageId $scratchPageId
    if (-not $remoteText.Contains($editMarker)) {
        throw "Notion page did not contain the pushed edit marker"
    }

    $draftFile = Join-Path $pageDir ("draft-$unique.md")
    $renamedDraftFile = Join-Path $pageDir ("draft-renamed-$unique.md")
    Write-Step "creating and renaming pending draft file"
    Write-Utf8NoBom -Path $draftFile -Contents "---`ntitle: `"Draft $unique`"`n---`n# Draft`n`nCreated through Cloud Files and not pushed.`n"
    Rename-ItemWithTimeout -Path $draftFile -NewName (Split-Path -Leaf $renamedDraftFile)
    Wait-ForStatusContains -Path $renamedDraftFile -Needle '"pending_virtual_create"'
    Write-Step "deleting pending draft file"
    Remove-ItemWithTimeout -Path $renamedDraftFile
    Wait-ForStatusContains -Path $pageDir -Needle '"clean": true'

    $childTitle = "Locality Cloud Files child $unique"
    $childDir = Join-Path $pageDir (ConvertTo-LocalitySlug $childTitle)
    $childPage = Join-Path $childDir "page.md"
    $childMarker = "Windows Cloud Files created child $unique"
    Write-Step "creating child page directory"
    New-DirectoryWithTimeout -Path $childDir
    Write-Utf8NoBom -Path $childPage -Contents "---`ntitle: `"$childTitle`"`n---`n# Created child`n`n$childMarker`n"
    Wait-ForStatusContains -Path $childPage -Needle '"pending_virtual_create"'
    Write-Step "pushing created child page"
    $pushChildOutput = Invoke-Native -FilePath $locBin -Arguments @("push", $childPage, "-y", "--json") -Step "push created child page"
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

    Write-Step "deleting child page directory"
    Remove-ItemWithTimeout -Path $childDir -Recurse
    Wait-ForStatusContains -Path $pageDir -Needle '"pending_virtual_delete"'
    Write-Step "pushing child page archive"
    Invoke-Native -FilePath $locBin -Arguments @("push", $pageDir, "-y", "--json") -Step "push deleted child page" | Out-Null
    Assert-NotionPageArchived -PageId $createdChildPageId

    Write-Host "ok: Windows Cloud Files live Notion e2e completed"
} catch {
    $failed = $true
    Write-Error $_ -ErrorAction Continue
} finally {
    if (Test-Path -LiteralPath $locBin) {
        try {
            Invoke-Native -FilePath $locBin -Arguments @("file-provider", "stop", $mountId, "--json") -Step "loc file-provider stop" | Out-Null
        } catch {
            Write-Warning "Cloud Files provider stop failed: $($_.Exception.Message)"
        }
        try {
            Invoke-Native -FilePath $locBin -Arguments @("file-provider", "unregister", $mountId, "--json") -Step "loc file-provider unregister" | Out-Null
        } catch {
            Write-Warning "Cloud Files unregister failed: $($_.Exception.Message)"
        }
        if ($tcpAddr) {
            try {
                Invoke-Native -FilePath $locBin -Arguments @("daemon", "stop", "--state-dir", $stateRoot, "--tcp-addr", $tcpAddr, "--json") -Step "loc daemon stop" | Out-Null
            } catch {
                Write-Warning "localityd stop failed: $($_.Exception.Message)"
            }
        }
    }
    Archive-NotionPage -PageId $createdChildPageId
    Archive-NotionPage -PageId $scratchPageId

    if ($failed) {
        $logs = @()
        $logRoot = Join-Path $stateRoot "logs"
        if (Test-Path -LiteralPath $logRoot) {
            $logs += Get-ChildItem -LiteralPath $logRoot -Filter "*.log" -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }
        }
        foreach ($log in $logs) {
            if (Test-Path -LiteralPath $log) {
                Write-Host "----- $log -----"
                Get-Content -LiteralPath $log -ErrorAction SilentlyContinue
            }
        }
    }

    if ($env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE_KEEP_TMP -eq "1") {
        Write-Host "kept Windows Cloud Files live temp root: $tmpRoot"
    } elseif ((Test-Path -LiteralPath $tmpRoot) -and $tmpRoot.StartsWith([System.IO.Path]::GetTempPath(), [System.StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $tmpRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($failed) {
    exit 1
}
