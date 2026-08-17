---
name: kicad-library
description: |
  Library management workflow for KiCAD — creating symbols, footprints, and managing libraries
  via MCP tools. Triggers on: "create a symbol", "make a footprint", "custom component",
  "register library", "find a part", "pin numbering", "new symbol", "new footprint",
  "add to library", "library path", "pad layout".
argument-hint: "[component or library task]"
---

# KiCAD Library Management Workflow

This skill guides Claude through creating and managing KiCAD symbols, footprints, and libraries
using Konnect MCP tools. ALL modifications go through MCP tools — never edit
.kicad_sym or .kicad_mod files directly.

---

## Toolset Loading

```
load_toolset('library')    # search_symbols, search_footprints, create_symbol, create_footprint,
                           # edit_footprint_pad, set_footprint_graphics, set_footprint_metadata,
                           # set_footprint_models, get_footprint_info, register_symbol_library,
                           # register_footprint_library, get_symbol_info
load_toolset('pcb_components') # update_footprints_from_library for placed instances
```

Always call `get_active_toolsets()` first to see what is already loaded.

---

## Refresh Placed Footprints

Editing a `.kicad_mod` library file does not change copies already embedded on a
board. After a library edit, call `update_footprints_from_library`; it is Konnect's
MCP equivalent of KiCad **Tools → Update Footprints from Library**.

This operation is not `update_pcb_from_schematic`. Schematic sync updates
schematic-owned fields while preserving artwork. Library refresh replaces supported
library-owned pads, graphics, attributes, metadata, and 3D models while preserving
the placed footprint's reference, value, position, rotation, board side, locked
state, KIID, symbol metadata, instance overrides, and pad nets by logical pad
number.

Always dry-run first:

```text
update_footprints_from_library(
  board,
  references?,
  library_ids?,
  dry_run,
  expected_plan_revision?
)
```

- Use `dry_run=true` and review `status`, `coverage`, `changes`, and
  `diagnostics`.
- Apply with `dry_run=false` and the exact returned `expected_plan_revision`.
- Keep the requested board open in live KiCad; there is no closed-board file
  fallback.
- One successful apply is one KiCad undo entry.
- A stale plan, unresolved library, removed connected pad, or unsupported
  library content returns a non-mutating conflict for the whole selection.

---

## Search First Principle

**Always search existing libraries before creating custom components.**

```
search_symbols(query)       # Search all symbol libraries
search_footprints(query)    # Search all footprint libraries
```

KiCAD ships with extensive libraries. Common parts almost always exist:
- Standard passives (R, C, L) → `Device` library
- Connectors → `Connector_Generic`, `Connector_USB`, `Connector_HDMI`, etc.
- Common ICs (STM32, ATmega, LM7805, NE555) → manufacturer-specific libraries
- Transistors/MOSFETs → `Transistor_FET`, `Transistor_BJT`

Only create a custom symbol/footprint when:
- The part does not exist in any library
- The existing symbol has wrong pin count/arrangement
- You need a proprietary/unusual package

---

## Symbol Creation

### Pin Numbering Conventions

| Component Type     | Convention                                           |
|--------------------|------------------------------------------------------|
| IC (DIP/SOIC/QFP) | Counter-clockwise from pin 1 (standard IC convention)|
| Passives (R, C, L) | Pin 1 and Pin 2                                    |
| Diodes             | Pin 1 = Anode (A), Pin 2 = Cathode (K)             |
| Transistors (BJT)  | 1=Base, 2=Collector, 3=Emitter (BCE)                |
| MOSFETs            | 1=Gate, 2=Drain, 3=Source (GDS)                     |
| Connectors         | Sequential from 1                                    |
| Crystal            | Pin 1, Pin 2 (+ case ground if 4-pin)               |

### Pin Types

