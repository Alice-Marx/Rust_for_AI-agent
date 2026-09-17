; Rust AI Agent Windows installer
; Compile with Inno Setup 7. The build script passes MyAppVersion from Cargo.toml.

#define MyAppName "Rust AI Agent"
#define MyAppPublisher "AliceMarx"
#define MyAppURL "https://github.com/Alice-Marx/Rust_for_AI-agent"
#define MyAppExeName "agent-desktop.exe"
#ifndef MyAppVersion
  #define MyAppVersion "0.2.0"
#endif
#define BuildRoot "..\..\dist\staging\windows-x64"

[Setup]
AppId={{63B45849-56B5-47DB-B60E-ED0E3099EBD0}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={localappdata}\Programs\Rust AI Agent
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
OutputDir=..\..\dist
OutputBaseFilename=Rust-AI-Agent-Setup-{#MyAppVersion}-x64
SetupArchitecture=x64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
VersionInfoVersion={#MyAppVersion}
VersionInfoCompany={#MyAppPublisher}
VersionInfoDescription={#MyAppName} desktop application and CLI
VersionInfoProductName={#MyAppName}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "chinesesimp"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#BuildRoot}\rust-ai-agent.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildRoot}\agent-desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildRoot}\agent-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildRoot}\cliproxyapi\*"; DestDir: "{app}\cliproxyapi"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "Start-RustAIAgent.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; DestName: "README.md"; Flags: ignoreversion
Source: "{#BuildRoot}\THIRD-PARTY-NOTICES\*"; DestDir: "{app}\THIRD-PARTY-NOTICES"; Flags: ignoreversion recursesubdirs createallsubdirs

[InstallDelete]
; Remove ZIP-era program files after migration. Persistent user data stays in
; {localappdata}\RustAIAgentData and is never removed by this installer.
Type: filesandordirs; Name: "{localappdata}\RustAIAgent"

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\Start-RustAIAgent.ps1"""; WorkingDir: "{app}"; IconFilename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\Start-RustAIAgent.ps1"""; WorkingDir: "{app}"; IconFilename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\Start-RustAIAgent.ps1"""; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

[Code]
function PathContains(const PathValue, Entry: String): Boolean;
var
  Remaining, Segment: String;
  SeparatorAt: Integer;
begin
  Result := False;
  Remaining := PathValue;
  while Remaining <> '' do
  begin
    SeparatorAt := Pos(';', Remaining);
    if SeparatorAt = 0 then
    begin
      Segment := Remaining;
      Remaining := '';
    end
    else
    begin
      Segment := Copy(Remaining, 1, SeparatorAt - 1);
      Delete(Remaining, 1, SeparatorAt);
    end;

    if CompareText(Trim(Segment), Trim(Entry)) = 0 then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

function RemovePathEntry(const PathValue, Entry: String): String;
var
  Remaining, Segment, ResultValue: String;
  SeparatorAt: Integer;
begin
  Remaining := PathValue;
  ResultValue := '';
  while Remaining <> '' do
  begin
    SeparatorAt := Pos(';', Remaining);
    if SeparatorAt = 0 then
    begin
      Segment := Remaining;
      Remaining := '';
    end
    else
    begin
      Segment := Copy(Remaining, 1, SeparatorAt - 1);
      Delete(Remaining, 1, SeparatorAt);
    end;

    if (Trim(Segment) <> '') and (CompareText(Trim(Segment), Trim(Entry)) <> 0) then
    begin
      if ResultValue <> '' then
        ResultValue := ResultValue + ';';
      ResultValue := ResultValue + Segment;
    end;
  end;
  Result := ResultValue;
end;

procedure AddAppDirectoryToUserPath;
var
  UserPath, AppDirectory: String;
begin
  AppDirectory := ExpandConstant('{app}');
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', UserPath) then
    UserPath := '';

  if not PathContains(UserPath, AppDirectory) then
  begin
    if (UserPath <> '') and (Copy(UserPath, Length(UserPath), 1) <> ';') then
      UserPath := UserPath + ';';
    RegWriteExpandStringValue(HKCU, 'Environment', 'Path', UserPath + AppDirectory);
  end;
end;

procedure RemoveAppDirectoryFromUserPath;
var
  UserPath, UpdatedPath, AppDirectory: String;
begin
  AppDirectory := ExpandConstant('{app}');
  if RegQueryStringValue(HKCU, 'Environment', 'Path', UserPath) then
  begin
    UpdatedPath := RemovePathEntry(UserPath, AppDirectory);
    if UpdatedPath <> UserPath then
      RegWriteExpandStringValue(HKCU, 'Environment', 'Path', UpdatedPath);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    AddAppDirectoryToUserPath;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RemoveAppDirectoryFromUserPath;
end;
