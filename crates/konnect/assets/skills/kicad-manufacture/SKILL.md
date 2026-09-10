---
name: kicad-manufacture
description: |
  Manufacturing and fabrication workflow for KiCAD projects via MCP tools. Triggers on: "send to fab",
  "order boards", "gerbers", "JLCPCB", "manufacturing", "export for production", "pick and place",
  "assembly files", "generate fabrication outputs", "BOM for fab", "production files", "fab house".
argument-hint: "[fab house or export task]"
---

# KiCAD Manufacturing & Fabrication Workflow

Prepare manufacturing outputs through Konnect MCP tools. Treat each tool result
as evidence with a named scope: a successful request is not by itself proof of a
complete, current, upload-ready package. A required check or artifact that cannot
be established makes the manufacturing verdict `INCOMPLETE`.

## Toolset loading

Load the required toolsets:

```
load_toolset('pcb_export')       # Gerber, drill, BOM, position, 3D, and direct DRC evidence
load_toolset('manufacturing')    # Package export, manufacturing preflight, and rough cost estimate
```

Load additional toolsets only for the branch that needs them:

```
load_toolset('sch_analysis')     # schematic inventory and footprint assignment
load_toolset('integration')      # local JLCPCB catalogue and alternatives
```

Call `get_active_toolsets()` before loading more.

### References by manufacturing branch

- Read [`references/gerber-layers.md`](references/gerber-layers.md) when
  selecting plot layers or accepting a generated artifact inventory.
- Read [`references/jlcpcb-rules.md`](references/jlcpcb-rules.md) only when
  JLCPCB is the selected fabricator or assembler. It defines how to capture the
  current order contract without caching volatile limits, prices, categories,
  or field names in this skill.

## 1. Capture the order contract

Before checking or exporting, record the selected fabricator, service tier,
stackup, copper weight, finish, assembly sides, stencil requirement, quantity,
and any controlled-impedance service. Record the source and retrieval date for
every vendor-controlled requirement. Project rules and output acceptance are
judged against that record.

Completion criterion: every applicable fabrication and assembly constraint has
one current authority, and no decision rests on an undated table in this skill.

## 2. Establish direct design evidence

Run direct KiCad DRC against the saved target board and resolve every error or
record a deliberate, reviewable waiver. Then run:

```
validate_for_manufacturing(board, fab_house?)
```

The current handler checks only:

- presence of at least one `Edge.Cuts` item;
- presence of footprints;
- the configured minimum trace width against its built-in fab profile;
- the coarse case where several nets exist but no routed tracks exist; and
- direct `kicad-cli` DRC evidence, which must be available and complete for a
  `READY` verdict.

Read `verdict`, `issues`, and `drc` together. A null or incomplete `drc`, a
`NOT READY` verdict, or an unadjudicated issue blocks release.

This preflight does not prove outline closure, copper on every pad, drill-size
acceptance, silkscreen clearance, stackup compatibility, or assembly readiness.
Establish those separately with direct DRC, the selected fabricator's current
contract, Gerber/drill inspection, BOM/CPL review, and the order preview.

Completion criterion: direct DRC is complete, every reported issue is resolved
or waived, and every check outside the handler's stated scope has named evidence.

## 3. Export into a fresh destination

Prefer a new, empty output directory for each invocation. This makes stale files
structurally unable to impersonate output from the current invocation.

For a package attempt:

```
export_manufacturing_package(board, output_dir, fab_house?, schematic?, jlcpcb_cpl_corrections_path?)
```

Pass `schematic` when assembly output requires a BOM. The tool attempts Gerber,
drill, position, and BOM exports according to the request; individual failures
can still leave a partial directory.

For `fab_house="jlcpcb"`, use millimetres (the default) and provide the BOM
fields, labels, and grouping required by the current order contract. The tool
emits `BOM-<project>.csv` plus `CPL-<project>.csv`; the CPL uses JLCPCB's
`Designator,Mid X,Mid Y,Layer,Rotation` schema, and KiCad is instructed to
enumerate grouped BOM references instead of compressing them into ranges. DNP
parts are excluded from both native exports; any remaining population mismatch
caused by board/schematic exclusion flags makes the package incomplete. This
conversion applies Konnect's independently verified built-in CPL correction
policy. Pass a checked-in project policy through
`jlcpcb_cpl_corrections_path` when an unmatched footprint or one exact
designator needs a correction. The precedence and JSON format are documented in
`docs/JLCPCB_CPL_CORRECTIONS.md`.

Inspect **placement_orientation.applied_corrections** and
**placement_orientation.unmatched_footprints**. **complete: true** proves the
package is structurally complete; it does **not** prove physical placement
orientation. Require **placement_orientation.status == "PREVIEW_REQUIRED"** to
be discharged by inspecting every component in JLCPCB Component Placements
before an order is approved. Never describe the CPL as physically validated
from the automated result alone.

### Artifact acceptance gate

1. Inspect `warnings` and `files_generated`. Any warning or missing requested
   artifact type keeps the result `INCOMPLETE`.
2. Reconcile the requested copper, mask, silkscreen, paste, and `Edge.Cuts`
   layers against the actual files in the fresh output directory.
3. Confirm every required artifact is a regular, non-empty file produced by the
   current invocation. A directory entry, reported path, or zero exit status is
   not enough.
4. Confirm the required plated and non-plated drill outputs for the actual board
   hole inventory. Absence is acceptable only when the design proves that output
   is inapplicable.
5. Open the Gerbers and drills in a viewer. Inspect layer registration, outline,
   apertures, holes/slots, mask, paste, and silkscreen.
6. For assembly, inspect BOM contents, DNP handling, designator coverage, CPL
   side/units/origin/rotation, and the fabricator's export preview.

The `files` field is derived from regular, non-empty artifacts verified at the
export boundary; `files_generated` describes each successful export. That
evidence does not establish vendor acceptance, correct component rotations, or
that unrelated stale files elsewhere in a reused directory are safe to upload.
Preserve the viewer and order-preview checks.

Completion criterion: an accepted manifest accounts for every required output,
every accepted path is fresh and non-empty, and visual/order previews agree with
the saved design.

## 4. Use manual exports when control is required

Use the individual tools when a package needs explicit layer, BOM, side, unit,
or filename choices:

```
export_gerber(board, output_dir, layers?, drill_file?)
export_bom(schematic, output, format?, fields?, group_by?, labels?, exclude_dnp?)
export_position_file(board, output, format?, side?, units?)
```

Apply the same fresh-destination and artifact acceptance gate. A manual sequence
does not lower the evidence requirement.

## 5. Treat cost output as a heuristic

```
estimate_cost(board, quantity?, layers?, fab_house?)
```

`estimate_cost` is an indicative heuristic built from fixed assumptions and
rough average component costs. Use it only for coarse comparisons. It is not a
vendor quote and does not know the selected finish, service, complete BOM,
shipping, taxes, coupons, or current pricing. Budget and purchasing decisions
require a current quote from the selected fabricator.

## 6. Record manufacturing acceptance

The final report must include:

- saved design revision or hash;
- direct DRC status and any explicit waivers;
- `validate_for_manufacturing` verdict, issues, and DRC coverage;
- selected fabricator/order contract with source and retrieval date;
- accepted artifact manifest with file type, path, and non-empty evidence;
- Gerber/drill viewer result;
- BOM/CPL and order-preview result when assembly is in scope;
- 3D/enclosure inspection status when mechanically relevant; and
- final `READY`, `NOT READY`, or `INCOMPLETE` verdict.

Only `READY` permits upload. Preserve the accepted manifest rather than telling
the user to upload every entry found in a reused directory.
