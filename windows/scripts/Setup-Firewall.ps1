#Requires -Version 5.1
<#
.SYNOPSIS
    Adds the inbound Windows Firewall rules AirBridge needs.

.DESCRIPTION
    Creates two rules through `netsh advfirewall`:

      * AirBridge File Transfer (TCP)  - the configured receive port, restricted
        to the AirBridge executable when it can be located.
      * AirBridge mDNS Discovery (UDP 5353) - lets multicast DNS replies reach
        the machine on private and domain networks.

    Only the Private and Domain profiles are opened. Public networks are left
    untouched. Running the script again replaces both rules, so a changed port
    or a moved executable is handled.

.PARAMETER Port
    The TCP port the AirBridge receiver listens on (1024-65535).

.PARAMETER Remove
    Deletes the two rules instead of adding them.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\Setup-Firewall.ps1 -Port 42517

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\Setup-Firewall.ps1 -Remove
#>
param(
    [ValidateRange(1024, 65535)]
    [int]$Port = 42517,

    [switch]$Remove
)

$ErrorActionPreference = 'Stop'
$transferRule = 'AirBridge File Transfer (TCP)'
$mdnsRule = 'AirBridge mDNS Discovery (UDP 5353)'

function Test-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-Administrator)) {
    Write-Host 'AirBridge firewall setup needs administrator rights.' -ForegroundColor Yellow
    Write-Host 'Windows will now show an elevation prompt.'

    $arguments = @(
        '-NoProfile'
        '-ExecutionPolicy', 'Bypass'
        '-File', ('"{0}"' -f $PSCommandPath)
        '-Port', $Port
    )

    if ($Remove) {
        $arguments += '-Remove'
    }

    try {
        Start-Process -FilePath 'powershell.exe' -ArgumentList $arguments -Verb RunAs | Out-Null
        exit 0
    }
    catch {
        Write-Host "Could not elevate: $($_.Exception.Message)" -ForegroundColor Red
        exit 1
    }
}

function Invoke-Netsh {
    param([string[]]$Arguments)

    & netsh.exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "netsh failed with exit code $LASTEXITCODE."
    }
}

try {
    if ($Remove) {
        Write-Host 'Removing AirBridge firewall rules...' -ForegroundColor Cyan
        & netsh.exe advfirewall firewall delete rule name="$transferRule" | Out-Null
        & netsh.exe advfirewall firewall delete rule name="$mdnsRule" | Out-Null
        Write-Host 'AirBridge firewall rules removed.' -ForegroundColor Green
    }
    else {
        $exePath = Join-Path (Split-Path -Parent $PSScriptRoot) 'AirBridge.exe'
        $hasExe = Test-Path -LiteralPath $exePath

        Write-Host "Opening the Windows Firewall for AirBridge (TCP $Port, UDP 5353)..." -ForegroundColor Cyan

        # Replace any previous rule so port and program changes take effect.
        & netsh.exe advfirewall firewall delete rule name="$transferRule" | Out-Null
        & netsh.exe advfirewall firewall delete rule name="$mdnsRule" | Out-Null

        if ($hasExe) {
            Invoke-Netsh @(
                'advfirewall', 'firewall', 'add', 'rule',
                "name=$transferRule",
                'dir=in',
                'action=allow',
                'protocol=TCP',
                "localport=$Port",
                'profile=private,domain',
                "program=$exePath",
                'enable=yes'
            )
        }
        else {
            Write-Host "AirBridge.exe was not found next to the scripts folder; the TCP rule will allow any program on port $Port." -ForegroundColor Yellow
            Invoke-Netsh @(
                'advfirewall', 'firewall', 'add', 'rule',
                "name=$transferRule",
                'dir=in',
                'action=allow',
                'protocol=TCP',
                "localport=$Port",
                'profile=private,domain',
                'enable=yes'
            )
        }

        Invoke-Netsh @(
            'advfirewall', 'firewall', 'add', 'rule',
            "name=$mdnsRule",
            'dir=in',
            'action=allow',
            'protocol=UDP',
            'localport=5353',
            'profile=private,domain',
            'enable=yes'
        )

        Write-Host ''
        Write-Host 'Done. Incoming AirBridge transfers and mDNS discovery are allowed on private and domain networks.' -ForegroundColor Green
        Write-Host 'Public networks remain blocked by these rules.'
    }
}
catch {
    Write-Host ''
    Write-Host "Firewall setup failed: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

if ($Host.Name -eq 'ConsoleHost' -and -not $env:AIRBRIDGE_NONINTERACTIVE) {
    Write-Host ''
    Read-Host 'Press Enter to close this window' | Out-Null
}

exit 0
