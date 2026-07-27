#Requires -Version 5.1
<#
.SYNOPSIS
    Downloads pinned ONNX model weights into .\models, verifying SHA-256.

.DESCRIPTION
    Model weights are deliberately not tracked in git (100-350MB each, and git
    keeps every revision forever). They are published once as GitHub Release
    assets and pulled down here at build time, both locally and in CI.

    Idempotent: a file already present with the correct hash is skipped, so
    this is cheap to run on every build.

.PARAMETER Force
    Re-download even if a valid local copy exists.
#>
[CmdletBinding()]
param(
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot  = Split-Path -Parent $PSScriptRoot
$modelDir  = Join-Path $repoRoot 'models'
$lockFile  = Join-Path $PSScriptRoot 'models.lock.json'

if (-not (Test-Path $lockFile)) {
    throw "Missing lockfile: $lockFile"
}

if (-not (Test-Path $modelDir)) {
    New-Item -ItemType Directory -Path $modelDir | Out-Null
}

$lock = Get-Content $lockFile -Raw | ConvertFrom-Json

# TLS 1.2 for Windows PowerShell 5.1, whose default is still TLS 1.0.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$placeholders = @()

foreach ($model in $lock.models) {
    $dest = Join-Path $modelDir $model.file

    if ($model.sha256 -eq 'REPLACE_ME') {
        $placeholders += $model.name
        continue
    }

    $expected = $model.sha256.ToLower()

    if ((Test-Path $dest) -and -not $Force) {
        $actual = (Get-FileHash $dest -Algorithm SHA256).Hash.ToLower()
        if ($actual -eq $expected) {
            Write-Host "[ok]   $($model.file) (cached, hash verified)"
            continue
        }
        Write-Warning "$($model.file) hash mismatch, re-downloading."
    }

    Write-Host "[get]  $($model.file) <- $($model.url)"

    # Download to a temp path so an interrupted transfer never leaves a
    # truncated file that looks valid to the next run.
    $tmp = "$dest.partial"
    try {
        # -UseBasicParsing and a null ProgressPreference: the default progress
        # bar makes Invoke-WebRequest roughly 10x slower on large files in 5.1.
        $prev = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $model.url -OutFile $tmp -UseBasicParsing
        $ProgressPreference = $prev
    }
    catch {
        if (Test-Path $tmp) { Remove-Item $tmp -Force }
        throw "Download failed for $($model.file): $_"
    }

    $actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        Remove-Item $tmp -Force
        throw "SHA-256 mismatch for $($model.file).`n  expected: $expected`n  actual:   $actual"
    }

    Move-Item $tmp $dest -Force
    Write-Host "[done] $($model.file) verified"
}

if ($placeholders.Count -gt 0) {
    Write-Host ''
    Write-Warning @"
Skipped $($placeholders.Count) model(s) still set to REPLACE_ME in scripts/models.lock.json:
  $($placeholders -join ', ')

Publish the .onnx files as assets on a GitHub Release, then fill in the real
url + sha256 for each. Get a digest with:
  (Get-FileHash .\models\<file> -Algorithm SHA256).Hash.ToLower()
"@
}

Write-Host ''
Write-Host "Models directory: $modelDir"
