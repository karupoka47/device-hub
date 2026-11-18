param(
    [string]$Binary = (Join-Path $PSScriptRoot '..\target\release\deka-hub.exe')
)
$ErrorActionPreference = 'Stop'
$Binary = (Resolve-Path $Binary).Path
$Install = Join-Path $env:LOCALAPPDATA 'Programs\DekaHub'
New-Item -ItemType Directory -Force $Install | Out-Null
$Target = Join-Path $Install 'deka-hub.exe'
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class DekaInstall {
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string t);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
}
'@
$Tray = [DekaInstall]::FindWindow('DeviceHub.Tray', 'DeviceHub.Tray')
if ($Tray -ne [IntPtr]::Zero) { [DekaInstall]::PostMessage($Tray, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null }
foreach ($Process in (Get-Process device-hub,deka-hub -ErrorAction SilentlyContinue)) {
    if (-not $Process.WaitForExit(5000)) { Stop-Process -Id $Process.Id -ErrorAction Stop }
    if (-not $Process.WaitForExit(10000)) {
        throw "Deka Hub did not exit; cannot replace the running executable."
    }
}
Copy-Item $Binary $Target -Force
foreach ($License in 'LICENSE-MIT','LICENSE-APACHE','THIRD_PARTY_NOTICES.md') {
    Copy-Item (Join-Path $PSScriptRoot "..\$License") $Install -Force
}
Copy-Item (Join-Path $PSScriptRoot '..\licenses') $Install -Recurse -Force
$Shell = New-Object -ComObject WScript.Shell
$Shortcut = $Shell.CreateShortcut((Join-Path ([Environment]::GetFolderPath('Programs')) 'Deka Hub.lnk'))
$Shortcut.TargetPath = $Target
$Shortcut.WorkingDirectory = $Install
$Shortcut.IconLocation = "$Target,0"
$Shortcut.Save()
New-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' `
    -Name 'DekaHub' -Value "`"$Target`" --background" -PropertyType String -Force | Out-Null
Remove-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name DeviceHub -ErrorAction SilentlyContinue
$OldShortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Device HUB.lnk'
if (Test-Path $OldShortcut) {
    $Old = $Shell.CreateShortcut($OldShortcut)
    if ($Old.TargetPath -eq (Join-Path $env:LOCALAPPDATA 'Programs\DeviceHub\device-hub.exe')) { Remove-Item $OldShortcut }
}
# Remove only the old installed binary; preferences and backups retain compatible paths.
$OldBinary = Join-Path $env:LOCALAPPDATA 'Programs\DeviceHub\device-hub.exe'
if (Test-Path $OldBinary) { Remove-Item $OldBinary }
Start-Process $Target
Write-Output "Installed: $Target"
Write-Output 'Startup: HKCU Run\DekaHub (--background). Close button hides to tray; tray menu exits.'
