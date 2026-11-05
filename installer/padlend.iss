; PadLend Inno Setup script (Inno Setup 6.3+).
;
; Build through tools/release.py --installer, which passes /DAppVersion=<ver>,
; /O<repo>\dist and /FPadLend-<ver>-setup. Manual build:
;   ISCC.exe /DAppVersion=1.0.0 installer\padlend.iss
;
; The installer is per-user by default ({localappdata}\Programs\PadLend, no
; elevation), matching the app's per-user config in %APPDATA%\PadLend. That
; config directory is intentionally NOT removed on uninstall so upgrades keep
; settings.

#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif

#define MyAppName "PadLend"
#define MyAppPublisher "PadLend contributors"
#define MyAppExeName "PadLend.exe"

[Setup]
AppId={{8F3D5A2E-7C41-4B9E-9A1D-2E6F4C8B1A73}
AppName={#MyAppName}
AppVersion={#AppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={localappdata}\Programs\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir=Output
OutputBaseFilename=PadLend-{#AppVersion}-setup
SetupIconFile=..\assets\padlend.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
RestartApplications=no
MinVersion=10.0
; Keep the version string in Add/Remove Programs readable.
VersionInfoVersion={#AppVersion}

[Languages]
; Inno Setup does not ship Simplified Chinese in every distribution (the CI
; runner only has Default.isl), so keep the installer in English rather than
; failing the release on a missing optional translation.
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\dist\PadLend\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent
