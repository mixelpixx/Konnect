//! `sch_wiring` toolset — wires, net labels, power symbols, junctions, no-connects.
//!
//! Key rule: Every wire add operation must auto-detect T-junctions and insert
//! junction dots. This uses `konnect_sexp::schematic::find_t_junctions`.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, opt_f64, opt_str, require_array, require_f64, require_str, ToolContext, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::snap_point,
    parser::parse_sexp,
    schematic::{
        extract_symbol_instances, extract_wires, find_lib_symbol, find_t_junctions,
        format_junction, format_wire, parse_at, pin_endpoint, pin_outward_direction,
        read_schematic, Wire,
    },
    writer::{
        apply_edits, find_balanced_block, find_block_starts, find_block_with_leading_whitespace,
        find_direct_child_blocks, find_enclosing_block, read_consistent, write_atomic_if_unchanged,
        SexpEdit,
    },
};
use serde_json::json;

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_wire",
            "Add a wire segment between two points. The wire must be horizontal or vertical. \
             T-junctions are automatically detected and junction dots inserted.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_add_wire(args, ctx).await }
        ),
        tool!(
            "batch_add_wire",
            "Add multiple wire segments in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "wires": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "x1": { "type": "number" }, "y1": { "type": "number" },
                                "x2": { "type": "number" }, "y2": { "type": "number" }
                            },
                            "required": ["x1", "y1", "x2", "y2"]
                        }
                    }
                },
                "required": ["schematic", "wires"]
            }),
            |args, ctx| async move { handle_batch_add_wire(args, ctx).await }
        ),
        tool!(
            "delete_schematic_wire",
            "Delete a wire segment by its UUID, or by matching BOTH endpoints \
             (all four of x1/y1/x2/y2, either direction). Fails without deleting \
             anything when no wire matches. Junction dots the wire leaves \
             unjustified are removed with it.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "uuid": { "type": "string", "description": "Wire UUID (preferred)" },
                    "x1": { "type": "number", "description": "Endpoint 1 X in mm (required with y1/x2/y2 when no uuid)" },
                    "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_delete_wire(args, ctx).await }
        ),
        tool!(
            "batch_delete_schematic_wire",
            "Delete multiple wire segments in a single file read/write cycle. \
             Junction dots the wires leave unjustified are removed with them, \
             reported as junctions_pruned_count.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "uuids": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["schematic", "uuids"]
            }),
            |args, ctx| async move { handle_batch_delete_wire(args, ctx).await }
        ),
        tool!(
            "split_wire_at_point",
            "Split a wire at a given point, creating two wire segments and a junction. \
             Note: a pin landing mid-wire only needs a junction dot to connect \
             (see add_junction) — splitting the wire is not required.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_split_wire_at_point(args, ctx).await }
        ),
        tool!(
            "add_schematic_net_label",
            "Add a net label to the schematic. Type can be 'net_label', 'global_label', \
             or 'hierarchical_label'.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string", "description": "Net name" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "rotation": { "type": "number", "default": 0 },
                    "label_type": {
                        "type": "string",
                        "enum": ["net_label", "global_label", "hierarchical_label"],
                        "default": "net_label"
                    },
                    "shape": {
                        "type": "string",
                        "description": "Shape for global/hierarchical labels (input/output/bidirectional/etc.)",
                        "default": "input"
                    }
                },
                "required": ["schematic", "net", "x", "y"]
            }),
            |args, ctx| async move { handle_add_net_label(args, ctx).await }
        ),
        tool!(
            "delete_schematic_net_label",
            "Delete a net label by net name and position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "net", "x", "y"]
            }),
            |args, ctx| async move { handle_delete_net_label(args, ctx).await }
        ),
        tool!(
            "rotate_schematic_label",
            "Rotate a net label to a new angle and update its justify direction accordingly.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "rotation": { "type": "number" }
                },
                "required": ["schematic", "net", "x", "y", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_label(args, ctx).await }
        ),
        tool!(
            "move_labels_by_offset",
            "Move all labels matching a net name by a given X/Y offset.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" },
                    "dx": { "type": "number" }, "dy": { "type": "number" }
                },
                "required": ["schematic", "net", "dx", "dy"]
            }),
            |args, ctx| async move { handle_move_labels_by_offset(args, ctx).await }
        ),
        tool!(
            "batch_rotate_labels",
            "Rotate multiple labels by net name in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "labels": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "net": { "type": "string" },
                                "x": { "type": "number" }, "y": { "type": "number" },
                                "rotation": { "type": "number" }
                            }
                        }
                    }
                },
                "required": ["schematic", "labels"]
            }),
            |args, ctx| async move { handle_batch_rotate_labels(args, ctx).await }
        ),
        tool!(
            "add_power_symbol",
            "Add a power symbol (VCC, GND, etc.) to the schematic. Auto-numbers the \
             internal #PWR reference to the lowest number free on the sheet.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "power_net": { "type": "string", "description": "Net name (e.g. 'VCC', 'GND')" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "rotation": { "type": "number", "default": 0 }
                },
                "required": ["schematic", "power_net", "x", "y"]
            }),
            |args, ctx| async move { handle_add_power_symbol(args, ctx).await }
        ),
        tool!(
            "add_no_connect",
            "Add a no-connect flag (X marker) to an unconnected pin endpoint.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_add_no_connect(args, ctx).await }
        ),
        tool!(
            "delete_no_connect",
            "Remove a no-connect flag at a given position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_delete_no_connect(args, ctx).await }
        ),
        tool!(
            "batch_delete_no_connect",
            "Delete multiple no-connect flags in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "positions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "x": { "type": "number" }, "y": { "type": "number" } }
                        }
                    }
                },
                "required": ["schematic", "positions"]
            }),
            |args, ctx| async move { handle_batch_delete_no_connect(args, ctx).await }
        ),
        tool!(
            "batch_add_no_connect",
            "Add multiple no-connect flags in a single file read/write cycle. Marking the unused \
             pins of one MCU is routinely 15-20 flags, which is 15-20 round trips through \
             add_no_connect.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "positions": {
                        "type": "array",
                        "description": "Pin endpoints to flag, as {x, y}",
                        "items": {
                            "type": "object",
                            "properties": { "x": { "type": "number" }, "y": { "type": "number" } },
                            "required": ["x", "y"]
                        }
                    }
                },
                "required": ["schematic", "positions"]
            }),
            |args, ctx| async move { handle_batch_add_no_connect(args, ctx).await }
        ),
        tool!(
            "add_junction",
            "Add a junction dot at a point where wires cross or T-intersect, or where \
             a pin lands mid-wire. A junction alone connects a mid-wire pin; \
             splitting the wire is not required.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_add_junction(args, ctx).await }
        ),
        tool!(
            "batch_add_junction",
            "Add multiple junction dots in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "positions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "x": { "type": "number" }, "y": { "type": "number" } }
                        }
                    }
                },
                "required": ["schematic", "positions"]
            }),
            |args, ctx| async move { handle_batch_add_junction(args, ctx).await }
        ),
        tool!(
            "connect_to_net",
            "Connect a pin to a named net by adding a short wire stub and a net label. \
             Name the pin with reference + pin_number, or give its coordinates directly.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string",
                        "description": "Component reference, e.g. 'U1'. Use with pin_number instead of pin_x/pin_y." },
                    "pin_number": { "type": "string", "description": "Pin number, e.g. '3'" },
                    "pin_x": { "type": "number", "description": "Pin X in mm; alternative to reference + pin_number" },
                    "pin_y": { "type": "number", "description": "Pin Y in mm" },
                    "net": { "type": "string" },
                    "direction": {
                        "type": "string",
                        "description": "Direction to route the wire stub. 'auto' (default) points it \
                                        away from the symbol body so the label text does not run back \
                                        across the pin names; it falls back to 'right' on a bare point.",
                        "enum": ["auto", "right", "left", "up", "down"],
                        "default": "auto"
                    },
                    "stub_length": { "type": "number", "default": 2.54,
                        "description": "Length of the wire stub in mm" },
                    "label_type": {
                        "type": "string",
                        "enum": ["net_label", "global_label"],
                        "default": "net_label"
                    }
                },
                "required": ["schematic", "net"]
            }),
            |args, ctx| async move { handle_connect_to_net(args, ctx).await }
        ),
        tool!(
            "connect_pins",
            "Connect two component pins by reference and pin number. \
             Looks up pin coordinates automatically and routes a wire between them.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "ref1": { "type": "string", "description": "First component reference (e.g. 'R1')" },
                    "pin1": { "type": "string", "description": "First pin number (e.g. '1')" },
                    "ref2": { "type": "string", "description": "Second component reference (e.g. 'U1')" },
                    "pin2": { "type": "string", "description": "Second pin number (e.g. '3')" }
                },
                "required": ["schematic", "ref1", "pin1", "ref2", "pin2"]
            }),
            |args, ctx| async move { handle_connect_pins(args, ctx).await }
        ),
        tool!(
            "add_schematic_connection",
            "Connect two schematic points directly with a wire (auto-routes H+V segments). \
             Use connect_pins if you have component references instead of coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_add_schematic_connection(args, ctx).await }
        ),
    ]
}

// ─── Shared: insert wires/labels BEFORE symbol instances ─────────────────────
//
// KiCAD 10 requires this element order in .kicad_sch files:
//   1. lib_symbols
//   2. wire, bus, junction, no_connect, net_label, global_label, text, etc.
//   3. symbol (instances) — MUST come last
//
// So wires and labels must be inserted before the first (symbol block,
// NOT at the end of the file.

pub(crate) fn insert_before_close(content: &str, new_sexp: &str) -> String {
    // Find the first top-level (symbol block — insert before it
    let item_pos = find_first_symbol_instance(content)
        .unwrap_or_else(|| content.rfind(')').unwrap_or(content.len()));
    let line_start = content[..item_pos].rfind('\n').map_or(0, |pos| pos + 1);
    let insert_pos = if content[line_start..item_pos]
        .chars()
        .all(char::is_whitespace)
    {
        line_start
    } else {
        item_pos
    };
    let insertion = format!("{}\n", new_sexp.trim_matches('\n'));
    let edits = vec![SexpEdit::insert(insert_pos, insertion)];
    apply_edits(content.to_string(), edits)
}

/// Find the byte offset of the first top-level symbol instance in the schematic.
/// Top-level instances have `(lib_id` as a child, while lib_symbols definitions don't.
/// Returns the position where wires/labels should be inserted BEFORE.
fn find_first_symbol_instance(content: &str) -> Option<usize> {
    for (start, end) in find_direct_child_blocks(content, "kicad_sch") {
        let block = &content[start..end];
        if block.starts_with("(symbol") && block.contains("(lib_id ") {
            return Some(start);
        }
    }
    None
}

// ─── Bridge: convert konnect-schematic-editor wires to konnect_sexp wires ──────

fn cse_wires_to_sexp(sch: &cse::Schematic) -> Vec<konnect_sexp::schematic::Wire> {
    sch.wires
        .iter()
        .map(|w| konnect_sexp::schematic::Wire {
            x1: w.start.0,
            y1: w.start.1,
            x2: w.end.0,
            y2: w.end.1,
            uuid: Some(w.uuid.clone()),
        })
        .collect()
}

// ─── Wire insertion with T-junction detection ─────────────────────────────────

/// Pin endpoints that lie strictly inside a wire segment. Each needs a
/// junction dot: KiCad connects a mid-wire pin only through a junction
/// (verified with kicad-cli 10 — no wire split required).
fn pins_mid_segment(pins: &[(f64, f64)], x1: f64, y1: f64, x2: f64, y2: f64) -> Vec<(f64, f64)> {
    let tol = 0.01;
    pins.iter()
        .copied()
        .filter(|&(px, py)| {
            konnect_sexp::geometry::point_on_segment(px, py, x1, y1, x2, y2, tol)
                && !konnect_sexp::geometry::points_coincident(px, py, x1, y1, tol)
                && !konnect_sexp::geometry::points_coincident(px, py, x2, y2, tol)
        })
        .collect()
}

/// Add a junction dot at each position that does not already carry one.
///
/// `find_t_junctions` reports every T on the sheet, not only the ones the new
/// wire made, so an unguarded loop re-emits a dot at every existing T on every
/// call — quadratic in a batch. `insert_wire_with_junctions` guards the same
/// way on the string path.
fn add_missing_junctions(sch: &mut cse::Schematic, positions: &[(f64, f64)]) {
    for &(x, y) in positions {
        if !sch
            .junctions
            .iter()
            .any(|j| konnect_sexp::geometry::points_coincident(x, y, j.x, j.y, 0.01))
        {
            sch.add_junction(x, y);
        }
    }
}

pub(crate) fn insert_wire_with_junctions(
    content: String,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
) -> String {
    // Parse existing wires to detect new T-junctions
    let tree = konnect_sexp::parse_sexp(&content).ok();
    let mut existing_wires = tree.as_ref().map(extract_wires).unwrap_or_default();

    // Existing junction positions, so a hit already marked isn't re-inserted
    // (L-bends, and any loop calling this repeatedly, would otherwise double it).
    let existing_junctions = tree
        .as_ref()
        .map(konnect_sexp::schematic::extract_junctions)
        .unwrap_or_default();

    // Add the new wire to the set before checking junctions (it may form T's too)
    let new_wire = konnect_sexp::schematic::Wire {
        x1,
        y1,
        x2,
        y2,
        uuid: None,
    };
    existing_wires.push(new_wire);

    let mut junctions = find_t_junctions(&existing_wires, 0.01);
    // Existing pins the new wire passes over also need junction dots.
    let pins = tree
        .as_ref()
        .map(crate::tools::all_pin_endpoints)
        .unwrap_or_default();
    for (px, py) in pins_mid_segment(&pins, x1, y1, x2, y2) {
        if !junctions
            .iter()
            .any(|&(jx, jy)| konnect_sexp::geometry::points_coincident(px, py, jx, jy, 0.01))
        {
            junctions.push((px, py));
        }
    }

    let mut c = content;
    c = insert_before_close(&c, &format_wire(x1, y1, x2, y2));
    for (jx, jy) in junctions {
        if existing_junctions
            .iter()
            .any(|(ex, ey)| konnect_sexp::geometry::points_coincident(jx, jy, *ex, *ey, 0.01))
        {
            continue;
        }
        c = insert_before_close(&c, &format_junction(jx, jy));
    }
    c
}

