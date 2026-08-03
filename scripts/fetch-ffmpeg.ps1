#Requires -Version 5.1
<#
.SYNOPSIS
    Downloads the LGPL ffmpeg build that gets bundled into the installer.

.DESCRIPTION
    ~129MB of binaries, deliberately not tracked in git -- the same reasoning as
    the ONNX weights: git keeps every revision forever.

    The build is LGPL, NOT GPL. That distinction is load-bearing: bundling a GPL
    ffmpeg would put this whole application under the GPL. LGPL permits
    distribution alongside non-GPL software provided the licence notice travels
    with it, which is why FFMPEG-LICENSE.txt is copied in too.

    ffplay.exe is dropped (a media player we never invoke). avdevice must be
    kept even though we use no capture devices -- ffmpeg.exe and ffprobe.exe
    link it and refuse to start without it.

.PARAMETER Force
    Re-download even if vendor\ffmpeg already looks complete.
#>
[CmdletBinding()]
param([switch]$Force)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
$vendor   = Join-Path $repoRoot 'vendor'
$target   = Join-Path $vendor 'ffmpeg'
$zipPath  = Join-Path $vendor 'ffmpeg-lgpl.zip'
$url      = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-lgpl-shared.zip'

$required = @(
    'ffmpeg.exe', 'ffprobe.exe',
    'avcodec-63.dll', 'avdevice-63.dll', 'avfilter-12.dll', 'avformat-63.dll',
    'avutil-61.dll', 'swresample-7.dll', 'swscale-10.dll'
)

if (-not $Force -and (Test-Path $target)) {
    $missing = $required | Where-Object { -not (Test-Path (Join-Path $target $_)) }
    if ($missing.Count -eq 0) {
        Write-Host "[ok] vendor\ffmpeg already complete ($($required.Count) files)"
        exit 0
    }
    Write-Warning "vendor\ffmpeg is incomplete (missing: $($missing -join ', ')); re-fetching."
}

New-Item -ItemType Directory -Force -Path $vendor | Out-Null
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

if ($Force -or -not (Test-Path $zipPath)) {
    Write-Host "[get] $url"
    $prev = $ProgressPreference
    # The default progress bar makes Invoke-WebRequest ~10x slower on 5.1.
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing
    $ProgressPreference = $prev
}

Write-Host "[extract] -> $target"
if (Test-Path $target) { Remove-Item $target -Recurse -Force }
New-Item -ItemType Directory -Force -Path $target | Out-Null

Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead($zipPath)
try {
    foreach ($entry in $zip.Entries) {
        $name = $entry.Name
        if (-not $name) { continue }
        $keep = ($required -contains $name) -or ($name -eq 'LICENSE.txt')
        if (-not $keep) { continue }
        $outName = if ($name -eq 'LICENSE.txt') { 'FFMPEG-LICENSE.txt' } else { $name }
        [IO.Compression.ZipFileExtensions]::ExtractToFile(
            $entry, (Join-Path $target $outName), $true)
    }
} finally {
    $zip.Dispose()
}

$missing = $required | Where-Object { -not (Test-Path (Join-Path $target $_)) }
if ($missing.Count -gt 0) {
    throw "extraction incomplete, missing: $($missing -join ', ')"
}

# Prove the binaries actually start rather than trusting the file list; a
# missing DLL shows up as a silent exit 127, not an extraction error.
foreach ($tool in @('ffmpeg', 'ffprobe')) {
    $exe = Join-Path $target "$tool.exe"
    $line = (& $exe -version 2>&1 | Select-Object -First 1)
    if (-not $line) { throw "$tool.exe did not run (missing DLL?)" }
    Write-Host "[ok] $line"
}

$mb = [math]::Round((Get-ChildItem $target -File | Measure-Object -Sum Length).Sum / 1MB, 1)
Write-Host "`nvendor\ffmpeg ready: $mb MB"
Remove-Item $zipPath -Force
