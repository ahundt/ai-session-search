# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

[CmdletBinding()]
param(
    [string]$BinDir,
    [switch]$Replace,
    [string]$Backup
)

$ErrorActionPreference = 'Stop'

if (-not $BinDir) {
    if ($env:AI_SESSION_SEARCH_BIN_DIR) {
        $BinDir = $env:AI_SESSION_SEARCH_BIN_DIR
    } elseif ($env:LOCALAPPDATA) {
        $BinDir = Join-Path $env:LOCALAPPDATA 'Programs\ai-session-search\bin'
    } else {
        throw 'Set -BinDir, AI_SESSION_SEARCH_BIN_DIR, or LOCALAPPDATA.'
    }
}
if ($Replace -and -not $Backup) {
    throw '-Replace requires an explicit -Backup path.'
}
if (-not $Replace -and $Backup) {
    throw '-Backup is valid only with -Replace.'
}

function Copy-NewFile {
    param([string]$Source, [string]$Destination)

    $inputStream = $null
    $outputStream = $null
    $created = $false
    try {
        $inputStream = [System.IO.File]::OpenRead($Source)
        $outputStream = [System.IO.FileStream]::new(
            $Destination,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None
        )
        $created = $true
        $inputStream.CopyTo($outputStream)
        $outputStream.Flush($true)
    } catch {
        if ($outputStream) { $outputStream.Dispose() }
        if ($inputStream) { $inputStream.Dispose() }
        if ($created) { [System.IO.File]::Delete($Destination) }
        throw
    } finally {
        if ($outputStream) { $outputStream.Dispose() }
        if ($inputStream) { $inputStream.Dispose() }
    }
}

$sourceBinary = Join-Path $PSScriptRoot 'aise.exe'
$sourceReceipt = Join-Path $PSScriptRoot 'aise-native-install.json'
if (-not [System.IO.File]::Exists($sourceBinary)) {
    throw "Archive-local executable is missing: $sourceBinary"
}
if (-not [System.IO.File]::Exists($sourceReceipt)) {
    throw "Archive-local install receipt is missing: $sourceReceipt"
}
[System.IO.Directory]::CreateDirectory($BinDir) | Out-Null
$destination = Join-Path $BinDir 'aise.exe'
$receiptDestination = Join-Path $BinDir 'aise-native-install.json'

$destinationItem = Get-Item -LiteralPath $destination -Force -ErrorAction SilentlyContinue
if ($null -ne $destinationItem) {
    $destinationIsLink = ($destinationItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0
    if ($destinationItem.PSIsContainer -and -not $destinationIsLink) {
        throw "Destination is not a regular file: $destination"
    }
    if (-not $Replace) {
        throw "Destination already exists: $destination"
    }
}
$backupItem = Get-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue
if ($Replace -and $null -ne $backupItem) {
    throw "Rollback backup already exists: $Backup"
}
$receiptItem = Get-Item -LiteralPath $receiptDestination -Force -ErrorAction SilentlyContinue
if ($null -ne $receiptItem) {
    $receiptIsLink = ($receiptItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0
    if ($receiptItem.PSIsContainer -or $receiptIsLink) {
        throw "Native install receipt destination is not a regular file: $receiptDestination"
    }
    if (-not $Replace) {
        throw "Native install receipt already exists: $receiptDestination"
    }
}

$stage = Join-Path $BinDir ('.aise.install.' + [System.IO.Path]::GetRandomFileName())
$receiptStage = Join-Path $BinDir ('.aise.receipt.' + [System.IO.Path]::GetRandomFileName())
$rollbackLink = $null -ne $destinationItem -and $destinationIsLink
$publishedBinary = $false
$completed = $false
try {
    Copy-NewFile $sourceBinary $stage
    Copy-NewFile $sourceReceipt $receiptStage
    if ($null -ne $destinationItem) {
        $backupParent = Split-Path -Parent $Backup
        if ($backupParent) { [System.IO.Directory]::CreateDirectory($backupParent) | Out-Null }
        if ($destinationIsLink) {
            Move-Item -LiteralPath $destination -Destination $Backup
        } else {
            Copy-NewFile $destination $Backup
        }
        [System.IO.File]::Move($stage, $destination, $true)
        $publishedBinary = $true
        $rollbackLink = $false
    } else {
        [System.IO.File]::Move($stage, $destination)
        $publishedBinary = $true
    }
    [System.IO.File]::Move($receiptStage, $receiptDestination, $true)
    $completed = $true
} finally {
    $rollback = Get-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue
    if (-not $completed -and $publishedBinary) {
        $published = Get-Item -LiteralPath $destination -Force -ErrorAction SilentlyContinue
        if ($null -ne $published) { Remove-Item -LiteralPath $destination -Force }
        if ($null -ne $destinationItem -and $null -ne $rollback) {
            Move-Item -LiteralPath $Backup -Destination $destination
        }
    } elseif ($rollbackLink -and $null -ne $rollback) {
        $published = Get-Item -LiteralPath $destination -Force -ErrorAction SilentlyContinue
        if ($null -ne $published) { Remove-Item -LiteralPath $destination -Force }
        Move-Item -LiteralPath $Backup -Destination $destination
    }
    if ([System.IO.File]::Exists($stage)) { [System.IO.File]::Delete($stage) }
    if ([System.IO.File]::Exists($receiptStage)) { [System.IO.File]::Delete($receiptStage) }
}

Write-Output "installed aise: $destination"
