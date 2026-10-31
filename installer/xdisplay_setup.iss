; ==============================================================================
; XDisplay installer (Inno Setup 6)
;
; Installs everything the Windows side needs, with no other downloads:
;   - xdisplay-host.exe (dashboard + tray app that streams to the phone/tablet)
;   - the virtual display driver (MttVDD, MIT licensed) that gives Windows a real second monitor
;   - Windows Firewall rules for Wi-Fi (local network only, optional)
;   - UsbDk (Red Hat, Apache-2.0) for plug-and-play USB without USB debugging
;   - the Android app (APK) for copying to a device
;
; Build: scripts\build_release.ps1 (or ISCC.exe installer\xdisplay_setup.iss after building
; the host and the release APK).
; ==============================================================================

#define MyAppName "XDisplay"
#ifndef MyAppVersion
  #define MyAppVersion "1.0.0"
#endif
#define MyAppPublisher "XDisplay contributors"
#define MyAppURL "https://github.com/24f2006988/xdisplay-app"
#define MyAppExeName "xdisplay-host.exe"
#define VddDir "{sd}\VirtualDisplayDriver"

[Setup]
AppId={{E584286C-49F1-4E65-B62D-8D5257BCE902}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
OutputDir=..\dist
OutputBaseFilename=XDisplay-Setup-{#MyAppVersion}
SetupIconFile=..\host\assets\xdisplay.ico
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.18362
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
VersionInfoVersion={#MyAppVersion}
VersionInfoDescription={#MyAppName} installer
; The host is closed explicitly in PrepareToInstall (it must detach its monitor first).
CloseApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "autostart"; Description: "Start XDisplay in the tray when I sign in to Windows"; GroupDescription: "Startup:"; Flags: unchecked
Name: "firewall"; Description: "Allow phones and tablets on my Wi-Fi to connect (Windows Firewall: TCP 59100 and UDP 59101, local network only)"; GroupDescription: "Wi-Fi:"; Flags: checkedonce
Name: "usbdk"; Description: "Install the plug-and-play USB driver (UsbDk), so USB works without USB debugging (needs a restart)"; GroupDescription: "USB:"; Flags: checkedonce

[Files]
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
; Virtual display driver package and its display modes
Source: "redist\vdd\*"; DestDir: "{app}\driver\vdd"; Flags: ignoreversion
Source: "..\driver\vdd_settings.xml"; DestDir: "{#VddDir}"; Flags: ignoreversion
; Wi-Fi helpers (firewall rules, connection troubleshooter)
Source: "..\scripts\setup_wifi_firewall.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "..\scripts\try_wifi.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
; UsbDk (Red Hat, signed)
Source: "redist\UsbDk_1.0.22_x64.msi"; DestDir: "{tmp}"; Flags: deleteafterinstall; Tasks: usbdk
Source: "redist\UsbDk-LICENSE.txt"; DestDir: "{app}\licenses"; DestName: "UsbDk-Apache-2.0.txt"; Flags: ignoreversion
; Android app, for copying to a phone or tablet
Source: "..\dist\XDisplay-{#MyAppVersion}.apk"; DestDir: "{app}\android"; DestName: "XDisplay.apk"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Wi-Fi troubleshooter"; Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -NoExit -File ""{app}\scripts\try_wifi.ps1"""
Name: "{group}\Android app (APK)"; Filename: "{app}\android"
Name: "{group}\Log files"; Filename: "{localappdata}\XDisplay\logs"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Dirs]
Name: "{localappdata}\XDisplay\logs"

[Registry]
; Where the virtual display driver reads its display modes from
Root: HKLM; Subkey: "SOFTWARE\MikeTheTech\VirtualDisplayDriver"; ValueType: string; ValueName: "VDDPATH"; ValueData: "{#VddDir}"
Root: HKLM; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: """{app}\{#MyAppExeName}"" --minimized"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
; (The virtual display driver is installed from [Code], in CurStepChanged(ssPostInstall).)
; One-shot cleanup at each sign-in: detaches a monitor left over by a crash or reboot (exits immediately)
Filename: "schtasks.exe"; Parameters: "/Create /TN ""XDisplay\Detach Virtual Display"" /TR ""\""{app}\{#MyAppExeName}\"" --detach-display-if-host-absent"" /SC ONLOGON /RL LIMITED /F"; Flags: runhidden; StatusMsg: "Configuring display safety cleanup..."
; Wi-Fi: stream (TCP) and discovery (UDP), local subnet only, Private/Domain networks
Filename: "netsh.exe"; Parameters: "advfirewall firewall delete rule name=""XDisplay Host (Wi-Fi stream, TCP 59100)"""; Flags: runhidden; Tasks: firewall
Filename: "netsh.exe"; Parameters: "advfirewall firewall delete rule name=""XDisplay Host (Wi-Fi discovery, UDP 59101)"""; Flags: runhidden; Tasks: firewall
Filename: "netsh.exe"; Parameters: "advfirewall firewall add rule name=""XDisplay Host (Wi-Fi stream, TCP 59100)"" dir=in action=allow protocol=TCP localport=59100 profile=private,domain remoteip=localsubnet"; Flags: runhidden; Tasks: firewall; StatusMsg: "Configuring Windows Firewall for Wi-Fi..."
Filename: "netsh.exe"; Parameters: "advfirewall firewall add rule name=""XDisplay Host (Wi-Fi discovery, UDP 59101)"" dir=in action=allow protocol=UDP localport=59101 profile=private,domain remoteip=localsubnet"; Flags: runhidden; Tasks: firewall
; Plug-and-play USB (Android Open Accessory over UsbDk). Left installed on uninstall: other
; software may share it.
Filename: "msiexec.exe"; Parameters: "/i ""{tmp}\UsbDk_1.0.22_x64.msi"" /qn /norestart"; Flags: runhidden; Tasks: usbdk; StatusMsg: "Installing the plug-and-play USB driver..."
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "taskkill.exe"; Parameters: "/IM {#MyAppExeName} /F"; Flags: runhidden; RunOnceId: "StopHost"
Filename: "{app}\{#MyAppExeName}"; Parameters: "--detach-display-if-host-absent"; Flags: runhidden; RunOnceId: "DetachDisplay"
Filename: "{app}\{#MyAppExeName}"; Parameters: "--remove-watchdog"; Flags: runhidden; RunOnceId: "StopWatchdog"
Filename: "schtasks.exe"; Parameters: "/Delete /TN ""XDisplay\Detach Virtual Display"" /F"; Flags: runhidden; RunOnceId: "RemoveDisplaySafetyTask"
Filename: "netsh.exe"; Parameters: "advfirewall firewall delete rule name=""XDisplay Host (Wi-Fi stream, TCP 59100)"""; Flags: runhidden; RunOnceId: "FwTcp"
Filename: "netsh.exe"; Parameters: "advfirewall firewall delete rule name=""XDisplay Host (Wi-Fi discovery, UDP 59101)"""; Flags: runhidden; RunOnceId: "FwUdp"
Filename: "netsh.exe"; Parameters: "advfirewall firewall delete rule name=""XDisplay Host Stream"""; Flags: runhidden; RunOnceId: "FwLegacy"

[Code]
const
  StateKey = 'Software\XDisplay';

var
  UsbDkWasPresent: Boolean;
  DriverNeedsRestart: Boolean;

function UsbDkPresent(): Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\UsbDk');
end;

function InitializeSetup(): Boolean;
begin
  UsbDkWasPresent := UsbDkPresent();
  DriverNeedsRestart := False;
  Result := True;
end;

// Close a running host (and its display guard) before files are replaced, then make sure the
// virtual monitor is not left on the desktop.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
  Existing: String;
begin
  Result := '';
  Exec('taskkill.exe', '/IM {#MyAppExeName} /F', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Existing := ExpandConstant('{app}\{#MyAppExeName}');
  if FileExists(Existing) then
  begin
    Exec(Existing, '--stop-watchdog', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Exec(Existing, '--detach-display-if-host-absent', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;
  RegDeleteValue(HKLM, 'Software\Microsoft\Windows\CurrentVersion\Run', 'XDisplayWatchdog');
end;

// Installs the virtual display driver: registers its device on a first install, updates the
// driver otherwise. Records whether this installer created the device, so uninstall removes
// only what it added (another program may use the same driver).
procedure InstallDisplayDriver();
var
  Exe: String;
  Code: Integer;
  Created: Cardinal;
begin
  Exe := ExpandConstant('{app}\{#MyAppExeName}');
  WizardForm.StatusLabel.Caption := 'Installing the virtual display driver...';
  if not RegQueryDWordValue(HKLM, StateKey, 'CreatedDisplayDevice', Created) then
  begin
    // --query-display-driver exits 0 when the device already exists.
    if Exec(Exe, '--query-display-driver', '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code <> 0) then
      RegWriteDWordValue(HKLM, StateKey, 'CreatedDisplayDevice', 1)
    else
      RegWriteDWordValue(HKLM, StateKey, 'CreatedDisplayDevice', 0);
  end;
  if not Exec(Exe, '--install-display-driver "' + ExpandConstant('{app}\driver\vdd\MttVDD.inf') + '"', '', SW_HIDE, ewWaitUntilTerminated, Code) then
    Code := -1;
  if Code = 3010 then
    DriverNeedsRestart := True
  else if Code <> 0 then
    SuppressibleMsgBox('The virtual display driver could not be installed (code ' + IntToStr(Code) + '). ' +
      'XDisplay will mirror your main screen instead of extending it.' + #13#10 + #13#10 +
      'Run the installer again, or see the log files in %LOCALAPPDATA%\XDisplay\logs.', mbError, MB_OK, IDOK);
  // Keep the new monitor off the desktop until a device connects.
  Exec(Exe, '--detach-display-if-host-absent', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    InstallDisplayDriver();
end;

function NeedRestart(): Boolean;
begin
  // UsbDk attaches to the USB controllers only when they start.
  Result := DriverNeedsRestart or ((not UsbDkWasPresent) and UsbDkPresent());
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Created: Cardinal;
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    if RegQueryDWordValue(HKLM, StateKey, 'CreatedDisplayDevice', Created) and (Created = 1) then
      Exec(ExpandConstant('{app}\{#MyAppExeName}'), '--uninstall-display-driver', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    RegDeleteKeyIncludingSubkeys(HKLM, StateKey);
  end;
end;
