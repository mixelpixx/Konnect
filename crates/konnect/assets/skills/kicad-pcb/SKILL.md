---
name: kicad-pcb
description: |
  Workflow skill for KiCAD PCB layout and routing via MCP tools. Triggers on: "layout the board",
  "route traces", "PCB", "place footprints", "copper pour", "board outline", "differential pair",
  "board setup", "track width", "via", "zone", "design rules", "stackup", "silkscreen".
argument-hint: "[layout task]"
---

# KiCAD PCB Layout Workflow

This skill guides Claude to perform PCB layout using Konnect MCP tools.
ALL modifications go through MCP tools — never edit .kicad_pcb files directly.

---

## Prerequisites

Most PCB layout operations require KiCAD to be running with the board file open. The IPC
connection communicates with the running KiCAD instance in real-time.

Some board-construction and component tools have guarded closed-board paths. IPC-first
tools fall back to the file only when the transport is unreachable and the target board
has not been observed live during this server session. File-only operations such as
`flip_component` proceed only when KiCad does not hold the target board open. These
paths use revision-aware atomic writes: placement preserves pads, graphics, attributes,
and models; moves preserve the existing angle; rotations update the footprint and its
child angles; flips mirror supported geometry and swap front/back layers. A reachable
KiCad rejection stays closed instead of racing the editor.

`unsafe_file_fallback` is a stop condition. It means Konnect reached this board live
earlier in the current server session but IPC is now unreachable, so the saved file may
be older than lost editor state. Pause mutation work, tell the user that Konnect left
the file unchanged, and ask them to reopen/recover, reconcile, and save the board in
KiCad. Continue through live IPC afterward. Preserve the guard: do not retry-loop,
restart Konnect automatically, or edit `.kicad_pcb` directly. If the user confirms a
clean close and an authoritative saved file, they may restart Konnect to deliberately
begin a new closed-board session.

If connection fails:
- Tell the user to open KiCAD and load the project
- The board (.kicad_pcb) must be open in the PCB editor
- KiCAD's IPC API must be enabled (default in KiCAD 8+)

---

## Toolset Loading

Before any PCB work, load the required toolsets:

```
load_toolset('pcb_board')        # board outline, layers, setup, stackup
load_toolset('pcb_components')   # place, refresh, move, rotate, align footprints
load_toolset('pcb_routing')      # traces, vias, differential pairs
load_toolset('sch_export')       # update PCB from the saved schematic hierarchy
```

Zones (`pcb_board`: add_zone; `pcb_routing`: add_copper_pour), component/net queries (`pcb_components`: find_component, get_component_list; `pcb_board`: get_board_info), and bulk placement (`pcb_components`: place_component_array, align_components, duplicate_component) are already covered by the toolsets loaded above.

Load additional toolsets as needed:

```
load_toolset('config')           # design rule storage: add_design_rule, list_design_rules
load_toolset('verification')     # run_drc, set_design_rules, set_predefined_sizes, check_clearance
```

Always call `get_active_toolsets()` first to see what is already loaded.

### References by decision

- Read [`references/layer-reference.md`](references/layer-reference.md) when
  selecting a copper, fabrication, user, or mechanical layer or deciding which
  side owns an item.
- Read [`references/trace-width-table.md`](references/trace-width-table.md) when
  sizing a current-carrying trace, via, or controlled-impedance route. It defines
  the required calculation inputs and acceptance record; it is not a lookup table.
- Read [`references/design-rules.md`](references/design-rules.md) when creating
  netclasses, configuring project constraints, or adjudicating DRC results.

---

## Layout Order

Follow this sequence for a clean PCB workflow:

1. **Board outline** — `set_board_size` or draw Edge.Cuts geometry. Both outline tools
   append, so resize with `delete_graphics(layer='Edge.Cuts')` first — a second call
   without it leaves two overlapping outlines and a DRC failure.
