Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

[StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
public struct DEVMODEA {
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)]
    public string dmDeviceName;
    public short dmSpecVersion;
    public short dmDriverVersion;
    public short dmSize;
    public short dmDriverExtra;
    public int dmFields;
    public int dmPositionX;
    public int dmPositionY;
    public int dmDisplayOrientation;
    public int dmDisplayFixedOutput;
    public short dmColor;
    public short dmDuplex;
    public short dmYResolution;
    public short dmTTOption;
    public short dmCollate;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)]
    public string dmFormName;
    public short dmLogPixels;
    public short dmBitsPerPel;
    public int dmPelsWidth;
    public int dmPelsHeight;
    public int dmDisplayFlags;
    public int dmDisplayFrequency;
    public int dmICMMethod;
    public int dmICMIntent;
    public int dmMediaType;
    public int dmDitherType;
    public int dmReserved1;
    public int dmReserved2;
    public int dmPanningWidth;
    public int dmPanningHeight;
}

public class DisplayChanger {
    [DllImport("user32.dll", CharSet = CharSet.Ansi)]
    public static extern bool EnumDisplaySettingsA(string deviceName, int modeNum, ref DEVMODEA devMode);

    [DllImport("user32.dll", CharSet = CharSet.Ansi)]
    public static extern int ChangeDisplaySettingsExA(string lpszDeviceName, ref DEVMODEA lpDevMode, IntPtr hwnd, uint dwflags, IntPtr lParam);

    public const uint CDS_UPDATEREGISTRY = 0x01;
    public const uint CDS_TEST = 0x02;
    public const int DISP_CHANGE_SUCCESSFUL = 0;
}
"@ -ErrorAction SilentlyContinue

$dev = "\\.\DISPLAY6"
$targetW = 3200
$targetH = 2136
$targetHz = 120

$dm = New-Object DEVMODEA
$dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf([Type]$dm.GetType())
$found = $false
$i = 0
while ([DisplayChanger]::EnumDisplaySettingsA($dev, $i, [ref]$dm)) {
    if ($dm.dmPelsWidth -eq $targetW -and $dm.dmPelsHeight -eq $targetH -and $dm.dmDisplayFrequency -eq $targetHz) {
        $found = $true
        break
    }
    $i++
}

if ($found) {
    Write-Host "Found target mode: $($targetW)x$($targetH) @ $($targetHz)Hz" -ForegroundColor Green
    $DM_POSITION = 0x00000020
    $DM_PELSWIDTH = 0x00080000
    $DM_PELSHEIGHT = 0x00100000
    $DM_DISPLAYFREQUENCY = 0x00400000
    
    $dm.dmPositionX = 1920
    $dm.dmPositionY = 0
    $dm.dmFields = $DM_POSITION -bor $DM_PELSWIDTH -bor $DM_PELSHEIGHT -bor $DM_DISPLAYFREQUENCY
    
    $CDS_UPDATEREGISTRY = 0x01
    $CDS_NORESET = 0x10000000
    $res1 = [DisplayChanger]::ChangeDisplaySettingsExA($dev, [ref]$dm, [IntPtr]::Zero, $CDS_UPDATEREGISTRY -bor $CDS_NORESET, [IntPtr]::Zero)
    Write-Host "Stage 1 (apply to monitor) result: $res1" -ForegroundColor Yellow
    $res2 = [DisplayChanger]::ChangeDisplaySettingsExA($null, [ref]$dm, [IntPtr]::Zero, 0, [IntPtr]::Zero)
    Write-Host "Stage 2 (commit desktop) result: $res2" -ForegroundColor Cyan
} else {
    Write-Host "Target mode not found in driver mode list" -ForegroundColor Red
}
