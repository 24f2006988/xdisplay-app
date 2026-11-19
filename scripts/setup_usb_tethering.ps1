<#
.SYNOPSIS
    One-time Windows setup so Android USB Tethering works automatically for every phone or tablet.
.DESCRIPTION
    Configures two things once on this PC (zero per-device drivers, zero USB debugging):
      1. Windows Firewall rules allowing XDisplay over USB Tethering (wired local-subnet links,
         even when Windows classifies a new Android device as a "Public" network) and over
         Private/Domain Wi-Fi. Public Wi-Fi remains blocked.
      2. A lightweight event-triggered scheduled task ("XDisplay\USB Tethering Route Priority")
         that raises the route metric of Android RNDIS / USB NCM adapters to 500 when they
         connect, so turning on USB Tethering never steals your PC's Wi-Fi/Ethernet internet.

    Safe to re-run. Run with -Remove to undo both changes cleanly.
.PARAMETER HostExe
    Optional path to xdisplay-host.exe. Auto-detected if omitted.
.PARAMETER TaskOnly
    Only register (or remove) the USB Tethering route-priority task without changing firewall rules.
.PARAMETER NoRouteTask
    Only configure the firewall rules without registering the route-priority scheduled task.
.PARAMETER Remove
    Removes the firewall rules and scheduled task.
.PARAMETER Quiet
    Suppresses interactive pauses (used by the installer and uninstaller).
#>
[CmdletBinding()]
param(
    [string]$HostExe = "",
    [switch]$TaskOnly,
    [switch]$NoRouteTask,
    [switch]$Remove,
    [switch]$Quiet
)

$ErrorActionPreference = "Stop"
$TaskName = "XDisplay\USB Tethering Route Priority"

