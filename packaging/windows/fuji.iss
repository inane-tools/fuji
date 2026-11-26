; The Windows installer, built in CI with Inno Setup 6:
;   iscc /DVersion=1.2.3 packaging\windows\fuji.iss
; from a release build (target\release\fuji.exe). It installs for the
; current user only, so Fuji's updater can run a newer installer silently
; without asking for administrator rights.

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppId={{747979FA-BFC7-4A91-B3AE-EE49F1346239}
AppName=Fuji
AppVersion={#Version}
AppVerName=Fuji {#Version}
AppPublisher=inane.tools
AppPublisherURL=https://inane.tools
AppSupportURL=https://github.com/inane-tools/fuji
AppUpdatesURL=https://github.com/inane-tools/fuji/releases
VersionInfoVersion={#Version}
DefaultDirName={localappdata}\Programs\Fuji
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=fuji-windows-x86_64-setup
SetupIconFile=fuji.ico
UninstallDisplayIcon={app}\fuji.exe
UninstallDisplayName=Fuji
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; An update closes a Fuji that is still running.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\..\target\release\fuji.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Fuji"; Filename: "{app}\fuji.exe"
Name: "{autodesktop}\Fuji"; Filename: "{app}\fuji.exe"; Tasks: desktopicon

[Run]
; Offered at the end of a normal install.
Filename: "{app}\fuji.exe"; Description: "{cm:LaunchProgram,Fuji}"; Flags: nowait postinstall skipifsilent
; An update (run silently by Fuji) starts the new version by itself.
Filename: "{app}\fuji.exe"; Flags: nowait; Check: WizardSilent

[UninstallDelete]
Type: files; Name: "{app}\fuji.exe.old"
