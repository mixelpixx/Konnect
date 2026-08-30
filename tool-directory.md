# Konnect — Tool Directory

Canonical reference for every MCP tool exposed by Konnect. Generated from the Rust source (not from hand-maintained prose), so it reflects what the binary actually serves.

**Source of truth**
- Toolset metadata + declared counts: `crates/konnect-core/src/router/registry.rs` (`ALL_TOOLSETS`)
- Meta-tool definitions: `crates/konnect-core/src/router/meta_tools.rs` (`meta_tool_descriptions()`)
- Per-tool names + descriptions: `crates/konnect-core/src/tools/<toolset>.rs` (each `tool!(…)` in the `tools()` vec)

Compatibility notes for removed or narrowed arguments are recorded in
[`docs/API_MIGRATIONS.md`](docs/API_MIGRATIONS.md).

## Overview

- **20 toolsets** organized into 10 categories
- **221 registered tools** + **7 always-visible meta-tools** = **228 total**
- **Discovery pattern**: the server pre-loads only the **starter kit** (`project`, `config`) so baseline `tools/list` costs ~2K tokens instead of ~23K. The LLM reads `list_toolboxes` → calls `load_toolset(name)` to expose additional tools on demand; `unload_toolset(name)` prunes them. `tools/list_changed` is notified on every mutation. If the LLM calls a tool whose toolset isn't loaded, the error names the owning toolset so recovery is a single `load_toolset` hop. `load_toolset` also accepts an array of names to load several toolsets with a single `tools/list` refresh.
- **Observability**: every `tools/call` is recorded — ring buffer of the last 100 calls + per-tool counters + JSONL at `<konnect dir>/logs/calls.jsonl`. The LLM self-diagnoses via `get_recent_calls` and `server_stats`.

## Meta-tools (always visible)

Seven tools, grouped into *discovery/routing*, *observability*, and *runtime diagnostics*.

### Discovery / routing

| Tool | Purpose |
|------|---------|
| `list_toolboxes` | List all 20 toolsets with category, tool count, and whether each is currently loaded. The LLM's starting point. |
| `load_toolset` | Load a toolset by name to expose its tools in `tools/list`. Returns the list of tools added. |
| `unload_toolset` | Unload a toolset to prune its tools from `tools/list`. Use when switching tasks to keep context small. |
| `get_active_toolsets` | Return the currently loaded toolsets and how many tools each provides. |

### Observability

| Tool | Purpose |
|------|---------|
| `get_recent_calls` | Last N tool calls (newest first) — `call_id`, tool, toolset, duration, status (ok/error/not_found), `error_kind`. The LLM's debug log. Default limit 20, max 100. |
| `server_stats` | Uptime, total/error call counts, per-tool totals + errors, and the JSONL log path. |

### Runtime diagnostics

| Tool | Purpose |
|------|---------|
| `get_installation_info` | Report the serving build version and commit, executable path, verified install source, on-disk binary version, KiCad CLI version, redacted IPC endpoint, proven stale-process evidence, and platform-specific restart guidance. |

---

## Project

### `project` · 7 tools
**Purpose:** Create, open, save, rename, snapshot KiCAD projects, and launch the live schematic viewer.
**Source:** [`crates/konnect-core/src/tools/project.rs`](crates/konnect-core/src/tools/project.rs)

| Tool | Description |
|------|-------------|
| `create_project` | Create a new KiCAD project at the given path. Creates the directory, a blank `.kicad_pro`, empty `.kicad_sch`, and blank `.kicad_pcb`; refuses to replace any existing project file. |
| `open_project` | List PCB documents open in the running KiCad UI and optionally check a specific `.kicad_pro` or `.kicad_pcb` path over IPC. |
| `save_project` | Save the currently open PCB board file via KiCAD IPC. Requires KiCAD to be running with IPC enabled. |
| `get_project_info` | Read project metadata from a `.kicad_pro` file. Returns name, schematic/PCB paths, last-modified time, project-file format version, and the generator versions recorded by the sibling design files. |
| `rename_project` | Rename the `.kicad_pro`/`.kicad_sch`/`.kicad_pcb`/`.kicad_prl` files *and* the internal references that carry the old name. Renaming the files alone makes KiCad treat the design as unannotated, losing every reference designator, because each symbol instance stores `(project "name")`. Supports `dry_run`. |
| `snapshot_project` | Export the schematic and PCB to PDF as a timestamped snapshot/checkpoint. Useful before major edits. |
| `open_schematic_viewer` | Launch the live schematic viewer (SVG with auto-refresh on file change). Use after placing components so the user can see changes in real time. |

---

## Schematic

### `sch_components` · 20 tools
**Purpose:** Add, edit, move, rotate, and delete schematic symbols, and set the page size.
**Source:** [`crates/konnect-core/src/tools/sch_components.rs`](crates/konnect-core/src/tools/sch_components.rs)

