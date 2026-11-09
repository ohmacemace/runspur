[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$PackagePath)

$ErrorActionPreference = "Stop"
$archive = (Resolve-Path -LiteralPath $PackagePath).Path
$tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$temporary = Join-Path $tempRoot ("runspur-npm-test-" + [Guid]::NewGuid().ToString("N"))
$prefix = Join-Path $temporary ("installed " + [char]0x6D4B + [char]0x8BD5)
$state = Join-Path $temporary "isolated state"
$project = Join-Path $temporary "project space"
$null = [IO.Directory]::CreateDirectory($project)
$control = $null
$stopped = $false

function Get-TestSchedulerProcessIds {
    param([string]$Program, [string]$StateDirectory)

    Get-CimInstance -ClassName Win32_Process -Filter "Name='schedulerctl.exe'" |
        Where-Object { $_.ExecutablePath -eq $Program -and ([string]$_.CommandLine).Contains($StateDirectory) } |
        Select-Object -ExpandProperty ProcessId
}

try {
    npm.cmd install --global --offline --ignore-scripts --no-audit --no-fund --prefix $prefix $archive
    if ($LASTEXITCODE -ne 0) { throw "npm could not install the CLI tarball." }

    $package = Join-Path $prefix "node_modules\runspur"
    $manifest = Get-Content -LiteralPath (Join-Path $package "package.json") -Raw | ConvertFrom-Json
    if ($manifest.name -ne "runspur" -or $manifest.private -or ($manifest.os -join ",") -ne "win32" -or ($manifest.cpu -join ",") -ne "x64") {
        throw "Unexpected CLI package identity or target platform."
    }
    if ($manifest.scripts -or $manifest.dependencies -or $manifest.optionalDependencies) {
        throw "Installation must use the packaged binaries directly."
    }
    if (-not (Get-Content -LiteralPath (Join-Path $package "LICENSE") -Raw).Contains("Copyright (c) 2026 mace")) {
        throw "The distribution is missing its copyright notice."
    }
    if ((Get-Item -LiteralPath (Join-Path $package "THIRD_PARTY_NOTICES.txt")).Length -eq 0) {
        throw "The distribution is missing its third-party notices."
    }
    if (@($manifest.bin.PSObject.Properties).Count -ne 1 -or $manifest.bin.runspur -ne "native/schedulerctl.exe") {
        throw "The package must expose runspur as its only command."
    }
    foreach ($legacyCommand in @("schedulerd", "schedulerctl", "scheduler-mcp")) {
        if (Test-Path -LiteralPath (Join-Path $prefix "$legacyCommand.cmd")) {
            throw "The package installed an unexpected $legacyCommand command."
        }
    }
    $control = Join-Path $prefix "runspur.cmd"
    $nativeControl = Join-Path $package $manifest.bin.runspur
    $mcp = Join-Path $package "native\scheduler-mcp.exe"
    if (@(Get-ChildItem -LiteralPath (Join-Path $package "native") -Filter "*.exe" -File).Count -ne 2) {
        throw "The package must contain only the CLI and MCP executables."
    }
    foreach ($program in @($control, $mcp)) {
        $output = & $program --version
        if ($LASTEXITCODE -ne 0 -or ([string]$output).Trim() -notmatch ('\s' + [Regex]::Escape($manifest.version) + '$')) {
            throw "The installed program $program does not match package version $($manifest.version)."
        }
    }
    # Only configure the fixture as a worker. This smoke test never executes an Agent.
    $profilePath = Join-Path $temporary "worker.json"
    $profile = @{ profile_id = "claude-cli"; kind = "claude_print"; program = $nativeControl } | ConvertTo-Json
    [IO.File]::WriteAllText($profilePath, $profile, [Text.UTF8Encoding]::new($false))
    & $control --data-dir $state profile-put --file $profilePath | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "The installed CLI could not register the test worker." }
    $health = & $control --data-dir $state doctor
    if ($LASTEXITCODE -ne 0 -or ($health | ConvertFrom-Json).service.version -ne $manifest.version) {
        throw "The packaged CLI did not automatically start a healthy scheduler."
    }
    $started = @(Get-TestSchedulerProcessIds -Program $nativeControl -StateDirectory $state)
    if ($started.Count -ne 1) { throw "Expected one isolated npm scheduler process after automatic startup." }

    $configuration = @("--data-dir", $state, "configure", "--project-root", $project,
        "--name", "npm-distribution-test", "--main-agent", "codex", "--workers", "claude-cli", "--permission")
    $projectManifest = Join-Path $project ".agent-scheduler\project.json"
    $hostDirectory = Join-Path $project ".codex"
    $preview = & $control @configuration read-only
    if ($LASTEXITCODE -ne 0 -or ($preview | ConvertFrom-Json).state -ne "preview") {
        throw "The installed CLI could not preview project configuration."
    }
    if ((Test-Path -LiteralPath $projectManifest) -or (Test-Path -LiteralPath $hostDirectory)) {
        throw "The preview changed project files."
    }

    $wizardInput = "npm-distribution-test`n1`n1`n1`n"
    $canceled = ($wizardInput + "n") | & $control --data-dir $state new --project-root $project
    if ($LASTEXITCODE -ne 0 -or ($canceled -join "`n") -notmatch "Project setup canceled\.") {
        throw "The project setup wizard did not cancel."
    }
    if ((Test-Path -LiteralPath $projectManifest) -or (Test-Path -LiteralPath $hostDirectory)) {
        throw "Canceling the project setup wizard changed project files."
    }
    $saved = ($wizardInput + "y") | & $control --data-dir $state new --project-root $project
    if ($LASTEXITCODE -ne 0 -or ($saved -join "`n") -notmatch "Project connected\.") {
        throw "The installed CLI could not connect the project from the wizard."
    }
    $config = Get-Content -LiteralPath $projectManifest -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($config.main_agent_profile_id -ne "codex-desktop" -or ($config.allowed_worker_profile_ids -join ",") -ne "claude-cli") {
        throw "The project setup wizard did not save the selected Agents."
    }
    if ($config.limits.allow_workspace_write -or $config.limits.allow_project_write) {
        throw "The installed CLI did not save read-only authorization."
    }
    $mcpConfig = Get-Content -LiteralPath (Join-Path $hostDirectory "config.toml") -Raw -Encoding UTF8
    if (-not ($mcpConfig.Contains($mcp) -or $mcpConfig.Contains($mcp.Replace('\', '\\')))) {
        throw "The installed CLI did not integrate its packaged MCP bridge."
    }

    $updated = "npm-distribution-updated`n`n`n2`ny" | & $control --data-dir $state edit --project-root $project
    if ($LASTEXITCODE -ne 0 -or ($updated -join "`n") -notmatch "Project updated\.") {
        throw "The installed CLI could not update the project from the wizard."
    }
    $updatedConfig = Get-Content -LiteralPath $projectManifest -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($updatedConfig.project_id -ne $config.project_id -or $updatedConfig.name -ne "npm-distribution-updated" -or -not $updatedConfig.limits.allow_workspace_write -or $updatedConfig.limits.allow_project_write) {
        throw "The installed CLI did not preserve the project and save scoped-write authorization."
    }
    $projects = & $control --data-dir $state projects
    if ($LASTEXITCODE -ne 0) { throw "The installed CLI could not query project configuration." }
    $registered = @($projects | ConvertFrom-Json | Where-Object { $_.config.project_id -eq $config.project_id })
    if ($registered.Count -ne 1 -or -not $registered[0].config.limits.allow_workspace_write -or $registered[0].config.limits.allow_project_write) {
        throw "The installed daemon did not report the updated authorization."
    }
    $history = & $control --data-dir $state tasks --project $config.project_id --origin ([Guid]::NewGuid().ToString())
    if ($LASTEXITCODE -ne 0 -or ($history | ConvertFrom-Json).items.Count -ne 0) {
        throw "The installed CLI could not query history by origin."
    }

    $projectWide = Join-Path $temporary "project wide write"
    $null = [IO.Directory]::CreateDirectory($projectWide)
    $saved = & $control --data-dir $state configure --project-root $projectWide --main-agent codex --workers claude-cli --permission project-write --apply
    if ($LASTEXITCODE -ne 0) { throw "The installed CLI could not configure a project with project-wide writes." }
    $config = ($saved | ConvertFrom-Json).project.config
    if (-not $config.limits.allow_project_write -or $config.limits.allow_workspace_write) {
        throw "The installed CLI did not save project-wide write authorization correctly."
    }
    $running = @(Get-TestSchedulerProcessIds -Program $nativeControl -StateDirectory $state)
    if ($running.Count -ne 1 -or $running[0] -ne $started[0]) {
        throw "The two npm smoke-test projects did not share their original scheduler process."
    }
    & $control --data-dir $state stop | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "The installed CLI could not stop the test scheduler." }
    $stopped = $true
    Write-Host "npm CLI $($manifest.version) passed offline installation, automatic scheduler startup and stop, shared scheduler PID $($started[0]), interactive project setup, history, and write authorization checks."
} finally {
    if ($control -and -not $stopped -and (Test-Path -LiteralPath $state)) {
        & $control --data-dir $state stop | Out-Null
    }
    $target = (Resolve-Path -LiteralPath $temporary).Path
    if ([IO.Path]::GetDirectoryName($target) -ne $tempRoot.TrimEnd('\') -or [IO.Path]::GetFileName($target) -notmatch '^runspur-npm-test-[0-9a-f]{32}$') {
        throw "Unexpected npm test cleanup directory: $target"
    }
    Remove-Item -LiteralPath $target -Recurse -Force
}
