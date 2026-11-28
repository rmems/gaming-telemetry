# SPDX-License-Identifier: MIT OR Apache-2.0
<#
.SYNOPSIS
    Install the one-time Windows prerequisites for CPU telemetry capture.

.DESCRIPTION
    The bundled LibreHardwareMonitor sidecar reads CPU sensors through the
    signed PawnIO kernel driver (https://pawnio.eu). PawnIO cannot be shipped
    inside this repo - it is a separately signed driver - so this script
    downloads the official installer and runs it silently.

    The installer is pinned to a fixed PawnIO release. After the script
    self-elevates, the download is staged in a fresh ProgramData directory
    whose DACL allows write access only to SYSTEM and Administrators.
    SHA-256 verification is required before the installer runs (a mismatch
    refuses to execute it). An Authenticode status other than Valid is a
    warning only, because the pinned hash already matched.

    The script self-elevates: PawnIO is a kernel driver and its installer
    requires administrator rights. On completion it prints the exact command
    to run the collector with an elevated token - in the terminal that
    launched the script, since the elevated window closes on exit.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/install-windows-deps.ps1
#>
[CmdletBinding()]
param(
    # Internal: set when the script relaunches itself in a new elevated window.
    [switch]$ElevatedChild
)

$ErrorActionPreference = 'Stop'

# Pinned release: https://github.com/namazso/PawnIO.Setup/releases/tag/2.2.0
# PawnIoSha256 is the SHA-256 of that release's PawnIO_setup.exe; update both
# together when moving to a newer PawnIO.
$PawnIoVersion = '2.2.0'
$PawnIoUrl = "https://github.com/namazso/PawnIO.Setup/releases/download/$PawnIoVersion/PawnIO_setup.exe"
$PawnIoSha256 = '1F519A22E47187F70A1379A48CA604981C4FCF694F4E65B734AAA74A9FBA3032'

# ERROR_SUCCESS_REBOOT_REQUIRED: the silent install succeeded but the driver
# only activates after a restart. Not a failure - but the script still exits
# with it, the same contract msiexec and PawnIO_setup.exe itself keep, so a
# caller can tell "success, restart pending" (3010) apart from "success,
# ready" (0) and failure (any other nonzero code).
$RebootRequiredCode = 3010

# ERROR_ALREADY_EXISTS: the installer reports a driver is already present.
# Reachable when an install was detected but its version could not satisfy
# the compatibility check below. This code is not fatal by itself; the
# post-install registry check fails if the registered version is still
# below the pinned release (or is unreadable).
$AlreadyInstalledCode = 183

function Get-PawnIoInstall {
    Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -match 'PawnIO' } |
        Select-Object -First 1
}

function Write-CollectorInstructions {
    Write-Host ''
    Write-Host 'Next: run the collector from an elevated terminal so the sidecar' -ForegroundColor Yellow
    Write-Host 'inherits admin rights (the PawnIO driver only serves elevated processes):' -ForegroundColor Yellow
    Write-Host ''
    Write-Host '  $env:SESSION_LABEL = "my-session"' -ForegroundColor White
    if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'gaming-telemetry.exe') -PathType Leaf) {
        Write-Host '  # From the extracted bundle directory:' -ForegroundColor White
        Write-Host '  .\gaming-telemetry.exe' -ForegroundColor White
    } else {
        Write-Host '  .\target\release\gaming-telemetry.exe' -ForegroundColor White
    }
}

function Stop-Setup {
    param([string]$Message, [int]$Code = 1)
    Write-Host $Message -ForegroundColor Red
    # A self-elevated window closes on exit; keep failures readable there.
    # An already-elevated terminal keeps its output, so it does not pause.
    if ($ElevatedChild) { [void](Read-Host 'Press Enter to close this window') }
    exit $Code
}

