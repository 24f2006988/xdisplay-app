<#
.SYNOPSIS
    XDisplay - One-Click Runner for Wi-Fi mode (no USB cable needed).
.DESCRIPTION
    The Wi-Fi counterpart of run_xdisplay.ps1: enables the virtual display, makes sure the
    firewall allows tablets on your network, shows which addresses the tablet can reach, opens
    XDisplay on the tablet if one happens to be plugged in for ADB, and runs the host.

    On the tablet: open XDisplay, tap your PC under "PCs on this Wi-Fi", enter the PIN from the
    PC dashboard the first time. Afterwards it reconnects without a PIN.
.PARAMETER RebuildHost
    Stop a running host and rebuild the release binary first.
.PARAMETER NoTabletLaunch
    Do not try to open the app on a USB-attached tablet.
#>
[CmdletBinding()]
param(
    [switch]$RebuildHost,
    [switch]$NoTabletLaunch
)

$ErrorActionPreference = "Continue"

Write-Host "=====================================================================" -ForegroundColor Cyan
Write-Host "          XDisplay Wi-Fi Runner                                      " -ForegroundColor Yellow
Write-Host "=====================================================================" -ForegroundColor Cyan
Write-Host ""

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    if (Test-Path "$env:USERPROFILE\.cargo\bin") { $env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path }
}
$repoRoot = (Resolve-Path "$PSScriptRoot\..").Path
$hostExe = Join-Path $repoRoot "target\release\xdisplay-host.exe"

# 1. Virtual secondary display
Write-Host "[1/5] Ensuring secondary virtual display is active..." -ForegroundColor Green
$driverScript = Join-Path $repoRoot "driver\enable_virtual_display.ps1"
if (Test-Path $driverScript) {
    & powershell -NoProfile -ExecutionPolicy Bypass -File $driverScript | Out-Null
    Write-Host "       -> Virtual display driver checked." -ForegroundColor Gray
}

# 2. Firewall: without these rules Windows silently blocks the tablet
Write-Host "[2/5] Checking Windows Firewall rules for Wi-Fi..." -ForegroundColor Green
$rules = @(Get-NetFirewallRule -DisplayName "XDisplay Host (Wi-Fi*" -ErrorAction SilentlyContinue | Where-Object Enabled -eq True)
if ($rules.Count -ge 2) {
    Write-Host "       -> TCP 59100 + UDP 59101 allowed on the local network." -ForegroundColor Gray
} else {
    Write-Host "       -> Rules missing; requesting administrator rights to add them..." -ForegroundColor Yellow
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $repoRoot "scripts\setup_wifi_firewall.ps1")
}

# 3. Network the tablet must share with this PC
Write-Host "[3/5] Networks the tablet can use (join one of these on the tablet):" -ForegroundColor Green
Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
    Where-Object { $_.IPAddress -notlike "127.*" -and $_.IPAddress -notlike "169.254.*" -and $_.PrefixOrigin -ne "WellKnown" } |
    ForEach-Object { Write-Host ("       {0,-16} {1}" -f $_.IPAddress, $_.InterfaceAlias) -ForegroundColor Gray }
$public = @(Get-NetConnectionProfile -ErrorAction SilentlyContinue | Where-Object NetworkCategory -eq "Public")
if ($public.Count -gt 0) {
    Write-Host "       [WARN] Windows treats these as 'Public' (XDisplay is blocked there):" -ForegroundColor Yellow
    $public | ForEach-Object { Write-Host "              - $($_.Name) ($($_.InterfaceAlias))" -ForegroundColor Yellow }
    Write-Host "              Set your Wi-Fi to Private: Settings > Network & internet > Wi-Fi > <network>." -ForegroundColor Yellow
}

# 4. Optional: open the app on a tablet that is plugged in for ADB
Write-Host "[4/5] Tablet app..." -ForegroundColor Green
$adbPath = "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe"
if ((-not $NoTabletLaunch) -and (Test-Path $adbPath)) {
    & $adbPath start-server | Out-Null
    if (& $adbPath devices | Select-String "\sdevice$") {
        & $adbPath shell am start -n com.xdisplay/.ConnectActivity | Out-Null
        Write-Host "       -> Opened XDisplay's connect screen on the tablet." -ForegroundColor Gray
    } else {
        Write-Host "       -> No USB tablet; open XDisplay on the tablet yourself." -ForegroundColor Gray
    }
} else {
    Write-Host "       -> Open XDisplay on the tablet yourself." -ForegroundColor Gray
}

# 5. Host
if ($RebuildHost) {
    Get-Process xdisplay-host -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
}
if ($RebuildHost -or (-not (Test-Path $hostExe))) {
    Write-Host "[5/5] Building xdisplay-host (release mode)..." -ForegroundColor Yellow
    Push-Location $repoRoot
    cargo build --release -p xdisplay-host
    Pop-Location
}

Write-Host ""
Write-Host "=====================================================================" -ForegroundColor Cyan
Write-Host "  On the tablet: tap your PC under 'PCs on this Wi-Fi'." -ForegroundColor Yellow
Write-Host "  First time only: type the 6-digit PIN from the dashboard's" -ForegroundColor Yellow
Write-Host "  'Wi-Fi & Pairing' card (system tray icon if the window is hidden)." -ForegroundColor Yellow
Write-Host "=====================================================================" -ForegroundColor Cyan

$existing = Get-Process xdisplay-host -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "[5/5] XDisplay Host is already running (PID: $($existing.Id)); showing its dashboard." -ForegroundColor Green
    & (Join-Path $repoRoot "scripts\launch_host.ps1") | Out-Null   # second launch only focuses the window
    return
}

Write-Host "[5/5] Starting XDisplay Host (Ctrl+C or tray > Exit to stop)..." -ForegroundColor Cyan
Push-Location $repoRoot
try { & $hostExe } finally { Pop-Location }
