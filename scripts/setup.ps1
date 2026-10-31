<#
.SYNOPSIS
    XDisplay - one-time guided setup. Run it once; afterwards use run_xdisplay_wifi.bat / run_xdisplay.bat.
.DESCRIPTION
    Checks everything XDisplay needs and fixes what it can (with winget):
      1. virtual display driver   2. Windows Firewall (Wi-Fi)   3. Windows host program
      4. Android tools (adb)      5. tablet app (installed over USB if a tablet is plugged in)
    Safe to re-run: finished steps are skipped.
.PARAMETER SkipTablet
    Do not try to install the app on a USB-attached tablet.
#>
[CmdletBinding()]
param([switch]$SkipTablet)

$ErrorActionPreference = "Continue"
$root = (Resolve-Path "$PSScriptRoot\..").Path
$todo = New-Object System.Collections.Generic.List[string]

function Step($n, $text) { Write-Host ""; Write-Host "[$n/5] $text" -ForegroundColor Green }
function Ok($t)   { Write-Host "       [OK]   $t" -ForegroundColor Gray }
function Warn($t) { Write-Host "       [!]    $t" -ForegroundColor Yellow }
function Have($c) { [bool](Get-Command $c -ErrorAction SilentlyContinue) }
function WingetInstall($id) {
    if (-not (Have winget)) { return $false }
    winget install --id $id -e --silent --accept-package-agreements --accept-source-agreements | Out-Null
    return ($LASTEXITCODE -eq 0)
}

Write-Host "=====================================================================" -ForegroundColor Cyan
Write-Host "   XDisplay first-time setup (about 2 minutes, one time only)" -ForegroundColor Yellow
Write-Host "=====================================================================" -ForegroundColor Cyan

