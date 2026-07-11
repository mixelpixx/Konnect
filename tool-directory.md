# Konnect — Tool Directory

Canonical reference for every MCP tool exposed by Konnect. Generated from the Rust source (not from hand-maintained prose), so it reflects what the binary actually serves.

**Source of truth**
- Toolset metadata + declared counts: `crates/konnect-core/src/router/registry.rs` (`ALL_TOOLSETS`)
- Meta-tool definitions: `crates/konnect-core/src/router/meta_tools.rs` (`meta_tool_descriptions()`)
- Per-tool names + descriptions: `crates/konnect-core/src/tools/<toolset>.rs` (each `tool!(…)` in the `tools()` vec)

## Overview

- **18 toolsets** organized into 10 categories
- **179 registered tools** + **6 always-visible meta-tools** = **185 total**
- **Discovery pattern**: the server pre-loads only the **starter kit** (`project`, `config`) so baseline `tools/list` costs ~2K tokens instead of ~23K. The LLM reads `list_toolboxes` → calls `load_toolset(name)` to expose additional tools on demand; `unload_toolset(name)` prunes them. `tools/list_changed` is notified on every mutation. If the LLM calls a tool whose toolset isn't loaded, the error names the owning toolset so recovery is a single `load_toolset` hop.
- **Observability**: every `tools/call` is recorded — ring buffer of the last 100 calls + per-tool counters + JSONL at `<konnect dir>/logs/calls.jsonl`. The LLM self-diagnoses via `get_recent_calls` and `server_stats`.

## Meta-tools (always visible)

Six tools, grouped into *discovery/routing* and *observability*.

### Discovery / routing

| Tool | Purpose |
|------|---------|
| `list_toolboxes` | List all 18 toolsets with category, tool count, and whether each is currently loaded. The LLM's starting point. |
| `load_toolset` | Load a toolset by name to expose its tools in `tools/list`. Returns the list of tools added. |
| `unload_toolset` | Unload a toolset to prune its tools from `tools/list`. Use when switching tasks to keep context small. |
| `get_active_toolsets` | Return the currently loaded toolsets and how many tools each provides. |

### Observability

| Tool | Purpose |
|------|---------|
| `get_recent_calls` | Last N tool calls (newest first) — `call_id`, tool, toolset, duration, status (ok/error/not_found), `error_kind`. The LLM's debug log. Default limit 20, max 100. |
| `server_stats` | Uptime, total/error call counts, per-tool totals + errors, and the JSONL log path. |

---

## Project

### `project` · 6 tools
**Purpose:** Create, open, save, snapshot KiCAD projects, and launch the live schematic viewer.
**Source:** [`crates/konnect-core/src/tools/project.rs`](crates/konnect-core/src/tools/project.rs)

| Tool | Description |
|------|-------------|
| `create_project` | Create a new KiCAD project at the given path. Creates the directory, a blank `.kicad_pro`, empty `.kicad_sch`, and blank `.kicad_pcb`. |
| `open_project` | Check whether a KiCAD project is currently open in the running KiCAD UI. Returns the active project path and whether KiCAD IPC is available. |
| `save_project` | Save the currently open PCB board file via KiCAD IPC. Requires KiCAD to be running with IPC enabled. |
| `get_project_info` | Read project metadata from a `.kicad_pro` file. Returns name, schematic/PCB paths, last-modified times. |
| `snapshot_project` | Export the schematic and PCB to PDF as a timestamped snapshot/checkpoint. Useful before major edits. |
| `open_schematic_viewer` | Launch the live schematic viewer (SVG with auto-refresh on file change). Use after placing components so the user can see changes in real time. |

---

## Schematic

### `sch_components` · 17 tools
**Purpose:** Add, edit, move, rotate, and delete schematic symbols.
**Source:** [`crates/konnect-core/src/tools/sch_components.rs`](crates/konnect-core/src/tools/sch_components.rs)

