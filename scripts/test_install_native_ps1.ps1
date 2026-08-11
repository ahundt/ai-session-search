# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

<#
.SYNOPSIS
Exercise scripts/install-native.ps1 against a synthetic archive bundle.

.DESCRIPTION
Nothing ran this installer before a tag push, so two defects shipped in it:

  * -Backup defaults to an empty string, and binding one to -LiteralPath is a
    parameter binding failure that -ErrorAction SilentlyContinue cannot
    suppress, so every plain -BinDir install died before copying anything.
  * File.Replace was passed $null for destinationBackupFileName, which
    PowerShell converts to an empty string, so every -Replace upgrade failed
    with "The value cannot be an empty string. (Parameter 'path')".

The installer is pure .NET file IO, so this runs on any platform PowerShell
runs on rather than only on Windows.
#>

[CmdletBinding()]
param(
    [string]$Installer = (Join-Path $PSScriptRoot 'install-native.ps1')
)

$ErrorActionPreference = 'Stop'

$failures = @()

function New-Bundle {
    param([string]$Root, [string]$Marker)

    $bundle = Join-Path $Root 'bundle'
    New-Item -ItemType Directory -Path $bundle -Force | Out-Null
    Copy-Item -LiteralPath $Installer -Destination (Join-Path $bundle 'install.ps1')
    Set-Content -LiteralPath (Join-Path $bundle 'aise.exe') -Value $Marker -NoNewline
    Set-Content -LiteralPath (Join-Path $bundle 'aise-native-install.json') `
        -Value '{"version":"test"}' -NoNewline
    return $bundle
}

function Invoke-Installer {
    # Splat a hashtable, not an array: array splatting binds positionally, so
    # @('-BinDir', $path) sets $BinDir to the literal string '-BinDir'.
    param([string]$Bundle, [hashtable]$Arguments)

    try {
        $output = & (Join-Path $Bundle 'install.ps1') @Arguments 2>&1
        return [pscustomobject]@{ Succeeded = $true; Output = ($output | Out-String) }
    } catch {
        # The installer sets $ErrorActionPreference = 'Stop' and throws, so a
        # refusal arrives here rather than as a non-zero exit code.
        return [pscustomobject]@{ Succeeded = $false; Output = $_.Exception.Message }
    }
}

function Assert {
    param([string]$Name, [bool]$Condition, [string]$Detail = '')

    if ($Condition) {
        Write-Host "  ok   $Name"
    } else {
        Write-Host "  FAIL $Name$(if ($Detail) { ": $Detail" })"
        $script:failures += $Name
    }
}

$root = Join-Path ([System.IO.Path]::GetTempPath()) ("aise-install-test-" + [System.IO.Path]::GetRandomFileName())
try {
    # A fresh install must not require -Replace or -Backup.
    $case = Join-Path $root 'fresh'
    $bundle = New-Bundle -Root $case -Marker 'first'
    $bin = Join-Path $case 'bin'
    $result = Invoke-Installer -Bundle $bundle -Arguments @{ BinDir = $bin }
    Assert 'fresh install succeeds' $result.Succeeded $result.Output
    Assert 'fresh install writes the executable' `
        ([System.IO.File]::Exists((Join-Path $bin 'aise.exe')))
    Assert 'fresh install writes the receipt' `
        ([System.IO.File]::Exists((Join-Path $bin 'aise-native-install.json')))

    # Re-installing over an existing file without -Replace must refuse.
    $result = Invoke-Installer -Bundle $bundle -Arguments @{ BinDir = $bin }
    Assert 'reinstall without -Replace refuses' (-not $result.Succeeded)
    Assert 'reinstall refusal names the destination' `
        ($result.Output -match 'Destination already exists') $result.Output

    # -Replace must upgrade in place and leave the previous build at -Backup.
    $upgrade = New-Bundle -Root (Join-Path $root 'upgrade') -Marker 'second'
    $backup = Join-Path $case 'backup/aise.exe.bak'
    $result = Invoke-Installer -Bundle $upgrade -Arguments @{ BinDir = $bin; Replace = $true; Backup = $backup }
    Assert '-Replace upgrade succeeds' $result.Succeeded $result.Output
    Assert '-Replace installs the new build' `
        ((Get-Content -LiteralPath (Join-Path $bin 'aise.exe') -Raw) -eq 'second')
    Assert '-Replace keeps the old build at -Backup' `
        ((Test-Path -LiteralPath $backup) -and ((Get-Content -LiteralPath $backup -Raw) -eq 'first'))

    # The two flags are only meaningful together.
    $result = Invoke-Installer -Bundle $upgrade -Arguments @{ BinDir = $bin; Replace = $true }
    Assert '-Replace without -Backup refuses' `
        ((-not $result.Succeeded) -and ($result.Output -match 'requires an explicit -Backup')) $result.Output

    $result = Invoke-Installer -Bundle $upgrade -Arguments @{ BinDir = $bin; Backup = (Join-Path $case 'unused.bak') }
    Assert '-Backup without -Replace refuses' `
        ((-not $result.Succeeded) -and ($result.Output -match 'valid only with -Replace')) $result.Output
} finally {
    if (Test-Path -LiteralPath $root) {
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($failures.Count -gt 0) {
    Write-Host ""
    Write-Host "$($failures.Count) installer assertion(s) failed"
    exit 1
}

Write-Host ""
Write-Host 'install-native.ps1: all assertions passed'
