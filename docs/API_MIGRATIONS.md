# Konnect MCP API migrations

Konnect's tool schemas are public API. This file records intentional argument
removals and the supported replacement workflow.

## Unreleased: `add_mounting_hole` uses KiCad's shipped footprint names (minor release)

`add_mounting_hole` wrote `MountingHole:MountingHole_{drill:.1}mm`. KiCad 10
ships no plain `MountingHole_3.2mm` — 3.2 mm exists only as the M3 family — and
spells round sizes without a decimal (`MountingHole_3mm`), so the default call
and every integer size produced a name no stock KiCad resolves (#462).

The lib_id now comes from a table of the footprints KiCad 10 ships (`3.2` →
`MountingHole_3.2mm_M3`, `3` → `MountingHole_3mm`, `4.3` →
`MountingHole_4.3mm_M4`, …), and a drill with no shipped footprint is refused
with `invalid_argument` on `drill_diameter` listing the shipped sizes, before
anything is written. When `MountingHole.pretty` resolves from the machine
(project or global `fp-lib-table`, or a discovered install), the hole placed is
KiCad's own library footprint, as `place_component` would place it; otherwise
Konnect's unplated-hole geometry is written under the shipped name. That
geometry's pad now equals the drill (no `+0.5` annulus), matching KiCad's plain
`MountingHole_*` footprints.

Results add `geometry` (`"library"` or `"inline"`) and, for inline geometry,
`geometry_note`. The `footprint` field is now read back from the saved board
rather than repeated from the request. No tool or argument was renamed or
removed; `drill_diameter` keeps its default of 3.2.

## Unreleased: DRC item ownership (minor release)

`run_drc` and `get_drc_violations` now say what owns each item of each
violation. Every item in `violations`, `unconnected_items`, and
`schematic_parity` gains up to four additive fields:

- `ownership_status`: `resolved`, `uuid_missing`, `not_found`, `ambiguous`, or
  `unavailable`.
- `owner`: `{"kind":"board"}` or a footprint owner with its reference and UUID
  when resolved; `null` for every unresolved state.
- `item_kind`: the board node head when uniquely resolved.
- `layer`: the item's single layer when uniquely resolved.

Ownership is derived only from exact UUIDs in the saved `.kicad_pcb`; it is
never inferred from KiCad's prose. Duplicate UUIDs are `ambiguous` rather than
resolved according to file order. If the saved board cannot be reread or
parsed, every item is marked `unavailable` and the report adds
`ownership_diagnostic` with the reason, while retaining all original DRC
findings.

Footprint ownership does not make a finding false. Footprint-owned `Edge.Cuts`
is fabrication geometry; ownership identifies whether the board or footprint
definition is the likely repair location. KiCad's existing `description`,
`pos`, `uuid`, `severity`, and `rule` fields remain unchanged.

## Unreleased: `create_symbol` draws a symbol body (minor release)

`create_symbol` accepts `graphics`, an array of drawing primitives, at the top
level beside `pins` and per unit inside `units[]`. The vocabulary is
`set_footprint_graphics`'s — `line`, `arc`, `rect`, `circle`, `poly`, with points
as `{x, y}` and `stroke_width_mm` — generated for both tools by one function, so
the two domains cannot drift. Only `fill` differs: a symbol adds KiCad's pale
`background`, which is what the stock libraries use for a body box.
`set_footprint_graphics`'s own schema is unchanged.

**Whether the key is present is itself the contract, and `[]` is present.**
Omitting `graphics` keeps the existing behaviour exactly: the automatic body
rectangle is sized to the pin names, and pins are slid out to the edge it
computes. Supplying `graphics` suppresses that body for that unit — including
`graphics: []`, which asks for a symbol with no body at all and cannot be
expressed any other way. Supplying it also means pin `x`/`y` are written exactly
as given rather than moved, which is the reason the feature exists.

Two consequences follow for callers who combine `graphics` with older arguments:

- A `glyph` on a unit that also supplies `graphics` is not drawn; the response
  carries a warning saying so, rather than discarding the request silently.
- A triangular `glyph` (op-amp, buffer, inverter, schmitt) carrying power pins
  normally moves them to a generated rectangular power unit, because the
  triangle's apex has no room for their names. **With `graphics` supplied that
  split no longer happens**, since a body the caller drew has whatever room they
  gave it, and moving the pins would overwrite the coordinates they supplied. A
  caller relying on the split must omit `graphics` for that unit.

`units[].body` reports `"graphics"` when geometry was supplied, alongside the
existing `"rectangle"` and glyph names. No tool, argument, or existing response
field was renamed or removed.

**Four request shapes that previously returned success now fail**, because each
wrote something other than what was asked for:

| request | before | now |
|---|---|---|
| `rect`, `circle` or `poly` with no `fill` (schema-required) | `(fill (type none))` written | `invalid_argument` naming `graphics[i]` |
| any primitive carrying a key outside its schema | key ignored, the rest drawn | `invalid_argument` naming `graphics[i].<key>` |
| a pin with no numeric `x`/`y` in a unit supplying `graphics` | pin written at `(0 0)` | `invalid_argument` naming `units[i].pins[j].x` |
| `graphics` present but not an array | read as absent; automatic body and success | `invalid_argument` |

Nothing is written to the library file in any of those cases. Callers that
depended on the defaults must now send `fill`, drop the extra key, or supply the
pin coordinates. This additive argument and these refusals are planned for the
next minor release.

## Unreleased: `estimate_cost` and `validate_for_manufacturing` count copper structurally (minor release)

Both tools counted copper layers by finding the substring `signal)` in the
board text, which misses every `power`, `mixed` and `jumper` copper layer and
quoted a six-layer board as two-layer (#461). Both now read the `(layers …)`
table through the same function `get_board_info` uses, so the three tools
report one number for one file.

`estimate_cost` keeps its optional `layers` argument as the count to quote at.
Two additive response fields make an override visible instead of silent:
`board.board_copper_layers` (what the file declares) beside the existing
`board.copper_layers` (what was priced), and a top-level `warnings` array that
names the discrepancy when they differ, or states that the file declares no
copper layers at all. Omitting `layers` now prices at the board's declared
count rather than a substring count clamped to a minimum of two; a board with
no `(layers …)` table reports `0` and a warning rather than an invented `2`.
`validate_for_manufacturing`'s `board_info.copper_layers` changes value on any
board with non-`signal` copper; its shape is unchanged.
## Unreleased: `find_single_pin_nets` counts pins, not labels (minor release)

`find_single_pin_nets` counted label instances per net name, so an ordinary net
— one label on a wire reaching two or more pins — was reported, and a genuine
single-pin net disappeared as soon as it carried a second label. Membership now
comes from the shared net graph: a net is reported when it reaches **at most one
pin**, zero included, since a label whose net reaches nothing is the orphan label
and the deleted-component stub the tool is sent looking for. Hierarchical sheet
pins count as pins; a power symbol's own pin does not, or the rail reaching
exactly one component pin would be hidden.

Every existing field remains with its existing meaning: `single_pin_net_count`,
and per net `net`, `x`, `y`, and `type`. `type` is still the kind of the *first*
label found, so a consumer matching on it is unaffected.

Results add five fields:

- `pin_count` — pins the net reaches, `0` or `1` for a reported net.
- `label_count` — label instances naming it, the value the old membership rule
  used. A net with no label at all is a different smell, so the count is kept.
- `pins` — the reached pin, as a one-or-zero-element array of the same
  `component_pin` / `sheet_pin` objects the other connectivity tools return.
- `label_types` — every distinct label kind naming the net, sorted. Local labels
  are extracted first, so a net carrying both a local and a global label reports
  `type: "NetLabel"` and says nothing about the global one; this field does.
- `cross_sheet_unverified` — true when any label naming the net can carry it off
  this sheet (global, hierarchical, or a power symbol). The answer is per sheet,
  and a flagged net is a lead rather than a finding. Such nets are still
  reported: a rail reaching one pin on this sheet is worth showing.

**`single_pin_net_count` changes meaning**: it now counts nets reaching at most
one actual pin, not nets named by exactly one label instance. A consumer reading
it as a defect count gets a smaller, truer number and needs no migration; one
that had learned to ignore this tool's noise can stop. No consumer can keep the
old set, since it was wrong in both directions.

No tool, argument, or existing response field was renamed or removed.

## Unreleased: guarded PCB file fallback reports its observed reason

Hybrid PCB mutation tools may use their existing direct-file fallback when
KiCad IPC is unreachable or when a reachable KiCad positively reports that the
requested board is not open. A successful file-path result now includes
`fallback_reason.kind` (`transport_unreachable` or `board_not_open`) and
`fallback_reason.message`; its warning is derived from that same observation.
Existing `source: "file"` and operation-specific fields remain unchanged.

If KiCad answers but any relevant open PCB document identity is empty, bare,
malformed, unresolved, duplicated, or otherwise prevents a complete comparison,
the tool fails closed with structured error kind `ambiguous_open_board` and the
requested `path`. No IPC mutation or file mutation is attempted. Callers should
make KiCad's open documents identifiable, then retry; they must not treat this
error as permission to edit the saved file directly.

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

## Unreleased: committed schematic component mutation readback (minor release)

Schematic component placement, batch placement, field edits and renames, moves,
rotations, annotations, and grouping now bind the selected symbol UUIDs before
writing and build their success responses from one reload of the committed
schematic. Existing response fields remain. Results add observed identity and
placement evidence including `schematic`, `reference`, `uuid`, `lib_id`,
`unit_count`, `units`, `fields`, coordinates, rotation, and instance paths or
references where applicable. Grouping returns the same evidence per component.

Success additionally requires every bound unit's observed unit number, library
ID, project/hierarchy paths, x/y, rotation, and property values to match the
preselected target plus the intended mutation. Placement compares requested
Reference/Value, library and unit, and its tool's coordinate rules (component
placement snaps to 1.27 mm; power placement retains requested coordinates).
Edits preserve the other bound values; moves preserve relative unit positions,
and rotations preserve relative unit angles. Coordinate/angle comparisons allow
only serialization rounding below 0.000001 mm/degrees.

Missing, malformed, stale-revision, or wrong-document identities refuse with
`stale_target`, including mismatched intended values. Component-target
resolution and committed readback reject duplicate UUID, reference/unit,
property, or instance identities and conflicting project, instance-unit,
or cross-unit hierarchy records
with the new `ambiguous_target` kind and include their candidates whenever
Konnect cannot prove one top-level symbol per bound UUID and one logical
reference across its units. A
post-write verification refusal can follow a committed write, so inspect and
reload the saved schematic before retrying. A move commits the symbol placement
before a separate junction-reconciliation write; if that second write or final
readback refuses, the move can already be durable. This additive response
change is planned for the next minor release; no tool or argument was renamed
or removed.

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

Missing, protected, malformed, stale, wrong-document, or editor-locked targets
refuse with `stale_target` before a write. Duplicate UUID or reference/unit
identities refuse with `ambiguous_target` when Konnect cannot prove a unique
safe deletion. A post-write readback
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

## Unreleased: JLCPCB manufacturing files use vendor-ready names and schema

`export_manufacturing_package(fab_house="jlcpcb", include_assembly=true)` now
publishes `BOM-<project>.csv` and `CPL-<project>.csv` instead of `bom.csv` and
`positions.csv`. The CPL contains JLCPCB's documented `Designator`, `Mid X`,
`Mid Y`, `Layer`, and `Rotation` columns rather than KiCad's native position
headers. The existing `files_generated.type="pick_and_place"` discriminator is
unchanged.

JLCPCB assembly exports require `position_units="mm"`. Grouped BOM references
are individually enumerated and DNP parts are excluded from both the BOM and
CPL. A malformed CPL, compressed BOM range, or BOM/CPL designator mismatch
returns an incomplete/error result instead of an upload instruction. Generic
and other-fabricator exports retain the existing `bom.csv`/`positions.csv`
names, inclusion policy, and KiCad-native position schema.

JLCPCB CPL rotation and position corrections are now applied after KiCad's
native geometry export. The optional `jlcpcb_cpl_corrections_path` input points
to a versioned project JSON policy; exact designator overrides take precedence
over the first matching project footprint prefix, which takes precedence over
Konnect's independently verified built-in rules. See
[JLCPCB CPL corrections](JLCPCB_CPL_CORRECTIONS.md) for the policy schema.

The response adds `placement_orientation` at the top level and on the
`pick_and_place` artifact. It records policy provenance, each applied rule with
before/after values, and every unmatched footprint. Its status is always
`PREVIEW_REQUIRED` and `physical_validation` is always `false`: a structurally
complete package is not evidence that JLCPCB's selected component models are
physically aligned. Inspect every part in Component Placements before ordering.

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
