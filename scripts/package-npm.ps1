[CmdletBinding()]
param(
    [string]$BinaryDir = "",
    [string]$OutputDir = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
if (-not $BinaryDir) { $BinaryDir = Join-Path $root "target\release" }
if (-not $OutputDir) { $OutputDir = Join-Path $root "dist" }
$BinaryDir = (Resolve-Path -LiteralPath $BinaryDir).Path
$OutputDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDir)
$tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$stage = Join-Path $tempRoot ("runspur-npm-package-" + [Guid]::NewGuid().ToString("N"))
$null = [IO.Directory]::CreateDirectory((Join-Path $stage "native"))

Push-Location -LiteralPath $root
try {
    $metadata = & cargo metadata --no-deps --format-version 1 --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed." }
    $version = ($metadata | ConvertFrom-Json).packages.Where({ $_.name -eq "scheduler-app" })[0].version
    $manifest = Get-Content -LiteralPath "apps/runspur-cli/package.json" -Raw | ConvertFrom-Json
    $manifest.version = $version
    $manifest.PSObject.Properties.Remove("private")
    $archive = Join-Path $OutputDir "$($manifest.name)-$version.tgz"
    if (Test-Path -LiteralPath $archive) { throw "The npm tarball already exists at $archive. Use a fresh output directory." }

    $nativePaths = @($manifest.bin.runspur, "native/scheduler-mcp.exe")
    foreach ($relativePath in $nativePaths) {
        $binary = Join-Path $BinaryDir ([IO.Path]::GetFileName($relativePath))
        $output = & $binary --version
        if ($LASTEXITCODE -ne 0 -or ([string]$output).Trim() -notmatch ('\s' + [Regex]::Escape($version) + '$')) {
            throw "Binary $binary does not match Cargo version $version."
        }
        [IO.File]::Copy($binary, (Join-Path $stage $relativePath))
    }
    $utf8 = [Text.UTF8Encoding]::new($false)
    [IO.File]::WriteAllText((Join-Path $stage "package.json"), ($manifest | ConvertTo-Json -Depth 8), $utf8)
    [IO.File]::Copy((Join-Path $root "LICENSE"), (Join-Path $stage "LICENSE"))
    [IO.File]::Copy((Join-Path $root "docs\cli-npm.md"), (Join-Path $stage "README.md"))
    & (Join-Path $PSScriptRoot "generate-notices.ps1") -OutputPath (Join-Path $stage "THIRD_PARTY_NOTICES.txt")

    $null = [IO.Directory]::CreateDirectory($OutputDir)
    npm.cmd pack $stage --pack-destination $OutputDir --ignore-scripts
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $archive -PathType Leaf)) { throw "npm pack failed." }
    Write-Host "Packaged $archive"
} finally {
    Pop-Location
    $target = (Resolve-Path -LiteralPath $stage).Path
    if ([IO.Path]::GetDirectoryName($target) -ne $tempRoot.TrimEnd('\') -or [IO.Path]::GetFileName($target) -notmatch '^runspur-npm-package-[0-9a-f]{32}$') {
        throw "Unexpected npm packaging cleanup directory: $target"
    }
    Remove-Item -LiteralPath $target -Recurse -Force
}
