//! `sch_wiring` toolset — wires, net labels, power symbols, junctions, no-connects.
//!
//! Key rule: Every wire add operation must auto-detect T-junctions and insert
//! junction dots. This uses `konnect_sexp::schematic::find_t_junctions`.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, opt_f64, opt_str, project_name_for, require_f64, require_str, ToolContext, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::snap_point,
    parser::parse_sexp,
    schematic::{
        extract_symbol_instances, extract_wires, find_t_junctions, format_junction, format_wire,
        parse_at, pin_endpoint, read_schematic,
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
             anything when no wire matches.",
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
            "Delete multiple wire segments in a single file read/write cycle.",
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
             internal #PWR reference.",
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
            "Connect a pin endpoint to a named net by adding a short wire stub and a net label.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "pin_x": { "type": "number" }, "pin_y": { "type": "number" },
                    "net": { "type": "string" },
                    "direction": {
                        "type": "string",
                        "description": "Direction to route the wire stub: 'right' (default), 'left', 'up', 'down'",
                        "enum": ["right", "left", "up", "down"],
                        "default": "right"
                    },
                    "stub_length": { "type": "number", "default": 2.54,
                        "description": "Length of the wire stub in mm" },
                    "label_type": {
                        "type": "string",
                        "enum": ["net_label", "global_label"],
                        "default": "net_label"
                    }
                },
                "required": ["schematic", "pin_x", "pin_y", "net"]
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
    let insert_pos = find_first_symbol_instance(content)
        .unwrap_or_else(|| content.rfind(')').unwrap_or(content.len()));
    let edits = vec![SexpEdit::insert(insert_pos, new_sexp)];
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
    for (jx, jy) in &junctions {
        sch.add_junction(*jx, *jy);
    }
    // Pins the new wire passes over mid-segment also need junction dots.
    let (_, tree) = read_schematic(&sch_path)?;
    let pins = crate::tools::all_pin_endpoints(&tree);
    for (px, py) in pins_mid_segment(&pins, x1, y1, x2, y2) {
        if !sch
            .junctions
            .iter()
            .any(|j| konnect_sexp::geometry::points_coincident(px, py, j.x, j.y, 0.01))
        {
            sch.add_junction(px, py);
        }
    }
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
    let wires = args["wires"].as_array().cloned().unwrap_or_default();

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
        for (jx, jy) in &junctions {
            sch.add_junction(*jx, *jy);
        }
        // Pins this wire passes over mid-segment also need junction dots.
        for (px, py) in pins_mid_segment(&pins, x1, y1, x2, y2) {
            if !sch
                .junctions
                .iter()
                .any(|j| konnect_sexp::geometry::points_coincident(px, py, j.x, j.y, 0.01))
            {
                sch.add_junction(px, py);
            }
        }
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

    let delete_range = if let Some(uuid) = opt_str(args, "uuid") {
        let search = format!(r#"(uuid "{uuid}")"#);
        let Some(wire_offset) = content.find(&search) else {
            return Ok(CallToolResult::error(format!(
                "Wire UUID '{uuid}' not found"
            )));
        };
        wire_block_with_leading_whitespace(&content, wire_offset)
    } else {
        let Some(x1) = opt_f64(args, "x1") else {
            return Ok(CallToolResult::error(
                "Provide either uuid or all x1/y1/x2/y2 coordinates",
            ));
        };
        let Some(y1) = opt_f64(args, "y1") else {
            return Ok(CallToolResult::error(
                "Provide either uuid or all x1/y1/x2/y2 coordinates",
            ));
        };
        let Some(x2) = opt_f64(args, "x2") else {
            return Ok(CallToolResult::error(
                "Provide either uuid or all x1/y1/x2/y2 coordinates",
            ));
        };
        let Some(y2) = opt_f64(args, "y2") else {
            return Ok(CallToolResult::error(
                "Provide either uuid or all x1/y1/x2/y2 coordinates",
            ));
        };
        find_wire_block_by_endpoints(&content, x1, y1, x2, y2)
    };

    let (del_start, del_end) = match delete_range {
        Some(r) => r,
        None => {
            return Ok(CallToolResult::error(
                "Cannot locate a wire block matching the requested identity",
            ))
        }
    };

    let edits = vec![SexpEdit::delete(del_start, del_end)];
    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    Ok(CallToolResult::text("Wire deleted."))
}

