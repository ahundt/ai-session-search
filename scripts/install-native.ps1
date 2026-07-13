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
if (-not [System.IO.File]::Exists($sourceBinary)) {
    throw "Archive-local executable is missing: $sourceBinary"
}
[System.IO.Directory]::CreateDirectory($BinDir) | Out-Null
$destination = Join-Path $BinDir 'aise.exe'

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

$stage = Join-Path $BinDir ('.aise.install.' + [System.IO.Path]::GetRandomFileName())
$rollbackLink = $null -ne $destinationItem -and $destinationIsLink
try {
    Copy-NewFile $sourceBinary $stage
    if ($null -ne $destinationItem) {
        $backupParent = Split-Path -Parent $Backup
        if ($backupParent) { [System.IO.Directory]::CreateDirectory($backupParent) | Out-Null }
        if ($destinationIsLink) {
            Move-Item -LiteralPath $destination -Destination $Backup
        } else {
            Copy-NewFile $destination $Backup
        }
        [System.IO.File]::Move($stage, $destination, $true)
        $rollbackLink = $false
    } else {
        [System.IO.File]::Move($stage, $destination)
    }
} finally {
    $rollback = Get-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue
    if ($rollbackLink -and $null -ne $rollback) {
        $published = Get-Item -LiteralPath $destination -Force -ErrorAction SilentlyContinue
        if ($null -ne $published) { Remove-Item -LiteralPath $destination -Force }
        Move-Item -LiteralPath $Backup -Destination $destination
    }
    if ([System.IO.File]::Exists($stage)) { [System.IO.File]::Delete($stage) }
}

Write-Output "installed aise: $destination"
