#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repository=$(cd -- "$script_dir/.." && pwd)
fixtures="$repository/crates/schematic-viewer/tests/fixtures"

for fixture in page-only.kicad_sch wire-only.kicad_sch; do
    echo "strict semantic golden: $fixture"
    KONNECT_VELLO_SVG_ORACLE=1 \
    KONNECT_MAX_SEMANTIC_RMSE=0 \
        "$script_dir/compare-schematic-renderers.sh" "$fixtures/$fixture"
done
