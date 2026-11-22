; Installer for Fractal+ on Windows, built with Inno Setup.
;
; It installs for the current user only, so it needs no administrator rights,
; and installing a newer version over it upgrades it in place.
;
; Usage: iscc /DVersion=X.Y.Z /DSourceDir=PATH\TO\Fractal+ /DIconFile=fractalplus.ico fractalplus.iss

#ifndef Version
  #define Version "0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\Fractal+"
#endif
#ifndef IconFile
  #define IconFile "fractalplus.ico"
#endif

[Setup]
; Never change the AppId: it is how a newer installer finds the installed app.
AppId={{754FDAC6-05BD-5224-9C6B-14DC4DF2181F}
AppName=Fractal+
AppVersion={#Version}
AppVerName=Fractal+ {#Version}
AppPublisher=Fractal+
AppPublisherURL=https://github.com/5TN1rcZRS79VAEFuUCRB/fractalplus
AppSupportURL=https://github.com/5TN1rcZRS79VAEFuUCRB/fractalplus/issues
AppUpdatesURL=https://github.com/5TN1rcZRS79VAEFuUCRB/fractalplus/releases
DefaultDirName={autopf}\Fractal+
DefaultGroupName=Fractal+
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=.
OutputBaseFilename=fractalplus-windows-x86_64-setup
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\fractalplus.ico
UninstallDisplayName=Fractal+
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Close a running Fractal+ before replacing its files, and start it again.
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#IconFile}"; DestDir: "{app}"; DestName: "fractalplus.ico"; Flags: ignoreversion

[InstallDelete]
; Files of an older version that the new one no longer has.
Type: filesandordirs; Name: "{app}\bin"
Type: filesandordirs; Name: "{app}\lib"
Type: filesandordirs; Name: "{app}\libexec"
Type: filesandordirs; Name: "{app}\share"

[Icons]
; The app user model ID lets Windows show the notifications of Fractal+.
Name: "{autoprograms}\Fractal+"; Filename: "{app}\bin\fractalplus.exe"; IconFilename: "{app}\fractalplus.ico"; AppUserModelID: "io.github._5TN1rcZRS79VAEFuUCRB.FractalPlus"
Name: "{autodesktop}\Fractal+"; Filename: "{app}\bin\fractalplus.exe"; IconFilename: "{app}\fractalplus.ico"; AppUserModelID: "io.github._5TN1rcZRS79VAEFuUCRB.FractalPlus"; Tasks: desktopicon

[Run]
Filename: "{app}\bin\fractalplus.exe"; Description: "{cm:LaunchProgram,Fractal+}"; Flags: nowait postinstall skipifsilent
