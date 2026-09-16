#Requires -Version 5.1
<#
.SYNOPSIS
    Baixa e instala (ou atualiza) o wgtui a partir do último GitHub Release.
.DESCRIPTION
    Equivalente Windows do clássico `curl -sSf URL | sh`:

        irm https://raw.githubusercontent.com/raphaelantoniocampos/wgtui/main/install.ps1 | iex

    Coloca o wgtui.exe em %LOCALAPPDATA%\Programs\wgtui (com o manifesto de
    exemplo em .\examples, para a aba Apps/Scripts já abrir com algo) e
    adiciona essa pasta ao PATH do usuário (sem precisar de Administrador).
    Rodar de novo atualiza para a versão mais recente.
#>

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repo = 'raphaelantoniocampos/wgtui'
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\wgtui'
$exePath = Join-Path $installDir 'wgtui.exe'
$downloadUrl = "https://github.com/$repo/releases/latest/download/wgtui.exe"

Write-Host "Baixando wgtui de $downloadUrl ..."
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Invoke-WebRequest -Uri $downloadUrl -OutFile $exePath

# Remove a marca "baixado da internet" (Zona 3) para não disparar o aviso do
# SmartScreen ao rodar um binário sem assinatura digital.
Unblock-File -Path $exePath

# Manifesto de exemplo, opcional: wgtui procura por *.json em .\examples ao
# lado do próprio exe, então isso já deixa a aba Apps/Scripts com conteúdo.
# Falha aqui não é fatal — o wgtui funciona normalmente sem manifesto.
try {
    $examplesDir = Join-Path $installDir 'examples'
    New-Item -ItemType Directory -Force -Path $examplesDir | Out-Null
    $examplesUrl = "https://raw.githubusercontent.com/$repo/main/examples/packages.json"
    Invoke-WebRequest -Uri $examplesUrl -OutFile (Join-Path $examplesDir 'packages.json')
} catch {
    Write-Warning "Não foi possível baixar o manifesto de exemplo (examples/packages.json): $_"
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$pathEntries = @()
if ($userPath) { $pathEntries = $userPath -split ';' }
if ($pathEntries -notcontains $installDir) {
    $newUserPath = if ($userPath) { "$userPath;$installDir" } else { $installDir }
    [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
    Write-Host "Adicionado $installDir ao PATH do usuário."
}
if (($env:Path -split ';') -notcontains $installDir) {
    $env:Path += ";$installDir"
}

if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
    Write-Warning "winget não encontrado. O wgtui oferece instalar na primeira execução, ou instale o 'App Installer' pela Microsoft Store."
}

Write-Host ""
Write-Host "wgtui instalado em $exePath"
Write-Host "Rode 'wgtui' (terminais novos já verão o PATH atualizado; neste também, já foi aplicado)."
