# SPDX-License-Identifier: MIT OR Apache-2.0
<#
.SYNOPSIS
    Install the one-time Windows prerequisites for CPU telemetry capture.

.DESCRIPTION
    The bundled LibreHardwareMonitor sidecar reads CPU sensors through the
    signed PawnIO kernel driver (https://pawnio.eu). PawnIO cannot be shipped
    inside this repo - it is a separately signed driver - so this script
    downloads the official installer and runs it silently.

    The installer is pinned to a fixed PawnIO release and verified by SHA-256
    and Authenticode signature before it runs: the download executes with
    administrator rights, so it is staged in a fresh admin-only directory
    under ProgramData rather than a user-writable path, where it cannot be
    swapped after verification.

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
# only activates after a restart. Not a failure.
$RebootRequiredCode = 3010

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
    Write-Host '  .\target\release\gaming-telemetry.exe' -ForegroundColor White
}

function Stop-Setup {
    param([string]$Message, [int]$Code = 1)
    Write-Host $Message -ForegroundColor Red
    # A self-elevated window closes on exit; keep failures readable there.
    # An already-elevated terminal keeps its output, so it does not pause.
    if ($ElevatedChild) { [void](Read-Host 'Press Enter to close this window') }
    exit $Code
}

# A re-run on a machine that already has PawnIO is not a failure (the
# installer can return e.g. 183); skip it - and the UAC prompt - entirely.
$installed = Get-PawnIoInstall
if ($installed) {
    $version = if ($installed.DisplayVersion) { $installed.DisplayVersion } else { 'version unknown' }
    Write-Host "PawnIO $version is already installed." -ForegroundColor Green
    Write-CollectorInstructions
    exit 0
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
    exit $childCode
}

# Elevated from here on - either self-elevated or already launched as admin.
$rebootRequired = $false

# Stage in a fresh admin-only directory: the installer must not run elevated
# from a user-writable path. A new ProgramData sub-directory inherits no user
# write access, and a unique name cannot have been pre-created, so the file
# verified below is the file that runs.
$stagingDir = Join-Path $env:ProgramData ('gaming-telemetry-' + [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Force $stagingDir | Out-Null
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
    } elseif ($procCode -ne 0) {
        Stop-Setup "PawnIO installer exited with code $procCode" $procCode
    }
} catch {
    Stop-Setup "Setup failed: $($_.Exception.Message)"
} finally {
    Remove-Item $stagingDir -Recurse -Force -ErrorAction SilentlyContinue
}

# Confirm the driver registered.
$installed = Get-PawnIoInstall
if (-not $installed -and -not $rebootRequired) {
    Stop-Setup 'PawnIO install finished but no registry entry was found.'
}
if ($installed -and -not $rebootRequired) {
    Write-Host "PawnIO $($installed.DisplayVersion) installed." -ForegroundColor Green
}

if ($rebootRequired) {
    Write-Host ''
    Write-Host 'Restart Windows before collecting CPU telemetry - the PawnIO' -ForegroundColor Yellow
    Write-Host 'driver only activates after a restart.' -ForegroundColor Yellow
    Write-CollectorInstructions
    exit $RebootRequiredCode
}

Write-CollectorInstructions
exit 0
