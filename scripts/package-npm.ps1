[CmdletBinding()]
param(
    [string]$BinaryDir = "",
    [string]$OutputDir = "",
    [string]$UiDir = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
if (-not $BinaryDir) { $BinaryDir = Join-Path $root "target\release" }
if (-not $OutputDir) { $OutputDir = Join-Path $root "dist" }
if (-not $UiDir) { $UiDir = Join-Path $root "apps\runspur-desktop\dist" }
$BinaryDir = (Resolve-Path -LiteralPath $BinaryDir).Path
$uiIndex = Join-Path $UiDir "index.html"
$assetDir = Join-Path $UiDir "assets"
if (-not (Test-Path -LiteralPath $uiIndex -PathType Leaf) -or -not (Test-Path -LiteralPath $assetDir -PathType Container)) {
    throw "Built web UI not found at $UiDir. Run npm.cmd run ui:build in apps/runspur-desktop."
}
$uiIndex = (Resolve-Path -LiteralPath $uiIndex).Path
$assets = @(Get-ChildItem -LiteralPath $assetDir -File)
if (-not ($assets | Where-Object Extension -eq ".js")) {
    throw "Built web UI at $UiDir is missing its Vite JavaScript assets."
}
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

    $nativePaths = @($manifest.bin.runspur, "native/schedulerd.exe", "native/scheduler-mcp.exe")
    foreach ($relativePath in $nativePaths) {
        $binary = Join-Path $BinaryDir ([IO.Path]::GetFileName($relativePath))
        $output = & $binary --version
        if ($LASTEXITCODE -ne 0 -or ([string]$output).Trim() -notmatch ('\s' + [Regex]::Escape($version) + '$')) {
            throw "Binary $binary does not match Cargo version $version."
        }
        [IO.File]::Copy($binary, (Join-Path $stage $relativePath))
    }
    $web = Join-Path $stage "web"
    $webAssets = Join-Path $web "assets"
    $null = [IO.Directory]::CreateDirectory($webAssets)
    [IO.File]::Copy($uiIndex, (Join-Path $web "index.html"))
    foreach ($asset in $assets) {
        [IO.File]::Copy($asset.FullName, (Join-Path $webAssets $asset.Name))
    }
    $utf8 = [Text.UTF8Encoding]::new($false)
    [IO.File]::WriteAllText((Join-Path $stage "package.json"), ($manifest | ConvertTo-Json -Depth 8), $utf8)
    [IO.File]::Copy((Join-Path $root "LICENSE"), (Join-Path $stage "LICENSE"))
    [IO.File]::Copy((Join-Path $root "docs\cli-npm.md"), (Join-Path $stage "README.md"))
    $notices = Join-Path $stage "THIRD_PARTY_NOTICES.txt"
    & (Join-Path $PSScriptRoot "generate-notices.ps1") -OutputPath $notices
    $tauriApi = Join-Path $root "apps\runspur-desktop\node_modules\@tauri-apps\api"
    $tauriManifest = Get-Content -LiteralPath (Join-Path $tauriApi "package.json") -Raw -Encoding UTF8 | ConvertFrom-Json
    $tauriLicense = Get-Content -LiteralPath (Join-Path $tauriApi "LICENSE-MIT") -Raw -Encoding UTF8
    [IO.File]::AppendAllText($notices, "`n$($tauriManifest.name) $($tauriManifest.version) (MIT)`n$tauriLicense`n", $utf8)

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