/// Route a wire between two points: a single straight wire when axis-aligned,
/// otherwise an H-then-V L-bend, each leg going through T-junction detection.
pub(crate) fn route_between(content: String, x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
        insert_wire_with_junctions(content, x1, y1, x2, y2)
    } else {
        let mid_x = x2;
        let mid_y = y1;
        let content = insert_wire_with_junctions(content, x1, y1, mid_x, mid_y);
        insert_wire_with_junctions(content, mid_x, mid_y, x2, y2)
    }
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_wire(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let (x1, y1) = snap_point(x1, y1, 1.27);
    let (x2, y2) = snap_point(x2, y2, 1.27);

    let mut sch = cse::Schematic::load(&sch_path)?;

    // T-junction detection: bridge cse wires to konnect_sexp wires
    let mut existing_wires = cse_wires_to_sexp(&sch);
    existing_wires.push(konnect_sexp::schematic::Wire {
        x1,
        y1,
        x2,
        y2,
        uuid: None,
    });
    let junctions = find_t_junctions(&existing_wires, 0.01);

    sch.add_wire(x1, y1, x2, y2);
    add_missing_junctions(&mut sch, &junctions);
    // Pins the new wire passes over mid-segment also need junction dots.
    let (_, tree) = read_schematic(&sch_path)?;
    let pins = crate::tools::all_pin_endpoints(&tree);
    add_missing_junctions(&mut sch, &pins_mid_segment(&pins, x1, y1, x2, y2));
    sch.overwrite()?;

    Ok(CallToolResult::json(
        &json!({ "added_wire": { "x1": x1, "y1": y1, "x2": x2, "y2": y2 } }),
    ))
}

async fn handle_batch_add_wire(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let wires = match require_array(args, "wires") {
        Ok(a) => a.clone(),
        Err(e) => return Ok(e),
    };

    // v0.6.0 made top-level required arguments refuse instead of defaulting,
    // but array items took the same silent path one level down (#234): an
    // element missing `x2` became a real wire to x=0 across the sheet,
    // reported as success. Validate every element before touching anything,
    // so a malformed batch changes nothing.
    for (i, w) in wires.iter().enumerate() {
        for key in ["x1", "y1", "x2", "y2"] {
            if w[key].as_f64().is_none() {
                return Ok(crate::tools::invalid_arg(
                    &format!("wires[{i}].{key}"),
                    "missing or not a number",
                ));
            }
        }
    }

    let mut sch = cse::Schematic::load(&sch_path)?;
    let mut added = 0usize;

    // Pin endpoints are fixed for the whole batch (only wires change below).
    let pins = read_schematic(&sch_path)
        .map(|(_, tree)| crate::tools::all_pin_endpoints(&tree))
        .unwrap_or_default();

    for w in &wires {
        let x1 = w["x1"].as_f64().unwrap_or(0.0);
        let y1 = w["y1"].as_f64().unwrap_or(0.0);
        let x2 = w["x2"].as_f64().unwrap_or(0.0);
        let y2 = w["y2"].as_f64().unwrap_or(0.0);
        let (x1, y1) = snap_point(x1, y1, 1.27);
        let (x2, y2) = snap_point(x2, y2, 1.27);

        // T-junction detection for each wire added incrementally.
        let mut existing_wires = cse_wires_to_sexp(&sch);
        existing_wires.push(konnect_sexp::schematic::Wire {
            x1,
            y1,
            x2,
            y2,
            uuid: None,
        });
        let junctions = find_t_junctions(&existing_wires, 0.01);

        sch.add_wire(x1, y1, x2, y2);
        add_missing_junctions(&mut sch, &junctions);
        // Pins this wire passes over mid-segment also need junction dots.
        add_missing_junctions(&mut sch, &pins_mid_segment(&pins, x1, y1, x2, y2));
        added += 1;
    }

    sch.overwrite()?;
    Ok(CallToolResult::json(&json!({ "added_wires": added })))
}

async fn handle_delete_wire(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let (del_start, del_end) = match locate_wire_for_delete(&content, args) {
        Ok(range) => range,
        Err(e) => return Ok(e),
    };

    let removed = wires_in_ranges(&content, &[(del_start, del_end)]);
    let edits = vec![SexpEdit::delete(del_start, del_end)];
    let new_content = apply_edits(content, edits);
    let (new_content, pruned) = prune_orphaned_junctions(new_content, &removed);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    Ok(CallToolResult::text(match pruned {
        0 => "Wire deleted.".to_string(),
        n => format!("Wire deleted. Removed {n} orphaned junction(s)."),
    }))
}

/// Byte range of the wire block the `uuid` or `x1/y1/x2/y2` arguments name.
fn locate_wire_for_delete(
    content: &str,
    args: &serde_json::Value,
) -> Result<(usize, usize), CallToolResult> {
    let range = if let Some(uuid) = opt_str(args, "uuid") {
        let search = format!(r#"(uuid "{uuid}")"#);
        let Some(wire_offset) = content.find(&search) else {
            return Err(CallToolResult::error(format!(
                "Wire UUID '{uuid}' not found"
            )));
        };
        wire_block_with_leading_whitespace(content, wire_offset)
    } else {
        let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
            opt_f64(args, "x1"),
            opt_f64(args, "y1"),
            opt_f64(args, "x2"),
            opt_f64(args, "y2"),
        ) else {
            return Err(CallToolResult::error(
                "Provide either uuid or all x1/y1/x2/y2 coordinates",
            ));
        };
        find_wire_block_by_endpoints(content, x1, y1, x2, y2)
    };

    range.ok_or_else(|| {
        CallToolResult::error("Cannot locate a wire block matching the requested identity")
    })
}

/// The wires inside the given byte ranges of the document.
fn wires_in_ranges(content: &str, ranges: &[(usize, usize)]) -> Vec<Wire> {
    // `extract_wires` expects wires as direct children of the parsed document
    // root, so wrap every range in one minimal root and parse it once.
    let mut wrapped = String::from("(kicad_sch ");
    for &(start, end) in ranges {
        wrapped.push_str(&content[start..end]);
    }
    wrapped.push(')');
    parse_sexp(&wrapped)
        .map(|node| extract_wires(&node))
        .unwrap_or_default()
}

/// Drop the junction dots the removed wires left with nothing to justify them.
///
/// Deleting a wire used to leave its dots behind, so relocating a block — delete
/// its wires, re-add them elsewhere — stranded junctions at the old coordinates.
/// A dot needs two wires to mean anything, with one exception: one wire plus a
/// pin landing mid-segment, which is exactly what `pins_mid_segment` creates.
/// So a junction is pruned when no wire is left through it, or one is and no pin
/// sits there. Junctions no removed wire touched are left alone, and moves that
/// strand a dot without deleting a wire are #120's half of the problem.
/// Round to the six decimals KiCAD writes, so arithmetic noise never reaches
/// the file.
fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

/// Re-evaluate the junction dots at `points` after geometry moved.
///
/// `prune_orphaned_junctions` answers the same question for a *wire* deletion:
/// it knows which dots to look at because it knows which wires went away. A
/// component move changes no wires at all — it moves *pins* — so the candidate
/// set has to come from the pins that appeared or disappeared, and the dot at
/// the vacated position has to be re-judged (#120).
///
/// All connectivity answers come from the shared [`ConnectivityIndex`] — this
/// must not become a fifth private definition of "what is attached here"
/// (#323). A dot is kept when anything still justifies it:
///
/// * two or more wires under the point — a T, a crossing joined on purpose, or
///   wire ends meeting: never touched;
/// * a pin or a hierarchical sheet pin still at the point;
/// * pins unresolvable (`lib_symbols` lookup failed) — that is not the same as
///   "no pin here", so only the unambiguous zero-wire case prunes then.
///
/// A dot is added only where a pin has landed mid-span on **exactly one** wire
/// and no no-connect flag sits there. One wire is the unambiguous case: with
/// two, wires that merely cross are separate nets until a junction says
/// otherwise, and adding the dot would merge them — the same silent
/// connectivity change this pass exists to stop. A no-connect is the user
/// saying "this pin stays unconnected", which a move must not override.
///
/// Returns the content plus (added, pruned).
pub(crate) fn reconcile_junctions_at(
    content: String,
    points: &[(f64, f64)],
) -> (String, usize, usize) {
    if points.is_empty() {
        return (content, 0, 0);
    }
    let Ok(tree) = parse_sexp(&content) else {
        return (content, 0, 0);
    };
    let wires = extract_wires(&tree);
    let labels = konnect_sexp::schematic::extract_all_net_labels(&tree);
    let idx = crate::tools::sch_connectivity::ConnectivityIndex::build(
        &tree,
        &wires,
        &labels,
        crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
    );
    // The shared index does not model buses yet, and KiCAD has bus junctions:
    // a dot on a bus tee joins bus segments, which no wire count can see. Any
    // candidate point touching a bus line is therefore outside this pass's
    // jurisdiction — neither pruned nor added at. This is a guard, not a fifth
    // attachment answer: the moment the index learns buses, it replaces this.
    let buses = konnect_sexp::schematic::extract_buses(&tree);
    let on_bus = |x: f64, y: f64| {
        buses.iter().any(|b| {
            konnect_sexp::geometry::point_on_segment(
                x,
                y,
                b.x1,
                b.y1,
                b.x2,
                b.y2,
                crate::tools::sch_connectivity::COINCIDENT_TOLERANCE,
            )
        })
    };
    let pins_known = !idx.placed_pins().is_empty() || extract_symbol_instances(&tree).is_empty();

    // Existing dots at the candidate points, with the byte range to delete —
    // the one thing the index does not hold.
    struct Dot {
        range: (usize, usize),
        at: (f64, f64),
    }
    let mut existing: Vec<Dot> = Vec::new();
    for start in find_block_starts(&content, "junction") {
        let Some((ws_start, block_end)) = find_block_with_leading_whitespace(&content, start)
        else {
            continue;
        };
        let Ok(node) = parse_sexp(&content[start..block_end]) else {
            continue;
        };
        let Some((jx, jy, _)) = parse_at(&node) else {
            continue;
        };
        existing.push(Dot {
            range: (ws_start, block_end),
            at: (jx, jy),
        });
    }

    const TOL: f64 = crate::tools::sch_connectivity::COINCIDENT_TOLERANCE;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut to_add: Vec<(f64, f64)> = Vec::new();
    for &(px, py) in points {
        if on_bus(px, py) {
            continue;
        }
        let here: Vec<&Dot> = existing
            .iter()
            .filter(|d| konnect_sexp::geometry::points_coincident(px, py, d.at.0, d.at.1, TOL))
            .collect();
        let attached = idx.has_pin(px, py) || idx.has_sheet_pin(px, py);
        if here.is_empty() {
            if idx.wires_at(px, py) == 1
                && idx.on_wire_interior(px, py)
                && idx.has_pin(px, py)
                && !idx.has_no_connect(px, py)
            {
                to_add.push((px, py));
            }
        } else {
            let keep = match idx.wires_at(px, py) {
                0 => false,
                1 => !pins_known || attached,
                _ => true,
            };
            if !keep {
                ranges.extend(here.iter().map(|d| d.range));
            }
        }
    }

    // Two coincident pin endpoints vacating one spot produce the same
    // candidate twice — dedup BEFORE counting, or the counts overstate what
    // happened and the add branch writes the same dot twice.
    ranges.sort_unstable();
    ranges.dedup();
    let pruned = ranges.len();
    let mut out = apply_edits(
        content,
        ranges
            .into_iter()
            .map(|(s, e)| SexpEdit::delete(s, e))
            .collect(),
    );
    let mut to_add: Vec<(f64, f64)> = to_add
        .into_iter()
        // Pin endpoints come out of arithmetic (136.19 + 3.81 = 139.70000000000002)
        // and `format_junction` interpolates the f64 verbatim, so round to the
        // 6 decimals KiCAD writes rather than leaking float noise into the file.
        .map(|(x, y)| (round6(x), round6(y)))
        .collect();
    to_add.sort_by(|a, b| a.partial_cmp(b).expect("rounded coordinates are finite"));
    to_add.dedup();
    let added = to_add.len();
    for (x, y) in to_add {
        out = insert_before_close(&out, &format_junction(x, y));
    }
    (out, added, pruned)
}

