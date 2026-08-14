//! `sch_batch` toolset — bulk/batch operations on schematic elements.
//!
//! **Critical invariant**: every write handler performs a single file read,
//! collects ALL mutations as `SexpEdit` values against the original content,
//! then calls `write_atomic` exactly once. This fixes the Python bug where
//! `batch_connect_to_net` did N separate read/write cycles.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    find_all_symbol_instance_blocks, get_path, opt_str, project_name_for, require_f64, require_str,
    ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident, snap_point},
    schematic::{
        extract_labels, extract_lib_pins, extract_lib_pins_for_unit, extract_symbol_instances,
        extract_wires, format_net_label, format_wire, pin_endpoint, read_schematic,
    },
    writer::{
        apply_edits, find_block_with_leading_whitespace, find_enclosing_direct_child_block,
        new_uuid, read_consistent, write_atomic_if_unchanged, SexpEdit,
    },
};
use serde_json::json;
use std::collections::HashSet;

// Re-use the crate-internal net-graph primitives from sch_analysis.
use super::sch_analysis::build_net_graph;
// Re-use the single-item component placer and pin-to-pin router.
use super::sch_components::place_one_component;
use super::sch_wiring::{resolve_pin_endpoint, route_between};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "batch_connect_to_net",
            "Connect multiple component pins to a named net by adding net labels at each pin \
             endpoint. Single file read → all labels inserted → single file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "net_name": { "type": "string", "description": "Name of the net to connect pins to" },
                    "pins": {
                        "type": "array",
                        "description": "List of {reference, pin_number} objects to connect",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "pin_number": { "type": "string" }
                            },
                            "required": ["reference", "pin_number"]
                        }
                    }
                },
                "required": ["schematic", "net_name", "pins"]
            }),
            |args, ctx| async move { handle_batch_connect_to_net(args, ctx).await }
        ),
        tool!(
            "batch_place_components",
            "Place multiple symbols from KiCAD libraries in a single file read/write cycle. \
             Pass explicit references -- there is no auto-numbering; an omitted reference \
             becomes '?' like an eeschema-unannotated symbol, same as add_schematic_component.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "components": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "lib_id": { "type": "string" },
                                "x": { "type": "number" }, "y": { "type": "number" },
                                "rotation": { "type": "number", "default": 0 },
                                "reference": { "type": "string" },
                                "value": { "type": "string" },
                                "unit": { "type": "integer", "default": 1 }
                            },
                            "required": ["lib_id", "x", "y"]
                        }
                    }
                },
                "required": ["schematic", "components"]
            }),
            |args, ctx| async move { handle_batch_place_components(args, ctx).await }
        ),
        tool!(
            "batch_connect_pins",
            "Connect multiple component pin pairs by reference and pin number, in a single \
             file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "connections": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref1": { "type": "string" }, "pin1": { "type": "string" },
                                "ref2": { "type": "string" }, "pin2": { "type": "string" }
                            },
                            "required": ["ref1", "pin1", "ref2", "pin2"]
                        }
                    }
                },
                "required": ["schematic", "connections"]
            }),
            |args, ctx| async move { handle_batch_connect_pins(args, ctx).await }
        ),
        tool!(
            "batch_delete",
            "Delete multiple schematic items (wires, labels, junctions, components) by UUID \
             or component reference designator — single file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "uuids": {
                        "type": "array",
                        "description": "UUIDs of items to delete",
                        "items": { "type": "string" }
                    },
                    "references": {
                        "type": "array",
                        "description": "Component reference designators to delete",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_batch_delete(args, ctx).await }
        ),
        tool!(
            "bulk_move_schematic_components",
            "Move multiple components by a uniform dx/dy offset in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Reference designators to move",
                        "items": { "type": "string" }
                    },
                    "dx": { "type": "number", "description": "X offset in mm" },
                    "dy": { "type": "number", "description": "Y offset in mm" }
                },
                "required": ["schematic", "references", "dx", "dy"]
            }),
            |args, ctx| async move { handle_bulk_move(args, ctx).await }
        ),
        tool!(
            "batch_edit_schematic_components",
            "Apply field updates (Value, Footprint, custom properties) to multiple components \
             in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "edits": {
                        "type": "array",
                        "description": "List of {reference, value?, footprint?, fields?} edit objects",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "value": { "type": "string" },
                                "footprint": { "type": "string" },
                                "fields": {
                                    "type": "object",
                                    "description": "Additional property fields as key:value pairs"
                                }
                            },
                            "required": ["reference"]
                        }
                    }
                },
                "required": ["schematic", "edits"]
            }),
            |args, ctx| async move { handle_batch_edit(args, ctx).await }
        ),
        tool!(
            "batch_delete_schematic_components",
            "Delete multiple components by reference designator in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Reference designators to delete",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic", "references"]
            }),
            |args, ctx| async move { handle_batch_delete_components(args, ctx).await }
        ),
        tool!(
            "connect_passthrough",
            "Add a wire stub and matching net label at a point to route a signal through \
             a region without drawing a full wire path. Direction controls stub orientation.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "net_name": { "type": "string", "description": "Net name for the passthrough label" },
                    "x": { "type": "number", "description": "X position of the stub root in mm" },
                    "y": { "type": "number", "description": "Y position of the stub root in mm" },
                    "direction": {
                        "type": "string",
                        "description": "Stub direction: 'left', 'right', 'up', 'down'",
                        "default": "right"
                    }
                },
                "required": ["schematic", "net_name", "x", "y"]
            }),
            |args, ctx| async move { handle_connect_passthrough(args, ctx).await }
        ),
        tool!(
            "add_schematic_text",
            "Add a text annotation (non-net label) to the schematic at a given position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "text": { "type": "string", "description": "Text content to add" },
                    "x": { "type": "number", "description": "X position in mm" },
                    "y": { "type": "number", "description": "Y position in mm" },
                    "size": { "type": "number", "description": "Font size in mm", "default": 1.27 },
                    "rotation": { "type": "number", "description": "Rotation in degrees", "default": 0 }
                },
                "required": ["schematic", "text", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_text(args, ctx).await }
        ),
        tool!(
            "get_schematic_layout",
            "Return a compact spatial summary of the schematic: component positions, \
             bounding box, and optionally wire segments and label locations.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "include_wires": { "type": "boolean", "description": "Include wire data", "default": true },
                    "include_labels": { "type": "boolean", "description": "Include label data", "default": true }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_layout(args, ctx).await }
        ),
        tool!(
            "validate_wire_connections",
            "Check all wire endpoints for floating ends (not connected to a pin, label, \
             or another wire). Reports each floating endpoint with its coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "tolerance": { "type": "number", "description": "Snap tolerance in mm", "default": 0.01 }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_wire_connections(args, ctx).await }
        ),
        tool!(
            "validate_component_connections",
            "Check that every non-passive pin on every component has at least one wire \
             or label connected. Reports unconnected pins with reference, pin number, \
             and schematic position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "ignore_power_pins": {
                        "type": "boolean",
                        "description": "Skip power-type pins in the check",
                        "default": false
                    },
                    "references": {
                        "type": "array",
                        "description": "Limit check to these reference designators (empty = all)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_component_connections(args, ctx).await }
        ),
    ]
}

