# Dynamically locate Virtual Display Driver (VDD) from WinGet packages or local AppData
$wingetPackages = Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages"
$vddPackage = Get-ChildItem -Path $wingetPackages -Filter "*Virtual-Display-Driver*" -Directory -ErrorAction SilentlyContinue | Select-Object -First 1

if ($vddPackage) {
    $devcon = Join-Path $vddPackage.FullName "Dependencies\devcon.exe"
    $inf = Join-Path $vddPackage.FullName "SignedDrivers\VDD\MttVDD.inf"
} else {
    $devconCmd = Get-Command devcon.exe -ErrorAction SilentlyContinue
    $devcon = if ($devconCmd) { $devconCmd.Source } else { "" }
    $inf = ""
}

Write-Host "=== XDisplay: Configuring Secondary Virtual Monitor (MttVDD) ===" -ForegroundColor Cyan

# 1. Ensure C:\VirtualDisplayDriver directory and vdd_settings.xml exist
$targetDir = "C:\VirtualDisplayDriver"
if (-not (Test-Path $targetDir)) {
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
}
$bundledXml = Join-Path (Split-Path -Parent $PSCommandPath) "vdd_settings.xml"
$destXml = Join-Path $targetDir "vdd_settings.xml"
$xmlChanged = $false
if (Test-Path $bundledXml) {
    $srcHash = (Get-FileHash -Path $bundledXml -Algorithm SHA256).Hash
    $dstHash = if (Test-Path $destXml) { (Get-FileHash -Path $destXml -Algorithm SHA256 -ErrorAction SilentlyContinue).Hash } else { "" }
    if ($srcHash -ne $dstHash) {
        Copy-Item -Path $bundledXml -Destination $destXml -Force -ErrorAction SilentlyContinue
        $xmlChanged = $true
    }
}

function Invoke-VddPipeReload {
    try {
        $pipe = New-Object System.IO.Pipes.NamedPipeClientStream(".", "MTTVirtualDisplayPipe", [System.IO.Pipes.PipeDirection]::InOut)
        $pipe.Connect(1500)
        $bytes = [System.Text.Encoding]::Unicode.GetBytes("RELOAD_DRIVER" + [char]0)
        $pipe.Write($bytes, 0, $bytes.Length)
        $pipe.Flush()
        $pipe.Dispose()
        Start-Sleep -Milliseconds 400
        return $true
    } catch {
        return $false
    }
}

# 2. Configure VDDPATH in HKLM registry so MttVDD loads our optimized 3:2 tablet resolutions
$regKey = "HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver"
$needsRestart = $false
try {
    if (-not (Test-Path $regKey)) {
        New-Item -Path $regKey -Force | Out-Null
        $needsRestart = $true
    }
    $currentVal = (Get-ItemProperty -Path $regKey -Name "VDDPATH" -ErrorAction SilentlyContinue).VDDPATH
    if ($currentVal -ne $targetDir) {
        Set-ItemProperty -Path $regKey -Name "VDDPATH" -Value $targetDir -Force | Out-Null
        $needsRestart = $true
        Write-Host " [OK] Set VDDPATH to $targetDir in registry." -ForegroundColor Green
    }
} catch {
    Write-Warning "Could not write to $regKey without administrator elevation."
}

# 3. Check device state
$vdd = Get-PnpDevice -InstanceId "ROOT\DISPLAY\0000" -ErrorAction SilentlyContinue
$vddMonitor = Get-PnpDevice -Class "Monitor" -ErrorAction SilentlyContinue | Where-Object { $_.FriendlyName -like "*VDD*" -and $_.Status -eq "OK" }

if ($vdd -and $vdd.Status -eq "OK" -and -not $needsRestart) {
    if ($xmlChanged -or -not $vddMonitor) {
        if (Invoke-VddPipeReload) {
            Write-Host " [OK] Reloaded Virtual Display Driver settings via pipe." -ForegroundColor Green
        }
    }
    Write-Host " [OK] Virtual Display Driver is active and configured." -ForegroundColor Green
} else {
    Write-Host " [*] Enabling/Restarting Virtual Display Driver to apply display modes..." -ForegroundColor Yellow
    if ($vdd -and $vdd.Status -ne "OK") {
        Enable-PnpDevice -InstanceId "ROOT\DISPLAY\0000" -Confirm:$false -ErrorAction SilentlyContinue
    }
    $bundledInf = Join-Path $PSScriptRoot "..\installer\redist\vdd\MttVDD.inf"
    $hostExe = Join-Path $PSScriptRoot "..\target\release\xdisplay-host.exe"
    if (-not $vdd -and -not ($devcon -and (Test-Path $devcon)) -and (Test-Path $bundledInf) -and (Test-Path $hostExe)) {
        # No winget package: install the driver bundled with the source tree (needs administrator).
        $p = Start-Process -FilePath $hostExe -ArgumentList "--install-display-driver", "`"$((Resolve-Path $bundledInf).Path)`"" -Wait -PassThru
        if ($p.ExitCode -eq 0 -or $p.ExitCode -eq 3010) { Write-Host " [OK] Virtual Display Driver installed." -ForegroundColor Green }
        else { Write-Warning "Installing the virtual display driver failed (exit code $($p.ExitCode)). Run this script as administrator." }
    }
    elseif ($devcon -and (Test-Path $devcon)) {
        if (-not $vdd) {
            & $devcon install "$inf" Root\MttVDD | Out-Null
        } else {
            & $devcon restart Root\MttVDD | Out-Null
        }
        Write-Host " [OK] Virtual Display Driver restarted successfully." -ForegroundColor Green
    }
}

$activeDisplays = Get-PnpDevice | Where-Object { $_.Class -eq "Display" -and $_.Status -eq "OK" }
Write-Host " Active display adapters:" -ForegroundColor Gray
$activeDisplays | ForEach-Object { Write-Host "   - $($_.FriendlyName) ($($_.InstanceId))" -ForegroundColor Gray }
