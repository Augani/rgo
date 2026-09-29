$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $env:RUNNER_TEMP) { throw 'RUNNER_TEMP must be a disposable CI directory' }
$repository = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$cli = Join-Path $repository 'target/debug/rgo.exe'
$wrapper = Join-Path $repository 'target/debug/rgo-rustc-wrapper.exe'
if (-not (Test-Path $cli) -or -not (Test-Path $wrapper)) { throw 'build both binaries before this probe' }
$version = (& $cli --version).Trim()
if ($version -match '^rgo ([0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)$') {
    $tag = "v$($Matches[1])"
} else { throw "unexpected CLI version: $version" }
$top = "rgo-$tag-x86_64-pc-windows-msvc"
$sandbox = Join-Path $env:RUNNER_TEMP "rgo installer $([guid]::NewGuid().ToString('N')) ü"
$stage = Join-Path $sandbox $top
$cargoHome = Join-Path $sandbox '.cargo'
$rgoHome = Join-Path $sandbox '.rgo'
$project = Join-Path $sandbox 'plain-cargo'
$archive = Join-Path $sandbox "$top.zip"
$installScript = Join-Path $repository 'scripts/install-windows.ps1'
$oldHome = $env:HOME
$oldProfile = $env:USERPROFILE
$oldCargoHome = $env:CARGO_HOME
$oldRgoHome = $env:RGO_HOME
$oldRustupHome = $env:RUSTUP_HOME
$oldToolchain = $env:RUSTUP_TOOLCHAIN
$rustupHome = (& rustup show home).Trim()
$oldPath = $env:PATH
$realCargo = (Get-Command cargo.exe -ErrorAction Stop).Source
function Get-RawUserPath {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    if ($null -eq $key) { return $null }
    try {
        return $key.GetValue('Path', $null,
            [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    } finally { $key.Dispose() }
}
$expectedUserPath = Get-RawUserPath
function Get-RawUserPathKind {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    if ($null -eq $key) { return $null }
    try {
        if (@($key.GetValueNames()) -notcontains 'Path') { return $null }
        return $key.GetValueKind('Path').ToString()
    } finally { $key.Dispose() }
}
function Normalize-PlanPath([string]$Value) {
    $full = [IO.Path]::GetFullPath($Value).Replace('/', '\')
    if ($full.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
        return '\\' + $full.Substring(8)
    }
    if ($full.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) {
        return $full.Substring(4)
    }
    return $full
}
function Plan-Property($Object, [string]$Path) {
    $expected = Normalize-PlanPath $Path
    $entries = @($Object.PSObject.Properties | Where-Object {
        [string]::Equals((Normalize-PlanPath $_.Name), $expected, [StringComparison]::OrdinalIgnoreCase)
    })
    if ($entries.Count -ne 1) { throw "installer plan omitted or duplicated $Path" }
    return $entries[0].Value
}
$expectedUserPathKind = Get-RawUserPathKind
function Get-ActivationBytes([string]$Cargo, [string]$Root) {
    $values = @{}
    foreach ($path in @(
        (Join-Path $Cargo '.rgo-home'), (Join-Path $Cargo '.rgo-install.json'),
        (Join-Path $Cargo 'config'), (Join-Path $Cargo 'config.toml'),
        (Join-Path $Root 'state/inner-wrapper'), (Join-Path $Root 'state/owner-cargo-home'),
        (Join-Path $Root 'state/storage-mode')
    )) {
        $values[$path] = if ([IO.File]::Exists($path)) {
            [Convert]::ToBase64String([IO.File]::ReadAllBytes($path))
        } else { $null }
    }
    return $values
}
New-Item -ItemType Directory -Force -Path @($stage, $cargoHome, $rgoHome, (Join-Path $project 'src')) | Out-Null
Copy-Item -LiteralPath $cli, $wrapper -Destination $stage
foreach ($file in @('README.md', 'doc.md', 'LICENSE-MIT', 'LICENSE-APACHE')) {
    Copy-Item -LiteralPath (Join-Path $repository $file) -Destination $stage
}
Compress-Archive -Path $stage -DestinationPath $archive
$sha = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
$versionParts = [regex]::Match($tag, '^v([0-9]+)\.([0-9]+)\.([0-9]+)$')
if (-not $versionParts.Success) { throw 'the private upgrade probe needs a stable semver build' }
$upgradeVersion = "$($versionParts.Groups[1].Value).$($versionParts.Groups[2].Value).$([int]::Parse($versionParts.Groups[3].Value) + 1)"
$upgradeTag = "v$upgradeVersion"
$upgradeTop = "rgo-$upgradeTag-x86_64-pc-windows-msvc"
$upgradeSource = Join-Path $sandbox 'upgrade-source'
$upgradeTarget = Join-Path $sandbox 'upgrade-target'
$upgradeStage = Join-Path $sandbox $upgradeTop
$upgradeArchive = Join-Path $sandbox "$upgradeTop.zip"
New-Item -ItemType Directory -Path @($upgradeSource, $upgradeStage) | Out-Null
Copy-Item -LiteralPath (Join-Path $repository 'Cargo.toml'), (Join-Path $repository 'Cargo.lock'),
    (Join-Path $repository 'crates') -Destination $upgradeSource -Recurse
$upgradeManifest = Join-Path $upgradeSource 'Cargo.toml'
$manifestText = [IO.File]::ReadAllText($upgradeManifest)
$originalVersion = $tag.Substring(1)
[IO.File]::WriteAllText($upgradeManifest, $manifestText.Replace($originalVersion, $upgradeVersion))
$savedTargetDir = $env:CARGO_TARGET_DIR
try {
    $env:CARGO_TARGET_DIR = $upgradeTarget
    & cargo build --offline --manifest-path $upgradeManifest -p rgo-storage -p rgo-rustc-wrapper | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'could not build a second version for the private upgrade probe' }
} finally {
    if ($null -eq $savedTargetDir) { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
    else { $env:CARGO_TARGET_DIR = $savedTargetDir }
}
Copy-Item -LiteralPath (Join-Path $upgradeTarget 'debug/rgo.exe'),
    (Join-Path $upgradeTarget 'debug/rgo-rustc-wrapper.exe') -Destination $upgradeStage
foreach ($file in @('README.md', 'doc.md', 'LICENSE-MIT', 'LICENSE-APACHE')) {
    Copy-Item -LiteralPath (Join-Path $repository $file) -Destination $upgradeStage
}
Compress-Archive -Path $upgradeStage -DestinationPath $upgradeArchive
$upgradeSha = (Get-FileHash -LiteralPath $upgradeArchive -Algorithm SHA256).Hash
$env:HOME = $sandbox
$env:USERPROFILE = $sandbox
$env:CARGO_HOME = $cargoHome
$env:RGO_HOME = $rgoHome
$env:RUSTUP_HOME = $rustupHome
$env:RUSTUP_TOOLCHAIN = 'stable'
$installArgs = @{ ReleaseTag = $tag; Archive = $archive; Sha256 = $sha
    DevelopmentBundle = $true; CargoHome = $cargoHome; RgoHome = $rgoHome; NoUserPath = $true }
$installed = $false
$heldCargo = $null
$heldRelease = $null
try {
    $plan = (& $cli setup --installer-plan-json --no-service | Out-String).Trim() | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or $plan.schema_version -ne 1) { throw 'Windows setup did not provide an installer activation plan' }
    if (-not $plan.files.PSObject.Properties[(Join-Path $cargoHome '.rgo-install.json')]) {
        throw 'Windows installer plan omitted its activation record'
    }
    & $installScript @installArgs -VerifyOnly
    & $installScript @installArgs -NoService
    $installed = $true
    $manifest = Join-Path $project 'Cargo.toml'
    [IO.File]::WriteAllText($manifest, "[package]`nname = 'rgo_windows_installer_probe'`nversion = '0.1.0'`nedition = '2021'`n")
    [IO.File]::WriteAllText((Join-Path $project 'src/main.rs'), 'fn main() { println!("rgo"); }')
    Remove-Item Env:RGO_HOME
    & cargo build --offline --manifest-path $manifest | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'plain Cargo build failed' }
    if (-not (Test-Path (Join-Path $project 'target/debug/rgo_windows_installer_probe.exe'))) { throw 'plain Cargo final binary is missing' }
    if (-not (Test-Path (Join-Path $rgoHome 'state/owner-cargo-home'))) { throw 'managed root was not activated' }
    $entry = Join-Path $cargoHome 'bin/rgo-rustc-wrapper.exe'
    Remove-Item -LiteralPath $entry
    & $installScript @installArgs -NoService -Repair
    if (-not (Test-Path $entry)) { throw 'repair did not restore the owned wrapper' }
    $statePath = Join-Path $cargoHome 'rgo/installer-windows.json'
    $oldStateText = [IO.File]::ReadAllText($statePath)
    $oldState = $oldStateText | ConvertFrom-Json
    $beforeFiles = Get-ActivationBytes $cargoHome $rgoHome
    $upgradeArgs = $installArgs.Clone()
    $upgradeArgs['ReleaseTag'] = $upgradeTag
    $upgradeArgs['Archive'] = $upgradeArchive
    $upgradeArgs['Sha256'] = $upgradeSha
    & $installScript @upgradeArgs -NoService
    $activeCli = Join-Path $cargoHome 'bin/rgo.exe'
    if ((& $activeCli --version).Trim() -ne "rgo $upgradeVersion") { throw 'upgraded command entrypoint has the wrong version' }
    if (-not (Test-Path (Join-Path $cargoHome "rgo/versions/$top/rgo.exe"))) {
        throw 'upgrade removed the previous release pair'
    }
    Remove-Item Env:RGO_HOME -ErrorAction SilentlyContinue
    & cargo build --offline --manifest-path $manifest | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'plain Cargo build failed after the version upgrade' }
    $newState = [IO.File]::ReadAllText($statePath) | ConvertFrom-Json
    $afterFiles = Get-ActivationBytes $cargoHome $rgoHome
    $changed = @()
    foreach ($path in $beforeFiles.Keys) {
        if (-not [string]::Equals($beforeFiles[$path], $afterFiles[$path], [StringComparison]::Ordinal)) {
            $changed += [ordered]@{ path = $path; before = $beforeFiles[$path]; after = $afterFiles[$path] }
        }
    }
    if (-not @($changed | Where-Object { $_.path -eq (Join-Path $cargoHome '.rgo-install.json') }).Count) {
        throw 'upgrade probe did not change the Cargo activation record'
    }
    $journalPath = Join-Path $cargoHome 'rgo/installer-windows-upgrade.json'
    $journal = [ordered]@{ schemaVersion = 1; oldState = $oldState; newState = $newState; files = $changed }
    [IO.File]::WriteAllText($journalPath, ($journal | ConvertTo-Json -Depth 8))
    Copy-Item -LiteralPath (Join-Path $cargoHome "rgo/versions/$top/rgo-rustc-wrapper.exe") -Destination $entry -Force
    [IO.File]::WriteAllText($statePath, $oldStateText)
    & $installScript @installArgs -NoService
    if (Test-Path $journalPath) { throw 'upgrade rollback left its journal behind' }
    if ((& $activeCli --version).Trim() -ne "rgo $originalVersion") { throw 'rollback did not restore the old command entrypoint' }
    Remove-Item Env:RGO_HOME -ErrorAction SilentlyContinue
    & cargo build --offline --manifest-path $manifest | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'plain Cargo build failed after the upgrade rollback' }
    & $installScript -Uninstall -CargoHome $cargoHome -RgoHome $rgoHome
    $installed = $false
    if (Test-Path (Join-Path $cargoHome '.rgo-install.json')) { throw 'uninstall left Cargo activation behind' }
    if (Test-Path (Join-Path $cargoHome 'bin/rgo.exe')) { throw 'uninstall left an owned CLI entrypoint behind' }
    # Compare the registry value without expanding %USERPROFILE% in either
    # environment; the installer was invoked with -NoUserPath throughout.
    if ([string](Get-RawUserPath) -cne [string]$expectedUserPath) {
        throw 'uninstall did not restore user PATH'
    }
    & cargo clean --manifest-path $manifest | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'plain Cargo clean failed after uninstall' }
    & cargo build --offline --manifest-path $manifest | Out-Null
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path (Join-Path $project 'target/debug/rgo_windows_installer_probe.exe'))) {
        throw 'plain Cargo build failed after uninstall'
    }
    $storageOnlyArgs = $installArgs.Clone()
    $storageOnlyArgs['NoWrapper'] = $true
    & $installScript @storageOnlyArgs -NoService
    $installed = $true
    $record = Get-Content -LiteralPath (Join-Path $cargoHome '.rgo-install.json') -Raw | ConvertFrom-Json
    if ($null -ne $record.wrapper_binary) { throw 'storage-only setup unexpectedly installed a compiler wrapper' }
    & $installScript -Uninstall -CargoHome $cargoHome -RgoHome $rgoHome
    $installed = $false
    if (Test-Path (Join-Path $cargoHome '.rgo-install.json')) { throw 'storage-only uninstall left Cargo activation behind' }

    $supervisedProject = Join-Path $sandbox 'supervised-cargo'
    New-Item -ItemType Directory -Force -Path (Join-Path $supervisedProject 'src') | Out-Null
    $supervisedManifest = Join-Path $supervisedProject 'Cargo.toml'
    [IO.File]::WriteAllText($supervisedManifest, "[package]`nname = 'rgo_windows_supervised_installer_probe'`nversion = '0.1.0'`nedition = '2021'`n")
    [IO.File]::WriteAllText((Join-Path $supervisedProject 'src/main.rs'), @'
fn main() {
    if let Some(ready) = std::env::var_os("RGO_INSTALLER_READY") {
        std::fs::write(ready, b"ready").unwrap();
        let release = std::env::var_os("RGO_INSTALLER_RELEASE").unwrap();
        while !std::path::Path::new(&release).exists() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
'@)
    $supervisedArgs = $installArgs.Clone()
    # Native build data has a persistent mode marker. Use a fresh storage root
    # so this probe never mixes native and supervised GC domains.
    $rgoHome = Join-Path $sandbox '.rgo-supervised'
    New-Item -ItemType Directory -Path $rgoHome | Out-Null
    $supervisedArgs['RgoHome'] = $rgoHome
    $supervisedArgs['Supervised'] = $true
    $supervisedArgs['RealCargo'] = $realCargo
    & $installScript @supervisedArgs -NoService
    $installed = $true
    $shim = Join-Path $cargoHome "rgo/shims/$tag/cargo.exe"
    if (-not (Test-Path $shim)) { throw 'supervised installer did not install the owned Cargo shim' }
    if (-not [string]::Equals((Get-Command cargo.exe).Source, $shim, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'unchanged cargo does not resolve to the owned shim'
    }
    $record = Get-Content -LiteralPath (Join-Path $cargoHome '.rgo-install.json') -Raw | ConvertFrom-Json
    if ($record.schema_version -ne 3 -or $record.supervised_cargo.real_cargo -ne $realCargo) {
        throw 'supervised installation record does not own the selected real Cargo proxy'
    }
    $recordPath = Join-Path $cargoHome '.rgo-install.json'
    $recordBeforePlan = [IO.File]::ReadAllBytes($recordPath)
    $upgradeCli = Join-Path $upgradeStage 'rgo.exe'
    $supervisedPlan = (& $upgradeCli setup --installer-plan-json --supervised --real-cargo $realCargo --no-service | Out-String).Trim() | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or $supervisedPlan.schema_version -ne 1) {
        throw 'staged cross-version CLI did not provide a supervised installer plan'
    }
    $newShim = Join-Path $cargoHome "rgo/shims/$upgradeTag/cargo.exe"
    $plannedBinary = Plan-Property $supervisedPlan.binaries $newShim
    $plannedRecord = (Plan-Property $supervisedPlan.files $recordPath).contents | ConvertFrom-Json
    $plannedFallback = Join-Path (Split-Path -Path $newShim -Parent) '.rgo-cargo-fallback.json'
    if ($plannedBinary -notmatch '^binary-blake3:[0-9a-f]{64}$' -or
        $plannedRecord.supervised_cargo.shim_contents -ne $plannedBinary -or
        $plannedRecord.binary_version -ne $upgradeVersion -or
        -not [string]::Equals((Normalize-PlanPath $plannedRecord.supervised_cargo.shim_path),
            (Normalize-PlanPath $newShim), [StringComparison]::OrdinalIgnoreCase) -or
        -not (Plan-Property $supervisedPlan.files $plannedFallback).contents -or
        (Test-Path -LiteralPath $newShim) -or
        ([Convert]::ToBase64String($recordBeforePlan) -cne
            [Convert]::ToBase64String([IO.File]::ReadAllBytes($recordPath)))) {
        throw 'supervised cross-version plan changed activation or omitted its verified new shim'
    }
    Remove-Item Env:RGO_HOME -ErrorAction SilentlyContinue
    Push-Location $supervisedProject
    try {
        & cmd.exe /C 'cargo build --offline' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'unchanged Cargo build failed through the installed shim' }
    } finally { Pop-Location }
    if (-not (Test-Path (Join-Path $supervisedProject 'target/debug/rgo_windows_supervised_installer_probe.exe'))) {
        throw 'supervised Cargo did not leave the final binary in the project'
    }
    $attributed = @(Get-ChildItem -LiteralPath (Join-Path $rgoHome 'builds') -Recurse -Filter '.rgo-context.json' -File |
        Where-Object {
            (Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json).workspace_root.EndsWith(
                '\supervised-cargo', [StringComparison]::OrdinalIgnoreCase)
        })
    if ($attributed.Count -ne 1) { throw 'unchanged Cargo build did not create one attributed managed context' }
    Remove-Item -LiteralPath $shim
    & $installScript @supervisedArgs -NoService -Repair
    if (-not (Test-Path $shim)) { throw 'supervised repair did not restore the owned Cargo shim' }
    $supervisedUpgradeArgs = $supervisedArgs.Clone()
    $supervisedUpgradeArgs['ReleaseTag'] = $upgradeTag
    $supervisedUpgradeArgs['Archive'] = $upgradeArchive
    $supervisedUpgradeArgs['Sha256'] = $upgradeSha
    $oldSupervisedState = [IO.File]::ReadAllText((Join-Path $cargoHome 'rgo/installer-windows.json'))
    $oldSupervisedRecord = [IO.File]::ReadAllText($recordPath)
    if (Test-Path -LiteralPath $plannedFallback) {
        throw 'read-only supervised plan or shim repair created the next-version fallback'
    }
    $heldReady = Join-Path $sandbox 'old-cargo-running'
    $heldRelease = Join-Path $sandbox 'release-old-cargo'
    $env:RGO_INSTALLER_READY = $heldReady
    $env:RGO_INSTALLER_RELEASE = $heldRelease
    try {
        $heldCargo = Start-Process -FilePath $shim -ArgumentList @('run', '--offline') `
            -WorkingDirectory $supervisedProject -PassThru `
            -RedirectStandardOutput (Join-Path $sandbox 'old-cargo.out') `
            -RedirectStandardError (Join-Path $sandbox 'old-cargo.err')
    } finally {
        Remove-Item Env:RGO_INSTALLER_READY -ErrorAction SilentlyContinue
        Remove-Item Env:RGO_INSTALLER_RELEASE -ErrorAction SilentlyContinue
    }
    $readyDeadline = [DateTime]::UtcNow.AddSeconds(15)
    while (-not (Test-Path -LiteralPath $heldReady)) {
        $heldCargo.Refresh()
        if ($heldCargo.HasExited -or [DateTime]::UtcNow -gt $readyDeadline) {
            throw 'old cargo run did not reach its live child before supervised upgrade'
        }
        Start-Sleep -Milliseconds 100
    }
    try {
        $env:RGO_SETUP_TEST_EXIT_AFTER_RECORD = '1'
        try {
            & $installScript @supervisedUpgradeArgs -NoService
            throw 'forced supervised setup interruption unexpectedly succeeded'
        } catch {
            if ($_.Exception.Message -notmatch 'exit 88' -or
                $_.Exception.Message -match 'rollback also failed') { throw }
        }
    } finally { Remove-Item Env:RGO_SETUP_TEST_EXIT_AFTER_RECORD -ErrorAction SilentlyContinue }
    $rollbackJournal = Test-Path (Join-Path $cargoHome 'rgo/installer-windows-upgrade.json')
    $rollbackState = [IO.File]::ReadAllText((Join-Path $cargoHome 'rgo/installer-windows.json')) -ceq $oldSupervisedState
    $rollbackRecord = [IO.File]::ReadAllText($recordPath) -ceq $oldSupervisedRecord
    $rollbackFallback = Test-Path -LiteralPath $plannedFallback
    $rollbackShim = Test-Path -LiteralPath $newShim
    $rollbackCargo = [string]::Equals((Get-Command cargo.exe).Source, $shim,
        [StringComparison]::OrdinalIgnoreCase)
    if ($rollbackJournal -or -not $rollbackState -or -not $rollbackRecord -or
        $rollbackFallback -or $rollbackShim -or -not $rollbackCargo) {
        throw "interrupted supervised upgrade rollback mismatch: journal=$rollbackJournal state=$rollbackState record=$rollbackRecord fallback=$rollbackFallback shim=$rollbackShim cargo=$rollbackCargo"
    }
    $heldCargo.Refresh()
    if ($heldCargo.HasExited) { throw 'supervised rollback terminated the running old Cargo session' }
    & $installScript @supervisedUpgradeArgs -NoService
    $heldCargo.Refresh()
    if ($heldCargo.HasExited) { throw 'supervised upgrade terminated the running old Cargo session' }
    [IO.File]::WriteAllText($heldRelease, 'release')
    if (-not $heldCargo.WaitForExit(15000) -or $heldCargo.ExitCode -ne 0) {
        throw 'old Cargo session did not exit normally after supervised upgrade'
    }
    $heldCargo = $null
    $upgradedRecord = Get-Content -LiteralPath $recordPath -Raw | ConvertFrom-Json
    $upgradedShim = Join-Path $cargoHome "rgo/shims/$upgradeTag/cargo.exe"
    if ($upgradedRecord.binary_version -ne $upgradeVersion -or
        -not [string]::Equals((Normalize-PlanPath $upgradedRecord.supervised_cargo.shim_path),
            (Normalize-PlanPath $upgradedShim), [StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-Path -LiteralPath $upgradedShim) -or
        (Get-FileHash -LiteralPath $upgradedShim -Algorithm SHA256).Hash -ne
            (Get-FileHash -LiteralPath $upgradeCli -Algorithm SHA256).Hash -or
        -not (Test-Path -LiteralPath $shim)) {
        throw 'supervised upgrade did not activate the verified new shim and retain the old one'
    }
    if (-not [string]::Equals((Get-Command cargo.exe).Source, $upgradedShim,
        [StringComparison]::OrdinalIgnoreCase)) {
        throw 'unchanged cargo does not resolve to the upgraded shim'
    }
    Push-Location $supervisedProject
    try {
        & cmd.exe /C 'cargo build --offline' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'unchanged Cargo build failed after supervised upgrade' }
    } finally { Pop-Location }
    $staleProject = Join-Path $sandbox 'stale-supervised-cargo'
    New-Item -ItemType Directory -Path (Join-Path $staleProject 'src') | Out-Null
    [IO.File]::WriteAllText((Join-Path $staleProject 'Cargo.toml'),
        "[package]`nname = 'rgo_windows_stale_shim_probe'`nversion = '0.1.0'`nedition = '2021'`n")
    [IO.File]::WriteAllText((Join-Path $staleProject 'src/main.rs'), 'fn main() {}')
    Push-Location $staleProject
    try {
        & $shim build --offline | Out-Null
        if ($LASTEXITCODE -ne 0 -or
            -not (Test-Path (Join-Path $staleProject 'target/debug/rgo_windows_stale_shim_probe.exe'))) {
            throw 'old-shell Cargo did not fall back to local storage after supervised upgrade'
        }
    } finally { Pop-Location }
    $heldReady = Join-Path $sandbox 'new-cargo-running'
    $heldRelease = Join-Path $sandbox 'release-new-cargo'
    $env:RGO_INSTALLER_READY = $heldReady
    $env:RGO_INSTALLER_RELEASE = $heldRelease
    try {
        $heldCargo = Start-Process -FilePath $upgradedShim -ArgumentList @('run', '--offline') `
            -WorkingDirectory $supervisedProject -PassThru `
            -RedirectStandardOutput (Join-Path $sandbox 'new-cargo.out') `
            -RedirectStandardError (Join-Path $sandbox 'new-cargo.err')
    } finally {
        Remove-Item Env:RGO_INSTALLER_READY -ErrorAction SilentlyContinue
        Remove-Item Env:RGO_INSTALLER_RELEASE -ErrorAction SilentlyContinue
    }
    $readyDeadline = [DateTime]::UtcNow.AddSeconds(15)
    while (-not (Test-Path -LiteralPath $heldReady)) {
        $heldCargo.Refresh()
        if ($heldCargo.HasExited -or [DateTime]::UtcNow -gt $readyDeadline) {
            throw 'new cargo run did not reach its live child before supervised uninstall'
        }
        Start-Sleep -Milliseconds 100
    }
    & $installScript -Uninstall -CargoHome $cargoHome -RgoHome $rgoHome
    $installed = $false
    $heldCargo.Refresh()
    if ($heldCargo.HasExited) { throw 'supervised uninstall terminated the running new Cargo session' }
    [IO.File]::WriteAllText($heldRelease, 'release')
    if (-not $heldCargo.WaitForExit(15000) -or $heldCargo.ExitCode -ne 0) {
        throw 'new Cargo session did not exit normally after supervised uninstall'
    }
    $heldCargo = $null
    if (-not (Test-Path $shim) -or -not (Test-Path $upgradedShim) -or
        -not (Test-Path (Join-Path (Split-Path $shim -Parent) '.rgo-cargo-fallback.json')) -or
        -not (Test-Path (Join-Path (Split-Path $upgradedShim -Parent) '.rgo-cargo-fallback.json'))) {
        throw 'supervised uninstall did not retain its old-shell Cargo fallback'
    }
    if ([string](Get-RawUserPath) -cne [string]$expectedUserPath) {
        throw 'supervised uninstall changed user PATH despite -NoUserPath'
    }
    Push-Location $supervisedProject
    try {
        & cmd.exe /C 'cargo build --offline' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'ordinary Cargo build failed after supervised uninstall' }
    } finally { Pop-Location }
    if (-not (Test-Path (Join-Path $supervisedProject 'target/debug/deps'))) {
        throw 'ordinary Cargo did not build locally after supervised uninstall'
    }
    $userPathArgs = $supervisedArgs.Clone()
    $userPathArgs.Remove('NoUserPath') | Out-Null
    & $installScript @userPathArgs -NoService
    $installed = $true
    $ownedUserPath = Get-RawUserPath
    $shimDirectory = Split-Path -Path $shim -Parent
    if (-not $ownedUserPath.StartsWith("$shimDirectory;", [StringComparison]::OrdinalIgnoreCase)) {
        throw 'supervised installer did not prepend the shim to raw User PATH'
    }
    $ownedKind = if ($null -eq $expectedUserPathKind) { 'String' } else { $expectedUserPathKind }
    if ((Get-RawUserPathKind) -ne $ownedKind) { throw 'supervised installer changed the User PATH registry type' }
    $installedProcessPath = $env:PATH
    try {
        $env:PATH = ([Environment]::GetEnvironmentVariable('Path', 'Machine'),
            [Environment]::GetEnvironmentVariable('Path', 'User')) -join ';'
        if (-not [string]::Equals((Get-Command cargo.exe -ErrorAction Stop).Source, $shim,
            [StringComparison]::OrdinalIgnoreCase)) {
            throw 'a newly composed Windows PATH would not resolve cargo.exe to the owned shim'
        }
    } finally { $env:PATH = $installedProcessPath }
    $userPathUpgradeArgs = $userPathArgs.Clone()
    $userPathUpgradeArgs['ReleaseTag'] = $upgradeTag
    $userPathUpgradeArgs['Archive'] = $upgradeArchive
    $userPathUpgradeArgs['Sha256'] = $upgradeSha
    & $installScript @userPathUpgradeArgs -NoService
    $upgradedUserPath = Get-RawUserPath
    $upgradedShimDirectory = Split-Path -Path $upgradedShim -Parent
    if (-not $upgradedUserPath.StartsWith("$upgradedShimDirectory;", [StringComparison]::OrdinalIgnoreCase) -or
        (Get-RawUserPathKind) -ne $ownedKind) {
        throw 'supervised upgrade did not switch raw User PATH to the new shim while preserving its type'
    }
    $installedProcessPath = $env:PATH
    try {
        $env:PATH = ([Environment]::GetEnvironmentVariable('Path', 'Machine'),
            [Environment]::GetEnvironmentVariable('Path', 'User')) -join ';'
        if (-not [string]::Equals((Get-Command cargo.exe -ErrorAction Stop).Source, $upgradedShim,
            [StringComparison]::OrdinalIgnoreCase)) {
            throw 'a newly composed Windows PATH would not resolve cargo.exe to the upgraded shim'
        }
    } finally { $env:PATH = $installedProcessPath }
    & $installScript -Uninstall -CargoHome $cargoHome -RgoHome $rgoHome
    $installed = $false
    if ([string](Get-RawUserPath) -cne [string]$expectedUserPath -or
        (Get-RawUserPathKind) -ne $expectedUserPathKind) {
        throw 'supervised uninstall did not restore raw User PATH and its registry type'
    }
    Write-Host 'Windows installer: native upgrade/rollback, storage-only uninstall, supervised shim repair/undo, and exact User PATH restoration passed'
} finally {
    if ($heldCargo -and -not $heldCargo.HasExited) {
        if ($heldRelease) { [IO.File]::WriteAllText($heldRelease, 'release') }
        if (-not $heldCargo.WaitForExit(10000)) { $heldCargo.Kill() }
    }
    if ($installed) {
        try { & $installScript -Uninstall -CargoHome $cargoHome -RgoHome $rgoHome | Out-Null }
        catch { Write-Warning "Best-effort sandbox uninstall failed: $_" }
    }
    $env:HOME = $oldHome
    $env:USERPROFILE = $oldProfile
    $env:CARGO_HOME = $oldCargoHome
    $env:RGO_HOME = $oldRgoHome
    $env:RUSTUP_HOME = $oldRustupHome
    $env:RUSTUP_TOOLCHAIN = $oldToolchain
    $env:PATH = $oldPath
}