| Tool | Description |
|------|-------------|
| `create_schematic` | Create a new blank `.kicad_sch` schematic file. |
| `add_schematic_component` | Add a symbol from a KiCAD library to the schematic. Snaps to the 1.27mm grid. |
| `delete_schematic_component` | Remove a symbol instance from the schematic by its reference designator. |
| `edit_schematic_component` | Update fields (Reference, Value, Footprint, custom properties) of a symbol instance. |
| `get_schematic_component` | Get all properties, position, and pin locations for a symbol instance. |
| `list_schematic_components` | List all symbol instances with positions, values, footprints, and pin locations. |
| `move_schematic_component` | Move a symbol to a new position. Does NOT adjust connected wires. |
| `rotate_schematic_component` | Rotate a symbol by setting its absolute rotation angle (0/90/180/270). |
| `move_connected` | Move a symbol and stretch/shrink connected wire stubs to preserve connections. |
| `move_region` | Move all symbols within a bounding box by a given offset. |
| `annotate_schematic` | Run kicad-cli to auto-assign reference designators (`R?` → `R1`, `U?` → `U1`, etc.). |
| `get_schematic_pin_locations` | Get exact (X,Y) coordinates of every pin on a symbol, accounting for rotation/mirroring. |
| `batch_get_schematic_pin_locations` | Get pin locations for multiple components in a single file read. |
| `add_component_annotation` | Add a custom property (annotation) to a symbol instance. |
| `group_components` | Add a group property to multiple components in the schematic. |
| `replace_component` | Replace a component's `lib_id` with a new library symbol (swap the component type). |
| `get_schematic_view` | Render the schematic to a PNG image (base64-encoded) via kicad-cli. |

### `sch_wiring` · 19 tools
**Purpose:** Wires, net labels, power symbols, junctions, no-connects, pin-to-pin connections.
**Source:** [`crates/konnect-core/src/tools/sch_wiring.rs`](crates/konnect-core/src/tools/sch_wiring.rs)

| Tool | Description |
|------|-------------|
| `add_wire` | Add a wire segment (H or V) between two points. T-junctions are auto-detected and junction dots inserted. |
| `batch_add_wire` | Add multiple wire segments in a single file read/write cycle. |
| `delete_schematic_wire` | Delete a wire segment by UUID or by matching start/end coordinates. |
| `batch_delete_schematic_wire` | Delete multiple wire segments in a single file read/write cycle. |
| `split_wire_at_point` | Split a wire at a given point, creating two segments and a junction. |
| `add_schematic_net_label` | Add a net label (`net_label`, `global_label`, or `hierarchical_label`). |
| `delete_schematic_net_label` | Delete a net label by net name and position. |
| `rotate_schematic_label` | Rotate a net label to a new angle and update its justify direction. |
| `move_labels_by_offset` | Move all labels matching a net name by a given X/Y offset. |
| `batch_rotate_labels` | Rotate multiple labels by net name in a single file read/write cycle. |
| `add_power_symbol` | Add a power symbol (VCC, GND, etc.). Auto-numbers the internal `#PWR` reference. |
| `add_no_connect` | Add a no-connect flag (X marker) to an unconnected pin endpoint. |
| `delete_no_connect` | Remove a no-connect flag at a given position. |
| `batch_delete_no_connect` | Delete multiple no-connect flags in a single file read/write cycle. |
| `add_junction` | Add a junction dot at a point where wires cross or T-intersect. |
| `batch_add_junction` | Add multiple junction dots in a single file read/write cycle. |
| `connect_to_net` | Connect a pin endpoint to a named net by adding a short wire stub + net label. |
| `connect_pins` | Connect two component pins by reference+pin number. Looks up pin coordinates and routes a wire. |
| `add_schematic_connection` | Connect two schematic points directly with a wire (auto H+V routing). Use `connect_pins` if you have references instead of coordinates. |

### `sch_analysis` · 15 tools
**Purpose:** Net connectivity, pin queries, trace paths, overlap/orphan detection.
**Source:** [`crates/konnect-core/src/tools/sch_analysis.rs`](crates/konnect-core/src/tools/sch_analysis.rs)

