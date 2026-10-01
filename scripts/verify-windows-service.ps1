$ErrorActionPreference = 'Stop'

if (-not $env:RUNNER_TEMP) {
    throw 'RUNNER_TEMP must point to a disposable CI directory'
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)][string]$Program,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    $output = (& $Program @Arguments 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "$Program $($Arguments -join ' ') failed (exit $LASTEXITCODE): $output"
    }
    return $output
}

function Get-OwnedDaemonPid([string]$Executable, [string]$Root) {
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        $daemonProcesses = @(Get-CimInstance Win32_Process -Filter "Name = 'rgo.exe'" | Where-Object {
            $_.ExecutablePath -and
            [string]::Equals($_.ExecutablePath, $Executable, [StringComparison]::OrdinalIgnoreCase) -and
            $_.CommandLine -and $_.CommandLine.Contains("daemon --foreground --home `"$Root`"")
        })
        if ($daemonProcesses.Count -eq 1) { return [int]$daemonProcesses[0].ProcessId }
        Start-Sleep -Milliseconds 100
    }
    throw "Expected one owned daemon process for $Root"
}

$sandbox = Join-Path $env:RUNNER_TEMP "rgo service $([guid]::NewGuid().ToString('N'))"
$cargoHome = Join-Path $sandbox '.cargo'
$rgoHome = Join-Path $sandbox '.rgo'
$project = Join-Path $sandbox 'plain-cargo'
New-Item -ItemType Directory -Force -Path @($cargoHome, $rgoHome, (Join-Path $project 'src')) | Out-Null

$priorHome = $env:HOME
$priorProfile = $env:USERPROFILE
$priorCargoHome = $env:CARGO_HOME
$priorRgoHome = $env:RGO_HOME
$priorRustupHome = $env:RUSTUP_HOME
$priorToolchain = $env:RUSTUP_TOOLCHAIN
$rustupHome = (& rustup show home).Trim()
$env:HOME = $sandbox
$env:USERPROFILE = $sandbox
$env:CARGO_HOME = $cargoHome
$env:RGO_HOME = $rgoHome
$env:RUSTUP_HOME = $rustupHome
$env:RUSTUP_TOOLCHAIN = 'stable'

$cli = Join-Path (Get-Location) 'target/debug/rgo.exe'
$wrapper = Join-Path (Get-Location) 'target/debug/rgo-rustc-wrapper.exe'
if (-not (Test-Path $cli) -or -not (Test-Path $wrapper)) {
    throw 'Build both workspace binaries before the Windows service smoke'
}
$realCargo = (Get-Command cargo.exe).Source
$priorPath = $env:PATH

$setupAttempted = $false
$undone = $false
try {
    $preview = Invoke-Checked $cli @('setup', '--dry-run')
    $serviceLine = @($preview -split '\r?\n' | Where-Object { $_ -match '^would install service ([^ ]+) at ' })
    if ($serviceLine.Count -ne 1) {
        throw "Could not identify the scoped task in setup preview: $preview"
    }
    $taskName = [regex]::Match($serviceLine[0], '^would install service ([^ ]+) at ').Groups[1].Value

    $setupAttempted = $true
    try {
        $setup = Invoke-Checked $cli @('setup')
    } catch {
        $daemonLog = Join-Path $rgoHome 'logs/daemon.log'
        if (Test-Path $daemonLog) {
            Write-Warning "Daemon diagnostics from private test home:`n$(Get-Content -Raw $daemonLog)"
        }
        throw
    }
    if ($setup -notmatch 'daemon service: healthy') {
        throw "Setup did not verify a healthy scheduled daemon: $setup"
    }
    Invoke-Checked 'schtasks.exe' @('/Query', '/TN', $taskName, '/XML', '/HRESULT') | Out-Null
    $firstDaemonPid = Get-OwnedDaemonPid $cli $rgoHome

    $manifest = Join-Path $project 'Cargo.toml'
    Set-Content -Encoding utf8 $manifest "[package]`nname = 'rgo_service_probe'`nversion = '0.1.0'`nedition = '2021'`n"
    Set-Content -Encoding utf8 (Join-Path $project 'src/main.rs') 'fn main() { println!("rgo"); }'

    # A fresh Cargo process must recover the installed storage root from the
    # Cargo-home pointer; the scheduled daemon uses its explicit --home value.
    Remove-Item Env:RGO_HOME
    Invoke-Checked 'cargo' @('build', '--offline', '--manifest-path', $manifest) | Out-Null
    if (-not (Test-Path (Join-Path $project 'target/debug/rgo_service_probe.exe'))) {
        throw 'Plain Cargo did not leave the requested executable in target/debug'
    }
    if (-not (Test-Path (Join-Path $rgoHome 'state/owner-cargo-home'))) {
        throw 'Setup did not record ownership in the selected storage root'
    }

    $repeatSetup = Invoke-Checked $cli @('setup')
    if ($repeatSetup -notmatch 'daemon service: healthy') {
        throw "Repeated setup did not verify a healthy scheduled daemon: $repeatSetup"
    }
    $replacementDaemonPid = Get-OwnedDaemonPid $cli $rgoHome
    if ($replacementDaemonPid -eq $firstDaemonPid) {
        throw 'Repeated setup left the previous scheduled daemon instance running'
    }

    Invoke-Checked $cli @('setup', '--undo') | Out-Null
    $null = & schtasks.exe /Query /TN $taskName /HRESULT 2>&1
    if ($LASTEXITCODE -eq 0) {
        throw "Undo left the scheduled task $taskName registered"
    }
    $config = Get-Content -Raw (Join-Path $cargoHome 'config.toml')
    if ($config -match 'rgo managed') {
        throw 'Undo left the managed Cargo configuration fence behind'
    }
    $undone = $true

    $supervisedHome = Join-Path $env:RUNNER_TEMP "rgo supervised service $([guid]::NewGuid().ToString('N'))"
    $supervisedCargoHome = Join-Path $supervisedHome '.cargo'
    $supervisedRgoHome = Join-Path $supervisedHome '.rgo'
    $supervisedProject = Join-Path $supervisedHome 'plain-cargo'
    New-Item -ItemType Directory -Force -Path @($supervisedCargoHome, $supervisedRgoHome, (Join-Path $supervisedProject 'src')) | Out-Null
    Set-Content -Encoding utf8 (Join-Path $supervisedProject 'Cargo.toml') "[package]`nname = 'rgo_supervised_service_probe'`nversion = '0.1.0'`nedition = '2021'`n"
    Set-Content -Encoding utf8 (Join-Path $supervisedProject 'src/main.rs') 'fn main() { println!("rgo"); }'
    $env:HOME = $supervisedHome
    $env:USERPROFILE = $supervisedHome
    $env:CARGO_HOME = $supervisedCargoHome
    $env:RGO_HOME = $supervisedRgoHome
    $supervisedSetupAttempted = $false
    $supervisedUndone = $false
    try {
        $plan = Invoke-Checked $cli @('setup', '--supervised', '--real-cargo', $realCargo, '--installer-plan-json') | ConvertFrom-Json
        if (-not $plan.service.label -or -not $plan.service.contents) {
            throw 'supervised setup did not plan a scheduled daemon'
        }
        $supervisedTask = $plan.service.label
        $supervisedSetupAttempted = $true
        $supervisedSetup = Invoke-Checked $cli @('setup', '--supervised', '--real-cargo', $realCargo)
        if ($supervisedSetup -notmatch 'daemon service: healthy') {
            throw "Supervised setup did not verify a healthy scheduled daemon: $supervisedSetup"
        }
        $taskXml = [xml](Invoke-Checked 'schtasks.exe' @('/Query', '/TN', $supervisedTask, '/XML', '/HRESULT'))
        $restart = $taskXml.SelectSingleNode('//*[local-name()="RestartOnFailure"]')
        if (-not $restart -or
            $restart.SelectSingleNode('./*[local-name()="Interval"]').InnerText -ne 'PT1M' -or
            $restart.SelectSingleNode('./*[local-name()="Count"]').InnerText -ne '255') {
            throw 'Supervised task omitted its bounded crash-restart policy'
        }
        $registration = $taskXml.SelectSingleNode('//*[local-name()="RegistrationTrigger"]')
        if (-not $registration -or
            $registration.SelectSingleNode('./*[local-name()="Repetition"]/*[local-name()="Interval"]').InnerText -ne 'PT1M') {
            throw 'Supervised task omitted its recurring recovery trigger'
        }
        $supervisedDaemonPid = Get-OwnedDaemonPid $cli $supervisedRgoHome
        $record = Get-Content -LiteralPath (Join-Path $supervisedCargoHome '.rgo-install.json') -Raw -Encoding UTF8 | ConvertFrom-Json
        $shimDir = Split-Path -Parent $record.supervised_cargo.shim_path
        $env:PATH = "$shimDir;$priorPath"
        Remove-Item Env:RGO_HOME
        Invoke-Checked 'cargo' @('build', '--offline', '--manifest-path', (Join-Path $supervisedProject 'Cargo.toml')) | Out-Null
        if (-not (Test-Path (Join-Path $supervisedProject 'target/debug/rgo_supervised_service_probe.exe'))) {
            throw 'Supervised Cargo did not leave the requested executable in target/debug'
        }
        $doctor = Invoke-Checked $cli @('doctor', '--verify', '--json') | ConvertFrom-Json
        if ($doctor.activation_verified -ne $true) {
            throw 'Supervised service activation did not verify plain Cargo'
        }
        # A killed service action must recover through Task Scheduler without
        # a new setup, status, or Cargo command starting the daemon for it.
        Stop-Process -Id $supervisedDaemonPid -Force
        $restartDeadline = (Get-Date).AddSeconds(95)
        $restartedDaemonPid = $null
        while ((Get-Date) -lt $restartDeadline) {
            $daemons = @(Get-CimInstance Win32_Process -Filter "Name = 'rgo.exe'" | Where-Object {
                $_.ExecutablePath -and
                [string]::Equals($_.ExecutablePath, $cli, [StringComparison]::OrdinalIgnoreCase) -and
                $_.CommandLine -and $_.CommandLine.Contains('daemon --foreground --home') -and
                $_.CommandLine.Contains($supervisedRgoHome)
            })
            if ($daemons.Count -eq 1 -and [int]$daemons[0].ProcessId -ne $supervisedDaemonPid) {
                $restartedDaemonPid = [int]$daemons[0].ProcessId
                break
            }
            Start-Sleep -Milliseconds 500
        }
        if (-not $restartedDaemonPid) {
            $taskState = Get-ScheduledTaskInfo -TaskPath '\rgo\' -TaskName ($supervisedTask -replace '^rgo\\', '')
            throw "Task Scheduler did not restart the crashed supervised daemon; last result $($taskState.LastTaskResult), next run $($taskState.NextRunTime)"
        }
        $recoveredDoctor = Invoke-Checked $cli @('doctor', '--json') | ConvertFrom-Json
        if (-not @($recoveredDoctor.entries | Where-Object {
            $_.level -eq 'ok' -and $_.message -like 'daemon responds with protocol*'
        }).Count) {
            throw 'Restarted supervised daemon did not answer the protocol health check'
        }
        Invoke-Checked $cli @('setup', '--undo') | Out-Null
        $null = & schtasks.exe /Query /TN $supervisedTask /HRESULT 2>&1
        if ($LASTEXITCODE -eq 0) {
            throw "Supervised undo left the scheduled task $supervisedTask registered"
        }
        $supervisedUndone = $true
    }
    finally {
        if ($supervisedSetupAttempted -and -not $supervisedUndone) {
            try { Invoke-Checked $cli @('setup', '--undo') | Out-Null }
            catch { Write-Warning "Best-effort supervised sandbox undo failed: $_" }
        }
        $env:PATH = $priorPath
    }
    Write-Host 'Windows scheduled services: native and supervised Cargo activated, used, and undone'
}
finally {
    if ($setupAttempted -and -not $undone) {
        try { Invoke-Checked $cli @('setup', '--undo') | Out-Null }
        catch { Write-Warning "Best-effort sandbox undo failed: $_" }
    }
    $env:HOME = $priorHome
    $env:USERPROFILE = $priorProfile
    $env:CARGO_HOME = $priorCargoHome
    $env:RGO_HOME = $priorRgoHome
    $env:RUSTUP_HOME = $priorRustupHome
    $env:RUSTUP_TOOLCHAIN = $priorToolchain
    $env:PATH = $priorPath
}

# The expected failed `schtasks /Query` after undo leaves LASTEXITCODE=1 even
# though the probe completed successfully; PowerShell otherwise propagates it.
exit 0