| Type            | Use For                                              |
|-----------------|------------------------------------------------------|
| `input`         | Logic/analog inputs, gate inputs                     |
| `output`        | Logic/analog outputs, push-pull drivers              |
| `bidirectional` | Data bus lines, I2C SDA, GPIO                        |
| `tri_state`     | Outputs with high-impedance state                    |
| `passive`       | Resistor/capacitor/inductor pins, crystal pins       |
| `power_in`      | VCC, VDD, GND pins (power consumer)                 |
| `power_out`     | Regulator output, power source pins                  |
| `open_collector`| Open-drain/open-collector outputs                    |
| `open_emitter`  | Open-emitter outputs                                 |
| `unspecified`   | Pins with no clear electrical type                   |
| `no_connect`    | Pins that must not be connected                      |

### Required Symbol Properties

Every symbol must have these fields:

| Property    | Description                          | Example                      |
|-------------|--------------------------------------|------------------------------|
| Reference   | Designator prefix                    | U, R, C, J, D, Q, L         |
| Value       | Part name or value                   | STM32F103C8, 10k, 100nF     |
| Footprint   | Default footprint assignment         | Package_SO:SOIC-8_3.9x4.9mm |
| Datasheet   | URL to datasheet                     | https://...                  |

Optional but recommended:
- `LCSC` — LCSC/JLCPCB part number for assembly
- `MPN` — Manufacturer part number
- `Description` — Short text description

### Symbol Layout Guidelines

- Pin 1 indicator (dot or bar) on the symbol body
- Inputs on the left side, outputs on the right side
- Power pins on top (VCC) and bottom (GND)
- Pin spacing: 2.54mm (100mil) standard grid
- Symbol body: rectangle for ICs, standard shapes for passives/discretes

---

## Footprint Creation

### Pad Types

| Type            | Use For                                              |
|-----------------|------------------------------------------------------|
| `smd`           | Surface-mount pads (no drill hole)                   |
| `thru_hole`     | Through-hole pads (plated drill)                     |
| `np_thru_hole`  | Non-plated through hole (mounting holes, slots)      |

### Pad Side and Shape Details

`create_footprint` defaults SMD pads to the front-side layer set
`["F.Cu", "F.Paste", "F.Mask"]`. For bottom-side SMD pads, set
`layers=["B.Cu", "B.Paste", "B.Mask"]` explicitly. Through-hole pads default to
`["*.Cu", "*.Mask"]`.

Each pad may also set:

- `rotation` in degrees; omit it for `0`.
- `roundrect_rratio` from `0` through `0.5` when `shape="roundrect"`.
- `layers` using only canonical pad layers: `F.Cu`, `B.Cu`, `F.Paste`, `B.Paste`,
  `F.Mask`, `B.Mask`, `*.Cu`, and `*.Mask`.

```text
create_footprint(
  ...,
  pads=[{
    number: "1",
    type: "smd",
    shape: "roundrect",
    x: -8.075,
    y: 4.7,
    width: 2.5,
    height: 2.55,
    layers: ["B.Cu", "B.Paste", "B.Mask"],
    rotation: 180,
    roundrect_rratio: 0.2
  }]
)
```

### Standard Pad Sizes Reference

| Package   | Pad Size (mm)   | Pitch (mm) | Notes                        |
|-----------|-----------------|------------|------------------------------|
| 0402      | 0.5 x 0.5      | —          | 2 pads, 0.5mm gap           |
| 0603      | 0.8 x 0.8      | —          | 2 pads, 0.8mm gap           |
| 0805      | 1.0 x 1.0      | —          | 2 pads, 1.0mm gap           |
| 1206      | 1.5 x 1.2      | —          | 2 pads, 1.6mm gap           |
| SOT-23    | 0.9 x 0.7      | 0.95       | 3 pads                       |
| SOT-23-5  | 0.9 x 0.7      | 0.95       | 5 pads                       |
| SOIC-8    | 1.5 x 0.6      | 1.27       | 8 pads, 5.4mm row spacing   |
| SOIC-16   | 1.5 x 0.6      | 1.27       | 16 pads, 5.4mm row spacing  |
| TSSOP-16  | 1.4 x 0.4      | 0.65       | 16 pads, 4.4mm row spacing  |
| QFP-32    | 1.5 x 0.3      | 0.8        | 32 pads, quad-flat           |
| QFP-48    | 1.5 x 0.3      | 0.5        | 48 pads, quad-flat           |
| QFN-32    | 0.7 x 0.25     | 0.5        | 32 pads + exposed pad       |
| QFN-48    | 0.7 x 0.25     | 0.5        | 48 pads + exposed pad       |