// ─── Private helpers ──────────────────────────────────────────────────────────

/// Find every `(symbol ...)` block for a reference designator, each with its
/// leading whitespace so deletion leaves clean formatting.
///
/// One entry per unit: deleting a multi-unit part means deleting all of them.
/// Returns `(block_start, block_end)` byte offsets in `content`.
fn find_symbol_blocks(content: &str, reference: &str) -> Vec<(usize, usize)> {
    find_all_symbol_instance_blocks(content, reference)
        .into_iter()
        .filter_map(|(sym_start, _)| find_block_with_leading_whitespace(content, sym_start))
        .collect()
}

/// Return `(val_start, val_end)` byte offsets in `content` for the *value* portion
/// of a `(property "FieldName" "VALUE" ...)` node, once per placed instance of
/// `reference`. Only the bytes inside the opening quote are included (i.e. the
/// replacement does NOT need to include surrounding quotes).
///
/// Multi-unit parts repeat their fields in every unit's block and KiCad expects
/// those copies to agree, so a field edit has to rewrite all of them.
fn field_value_ranges(content: &str, reference: &str, field: &str) -> Vec<(usize, usize)> {
    find_all_symbol_instance_blocks(content, reference)
        .into_iter()
        .filter_map(|(sym_start, sym_end)| {
            let sym_block = &content[sym_start..sym_end];

            let field_search = format!(r#"(property "{field}" ""#);
            let field_rel = sym_block.find(&field_search)?;
            let val_start = sym_start + field_rel + field_search.len();
            // find the closing quote of the current value
            let val_end = val_start + content[val_start..].find('"')?;
            Some((val_start, val_end))
        })
        .collect()
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_batch_connect_to_net(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pins = match args["pins"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'pins' array")),
    };

    let (content, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut inserts = String::new();
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for pin_spec in &pins {
        let reference = match pin_spec["reference"].as_str() {
            Some(r) => r,
            None => {
                errors.push("Missing 'reference' in pin spec".into());
                continue;
            }
        };
        let pin_number = match pin_spec["pin_number"].as_str() {
            Some(p) => p,
            None => {
                errors.push("Missing 'pin_number' in pin spec".into());
                continue;
            }
        };

        // A multi-unit symbol is placed as one instance PER UNIT, all sharing
        // the reference. The requested pin lives in exactly one of them and
        // must be transformed by *that* instance's placement. Taking the first
        // instance and transforming every pin by it puts the label on unit 1's
        // pin instead — silently shorting two nets together, with no error.
        let candidates: Vec<_> = instances
            .iter()
            .filter(|i| i.reference == reference)
            .collect();
        if candidates.is_empty() {
            errors.push(format!("Component '{}' not found", reference));
            continue;
        }

        let pin_ep = candidates.iter().find_map(|inst| {
            let sym = lib_syms
                .iter()
                .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id))?;
            extract_lib_pins_for_unit(sym, inst.unit)
                .into_iter()
                .find(|p| p.number == pin_number)
                .map(|p| pin_endpoint(&p, inst.pin_transform()))
        });

        match pin_ep {
            Some((px, py)) => {
                inserts.push_str(&format_net_label(&net_name, px, py, 0.0));
                added.push(json!({
                    "reference": reference,
                    "pin": pin_number,
                    "x": px,
                    "y": py
                }));
            }
            None => errors.push(format!("Pin '{}' not found on '{}'", pin_number, reference)),
        }
    }

    if !inserts.is_empty() {
        let expected = content.clone();
        let close_pos = content.rfind(')').unwrap_or(content.len());
        let edits = vec![SexpEdit::insert(close_pos, inserts)];
        let new_content = apply_edits(content, edits);
        write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    }

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "added": added,
        "added_count": added.len(),
        "errors": errors
    })))
}