fn prune_orphaned_junctions(content: String, removed: &[Wire]) -> (String, usize) {
    const TOL: f64 = 0.01;
    // Two is as high as this needs to count, so it stops there.
    let wires_through = |wires: &[Wire], jx: f64, jy: f64| {
        wires
            .iter()
            .filter(|w| {
                konnect_sexp::geometry::point_on_segment(jx, jy, w.x1, w.y1, w.x2, w.y2, TOL)
            })
            .take(2)
            .count()
    };

    // The dots the removed wires ran through, with the block to delete. Most
    // deletions find none, and the document parse below is only for those.
    let mut candidates: Vec<((usize, usize), (f64, f64))> = Vec::new();
    for start in find_block_starts(&content, "junction") {
        let Some((ws_start, block_end)) = find_block_with_leading_whitespace(&content, start)
        else {
            continue;
        };
        let Ok(node) = parse_sexp(&content[start..block_end]) else {
            continue;
        };
        let Some((jx, jy, _)) = parse_at(&node) else {
            continue;
        };
        if wires_through(removed, jx, jy) > 0 {
            candidates.push(((ws_start, block_end), (jx, jy)));
        }
    }
    if candidates.is_empty() {
        return (content, 0);
    }

    let Ok(tree) = parse_sexp(&content) else {
        return (content, 0);
    };
    let remaining = extract_wires(&tree);
    // Resolving pins walks every symbol against `lib_symbols`, so it waits
    // until a dot is down to its last wire and the answer decides the case.
    let mut pins: Option<(Vec<(f64, f64)>, bool)> = None;

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (range, (jx, jy)) in candidates {
        let orphaned = match wires_through(&remaining, jx, jy) {
            0 => true,
            1 => {
                let (pins, pins_known) = pins.get_or_insert_with(|| {
                    let pins = crate::tools::all_pin_endpoints(&tree);
                    // A sheet with symbols but no resolvable pin means the
                    // `lib_symbols` lookup failed, not that nothing is
                    // connected. Only the unambiguous zero-wire case prunes
                    // then, rather than guess a pin isn't there.
                    let known = !pins.is_empty() || extract_symbol_instances(&tree).is_empty();
                    (pins, known)
                });
                *pins_known
                    && !pins.iter().any(|&(px, py)| {
                        konnect_sexp::geometry::points_coincident(px, py, jx, jy, TOL)
                    })
            }
            _ => false,
        };
        if orphaned {
            ranges.push(range);
        }
    }

    let pruned = ranges.len();
    let edits = ranges
        .into_iter()
        .map(|(s, e)| SexpEdit::delete(s, e))
        .collect();
    (apply_edits(content, edits), pruned)
}

async fn handle_batch_delete_wire(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let uuids: Vec<String> = match require_array(args, "uuids") {
        Ok(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut errors = Vec::new();

    // Collect all delete ranges first, then apply in reverse order
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for uuid in &uuids {
        let search = format!(r#"(uuid "{uuid}")"#);
        match content.find(&search) {
            Some(offset) => match wire_block_with_leading_whitespace(&content, offset) {
                Some(range) => ranges.push(range),
                None => errors.push(format!(
                    "UUID '{uuid}' exists but is not inside a parseable wire block"
                )),
            },
            None => errors.push(format!("Wire UUID '{uuid}' not found")),
        }
    }
    ranges.sort_unstable();
    ranges.dedup();
    let deleted = ranges.len();

    if deleted == 0 && !uuids.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No wires deleted: {}",
            errors.join("; ")
        )));
    }

    let removed = wires_in_ranges(&content, &ranges);
    let edits: Vec<SexpEdit> = ranges
        .into_iter()
        .map(|(s, e)| SexpEdit::delete(s, e))
        .collect();
    let content = apply_edits(content, edits);
    let (content, pruned) = prune_orphaned_junctions(content, &removed);
    write_atomic_if_unchanged(&sch_path, &expected, &content)?;
    Ok(CallToolResult::json(&json!({
        "deleted": deleted,
        "junctions_pruned_count": pruned,
        "errors": errors
    })))
}

fn wire_block_with_leading_whitespace(
    content: &str,
    contained_offset: usize,
) -> Option<(usize, usize)> {
    let (wire_start, _) = find_enclosing_block(content, "wire", contained_offset)?;
    find_block_with_leading_whitespace(content, wire_start)
}

fn find_wire_block_by_endpoints(
    content: &str,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
) -> Option<(usize, usize)> {
    const TOLERANCE: f64 = 1e-6;
    let same = |a: f64, b: f64| (a - b).abs() <= TOLERANCE;

    for start in find_block_starts(content, "wire") {
        let Some((block_start, block_end)) = find_balanced_block(content, start) else {
            continue;
        };
        let matches = wires_in_ranges(content, &[(block_start, block_end)])
            .into_iter()
            .any(|wire| {
                (same(wire.x1, x1) && same(wire.y1, y1) && same(wire.x2, x2) && same(wire.y2, y2))
                    || (same(wire.x1, x2)
                        && same(wire.y1, y2)
                        && same(wire.x2, x1)
                        && same(wire.y2, y1))
            });
        if matches {
            return find_block_with_leading_whitespace(content, block_start);
        }
    }
    None
}

async fn handle_split_wire_at_point(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let px = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let py = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let tree = parse_sexp(&content)?;
    let wires = extract_wires(&tree);

    // Find the wire that contains point (px, py) but is not an endpoint
    let target = wires.iter().find(|w| {
        !konnect_sexp::geometry::points_coincident(px, py, w.x1, w.y1, 0.01)
            && !konnect_sexp::geometry::points_coincident(px, py, w.x2, w.y2, 0.01)
            && konnect_sexp::geometry::point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, 0.01)
    });

    let w = match target {
        Some(w) => w.clone(),
        None => {
            return Ok(CallToolResult::error(
                "No wire found passing through that point",
            ))
        }
    };

    // Delete the original wire and insert two halves + junction. Not through
    // `handle_delete_wire`: the two halves cover the same segment, so the
    // wire's junctions stay justified and must not be pruned in between.
    let block = match &w.uuid {
        Some(uuid) => content
            .find(&format!(r#"(uuid "{uuid}")"#))
            .and_then(|offset| wire_block_with_leading_whitespace(&content, offset)),
        None => find_wire_block_by_endpoints(&content, w.x1, w.y1, w.x2, w.y2),
    };
    let Some((del_start, del_end)) = block else {
        return Ok(CallToolResult::error(
            "Cannot locate a wire block matching the requested identity",
        ));
    };
    let content = apply_edits(content, vec![SexpEdit::delete(del_start, del_end)]);

    let w1 = format_wire(w.x1, w.y1, px, py);
    let w2 = format_wire(px, py, w.x2, w.y2);
    let junc = format_junction(px, py);
    let insert = format!("{w1}{w2}{junc}");
    let new_content = insert_before_close(&content, &insert);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "split_at": { "x": px, "y": py },
        "wire_a": { "x1": w.x1, "y1": w.y1, "x2": px, "y2": py },
        "wire_b": { "x1": px, "y1": py, "x2": w.x2, "y2": w.y2 }
    })))
}

async fn handle_add_net_label(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let rotation = opt_f64(args, "rotation").unwrap_or(0.0);
    let label_type = opt_str(args, "label_type").unwrap_or("net_label");
    let shape = opt_str(args, "shape").unwrap_or("input");

    let mut sch = cse::Schematic::load(&sch_path)?;

    // set_rotation also writes the (effects … (justify …)) block. justify is
    // what turns the text away from the anchor, so a label created without one
    // renders backwards at 180°/270°, over whatever it points at.
    match label_type {
        "global_label" => {
            sch.add_global_label(&net, shape, x, y);
            let idx = sch.global_labels.len() - 1;
            if let Some(gl) = sch.global_labels.get_mut(idx) {
                gl.set_rotation(rotation);
            }
        }
        "hierarchical_label" => {
            sch.add_hierarchical_label(&net, shape, x, y);
            let idx = sch.hierarchical_labels.len() - 1;
            if let Some(hl) = sch.hierarchical_labels.get_mut(idx) {
                hl.set_rotation(rotation);
            }
        }
        _ => {
            let label = sch.add_label(&net, x, y);
            label.set_rotation(rotation);
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(
        &json!({ "added_label": net, "type": label_type, "x": x, "y": y }),
    ))
}

async fn handle_delete_net_label(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let target_x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let target_y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let labels = find_label_blocks(&content);
    let named: Vec<&LabelBlock> = labels.iter().filter(|l| l.net == net).collect();

    if named.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No label named '{}' in this schematic",
            net
        )));
    }

    // Exact position match. Deleting the *nearest* label instead would silently
    // remove a same-named label elsewhere on the sheet — same-named labels are
    // how KiCAD joins nets, so they are the normal case, not an edge case.
    let matched: Vec<&&LabelBlock> = named
        .iter()
        .filter(|l| same_point(l.x, target_x) && same_point(l.y, target_y))
        .collect();

    let label = match matched.as_slice() {
        [one] => **one,
        [] => {
            let positions: Vec<String> = named
                .iter()
                .map(|l| format!("{} at ({}, {})", l.kind, l.x, l.y))
                .collect();
            return Ok(CallToolResult::error(format!(
                "No label '{}' at ({}, {}). Found {} label(s) named '{}': {}",
                net,
                target_x,
                target_y,
                named.len(),
                net,
                positions.join("; ")
            )));
        }
        _ => {
            return Ok(CallToolResult::error(format!(
                "{} labels named '{}' share position ({}, {}) — delete by uuid is not \
                 supported yet; remove the duplicates in eeschema",
                matched.len(),
                net,
                target_x,
                target_y
            )));
        }
    };

    let (del_start, del_end) = find_block_with_leading_whitespace(&content, label.start)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse label block"))?;

    let kind = label.kind;
    let edits = vec![SexpEdit::delete(del_start, del_end)];
    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    Ok(CallToolResult::json(&json!({
        "deleted_label": net,
        "type": kind,
        "at": { "x": target_x, "y": target_y }
    })))
}

/// One label block located in the raw file text.
struct LabelBlock {
    /// Byte offset of the block's opening paren.
    start: usize,
    /// S-expression tag: `label`, `global_label`, or `hierarchical_label`.
    kind: &'static str,
    net: String,
    x: f64,
    y: f64,
}

/// KiCAD's three label tags. `label` is the plain net label — the type
/// `add_schematic_net_label` writes by default. (`net_label` is this codebase's
/// internal name for it and never appears in a .kicad_sch.)
const LABEL_TAGS: [&str; 3] = ["label", "global_label", "hierarchical_label"];

/// Locate every label block in `content` by scanning forward for the label tags
/// and parsing each block, rather than searching for a name string and walking
/// backwards — a quoted net name also appears in symbol properties, pin names,
/// and sheet pins, and walking back from one of those lands on an unrelated
/// block.
fn find_label_blocks(content: &str) -> Vec<LabelBlock> {
    let mut out = Vec::new();
    for kind in LABEL_TAGS {
        for start in find_block_starts(content, kind) {
            let Some((bs, be)) = find_balanced_block(content, start) else {
                continue;
            };
            let Ok(node) = parse_sexp(&content[bs..be]) else {
                continue;
            };
            // (label "NAME" (at X Y ROT) …) — the name is the first argument,
            // and (at) is a direct child, so a nested (at) on a global label's
            // intersheet-refs property can't be mistaken for the anchor.
            let Some(net) = node.get(1).and_then(|n| n.as_str()) else {
                continue;
            };
            let Some((x, y, _)) = parse_at(&node) else {
                continue;
            };
            out.push(LabelBlock {
                start: bs,
                kind,
                net: net.to_string(),
                x,
                y,
            });
        }
    }
    out
}

/// Compare schematic coordinates. KiCAD stores mm to 4 decimals, so this is an
/// exact match in practice while tolerating float round-trip noise.
fn same_point(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

async fn handle_rotate_label(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let labels = find_label_blocks(&content);
    let named: Vec<&LabelBlock> = labels.iter().filter(|l| l.net == net).collect();
    let Some(label) = named
        .iter()
        .find(|l| same_point(l.x, x) && same_point(l.y, y))
    else {
        let positions: Vec<String> = named
            .iter()
            .map(|l| format!("{} at ({}, {})", l.kind, l.x, l.y))
            .collect();
        return Ok(CallToolResult::error(if positions.is_empty() {
            format!("No label named '{}' in this schematic", net)
        } else {
            format!(
                "No label '{}' at ({}, {}). Found: {}",
                net,
                x,
                y,
                positions.join("; ")
            )
        }));
    };

    let (block_start, block_end) = find_balanced_block(&content, label.start)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse label block"))?;
    let block = &content[block_start..block_end];

    let mut edits = Vec::new();

    // 1. The (at X Y ROT) anchor.
    let at_rel = block
        .find("(at ")
        .ok_or_else(|| anyhow::anyhow!("No (at) in label block"))?;
    let at_val = block_start + at_rel + "(at ".len();
    let at_close = content[at_val..]
        .find(')')
        .map(|o| at_val + o)
        .ok_or_else(|| anyhow::anyhow!("Malformed (at)"))?;
    edits.push(SexpEdit::replace(
        at_val,
        at_close,
        format!("{x} {y} {rotation}"),
    ));

    // 2. The justify, which is what actually turns the text — rotating the
    //    anchor alone leaves the text running back over whatever the label
    //    points at. Plain labels also carry `bottom` to lift text off the wire.
    let plain = label.kind == "label";
    let justify = konnect_sexp::schematic::label_justify(rotation);
    let justify_sexp = if plain {
        format!("(justify {justify} bottom)")
    } else {
        format!("(justify {justify})")
    };

    if let Some(j_rel) = block.find("(justify ") {
        // Replace the existing justify in place.
        let j_start = block_start + j_rel;
        let j_end = find_balanced_block(&content, j_start)
            .map(|(_, e)| e)
            .ok_or_else(|| anyhow::anyhow!("Malformed (justify)"))?;
        edits.push(SexpEdit::replace(j_start, j_end, justify_sexp));
    } else if let Some(e_rel) = block.find("(effects") {
        // An effects block with no justify — add one just inside it.
        let e_start = block_start + e_rel;
        let (_, e_end) = find_balanced_block(&content, e_start)
            .ok_or_else(|| anyhow::anyhow!("Malformed (effects)"))?;
        edits.push(SexpEdit::insert(e_end - 1, format!(" {justify_sexp}")));
    } else {
        // No effects at all — the shape add_schematic_net_label used to write.
        // Insert a complete block where eeschema puts it: before the uuid,
        // matching that line's indentation.
        let insert_at = block
            .find("(uuid")
            .map(|r| block_start + r)
            .unwrap_or(block_end - 1);
        let line_start = content[..insert_at]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(insert_at);
        let indent: String = content[line_start..insert_at]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        edits.push(SexpEdit::insert(
            insert_at,
            format!("(effects (font (size 1.27 1.27)) {justify_sexp})\n{indent}"),
        ));
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    Ok(CallToolResult::json(&json!({
        "rotated_label": net,
        "type": label.kind,
        "rotation": rotation,
        "justify": justify
    })))
}

async fn handle_move_labels_by_offset(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let labels = find_label_blocks(&content);
    let matching: Vec<&LabelBlock> = labels.iter().filter(|l| l.net == net).collect();
    if matching.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No label named '{}' in this schematic",
            net
        )));
    }

    // Edit each label's (at X Y ROT) anchor in place, preserving the rotation.
    let mut edits = Vec::new();
    for label in &matching {
        let (block_start, block_end) = find_balanced_block(&content, label.start)
            .ok_or_else(|| anyhow::anyhow!("Cannot parse label block"))?;
        let block = &content[block_start..block_end];
        let at_rel = block
            .find("(at ")
            .ok_or_else(|| anyhow::anyhow!("No (at) in label block"))?;
        let at_val = block_start + at_rel + "(at ".len();
        let at_close = content[at_val..]
            .find(')')
            .map(|o| at_val + o)
            .ok_or_else(|| anyhow::anyhow!("Malformed (at)"))?;
        let rotation = content[at_val..at_close]
            .split_whitespace()
            .nth(2)
            .unwrap_or("0")
            .to_string();
        edits.push(SexpEdit::replace(
            at_val,
            at_close,
            format!("{} {} {}", label.x + dx, label.y + dy, rotation),
        ));
    }

    let moved = edits.len();
    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "moved_labels": moved, "net": net }),
    ))
}

