#Requires -RunAsAdministrator
param([string]$Account)

$ErrorActionPreference = 'Stop'
$registry = Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\EqualizerAPO' -Name ConfigPath
$configDir = (Resolve-Path -LiteralPath $registry.ConfigPath).Path
$config = Join-Path $configDir 'config.txt'
if (-not (Test-Path -LiteralPath $config -PathType Leaf)) {
    throw "Equalizer APO config.txt was not found in $configDir."
}

$sid = if ($Account) {
    [Security.Principal.NTAccount]::new($Account).Translate([Security.Principal.SecurityIdentifier])
} else {
    [Security.Principal.WindowsIdentity]::GetCurrent().User
}
$profileDir = Join-Path $configDir 'KopuzEQ'
New-Item -ItemType Directory -Path $profileDir -Force | Out-Null
$profile = Join-Path $profileDir 'profile.txt'
if (-not (Test-Path -LiteralPath $profile)) {
    Set-Content -LiteralPath $profile -Encoding ascii -Value '# Kopuz system EQ v1 false 0 0 0 0 0 0'
}

# Give the listening account write access only to the files Kopuz manages.
& icacls.exe $profileDir /grant "*$($sid.Value):(OI)(CI)M"
if ($LASTEXITCODE -ne 0) {
    throw "Could not grant access to $profileDir."
}

$include = 'Include: KopuzEQ\profile.txt'
$registered = Get-Content -LiteralPath $config | Where-Object { $_.Trim() -ieq $include }
if (-not $registered) {
    $backup = Join-Path $configDir "config.before-kopuz-eq.$([Guid]::NewGuid().ToString('N')).txt"
    Copy-Item -LiteralPath $config -Destination $backup
    Add-Content -LiteralPath $config -Value "`r`n# Kopuz system EQ`r`nDevice: all`r`nChannel: ALL`r`n$include"
    Write-Host "Original Equalizer APO configuration backed up to $backup"
}
Write-Host 'Kopuz system EQ is configured. Open Settings > Equalizer in Kopuz.'
