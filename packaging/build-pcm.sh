#!/usr/bin/env bash
# Assemble the KiCAD PCM plugin package for Konnect (macOS/Linux port of build-pcm.ps1).
#
# Produces a zip laid out the way KiCAD's Plugin and Content Manager expects for
# "Install from File" and for kicad-addons repository submission:
#
#   metadata.json                 PCM package manifest (version stamped in)
#   plugins/                      installed into KiCAD's scripting/plugins dir
#     __init__.py                 ActionPlugin launcher (settings dialog, server control)
#     plugin.json                 KiCAD 10 IPC plugin manifest (entrypoint stamped)
#     settings_dialog.py          wxPython settings UI
#     resources/icon.png          toolbar icon (referenced by __init__.py)
#     bin/konnect                 the MCP server binary
#     bin/schematic-viewer        (optional) live schematic viewer
#   resources/
#     icon.png                    icon shown in the PCM dialog
#
# The IPC entrypoint in plugin.json is stamped to this platform's binary name
# (bin/konnect) so the KiCAD 10 IPC action resolves on macOS/Linux, mirroring
# build-pcm.ps1 which stamps bin/konnect.exe for Windows.
#
# The package declares the platform it carries binaries for, so KiCAD's PCM
# only offers it to machines that can run it. A package bundles one native
# binary, so it is never valid for "all platforms".
#
# Usage:
#   packaging/build-pcm.sh [--version X.Y.Z] [--binary PATH] [--viewer PATH]
#                          [--platform macos|linux] [--out DIR]
#
# Defaults: version from Cargo.toml, binary target/release/konnect, platform
# from the host, output dist/.
# Prints the zip path, size, and SHA256 (needed for the kicad-addons metadata).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin_name="konnect"

version=""
binary="$repo_root/target/release/$bin_name"
viewer=""
platform=""
out_dir="$repo_root/dist"

usage() {
    cat >&2 <<'EOF'
Usage: packaging/build-pcm.sh [--version X.Y.Z] [--binary PATH] [--viewer PATH]
                              [--platform macos|linux] [--out DIR]

Assembles the KiCAD PCM plugin zip (macOS/Linux port of build-pcm.ps1).
Defaults: version from Cargo.toml, binary target/release/konnect, platform from
the host, output dist/.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) version="${2:?--version needs a value}"; shift 2;;
        --binary)  binary="${2:?--binary needs a value}";   shift 2;;
        --viewer)  viewer="${2:?--viewer needs a value}";   shift 2;;
        --platform) platform="${2:?--platform needs a value}"; shift 2;;
        --out)     out_dir="${2:?--out needs a value}";     shift 2;;
        -h|--help) usage; exit 0;;
        *) echo "Unknown argument: $1" >&2; usage; exit 2;;
    esac
done

# Default version = workspace version from Cargo.toml
if [ -z "$version" ]; then
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)"
fi
[ -n "$version" ] || { echo "Could not determine version; pass --version" >&2; exit 1; }

# Default platform = the host we're packaging on.
if [ -z "$platform" ]; then
    case "$(uname -s)" in
        Darwin) platform="macos";;
        Linux)  platform="linux";;
        *) echo "Cannot infer platform from $(uname -s); pass --platform" >&2; exit 1;;
    esac
fi
case "$platform" in
    macos|linux) ;;
    windows) echo "Windows packages come from build-pcm.ps1" >&2; exit 2;;
    *) echo "--platform must be 'macos' or 'linux' (got '$platform')" >&2; exit 2;;
esac
if [ ! -f "$binary" ]; then
    echo "Binary not found: $binary" >&2
    echo "Build it first: cargo build --release -p konnect" >&2
    exit 1
fi

staging="$(mktemp -d)/konnect-pcm-$version"
mkdir -p "$staging/plugins/bin" "$staging/plugins/resources" "$staging/resources"

# Plugin files
cp "$repo_root/plugin/__init__.py"        "$staging/plugins/"
cp "$repo_root/plugin/plugin.json"        "$staging/plugins/"
cp "$repo_root/plugin/settings_dialog.py" "$staging/plugins/"
cp "$binary"                              "$staging/plugins/bin/$bin_name"
chmod +x "$staging/plugins/bin/$bin_name"
if [ -n "$viewer" ] && [ -f "$viewer" ]; then
    cp "$viewer" "$staging/plugins/bin/schematic-viewer"
    chmod +x "$staging/plugins/bin/schematic-viewer"
    echo "Included schematic viewer: $viewer"
fi

# Icons: PCM dialog icon at resources/, toolbar icon inside plugins/
cp "$repo_root/packaging/resources/icon.png" "$staging/resources/icon.png"
cp "$repo_root/packaging/resources/icon.png" "$staging/plugins/resources/icon.png"

# Stamp the IPC entrypoint to this platform's binary name. plugin.json ships a
# Windows default (bin/konnect.exe); the packaging step is authoritative so a
# package built on any OS points at the binary it actually bundles.
python3 - "$staging/plugins/plugin.json" "bin/$bin_name" <<'PY'
import json, sys
path, entry = sys.argv[1], sys.argv[2]
m = json.load(open(path))
for a in m.get("actions", []):
    if a.get("entrypoint", "").startswith("bin/"):
        a["entrypoint"] = entry
json.dump(m, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY

# metadata.json: stamp version + install_size + platforms. download_* are
# OMITTED inside the zip, matching build-pcm.ps1: the schema only requires
# version/status/kicad_version, and placeholder values are lies that PCM shows
# to users ("Download Size: 1 B"). The real values (printed below) go into the
# kicad-addons repository submission. install_size is the staged file total
# BEFORE metadata.json is written, matching build-pcm.ps1.
install_size="$(python3 -c 'import os,sys; print(sum(os.path.getsize(os.path.join(d,f)) for d,_,fs in os.walk(sys.argv[1]) for f in fs))' "$staging")"
python3 - "$repo_root/packaging/metadata.json" "$staging/metadata.json" "$version" "$install_size" "$platform" <<'PY'
import json, sys
src, dst, version, install_size, platform = sys.argv[1:6]
m = json.load(open(src))
v = m["versions"][0]
v["version"] = version
v["install_size"] = int(install_size)
# This package carries one platform's native binary — say so, or PCM offers a
# macOS build to a Windows user and vice versa.
v["platforms"] = [platform]
for field in ("download_sha256", "download_url", "download_size"):
    v.pop(field, None)
json.dump(m, open(dst, "w"), indent=2)
open(dst, "a").write("\n")
PY

# Zip it (staged contents at the archive root, matching Compress-Archive)
mkdir -p "$out_dir"
# zip runs from inside $staging below; a relative --out would resolve there
# and fail with "Could not create output file". Absolutize it first.
out_dir="$(cd "$out_dir" && pwd)"
zip_path="$out_dir/konnect-pcm-v$version-$platform.zip"
rm -f "$zip_path"
( cd "$staging" && zip -rqX "$zip_path" metadata.json plugins resources )

if command -v shasum >/dev/null 2>&1; then
    sha="$(shasum -a 256 "$zip_path" | awk '{print $1}')"
else
    sha="$(sha256sum "$zip_path" | awk '{print $1}')"
fi
size="$(python3 -c 'import os,sys; print(os.path.getsize(sys.argv[1]))' "$zip_path")"

echo ""
echo "PCM package: $zip_path"
echo "  download_size:   $size"
echo "  install_size:    $install_size"
echo "  download_sha256: $sha"
echo "  platforms:       [$platform]"
echo "  download_url:    https://github.com/mixelpixx/Konnect/releases/download/v$version/konnect-pcm-v$version-$platform.zip"
