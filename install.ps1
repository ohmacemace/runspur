[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$InstallDir = "",
    [switch]$NoPathUpdate,
    [string]$ArchivePath = "",
    [string]$ChecksumPath = ""
)

& {
    Set-StrictMode -Version Latest
    $ErrorActionPreference = "Stop"
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        throw "This release supports Windows x64 only."
    }
    $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    if ($architecture -ne "X64") { throw "Unsupported OS architecture: $architecture. Windows x64 is required." }
    if ([string]::IsNullOrWhiteSpace($InstallDir)) {
        $InstallDir = Join-Path ([Environment]::GetFolderPath("LocalApplicationData")) "Runspur\bin"
    }
    $InstallDir = [IO.Path]::GetFullPath($InstallDir)
    $repository = "ohmacemace/runspur"
    $files = @("schedulerd.exe", "schedulerctl.exe", "scheduler-mcp.exe", "README.md", "LICENSE", "THIRD_PARTY_NOTICES.txt")
    $markerName = ".runspur-install.json"
    $temporary = Join-Path ([IO.Path]::GetTempPath()) ("runspur-install-" + [Guid]::NewGuid().ToString("N"))
    $null = [IO.Directory]::CreateDirectory($temporary)
    $lock = $null
    $incoming = $null
    $backup = $null
    $keepBackup = $false
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $headers = @{ "User-Agent" = "Runspur-installer" }
        $offline = -not [string]::IsNullOrWhiteSpace($ArchivePath)
        if ($offline -ne (-not [string]::IsNullOrWhiteSpace($ChecksumPath))) {
            throw "Offline installation requires both -ArchivePath and -ChecksumPath."
        }
        if ($Version -eq "latest") {
            if ($offline) { throw "Offline installation requires an explicit -Version, for example v0.1.0." }
            # Public release redirects do not consume the anonymous GitHub REST API quota.
            $response = Invoke-WebRequest -UseBasicParsing -Method Head -Uri "https://github.com/$repository/releases/latest" -Headers $headers -MaximumRedirection 8
            if ($null -ne $response.BaseResponse.PSObject.Properties["ResponseUri"]) {
                $resolved = [Uri]$response.BaseResponse.ResponseUri
            } else {
                $resolved = [Uri]$response.BaseResponse.RequestMessage.RequestUri
            }
            $releasePath = '^/' + [Regex]::Escape($repository) + '/releases/tag/(v[^/]+)/?$'
            if ($resolved.Host -ne "github.com" -or $resolved.AbsolutePath -notmatch $releasePath) {
                throw "Could not resolve a release tag from the public latest-release redirect."
            }
            $Version = [Uri]::UnescapeDataString($Matches[1])
        }
        if ($Version -notmatch '^v?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') { throw "Invalid release version: $Version" }
        if (-not $Version.StartsWith("v")) { $Version = "v$Version" }
        $expectedVersion = $Version.Substring(1)
        $assetName = "runspur-$Version-windows-x86_64.zip"
        $archive = Join-Path $temporary $assetName
        $checksums = Join-Path $temporary "SHA256SUMS.txt"
        if ($offline) {
            [IO.File]::Copy([IO.Path]::GetFullPath($ArchivePath), $archive)
            [IO.File]::Copy([IO.Path]::GetFullPath($ChecksumPath), $checksums)
        } else {
            $base = "https://github.com/$repository/releases/download/$Version"
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$assetName" -Headers $headers -OutFile $archive
            Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS.txt" -Headers $headers -OutFile $checksums
        }
        $pattern = '(?m)^([a-fA-F0-9]{64})[ \t]+\*?' + [Regex]::Escape($assetName) + '[ \t]*\r?$'
        $entries = [Regex]::Matches([IO.File]::ReadAllText($checksums), $pattern)
        if ($entries.Count -ne 1) { throw "Expected exactly one checksum for $assetName." }
        $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
        if (-not $actual.Equals($entries[0].Groups[1].Value, [StringComparison]::OrdinalIgnoreCase)) {
            throw "SHA-256 mismatch. The installation was not changed."
        }

        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        try {
            [long]$size = 0
            foreach ($entry in $zip.Entries) {
                if ($files -cnotcontains $entry.FullName) { throw "Unexpected archive entry: $($entry.FullName)" }
                $size += $entry.Length
            }
            if ($size -gt 512MB) { throw "Archive exceeds the extraction size limit." }
            foreach ($name in $files) {
                if (@($zip.Entries | Where-Object { $_.FullName -ceq $name }).Count -ne 1) {
                    throw "Archive must contain exactly one $name."
                }
            }
        } finally { $zip.Dispose() }
        $extracted = Join-Path $temporary "extracted"
        [IO.Compression.ZipFile]::ExtractToDirectory($archive, $extracted)
        foreach ($name in $files[0..2]) {
            $output = & (Join-Path $extracted $name) --version
            if ($LASTEXITCODE -ne 0 -or ([string]$output).Trim() -notmatch ('\s' + [Regex]::Escape($expectedVersion) + '$')) {
                throw "Program version does not match $Version`: $name"
            }
        }

        if (Test-Path -LiteralPath $InstallDir) {
            $info = Get-Item -LiteralPath $InstallDir -Force
            if (-not $info.PSIsContainer -or ($info.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                throw "InstallDir must be a regular directory."
            }
        }
        $null = [IO.Directory]::CreateDirectory($InstallDir)
        $lockPath = Join-Path $InstallDir ".runspur-install.lock"
        $lock = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        $marker = Join-Path $InstallDir $markerName
        if (Test-Path -LiteralPath $marker) {
            $previous = [IO.File]::ReadAllText($marker) | ConvertFrom-Json
            if ($previous.schema_version -ne 1 -or $previous.product -ne "runspur") { throw "Unrecognized installation marker." }
        } else {
            foreach ($name in $files) {
                if (Test-Path -LiteralPath (Join-Path $InstallDir $name)) {
                    throw "Refusing to overwrite an unmanaged file: $name"
                }
            }
        }
        $incoming = Join-Path $InstallDir (".runspur-incoming-" + [Guid]::NewGuid().ToString("N"))
        $backup = Join-Path $InstallDir (".runspur-backup-" + [Guid]::NewGuid().ToString("N"))
        $null = [IO.Directory]::CreateDirectory($incoming)
        $null = [IO.Directory]::CreateDirectory($backup)
        foreach ($name in $files) { [IO.File]::Copy((Join-Path $extracted $name), (Join-Path $incoming $name)) }
        $metadata = @{ schema_version = 1; product = "runspur"; version = $expectedVersion; installed_at = [DateTime]::UtcNow.ToString("o") } | ConvertTo-Json
        [IO.File]::WriteAllText((Join-Path $incoming $markerName), $metadata, [Text.UTF8Encoding]::new($false))
        $installed = [Collections.Generic.List[string]]::new()
        $saved = [Collections.Generic.List[string]]::new()
        try {
            foreach ($name in @($files) + @($markerName)) {
                $destination = Join-Path $InstallDir $name
                if (Test-Path -LiteralPath $destination) {
                    Move-Item -LiteralPath $destination -Destination (Join-Path $backup $name)
                    $saved.Add($name)
                }
                Move-Item -LiteralPath (Join-Path $incoming $name) -Destination $destination
                $installed.Add($name)
            }
        } catch {
            $failure = $_
            try {
                foreach ($name in $installed) { Remove-Item -LiteralPath (Join-Path $InstallDir $name) -Force }
                foreach ($name in $saved) { Move-Item -LiteralPath (Join-Path $backup $name) -Destination (Join-Path $InstallDir $name) }
            } catch {
                $keepBackup = $true
                throw "Update failed and rollback needs manual recovery from $backup. Close Runspur programs before updating."
            }
            throw $failure
        }
        if (-not $NoPathUpdate) {
            $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
            $parts = @($userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
            $normalized = @($parts | ForEach-Object { $_.Trim().Trim('"').TrimEnd('\') })
            if ($normalized -inotcontains $InstallDir.TrimEnd('\')) {
                [Environment]::SetEnvironmentVariable("Path", (@($parts) + @($InstallDir) -join ';'), "User")
            }
            $current = @($env:Path -split ';' | ForEach-Object { $_.Trim().Trim('"').TrimEnd('\') })
            if ($current -inotcontains $InstallDir.TrimEnd('\')) { $env:Path = "$InstallDir;$env:Path" }
        }
        Write-Host "Installed Runspur $expectedVersion to $InstallDir"
        Write-Host "Start the daemon: schedulerd"
        Write-Host "Manage projects: schedulerctl --help"
        if (-not $NoPathUpdate) { Write-Host "Open a new terminal if an existing terminal cannot find the programs." }
    } finally {
        if ($null -ne $lock) { $lock.Dispose(); Remove-Item -LiteralPath $lockPath -Force -ErrorAction SilentlyContinue }
        if ($null -ne $incoming -and (Test-Path -LiteralPath $incoming)) { Remove-Item -LiteralPath $incoming -Recurse -Force }
        if (-not $keepBackup -and $null -ne $backup -and (Test-Path -LiteralPath $backup)) { Remove-Item -LiteralPath $backup -Recurse -Force }
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Recurse -Force }
    }
}
