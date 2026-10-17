; Instalador de Huillín (Inno Setup 6). Uso: iscc /DAppVersion=0.1.0 installer\huillin.iss
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6E2B7C1A-4D53-4F0B-9A6E-5B1F0C3D8A27}
AppName=Huillín
AppVersion={#AppVersion}
AppPublisher=storo
AppPublisherURL=https://github.com/storo/huillin
AppSupportURL=https://github.com/storo/huillin/issues
DefaultDirName={autopf}\Huillín
DefaultGroupName=Huillín
UninstallDisplayIcon={app}\huillin.exe
SetupIconFile=..\ui\huillin.ico
OutputDir=..\dist
OutputBaseFilename=Huillin-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
; Instalación por usuario (sin pedir administrador); el usuario puede elegir "para todos".
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
CloseApplications=yes
LicenseFile=..\LICENSE

[Languages]
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked
Name: "assoc"; Description: "Abrir archivos .torrent y enlaces magnet con Huillín"

[Files]
Source: "..\target\release\huillin.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Huillín"; Filename: "{app}\huillin.exe"
Name: "{autodesktop}\Huillín"; Filename: "{app}\huillin.exe"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Classes\.torrent"; ValueType: string; ValueData: "Huillin.torrent"; Flags: uninsdeletevalue; Tasks: assoc
Root: HKCU; Subkey: "Software\Classes\Huillin.torrent"; ValueType: string; ValueData: "Archivo torrent"; Flags: uninsdeletekey; Tasks: assoc
Root: HKCU; Subkey: "Software\Classes\Huillin.torrent\DefaultIcon"; ValueType: string; ValueData: """{app}\huillin.exe"",0"; Tasks: assoc
Root: HKCU; Subkey: "Software\Classes\Huillin.torrent\shell\open\command"; ValueType: string; ValueData: """{app}\huillin.exe"" ""%1"""; Tasks: assoc
Root: HKCU; Subkey: "Software\Classes\magnet"; ValueType: string; ValueData: "URL:Magnet"; Flags: uninsdeletekey; Tasks: assoc
Root: HKCU; Subkey: "Software\Classes\magnet"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""; Tasks: assoc
Root: HKCU; Subkey: "Software\Classes\magnet\DefaultIcon"; ValueType: string; ValueData: """{app}\huillin.exe"",0"; Tasks: assoc
Root: HKCU; Subkey: "Software\Classes\magnet\shell\open\command"; ValueType: string; ValueData: """{app}\huillin.exe"" ""%1"""; Tasks: assoc

[Run]
Filename: "{app}\huillin.exe"; Description: "{cm:LaunchProgram,Huillín}"; Flags: nowait postinstall skipifsilent

[Code]
// Huillín necesita WebView2 Runtime (incluido en Windows 11 y en Windows 10 actualizado).
function WebView2Installed: Boolean;
var v: String;
begin
  Result := RegQueryStringValue(HKLM32, 'SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', v) and (v <> '') and (v <> '0.0.0.0');
  if not Result then
    Result := RegQueryStringValue(HKCU, 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', v) and (v <> '') and (v <> '0.0.0.0');
end;

function InitializeSetup: Boolean;
var code: Integer;
begin
  Result := True;
  if not WebView2Installed then
    if MsgBox('Huillín necesita Microsoft WebView2 Runtime y no se ha detectado.' + #13#10 +
              '¿Abrir la página de descarga ahora? (puedes continuar con la instalación después)',
              mbConfirmation, MB_YESNO) = IDYES then
      ShellExec('open', 'https://developer.microsoft.com/microsoft-edge/webview2/', '', '', SW_SHOW, ewNoWait, code);
end;
