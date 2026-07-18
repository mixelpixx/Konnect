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
# The package declares the platform it carries binaries for, so KiCAD's PCM only
# offers it to machines that can run it. A package bundles one native binary, so
# it is never valid for "all platforms".
#
# Usage:
#   ./packaging/build-pcm.ps1 -Version 0.1.0 -BinaryPath target/release/konnect.exe `
#       [-ViewerPath crates/schematic-viewer/target/release/schematic-viewer.exe] `
#       [-Platform windows] [-OutDir dist]
#
# Prints the zip path, size, and SHA256 (needed for the kicad-addons
# repository metadata).

param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$BinaryPath,
    [string]$ViewerPath = "",
    [ValidateSet("windows")][string]$Platform = "windows",
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

# Stamp the IPC entrypoint to this platform's binary name. plugin.json ships the
# Windows default; the packaging step is authoritative so a package points at the
# binary it actually bundles (build-pcm.sh stamps bin/konnect for macOS/Linux).
$pluginManifest = Get-Content "$staging/plugins/plugin.json" -Raw | ConvertFrom-Json
foreach ($action in $pluginManifest.actions) {
    if ($action.entrypoint -like "bin/*") { $action.entrypoint = "bin/konnect.exe" }
}
$pluginManifest | ConvertTo-Json -Depth 10 | Set-Content "$staging/plugins/plugin.json"

# Icons: PCM dialog icon at resources/, toolbar icon inside plugins/
Copy-Item "$repoRoot/packaging/resources/icon.png" "$staging/resources/icon.png"
Copy-Item "$repoRoot/packaging/resources/icon.png" "$staging/plugins/resources/icon.png"

# Metadata with the version stamped in. The schema only *requires* version/
# status/kicad_version; download_* fields are OMITTED inside the zip — an
# empty string violates the sha256 pattern and placeholder values are lies.
# The real values (printed below) go into the kicad-addons repo submission.
$metadata = Get-Content "$repoRoot/packaging/metadata.json" -Raw | ConvertFrom-Json
$metadata.versions[0].version = $Version
$installSize = (Get-ChildItem $staging -Recurse -File | Measure-Object Length -Sum).Sum
$metadata.versions[0] | Add-Member -NotePropertyName install_size -NotePropertyValue ([long]$installSize) -Force
# This package carries one platform's native binary — say so, or PCM offers a
# Windows build to a macOS user and vice versa. [string[]] keeps ConvertTo-Json
# from collapsing the single-element array into a bare string.
$metadata.versions[0] | Add-Member -NotePropertyName platforms -NotePropertyValue ([string[]]@($Platform)) -Force
foreach ($field in @("download_sha256", "download_url", "download_size")) {
    $metadata.versions[0].PSObject.Properties.Remove($field)
}
$metadata | ConvertTo-Json -Depth 10 | Set-Content "$staging/metadata.json"

# Zip it
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$zipPath = Join-Path (Resolve-Path $OutDir) "konnect-pcm-v$Version-$Platform.zip"
if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
Compress-Archive -Path "$staging/*" -DestinationPath $zipPath

$sha = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
$size = (Get-Item $zipPath).Length

Write-Host ""
Write-Host "PCM package: $zipPath"
Write-Host "  download_size:   $size"
Write-Host "  install_size:    $installSize"
Write-Host "  download_sha256: $sha"
Write-Host "  platforms:       [$Platform]"
Write-Host "  download_url:    https://github.com/mixelpixx/Konnect/releases/download/v$Version/konnect-pcm-v$Version-$Platform.zip"
