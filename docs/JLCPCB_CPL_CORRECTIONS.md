# JLCPCB CPL corrections

KiCad and JLCPCB do not always use the same zero-degree orientation for a
package. A CPL can therefore pass JLCPCB's structural checks while an IC or
connector is physically rotated in Component Placements.

`export_manufacturing_package(fab_house="jlcpcb")` applies a small, versioned
built-in correction policy after KiCad exports the native component geometry.
Pass `jlcpcb_cpl_corrections_path` to add project rules without editing Konnect.
Keep that JSON file with the KiCad project so the manufacturing decision is
reviewable and repeatable.

## Policy format

```json
{
  "schema_version": 1,
  "policy_id": "my-clock-jlcpcb-v1",
  "provenance": "Verified in JLCPCB Component Placements on 2026-09-10",
  "footprint_rules": [
    {
      "id": "display-driver-soic",
      "footprint_prefix": "SOIC-8_",
      "rotation_degrees": 270,
      "offset_x_mm": 0.0,
      "offset_y_mm": 0.0
    }
  ],
  "component_overrides": [
    {
      "id": "u7-selected-lcsc-model",
      "designator": "U7",
      "rotation_degrees": 90,
      "offset_x_mm": 0.15,
      "offset_y_mm": -0.1
    }
  ]
}
```

All keys shown at the policy and rule levels are checked. Unknown keys,
unsupported schema versions, duplicate rule IDs, duplicate designator
overrides, empty IDs, and non-finite correction values fail the export.

## Matching and coordinates

Konnect strips a KiCad library prefix such as `Package_SO:` before matching
`footprint_prefix`. Matching is case-sensitive. Precedence is deterministic:

1. exact `designator` in the project policy;
2. first matching project `footprint_rules` entry;
3. first matching built-in footprint rule;
4. no correction.

`rotation_degrees` is added for top-side parts and applied with the documented
bottom-side view transform. The result is normalized into `[0, 360)`.
`offset_x_mm` and `offset_y_mm` are additions in the exported board coordinate
axes; they are not local component-axis offsets.

Use a designator override when a specific LCSC/JLCPCB placement model differs
from other parts sharing the same KiCad footprint. Use a footprint rule only
after verifying that the correction is valid for every matched package in the
project.

## Required evidence

The tool response contains:

- `applied_corrections`: designator, footprint, side, policy/rule IDs, and the
  position and rotation before and after the correction;
- `unmatched_footprints`: every exported component for which no rule matched;
- `policies`: the policy ID, schema version, provenance, and precedence; and
- `status: "PREVIEW_REQUIRED"` with `physical_validation: false`.

An unmatched footprint is not automatically wrong, and a matched footprint is
not automatically right. Upload the Gerbers, BOM, and CPL together, open
JLCPCB's Component Placements view, and inspect every component—especially ICs,
connectors, diodes, LEDs, and other polarized parts. Only that preview validates
the selected JLCPCB component models for the order.

Konnect does not bundle the GPL-3.0 JLCKicadTools correction database. The
built-in policy contains only narrowly scoped rules independently verified by
the Konnect project and identifies its provenance in every response.