async fn handle_batch_rotate_labels(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let labels = match require_array(args, "labels") {
        Ok(a) => a.clone(),
        Err(e) => return Ok(e),
    };
    let mut rotated = 0usize;
    for label_arg in &labels {
        let full_args = json!({
            "schematic": sch_path.display().to_string(),
            "net": label_arg["net"],
            "x": label_arg["x"],
            "y": label_arg["y"],
            "rotation": label_arg["rotation"]
        });
        handle_rotate_label(&full_args, ctx).await?;
        rotated += 1;
    }
    Ok(CallToolResult::json(&json!({ "rotated": rotated })))
}

/// The lowest `#PWR` number no symbol on the sheet is using.
///
/// Counting the power symbols instead re-issues a live designator after a
/// deletion: drop `#PWR028` from a sheet of 29 and the count is 28, so the
/// next symbol is handed `#PWR029` — still in use, silently duplicated.
fn next_pwr_number(sch: &cse::Schematic) -> u32 {
    let used: std::collections::HashSet<u32> = sch
        .symbols
        .iter()
        .filter_map(|s| s.reference())
        .filter_map(|r| r.strip_prefix("#PWR"))
        .filter_map(|n| n.parse::<u32>().ok())
        .collect();
    (1u32..).find(|n| !used.contains(n)).unwrap_or(1)
}

async fn handle_add_power_symbol(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let power_net = match require_str(args, "power_net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let rotation = opt_f64(args, "rotation").unwrap_or(0.0);

    let mut sch = cse::Schematic::load(&sch_path)?;
    let context = match crate::tools::sheet_instance_context(&sch_path, &mut sch) {
        Ok(context) => context,
        Err(error) => return Ok(error.into_tool_result()),
    };
    if let Err(error) = crate::tools::validate_sheet_instance_state(&sch_path, &sch, &context) {
        return Ok(error.into_tool_result());
    }

    let pwr_ref = format!("#PWR{:03}", next_pwr_number(&sch));

    // Embed the power symbol definition in lib_symbols
    let lib_id = format!("power:{}", power_net);
    let src = match crate::tools::library::KiCadSymbolSource::for_file(&sch_path) {
        Ok(source) => source,
        Err(error) => return Ok(error.into_tool_result()),
    };
    if !cse::library::ensure_lib_symbol(&mut sch, &lib_id, &src) {
        return Ok(crate::tools::lib_symbol_not_found_error(&lib_id, &src));
    }
    let metadata = cse::library::symbol_metadata(&sch, &lib_id);

    // Build the Symbol struct
    let mut sym = cse::Symbol::new(format!("power:{}", power_net), x, y);
    sym.at.rotation = Some(rotation);
    sym.unit = 1;
    sym.in_bom = true;
    sym.on_board = true;
    sym.uuid = uuid::Uuid::new_v4().to_string();

    // Property (at …) is absolute sheet coords — same as add_schematic_component.
    // Bare Property::new writes no (at); KiCad then defaults to (0,0) and every
    // #PWR piles up in the top-left corner. Hide Reference like eeschema does
    // (property-level `(hide yes)`, matching what KiCad 10 itself writes).
    //
    // The library anchors matter most here: GND anchors Value below the
    // graphic but VCC/+5V/+3V3 anchor it above, so one fixed offset cannot
    // suit both (#101).
    let anchors = cse::library::field_anchors(&sch, &lib_id);
    let t = konnect_sexp::geometry::PinTransform {
        comp_x: x,
        comp_y: y,
        rotation_deg: rotation,
        mirror_x: false,
        mirror_y: false,
    };
    let (ref_x, ref_y, ref_rot) =
        crate::tools::field_at(anchors.reference_at, crate::tools::FALLBACK_REFERENCE_AT, t);
    let (val_x, val_y, val_rot) =
        crate::tools::field_at(anchors.value_at, crate::tools::FALLBACK_VALUE_AT, t);
    let positioned = crate::tools::positioned_property;
    let centred = cse::library::FieldJustify::default();
    sym.properties.push(positioned(
        "Reference",
        &pwr_ref,
        ref_x,
        ref_y,
        ref_rot,
        true,
        anchors.reference_justify,
    ));
    sym.properties.push(positioned(
        "Value",
        &power_net,
        val_x,
        val_y,
        val_rot,
        false,
        anchors.value_justify,
    ));
    sym.properties
        .push(positioned("Footprint", "", x, y, 0.0, true, centred));
    sym.properties.push(positioned(
        "Datasheet",
        &metadata.datasheet,
        x,
        y,
        0.0,
        true,
        centred,
    ));
    sym.properties.push(positioned(
        "Description",
        &metadata.description,
        x,
        y,
        0.0,
        true,
        centred,
    ));

    // Instance entry, keyed to the root sheet UUID like eeschema writes it —
    // without a resolvable "/<root-uuid>" path KiCAD's netlister drops the
    // symbol from net formation.
    for instance_path in &context.instance_paths {
        sym.set_instance_path(&context.project_name, instance_path, &pwr_ref, 1);
    }

    let uuid = sym.uuid.clone();
    sch.add_symbol(sym);
    sch.overwrite()?;

    // A power pin landing mid-segment on an existing wire needs a junction
    // dot, or KiCad ERC reports it as not connected.
    let junctions_added = crate::tools::add_pin_midwire_junctions(&sch_path, &pwr_ref)?;
    let committed = cse::Schematic::load(&sch_path)?;
    let mut observed = match super::sch_components::placed_component_readback(
        &sch_path, &committed, &uuid, &context,
    ) {
        Ok(result) => result,
        Err(error) => return Ok(error),
    };
    observed["added_power"] = observed["value"].clone();
    observed["junctions_added"] = json!(junctions_added
        .iter()
        .map(|(x, y)| json!({"x": x, "y": y}))
        .collect::<Vec<_>>());

    Ok(CallToolResult::json(&observed))
}

async fn handle_add_no_connect(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    sch.add_no_connect(x, y);
    sch.overwrite()?;
    Ok(CallToolResult::json(
        &json!({ "added_no_connect": { "x": x, "y": y } }),
    ))
}

async fn handle_batch_add_no_connect(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let positions = match args["positions"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'positions' array")),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for (i, p) in positions.iter().enumerate() {
        match (p["x"].as_f64(), p["y"].as_f64()) {
            (Some(x), Some(y)) => {
                sch.add_no_connect(x, y);
                added.push(json!({ "x": x, "y": y }));
            }
            _ => errors.push(format!("Position {i}: needs numeric x and y")),
        }
    }
    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "added_count": added.len(),
        "added": added,
        "errors": errors
    })))
}

async fn handle_delete_no_connect(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let Some((del_start, del_end)) = find_no_connect_block_at(&content, x, y) else {
        return Ok(CallToolResult::error(
            "No-connect not found at that position",
        ));
    };
    let new_content = apply_edits(content, vec![SexpEdit::delete(del_start, del_end)]);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    Ok(CallToolResult::text("No-connect deleted."))
}

/// Byte range of the `(no_connect …)` block whose `(at …)` is `(x, y)`.
///
/// The previous implementation searched for the literal
/// `"(no_connect (at {x} {y})"`. No-connect blocks are never written on one
/// line — this crate's writer takes the multi-line branch for any node with
/// list children, and eeschema does the same with tabs — so that string never
/// matched anything and both delete tools were inert (#114). Same failure
/// class as the wire deletion in #64; this reuses the #69 block machinery the
/// wire path already uses, including its coordinate tolerance.
fn find_no_connect_block_at(content: &str, x: f64, y: f64) -> Option<(usize, usize)> {
    const TOLERANCE: f64 = 1e-6;
    let same = |a: f64, b: f64| (a - b).abs() <= TOLERANCE;

    for start in find_block_starts(content, "no_connect") {
        let Some((block_start, block_end)) = find_balanced_block(content, start) else {
            continue;
        };
        let Ok(node) = parse_sexp(&content[block_start..block_end]) else {
            continue;
        };
        let Some(at) = node.find("at") else { continue };
        let (Some(bx), Some(by)) = (at.get_f64(1), at.get_f64(2)) else {
            continue;
        };
        if same(bx, x) && same(by, y) {
            return find_block_with_leading_whitespace(content, block_start);
        }
    }
    None
}

async fn handle_batch_delete_no_connect(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let positions = match require_array(args, "positions") {
        Ok(a) => a.clone(),
        Err(e) => return Ok(e),
    };

    // One read, collect every range, one write — matching batch_delete_wire.
    // The old loop delegated to the single-item handler and counted `.is_ok()`,
    // but that handler returns `Ok(CallToolResult::error(..))` when nothing
    // matches, so every failure counted as a success and the tool reported
    // deletions it had not made (#114).
    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for pos in &positions {
        let (Some(x), Some(y)) = (pos["x"].as_f64(), pos["y"].as_f64()) else {
            errors.push(format!("Position {pos} needs numeric x and y"));
            continue;
        };
        match find_no_connect_block_at(&content, x, y) {
            Some(range) => ranges.push(range),
            None => errors.push(format!("No no-connect at ({x}, {y})")),
        }
    }
    ranges.sort_unstable();
    ranges.dedup();
    let deleted = ranges.len();

    if deleted == 0 && !positions.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No no-connects deleted: {}",
            errors.join("; ")
        )));
    }

    let edits: Vec<SexpEdit> = ranges
        .into_iter()
        .map(|(s, e)| SexpEdit::delete(s, e))
        .collect();
    let content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &content)?;
    Ok(CallToolResult::json(&json!({
        "deleted": deleted,
        "errors": errors
    })))
}

async fn handle_add_junction(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    sch.add_junction(x, y);
    sch.overwrite()?;
    Ok(CallToolResult::json(
        &json!({ "added_junction": { "x": x, "y": y } }),
    ))
}

async fn handle_batch_add_junction(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let positions = match require_array(args, "positions") {
        Ok(a) => a.clone(),
        Err(e) => return Ok(e),
    };
    let mut sch = cse::Schematic::load(&sch_path)?;
    for pos in &positions {
        let x = pos["x"].as_f64().unwrap_or(0.0);
        let y = pos["y"].as_f64().unwrap_or(0.0);
        sch.add_junction(x, y);
    }
    sch.overwrite()?;
    Ok(CallToolResult::json(&json!({ "added": positions.len() })))
}

