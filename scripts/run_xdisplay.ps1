<#
.SYNOPSIS
    XDisplay - Universal One-Click Runner (PowerShell)
.DESCRIPTION
    Automates virtual display driver enablement, ADB reverse forwarding,
    tablet client launch, and Windows host execution with full error handling.
#>

[CmdletBinding()]
param(
    [switch]$RebuildHost,
    [switch]$ReinstallClient,
    [switch]$NoClientLaunch
)

$ErrorActionPreference = "Continue"

Write-Host "=====================================================================" -ForegroundColor Cyan
Write-Host "          XDisplay Universal Runner & Orchestrator                   " -ForegroundColor Yellow
Write-Host "=====================================================================" -ForegroundColor Cyan
Write-Host ""

# Ensure Cargo in PATH
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    if (Test-Path "$env:USERPROFILE\.cargo\bin") {
        $env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
    }
}

# Prioritize Android SDK platform-tools to prevent version collisions with Android Studio
if (Test-Path "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe") {
    $env:Path = "$env:LOCALAPPDATA\Android\Sdk\platform-tools;" + $env:Path
}

$repoRoot = (Resolve-Path "$PSScriptRoot\..").Path

# 1. Enable Virtual Secondary Display
Write-Host "[1/4] Ensuring secondary virtual display is active..." -ForegroundColor Green
$driverScript = Join-Path $repoRoot "driver\enable_virtual_display.ps1"
if (Test-Path $driverScript) {
    & powershell -NoProfile -ExecutionPolicy Bypass -File $driverScript | Out-Null
    Write-Host "       -> Virtual display driver checked." -ForegroundColor Gray
}

# 2. ADB Check & Reverse Port Forwarding
Write-Host "[2/4] Checking USB ADB connection..." -ForegroundColor Green
$adbCmd = Get-Command adb -ErrorAction SilentlyContinue
if ($adbCmd) {
    & adb start-server | Out-Null
    $devices = adb devices | Where-Object { $_ -match "\s+device$" }
    if ($devices) {
        $deviceId = ($devices[0] -split "\s+")[0]
        Write-Host "       -> Authorized tablet found: $deviceId" -ForegroundColor Cyan
        
        Write-Host "       -> Configuring port forwarding (adb reverse tcp:59100 tcp:59100)..." -ForegroundColor Gray
        adb reverse tcp:59100 tcp:59100 | Out-Null

        if ($ReinstallClient) {
            Write-Host "       -> Reinstalling client APK to tablet..." -ForegroundColor Yellow
            $apkPath = Join-Path $repoRoot "client\app\build\outputs\apk\debug\app-debug.apk"
            if (Test-Path $apkPath) {
                adb push $apkPath /data/local/tmp/xdisplay.apk | Out-Null
                adb shell pm install -r -t /data/local/tmp/xdisplay.apk | Out-Null
            }
        }

        if (-not $NoClientLaunch) {
            Write-Host "       -> Launching XDisplay app on tablet..." -ForegroundColor Green
            adb shell am start -n com.xdisplay/.MainActivity | Out-Null
        }
    } else {
        Write-Host "       -> [WARN] No authorized ADB device found. Ensure USB debugging is ON." -ForegroundColor Yellow
    }
} else {
    Write-Host "       -> [WARN] 'adb' command not found in PATH." -ForegroundColor Yellow
}

# 3. Compile Host if needed or requested
$hostExe = Join-Path $repoRoot "target\release\xdisplay-host.exe"
if ($RebuildHost) {
    # Stop existing host process before rebuild so binary isn't locked
    Get-Process xdisplay-host -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
}

if ($RebuildHost -or (-not (Test-Path $hostExe))) {
    Write-Host "[3/4] Building xdisplay-host (release mode)..." -ForegroundColor Yellow
    Push-Location $repoRoot
    cargo build --release -p xdisplay-host
    Pop-Location
} else {
    Write-Host "[3/4] Release binary ready ($hostExe)" -ForegroundColor Green
}

# 4. Launch Host
$existing = Get-Process xdisplay-host -ErrorAction SilentlyContinue
if ($existing -and (-not $RebuildHost)) {
    Write-Host "[4/4] XDisplay Host is already running (PID: $($existing.Id))." -ForegroundColor Green
    Write-Host "       - GUI Dashboard and System Tray are active." -ForegroundColor Gray
    Write-Host "       - To restart, pass -RebuildHost or close from the system tray." -ForegroundColor Gray
    return
}

Write-Host "[4/4] Starting XDisplay Host..." -ForegroundColor Cyan
Write-Host "       - GUI Dashboard and System Tray are active." -ForegroundColor Gray
Write-Host "       - Press Ctrl+C in this console or choose Exit from the tray icon to quit." -ForegroundColor Gray
Write-Host "=====================================================================" -ForegroundColor Cyan
Write-Host ""

Push-Location $repoRoot
try {
    & $hostExe
} finally {
    Pop-Location
}
