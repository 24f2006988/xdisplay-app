<#
.SYNOPSIS
    Retry connecting the tablet over Wi-Fi: checks the PC side, then opens the tablet's connect screen.
.DESCRIPTION
    Run this, read the PIN from the XDisplay dashboard window that comes to the front, then on the
    tablet tap your PC and type the PIN. After "Paired" you can unplug USB; the tablet reconnects
    without a PIN from then on.
#>
$ErrorActionPreference = "Continue"
$repo = (Resolve-Path "$PSScriptRoot\..").Path
$adb = "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe"
if (-not (Test-Path $adb)) { $adb = "adb" }

function Step($ok, $text) {
    if ($ok) { Write-Host "  [ OK ] $text" -ForegroundColor Green } else { Write-Host "  [FAIL] $text" -ForegroundColor Red }
}

Write-Host "`nChecking the PC..." -ForegroundColor Cyan

# 1. Host running and listening
$hostProc = Get-Process xdisplay-host -ErrorAction SilentlyContinue
if (-not $hostProc) {
    Write-Host "  Host not running; starting it..." -ForegroundColor Yellow
    & "$repo\scripts\launch_host.ps1" | Out-Null
    Start-Sleep 4
}
$listen = Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue |
    Where-Object { $_.OwningProcess -eq (Get-Process xdisplay-host -ErrorAction SilentlyContinue | Select-Object -First 1).Id }
$port = ($listen | Select-Object -First 1).LocalPort
Step ($null -ne $port) "XDisplay host listening on TCP $port"

# 2. Firewall rules
$rules = Get-NetFirewallRule -DisplayName "XDisplay Host (Wi-Fi*" -ErrorAction SilentlyContinue | Where-Object Enabled -eq True
Step ($rules.Count -ge 2) "Firewall rules for Wi-Fi present ($($rules.Count) of 2)"
if ($rules.Count -lt 2) {
    Write-Host "         Run once as administrator:  powershell -ExecutionPolicy Bypass -File `"$repo\scripts\setup_wifi_firewall.ps1`"" -ForegroundColor Yellow
}

# 3. Which network the tablet must be on, and whether Windows trusts it
Write-Host "`nThe tablet must be on one of these networks (same Wi-Fi as this PC):" -ForegroundColor Cyan
Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
    Where-Object { $_.IPAddress -notlike "127.*" -and $_.IPAddress -notlike "169.254.*" -and $_.PrefixOrigin -ne "WellKnown" } |
    ForEach-Object { Write-Host ("  {0,-16} {1}" -f $_.IPAddress, $_.InterfaceAlias) }
$public = Get-NetConnectionProfile -ErrorAction SilentlyContinue | Where-Object NetworkCategory -eq "Public"
if ($public) {
    Write-Host "`n  These networks are 'Public', where the XDisplay rules do NOT apply:" -ForegroundColor Yellow
    $public | ForEach-Object { Write-Host "    - $($_.Name) ($($_.InterfaceAlias))" -ForegroundColor Yellow }
    Write-Host "  Switch your Wi-Fi to Private: Settings > Network & internet > Wi-Fi > <network> > Private." -ForegroundColor Yellow
}

# 4. Bring up the dashboard (shows the PIN) and the tablet's connect screen
& "$repo\scripts\launch_host.ps1" | Out-Null   # a second launch just focuses the existing dashboard
$device = (& $adb devices | Select-String "\sdevice$")
if ($device) {
    & $adb shell am start -n com.xdisplay/.ConnectActivity | Out-Null
    Write-Host "`nTablet: connect screen opened." -ForegroundColor Green
} else {
    Write-Host "`nNo USB tablet found (fine if already paired): open XDisplay on the tablet yourself." -ForegroundColor Yellow
}

Write-Host @"

Next:
  1. On the PC, look at the XDisplay dashboard > 'Wi-Fi & Pairing' card for the 6-digit PIN.
     (Hidden? Click the XDisplay icon in the system tray.)
  2. On the tablet, tap your PC under 'PCs on this Wi-Fi' and type the PIN.
  3. When the picture appears, unplug USB.

If it fails, the tablet shows the reason on its connect screen; send me that text.
"@