# 1. Virtual display driver
Step 1 "Virtual display driver (makes the tablet a real second monitor)"
$vdd = Get-PnpDevice -InstanceId "ROOT\DISPLAY\0000" -ErrorAction SilentlyContinue
$pkgDir = Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages"
$haveVddPkg = [bool](Get-ChildItem $pkgDir -Filter "*Virtual-Display-Driver*" -Directory -ErrorAction SilentlyContinue)
if (-not $vdd -and -not $haveVddPkg) {
    Write-Host "       Installing Virtual Display Driver with winget..." -ForegroundColor Gray
    if (-not (WingetInstall "VirtualDrivers.Virtual-Display-Driver")) {
        Warn "Could not install it automatically."
        $todo.Add("Install the driver: winget install VirtualDrivers.Virtual-Display-Driver")
    }
}
$drv = Join-Path $root "driver\enable_virtual_display.ps1"
if (Test-Path $drv) {
    # Installing/configuring the driver needs administrator rights; elevate when not active or not configured.
    $vdd = Get-PnpDevice -InstanceId "ROOT\DISPLAY\0000" -ErrorAction SilentlyContinue
    $regVddPath = (Get-ItemProperty -Path "HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver" -Name "VDDPATH" -ErrorAction SilentlyContinue).VDDPATH
    if ($vdd -and $vdd.Status -eq "OK" -and $regVddPath -eq "C:\VirtualDisplayDriver") {
        Ok "Virtual display is active and configured with tablet display modes."
    } else {
        Write-Host "       Configuring virtual display driver & resolutions (Windows will ask for administrator permission)..." -ForegroundColor Gray
        Start-Process powershell -Verb RunAs -Wait -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$drv`""
        $vdd = Get-PnpDevice -InstanceId "ROOT\DISPLAY\0000" -ErrorAction SilentlyContinue
        if ($vdd -and $vdd.Status -eq "OK") { Ok "Virtual display is active." }
        else { Warn "Virtual display not active yet."; $todo.Add("Run setup.bat again and accept the administrator prompt (or reboot once).") }
    }
}
# UsbDk is no longer used or installed: it is a filter on every USB stack and blue-screened
# Windows (0x10D) when a device was unplugged. Earlier versions installed it; flag it for removal.
if (Get-Service UsbDk -ErrorAction SilentlyContinue) {
    Warn "The UsbDk USB driver from an earlier XDisplay is installed. It can crash Windows when a USB device is unplugged."
    $todo.Add("Uninstall 'UsbDk Runtime Libraries' in Settings > Apps, then restart Windows. XDisplay no longer needs it.")
} else {
    Ok "USB works over USB debugging (adb); no extra driver needed."
}

# 2. Firewall
Step 2 "Windows Firewall (lets the tablet reach this PC over Wi-Fi)"
$rules = @(Get-NetFirewallRule -DisplayName "XDisplay Host (Wi-Fi*" -ErrorAction SilentlyContinue | Where-Object Enabled -eq True)
if ($rules.Count -ge 2) { Ok "Already allowed (local network only)." }
else {
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $root "scripts\setup_wifi_firewall.ps1")
    $rules = @(Get-NetFirewallRule -DisplayName "XDisplay Host (Wi-Fi*" -ErrorAction SilentlyContinue | Where-Object Enabled -eq True)
    if ($rules.Count -ge 2) { Ok "Rules added." } else { Warn "Firewall rules missing (Wi-Fi mode will not connect)."; $todo.Add("Run scripts\setup_wifi_firewall.ps1 as administrator.") }
}
$public = @(Get-NetConnectionProfile -ErrorAction SilentlyContinue | Where-Object NetworkCategory -eq "Public")
if ($public.Count -gt 0) {
    Warn "Windows treats '$($public[0].Name)' as a Public network, which blocks XDisplay."
    $todo.Add("Set your Wi-Fi to Private: Settings > Network & internet > Wi-Fi > your network > Network profile type > Private.")
}

# 3. Host program
Step 3 "XDisplay Windows program"
$hostExe = @((Join-Path $root "xdisplay-host.exe"), (Join-Path $root "target\release\xdisplay-host.exe")) | Where-Object { Test-Path $_ } | Select-Object -First 1
if ($hostExe) { Ok "Found $hostExe" }
else {
    if (-not (Have cargo) -and (Test-Path "$env:USERPROFILE\.cargo\bin")) { $env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path }
    if (-not (Have cargo)) {
        Write-Host "       Installing Rust with winget (needed to build from source)..." -ForegroundColor Gray
        WingetInstall "Rustlang.Rustup" | Out-Null
        $env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
    }
    if (Have cargo) {
        Write-Host "       Building (first build takes a few minutes)..." -ForegroundColor Gray
        Push-Location $root; cargo build --release -p xdisplay-host; Pop-Location
        if (Test-Path (Join-Path $root "target\release\xdisplay-host.exe")) { Ok "Built." }
        else { Warn "Build failed."; $todo.Add("Install 'Visual Studio Build Tools' with the C++ workload (winget install Microsoft.VisualStudio.2022.BuildTools), then run setup.bat again.") }
    } else { Warn "Rust not available."; $todo.Add("Install Rust from https://rustup.rs, then run setup.bat again.") }
}

# 4. adb
Step 4 "Android tools (adb, used for the USB cable and to install the app)"
$sdkAdb = "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe"
if (Test-Path $sdkAdb) { $adb = $sdkAdb } elseif (Have adb) { $adb = (Get-Command adb).Source } else {
    Write-Host "       Installing Android platform-tools with winget..." -ForegroundColor Gray
    WingetInstall "Google.PlatformTools" | Out-Null
    $adb = (Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Recurse -Filter adb.exe -ErrorAction SilentlyContinue | Select-Object -First 1).FullName
    if (-not $adb -and (Have adb)) { $adb = (Get-Command adb).Source }
}
if ($adb) { Ok "adb found." } else { Warn "adb not found (only needed for USB mode and one-click app install)." }

# 5. Tablet app
Step 5 "Tablet app"
$apk = @(
    (Get-ChildItem (Join-Path $root "dist") -Filter "XDisplay-*.apk" -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1).FullName,
    (Join-Path $root "android\XDisplay.apk"),
    (Join-Path $root "client\app\build\outputs\apk\release\app-release.apk"),
    (Join-Path $root "client\app\build\outputs\apk\debug\app-debug.apk")
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
$installed = $false
if ($SkipTablet -or -not $adb) { }
elseif (-not $apk) { Warn "No APK found next to this folder." }
else {
    & $adb start-server | Out-Null
    if (& $adb devices | Select-String "\sdevice$") {
        Write-Host "       Tablet detected - installing app..." -ForegroundColor Gray
        & $adb install -r -t $apk | Out-Null
        if ($LASTEXITCODE -eq 0) { $installed = $true; Ok "XDisplay installed on the tablet." }
    }
}
if (-not $installed) {
    Warn "App not installed on a tablet yet."
    if ($apk) {
        $todo.Add("Get the app on the tablet: plug it in by USB (USB debugging on) and run setup.bat again, or copy this file to the tablet and open it: $apk")
    } else {
        $todo.Add("Build the app: scripts\build_and_deploy_android.bat (tablet plugged in with USB debugging on).")
    }
}

# Summary
Write-Host ""
Write-Host "=====================================================================" -ForegroundColor Cyan
if ($todo.Count -eq 0) {
    Write-Host "  All set!  Double-click  run_xdisplay_wifi.bat  (or run_xdisplay.bat for USB)." -ForegroundColor Green
    Write-Host "  On the tablet open XDisplay, tap your PC, type the 6-digit PIN once." -ForegroundColor Green
} else {
    Write-Host "  Almost there. Still to do:" -ForegroundColor Yellow
    $i = 1; foreach ($t in $todo) { Write-Host "   $i. $t" -ForegroundColor Yellow; $i++ }
}
Write-Host "=====================================================================" -ForegroundColor Cyan