$RuleNames = @(
    "XDisplay Host (USB Tethering TCP)",
    "XDisplay Host (USB Tethering UDP)",
    "XDisplay Host (Wi-Fi TCP)",
    "XDisplay Host (Wi-Fi UDP)",
    "XDisplay Host (Wi-Fi stream, TCP 59100)",
    "XDisplay Host (Wi-Fi discovery, UDP 59101)",
    "XDisplay Host Stream"
)

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    if (-not $Quiet) { Write-Host "Requesting administrator rights..." -ForegroundColor Yellow }
    $argList = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"")
    if ($HostExe)     { $argList += @("-HostExe", "`"$HostExe`"") }
    if ($TaskOnly)    { $argList += "-TaskOnly" }
    if ($NoRouteTask) { $argList += "-NoRouteTask" }
    if ($Remove)      { $argList += "-Remove" }
    if ($Quiet)       { $argList += "-Quiet" }
    $proc = Start-Process powershell.exe -Verb RunAs -ArgumentList $argList -Wait -PassThru
    exit $proc.ExitCode
}

if (-not $HostExe) {
    $root = (Resolve-Path "$PSScriptRoot\..").Path
    $HostExe = @(
        (Join-Path $root "xdisplay-host.exe"),
        (Join-Path $root "target\release\xdisplay-host.exe"),
        (Join-Path $root "target\debug\xdisplay-host.exe")
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
}

if ($Remove) {
    if (-not $TaskOnly) {
        foreach ($name in $RuleNames) {
            Remove-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue
        }
    }
    & schtasks.exe /Delete /TN $TaskName /F 2>$null | Out-Null
    if (-not $Quiet) {
        Write-Host "XDisplay USB Tethering setup removed." -ForegroundColor Green
        Start-Sleep -Seconds 2
    }
    exit 0
}

# 1. Windows Firewall rules (USB Tethering on Wired local-subnet + Wi-Fi on Private/Domain)
if (-not $TaskOnly) {
    foreach ($name in $RuleNames) {
        Remove-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue
    }

    if ($HostExe -and (Test-Path $HostExe)) {
        New-NetFirewallRule -DisplayName "XDisplay Host (USB Tethering TCP)" -Direction Inbound -Action Allow `
            -Program $HostExe -Protocol TCP -Profile Public, Private, Domain -InterfaceType Wired `
            -RemoteAddress LocalSubnet | Out-Null
        New-NetFirewallRule -DisplayName "XDisplay Host (USB Tethering UDP)" -Direction Inbound -Action Allow `
            -Program $HostExe -Protocol UDP -Profile Public, Private, Domain -InterfaceType Wired `
            -RemoteAddress LocalSubnet | Out-Null
        New-NetFirewallRule -DisplayName "XDisplay Host (Wi-Fi TCP)" -Direction Inbound -Action Allow `
            -Program $HostExe -Protocol TCP -Profile Private, Domain `
            -RemoteAddress LocalSubnet | Out-Null
        New-NetFirewallRule -DisplayName "XDisplay Host (Wi-Fi UDP)" -Direction Inbound -Action Allow `
            -Program $HostExe -Protocol UDP -Profile Private, Domain `
            -RemoteAddress LocalSubnet | Out-Null
    } else {
        New-NetFirewallRule -DisplayName "XDisplay Host (USB Tethering TCP)" -Direction Inbound -Action Allow `
            -Protocol TCP -LocalPort 59100 -Profile Public, Private, Domain -InterfaceType Wired `
            -RemoteAddress LocalSubnet | Out-Null
        New-NetFirewallRule -DisplayName "XDisplay Host (USB Tethering UDP)" -Direction Inbound -Action Allow `
            -Protocol UDP -LocalPort 59101 -Profile Public, Private, Domain -InterfaceType Wired `
            -RemoteAddress LocalSubnet | Out-Null
        New-NetFirewallRule -DisplayName "XDisplay Host (Wi-Fi TCP)" -Direction Inbound -Action Allow `
            -Protocol TCP -LocalPort 59100 -Profile Private, Domain `
            -RemoteAddress LocalSubnet | Out-Null
        New-NetFirewallRule -DisplayName "XDisplay Host (Wi-Fi UDP)" -Direction Inbound -Action Allow `
            -Protocol UDP -LocalPort 59101 -Profile Private, Domain `
            -RemoteAddress LocalSubnet | Out-Null
    }

    if (-not $Quiet) {
        Write-Host "[OK] Firewall configured for USB Tethering (any Android device) and Private Wi-Fi." -ForegroundColor Green
    }
}

# 2. Event-triggered route priority task (keeps PC's Wi-Fi/Ethernet as primary internet while USB tethered)
if (-not $NoRouteTask) {
    if ($HostExe -and (Test-Path $HostExe)) {
        $escapedExe = [System.Security.SecurityElement]::Escape($HostExe)
        $xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Keeps PC Wi-Fi/Ethernet as primary internet when an Android USB Tethering adapter connects.</Description>
  </RegistrationInfo>
  <Triggers>
    <EventTrigger>
      <Enabled>true</Enabled>
      <Subscription>&lt;QueryList&gt;&lt;Query Id="0" Path="Microsoft-Windows-NetworkProfile/Operational"&gt;&lt;Select Path="Microsoft-Windows-NetworkProfile/Operational"&gt;*[System[Provider[@Name='Microsoft-Windows-NetworkProfile'] and EventID=10000]]&lt;/Select&gt;&lt;/Query&gt;&lt;/QueryList&gt;</Subscription>
    </EventTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <ExecutionTimeLimit>PT1M</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>$escapedExe</Command>
      <Arguments>--configure-usb-tether</Arguments>
    </Exec>
  </Actions>
</Task>
"@
        $tmpXml = Join-Path $env:TEMP ("xdisplay-tether-task-" + [guid]::NewGuid().ToString("N") + ".xml")
        try {
            [System.IO.File]::WriteAllText($tmpXml, $xml, [System.Text.Encoding]::Unicode)
            & schtasks.exe /Create /TN $TaskName /XML $tmpXml /F | Out-Null
        } finally {
            Remove-Item -LiteralPath $tmpXml -Force -ErrorAction SilentlyContinue
        }

        # Apply immediately to any USB tethering adapter already plugged in.
        & $HostExe --configure-usb-tether 2>$null | Out-Null
        if (-not $Quiet) {
            Write-Host "[OK] USB Tethering route protection installed (your PC keeps its own Wi-Fi/Ethernet internet)." -ForegroundColor Green
        }
    } elseif (-not $Quiet) {
        Write-Host "[!] xdisplay-host.exe not found yet; skipped route priority task." -ForegroundColor Yellow
    }
}

if (-not $Quiet) {
    Write-Host ""
    Write-Host "All set! Plug in ANY Android phone or tablet, turn on USB Tethering, and open XDisplay." -ForegroundColor Cyan
    Write-Host "First time on a device: enter the 6-digit PIN from the PC dashboard. After that it connects automatically." -ForegroundColor Gray
    Start-Sleep -Seconds 3
}
