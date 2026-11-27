# Fetches NVIDIA Streamline and DLSS for VICTORY_DLSS=1 (docs/dlss.md).
#   scripts\get-dlss.ps1            into %LOCALAPPDATA%\victory\dlss
#   scripts\get-dlss.ps1 -To <dir>  elsewhere (then set VICTORY_DLSS_DIR=<dir>)
# Downloads NVIDIA's public Streamline SDK release from GitHub (about 280 MB, once), checks its SHA-256, and keeps
# only the signed runtime files the layer loads, with their licence texts. Nothing of NVIDIA's is ever put into this
# repository: the folder is outside it, and the layer loads only files with a valid signature.
# By running this you accept NVIDIA's licences for these files (nvngx_dlss.license.txt and the SDK's license.txt).
param(
    [string]$To = (Join-Path $env:LOCALAPPDATA 'victory\dlss'),
    [switch]$Keep  # keep the downloaded zip next to the files
)
$ErrorActionPreference = 'Stop'
# The release the layer's declarations match (crates/victory-gpu/src/dlss.rs SDK_VERSION): Streamline 2.14.16,
# DLSS 310.9.16 (the transformer models: presets J, K, L and M).
$version = '2.14.16'
$url = "https://github.com/NVIDIA-RTX/Streamline/releases/download/v$version/streamline-sdk-v$version.zip"
$sha256 = '32041E51DB0CAB709A271B72A352281B690C2ABBD669A827809401A5DD23956B'
$wanted = @(
    'sl.interposer.dll', 'sl.common.dll', 'sl.dlss.dll', 'nvngx_dlss.dll',
    # For frame generation and Reflex later (docs/dlss.md, steps 3-4); not loaded yet.
    'sl.dlss_g.dll', 'nvngx_dlssg.dll', 'sl.reflex.dll', 'sl.pcl.dll',
    'nvngx_dlss.license.txt', 'reflex.license.txt'
)
$repo = Split-Path $PSScriptRoot -Parent
$full = [IO.Path]::GetFullPath($To)
if ($full.StartsWith([IO.Path]::GetFullPath($repo), [StringComparison]::OrdinalIgnoreCase)) {
    throw "$full is inside the repository; NVIDIA's files must stay outside it"
}
New-Item -ItemType Directory -Force $full | Out-Null
$zip = Join-Path $full "streamline-sdk-v$version.zip"
if (-not (Test-Path $zip) -or (Get-FileHash $zip -Algorithm SHA256).Hash -ne $sha256) {
    Write-Host "Downloading Streamline $version from NVIDIA's GitHub (about 280 MB)..."
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
}
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash
if ($hash -ne $sha256) {
    Remove-Item -LiteralPath $zip -Force
    throw "the download's SHA-256 is $hash, not ${sha256}: removed"
}
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead($zip)
try {
    foreach ($entry in $archive.Entries) {
        if ($entry.FullName -match '^bin/x64/([^/]+)$' -and $wanted -contains $Matches[1]) {
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $full $Matches[1]), $true)
        } elseif ($entry.FullName -eq 'license.txt') {
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $full 'streamline.license.txt'), $true)
        }
    }
} finally {
    $archive.Dispose()
}
foreach ($dll in $wanted | Where-Object { $_ -like '*.dll' }) {
    $status = (Get-AuthenticodeSignature (Join-Path $full $dll)).Status
    if ($status -ne 'Valid') { throw "${dll}: signature $status" }
}
if (-not $Keep) { Remove-Item -LiteralPath $zip -Force }
Write-Host "Streamline $version and DLSS are in $full. Start with VICTORY_DLSS=1 (docs/dlss.md)."
if ($full -ne [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'victory\dlss'))) {
    Write-Host "Set VICTORY_DLSS_DIR=$full so the layer finds them."
}