| Tool | Description |
|------|-------------|
| `list_schematic_wires` | List all wire segments with start/end coordinates and UUIDs. |
| `list_schematic_nets` | List all distinct net names from net labels, global labels, and power symbols. |
| `list_schematic_labels` | List all label instances (net/global/hierarchical) with positions, names, and types. |
| `get_net_connections` | Get all pins and labels connected to a named net. |
| `get_net_connectivity` | Build the full connectivity graph for a net using union-find. Returns wires, labels, and T-junction locations. |
| `get_pin_connections` | Get the net connected to a specific pin by tracing wires from the pin endpoint. |
| `get_pin_net_name` | Return just the net name for a specific pin on a component. |
| `get_component_nets` | Get all nets connected to every pin of a component. |
| `get_net_components` | Get all components (and their pins) connected to a named net. |
| `trace_from_point` | Trace connectivity from any (X,Y) point — returns what is at that point and the net it belongs to. |
| `find_orphan_items` | Find dangling wire ends, floating labels, and unconnected pin endpoints (0.05mm tolerance). |
| `find_shorted_nets` | Detect accidentally merged nets — pairs of distinct net names sharing a wire path. |
| `find_single_pin_nets` | Find nets with only one label/connection — often indicates a missing counterpart. |
| `get_connected_items` | Get all wires, labels, and components connected to a given component by tracing each of its pins. |
| `check_schematic_overlaps` | Find overlapping symbols or labels that may indicate placement errors. |

### `sch_batch` · 10 tools
**Purpose:** Bulk add, edit, delete, and move schematic elements in one call.
**Source:** [`crates/konnect-core/src/tools/sch_batch.rs`](crates/konnect-core/src/tools/sch_batch.rs)

| Tool | Description |
|------|-------------|
| `batch_connect_to_net` | Connect many pins to a named net by adding labels at each endpoint. Single read → all labels inserted → single write. |
| `batch_delete` | Delete multiple schematic items (wires, labels, junctions, components) by UUID or reference — single file write. |
| `bulk_move_schematic_components` | Move multiple components by a uniform dx/dy offset in a single atomic write. |
| `batch_edit_schematic_components` | Apply field updates (Value, Footprint, custom properties) to multiple components in a single atomic write. |
| `batch_delete_schematic_components` | Delete multiple components by reference designator in a single atomic write. |
| `connect_passthrough` | Add a wire stub and matching net label at a point to route a signal through a region without drawing a full path. |
| `add_schematic_text` | Add a text annotation (non-net label) to the schematic at a given position. |
| `get_schematic_layout` | Return a compact spatial summary of the schematic: component positions, bounding box, optionally wires and labels. |
| `validate_wire_connections` | Check all wire endpoints for floating ends not connected to a pin, label, or another wire. |
| `validate_component_connections` | Check that every non-passive pin has at least one wire or label connected. Reports unconnected pins. |

### `sch_export` · 6 tools
**Purpose:** Export schematic to SVG/PDF/netlist, run ERC.
**Source:** [`crates/konnect-core/src/tools/sch_export.rs`](crates/konnect-core/src/tools/sch_export.rs)

| Tool | Description |
|------|-------------|
| `export_schematic_svg` | Export a schematic sheet to an SVG file using kicad-cli. |
| `export_schematic_pdf` | Export a schematic sheet to a PDF file using kicad-cli. |
| `generate_netlist` | Generate a KiCAD netlist file from the schematic using kicad-cli. |
| `export_netlist_summary` | Return a human-readable JSON netlist summary (components, nets, pin counts). Does not require kicad-cli. |
| `run_erc` | Run the Electrical Rules Check via kicad-cli and return violations filtered by severity. |
| `fix_connectivity` | Scan for near-miss wire endpoints within `snap_tolerance` of a pin/label and snap them into place. Supports `dry_run`. |

### `sch_hierarchy` · 7 tools
**Purpose:** Hierarchical sheets: add, edit, move, delete, duplicate a sheet, plus recursive hierarchy and page-numbering queries.
**Source:** [`crates/konnect-core/src/tools/sch_hierarchy.rs`](crates/konnect-core/src/tools/sch_hierarchy.rs)

Sheet lifecycle only (PR-A); pin lifecycle (`import_sheet_pins`, `add_sheet_pin`, `edit_sheet_pin`, `delete_sheet_pin`) lands in a follow-up.

