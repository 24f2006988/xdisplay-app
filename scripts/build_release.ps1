<#
.SYNOPSIS
    Builds a complete XDisplay release into dist\: the Android APK, the Windows installer and
    SHA256SUMS.txt.
.DESCRIPTION
    Runs the tests, builds the Android native library for every supported CPU (arm64-v8a,
    armeabi-v7a, x86_64), the APK, the Windows host, and the Inno Setup installer.

    Needs: Rust (with the Android targets and cargo-ndk), the Android SDK + NDK, JDK 17 and
    Inno Setup 6. The APK is signed when a keystore is configured (see client/app/build.gradle.kts:
    XDISPLAY_KEYSTORE_PROPERTIES or client/keystore.properties); otherwise it is left unsigned.
.PARAMETER SkipTests
    Skip the unit tests.
#>
[CmdletBinding()]
param([switch]$SkipTests)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..").Path
$dist = Join-Path $root "dist"

function Step($text) { Write-Host ""; Write-Host "==> $text" -ForegroundColor Cyan }
function Run($exe, [string[]]$arguments, $dir = $root) {
    Push-Location $dir
    try {
        & $exe @arguments
        if ($LASTEXITCODE -ne 0) { throw "$exe $($arguments -join ' ') failed with exit code $LASTEXITCODE" }
    } finally { Pop-Location }
}

$version = (Select-String -Path (Join-Path $root "host\Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
Write-Host "XDisplay $version" -ForegroundColor Yellow

# Toolchain discovery
$sdk = @($env:ANDROID_HOME, $env:ANDROID_SDK_ROOT, (Join-Path $env:LOCALAPPDATA "Android\Sdk")) |
    Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $sdk) { throw "Android SDK not found (set ANDROID_HOME)." }
if (-not $env:ANDROID_NDK_HOME) {
    $ndk = Get-ChildItem (Join-Path $sdk "ndk") -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending | Select-Object -First 1
    if (-not $ndk) { throw "Android NDK not found (install it with the SDK manager or set ANDROID_NDK_HOME)." }
    $env:ANDROID_NDK_HOME = $ndk.FullName
}
$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found (winget install JRSoftware.InnoSetup)." }

New-Item -ItemType Directory -Force $dist | Out-Null

if (-not $SkipTests) {
    Step "Tests"
    Run cargo @("test", "--release", "--workspace")
    Run cargo @("test", "--release") (Join-Path $root "client\native")
}

Step "Android native library (arm64-v8a, armeabi-v7a, x86_64)"
Run cargo @("ndk", "-t", "arm64-v8a", "-t", "armeabi-v7a", "-t", "x86_64", "-P", "26", "-o", "..\app\src\main\jniLibs", "build", "--release") (Join-Path $root "client\native")

Step "Android APK"
Run (Join-Path $root "client\gradlew.bat") @("assembleRelease", "--console=plain") (Join-Path $root "client")
$apkDir = Join-Path $root "client\app\build\outputs\apk\release"
$apk = @("app-release.apk", "app-release-unsigned.apk") | ForEach-Object { Join-Path $apkDir $_ } | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $apk) { throw "No APK produced in $apkDir" }
if ($apk -like "*unsigned*") { Write-Warning "The APK is unsigned (no keystore configured); Android will not install it as is." }
Copy-Item $apk (Join-Path $dist "XDisplay-$version.apk") -Force

Step "Windows host"
Run cargo @("build", "--release", "-p", "xdisplay-host")

Step "Windows installer"
Run $iscc @("/Q", "/DMyAppVersion=$version", (Join-Path $root "installer\xdisplay_setup.iss"))

Step "Checksums"
$files = @("XDisplay-$version.apk", "XDisplay-Setup-$version.exe")
$lines = foreach ($f in $files) {
    $hash = (Get-FileHash (Join-Path $dist $f) -Algorithm SHA256).Hash.ToLower()
    "$hash  $f"
}
# LF line endings, so `sha256sum -c SHA256SUMS.txt` works on Linux and macOS too.
[System.IO.File]::WriteAllText((Join-Path $dist "SHA256SUMS.txt"), (($lines -join "`n") + "`n"))
$lines | ForEach-Object { Write-Host "  $_" }

Write-Host ""
Write-Host "Done. Release files are in $dist" -ForegroundColor Green
