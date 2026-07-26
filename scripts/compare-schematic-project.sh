#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <project-directory>" >&2
    exit 2
fi

project=$1
if [[ ! -d "$project" ]]; then
    echo "project directory does not exist: $project" >&2
    exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
mapfile -d '' schematics < <(find "$project" -maxdepth 1 -type f -name '*.kicad_sch' -print0 | sort -z)
if [[ ${#schematics[@]} -eq 0 ]]; then
    echo "no .kicad_sch files found in: $project" >&2
    exit 2
fi

for schematic in "${schematics[@]}"; do
    echo "$(basename -- "$schematic")"
    "$script_dir/compare-schematic-renderers.sh" "$schematic"
done
