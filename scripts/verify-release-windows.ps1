param([Parameter(Mandatory = $true)][string]$BinaryDir)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $env:RUNNER_TEMP) { throw 'RUNNER_TEMP must be a disposable CI directory' }
$repository = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$binaryDir = (Resolve-Path $BinaryDir).Path
$cli = Join-Path $binaryDir 'rgo.exe'
$wrapper = Join-Path $binaryDir 'rgo-rustc-wrapper.exe'
if (-not (Test-Path $cli) -or -not (Test-Path $wrapper)) { throw 'optimized release binaries are missing' }
$version = (& $cli --version).Trim()
if ($version -notmatch '^rgo ([0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)$') {
    throw "unexpected CLI version: $version"
}
$tag = "v$($Matches[1])"
$top = "rgo-$tag-x86_64-pc-windows-msvc"
$root = Join-Path $env:RUNNER_TEMP "rgor-$([guid]::NewGuid().ToString('N'))"
$stage = Join-Path $root $top
$archive = Join-Path $root "$top.zip"
$installer = Join-Path $repository 'scripts/install-windows.ps1'
$realCargo = (Get-Command cargo.exe -ErrorAction Stop).Source
$rustupHome = (& rustup show home).Trim()
$saved = @{}
foreach ($name in @('HOME', 'USERPROFILE', 'CARGO_HOME', 'RGO_HOME', 'RUSTUP_HOME', 'RUSTUP_TOOLCHAIN', 'PATH')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

try {
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item -LiteralPath $cli, $wrapper -Destination $stage
    foreach ($file in @('README.md', 'doc.md', 'LICENSE-MIT', 'LICENSE-APACHE')) {
        Copy-Item -LiteralPath (Join-Path $repository $file) -Destination $stage
    }
    Compress-Archive -Path $stage -DestinationPath $archive
    $sha = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash

    foreach ($mode in @('native', 'supervised')) {
        $home = Join-Path $root $mode
        $cargoHome = Join-Path $home '.cargo'
        $rgoHome = Join-Path $home '.rgo'
        $project = Join-Path $home 'project'
        New-Item -ItemType Directory -Force -Path $cargoHome, (Join-Path $project 'src') | Out-Null
        $originalConfig = "[net]`noffline = true`n"
        [IO.File]::WriteAllText((Join-Path $cargoHome 'config.toml'), $originalConfig)
        $env:HOME = $home
        $env:USERPROFILE = $home
        $env:CARGO_HOME = $cargoHome
        $env:RGO_HOME = $rgoHome
        $env:RUSTUP_HOME = $rustupHome
        $env:RUSTUP_TOOLCHAIN = 'stable'
        $env:PATH = $saved['PATH']

        $arguments = @{
            ReleaseTag = $tag; Archive = $archive; Sha256 = $sha;
            DevelopmentBundle = $true; CargoHome = $cargoHome;
            RgoHome = $rgoHome; NoService = $true; NoUserPath = $true
        }
        if ($mode -eq 'supervised') {
            $arguments['Supervised'] = $true
            $arguments['RealCargo'] = $realCargo
        }
        & $installer @arguments
        $recordPath = Join-Path $cargoHome '.rgo-install.json'
        if (-not (Test-Path $recordPath)) { throw "$mode install did not activate Cargo" }
        $record = Get-Content -LiteralPath $recordPath -Raw | ConvertFrom-Json
        [IO.File]::WriteAllText((Join-Path $project 'Cargo.toml'),
            "[package]`nname = 'rgo_release_probe'`nversion = '0.1.0'`nedition = '2021'`n")
        [IO.File]::WriteAllText((Join-Path $project 'src/main.rs'), 'fn main() { println!("release"); }')
        Remove-Item Env:RGO_HOME
        if ($mode -eq 'supervised') {
            $env:PATH = "$(Split-Path -Parent $record.supervised_cargo.shim_path);$($env:PATH)"
        }
        & cargo.exe build --offline --manifest-path (Join-Path $project 'Cargo.toml') | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "$mode unchanged Cargo build failed" }
        if (-not (Test-Path (Join-Path $project 'target/debug/rgo_release_probe.exe'))) {
            throw "$mode Cargo did not preserve the requested executable"
        }
        $contexts = @(Get-ChildItem -LiteralPath (Join-Path $rgoHome 'builds') -Recurse -File -Force -Filter '.rgo-context.json')
        if ($contexts.Count -lt 1) {
            throw "$mode unchanged Cargo did not activate managed storage"
        }
        $env:PATH = $saved['PATH']
        & $installer -Uninstall -CargoHome $cargoHome -RgoHome $rgoHome
        if (Test-Path $recordPath) { throw "$mode uninstall left Cargo activation behind" }
        if ([IO.File]::ReadAllText((Join-Path $cargoHome 'config.toml')) -cne $originalConfig) {
            throw "$mode uninstall changed the prior Cargo config"
        }
        Write-Host "x86_64-pc-windows-msvc: optimized $mode install, plain Cargo build, and undo passed"
    }
} finally {
    foreach ($name in $saved.Keys) {
        [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process')
    }
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
