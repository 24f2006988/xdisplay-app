@echo off
setlocal enabledelayedexpansion

title XDisplay - Ultra-Low Latency Second Screen ^& Tablet
cls

echo =====================================================================
echo           XDisplay: One-Click Launcher ^& Runtime Manager
echo =====================================================================
echo.

:: Ensure Cargo is in PATH
where cargo >nul 2>&1
if %ERRORLEVEL% neq 0 (
    if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
        set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
    )
)

set "REPO_ROOT=%~dp0"
if "%REPO_ROOT:~-1%"=="\" set "REPO_ROOT=%REPO_ROOT:~0,-1%"

:: Step 1: Check Display Driver
echo [1/4] Checking Virtual Display Adapter...
powershell -NoProfile -ExecutionPolicy Bypass -Command "& { & '%REPO_ROOT%\driver\enable_virtual_display.ps1' }"
if %ERRORLEVEL% neq 0 (
    echo [WARN] Virtual display check returned warning. Continuing...
) else (
    echo [OK] Virtual display adapter ready.
)
echo.

:: Step 2: Check ADB and Connected Device
echo [2/4] Checking connected Android tablet via USB ADB / Direct Plug-and-Play...

:: Search for adb.exe in common installation directories
if exist "%LOCALAPPDATA%\Android\Sdk\platform-tools\adb.exe" (
    set "PATH=%LOCALAPPDATA%\Android\Sdk\platform-tools;%PATH%"
)
if exist "%REPO_ROOT%\tools\adb.exe" (
    set "PATH=%REPO_ROOT%\tools;%PATH%"
)
for /d %%D in ("%LOCALAPPDATA%\Microsoft\WinGet\Packages\*PlatformTools*") do (
    if exist "%%D\platform-tools\adb.exe" (
        set "PATH=%%D\platform-tools;%PATH%"
    )
)

where adb >nul 2>&1
if %ERRORLEVEL% neq 0 (
    echo [INFO] 'adb' not found. Direct USB / Wi-Fi mode active.
    echo        Plug in your tablet or connect via Wi-Fi.
    goto :LAUNCH_HOST
)

:: Pre-start ADB server outside loops so background daemon never inherits pipe handles
adb start-server >nul 2>&1

set DEVICE_ID=
for /f "tokens=1,2" %%A in ('adb devices') do (
    if "%%B"=="device" (
        set DEVICE_ID=%%A
    )
)

if "%DEVICE_ID%"=="" goto :NO_DEVICE

echo [OK] Connected to tablet: %DEVICE_ID%
echo Setting up USB port forwarding: adb reverse tcp:59100 tcp:59100...
adb reverse tcp:59100 tcp:59100

echo [3/4] Launching XDisplay on tablet...
adb shell am start -n com.xdisplay/.MainActivity
goto :LAUNCH_HOST

:NO_DEVICE
echo [INFO] No tablet with USB debugging found. Using plug-and-play USB instead:
echo        plug in the tablet and tap OK on "Open XDisplay for this USB accessory?".
echo        (Wi-Fi works too: open XDisplay on the tablet and pick this PC.)

:LAUNCH_HOST
echo.

:: Step 4: Launch Host Executable
echo [4/4] Starting XDisplay Windows Host...
set "HOST_EXE=%REPO_ROOT%\target\release\xdisplay-host.exe"

tasklist /fi "imagename eq xdisplay-host.exe" 2>nul | findstr /i "xdisplay-host.exe" >nul 2>&1
if %ERRORLEVEL% equ 0 (
    echo [INFO] Restarting existing XDisplay Host instance to refresh USB ^& display state...
    taskkill /f /im xdisplay-host.exe >nul 2>&1
    timeout /t 1 /nobreak >nul
)

if not exist "%HOST_EXE%" (
    echo Release binary not found. Compiling release build now...
    cd /d "%REPO_ROOT%"
    cargo build --release -p xdisplay-host
    if !ERRORLEVEL! neq 0 (
        echo [ERROR] Failed to compile xdisplay-host!
        pause
        exit /b 1
    )
)

echo.
echo =====================================================================
echo   XDisplay is running^^!
echo   - GUI Dashboard: check system tray icon or taskbar window
echo   - To stop: press Ctrl+C in this console or Exit from the tray menu
echo =====================================================================
echo.

cd /d "%REPO_ROOT%"
"%HOST_EXE%"

pause
