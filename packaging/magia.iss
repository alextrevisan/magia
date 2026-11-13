; Inno Setup script for the Windows installer.
; usage: iscc /DVersion=0.3.0 /DSrc=<dir with magia.exe and magia-gui.exe> /O<outdir> packaging\magia.iss
; Installs per user (no admin rights): the desktop app, the `magia` CLI (optionally on PATH),
; Start menu and desktop shortcuts, and an uninstaller.

#ifndef Version
  #define Version "0.3.0"
#endif
#ifndef Src
  #define Src "..\target\release"
#endif

[Setup]
AppId={{6D3C1F0A-7B2E-4C55-9A61-8E2F4B7D9C13}
AppName=Magia
AppVersion={#Version}
AppPublisher=Alexandro Trevisan
AppPublisherURL=https://github.com/alextrevisan/magia
DefaultDirName={autopf}\Magia
DefaultGroupName=Magia
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputBaseFilename=magia-{#Version}-windows-x86_64-setup
SetupIconFile=magia.ico
UninstallDisplayIcon={app}\magia.ico
LicenseFile=..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add the magia command to PATH (use it from any terminal)"; GroupDescription: "Command line:"

[Files]
Source: "{#Src}\magia-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Src}\magia.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "magia.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\docs\magia.example.jsonc"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Magia"; Filename: "{app}\magia-gui.exe"; IconFilename: "{app}\magia.ico"
Name: "{group}\Uninstall Magia"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Magia"; Filename: "{app}\magia-gui.exe"; IconFilename: "{app}\magia.ico"; Tasks: desktopicon

[Registry]
; Per-user PATH (HKCU) or system PATH (HKLM) depending on the install mode.
Root: HKA; Subkey: "{code:EnvKey}"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Tasks: addtopath; Check: NeedsAddPath(ExpandConstant('{app}'))

[Run]
Filename: "{app}\magia-gui.exe"; Description: "{cm:LaunchProgram,Magia}"; Flags: nowait postinstall skipifsilent

[Code]
function EnvKey(Param: string): string;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

function NeedsAddPath(Dir: string): Boolean;
var
  Paths: string;
  RootKey: Integer;
begin
  if IsAdminInstallMode then RootKey := HKEY_LOCAL_MACHINE else RootKey := HKEY_CURRENT_USER;
  if not RegQueryStringValue(RootKey, EnvKey(''), 'Path', Paths) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') = 0;
end;

procedure RemoveFromPath(Dir: string);
var
  Paths: string;
  RootKey, P: Integer;
begin
  if IsAdminInstallMode then RootKey := HKEY_LOCAL_MACHINE else RootKey := HKEY_CURRENT_USER;
  if not RegQueryStringValue(RootKey, EnvKey(''), 'Path', Paths) then exit;
  P := Pos(';' + Uppercase(Dir), Uppercase(Paths));
  if P = 0 then exit;
  Delete(Paths, P, Length(Dir) + 1);
  RegWriteExpandStringValue(RootKey, EnvKey(''), 'Path', Paths);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
