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
    schematic::{
        extract_lib_pins, extract_symbol_instances, extract_wires, find_t_junctions,
        format_junction, format_wire, pin_endpoint, read_schematic,
    },
    writer::{apply_edits, find_block_with_leading_whitespace, write_atomic, SexpEdit},
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
            "Delete a wire segment by its UUID or by matching its start/end coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "uuid": { "type": "string", "description": "Wire UUID (preferred)" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
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
            "Split a wire at a given point, creating two wire segments and a junction.",
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
            "add_junction",
            "Add a junction dot at a point where wires cross or T-intersect.",
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

fn insert_before_close(content: &str, new_sexp: &str) -> String {
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
    // Pattern: a symbol instance always contains (lib_id "...") shortly after (symbol
    // lib_symbols definitions contain sub-symbols but NOT (lib_id
    let mut pos = 0;
    while let Some(found) = content[pos..].find("\n  (symbol") {
        let abs = pos + found;
        // Check if this symbol block contains (lib_id within the next ~200 chars
        let lookahead = &content[abs..content.len().min(abs + 200)];
        if lookahead.contains("(lib_id ") {
            // This is a top-level symbol instance, not a lib_symbols definition
            return Some(abs + 1); // +1 to skip the \n
        }
        pos = abs + 1;
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

fn insert_wire_with_junctions(content: String, x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    // Parse existing wires to detect new T-junctions
    let tree = konnect_sexp::parse_sexp(&content).ok();
    let mut existing_wires = tree.as_ref().map(extract_wires).unwrap_or_default();

    // Add the new wire to the set before checking junctions (it may form T's too)
    let new_wire = konnect_sexp::schematic::Wire {
        x1,
        y1,
        x2,
        y2,
        uuid: None,
    };
    existing_wires.push(new_wire);

    let junctions = find_t_junctions(&existing_wires, 0.01);

    let mut c = content;
    c = insert_before_close(&c, &format_wire(x1, y1, x2, y2));
    for (jx, jy) in junctions {
        c = insert_before_close(&c, &format_junction(jx, jy));
    }
    c
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

    for w in &wires {
        let x1 = w["x1"].as_f64().unwrap_or(0.0);
        let y1 = w["y1"].as_f64().unwrap_or(0.0);
        let x2 = w["x2"].as_f64().unwrap_or(0.0);
        let y2 = w["y2"].as_f64().unwrap_or(0.0);
        let (x1, y1) = snap_point(x1, y1, 1.27);
        let (x2, y2) = snap_point(x2, y2, 1.27);

        // T-junction detection for each wire added incrementally
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
    let content = std::fs::read_to_string(&sch_path)?;

    let search_str = if let Some(uuid) = opt_str(args, "uuid") {
        format!(r#"(uuid "{uuid}")"#)
    } else {
        let x1 = opt_f64(args, "x1").unwrap_or(0.0);
        let y1 = opt_f64(args, "y1").unwrap_or(0.0);
        format!("(start {x1} {y1})")
    };

    let wire_offset = match content.find(&search_str) {
        Some(o) => o,
        None => return Ok(CallToolResult::error("Wire not found")),
    };

    // Walk back to the (wire ...) block start
    let before = &content[..wire_offset];
    let wire_start = before.rfind("\n  (wire").map(|p| p + 1).unwrap_or(0);
    let (del_start, del_end) = match find_block_with_leading_whitespace(&content, wire_start) {
        Some(r) => r,
        None => return Ok(CallToolResult::error("Cannot parse wire block")),
    };

    let edits = vec![SexpEdit::delete(del_start, del_end)];
    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;
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

    let mut content = std::fs::read_to_string(&sch_path)?;
    let mut deleted = 0usize;

    // Collect all delete ranges first, then apply in reverse order
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for uuid in &uuids {
        let search = format!(r#"(uuid "{uuid}")"#);
        if let Some(offset) = content.find(&search) {
            let before = &content[..offset];
            if let Some(wire_start) = before.rfind("\n  (wire").map(|p| p + 1) {
                if let Some(range) = find_block_with_leading_whitespace(&content, wire_start) {
                    ranges.push(range);
                    deleted += 1;
                }
            }
        }
    }

    let edits: Vec<SexpEdit> = ranges
        .into_iter()
        .map(|(s, e)| SexpEdit::delete(s, e))
        .collect();
    content = apply_edits(content, edits);
    write_atomic(&sch_path, &content)?;
    Ok(CallToolResult::json(&json!({ "deleted": deleted })))
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
        json!({ "schematic": sch_path.display().to_string(), "x1": w.x1, "y1": w.y1 })
    };
    handle_delete_wire(&del_args, ctx).await?;

    let content = std::fs::read_to_string(&sch_path)?;
    let w1 = format_wire(w.x1, w.y1, px, py);
    let w2 = format_wire(px, py, w.x2, w.y2);
    let junc = format_junction(px, py);
    let close = content.rfind(')').unwrap_or(content.len());
    let edits = vec![SexpEdit::insert(close, format!("{}{}{}", w1, w2, junc))];
    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;

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

    match label_type {
        "global_label" => {
            sch.add_global_label(&net, shape, x, y);
            // Set rotation on the just-added global label
            let idx = sch.global_labels.len() - 1;
            if let Some(gl) = sch.global_labels.get_mut(idx) {
                gl.at.rotation = Some(rotation);
            }
        }
        "hierarchical_label" => {
            sch.add_hierarchical_label(&net, shape, x, y);
            // Set rotation on the just-added hierarchical label
            let idx = sch.hierarchical_labels.len() - 1;
            if let Some(hl) = sch.hierarchical_labels.get_mut(idx) {
                hl.at.rotation = Some(rotation);
            }
        }
        _ => {
            let label = sch.add_label(&net, x, y);
            label.at.rotation = Some(rotation);
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

    let content = std::fs::read_to_string(&sch_path)?;

    // Find ALL label occurrences with this net name, then pick the closest to (target_x, target_y).
    // This handles the common case of multiple labels on the same net.
    let search = format!(r#""{net}""#);
    let label_starts_patterns = ["(net_label", "(global_label", "(hierarchical_label"];

    let mut best_start = None;
    let mut best_dist = f64::MAX;

    let mut search_from = 0usize;
    while let Some(name_offset) = content[search_from..]
        .find(&search)
        .map(|i| i + search_from)
    {
        // Walk back to find the enclosing label block
        let before = &content[..name_offset];
        if let Some(label_start) = label_starts_patterns
            .iter()
            .filter_map(|s| before.rfind(s))
            .max()
        {
            // Parse the (at X Y) from this block to check proximity
            let block_rest = &content[label_start..];
            if let Some(at_pos) = block_rest.find("(at ") {
                let at_str = &block_rest[at_pos + 4..];
                let parts: Vec<&str> = at_str.split([' ', ')']).collect();
                if parts.len() >= 2 {
                    let lx: f64 = parts[0].parse().unwrap_or(f64::MAX);
                    let ly: f64 = parts[1].parse().unwrap_or(f64::MAX);
                    let dist = (lx - target_x).abs() + (ly - target_y).abs();
                    if dist < best_dist {
                        best_dist = dist;
                        best_start = Some(label_start);
                    }
                }
            }
        }
        search_from = name_offset + 1;
    }

    let label_start = best_start.ok_or_else(|| anyhow::anyhow!("Label '{}' not found", net))?;

    let (del_start, del_end) = find_block_with_leading_whitespace(&content, label_start)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse label block"))?;

    let edits = vec![SexpEdit::delete(del_start, del_end)];
    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;
    Ok(CallToolResult::json(
        &json!({ "deleted_label": net, "at": { "x": target_x, "y": target_y } }),
    ))
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

    let content = std::fs::read_to_string(&sch_path)?;
    let search = format!(r#""{net}""#);
    let found = content
        .find(&search)
        .ok_or_else(|| anyhow::anyhow!("Label '{}' not found", net))?;
    let before = &content[..found];
    let label_start = ["(net_label", "(global_label", "(hierarchical_label"]
        .iter()
        .filter_map(|s| before.rfind(s))
        .max()
        .ok_or_else(|| anyhow::anyhow!("Label block not found"))?;

    // Find the (at X Y ROT) in the label block
    let at_search = "(at ";
    let at_pos = content[label_start..]
        .find(at_search)
        .map(|o| label_start + o + at_search.len())
        .ok_or_else(|| anyhow::anyhow!("No (at) in label block"))?;
    let close_pos = content[at_pos..]
        .find(')')
        .map(|o| at_pos + o)
        .ok_or_else(|| anyhow::anyhow!("Malformed (at)"))?;

    let new_at = format!("{x} {y} {rotation}");
    let edits = vec![SexpEdit::replace(at_pos, close_pos, new_at)];
    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;
    Ok(CallToolResult::json(
        &json!({ "rotated_label": net, "rotation": rotation }),
    ))
}

async fn handle_move_labels_by_offset(
    args: &serde_json::Value,
    ctx: &ToolContext,
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

    let (_, tree) = read_schematic(&sch_path)?;
    let labels = konnect_sexp::schematic::extract_labels(&tree);

    let matching: Vec<_> = labels.iter().filter(|l| l.net == net).cloned().collect();
    let mut moved = 0usize;

    for label in &matching {
        let rotate_args = json!({
            "schematic": sch_path.display().to_string(),
            "net": net,
            "x": label.x + dx,
            "y": label.y + dy,
            "rotation": label.rotation
        });
        handle_rotate_label(&rotate_args, ctx).await?;
        moved += 1;
    }

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
    cse::library::ensure_lib_symbol(&mut sch, &lib_id);

    // Build the Symbol struct
    let mut sym = cse::Symbol::new(format!("power:{}", power_net), x, y);
    sym.at.rotation = Some(rotation);
    sym.unit = 1;
    sym.in_bom = true;
    sym.on_board = true;
    sym.uuid = uuid::Uuid::new_v4().to_string();
    sym.properties
        .push(cse::Property::new("Reference", &pwr_ref));
    sym.properties.push(cse::Property::new("Value", &power_net));
    sym.properties.push(cse::Property::new("Footprint", ""));
    sym.properties.push(cse::Property::new("Datasheet", ""));

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

    Ok(CallToolResult::json(&json!({
        "added_power": power_net,
        "reference": pwr_ref,
        "x": x, "y": y
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

    let content = std::fs::read_to_string(&sch_path)?;
    let search = format!("(no_connect (at {x} {y})");
    let pos = match content.find(&search) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(
                "No-connect not found at that position",
            ))
        }
    };
    let (del_start, del_end) = find_block_with_leading_whitespace(&content, pos)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse no_connect block"))?;
    let edits = vec![SexpEdit::delete(del_start, del_end)];
    let new_content = apply_edits(content, edits);
    write_atomic(&sch_path, &new_content)?;
    Ok(CallToolResult::text("No-connect deleted."))
}

async fn handle_batch_delete_no_connect(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let positions = args["positions"].as_array().cloned().unwrap_or_default();
    let mut deleted = 0usize;
    for pos in &positions {
        let del_args = json!({
            "schematic": sch_path.display().to_string(),
            "x": pos["x"], "y": pos["y"]
        });
        if handle_delete_no_connect(&del_args, ctx).await.is_ok() {
            deleted += 1;
        }
    }
    Ok(CallToolResult::json(&json!({ "deleted": deleted })))
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
    let mut new_content = content;
    if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
        // Already axis-aligned: single wire
        new_content = insert_wire_with_junctions(new_content, x1, y1, x2, y2);
    } else {
        // L-bend: horizontal then vertical
        let mid_x = x2;
        let mid_y = y1;
        new_content = insert_wire_with_junctions(new_content.clone(), x1, y1, mid_x, mid_y);
        new_content = insert_wire_with_junctions(new_content, mid_x, mid_y, x2, y2);
    }

    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "connected": {
            "from": { "ref": ref1, "pin": pin1, "x": x1, "y": y1 },
            "to":   { "ref": ref2, "pin": pin2, "x": x2, "y": y2 }
        }
    })))
}

/// Resolve a pin's schematic-space endpoint by reference and pin number.
/// Uses the same pattern as sch_analysis::handle_get_pin_connections.
fn resolve_pin_endpoint(
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

    let pins = extract_lib_pins(lib_sym);
    let lib_pin = pins
        .iter()
        .find(|p| p.number == pin_number)
        .ok_or_else(|| anyhow::anyhow!("Pin '{}' not found on '{}'", pin_number, reference))?;

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

    let mut content = std::fs::read_to_string(&sch_path)?;

    if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
        // Already axis-aligned: single wire
        content = insert_wire_with_junctions(content, x1, y1, x2, y2);
    } else {
        // Route with an L-bend: H segment then V segment
        let mid_x = x2;
        let mid_y = y1;
        content = insert_wire_with_junctions(content.clone(), x1, y1, mid_x, mid_y);
        content = insert_wire_with_junctions(content, mid_x, mid_y, x2, y2);
    }

    write_atomic(&sch_path, &content)?;
    Ok(CallToolResult::json(&json!({
        "connected": { "from": [x1, y1], "to": [x2, y2] }
    })))
}
