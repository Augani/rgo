#Requires -Version 5.1
<#
.SYNOPSIS
    Verify and activate a matched rgo Windows release; supervised mode adds an
    owned Cargo shim ahead of the real Cargo proxy on PATH.
.DESCRIPTION
    Release archives require a SHA-256 match and a GitHub artifact attestation.
    DevelopmentBundle is only for a local, disposable build probe. Version
    upgrades are limited to installations made with -NoService.
#>
param(
    [string]$ReleaseTag,
    [switch]$Latest,
    [string]$Repository,
    [string]$Archive,
    [string]$Sha256Sums,
    [string]$Sha256,
    [string]$CargoHome,
    [string]$RgoHome,
    [string]$InstallRoot,
    [string]$BinDir,
    [switch]$NoService,
    [switch]$NoWrapper,
    [switch]$Supervised,
    [string]$RealCargo,
    [switch]$NoUserPath,
    [switch]$VerifyOnly,
    [switch]$Repair,
    [switch]$Uninstall,
    [switch]$DevelopmentBundle
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Full-Path([string]$Value) {
    $path = [IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($Value))
    # std::fs::canonicalize may record Windows extended-length paths, while
    # installer destinations and Cargo's environment use ordinary paths.
    if ($path.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
        return '\\' + $path.Substring(8)
    }
    if ($path.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) {
        return $path.Substring(4)
    }
    return $path
}

function Test-SamePath([string]$Left, [string]$Right) {
    return [string]::Equals((Full-Path $Left), (Full-Path $Right), [StringComparison]::OrdinalIgnoreCase)
}

function Test-ChildPath([string]$Child, [string]$Parent) {
    $prefix = (Full-Path $Parent).TrimEnd('\') + '\'
    return (Full-Path $Child).StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
}

function Invoke-Checked([string]$Program, [string[]]$Arguments) {
    $result = (& $Program @Arguments 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "$Program $($Arguments -join ' ') failed (exit $LASTEXITCODE): $result"
    }
    return $result
}

function Write-JsonAtomic([string]$Path, $Value) {
    $temporary = "$Path.$([guid]::NewGuid().ToString('N')).tmp"
    try {
        [IO.File]::WriteAllText($temporary, ($Value | ConvertTo-Json -Depth 8), [System.Text.UTF8Encoding]::new($false))
        if (Test-Path -LiteralPath $Path) { throw "refusing to overwrite $Path" }
        [IO.File]::Move($temporary, $Path)
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}

function Replace-FileAtomic([string]$Source, [string]$Destination) {
    # PowerShell's overload binder turns a null backup argument into an empty
    # string, which File.Replace rejects. Reflection preserves the real null.
    $method = [IO.File].GetMethod('Replace', [type[]]@([string], [string], [string]))
    $method.Invoke($null, [object[]]@($Source, $Destination, $null)) | Out-Null
}

function Replace-JsonAtomic([string]$Path, $Value) {
    $temporary = "$Path.$([guid]::NewGuid().ToString('N')).tmp"
    try {
        [IO.File]::WriteAllText($temporary, ($Value | ConvertTo-Json -Depth 8), [System.Text.UTF8Encoding]::new($false))
        Assert-PlainFile $Path
        Replace-FileAtomic $temporary $Path
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}

function Read-Json([string]$Path) {
    return (Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json)
}

function Assert-PlainDirectory([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    Assert-Condition ($item.PSIsContainer -and -not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) "installation directory is not a plain directory: $Path"
}

function Assert-PlainFile([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    Assert-Condition (-not $item.PSIsContainer -and -not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) "installation file is not a plain file: $Path"
}

function Test-SupervisedState($State) {
    if ($State -is [System.Collections.IDictionary]) {
        return ($State.Contains('supervised') -and [bool]$State['supervised'])
    }
    return [bool]($State.PSObject.Properties['supervised'] -and $State.supervised)
}

function Select-OwnedShim($State) {
    $script:shimDir = $script:shimRoot
    $script:shimPath = Join-Path $script:shimDir 'cargo.exe'
    if (-not (Test-SupervisedState $State)) { return }
    if (-not $State.PSObject.Properties['shimPath'] -or -not $State.shimPath) { return }
    Assert-Condition ($State.versionDirectory -match '^rgo-(v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)-x86_64-pc-windows-msvc$') 'installer state has an invalid shim version'
    $expected = Join-Path (Join-Path $script:shimRoot $Matches[1]) 'cargo.exe'
    Assert-Condition (Test-SamePath $State.shimPath $expected) 'installer state names an unexpected Cargo shim'
    $script:shimPath = $expected
    $script:shimDir = Split-Path -Path $expected -Parent
}

function Download-Https([string]$Url, [string]$Destination) {
    Add-Type -AssemblyName System.Net.Http
    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $client = [System.Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(45)
    $client.DefaultRequestHeaders.UserAgent.ParseAdd('rgo-installer/0.1')
    try {
        $current = [Uri]$Url
        for ($hop = 0; $hop -lt 6; $hop++) {
            Assert-Condition ($current.Scheme -eq 'https') "release URL or redirect is not HTTPS: $current"
            $response = $client.GetAsync($current, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            try {
                if ([int]$response.StatusCode -in @(301, 302, 303, 307, 308)) {
                    Assert-Condition ($null -ne $response.Headers.Location) "release redirect has no Location: $current"
                    $current = [Uri]::new($current, $response.Headers.Location)
                    continue
                }
                $response.EnsureSuccessStatusCode() | Out-Null
                $inputStream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
                $outputStream = [IO.File]::Open($Destination, [IO.FileMode]::CreateNew)
                try {
                    $buffer = New-Object byte[] (1024 * 1024)
                    [long]$total = 0
                    while (($read = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                        $total += $read
                        Assert-Condition ($total -le 209715200) 'release download exceeds 200 MiB'
                        $outputStream.Write($buffer, 0, $read)
                    }
                } finally {
                    $outputStream.Dispose()
                    $inputStream.Dispose()
                }
                return
            } finally {
                $response.Dispose()
            }
        }
        throw 'too many release redirects'
    } finally {
        $client.Dispose()
        $handler.Dispose()
    }
}

function Get-LatestReleaseTag([string]$Repo) {
    $metadataPath = Join-Path ([IO.Path]::GetTempPath()) "rgo-release-$([guid]::NewGuid().ToString('N')).json"
    try {
        Download-Https "https://api.github.com/repos/$Repo/releases/latest" $metadataPath
        Assert-Condition ((Get-Item -LiteralPath $metadataPath).Length -le 65536) 'latest release metadata exceeds 64 KiB'
        $release = Read-Json $metadataPath
        Assert-Condition ($release.PSObject.Properties['tag_name'] -and
            $release.PSObject.Properties['draft'] -and
            $release.PSObject.Properties['prerelease']) 'latest release metadata is incomplete'
        $tag = [string]$release.tag_name
        Assert-Condition ($tag -match '^v[0-9]+\.[0-9]+\.[0-9]+$' -and
            $release.draft -eq $false -and $release.prerelease -eq $false) 'latest release is not a published stable rgo version'
        return $tag
    } finally {
        if (Test-Path -LiteralPath $metadataPath) { Remove-Item -LiteralPath $metadataPath -Force }
    }
}

function Expected-Digest([string]$Manifest, [string]$Asset) {
    $matchesForAsset = @()
    foreach ($line in ($Manifest -split '\r?\n')) {
        if ($line -match '^([0-9a-fA-F]{64})\s+\*?(.+?)\s*$' -and $Matches[2] -eq $Asset) {
            $matchesForAsset += $Matches[1].ToLowerInvariant()
        }
    }
    Assert-Condition ($matchesForAsset.Count -eq 1) "expected one SHA256SUMS entry for $Asset"
    return $matchesForAsset[0]
}

function Extract-VerifiedZip([string]$ZipPath, [string]$Destination, [string]$Top) {
    Add-Type -AssemblyName System.IO.Compression
    $allowed = @('rgo.exe', 'rgo-rustc-wrapper.exe', 'README.md', 'doc.md', 'LICENSE-MIT', 'LICENSE-APACHE')
    $seen = @{}
    [long]$total = 0
    $archiveStream = [IO.File]::OpenRead($ZipPath)
    try {
        $zip = [IO.Compression.ZipArchive]::new($archiveStream, [IO.Compression.ZipArchiveMode]::Read, $true)
        try {
            foreach ($entry in $zip.Entries) {
                $name = $entry.FullName
                if ($name -eq "$Top/") { continue }
                Assert-Condition ($name -notmatch '\\' -and $name.StartsWith("$Top/", [StringComparison]::Ordinal)) "unsafe archive path: $name"
                $leaf = $name.Substring($Top.Length + 1)
                Assert-Condition ($allowed -ccontains $leaf -and -not $seen.ContainsKey($leaf)) "unexpected or duplicate archive member: $name"
                $fileKind = ($entry.ExternalAttributes -shr 16) -band 0xF000
                Assert-Condition ($fileKind -eq 0 -or $fileKind -eq 0x8000) "non-file archive member: $name"
                $total += $entry.Length
                Assert-Condition ($total -le 524288000) 'release bundle exceeds 500 MiB unpacked'
                $seen[$leaf] = $true
                $source = $entry.Open()
                $destinationPath = Join-Path $Destination $leaf
                try {
                    $target = [IO.File]::Open($destinationPath, [IO.FileMode]::CreateNew)
                    try { $source.CopyTo($target) } finally { $target.Dispose() }
                } finally { $source.Dispose() }
                Assert-Condition ((Get-Item -LiteralPath $destinationPath).Length -eq $entry.Length) "truncated archive member: $name"
            }
        } finally { $zip.Dispose() }
    } finally { $archiveStream.Dispose() }
    Assert-Condition ($seen.ContainsKey('rgo.exe') -and $seen.ContainsKey('rgo-rustc-wrapper.exe')) 'release bundle lacks one or both executables'
}

function File-Digest([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Resolve-AttestationVerifier([string]$Scratch) {
    $installed = Get-Command gh -CommandType Application -ErrorAction SilentlyContinue
    if ($installed) {
        try {
            $helpText = (& $installed.Source attestation verify --help 2>&1 | Out-String)
            if ($LASTEXITCODE -eq 0 -and $helpText.Contains('--source-ref')) {
                return $installed.Source
            }
        } catch { }
    }

    # Pinned to cli/cli's verified v2.101.0 release. Keep the verifier only in
    # this installation attempt's scratch directory; never modify the user's PATH.
    $asset = 'gh_2.101.0_windows_amd64.zip'
    $expected = 'bc6c814367b193cd8e713611d61e36013c0ef843b8f516458fe3eda039192794'
    $archive = Join-Path $Scratch $asset
    Download-Https "https://github.com/cli/cli/releases/download/v2.101.0/$asset" $archive
    Assert-Condition ((File-Digest $archive) -eq $expected) 'GitHub CLI verifier SHA-256 mismatch'
    $binary = Join-Path $Scratch 'gh.exe'
    Add-Type -AssemblyName System.IO.Compression
    $stream = [IO.File]::OpenRead($archive)
    try {
        $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Read, $true)
        try {
            $verifierEntries = @($zip.Entries | Where-Object { $_.FullName -ceq 'bin/gh.exe' })
            Assert-Condition ($verifierEntries.Count -eq 1 -and $verifierEntries[0].Length -le 104857600) 'GitHub CLI verifier archive has an invalid binary'
            $source = $verifierEntries[0].Open()
            try {
                $target = [IO.File]::Open($binary, [IO.FileMode]::CreateNew)
                try { $source.CopyTo($target) } finally { $target.Dispose() }
            } finally { $source.Dispose() }
        } finally { $zip.Dispose() }
    } finally { $stream.Dispose() }
    Assert-Condition ((Get-Item -LiteralPath $binary).Length -le 104857600) 'GitHub CLI verifier binary exceeds the size limit'
    $version = Invoke-Checked $binary @('--version')
    Assert-Condition ($version.StartsWith('gh version 2.101.0 ', [StringComparison]::Ordinal)) 'downloaded GitHub CLI verifier has the wrong version'
    return $binary
}

function Encoded-File([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    Assert-PlainFile $Path
    $item = Get-Item -LiteralPath $Path -Force
    Assert-Condition ($item.Length -le 4194304) "activation file is too large to journal: $Path"
    return [Convert]::ToBase64String([IO.File]::ReadAllBytes($Path))
}

function Write-EncodedFile([string]$Path, [AllowNull()][object]$Encoded) {
    if ($null -eq $Encoded) {
        if (Test-Path -LiteralPath $Path) { Remove-Item -LiteralPath $Path -Force }
        return
    }
    Assert-Condition ($Encoded -is [string]) "invalid activation snapshot for $Path"
    $temporary = "$Path.$([guid]::NewGuid().ToString('N')).tmp"
    try {
        [IO.File]::WriteAllBytes($temporary, [Convert]::FromBase64String($Encoded))
        if (Test-Path -LiteralPath $Path) {
            Assert-PlainFile $Path
            Replace-FileAtomic $temporary $Path
        } else { [IO.File]::Move($temporary, $Path) }
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}

function Publish-CommandCopy([string]$Source, [string]$Destination) {
    $temporary = "$Destination.$([guid]::NewGuid().ToString('N')).tmp"
    try {
        Copy-Item -LiteralPath $Source -Destination $temporary
        if (Test-Path -LiteralPath $Destination) {
            Assert-PlainFile $Destination
            Replace-FileAtomic $temporary $Destination
        } else { [IO.File]::Move($temporary, $Destination) }
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}

function Stage-VerifiedVersion([string]$Extracted, [string]$VersionDir, [string]$CliDigest, [string]$WrapperDigest) {
    if (-not (Test-Path -LiteralPath $VersionDir)) {
        $staged = Join-Path $script:versions ".stage-$([guid]::NewGuid().ToString('N'))"
        New-Item -ItemType Directory -Path $staged | Out-Null
        try {
            foreach ($name in @('rgo.exe', 'rgo-rustc-wrapper.exe', 'README.md', 'doc.md', 'LICENSE-MIT', 'LICENSE-APACHE')) {
                $source = Join-Path $Extracted $name
                if (Test-Path -LiteralPath $source) { Copy-Item -LiteralPath $source -Destination (Join-Path $staged $name) }
            }
            [IO.Directory]::Move($staged, $VersionDir)
        } finally {
            if (Test-Path -LiteralPath $staged) { Remove-Item -LiteralPath $staged -Recurse -Force }
        }
    }
    Assert-PlainDirectory $VersionDir
    $cli = Join-Path $VersionDir 'rgo.exe'
    $wrapper = Join-Path $VersionDir 'rgo-rustc-wrapper.exe'
    Assert-PlainFile $cli
    Assert-PlainFile $wrapper
    Assert-Condition ((File-Digest $cli) -eq $CliDigest -and (File-Digest $wrapper) -eq $WrapperDigest) 'versioned binaries differ from verified bundle'
}

function Assert-Record($State, [string]$Cli, [string]$Wrapper) {
    $recordPath = Join-Path $script:resolvedCargoHome '.rgo-install.json'
    Assert-Condition (Test-Path -LiteralPath $recordPath -PathType Leaf) "Cargo activation record is missing: $recordPath"
    $record = Read-Json $recordPath
    $wrapperOwned = if ($record.wrapper_binary) {
        (Test-SamePath $record.wrapper_binary $Wrapper) -and
        ($record.managed_keys -contains 'build.rustc-wrapper')
    } else {
        $record.managed_keys -notcontains 'build.rustc-wrapper'
    }
    Assert-Condition ($State.versionDirectory -match '^rgo-v(.+)-x86_64-pc-windows-msvc$') 'installer state has an invalid release directory'
    $expectedVersion = $Matches[1]
    $supervised = Test-SupervisedState $State
    $activationOwned = if ($supervised) {
        $shim = $record.supervised_cargo
        $shim -and $record.schema_version -eq 3 -and
        -not $record.wrapper_binary -and @($record.managed_keys).Count -eq 0 -and
        (Test-SamePath $shim.shim_path $script:shimPath) -and
        (Test-SamePath $shim.real_cargo $State.realCargo)
    } else {
        $record.schema_version -eq 2 -and -not $record.supervised_cargo -and $wrapperOwned
    }
    Assert-Condition ((Test-SamePath $record.cargo_home $script:resolvedCargoHome) -and
        (Test-SamePath $record.rgo_home $State.rgoHome) -and
        (Test-SamePath $record.rgo_binary $Cli) -and
        $activationOwned -and $record.binary_version -eq $expectedVersion) 'Cargo activation record belongs to a different installation'
}

function Assert-ShimFallback([string]$ShimPath, [string]$CargoHome, [string]$RealCargo) {
    $path = Join-Path (Split-Path -Path $ShimPath -Parent) '.rgo-cargo-fallback.json'
    Assert-PlainFile $path
    $fallback = Read-Json $path
    Assert-Condition ($fallback.schema_version -eq 1 -and
        (Test-SamePath $fallback.cargo_home $CargoHome) -and
        (Test-SamePath $fallback.real_cargo $RealCargo)) "Cargo fallback differs from its owned proxy: $path"
}

function Assert-PlainCargoActivation([string]$Cli, $State) {
    $previous = $env:RGO_HOME
    $previousPath = $env:PATH
    Remove-Item Env:RGO_HOME -ErrorAction SilentlyContinue
    try {
        if (Test-SupervisedState $State) { Prepend-ProcessPath $script:shimDir }
        Invoke-Checked $Cli @('doctor', '--verify') | Out-Null
    }
    finally {
        $env:PATH = $previousPath
        if ($null -eq $previous) { Remove-Item Env:RGO_HOME -ErrorAction SilentlyContinue }
        else { $env:RGO_HOME = $previous }
    }
}

function Assert-ServiceHealthy([string]$Cli, $State) {
    if ($State.noService) { return }
    $arguments = @('setup', '--dry-run')
    $noWrapper = if ($State -is [System.Collections.IDictionary]) { [bool]$State['noWrapper'] }
        else { [bool]($State.PSObject.Properties['noWrapper'] -and $State.noWrapper) }
    if ($noWrapper) { $arguments += '--no-wrapper' }
    if (Test-SupervisedState $State) { $arguments += @('--supervised', '--real-cargo', $State.realCargo) }
    Invoke-Checked $Cli $arguments | Out-Null
    $report = Invoke-Checked $Cli @('doctor', '--json') | ConvertFrom-Json
    $service = @($report.entries | Where-Object { $_.message -like 'daemon service *' })
    Assert-Condition ($service.Count -eq 1 -and $service[0].level -eq 'ok') 'owned daemon service is not running; rerun with -Repair'
}

function Test-ExactText([AllowNull()][object]$Left, [AllowNull()][object]$Right) {
    if ($null -eq $Left -or $null -eq $Right) {
        return ($null -eq $Left -and $null -eq $Right)
    }
    Assert-Condition ($Left -is [string] -and $Right -is [string]) 'invalid activation snapshot text'
    return [string]::Equals($Left, $Right, [StringComparison]::Ordinal)
}

function Test-ActivationPath([string]$Path, [string]$FallbackPath) {
    if ($FallbackPath -and (Test-SamePath $Path $FallbackPath)) { return $true }
    $allowed = @(
        (Join-Path $script:resolvedCargoHome '.rgo-home'),
        (Join-Path $script:resolvedCargoHome '.rgo-install.json'),
        (Join-Path $script:resolvedCargoHome 'config'),
        (Join-Path $script:resolvedCargoHome 'config.toml'),
        (Join-Path $script:resolvedRgoHome 'state/inner-wrapper'),
        (Join-Path $script:resolvedRgoHome 'state/owner-cargo-home'),
        (Join-Path $script:resolvedRgoHome 'state/storage-mode')
    )
    return @($allowed | Where-Object { Test-SamePath $_ $Path }).Count -eq 1
}

function Upgrade-Plan([string]$Cli, [bool]$NoWrapper, [bool]$Supervised,
    [string]$RealCargo, [string]$ShimPath) {
    $arguments = @('setup', '--installer-plan-json', '--no-service')
    if ($NoWrapper) { $arguments += '--no-wrapper' }
    if ($Supervised) { $arguments += @('--supervised', '--real-cargo', $RealCargo) }
    $output = Invoke-Checked $Cli $arguments
    $plan = ($output -split '\r?\n')[-1] | ConvertFrom-Json
    Assert-Condition ($plan.schema_version -eq 1 -and $plan.files) 'staged CLI returned an invalid installer plan'
    $fallbackPath = if ($Supervised) { Join-Path (Split-Path -Path $ShimPath -Parent) '.rgo-cargo-fallback.json' } else { $null }
    if ($Supervised) {
        Assert-Condition ($plan.binaries -and @($plan.binaries.PSObject.Properties).Count -eq 1) 'staged CLI did not plan exactly one Cargo shim binary'
        $binary = @($plan.binaries.PSObject.Properties)[0]
        Assert-Condition ((Test-SamePath $binary.Name $ShimPath) -and
            $binary.Value -match '^binary-blake3:[0-9a-f]{64}$') 'staged CLI planned an unexpected Cargo shim binary'
    }
    $entries = @()
    foreach ($property in $plan.files.PSObject.Properties) {
        $path = $property.Name
        Assert-Condition (Test-ActivationPath $path $fallbackPath) "staged CLI plans an unexpected activation write: $path"
        $contents = if ($null -eq $property.Value) { $null } else { $property.Value.contents }
        Assert-Condition ($null -eq $contents -or $contents -is [string]) "invalid planned contents for $path"
        $encoded = if ($null -eq $contents) { $null }
            else { [Convert]::ToBase64String([System.Text.UTF8Encoding]::new($false).GetBytes($contents)) }
        $entries += [ordered]@{ path = $path; before = (Encoded-File $path); after = $encoded }
    }
    Assert-Condition (@($entries | Where-Object { Test-SamePath $_.path (Join-Path $script:resolvedCargoHome '.rgo-install.json') }).Count -eq 1) 'staged CLI plan omitted the installation record'
    if ($Supervised) {
        Assert-Condition (@($entries | Where-Object { Test-SamePath $_.path $fallbackPath }).Count -eq 1) 'staged CLI plan omitted the Cargo fallback'
        $recordPath = Join-Path $script:resolvedCargoHome '.rgo-install.json'
        $recordEntry = @($entries | Where-Object { Test-SamePath $_.path $recordPath })[0]
        $record = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($recordEntry.after)) | ConvertFrom-Json
        Assert-Condition ($record.schema_version -eq 3 -and
            (Test-SamePath $record.supervised_cargo.shim_path $ShimPath) -and
            (Test-SamePath $record.supervised_cargo.real_cargo $RealCargo) -and
            $record.supervised_cargo.shim_contents -eq $binary.Value) 'staged CLI planned an inconsistent Cargo activation'
    }
    return $entries
}

function Assert-UpgradeState($State, [string]$Role) {
    Assert-Condition ($State.schemaVersion -eq 1 -and $State.noService -and
        (Test-SamePath $State.cargoHome $script:resolvedCargoHome) -and
        (Test-SamePath $State.rgoHome $script:resolvedRgoHome) -and
        (Test-SamePath $State.installRoot $script:resolvedInstallRoot) -and
        (Test-SamePath $State.binDir $script:resolvedBinDir) -and
        $State.versionDirectory -match '^rgo-v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?-x86_64-pc-windows-msvc$' -and
        $State.archiveDigest -match '^[0-9a-f]{64}$' -and
        $State.cliDigest -match '^[0-9a-f]{64}$' -and $State.wrapperDigest -match '^[0-9a-f]{64}$') "invalid $Role installer state"
}

function Assert-SupervisedUpgradePair($OldState, $NewState) {
    Assert-UpgradeState $OldState 'old'
    Assert-UpgradeState $NewState 'new'
    Assert-Condition ((Test-SupervisedState $OldState) -and (Test-SupervisedState $NewState) -and
        -not $OldState.noWrapper -and -not $NewState.noWrapper -and
        $OldState.versionDirectory -ne $NewState.versionDirectory -and
        (Test-SamePath $OldState.realCargo $NewState.realCargo) -and
        [bool]$OldState.noUserPath -eq [bool]$NewState.noUserPath -and
        [bool]$OldState.pathAdded -eq [bool]$NewState.pathAdded -and
        [bool]$OldState.priorUserPathPresent -eq [bool]$NewState.priorUserPathPresent -and
        $OldState.priorUserPathRaw -ceq $NewState.priorUserPathRaw -and
        $OldState.priorUserPathKind -eq $NewState.priorUserPathKind -and
        $OldState.priorUserPath -eq $NewState.priorUserPath) 'supervised upgrade states disagree about installation ownership'
    Assert-Condition ($OldState.versionDirectory -match '^rgo-(v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)-x86_64-pc-windows-msvc$') 'old supervised state has an invalid version'
    $oldShim = Join-Path (Join-Path $script:shimRoot $Matches[1]) 'cargo.exe'
    Assert-Condition ($NewState.versionDirectory -match '^rgo-(v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)-x86_64-pc-windows-msvc$') 'new supervised state has an invalid version'
    $newShim = Join-Path (Join-Path $script:shimRoot $Matches[1]) 'cargo.exe'
    Assert-Condition ((Test-SamePath $OldState.shimPath $oldShim) -and
        (Test-SamePath $NewState.shimPath $newShim) -and
        -not (Test-SamePath $oldShim $newShim)) 'supervised upgrade names an unexpected Cargo shim'
    return @{ oldShim = $oldShim; newShim = $newShim
        newFallback = (Join-Path (Split-Path -Path $newShim -Parent) '.rgo-cargo-fallback.json') }
}

function Invoke-WithSetupLocks([scriptblock]$Action) {
    $paths = @(
        (Join-Path $script:resolvedCargoHome '.rgo-setup.lock'),
        (Join-Path $script:resolvedRgoHome 'state/.rgo-service.lock')
    )
    $handles = @()
    try {
        foreach ($path in $paths) {
            if (Test-Path -LiteralPath $path) { Assert-PlainFile $path }
            $handle = [IO.File]::Open($path, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::ReadWrite)
            $locked = $false
            for ($attempt = 0; $attempt -lt 50; $attempt++) {
                try { $handle.Lock(0, 1); $locked = $true; break }
                catch [IO.IOException] { Start-Sleep -Milliseconds 100 }
            }
            if (-not $locked) { $handle.Dispose(); throw "could not acquire setup lock: $path" }
            $handles += $handle
        }
        & $Action
    } finally {
        for ($index = $handles.Count - 1; $index -ge 0; $index--) {
            try { $handles[$index].Unlock(0, 1) } finally { $handles[$index].Dispose() }
        }
    }
}

function Recover-Upgrade([string]$JournalPath) {
    if (-not (Test-Path -LiteralPath $JournalPath)) { return }
    Invoke-WithSetupLocks { Recover-UpgradeGuarded $JournalPath }
}

function Recover-UpgradeGuarded([string]$JournalPath) {
    if (-not (Test-Path -LiteralPath $JournalPath)) { return }
    Assert-PlainFile $JournalPath
    Assert-Condition ((Get-Item -LiteralPath $JournalPath).Length -le 67108864) 'upgrade journal is too large'
    $journal = Read-Json $JournalPath
    if ($journal.schemaVersion -eq 2) {
        Recover-SupervisedUpgradeGuarded $JournalPath $journal
        return
    }
    Assert-Condition ($journal.schemaVersion -eq 1) 'unsupported upgrade journal'
    Assert-UpgradeState $journal.oldState 'old'
    Assert-UpgradeState $journal.newState 'new'
    $seen = @{}
    foreach ($entry in $journal.files) {
        Assert-Condition (Test-ActivationPath $entry.path) "upgrade journal names an unexpected path: $($entry.path)"
        Assert-Condition (-not $seen.ContainsKey($entry.path)) "duplicate upgrade journal path: $($entry.path)"
        $seen[$entry.path] = $true
    }
    Assert-Condition ($seen.ContainsKey((Join-Path $script:resolvedCargoHome '.rgo-install.json'))) 'upgrade journal omitted the activation record'
    $oldCli = Join-Path (Join-Path $script:versions $journal.oldState.versionDirectory) 'rgo.exe'
    $oldWrapper = Join-Path (Join-Path $script:versions $journal.oldState.versionDirectory) 'rgo-rustc-wrapper.exe'
    Assert-PlainFile $oldCli
    Assert-PlainFile $oldWrapper
    Assert-Condition ((File-Digest $oldCli) -eq $journal.oldState.cliDigest -and
        (File-Digest $oldWrapper) -eq $journal.oldState.wrapperDigest) 'old rollback binaries changed'
    $state = Read-Json $script:statePath
    $currentStateJson = $state | ConvertTo-Json -Depth 8 -Compress
    $committed = Test-ExactText $currentStateJson ($journal.newState | ConvertTo-Json -Depth 8 -Compress)
    Assert-Condition ($committed -or
        (Test-ExactText $currentStateJson ($journal.oldState | ConvertTo-Json -Depth 8 -Compress))) 'upgrade journal and installed state disagree'
    foreach ($entry in $journal.files) {
        Assert-Condition (Test-ActivationPath $entry.path) "upgrade journal names an unexpected path: $($entry.path)"
        $current = Encoded-File $entry.path
        if ($committed) {
            Assert-Condition (Test-ExactText $current $entry.after) "committed activation file changed: $($entry.path)"
        } else {
            Assert-Condition ((Test-ExactText $current $entry.before) -or
                (Test-ExactText $current $entry.after)) "activation file changed during upgrade: $($entry.path)"
        }
    }
    $newCli = Join-Path (Join-Path $script:versions $journal.newState.versionDirectory) 'rgo.exe'
    $newWrapper = Join-Path (Join-Path $script:versions $journal.newState.versionDirectory) 'rgo-rustc-wrapper.exe'
    if ($committed) {
        Assert-PlainFile $newCli
        Assert-PlainFile $newWrapper
        Assert-Condition ((File-Digest $newCli) -eq $journal.newState.cliDigest -and
            (File-Digest $newWrapper) -eq $journal.newState.wrapperDigest) 'committed upgrade binaries changed'
    }
    $commands = @(
        @((Join-Path $script:resolvedBinDir 'rgo.exe'), $oldCli, $newCli, $journal.oldState.cliDigest, $journal.newState.cliDigest),
        @((Join-Path $script:resolvedBinDir 'rgo-rustc-wrapper.exe'), $oldWrapper, $newWrapper, $journal.oldState.wrapperDigest, $journal.newState.wrapperDigest)
    )
    foreach ($entry in $commands) {
        Assert-PlainFile $entry[0]
        $digest = File-Digest $entry[0]
        Assert-Condition ($digest -eq $entry[3] -or $digest -eq $entry[4]) "command entrypoint changed during upgrade: $($entry[0])"
        if ($committed) { Assert-Condition ($digest -eq $entry[4]) "committed command entrypoint is stale: $($entry[0])" }
    }
    if ($committed) {
        Assert-Record $journal.newState $newCli $newWrapper
    } else {
        foreach ($entry in $journal.files) { Write-EncodedFile $entry.path $entry.before }
        foreach ($entry in $commands) {
            if ((File-Digest $entry[0]) -ne $entry[3]) { Publish-CommandCopy $entry[1] $entry[0] }
        }
        Assert-Record $journal.oldState $oldCli $oldWrapper
    }
    Remove-Item -LiteralPath $JournalPath -Force
}

function Recover-SupervisedUpgradeGuarded([string]$JournalPath, $Journal) {
    $pair = Assert-SupervisedUpgradePair $Journal.oldState $Journal.newState
    $oldDir = Join-Path $script:versions $Journal.oldState.versionDirectory
    $newDir = Join-Path $script:versions $Journal.newState.versionDirectory
    $oldCli = Join-Path $oldDir 'rgo.exe'
    $oldWrapper = Join-Path $oldDir 'rgo-rustc-wrapper.exe'
    $newCli = Join-Path $newDir 'rgo.exe'
    $newWrapper = Join-Path $newDir 'rgo-rustc-wrapper.exe'
    foreach ($entry in @(
        @($oldCli, $Journal.oldState.cliDigest), @($oldWrapper, $Journal.oldState.wrapperDigest),
        @($newCli, $Journal.newState.cliDigest), @($newWrapper, $Journal.newState.wrapperDigest),
        @($pair.oldShim, $Journal.oldState.cliDigest)
    )) {
        Assert-PlainFile $entry[0]
        Assert-Condition ((File-Digest $entry[0]) -eq $entry[1]) "upgrade recovery binary changed: $($entry[0])"
    }
    Assert-ShimFallback $pair.oldShim $script:resolvedCargoHome $Journal.oldState.realCargo
    $newShimExists = Test-Path -LiteralPath $pair.newShim
    if ($newShimExists) {
        Assert-PlainFile $pair.newShim
        Assert-Condition ((File-Digest $pair.newShim) -eq $Journal.newState.cliDigest) 'new Cargo shim changed during upgrade'
    }
    $state = Read-Json $script:statePath
    $stateJson = $state | ConvertTo-Json -Depth 8 -Compress
    $committed = Test-ExactText $stateJson ($Journal.newState | ConvertTo-Json -Depth 8 -Compress)
    Assert-Condition ($committed -or
        (Test-ExactText $stateJson ($Journal.oldState | ConvertTo-Json -Depth 8 -Compress))) 'supervised upgrade journal and installer state disagree'
    $seen = @{}
    foreach ($entry in $Journal.files) {
        Assert-Condition (Test-ActivationPath $entry.path $pair.newFallback) "supervised upgrade journal names an unexpected path: $($entry.path)"
        $key = Full-Path $entry.path
        Assert-Condition (-not $seen.ContainsKey($key)) "duplicate supervised upgrade path: $($entry.path)"
        $seen[$key] = $true
        $current = Encoded-File $entry.path
        if ($committed) {
            Assert-Condition (Test-ExactText $current $entry.after) "committed activation file changed: $($entry.path)"
        } else {
            Assert-Condition ((Test-ExactText $current $entry.before) -or
                (Test-ExactText $current $entry.after)) "activation file changed during supervised upgrade: $($entry.path)"
        }
    }
    Assert-Condition ($seen.ContainsKey((Full-Path (Join-Path $script:resolvedCargoHome '.rgo-install.json'))) -and
        $seen.ContainsKey((Full-Path $pair.newFallback))) 'supervised upgrade journal omitted the record or new fallback'
    $commands = @(
        @((Join-Path $script:resolvedBinDir 'rgo.exe'), $oldCli, $newCli, $Journal.oldState.cliDigest, $Journal.newState.cliDigest),
        @((Join-Path $script:resolvedBinDir 'rgo-rustc-wrapper.exe'), $oldWrapper, $newWrapper, $Journal.oldState.wrapperDigest, $Journal.newState.wrapperDigest)
    )
    foreach ($entry in $commands) {
        Assert-PlainFile $entry[0]
        $digest = File-Digest $entry[0]
        Assert-Condition ($digest -eq $entry[3] -or $digest -eq $entry[4]) "command entrypoint changed during supervised upgrade: $($entry[0])"
        if ($committed) { Assert-Condition ($digest -eq $entry[4]) "committed command entrypoint is stale: $($entry[0])" }
    }
    if (-not $Journal.oldState.noUserPath) {
        Select-OwnedShim $Journal.oldState
        $expectedOldPath = Expected-OwnedUserPath $Journal.oldState
        Select-OwnedShim $Journal.newState
        $expectedNewPath = Expected-OwnedUserPath $Journal.newState
        $kind = if ($Journal.oldState.priorUserPathPresent) { $Journal.oldState.priorUserPathKind } else { 'String' }
        Assert-Condition ($Journal.userPathBefore.present -and $Journal.userPathAfter.present -and
            $Journal.userPathBefore.value -ceq $expectedOldPath -and
            $Journal.userPathAfter.value -ceq $expectedNewPath -and
            $Journal.userPathBefore.kind -eq $kind -and $Journal.userPathAfter.kind -eq $kind) 'supervised upgrade journal has an invalid User PATH transition'
        $currentPath = Get-UserPathSnapshot
        $before = $currentPath.present -and $currentPath.kind -eq $kind -and
            $currentPath.value -ceq $expectedOldPath
        $after = $currentPath.present -and $currentPath.kind -eq $kind -and
            $currentPath.value -ceq $expectedNewPath
        Assert-Condition ($before -or $after) 'User PATH changed outside the supervised upgrade; refusing to overwrite it'
        if ($committed) { Assert-Condition $after 'committed supervised upgrade has a stale User PATH' }
    }
    if ($committed) {
        Assert-Condition $newShimExists 'committed supervised upgrade has no new Cargo shim'
        Assert-ShimFallback $pair.newShim $script:resolvedCargoHome $Journal.newState.realCargo
        Select-OwnedShim $Journal.newState
        Assert-Record $Journal.newState $newCli $newWrapper
    } else {
        foreach ($entry in $Journal.files) {
            $restore = if ((Test-SamePath $entry.path $pair.newFallback) -and $newShimExists) {
                $entry.after
            } else { $entry.before }
            Write-EncodedFile $entry.path $restore
        }
        if (-not $Journal.oldState.noUserPath) {
            $currentPath = Get-UserPathSnapshot
            if ($currentPath.value -cne $Journal.userPathBefore.value) {
                Set-UserPathRaw $true $Journal.userPathBefore.value $Journal.userPathBefore.kind
            }
        }
        foreach ($entry in $commands) {
            if ((File-Digest $entry[0]) -ne $entry[3]) { Publish-CommandCopy $entry[1] $entry[0] }
        }
        Select-OwnedShim $Journal.oldState
        Prepend-ProcessPath $script:shimDir
        Assert-Record $Journal.oldState $oldCli $oldWrapper
        if ($newShimExists) {
            Assert-ShimFallback $pair.newShim $script:resolvedCargoHome $Journal.newState.realCargo
        }
    }
    Remove-Item -LiteralPath $JournalPath -Force
}

function Invoke-NoServiceUpgrade($OldState, [string]$Top, [string]$ArchiveDigest,
    [string]$CliDigest, [string]$WrapperDigest, [string]$Extracted, [bool]$NewNoWrapper) {
    Assert-UpgradeState $OldState 'old'
    Assert-Condition (-not (Test-SupervisedState $OldState)) 'supervised Windows upgrades require versioned shim paths; undo and use a fresh storage root for now'
    Assert-Condition ($script:NoService -and -not $script:Repair) 'version upgrades currently require -NoService and cannot use -Repair'
    $oldNoWrapper = [bool]($OldState.PSObject.Properties['noWrapper'] -and $OldState.noWrapper)
    Assert-Condition ($NewNoWrapper -eq $oldNoWrapper) 'change wrapper mode separately from a version upgrade'
    Assert-Condition ($OldState.versionDirectory -ne $Top) 'a release tag cannot be repacked with different bytes'
    Assert-Condition (-not (Test-Path -LiteralPath $script:pendingPath)) 'finish the pending first installation before upgrading'
    $oldDir = Join-Path $script:versions $OldState.versionDirectory
    $oldCli = Join-Path $oldDir 'rgo.exe'
    $oldWrapper = Join-Path $oldDir 'rgo-rustc-wrapper.exe'
    Assert-PlainDirectory $oldDir
    Assert-PlainFile $oldCli
    Assert-PlainFile $oldWrapper
    Assert-Condition ((File-Digest $oldCli) -eq $OldState.cliDigest -and
        (File-Digest $oldWrapper) -eq $OldState.wrapperDigest) 'old release pair changed; repair it before upgrading'
    Assert-Record $OldState $oldCli $oldWrapper
    foreach ($entry in @(
        @((Join-Path $script:resolvedBinDir 'rgo.exe'), $OldState.cliDigest),
        @((Join-Path $script:resolvedBinDir 'rgo-rustc-wrapper.exe'), $OldState.wrapperDigest)
    )) {
        Assert-PlainFile $entry[0]
        Assert-Condition ((File-Digest $entry[0]) -eq $entry[1]) "old command entrypoint changed: $($entry[0])"
    }
    $newDir = Join-Path $script:versions $Top
    Stage-VerifiedVersion $Extracted $newDir $CliDigest $WrapperDigest
    $newCli = Join-Path $newDir 'rgo.exe'
    $newWrapper = Join-Path $newDir 'rgo-rustc-wrapper.exe'
    $noUserPath = $OldState.PSObject.Properties['noUserPath'] -and $OldState.noUserPath
    $newState = [ordered]@{
        schemaVersion = 1; cargoHome = $script:resolvedCargoHome; rgoHome = $script:resolvedRgoHome
        installRoot = $script:resolvedInstallRoot; binDir = $script:resolvedBinDir
        versionDirectory = $Top; archiveDigest = $ArchiveDigest
        cliDigest = $CliDigest; wrapperDigest = $WrapperDigest
        noService = $true; noWrapper = $NewNoWrapper; noUserPath = [bool]$noUserPath
        pathAdded = [bool]$OldState.pathAdded; priorUserPath = $OldState.priorUserPath
    }
    $files = @(Upgrade-Plan $newCli $NewNoWrapper)
    $recordPath = Join-Path $script:resolvedCargoHome '.rgo-install.json'
    $recordEntry = @($files | Where-Object { Test-SamePath $_.path $recordPath })[0]
    Assert-Condition ($null -ne $recordEntry.after) 'staged CLI plans to remove the installation record'
    $oldRecord = Read-Json $recordPath
    $plannedRecord = [Text.Encoding]::UTF8.GetString(
        [Convert]::FromBase64String($recordEntry.after)) | ConvertFrom-Json
    Assert-Condition ([bool]$oldRecord.wrapper_binary -eq [bool]$plannedRecord.wrapper_binary) 'the effective compiler-wrapper mode changed; restore the previous Cargo configuration before upgrading'
    Write-JsonAtomic $script:upgradeJournalPath ([ordered]@{
        schemaVersion = 1; oldState = $OldState; newState = $newState; files = $files
    })
    try {
        $setupArgs = @('setup', '--no-service')
        if ($NewNoWrapper) { $setupArgs += '--no-wrapper' }
        Invoke-Checked $newCli $setupArgs | Out-Null
        foreach ($entry in $files) {
            Assert-Condition (Test-ExactText (Encoded-File $entry.path) $entry.after) "setup differed from its installer plan: $($entry.path)"
        }
        Assert-Record $newState $newCli $newWrapper
        Assert-PlainCargoActivation $newCli $newState
        Publish-CommandCopy $newWrapper (Join-Path $script:resolvedBinDir 'rgo-rustc-wrapper.exe')
        Publish-CommandCopy $newCli (Join-Path $script:resolvedBinDir 'rgo.exe')
        Replace-JsonAtomic $script:statePath $newState
        Remove-Item -LiteralPath $script:upgradeJournalPath -Force
    } catch {
        $failure = $_
        try { Recover-Upgrade $script:upgradeJournalPath }
        catch { throw "upgrade failed ($failure); rollback also failed: $_" }
        $current = Read-Json $script:statePath
        if ($current.archiveDigest -eq $ArchiveDigest -and $current.versionDirectory -eq $Top) {
            Write-Host "Upgrade to $Top committed; recovered its final journal cleanup"
            return
        }
        throw $failure
    }
    Write-Host "Upgraded rgo to $Top; previous binaries remain available for running Cargo processes"
}

function Invoke-SupervisedUpgrade($OldState, [string]$Top, [string]$ArchiveDigest,
    [string]$CliDigest, [string]$WrapperDigest, [string]$Extracted) {
    Assert-Condition ($script:NoService -and -not $script:Repair) 'supervised version upgrades require -NoService and cannot use -Repair'
    Assert-Condition (-not (Test-Path -LiteralPath $script:pendingPath)) 'finish the pending first installation before upgrading'
    Assert-Condition ($OldState.versionDirectory -ne $Top) 'a release tag cannot be repacked with different bytes'
    Assert-UpgradeState $OldState 'old'
    Select-OwnedShim $OldState
    $oldShim = $script:shimPath
    $oldDir = Join-Path $script:versions $OldState.versionDirectory
    $oldCli = Join-Path $oldDir 'rgo.exe'
    $oldWrapper = Join-Path $oldDir 'rgo-rustc-wrapper.exe'
    foreach ($entry in @(@($oldCli, $OldState.cliDigest), @($oldWrapper, $OldState.wrapperDigest),
        @($oldShim, $OldState.cliDigest),
        @((Join-Path $script:resolvedBinDir 'rgo.exe'), $OldState.cliDigest),
        @((Join-Path $script:resolvedBinDir 'rgo-rustc-wrapper.exe'), $OldState.wrapperDigest))) {
        Assert-PlainFile $entry[0]
        Assert-Condition ((File-Digest $entry[0]) -eq $entry[1]) "old supervised binary changed: $($entry[0])"
    }
    Assert-ShimFallback $oldShim $script:resolvedCargoHome $OldState.realCargo
    Assert-Record $OldState $oldCli $oldWrapper
    Assert-OwnedUserPath $OldState
    $beforePath = if ($OldState.noUserPath) { $null } else { Get-UserPathSnapshot }
    if (-not $OldState.noUserPath) {
        $kind = if ($OldState.priorUserPathPresent) { $OldState.priorUserPathKind } else { 'String' }
        Assert-Condition ($beforePath.present -and $beforePath.kind -eq $kind -and
            $beforePath.value -ceq (Expected-OwnedUserPath $OldState)) 'owned User PATH is not active; repair it before upgrading'
    }
    $newDir = Join-Path $script:versions $Top
    Stage-VerifiedVersion $Extracted $newDir $CliDigest $WrapperDigest
    $newCli = Join-Path $newDir 'rgo.exe'
    $newWrapper = Join-Path $newDir 'rgo-rustc-wrapper.exe'
    Assert-Condition ($Top -match '^rgo-(v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)-x86_64-pc-windows-msvc$') 'new release has an invalid shim version'
    $expectedVersion = $Matches[1].Substring(1)
    $newShim = Join-Path (Join-Path $script:shimRoot $Matches[1]) 'cargo.exe'
    $newState = [pscustomobject]([ordered]@{
        schemaVersion = 1; cargoHome = $script:resolvedCargoHome; rgoHome = $script:resolvedRgoHome
        installRoot = $script:resolvedInstallRoot; binDir = $script:resolvedBinDir
        versionDirectory = $Top; archiveDigest = $ArchiveDigest
        cliDigest = $CliDigest; wrapperDigest = $WrapperDigest
        noService = $true; noWrapper = $false; noUserPath = [bool]$OldState.noUserPath
        supervised = $true; realCargo = $OldState.realCargo; shimPath = $newShim
        pathAdded = [bool]$OldState.pathAdded; priorUserPath = $OldState.priorUserPath
        priorUserPathPresent = [bool]$OldState.priorUserPathPresent
        priorUserPathRaw = $OldState.priorUserPathRaw; priorUserPathKind = $OldState.priorUserPathKind
    })
    $pair = Assert-SupervisedUpgradePair $OldState $newState
    if (Test-Path -LiteralPath $pair.newShim) {
        Assert-PlainFile $pair.newShim
        Assert-Condition ((File-Digest $pair.newShim) -eq $CliDigest) 'new Cargo shim exists with different bytes'
        Assert-ShimFallback $pair.newShim $script:resolvedCargoHome $OldState.realCargo
    }
    $files = @(Upgrade-Plan $newCli $false $true $OldState.realCargo $pair.newShim)
    $recordPath = Join-Path $script:resolvedCargoHome '.rgo-install.json'
    $recordEntry = @($files | Where-Object { Test-SamePath $_.path $recordPath })[0]
    $plannedRecord = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($recordEntry.after)) | ConvertFrom-Json
    Assert-Condition ((Test-SamePath $plannedRecord.rgo_binary $newCli) -and
        $plannedRecord.binary_version -eq $expectedVersion) 'staged CLI planned an unexpected release version'
    $afterPath = if ($OldState.noUserPath) { $null } else {
        Select-OwnedShim $newState
        @{ present = $true; value = (Expected-OwnedUserPath $newState); kind = $beforePath.kind }
    }
    Select-OwnedShim $OldState
    Write-JsonAtomic $script:upgradeJournalPath ([ordered]@{
        schemaVersion = 2; oldState = $OldState; newState = $newState
        files = $files; userPathBefore = $beforePath; userPathAfter = $afterPath
    })
    try {
        Invoke-Checked $newCli @('setup', '--supervised', '--real-cargo', $OldState.realCargo, '--no-service') | Out-Null
        foreach ($entry in $files) {
            Assert-Condition (Test-ExactText (Encoded-File $entry.path) $entry.after) "supervised setup differed from its installer plan: $($entry.path)"
        }
        Assert-PlainFile $pair.newShim
        Assert-Condition ((File-Digest $pair.newShim) -eq $CliDigest) 'new Cargo shim differs from the verified release'
        Select-OwnedShim $newState
        Assert-ShimFallback $pair.newShim $script:resolvedCargoHome $OldState.realCargo
        Assert-Record $newState $newCli $newWrapper
        Assert-PlainCargoActivation $newCli $newState
        if (-not $OldState.noUserPath) {
            $currentPath = Get-UserPathSnapshot
            Assert-Condition ($currentPath.present -and $currentPath.kind -eq $beforePath.kind -and
                $currentPath.value -ceq $beforePath.value) 'User PATH changed while supervised upgrade was staged'
            Set-UserPathRaw $true $afterPath.value $afterPath.kind
        }
        Prepend-ProcessPath $script:shimDir
        Publish-CommandCopy $newWrapper (Join-Path $script:resolvedBinDir 'rgo-rustc-wrapper.exe')
        Publish-CommandCopy $newCli (Join-Path $script:resolvedBinDir 'rgo.exe')
        Replace-JsonAtomic $script:statePath $newState
        Remove-Item -LiteralPath $script:upgradeJournalPath -Force
    } catch {
        $failure = $_
        try { Recover-Upgrade $script:upgradeJournalPath }
        catch { throw "supervised upgrade failed ($failure); rollback also failed: $_" }
        $current = Read-Json $script:statePath
        if ($current.archiveDigest -eq $ArchiveDigest -and $current.versionDirectory -eq $Top) {
            Write-Host "Upgrade to $Top committed; recovered its final journal cleanup"
            return
        }
        throw $failure
    }
    Write-Host "Upgraded supervised rgo to $Top; old Cargo shims remain safe for existing shells"
}

function Add-ProcessPath([string]$Directory) {
    $processEntries = @($env:PATH -split ';' | Where-Object { $_ })
    if (@($processEntries | Where-Object { Test-SamePath $_ $Directory }).Count -eq 0) {
        $env:PATH = "$env:PATH;$Directory"
    }
}

function Prepend-ProcessPath([string]$Directory) {
    $entries = @($env:PATH -split ';' | Where-Object { $_ -and -not (Test-SamePath $_ $Directory) })
    $env:PATH = (@($Directory) + $entries) -join ';'
}

function Get-UserPathSnapshot {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    if ($null -eq $key) { return @{ present = $false; value = $null; kind = $null } }
    try {
        if (@($key.GetValueNames()) -notcontains 'Path') {
            return @{ present = $false; value = $null; kind = $null }
        }
        $kind = $key.GetValueKind('Path')
        Assert-Condition ($kind -in @([Microsoft.Win32.RegistryValueKind]::String,
            [Microsoft.Win32.RegistryValueKind]::ExpandString)) 'User PATH has an unsupported registry value type'
        $value = $key.GetValue('Path', $null,
            [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        return @{ present = $true; value = [string]$value; kind = $kind.ToString() }
    } finally { $key.Dispose() }
}

function Notify-UserPathChange {
    if (-not ('RgoPathBroadcast' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class RgoPathBroadcast {
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr SendMessageTimeout(IntPtr window, uint message,
        IntPtr wParam, string lParam, uint flags, uint timeout, out IntPtr result);
}
'@
    }
    $result = [IntPtr]::Zero
    $sent = [RgoPathBroadcast]::SendMessageTimeout([IntPtr]65535, [uint32]26,
        [IntPtr]::Zero, 'Environment', [uint32]2, [uint32]1000, [ref]$result)
    if ($sent -eq [IntPtr]::Zero) {
        Write-Warning 'User PATH was saved, but the shell notification did not complete; sign out and back in if a fresh shell still finds the old Cargo'
    }
}

function Set-UserPathRaw([bool]$Present, [AllowNull()][string]$Value, [AllowNull()][string]$Kind) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment', $true)
    Assert-Condition ($null -ne $key) 'cannot open the current user environment registry key for writing'
    try {
        if ($Present) {
            $registryKind = [Microsoft.Win32.RegistryValueKind]$Kind
            Assert-Condition ($registryKind -in @([Microsoft.Win32.RegistryValueKind]::String,
                [Microsoft.Win32.RegistryValueKind]::ExpandString)) 'refusing an unsupported User PATH registry value type'
            $key.SetValue('Path', $Value, $registryKind)
        } else {
            $key.DeleteValue('Path', $false)
        }
    } finally { $key.Dispose() }
    Notify-UserPathChange
}

function Expected-OwnedUserPath($State) {
    if (Test-SupervisedState $State) {
        $parts = @($script:shimDir)
        if ($State.priorUserPathPresent -and $State.priorUserPathRaw) { $parts += $State.priorUserPathRaw }
        if ($State.pathAdded) { $parts += $script:resolvedBinDir }
        return ($parts -join ';')
    }
    $parts = @()
    if ($State.pathAdded) {
        # The earlier native installer normalized empty PATH entries before
        # appending its command directory. Accept that exact owned shape.
        $parts += @($State.priorUserPath -split ';' | Where-Object { $_ })
        $parts += $script:resolvedBinDir
    } elseif ($State.priorUserPath) {
        $parts += $State.priorUserPath
    }
    return ($parts -join ';')
}

function Assert-OwnedUserPath($State) {
    if ($State.noUserPath) { return }
    if (Test-SupervisedState $State) {
        $current = Get-UserPathSnapshot
        $prior = ([bool]$current.present -eq [bool]$State.priorUserPathPresent) -and
            ($current.value -ceq $State.priorUserPathRaw) -and
            ($current.kind -eq $State.priorUserPathKind)
        $activeKind = if ($State.priorUserPathPresent) { $State.priorUserPathKind } else { 'String' }
        $active = $current.present -and ($current.kind -eq $activeKind) -and
            ($current.value -ceq (Expected-OwnedUserPath $State))
        Assert-Condition ($prior -or $active) 'User PATH changed since activation; preserve the owned rgo shim entry before retrying'
        return
    }
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    $expected = Expected-OwnedUserPath $State
    Assert-Condition ($current -eq $State.priorUserPath -or $current -eq $expected) 'User PATH changed since activation; preserve the owned rgo entries before retrying'
}

function Publish-OwnedPath($State) {
    if (-not $State.noUserPath) {
        Assert-OwnedUserPath $State
        $expected = Expected-OwnedUserPath $State
        if (Test-SupervisedState $State) {
            $current = Get-UserPathSnapshot
            if ($current.value -cne $expected) {
                $kind = if ($State.priorUserPathPresent) { $State.priorUserPathKind } else { 'String' }
                Set-UserPathRaw $true $expected $kind
            }
        } else {
            $current = [Environment]::GetEnvironmentVariable('Path', 'User')
            if ($current -ne $expected) {
                [Environment]::SetEnvironmentVariable('Path', $expected, 'User')
            }
        }
    }
    Add-ProcessPath $script:resolvedBinDir
    if (Test-SupervisedState $State) { Prepend-ProcessPath $script:shimDir }
}

function Restore-OwnedPath($State) {
    if ($State.noUserPath) { return }
    Assert-OwnedUserPath $State
    if (Test-SupervisedState $State) {
        $current = Get-UserPathSnapshot
        if ($current.present -ne $State.priorUserPathPresent -or
            $current.value -cne $State.priorUserPathRaw -or
            $current.kind -ne $State.priorUserPathKind) {
            Set-UserPathRaw ([bool]$State.priorUserPathPresent) $State.priorUserPathRaw $State.priorUserPathKind
        }
        return
    }
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($current -ne $State.priorUserPath) {
        [Environment]::SetEnvironmentVariable('Path', $State.priorUserPath, 'User')
    }
}

function Resolve-RealCargo([string]$Requested) {
    if ($Requested) {
        $path = Full-Path $Requested
    } else {
        $path = $null
        foreach ($command in @(Get-Command cargo.exe -All -ErrorAction SilentlyContinue)) {
            if ($command.CommandType -ne 'Application') { continue }
            $candidate = Full-Path $command.Source
            if ($candidate -match '[\\/]rgo[\\/]shims[\\/](?:[^\\/]+[\\/])?cargo\.exe$') { continue }
            $path = $candidate
            break
        }
    }
    Assert-Condition ([bool]$path) 'no real cargo.exe was found; pass -RealCargo with its absolute path'
    Assert-Condition (([IO.Path]::GetFileName($path) -ieq 'cargo.exe') -and
        -not (Test-SamePath $path $script:shimPath)) 'the real Cargo proxy must be an absolute cargo.exe outside rgo shims'
    Assert-PlainFile $path
    Assert-Condition ((Invoke-Checked $path @('--version')) -match '^cargo [0-9]+\.[0-9]+\.[0-9]+') 'the selected real Cargo proxy did not report a Cargo version'
    return $path
}

function Assert-Platform {
    Assert-Condition ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'this installer requires Windows'
    Assert-Condition ([Environment]::Is64BitOperatingSystem -and
        [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() -eq 'X64') 'the prebuilt Windows bundle requires x86_64 Windows'
}

Assert-Platform
if ($Supervised -and -not ($Uninstall -or $VerifyOnly)) {
    Assert-Condition (-not $NoWrapper) '-Supervised cannot be combined with -NoWrapper'
}
if (-not $CargoHome) { $CargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' } }
$resolvedCargoHome = Full-Path $CargoHome
if (-not $RgoHome) { $RgoHome = if ($env:RGO_HOME) { $env:RGO_HOME } else { Join-Path $env:USERPROFILE '.rgo' } }
$resolvedRgoHome = Full-Path $RgoHome
if (-not $InstallRoot) { $InstallRoot = Join-Path $resolvedCargoHome 'rgo' }
if (-not $BinDir) { $BinDir = Join-Path $resolvedCargoHome 'bin' }
$resolvedInstallRoot = Full-Path $InstallRoot
$resolvedBinDir = Full-Path $BinDir
$shimRoot = Join-Path $resolvedCargoHome 'rgo/shims'
$shimDir = $shimRoot
$shimPath = Join-Path $shimDir 'cargo.exe'
Assert-Condition ((Test-ChildPath $resolvedInstallRoot $resolvedCargoHome) -and
    (Test-ChildPath $resolvedBinDir $resolvedCargoHome)) 'install root and command directory must be inside Cargo home'
$versions = Join-Path $resolvedInstallRoot 'versions'
$statePath = Join-Path $resolvedInstallRoot 'installer-windows.json'
$pendingPath = Join-Path $resolvedInstallRoot 'installer-windows-pending.json'
$upgradeJournalPath = Join-Path $resolvedInstallRoot 'installer-windows-upgrade.json'
$previousCargoHome = $env:CARGO_HOME
$previousRgoHome = $env:RGO_HOME

$lock = $null
if (-not $VerifyOnly) {
    New-Item -ItemType Directory -Force -Path $resolvedInstallRoot | Out-Null
    Assert-PlainDirectory $resolvedCargoHome
    Assert-PlainDirectory $resolvedInstallRoot
    $lock = [IO.File]::Open((Join-Path $resolvedInstallRoot '.install-windows.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
}
$env:CARGO_HOME = $resolvedCargoHome
$env:RGO_HOME = $resolvedRgoHome
try {
    if (-not $VerifyOnly) { Recover-Upgrade $upgradeJournalPath }
    if ($Uninstall) {
        Assert-Condition (-not ($VerifyOnly -or $Repair -or $ReleaseTag -or $Latest -or $Archive -or $Repository)) '-Uninstall cannot be combined with install inputs'
        if (-not (Test-Path -LiteralPath $statePath) -and -not (Test-Path -LiteralPath $pendingPath)) {
            throw 'no installer-owned Windows activation was found'
        }
        $state = if (Test-Path -LiteralPath $statePath) { Read-Json $statePath } else { Read-Json $pendingPath }
        Select-OwnedShim $state
        Assert-Condition ((Test-SamePath $state.cargoHome $resolvedCargoHome) -and
            (Test-SamePath $state.installRoot $resolvedInstallRoot) -and
            (Test-SamePath $state.binDir $resolvedBinDir) -and
            (Test-SamePath $state.rgoHome $resolvedRgoHome)) 'installer state belongs to a different destination'
        Assert-Condition ($state.schemaVersion -eq 1 -and $state.versionDirectory -match '^rgo-v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?-x86_64-pc-windows-msvc$') 'installer state has an unsupported version or path'
        Assert-OwnedUserPath $state
        $versionDir = Join-Path $versions $state.versionDirectory
        $cli = Join-Path $versionDir 'rgo.exe'
        $wrapper = Join-Path $versionDir 'rgo-rustc-wrapper.exe'
        $ownedCli = Join-Path $resolvedBinDir 'rgo.exe'
        $ownedWrapper = Join-Path $resolvedBinDir 'rgo-rustc-wrapper.exe'
        foreach ($pair in @(@($ownedCli, $state.cliDigest), @($ownedWrapper, $state.wrapperDigest))) {
            if (Test-Path -LiteralPath $pair[0]) {
                Assert-PlainFile $pair[0]
                Assert-Condition ((File-Digest $pair[0]) -eq $pair[1]) "command entrypoint changed: $($pair[0])"
            }
        }
        if (Test-Path -LiteralPath (Join-Path $resolvedCargoHome '.rgo-install.json')) {
            Assert-Record $state $cli $wrapper
            $undoArgs = @('setup', '--undo')
            if ($state.noService) { $undoArgs += '--no-service' }
            Invoke-Checked $cli $undoArgs | Out-Null
        }
        Assert-Condition (-not (Test-Path -LiteralPath (Join-Path $resolvedCargoHome '.rgo-install.json'))) 'Cargo activation remains after undo'
        if (Test-SupervisedState $state) {
            if ($state.PSObject.Properties['shimPath'] -and $state.shimPath) {
                if (Test-Path -LiteralPath $shimPath) {
                    Assert-PlainFile $shimPath
                    Assert-Condition ((File-Digest $shimPath) -eq $state.cliDigest) 'retained Cargo fallback changed during undo'
                    Assert-PlainFile (Join-Path $shimDir '.rgo-cargo-fallback.json')
                }
            } else {
                Assert-Condition (-not (Test-Path -LiteralPath $shimPath)) 'legacy Cargo shim remains after undo'
            }
        }
        foreach ($path in @($ownedCli, $ownedWrapper)) {
            if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
        }
        Restore-OwnedPath $state
        if (Test-Path -LiteralPath $statePath) { Remove-Item -LiteralPath $statePath -Force }
        if (Test-Path -LiteralPath $pendingPath) { Remove-Item -LiteralPath $pendingPath -Force }
        Write-Host "Removed owned Cargo activation and commands; retained the versioned Cargo fallback for old shells and managed data at $resolvedRgoHome"
        return
    }

    Assert-Condition (-not $Latest -or -not ($ReleaseTag -or $Archive -or $DevelopmentBundle -or $Repair)) '-Latest cannot be combined with -ReleaseTag, -Archive, -DevelopmentBundle, or -Repair'
    if ($Repository) {
        Assert-Condition ($Repository -match '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' -and
            @('.', '..') -notcontains ($Repository -split '/')[0] -and
            @('.', '..') -notcontains ($Repository -split '/')[1]) '-Repository must be OWNER/REPO'
    }
    if (-not $Repository -and -not $DevelopmentBundle) { $Repository = 'Augani/rgo' }
    if ($Latest) {
        $ReleaseTag = Get-LatestReleaseTag $Repository
        Write-Host "Selected $Repository $ReleaseTag"
    }
    Assert-Condition ($ReleaseTag -match '^v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$') 'use -Latest or -ReleaseTag with an exact vMAJOR.MINOR.PATCH tag'
    Assert-Condition (-not $DevelopmentBundle -or $Archive) '-DevelopmentBundle requires a local archive'
    Assert-Condition (-not ($Sha256 -and $Sha256Sums)) 'use either -Sha256 or -Sha256Sums'
    Assert-Condition (-not $Sha256 -or $Sha256 -match '^[0-9a-fA-F]{64}$') '-Sha256 must be 64 hex characters'
    Assert-Condition (-not $Repair -or -not $VerifyOnly) '-Repair cannot be combined with -VerifyOnly'

    $top = "rgo-$ReleaseTag-x86_64-pc-windows-msvc"
    $asset = "$top.zip"
    $scratch = Join-Path ([IO.Path]::GetTempPath()) "rgo-download-$([guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $scratch | Out-Null
    try {
        $zipPath = Join-Path $scratch $asset
        if ($Archive) {
            Assert-Condition ((Get-Item -LiteralPath $Archive).Length -le 209715200) 'release archive exceeds 200 MiB'
            Copy-Item -LiteralPath $Archive -Destination $zipPath
        } else {
            Download-Https "https://github.com/$Repository/releases/download/$ReleaseTag/$asset" $zipPath
        }
        $manifest = if ($Sha256Sums) { Get-Content -LiteralPath $Sha256Sums -Raw }
            elseif ($Sha256) { $null }
            elseif (-not $Archive) {
                $manifestPath = Join-Path $scratch 'SHA256SUMS.txt'
                Download-Https "https://github.com/$Repository/releases/download/$ReleaseTag/SHA256SUMS.txt" $manifestPath
                Get-Content -LiteralPath $manifestPath -Raw
            } else { throw 'a local archive requires -Sha256Sums or -Sha256' }
        $expected = if ($Sha256) { $Sha256.ToLowerInvariant() } else { Expected-Digest $manifest $asset }
        $digest = File-Digest $zipPath
        Assert-Condition ($digest -eq $expected) "archive SHA-256 mismatch: expected $expected, got $digest"
        if (-not $DevelopmentBundle) {
            $gh = Resolve-AttestationVerifier $scratch
            Invoke-Checked $gh @('attestation', 'verify', $zipPath, '--repo', $Repository,
                '--signer-workflow', "$Repository/.github/workflows/release.yml", '--source-ref', "refs/tags/$ReleaseTag") | Out-Null
        }
        $extracted = Join-Path $scratch 'extracted'
        New-Item -ItemType Directory -Path $extracted | Out-Null
        Extract-VerifiedZip $zipPath $extracted $top
        $candidateCli = Join-Path $extracted 'rgo.exe'
        $candidateWrapper = Join-Path $extracted 'rgo-rustc-wrapper.exe'
        $cliDigest = File-Digest $candidateCli
        $wrapperDigest = File-Digest $candidateWrapper
        $binaryVersion = $ReleaseTag.Substring(1)
        Assert-Condition ((Invoke-Checked $candidateCli @('--version')) -eq "rgo $binaryVersion") 'archive CLI has the wrong version'
        Assert-Condition ((Invoke-Checked $candidateWrapper @('--rgo-version')) -match "^rgo-rustc-wrapper $([regex]::Escape($binaryVersion)) protocol [0-9]+$") 'archive wrapper has the wrong version or protocol banner'
        if ($VerifyOnly) { Write-Host "Verified $asset ($digest)"; return }

        New-Item -ItemType Directory -Force -Path @($versions, $resolvedBinDir) | Out-Null
        Assert-PlainDirectory $versions
        Assert-PlainDirectory $resolvedBinDir
        $versionDir = Join-Path $versions $top
        $cli = Join-Path $versionDir 'rgo.exe'
        $wrapper = Join-Path $versionDir 'rgo-rustc-wrapper.exe'
        $commandCli = Join-Path $resolvedBinDir 'rgo.exe'
        $commandWrapper = Join-Path $resolvedBinDir 'rgo-rustc-wrapper.exe'
        if (Test-Path -LiteralPath $statePath) {
            $state = Read-Json $statePath
            Select-OwnedShim $state
            Assert-Condition ((Test-SamePath $state.cargoHome $resolvedCargoHome) -and
                (Test-SamePath $state.installRoot $resolvedInstallRoot) -and
                (Test-SamePath $state.binDir $resolvedBinDir) -and
                (Test-SamePath $state.rgoHome $resolvedRgoHome)) 'installer state belongs to another destination'
            Assert-Condition ($state.schemaVersion -eq 1) 'unsupported installer state version'
            $installedSupervised = Test-SupervisedState $state
            if ($PSBoundParameters.ContainsKey('Supervised')) {
                Assert-Condition ([bool]$Supervised -eq $installedSupervised) 'installation mode differs from the owned installer state'
            }
            if ($installedSupervised) {
                Assert-Condition (-not $NoWrapper) 'supervised Windows installation cannot use -NoWrapper'
                if ($RealCargo) {
                    Assert-Condition (Test-SamePath (Full-Path $RealCargo) $state.realCargo) '-RealCargo differs from the owned installation'
                }
            }
            if ($state.versionDirectory -ne $top) {
                if ($installedSupervised) {
                    Invoke-SupervisedUpgrade $state $top $digest $cliDigest $wrapperDigest $extracted
                } else {
                    $oldNoWrapper = $state.PSObject.Properties['noWrapper'] -and $state.noWrapper
                    $nextNoWrapper = if ($PSBoundParameters.ContainsKey('NoWrapper')) { [bool]$NoWrapper }
                        else { [bool]$oldNoWrapper }
                    Invoke-NoServiceUpgrade $state $top $digest $cliDigest $wrapperDigest $extracted $nextNoWrapper
                }
                return
            }
            Assert-Condition ($state.versionDirectory -eq $top -and $state.archiveDigest -eq $digest -and
                $state.cliDigest -eq $cliDigest -and $state.wrapperDigest -eq $wrapperDigest) 'this release tag has different bytes from the installed archive; refusing a repack'
            Assert-OwnedUserPath $state
            Assert-Record $state $cli $wrapper
            $pointerPath = Join-Path $resolvedCargoHome '.rgo-home'
            $pointerBefore = Encoded-File $pointerPath
            $utf8 = [System.Text.UTF8Encoding]::new($false, $true)
            $pointerExpected = [Convert]::ToBase64String($utf8.GetBytes("$resolvedRgoHome`n"))
            $pointerNeedsRepair = -not (Test-ExactText $pointerBefore $pointerExpected)
            if ($pointerNeedsRepair) {
                Assert-Condition $Repair 'Cargo storage pointer is missing or changed; rerun with -Repair and the same verified bundle'
                if ($null -ne $pointerBefore) {
                    try {
                        $pointerRoot = $utf8.GetString([Convert]::FromBase64String($pointerBefore)).TrimEnd([char[]]"`r`n")
                        if ([IO.Path]::IsPathRooted($pointerRoot)) {
                            Assert-Condition (Test-SamePath $pointerRoot $resolvedRgoHome) 'repair refuses a pointer to a different absolute storage root'
                        }
                    } catch [System.Text.DecoderFallbackException] { }
                }
                Assert-PlainDirectory $resolvedRgoHome
                Assert-PlainDirectory (Join-Path $resolvedRgoHome 'state')
                $ownerPath = Join-Path $resolvedRgoHome 'state/owner-cargo-home'
                $modePath = Join-Path $resolvedRgoHome 'state/storage-mode'
                $modeText = if ($installedSupervised) { "supervised`n" } else { "native`n" }
                $ownerBefore = Encoded-File $ownerPath
                $modeBefore = Encoded-File $modePath
                Assert-Condition ((Test-ExactText $ownerBefore ([Convert]::ToBase64String($utf8.GetBytes("$resolvedCargoHome`n")))) -and
                    (Test-ExactText $modeBefore ([Convert]::ToBase64String($utf8.GetBytes($modeText))))) 'repair requires the recorded Cargo-home owner and storage mode'
                $repairGuards = @(
                    @{ path = $statePath; before = (Encoded-File $statePath) },
                    @{ path = (Join-Path $resolvedCargoHome '.rgo-install.json'); before = (Encoded-File (Join-Path $resolvedCargoHome '.rgo-install.json')) },
                    @{ path = $ownerPath; before = $ownerBefore },
                    @{ path = $modePath; before = $modeBefore }
                )
            }
            if (-not (Test-Path -LiteralPath $versionDir)) {
                Assert-Condition $Repair "owned version directory is missing: $versionDir"
                New-Item -ItemType Directory -Path $versionDir | Out-Null
            }
            Assert-PlainDirectory $versionDir
            foreach ($pair in @(@($cli, $cliDigest), @($wrapper, $wrapperDigest), @($commandCli, $cliDigest), @($commandWrapper, $wrapperDigest))) {
                if (Test-Path -LiteralPath $pair[0]) { Assert-PlainFile $pair[0] }
                if (-not (Test-Path -LiteralPath $pair[0]) -or (File-Digest $pair[0]) -ne $pair[1]) {
                    Assert-Condition $Repair "owned binary is missing or changed: $($pair[0]); rerun with -Repair and the same verified bundle"
                    if (Test-Path -LiteralPath $pair[0]) { throw "repair refuses to replace an in-use or modified executable: $($pair[0])" }
                    $source = if ($pair[0] -like '*rgo-rustc-wrapper.exe') { $candidateWrapper } else { $candidateCli }
                    Copy-Item -LiteralPath $source -Destination $pair[0]
                }
            }
            if ($pointerNeedsRepair) {
                Invoke-WithSetupLocks {
                    foreach ($guard in $repairGuards) {
                        Assert-Condition (Test-ExactText (Encoded-File $guard.path) $guard.before) "activation changed during repair: $($guard.path)"
                    }
                    Assert-Condition (Test-ExactText (Encoded-File $pointerPath) $pointerBefore) 'Cargo storage pointer changed during repair'
                    Write-EncodedFile $pointerPath $pointerExpected
                }
            }
            try {
                $shimNeedsRepair = $false
                if ($installedSupervised) {
                    $fallbackPath = Join-Path $shimDir '.rgo-cargo-fallback.json'
                    if (-not (Test-Path -LiteralPath $shimPath) -or -not (Test-Path -LiteralPath $fallbackPath)) {
                        Assert-Condition $Repair 'owned Cargo shim or fallback is missing; rerun with -Repair and the same verified bundle'
                        $shimNeedsRepair = $true
                    }
                }
                if ($shimNeedsRepair -or ($Repair -and -not $state.noService)) {
                    $repairSetupArgs = @('setup')
                    if ($state.noService) { $repairSetupArgs += '--no-service' }
                    if ($installedSupervised) { $repairSetupArgs += @('--supervised', '--real-cargo', $state.realCargo) }
                    Invoke-Checked $cli $repairSetupArgs | Out-Null
                }
                if ($installedSupervised) {
                    Assert-PlainFile $shimPath
                    Assert-Condition ((File-Digest $shimPath) -eq $cliDigest) 'owned Cargo shim differs from the verified release'
                    Assert-PlainFile $fallbackPath
                    $fallback = Read-Json $fallbackPath
                    Assert-Condition ($fallback.schema_version -eq 1 -and
                        (Test-SamePath $fallback.cargo_home $resolvedCargoHome) -and
                        (Test-SamePath $fallback.real_cargo $state.realCargo)) 'owned Cargo fallback differs from the installer state'
                }
                Assert-PlainCargoActivation $cli $state
                Assert-ServiceHealthy $cli $state
            }
            catch {
                $verificationError = $_
                if ($pointerNeedsRepair) {
                    try {
                        Invoke-WithSetupLocks {
                            foreach ($guard in $repairGuards) {
                                Assert-Condition (Test-ExactText (Encoded-File $guard.path) $guard.before) "activation changed during pointer rollback: $($guard.path)"
                            }
                            Assert-Condition (Test-ExactText (Encoded-File $pointerPath) $pointerExpected) 'Cargo storage pointer changed during pointer rollback'
                            Write-EncodedFile $pointerPath $pointerBefore
                        }
                    } catch { throw "repair verification failed ($verificationError); pointer rollback failed ($_)" }
                }
                throw $verificationError
            }
            if (Test-Path -LiteralPath $pendingPath) {
                $pending = Read-Json $pendingPath
                Assert-Condition ($pending.archiveDigest -eq $digest -and $pending.versionDirectory -eq $top) 'stale pending activation conflicts with installed state'
                Remove-Item -LiteralPath $pendingPath -Force
            }
            Write-Host "Verified active rgo $binaryVersion at $versionDir"
            return
        }

        if ($Supervised) {
            $shimDir = Join-Path $shimRoot $ReleaseTag
            $shimPath = Join-Path $shimDir 'cargo.exe'
        }
        $rawUserPath = if ($Supervised -and -not $NoUserPath) { Get-UserPathSnapshot } else { $null }
        $priorUserPathRaw = if ($rawUserPath) { $rawUserPath.value } else { $null }
        $priorUserPathKind = if ($rawUserPath) { $rawUserPath.kind } else { $null }
        $stateShimPath = if ($Supervised) { $shimPath } else { $null }
        $state = [ordered]@{
            schemaVersion = 1; cargoHome = $resolvedCargoHome; rgoHome = $resolvedRgoHome
            installRoot = $resolvedInstallRoot; binDir = $resolvedBinDir
            versionDirectory = $top; archiveDigest = $digest
            cliDigest = $cliDigest; wrapperDigest = $wrapperDigest
            noService = [bool]$NoService; noWrapper = [bool]$NoWrapper; noUserPath = [bool]$NoUserPath
            supervised = [bool]$Supervised; realCargo = $null
            shimPath = $stateShimPath
            pathAdded = $false; priorUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            priorUserPathPresent = [bool]($rawUserPath -and $rawUserPath.present)
            priorUserPathRaw = $priorUserPathRaw; priorUserPathKind = $priorUserPathKind
        }
        $resuming = Test-Path -LiteralPath $pendingPath
        if ($resuming) {
            $pending = Read-Json $pendingPath
            Assert-Condition ($pending.archiveDigest -eq $digest -and $pending.versionDirectory -eq $top -and
                $pending.cliDigest -eq $cliDigest -and $pending.wrapperDigest -eq $wrapperDigest -and
                (Test-SamePath $pending.cargoHome $resolvedCargoHome) -and
                (Test-SamePath $pending.rgoHome $resolvedRgoHome) -and
                (Test-SamePath $pending.binDir $resolvedBinDir)) 'pending activation requires its original verified bundle and destinations'
            if (-not $pending.PSObject.Properties['noWrapper']) {
                $pending | Add-Member -NotePropertyName noWrapper -NotePropertyValue $false
            }
            if ($PSBoundParameters.ContainsKey('Supervised')) {
                Assert-Condition ([bool]$Supervised -eq (Test-SupervisedState $pending)) 'pending activation has a different supervised mode'
            }
            if ((Test-SupervisedState $pending) -and $RealCargo) {
                Assert-Condition (Test-SamePath (Full-Path $RealCargo) $pending.realCargo) '-RealCargo differs from the pending activation'
            }
            $state = $pending
            Select-OwnedShim $state
        } else {
            if (Test-SupervisedState $state) { $state.realCargo = Resolve-RealCargo $RealCargo }
            Assert-Condition (-not (Test-Path -LiteralPath (Join-Path $resolvedCargoHome '.rgo-install.json'))) 'Cargo is already activated outside this installer'
            Assert-Condition (-not (Test-Path -LiteralPath $commandCli) -and -not (Test-Path -LiteralPath $commandWrapper)) 'rgo command entrypoint already exists outside this installer'
            $priorCommand = Get-Command rgo -ErrorAction SilentlyContinue
            Assert-Condition (-not $priorCommand) 'another rgo command already resolves on PATH; remove that conflict before activation'
            $userEntries = @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';' | Where-Object { $_ })
            $state.pathAdded = -not $NoUserPath -and @($userEntries | Where-Object { Test-SamePath $_ $resolvedBinDir }).Count -eq 0
            if (Test-SupervisedState $state) {
                Assert-Condition (@($userEntries | Where-Object { Test-SamePath $_ $shimDir }).Count -eq 0) 'Cargo shim directory is already on User PATH without installer ownership'
            }
            Write-JsonAtomic $pendingPath $state
        }
        Stage-VerifiedVersion $extracted $versionDir $cliDigest $wrapperDigest
        if (Test-Path -LiteralPath (Join-Path $resolvedCargoHome '.rgo-install.json')) { Assert-Record $state $cli $wrapper }
        $setupArgs = @('setup')
        if ($state.noService) { $setupArgs += '--no-service' }
        if ($state.noWrapper) { $setupArgs += '--no-wrapper' }
        if (Test-SupervisedState $state) { $setupArgs += @('--supervised', '--real-cargo', $state.realCargo) }
        Invoke-Checked $cli $setupArgs | Out-Null
        Assert-Record $state $cli $wrapper
        if (Test-SupervisedState $state) {
            Assert-PlainFile $shimPath
            Assert-Condition ((File-Digest $shimPath) -eq $cliDigest) 'owned Cargo shim differs from the verified release'
            $fallbackPath = Join-Path $shimDir '.rgo-cargo-fallback.json'
            Assert-PlainFile $fallbackPath
            $fallback = Read-Json $fallbackPath
            Assert-Condition ($fallback.schema_version -eq 1 -and
                (Test-SamePath $fallback.cargo_home $resolvedCargoHome) -and
                (Test-SamePath $fallback.real_cargo $state.realCargo)) 'owned Cargo fallback differs from the installer state'
        }
        Assert-PlainCargoActivation $cli $state
        Assert-ServiceHealthy $cli $state
        foreach ($pair in @(@($commandWrapper, $wrapper, $wrapperDigest), @($commandCli, $cli, $cliDigest))) {
            if (Test-Path -LiteralPath $pair[0]) {
                Assert-PlainFile $pair[0]
                Assert-Condition ((File-Digest $pair[0]) -eq $pair[2]) "command entrypoint changed: $($pair[0])"
            } else { Copy-Item -LiteralPath $pair[1] -Destination $pair[0] }
        }
        Publish-OwnedPath $state
        Write-JsonAtomic $statePath $state
        Remove-Item -LiteralPath $pendingPath -Force
        Write-Host "Activated rgo $binaryVersion. Open a new shell and use cargo normally."
        if (-not $state.noService) { Write-Host 'Background service was verified during setup; automatic destructive GC remains disabled by default.' }
    } finally {
        Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
    }
} catch {
    if (Test-Path -LiteralPath $pendingPath) {
        Write-Warning 'Activation is pending. Retry with the same verified bundle and destinations, or run -Uninstall to undo owned changes.'
    }
    throw
} finally {
    if ($lock) { $lock.Dispose() }
    if ($null -eq $previousCargoHome) { Remove-Item Env:CARGO_HOME -ErrorAction SilentlyContinue }
    else { $env:CARGO_HOME = $previousCargoHome }
    if ($null -eq $previousRgoHome) { Remove-Item Env:RGO_HOME -ErrorAction SilentlyContinue }
    else { $env:RGO_HOME = $previousRgoHome }
}