async fn handle_connect_to_net(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let direction = opt_str(args, "direction").unwrap_or("auto");
    let stub_length = opt_f64(args, "stub_length").unwrap_or(2.54);
    let label_type = opt_str(args, "label_type").unwrap_or("net_label");

    let (_, tree) = read_schematic(&sch_path)?;

    // Name the pin, or give its coordinates. Naming it cannot miss the
    // endpoint, and the error says which pin was wrong.
    let (pin_x, pin_y, outward) = match (opt_str(args, "reference"), opt_str(args, "pin_number")) {
        (Some(reference), Some(pin_number)) => {
            let instances = extract_symbol_instances(&tree);
            let lib_syms = tree
                .find("lib_symbols")
                .map(|n| n.find_all("symbol"))
                .unwrap_or_default();
            match resolve_placed_pin(&instances, &lib_syms, reference, pin_number) {
                Ok((pin, t)) => {
                    let (x, y) = pin_endpoint(&pin, t);
                    (x, y, Some(pin_outward_direction(&pin, t)))
                }
                Err(e) => return Ok(CallToolResult::error(e.to_string())),
            }
        }
        (Some(_), None) | (None, Some(_)) => {
            return Ok(CallToolResult::error(
                "'reference' and 'pin_number' must be given together",
            ))
        }
        (None, None) => match (require_f64(args, "pin_x"), require_f64(args, "pin_y")) {
            (Ok(x), Ok(y)) => (x, y, None),
            // Structured, not free text: a missing coordinate is an
            // InvalidArgument like any other, and the reason names the way in
            // that the caller did not take.
            (x, _) => {
                let field = if x.is_err() { "pin_x" } else { "pin_y" };
                return Ok(CallToolResult::error_kind(
                    crate::mcp::error::ToolErrorKind::InvalidArgument {
                        field: field.to_string(),
                        reason: "give either 'reference' + 'pin_number' or 'pin_x' + 'pin_y'"
                            .into(),
                    },
                    format!(
                        "Missing '{field}': give either 'reference' + 'pin_number' \
                         or 'pin_x' + 'pin_y'"
                    ),
                ));
            }
        },
    };

    // Where the stub goes, and how the label at its end must read: text
    // running back over the symbol covers its pin names (pin_label_rotation).
    let dir = match outward {
        Some(d) => crate::tools::stub_direction(direction, Some(d)),
        None => crate::tools::resolve_stub_direction(direction, (pin_x, pin_y), &tree),
    };
    let (label_x, label_y) = (pin_x + dir.dx * stub_length, pin_y + dir.dy * stub_length);
    let label_rot = dir.label_rotation;

    let mut sch = cse::Schematic::load(&sch_path)?;

    // T-junction detection for the wire stub
    let mut existing_wires = cse_wires_to_sexp(&sch);
    existing_wires.push(konnect_sexp::schematic::Wire {
        x1: pin_x,
        y1: pin_y,
        x2: label_x,
        y2: label_y,
        uuid: None,
    });
    let junctions = find_t_junctions(&existing_wires, 0.01);

    // Add wire stub
    sch.add_wire(pin_x, pin_y, label_x, label_y);
    add_missing_junctions(&mut sch, &junctions);
    // Pins the stub passes over mid-segment also need junction dots.
    let pins = crate::tools::all_pin_endpoints(&tree);
    add_missing_junctions(
        &mut sch,
        &pins_mid_segment(&pins, pin_x, pin_y, label_x, label_y),
    );

    // set_rotation, not `at.rotation = …`: a bare rotation leaves `effects`
    // unset, and KiCad then centres the text on the anchor (#43).
    match label_type {
        "global_label" => {
            sch.add_global_label(&net, "input", label_x, label_y);
            let idx = sch.global_labels.len() - 1;
            if let Some(gl) = sch.global_labels.get_mut(idx) {
                gl.set_rotation(label_rot);
            }
        }
        _ => {
            sch.add_label(&net, label_x, label_y)
                .set_rotation(label_rot);
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "connected": net,
        "direction": dir.name,
        "wire": { "x1": pin_x, "y1": pin_y, "x2": label_x, "y2": label_y },
        "label": { "x": label_x, "y": label_y, "rotation": label_rot }
    })))
}

async fn handle_connect_pins(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin1 = match require_str(args, "pin1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin2 = match require_str(args, "pin2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    // Parse the schematic tree
    let (content, tree) = read_schematic(&sch_path)?;
    let expected = content.clone();
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Resolve pin1 board-space endpoint
    let (x1, y1) = resolve_pin_endpoint(&instances, &lib_syms, &ref1, &pin1)?;
    // Resolve pin2 board-space endpoint
    let (x2, y2) = resolve_pin_endpoint(&instances, &lib_syms, &ref2, &pin2)?;

    // Route wire(s) between the two pin endpoints
    let new_content = route_between(content, x1, y1, x2, y2);

    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "connected": {
            "from": { "ref": ref1, "pin": pin1, "x": x1, "y": y1 },
            "to":   { "ref": ref2, "pin": pin2, "x": x2, "y": y2 }
        }
    })))
}

/// Resolve a pin's schematic-space endpoint by reference and pin number.
/// Uses the same pattern as sch_analysis::handle_get_pin_connections.
pub(crate) fn resolve_pin_endpoint(
    instances: &[konnect_sexp::schematic::SymbolInstance],
    lib_syms: &[&konnect_sexp::parser::SexpNode],
    reference: &str,
    pin_number: &str,
) -> anyhow::Result<(f64, f64)> {
    let (pin, t) = resolve_placed_pin(instances, lib_syms, reference, pin_number)?;
    Ok(pin_endpoint(&pin, t))
}

