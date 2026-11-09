[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$InstallerPath,
    [Parameter(Mandatory = $true)][string]$ExpectedVersion
)

$ErrorActionPreference = 'Stop'

# NSIS also registers the product for the current user, even with a custom install directory.
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or
    $env:RUNNER_OS -ne 'Windows' -or $env:RUNNER_ARCH -ne 'X64' -or -not $env:RUNNER_TEMP) {
    throw 'Run the installer smoke only on a clean GitHub-hosted Windows runner.'
}

function Get-RunspurRegistration {
    foreach ($key in @(
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
    )) {
        if (Test-Path -LiteralPath $key) {
            Get-ItemProperty -Path "$key\*" |
                Where-Object { $_.DisplayName -match '^Runspur(?:\s|$)' }
        }
    }
}

function Invoke-Nsis {
    param([string]$Program, [string]$Arguments)

    $process = Start-Process -FilePath $Program -ArgumentList $Arguments -PassThru -WindowStyle Hidden
    if (-not $process.WaitForExit(120000)) {
        $process.Kill()
        $null = $process.WaitForExit(5000)
        throw "NSIS process timed out: $Program"
    }
    if ($process.ExitCode -ne 0) { throw "NSIS process failed with exit code $($process.ExitCode): $Program" }
}

function Uninstall-TestApp {
    param([string]$Program, [string]$Directory)

    Invoke-Nsis -Program $Program -Arguments '/S'
    # The temporary uninstaller removes the directory before deleting its registration.
    $clock = [Diagnostics.Stopwatch]::StartNew()
    do {
        $directoryPresent = Test-Path -LiteralPath $Directory
        $registrationPresent = @(Get-RunspurRegistration).Count -ne 0
        if (-not $directoryPresent -and -not $registrationPresent) { return }
        Start-Sleep -Milliseconds 250
    } while ($clock.Elapsed.TotalSeconds -lt 30)
    throw "Silent uninstall did not finish (directory present: $directoryPresent, registration present: $registrationPresent)."
}

function Get-InstalledScheduler {
    param([string]$Program, [string]$StateDirectory)

    $name = [IO.Path]::GetFileName($Program)
    $directory = [regex]::Escape($StateDirectory)
    $stateArgument = '(?i)(?:^|\s)--data-dir(?:=|\s+)(?:"' + $directory + '"|' + $directory + ')(?:\s|$)'
    Get-CimInstance -ClassName Win32_Process -Filter "Name='$name'" |
        Where-Object { $_.ExecutablePath -eq $Program -and $_.CommandLine -match $stateArgument }
}

function Close-TestApp {
    param([Diagnostics.Process]$Process)

    if ($Process -and -not $Process.HasExited) {
        if ((-not $Process.CloseMainWindow() -or -not $Process.WaitForExit(5000)) -and -not $Process.HasExited) {
            $Process.Kill()
            if (-not $Process.WaitForExit(5000)) { throw 'The test Desktop process did not exit.' }
        }
    }
}

function Remove-TestDirectory {
    param([string]$Path, [string]$Parent, [string]$NamePattern)

    if (-not (Test-Path -LiteralPath $Path)) { return }
    $target = (Resolve-Path -LiteralPath $Path).Path
    $directory = Get-Item -LiteralPath $target -Force
    if ($target -ne $Path -or [IO.Path]::GetDirectoryName($target) -ne $Parent -or
        [IO.Path]::GetFileName($target) -notmatch $NamePattern -or -not $directory.PSIsContainer -or
        ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
        @(Get-ChildItem -LiteralPath $target -Recurse -Force -Attributes ReparsePoint).Count -ne 0) {
        throw "Refusing to clean an unexpected test directory: $target"
    }
    Remove-Item -LiteralPath $target -Recurse -Force
}

