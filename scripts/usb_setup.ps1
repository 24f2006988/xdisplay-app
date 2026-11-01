<#
.SYNOPSIS
    One-time setup for plug-and-play USB without USB debugging. Run XDisplay-USB-Setup.cmd.
.DESCRIPTION
    With USB debugging off, Windows binds an Android device to its file-transfer (MTP) driver,
    and the host cannot ask it to switch to accessory mode through that driver. This script binds
    Microsoft's own WinUSB driver (the inbox winusb.inf, signed by Microsoft) to:
      1. the device's file-transfer interface, so the host can send the switch request, and
      2. the accessory device it becomes (18D1:2D00), which has no driver on a clean PC.
    Both bindings are per device (by serial number): other phones, cameras and USB devices keep
    their drivers. No filter driver is installed (unlike UsbDk, which 1.0.1 removed because it
    crashed Windows). While bound, Windows cannot browse that device's files over USB.

    Every binding is recorded in HKLM\SOFTWARE\XDisplay\UsbBindings; -Undo removes them all and
    Windows reinstalls the normal drivers. The XDisplay uninstaller runs -Undo too.
.PARAMETER Undo
    Remove every binding this script made.
.PARAMETER Quiet
    Do not wait for Enter at the end (for the installer).
.PARAMETER WaitSeconds
    How long to wait for a device to be plugged in, and for it to come back as an accessory.
.PARAMETER LogFile
    Also write the output to this file.
#>
[CmdletBinding()]
param(
    [switch]$Undo,
    [switch]$Quiet,
    [int]$WaitSeconds = 90,
    [string]$LogFile,
    [switch]$Elevated
)

$ErrorActionPreference = 'Stop'
$BindingsKey = 'HKLM:\SOFTWARE\XDisplay\UsbBindings'

function Say([string]$text, [string]$color = 'Gray') { Write-Host $text -ForegroundColor $color }
function Finish([int]$code) {
    if ($LogFile) { try { Stop-Transcript | Out-Null } catch {} }
    if (-not $Quiet) { Write-Host ''; Read-Host 'Press Enter to close' | Out-Null }
    exit $code
}

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

