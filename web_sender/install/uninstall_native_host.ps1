# Removes the web_receiver Native Messaging Host registration written by
# install_native_host.ps1. Tolerates missing entries (idempotent cleanup).
#
# Usage (run from workspace root):
#   powershell -ExecutionPolicy Bypass -File .\web_sender\install\uninstall_native_host.ps1

[CmdletBinding()]
param()

$ErrorActionPreference = "Continue"

$HostName     = "com.hourglass.web_receiver"
$ManifestPath = Join-Path $env:LOCALAPPDATA "Hourglass\$HostName.json"

$RegKeys = @(
    "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName",
    "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\$HostName"
)

foreach ($k in $RegKeys) {
    if (Test-Path $k) {
        Remove-Item -Path $k -Recurse -Force
        Write-Host "[ok] removed registry key: $k"
    } else {
        Write-Host "[skip] not present: $k"
    }
}

if (Test-Path $ManifestPath) {
    Remove-Item -Path $ManifestPath -Force
    Write-Host "[ok] removed manifest: $ManifestPath"
} else {
    Write-Host "[skip] not present: $ManifestPath"
}

Write-Host ""
Write-Host "uninstall complete. The browser may still hold the host name in memory until it is restarted."
