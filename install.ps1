$ErrorActionPreference = "Stop"

$Repo = if ($env:FERRUS_INSTALL_REPO) { $env:FERRUS_INSTALL_REPO } else { "RomanEmreis/ferrus" }
$Version = if ($env:FERRUS_INSTALL_VERSION) { $env:FERRUS_INSTALL_VERSION } else { "latest" }
$Target = ""
$Archive = ""
$ChecksumFile = ""
$ReleaseUrl = ""
$ChecksumUrl = ""
$InstallDir = ""
$TempDir = ""
$HasChecksum = $false

function Get-Target {
    $arch = $env:PROCESSOR_ARCHITECTURE
    $archWow64 = $env:PROCESSOR_ARCHITEW6432

    if ($archWow64) {
        $arch = $archWow64
    }

    if (-not $arch) {
        try {
            $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        } catch {
            $arch = $null
        }
    }

    if (-not $arch) {
        throw "failed to detect Windows architecture"
    }

    switch ($arch.ToUpperInvariant()) {
        "AMD64" { return "x86_64-pc-windows-msvc" }
        "X64"   { return "x86_64-pc-windows-msvc" }
        "ARM64" { throw "unsupported Windows architecture: ARM64. Supported target: x86_64" }
        default { throw "unsupported Windows architecture: $arch. Supported target: x86_64" }
    }
}

function Resolve-Urls {
    if ($Version -eq "latest") {
        $script:ReleaseUrl = "https://github.com/$Repo/releases/latest/download/$Archive"
        $script:ChecksumUrl = "https://github.com/$Repo/releases/latest/download/$ChecksumFile"
    } else {
        $script:ReleaseUrl = "https://github.com/$Repo/releases/download/$Version/$Archive"
        $script:ChecksumUrl = "https://github.com/$Repo/releases/download/$Version/$ChecksumFile"
    }
}

function Get-InstallDir {
    if ($env:FERRUS_INSTALL_DIR) {
        return $env:FERRUS_INSTALL_DIR
    }

    if ($env:LOCALAPPDATA) {
        return (Join-Path $env:LOCALAPPDATA "ferrus\bin")
    }

    return (Join-Path $HOME ".local\bin")
}

function Download-File {
    param(
        [string]$Url,
        [string]$Output,
        [string]$Label,
        [string]$AssetName,
        [bool]$AllowMissing = $false
    )

    try {
        $requestParams = @{
            Uri     = $Url
            OutFile = $Output
        }

        if ($PSVersionTable.PSEdition -ne "Core") {
            $requestParams.UseBasicParsing = $true
        }

        Invoke-WebRequest @requestParams
        return $true
    } catch {
        $statusCode = $null
        if ($_.Exception.Response -and $_.Exception.Response.StatusCode) {
            $statusCode = [int]$_.Exception.Response.StatusCode
        }

        if ($statusCode -eq 404 -and $AllowMissing) {
            return $false
        }

        if ($statusCode -eq 404) {
            throw "$Label was not found: $Url`nhint: the requested release may not include asset $AssetName for version $Version"
        }

        if ($statusCode) {
            throw "failed to download $Label from $Url (HTTP $statusCode)"
        }

        throw "failed to download $Label from $Url"
    }
}

function Download-Archive {
    Download-File -Url $ReleaseUrl -Output (Join-Path $TempDir $Archive) -Label "release archive" -AssetName $Archive | Out-Null
}

function Download-Checksum {
    $checksumPath = Join-Path $TempDir $ChecksumFile
    if (Download-File -Url $ChecksumUrl -Output $checksumPath -Label "checksum file" -AssetName $ChecksumFile -AllowMissing $true) {
        $script:HasChecksum = $true
        return
    }

    if ($Version -eq "latest") {
        throw "checksum file was not found: $ChecksumUrl`nhint: the latest release is expected to publish asset $ChecksumFile"
    }

    $script:HasChecksum = $false
    Write-Warning "checksum file $ChecksumFile is not available for $Version; proceeding without checksum verification for this pinned install"
}

function Verify-Checksum {
    if (-not $HasChecksum) {
        return
    }

    $checksum = (Get-Content (Join-Path $TempDir $ChecksumFile) | Select-Object -First 1).Split(" ", [System.StringSplitOptions]::RemoveEmptyEntries)[0]
    if (-not $checksum) {
        throw "checksum file is empty or malformed: $ChecksumFile"
    }

    $actualChecksum = (Get-FileHash (Join-Path $TempDir $Archive) -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($checksum.ToLowerInvariant() -ne $actualChecksum) {
        throw "checksum verification failed for $Archive`nexpected: $checksum`nactual:   $actualChecksum"
    }
}

function Verify-ArchiveLayout {
    $extractDir = Join-Path $TempDir "verify"
    Expand-Archive -Path (Join-Path $TempDir $Archive) -DestinationPath $extractDir -Force

    $expectedPath = Join-Path $extractDir "ferrus-$Target\ferrus.exe"
    if (-not (Test-Path $expectedPath)) {
        throw "archive does not contain expected binary entry: ferrus-$Target/ferrus.exe"
    }

    Remove-Item -Recurse -Force $extractDir
}

function Install-Binary {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $HOME ".ferrus\projects") | Out-Null
    Expand-Archive -Path (Join-Path $TempDir $Archive) -DestinationPath $TempDir -Force

    $binPath = Join-Path $TempDir "ferrus-$Target\ferrus.exe"
    if (-not (Test-Path $binPath)) {
        throw "ferrus.exe not found in archive"
    }

    $destination = Join-Path $InstallDir "ferrus.exe"
    if (Test-Path $destination) {
        Write-Warning "overwriting existing installation at $destination"
    }

    Copy-Item $binPath $destination -Force
}

function Print-Success {
    $destination = Join-Path $InstallDir "ferrus.exe"
    $installedVersion = ""
    try {
        $installedVersion = (& $destination --version 2>$null)
    } catch {
        $installedVersion = ""
    }

    Write-Host "installed ferrus to $destination"
    if ($installedVersion) {
        Write-Host "version: $installedVersion"
    } else {
        Write-Warning "failed to determine installed ferrus version"
    }

    $pathEntries = $env:PATH -split ';'
    if ($pathEntries -notcontains $InstallDir) {
        Write-Warning "$InstallDir is not on PATH"
        Write-Warning "add this to your PowerShell profile:"
        Write-Warning "  `$env:PATH = `"$InstallDir;`$env:PATH`""
    }
}

$Target = Get-Target
$Archive = "ferrus-$Target.zip"
$ChecksumFile = "$Archive.sha256"
$InstallDir = Get-InstallDir

Resolve-Urls

Write-Host "installing ferrus ($Version) for $Target"
Write-Host "download: $ReleaseUrl"

$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("ferrus-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TempDir | Out-Null

try {
    Download-Archive
    Download-Checksum
    Verify-Checksum
    Verify-ArchiveLayout
    Install-Binary
    Print-Success
} finally {
    if (Test-Path $TempDir) {
        Remove-Item -Recurse -Force $TempDir
    }
}