### Courtyard

- Extend courtyard **0.25mm** beyond the outermost pad edges on all sides
- This ensures minimum spacing between components during assembly
- Use `F.CrtYd` layer, 0.05mm line width

### Footprint Layers

| Layer      | Purpose                                               |
|------------|-------------------------------------------------------|
| F.Cu       | Front copper (pads)                                   |
| B.Cu       | Back copper (pads for bottom-side components)         |
| F.Mask     | Front solder mask opening (auto-generated from pads)  |
| B.Mask     | Back solder mask opening                              |
| F.Paste    | Front solder paste (stencil openings)                 |
| B.Paste    | Back solder paste (stencil openings)                  |
| F.SilkS    | Front silkscreen (component outline, pin 1 marker)    |
| F.CrtYd    | Front courtyard (assembly spacing)                    |
| F.Fab      | Front fabrication (true component dimensions)         |

### Footprint Layout Guidelines

- Pin 1 marker on silkscreen (dot, bar, or chamfered corner)
- Component outline on F.Fab layer with true dimensions
- Silkscreen outline 0.1mm outside F.Fab outline
- Reference (`%R`) on F.SilkS, readable at 1.0mm text height
- Value on F.Fab layer

### Existing Footprint Graphics

Use `set_footprint_graphics` to add or change line, arc, rectangle, circle, or polygon
primitives in an existing `.kicad_mod`. Never patch the file directly.

```text
set_footprint_graphics(
  footprint path,
  selector={layer: "B.CrtYd"},
  mode="replace",
  graphics=[...]
)
```

- `append` preserves existing supported graphics on the selected layer.
- `replace` replaces all supported graphics on the selected layer in one atomic write.
- `delete` removes all supported graphics on the selected layer.
- Text, pads, properties, models, groups, and graphics on other layers are preserved.
- Replacement/deletion stops with a conflict if a selected graphic belongs to a group;
  do not work around this by editing the file.
- Polygons close automatically. Supply at least three distinct points; repeating the
  first point at the end is optional.
- Use `get_footprint_info` with graphics inclusion enabled and a layer filter to verify
  the resulting type, geometry, stroke width, fill, and item ID.

### Existing Footprint Metadata

Use `set_footprint_metadata` to replace a footprint description, search tags, or
KiCad footprint attributes without changing pads, graphics, properties, groups, or
3D models.

```text
set_footprint_metadata(
  footprint_path=".../Part.kicad_mod",
  description="Optional replacement",
  tags=["keyboard", "hot_swap"],
  attributes=["exclude_from_pos_files"]
)
```

- Supply at least one metadata field.
- Only supplied fields change.
- Empty `tags` or `attributes` removes the corresponding block.
- `exclude_from_pos_files` omits the footprint from pick-and-place output without
  implicitly adding `exclude_from_bom`.
- Use `edit_footprint_pad` with `new_number` and optional `match_all=true` to
  renumber one or every matching direct-child pad atomically.

### Existing Footprint 3D Models

Use `set_footprint_models` to append, replace, or delete top-level 3D model
associations without changing any non-model footprint content.

```text
set_footprint_models(
  footprint_path=".../Part.kicad_mod",
  mode="replace",
  models=[{
    path: "../models/Part.step",
    offset: {x: 0, y: 0, z: 0},
    scale: {x: 1, y: 1, z: 1},
    rotate: {x: 0, y: 0, z: 90}
  }]
)
```

