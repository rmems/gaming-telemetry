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
$InformationPreference = 'Continue'

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
    Write-Information ''
    Write-Information 'Next: run the collector from an elevated terminal so the sidecar'
    Write-Information 'inherits admin rights (the PawnIO driver only serves elevated processes):'
    Write-Information ''
    Write-Information '  $env:SESSION_LABEL = "my-session"'
    if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'gaming-telemetry.exe') -PathType Leaf) {
        Write-Information '  # From the extracted bundle directory:'
        Write-Information '  .\gaming-telemetry.exe'
    } else {
        Write-Information '  .\target\release\gaming-telemetry.exe'
    }
}

function Exit-Setup {
    param([string]$Message, [int]$Code = 1)
    Write-Error -Message $Message -ErrorAction Continue
    # A self-elevated window closes on exit; keep failures readable there.
    # An already-elevated terminal keeps its output, so it does not pause.
    if ($ElevatedChild) { [void](Read-Host 'Press Enter to close this window') }
    exit $Code
}

function New-AdminOnlyDirectory {
    [CmdletBinding(SupportsShouldProcess)]
    param([Parameter(Mandatory)][string]$Path)
    # Protected DACL: SYSTEM and Administrators only. The default ProgramData
    # ACL grants BUILTIN\Users (CI)(WD,AD,WEA,WA), which would otherwise be
    # inherited and let a standard user plant PawnIO_setup.exe in the new
    # folder before the download, keeping a writable security descriptor
    # after Invoke-WebRequest -OutFile.
    if (Test-Path -LiteralPath $Path) {
        Exit-Setup "Staging path already exists: $Path"
    }
    if (-not $PSCmdlet.ShouldProcess($Path, 'Create private installer staging directory')) {
        throw 'Installer staging directory creation was declined.'
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

function Get-PawnIoVersion {
    $installed = Get-PawnIoInstall
    $version = $null
    if ([version]::TryParse([string]$installed.DisplayVersion, [ref]$version)) {
        return $version
    }
}

function Test-PawnIoDriver {
    $driver = Get-Service -Name PawnIO -ErrorAction SilentlyContinue
    return ($null -ne $driver -and $driver.Status -eq 'Running')
}

function Invoke-ElevatedSetup {
    Write-Information 'Elevating for the kernel-driver install (UAC prompt)...'
    $elevateArgs = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -ElevatedChild' -f $PSCommandPath
    try {
        $elevated = Start-Process powershell -ArgumentList $elevateArgs -Verb RunAs -Wait -PassThru
    } catch {
        # Includes the UAC prompt being declined.
        Exit-Setup "Elevation failed or was declined: $($_.Exception.Message)"
    }
    $childCode = $elevated.ExitCode
    if ($null -eq $childCode) { $childCode = 1 }
    switch ($childCode) {
        0 {
            Write-Information 'PawnIO setup completed.'
            Write-CollectorInstructions
        }
        $RebootRequiredCode {
            Write-Information 'PawnIO installed. Restart Windows before collecting CPU telemetry.'
            Write-CollectorInstructions
        }
        default {
            Write-Information "Elevated setup did not complete (exit code $childCode)."
        }
    }
    # Propagated intact: a child's 3010 is the success-with-restart code,
    # not a failure (see $RebootRequiredCode above).
    return $childCode
}

function Invoke-PawnIoInstaller {
    # Stage under ProgramData in a fresh directory whose DACL is set at creation
    # to SYSTEM and Administrators only (protected, no inherited Users write).
    # The installer must not run elevated from a user-writable path.
    $stagingDir = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) ('gaming-telemetry-' + [guid]::NewGuid().ToString('N'))
    New-AdminOnlyDirectory $stagingDir
    try {
        $installer = Join-Path $stagingDir 'PawnIO_setup.exe'

        Write-Information "Downloading PawnIO $PawnIoVersion installer..."
        if ($PSVersionTable.PSEdition -eq 'Desktop') {
            [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        }
        Invoke-WebRequest -Uri $PawnIoUrl -OutFile $installer -UseBasicParsing

        $hash = (Get-FileHash $installer -Algorithm SHA256).Hash
        if ($hash -ne $PawnIoSha256) {
            Exit-Setup "PawnIO installer SHA-256 mismatch (expected $PawnIoSha256, got $hash). Refusing to run it."
        }
        $sig = Get-AuthenticodeSignature $installer
        if ($sig.Status -ne 'Valid') {
            Write-Warning "PawnIO installer Authenticode status is '$($sig.Status)'; the pinned SHA-256 still matched."
        }

        Write-Information 'Installing PawnIO (signed kernel driver)...'
        # PawnIO_setup.exe is a custom installer: -install -silent performs an
        # unattended install; -uninstall -silent removes it.
        $proc = Start-Process $installer -ArgumentList '-install','-silent' -Wait -PassThru
        $procCode = $proc.ExitCode
        if ($null -eq $procCode) {
            Exit-Setup 'PawnIO installer ended without an exit code.'
        }
        return $procCode
    } finally {
        Remove-Item -LiteralPath $stagingDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Assert-PawnIoReady {
    $installedVersion = Get-PawnIoVersion
    if (-not $installedVersion -or $installedVersion -lt [version]$PawnIoVersion) {
        Exit-Setup "PawnIO $PawnIoVersion or newer did not register. Check the installed version before retrying."
    }
    if (-not (Test-PawnIoDriver)) {
        Exit-Setup 'The PawnIO driver is not running. Restart Windows and check the driver before collecting.'
    }
    Write-Information "PawnIO $installedVersion is running."
}

function Invoke-PawnIoSetup {
    $installedVersion = Get-PawnIoVersion
    if ($installedVersion -ge [version]$PawnIoVersion -and (Test-PawnIoDriver)) {
        Write-Information "PawnIO $installedVersion is already running."
        Write-CollectorInstructions
        return 0
    }

    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        return Invoke-ElevatedSetup
    }

    $result = Invoke-PawnIoInstaller
    if ($result -eq $RebootRequiredCode) {
        Write-Warning 'Restart Windows before collecting CPU telemetry.'
        Write-CollectorInstructions
        return $result
    }
    if ($result -notin 0, $AlreadyInstalledCode) {
        Exit-Setup "PawnIO installer exited with code $result" $result
    }
    Assert-PawnIoReady
    Write-CollectorInstructions
    return 0
}

try {
    exit (Invoke-PawnIoSetup)
} catch {
    Exit-Setup "Setup failed: $($_.Exception.Message)"
}
