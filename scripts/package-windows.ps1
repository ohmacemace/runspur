[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [string]$BinaryDir = "",
    [string]$OutputDir = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
if ($Tag -notmatch '^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') { throw "Invalid version tag: $Tag" }
$version = $Tag.Substring(1)
if (-not $BinaryDir) { $BinaryDir = Join-Path $root "target\release" }
if (-not $OutputDir) { $OutputDir = Join-Path $root "dist" }
$metadataText = & cargo metadata --no-deps --format-version 1 --locked
if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed." }
$metadata = $metadataText | ConvertFrom-Json
foreach ($package in $metadata.packages) {
    if ($package.version -ne $version) { throw "Tag $Tag differs from Cargo version $($package.version)." }
}
$null = [IO.Directory]::CreateDirectory([IO.Path]::GetFullPath($OutputDir))
$archive = Join-Path $OutputDir "runspur-$Tag-windows-x86_64.zip"
$checksumPath = Join-Path $OutputDir "SHA256SUMS.txt"
if (Test-Path -LiteralPath $archive) { throw "Release archive already exists; use a fresh output directory." }
$stage = Join-Path ([IO.Path]::GetTempPath()) ("runspur-package-" + [Guid]::NewGuid().ToString("N"))
$null = [IO.Directory]::CreateDirectory($stage)
try {
    foreach ($name in @("schedulerd.exe", "schedulerctl.exe", "scheduler-mcp.exe")) {
        $binary = Join-Path $BinaryDir $name
        $output = & $binary --version
        if ($LASTEXITCODE -ne 0 -or ([string]$output).Trim() -notmatch ('\s' + [Regex]::Escape($version) + '$')) {
            throw "Binary $name does not match $Tag."
        }
        [IO.File]::Copy([IO.Path]::GetFullPath($binary), (Join-Path $stage $name))
    }
    foreach ($name in @("README.md", "LICENSE")) { [IO.File]::Copy((Join-Path $root $name), (Join-Path $stage $name)) }
    & (Join-Path $PSScriptRoot "generate-notices.ps1") -OutputPath (Join-Path $stage "THIRD_PARTY_NOTICES.txt")
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::CreateFromDirectory($stage, [IO.Path]::GetFullPath($archive))
    $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText([IO.Path]::GetFullPath($checksumPath), "$hash  $([IO.Path]::GetFileName($archive))`n", [Text.UTF8Encoding]::new($false))
    Write-Host "Packaged $archive"
} finally {
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
}