function New-AdminOnlyDirectory {
    param([Parameter(Mandatory)][string]$Path)
    # Protected DACL: SYSTEM and Administrators only. The default ProgramData
    # ACL grants BUILTIN\Users (CI)(WD,AD,WEA,WA), which would otherwise be
    # inherited and let a standard user plant PawnIO_setup.exe in the new
    # folder before the download, keeping a writable security descriptor
    # after Invoke-WebRequest -OutFile.
    if (Test-Path -LiteralPath $Path) {
        Stop-Setup "Staging path already exists: $Path"
    }
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false)
    $full = [System.Security.AccessControl.FileSystemRights]::FullControl
    $inherit = [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [System.Security.AccessControl.InheritanceFlags]::ObjectInherit
    $propagate = [System.Security.AccessControl.PropagationFlags]::None
    $allow = [System.Security.AccessControl.AccessControlType]::Allow
    foreach ($sid in @(
        [System.Security.Principal.SecurityIdentifier]::new('S-1-5-18'),
        [System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
    )) {
        $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
            $sid, $full, $inherit, $propagate, $allow
        )
        $acl.AddAccessRule($rule)
    }
    # Desktop (.NET Framework) exposes Directory.CreateDirectory(path, acl).
    # PowerShell 7 (.NET) moved the ACL-aware create to FileSystemAclExtensions.
    if ($PSVersionTable.PSEdition -eq 'Core') {
        $dirInfo = [System.IO.DirectoryInfo]::new($Path)
        [System.IO.FileSystemAclExtensions]::Create($dirInfo, $acl) | Out-Null
    } else {
        [System.IO.Directory]::CreateDirectory($Path, $acl) | Out-Null
    }
}

# A re-run on a machine that already has a compatible PawnIO is not a
# failure; skip the installer - and the UAC prompt - entirely. Older
# versions do NOT take this shortcut: PawnIO 2.1.0 dropped the \\.\PawnIO
# device alias the sidecar probes (restored in 2.2.0), so anything below
# the pinned release falls through to the installer, which upgrades it in
# place.
$installed = Get-PawnIoInstall
if ($installed) {
    $installedVersion = $null
    if ($installed.DisplayVersion) {
        try { $installedVersion = [version]$installed.DisplayVersion } catch { }
    }
    if ($installedVersion -and $installedVersion -ge [version]$PawnIoVersion) {
        Write-Host "PawnIO $installedVersion is already installed." -ForegroundColor Green
        Write-CollectorInstructions
        exit 0
    }
    if ($installedVersion) {
        Write-Host "PawnIO $installedVersion predates the pinned $PawnIoVersion; upgrading." -ForegroundColor Yellow
    } else {
        Write-Host "PawnIO is installed but its version is unreadable; running the pinned $PawnIoVersion installer." -ForegroundColor Yellow
    }
}

# Self-elevate: the kernel-driver installer requires administrator rights.
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Elevating for the kernel-driver install (UAC prompt)...' -ForegroundColor Cyan
    $elevateArgs = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -ElevatedChild' -f $PSCommandPath
    try {
        $elevated = Start-Process powershell -ArgumentList $elevateArgs -Verb RunAs -Wait -PassThru
    } catch {
        # Includes the UAC prompt being declined.
        Stop-Setup "Elevation failed or was declined: $($_.Exception.Message)"
    }
    $childCode = $elevated.ExitCode
    if ($null -eq $childCode) { $childCode = 1 }
    switch ($childCode) {
        0 {
            Write-Host 'PawnIO setup completed.' -ForegroundColor Green
            Write-CollectorInstructions
        }
        $RebootRequiredCode {
            Write-Host 'PawnIO installed. Restart Windows before collecting CPU telemetry.' -ForegroundColor Yellow
            Write-CollectorInstructions
        }
        default {
            Write-Host "Elevated setup did not complete (exit code $childCode)." -ForegroundColor Red
        }
    }
    # Propagated intact: a child's 3010 is the success-with-restart code,
    # not a failure (see $RebootRequiredCode above).
    exit $childCode
}

# Elevated from here on - either self-elevated or already launched as admin.
$rebootRequired = $false