- `append` and `replace` require at least one model.
- `delete` requires models to be omitted or empty and removes every top-level model.
- Omitted transforms default to offset `0/0/0`, scale `1/1/1`, and rotation `0/0/0`.
- Multiple model blocks are written in payload order in one atomic operation.

---

## Library Registration

### Register a Symbol Library

```
register_symbol_library(nickname, library_path, scope?, project?, replace_existing?)
```

### Register a Footprint Library

```
register_footprint_library(nickname, library_path, scope?, project?, replace_existing?)
```

- Registration is idempotent by default: an existing nickname is left unchanged.
- Set `replace_existing=true` to update a stale URI for that nickname in place.
- Project-local and same-repository sibling libraries are written as portable
  `${KIPRJMOD}` URIs; existing options, descriptions, and unrelated entries are preserved.

### Scope

| Scope       | Location                        | Visible To           |
|-------------|----------------------------------|----------------------|
| `global`    | User-level sym-lib-table         | All projects         |
| `project`   | Project-level sym-lib-table      | This project only    |

**Recommendation**: Use `project` scope for project-specific custom parts.
Use `global` scope only for reusable personal libraries used across multiple projects.

### Library File Locations

- Symbol libraries: `*.kicad_sym` files
- Footprint libraries: directories containing `*.kicad_mod` files
- Project-level tables: `sym-lib-table` and `fp-lib-table` in project directory

---

## IPC Naming Conventions (Brief Reference)

Standard footprint naming follows IPC-7351:

```
[Type]_[Dimensions]_[Pitch]_[Suffix]
```

Examples:
- `R_0402_1005Metric` — 0402 resistor (1.0x0.5mm metric)
- `C_0805_2012Metric` — 0805 capacitor (2.0x1.2mm metric)
- `SOIC-8_3.9x4.9mm_P1.27mm` — SOIC-8 with 1.27mm pitch
- `QFN-32-1EP_5x5mm_P0.5mm` — QFN-32 with exposed pad, 5x5mm body
- `SOT-23` — SOT-23 3-pin
- `TSSOP-16_4.4x5mm_P0.65mm` — TSSOP-16 with 0.65mm pitch

Dimension format: `LxW` in mm (body dimensions, not pad-to-pad).

---

## Common Workflows

### Create a New IC Symbol + Footprint

1. `search_symbols(query)` — pass the part name; confirm it does not exist
2. `search_footprints(query)` — pass the package name; check if the footprint
   exists (often it does)
3. Create symbol with correct pin count, names, numbers, and types
4. Assign existing footprint OR create custom footprint from datasheet
5. Register library if new
6. Set Footprint property on symbol to link them

### Add LCSC Number to Existing Components

1. Load `sch_analysis` toolset
2. Find components missing LCSC field
3. Search JLCPCB parts for matching part numbers
4. Update component fields with LCSC numbers

### Create Project-Specific Library

1. Create new `.kicad_sym` file for symbols
2. Create new directory for footprints
3. Register both with `project` scope
4. Add custom components as needed

---

## Rules

1. **Always search before creating** — most parts already exist in KiCAD libraries
2. **Never edit .kicad_sym or .kicad_mod directly** — use MCP tools only
3. **Follow pin numbering conventions** — IC pins counter-clockwise from pin 1
4. **Set pin types correctly** — ERC depends on accurate pin types
5. **Include all required properties** — Reference, Value, Footprint, Datasheet
6. **Use 0.25mm courtyard margin** — standard clearance for assembly
7. **Mark pin 1 clearly** — both on symbol and footprint silkscreen
8. **Use project scope by default** — avoid polluting global libraries
9. **Name footprints per IPC** — consistent naming helps future reuse
10. **Load toolsets first** — check `get_active_toolsets()` and load `library` before starting
11. **Use layer-scoped graphic edits deliberately** — `replace` and `delete` affect every
    supported primitive on the selected layer
