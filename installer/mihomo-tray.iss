; Установщик Mihomo Tray (Inno Setup). Версия передаётся ключом /DAppVersion=2.4.0
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6E1B2C0A-5C7D-4B8E-9D55-3B7C1F2A4E10}
AppName=Mihomo Tray
AppVersion={#AppVersion}
AppPublisher=1voyance
AppPublisherURL=https://github.com/1voyance/mvptray
DefaultDirName=C:\mihomo\tray-rs
DisableDirPage=no
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=mihomo-tray-setup-v{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\mihomo-tray.exe

[Languages]
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"

[Files]
Source: "..\dist\mihomo-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\autostart.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\install-autostart.bat"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\uninstall-autostart.bat"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Mihomo Tray"; Filename: "{app}\mihomo-tray.exe"

[Run]
Filename: "{app}\mihomo-tray.exe"; Description: "Запустить Mihomo Tray"; Flags: nowait postinstall skipifsilent
