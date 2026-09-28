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
    Write-Host 'Windows scheduled service: activated, used by plain Cargo, and undone'
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
}
