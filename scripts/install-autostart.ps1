# Register Device HUB to start at Windows logon (HKCU Run) with --minimized.
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\install-autostart.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\install-autostart.ps1 -Uninstall
param(
    [string]$Exe = "",
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$key = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"

if ($Uninstall) {
    Remove-ItemProperty -Path $key -Name "DeviceHub" -ErrorAction SilentlyContinue
    Write-Output "Device HUB autostart removed."
    exit 0
}

if (-not $Exe) {
    $candidates = @(
        (Join-Path $root "target\release\device-hub.exe"),
        (Join-Path $root "target\debug\device-hub.exe")
    )
    $Exe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Exe -or -not (Test-Path $Exe)) {
    throw "device-hub.exe not found. Run 'cargo build --release -p device-hub' first."
}
$path = (Resolve-Path $Exe).Path
Set-ItemProperty -Path $key -Name "DeviceHub" -Value "`"$path`" --minimized"
Write-Output "Device HUB autostart registered: $path --minimized"