| Tool | Description |
|------|-------------|
| `create_schematic` | Create a new blank `.kicad_sch` schematic file, on A4 unless another paper size is given. Use `set_schematic_page` to change it later. |
| `set_schematic_page` | Set the sheet's paper size (A0–A5, A–E, US Letter/Legal/Ledger) and orientation. Returns the size in mm — content outside the frame still exports and still nets up, so a too-small page is a silent defect. |
| `add_schematic_component` | Add a symbol from a KiCAD library to the schematic. Snaps to the 1.27mm grid, preserves every saved hierarchy instance, and reports committed-file readback. Refuses stale instance metadata before writing. |
| `delete_schematic_component` | Remove a component and all of its placed units by reference designator. |
| `edit_schematic_component` | Update shared fields consistently across every placed unit of a component. |
| `get_schematic_component` | Get shared properties and every placed unit's position for a component. |
| `list_schematic_components` | List all symbol instances with positions, values, footprints, and pin locations. |
| `move_schematic_component` | Move the lowest-numbered unit to a new position and translate every other unit by the same delta. Does NOT adjust connected wires. |
| `rotate_schematic_component` | Set the lowest-numbered unit's absolute rotation and rotate every other unit by the same delta. |
| `move_connected` | Move a symbol and stretch/shrink connected wire stubs to preserve connections. |
| `move_region` | Move all symbols within a bounding box by a given offset. |
| `annotate_schematic` | Run kicad-cli to auto-assign reference designators (`R?` → `R1`, `U?` → `U1`, etc.). |
| `get_schematic_pin_locations` | Get exact (X,Y) coordinates of every pin on every placed unit, accounting for rotation/mirroring, plus each pin's `orientation_degrees` and `length_mm`. |
| `batch_get_schematic_pin_locations` | Get pin locations for multiple components in a single file read, with the same per-pin fields. |
| `add_component_annotation` | Add or update a custom property across every placed unit of a component. |
| `group_components` | Add or update a group property across every placed unit of multiple components. |
| `replace_component` | Replace every placed unit's `lib_id` while preserving and validating its unit number. |
| `update_symbols_from_library` | Re-embed placed symbols' definitions from their libraries, like KiCad's "Update Symbols from Library". Refuses a symbol whose pins moved or disappeared (wires attach at pin coordinates) unless `allow_pin_moves` is set. |
| `reset_schematic_field_positions` | Move each symbol's Reference and Value text back to its library anchor, through the symbol's rotation — KiCad's "Reset field text positions". Repairs sheets whose fields sit at a uniform offset. |
| `get_schematic_view` | Render a sheet with kicad-cli and return the path to the SVG it wrote. There is no PNG — KiCad has no schematic rasteriser. The file lands in a temp directory; use `export_schematic_svg` to choose the location. |

### `sch_wiring` · 20 tools
**Purpose:** Wires, net labels, power symbols, junctions, no-connects, pin-to-pin connections.
**Source:** [`crates/konnect-core/src/tools/sch_wiring.rs`](crates/konnect-core/src/tools/sch_wiring.rs)

| Tool | Description |
|------|-------------|
| `add_wire` | Add a wire segment (H or V) between two points. T-junctions are auto-detected and junction dots inserted. |
| `batch_add_wire` | Add multiple wire segments in a single file read/write cycle. |
| `delete_schematic_wire` | Delete a wire segment by UUID or by matching start/end coordinates, pruning the junction dots it leaves unjustified. |
| `batch_delete_schematic_wire` | Delete multiple wire segments in a single file read/write cycle, pruning the junction dots they leave unjustified. |
| `split_wire_at_point` | Split a wire at a given point, creating two segments and a junction. |
| `add_schematic_net_label` | Add a net label (`net_label`, `global_label`, or `hierarchical_label`). |
| `delete_schematic_net_label` | Delete a net label by net name and position. |
| `rotate_schematic_label` | Rotate a net label to a new angle and update its justify direction. |
| `move_labels_by_offset` | Move all labels matching a net name by a given X/Y offset. |
| `batch_rotate_labels` | Rotate multiple labels by net name in a single file read/write cycle. |
| `add_power_symbol` | Add a power symbol (VCC, GND, etc.). Auto-numbers the internal `#PWR` reference to the lowest number free on the sheet. Preserves every saved hierarchy instance and reports committed-file readback; refuses stale instance metadata before writing. |
| `add_no_connect` | Add a no-connect flag (X marker) to an unconnected pin endpoint. |
| `delete_no_connect` | Remove a no-connect flag at a given position. |
| `batch_delete_no_connect` | Delete multiple no-connect flags in a single file read/write cycle. |
| `batch_add_no_connect` | Add multiple no-connect flags in one write. Marking one MCU's unused pins is routinely 15–20 flags. |
| `add_junction` | Add a junction dot at a point where wires cross or T-intersect. |
| `batch_add_junction` | Add multiple junction dots in a single file read/write cycle. |
| `connect_to_net` | Connect a pin to a named net by adding a short wire stub + net label. Name the pin (reference + pin_number) or give its coordinates; the stub direction defaults to `auto`, pointing away from the symbol body. |
| `connect_pins` | Connect two component pins by reference+pin number. Looks up pin coordinates and routes a wire. |
| `add_schematic_connection` | Connect two schematic points directly with a wire (auto H+V routing). Use `connect_pins` if you have references instead of coordinates. |

