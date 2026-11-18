# Remove Logitech G HUB and hand the G502 HERO over to Device HUB.
# Must be run as Administrator (UAC prompt).
#
#   powershell -ExecutionPolicy Bypass -File scripts\replace-ghub.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\replace-ghub.ps1 -KeepAutostart:$false
param(
    [switch]$SkipUninstall,
    [bool]$KeepAutostart = $true
)

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p = New-Object Security.Principal.WindowsPrincipal($id)
    return $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-Admin)) {
    Write-Error "Run this script as Administrator."
    exit 1
}

Write-Output "== stopping G HUB =="
Get-Process -Name lghub*,logi_* -ErrorAction SilentlyContinue |
    ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
Get-Service -Name LGHUBUpdaterService,logi_lamparray_service -ErrorAction SilentlyContinue |
    ForEach-Object { Stop-Service -Name $_.Name -Force -ErrorAction SilentlyContinue }

if (-not $SkipUninstall) {
    $updater = "C:\Program Files\LGHUB\lghub_updater.exe"
    if (Test-Path $updater) {
        Write-Output "== uninstalling G HUB (silent) =="
        Start-Process -FilePath $updater -ArgumentList "--uninstall", "--full" -Wait
    } else {
        $mgr = "C:\Program Files\LGHUB\lghub_software_manager.exe"
        if (Test-Path $mgr) {
            Write-Output "== lghub_updater.exe not found; launching software manager =="
            Start-Process -FilePath $mgr -Wait
        } else {
            Write-Output "G HUB is not installed."
        }
    }
}

# Give the uninstaller a moment to release the HID++ interface.
Start-Sleep -Seconds 2

if ($KeepAutostart) {
    Write-Output "== registering Device HUB autostart =="
    & (Join-Path $PSScriptRoot "install-autostart.ps1")
}

$exe = Join-Path $root "target\release\device-hub.exe"
if (Test-Path $exe) {
    Write-Output "== starting Device HUB =="
    Start-Process -FilePath $exe
} else {
    Write-Output "target\release\device-hub.exe not found. Build it and start it manually."
}
