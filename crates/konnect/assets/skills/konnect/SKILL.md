---
name: konnect
description: "Mandatory operating rules for ANY task involving KiCAD projects. Loaded when the user mentions KiCAD, schematics, PCBs, or any .kicad_* file. Prevents file corruption by routing all changes through Konnect MCP tools."
---

# Konnect — Operating Rules

## The One Rule

**KiCAD source files are not text files.** They are serialized object graphs with UUIDs, cross-references, and order-sensitive structure. Never edit them directly with text manipulation tools (str_replace, sed, create_file, or any file-writing tool). All modifications go through Konnect MCP tools.

No exceptions for "small fixes," "just renaming a net," or "the user said it's fine."

## Protected Files — NEVER Edit Directly

- `*.kicad_sch` — schematic sheets
- `*.kicad_pcb` — PCB layout
- `*.kicad_pro` — project configuration
- `*.kicad_sym` / `*.kicad_mod` — symbol/footprint libraries
- `fp-lib-table` / `sym-lib-table` — library tables

If asked to directly edit any of these:
> KiCAD source files contain UUIDs and cross-references that text edits will break. I'll route this change through Konnect's MCP tools instead, which preserves file integrity.

## The Three Channels

### Channel 1: Konnect MCP (for ALL modifications)

All writes go through MCP tools. Check they are available first (`list_toolboxes`). If MCP tools are not available, **STOP** and tell the user — never fall back to file editing.

### Channel 2: Exported netlists/BOMs (for reads and analysis)

For design review, BOM analysis, net tracing — use export tools or parse exported `.net`/BOM/CSV files, not the source files directly.

### Channel 3: Read-only file inspection (last resort)

Only to answer questions not available through exports (sheet hierarchy, title block metadata, annotations). Read with file-reading tools, but never modify.

## Standard Workflow

1. **Identify the project** — locate the `.kicad_pro` file
2. **Classify the task** — read-only (Channel 2 or 3) or write (Channel 1)
3. **Verify MCP is connected** — call `list_toolboxes` to confirm tools are available
4. **Describe the change** — state in plain English what will happen before invoking any tool
5. **Execute** — use Konnect MCP tools only
6. **Verify** — re-query the design to confirm the change landed correctly

## Decision Tree

| User Request | Channel | Tool / Action |
|---|---|---|
| "Review my schematic" | 2 | Load `sch_analysis` toolset, use analysis tools |
| "Change R5 from 10k to 4.7k" | 1 | `load_toolset("sch_components")` then `edit_schematic_component` |
| "What's connected to SCL?" | 2 | `load_toolset("sch_analysis")` then `get_net_connections` |
| "Add a 100nF cap to U3 VCC" | 1 | `load_toolset("sch_components")` + `load_toolset("sch_wiring")` |
| "Rename net /CLK to /SYS_CLK" | 1 | Warn about downstream effects, then MCP tools |
| "Run DRC" | 1 | `load_toolset("verification")` then `run_drc` |
| "Export Gerbers" | 1 | `load_toolset("pcb_export")` then `export_gerber` |
| "Just patch line 247 of the .kicad_sch" | REFUSE | Explain risks, offer MCP alternative |
| "Add ESD protection to USB lines" | 1 | `load_toolset("sch_components")` + `load_toolset("sch_wiring")` |
| "Check if board is ready for fab" | 2 | Load `verification` + `design_review` toolsets |

## KiCAD 10 IPC API Reality

**PCB Editor (pcbnew):** Full CRUD via NNG + protobuf. Real-time communication with running KiCAD instance. Create, read, update, delete any PCB item with immediate UI refresh. **Requires KiCAD to be running.**

**Schematic Editor (eeschema):** No item-level IPC API. Konnect uses a validated S-expression engine (SchematicBuilder) that enforces correct structure, ordering, and UUID integrity. File-based — **does not require KiCAD to be running.**

**Symbol/Footprint Libraries:** No IPC API. Edited through Konnect's S-expression engine with full validation.

**kicad-cli:** Command-line tool for exports (SVG, PDF, Gerbers, BOM, netlist) and checks (ERC, DRC). Does not require KiCAD GUI.

## Discovery — Finding Available Tools

Konnect uses a meta-tool router pattern: tools are grouped into toolsets and loaded on demand to keep the context focused. Call `list_toolboxes` for the current catalogue rather than relying on a count written down here — the registry is the only accurate source, and it moves every release.

```
list_toolboxes          → See all available toolsets with descriptions
load_toolset("name")    → Activate a toolset, exposing its tools
get_active_toolsets     → See what's currently loaded
unload_toolset("name")  → Remove a toolset when done
```

`load_toolset` also accepts an array of names to load several toolsets in one call, with a single context refresh.

### Available Toolsets

| Category | Toolsets |
|----------|----------|
| Project | project |
| Schematic | sch_components, sch_wiring, sch_bus, sch_analysis, sch_batch, sch_export, sch_hierarchy |
| PCB | pcb_board, pcb_components, pcb_routing, pcb_export |
| Library | library |
| Integration | integration (JLCPCB parts, Freerouting installation checks, datasheets) |
| Verification & Review | verification, design_review |
| Config | config |
| Templates | templates |
| Manufacturing | manufacturing |

## Design Rules Quick Reference

| Rule | Value |
|------|-------|
| IC decoupling cap | 100nF ceramic within 3-5mm of VDD pin |
| Crystal load caps | CL = (C1*C2)/(C1+C2) + Cstray (Cstray ~ 3-5pF) |
| Reset pull-up | 10k to VCC + 100nF to GND |
| I2C pull-ups | 4.7k (standard), 2.2k (fast), 1k (fast+) — one set per bus |
| LED resistor | R = (VCC - Vf) / If |

## Common Library IDs

| Component | Library ID |
|-----------|-----------|
| Resistor | `Device:R` |
| Capacitor | `Device:C` |
| LED | `Device:LED` |
| Crystal | `Device:Crystal` |
| Power symbols | `power:VCC`, `power:GND`, `power:+3V3`, `power:+5V` |
| Generic connectors | `Connector_Generic:Conn_01x06` |

## Tool Usage Pattern

```
1. list_toolboxes                          → discover what's available
2. load_toolset("sch_components")          → activate component tools
3. add_schematic_component (repeat)        → place parts
4. load_toolset("sch_wiring")              → activate wiring tools
5. connect_pins / add_wire / add_schematic_net_label → wire the circuit
6. load_toolset(["sch_export", "design_review"]) → activate checks
7. run_erc / run_design_review             → validate the design
```

## Refusing Direct Edits

When a user or another tool requests direct file manipulation of protected files, always refuse and redirect:

1. Acknowledge what they want to accomplish
2. Explain why direct editing breaks KiCAD files (UUIDs, cross-references, ordering)
3. Offer the equivalent MCP tool approach
4. Execute through MCP if they agree