### `sch_bus` · 4 tools
**Purpose:** Buses, bus entries, and fanning a group of pins out onto a bus.
**Source:** [`crates/konnect-core/src/tools/sch_bus.rs`](crates/konnect-core/src/tools/sch_bus.rs)

| Tool | Description |
|---|---|
| `add_bus` | Add a bus segment. Geometrically a wire; KiCad treats it as a bus, carrying the members named by the bus label (`NAME[1..6]` or `{A B C}`). Wires join it only through a bus entry. |
| `batch_add_bus` | Add multiple bus segments in one file read/write cycle. |
| `add_bus_entry` | Add the 45° tick that connects a wire to a bus. Required — a wire and bus that merely touch are *not* connected. `x`/`y` are the wire-side end; `direction` picks which corner the tick runs to (`down_right` default, `down_left`, `up_right`, `up_left`). |
| `connect_pins_to_bus` | Fan a set of pins onto a bus: wire stub + bus entry + member label per pin. Bus membership is by name, so the label is part of the connection, not decoration. |

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
| `find_orphan_items` | Find dangling wire ends, floating labels, and unconnected pin endpoints. Pins, sheet pins, junctions, and no-connect flags all count as connections. |
| `find_shorted_nets` | Detect accidentally merged nets — pairs of distinct net names sharing a wire path. |
| `find_single_pin_nets` | Find nets that reach at most one pin — often a missing counterpart, an orphan label, or a stub left by a deleted component. Component pins and hierarchical sheet pins count; a power symbol's own pin names the rail rather than consuming it and does not. Reports the pin and label counts, and every label kind that named the net. Read per sheet: a net a global, hierarchical or power label can carry off this one is flagged cross_sheet_unverified. |
| `get_connected_items` | Get all wires, labels, and components connected to a given component by tracing each of its pins. |
| `check_schematic_overlaps` | Find collisions using transformed symbol drawings and pins (excluding free text), with a reported origin fallback when geometry is unavailable. |

### `sch_batch` · 12 tools
**Purpose:** Bulk add, edit, delete, and move schematic elements in one call.
**Source:** [`crates/konnect-core/src/tools/sch_batch.rs`](crates/konnect-core/src/tools/sch_batch.rs)

| Tool | Description |
|------|-------------|
| `batch_connect_to_net` | Connect many pins to a named net by adding labels at each endpoint, oriented away from the symbol body. Single read → all labels inserted → single write. |
| `batch_delete` | Delete multiple schematic items (wires, labels, junctions, components) by UUID or reference — single file write. |
| `bulk_move_schematic_components` | Move multiple components by a uniform dx/dy offset in a single atomic write. |
| `batch_edit_schematic_components` | Apply field updates (Value, Footprint, custom properties) to multiple components in a single atomic write. |
| `batch_delete_schematic_components` | Delete multiple components by reference designator in a single atomic write. |
| `connect_passthrough` | Add a wire stub and matching net label at a point to route a signal through a region without drawing a full path. Direction defaults to `auto`. |
| `add_schematic_text` | Add a text annotation (non-net label) to the schematic at a given position. Aligns the text against that position with `justify`, per axis and defaulting to `left bottom` as KiCad does; an omitted axis is centred, and `center` centres both. Takes `bold`, `italic`, `thickness` and `color` for the font. |
| `get_schematic_layout` | Return component positions and transformed drawing/pin bounds (excluding free text), reporting unresolved geometry; optionally include wires and labels. |
| `validate_wire_connections` | Check all wire endpoints for floating ends not connected to a pin, label, or another wire. |
| `validate_component_connections` | Check that every non-passive pin has at least one wire or label connected. Reports unconnected pins. |
| `batch_place_components` | Place multiple symbols from KiCAD libraries in one write with committed-file readback. Preserves every saved hierarchy instance and preflights stale metadata before any placement. Pass explicit references -- there is no auto-numbering; an omitted reference becomes '?' like an eeschema-unannotated symbol, same as `add_schematic_component`. |
| `batch_connect_pins` | Connect multiple component pin pairs by reference and pin number, in a single file read/write cycle. |