| Tool | Description |
|------|-------------|
| `add_hierarchical_sheet` | Insert a hierarchical sheet into a parent schematic, linking it to a child `.kicad_sch` file. Creates the child file if it doesn't exist, or links to an existing one (multi-instance reuse). Patches existing symbols' instance paths if the linked file already has components. |
| `edit_sheet` | Rename, resize, reposition, or repoint (`Sheetfile`) an existing sheet. |
| `move_sheet` | Reposition a sheet on the parent canvas without touching any other field. |
| `delete_sheet` | Remove a sheet reference from the parent schematic. Does not delete the child file. |
| `duplicate_sheet` | Copy an existing sheet and its child file under a new name/file, offset from the source, with an independent internal UUID. |
| `get_sheet_hierarchy` | Recursively walk the sheet tree, returning nested JSON with each sheet's name/file/uuid/position/size/page/pins and its own children. |
| `renumber_sheet_pages` | Walk the whole sheet tree and reassign sequential page numbers in depth-first order, fixing gaps left by delete/duplicate. |

---

## PCB

### `pcb_board` · 10 tools
**Purpose:** Board outline, layers, zones, mounting holes, board text.
**Source:** [`crates/konnect-core/src/tools/pcb_board.rs`](crates/konnect-core/src/tools/pcb_board.rs)

| Tool | Description |
|------|-------------|
| `set_board_size` | Set the PCB board outline to a rectangle on the Edge.Cuts layer. |
| `get_board_info` | Return metadata about the PCB: title, revision, company, layer count, paper size. |
| `get_board_extents` | Return the bounding box of all objects on the board (IPC, falls back to file parse). |
| `get_layer_list` | Return all layers defined in the board with names and types. |
| `add_layer` | Add a new inner copper or technical layer to the board stack. |
| `set_active_layer` | Set the active layer recorded in the board file's setup section. |
| `add_board_outline` | Add a rectangular board outline on the Edge.Cuts layer at specified coordinates. |
| `add_mounting_hole` | Add an NPTH mounting hole footprint at the specified position. |
| `add_board_text` | Add a silkscreen or fabrication text string to the board. |
| `add_zone` | Add a copper fill zone polygon on a specified layer and net. |

### `pcb_components` · 13 tools
**Purpose:** Place, move, rotate, align, and duplicate PCB footprints.
**Source:** [`crates/konnect-core/src/tools/pcb_components.rs`](crates/konnect-core/src/tools/pcb_components.rs)

| Tool | Description |
|------|-------------|
| `place_component` | Place a footprint on the PCB at a given position and layer via KiCAD IPC. |
| `move_component` | Move a placed footprint to a new X/Y position via KiCAD IPC. |
| `rotate_component` | Set the rotation angle of a placed footprint via KiCAD IPC. |
| `delete_component` | Remove a footprint from the board via KiCAD IPC. |
| `edit_component` | Update the value or other properties of a placed footprint via KiCAD IPC. |
| `find_component` | Find a footprint by reference designator and return its position. |
| `get_component_pads` | Return pad positions and net assignments for a footprint. |
| `get_pad_position` | Return the schematic-space position of a specific pad number on a footprint. |
| `get_component_list` | List all footprints on the board with positions, layers, and values. |
| `place_component_array` | Place multiple copies of a footprint in a grid or line array via KiCAD IPC. |
| `align_components` | Align multiple footprints along a common X or Y axis via KiCAD IPC. |
| `duplicate_component` | Duplicate an existing footprint at a new position via KiCAD IPC. |
| `get_board_2d_view` | Render the PCB as a 2D image using kicad-cli; returns base64 PNG. |

### `pcb_routing` · 12 tools
**Purpose:** Traces, vias, copper pours, net classes, differential pairs.
**Source:** [`crates/konnect-core/src/tools/pcb_routing.rs`](crates/konnect-core/src/tools/pcb_routing.rs)

| Tool | Description |
|------|-------------|
| `add_net` | Add a new net entry to the PCB file (S-expression insert, no IPC required). |
| `route_trace` | Route a trace segment between two points on a copper layer via KiCAD IPC. |
| `route_pad_to_pad` | Route a direct trace between two pads of named components (L-bend routing) via IPC. |
| `add_via` | Add a through-hole via at a position and assign it to a net via IPC. |
| `add_copper_pour` | Add a copper fill zone polygon on a layer/net via S-expression insert. |
| `delete_trace` | Delete a trace segment identified by its UUID via KiCAD IPC. |
| `query_traces` | List trace segments on the board, optionally filtered by net and/or layer. |
| `get_nets_list` | Return all nets defined on the PCB via KiCAD IPC. |
| `modify_trace` | Modify a trace segment by deleting and re-adding it with new parameters. |
| `create_netclass` | Add a netclass definition to the board's design rules (S-expression insert). |
| `assign_net_to_class` | Assign a net to an existing netclass in the PCB file (S-expression edit). |
| `route_differential_pair` | Route a differential pair (two parallel traces with a specified gap). |