/// Extract the message text out of a `CallToolResult` error, for folding a
/// single-item handler's structured error into a batch tool's `errors` list.
fn error_text(result: &CallToolResult) -> String {
    match result.content.first() {
        Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
        _ => "unknown error".to_string(),
    }
}

async fn handle_batch_place_components(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let components = match args["components"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'components' array")),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    let root_uuid = crate::tools::ensure_root_uuid(&mut sch);
    let project_name = project_name_for(&sch_path);

    let mut placed: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for comp in &components {
        let Some(lib_id) = comp["lib_id"].as_str() else {
            errors.push("Missing 'lib_id' in component spec".into());
            continue;
        };
        let (Some(x), Some(y)) = (comp["x"].as_f64(), comp["y"].as_f64()) else {
            errors.push(format!("Missing 'x'/'y' for '{}'", lib_id));
            continue;
        };
        let rotation = comp["rotation"].as_f64().unwrap_or(0.0);
        let reference = comp["reference"].as_str().unwrap_or("?");
        let value = comp["value"].as_str();
        let unit = comp["unit"].as_f64().unwrap_or(1.0) as u32;

        match place_one_component(
            &mut sch,
            &root_uuid,
            &project_name,
            lib_id,
            x,
            y,
            rotation,
            reference,
            value,
            unit,
        ) {
            Ok(v) => placed.push(v),
            Err(e) => errors.push(error_text(&e)),
        }
    }

    if !placed.is_empty() {
        sch.overwrite()?;
    }

    let mut result = CallToolResult::json(&json!({
        "placed": placed,
        "placed_count": placed.len(),
        "errors": errors
    }));
    result.is_error = placed.is_empty() && !errors.is_empty();
    Ok(result)
}

async fn handle_batch_connect_pins(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let connections = match args["connections"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'connections' array")),
    };

    let (content, tree) = read_schematic(&sch_path)?;
    let expected = content.clone();
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Resolve every endpoint from the initial tree before any wire is
    // inserted -- symbols/lib_symbols never change as wires are added, so
    // this is safe to do up front instead of re-resolving per connection.
    let mut resolved: Vec<(f64, f64, f64, f64)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for conn in &connections {
        let (Some(ref1), Some(pin1), Some(ref2), Some(pin2)) = (
            conn["ref1"].as_str(),
            conn["pin1"].as_str(),
            conn["ref2"].as_str(),
            conn["pin2"].as_str(),
        ) else {
            errors.push("Missing ref1/pin1/ref2/pin2 in connection spec".into());
            continue;
        };
        match (
            resolve_pin_endpoint(&instances, &lib_syms, ref1, pin1),
            resolve_pin_endpoint(&instances, &lib_syms, ref2, pin2),
        ) {
            (Ok((x1, y1)), Ok((x2, y2))) => resolved.push((x1, y1, x2, y2)),
            (Err(e), _) | (_, Err(e)) => errors.push(e.to_string()),
        }
    }

    // ponytail: re-parses content per wire; incremental tree edits if batches get huge.
    let mut new_content = content;
    for (x1, y1, x2, y2) in &resolved {
        new_content = route_between(new_content, *x1, *y1, *x2, *y2);
    }

    if !resolved.is_empty() {
        write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    }

    let mut result = CallToolResult::json(&json!({
        "connected_count": resolved.len(),
        "errors": errors
    }));
    result.is_error = resolved.is_empty() && !errors.is_empty();
    Ok(result)
}

async fn handle_batch_delete(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut delete_ranges: HashSet<(usize, usize)> = HashSet::new();

    // Delete by UUID — walk back from uuid node to enclosing top-level block
    if let Some(uuids) = args["uuids"].as_array() {
        for uuid_val in uuids {
            let uuid = match uuid_val.as_str() {
                Some(u) => u,
                None => continue,
            };
            let pattern = format!(r#"(uuid "{}")"#, uuid);
            match content.find(&pattern) {
                Some(uuid_pos) => {
                    match find_enclosing_direct_child_block(&content, "kicad_sch", uuid_pos) {
                        Some((block_start, block_end)) => {
                            let item = &content[block_start..block_end];
                            if !is_deletable_schematic_item(item) {
                                errors.push(format!(
                                    "UUID '{}' belongs to protected schematic structure '{}'",
                                    uuid,
                                    sexp_tag(item)
                                ));
                                continue;
                            }
                            match find_block_with_leading_whitespace(&content, block_start) {
                                Some((del_start, del_end)) => {
                                    if delete_ranges.insert((del_start, del_end)) {
                                        edits.push(SexpEdit::delete(del_start, del_end));
                                        deleted.push(uuid.to_string());
                                    }
                                }
                                None => {
                                    errors.push(format!("Cannot parse block for UUID '{}'", uuid))
                                }
                            }
                        }
                        None => errors.push(format!("Cannot locate block for UUID '{}'", uuid)),
                    }
                }
                None => errors.push(format!("UUID '{}' not found", uuid)),
            }
        }
    }

    // Delete by reference designator
    if let Some(refs) = args["references"].as_array() {
        for ref_val in refs {
            let reference = match ref_val.as_str() {
                Some(r) => r,
                None => continue,
            };
            let blocks = find_symbol_blocks(&content, reference);
            if blocks.is_empty() {
                errors.push(format!("Component '{}' not found", reference));
                continue;
            }
            // Every unit of a multi-unit part, or the whole component is not gone.
            let mut any = false;
            for (del_start, del_end) in blocks {
                if delete_ranges.insert((del_start, del_end)) {
                    edits.push(SexpEdit::delete(del_start, del_end));
                    any = true;
                }
            }
            if any {
                deleted.push(reference.to_string());
            }
        }
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "errors": errors
    })))
}