### `sch_export` · 10 tools
**Purpose:** Export schematic to SVG/PDF/PNG/netlist, run ERC, and synchronize a live PCB.
**Source:** [`crates/konnect-core/src/tools/sch_export.rs`](crates/konnect-core/src/tools/sch_export.rs)

| Tool | Description |
|------|-------------|
| `export_schematic_svg` | Export a schematic sheet to SVG using kicad-cli, with optional monochrome rendering and colour theme. |
| `render_schematic_png` | Render a sheet to PNG: kicad-cli SVG rasterized in-process with deterministic stroke-font rendering. Returns the path and actual pixel dimensions; `inline` adds base64 content so the caller can inspect its own output. |
| `set_visual_baseline` | Capture the current render of a sheet as its visual baseline under the project's `.konnect/baselines/`, recording the source hash and renderer identity. |
| `compare_visual_baseline` | Re-render at the baseline's width and report pixel drift vs a 2% threshold, with the changed region's bounding box; "no baseline stored" is an explicit result, and a stale-renderer baseline is flagged, never silently trusted. |
| `export_schematic_pdf` | Export a schematic to PDF using kicad-cli, optionally monochrome or limited to the root sheet. |
| `generate_netlist` | Generate a KiCAD netlist file from the schematic using kicad-cli. |
| `export_netlist_summary` | Return a human-readable JSON netlist summary (components, nets, pin counts). Nets come from labels and power symbols. Does not require kicad-cli. |
| `run_erc` | Run the Electrical Rules Check via kicad-cli and return violations filtered by severity. |
| `fix_connectivity` | Scan for near-miss wire endpoints within `snap_tolerance` of a pin/label and snap them into place. Supports `dry_run`. |
| `update_pcb_from_schematic` | Plan or atomically apply saved schematic hierarchy changes to the live KiCad PCB. Defaults to a non-mutating dry run; apply requires its exact plan revision. Preserves placement, routing, board-only footprints, and footprint artwork. |

### `sch_hierarchy` · 12 tools
**Purpose:** Hierarchical sheets: add/edit/move/delete/duplicate a sheet, hierarchy and page-numbering queries, import/add/edit/delete sheet pins, pin/label sync validation.
**Source:** [`crates/konnect-core/src/tools/sch_hierarchy.rs`](crates/konnect-core/src/tools/sch_hierarchy.rs)

| Tool | Description |
|------|-------------|
| `add_hierarchical_sheet` | Insert a hierarchical sheet into a parent schematic, linking it to a child `.kicad_sch` file. Creates the child file if it doesn't exist, or links to an existing one (multi-instance reuse). Patches existing symbols' instance paths if the linked file already has components. |
| `edit_sheet` | Rename, resize, reposition, or repoint (`Sheetfile`) an existing sheet. |
| `move_sheet` | Reposition a sheet on the parent canvas without touching any other field. |
| `delete_sheet` | Remove a sheet reference from the parent schematic. Does not delete the child file. |
| `duplicate_sheet` | Copy an existing sheet and its child file under a new name/file, offset from the source, with an independent internal UUID. |
| `get_sheet_hierarchy` | Recursively walk the sheet tree, returning nested JSON with each sheet's name/file/uuid/position/size/page/pins and its own children. |
| `renumber_sheet_pages` | Walk the whole sheet tree and reassign sequential page numbers in depth-first order, fixing gaps left by delete/duplicate. |
| `import_sheet_pins` | Scan the child sheet's hierarchical_labels and auto-generate matching pins on the parent sheet block, skipping names that already have a pin — the primary way pins get created. |
| `add_sheet_pin` | Manually add a single pin to an existing sheet block. |
| `edit_sheet_pin` | Rename a pin, change its electrical type, or reposition it along the sheet border. |
| `delete_sheet_pin` | Remove a single pin without touching the rest of the sheet. |
| `validate_sheet_pins` | Read-only. Walk the sheet tree and report hierarchical_labels with no matching parent pin, and pins with no matching child label. |

---

## PCB

### `pcb_board` · 12 tools
**Purpose:** Board outline, layers, zones, mounting holes, board text, SVG logo import.
**Source:** [`crates/konnect-core/src/tools/pcb_board.rs`](crates/konnect-core/src/tools/pcb_board.rs)