### `pcb_export` · 13 tools
**Purpose:** Gerber, PDF, SVG, 3D model, BOM, pick-and-place, DRC, DXF/GenCAD/IPC-2581/ODB++.
**Source:** [`crates/konnect-core/src/tools/pcb_export.rs`](crates/konnect-core/src/tools/pcb_export.rs)

| Tool | Description |
|------|-------------|
| `export_gerber` | Export Gerber production files for all copper/mask layers using kicad-cli. |
| `export_pdf` | Export the PCB layout to a PDF file using kicad-cli. |
| `export_svg` | Export the PCB layout to an SVG file using kicad-cli. |
| `export_3d` | Export the PCB as a 3D model (STEP or VRML) using kicad-cli. |
| `export_bom` | Generate a Bill of Materials (BOM) CSV from the schematic's component data. |
| `export_netlist` | Export the PCB netlist in KiCAD or IPC-D-356 format. |
| `export_position_file` | Generate a component placement (pick-and-place) position file for SMT assembly. |
| `export_dxf` | Export the PCB to DXF, one file per layer, using kicad-cli. For mechanical CAD interchange. |
| `export_gencad` | Export the PCB in GenCAD format using kicad-cli. |
| `export_ipc2581` | Export the PCB in IPC-2581 format using kicad-cli — a unified fab/assembly/test data format. |
| `export_odb` | Export the PCB in ODB++ format using kicad-cli — a unified fabrication data format. |
| `refill_zones` | Refill all copper pour zones using kicad-cli (`zone-fill`). |
| `get_drc_violations` | Run the Design Rule Check and return a list of violations. |

---

## Library

### `library` · 14 tools
**Purpose:** Symbol libraries, footprint libraries, search and registration.
**Source:** [`crates/konnect-core/src/tools/library.rs`](crates/konnect-core/src/tools/library.rs)

| Tool | Description |
|------|-------------|
| `create_footprint` | Create a new footprint (`.kicad_mod`) file from a pad layout description. |
| `edit_footprint_pad` | Edit the size, shape, or position of a pad in an existing `.kicad_mod`. |
| `register_footprint_library` | Register a local footprint library directory in the KiCAD global or project library table. |
| `list_footprint_libraries` | List all registered footprint libraries (global and/or project). |
| `create_symbol` | Create a new KiCAD schematic symbol and append it to a `.kicad_sym` library. |
| `delete_symbol` | Delete a symbol definition from a `.kicad_sym` library. |
| `list_symbols_in_library` | List all symbol names defined in a `.kicad_sym` library file. |
| `register_symbol_library` | Register a `.kicad_sym` library file in the KiCAD global or project symbol table. |
| `list_symbol_libraries` | List all registered symbol libraries (global and/or project). |
| `search_symbols` | Search for symbols across all registered libraries by name or keyword. |
| `list_library_footprints` | List all footprints in a specific registered library (`.pretty` directory). |
| `get_footprint_info` | Return detailed information about a footprint: pad layout, courtyard, description. |
| `search_footprints` | Search for footprints across all registered libraries by name or keyword. |
| `get_symbol_info` | Return detailed information about a schematic symbol: pins, properties, description. |

---

## Integration

### `integration` · 9 tools
**Purpose:** JLCPCB parts database, Freerouting autoroute, datasheet URLs.
**Source:** [`crates/konnect-core/src/tools/integration.rs`](crates/konnect-core/src/tools/integration.rs)