2. **Update from schematic** — call `update_pcb_from_schematic` first with
   `dry_run: true`. Review `status`, `coverage`, `diagnostics`, and staged positions.
   Apply only with `dry_run: false` and the exact returned
   `expected_plan_revision` value. The saved schematic hierarchy must be closed in the
   schematic editor, and the target board must be open in KiCad. A conflict is
   non-mutating; resolve it and rerun the dry run. A successful apply is one KiCad
   undo entry, so Ctrl-Z reverses the whole update.
3. **Refresh changed libraries** — when a linked footprint library changed, use
   `update_footprints_from_library`, the MCP equivalent of KiCad **Tools → Update
   Footprints from Library**. This is distinct from `update_pcb_from_schematic`:
   it refreshes supported library-owned pads, graphics, attributes, metadata, and
   3D models without changing references, placement, side, rotation, KIID, symbol
   metadata, instance overrides, or pad nets. Always call it first with
   `dry_run: true`; apply only with `dry_run: false` and the exact returned
   `expected_plan_revision`. The requested board must be open in live KiCad, one
   apply is one undo entry, and unsupported or stale content returns a non-mutating
   conflict instead of silently dropping it.
4. **Place components** — position all footprints
5. **Route traces** — connect all nets
6. **Copper pour** — add ground/power zones last
7. **DRC** — run design rule check
8. **Save** — `save_project`

Do NOT add copper pours before routing is complete — they interfere with interactive routing.

---

## Placement

### Strategy

- Group components by functional block (power, digital, analog, connectors)
- Place ICs first, then their associated passives
- Decoupling caps: within 2mm of their IC power pins, on same layer
- Connectors: at board edges, accessible for cables
- High-frequency components: minimize trace lengths between them
- Thermal considerations: power components away from sensitive analog

### Placement Tools

| Tool                      | Use Case                                    |
|---------------------------|---------------------------------------------|
| `place_component`         | Position one footprint via IPC or safe file fallback |
| `update_footprints_from_library` | Refresh placed definitions from linked libraries |
| `move_component`          | Relocate a footprint via IPC or safe file fallback |
| `rotate_component`        | Rotate a footprint via IPC or safe file fallback |
| `flip_component`          | Set F.Cu/B.Cu on a closed board with geometry mirroring |
| `align_components`        | Align multiple components (top/bottom/left/right/center) |
| `place_component_array`   | Grid placement for repeated elements        |

### Score-first automation

Load `load_toolset('placement')` for the automation loop. The discipline is
score, change, re-score — every planner reports the board's score before and
after its own plan, so a change is judged before it is made:

1. `score_placement` — 0-100 with named deductions; hard failures (courtyard
   overlaps, parts outside the outline) decide the verdict regardless of the
   number, and a board with no outline can never pass.
2. `auto_place_from_schematic` — deterministic first placement by net
   clusters; explicitly a starting point, not a final layout.
3. `refine_placement_force_directed` — deterministic spring embedder; pass
   `locked` for parts that must not move. Same input, same plan.
4. `place_decoupling_caps` — plans a row beside an IC, paired by shared nets.
5. `plan_bga_fanout` — pitch detected from the pad grid; `apply` executes as
   one KiCad undo commit over live IPC.

Every planner is dry-run by default; apply refuses while KiCad holds the
board open live (fanout apply is the inverse: it REQUIRES the live board).

### Placement Tips

- Use mm coordinates (KiCAD default for PCB)
- Standard grid: 0.5mm for placement, 0.25mm for fine adjustment
- Check component courtyard overlaps after placement
- Reference designator text: F.SilkS layer, 1mm height default

---

## Routing

### Routing Tools

| Tool                      | Use Case                                    |
|---------------------------|---------------------------------------------|
| `route_pad_to_pad`        | Direct connection, auto L-bend routing      |
| `route_trace`             | Manual segment-by-segment routing           |
| `route_differential_pair` | Matched-length USB/LVDS/Ethernet pairs      |
| `add_via`                 | Layer transition                            |
| `create_netclass`         | Define width/clearance rules for net groups |