| Tool | Description |
|------|-------------|
| `set_board_size` | Add a rectangular board outline of the given dimensions on the Edge.Cuts layer. Appends — clear the old edges with `delete_graphics` first. |
| `get_board_info` | Return metadata about the PCB: title, revision, company, paper size (with `paper_size_mm` dimensions on a custom User size), `layer_count`, `copper_layer_count`, and `net_count` (IPC, falls back to a file parse that counts from the tree, so KiCad 10 boards report real numbers instead of 0). |
| `get_board_extents` | Return the bounding box of all objects on the board (IPC, falls back to file parse). |
| `get_layer_list` | Return all layers defined in the board: `id`, `name`, `type`, plus the optional `user_name` label and a `copper` flag. |
| `add_layer` | Add a new inner copper or technical layer to the board stack. Rejects a non-canonical layer name — KiCad refuses to open a board containing one. Use the canonical name and pass your own label as its user name. |
| `set_active_layer` | Set the active layer recorded in the board file's setup section. |
| `add_board_outline` | Add a rectangular Edge.Cuts outline with sharp or circular rounded corners, identically over IPC and file fallback. Appends — clear the old edges with `delete_graphics` first. |
| `delete_graphics` | Delete board graphics (lines, rects, arcs, circles, polys, curves, text, textboxes, dimensions) matching a UUID/layer/type filter; `dry_run` lists them instead. |
| `add_mounting_hole` | Add an NPTH mounting hole footprint at the specified position. |
| `add_board_text` | Add a silkscreen or fabrication text string to the board. |
| `add_zone` | Add a copper fill zone polygon on a specified layer and net, with optional `name`, `priority` and `pad_connection` (`solid`/`thermal`/`none`). Tries KiCad IPC first — a live board gets the zone through the API and a refill, so it appears immediately and is undoable — and falls back to an S-expression file insert only when no live KiCad answers, reporting `source` and a `warning` when it does. Refuses a net the board does not declare rather than binding copper to net 0, and refuses outright if KiCad answers but rejects the request. |
| `import_svg_logo` | Import an SVG file as filled silkscreen/copper artwork (curves flattened to polygons). |

### `pcb_components` · 19 tools
**Purpose:** Place, refresh, move, rotate, flip, align, duplicate and repair PCB footprints; inspect pads; inspect and edit a placed footprint's graphics.
**Source:** [`crates/konnect-core/src/tools/pcb_components.rs`](crates/konnect-core/src/tools/pcb_components.rs)

| Tool | Description |
|------|-------------|
| `place_component` | Place a footprint through live KiCAD IPC when reachable, or use a revision-aware file fallback when no KiCAD process can hold the board open. The fallback preserves complete footprint content and rejects duplicate references. |
| `move_component` | Move a placed footprint through live KiCAD IPC when reachable, or use a revision-aware closed-board file fallback. |
| `rotate_component` | Set a placed footprint's absolute rotation through live KiCAD IPC when reachable, or use a revision-aware closed-board file fallback that updates child angles. |
| `set_component_placements` | Set X/Y positions and absolute rotations for multiple existing footprints atomically, using one live KiCAD update and one undo step or one revision-aware closed-board write. |
| `flip_component` | Set a placed footprint to F.Cu or B.Cu on a closed board with KiCAD-equivalent geometry mirroring and revision checks; refuses live-editor races and unsupported geometry. |
| `delete_component` | Remove a footprint from the board via KiCAD IPC. |
| `edit_component` | Update the value or other properties of a placed footprint via KiCAD IPC. |
| `repair_corrupted_footprints` | Dry-run and atomically repair the exact legacy corruption from issue #244: anonymous layerless pads that replaced footprint drawing shapes. Restores the affected shapes from the registered library while preserving live placement, identity, pad nets and non-shape children; apply requires the dry-run revision and is one KiCAD undo commit. |
| `find_component` | Find a footprint by reference designator and return its position. |
| `update_footprints_from_library` | Plan or atomically apply KiCad's Update Footprints from Library operation to placed footprints on the live board. Defaults to a non-mutating dry run; apply requires its exact plan revision. Preserves placed-instance state and pad nets while refreshing supported library-owned content. |
| `list_board_footprint_graphics` | List the graphic items inside a footprint placed on the board — silkscreen, fabrication, and courtyard artwork — with the UUID needed to edit one. Reports `editable`, plus `outlines` and `holes` for polygons. Requires KiCAD running with the board open. |
| `edit_board_footprint_graphic` | Replace the vertices of a single-outline polygon inside a placed footprint, selected by UUID, without re-placing the part. Anything with multiple outlines or holes is refused by name rather than flattened. Requires KiCAD running with the board open. |
| `get_component_pads` | Return live board-space pad positions, layers, and net assignments when KiCAD IPC is reachable; fall back to the saved board only when IPC is unreachable. A pad whose saved net node is present but unreadable reports `null` rather than an empty string. |
| `get_pad_position` | Return the live board-space position, layers, and net assignment of a specific pad number. |
| `get_component_list` | List all footprints on the board with positions, layers, and values. |
| `place_component_array` | Place multiple copies of a footprint in a grid or line array via KiCAD IPC. |
| `align_components` | Align multiple footprints along a common X or Y axis via KiCAD IPC. |
| `duplicate_component` | Duplicate an existing footprint at a new position via KiCAD IPC. |
| `get_board_2d_view` | Render the board with kicad-cli and return a base64 PNG. This is the 3-D render viewed from the top, not a layer plot, and takes no layer selection — use `export_svg` for layer-aware output. |

