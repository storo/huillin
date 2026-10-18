<#
Firma un ejecutable con un certificado de Code Signing ya instalado.
Requiere SignTool del Windows SDK y acceso a la clave privada (token/HSM).
Firmar huillin.exe ANTES de empaquetar; después firmar el instalador.
No escribir contraseñas ni exportar claves privadas a este repositorio.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string[]]$FilePath,
    [Parameter(Mandatory)][ValidatePattern('^[A-Fa-f0-9]{40}$')][string]$Thumbprint,
    [string]$TimestampServer = 'http://timestamp.digicert.com',
    [string]$SignTool,
    [switch]$LocalMachine
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows -and $PSVersionTable.PSEdition -eq 'Core') { throw 'Requiere Windows.' }
if (-not $SignTool) {
    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($command) { $SignTool = $command.Source }
    else {
        $sdk = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
        $SignTool = Get-ChildItem -LiteralPath $sdk -Filter signtool.exe -Recurse |
            Where-Object FullName -Match '\\x64\\signtool.exe$' |
            Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
    }
}
if (-not $SignTool -or -not (Test-Path -LiteralPath $SignTool)) { throw 'Instala las herramientas de firma del Windows SDK o indica -SignTool.' }
$store = if ($LocalMachine) { 'LocalMachine' } else { 'CurrentUser' }
$cert = Get-Item -LiteralPath "Cert:\$store\My\$Thumbprint"
if (-not $cert.HasPrivateKey) { throw 'El certificado no tiene acceso a su clave privada.' }
if ($cert.NotBefore -gt (Get-Date) -or $cert.NotAfter -lt (Get-Date)) { throw 'El certificado está fuera de su período de validez.' }
if ('1.3.6.1.5.5.7.3.3' -notin @($cert.EnhancedKeyUsageList.ObjectId.Value)) { throw 'El certificado no permite Code Signing.' }
$paths = @($FilePath | ForEach-Object { (Resolve-Path -LiteralPath $_).Path })
foreach ($path in $paths) {
    $signArgs = @('sign', '/sha1', $Thumbprint, '/s', 'My', '/fd', 'SHA256', '/tr', $TimestampServer, '/td', 'SHA256')
    if ($LocalMachine) { $signArgs += '/sm' }
    & $SignTool @signArgs $path
    if ($LASTEXITCODE -ne 0) { throw "No se pudo firmar: $path" }
    & $SignTool verify /pa /all /v $path
    if ($LASTEXITCODE -ne 0) { throw "Falló la verificación: $path" }
    $signature = Get-AuthenticodeSignature -LiteralPath $path
    if ($signature.Status -ne 'Valid' -or -not $signature.TimeStamperCertificate) { throw "Firma inválida o sin sello de tiempo: $path" }
    Get-FileHash -LiteralPath $path -Algorithm SHA256
}
