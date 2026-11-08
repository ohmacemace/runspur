[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [string]$PackageDir = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
if (-not $PackageDir) { $PackageDir = Join-Path $root "dist" }
$archiveName = "runspur-$Tag-windows-x86_64.zip"
$archive = [IO.Path]::GetFullPath((Join-Path $PackageDir $archiveName))
$checksums = [IO.Path]::GetFullPath((Join-Path $PackageDir "SHA256SUMS.txt"))
$installer = Join-Path $root "install.ps1"
$temporary = Join-Path ([IO.Path]::GetTempPath()) ("runspur-distribution-test-" + [Guid]::NewGuid().ToString("N"))
$null = [IO.Directory]::CreateDirectory($temporary)
$install = Join-Path $temporary ("installed " + [char]0x6D4B + [char]0x8BD5)
$pathBefore = [Environment]::GetEnvironmentVariable("Path", "User")
$processPathBefore = $env:Path
$service = $null
$script:passed = 0

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Passed([string]$Name) {
    $script:passed++
    Write-Host "PASS: $Name"
}
function Expect-Failure([scriptblock]$Action, [string]$Pattern = "") {
    $failed = $false
    try { & $Action } catch {
        $failed = $true
        if ($Pattern -and $_.Exception.Message -notmatch $Pattern) { throw }
    }
    Assert-True $failed "The operation unexpectedly succeeded."
}
function Install-Local([string]$Directory = $install) {
    & $installer -Version $Tag -ArchivePath $archive -ChecksumPath $checksums -InstallDir $Directory -NoPathUpdate
}

try {
    Install-Local
    $marker = Join-Path $install ".runspur-install.json"
    $metadata = [IO.File]::ReadAllText($marker) | ConvertFrom-Json
    Assert-True ($metadata.version -eq $Tag.Substring(1)) "Installed version does not match the tag."
    Assert-True ([IO.File]::ReadAllText((Join-Path $install "LICENSE")).Contains("Copyright (c) 2026 mace")) "Copyright notice was not retained."
    Assert-True ((Get-Item -LiteralPath (Join-Path $install "THIRD_PARTY_NOTICES.txt")).Length -gt 0) "Third-party notices are missing."
    Passed "Verified offline install with spaces and Unicode in the path"

    Install-Local
    Assert-True (([IO.File]::ReadAllText($marker) | ConvertFrom-Json).version -eq $metadata.version) "Repeat installation changed the version."
    Passed "Repeat installation into a managed directory"

    # Make the prior binary observably different so rollback must restore its exact bytes.
    $daemon = Join-Path $install "schedulerd.exe"
    $oldBytes = [IO.File]::ReadAllBytes($daemon)
    [IO.File]::WriteAllBytes($daemon, [byte[]]($oldBytes + [byte[]]@(1, 2, 3, 4)))
    $oldHash = (Get-FileHash -LiteralPath $daemon -Algorithm SHA256).Hash
    $oldMarker = [IO.File]::ReadAllText($marker)
    $held = [IO.File]::Open((Join-Path $install "schedulerctl.exe"), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try { Expect-Failure { Install-Local } } finally { $held.Dispose() }
    Assert-True ((Get-FileHash -LiteralPath $daemon -Algorithm SHA256).Hash -eq $oldHash) "A failed update did not restore the previous binary."
    Assert-True ([IO.File]::ReadAllText($marker) -eq $oldMarker) "A failed update changed the installation marker."
    Install-Local
    Passed "Locked-file update rolls back already replaced files"

    $unmanaged = Join-Path $temporary "unmanaged"
    $null = [IO.Directory]::CreateDirectory($unmanaged)
    $owned = Join-Path $unmanaged "schedulerctl.exe"
    [IO.File]::WriteAllText($owned, "user-owned")
    Expect-Failure { Install-Local $unmanaged } "unmanaged"
    Assert-True ([IO.File]::ReadAllText($owned) -eq "user-owned") "An unmanaged file was overwritten."
    Passed "Unmanaged file collision is preserved"

    $bad = Join-Path $temporary "corrupt"
    $null = [IO.Directory]::CreateDirectory($bad)
    $badArchive = Join-Path $bad $archiveName
    [IO.File]::Copy($archive, $badArchive)
    $stream = [IO.File]::Open($badArchive, [IO.FileMode]::Append, [IO.FileAccess]::Write)
    try { $stream.WriteByte(1) } finally { $stream.Dispose() }
    Expect-Failure { & $installer -Version $Tag -ArchivePath $badArchive -ChecksumPath $checksums -InstallDir $install -NoPathUpdate } "SHA-256 mismatch"
    Passed "Corrupt archive is rejected before installation"

    $evil = Join-Path $temporary "traversal"
    $null = [IO.Directory]::CreateDirectory($evil)
    $evilArchive = Join-Path $evil $archiveName
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::Open($evilArchive, [IO.Compression.ZipArchiveMode]::Create)
    try {
        $entry = $zip.CreateEntry("../outside.txt")
        $entryStream = $entry.Open()
        try { $entryStream.WriteByte(1) } finally { $entryStream.Dispose() }
    } finally { $zip.Dispose() }
    $evilChecksum = Join-Path $evil "SHA256SUMS.txt"
    $evilHash = (Get-FileHash -LiteralPath $evilArchive -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText($evilChecksum, "$evilHash  $archiveName`n")
    Expect-Failure { & $installer -Version $Tag -ArchivePath $evilArchive -ChecksumPath $evilChecksum -InstallDir $install -NoPathUpdate } "Unexpected archive entry"
    Passed "Checksum-valid path-traversal archive is rejected"

    Assert-True ([Environment]::GetEnvironmentVariable("Path", "User") -eq $pathBefore) "NoPathUpdate modified the user PATH."
    Assert-True ($env:Path -eq $processPathBefore) "NoPathUpdate modified the process PATH."
    Passed "NoPathUpdate preserves both PATH values"

    $state = Join-Path $temporary "state"
    $project = Join-Path $temporary "project space"
    $null = [IO.Directory]::CreateDirectory($project)
    $service = Start-Process -FilePath $daemon -ArgumentList @("--data-dir", ('"' + $state + '"')) -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $temporary "daemon.stdout.log") -RedirectStandardError (Join-Path $temporary "daemon.stderr.log")
    $control = Join-Path $install "schedulerctl.exe"
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    $healthy = $false
    do {
        Start-Sleep -Milliseconds 100
        if ($service.HasExited) { throw "The installed daemon exited before becoming healthy." }
        try {
            $health = & $control --data-dir $state doctor 2>$null
            if ($LASTEXITCODE -eq 0) {
                $parsed = $health | ConvertFrom-Json
                $healthy = $parsed.service.version -eq $Tag.Substring(1)
            }
        } catch { $healthy = $false }
    } while (-not $healthy -and [DateTime]::UtcNow -lt $deadline)
    Assert-True $healthy "The installed daemon did not become healthy."
    $saved = & $control --data-dir $state init --project-root $project --name "distribution-test"
    Assert-True ($LASTEXITCODE -eq 0) "The installed CLI could not initialize a project."
    $null = $saved | ConvertFrom-Json
    $integrated = & $control --data-dir $state integrate --project-root $project --mcp-program (Join-Path $install "scheduler-mcp.exe")
    Assert-True ($LASTEXITCODE -eq 0) "The installed CLI could not install project integration."
    Assert-True (($integrated | ConvertFrom-Json).state -eq "saved_requires_host_reload") "Unexpected project integration state."
    Passed "Installed release programs run a real daemon/CLI/project round trip"
    Write-Host "Distribution tests passed: $script:passed"
} finally {
    if ($null -ne $service -and -not $service.HasExited) { $service.Kill(); $null = $service.WaitForExit(5000) }
    if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Recurse -Force }
}