| Tool | Description |
|------|-------------|
| `download_jlcpcb_database` | Download or update the local JLCPCB parts database cache (SQLite). |
| `search_jlcpcb_parts` | Search the local JLCPCB database by keyword, value, or category. |
| `get_jlcpcb_part` | Retrieve full details for a single JLCPCB part by LCSC part number. |
| `suggest_jlcpcb_alternatives` | Suggest JLCPCB-stocked alternatives for a given component value and footprint. |
| `get_jlcpcb_database_stats` | Statistics about the local JLCPCB cache: part count, last updated, file size. |
| `enrich_datasheets` | Fetch and cache datasheet URLs for all components in a schematic (LCSC API). |
| `get_datasheet_url` | Retrieve the datasheet URL for a component by MPN or LCSC ID. |
| `autoroute` | Run Freerouting autorouter: export DSN → autoroute → import SES result. |
| `check_freerouting` | Verify that the Freerouting JAR is available and return its version. |

---

## Verification

### `verification` · 8 tools
**Purpose:** ERC, DRC, design rules, KiCAD UI control.
**Source:** [`crates/konnect-core/src/tools/verification.rs`](crates/konnect-core/src/tools/verification.rs)

| Tool | Description |
|------|-------------|
| `run_drc` | Run the Design Rule Check on the PCB and return structured violation results. |
| `set_design_rules` | Set board-level design rules (clearance, trace width, via size) in the PCB file. |
| `get_design_rules` | Return the current design rule constraints defined in the PCB file. |
| `check_kicad_ui` | Check whether the KiCAD GUI application is running and responsive. |
| `launch_kicad_ui` | Launch the KiCAD GUI application and optionally open a project file. |
| `copy_routing_pattern` | Copy a routing pattern (traces and vias) from one region of the board to another. |
| `set_layer_constraints` | Set per-layer design constraints (min trace width, clearance) in board setup. |
| `check_clearance` | Check the physical clearance (distance) between two components on the PCB. |

---

## Configuration

### `config` · 7 tools
**Purpose:** User preferences, project rules, design rules, fab constraints. **Call `load_user_config` at session start.**
**Source:** [`crates/konnect-core/src/tools/config.rs`](crates/konnect-core/src/tools/config.rs)

| Tool | Description |
|------|-------------|
| `load_user_config` | Load the user's global Konnect preferences (manufacturers, fab constraints, default passives, design rules). Call at session start. |
| `save_user_config` | Update a user preference using dot-notation, e.g. `fab_constraints.fab_house`. |
| `load_project_config` | Load project-specific config from `<project_dir>/.konnect/project.json`. Project overrides user. |
| `save_project_config` | Save a project-specific rule or override (same dot-notation as `save_user_config`). |
| `get_effective_config` | Return the merged config (user defaults + project overrides). The config Claude should use for design decisions. |
| `add_design_rule` | Add a natural-language design rule Claude should follow. Examples: "Always use 100nF X7R for MCU decoupling within 3mm of power pin". |
| `list_design_rules` | List all active design rules (user-level + project-level). |

---

## Design Review

### `design_review` · 6 tools
**Purpose:** AI-powered design audits: decoupling, connections, power rails, DFM, BOM health.
**Source:** [`crates/konnect-core/src/tools/design_review.rs`](crates/konnect-core/src/tools/design_review.rs)

| Tool | Description |
|------|-------------|
| `audit_decoupling` | Check that all ICs have appropriate decoupling caps. Finds power pins without nearby caps and flags wrong values. |
| `audit_connections` | Check for common connection mistakes: missing pull-ups on I2C/reset, missing series resistors on LEDs, floating inputs, shorted outputs. |
| `audit_power_rails` | Check power rail integrity: missing bulk capacitance, no test points, missing regulator output caps. |
| `audit_manufacturing` | DFM checks for the configured fab house: component spacing, silkscreen overlap, via-in-pad, acid traps, board-outline issues. |
| `run_design_review` | Run all available audit checks and produce a consolidated report. Call this when the user asks "is my board ready?" |
| `check_bom_health` | Analyze the BOM for supply-chain risks: parts with no MPN, lifecycle warnings, low stock, unavailable from preferred distributors. |

---

## Templates

### `templates` · 4 tools
**Purpose:** Reference circuit library — USB-C, LDO, buck converter, STM32, I2C, LED — verified component values.
**Source:** [`crates/konnect-core/src/tools/templates.rs`](crates/konnect-core/src/tools/templates.rs)