/// The named pin and the transform placing it, for callers that need more than
/// its coordinates — `pin_outward_direction` and `pin_label_rotation` both take
/// this pair, and deriving them here beats searching the sheet for the point we
/// just computed.
pub(crate) fn resolve_placed_pin(
    instances: &[konnect_sexp::schematic::SymbolInstance],
    lib_syms: &[&konnect_sexp::parser::SexpNode],
    reference: &str,
    pin_number: &str,
) -> anyhow::Result<(
    konnect_sexp::schematic::LibPin,
    konnect_sexp::geometry::PinTransform,
)> {
    // A multi-unit part places one instance per unit, all sharing the
    // reference, so any of them may own the pin: an LM2904's power pins live
    // on the unit the schematic draws separately from either amplifier.
    let placed: Vec<_> = instances
        .iter()
        .filter(|i| i.reference == reference)
        .collect();
    let Some(first) = placed.first() else {
        anyhow::bail!("Component '{}' not found", reference);
    };

    let mut searched = Vec::new();
    for inst in &placed {
        // find_lib_symbol, not a lib_id match: an instance carrying a
        // (lib_name …) is a sheet-local derived symbol whose pins can sit
        // elsewhere than the base definition's, or whose base was never
        // embedded at all (#143).
        let Some(lib_sym) = find_lib_symbol(lib_syms, inst) else {
            continue;
        };
        searched.push(inst.unit);
        // Unit-aware (#35): only the unit that owns the pin may answer for it,
        // or the wire lands on a superimposed phantom.
        if let Some(lib_pin) =
            konnect_sexp::schematic::extract_lib_pins_for_unit(lib_sym, inst.unit)
                .into_iter()
                .find(|p| p.number == pin_number)
        {
            return Ok((lib_pin, inst.pin_transform()));
        }
    }

    if searched.is_empty() {
        anyhow::bail!("Library symbol '{}' not found", first.lib_id);
    }
    anyhow::bail!(
        "Pin '{}' not found on '{}' ({} {})",
        pin_number,
        reference,
        if searched.len() == 1 { "unit" } else { "units" },
        searched
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

async fn handle_add_schematic_connection(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let content = route_between(content, x1, y1, x2, y2);

    write_atomic_if_unchanged(&sch_path, &expected, &content)?;
    Ok(CallToolResult::json(&json!({
        "connected": { "from": [x1, y1], "to": [x2, y2] }
    })))
}

#[cfg(test)]
mod unit_aware_wiring_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// A schematic with an embedded LM2904-style dual op-amp (unit 1 = pins
    /// 1-3, unit 2 = pins 5-7) placed twice: U1 as unit 1, U2 as unit 2.
    fn dual_opamp_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let pin = |num: &str, x: f64, y: f64, angle: u32| {
            format!(
                "\t\t\t(pin passive line (at {x} {y} {angle}) (length 2.54)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"{num}\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n"
            )
        };
        let lib_sym = format!(
            "\t\t(symbol \"Test:OP2\"\n\t\t\t(symbol \"OP2_1_1\"\n{}{}{}\t\t\t)\n\t\t\t(symbol \"OP2_2_1\"\n{}{}{}\t\t\t)\n\t\t)\n",
            pin("1", -7.62, 2.54, 0),
            pin("2", -7.62, -2.54, 0),
            pin("3", 7.62, 0.0, 180),
            pin("5", -7.62, 2.54, 0),
            pin("6", -7.62, -2.54, 0),
            pin("7", 7.62, 0.0, 180),
        );
        let inst = |reference: &str, unit: u32, x: f64, uuid: &str| {
            format!(
                "\t(symbol\n\t\t(lib_id \"Test:OP2\")\n\t\t(at {x} 80 0)\n\t\t(unit {unit})\n\t\t(uuid \"{uuid}\")\n\t\t(property \"Reference\" \"{reference}\"\n\t\t\t(at {x} 75 0)\n\t\t)\n\t)\n"
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dual.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"3af69a4c-1faa-40bd-91dc-c4fc245c4cbd\")\n\t(lib_symbols\n{}\t)\n{}{})\n",
                lib_sym,
                inst("U1", 1, 100.0, "aaaaaaaa-1111-1111-1111-111111111111"),
                inst("U2", 2, 150.0, "bbbbbbbb-2222-2222-2222-222222222222"),
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn connect_pins_uses_the_instance_unit() {
        let (_d, path) = dual_opamp_schematic();

        // U1 is unit 1: its pins are 1-3. U2 is unit 2: pins 5-7.
        let ok = handle_connect_pins(
            &json!({
                "schematic": path.display().to_string(),
                "ref1": "U1", "pin1": "1",
                "ref2": "U2", "pin2": "5"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(
            !ok.is_error,
            "unit-owned pins must connect: {:?}",
            ok.content
        );

        // Pin 5 belongs to unit 2 — asking for it on the unit-1 instance must
        // fail instead of wiring to a superimposed phantom position (#35).
        let err = handle_connect_pins(
            &json!({
                "schematic": path.display().to_string(),
                "ref1": "U1", "pin1": "5",
                "ref2": "U2", "pin2": "6"
            }),
            &test_ctx(),
        )
        .await;
        let msg = format!("{:?}", err);
        assert!(
            err.is_err() || err.as_ref().is_ok_and(|r| r.is_error),
            "pin 5 on a unit-1 instance must not resolve: {msg}"
        );
        assert!(
            msg.contains("unit 1"),
            "error should name the instance unit: {msg}"
        );
    }

    /// U1 has a single pin at (101.6, 76.2) — on the 1.27 grid so add_wire's
    /// snapping keeps the new wire exactly through it.
    fn single_pin_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pin.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Test:P1\"\n\t\t\t(symbol \"P1_1_1\"\n\t\t\t\t(pin passive line (at 0 0 0) (length 2.54)\n\t\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at 101.6 76.2 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 101.6 71.12 0)\n\t\t)\n\t)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n",
        )
        .unwrap();
        (dir, path)
    }

    /// Drawing a wire across an existing pin mid-segment must auto-insert a
    /// junction dot — KiCad connects a mid-wire pin only through a junction.
    #[tokio::test]
    async fn add_wire_over_pin_inserts_junction() {
        let (_d, path) = single_pin_schematic();
        let result = handle_add_wire(
            &json!({
                "schematic": path.display().to_string(),
                "x1": 96.52, "y1": 76.2, "x2": 106.68, "y2": 76.2
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        let after = std::fs::read_to_string(&path).unwrap();
        let tree = konnect_sexp::parse_sexp(&after).unwrap();
        let juncs = konnect_sexp::schematic::extract_junctions(&tree);
        assert!(
            juncs
                .iter()
                .any(|&(x, y)| (x - 101.6).abs() < 0.01 && (y - 76.2).abs() < 0.01),
            "junction expected at the mid-wire pin, got {juncs:?}"
        );
    }

    // ─── One junction per T, however many wires arrive ─────────────────────

    /// An empty sheet: these tests only need somewhere to put wires.
    fn bare_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bare.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n",
        )
        .unwrap();
        (dir, path)
    }

    /// A rail plus three taps hanging off it: each tap makes one T.
    fn rail_and_taps() -> (Vec<serde_json::Value>, Vec<(f64, f64)>) {
        let rail = json!({ "x1": 101.6, "y1": 101.6, "x2": 127.0, "y2": 101.6 });
        let taps: Vec<f64> = vec![106.68, 111.76, 116.84];
        let wires = std::iter::once(rail)
            .chain(
                taps.iter()
                    .map(|&x| json!({ "x1": x, "y1": 101.6, "x2": x, "y2": 106.68 })),
            )
            .collect();
        (wires, taps.into_iter().map(|x| (x, 101.6)).collect())
    }

    fn junctions_at(path: &std::path::Path, x: f64, y: f64) -> usize {
        let tree = konnect_sexp::parse_sexp(&std::fs::read_to_string(path).unwrap()).unwrap();
        konnect_sexp::schematic::extract_junctions(&tree)
            .iter()
            .filter(|&&(jx, jy)| (jx - x).abs() < 0.01 && (jy - y).abs() < 0.01)
            .count()
    }

    /// `find_t_junctions` reports every T on the sheet, so each call used to
    /// re-emit a dot at every T already there — five wires left five dots
    /// stacked on one point.
    #[tokio::test]
    async fn repeated_add_wire_leaves_one_junction_per_t() {
        let (_d, path) = bare_schematic();
        let (wires, tees) = rail_and_taps();
        for w in &wires {
            let mut args = json!({ "schematic": path.display().to_string() });
            for (k, v) in w.as_object().unwrap() {
                args[k] = v.clone();
            }
            let result = handle_add_wire(&args, &test_ctx()).await.unwrap();
            assert!(!result.is_error, "{:?}", result.content);
        }
        for (x, y) in tees {
            assert_eq!(junctions_at(&path, x, y), 1, "T at ({x}, {y})");
        }
    }

    /// The same in one batch, where the duplication was quadratic.
    #[tokio::test]
    async fn batch_add_wire_leaves_one_junction_per_t() {
        let (_d, path) = bare_schematic();
        let (wires, tees) = rail_and_taps();
        let result = handle_batch_add_wire(
            &json!({ "schematic": path.display().to_string(), "wires": wires }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        for (x, y) in tees {
            assert_eq!(junctions_at(&path, x, y), 1, "T at ({x}, {y})");
        }
    }

    /// Fixture provenance: built with Konnect tools against the stock `Device:R`,
    /// then rewritten by `kicad-cli sch upgrade` so the committed text is
    /// eeschema's own output — it carries `ki_fp_filters`, `embedded_fonts`,
    /// `exclude_from_sim` and full pin geometry that a hand-written sheet does
    /// not (CONTRIBUTING.md). One sheet holds every case at a distinct
    /// coordinate, so each test also proves the pass leaves the others alone.
    ///
    ///   (120.65, 139.7)   R1's pin mid-span on a lone wire, dot earned
    ///   (120.65, 170.18)  a real T of two wires, dot always justified
    ///   (190.5,  140.0)   two wires merely crossing, no dot
    ///   (190.5,  200.66)  R3's pin mid-span but flagged no-connect, no dot
    ///   (260.35, 140.0)   a bus tee with its dot
    const RECONCILE_SCH: &str = include_str!("../../tests/fixtures/junction_reconcile.kicad_sch");

    fn dots(src: &str) -> Vec<(String, String)> {
        regex_lite_junctions(src)
    }

    /// Junction coordinates, as written.
    fn regex_lite_junctions(src: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for chunk in src.split("(junction").skip(1) {
            if let Some(at) = chunk.split("(at ").nth(1) {
                if let Some(inner) = at.split(')').next() {
                    let mut it = inner.split_whitespace();
                    if let (Some(x), Some(y)) = (it.next(), it.next()) {
                        out.push((x.to_string(), y.to_string()));
                    }
                }
            }
        }
        out
    }

    fn has_dot(src: &str, x: &str, y: &str) -> bool {
        dots(src).iter().any(|(a, b)| a == x && b == y)
    }

    /// R1's pin still sits at (120.65, 139.7), so its dot is justified and must
    /// survive being asked about. The pruning case needs the pin to actually
    /// leave, which is a move — covered end-to-end in sch_batch against this
    /// same fixture.
    #[test]
    fn reconcile_keeps_a_dot_its_pin_still_justifies() {
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(120.65, 139.7)]);
        assert_eq!((added, pruned), (0, 0), "the pin is still there");
        assert!(has_dot(&out, "120.65", "139.7"));
    }

    /// A hierarchical sheet pin justifies a dot exactly as a symbol pin does —
    /// the `|| idx.has_sheet_pin(..)` half of the keep rule, which nothing else
    /// reaches. Candidate points come from moved *symbol* pins, so the branch
    /// only decides anything when a symbol pin vacates a point a sheet pin also
    /// holds: in this fixture R1's pin, `test`'s SHPIN and the dot all sit at
    /// (139.7, 190.5) on a wire's interior. Ask about it with R1 gone — the
    /// post-move state — and the dot must stay.
    ///
    /// Its own fixture rather than an addition to `junction_reconcile`, so the
    /// tests already merged against that sheet keep the inputs they were written
    /// for. Built with Konnect tools, then rewritten by `kicad-cli sch upgrade`
    /// so the committed text is eeschema's own — including the sheet-pin
    /// placement, which KiCad snaps to the sheet border.
    #[test]
    fn reconcile_keeps_a_dot_a_sheet_pin_still_justifies() {
        const SHEET_PIN_SCH: &str =
            include_str!("../../tests/fixtures/junction_sheet_pin.kicad_sch");
        let without_r1 = fixture_without_symbol(SHEET_PIN_SCH, "R1");

        let (out, added, pruned) = reconcile_junctions_at(without_r1, &[(139.7, 190.5)]);
        assert_eq!(
            (added, pruned),
            (0, 0),
            "the sheet pin still justifies the dot"
        );
        assert!(
            has_dot(&out, "139.7", "190.5"),
            "the dot must survive: {out}"
        );
    }

    /// The fixture with one placed symbol removed, so its pins vanish — the
    /// post-move state without needing a move.
    ///
    /// Guarded, because a silent failure here would be invisible:
    /// `reconcile_junctions_at` returns its input unchanged when the sheet does
    /// not parse, which is exactly what a passing sheet-pin test looks like. If
    /// this ever stops removing what it should, the assertions below fail loudly
    /// instead of the test going quietly green.
    fn fixture_without_symbol(src: &str, reference: &str) -> String {
        let needle = format!("(property \"Reference\" \"{reference}\"");
        let at = src.find(&needle).expect("fixture carries that reference");
        let start = src[..at]
            .rfind("(symbol")
            .expect("reference sits inside a symbol");
        let mut depth = 0usize;
        let mut end = start;
        for (i, c) in src[start..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(end > start, "never closed the symbol block");
        let out = format!("{}{}", &src[..start], &src[end..]);
        assert!(!out.contains(&needle), "the symbol is still there");
        assert_eq!(
            out.matches('(').count(),
            out.matches(')').count(),
            "removal left unbalanced parens, so the sheet would not parse"
        );
        out
    }

    /// A real T — two wires meeting — is never touched.
    #[test]
    fn reconcile_keeps_a_real_tee() {
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(120.65, 170.18)]);
        assert_eq!((added, pruned), (0, 0), "a T stays");
        assert!(has_dot(&out, "120.65", "170.18"));
    }

    /// Two wires that merely CROSS are separate nets in KiCad until a junction
    /// says otherwise. A pin landing there must NOT get one: that would merge
    /// two nets, the silent connectivity change #120 exists to stop.
    #[test]
    fn a_pin_landing_on_a_crossing_does_not_merge_the_nets() {
        let before = dots(RECONCILE_SCH).len();
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(190.5, 140.0)]);
        assert_eq!(
            (added, pruned),
            (0, 0),
            "an ambiguous crossing is left alone"
        );
        assert_eq!(dots(&out).len(), before, "no dot invented at the crossing");
    }

    /// A no-connect flag is the user saying "this pin stays unconnected" — a
    /// move landing it mid-span must not wire it up with a dot.
    #[test]
    fn reconcile_does_not_add_a_dot_over_a_no_connect() {
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(190.5, 200.66)]);
        assert_eq!((added, pruned), (0, 0), "a no-connect forbids the dot");
        assert!(!has_dot(&out, "190.5", "200.66"));
    }

    /// The fixture with R1's justified dot removed — the sheet as it would
    /// look after that dot was lost, so the ADD branch has real work.
    fn fixture_missing_r1s_dot() -> String {
        let src = RECONCILE_SCH;
        let at = src
            .find("(at 120.65 139.7)")
            .expect("fixture carries R1's dot");
        let start = src[..at].rfind("(junction").expect("inside a junction");
        let mut depth = 0usize;
        let mut end = start;
        for (offset, byte) in src[start..].bytes().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + offset + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let stripped = format!("{}{}", &src[..start], &src[end..]);
        assert!(!has_dot(&stripped, "120.65", "139.7"), "dot removed");
        stripped
    }

    /// The positive half of the reconcile: a pin sitting mid-span on exactly
    /// one wire with no dot gets one, and float noise in the candidate
    /// coordinate never reaches the file.
    #[test]
    fn a_pin_left_mid_wire_gets_its_dot_back() {
        let (out, added, pruned) =
            reconcile_junctions_at(fixture_missing_r1s_dot(), &[(120.65, 139.70000000000002)]);
        assert_eq!((added, pruned), (1, 0), "the missing dot comes back");
        assert!(
            has_dot(&out, "120.65", "139.7"),
            "rounded, not 139.70000000000002"
        );
        assert!(
            !out.contains("139.70000000000002"),
            "no float noise in the file"
        );
    }

    /// Two coincident pin endpoints vacating or landing on one spot hand the
    /// reconciler the same candidate twice. The counts must describe what
    /// happened to the sheet, not how many times we asked — and the sheet
    /// must not gain two identical dots.
    #[test]
    fn duplicate_candidates_produce_one_dot_and_honest_counts() {
        let (out, added, _pruned) = reconcile_junctions_at(
            fixture_missing_r1s_dot(),
            &[(120.65, 139.7), (120.65, 139.7)],
        );
        assert_eq!(added, 1, "one dot added, however many times we were asked");
        let dots_there = dots(&out)
            .iter()
            .filter(|(x, y)| x == "120.65" && y == "139.7")
            .count();
        assert_eq!(dots_there, 1, "exactly one junction block written");
    }

    /// A dot on a bus tee is a BUS junction — it joins bus segments, which no
    /// wire count can see. Bus points are outside this pass's jurisdiction.
    #[test]
    fn reconcile_never_touches_a_dot_on_a_bus() {
        let (out, added, pruned) =
            reconcile_junctions_at(RECONCILE_SCH.to_string(), &[(260.35, 140.0)]);
        assert_eq!(
            (added, pruned),
            (0, 0),
            "bus junctions are not ours to judge"
        );
        assert!(has_dot(&out, "260.35", "140"), "the bus tee keeps its dot");
    }

    /// Regression for #234: a malformed element used to default its missing
    /// coordinate to 0 and land a real wire across the sheet, while the
    /// single-wire tool refused the same omission.
    #[tokio::test]
    async fn batch_add_wire_refuses_a_malformed_element_and_writes_nothing() {
        let (_d, path) = bare_schematic();
        let before = std::fs::read_to_string(&path).unwrap();

        let result = handle_batch_add_wire(
            &json!({
                "schematic": path.display().to_string(),
                "wires": [
                    { "x1": 150.0, "y1": 150.0, "x2": 160.0, "y2": 150.0 },
                    { "x1": 170.0, "y1": 170.0, "y2": 170.0 }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error, "{:?}", result.content);
        let msg = format!("{:?}", result.content);
        assert!(
            msg.contains("wires[1].x2"),
            "must name the element and field: {msg}"
        );
        // The valid first element must not have been written either — the
        // batch fails atomically.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    // ─── connect_to_net stub orientation ───────────────────────────────────

    async fn connect_to_net(path: &std::path::Path, args: serde_json::Value) -> String {
        let mut full = json!({ "schematic": path.display().to_string() });
        for (k, v) in args.as_object().unwrap() {
            full[k] = v.clone();
        }
        let result = handle_connect_to_net(&full, &test_ctx()).await.unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        std::fs::read_to_string(path).unwrap()
    }

    /// U1's pins 1 and 2 face west, so the stub must leave westward and its
    /// label read right-to-left, clear of the body.
    #[tokio::test]
    async fn auto_routes_the_stub_away_from_the_symbol_body() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "IN" }),
        )
        .await;
        // Pin 1 tip is (92.38, 77.46); the stub runs 2.54 mm further west.
        assert!(after.contains("(at 89.84 77.46 180)"), "{after}");
        // An east-facing pin on the same part goes the other way.
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "3", "net": "OUT" }),
        )
        .await;
        assert!(after.contains("(at 110.16 80 0)"), "{after}");
    }

    /// Labels here used to carry no `(effects)` at all, so KiCad centred the
    /// text on the anchor — the defect #43 fixed for add_schematic_net_label.
    #[tokio::test]
    async fn the_label_carries_a_justify_matching_its_rotation() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "IN" }),
        )
        .await;
        assert!(
            after.contains("(justify right"),
            "a 180° label must be right-justified: {after}"
        );
    }

    /// An explicit direction still wins over the derived one.
    #[tokio::test]
    async fn an_explicit_direction_overrides_the_derived_one() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "IN",
                    "direction": "right" }),
        )
        .await;
        assert!(after.contains("(at 94.92 77.46 0)"), "{after}");
    }

    /// A vertical stub keeps its text horizontal: of 2562 wire-anchored labels
    /// in the KiCad demos, only ~1% are rotated 90 or 270.
    #[tokio::test]
    async fn a_vertical_stub_keeps_its_label_horizontal() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "IN",
                    "direction": "down" }),
        )
        .await;
        assert!(after.contains("(at 92.38 80 0)"), "{after}");
    }

    /// Coordinates still work, and a bare point falls back to the old default.
    #[tokio::test]
    async fn coordinates_still_work_and_a_bare_point_goes_right() {
        let (_d, path) = dual_opamp_schematic();
        let after = connect_to_net(
            &path,
            json!({ "pin_x": 50.0, "pin_y": 50.0, "net": "FREE" }),
        )
        .await;
        assert!(after.contains("(at 52.54 50 0)"), "{after}");
    }

    /// [`single_pin_schematic`] with a second part butted onto U1's pin: both
    /// tips sit at (101.6, 76.2) and face opposite ways.
    fn butted_pins_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, path) = single_pin_schematic();
        let content = std::fs::read_to_string(&path).unwrap();
        let u2 = "\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at 101.6 76.2 180)\n\t\t(unit 1)\n\t\t(uuid \"u2\")\n\t\t(property \"Reference\" \"U2\"\n\t\t\t(at 101.6 71.12 0)\n\t\t)\n\t)\n";
        let at = content.find("\t(sheet_instances").unwrap();
        std::fs::write(&path, format!("{}{u2}{}", &content[..at], &content[at..])).unwrap();
        (dir, path)
    }

    /// Naming a pin carries its own direction. Deriving one from a coordinate
    /// cannot: two pins share this point and disagree about which way is out,
    /// so that path has to fall back to "right".
    #[tokio::test]
    async fn a_named_pin_outranks_the_coordinate_lookup_where_pins_stack() {
        let (_d, path) = butted_pins_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "1", "net": "SHARED" }),
        )
        .await;
        assert!(after.contains("(at 99.06 76.2 180)"), "{after}");

        let (_d, path) = butted_pins_schematic();
        let after = connect_to_net(
            &path,
            json!({ "pin_x": 101.6, "pin_y": 76.2, "net": "SHARED" }),
        )
        .await;
        assert!(after.contains("(at 104.14 76.2 0)"), "{after}");
    }

    /// [`dual_opamp_schematic`] with both units placed under one reference.
    /// KiCad gives every unit its own `(symbol …)` node sharing the Reference
    /// property — 26 of the KiCad 10 demo schematics do this.
    fn multi_unit_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, path) = dual_opamp_schematic();
        let content = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"U2\"", "\"U1\"");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// Pin 5 belongs to unit 2, so resolving it means searching every unit
    /// placed under the reference. Stopping at the first would make an
    /// op-amp's power pins unreachable by name.
    #[tokio::test]
    async fn a_pin_on_another_unit_of_the_same_reference_resolves() {
        let (_d, path) = multi_unit_schematic();
        let after = connect_to_net(
            &path,
            json!({ "reference": "U1", "pin_number": "5", "net": "PWR" }),
        )
        .await;
        // Unit 2 sits at x=150, putting pin 5's tip at (142.38, 77.46).
        assert!(after.contains("(at 139.84 77.46 180)"), "{after}");
    }

    /// A pin on no placed unit still fails, and the error names every unit
    /// that was searched rather than only the first.
    #[tokio::test]
    async fn a_pin_on_no_placed_unit_names_the_units_searched() {
        let (_d, path) = multi_unit_schematic();
        let result = handle_connect_to_net(
            &json!({ "schematic": path.display().to_string(),
                     "reference": "U1", "pin_number": "99", "net": "NOPE" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{:?}", result.content);
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("units 1, 2"), "{text}");
    }

    #[tokio::test]
    async fn naming_only_half_a_pin_is_an_error() {
        let (_d, path) = dual_opamp_schematic();
        let result = handle_connect_to_net(
            &json!({ "schematic": path.display().to_string(),
                     "reference": "U1", "net": "IN" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{:?}", result.content);
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn sch_with(labels: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("labels.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"3af69a4c-1faa-40bd-91dc-c4fc245c4cbd\")\n  (paper \"A4\")\n  (lib_symbols\n  )\n{labels}\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    async fn delete(path: &std::path::Path, net: &str, x: f64, y: f64) -> CallToolResult {
        handle_delete_net_label(
            &json!({ "schematic": path.display().to_string(), "net": net, "x": x, "y": y }),
            &test_ctx(),
        )
        .await
        .unwrap()
    }

    const TWO_PLAIN: &str = "  (label \"VCC\"\n    (at 100 100 0)\n    (uuid \"11111111-1111-1111-1111-111111111111\")\n  )\n  (label \"VCC\"\n    (at 200 100 0)\n    (uuid \"22222222-2222-2222-2222-222222222222\")\n  )";

    #[tokio::test]
    async fn deletes_the_plain_label_the_add_tool_writes() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let result = delete(&path, "VCC", 200.0, 100.0).await;
        assert!(!result.is_error, "plain (label) blocks must be deletable");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("(at 100 100 0)"),
            "the label at (100,100) must survive"
        );
        assert!(
            !after.contains("(at 200 100 0)"),
            "the targeted label at (200,100) must be gone"
        );
    }

    #[tokio::test]
    async fn wrong_coordinates_delete_nothing_and_report_the_real_positions() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let before = std::fs::read_to_string(&path).unwrap();

        let result = delete(&path, "VCC", 300.0, 300.0).await;
        assert!(result.is_error, "a miss must not fall back to nearest-wins");

        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected a text result");
        };
        assert!(
            text.contains("100") && text.contains("200"),
            "error should list the actual label positions: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "file must be untouched when nothing matched"
        );
    }

    #[tokio::test]
    async fn same_name_label_of_another_kind_elsewhere_is_not_collateral() {
        // The old backwards-scan could walk from any occurrence of the quoted
        // name to an unrelated block and delete that instead.
        let (_d, path) = sch_with(
            "  (global_label \"VBUS\"\n    (shape input)\n    (at 50 50 0)\n    (uuid \"33333333-3333-3333-3333-333333333333\")\n  )\n  (label \"VBUS\"\n    (at 150 150 0)\n    (uuid \"44444444-4444-4444-4444-444444444444\")\n  )",
        );

        let result = delete(&path, "VBUS", 150.0, 150.0).await;
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("(global_label \"VBUS\""),
            "the global label at a different position must survive"
        );
        assert!(!after.contains("(at 150 150 0)"));
    }

    #[tokio::test]
    async fn global_and_hierarchical_labels_are_deletable_by_exact_position() {
        for (kind, block) in [
            (
                "global_label",
                "  (global_label \"NET\"\n    (shape input)\n    (at 10 20 0)\n    (uuid \"55555555-5555-5555-5555-555555555555\")\n  )",
            ),
            (
                "hierarchical_label",
                "  (hierarchical_label \"NET\"\n    (shape input)\n    (at 10 20 0)\n    (uuid \"66666666-6666-6666-6666-666666666666\")\n  )",
            ),
        ] {
            let (_d, path) = sch_with(block);
            let result = delete(&path, "NET", 10.0, 20.0).await;
            assert!(!result.is_error, "{kind} should be deletable");
            assert!(!std::fs::read_to_string(&path).unwrap().contains(kind));
        }
    }

    #[tokio::test]
    async fn a_net_name_appearing_in_a_property_does_not_confuse_the_match() {
        // "VCC" also occurs as a symbol property value; only the real label
        // block at the requested position may be deleted.
        let (_d, path) = sch_with(
            "  (symbol\n    (lib_id \"Device:R\")\n    (at 60 60 0)\n    (property \"Value\" \"VCC\"\n      (at 60 62 0)\n    )\n  )\n  (label \"VCC\"\n    (at 100 100 0)\n    (uuid \"77777777-7777-7777-7777-777777777777\")\n  )",
        );

        let result = delete(&path, "VCC", 100.0, 100.0).await;
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("(property \"Value\" \"VCC\""),
            "the symbol property must be untouched"
        );
        assert!(!after.contains("(label \"VCC\""));
    }

    #[tokio::test]
    async fn unknown_net_name_is_an_error() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let result = delete(&path, "NOPE", 100.0, 100.0).await;
        assert!(result.is_error);
    }

    // ─── justify / rotation ────────────────────────────────────────────────

    async fn rotate(path: &std::path::Path, net: &str, x: f64, y: f64, rot: f64) -> CallToolResult {
        handle_rotate_label(
            &json!({ "schematic": path.display().to_string(), "net": net,
                     "x": x, "y": y, "rotation": rot }),
            &test_ctx(),
        )
        .await
        .unwrap()
    }

    fn justify_of(body: &str, net: &str) -> String {
        let start = body.find(&format!("\"{net}\"")).expect("label present");
        let block = &body[start..];
        let end = block.find("(uuid").unwrap_or(block.len());
        match block[..end].find("(justify ") {
            Some(j) => {
                let rest = &block[..end][j + "(justify ".len()..];
                rest[..rest.find(')').unwrap()].trim().to_string()
            }
            None => "<none>".to_string(),
        }
    }

    #[tokio::test]
    async fn rotate_creates_the_effects_block_when_absent() {
        // The shape add_schematic_net_label used to write: no (effects) at all.
        let (_d, path) = sch_with(
            "  (global_label \"EN\"\n    (shape input)\n    (at 10 20 0)\n    (uuid \"88888888-8888-8888-8888-888888888888\")\n  )",
        );
        let result = rotate(&path, "EN", 10.0, 20.0, 180.0).await;
        assert!(!result.is_error);

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("(at 10 20 180)"), "anchor must rotate");
        assert_eq!(
            justify_of(&body, "EN"),
            "right",
            "a 180° label must be right-justified or its text renders backwards"
        );
    }

    #[tokio::test]
    async fn rotate_replaces_an_existing_justify_and_keeps_the_font() {
        let (_d, path) = sch_with(
            "  (global_label \"EN\"\n    (shape input)\n    (at 10 20 0)\n    (effects (font (size 2.54 2.54)) (justify left))\n    (uuid \"99999999-9999-9999-9999-999999999999\")\n  )",
        );
        assert!(!rotate(&path, "EN", 10.0, 20.0, 180.0).await.is_error);

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(justify_of(&body, "EN"), "right");
        assert!(
            body.contains("(size 2.54 2.54)"),
            "the file's own font must be preserved"
        );
        assert_eq!(body.matches("(justify").count(), 1, "no duplicate justify");
    }

    #[tokio::test]
    async fn rotate_adds_justify_to_an_effects_block_that_lacks_one() {
        let (_d, path) = sch_with(
            "  (global_label \"EN\"\n    (shape input)\n    (at 10 20 0)\n    (effects (font (size 1.27 1.27)))\n    (uuid \"aaaaaaaa-9999-9999-9999-999999999999\")\n  )",
        );
        assert!(!rotate(&path, "EN", 10.0, 20.0, 270.0).await.is_error);
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(justify_of(&body, "EN"), "right", "270° is right-justified");
        assert_eq!(body.matches("(effects").count(), 1);
    }

    #[tokio::test]
    async fn rotating_back_to_zero_restores_left() {
        let (_d, path) = sch_with(
            "  (global_label \"EN\"\n    (shape input)\n    (at 10 20 180)\n    (effects (font (size 1.27 1.27)) (justify right))\n    (uuid \"bbbbbbbb-9999-9999-9999-999999999999\")\n  )",
        );
        assert!(!rotate(&path, "EN", 10.0, 20.0, 0.0).await.is_error);
        assert_eq!(
            justify_of(&std::fs::read_to_string(&path).unwrap(), "EN"),
            "left"
        );
    }

    #[tokio::test]
    async fn plain_labels_keep_the_bottom_alignment_eeschema_writes() {
        let (_d, path) = sch_with(
            "  (label \"MID\"\n    (at 10 20 0)\n    (uuid \"cccccccc-9999-9999-9999-999999999999\")\n  )",
        );
        assert!(!rotate(&path, "MID", 10.0, 20.0, 180.0).await.is_error);
        assert_eq!(
            justify_of(&std::fs::read_to_string(&path).unwrap(), "MID"),
            "right bottom"
        );
    }

    #[tokio::test]
    async fn rotate_reports_real_positions_when_coordinates_miss() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let result = rotate(&path, "VCC", 555.0, 555.0, 180.0).await;
        assert!(result.is_error, "must not rotate the nearest label instead");
    }

    // ─── move by offset ────────────────────────────────────────────────────

    #[tokio::test]
    async fn move_labels_by_offset_actually_moves_every_matching_label() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let result = handle_move_labels_by_offset(
            &json!({ "schematic": path.display().to_string(), "net": "VCC", "dx": 2.54, "dy": -1.27 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("(at 102.54 98.73 0)"),
            "first label moved: {after}"
        );
        assert!(
            after.contains("(at 202.54 98.73 0)"),
            "second label moved: {after}"
        );
    }

    #[tokio::test]
    async fn move_labels_by_offset_errors_on_unknown_net() {
        let (_d, path) = sch_with(TWO_PLAIN);
        let before = std::fs::read_to_string(&path).unwrap();
        let result = handle_move_labels_by_offset(
            &json!({ "schematic": path.display().to_string(), "net": "NOPE", "dx": 1.0, "dy": 1.0 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "zero matches must not report success");
        assert_eq!(before, std::fs::read_to_string(&path).unwrap());
    }
}

#[cfg(test)]
mod wire_delete_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    const WIRE_1: &str = "11111111-1111-1111-1111-111111111111";
    const WIRE_2: &str = "22222222-2222-2222-2222-222222222222";

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn tab_indented_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire-delete.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(generator_version \"10.0\")\n\t(uuid \"00000000-0000-0000-0000-000000000001\")\n\t(paper \"A4\")\n\t(wire\n\t\t(pts\n\t\t\t(xy 50.8 50.8) (xy 60.96 50.8)\n\t\t)\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{WIRE_1}\")\n\t)\n\t(wire\n\t\t(pts\n\t\t\t(xy 50.8 60.96) (xy 60.96 60.96)\n\t\t)\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{WIRE_2}\")\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn delete_wire_preserves_tab_indented_schematic_and_neighbors() {
        let (_dir, path) = tab_indented_schematic();
        let result = handle_delete_wire(
            &json!({ "schematic": path.display().to_string(), "uuid": WIRE_1 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(WIRE_1));
        assert!(after.contains(WIRE_2));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn delete_wire_matches_reversed_endpoint_coordinates() {
        let (_dir, path) = tab_indented_schematic();
        let result = handle_delete_wire(
            &json!({
                "schematic": path.display().to_string(),
                "x1": 60.96,
                "y1": 50.8,
                "x2": 50.8,
                "y2": 50.8
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(WIRE_1));
        assert!(after.contains(WIRE_2));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn batch_delete_wire_handles_tabs_and_duplicate_requests() {
        let (_dir, path) = tab_indented_schematic();
        let result = handle_batch_delete_wire(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [WIRE_1, WIRE_1, WIRE_2]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(WIRE_1));
        assert!(!after.contains(WIRE_2));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn batch_delete_wire_fails_closed_when_nothing_matches() {
        let (_dir, path) = tab_indented_schematic();
        let before = std::fs::read_to_string(&path).unwrap();
        let result = handle_batch_delete_wire(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": ["ffffffff-ffff-ffff-ffff-ffffffffffff"]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    // ─── Deleting a wire takes its junction dots with it ───────────────────

    /// The same two wires the fixtures above use, named for their role here.
    const RAIL: &str = WIRE_1;
    const TAP: &str = WIRE_2;

    /// A rail with one tap onto it, the dot at the T, and a second dot far
    /// away that no delete below touches.
    fn t_junction_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junction-prune.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols)\n\t(wire\n\t\t(pts (xy 50.8 50.8) (xy 76.2 50.8))\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{RAIL}\")\n\t)\n\t(wire\n\t\t(pts (xy 63.5 50.8) (xy 63.5 63.5))\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{TAP}\")\n\t)\n\t(junction (at 63.5 50.8) (diameter 0) (color 0 0 0 0) (uuid \"aaaa0000-0000-0000-0000-000000000001\"))\n\t(junction (at 25.4 25.4) (diameter 0) (color 0 0 0 0) (uuid \"aaaa0000-0000-0000-0000-000000000002\"))\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    fn junctions_in(path: &std::path::Path) -> Vec<(f64, f64)> {
        let content = std::fs::read_to_string(path).unwrap();
        let tree = konnect_sexp::parse_sexp(&content).unwrap();
        konnect_sexp::schematic::extract_junctions(&tree)
    }

    #[tokio::test]
    async fn deleting_every_wire_through_a_junction_removes_the_dot() {
        let (_dir, path) = t_junction_schematic();
        let result = handle_batch_delete_wire(
            &json!({ "schematic": path.display().to_string(), "uuids": [RAIL, TAP] }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        // The far dot is on no deleted wire, so it stays whatever justifies it.
        assert_eq!(junctions_in(&path), vec![(25.4, 25.4)]);
    }

    #[tokio::test]
    async fn deleting_the_tap_removes_the_dot_left_on_the_rail() {
        let (_dir, path) = t_junction_schematic();
        let result = handle_delete_wire(
            &json!({ "schematic": path.display().to_string(), "uuid": TAP }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains(RAIL), "the rail must survive: {after}");
        assert_eq!(junctions_in(&path), vec![(25.4, 25.4)]);
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn a_dot_on_a_mid_segment_pin_survives_losing_its_second_wire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pin-junction.kicad_sch");
        // U1's pin tip is at (101.6, 76.2): a rail straight through it, a stub
        // arriving from below, and the dot the T needs.
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Test:P1\"\n\t\t\t(symbol \"P1_1_1\"\n\t\t\t\t(pin passive line (at 0 0 0) (length 2.54)\n\t\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at 101.6 76.2 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 101.6 71.12 0)\n\t\t)\n\t)\n\t(wire\n\t\t(pts (xy 96.52 76.2) (xy 106.68 76.2))\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{RAIL}\")\n\t)\n\t(wire\n\t\t(pts (xy 101.6 76.2) (xy 101.6 68.58))\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{TAP}\")\n\t)\n\t(junction (at 101.6 76.2) (diameter 0) (color 0 0 0 0) (uuid \"aaaa0000-0000-0000-0000-000000000003\"))\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();

        let result = handle_delete_wire(
            &json!({ "schematic": path.display().to_string(), "uuid": TAP }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        // One wire left through the dot, but the pin lands mid-segment there —
        // that is the connection, and pruning it would silently break the net.
        assert_eq!(junctions_in(&path), vec![(101.6, 76.2)]);
    }

    #[tokio::test]
    async fn split_wire_keeps_the_dots_on_the_wire_it_replaces() {
        let (_dir, path) = t_junction_schematic();
        let result = handle_split_wire_at_point(
            &json!({ "schematic": path.display().to_string(), "x": 68.58, "y": 50.8 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let mut juncs = junctions_in(&path);
        juncs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(juncs, vec![(25.4, 25.4), (63.5, 50.8), (68.58, 50.8)]);
    }

    #[tokio::test]
    async fn split_wire_without_uuid_deletes_by_complete_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire-split.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(wire\n\t\t(pts (xy 0 0) (xy 10 0))\n\t\t(stroke (width 0) (type default))\n\t)\n)\n",
        )
        .unwrap();

        let result = handle_split_wire_at_point(
            &json!({
                "schematic": path.display().to_string(),
                "x": 5.0,
                "y": 0.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        let parsed = konnect_sexp::parse_sexp(&after).unwrap();
        let wires = extract_wires(&parsed);
        assert_eq!(wires.len(), 2);
        assert!(after.contains("(junction"));
        assert!(!wires.iter().any(|wire| wire.x1 == 0.0 && wire.x2 == 10.0));
    }
}

#[cfg(test)]
mod power_symbol_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    #[tokio::test]
    async fn add_power_symbol_places_hidden_reference_near_the_symbol() {
        // Pre-seed lib_symbols so ensure_lib_symbol succeeds without a KiCad install.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("power.kicad_sch");
        std::fs::write(
            &path,
            // Anchors copied from KiCad 10's power.kicad_sym: GND points down,
            // so both fields anchor below the origin in Y-up library space.
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:GND\"\n      (property \"Reference\" \"#PWR\" (at 0 -6.35 0))\n      (property \"Value\" \"GND\" (at 0 -3.81 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        let result = handle_add_power_symbol(
            &json!({
                "schematic": path.display().to_string(),
                "power_net": "GND",
                "x": 100.0,
                "y": 80.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch
            .symbols
            .iter()
            .find(|s| s.reference() == Some("#PWR001"))
            .expect("power symbol instance");
        let ref_prop = sym
            .properties
            .iter()
            .find(|p| p.name == "Reference")
            .unwrap();
        let ref_sexp = cse::sexp::writer::write(&ref_prop.to_sexp());
        assert!(
            ref_sexp.contains("(at 100") && ref_sexp.contains("86.35"),
            "Reference must sit at the library's anchor near the symbol, not \
             sheet origin: {ref_sexp}"
        );
        let hide_at = ref_sexp
            .find("(hide yes)")
            .expect("KiCad 10 property-level hide");
        let effects_at = ref_sexp.find("(effects").expect("effects");
        assert!(
            hide_at < effects_at,
            "hide must be a property sibling before effects (not inside effects): {ref_sexp}"
        );
        let val_prop = sym.properties.iter().find(|p| p.name == "Value").unwrap();
        let val_sexp = cse::sexp::writer::write(&val_prop.to_sexp());
        assert!(
            val_sexp.contains("(at 100") && val_sexp.contains("83.81"),
            "Value must sit near the symbol: {val_sexp}"
        );
        assert!(
            !val_sexp.contains("hide"),
            "Value must stay visible on power symbols: {val_sexp}"
        );
        assert!(
            !after.contains("(property \"Reference\" \"#PWR001\")\n"),
            "must not write a bare Reference with no (at)"
        );
    }

    #[tokio::test]
    async fn add_power_symbol_copies_library_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("power-metadata.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:GND\"\n      (property \"Reference\" \"#PWR\" (at 0 -6.35 0))\n      (property \"Value\" \"GND\" (at 0 -3.81 0))\n      (property \"Datasheet\" \"https://example.com/gnd.pdf\" (at 0 0 0))\n      (property \"Description\" \"Ground power symbol\" (at 0 0 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        let result = handle_add_power_symbol(
            &json!({
                "schematic": path.display().to_string(),
                "power_net": "GND",
                "x": 100.0,
                "y": 80.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch
            .symbols
            .iter()
            .find(|s| s.reference() == Some("#PWR001"))
            .expect("power symbol instance");
        assert_eq!(
            sym.properties
                .iter()
                .find(|p| p.name == "Datasheet")
                .map(|p| p.value.as_str()),
            Some("https://example.com/gnd.pdf")
        );
        assert_eq!(
            sym.properties
                .iter()
                .find(|p| p.name == "Description")
                .map(|p| p.value.as_str()),
            Some("Ground power symbol")
        );
    }

    /// An up-pointing rail anchors its Value *above* the graphic, where a
    /// fixed +3.81 offset used to put it below (#101). VCC's library anchor is
    /// (0, +3.556) in Y-up space, so the sheet coordinate is y − 3.556.
    #[tokio::test]
    async fn up_pointing_power_symbol_puts_value_above_the_graphic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vcc.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:VCC\"\n      (property \"Reference\" \"#PWR\" (at 0 -3.81 0))\n      (property \"Value\" \"VCC\" (at 0 3.556 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        let result = handle_add_power_symbol(
            &json!({
                "schematic": path.display().to_string(),
                "power_net": "VCC",
                "x": 100.0,
                "y": 80.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch
            .symbols
            .iter()
            .find(|s| s.reference() == Some("#PWR001"))
            .expect("power symbol instance");
        let val_prop = sym.properties.iter().find(|p| p.name == "Value").unwrap();
        let val_sexp = cse::sexp::writer::write(&val_prop.to_sexp());
        assert!(
            val_sexp.contains("76.444"),
            "VCC's Value belongs above the symbol at y-3.556, not below: {val_sexp}"
        );
    }

    /// Numbering by count re-issued a designator that was still on the sheet:
    /// delete `#PWR002` of three and the next add produced a second `#PWR003`.
    #[tokio::test]
    async fn add_power_symbol_fills_a_freed_number_instead_of_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gnd.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:GND\"\n      (property \"Reference\" \"#PWR\" (at 0 -6.35 0))\n      (property \"Value\" \"GND\" (at 0 -3.81 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        let add = |x: f64| {
            let args = json!({
                "schematic": path.display().to_string(),
                "power_net": "GND",
                "x": x,
                "y": 80.0
            });
            async move {
                let result = handle_add_power_symbol(&args, &test_ctx()).await.unwrap();
                assert!(!result.is_error, "{result:?}");
            }
        };
        add(100.0).await;
        add(110.0).await;
        add(120.0).await;

        let mut sch = cse::Schematic::load(&path).unwrap();
        sch.symbols.retain(|s| s.reference() != Some("#PWR002"));
        sch.overwrite().unwrap();

        add(130.0).await;

        let sch = cse::Schematic::load(&path).unwrap();
        let mut refs: Vec<&str> = sch.symbols.iter().filter_map(|s| s.reference()).collect();
        refs.sort_unstable();
        assert_eq!(
            refs,
            ["#PWR001", "#PWR002", "#PWR003"],
            "the freed number belongs to the new symbol, and nothing may repeat"
        );
    }
}

#[cfg(test)]
mod no_connect_delete_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// Tab-indented, multi-line no-connects — the shape eeschema and this
    /// crate's own writer both produce. The old literal-string search looked
    /// for `(no_connect (at X Y)` on one line, which no real file contains.
    fn schematic_with_two_no_connects() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nc.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch
	(version 20250610)
	(generator \"eeschema\")
	(uuid \"root\")
	(paper \"A4\")
	(no_connect
		(at 127 63.5)
		(uuid \"nc-1\")
	)
	(no_connect
		(at 140 70)
		(uuid \"nc-2\")
	)
)
",
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn delete_no_connect_removes_a_multiline_block() {
        let (_d, path) = schematic_with_two_no_connects();
        let result = handle_delete_no_connect(
            &json!({ "schematic": path.display().to_string(), "x": 127.0, "y": 63.5 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("nc-1"),
            "the targeted no-connect is still on disk: {after}"
        );
        assert!(after.contains("nc-2"), "deleted the wrong block: {after}");
        assert!(
            konnect_sexp::parse_sexp(&after).is_ok(),
            "file no longer parses: {after}"
        );
    }

    #[tokio::test]
    async fn deleting_a_missing_no_connect_reports_an_error() {
        let (_d, path) = schematic_with_two_no_connects();
        let before = std::fs::read_to_string(&path).unwrap();
        let result = handle_delete_no_connect(
            &json!({ "schematic": path.display().to_string(), "x": 999.0, "y": 999.0 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "a miss must not report success");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a failed delete must leave the file byte-identical"
        );
    }

    /// The batch variant used to count `.is_ok()` on a handler that returns
    /// `Ok(CallToolResult::error(..))` for a miss, so it reported a deletion
    /// for every position whether or not anything was removed.
    #[tokio::test]
    async fn batch_delete_counts_only_what_it_removed() {
        let (_d, path) = schematic_with_two_no_connects();
        let result = handle_batch_delete_no_connect(
            &json!({
                "schematic": path.display().to_string(),
                "positions": [ { "x": 127.0, "y": 63.5 }, { "x": 999.0, "y": 999.0 } ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["deleted"], 1, "only one position exists: {body}");
        assert_eq!(
            body["errors"].as_array().map(|e| e.len()),
            Some(1),
            "the missing position must be reported: {body}"
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("nc-1"));
        assert!(after.contains("nc-2"));
    }
}

#[cfg(test)]
mod batch_no_connect_tests {
    use super::tools;
    use crate::tools::ToolContext;
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    const SCH: &str = "(kicad_sch\n  (version 20260306)\n  (generator \"eeschema\")\n  (uuid \"root\")\n  (paper \"A4\")\n  (lib_symbols)\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n";

    async fn run(positions: serde_json::Value) -> (String, serde_json::Value) {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();
        let def = tools()
            .into_iter()
            .find(|t| t.name == "batch_add_no_connect")
            .unwrap();
        let cfg = crate::tools::ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        };
        let ctx = Arc::new(ToolContext::new(
            cfg,
            Arc::new(crate::router::ToolRouter::new()),
        ));
        let args = json!({ "schematic": f.path().to_str().unwrap(), "positions": positions });
        let res = (def.handler)(&args, ctx).await.unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &res.content[0] else {
            panic!("expected text content")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        (std::fs::read_to_string(f.path()).unwrap(), body)
    }

    #[tokio::test]
    async fn writes_every_flag_in_one_pass() {
        let (out, body) = run(json!([{"x": 10.0, "y": 20.0}, {"x": 30.0, "y": 40.0}])).await;
        assert_eq!(body["added_count"], 2);
        assert_eq!(out.matches("(no_connect").count(), 2);
    }

    /// A malformed entry must not abort the rest — the useful failure mode when
    /// flagging twenty pins is "nineteen landed and one is reported".
    #[tokio::test]
    async fn a_bad_entry_is_reported_without_losing_the_others() {
        let (out, body) =
            run(json!([{"x": 10.0, "y": 20.0}, {"x": "nope"}, {"x": 1.0, "y": 2.0}])).await;
        assert_eq!(body["added_count"], 2);
        assert_eq!(body["errors"].as_array().unwrap().len(), 1);
        assert_eq!(out.matches("(no_connect").count(), 2);
    }

    #[tokio::test]
    async fn empty_list_is_a_no_op() {
        let (out, body) = run(json!([])).await;
        assert_eq!(body["added_count"], 0);
        assert_eq!(out.matches("(no_connect").count(), 0);
    }
}