# Stage under ProgramData in a fresh directory whose DACL is set at creation
# to SYSTEM and Administrators only (protected, no inherited Users write).
# The installer must not run elevated from a user-writable path.
$stagingDir = Join-Path $env:ProgramData ('gaming-telemetry-' + [guid]::NewGuid().ToString('N'))
try {
    New-AdminOnlyDirectory $stagingDir
    $installer = Join-Path $stagingDir 'PawnIO_setup.exe'

    Write-Host "Downloading PawnIO $PawnIoVersion installer..." -ForegroundColor Cyan
    Invoke-WebRequest -Uri $PawnIoUrl -OutFile $installer -UseBasicParsing

    $hash = (Get-FileHash $installer -Algorithm SHA256).Hash
    if ($hash -ne $PawnIoSha256) {
        Stop-Setup "PawnIO installer SHA-256 mismatch (expected $PawnIoSha256, got $hash). Refusing to run it."
    }
    $sig = Get-AuthenticodeSignature $installer
    if ($sig.Status -ne 'Valid') {
        Write-Warning "PawnIO installer Authenticode status is '$($sig.Status)'; the pinned SHA-256 still matched."
    }

    Write-Host 'Installing PawnIO (signed kernel driver)...' -ForegroundColor Cyan
    # PawnIO_setup.exe is a custom installer: -install -silent performs an
    # unattended install; -uninstall -silent removes it.
    $proc = Start-Process $installer -ArgumentList '-install','-silent' -Wait -PassThru
    $procCode = $proc.ExitCode
    if ($null -eq $procCode) {
        Stop-Setup 'PawnIO installer ended without an exit code.'
    } elseif ($procCode -eq $RebootRequiredCode) {
        $rebootRequired = $true
        $installed = Get-PawnIoInstall
        if ($installed) {
            Write-Host "PawnIO $($installed.DisplayVersion) installed." -ForegroundColor Green
        } else {
            Write-Host 'PawnIO installed; its registry entry appears after the pending restart.' -ForegroundColor Yellow
        }
    } elseif ($procCode -eq $AlreadyInstalledCode) {
        Write-Host 'PawnIO installer reports a driver is already installed.' -ForegroundColor Yellow
    } elseif ($procCode -ne 0) {
        Stop-Setup "PawnIO installer exited with code $procCode" $procCode
    }
} catch {
    Stop-Setup "Setup failed: $($_.Exception.Message)"
} finally {
    Remove-Item $stagingDir -Recurse -Force -ErrorAction SilentlyContinue
}

# Confirm the driver registered, and that it is the pinned release.
# Exit 183 (already installed) is not success if the registry still names a
# version below $PawnIoVersion: the installer refused the in-place upgrade.
$installed = Get-PawnIoInstall
if (-not $installed -and -not $rebootRequired) {
    Stop-Setup 'PawnIO install finished but no registry entry was found.'
}
if ($installed -and -not $rebootRequired) {
    $installedVersion = $null
    if ($installed.DisplayVersion) {
        try { $installedVersion = [version]$installed.DisplayVersion } catch { }
    }
    if (-not $installedVersion -or $installedVersion -lt [version]$PawnIoVersion) {
        $shown = if ($installedVersion) { "$installedVersion" } else { 'an unreadable version' }
        Stop-Setup "PawnIO $shown is still installed; uninstall it, then rerun this script to install $PawnIoVersion."
    }
    Write-Host "PawnIO $($installed.DisplayVersion) installed." -ForegroundColor Green
}

if ($rebootRequired) {
    Write-Host ''
    Write-Host 'Restart Windows before collecting CPU telemetry - the PawnIO' -ForegroundColor Yellow
    Write-Host 'driver only activates after a restart.' -ForegroundColor Yellow
    Write-CollectorInstructions
    # Deliberate: 3010 is the standard success-with-restart-required code
    # ($RebootRequiredCode above), so a caller can distinguish it from a
    # ready-now success. It is not a failure.
    exit $RebootRequiredCode
}

Write-CollectorInstructions
exit 0
