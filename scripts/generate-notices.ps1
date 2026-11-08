[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$OutputPath)

$ErrorActionPreference = "Stop"
$metadataText = & cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed." }
$metadata = $metadataText | ConvertFrom-Json
$nodes = @{}
foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
$reachable = [Collections.Generic.HashSet[string]]::new()
$queue = [Collections.Generic.Queue[string]]::new()
foreach ($id in $metadata.workspace_members) { $queue.Enqueue($id) }
while ($queue.Count -gt 0) {
    $id = $queue.Dequeue()
    if (-not $reachable.Add($id)) { continue }
    foreach ($dependency in $nodes[$id].deps) { $queue.Enqueue($dependency.pkg) }
}
$text = [Text.StringBuilder]::new()
$null = $text.AppendLine("Runspur third-party license notices")
$null = $text.AppendLine("Generated from Cargo.lock and the dependency packages used for Windows x64.")
$null = $text.AppendLine("The following are dependency-provided license/notice texts, not a license change.")
$count = 0
foreach ($package in @($metadata.packages | Sort-Object name, version)) {
    if ($null -eq $package.source -or -not $reachable.Contains($package.id)) { continue }
    $directory = Split-Path $package.manifest_path -Parent
    $candidates = [Collections.Generic.List[string]]::new()
    if ($package.license_file) {
        $file = $package.license_file
        if (-not [IO.Path]::IsPathRooted($file)) { $file = Join-Path $directory $file }
        if (Test-Path -LiteralPath $file -PathType Leaf) { $candidates.Add($file) }
    }
    foreach ($item in Get-ChildItem -LiteralPath $directory -Force) {
        if ($item.Name -notmatch '^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)([-_.].*|S)?$') { continue }
        if ($item.PSIsContainer) {
            foreach ($file in Get-ChildItem -LiteralPath $item.FullName -Recurse -File) { $candidates.Add($file.FullName) }
        } else { $candidates.Add($item.FullName) }
    }
    $files = @($candidates | Sort-Object -Unique)
    if ($files.Count -eq 0) { throw "No packaged license text found for $($package.name) $($package.version)." }
    $null = $text.AppendLine("`n========================================================================")
    $null = $text.AppendLine("$($package.name) $($package.version)")
    $null = $text.AppendLine("Declared license: $($package.license)")
    if ($package.repository) { $null = $text.AppendLine("Source: $($package.repository)") }
    foreach ($file in $files) {
        if ((Get-Item -LiteralPath $file).Length -gt 1MB) { throw "Unexpectedly large license file for $($package.name)." }
        $null = $text.AppendLine("`n--- $([IO.Path]::GetFileName($file)) ---")
        $null = $text.AppendLine([IO.File]::ReadAllText($file))
    }
    $count++
}
$null = $text.AppendLine("`nBundled SQLite is dedicated to the public domain: https://sqlite.org/copyright.html")
$parent = Split-Path ([IO.Path]::GetFullPath($OutputPath)) -Parent
if (-not (Test-Path -LiteralPath $parent -PathType Container)) { throw "Notice output parent does not exist." }
[IO.File]::WriteAllText([IO.Path]::GetFullPath($OutputPath), $text.ToString(), [Text.UTF8Encoding]::new($false))
Write-Host "Collected license notices for $count dependency packages."
