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
$panel = $null
$nativeDaemon = $null
$origin = $null
$token = $null
$schedulerStopped = $false

function Get-TestSchedulerProcessIds {
    param([string]$Program, [string]$StateDirectory)

    Get-CimInstance -ClassName Win32_Process -Filter "Name='schedulerd.exe'" |
        Where-Object { (([string]$_.ExecutablePath) -replace '^\\\\\?\\', '') -eq $Program -and ([string]$_.CommandLine).Contains($StateDirectory) } |
        Select-Object -ExpandProperty ProcessId
}

function Invoke-RunspurApi {
    param([string]$Command, [hashtable]$Arguments = @{})

    $headers = @{ Authorization = "Bearer $token"; Origin = $origin }
    $body = ConvertTo-Json -InputObject $Arguments -Depth 12 -Compress
    $result = Invoke-RestMethod -UseBasicParsing -TimeoutSec 35 -Method Post -Uri "$origin/api/$Command" -Headers $headers -ContentType "application/json" -Body $body
    $result
}

function Assert-RunspurApiRejected {
    param([hashtable]$Headers)

    try {
        Invoke-RestMethod -UseBasicParsing -TimeoutSec 5 -Method Post -Uri "$origin/api/projects_list" -Headers $Headers -ContentType "application/json" -Body "{}" | Out-Null
    } catch {
        if ([int]$_.Exception.Response.StatusCode -in 401, 403) { return }
        throw
    }
    throw "The browser panel allowed an unauthorized API request."
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
    $notices = Get-Content -LiteralPath (Join-Path $package "THIRD_PARTY_NOTICES.txt") -Raw -Encoding UTF8
    if (-not $notices.Contains("@tauri-apps/api")) {
        throw "The distribution is missing its third-party notices."
    }
    if (@($manifest.bin.PSObject.Properties).Count -ne 1 -or $manifest.bin.runspur -ne "native/runspur.exe") {
        throw "The package must expose runspur as its only command."
    }
    $legacyCommands = @("new", "edit", "configure", "projects", "sessions", "tasks", "task", "doctor", "profiles", "settings", "profile-put", "init", "integrate", "claude", "status", "cancel")
    foreach ($legacyShim in @("schedulerd", "schedulerctl", "scheduler-mcp")) {
        if (Test-Path -LiteralPath (Join-Path $prefix "$legacyShim.cmd")) {
            throw "The package installed an unexpected $legacyShim command."
        }
    }
    $control = Join-Path $prefix "runspur.cmd"
    $nativeControl = Join-Path $package $manifest.bin.runspur
    $nativeDaemon = Join-Path $package "native\schedulerd.exe"
    $mcp = Join-Path $package "native\scheduler-mcp.exe"
    if (@(Get-ChildItem -LiteralPath (Join-Path $package "native") -Filter "*.exe" -File).Count -ne 3) {
        throw "The package must contain the CLI, scheduler, and MCP executables."
    }
    foreach ($program in @($control, $nativeDaemon, $mcp)) {
        $output = & $program --version
        if ($LASTEXITCODE -ne 0 -or ([string]$output).Trim() -notmatch ('\s' + [Regex]::Escape($manifest.version) + '$')) {
            throw "The installed program $program does not match package version $($manifest.version)."
        }
    }
    $help = & $control --help | Out-String
    if ($LASTEXITCODE -ne 0 -or $help -notmatch '(?m)^\s+tui\s' -or $help -notmatch '(?m)^\s+start\s' -or $help -notmatch '(?m)^\s+stop\s' -or $help -match ('(?m)^\s+(?:' + ($legacyCommands -join '|') + ')\s')) {
        throw "The installed CLI help does not expose the expected start, stop, and tui commands."
    }
    $previousErrorPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = "SilentlyContinue"
        foreach ($command in $legacyCommands) {
            & $control $command 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) { throw "The installed CLI unexpectedly accepted '$command'." }
        }
    } finally {
        $ErrorActionPreference = $previousErrorPreference
    }

    $web = Join-Path $package "web"
    if (-not (Test-Path -LiteralPath (Join-Path $web "index.html") -PathType Leaf)) {
        throw "The installed CLI is missing the browser panel."
    }

    # Discovery only. The fixture is selected as a child Agent and never executed.
    $profileHome = Join-Path $temporary "profile"
    $profileBin = Join-Path $profileHome ".local\bin"
    $null = [IO.Directory]::CreateDirectory($profileBin)
    [IO.File]::Copy($nativeControl, (Join-Path $profileBin "claude.exe"))
    $outputLog = Join-Path $temporary "panel.stdout.log"
    $errorLog = Join-Path $temporary "panel.stderr.log"
    $oldProfile = $env:USERPROFILE
    try {
        $env:USERPROFILE = $profileHome
        $panelArgs = @("tui", "--no-open", "--data-dir", ('"' + $state + '"'))
        $panel = Start-Process -FilePath $nativeControl -ArgumentList $panelArgs -PassThru -WindowStyle Hidden -RedirectStandardOutput $outputLog -RedirectStandardError $errorLog
    } finally {
        $env:USERPROFILE = $oldProfile
    }

    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        if ($panel.HasExited) { throw "The installed browser panel exited. $(Get-Content -LiteralPath $errorLog -Raw)" }
        $panelOutput = Get-Content -LiteralPath $outputLog -Raw -ErrorAction SilentlyContinue
        if ($panelOutput -match 'Runspur UI: (?<origin>http://127\.0\.0\.1:\d+)/#token=(?<token>[0-9a-f]{64})') {
            $origin = $Matches.origin
            $token = $Matches.token
            break
        }
        Start-Sleep -Milliseconds 100
    } until ([DateTime]::UtcNow -ge $deadline)
    if (-not $origin) { throw "The installed browser panel did not announce its local URL." }

    $page = Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Uri "$origin/"
    $module = [regex]::Match($page.Content, '<script\b[^>]*\bsrc="(?<path>/assets/[^"]+\.js)"')
    if (-not $module.Success) { throw "The browser panel did not serve the bundled Vite page." }
    $scriptAsset = Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Uri ($origin + $module.Groups["path"].Value)
    if (-not $scriptAsset.Content) { throw "The browser panel did not serve its JavaScript asset." }
    Assert-RunspurApiRejected @{ Origin = $origin }
    Assert-RunspurApiRejected @{ Origin = $origin; Authorization = "Bearer invalid-token" }
    Assert-RunspurApiRejected @{ Origin = "http://localhost:1"; Authorization = "Bearer $token" }

    $service = Invoke-RunspurApi "start_service"
    if ($service.status -ne "ready" -or $service.health.version -ne $manifest.version) {
        throw "The browser panel did not start a healthy scheduler."
    }
    $started = @(Get-TestSchedulerProcessIds -Program $nativeDaemon -StateDirectory $state)
    if ($started.Count -ne 1) { throw "Expected one isolated scheduler process after browser panel startup." }
    & $control start --data-dir $state | Out-Null
    $reused = @(Get-TestSchedulerProcessIds -Program $nativeDaemon -StateDirectory $state)
    if ($LASTEXITCODE -ne 0 -or $reused.Count -ne 1 -or $reused[0] -ne $started[0]) {
        throw "The headless start command did not reuse the browser panel's scheduler."
    }
    if (@(Invoke-RunspurApi "projects_list").Count -ne 0) { throw "The isolated browser panel did not start with an empty project list." }
    $profiles = @(Invoke-RunspurApi "agent_profiles")
    if (-not ($profiles | Where-Object { $_.profile_id -eq "claude-cli" -and $_.worker })) {
        throw "The browser panel did not discover the isolated child Agent fixture."
    }

    $projectManifest = Join-Path $project ".agent-scheduler\project.json"
    $hostDirectory = Join-Path $project ".codex"
    $draft = Invoke-RunspurApi "project_read" @{ root = $project }
    $draft.name = "npm-distribution-test"
    $draft.main_agent_profile_id = "codex-desktop"
    $draft.worker_profile_ids = @("claude-cli")
    $preview = Invoke-RunspurApi "project_preview" @{ input = $draft }
    if (-not $preview.token -or -not $preview.skill_path -or -not $preview.mcp_path) {
        throw "The browser panel did not return a complete project preview."
    }
    if ((Test-Path -LiteralPath $projectManifest) -or (Test-Path -LiteralPath $hostDirectory)) {
        throw "The browser panel preview changed project files."
    }
    $saved = Invoke-RunspurApi "project_apply" @{ token = $preview.token }
    $config = Get-Content -LiteralPath $projectManifest -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($saved.config.project_id -ne $config.project_id -or $config.main_agent_profile_id -ne "codex-desktop" -or ($config.allowed_worker_profile_ids -join ",") -ne "claude-cli") {
        throw "The browser panel did not save the selected project and Agents."
    }
    if ($config.limits.allow_workspace_write -or $config.limits.allow_project_write) {
        throw "The browser panel did not save read-only authorization."
    }
    $mcpConfig = Get-Content -LiteralPath (Join-Path $hostDirectory "config.toml") -Raw -Encoding UTF8
    if (-not ($mcpConfig.Contains($mcp) -or $mcpConfig.Contains($mcp.Replace('\', '\\')))) {
        throw "The browser panel did not integrate its packaged MCP bridge."
    }

    $draft = Invoke-RunspurApi "project_read" @{ root = $project }
    $draft.name = "npm-distribution-updated"
    $draft.allow_workspace_write = $true
    $preview = Invoke-RunspurApi "project_preview" @{ input = $draft }
    $saved = Invoke-RunspurApi "project_apply" @{ token = $preview.token }
    if ($saved.config.project_id -ne $config.project_id -or $saved.config.name -ne "npm-distribution-updated" -or -not $saved.config.limits.allow_workspace_write -or $saved.config.limits.allow_project_write) {
        throw "The browser panel did not preserve the project and save scoped-write authorization."
    }
    $projects = @(Invoke-RunspurApi "projects_list")
    $registered = @($projects | Where-Object { $_.config.project_id -eq $config.project_id })
    if ($registered.Count -ne 1 -or -not $registered[0].config.limits.allow_workspace_write -or $registered[0].config.limits.allow_project_write) {
        throw "The browser panel did not report the updated authorization."
    }
    $history = Invoke-RunspurApi "query_task_history" @{ projectId = $config.project_id; originId = $null; before = $null; limit = 25 }
    if (-not $history.PSObject.Properties["items"] -or @($history.items).Count -ne 0) {
        throw "The browser panel could not query empty dispatch history for the project."
    }

    $projectWide = Join-Path $temporary "project wide write"
    $null = [IO.Directory]::CreateDirectory($projectWide)
    $draft = Invoke-RunspurApi "project_read" @{ root = $projectWide }
    $draft.worker_profile_ids = @("claude-cli")
    $draft.allow_project_write = $true
    $preview = Invoke-RunspurApi "project_preview" @{ input = $draft }
    $saved = Invoke-RunspurApi "project_apply" @{ token = $preview.token }
    if (-not $saved.config.limits.allow_project_write) {
        throw "The browser panel did not save project-wide write authorization."
    }
    $running = @(Get-TestSchedulerProcessIds -Program $nativeDaemon -StateDirectory $state)
    if ($running.Count -ne 1 -or $running[0] -ne $started[0]) {
        throw "The two browser panel projects did not share their original scheduler process."
    }
    Invoke-RunspurApi "stop_service" | Out-Null
    if (@(Get-TestSchedulerProcessIds -Program $nativeDaemon -StateDirectory $state).Count -ne 0) {
        throw "The browser panel did not stop its scheduler."
    }

    $startOutput = & $control start --data-dir $state | Out-String
    if ($LASTEXITCODE -ne 0 -or $startOutput -notmatch "Runspur scheduler is ready\." -or $startOutput -match '#token=') {
        throw "The installed CLI could not start the scheduler headlessly."
    }
    $service = Invoke-RunspurApi "service_status"
    $headless = @(Get-TestSchedulerProcessIds -Program $nativeDaemon -StateDirectory $state)
    if ($service.status -ne "ready" -or $headless.Count -ne 1) {
        throw "The headless command did not leave one healthy scheduler."
    }
    $service = Invoke-RunspurApi "start_service"
    $reused = @(Get-TestSchedulerProcessIds -Program $nativeDaemon -StateDirectory $state)
    if ($service.status -ne "ready" -or $reused.Count -ne 1 -or $reused[0] -ne $headless[0]) {
        throw "The browser panel did not reuse the headless scheduler."
    }
    & $control stop --data-dir $state | Out-Null
    if ($LASTEXITCODE -ne 0 -or @(Get-TestSchedulerProcessIds -Program $nativeDaemon -StateDirectory $state).Count -ne 0) {
        throw "The installed CLI did not stop the headless scheduler."
    }
    & $control stop --data-dir $state | Out-Null
    if ($LASTEXITCODE -ne 0 -or @(Get-TestSchedulerProcessIds -Program $nativeDaemon -StateDirectory $state).Count -ne 0) {
        throw "The repeated headless stop command did not leave the scheduler stopped."
    }
    $schedulerStopped = $true
    Write-Host "npm CLI $($manifest.version) passed offline installation, browser assets and API authorization, project preview and apply, history, and browser and headless scheduler lifecycle checks."
} finally {
    if ($panel -and -not $panel.HasExited) {
        if (-not $schedulerStopped -and $origin) {
            try { Invoke-RunspurApi "stop_service" | Out-Null } catch { Write-Warning "The isolated scheduler did not stop through the browser panel." }
        }
        $panel.Kill()
        $null = $panel.WaitForExit(5000)
    }
    if ($nativeDaemon) {
        foreach ($processId in @(Get-TestSchedulerProcessIds -Program $nativeDaemon -StateDirectory $state)) {
            $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
            if ($process -and -not $process.HasExited) {
                $process.Kill()
                $null = $process.WaitForExit(5000)
            }
        }
    }
    $target = (Resolve-Path -LiteralPath $temporary).Path
    if ([IO.Path]::GetDirectoryName($target) -ne $tempRoot.TrimEnd('\') -or [IO.Path]::GetFileName($target) -notmatch '^runspur-npm-test-[0-9a-f]{32}$') {
        throw "Unexpected npm test cleanup directory: $target"
    }
    Remove-Item -LiteralPath $target -Recurse -Force
}
