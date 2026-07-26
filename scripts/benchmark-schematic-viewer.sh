#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
    echo "usage: $0 <root.kicad_sch> [iterations]" >&2
    exit 2
fi

schematic=$1
iterations=${2:-20}
if [[ ! -f "$schematic" ]]; then
    echo "schematic does not exist: $schematic" >&2
    exit 2
fi
if [[ ! "$iterations" =~ ^[1-9][0-9]*$ ]]; then
    echo "iterations must be a positive integer" >&2
    exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repository=$(cd -- "$script_dir/.." && pwd)
output=$(cargo run --quiet --release \
    --manifest-path "$repository/crates/schematic-viewer/Cargo.toml" \
    --no-default-features --features renderer-vello -- \
    --benchmark-load "$iterations" "$schematic")
echo "$output"

active_p95=$(sed -n 's/.*active_p95_ms=\([0-9.]*\).*/\1/p' <<<"$output")
hierarchy_p95=$(sed -n 's/.*hierarchy_p95_ms=\([0-9.]*\).*/\1/p' <<<"$output")
if [[ -z "$active_p95" || -z "$hierarchy_p95" ]]; then
    echo "could not parse benchmark output" >&2
    exit 1
fi

if [[ -n "${KONNECT_MAX_ACTIVE_P95_MS:-}" ]]; then
    awk -v actual="$active_p95" -v maximum="$KONNECT_MAX_ACTIVE_P95_MS" \
        'BEGIN { exit !(actual <= maximum) }' || {
        echo "active-sheet p95 ${active_p95} ms exceeds ${KONNECT_MAX_ACTIVE_P95_MS} ms" >&2
        exit 1
    }
fi
if [[ -n "${KONNECT_MAX_HIERARCHY_P95_MS:-}" ]]; then
    awk -v actual="$hierarchy_p95" -v maximum="$KONNECT_MAX_HIERARCHY_P95_MS" \
        'BEGIN { exit !(actual <= maximum) }' || {
        echo "hierarchy p95 ${hierarchy_p95} ms exceeds ${KONNECT_MAX_HIERARCHY_P95_MS} ms" >&2
        exit 1
    }
fi