$installer = Get-Item -LiteralPath $InstallerPath
if ($installer.PSIsContainer -or $installer.Extension -ne '.exe') {
    throw 'A built Runspur Desktop NSIS executable is required.'
}
$tempRoot = (Resolve-Path -LiteralPath $env:RUNNER_TEMP).Path.TrimEnd('\')
$localData = [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
if (-not [IO.Path]::IsPathRooted($localData)) { throw 'Cannot locate the Windows local application data directory.' }
$runspurData = Join-Path $localData 'Runspur'
if ((Test-Path -LiteralPath $runspurData) -or @(Get-RunspurRegistration).Count -ne 0 -or
    @(Get-Process -Name 'runspur-desktop', 'schedulerd', 'schedulerctl', 'runspur' -ErrorAction SilentlyContinue).Count -ne 0) {
    throw 'The runner already has Runspur data, registration, or processes.'
}

$temporary = Join-Path $tempRoot ('runspur-desktop-test-' + [Guid]::NewGuid().ToString('N'))
if (Test-Path -LiteralPath $temporary) { throw "The test directory already exists: $temporary" }
$null = [IO.Directory]::CreateDirectory($temporary)
$install = Join-Path $temporary ('installed ' + [char]0x6D4B + [char]0x8BD5)
$desktop = Join-Path $install 'runspur-desktop.exe'
$daemon = Join-Path $install 'schedulerd.exe'
$control = Join-Path $install 'schedulerctl.exe'
$uninstaller = Join-Path $install 'uninstall.exe'
$useDesktop = [Environment]::UserInteractive -and (Get-Process -Id $PID).SessionId -ne 0
$defaultState = Join-Path $runspurData 'Runspur\data'
$state = if ($useDesktop) { $defaultState } else { Join-Path $temporary 'isolated state' }
$app = $null
$uninstallAttempted = $false

try {
    # NSIS requires /D to be last and unquoted, including when the path has spaces.
    Invoke-Nsis -Program $installer.FullName -Arguments "/S /D=$install"
    foreach ($name in 'runspur-desktop.exe', 'schedulerd.exe', 'schedulerctl.exe', 'scheduler-mcp.exe', 'uninstall.exe') {
        if (-not (Test-Path -LiteralPath (Join-Path $install $name) -PathType Leaf)) { throw "Installed file missing: $name" }
    }
    $appVersion = (Get-Item -LiteralPath $desktop).VersionInfo.ProductVersion
    if ($appVersion -ne $ExpectedVersion) { throw "Desktop reported product version $appVersion; expected $ExpectedVersion." }
    $versionPattern = '\s' + [regex]::Escape($ExpectedVersion) + '$'
    foreach ($program in @($daemon, $control, (Join-Path $install 'scheduler-mcp.exe'))) {
        $version = & $program --version | Out-String
        if ($LASTEXITCODE -ne 0 -or $version.Trim() -notmatch $versionPattern) {
            throw "Installed executable did not report version ${ExpectedVersion}: $program"
        }
    }

    if ($useDesktop) {
        $app = Start-Process -FilePath $desktop -PassThru -WindowStyle Hidden
        $ownerId = $app.Id
    } else {
        Write-Host 'Windows session is noninteractive. Checking the installed scheduler directly.'
        $service = Start-Process -FilePath $daemon -ArgumentList ('--data-dir "' + $state + '"') -PassThru -WindowStyle Hidden
        $ownerId = $service.Id
    }
    $database = Join-Path $state 'scheduler.sqlite3'
    $marker = Join-Path $state '.runspur-state-version'
    $clock = [Diagnostics.Stopwatch]::StartNew()
    do {
        if ($app -and $app.HasExited) { throw "Runspur Desktop exited before starting its scheduler with code $($app.ExitCode)." }
        $owned = @(Get-InstalledScheduler -Program $daemon -StateDirectory $state)
        $ready = $owned.Count -eq 1 -and
            $(if ($useDesktop) { $owned[0].ParentProcessId -eq $ownerId } else { $owned[0].ProcessId -eq $ownerId }) -and
            (Test-Path -LiteralPath $database -PathType Leaf) -and (Test-Path -LiteralPath $marker -PathType Leaf)
        if (-not $ready) { Start-Sleep -Milliseconds 250 }
    } until ($ready -or $clock.Elapsed.TotalSeconds -ge 45)
    if (-not $ready) { throw 'The exact installed scheduler did not start with the expected state directory.' }
    $daemonId = $owned[0].ProcessId

    $healthJson = & $control --data-dir $state doctor | Out-String
    if ($LASTEXITCODE -ne 0) { throw 'The installed scheduler did not answer doctor.' }
    $health = ($healthJson | ConvertFrom-Json).service
    $owned = @(Get-InstalledScheduler -Program $daemon -StateDirectory $state)
    if ($health.version -ne $ExpectedVersion -or $health.platform -ne 'windows' -or $health.active_workers -ne 0 -or
        $owned.Count -ne 1 -or $owned[0].ProcessId -ne $daemonId -or
        @(Get-InstalledScheduler -Program $control -StateDirectory $state).Count -ne 0) {
        throw 'Doctor did not verify the original installed scheduler process and version.'
    }

    Close-TestApp -Process $app
    & $control --data-dir $state stop | Out-Null
    if ($LASTEXITCODE -ne 0 -or @(Get-InstalledScheduler -Program $daemon -StateDirectory $state).Count -ne 0) {
        throw 'The installed scheduler did not stop cleanly.'
    }
    $savedDatabase = (Get-FileHash -LiteralPath $database -Algorithm SHA256).Hash
    $uninstallAttempted = $true
    Uninstall-TestApp -Program $uninstaller -Directory $install
    if (-not (Test-Path -LiteralPath $marker -PathType Leaf) -or
        -not (Test-Path -LiteralPath $database -PathType Leaf) -or
        (Get-FileHash -LiteralPath $database -Algorithm SHA256).Hash -ne $savedDatabase) {
        throw 'Silent uninstall changed or removed the test scheduler state.'
    }
    $mode = if ($useDesktop) { 'Desktop automatic scheduler startup' } else { 'installed scheduler startup' }
    Write-Host "Runspur Desktop $ExpectedVersion passed NSIS installation, $mode, health, shutdown, and state-preserving uninstall."
} finally {
    Close-TestApp -Process $app
    foreach ($program in @($daemon, $control)) {
        foreach ($owned in @(Get-InstalledScheduler -Program $program -StateDirectory $state)) {
            $process = Get-Process -Id $owned.ProcessId -ErrorAction SilentlyContinue
            if ($process -and -not $process.HasExited -and $process.Path -eq $program) {
                $process.Kill()
                if (-not $process.WaitForExit(5000)) { throw 'An owned smoke-test scheduler did not exit.' }
            }
        }
    }
    if (-not $uninstallAttempted -and (Test-Path -LiteralPath $uninstaller -PathType Leaf)) {
        Uninstall-TestApp -Program $uninstaller -Directory $install
    }
    if ($useDesktop -and (Test-Path -LiteralPath (Join-Path $defaultState '.runspur-state-version') -PathType Leaf)) {
        Remove-TestDirectory -Path $defaultState -Parent (Join-Path $runspurData 'Runspur') -NamePattern '^data$'
    }
    Remove-TestDirectory -Path $temporary -Parent $tempRoot -NamePattern '^runspur-desktop-test-[0-9a-f]{32}$'
}