| Tool | Description |
|------|-------------|
| `search_templates` | Search the reference circuit template library. Returns matches for common subcircuits; templates have verified component values. |
| `get_template` | Get full details for a template: components, connections, design notes. Use the template ID from `search_templates`. |
| `apply_template` | Instantiate a template into the current schematic. Places all components and wires per the connection map; `net_mappings` re-binds template nets to project nets. |
| `list_template_categories` | List all available template categories and the number of templates in each. |

**Built-in templates** (loaded by `load_all_templates` in `templates.rs`):
`usb_c_5v_sink`, `ldo_3v3`, `stm32_minimal`, `i2c_pullups`, `led_indicator`, `buck_converter`.

---

## Manufacturing

### `manufacturing` · 3 tools
**Purpose:** Design-to-fab pipeline: export Gerber+BOM+positions package, validate for fab house, estimate cost.
**Source:** [`crates/konnect-core/src/tools/manufacturing.rs`](crates/konnect-core/src/tools/manufacturing.rs)

| Tool | Description |
|------|-------------|
| `export_manufacturing_package` | Generate ALL files needed for PCB fab + assembly in one call: Gerbers, drill, fab-house BOM, pick-and-place. Targets JLCPCB, PCBWay, etc. |
| `validate_for_manufacturing` | Pre-flight check before ordering: verifies the design is ready for the target fab house (board outline, design rules, BOM completeness, assembly constraints). |
| `estimate_cost` | Estimate total manufacturing cost (PCB + components + assembly) with itemized breakdown. |

---

## Appendix: Structural observations

### Is the structure intelligent?

**Yes — the split holds up.** A few observations worth tracking as the tool surface grows:

1. **Categories mirror the KiCAD editor boundaries** — Schematic (`sch_*`), PCB (`pcb_*`), plus library/integration/verification/review/templates/manufacturing as cross-cutting concerns. A new tool's home is usually obvious.

2. **Batch tools are split across two places**:
   - `sch_batch` holds top-level batch primitives (`batch_connect_to_net`, `batch_delete`, `bulk_move_schematic_components`, etc.) plus validation
   - `sch_wiring` / `sch_components` also contain `batch_*` tools (`batch_add_wire`, `batch_delete_no_connect`, `batch_rotate_labels`, `batch_get_schematic_pin_locations`, `batch_delete_schematic_components`, `batch_edit_schematic_components`)
   The split is defensible (tight-scope batches live with their domain; cross-domain batches live in `sch_batch`) but worth a one-paragraph convention note in DEV.md so future additions land consistently.

3. **Cross-toolset cleanups** (historical notes):
   - `search_footprints` and `get_symbol_info` were originally in `verification`; moved to `library` where they belong semantically. Users who were loading `verification` for these will be auto-redirected by the smart "tool not loaded" error.
   - `get_drc_violations` (`pcb_export`) and `run_drc` (`verification`) run the same kicad-cli check. Their tool descriptions now cross-reference each other and steer the LLM toward `run_drc` for interactive use (cleaner summary with error/warning counts) and `get_drc_violations` for bundling into a build package.

### Implementation notes

- The `tool!(name, description, input_schema, handler)` macro lives in `crates/konnect-core/src/tools/mod.rs` and produces a `ToolDef` inserted into each toolset's `tools()` vec.
- Dispatch: `router::registry::tools_for(name)` maps each toolset string to its `tools::<mod>::tools()` vec; `handler.rs` looks up `tools/call` in the currently-loaded toolsets. If the tool exists but its toolset isn't loaded, the error names the owning toolset for single-hop recovery (`router::ToolRouter::find_toolset_for_tool`).
- Some schematic handlers are mid-migration from raw `konnect-sexp` to the typed `konnect-schematic-editor` model (Phase 2 Waves 1–4, see commits `1314ed2`…`f92b3b1`). Tool names and semantics are unchanged; only the internal implementation is in flux.

### Regenerating this doc

The tool list is extracted mechanically from the `tool!(...)` invocations in `crates/konnect-core/src/tools/*.rs`. To regenerate after adding tools, re-run the same extraction and re-verify counts against `registry.rs::ALL_TOOLSETS` — the row count in each table here must equal each toolset's `tool_count`.
