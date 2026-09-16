#ifndef PayloadDir
  #error PayloadDir must point at a verified portable package
#endif
#ifndef OutputDir
  #error OutputDir must be provided by build-windows-installer.ps1
#endif
#ifndef AppVersion
  #error AppVersion must be provided by build-windows-installer.ps1
#endif

#define AppName "MultiCore"
#define AppPublisher "MultiCore"
#define AppExeName "MultiCore.exe"

[Setup]
AppId={{D95AB954-248C-44C9-9E3A-952398802A2D}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={localappdata}\Programs\MultiCore
DefaultGroupName=MultiCore
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=MultiCore-Setup-x64
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
CloseApplicationsFilter=MultiCore.exe,multicore-daemon.exe,multicore-updater.exe,mihomo.exe,xray.exe
RestartApplications=no
UninstallDisplayIcon={app}\current\MultiCore.exe
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoProductVersion={#AppVersion}

[Languages]
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Создать ярлык на рабочем столе"; GroupDescription: "Ярлыки:"; Flags: unchecked
Name: "autostart"; Description: "Запускать MultiCore вместе с Windows"; GroupDescription: "Автозапуск:"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}\current"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\MultiCore"; Filename: "{app}\current\MultiCore.exe"; WorkingDir: "{app}\current"
Name: "{autodesktop}\MultiCore"; Filename: "{app}\current\MultiCore.exe"; WorkingDir: "{app}\current"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "MultiCore"; ValueData: """{app}\current\MultiCore.exe"" --background"; Tasks: autostart
Root: HKCU; Subkey: "Software\Classes\multicore"; ValueType: string; ValueName: ""; ValueData: "URL:MultiCore Protocol"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\multicore"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\multicore\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\current\MultiCore.exe,0"
Root: HKCU; Subkey: "Software\Classes\multicore\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\current\MultiCore.exe"" ""%1"""

[Run]
Filename: "{app}\current\MultiCore.exe"; Description: "Запустить MultiCore"; WorkingDir: "{app}\current"; Verb: "runas"; Flags: postinstall shellexec skipifsilent

[UninstallDelete]
Type: filesandordirs; Name: "{app}\.multicore-previous-*"
Type: filesandordirs; Name: "{app}\.multicore-failed-*"
Type: filesandordirs; Name: "{app}\.multicore-stage-*"

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  AutostartValue: String;
  ExpectedAutostartValue: String;
begin
  if CurUninstallStep <> usUninstall then
    exit;

  ExpectedAutostartValue := '"' + ExpandConstant('{app}\current\MultiCore.exe') + '" --background';
  if RegQueryStringValue(
       HKCU,
       'Software\Microsoft\Windows\CurrentVersion\Run',
       'MultiCore',
       AutostartValue) and (CompareText(AutostartValue, ExpectedAutostartValue) = 0) then
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'MultiCore');
end;