### route_pad_to_pad

The primary routing tool. Looks up both pad positions on the board and lays an
L-shaped trace between them.

```
route_pad_to_pad(board, net_name, ref1, pad1, ref2, pad2, layer?, width?)
```

- Emits one segment when the pads already share an X or Y, two otherwise
- Specify the width in mm from the accepted project netclass or sizing record.
- Routes entirely on `layer` (default `F.Cu`) — it does not add a via. To
  change layer mid-route, place the via yourself with `add_via` and route each
  side separately

### route_trace

One straight segment between two explicit points, for when you want to control
the path yourself.

```
route_trace(board, net_name, layer, x1, y1, x2, y2, width?)
```

- Use when auto-routing creates suboptimal paths
- There is no waypoint list: call it once per segment to build a polyline
- Coordinates are board-space mm

### route_differential_pair

For differential signals (USB, HDMI, Ethernet, LVDS).

```
route_differential_pair(board, net_pos, net_neg, x1, y1, x2, y2, gap?, layer?, width?)
```

- Lays two straight traces parallel to the given line, offset `(gap + width)/2`
  either side, so spacing is constant along the segment
- Not a length-matching router: it adds no serpentine tuning, and equal length
  only follows from the two traces being parallel segments. Skew introduced
  before or after this call is yours to correct
- Common pairs: USB_D+/USB_D-, LVDS_P/LVDS_N

### Netclasses

Define routing rules for groups of nets:

```
create_netclass(board, name, trace_width?, clearance?, via_drill?, via_diameter?)
```

The class is written to the project's `.kicad_pro` file, which is where KiCad
has kept netclasses since v7 — the board file is not modified.

Before creating or updating a class, read `get_netclasses` and the applicable
design-rule/trace-sizing references. Derive width, clearance, gap, drill, and
diameter from the selected fabrication contract, stackup, and electrical
calculation. Read the classes back after the write and confirm every special net
resolves through the intended class. Missing inputs make the rule `INCOMPLETE`.

### Pre-defined sizes

Netclass width is the default. The Track/Via dropdowns are a separate palette
in the sibling `.kicad_pro`. Fill them with `set_predefined_sizes` so `W` /
`Shift+W` can step through extra widths without changing netclasses:

The values below show call syntax only; they are not engineering recommendations.
Replace every value with one from the accepted project sizing record, derived
from the current fabrication contract, stackup, and electrical requirements. If
that evidence is unavailable, report the sizing task as `INCOMPLETE` instead of
reusing these illustrative values.

```
set_predefined_sizes(board, track_widths=[0.2, 0.5, 0.8],
    via_dimensions=[{diameter:0.6, drill:0.3}, {diameter:0.8, drill:0.4}])
```

A leading 0 mm / 0,0 via is always kept as “use netclass values”. These sizes
are not DRC limits. KiCad reads the list on next project open.

---

## Copper Pour

Zone tools live in the `pcb_board` toolset.

### add_zone

Creates a copper pour area (polygon fill).

```
add_zone(board, net_name, layer, points, clearance?, min_width?,
         name?, priority?, pad_connection?)
```

- Almost always GND net on both F.Cu and B.Cu
- `points` is the outline polygon; define it slightly inside the board edge
  (0.5mm inset)
- `priority` defaults to 0; the higher priority wins where two pours overlap
- `pad_connection` is `solid` | `thermal` | `none`, defaulting to `thermal`
  as KiCad does
- With KiCad running on this board the zone is created over IPC and refilled
  for you, so it appears at once and is in KiCad's undo stack. Without a live
  KiCad it goes into the file instead, and the result says so (`source: file`)
  and carries a `warning` describing the process-local evidence and cold-start
  limitation. A board observed live earlier in this server session fails with
  `unsafe_file_fallback` instead of writing the file.

### refill_zones

