param(
    [string]$Binary = (Join-Path $PSScriptRoot '..\target\release\device-hub.exe')
)
$ErrorActionPreference = 'Stop'
$Binary = (Resolve-Path $Binary).Path
$Install = Join-Path $env:LOCALAPPDATA 'Programs\DeviceHub'
New-Item -ItemType Directory -Force $Install | Out-Null
$Target = Join-Path $Install 'device-hub.exe'
foreach ($Process in (Get-Process device-hub -ErrorAction SilentlyContinue)) {
    Stop-Process -Id $Process.Id -ErrorAction Stop
    if (-not $Process.WaitForExit(10000)) {
        throw "Device HUB did not exit; cannot replace the running executable."
    }
}
Copy-Item $Binary $Target -Force
foreach ($License in 'LICENSE-MIT','LICENSE-APACHE','THIRD_PARTY_NOTICES.md') {
    Copy-Item (Join-Path $PSScriptRoot "..\$License") $Install -Force
}
Copy-Item (Join-Path $PSScriptRoot '..\licenses') $Install -Recurse -Force
$Shell = New-Object -ComObject WScript.Shell
$Shortcut = $Shell.CreateShortcut((Join-Path ([Environment]::GetFolderPath('Programs')) 'Device HUB.lnk'))
$Shortcut.TargetPath = $Target
$Shortcut.WorkingDirectory = $Install
$Shortcut.IconLocation = "$Target,0"
$Shortcut.Save()
New-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' `
    -Name 'DeviceHub' -Value "`"$Target`" --background" -PropertyType String -Force | Out-Null
Start-Process $Target
Write-Output "Installed: $Target"
Write-Output 'Startup: HKCU Run\DeviceHub (--background). Close button hides to tray; tray menu exits.'