async fn handle_batch_delete_wire(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let uuids: Vec<String> = args["uuids"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

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
        // `extract_wires` expects wires to be direct children of the parsed
        // document root, so wrap this standalone block in a minimal root.
        let wrapped = format!("(kicad_sch {})", &content[block_start..block_end]);
        let Ok(node) = parse_sexp(&wrapped) else {
            continue;
        };
        let matches = extract_wires(&node).into_iter().any(|wire| {
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
    ctx: &ToolContext,
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

    let (_, tree) = read_schematic(&sch_path)?;
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

    // Delete the original wire and insert two halves + junction
    let del_args = if let Some(uuid) = &w.uuid {
        json!({ "schematic": sch_path.display().to_string(), "uuid": uuid })
    } else {
        json!({
            "schematic": sch_path.display().to_string(),
            "x1": w.x1,
            "y1": w.y1,
            "x2": w.x2,
            "y2": w.y2
        })
    };
    let delete_result = handle_delete_wire(&del_args, ctx).await?;
    if delete_result.is_error {
        return Ok(delete_result);
    }

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
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
    let labels = args["labels"].as_array().cloned().unwrap_or_default();
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

    // Auto-number the #PWR reference by counting existing power symbols
    let pwr_count = sch
        .symbols
        .iter()
        .filter(|s| {
            s.reference()
                .map(|r| r.starts_with("#PWR"))
                .unwrap_or(false)
        })
        .count();
    let pwr_ref = format!("#PWR{:03}", pwr_count + 1);

    // Embed the power symbol definition in lib_symbols
    let lib_id = format!("power:{}", power_net);
    if !cse::library::ensure_lib_symbol(&mut sch, &lib_id) {
        return Ok(crate::tools::lib_symbol_not_found_error(&lib_id));
    }

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
    let positioned = crate::tools::positioned_property;
    sym.properties
        .push(positioned("Reference", &pwr_ref, x, y - 3.81, 0.0, true));
    sym.properties
        .push(positioned("Value", &power_net, x, y + 3.81, 0.0, false));
    sym.properties
        .push(positioned("Footprint", "", x, y, 0.0, true));
    sym.properties
        .push(positioned("Datasheet", "", x, y, 0.0, true));

    // Instance entry, keyed to the root sheet UUID like eeschema writes it —
    // without a resolvable "/<root-uuid>" path KiCAD's netlister drops the
    // symbol from net formation.
    let root_uuid = crate::tools::ensure_root_uuid(&mut sch);
    sym.set_instance_path(
        &project_name_for(&sch_path),
        &format!("/{}", root_uuid),
        &pwr_ref,
        1,
    );

    sch.add_symbol(sym);
    sch.overwrite()?;

    // A power pin landing mid-segment on an existing wire needs a junction
    // dot, or KiCad ERC reports it as not connected.
    let junctions_added = crate::tools::add_pin_midwire_junctions(&sch_path, &pwr_ref)?;

    Ok(CallToolResult::json(&json!({
        "added_power": power_net,
        "reference": pwr_ref,
        "x": x, "y": y,
        "junctions_added": junctions_added.iter().map(|(x, y)| json!({"x": x, "y": y})).collect::<Vec<_>>()
    })))
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
    let positions = args["positions"].as_array().cloned().unwrap_or_default();

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
    let positions = args["positions"].as_array().cloned().unwrap_or_default();
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
    let pin_x = match require_f64(args, "pin_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let pin_y = match require_f64(args, "pin_y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let direction = opt_str(args, "direction").unwrap_or("right");
    let stub_length = opt_f64(args, "stub_length").unwrap_or(2.54);
    let label_type = opt_str(args, "label_type").unwrap_or("net_label");

    // Compute label endpoint and label rotation based on direction.
    // Label rotation follows KiCAD convention: 0° = text reads left-to-right,
    // label anchor is at the wire connection end.
    let (label_x, label_y, label_rot) = match direction {
        "left" => (pin_x - stub_length, pin_y, 180.0),
        "up" => (pin_x, pin_y - stub_length, 90.0),
        "down" => (pin_x, pin_y + stub_length, 270.0),
        _ => (pin_x + stub_length, pin_y, 0.0), // "right" default
    };

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
    for (jx, jy) in &junctions {
        sch.add_junction(*jx, *jy);
    }
    // Pins the stub passes over mid-segment also need junction dots.
    let pins = read_schematic(&sch_path)
        .map(|(_, tree)| crate::tools::all_pin_endpoints(&tree))
        .unwrap_or_default();
    for (px, py) in pins_mid_segment(&pins, pin_x, pin_y, label_x, label_y) {
        if !sch
            .junctions
            .iter()
            .any(|j| konnect_sexp::geometry::points_coincident(px, py, j.x, j.y, 0.01))
        {
            sch.add_junction(px, py);
        }
    }

    // Add label
    match label_type {
        "global_label" => {
            sch.add_global_label(&net, "input", label_x, label_y);
            let idx = sch.global_labels.len() - 1;
            if let Some(gl) = sch.global_labels.get_mut(idx) {
                gl.at.rotation = Some(label_rot);
            }
        }
        _ => {
            let label = sch.add_label(&net, label_x, label_y);
            label.at.rotation = Some(label_rot);
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "connected": net,
        "direction": direction,
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
    let inst = instances
        .iter()
        .find(|i| i.reference == reference)
        .ok_or_else(|| anyhow::anyhow!("Component '{}' not found", reference))?;
    let lib_sym = lib_syms
        .iter()
        .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id))
        .ok_or_else(|| anyhow::anyhow!("Library symbol '{}' not found", inst.lib_id))?;

    // Unit-aware (#35): only this instance's unit owns the pin — asking unit 1
    // of an LM2904 for pin 7 must fail, not wire to a superimposed phantom.
    let pins = konnect_sexp::schematic::extract_lib_pins_for_unit(lib_sym, inst.unit);
    let lib_pin = pins
        .iter()
        .find(|p| p.number == pin_number)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Pin '{}' not found on '{}' (unit {})",
                pin_number,
                reference,
                inst.unit
            )
        })?;

    Ok(pin_endpoint(lib_pin, inst.pin_transform()))
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
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:GND\"\n      (property \"Reference\" \"#PWR\" (at 0 0 0))\n      (property \"Value\" \"GND\" (at 0 0 0))\n    )\n  )\n)\n",
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
            ref_sexp.contains("(at 100") && ref_sexp.contains("76.19"),
            "Reference must sit near the symbol, not sheet origin: {ref_sexp}"
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
