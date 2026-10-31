@echo off
setlocal enabledelayedexpansion

title XDisplay - Android Client Build & Deploy
cls

echo =====================================================================
echo       XDisplay: Build NDK + Build APK + Deploy to Tablet
echo =====================================================================
echo.

set "REPO_ROOT=%~dp0.."
pushd "%REPO_ROOT%"
set "REPO_ROOT=%CD%"
popd

if not defined ANDROID_NDK_HOME (
    if exist "%LOCALAPPDATA%\Android\Sdk\ndk" (
        for /f "delims=" %%I in ('dir /b /ad /o-n "%LOCALAPPDATA%\Android\Sdk\ndk"') do (
            if not defined ANDROID_NDK_HOME set "ANDROID_NDK_HOME=%LOCALAPPDATA%\Android\Sdk\ndk\%%I"
        )
    )
)

if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
    set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
)

echo [1/3] Building Rust Native NDK library (arm64-v8a release)...
cd /d "%REPO_ROOT%\client\native"
cargo ndk -t arm64-v8a -o ..\app\src\main\jniLibs build --release
if %ERRORLEVEL% neq 0 (
    echo [ERROR] cargo-ndk build failed!
    pause
    exit /b 1
)
echo [OK] jniLibs updated successfully.
echo.

echo [2/3] Building Android APK with Gradle...
cd /d "%REPO_ROOT%\client"
call gradlew.bat assembleDebug
if %ERRORLEVEL% neq 0 (
    echo [ERROR] Gradle build failed!
    pause
    exit /b 1
)
echo [OK] APK built: app\build\outputs\apk\debug\app-debug.apk
echo.

echo [3/3] Deploying to Android tablet...
set "APK_PATH=%REPO_ROOT%\client\app\build\outputs\apk\debug\app-debug.apk"
adb push "%APK_PATH%" /data/local/tmp/xdisplay.apk
if %ERRORLEVEL% neq 0 (
    echo [ERROR] Failed to push APK to tablet. Check USB connection.
    pause
    exit /b 1
)

adb shell pm install -r -t /data/local/tmp/xdisplay.apk
if %ERRORLEVEL% neq 0 (
    echo [ERROR] Failed to install APK on tablet!
    pause
    exit /b 1
)

echo [OK] App successfully installed on tablet!
echo Launching app...
adb shell am start -n com.xdisplay/.MainActivity

echo.
echo =====================================================================
echo   Deployment completed! Tablet is running the latest XDisplay.
echo =====================================================================
echo.
pause
