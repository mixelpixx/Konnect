#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
    echo "usage: $0 <schematic.kicad_sch> [diff.png]" >&2
    exit 2
fi

schematic=$1
diff_output=${2:-}
for command in cargo kicad-cli rsvg-convert magick; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "required command is unavailable: $command" >&2
        exit 2
    fi
done
if [[ ! -f "$schematic" ]]; then
    echo "schematic does not exist: $schematic" >&2
    exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repository=$(cd -- "$script_dir/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

stem=$(basename -- "$schematic" .kicad_sch)
reference_dir="$work/reference"
mkdir -- "$reference_dir"
kicad-cli sch export svg --output "$reference_dir" "$schematic" >/dev/null

native_png="$work/native.png"
reference_png="$work/reference.png"
reference_full_png="$work/reference-full.png"
pixels_per_mm=10
KONNECT_SVG_ORDER_ORACLE="$reference_dir/$stem.svg" cargo run --quiet \
    --manifest-path "$repository/crates/schematic-viewer/Cargo.toml" \
    --no-default-features --features renderer-vello -- \
    --render-png "$native_png" "$schematic"
read -r native_width native_height < <(magick identify -format '%w %h\n' "$native_png")
# Render at the same fixed physical scale as the native scene. KiCad's paper
# dimensions are a few micrometres larger than their nominal sizes, so the
# natural raster is one pixel larger and must be clipped rather than stretched.
dpi=$(awk -v scale="$pixels_per_mm" 'BEGIN { print scale * 25.4 }')
rsvg-convert --dpi-x "$dpi" --dpi-y "$dpi" \
    --output "$reference_full_png" "$reference_dir/$stem.svg"
magick "$reference_full_png" -crop "${native_width}x${native_height}+0+0" +repage \
    "$reference_png"

metric=$(magick compare -metric RMSE "$reference_png" "$native_png" null: 2>&1 || true)
normalized=$(sed -n 's/.*(\([^)]*\)).*/\1/p' <<<"$metric")
if [[ -z "$normalized" ]]; then
    echo "could not parse ImageMagick RMSE: $metric" >&2
    exit 1
fi

echo "KiCad/Vello normalized RMSE: $normalized"
if [[ -n "$diff_output" ]]; then
    magick compare "$reference_png" "$native_png" "$diff_output" 2>/dev/null || true
    echo "Diff image: $diff_output"
fi

if [[ "${KONNECT_VELLO_SVG_ORACLE:-0}" == "1" ]]; then
    oracle_png="$work/reference-vello.png"
    cargo run --quiet \
        --manifest-path "$repository/crates/schematic-viewer/Cargo.toml" \
        --no-default-features --features golden-svg-reference -- \
        --render-svg-png "$oracle_png" "$native_width" "$native_height" \
        "$reference_dir/$stem.svg"
    oracle_metric=$(magick compare -metric RMSE "$oracle_png" "$native_png" null: 2>&1 || true)
    oracle_normalized=$(sed -n 's/.*(\([^)]*\)).*/\1/p' <<<"$oracle_metric")
    if [[ -z "$oracle_normalized" ]]; then
        echo "could not parse same-Vello RMSE: $oracle_metric" >&2
        exit 1
    fi
    echo "KiCad SVG/native semantic same-Vello RMSE: $oracle_normalized"
    if [[ -n "${KONNECT_MAX_SEMANTIC_RMSE:-}" ]]; then
        awk -v actual="$oracle_normalized" -v maximum="$KONNECT_MAX_SEMANTIC_RMSE" \
            'BEGIN { exit !(actual <= maximum) }' || {
            echo "semantic RMSE $oracle_normalized exceeds $KONNECT_MAX_SEMANTIC_RMSE" >&2
            exit 1
        }
    fi
fi

# Set KONNECT_MAX_RENDER_RMSE=0 for a strict pixel-identical gate. During
# renderer development it can be raised explicitly to guard an intermediate
# baseline without presenting that baseline as parity.
if [[ -n "${KONNECT_MAX_RENDER_RMSE:-}" ]]; then
    awk -v actual="$normalized" -v maximum="$KONNECT_MAX_RENDER_RMSE" \
        'BEGIN { exit !(actual <= maximum) }' || {
        echo "render RMSE $normalized exceeds $KONNECT_MAX_RENDER_RMSE" >&2
        exit 1
    }
fi