### `pcb_routing` · 15 tools
**Purpose:** Traces, vias, copper pours, net classes, differential pairs, and strict Specctra SES import.
**Source:** [`crates/konnect-core/src/tools/pcb_routing.rs`](crates/konnect-core/src/tools/pcb_routing.rs)

| Tool | Description |
|------|-------------|
| `add_net` | Add a new net entry to the PCB file (S-expression insert, no IPC required). Pre-KiCad-10 boards only: KiCad 10 has no top-level net table, so this fails closed there and points at `route_trace` / `add_via` / `add_copper_pour`, which create a net by naming it on copper. |
| `route_trace` | Route a trace segment between two points on a copper layer via KiCAD IPC. |
| `route_pad_to_pad` | Route a direct trace between two pads of named components (L-bend routing) via IPC. |
| `add_via` | Add a through-hole via at a position and assign it to a net via IPC. |
| `plan_specctra_ses_import` | Strictly validate a Freerouting SES against its revision-bound manifest and the exact live board, returning every planned route item and the preserved locked-track/via inventory without mutation. |
| `apply_specctra_ses` | Preserve the manifest-bound locked straight tracks and through vias, apply a validated SES through KiCad IPC as one undo transaction, verify post-commit IPC read-back, create a separate candidate board, and report direct KiCad DRC evidence (including whether it is clean). |
| `add_copper_pour` | Alias of `add_zone`, kept for compatibility: same arguments, same defaults, same IPC-first behaviour. (Its `min_width` default was 0.25 and is now 0.2, matching `add_zone` and KiCad.) |
| `delete_trace` | Delete one observed trace segment by UUID via KiCad IPC. Refuses non-trace or stale UUIDs before deletion, targets the requested board, and reports the observed preimage only after readback proves the segment is absent. |
| `query_traces` | List trace segments on the board, optionally filtered by net and/or layer. |
| `get_nets_list` | Return all nets defined on the PCB via KiCAD IPC. |
| `modify_trace` | Modify a trace segment by deleting and re-adding it with new parameters. |
| `create_netclass` | Create or update a netclass in the project's `net_settings` (the sibling `.kicad_pro`, where KiCad keeps netclasses since v7). Never touches the board file. An update changes only the settings named; use `get_netclasses` to look, since naming an unknown class here creates it. |
| `get_netclasses` | Read every netclass with its settings, its `netclass_patterns` and the board nets those patterns match. Reads the `.kicad_pro` and the board file, so KiCad need not be running. Reports `Default` (marked) and any pattern naming a class that does not exist. |
| `assign_net_to_class` | Assign a net to an existing netclass via a `netclass_patterns` entry in the `.kicad_pro`; reassigning moves the entry. |
| `route_differential_pair` | Route a differential pair (two parallel traces with a specified gap). |

### `placement` · 5 tools
**Purpose:** Placement quality and automation — score, plan decoupling rows, plan BGA fanouts; every plan reports its own before/after score.
**Source:** [`crates/konnect-core/src/tools/placement.rs`](crates/konnect-core/src/tools/placement.rs)

| Tool | Description |
|------|-------------|
| `score_placement` | Score the placement 0-100 with named deductions (courtyard overlaps, off-board parts, connector edge distance, decoupling distance). Hard failures decide the verdict regardless of the numeric score; a missing outline blocks a pass rather than passing silently. |
| `place_decoupling_caps` | Plan (dry-run default) or apply a row of decoupling caps beside an IC, paired by shared nets, never reference guessing; the response carries the board score before and after the plan. |
| `plan_bga_fanout` | Plan a BGA fanout with the pitch detected from the pad grid: dogbone or inline vias for inner pads, stub traces, conservative via sizes. Apply executes the whole plan as one KiCad undo commit over live IPC. |
| `auto_place_from_schematic` | Deterministic first placement: net-clustered groups laid out as grids inside the outline, courtyards non-overlapping; explicitly a starting point, with before/after scores in the response. |
| `refine_placement_force_directed` | Deterministic spring embedder: shared nets pull (power 3x, differential pairs 5x), courtyards repel, edges constrain, collisions resolved on a snap grid. Same input, same plan — no randomness, no clocks. Locked references never move. |

---

### `pcb_export` · 14 tools
**Purpose:** Gerber, PDF, SVG, 3D model, BOM, revision-bound Specctra DSN, pick-and-place, DRC, DXF/GenCAD/IPC-2581/ODB++.
**Source:** [`crates/konnect-core/src/tools/pcb_export.rs`](crates/konnect-core/src/tools/pcb_export.rs)