fn sexp_tag(block: &str) -> &str {
    let Some(after_open) = block.strip_prefix('(') else {
        return "";
    };
    let end = after_open
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(after_open.len());
    &after_open[..end]
}

// Blocklist of structural forms, not an allowlist of item kinds: deleting a
// drawing item (text, bus, sheet, image, polyline, …) by UUID has always
// worked and must keep working — only the schematic's skeleton is protected.
fn is_deletable_schematic_item(block: &str) -> bool {
    !matches!(
        sexp_tag(block),
        "version"
            | "generator"
            | "generator_version"
            | "uuid"
            | "paper"
            | "title_block"
            | "lib_symbols"
            | "sheet_instances"
            | "symbol_instances"
            | "embedded_fonts"
    )
}

async fn handle_bulk_move(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = args["references"].as_array().cloned().unwrap_or_default();
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
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut moved: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };

        // Every placement of this reference — a multi-unit part has one block
        // per unit, and shifting only the first would tear the part apart.
        let blocks = find_all_symbol_instance_blocks(&content, reference);
        if blocks.is_empty() {
            errors.push(format!("'{}' not found", reference));
            continue;
        }

        let mut placements: Vec<serde_json::Value> = Vec::new();
        for (sym_start, sym_end) in blocks {
            // Find first (at X Y [ROT]) inside this symbol block
            let sym_block = &content[sym_start..sym_end];
            let at_pat = "(at ";
            let at_rel = match sym_block.find(at_pat) {
                Some(r) => r,
                None => {
                    errors.push(format!("No (at) in symbol '{}'", reference));
                    continue;
                }
            };
            let at_abs = sym_start + at_rel + at_pat.len();
            let close_rel = sym_block[at_rel..].find(')').unwrap_or(0);
            let at_end = sym_start + at_rel + close_rel;

            let at_str = &content[at_abs..at_end];
            let parts: Vec<&str> = at_str.split_whitespace().collect();
            let x = parts
                .first()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let y = parts
                .get(1)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let rot = parts
                .get(2)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);

            let (new_x, new_y) = snap_point(x + dx, y + dy, 1.27);
            edits.push(SexpEdit::replace(
                at_abs,
                at_end,
                format!("{new_x} {new_y} {rot}"),
            ));
            placements.push(json!({
                "old_x": x, "old_y": y,
                "new_x": new_x, "new_y": new_y
            }));
        }

        if !placements.is_empty() {
            moved.push(json!({
                "reference": reference,
                "units": placements.len(),
                "placements": placements
            }));
        }
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved,
        "dx": dx, "dy": dy,
        "errors": errors
    })))
}

async fn handle_batch_edit(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let edits_arr = match args["edits"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'edits' array")),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut file_edits: Vec<SexpEdit> = Vec::new();
    let mut changed: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for edit_spec in &edits_arr {
        let reference = match edit_spec["reference"].as_str() {
            Some(r) => r,
            None => {
                errors.push("Missing 'reference' in edit spec".into());
                continue;
            }
        };

        let mut component_changes: Vec<String> = Vec::new();

        // Standard fields, then arbitrary extra fields from the "fields" object.
        // Each is rewritten in every unit's block, which is where a multi-unit
        // part keeps its copies of the value.
        let extra = edit_spec["fields"].as_object();
        let specs = [("Value", "value"), ("Footprint", "footprint")]
            .into_iter()
            .filter_map(|(field, key)| Some((field.to_string(), edit_spec[key].as_str()?)))
            .chain(
                extra
                    .into_iter()
                    .flatten()
                    .filter_map(|(name, val)| Some((name.clone(), val.as_str()?))),
            );

        for (field, new_val) in specs {
            let ranges = field_value_ranges(&content, reference, &field);
            if ranges.is_empty() {
                errors.push(format!("Field '{}' not found on '{}'", field, reference));
                continue;
            }
            let units = ranges.len();
            for (start, end) in ranges {
                file_edits.push(SexpEdit::replace(start, end, new_val.to_string()));
            }
            component_changes.push(if units > 1 {
                format!("{} → {} ({} units)", field, new_val, units)
            } else {
                format!("{} → {}", field, new_val)
            });
        }

        if !component_changes.is_empty() {
            changed.push(json!({
                "reference": reference,
                "changes": component_changes
            }));
        }
    }

    let new_content = apply_edits(content, file_edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "updated_count": changed.len(),
        "updated": changed,
        "errors": errors
    })))
}

async fn handle_batch_delete_components(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = match args["references"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'references' array")),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };
        let blocks = find_symbol_blocks(&content, reference);
        if blocks.is_empty() {
            errors.push(format!("Component '{}' not found", reference));
            continue;
        }
        // Every unit of a multi-unit part, or the whole component is not gone.
        for (del_start, del_end) in blocks {
            edits.push(SexpEdit::delete(del_start, del_end));
        }
        deleted.push(reference.to_string());
    }

    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "errors": errors
    })))
}

async fn handle_connect_passthrough(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net_name = match require_str(args, "net_name") {
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
    let direction = opt_str(args, "direction").unwrap_or("right");

    // Stub is 2.54mm (2×1.27 grid units)
    let stub = 2.54_f64;
    let (wire_end_x, wire_end_y, label_rot) = match direction {
        "left" => (x - stub, y, 180.0),
        "up" => (x, y - stub, 90.0),
        "down" => (x, y + stub, 270.0),
        _ => (x + stub, y, 0.0), // "right" default
    };

    let wire_sexp = format_wire(x, y, wire_end_x, wire_end_y);
    let label_sexp = format_net_label(&net_name, wire_end_x, wire_end_y, label_rot);

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let edits = vec![SexpEdit::insert(
        close_pos,
        format!("{wire_sexp}{label_sexp}"),
    )];
    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "stub_root": { "x": x, "y": y },
        "label_position": { "x": wire_end_x, "y": wire_end_y },
        "direction": direction,
        "label_rotation": label_rot
    })))
}