**Must call `refill_zones` after any change that affects copper pour:**
- After adding/moving components
- After routing new traces
- After modifying zone outlines
- After changing design rules

Zones do not auto-update — stale fills cause DRC errors.

### Zone Tips

- GND pour on both layers is standard practice
- Leave spoke thermal reliefs for through-hole pads (easier soldering)
- Use keepout zones to prevent copper in sensitive areas
- Zone clearance typically 0.3-0.5mm from traces

---

## Layer Reference

| Layer    | Name     | Purpose                              |
|----------|----------|--------------------------------------|
| F.Cu     | Front Copper   | Top copper traces and pads     |
| B.Cu     | Back Copper    | Bottom copper traces and pads  |
| F.SilkS  | Front Silk     | Top silkscreen (text, outlines)|
| B.SilkS  | Back Silk      | Bottom silkscreen              |
| F.Mask   | Front Mask     | Top solder mask openings       |
| B.Mask   | Back Mask      | Bottom solder mask openings    |
| Edge.Cuts| Board Outline  | Physical board boundary        |
| F.Fab    | Front Fab      | Top fabrication drawing        |
| B.Fab    | Back Fab       | Bottom fabrication drawing     |
| F.CrtYd  | Front Courtyard| Top component clearance area   |
| B.CrtYd  | Back Courtyard | Bottom component clearance area|
| In1.Cu   | Inner 1        | Internal copper layer 1        |
| In2.Cu   | Inner 2        | Internal copper layer 2        |

### Layer Usage Guidelines

- Route signals on F.Cu and B.Cu (2-layer) or add inner layers for complex boards
- Board outline MUST be on Edge.Cuts (closed polygon or rectangle)
- Silkscreen for reference designators and polarity marks
- Courtyard defines minimum spacing between components
- Use F.Fab/B.Fab for assembly drawings and component outlines

---

## Design Rule Check

After completing layout:

```
run_drc()
```

Common DRC errors and fixes:
- **Clearance violation**: move trace or component further apart
- **Unconnected net**: route missing connection
- **Track too close to edge**: move inward from board outline
- **Courtyard overlap**: increase spacing between components
- **Zone fill error**: run `refill_zones`

### Read `owner` before deciding on a board-edge violation

Every violation item carries `owner` and `ownership_status`. Read them before
choosing a fix — `"Circle of J1 on Edge.Cuts"` reads identically whether that
geometry is the board outline or a cutout the footprint carries itself.

- `owner.kind: "board"` — the item is the board's own geometry. Move the
  offending copper inward, or change the outline.
- `owner.kind: "footprint"` — the geometry belongs to that footprint
  (`owner.reference` names it), typically a connector's locking-peg cutout. It
  is still real fabrication geometry and the violation is still real, but the
  pad and the cutout move together, so **repositioning the component cannot fix
  it**. Review the footprint definition or the rule instead.
- `ownership_status` other than `"resolved"` (`"uuid_missing"`,
  `"not_found"`) — ownership is unknown, and `owner` is `null`. Do not assume
  the board owns it; check with `list_board_footprint_graphics` before advising
  a move.

---

## Rules

1. **Never edit .kicad_pcb directly** — all changes go through MCP tools
2. **Always verify placement after moves** — components may snap to unexpected positions
3. **Board outline first** — define the physical boundary before placing anything
4. **Refill zones after changes** — stale zone fills cause phantom DRC errors
5. **Check DRC before finishing** — run `run_drc()` and resolve all errors
6. **Use netclasses for consistency** — define track widths per net type, not per trace
7. **KiCAD normally must be running** — use guarded closed-board paths only when a
   tool explicitly offers them. Treat `unsafe_file_fallback` as a human recovery
   boundary; other PCB edits still require the live IPC connection.
8. **Save frequently** — call `save_project` after major operations
9. **Load toolsets first** — check `get_active_toolsets()` and load what you need
10. **Copper pour last** — add zones only after routing is substantially complete