| Tool | Description |
|------|-------------|
| `export_gerber` | Export Gerber production files for all copper/mask layers using kicad-cli. |
| `export_pdf` | Export selected PCB layers to one PDF file using kicad-cli, optionally in black and white. |
| `export_svg` | Export selected PCB layers to one SVG file using kicad-cli, optionally in black and white. |
| `export_3d` | Export the PCB as a 3D model using kicad-cli, with explicit control over unspecified footprint models. |
| `export_bom` | Generate KiCad 10's CSV Bill of Materials from schematic fields. |
| `export_netlist` | Export the PCB netlist in KiCAD or IPC-D-356 format. |
| `export_specctra_dsn` | Export a deterministic, revision-bound Specctra DSN plus reverse manifest from a supported live KiCad board. The Rust exporter is the default. On KiCad 10, explicitly set `native_bridge_mode` to `prefer` or `require` for the optional authenticated ActionPlugin native exporter. Preserves locked straight tracks and through vias as fixed wiring; refuses unlocked routing, arcs, unsupported geometry, or incomplete rules. |
| `export_position_file` | Generate a component placement (pick-and-place) position file for SMT assembly. |
| `export_dxf` | Export the PCB to DXF, one file per requested layer, using kicad-cli. `layers` is required — there is no all-layers default. For mechanical CAD interchange. |
| `export_gencad` | Export the PCB in GenCAD format using kicad-cli. |
| `export_ipc2581` | Export the PCB in IPC-2581 format using kicad-cli — a unified fab/assembly/test data format. |
| `export_odb` | Export the PCB in ODB++ format using kicad-cli — a unified fabrication data format. |
| `refill_zones` | Refill every copper pour zone over KiCad IPC. Per-zone selection is not available; requires a running KiCad with the board open. |
| `get_drc_violations` | Run the Design Rule Check and return a list of violations. |

---

## Library

### `library` · 17 tools
**Purpose:** Search, register, and author symbol and footprint libraries — create symbols and footprints, edit pads, graphics, metadata and 3D models.
**Source:** [`crates/konnect-core/src/tools/library.rs`](crates/konnect-core/src/tools/library.rs)

| Tool | Description |
|------|-------------|
| `create_footprint` | Create a new footprint (`.kicad_mod`) file from a pad layout description. |
| `edit_footprint_pad` | Atomically edit or renumber matching pads, including valid circle/rect/oval/roundrect shape transitions and independent dimensions. |
| `set_footprint_graphics` | Atomically append, replace, or delete line, arc, rectangle, circle, and polygon primitives on one footprint layer. Replacement/deletion preserves unrelated source and rejects graphics referenced by a group. |
| `set_footprint_metadata` | Atomically replace a footprint description, tags, or supported attributes while preserving unrelated source. Empty tags or attributes remove their block. |
| `set_footprint_models` | Atomically append, replace, or delete one or more top-level 3D model blocks with optional offset, scale, and rotation transforms. |
| `register_footprint_library` | Register a local footprint library directory in the KiCAD global or project library table. Set `replace_existing` to update a stale URI in place while preserving entry metadata. |
| `list_footprint_libraries` | List all registered footprint libraries (global and/or project). |
| `create_symbol` | Create a new KiCAD schematic symbol and append it to a `.kicad_sym` library. |
| `delete_symbol` | Delete a symbol definition from a `.kicad_sym` library. |
| `list_symbols_in_library` | List all symbol names defined in a `.kicad_sym` library file. |
| `register_symbol_library` | Register a `.kicad_sym` library file in the KiCAD global or project symbol table. Reports `inserted`/`unchanged`/`updated`; set `replace_existing` to update a stale URI in place while preserving entry metadata. |
| `list_symbol_libraries` | List all registered symbol libraries (global and/or project). |
| `search_symbols` | Search for symbols across all registered libraries by name or keyword. |
| `list_library_footprints` | List all footprints in a specific registered library (`.pretty` directory). |
| `get_footprint_info` | Return detailed information about a footprint. Set `include_graphics` (and optionally `graphics_layer`) to inspect supported top-level primitives, geometry, stroke, fill, and item IDs. |
| `search_footprints` | Search for footprints across all registered libraries by name or keyword. |
| `get_symbol_info` | Return detailed information about a schematic symbol: pins, properties, description. |

---

## Integration

### `integration` · 9 tools
**Purpose:** JLCPCB parts database, local Freerouting MCP routing, datasheet URLs.
**Source:** [`crates/konnect-core/src/tools/integration.rs`](crates/konnect-core/src/tools/integration.rs)

