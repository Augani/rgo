param(
    [Parameter(Mandatory = $true)][string]$Stage,
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string]$Archive
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression

# ZipArchive entry names use forward slashes on every host. Compress-Archive
# writes backslashes on Windows, which the installer rejects as unsafe paths.
$stream = [IO.File]::Open($Archive, [IO.FileMode]::CreateNew)
try {
    $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $false)
    try {
        foreach ($filename in @('rgo.exe', 'rgo-rustc-wrapper.exe', 'README.md', 'doc.md', 'LICENSE-MIT', 'LICENSE-APACHE')) {
            $source = [IO.File]::OpenRead((Join-Path $Stage $filename))
            try {
                $entry = $zip.CreateEntry("$Name/$filename", [IO.Compression.CompressionLevel]::Optimal)
                $destination = $entry.Open()
                try {
                    $source.CopyTo($destination)
                } finally {
                    $destination.Dispose()
                }
            } finally {
                $source.Dispose()
            }
        }
    } finally {
        $zip.Dispose()
    }
} finally {
    $stream.Dispose()
}
