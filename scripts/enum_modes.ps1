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

public class DisplayHelper {
    [DllImport("user32.dll", CharSet = CharSet.Ansi)]
    public static extern bool EnumDisplaySettingsA(string deviceName, int modeNum, ref DEVMODEA devMode);
}
"@ -ErrorAction SilentlyContinue

# Find the VDD monitor device name
Get-CimInstance Win32_DisplayConfiguration | Format-Table DeviceName, DisplayFlags

# Try \\.\DISPLAY1 through \\.\DISPLAY8
1..8 | ForEach-Object {
    $dev = "\\.\DISPLAY$_"
    $dm = New-Object DEVMODEA
    $dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf([Type]$dm.GetType())
    if ([DisplayHelper]::EnumDisplaySettingsA($dev, -1, [ref]$dm)) {
        Write-Host "Current mode on $dev : $($dm.dmPelsWidth)x$($dm.dmPelsHeight) @ $($dm.dmDisplayFrequency)Hz"
        $modes = @()
        $i = 0
        while ([DisplayHelper]::EnumDisplaySettingsA($dev, $i, [ref]$dm)) {
            $modes += "$($dm.dmPelsWidth)x$($dm.dmPelsHeight) @ $($dm.dmDisplayFrequency)Hz"
            $i++
        }
        $unique = $modes | Select-Object -Unique
        Write-Host "  Available resolutions on $dev : $($unique -join ', ')"
    }
}
