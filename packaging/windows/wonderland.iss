; Wonderland Windows installer
; Compile with Inno Setup 7. The build script passes MyAppVersion from Cargo.toml.

#define MyAppName "Wonderland"
#define MyAppPublisher "AliceMarx"
#define MyAppURL "https://github.com/Alice-Marx/Rust_for_AI-agent"
#define MyAppExeName "wonderland-desktop.exe"
#ifndef MyAppVersion
  #define MyAppVersion "0.11.2"
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
; 安装目录与数据目录都必须可选，用户可以全部放到非系统盘。
DisableDirPage=no
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
; The update helper passes /CLOSEAPPLICATIONS. Restart Manager examines only
; executable and DLL files installed below {app}; it does not select a process
; merely because it listens on the same localhost port.
CloseApplications=yes
CloseApplicationsFilter=*.exe,*.dll
RestartApplications=no
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
var
  DataDirPage: TInputDirWizardPage;
  DataDirWasExisting: Boolean;
  DataDirAutoSelected: Boolean;

function DefaultDataDirectory: String;
var
  InstallDrive, ProfileDrive: String;
begin
  InstallDrive := ExtractFileDrive(ExpandConstant('{app}'));
  ProfileDrive := ExtractFileDrive(ExpandConstant('{localappdata}'));
  if (InstallDrive <> '') and (CompareText(InstallDrive, ProfileDrive) <> 0) then
    Result := AddBackslash(InstallDrive) + 'WonderlandData'
  else
    Result := ExpandConstant('{localappdata}') + '\WonderlandData';
end;

function ExistingDataDirectory: String;
var
  StoredValue: String;
  StoredFileValue: AnsiString;
begin
  Result := '';

  { The installed marker is the source of truth for upgrades in the same
    directory.  It must be read before the page receives a default value. }
  if LoadStringFromFile(AddBackslash(ExpandConstant('{app}')) +
     'data-location.txt', StoredFileValue) then
  begin
    StoredValue := Trim(StoredFileValue);
    if StoredValue <> '' then
    begin
      Result := StoredValue;
      Exit;
    end;
  end;

  { Keep the location even when the user moves the program directory during
    an upgrade.  Older installers did not record this value, hence the
    environment-variable fallback below. }
  StoredValue := Trim(GetPreviousData('WonderlandDataDirectory', ''));
  if StoredValue <> '' then
  begin
    Result := StoredValue;
    Exit;
  end;

  StoredValue := Trim(GetEnv('AGENT_DATA_DIR'));
  if StoredValue <> '' then
    Result := StoredValue;
end;

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
  begin
    AddAppDirectoryToUserPath;
    SaveStringToFile(ExpandConstant('{app}') + '\data-location.txt',
      DataDirPage.Values[0], False);
  end;
end;

procedure RegisterPreviousData(PreviousDataKey: Integer);
begin
  SetPreviousData(PreviousDataKey, 'WonderlandDataDirectory',
    DataDirPage.Values[0]);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RemoveAppDirectoryFromUserPath;
end;

procedure InitializeWizard;
var
  StoredDataDir: String;
begin
  DataDirPage := CreateInputDirPage(wpSelectDir,
    '选择数据目录',
    'Wonderland 的任务数据、数据库和日志将保存到这里。',
    '如果不希望占用 C 盘，请选择其他磁盘上的目录。目录不存在时会自动创建。' + #13#10 +
    '提示：数据目录与安装目录相互独立；卸载程序不会删除数据目录。',
    False,
    '');
  DataDirPage.Add('');
  StoredDataDir := ExistingDataDirectory;
  DataDirWasExisting := StoredDataDir <> '';
  DataDirAutoSelected := not DataDirWasExisting;
  if DataDirWasExisting then
    DataDirPage.Values[0] := StoredDataDir
  else
    DataDirPage.Values[0] := DefaultDataDirectory;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = wpSelectDir then
  begin
    { A fresh install follows a newly selected non-system drive.  An upgrade
      keeps its recorded data directory, and a value accepted on the data
      page is treated as an explicit user choice. }
    if (not DataDirWasExisting) and DataDirAutoSelected then
      DataDirPage.Values[0] := DefaultDataDirectory;
  end
  else if CurPageID = DataDirPage.ID then
  begin
    DataDirAutoSelected := False;
  end;
end;

