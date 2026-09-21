; Wonderland Windows installer
; Compile with Inno Setup 7. The build script passes MyAppVersion from Cargo.toml.

#define MyAppName "Wonderland"
#define MyAppPublisher "AliceMarx"
#define MyAppURL "https://github.com/Alice-Marx/Rust_for_AI-agent"
#define MyAppExeName "wonderland-desktop.exe"
#ifndef MyAppVersion
  #define MyAppVersion "0.9.0"
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
DefaultDirName={localappdata}\Programs\Wonderland
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
OutputDir=..\..\dist
OutputBaseFilename=Wonderland-Setup-{#MyAppVersion}-x64
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
Compression=lzma2/ultra64
SolidCompression=yes
SetupIconFile=..\..\assets\icons\icon.ico
WizardStyle=modern
ChangesEnvironment=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
VersionInfoVersion={#MyAppVersion}
VersionInfoCompany={#MyAppPublisher}
VersionInfoDescription={#MyAppName} desktop application and CLI
VersionInfoProductName={#MyAppName}

; 简体中文语言包是 Inno 的非官方翻译，需要单独安装；缺失时只打包英文向导，
; 应用本身的界面仍然是中文。
[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
#if FileExists(AddBackslash(CompilerPath) + "Languages\ChineseSimplified.isl")
Name: "chinesesimp"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
#endif

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#BuildRoot}\wonderland.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildRoot}\wonderland-desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildRoot}\wonderland-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildRoot}\*.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#BuildRoot}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildRoot}\docs\*"; DestDir: "{app}\docs"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#BuildRoot}\cliproxyapi\*"; DestDir: "{app}\cliproxyapi"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "Start-Wonderland.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; DestName: "README.md"; Flags: ignoreversion
Source: "{#BuildRoot}\THIRD-PARTY-NOTICES\*"; DestDir: "{app}\THIRD-PARTY-NOTICES"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\Start-Wonderland.ps1"""; WorkingDir: "{app}"; IconFilename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\Start-Wonderland.ps1"""; WorkingDir: "{app}"; IconFilename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\Start-Wonderland.ps1"""; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

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