# ---------------------------------------------------------------------------------------------
# Not elevated: start the host as the normal user (so it is not left running as admin), then
# restart this script elevated. That is the only UAC prompt.
# ---------------------------------------------------------------------------------------------
if (-not $isAdmin) {
    if (-not $Undo -and -not (Get-Process xdisplay-host -ErrorAction SilentlyContinue)) {
        $hostExe = @("$PSScriptRoot\..\xdisplay-host.exe", "$PSScriptRoot\..\target\release\xdisplay-host.exe") |
            Where-Object { Test-Path $_ } | Select-Object -First 1
        if ($hostExe) {
            Say "Starting XDisplay..."
            Start-Process -FilePath (Resolve-Path $hostExe).Path -ArgumentList '--minimized' | Out-Null
        }
    }
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Elevated', '-WaitSeconds', $WaitSeconds)
    if ($Undo) { $argList += '-Undo' }
    if ($Quiet) { $argList += '-Quiet' }
    if ($LogFile) { $argList += @('-LogFile', "`"$LogFile`"") }
    try {
        $p = Start-Process powershell.exe -Verb RunAs -ArgumentList $argList -PassThru -Wait
        exit $p.ExitCode
    } catch {
        Say "Windows needs administrator approval to change a device driver, and it was not given." Yellow
        Say "Run XDisplay-USB-Setup again and choose Yes." Yellow
        Finish 1
    }
}

if ($LogFile) { Start-Transcript -Path $LogFile -Force | Out-Null }
$Host.UI.RawUI.WindowTitle = 'XDisplay - USB setup (no USB debugging)'

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32;
using Microsoft.Win32.SafeHandles;

public static class XdUsb {
    // Device interface WinUSB registers for devices bound by this script (any fixed GUID works;
    // the host's libusb finds WinUSB devices by their driver, not by this GUID).
    public static readonly Guid InterfaceGuid = new Guid("6f1a3c52-8d0e-4b7a-9f61-2c4e5a8b7d19");
    static readonly Guid UsbDeviceClass = new Guid("88BAE032-5A81-49f0-BC3D-A4FF138216D6");
    static readonly IntPtr INVALID = new IntPtr(-1);

    [StructLayout(LayoutKind.Sequential)]
    struct SP_DEVINFO_DATA { public uint cbSize; public Guid ClassGuid; public uint DevInst; public IntPtr Reserved; }
    [StructLayout(LayoutKind.Sequential)]
    struct SP_DEVICE_INTERFACE_DATA { public uint cbSize; public Guid InterfaceClassGuid; public uint Flags; public IntPtr Reserved; }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct SP_DEVINSTALL_PARAMS {
        public uint cbSize; public uint Flags; public uint FlagsEx; public IntPtr hwndParent;
        public IntPtr InstallMsgHandler; public IntPtr InstallMsgHandlerContext; public IntPtr FileQueue;
        public UIntPtr ClassInstallReserved; public uint Reserved;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string DriverPath;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct SP_DRVINFO_DATA {
        public uint cbSize; public uint DriverType; public UIntPtr Reserved;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string Description;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string MfgName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string ProviderName;
        public long DriverDate; public ulong DriverVersion;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct SP_PROPCHANGE_PARAMS { public uint cbSize; public uint InstallFunction; public uint StateChange; public uint Scope; public uint HwProfile; }
    [StructLayout(LayoutKind.Sequential)]
    struct SP_REMOVEDEVICE_PARAMS { public uint cbSize; public uint InstallFunction; public uint Scope; public uint HwProfile; }
    [StructLayout(LayoutKind.Sequential, Pack = 1)]
    struct WINUSB_SETUP_PACKET { public byte RequestType; public byte Request; public ushort Value; public ushort Index; public ushort Length; }

    const uint DIGCF_PRESENT = 0x2, DIGCF_DEVICEINTERFACE = 0x10;
    const uint SPDIT_CLASSDRIVER = 1;
    const uint SPDRP_CLASSGUID = 0x8, SPDRP_LOWERFILTERS = 0x12;
    const uint DI_ENUMSINGLEINF = 0x00010000, DI_FLAGSEX_ALLOWEXCLUDEDDRVS = 0x00000800;
    const uint DIF_REMOVE = 0x5, DIF_PROPERTYCHANGE = 0x12;
    const uint DICS_PROPCHANGE = 3, DICS_FLAG_GLOBAL = 1, DI_REMOVEDEVICE_GLOBAL = 1;
    const uint DIREG_DEV = 1, KEY_READ_WRITE = 0x20019 | 0x20006;

    [DllImport("setupapi.dll", SetLastError = true)] static extern IntPtr SetupDiCreateDeviceInfoList(IntPtr cls, IntPtr hwnd);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern IntPtr SetupDiGetClassDevsW(ref Guid cls, IntPtr enumerator, IntPtr hwnd, uint flags);
    [DllImport("setupapi.dll", SetLastError = true)] static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool SetupDiOpenDeviceInfoW(IntPtr set, string id, IntPtr hwnd, uint flags, ref SP_DEVINFO_DATA d);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool SetupDiGetDeviceInstallParamsW(IntPtr set, ref SP_DEVINFO_DATA d, ref SP_DEVINSTALL_PARAMS p);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool SetupDiSetDeviceInstallParamsW(IntPtr set, ref SP_DEVINFO_DATA d, ref SP_DEVINSTALL_PARAMS p);
    [DllImport("setupapi.dll", SetLastError = true)] static extern bool SetupDiBuildDriverInfoList(IntPtr set, ref SP_DEVINFO_DATA d, uint type);
    [DllImport("setupapi.dll", SetLastError = true)] static extern bool SetupDiDestroyDriverInfoList(IntPtr set, ref SP_DEVINFO_DATA d, uint type);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool SetupDiEnumDriverInfoW(IntPtr set, ref SP_DEVINFO_DATA d, uint type, uint index, ref SP_DRVINFO_DATA drv);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool SetupDiSetDeviceRegistryPropertyW(IntPtr set, ref SP_DEVINFO_DATA d, uint prop, byte[] buf, uint size);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool SetupDiGetDeviceRegistryPropertyW(IntPtr set, ref SP_DEVINFO_DATA d, uint prop, out uint type, byte[] buf, uint size, out uint required);
    [DllImport("setupapi.dll", SetLastError = true)] static extern bool SetupDiSetClassInstallParamsW(IntPtr set, ref SP_DEVINFO_DATA d, ref SP_PROPCHANGE_PARAMS p, uint size);
    [DllImport("setupapi.dll", SetLastError = true)] static extern bool SetupDiSetClassInstallParamsW(IntPtr set, ref SP_DEVINFO_DATA d, ref SP_REMOVEDEVICE_PARAMS p, uint size);
    [DllImport("setupapi.dll", SetLastError = true)] static extern bool SetupDiCallClassInstaller(uint dif, IntPtr set, ref SP_DEVINFO_DATA d);
    [DllImport("setupapi.dll", SetLastError = true)] static extern IntPtr SetupDiOpenDevRegKey(IntPtr set, ref SP_DEVINFO_DATA d, uint scope, uint profile, uint keyType, uint access);
    [DllImport("setupapi.dll", SetLastError = true)] static extern bool SetupDiEnumDeviceInterfaces(IntPtr set, IntPtr devInfo, ref Guid cls, uint index, ref SP_DEVICE_INTERFACE_DATA ifd);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool SetupDiGetDeviceInterfaceDetailW(IntPtr set, ref SP_DEVICE_INTERFACE_DATA ifd, IntPtr detail, uint size, out uint required, ref SP_DEVINFO_DATA d);
    [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool SetupDiGetDeviceInstanceIdW(IntPtr set, ref SP_DEVINFO_DATA d, StringBuilder id, int size, out int required);
    [DllImport("newdev.dll", SetLastError = true)] static extern bool DiInstallDevice(IntPtr hwnd, IntPtr set, ref SP_DEVINFO_DATA d, ref SP_DRVINFO_DATA drv, uint flags, out bool reboot);
    [DllImport("cfgmgr32.dll", CharSet = CharSet.Unicode)] static extern int CM_Locate_DevNodeW(out uint devInst, string id, uint flags);
    [DllImport("cfgmgr32.dll")] static extern int CM_Reenumerate_DevNode(uint devInst, uint flags);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern SafeFileHandle CreateFileW(string path, uint access, uint share, IntPtr sec, uint disposition, uint flags, IntPtr template);
    [DllImport("winusb.dll", SetLastError = true)] static extern bool WinUsb_Initialize(SafeFileHandle h, out IntPtr iface);
    [DllImport("winusb.dll", SetLastError = true)] static extern bool WinUsb_Free(IntPtr iface);
    [DllImport("winusb.dll", SetLastError = true)] static extern bool WinUsb_SetPipePolicy(IntPtr iface, byte pipe, uint policy, uint len, ref uint value);
    [DllImport("winusb.dll", SetLastError = true)] static extern bool WinUsb_ControlTransfer(IntPtr iface, WINUSB_SETUP_PACKET setup, byte[] buf, uint len, out uint transferred, IntPtr overlapped);

    static Win32Exception Fail(string what) {
        int e = Marshal.GetLastWin32Error();
        return new Win32Exception(e, what + " failed: " + new Win32Exception(e).Message + " (0x" + e.ToString("X") + ")");
    }

    static SP_DEVINFO_DATA NewDevInfo() {
        SP_DEVINFO_DATA d = new SP_DEVINFO_DATA();
        d.cbSize = (uint)Marshal.SizeOf(typeof(SP_DEVINFO_DATA));
        return d;
    }

    static IntPtr Open(string id, out SP_DEVINFO_DATA dev) {
        IntPtr set = SetupDiCreateDeviceInfoList(IntPtr.Zero, IntPtr.Zero);
        if (set == INVALID) throw Fail("SetupDiCreateDeviceInfoList");
        dev = NewDevInfo();
        if (!SetupDiOpenDeviceInfoW(set, id, IntPtr.Zero, 0, ref dev)) {
            Win32Exception e = Fail("Opening " + id);
            SetupDiDestroyDeviceInfoList(set);
            throw e;
        }
        return set;
    }

    // "WinUsb Device" from the inbox winusb.inf, as Device Manager lists it under "Universal
    // Serial Bus devices" with "Show compatible hardware" unticked.
    static bool FindWinUsbDriver(IntPtr set, ref SP_DEVINFO_DATA dev, out SP_DRVINFO_DATA found) {
        found = new SP_DRVINFO_DATA();
        SP_DEVINSTALL_PARAMS p = new SP_DEVINSTALL_PARAMS();
        p.cbSize = (uint)Marshal.SizeOf(typeof(SP_DEVINSTALL_PARAMS));
        if (!SetupDiGetDeviceInstallParamsW(set, ref dev, ref p)) throw Fail("SetupDiGetDeviceInstallParams");
        p.Flags |= DI_ENUMSINGLEINF;
        p.FlagsEx |= DI_FLAGSEX_ALLOWEXCLUDEDDRVS;
        p.DriverPath = Environment.GetFolderPath(Environment.SpecialFolder.Windows) + "\\INF\\winusb.inf";
        if (!SetupDiSetDeviceInstallParamsW(set, ref dev, ref p)) throw Fail("SetupDiSetDeviceInstallParams");
        if (!SetupDiBuildDriverInfoList(set, ref dev, SPDIT_CLASSDRIVER)) throw Fail("SetupDiBuildDriverInfoList");
        for (uint i = 0; ; i++) {
            SP_DRVINFO_DATA drv = new SP_DRVINFO_DATA();
            drv.cbSize = (uint)Marshal.SizeOf(typeof(SP_DRVINFO_DATA));
            if (!SetupDiEnumDriverInfoW(set, ref dev, SPDIT_CLASSDRIVER, i, ref drv)) break;
            if (string.Equals(drv.Description, "WinUsb Device", StringComparison.OrdinalIgnoreCase)) {
                found = drv;
                return true;
            }
        }
        SetupDiDestroyDriverInfoList(set, ref dev, SPDIT_CLASSDRIVER);
        return false;
    }

    static void Restart(IntPtr set, ref SP_DEVINFO_DATA dev) {
        SP_PROPCHANGE_PARAMS p = new SP_PROPCHANGE_PARAMS();
        p.cbSize = 8;
        p.InstallFunction = DIF_PROPERTYCHANGE;
        p.StateChange = DICS_PROPCHANGE;
        p.Scope = DICS_FLAG_GLOBAL;
        if (!SetupDiSetClassInstallParamsW(set, ref dev, ref p, (uint)Marshal.SizeOf(typeof(SP_PROPCHANGE_PARAMS))) ||
            !SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set, ref dev))
            throw Fail("Restarting the device");
    }

    /// Binds WinUSB to one device instance. Returns true when Windows needs a restart.
    public static bool Bind(string id) {
        SP_DEVINFO_DATA dev;
        IntPtr set = Open(id, out dev);
        try {
            SP_DRVINFO_DATA drv;
            if (!FindWinUsbDriver(set, ref dev, out drv)) {
                // The class-driver list only holds drivers of the device's current class.
                byte[] cls = Encoding.Unicode.GetBytes(UsbDeviceClass.ToString("B") + "\0");
                if (!SetupDiSetDeviceRegistryPropertyW(set, ref dev, SPDRP_CLASSGUID, cls, (uint)cls.Length))
                    throw Fail("Setting the device class");
                if (!FindWinUsbDriver(set, ref dev, out drv))
                    throw new InvalidOperationException("winusb.inf has no \"WinUsb Device\" entry on this Windows version");
            }
            bool reboot;
            if (!DiInstallDevice(IntPtr.Zero, set, ref dev, ref drv, 0, out reboot)) throw Fail("Installing WinUSB");

            // The file-transfer driver puts WinUSB in the device's lower filters; as the function
            // driver it must not be there too.
            uint type, needed;
            byte[] buf = new byte[1024];
            if (SetupDiGetDeviceRegistryPropertyW(set, ref dev, SPDRP_LOWERFILTERS, out type, buf, (uint)buf.Length, out needed) && needed > 2)
                SetupDiSetDeviceRegistryPropertyW(set, ref dev, SPDRP_LOWERFILTERS, null, 0);

            // A device interface of our own, so the switch below can open the device.
            IntPtr hkey = SetupDiOpenDevRegKey(set, ref dev, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ_WRITE);
            if (hkey == INVALID) throw Fail("Opening the device's settings");
            using (RegistryKey key = RegistryKey.FromHandle(new SafeRegistryHandle(hkey, true)))
                key.SetValue("DeviceInterfaceGUIDs", new string[] { InterfaceGuid.ToString("B") }, RegistryValueKind.MultiString);
            Restart(set, ref dev);
            return reboot;
        } finally {
            SetupDiDestroyDeviceInfoList(set);
        }
    }

    /// Removes a device node (present or not); Windows reinstalls its normal driver on rescan.
    public static void Remove(string id) {
        SP_DEVINFO_DATA dev;
        IntPtr set = Open(id, out dev);
        try {
            SP_REMOVEDEVICE_PARAMS p = new SP_REMOVEDEVICE_PARAMS();
            p.cbSize = 8;
            p.InstallFunction = DIF_REMOVE;
            p.Scope = DI_REMOVEDEVICE_GLOBAL;
            if (!SetupDiSetClassInstallParamsW(set, ref dev, ref p, (uint)Marshal.SizeOf(typeof(SP_REMOVEDEVICE_PARAMS))) ||
                !SetupDiCallClassInstaller(DIF_REMOVE, set, ref dev))
                throw Fail("Removing " + id);
        } finally {
            SetupDiDestroyDeviceInfoList(set);
        }
    }

    public static void Rescan() {
        uint root;
        if (CM_Locate_DevNodeW(out root, null, 0) == 0) CM_Reenumerate_DevNode(root, 0);
    }

    static string InterfacePath(string id) {
        Guid g = InterfaceGuid;
        IntPtr set = SetupDiGetClassDevsW(ref g, IntPtr.Zero, IntPtr.Zero, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if (set == INVALID) return null;
        try {
            for (uint i = 0; ; i++) {
                SP_DEVICE_INTERFACE_DATA ifd = new SP_DEVICE_INTERFACE_DATA();
                ifd.cbSize = (uint)Marshal.SizeOf(typeof(SP_DEVICE_INTERFACE_DATA));
                if (!SetupDiEnumDeviceInterfaces(set, IntPtr.Zero, ref g, i, ref ifd)) return null;
                SP_DEVINFO_DATA dev = NewDevInfo();
                uint needed;
                SetupDiGetDeviceInterfaceDetailW(set, ref ifd, IntPtr.Zero, 0, out needed, ref dev);
                IntPtr detail = Marshal.AllocHGlobal((int)needed);
                try {
                    Marshal.WriteInt32(detail, IntPtr.Size == 8 ? 8 : 6);
                    if (!SetupDiGetDeviceInterfaceDetailW(set, ref ifd, detail, needed, out needed, ref dev)) continue;
                    StringBuilder sb = new StringBuilder(512);
                    int req;
                    if (SetupDiGetDeviceInstanceIdW(set, ref dev, sb, sb.Capacity, out req) &&
                        string.Equals(sb.ToString(), id, StringComparison.OrdinalIgnoreCase))
                        return Marshal.PtrToStringUni(new IntPtr(detail.ToInt64() + 4));
                } finally {
                    Marshal.FreeHGlobal(detail);
                }
            }
        } finally {
            SetupDiDestroyDeviceInfoList(set);
        }
    }

    /// Sends the Android Open Accessory identification and START (the same strings as the host,
    /// host/src/transport/aoa.rs). Returns the AOA version. Throws when the device cannot be
    /// opened (for example because the running host has it open: it then switches it itself).
    public static int RequestAccessoryMode(string id, string[] strings) {
        string path = InterfacePath(id);
        if (path == null) throw new InvalidOperationException("WinUSB has not started on the device yet");
        using (SafeFileHandle h = CreateFileW(path, 0xC0000000, 3, IntPtr.Zero, 3, 0x40000080, IntPtr.Zero)) {
            if (h.IsInvalid) throw Fail("Opening the device");
            IntPtr iface;
            if (!WinUsb_Initialize(h, out iface)) throw Fail("WinUsb_Initialize");
            try {
                uint timeout = 1000;
                WinUsb_SetPipePolicy(iface, 0, 3 /* PIPE_TRANSFER_TIMEOUT */, 4, ref timeout);
                uint n;
                byte[] version = new byte[2];
                WINUSB_SETUP_PACKET get = new WINUSB_SETUP_PACKET();
                get.RequestType = 0xC0; get.Request = 51; get.Length = 2;
                if (!WinUsb_ControlTransfer(iface, get, version, 2, out n, IntPtr.Zero)) throw Fail("AOA GET_PROTOCOL");
                int v = version[0] | (version[1] << 8);
                if (n < 2 || v < 1) throw new NotSupportedException("The device does not support Android accessory mode");
                for (int i = 0; i < strings.Length; i++) {
                    byte[] text = Encoding.UTF8.GetBytes(strings[i] + "\0");
                    WINUSB_SETUP_PACKET s = new WINUSB_SETUP_PACKET();
                    s.RequestType = 0x40; s.Request = 52; s.Index = (ushort)i; s.Length = (ushort)text.Length;
                    if (!WinUsb_ControlTransfer(iface, s, text, (uint)text.Length, out n, IntPtr.Zero)) throw Fail("AOA SEND_STRING");
                }
                WINUSB_SETUP_PACKET start = new WINUSB_SETUP_PACKET();
                start.RequestType = 0x40; start.Request = 53;
                WinUsb_ControlTransfer(iface, start, null, 0, out n, IntPtr.Zero); // the device may drop off mid-reply
                return v;
            } finally {
                WinUsb_Free(iface);
            }
        }
    }
}
'@

# Must match XDISPLAY_* in host/src/transport/aoa.rs (and the app's accessory_filter.xml).
$AoaStrings = @('XDisplay', 'XDisplay Companion', 'XDisplay Ultra-Low Latency Display', '1.0',
    'https://github.com/24f2006988/xdisplay', '1')

function Get-Bindings {
    if (-not (Test-Path $BindingsKey)) { return @{} }
    $props = Get-ItemProperty $BindingsKey
    $map = @{}
    foreach ($p in $props.PSObject.Properties) {
        if ($p.Name -notlike 'PS*') { $map[$p.Name] = $p.Value }
    }
    return $map
}

function Save-Binding([string]$id, [string]$kind) {
    New-Item -Path $BindingsKey -Force | Out-Null
    New-ItemProperty -Path $BindingsKey -Name $id -Value $kind -PropertyType String -Force | Out-Null
}

# Android file-transfer (MTP) interfaces still on the normal driver: Android announces MTP with
# the Microsoft OS descriptor compatible ID USB\MS_COMP_MTP. Apple devices never do.
function Get-MtpDevices {
    Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object {
        $_.InstanceId -like 'USB\VID_*' -and $_.InstanceId -notlike 'USB\VID_05AC*' -and
        $_.Service -ne 'WinUSB' -and ($_.CompatibleID -contains 'USB\MS_COMP_MTP')
    }
}

# The accessory interface after the switch, when no driver is bound to it: 18D1:2D00 as a whole
# device, or interface 0 of the composite 2D01/2D04/2D05 (accessory plus adb/audio).
function Get-UnboundAccessories {
    Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object {
        ($_.InstanceId -match '^USB\\VID_18D1&PID_2D00\\' -or $_.InstanceId -match '^USB\\VID_18D1&PID_2D0[145]&MI_00\\') -and
        -not $_.Service
    }
}

function Bind-Device($dev, [string]$kind) {
    Say "  Binding WinUSB to $($dev.FriendlyName) ($($dev.InstanceId))..."
    $reboot = [XdUsb]::Bind($dev.InstanceId)
    Save-Binding $dev.InstanceId $kind
    # Windows defers the change when something holds the device open; for USB, re-plugging it
    # loads the new driver just as a restart would.
    if ($reboot) { Say "  The device is in use; the change applies when it is plugged in again." Yellow }
    return $reboot
}

# ---------------------------------------------------------------------------------------------
# Undo
# ---------------------------------------------------------------------------------------------
if ($Undo) {
    $bindings = Get-Bindings
    if ($bindings.Count -eq 0) {
        Say "Nothing to undo: XDisplay USB setup has not changed any device driver."
        Finish 0
    }
    foreach ($id in $bindings.Keys) {
        try {
            [XdUsb]::Remove($id)
            Say "Restored the normal driver for $id"
        } catch {
            Say "Could not reset $id ($($_.Exception.Message)); it is probably gone already." DarkGray
        }
        Remove-ItemProperty -Path $BindingsKey -Name $id -ErrorAction SilentlyContinue
    }
    [XdUsb]::Rescan()
    Say ""
    Say "Done. Unplug and plug the device in again; Windows uses its file-transfer driver again." Green
    Finish 0
}

# ---------------------------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------------------------
Say "=====================================================================" Cyan
Say "  XDisplay USB setup: plug-and-play USB without USB debugging" Cyan
Say "=====================================================================" Cyan
Say "This binds Microsoft's WinUSB driver to your phone/tablet only. While it is bound, Windows"
Say "cannot browse that device's files over USB. Undo any time: XDisplay-USB-Setup.cmd -Undo"
Say "(or 'Undo USB setup' in the Start menu)."
Say ""

$needReboot = $false
try {
    # Left over from an earlier run that stopped half way.
    foreach ($acc in @(Get-UnboundAccessories)) { $needReboot = (Bind-Device $acc 'accessory') -or $needReboot }

    $bindings = Get-Bindings
    $deadline = (Get-Date).AddSeconds($WaitSeconds)
    $hinted = $false
    do {
        $mtp = @(Get-MtpDevices)
        $ready = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object {
            $bindings[$_.InstanceId] -eq 'mtp' -and $_.Service -eq 'WinUSB' })
        $inAccessoryMode = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object {
            $_.InstanceId -match '^USB\\VID_18D1&PID_2D0[0145]' -and $_.Service })
        if ($mtp.Count -or $ready.Count -or $inAccessoryMode.Count) { break }
        if (-not $hinted) {
            Say "Plug in your phone or tablet now. If it asks what the USB connection is for, choose" Yellow
            Say "'File transfer' (MTP). USB debugging can stay off." Yellow
            $hinted = $true
        }
        Start-Sleep -Milliseconds 1000
    } while ((Get-Date) -lt $deadline)

    if (-not ($mtp.Count -or $ready.Count -or $inAccessoryMode.Count)) {
        Say ""
        Say "No Android device in file-transfer mode was found." Yellow
        Say "On the device, pull down the notification 'Charging this device via USB', choose" Yellow
        Say "'File transfer', and run this setup again." Yellow
        Finish 2
    }

    $switchTargets = @()
    foreach ($dev in $mtp) {
        $needReboot = (Bind-Device $dev 'mtp') -or $needReboot
        $switchTargets += $dev.InstanceId
    }
    foreach ($dev in $ready) {
        Say "  $($dev.FriendlyName) is already set up."
        $switchTargets += $dev.InstanceId
    }

    # Ask the device to switch to accessory mode. If the XDisplay host is running it does the
    # same on its own, and whichever opens the device first sends the request.
    foreach ($id in $switchTargets) {
        $until = (Get-Date).AddSeconds(10)
        while ($true) {
            try {
                $v = [XdUsb]::RequestAccessoryMode($id, $AoaStrings)
                Say "  Asked the device to switch to accessory mode (AOA v$v)."
                break
            } catch [System.NotSupportedException] {
                Say "  This device does not support Android accessory mode; use Wi-Fi with it." Yellow
                break
            } catch {
                if ((Get-Date) -ge $until -or -not (Get-PnpDevice -InstanceId $id -PresentOnly -ErrorAction SilentlyContinue)) { break }
                Start-Sleep -Milliseconds 500
            }
        }
    }

    # Bind the accessory device the tablet comes back as.
    Say ""
    Say "Waiting for the device to reconnect as an XDisplay accessory..."
    $deadline = (Get-Date).AddSeconds($WaitSeconds)
    $done = $false
    do {
        foreach ($acc in @(Get-UnboundAccessories)) { $needReboot = (Bind-Device $acc 'accessory') -or $needReboot }
        $bound = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object {
            $_.InstanceId -match '^USB\\VID_18D1&PID_2D0[0145]' -and $_.Service })
        if ($bound.Count) { $done = $true; break }
        Start-Sleep -Milliseconds 1000
    } while ((Get-Date) -lt $deadline)

    Say ""
    if ($done) {
        Say "USB setup finished." Green
        Say "On the device, tap OK on 'Open XDisplay for this USB accessory?' (tick 'Always')." Green
        Say "From now on just plug it in while XDisplay is running on the PC." Green
    } else {
        Say "The device did not come back as an accessory yet. Make sure the XDisplay app is" Yellow
        Say "installed on it, then unplug it, plug it in again and run this setup once more." Yellow
    }
    if ($needReboot) { Say "Unplug the device and plug it in again to finish the driver change." Yellow }
    Finish ($(if ($done) { 0 } else { 3 }))
} catch {
    Say ""
    Say "USB setup failed: $($_.Exception.Message)" Red
    Finish 1
}
