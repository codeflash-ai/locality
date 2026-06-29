$ErrorActionPreference = "Stop"

function Test-LocalityWindowsCodeSigningRequested {
    return (
        $env:LOCALITY_WINDOWS_CODESIGN -eq "1" -or
        -not [string]::IsNullOrWhiteSpace($env:WINDOWS_CODESIGN_CERT_SHA1) -or
        -not [string]::IsNullOrWhiteSpace($env:WINDOWS_CODESIGN_CERT_SUBJECT) -or
        -not [string]::IsNullOrWhiteSpace($env:WINDOWS_CODESIGN_SUBJECT)
    )
}

function Test-LocalityWindowsExternalCodeSigningRequested {
    return $env:LOCALITY_WINDOWS_EXTERNAL_CODESIGN -eq "1"
}

function Get-LocalitySignTool {
    if (-not [string]::IsNullOrWhiteSpace($env:WINDOWS_SIGNTOOL)) {
        if (Test-Path -LiteralPath $env:WINDOWS_SIGNTOOL) {
            return (Resolve-Path -LiteralPath $env:WINDOWS_SIGNTOOL).Path
        }
        throw "WINDOWS_SIGNTOOL points at a missing file: $env:WINDOWS_SIGNTOOL"
    }

    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $roots = @(
        (Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"),
        (Join-Path $env:ProgramFiles "Windows Kits\10\bin")
    ) | Where-Object { $_ -and (Test-Path -LiteralPath $_) }

    foreach ($root in $roots) {
        $candidate = Get-ChildItem -LiteralPath $root -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match "\\x64\\signtool\.exe$" } |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($candidate) {
            return $candidate.FullName
        }
    }

    throw "signtool.exe was not found. Install the Windows SDK or set WINDOWS_SIGNTOOL."
}

function Invoke-LocalityWindowsCodeSign {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    if (-not (Test-LocalityWindowsCodeSigningRequested)) {
        return $false
    }
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "Cannot sign missing file: $Path"
    }

    $signTool = Get-LocalitySignTool
    $timestamp = if ($env:WINDOWS_CODESIGN_TIMESTAMP_URL) {
        $env:WINDOWS_CODESIGN_TIMESTAMP_URL
    } else {
        "http://timestamp.digicert.com"
    }
    $subject = if ($env:WINDOWS_CODESIGN_CERT_SUBJECT) {
        $env:WINDOWS_CODESIGN_CERT_SUBJECT
    } else {
        $env:WINDOWS_CODESIGN_SUBJECT
    }

    $args = @("sign", "/fd", "SHA256", "/td", "SHA256", "/tr", $timestamp)
    if ($env:WINDOWS_CODESIGN_CERT_SHA1) {
        $args += @("/sha1", $env:WINDOWS_CODESIGN_CERT_SHA1)
    } elseif ($subject) {
        $args += @("/n", $subject)
    } else {
        throw "Windows code signing was requested, but WINDOWS_CODESIGN_CERT_SHA1 or WINDOWS_CODESIGN_CERT_SUBJECT is not set."
    }
    $args += $Path

    & $signTool @args
    if ($LASTEXITCODE -ne 0) {
        throw "signtool failed for $Path with exit code $LASTEXITCODE"
    }
    return $true
}

function Assert-LocalityWindowsSigned {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if (-not $signature.SignerCertificate) {
        throw "Expected a Windows Authenticode signature on $Path"
    }
}