async fn handle_add_schematic_text(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let text = match require_str(args, "text") {
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
    let size = args["size"].as_f64().unwrap_or(1.27);
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let uuid = new_uuid();

    // Escape quotes in text content
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");

    let text_sexp = format!(
        "\n  (text \"{escaped}\"\n    (at {x} {y} {rotation})\n    \
         (effects (font (size {size} {size})))\n    (uuid \"{uuid}\")\n  )"
    );

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let edits = vec![SexpEdit::insert(close_pos, text_sexp)];
    let new_content = apply_edits(content, edits);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added": text,
        "x": x, "y": y,
        "size": size,
        "rotation": rotation,
        "uuid": uuid
    })))
}

async fn handle_get_layout(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let include_wires = args["include_wires"].as_bool().unwrap_or(true);
    let include_labels = args["include_labels"].as_bool().unwrap_or(true);

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);

    let components: Vec<serde_json::Value> = instances
        .iter()
        .map(|i| {
            json!({
                "reference": i.reference,
                "value": i.value,
                "lib_id": i.lib_id,
                "x": i.x, "y": i.y,
                "rotation": i.rotation,
                "mirror_x": i.mirror_x,
                "mirror_y": i.mirror_y
            })
        })
        .collect();

    // Bounding box over component origins
    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
    for i in &instances {
        min_x = min_x.min(i.x);
        min_y = min_y.min(i.y);
        max_x = max_x.max(i.x);
        max_y = max_y.max(i.y);
    }
    let bbox = if instances.is_empty() {
        json!({ "x_min": 0, "y_min": 0, "x_max": 0, "y_max": 0 })
    } else {
        json!({ "x_min": min_x, "y_min": min_y, "x_max": max_x, "y_max": max_y })
    };

    let mut result = json!({
        "component_count": components.len(),
        "components": components,
        "bounding_box": bbox
    });

    if include_wires {
        let wires = extract_wires(&tree);
        let wire_data: Vec<serde_json::Value> = wires
            .iter()
            .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2, "uuid": w.uuid }))
            .collect();
        result["wire_count"] = json!(wire_data.len());
        result["wires"] = json!(wire_data);
    }

    if include_labels {
        let labels = extract_labels(&tree);
        let label_data: Vec<serde_json::Value> = labels
            .iter()
            .map(|l| json!({ "net": l.net, "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
            .collect();
        result["label_count"] = json!(label_data.len());
        result["labels"] = json!(label_data);
    }

    Ok(CallToolResult::json(&result))
}

async fn handle_validate_wire_connections(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tol = args["tolerance"].as_f64().unwrap_or(0.01);

    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Collect all valid pin endpoints
    let mut pin_points: Vec<(f64, f64)> = Vec::new();
    for inst in &instances {
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            for pin in extract_lib_pins(sym) {
                pin_points.push(pin_endpoint(&pin, t));
            }
        }
    }

    let label_points: Vec<(f64, f64)> = labels.iter().map(|l| (l.x, l.y)).collect();
    // All wire endpoints as a flat list (for quick counting)
    let all_wire_eps: Vec<(f64, f64)> = wires
        .iter()
        .flat_map(|w| [(w.x1, w.y1), (w.x2, w.y2)])
        .collect();

    let is_connected = |px: f64, py: f64| -> bool {
        // Another wire endpoint at the same position (count >= 2 because px/py itself is in the list)
        let same_ep_count = all_wire_eps
            .iter()
            .filter(|(wx, wy)| points_coincident(px, py, *wx, *wy, tol))
            .count();
        if same_ep_count >= 2 {
            return true;
        }

        // T-junction: lies on the INTERIOR of another wire
        if wires.iter().any(|w| {
            point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, tol)
                && !points_coincident(px, py, w.x1, w.y1, tol)
                && !points_coincident(px, py, w.x2, w.y2, tol)
        }) {
            return true;
        }

        // Label at this point
        if label_points
            .iter()
            .any(|(lx, ly)| points_coincident(px, py, *lx, *ly, tol))
        {
            return true;
        }

        // Pin endpoint at this point
        if pin_points
            .iter()
            .any(|(ppx, ppy)| points_coincident(px, py, *ppx, *ppy, tol))
        {
            return true;
        }

        false
    };

    let mut floating: Vec<serde_json::Value> = Vec::new();
    for w in &wires {
        for (px, py) in [(w.x1, w.y1), (w.x2, w.y2)] {
            if !is_connected(px, py) {
                floating.push(json!({ "x": px, "y": py, "wire_uuid": w.uuid }));
            }
        }
    }

    Ok(CallToolResult::json(&json!({
        "valid": floating.is_empty(),
        "floating_count": floating.len(),
        "floating_endpoints": floating
    })))
}