| Tool | Description |
|------|-------------|
| `download_jlcpcb_database` | Download or update the local JLCPCB parts database cache (SQLite). |
| `search_jlcpcb_parts` | Search the local JLCPCB database by keyword, value, or category. |
| `get_jlcpcb_part` | Retrieve full details for a single JLCPCB part by LCSC part number. |
| `suggest_jlcpcb_alternatives` | Suggest JLCPCB-stocked alternatives for a given component value and footprint. |
| `get_jlcpcb_database_stats` | Statistics about the local JLCPCB cache: part count, last updated, file size. |
| `enrich_datasheets` | Fetch and cache datasheet URLs for all components in a schematic (LCSC API). |
| `get_datasheet_url` | Retrieve the datasheet URL for a component by MPN or LCSC ID — from the local JLCPCB catalog first, falling back to the LCSC API. |
| `check_freerouting` | Locate a Freerouting installation, including KiCad PCM plugin directories, then report `engine_found`, `native_mcp_available`, and `bridge_available` as separate observed facts. |
| `route_specctra_dsn` | Route a DSN through the discovered local Freerouting JAR's native headless MCP server and create a new SES without cloud upload or replacement. The owned unauthenticated service is loopback-only; its child is reaped on every exit path. |

Migration from the former `autoroute` tool: use `export_specctra_dsn`,
`route_specctra_dsn`, then `plan_specctra_ses_import` / `apply_specctra_ses`.
Konnect delegates routing to Freerouting's native MCP server instead of duplicating
the router or relying on the KiCad ActionPlugin workflow.

---

## Verification

### `verification` · 10 tools
**Purpose:** DRC, design rules, layer constraints, clearance checks, KiCAD UI control. ERC lives in `sch_export` (`run_erc`), not here.
**Source:** [`crates/konnect-core/src/tools/verification.rs`](crates/konnect-core/src/tools/verification.rs)

| Tool | Description |
|------|-------------|
| `run_drc` | Run KiCad's complete configured DRC ruleset and return structured violation results. |
| `set_design_rules` | Set board-level design rules (clearance, trace width, via size) in the sibling `.kicad_pro` project file. The board file is not modified. |
| `get_design_rules` | Return the current design rule constraints from the sibling `.kicad_pro` project file. |
| `set_predefined_sizes` | Write the PCB editor Pre-defined Sizes list (track widths and via pad/drill pairs) into the sibling `.kicad_pro`. These fill the Track/Via dropdowns; they are not DRC limits. |
| `get_predefined_sizes` | Return the Pre-defined Sizes list from the sibling `.kicad_pro`, including the 0 / 0,0 netclass sentinel. |
| `check_kicad_ui` | Check whether the KiCad GUI is running and whether IPC responds within the requested bounded timeout. |
| `launch_kicad_ui` | Launch the KiCAD GUI application and optionally open a project file. |
| `copy_routing_pattern` | Copy a routing pattern (traces and vias) from one region of the board to another. |
| `set_layer_constraints` | Set per-layer design constraints (min trace width, clearance) as named rules in the sibling `.kicad_dru` custom-rules file. |
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
| `audit_decoupling` | Audit schematic connectivity between IC power nets and decoupling capacitors; does not measure PCB placement distance. Defaults to one file; `schematic_scope: hierarchy` covers every reachable sheet instance. |
| `audit_connections` | Check for common connection mistakes: missing pull-ups on I2C/reset, missing series resistors on LEDs, floating inputs, shorted outputs. Defaults to one file; `schematic_scope: hierarchy` covers every reachable sheet instance. |
| `audit_power_rails` | Check power rail integrity: missing bulk capacitance, no test points, missing regulator output caps. Defaults to one file; `schematic_scope: hierarchy` covers every reachable sheet instance. |
| `audit_manufacturing` | DFM checks for the configured fab house: component spacing, silkscreen overlap, via-in-pad, acid traps, board-outline issues. |
| `run_design_review` | Run all available audit checks across every reachable schematic sheet and produce a consolidated report with status, coverage, and diagnostics. Returns `INCOMPLETE` rather than approval when coverage is partial or failed. |
| `check_bom_health` | Analyze the BOM for supply-chain risks: parts with no MPN, lifecycle warnings, low stock, unavailable from preferred distributors. Defaults to one file; `schematic_scope: hierarchy` covers every reachable sheet instance. |

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
| `validate_for_manufacturing` | Board pre-flight before ordering: checks outline, design rules, footprints, routing evidence, and complete DRC results. |
| `estimate_cost` | Estimate total manufacturing cost from board dimensions, layers, and footprint count, with an itemized breakdown. |

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
- Some schematic handlers are mid-migration from raw `konnect-sexp` to the typed `konnect-schematic-editor` model (Phase 2 Waves 1–4). Tool names and semantics are unchanged; only the internal implementation is in flux.

### Regenerating this doc

The tool list is extracted mechanically from the `tool!(...)` invocations in `crates/konnect-core/src/tools/*.rs`. To regenerate after adding tools, re-run the same extraction and re-verify counts against `registry.rs::ALL_TOOLSETS` — the row count in each table here must equal each toolset's `tool_count`.
