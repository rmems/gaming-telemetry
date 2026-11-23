# SPDX-License-Identifier: MIT OR Apache-2.0
<#
.SYNOPSIS
    Install the one-time Windows prerequisites for CPU telemetry capture.

.DESCRIPTION
    The bundled LibreHardwareMonitor sidecar reads CPU sensors through the
    signed PawnIO kernel driver (https://pawnio.eu). PawnIO cannot be shipped
    inside this repo — it is a separately signed driver — so this script
    downloads the official installer and runs it silently.

    The script self-elevates: PawnIO is a kernel driver and its installer
    requires administrator rights. It then prints the exact command to run
    the collector with an elevated token.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/install-windows-deps.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

# Self-elevate if not already admin.
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Elevating for the kernel-driver install (UAC prompt)...' -ForegroundColor Cyan
    $args = '-NoProfile -ExecutionPolicy Bypass -File "{0}"' -f $PSCommandPath
    Start-Process powershell -ArgumentList $args -Verb RunAs -Wait
    exit $LASTEXITCODE
}

$installer = Join-Path $env:TEMP 'PawnIO_setup.exe'
Write-Host 'Downloading PawnIO installer...' -ForegroundColor Cyan
Invoke-WebRequest -Uri 'https://github.com/namazso/PawnIO.Setup/releases/latest/download/PawnIO_setup.exe' `
    -OutFile $installer -UseBasicParsing

Write-Host 'Installing PawnIO (signed kernel driver)...' -ForegroundColor Cyan
# PawnIO_setup.exe is a custom installer: -install -silent performs an
# unattended install; -uninstall -silent removes it.
$proc = Start-Process $installer -ArgumentList '-install','-silent' -Wait -PassThru
if ($proc.ExitCode -ne 0) {
    Write-Error "PawnIO installer exited with code $($proc.ExitCode)"
    exit $proc.ExitCode
}

# Confirm the driver registered.
$installed = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
    Where-Object { $_.DisplayName -match 'PawnIO' }
if (-not $installed) {
    Write-Error 'PawnIO install finished but no registry entry was found.'
    exit 1
}
Write-Host "PawnIO $($installed.DisplayVersion) installed." -ForegroundColor Green

Write-Host ''
Write-Host 'Next: run the collector from an elevated terminal so the sidecar' -ForegroundColor Yellow
Write-Host 'inherits admin rights (the PawnIO driver only serves elevated processes):' -ForegroundColor Yellow
Write-Host ''
Write-Host '  $env:SESSION_LABEL = "my-session"' -ForegroundColor White
Write-Host '  .\target\release\gaming-telemetry.exe' -ForegroundColor White