async fn handle_validate_component_connections(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let filter_refs: Vec<String> = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let tol = 0.01_f64;

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // No-connect positions (pins with intentional no-connect markers are exempt)
    let no_connect_pts: Vec<(f64, f64)> = tree
        .find_all("no_connect")
        .iter()
        .filter_map(|n| {
            let at = n.find("at")?;
            Some((at.get_f64(1)?, at.get_f64(2)?))
        })
        .collect();

    // Build net graph so we can check connectivity. Junctions matter: a pin
    // sitting mid-wire is connected only through a junction dot, so without
    // them this validator reports false "not connected" (#104).
    let junction_pts = konnect_sexp::schematic::extract_junctions(&tree);
    let mut g = build_net_graph(&wires, &labels, &junction_pts);
    // Also build flat wire-endpoint list for direct presence checks
    let all_wire_eps: Vec<(f64, f64)> = wires
        .iter()
        .flat_map(|w| [(w.x1, w.y1), (w.x2, w.y2)])
        .collect();

    // `g.net_at` requires &mut self, so we need a `mut` closure.
    let mut has_connection = |px: f64, py: f64| -> bool {
        // Connected to a wire endpoint
        if all_wire_eps
            .iter()
            .any(|(wx, wy)| points_coincident(px, py, *wx, *wy, tol))
        {
            return true;
        }
        // A pin landing mid-wire connects only through a junction dot — KiCad's
        // netlister registers the unsplit wire at a junction point, so a dot
        // alone is enough and no wire split is required (#104).
        if junction_pts
            .iter()
            .any(|(jx, jy)| points_coincident(px, py, *jx, *jy, tol))
            && wires
                .iter()
                .any(|w| point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, tol))
        {
            return true;
        }
        // Or has a named net (label at or reachable from pin via wires)
        g.net_at(px, py).is_some()
    };

    let mut unconnected: Vec<serde_json::Value> = Vec::new();

    for inst in &instances {
        if !filter_refs.is_empty() && !filter_refs.contains(&inst.reference) {
            continue;
        }
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            for pin in extract_lib_pins(sym) {
                let (px, py) = pin_endpoint(&pin, t);

                // Skip intentional no-connects
                if no_connect_pts
                    .iter()
                    .any(|(nx, ny)| points_coincident(px, py, *nx, *ny, tol))
                {
                    continue;
                }

                if !has_connection(px, py) {
                    unconnected.push(json!({
                        "reference": inst.reference,
                        "value": inst.value,
                        "pin": pin.number,
                        "pin_name": pin.name,
                        "x": px,
                        "y": py
                    }));
                }
            }
        }
    }

    Ok(CallToolResult::json(&json!({
        "valid": unconnected.is_empty(),
        "unconnected_count": unconnected.len(),
        "unconnected_pins": unconnected
    })))
}

