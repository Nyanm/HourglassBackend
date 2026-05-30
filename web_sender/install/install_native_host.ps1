# Registers the web_receiver Native Messaging Host with Chrome and / or Edge.
#
# Usage (run from workspace root):
#   powershell -ExecutionPolicy Bypass -File .\web_sender\install\install_native_host.ps1 `
#       -ChromeExtId <chrome-ext-id> -EdgeExtId <edge-ext-id>
#
# At least one of -ChromeExtId / -EdgeExtId must be supplied. -ReceiverExe is
# optional; when omitted the script tries target\release\web_receiver.exe then
# target\debug\web_receiver.exe under the workspace root (two levels up from
# this script's own directory).
#
# HKCU only -- no admin elevation needed. Re-running is idempotent.

[CmdletBinding()]
param(
    [string]$ChromeExtId,
    [string]$EdgeExtId,
    [string]$ReceiverExe
)

$ErrorActionPreference = "Stop"

$HostName        = "com.hourglass.web_receiver"
$HostDescription = "Hourglass web sensor receiver"

# ----- locate web_receiver.exe ------------------------------------------------
# script lives at <root>\web_sender\install\install_native_host.ps1, so workspace root is two levels up
$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Resolve-Path (Join-Path $ScriptDir "..\..")

if (-not $ReceiverExe) {
    $Candidates = @(
        (Join-Path $ProjectRoot "target\release\web_receiver.exe"),
        (Join-Path $ProjectRoot "target\debug\web_receiver.exe")
    )
    foreach ($c in $Candidates) {
        if (Test-Path $c) { $ReceiverExe = $c; break }
    }
}
if (-not $ReceiverExe -or -not (Test-Path $ReceiverExe)) {
    throw "web_receiver.exe not found. Build it first (cargo build --release -p web_receiver) or pass -ReceiverExe <abs path>."
}
$ReceiverExe = (Resolve-Path $ReceiverExe).Path

# ----- assemble allowed_origins ----------------------------------------------
$AllowedOrigins = @()
if ($ChromeExtId) { $AllowedOrigins += "chrome-extension://$ChromeExtId/" }
if ($EdgeExtId)   { $AllowedOrigins += "chrome-extension://$EdgeExtId/" }
if ($AllowedOrigins.Count -eq 0) {
    throw "Provide at least one of -ChromeExtId or -EdgeExtId. Get the id from chrome://extensions or edge://extensions after loading web_sender as an unpacked extension."
}

# ----- write host manifest ---------------------------------------------------
$HostDir = Join-Path $env:LOCALAPPDATA "Hourglass"
if (-not (Test-Path $HostDir)) { New-Item -ItemType Directory -Path $HostDir | Out-Null }

$ManifestPath = Join-Path $HostDir "$HostName.json"

$Manifest = [ordered]@{
    name            = $HostName
    description     = $HostDescription
    path            = $ReceiverExe
    type            = "stdio"
    allowed_origins = $AllowedOrigins
}

# UTF-8 without BOM; Chromium parses host manifests with strict JSON expectations
$Json = $Manifest | ConvertTo-Json -Depth 5
[System.IO.File]::WriteAllText($ManifestPath, $Json, [System.Text.UTF8Encoding]::new($false))

Write-Host "[ok] wrote manifest: $ManifestPath"

# ----- register in HKCU ------------------------------------------------------
function Register-NativeHost {
    param(
        [string]$BrowserDisplayName,
        [string]$BrowserRegRoot
    )
    $KeyPath = "$BrowserRegRoot\$HostName"
    if (-not (Test-Path $KeyPath)) {
        New-Item -Path $KeyPath -Force | Out-Null
    }
    # default (unnamed) value of the registry key must equal the manifest path
    Set-ItemProperty -Path $KeyPath -Name "(default)" -Value $ManifestPath
    Write-Host "[ok] registered for ${BrowserDisplayName}: $KeyPath"
}

if ($ChromeExtId) {
    Register-NativeHost -BrowserDisplayName "Chrome" `
                        -BrowserRegRoot     "HKCU:\Software\Google\Chrome\NativeMessagingHosts"
}
if ($EdgeExtId) {
    Register-NativeHost -BrowserDisplayName "Edge" `
                        -BrowserRegRoot     "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts"
}

Write-Host ""
Write-Host "--- summary ---"
Write-Host "host name :  $HostName"
Write-Host "exe       :  $ReceiverExe"
Write-Host "manifest  :  $ManifestPath"
Write-Host "origins   :"
foreach ($o in $AllowedOrigins) { Write-Host "             $o" }
Write-Host ""
Write-Host "next: completely close and reopen the browser(s), then reload the extension. The extension's connectNative('$HostName') should now spawn web_receiver.exe."
