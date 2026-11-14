; The Windows installer. scripts/package.sh --installers compiles it with
; AppVersion, Payload (the folder holding pw.exe) and LicenseFile defined,
; and names the output.
#define AppName "process-watch"
#define AppExe "pw.exe"
#define AppUrl "https://github.com/danpozmanter/process-watch"

[Setup]
AppId={{D0E36597-BE56-4BD9-BD1A-44B85360873A}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=process-watch contributors
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
AlwaysShowDirOnReadyPage=yes
; A per-user install needs no administrator; the dialog offers an all-users one.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
LicenseFile={#LicenseFile}
UninstallDisplayIcon={app}\{#AppExe}
UninstallFilesDir={app}
ChangesEnvironment=yes
; A portable extraction leaves nothing to uninstall: deleting the folder
; removes pw and its recordings.
Uninstallable=not WizardIsTaskSelected('portablemode')

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "envpath"; Description: "Add pw to PATH"
Name: "recorder"; Description: "Start the recorder now and at every sign-in"
Name: "portablemode"; Description: "Extract only, keeping recordings in the folder (portable mode)"; Flags: unchecked

[Files]
Source: "{#Payload}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs

[Dirs]
Name: "{app}\data"; Tasks: portablemode

; The recorder runs without a console window under a headless console host.
[Icons]
Name: "{autostartup}\process-watch recorder"; Filename: "{sys}\conhost.exe"; Parameters: "--headless ""{app}\{#AppExe}"" record"; Tasks: recorder and not portablemode

[Run]
Filename: "{sys}\conhost.exe"; Parameters: "--headless ""{app}\{#AppExe}"" record"; Flags: nowait runhidden; Tasks: recorder and not portablemode

[Code]
const
  SystemEnvironment = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';
  UserEnvironment = 'Environment';

{ An all-users install edits the system PATH, a per-user one the user's. }
function EnvironmentKey(var Root: Integer): String;
begin
  if IsAdminInstallMode then begin
    Root := HKEY_LOCAL_MACHINE;
    Result := SystemEnvironment;
  end else begin
    Root := HKEY_CURRENT_USER;
    Result := UserEnvironment;
  end;
end;

procedure AddToPath(Dir: String);
var
  Root: Integer;
  Key, Path: String;
begin
  Key := EnvironmentKey(Root);
  if not RegQueryStringValue(Root, Key, 'Path', Path) then
    Path := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') > 0 then
    Exit;
  if (Path <> '') and (Path[Length(Path)] <> ';') then
    Path := Path + ';';
  RegWriteExpandStringValue(Root, Key, 'Path', Path + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Root, At: Integer;
  Key, Path: String;
begin
  Key := EnvironmentKey(Root);
  if not RegQueryStringValue(Root, Key, 'Path', Path) then
    Exit;
  Path := ';' + Path + ';';
  At := Pos(';' + Uppercase(Dir) + ';', Uppercase(Path));
  if At = 0 then
    Exit;
  Delete(Path, At, Length(Dir) + 1);
  RegWriteExpandStringValue(Root, Key, 'Path', Copy(Path, 2, Length(Path) - 2));
end;

{ Stops the recorder this installation runs, and no other pw, so its files
  can be replaced or removed. The path is quoted for PowerShell, which
  doubles a quote inside single quotes. }
procedure StopRecorder;
var
  Exe: String;
  ResultCode: Integer;
begin
  Exe := ExpandConstant('{app}\{#AppExe}');
  StringChangeEx(Exe, '''', '''''', True);
  Exec('powershell.exe',
    '-NoProfile -NonInteractive -Command "Get-Process pw -ErrorAction SilentlyContinue | ' +
    'Where-Object Path -eq ''' + Exe + ''' | Stop-Process -Force"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    StopRecorder;
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('envpath')
      and not WizardIsTaskSelected('portablemode') then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    StopRecorder;
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
