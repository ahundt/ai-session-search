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
    if (($destinationItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Refusing symbolic-link destination: $destination"
    }
    if ($destinationItem.PSIsContainer) {
        throw "Destination is not a regular file: $destination"
    }
}

$stage = Join-Path $BinDir ('.aise.install.' + [System.IO.Path]::GetRandomFileName())
try {
    Copy-NewFile $sourceBinary $stage
    if ([System.IO.File]::Exists($destination)) {
        if (-not $Replace) {
            throw "Destination already exists: $destination"
        }
        $backupParent = Split-Path -Parent $Backup
        if ($backupParent) { [System.IO.Directory]::CreateDirectory($backupParent) | Out-Null }
        Copy-NewFile $destination $Backup
        [System.IO.File]::Move($stage, $destination, $true)
    } else {
        [System.IO.File]::Move($stage, $destination)
    }
} finally {
    if ([System.IO.File]::Exists($stage)) { [System.IO.File]::Delete($stage) }
}

Write-Output "installed aise: $destination"