#[cfg(test)]
mod batch_delete_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
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
    async fn batch_delete_uuid_is_tab_indentation_safe_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-delete.kicad_sch");
        let uuid = "11111111-1111-1111-1111-111111111111";
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(wire\n\t\t(pts (xy 0 0) (xy 10 0))\n\t\t(uuid \"{uuid}\")\n\t)\n\t(text \"keep me\" (at 5 5 0) (uuid \"text\"))\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();

        let result = handle_batch_delete(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [uuid, "root", uuid]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(uuid));
        assert!(after.contains("(uuid \"root\")"));
        assert!(after.contains("keep me"));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    #[tokio::test]
    async fn batch_delete_uuid_removes_top_level_text_but_preserves_structure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch-delete-text.kicad_sch");
        let text_uuid = "22222222-2222-2222-2222-222222222222";
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20260306)\n  (generator \"eeschema\")\n  (uuid \"root\")\n  (text \"obsolete caption\"\n    (at 5 5 0)\n    (effects (font (size 1.27 1.27)))\n    (uuid \"{text_uuid}\")\n  )\n  (sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();

        let result = handle_batch_delete(
            &json!({
                "schematic": path.display().to_string(),
                "uuids": [text_uuid]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("obsolete caption"));
        assert!(after.contains("(uuid \"root\")"));
        assert!(after.contains("(sheet_instances"));
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }
}

#[cfg(test)]
mod batch_place_and_connect_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
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

    // Pre-seed lib_symbols so ensure_lib_symbol short-circuits without KiCad
    // (precedent: sch_components.rs add_schematic_component_hides_power_reference).
    const DEVICE_R: &str = "    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 0 0 0))\n      (property \"Value\" \"R\" (at 0 0 0))\n    )\n";

    fn seeded_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("place.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n{DEVICE_R}  )\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn batch_place_components_dedupes_lib_symbols() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Device:R", "x": 100.0, "y": 100.0, "reference": "R1" },
                    { "lib_id": "Device:R", "x": 110.0, "y": 100.0, "reference": "R2" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(sch.symbols.by_reference("R1").is_some());
        assert!(sch.symbols.by_reference("R2").is_some());

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.matches("(symbol \"Device:R\"").count(),
            1,
            "lib_symbols entry must not be duplicated: {after}"
        );
    }

    #[tokio::test]
    async fn batch_place_components_collects_per_item_errors() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Device:R", "x": 100.0, "y": 100.0, "reference": "R1" },
                    { "lib_id": "Nonexistent_xyzzy:Foo", "x": 110.0, "y": 100.0, "reference": "R2" },
                    { "lib_id": "Device:R", "x": 120.0, "y": 100.0, "reference": "R3" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["placed_count"], 2);
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 1);

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(sch.symbols.by_reference("R1").is_some());
        assert!(sch.symbols.by_reference("R3").is_some());
        assert!(sch.symbols.by_reference("R2").is_none());
    }

    #[tokio::test]
    async fn batch_place_components_total_failure_sets_is_error() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Nonexistent_xyzzy:Foo", "x": 100.0, "y": 100.0, "reference": "R1" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{result:?}");
    }

    /// Six single-pin instances of a synthetic part, positioned so that
    /// connecting them by pin pairs produces a T-junction on the second pair.
    fn multi_point_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let pin_def = "\t\t\t(pin passive line (at 0 0 0) (length 0)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n";
        let lib_sym = format!("\t\t(symbol \"Test:PT\"\n{pin_def}\t\t)\n");
        let inst = |reference: &str, x: f64, y: f64, uuid: &str| {
            format!(
                "\t(symbol\n\t\t(lib_id \"Test:PT\")\n\t\t(at {x} {y} 0)\n\t\t(uuid \"{uuid}\")\n\t\t(property \"Reference\" \"{reference}\"\n\t\t\t(at {x} {y} 0)\n\t\t)\n\t)\n"
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("points.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"3af69a4c-1faa-40bd-91dc-c4fc245c4cbd\")\n\t(lib_symbols\n{}\t)\n{}{}{}{}{}{})\n",
                lib_sym,
                inst("R1", 100.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000001"),
                inst("R2", 120.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000002"),
                inst("R3", 110.0, 80.0, "aaaaaaaa-0000-0000-0000-000000000003"),
                inst("R4", 110.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000004"),
                inst("R5", 200.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000005"),
                inst("R6", 220.0, 100.0, "aaaaaaaa-0000-0000-0000-000000000006"),
            ),
        )
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn batch_connect_pins_dedupes_junction_and_collects_errors() {
        // R3-R4's wire T-lands on R1-R2's wire at (110, 100) -- without the
        // STEP 1 fix, processing the third connection re-detects that same
        // T-junction from the raw wire list and inserts a second dot.
        let (_d, path) = multi_point_schematic();
        let result = handle_batch_connect_pins(
            &json!({
                "schematic": path.display().to_string(),
                "connections": [
                    { "ref1": "R1", "pin1": "1", "ref2": "R2", "pin2": "1" },
                    { "ref1": "R3", "pin1": "1", "ref2": "R4", "pin2": "1" },
                    { "ref1": "R5", "pin1": "1", "ref2": "R6", "pin2": "1" },
                    { "ref1": "Rbad", "pin1": "1", "ref2": "R6", "pin2": "1" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.matches("(junction").count(),
            1,
            "the T-junction at (110, 100) must not be re-inserted: {after}"
        );

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["connected_count"], 3);
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod midwire_pin_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
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

    /// U1 has a single pin at (100,80), sitting strictly mid-segment on a wire
    /// from (90,80) to (110,80).
    fn midwire_schematic(with_junction: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let junction = if with_junction {
            "\t(junction (at 100 80) (diameter 0) (color 0 0 0 0) (uuid \"j1\"))\n"
        } else {
            ""
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("midwire.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Test:P1\"\n\t\t\t(symbol \"P1_1_1\"\n\t\t\t\t(pin passive line (at 0 0 0) (length 2.54)\n\t\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n\t(wire\n\t\t(pts (xy 90 80) (xy 110 80))\n\t\t(uuid \"w1\")\n\t)\n{junction}\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 100 75 0)\n\t\t)\n\t)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    /// KiCad connects a pin mid-wire only through a junction dot; the
    /// validator must mirror that instead of demanding a wire endpoint.
    #[tokio::test]
    async fn midwire_pin_connects_with_junction_only() {
        for (with_junction, expect_valid) in [(true, true), (false, false)] {
            let (_d, path) = midwire_schematic(with_junction);
            let result = handle_validate_component_connections(
                &json!({ "schematic": path.display().to_string() }),
                &test_ctx(),
            )
            .await
            .unwrap();
            let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
                panic!("expected text content");
            };
            let body: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(
                body["valid"].as_bool(),
                Some(expect_valid),
                "with_junction={with_junction}: {body}"
            );
        }
    }
}

#[cfg(test)]
mod multi_unit_pin_tests {
    use crate::tools::sch_batch::tools;
    use konnect_sexp::schematic::{
        extract_lib_pins_for_unit, extract_symbol_instances, pin_endpoint, read_schematic,
    };
    use std::io::Write;

    /// Two units of one symbol, placed 15.24mm apart. Unit 1 owns pin 1, unit 2
    /// owns pin 3; both sit at local x = -7.62 in their own unit's drawing.
    const SCH: &str = r#"(kicad_sch
	(version 20241209)
	(lib_symbols
		(symbol "74xx:74HC14"
			(symbol "74HC14_1_1"
				(pin input line (at -7.62 0 0) (length 2.54)
					(name "A" (effects (font (size 1.27 1.27))))
					(number "1" (effects (font (size 1.27 1.27))))
				)
			)
			(symbol "74HC14_2_1"
				(pin input line (at -7.62 0 0) (length 2.54)
					(name "A" (effects (font (size 1.27 1.27))))
					(number "3" (effects (font (size 1.27 1.27))))
				)
			)
		)
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 100 0)
		(unit 1)
		(property "Reference" "U1" (at 100 100 0))
		(property "Value" "74HC14" (at 100 100 0))
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 115.24 0)
		(unit 2)
		(property "Reference" "U1" (at 100 115.24 0))
		(property "Value" "74HC14" (at 100 115.24 0))
	)
)
"#;

    /// The regression: resolving a pin used the FIRST instance with a matching
    /// reference, so every pin of a multi-unit part was transformed by unit 1's
    /// placement. Two nets then landed on one coordinate and were silently
    /// shorted — no error, no warning.
    #[test]
    fn each_unit_resolves_its_own_pin_position() {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let (_c, tree) = read_schematic(f.path()).unwrap();
        let instances = extract_symbol_instances(&tree);
        let lib_syms = tree
            .find("lib_symbols")
            .map(|n| n.find_all("symbol"))
            .unwrap_or_default();

        let resolve = |number: &str| -> Option<(f64, f64)> {
            instances
                .iter()
                .filter(|i| i.reference == "U1")
                .find_map(|inst| {
                    let sym = lib_syms
                        .iter()
                        .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id))?;
                    extract_lib_pins_for_unit(sym, inst.unit)
                        .into_iter()
                        .find(|p| p.number == number)
                        .map(|p| pin_endpoint(&p, inst.pin_transform()))
                })
        };

        let p1 = resolve("1").expect("unit 1 pin 1");
        let p3 = resolve("3").expect("unit 2 pin 3");

        assert!(
            (p1.1 - p3.1).abs() > 1.0,
            "unit 1 and unit 2 pins must not land on the same point \
             (got {p1:?} and {p3:?}) — that is the short this guards against"
        );
        assert!(
            (p1.1 - 100.0).abs() < 0.01,
            "unit 1 pin should sit at y=100, got {p1:?}"
        );
        assert!(
            (p3.1 - 115.24).abs() < 0.01,
            "unit 2 pin should sit at y=115.24, got {p3:?}"
        );
    }

    #[test]
    fn batch_connect_to_net_is_registered() {
        assert!(tools().iter().any(|t| t.name == "batch_connect_to_net"));
    }
}

#[cfg(test)]
mod multi_unit_field_tests {
    use super::{field_value_ranges, find_symbol_blocks};
    use konnect_sexp::writer::{apply_edits, SexpEdit};

    /// A 3-unit part plus an unrelated single-unit part. Every unit repeats the
    /// reference and carries its own copy of the shared fields, which is how
    /// eeschema writes them.
    const SCH: &str = r#"(kicad_sch
	(version 20241209)
	(lib_symbols
		(symbol "74xx:74HC14"
			(property "Reference" "U")
			(property "Footprint" "")
		)
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 100 0)
		(unit 1)
		(property "Reference" "U6" (at 100 100 0))
		(property "Value" "74HC14" (at 100 100 0))
		(property "Footprint" "" (at 100 100 0))
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 115.24 0)
		(unit 2)
		(property "Reference" "U6" (at 100 115.24 0))
		(property "Value" "74HC14" (at 100 115.24 0))
		(property "Footprint" "" (at 100 115.24 0))
	)
	(symbol
		(lib_id "74xx:74HC14")
		(at 100 130.48 0)
		(unit 7)
		(property "Reference" "U6" (at 100 130.48 0))
		(property "Value" "74HC14" (at 100 130.48 0))
		(property "Footprint" "" (at 100 130.48 0))
	)
	(symbol
		(lib_id "Device:R")
		(at 200 100 0)
		(unit 1)
		(property "Reference" "R1" (at 200 100 0))
		(property "Value" "10k" (at 200 100 0))
		(property "Footprint" "" (at 200 100 0))
	)
)
"#;

    /// The regression: field lookup stopped at the first instance, so assigning
    /// a footprint to a multi-unit part left units 2..n blank. KiCad then had
    /// one part claiming two different footprints.
    #[test]
    fn field_edit_reaches_every_unit() {
        let ranges = field_value_ranges(SCH, "U6", "Footprint");
        assert_eq!(
            ranges.len(),
            3,
            "expected one Footprint per unit: {ranges:?}"
        );

        let edits = ranges
            .iter()
            .map(|&(s, e)| SexpEdit::replace(s, e, "Package_SO:SOIC-14".to_string()))
            .collect();
        let out = apply_edits(SCH.to_string(), edits);
        assert_eq!(
            out.matches(r#"(property "Footprint" "Package_SO:SOIC-14""#)
                .count(),
            3
        );
        // The neighbouring single-unit part must be untouched.
        assert!(out.contains(r#"(property "Reference" "R1" (at 200 100 0))"#));
        assert_eq!(
            out.matches(r#"(property "Footprint" "" (at 200"#).count(),
            1
        );
    }

    #[test]
    fn single_unit_part_still_edits_once() {
        let ranges = field_value_ranges(SCH, "R1", "Value");
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn missing_field_yields_no_ranges() {
        assert!(field_value_ranges(SCH, "U6", "Datasheet").is_empty());
        assert!(field_value_ranges(SCH, "U99", "Value").is_empty());
    }

    /// Deleting one unit's block used to leave the other six behind as orphans
    /// referencing a component the caller believes is gone.
    #[test]
    fn delete_removes_every_unit() {
        let blocks = find_symbol_blocks(SCH, "U6");
        assert_eq!(blocks.len(), 3, "expected one block per unit: {blocks:?}");

        let edits = blocks
            .iter()
            .map(|&(s, e)| SexpEdit::delete(s, e))
            .collect();
        let out = apply_edits(SCH.to_string(), edits);
        assert!(
            !out.contains(r#""Reference" "U6""#),
            "no U6 unit should survive:\n{out}"
        );
        assert!(out.contains(r#""Reference" "R1""#), "R1 must survive");
        // The lib_symbols definition is not an instance and must stay.
        assert!(out.contains(r#"(symbol "74xx:74HC14""#));
    }

    /// The blocks must not overlap, or apply_edits would splice the file wrong.
    #[test]
    fn unit_blocks_are_disjoint_and_ordered() {
        let blocks = find_symbol_blocks(SCH, "U6");
        for w in blocks.windows(2) {
            assert!(w[0].1 <= w[1].0, "blocks overlap: {:?} {:?}", w[0], w[1]);
        }
    }
}
