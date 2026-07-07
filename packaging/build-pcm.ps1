# Assemble the KiCAD PCM plugin package for Konnect.
#
# Produces a zip laid out the way KiCAD's Plugin and Content Manager expects
# for "Install from File" and for kicad-addons repository submission:
#
#   metadata.json            PCM package manifest (version stamped in)
#   plugins/                 installed into KiCAD's scripting/plugins dir
#     __init__.py            ActionPlugin launcher (settings dialog, server control)
#     plugin.json            KiCAD 10 IPC plugin manifest
#     settings_dialog.py     wxPython settings UI
#     resources/icon.png     toolbar icon (referenced by __init__.py)
#     bin/konnect.exe        the MCP server binary
#     bin/schematic-viewer.exe  (optional) live schematic viewer
#   resources/
#     icon.png               icon shown in the PCM dialog
#
# Usage:
#   ./packaging/build-pcm.ps1 -Version 0.1.0 -BinaryPath target/release/konnect.exe `
#       [-ViewerPath crates/schematic-viewer/target/release/schematic-viewer.exe] `
#       [-OutDir dist]
#
# Prints the zip path, size, and SHA256 (needed for the kicad-addons
# repository metadata).

param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$BinaryPath,
    [string]$ViewerPath = "",
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not (Test-Path $BinaryPath)) { throw "Binary not found: $BinaryPath" }

$staging = Join-Path ([System.IO.Path]::GetTempPath()) "konnect-pcm-$Version"
if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
New-Item -ItemType Directory -Path "$staging/plugins/bin", "$staging/plugins/resources", "$staging/resources" -Force | Out-Null

# Plugin files
Copy-Item "$repoRoot/plugin/__init__.py" "$staging/plugins/"
Copy-Item "$repoRoot/plugin/plugin.json" "$staging/plugins/"
Copy-Item "$repoRoot/plugin/settings_dialog.py" "$staging/plugins/"
Copy-Item $BinaryPath "$staging/plugins/bin/konnect.exe"
if ($ViewerPath -and (Test-Path $ViewerPath)) {
    Copy-Item $ViewerPath "$staging/plugins/bin/schematic-viewer.exe"
    Write-Host "Included schematic viewer: $ViewerPath"
}

# Icons: PCM dialog icon at resources/, toolbar icon inside plugins/
Copy-Item "$repoRoot/packaging/resources/icon.png" "$staging/resources/icon.png"
Copy-Item "$repoRoot/packaging/resources/icon.png" "$staging/plugins/resources/icon.png"

# Metadata with the version stamped in. download_* fields stay blank inside
# the zip (PCM ignores them for install-from-file); the values printed below
# go into the kicad-addons repository submission.
$metadata = Get-Content "$repoRoot/packaging/metadata.json" -Raw | ConvertFrom-Json
$metadata.versions[0].version = $Version
$installSize = (Get-ChildItem $staging -Recurse -File | Measure-Object Length -Sum).Sum
$metadata.versions[0].install_size = [long]$installSize
# KiCAD 10 PCM validates the *format* of these fields even for install-from-file.
# Use schema-valid placeholders; the real values (from the printed output below)
# get filled in when the kicad-addons repository entry is authored.
$metadata.versions[0].download_sha256 = "0" * 64
$metadata.versions[0].download_url = "https://example.invalid/placeholder.zip"
$metadata.versions[0].download_size = 1
$metadata | ConvertTo-Json -Depth 10 | Set-Content "$staging/metadata.json"

# Zip it
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$zipPath = Join-Path (Resolve-Path $OutDir) "konnect-pcm-v$Version.zip"
if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
Compress-Archive -Path "$staging/*" -DestinationPath $zipPath

$sha = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
$size = (Get-Item $zipPath).Length

Write-Host ""
Write-Host "PCM package: $zipPath"
Write-Host "  download_size:   $size"
Write-Host "  install_size:    $installSize"
Write-Host "  download_sha256: $sha"
Write-Host "  download_url:    https://github.com/mixelpixx/Konnect/releases/download/v$Version/konnect-pcm-v$Version.zip"
