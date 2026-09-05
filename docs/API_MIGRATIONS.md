# Konnect MCP API migrations

Konnect's tool schemas are public API. This file records intentional argument
removals and the supported replacement workflow.

## Unreleased: `score_placement` reports interface filter caps (minor release)

`score_placement`'s decoupling deduction no longer fires on a capacitor placed on
a connector's own pins for cable/EMI filtering (#411). A cap beyond its value
family's limit from the nearest shared-net IC is exempted only when a `J*`
connector carries **every** net that cap carries and its courtyard edge is within
that same limit of the cap's center. The cap must be on the connector's face,
unless the connector's pads carrying those nets reach both copper faces — a
plated through-hole pin, not an SMD one.

Results add `interface_filter_caps`: one entry per exempted cap, with
`reference`, `value`, `connector`, `connector_distance_mm`, and `limit_mm`. It is
evidence rather than a score — an exempted cap deducts nothing, and a waiver that
vanished silently would be indistinguishable from a check that never ran. The
array is empty on boards without such caps.

No tool, argument, or existing response field was renamed or removed. This
additive response field is planned for the next minor release.

## Unreleased: type-safe trace deletion (minor release)

`delete_trace` now accepts only a UUID observed in the requested live board's
trace-segment inventory. Via, zone, graphic, footprint, missing, and stale UUIDs
return `stale_target` before `DeleteItems` is sent. A successful call reads the
same board again and refuses success if the segment remains.

The existing `deleted_uuid` field remains, but it is now derived from the
observed segment rather than echoed from the request. Results add
`deleted_type: "trace_segment"`, the observed net/layer/width/endpoints under
`preimage`, and `postcondition: "absent_from_trace_readback"`. No argument or
tool was renamed or removed.

## Unreleased: connectivity-safe component deletion (minor release)

`delete_schematic_component`, `batch_delete`, and
`batch_delete_schematic_components` now resolve a complete logical component
before writing, remove only no-connect markers owned exclusively by deleted
pins, and reconcile junctions only at affected pin endpoints. Wires and labels
remain, matching KiCad's plain-delete behavior. Selecting one placed-unit UUID
through `batch_delete` deletes every placed unit of that reference.

The existing single-delete fields (`deleted`, `deleted_units`) and batch fields
(`deleted_count`, `deleted`, `errors`) remain. Single-delete results add
`deleted_unit_uuids`, plus count-and-UUID evidence for removed no-connects and
added or pruned junctions. Batch results add `deleted_components` (including
each reference's observed unit count and UUIDs), `deleted_item_uuids`, and the
same connectivity evidence fields. These values come from reloading the
committed schematic rather than echoing requested selectors.

Missing, protected, duplicate, malformed, stale, wrong-document, or
editor-locked targets refuse with the existing `stale_target` kind before a
write when Konnect cannot prove a unique safe deletion. A post-write readback
that still observes a selected reference or UUID also returns `stale_target`;
inspect and reload the saved schematic before retrying because that refusal can
follow a committed write. This additive response change is planned for the next
minor release; no tool or argument was renamed or removed.

## Unreleased: complete schematic placement instances (minor release)

`add_schematic_component`, `batch_place_components`, and `add_power_symbol`
preserve every instance path when the saved root reuses a child schematic.
Existing inputs and response fields remain. Placement results now include
`schematic`, `project`, `instance_paths`, and observed symbol fields (`uuid`,
`added`, `reference`, `value`, `x`, `y`, `rotation`, `unit`). Batch results put
these fields in each `placed` entry; power placement retains `added_power` and
`junctions_added`. Values come from reloading the committed file.

Missing, foreign, duplicate, malformed, or obsolete saved instance paths,
references, or units return `stale_target` with `target` and `reason` before
placement writes. Repair the saved hierarchy and its complete symbol instance
metadata before retrying. Ambiguous
project ownership continues to use the existing `conflict` kind from #189.
If post-write readback cannot verify the target or symbol, `stale_target` may
follow a committed write: inspect/reload the file before retrying to avoid a
duplicate placement. This observes saved files only and does not claim an
atomic snapshot of the complete hierarchy or unsaved editor state.

See [Schematic project ownership](PROJECT_OWNERSHIP.md#placement-instance-validation)
for the acceptance matrix and limits. This additive response change is planned
for the next minor release; no tool or argument was renamed or removed.

## Unreleased: schematic ownership conflicts (minor release)

Symbol-loading operations and ERC root detection now refuse unproven or ambiguous
ancestor project ownership with the existing `conflict` kind. Previously, some
of these cases silently inherited unrelated libraries or treated the schematic
as projectless. `error.paths` names the schematic directory and every candidate
root. Restore the saved hierarchy or separate the independent document from the
unrelated project before retrying. Loose schematics with no candidate project
and adjacent library-table authority remain supported. See
[Schematic project ownership](PROJECT_OWNERSHIP.md) for the behavior and limits.

## Unreleased: Rust Specctra export is the default

`export_specctra_dsn.native_bridge_mode` now defaults to `disable`, so an
omitted value always selects the Rust/IPC exporter. This keeps the default path
free of Python and SWIG and makes its KiCad 11 direction explicit.

KiCad 10 users who deliberately want the authenticated ActionPlugin bridge can
pass `prefer` (use the native export when available, otherwise Rust) or
`require` (refuse when the native bridge is unavailable). No tool or argument
was removed.

## Unreleased: remove inputs that never affected an operation

The following optional inputs were advertised but never read by their handlers.
Keeping them would let a client believe a request was honoured when the result was
identical without it.

| Removed input | Migration |
|---|---|
| `import_sheet_pins.project_name` | Omit it. Importing hierarchical labels as sheet pins does not modify project-instance paths. |
| `refill_zones.zones` | Omit it. KiCad IPC refills every zone on the active board and exposes no per-net selector. |
| `run_drc.tests` | Omit it. `kicad-cli pcb drc` runs the complete configured ruleset. Configure rules/waivers in KiCad; `severity` and `limit` only filter Konnect's returned report. |
| `audit_decoupling.board` and `audit_decoupling.max_distance_mm` | Run `audit_decoupling(schematic)` for net-connectivity coverage, then use PCB placement/clearance inspection for physical capacitor distance. The audit never measured PCB distance. |
| `export_manufacturing_package.quantity` | Omit it from export. Manufacturing files are quantity-independent; pass `quantity` to `estimate_cost` for pricing context. |
| `validate_for_manufacturing.schematic` | Run the board validator without it. Use `check_bom_health(schematic)` for the separate schematic/BOM review. |
| `estimate_cost.schematic` | Omit it. The estimator counts placed board footprints, which are the components relevant to assembly pricing. |
| `move_connected.*` (all parameters) | The tool now refuses unconditionally: it never implemented the connected move and silently delegated to a plain symbol move while reporting connections preserved (#315). Use `move_schematic_component`, then re-route the affected nets. The parameters return when the wire-carrying move is actually built. |

These removals narrow the schema to behavior Konnect can verify. They do not change
the generated files or analysis because the removed values had no implementation.
