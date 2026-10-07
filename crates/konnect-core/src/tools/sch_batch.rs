//! `sch_batch` toolset — bulk/batch operations on schematic elements.
//!
//! **Critical invariant**: every write handler performs a single file read,
//! plans or applies all mutations in memory, then commits exactly once. This
//! fixes the Python bug where `batch_connect_to_net` did N separate read/write
//! cycles.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    find_all_symbol_instance_blocks, get_path, invalid_arg, opt_f64, opt_str, opt_u32,
    require_array, require_f64, require_str, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::{points_coincident, snap_point},
    schematic::{
        extract_all_net_labels, extract_buses, extract_labels, extract_symbol_instances,
        extract_wires, find_lib_symbol, format_net_label, format_wire, pin_endpoint,
        pin_outward_direction, read_schematic, symbol_bounds_for_instance, SymbolBounds,
    },
    writer::{apply_edits, new_uuid, read_consistent, write_atomic_if_unchanged, SexpEdit},
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::sch_connectivity::{ConnectivityIndex, COINCIDENT_TOLERANCE};
// Re-use the single-item component placer and pin-to-pin router.
use super::sch_annotate::opt_string;
use super::sch_components::{
    commit_component_deletion, indexed_uuid_items, place_one_component, placed_component_readback,
    plan_component_and_item_deletions, set_property_value, ComponentDeleteTargetError,
};
use super::sch_wiring::{
    add_stub_label, add_stub_wire, resolve_pin_endpoint, resolve_placed_pin, route_between,
};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "batch_connect_to_net",
            "Connect multiple component pins to a named net. By default it adds a net label \
             at each pin endpoint with no wire. Pass stub_length, direction and label_type \
             for connect_to_net's placement, applied to every pin in the call. \
             Single file read → all edits → single file write. \
             Labels are sheet-local: a sheet instanced N times gets N independent nets. \
             Use add_power_symbol or a global_label for a rail shared by every instance, and \
             only a local label for one that must stay per-instance — power symbols and \
             global labels are one net across all sheets and instances.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "net_name": { "type": "string", "description": "Name of the net to connect pins to" },
                    "net": { "type": "string",
                        "description": "Alias of net_name, as connect_to_net names it. Giving both with different values is an error." },
                    "direction": {
                        "type": "string",
                        "description": "Direction of each pin's stub, and of its label text. 'auto' (default) \
                                        points away from the symbol body.",
                        "enum": ["auto", "right", "left", "up", "down"],
                        "default": "auto"
                    },
                    "stub_length": { "type": "number", "minimum": 0, "default": 0,
                        "description": "Length in mm of the wire stub at each pin. 0 (default) puts the \
                                        label on the pin endpoint with no wire; connect_to_net uses 2.54." },
                    "label_type": {
                        "type": "string",
                        "enum": ["net_label", "global_label"],
                        "default": "net_label"
                    },
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
                "required": ["schematic", "pins"]
            }),
            |args, ctx| async move { handle_batch_connect_to_net(args, ctx).await }
        ),
        tool!(
            "batch_add_power_symbol",
            "Tie several pins to one power net the way a schematic is drawn by hand: pins \
             that face the same way, sit on one row and lie within max_gap of a neighbour \
             get a short stub each, a bar joining the stubs, and ONE power symbol on the \
             bar. Use it for the GND or supply pins along one edge of an IC, or for a row \
             of decoupling caps (one call per rail). A pin with no neighbour gets a symbol \
             on its endpoint, as add_power_symbol does. Stacked pins share one symbol, and \
             a pin already carrying this net's power symbol is skipped. A group whose stubs \
             or bar would touch any other pin, wire, label or junction is not joined; its \
             pins get a symbol each and the group reports why. Unresolvable pins refuse \
             the whole call before writing. One file write with committed-file readback.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "power_net": { "type": "string", "description": "Net name (e.g. 'GND', '+3V3')" },
                    "pins": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Pins to tie to power_net, as {reference, pin_number}",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "pin_number": { "type": "string" }
                            },
                            "required": ["reference", "pin_number"]
                        }
                    },
                    "max_gap": { "type": "number", "minimum": 0, "default": 10.16,
                        "description": "Largest distance in mm between neighbouring pins of one group" },
                    "stub_length": { "type": "number", "exclusiveMinimum": 0, "default": 2.54,
                        "description": "Stub length in mm from each pin to the bar; a multiple of 1.27" }
                },
                "required": ["schematic", "power_net", "pins"]
            }),
            |args, ctx| async move { super::sch_wiring::handle_batch_add_power_symbol(args, ctx).await }
        ),
        tool!(
            "batch_place_components",
            "Place multiple symbols from KiCAD libraries in one write with committed-file \
             readback. Preserves every saved hierarchy instance and preflights stale metadata \
             before any placement. Copies each library Value and Footprint unless that entry \
             explicitly overrides it. Pins that land mid-segment on an existing wire gain the \
             required junction in the same atomic write; the response reports \
             junctions_added_count and junctions_pruned_count. \
             There is no auto-numbering: an omitted reference becomes the library prefix \
             plus '?' (e.g. 'R?', '#FLG?'), like an eeschema-unannotated symbol, same as \
             add_schematic_component; annotate_schematic numbers it.",
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
                                "mirror": { "type": "string", "enum": ["x", "y", "none"], "description": "Reflect the placed symbol about an axis, using eeschema's own vocabulary: 'x' negates screen-Y, 'y' negates screen-X. Omit (or 'none') for an unmirrored symbol. Mirroring is applied after rotation, and is not interchangeable with rotation 180 for a symbol whose pins are not symmetric." },
                                "reference": { "type": "string" },
                                "value": { "type": "string" },
                                "footprint": { "type": "string" },
                                "unit": { "type": "integer", "minimum": 1, "default": 1 }
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
            "Move multiple components by a uniform dx/dy offset in a single atomic file \
             write. Junction dots are re-judged where the pins moved: a dot the pins \
             leave unjustified is removed and a pin landing mid-span on a wire gains \
             one, reported as junctions_pruned_count and junctions_added_count. A \
             no-connect flag travels with the pin it protects, reported as \
             no_connects_moved; the shift is refused before writing when that pin \
             cannot be followed one-to-one.",
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
             in a single atomic file write. Missing custom properties are created on every \
             placed unit only when create_missing is true.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "create_missing": {
                        "type": "boolean",
                        "default": false,
                        "description": "Create missing custom properties on every placed unit. Built-in properties remain update-only."
                    },
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
                                    "additionalProperties": { "type": "string" },
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
                        "description": "Stub direction. 'auto' (default) points it away from \
                                        the symbol body when a pin sits at (x, y), so the label \
                                        text does not run back across the symbol; it falls back \
                                        to 'right' on a bare point.",
                        "enum": ["auto", "right", "left", "up", "down"],
                        "default": "auto"
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
                    "rotation": { "type": "number", "description": "Rotation in degrees", "default": 0 },
                    "bold": { "type": "boolean", "description": "Draw the text bold (default false).", "default": false },
                    "italic": { "type": "boolean", "description": "Draw the text italic (default false).", "default": false },
                    "thickness": { "type": "number", "description": "Stroke thickness in mm. KiCad pairs it with size - it writes 0.254 at size 1.27 and 0.4 at size 2. Omitted, KiCad picks its own." },
                    "color": {
                        "type": "array",
                        "description": "Text colour as KiCad writes it: [r, g, b] or [r, g, b, a], channels 0-255 and alpha 0-1 (default 1). Omitted, the text takes the sheet's default colour.",
                        "items": { "type": "number" },
                        "minItems": 3,
                        "maxItems": 4
                    },
                    "justify": {
                        "type": "string",
                        "description": "Alignment of the text against x/y: at most one horizontal token (left, right) and one vertical token (top, bottom), space separated. An axis you leave out is centred - KiCad has no 'center' keyword and encodes centring by omission, so 'bottom' means horizontally centred and bottom-aligned. 'center' is shorthand for centring both axes. Defaults to 'left bottom', what KiCad itself writes for a placed annotation; a centred horizontal axis can carry a long line off the page.",
                        "default": "left bottom"
                    }
                },
                "required": ["schematic", "text", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_text(args, ctx).await }
        ),
        tool!(
            "get_schematic_layout",
            "Return a compact spatial summary of the schematic: component positions, \
             transformed drawing/pin bounds (excluding free text), and optionally wire segments \
             and label locations. Reports any component whose embedded library geometry could \
             not be resolved.",
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
            "Check that every connectable pin on every component has at least one wire \
             or label connected. Symbol pins typed no_connect and pins carrying a \
             no-connect marker are exempt. Reports unconnected pins with reference, \
             pin number, electrical type, and schematic position.",
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
#[cfg(test)]
fn find_symbol_blocks(content: &str, reference: &str) -> Vec<(usize, usize)> {
    find_all_symbol_instance_blocks(content, reference)
        .into_iter()
        .filter_map(|(sym_start, _)| {
            konnect_sexp::writer::find_block_with_leading_whitespace(content, sym_start)
        })
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

/// The net a `batch_connect_to_net` call names: `net_name`, or `net` as
/// `connect_to_net` spells it. Both may be given only when they agree.
fn batch_net_name(args: &serde_json::Value) -> Result<String, CallToolResult> {
    match (opt_string(args, "net_name")?, opt_string(args, "net")?) {
        (Some(a), Some(b)) if a != b => Err(invalid_arg(
            "net",
            "conflicts with 'net_name'; give one, or the same value in both",
        )),
        (Some(name), _) | (None, Some(name)) => Ok(name),
        (None, None) => Err(invalid_arg(
            "net_name",
            "missing; give 'net_name' or its alias 'net'",
        )),
    }
}

async fn handle_batch_connect_to_net(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net_name = match batch_net_name(args) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let pins = match args["pins"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'pins' array")),
    };
    // Defaults are the batch tool's own output from before these options:
    // a local label on the pin endpoint and no wire (#701).
    let direction = opt_str(args, "direction").unwrap_or("auto");
    let stub_length = opt_f64(args, "stub_length").unwrap_or(0.0);
    let label_type = opt_str(args, "label_type").unwrap_or("net_label");
    // Plain labels on the pin keep the string splice; anything else goes
    // through the editor model connect_to_net uses.
    let is_splice = stub_length == 0.0 && label_type == "net_label";
    let is_legacy_placement = is_splice && direction == "auto";

    let (content, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let mut sch = if is_splice {
        None
    } else {
        Some(cse::Schematic::from_source(&sch_path, content.clone())?)
    };
    // Only a stub wire can pass over a pin mid-segment.
    let pin_points = if stub_length > 0.0 {
        crate::tools::all_pin_endpoints(&tree)
    } else {
        Vec::new()
    };
    let mut is_changed = false;

    let mut inserts = String::new();
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    // Label positions already carrying this net, so a second never lands on
    // the first. Seeded from the file, extended as we go.
    let mut labelled: Vec<(f64, f64)> = extract_labels(&tree)
        .iter()
        .filter(|l| l.net == net_name)
        .map(|l| (l.x, l.y))
        .collect();

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

        let (pin, t) = match resolve_placed_pin(&instances, &lib_syms, reference, pin_number) {
            Ok(p) => p,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        let (px, py) = pin_endpoint(&pin, t);
        // `auto` is pin_label_rotation's direction, so the default rotation
        // is unchanged.
        let dir = crate::tools::stub_direction(direction, Some(pin_outward_direction(&pin, t)));
        let (lx, ly) = dir.end((px, py), stub_length);
        let rotation = dir.label_rotation;

        // Symbols stack several pins on one endpoint; a label each renders as
        // a smear. They stay connected by that endpoint. Two stubs can also
        // end on one spot, so the label is judged apart from this pin's wire.
        let needs_label = !labelled
            .iter()
            .any(|(x, y)| points_coincident(*x, *y, lx, ly, 0.01));
        let needs_wire = match sch.as_ref() {
            Some(sch) if stub_length > 0.0 => !has_wire(sch, (px, py), (lx, ly)),
            _ => false,
        };
        if needs_wire {
            if let Some(sch) = sch.as_mut() {
                add_stub_wire(sch, &pin_points, (px, py), (lx, ly));
            }
        }
        if needs_label {
            if let Some(sch) = sch.as_mut() {
                add_stub_label(sch, label_type, &net_name, (lx, ly), rotation);
            } else {
                inserts.push_str(&format_net_label(&net_name, lx, ly, rotation));
            }
            labelled.push((lx, ly));
        }
        let duplicate = !needs_wire && !needs_label;
        is_changed |= !duplicate;
        let mut entry = json!({
            "reference": reference,
            "pin": pin_number,
            "x": px,
            "y": py,
            "rotation": rotation
        });
        if !is_legacy_placement {
            entry["direction"] = json!(dir.name);
            entry["label"] = json!({ "x": lx, "y": ly, "rotation": rotation });
            if needs_wire {
                entry["wire"] = json!({ "x1": px, "y1": py, "x2": lx, "y2": ly });
            }
        }
        if duplicate {
            entry["deduplicated"] = json!(true);
        }
        added.push(entry);
    }

    if is_changed {
        match sch {
            // Revision-checked against `content`: a concurrent edit refuses
            // the write rather than being overwritten.
            Some(sch) => sch.overwrite()?,
            None => {
                let expected = content.clone();
                // Labels are element class 2; symbol instances MUST come last, so a
                // splice at the file's final `)` puts them after the instances and
                // KiCad refuses the whole file (#156, same bug as add_schematic_text).
                let new_content = crate::tools::sch_wiring::insert_before_close(&content, &inserts);
                write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
            }
        }
    }

    let mut body = json!({
        "net": net_name,
        "added": added,
        "added_count": added.len(),
        "errors": errors
    });
    if !is_legacy_placement {
        body["stub_length"] = json!(stub_length);
        body["label_type"] = json!(label_type);
    }
    Ok(CallToolResult::json(&body))
}

/// Whether `sch` already has a wire between `a` and `b`, either way round.
fn has_wire(sch: &cse::Schematic, a: (f64, f64), b: (f64, f64)) -> bool {
    let at = |p: (f64, f64), q: (f64, f64)| points_coincident(p.0, p.1, q.0, q.1, 0.01);
    sch.wires
        .iter()
        .any(|w| (at(w.start, a) && at(w.end, b)) || (at(w.start, b) && at(w.end, a)))
}

/// Extract the message text out of a `CallToolResult` error, for folding a
/// single-item handler's structured error into a batch tool's `errors` list.
pub(crate) fn error_text(result: &CallToolResult) -> String {
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

    let before_source = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::from_source(&sch_path, before_source.clone())?;
    let context = match crate::tools::sheet_instance_context(&sch_path, &mut sch) {
        Ok(context) => context,
        Err(error) => return Ok(error.into_tool_result()),
    };
    if let Err(error) = crate::tools::validate_sheet_instance_state(&sch_path, &sch, &context) {
        return Ok(error.into_tool_result());
    }
    // Built once: the lib-table parse is memoised across the whole batch.
    let src = match crate::tools::library::KiCadSymbolSource::for_file(&sch_path) {
        Ok(source) => source,
        Err(error) => return Ok(error.into_tool_result()),
    };

    let mut placements = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut failures: Vec<Value> = Vec::new();

    for (index, comp) in components.iter().enumerate() {
        let Some(lib_id) = comp["lib_id"].as_str() else {
            let message = "Missing 'lib_id' in component spec".to_string();
            errors.push(message.clone());
            failures.push(json!({"index": index, "kind": "invalid_item", "message": message}));
            continue;
        };
        let (Some(x), Some(y)) = (comp["x"].as_f64(), comp["y"].as_f64()) else {
            let message = format!("Missing 'x'/'y' for '{}'", lib_id);
            errors.push(message.clone());
            failures.push(json!({"index": index, "lib_id": lib_id, "kind": "invalid_item", "message": message}));
            continue;
        };
        let rotation = comp["rotation"].as_f64().unwrap_or(0.0);
        // One malformed entry refuses only itself, as every other per-entry
        // problem in this loop does; the batch is not abandoned because a
        // caller misspelled one axis.
        let mirror = match super::sch_components::mirror_arg(comp, "mirror") {
            Ok(mirror) => mirror,
            Err(_) => {
                let message = format!(
                    "Invalid 'mirror' for '{}': expected \"x\", \"y\" or \"none\"",
                    lib_id
                );
                errors.push(message.clone());
                failures.push(json!({"index": index, "lib_id": lib_id, "kind": "invalid_item", "message": message}));
                continue;
            }
        };
        let reference = comp["reference"].as_str();
        let value = comp["value"].as_str();
        let footprint = comp["footprint"].as_str();
        let unit = match opt_u32(comp, "unit") {
            Ok(unit) => unit.unwrap_or(1),
            Err(error) => {
                let message = error_text(&error);
                errors.push(message.clone());
                failures.push(json!({"index": index, "lib_id": lib_id, "kind": "invalid_item", "message": message}));
                continue;
            }
        };

        match place_one_component(
            &mut sch,
            &context.instance_paths,
            &context.project_name,
            lib_id,
            x,
            y,
            rotation,
            mirror,
            reference,
            value,
            footprint,
            unit,
            &src,
        ) {
            Ok(placed) => {
                let (expected_x, expected_y) = snap_point(x, y, 1.27);
                placements.push(super::sch_components::ComponentTargetUnit::placement(
                    &placed.uuid,
                    &context,
                    lib_id,
                    expected_x,
                    expected_y,
                    rotation,
                    mirror,
                    &placed.reference,
                    &placed.fields,
                    unit,
                ));
            }
            Err(error) => {
                let kind = crate::mcp::error::extract_error_kind(&error)
                    .unwrap_or_else(|| "placement_failed".to_string());
                let message = error_text(&error);
                errors.push(message.clone());
                failures.push(json!({
                    "index": index,
                    "lib_id": lib_id,
                    "reference": reference.unwrap_or("?"),
                    "kind": kind,
                    "message": message
                }));
            }
        }
    }

    let mut placed = Vec::new();
    let mut junctions_added = 0;
    let mut junctions_pruned = 0;
    if !placements.is_empty() {
        let candidate = sch.to_source();
        let (candidate, added, pruned) =
            super::sch_wiring::reconcile_junctions_for_placement(&before_source, candidate);
        junctions_added = added;
        junctions_pruned = pruned;
        if let Err(error) = write_atomic_if_unchanged(&sch_path, &before_source, &candidate) {
            let result = super::mutation_outcome_uncertain(
                &sch_path,
                "batch_place_components",
                format!("schematic persistence failed: {error}"),
            );
            return Ok(crate::outcome::attach(
                result,
                crate::outcome::summary(
                    crate::outcome::OutcomeStatus::Uncertain,
                    sch_path.display().to_string(),
                    "saved_file_readback",
                    components.len(),
                    0,
                    components.len(),
                    Some(crate::outcome::inspect_before_retry()),
                ),
            ));
        }
        let committed = match super::sch_components::load_committed_component_schematic(&sch_path) {
            Ok(committed) => committed,
            Err(error) => {
                let result = super::mutation_outcome_uncertain(
                    &sch_path,
                    "batch_place_components",
                    format!("saved schematic could not be reloaded: {error}"),
                );
                return Ok(crate::outcome::attach(
                    result,
                    crate::outcome::summary(
                        crate::outcome::OutcomeStatus::Uncertain,
                        sch_path.display().to_string(),
                        "saved_file_readback",
                        components.len(),
                        0,
                        components.len(),
                        Some(crate::outcome::inspect_before_retry()),
                    ),
                ));
            }
        };
        for placement in &placements {
            match placed_component_readback(&sch_path, &committed, placement, &context) {
                Ok(result) => placed.push(result),
                Err(error) => {
                    let reason = error_text(&error);
                    let uncertain = super::mutation_outcome_uncertain(
                        &sch_path,
                        "batch_place_components",
                        format!("saved placement readback failed: {reason}"),
                    );
                    return Ok(crate::outcome::attach(
                        uncertain,
                        crate::outcome::summary(
                            crate::outcome::OutcomeStatus::Uncertain,
                            sch_path.display().to_string(),
                            "saved_file_readback",
                            components.len(),
                            placed.len(),
                            components.len().saturating_sub(placed.len()),
                            Some(crate::outcome::inspect_before_retry()),
                        ),
                    ));
                }
            }
        }
    }

    let status = if failures.is_empty() {
        crate::outcome::OutcomeStatus::Complete
    } else if placed.is_empty() {
        crate::outcome::OutcomeStatus::Failed
    } else {
        crate::outcome::OutcomeStatus::Partial
    };
    let retry = match status {
        crate::outcome::OutcomeStatus::Complete => None,
        crate::outcome::OutcomeStatus::Partial => Some(crate::outcome::retry_failed_items(
            failures
                .iter()
                .filter_map(|failure| failure["index"].as_u64().map(|index| index as usize))
                .collect(),
        )),
        crate::outcome::OutcomeStatus::Failed => Some(crate::outcome::retry_whole_request()),
        crate::outcome::OutcomeStatus::Uncertain => unreachable!(),
    };
    let mut result = CallToolResult::json(&json!({
        "placed": placed,
        "placed_count": placed.len(),
        "junctions_added_count": junctions_added,
        "junctions_pruned_count": junctions_pruned,
        "errors": errors,
        "failures": failures
    }));
    result.is_error = placed.is_empty() && !errors.is_empty();
    Ok(crate::outcome::attach(
        result,
        crate::outcome::summary(
            status,
            sch_path.display().to_string(),
            "saved_file_readback",
            components.len(),
            placed.len(),
            failures.len(),
            retry,
        ),
    ))
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
    ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    handle_structural_batch_delete(args, ctx, true).await
}

// Blocklist of structural forms, not an allowlist of item kinds: deleting a
// drawing item (text, bus, sheet, image, polyline, …) by UUID has always
// worked and must keep working — only the schematic's skeleton is protected.
fn is_deletable_schematic_tag(tag: &str) -> bool {
    !matches!(
        tag,
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

/// Edits translating every `(property …)` anchor inside the symbol block at
/// `sym_start..sym_end` by `(ddx, ddy)`.
///
/// A property's own rotation is left untouched: a translation does not turn
/// text. Block starts come from `find_block_starts`, which is string-aware, so
/// a property *value* containing `(property` cannot be mistaken for one.
fn property_translation_edits(
    content: &str,
    sym_start: usize,
    sym_end: usize,
    ddx: f64,
    ddy: f64,
) -> Vec<SexpEdit> {
    if ddx == 0.0 && ddy == 0.0 {
        return Vec::new();
    }
    let mut edits = Vec::new();
    for prop_start in konnect_sexp::writer::find_block_starts(content, "property") {
        if prop_start < sym_start || prop_start >= sym_end {
            continue;
        }
        let Some((_, prop_end)) = konnect_sexp::writer::find_balanced_block(content, prop_start)
        else {
            continue;
        };
        let prop = &content[prop_start..prop_end];
        // The property's own (at …), not one nested deeper in (effects …).
        let Some(at_rel) = prop.find("(at ") else {
            continue;
        };
        let at_abs = prop_start + at_rel + "(at ".len();
        let Some(close_rel) = prop[at_rel..].find(')') else {
            continue;
        };
        let at_end = prop_start + at_rel + close_rel;
        let parts: Vec<&str> = content[at_abs..at_end].split_whitespace().collect();
        let (Some(px), Some(py)) = (
            parts.first().and_then(|s| s.parse::<f64>().ok()),
            parts.get(1).and_then(|s| s.parse::<f64>().ok()),
        ) else {
            continue;
        };
        let rot = parts.get(2).copied().unwrap_or("0");
        edits.push(SexpEdit::replace(
            at_abs,
            at_end,
            format!(
                "{} {} {rot}",
                cse::types::fmt_f64(px + ddx),
                cse::types::fmt_f64(py + ddy)
            ),
        ));
    }
    edits
}

async fn handle_bulk_move(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = match require_array(args, "references") {
        Ok(a) => a.clone(),
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
            // Property coordinates are ABSOLUTE in .kicad_sch, so the field
            // text does not follow the symbol on its own — moving only the
            // symbol's own (at …) strands Reference and Value at the old
            // location (#202). Shift them by the delta the symbol *actually*
            // moved, which is the snapped one, or they drift relative to the
            // part. `Symbol::translate` does the same on the typed path.
            edits.extend(property_translation_edits(
                &content,
                sym_start,
                sym_end,
                new_x - x,
                new_y - y,
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

    // No-connect carry needs the symbols behind its markers (#626). Junction
    // reconciliation owns the content-to-content pin diff below (#624), so a
    // sheet without markers skips this separate planning parse entirely.
    let needs_trees = expected.contains("(no_connect");
    let tree_of = |src: &str| -> Option<konnect_sexp::SexpNode> {
        needs_trees
            .then(|| konnect_sexp::parse_sexp(src).ok())
            .flatten()
    };
    let before_tree = tree_of(&expected);

    let new_content = apply_edits(content, edits);
    let after_tree = tree_of(&new_content);

    // A no-connect belongs to the pin, not to the coordinate it was dropped
    // on, so it travels in this same write — and the reconciliation below then
    // sees it at the arrival point and adds no dot (#626).
    let carried = if expected.contains("(no_connect") {
        let (Some(before), Some(after)) = (&before_tree, &after_tree) else {
            return Ok(crate::tools::sch_wiring::no_connect_carry_unreadable(
                &sch_path,
            ));
        };
        match crate::tools::sch_wiring::plan_no_connect_carry(before, after) {
            Ok(carried) => carried,
            Err(error) => return Ok(error.into_result(&sch_path)),
        }
    } else {
        Vec::new()
    };
    let Some(new_content) =
        crate::tools::sch_wiring::apply_no_connect_carries(new_content, &carried)
    else {
        return Ok(crate::tools::sch_wiring::no_connect_carry_unwritable(
            &sch_path,
        ));
    };

    let (new_content, junctions_added, junctions_pruned) =
        crate::tools::sch_wiring::reconcile_junctions_for_placement(&expected, new_content);

    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    let moved_markers = match crate::tools::sch_wiring::observed_no_connect_moves(
        &sch_path,
        "bulk_move_schematic_components",
        &carried,
    )? {
        Ok(moved) => moved,
        Err(error) => return Ok(error),
    };

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved,
        "dx": dx, "dy": dy,
        "junctions_added_count": junctions_added,
        "junctions_pruned_count": junctions_pruned,
        "no_connects_moved_count": moved_markers.len(),
        "no_connects_moved": moved_markers,
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

    let create_missing = match args.get("create_missing") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Ok(invalid_arg("create_missing", "must be a boolean")),
    };

    let expected = read_consistent(&sch_path)?;
    let mut content = expected.clone();
    let mut changed: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut seen_assignments = HashSet::new();
    let mut updated_unit_copies = 0;
    let mut created_unit_copies = 0;

    for edit_spec in &edits_arr {
        let reference = match edit_spec["reference"].as_str() {
            Some(r) => r,
            None => {
                errors.push("Missing 'reference' in edit spec".into());
                continue;
            }
        };

        let mut component_changes: Vec<String> = Vec::new();
        let mut field_results = Vec::new();
        let mut updated_fields = BTreeSet::new();
        let mut created_fields = BTreeSet::new();

        // Standard fields, then arbitrary extra fields from the "fields" object.
        // Each is rewritten in every unit's block, which is where a multi-unit
        // part keeps its copies of the value.
        let extra = match edit_spec.get("fields") {
            None | Some(Value::Null) => None,
            Some(Value::Object(fields)) => Some(fields),
            Some(_) => {
                errors.push(format!("'fields' for '{reference}' must be an object"));
                None
            }
        };
        let mut specs: Vec<(String, String, bool)> =
            [("Value", "value"), ("Footprint", "footprint")]
                .into_iter()
                .filter_map(|(field, key)| {
                    edit_spec[key]
                        .as_str()
                        .map(|value| (field.to_string(), value.to_string(), false))
                })
                .collect();

        if let Some(extra) = extra {
            for (name, value) in extra {
                let Some(value) = value.as_str() else {
                    errors.push(format!(
                        "Field '{name}' on '{reference}' must have a string value"
                    ));
                    continue;
                };
                if name == "Reference" {
                    errors.push(format!(
                        "Field 'Reference' on '{reference}' cannot be edited through 'fields'"
                    ));
                    continue;
                }
                let may_create =
                    create_missing && !matches!(name.as_str(), "Value" | "Footprint" | "Datasheet");
                specs.push((name.clone(), value.to_string(), may_create));
            }
        }

        for (field, new_val, may_create) in specs {
            if !seen_assignments.insert((reference.to_string(), field.clone())) {
                errors.push(format!(
                    "Duplicate assignment for field '{field}' on '{reference}'"
                ));
                continue;
            }

            if !may_create && field_value_ranges(&content, reference, &field).is_empty() {
                errors.push(format!("Field '{}' not found on '{}'", field, reference));
                continue;
            }

            let (next, counts) =
                match set_property_value(&content, reference, &field, &new_val, may_create) {
                    Ok(result) => result,
                    Err(error) => {
                        errors.push(error);
                        continue;
                    }
                };
            content = next;
            updated_unit_copies += counts.updated;
            created_unit_copies += counts.added;
            if counts.updated > 0 {
                updated_fields.insert(field.clone());
            }
            if counts.added > 0 {
                created_fields.insert(field.clone());
            }
            let units = counts.updated + counts.added;
            component_changes.push(if units > 1 {
                format!("{} → {} ({} units)", field, new_val, units)
            } else {
                format!("{} → {}", field, new_val)
            });
            field_results.push(json!({
                "name": field,
                "value": new_val,
                "updated_units": counts.updated,
                "created_units": counts.added
            }));
        }

        if !component_changes.is_empty() {
            changed.push(json!({
                "reference": reference,
                "changes": component_changes,
                "fields": field_results,
                "updated_fields": updated_fields,
                "created_fields": created_fields
            }));
        }
    }

    write_atomic_if_unchanged(&sch_path, &expected, &content)?;

    Ok(CallToolResult::json(&json!({
        "updated_count": changed.len(),
        "updated_unit_copies": updated_unit_copies,
        "created_unit_copies": created_unit_copies,
        "updated": changed,
        "errors": errors
    })))
}

async fn handle_batch_delete_components(
    args: &serde_json::Value,
    ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    handle_structural_batch_delete(args, ctx, false).await
}

async fn handle_structural_batch_delete(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
    allow_uuids: bool,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = match args["references"].as_array() {
        Some(a) => a.clone(),
        None if allow_uuids => Vec::new(),
        None => return Ok(CallToolResult::error("Missing 'references' array")),
    };

    let content = read_consistent(&sch_path)?;
    let tree = match konnect_sexp::parse_sexp(&content) {
        Ok(tree) => tree,
        Err(error) => {
            return Ok(ComponentDeleteTargetError::Stale {
                target: sch_path.display().to_string(),
                reason: error.to_string(),
            }
            .into_result());
        }
    };
    let instances = extract_symbol_instances(&tree);
    let available_references = instances
        .iter()
        .map(|instance| instance.reference.clone())
        .collect::<BTreeSet<_>>();
    let mut reference_by_uuid: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for instance in &instances {
        if let Some(uuid) = &instance.uuid {
            reference_by_uuid
                .entry(uuid.clone())
                .or_default()
                .insert(instance.reference.clone());
        }
    }
    let indexed = match indexed_uuid_items(&sch_path, &content) {
        Ok(indexed) => indexed,
        Err(error) => return Ok(error.into_result()),
    };

    let mut selected_references = Vec::new();
    let mut seen_references = HashSet::new();
    let mut selected_item_uuids = Vec::new();
    let mut seen_item_uuids = HashSet::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => {
                errors.push("Component reference must be a string".to_owned());
                continue;
            }
        };
        if !available_references.contains(reference) {
            errors.push(format!("Component '{}' not found", reference));
            continue;
        }
        if seen_references.insert(reference.to_owned()) {
            selected_references.push(reference.to_owned());
        }
    }

    if allow_uuids {
        if let Some(uuids) = args["uuids"].as_array() {
            for uuid_value in uuids {
                let Some(uuid) = uuid_value.as_str() else {
                    errors.push("Schematic UUID must be a string".to_owned());
                    continue;
                };
                let Some(item) = indexed.get(uuid) else {
                    errors.push(format!("UUID '{}' not found", uuid));
                    continue;
                };
                if !is_deletable_schematic_tag(&item.kind) {
                    errors.push(format!(
                        "UUID '{}' belongs to protected schematic structure '{}'",
                        uuid, item.kind
                    ));
                    continue;
                }
                if item.kind == "symbol" {
                    let Some(references) = reference_by_uuid.get(uuid) else {
                        return Ok(ComponentDeleteTargetError::Stale {
                            target: format!("schematic symbol UUID {uuid}"),
                            reason: "the symbol has no structural reference identity".to_owned(),
                        }
                        .into_result());
                    };
                    if references.len() != 1 {
                        return Ok(ComponentDeleteTargetError::Ambiguous {
                            target: format!("schematic symbol UUID {uuid}"),
                            candidates: references.iter().cloned().collect(),
                        }
                        .into_result());
                    }
                    let reference = references.iter().next().expect("one reference").clone();
                    if seen_references.insert(reference.clone()) {
                        selected_references.push(reference);
                    }
                } else if seen_item_uuids.insert(uuid.to_owned()) {
                    selected_item_uuids.push(uuid.to_owned());
                }
            }
        }
    }

    if selected_references.is_empty() && selected_item_uuids.is_empty() {
        return Ok(ComponentDeleteTargetError::Stale {
            target: sch_path.display().to_string(),
            reason: if errors.is_empty() {
                "no schematic items were selected".to_owned()
            } else {
                errors.join("; ")
            },
        }
        .into_result());
    }

    let plan = match plan_component_and_item_deletions(
        &sch_path,
        &content,
        &selected_references,
        &selected_item_uuids,
    ) {
        Ok(plan) => plan,
        Err(error) => return Ok(error.into_result()),
    };
    let outcome = match commit_component_deletion(&sch_path, plan)? {
        Ok(outcome) => outcome,
        Err(refusal) => return Ok(refusal),
    };

    let deleted_references = selected_references
        .into_iter()
        .filter(|reference| outcome.units_by_reference.contains_key(reference))
        .collect::<Vec<_>>();
    let deleted_items = selected_item_uuids
        .into_iter()
        .filter(|uuid| outcome.item_uuids.contains(uuid))
        .collect::<Vec<_>>();
    let deleted_components = deleted_references
        .iter()
        .map(|reference| {
            let unit_uuids = &outcome.units_by_reference[reference];
            json!({
                "reference": reference,
                "deleted_units": unit_uuids.len(),
                "deleted_unit_uuids": unit_uuids
            })
        })
        .collect::<Vec<_>>();
    let deleted = deleted_references
        .iter()
        .cloned()
        .chain(deleted_items.iter().cloned())
        .collect::<Vec<_>>();

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "deleted_components": deleted_components,
        "deleted_item_uuids": deleted_items,
        "removed_no_connects_count": outcome.marker_uuids.len(),
        "removed_no_connect_uuids": outcome.marker_uuids,
        "junctions_added_count": outcome.added_junctions.len(),
        "junctions_added_uuids": outcome.added_junctions,
        "junctions_pruned_count": outcome.pruned_junctions.len(),
        "junctions_pruned_uuids": outcome.pruned_junctions,
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
    let direction = opt_str(args, "direction").unwrap_or("auto");

    let (content, tree) = read_schematic(&sch_path)?;
    let dir = crate::tools::resolve_stub_direction(direction, (x, y), &tree);

    // Stub is 2.54mm (2×1.27 grid units)
    let stub = 2.54_f64;
    let (wire_end_x, wire_end_y) = (x + dir.dx * stub, y + dir.dy * stub);

    let wire_sexp = format_wire(x, y, wire_end_x, wire_end_y);
    let label_sexp = format_net_label(&net_name, wire_end_x, wire_end_y, dir.label_rotation);

    let expected = content.clone();
    // Wires and labels are element class 2; symbol instances MUST come last.
    let new_content = crate::tools::sch_wiring::insert_before_close(
        &content,
        &format!("{wire_sexp}{label_sexp}"),
    );
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "stub_root": { "x": x, "y": y },
        "label_position": { "x": wire_end_x, "y": wire_end_y },
        "direction": dir.name,
        "label_rotation": dir.label_rotation
    })))
}

/// The font attributes KiCad writes inside `(effects (font …))`, in its own
/// order: size, thickness, bold, italic, colour.
///
/// Measured against KiCad 10's bundled demos — of 949 `(text …)` blocks, 362
/// carry `(bold yes)`, 145 a `(thickness …)`, 77 a `(color …)` and 10
/// `(italic yes)`. All of them sit inside `(font …)`, not beside it.
fn schematic_text_font(args: &serde_json::Value, size: f64) -> Result<String, String> {
    let mut font = format!("(size {size} {size})");

    if let Some(thickness) = args.get("thickness") {
        let thickness = thickness
            .as_f64()
            .ok_or_else(|| "thickness must be a number".to_string())?;
        if !(thickness.is_finite() && thickness > 0.0) {
            return Err(format!(
                "thickness must be a positive number, got {thickness}"
            ));
        }
        font.push_str(&format!(" (thickness {thickness})"));
    }
    // KiCad writes the token only when it is on, and omits it otherwise.
    if args["bold"].as_bool().unwrap_or(false) {
        font.push_str(" (bold yes)");
    }
    if args["italic"].as_bool().unwrap_or(false) {
        font.push_str(" (italic yes)");
    }
    if let Some(color) = args.get("color") {
        font.push_str(&format!(" {}", schematic_text_color(color)?));
    }

    Ok(format!("(font {font})"))
}

/// Translate a caller's colour into KiCad's `(color R G B A)`.
///
/// Accepts `[r, g, b]` or `[r, g, b, a]`. The three channels are 0–255
/// integers as KiCad writes them; alpha is 0–1 and defaults to 1, because a
/// colour given without one is meant to be seen.
fn schematic_text_color(value: &serde_json::Value) -> Result<String, String> {
    let parts = value
        .as_array()
        .ok_or_else(|| "color must be an array [r, g, b] or [r, g, b, a]".to_string())?;
    if parts.len() != 3 && parts.len() != 4 {
        return Err(format!(
            "color takes 3 or 4 values [r, g, b(, a)], got {}",
            parts.len()
        ));
    }

    let mut channels = [0u16; 3];
    for (channel, part) in channels.iter_mut().zip(parts) {
        let raw = part
            .as_f64()
            .ok_or_else(|| "color channels must be numbers".to_string())?;
        if !(0.0..=255.0).contains(&raw) || raw.fract() != 0.0 {
            return Err(format!(
                "color channels are whole numbers from 0 to 255, got {raw}"
            ));
        }
        *channel = raw as u16;
    }

    let alpha = match parts.get(3) {
        None => 1.0,
        Some(value) => {
            let alpha = value
                .as_f64()
                .ok_or_else(|| "color alpha must be a number".to_string())?;
            if !(0.0..=1.0).contains(&alpha) {
                return Err(format!("color alpha runs from 0 to 1, got {alpha}"));
            }
            alpha
        }
    };

    Ok(format!(
        "(color {} {} {} {})",
        channels[0], channels[1], channels[2], alpha
    ))
}

/// KiCad's default alignment for a placed text annotation.
///
/// Measured against KiCad 10's own demo projects: every text block in the
/// current file format carries `(justify left bottom)`.
const DEFAULT_TEXT_JUSTIFY: &str = "left bottom";

/// Translate a caller's alignment into the `(justify ...)` clause KiCad writes.
///
/// Alignment is per axis, and centring an axis means leaving its token out:
/// `left` is left-aligned and vertically centred, `bottom` is horizontally
/// centred and bottom-aligned. `"center"` centres both and so returns an empty
/// string. There is no token for it — `(justify center)` makes KiCad refuse the
/// whole file, the same way a misplaced item does.
fn schematic_text_justify(value: &str) -> Result<String, String> {
    fn claim(
        slot: &mut Option<&'static str>,
        value: &'static str,
        token: &str,
    ) -> Result<(), String> {
        if let Some(existing) = slot {
            return Err(format!(
                "justify names '{existing}' and '{token}' on the same axis - use at most one horizontal (left, right) and one vertical (top, bottom) token"
            ));
        }
        *slot = Some(value);
        Ok(())
    }

    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("center") {
        return Ok(String::new());
    }

    let mut horizontal: Option<&'static str> = None;
    let mut vertical: Option<&'static str> = None;
    for token in trimmed.split_whitespace() {
        match token.to_ascii_lowercase().as_str() {
            "left" => claim(&mut horizontal, "left", token)?,
            "right" => claim(&mut horizontal, "right", token)?,
            "top" => claim(&mut vertical, "top", token)?,
            "bottom" => claim(&mut vertical, "bottom", token)?,
            _ => {
                return Err(format!(
                    "unknown justify token '{token}' - use left, right, top, bottom, or center"
                ))
            }
        }
    }

    // KiCad writes the horizontal token first.
    let mut parts = Vec::with_capacity(2);
    parts.extend(horizontal);
    parts.extend(vertical);
    Ok(format!(" (justify {})", parts.join(" ")))
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
    let justify = args["justify"].as_str().unwrap_or(DEFAULT_TEXT_JUSTIFY);
    // Without a justify token KiCad centres the text on `x`, so a long line
    // crosses the page edge and is dropped from the PDF export while the
    // .kicad_sch still looks complete.
    let justify_sexp = match schematic_text_justify(justify) {
        Ok(v) => v,
        Err(e) => return Ok(CallToolResult::error(e)),
    };
    let font_sexp = match schematic_text_font(args, size) {
        Ok(v) => v,
        Err(e) => return Ok(CallToolResult::error(e)),
    };
    let uuid = new_uuid();

    // Escape for a KiCad quoted string. Newlines and tabs must become their
    // two-character escapes: KiCad's reader rejects a literal newline inside
    // quotes, and it fails at the *file* level — a multi-line annotation makes
    // the whole schematic unloadable with only "Failed to load schematic".
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "")
        .replace('\n', "\\n")
        .replace('\t', "\\t");

    let text_sexp = format!(
        "\n  (text \"{escaped}\"\n    (at {x} {y} {rotation})\n    \
         (effects {font_sexp}{justify_sexp})\n    (uuid \"{uuid}\")\n  )"
    );

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    // Before the first symbol instance, not at the end of the file: KiCad 10
    // requires symbol instances to come last and refuses to load a schematic
    // with a `(text …)` after them.
    let new_content = crate::tools::sch_wiring::insert_before_close(&content, &text_sexp);
    write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added": text,
        "x": x, "y": y,
        "size": size,
        "rotation": rotation,
        "justify": justify,
        "font": font_sexp,
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

    let lib_symbols = tree
        .find("lib_symbols")
        .map(|node| node.find_all("symbol"))
        .unwrap_or_default();
    let placements = instances
        .iter()
        .map(|instance| {
            let bounds = find_lib_symbol(&lib_symbols, instance)
                .and_then(|symbol| symbol_bounds_for_instance(symbol, instance));
            (instance, bounds)
        })
        .collect::<Vec<_>>();
    let bounds_json = |bounds: SymbolBounds| {
        json!({
            "x_min": bounds.min_x,
            "y_min": bounds.min_y,
            "x_max": bounds.max_x,
            "y_max": bounds.max_y,
            "width": bounds.width(),
            "height": bounds.height()
        })
    };
    let components: Vec<serde_json::Value> = placements
        .iter()
        .map(|(instance, bounds)| {
            json!({
                "reference": instance.reference,
                "value": instance.value,
                "lib_id": instance.lib_id,
                "unit": instance.unit,
                "x": instance.x, "y": instance.y,
                "rotation": instance.rotation,
                "mirror_x": instance.mirror_x,
                "mirror_y": instance.mirror_y,
                "bounds": bounds.map(&bounds_json)
            })
        })
        .collect();

    // Enclose actual placed graphics and pin extents. If a library definition
    // is missing, preserve the old origin coverage for that one instance and
    // report the unresolved reference instead of silently understating it.
    let mut overall: Option<SymbolBounds> = None;
    let mut unresolved_bounds = Vec::new();
    for (instance, bounds) in &placements {
        let bounds = bounds.unwrap_or_else(|| {
            unresolved_bounds.push(instance.reference.clone());
            SymbolBounds {
                min_x: instance.x,
                min_y: instance.y,
                max_x: instance.x,
                max_y: instance.y,
            }
        });
        match &mut overall {
            Some(overall) => {
                overall.min_x = overall.min_x.min(bounds.min_x);
                overall.min_y = overall.min_y.min(bounds.min_y);
                overall.max_x = overall.max_x.max(bounds.max_x);
                overall.max_y = overall.max_y.max(bounds.max_y);
            }
            None => overall = Some(bounds),
        }
    }
    let bbox = overall.map_or_else(
        || json!({ "x_min": 0, "y_min": 0, "x_max": 0, "y_max": 0, "width": 0, "height": 0 }),
        bounds_json,
    );

    let mut result = json!({
        "component_count": components.len(),
        "components": components,
        "bounding_box": bbox,
        "bounds_resolved": placements.len() - unresolved_bounds.len(),
        "bounds_unresolved": unresolved_bounds
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
    let buses = extract_buses(&tree);
    let labels = extract_all_net_labels(&tree);
    let index = ConnectivityIndex::build(&tree, &wires, &buses, &labels, tol);

    let floating: Vec<serde_json::Value> = index
        .floating_wire_ends()
        .into_iter()
        .map(|(x, y, wire_uuid)| json!({ "x": x, "y": y, "wire_uuid": wire_uuid }))
        .collect();

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
    let filter_refs: HashSet<String> = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let ignore_power_pins = args["ignore_power_pins"].as_bool().unwrap_or(false);
    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let buses = extract_buses(&tree);
    let labels = extract_all_net_labels(&tree);
    let index = ConnectivityIndex::build(&tree, &wires, &buses, &labels, COINCIDENT_TOLERANCE);

    let mut unconnected: Vec<serde_json::Value> = Vec::new();

    for placed in index.placed_pins() {
        if !filter_refs.is_empty() && !filter_refs.contains(&placed.reference) {
            continue;
        }
        let (px, py) = placed.at;

        // A library-declared no-connect pin is intentional by definition and
        // does not need a placed X marker (#267).
        if placed.pin.electrical_type == "no_connect" {
            continue;
        }
        if ignore_power_pins
            && matches!(
                placed.pin.electrical_type.as_str(),
                "power_in" | "power_out"
            )
        {
            continue;
        }

        // Skip intentional no-connects.
        if index.has_no_connect(px, py) {
            continue;
        }

        if !index.attaches_pin(px, py) {
            unconnected.push(json!({
                "reference": placed.reference,
                "value": placed.value,
                "pin": placed.pin.number,
                "pin_name": placed.pin.name,
                "pin_type": placed.pin.electrical_type,
                "x": px,
                "y": py
            }));
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
                eager_toolsets: false,
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
mod connectivity_safe_batch_delete_tests {
    use super::*;
    use crate::mcp::{error::extract_error_kind, protocol::ToolContent};
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    const CONNECTIVITY: &str = include_str!("../../tests/fixtures/junction_reconcile.kicad_sch");
    const ECC83: &str = include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch");

    fn context() -> ToolContext {
        ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn fixture(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("batch-delete.kicad_sch");
        std::fs::write(&path, content).unwrap();
        (directory, path)
    }

    fn body(result: &CallToolResult) -> serde_json::Value {
        assert!(!result.is_error, "{result:?}");
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        serde_json::from_str(text).unwrap()
    }

    fn has_junction(content: &str, x: f64, y: f64) -> bool {
        let tree = konnect_sexp::parse_sexp(content).unwrap();
        konnect_sexp::schematic::extract_junctions(&tree)
            .iter()
            .any(|&(jx, jy)| konnect_sexp::geometry::points_coincident(x, y, jx, jy, 0.01))
    }

    #[test]
    fn schematic_skeleton_tags_are_never_uuid_delete_targets() {
        for tag in [
            "version",
            "generator",
            "generator_version",
            "uuid",
            "paper",
            "title_block",
            "lib_symbols",
            "sheet_instances",
            "symbol_instances",
            "embedded_fonts",
        ] {
            assert!(!is_deletable_schematic_tag(tag), "{tag}");
        }
        assert!(is_deletable_schematic_tag("text"));
        assert!(is_deletable_schematic_tag("wire"));
    }

    #[tokio::test]
    async fn both_aliases_dedupe_references_and_report_missing_without_partial_units() {
        for generic in [true, false] {
            let (_directory, path) = fixture(ECC83);
            let args = json!({
                "schematic": path,
                "references": ["U1", "MISSING", "U1"]
            });
            let result = if generic {
                handle_batch_delete(&args, &context()).await.unwrap()
            } else {
                handle_batch_delete_components(&args, &context())
                    .await
                    .unwrap()
            };
            let response = body(&result);

            assert_eq!(response["deleted_count"], 1);
            assert_eq!(response["deleted"], json!(["U1"]));
            assert_eq!(response["deleted_components"][0]["deleted_units"], 3);
            assert_eq!(response["errors"], json!(["Component 'MISSING' not found"]));
            let after = konnect_sexp::schematic::read_schematic(&path).unwrap().1;
            let instances = extract_symbol_instances(&after);
            assert!(instances.iter().all(|instance| instance.reference != "U1"));
            assert!(instances.iter().any(|instance| instance.reference == "R1"));
        }
    }

    #[tokio::test]
    async fn overlapping_reference_and_unit_uuid_delete_one_whole_component() {
        let (_directory, path) = fixture(ECC83);
        let tree = konnect_sexp::schematic::read_schematic(&path).unwrap().1;
        let uuid = extract_symbol_instances(&tree)
            .into_iter()
            .find(|instance| instance.reference == "U1")
            .and_then(|instance| instance.uuid)
            .unwrap();
        let result = handle_batch_delete(
            &json!({
                "schematic": path,
                "references": ["U1"],
                "uuids": [uuid, uuid]
            }),
            &context(),
        )
        .await
        .unwrap();
        let response = body(&result);

        assert_eq!(response["deleted"], json!(["U1"]));
        assert_eq!(response["deleted_components"][0]["deleted_units"], 3);
        assert_eq!(response["deleted_item_uuids"], json!([]));
    }

    #[tokio::test]
    async fn component_batch_reconciles_junctions_and_no_connects_once() {
        for generic in [true, false] {
            let (_directory, path) = fixture(CONNECTIVITY);
            let args = json!({ "schematic": path, "references": ["R1", "R3"] });
            let result = if generic {
                handle_batch_delete(&args, &context()).await.unwrap()
            } else {
                handle_batch_delete_components(&args, &context())
                    .await
                    .unwrap()
            };
            let response = body(&result);

            assert_eq!(response["deleted_count"], 2);
            assert_eq!(response["removed_no_connects_count"], 1);
            assert_eq!(response["junctions_pruned_count"], 1);
            let committed = std::fs::read_to_string(&path).unwrap();
            assert!(!has_junction(&committed, 120.65, 139.7));
            assert!(has_junction(&committed, 120.65, 170.18));
            assert!(!committed.contains("3f9dbc19-858e-4bf8-b937-b169159de4c8"));
        }
    }

    #[tokio::test]
    async fn total_missing_or_protected_selection_is_structured_stale_and_unchanged() {
        let cases = [
            json!({ "references": ["MISSING"] }),
            json!({ "uuids": ["5a1d3bbf-65fe-4cc0-9d9e-4ec47d238186"] }),
            json!({ "uuids": ["8026d02f-ff62-464a-9ce5-e55f42254b73"] }),
        ];
        for selection in cases {
            let (_directory, path) = fixture(CONNECTIVITY);
            let mut args = selection;
            args["schematic"] = json!(path);
            let result = handle_batch_delete(&args, &context()).await.unwrap();

            assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), CONNECTIVITY);
        }
    }

    #[tokio::test]
    async fn duplicate_top_level_uuid_is_ambiguous_and_unchanged() {
        let closing = CONNECTIVITY.rfind("\n)").unwrap();
        let duplicate =
            "\t(junction\n\t\t(at 1 1)\n\t\t(uuid \"6f08a78f-7ec2-45e6-ba39-1d930be32b74\")\n\t)\n";
        let original = format!(
            "{}{}{}",
            &CONNECTIVITY[..closing + 1],
            duplicate,
            &CONNECTIVITY[closing + 1..]
        );
        let (_directory, path) = fixture(&original);
        let result = handle_batch_delete_components(
            &json!({ "schematic": path, "references": ["R1"] }),
            &context(),
        )
        .await
        .unwrap();

        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("ambiguous_target")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn stale_revision_refuses_the_whole_batch_without_overwriting() {
        let (_directory, path) = fixture(CONNECTIVITY);
        let plan = plan_component_and_item_deletions(
            &path,
            CONNECTIVITY,
            &["R1".to_owned(), "R3".to_owned()],
            &[],
        )
        .unwrap();
        let newer = CONNECTIVITY.replace("(paper \"A4\")", "(paper \"A3\")");
        std::fs::write(&path, &newer).unwrap();

        let refusal = commit_component_deletion(&path, plan)
            .unwrap()
            .expect_err("stale batch must refuse");
        assert_eq!(
            extract_error_kind(&refusal).as_deref(),
            Some("stale_target")
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), newer);
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
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    // Pre-seed lib_symbols so ensure_lib_symbol short-circuits without KiCad
    // (precedent: sch_components.rs add_schematic_component_hides_power_reference).
    const DEVICE_R: &str = "    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 0 0 0))\n      (property \"Value\" \"R\" (at 0 0 0))\n    )\n";
    const DEVICE_WITH_DEFAULTS: &str = "    (symbol \"Device:WITH_DEFAULTS\"\n      (property \"Reference\" \"U\" (at 0 0 0))\n      (property \"Value\" \"Library Default\" (at 0 0 0))\n      (property \"Footprint\" \"Package_DIP:DIP-8_W7.62mm\" (at 0 0 0) hide)\n    )\n";

    fn seeded_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("place.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n{DEVICE_R}{DEVICE_WITH_DEFAULTS}  )\n)\n"
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
        assert!(
            !after
                .lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "batch placement must not leave trailing whitespace: {after:?}"
        );
    }

    /// An entry without `reference` keeps the stock library prefix (#669).
    // Holds the env guard across awaits on purpose; see the note on
    // `sch_components::tests`.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn batch_place_without_reference_keeps_library_prefix() {
        let (dir, _env) = crate::tools::stock_reference_prefix_libraries();
        let path = dir.path().join("prefix.kicad_sch");
        std::fs::write(&path, crate::tools::blank_schematic_template()).unwrap();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Device:R", "x": 101.6, "y": 101.6 },
                    { "lib_id": "power:PWR_FLAG", "x": 127.0, "y": 101.6 }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        assert_eq!(
            crate::tools::placed_references(&path),
            vec![
                ("Device:R".into(), "R?".into(), vec!["R?".into()]),
                (
                    "power:PWR_FLAG".into(),
                    "#FLG?".into(),
                    vec!["#FLG?".into()]
                ),
            ]
        );
    }

    #[tokio::test]
    async fn batch_placement_uses_library_fields_and_per_entry_overrides() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Device:WITH_DEFAULTS", "x": 100.0, "y": 100.0, "reference": "U1" },
                    { "lib_id": "Device:WITH_DEFAULTS", "x": 120.0, "y": 100.0, "reference": "U2",
                      "value": "Explicit Value", "footprint": "Package_DIP:DIP-14_W7.62mm" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected JSON text")
        };
        let response: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(response["placed"][0]["fields"]["Value"], "Library Default");
        assert_eq!(
            response["placed"][0]["fields"]["Footprint"],
            "Package_DIP:DIP-8_W7.62mm"
        );
        assert_eq!(response["placed"][1]["fields"]["Value"], "Explicit Value");
        assert_eq!(
            response["placed"][1]["fields"]["Footprint"],
            "Package_DIP:DIP-14_W7.62mm"
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
        assert_eq!(parsed["failures"][0]["index"], 1);
        assert_eq!(parsed["outcome"]["status"], "partial");
        assert_eq!(parsed["outcome"]["requested"], 3);
        assert_eq!(parsed["outcome"]["completed"], 2);
        assert_eq!(parsed["outcome"]["failed"], 1);
        assert_eq!(parsed["outcome"]["retry"]["scope"], "failed_items");
        assert_eq!(parsed["outcome"]["retry"]["item_indexes"], json!([1]));

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(sch.symbols.by_reference("R1").is_some());
        assert!(sch.symbols.by_reference("R3").is_some());
        assert!(sch.symbols.by_reference("R2").is_none());
    }

    /// Per-component `mirror` reaches the file, and a misspelled axis refuses
    /// only its own entry — the batch's other placements still land, as they
    /// do for every other per-item problem here. Silently placing the third
    /// symbol unmirrored would be the failure the argument exists to end
    /// (#450).
    #[tokio::test]
    async fn batch_place_components_mirrors_per_component_and_refuses_a_bad_axis() {
        let (_d, path) = seeded_schematic();
        let result = handle_batch_place_components(
            &json!({
                "schematic": path.display().to_string(),
                "components": [
                    { "lib_id": "Device:R", "x": 100.0, "y": 100.0, "reference": "R1" },
                    { "lib_id": "Device:R", "x": 110.0, "y": 100.0, "reference": "R2", "mirror": "y" },
                    { "lib_id": "Device:R", "x": 120.0, "y": 100.0, "reference": "R3", "mirror": "Y" }
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
        assert!(sch.symbols.by_reference("R3").is_none());
        assert_eq!(
            sch.symbols.by_reference("R1").unwrap().mirror.as_deref(),
            None
        );
        assert_eq!(
            sch.symbols.by_reference("R2").unwrap().mirror.as_deref(),
            Some("y")
        );
    }

    #[tokio::test]
    async fn batch_place_components_total_failure_sets_is_error() {
        let (_d, path) = seeded_schematic();
        let before = std::fs::read(&path).unwrap();
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
        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text,
            _ => panic!("expected text"),
        };
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["outcome"]["status"], "failed");
        assert_eq!(parsed["outcome"]["retry"]["scope"], "whole_request");
        assert_eq!(std::fs::read(&path).unwrap(), before);
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
                eager_toolsets: false,
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

    fn typed_pin_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("typed-pins.kicad_sch");
        std::fs::write(
            &path,
            r#"(kicad_sch
  (version 20260306)
  (generator "eeschema")
  (uuid "root")
  (lib_symbols
    (symbol "Test:Typed"
      (symbol "Typed_1_1"
        (pin no_connect line (at 0 0 0) (length 0)
          (name "NC" (effects (font (size 1.27 1.27))))
          (number "1" (effects (font (size 1.27 1.27)))))
        (pin power_in line (at 0 2.54 0) (length 2.54)
          (name "VDD" (effects (font (size 1.27 1.27))))
          (number "2" (effects (font (size 1.27 1.27)))))
        (pin output line (at 0 5.08 0) (length 2.54)
          (name "OUT" (effects (font (size 1.27 1.27))))
          (number "3" (effects (font (size 1.27 1.27))))))
      (symbol "Typed_2_1"
        (pin input line (at 0 7.62 0) (length 2.54)
          (name "OTHER_UNIT" (effects (font (size 1.27 1.27))))
          (number "4" (effects (font (size 1.27 1.27))))))))
  (symbol
    (lib_id "Test:Typed")
    (at 100 80 0)
    (unit 1)
    (uuid "u1")
    (property "Reference" "U1" (at 100 75 0))
    (property "Value" "Typed" (at 100 77 0)))
  (sheet_instances (path "/" (page "1"))))
"#,
        )
        .unwrap();
        (dir, path)
    }

    async fn validate_components_json(
        schematic: &std::path::Path,
        ignore_power_pins: bool,
    ) -> serde_json::Value {
        let result = handle_validate_component_connections(
            &json!({
                "schematic": schematic.display().to_string(),
                "ignore_power_pins": ignore_power_pins
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn declared_no_connect_and_other_unit_pins_are_not_reported() {
        let (_dir, path) = typed_pin_schematic();
        let body = validate_components_json(&path, false).await;
        let pins = body["unconnected_pins"].as_array().unwrap();

        assert_eq!(body["unconnected_count"], 2);
        assert_eq!(pins[0]["pin"], "2");
        assert_eq!(pins[0]["pin_type"], "power_in");
        assert_eq!(pins[1]["pin"], "3");
        assert_eq!(pins[1]["pin_type"], "output");
        assert!(pins.iter().all(|pin| pin["pin"] != "1"));
        assert!(pins.iter().all(|pin| pin["pin"] != "4"));
    }

    #[tokio::test]
    async fn ignore_power_pins_option_is_effective() {
        let (_dir, path) = typed_pin_schematic();
        let body = validate_components_json(&path, true).await;
        let pins = body["unconnected_pins"].as_array().unwrap();

        assert_eq!(body["unconnected_count"], 1);
        assert_eq!(pins[0]["pin"], "3");
        assert_eq!(pins[0]["pin_type"], "output");
    }
}

#[cfg(test)]
mod connect_to_net_orientation_tests {
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
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// One pin per edge, plus two pins stacked on one endpoint. Placed at
    /// (100, 100): west tip (89.84, 100), east (110.16, 100), north
    /// (100, 89.84), south (100, 110.16), stack (89.84, 94.92).
    fn quad_schematic() -> (tempfile::TempDir, std::path::PathBuf) {
        let pin = |x: f64, y: f64, angle: f64, name: &str, number: &str| {
            format!(
                "        (pin passive line (at {x} {y} {angle}) (length 2.54)\n\
                 \x20         (name \"{name}\") (number \"{number}\"))\n"
            )
        };
        let body = format!(
            "{}{}{}{}{}{}",
            pin(-10.16, 0.0, 0.0, "WEST", "1"),
            pin(10.16, 0.0, 180.0, "EAST", "2"),
            pin(0.0, 10.16, 270.0, "NORTH", "3"),
            pin(0.0, -10.16, 90.0, "SOUTH", "4"),
            pin(-10.16, 5.08, 0.0, "GND", "5"),
            pin(-10.16, 5.08, 0.0, "GND", "6"),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quad.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  \
                 (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  \
                 (lib_symbols\n    (symbol \"Test:QUAD\"\n      (symbol \"QUAD_1_1\"\n\
                 {body}      )\n    )\n  )\n  (symbol\n    (lib_id \"Test:QUAD\")\n    \
                 (at 100 100 0)\n    (unit 1)\n    \
                 (property \"Reference\" \"U1\" (at 100 90 0))\n    \
                 (property \"Value\" \"QUAD\" (at 100 110 0))\n  )\n)\n"
            ),
        )
        .unwrap();
        (dir, path)
    }

    /// The `(at x y ROT)` and justify of the label for `net`.
    fn label_of(body: &str, net: &str) -> (String, String) {
        let start = body
            .find(&format!("(label \"{net}\""))
            .expect("label present");
        let block = &body[start..];
        let end = block.find("(uuid").unwrap_or(block.len());
        let block = &block[..end];
        let at = {
            let i = block.find("(at ").expect("at present") + 4;
            block[i..][..block[i..].find(')').unwrap()]
                .trim()
                .to_string()
        };
        let justify = match block.find("(justify ") {
            Some(j) => {
                let rest = &block[j + "(justify ".len()..];
                rest[..rest.find(')').unwrap()].trim().to_string()
            }
            None => "<none>".to_string(),
        };
        (at, justify)
    }

    async fn connect(path: &std::path::Path, net: &str, pin_number: &str) -> String {
        let result = handle_batch_connect_to_net(
            &json!({
                "schematic": path.display().to_string(),
                "net_name": net,
                "pins": [{ "reference": "U1", "pin_number": pin_number }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        std::fs::read_to_string(path).unwrap()
    }

    /// The reported bug: a left-edge pin's label was written at rotation 0,
    /// so its text ran east across the body, over the pin names.
    #[tokio::test]
    async fn a_left_edge_pin_gets_a_label_reading_away_from_the_body() {
        let (_d, path) = quad_schematic();
        let after = connect(&path, "SWDIO", "1").await;
        assert_eq!(
            label_of(&after, "SWDIO"),
            ("89.84 100 180".into(), "right bottom".into())
        );
        assert!(konnect_sexp::parse_sexp(&after).is_ok(), "{after}");
        assert!(
            !after
                .lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "label insertion must not leave the symbol line's indent behind: {after:?}"
        );
    }

    #[tokio::test]
    async fn a_right_edge_pin_keeps_reading_east() {
        let (_d, path) = quad_schematic();
        let after = connect(&path, "XTAL", "2").await;
        assert_eq!(
            label_of(&after, "XTAL"),
            ("110.16 100 0".into(), "left bottom".into())
        );
    }

    /// eeschema never turns a pin-anchored label sideways, whichever way a
    /// vertical pin faces — see `pin_label_rotation`.
    #[tokio::test]
    async fn vertical_pins_keep_their_label_horizontal() {
        let (_d, path) = quad_schematic();
        let after = connect(&path, "TOP", "3").await;
        assert_eq!(label_of(&after, "TOP").0, "100 89.84 0");
        let after = connect(&path, "BOTTOM", "4").await;
        assert_eq!(label_of(&after, "BOTTOM").0, "100 110.16 0");
    }

    /// Pins on one endpoint are already connected, so one label serves them
    /// all; superimposed copies render as a smear.
    #[tokio::test]
    async fn stacked_pins_share_a_single_label() {
        let (_d, path) = quad_schematic();
        let result = handle_batch_connect_to_net(
            &json!({
                "schematic": path.display().to_string(),
                "net_name": "GND",
                "pins": [
                    { "reference": "U1", "pin_number": "5" },
                    { "reference": "U1", "pin_number": "6" }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        // Both pins are reported connected — the second is not an error.
        assert_eq!(parsed["added_count"], 2);
        assert_eq!(parsed["errors"].as_array().unwrap().len(), 0);
        assert_eq!(parsed["added"][1]["deduplicated"], json!(true));

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.matches("(label \"GND\"").count(), 1, "{after}");
    }

    /// Re-running must not stack a second label on the first.
    #[tokio::test]
    async fn re_connecting_the_same_pin_adds_no_second_label() {
        let (_d, path) = quad_schematic();
        connect(&path, "SWDIO", "1").await;
        let after = connect(&path, "SWDIO", "1").await;
        assert_eq!(after.matches("(label \"SWDIO\"").count(), 1, "{after}");
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

#[cfg(test)]
mod multi_unit_handler_tests {
    use super::*;
    use crate::tools::{ServerConfig, ToolContext};
    use konnect_sexp::schematic::{extract_symbol_instances, read_schematic, SymbolInstance};
    use std::io::Write;
    use std::sync::Arc;

    const ECC83: &str = include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch");

    fn fixture_file() -> tempfile::NamedTempFile {
        let mut schematic = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        schematic.write_all(ECC83.as_bytes()).unwrap();
        schematic.flush().unwrap();
        schematic
    }

    fn instances(path: &std::path::Path) -> Vec<SymbolInstance> {
        let (_, tree) = read_schematic(path).unwrap();
        extract_symbol_instances(&tree)
    }

    async fn call(
        path: &std::path::Path,
        tool: &str,
        mut args: serde_json::Value,
    ) -> serde_json::Value {
        args["schematic"] = json!(path.to_str().unwrap());
        let definition = tools()
            .into_iter()
            .find(|tool_def| tool_def.name == tool)
            .unwrap();
        let context = ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        );
        let result = (definition.handler)(&args, Arc::new(context))
            .await
            .unwrap();
        assert!(!result.is_error, "{tool} failed: {result:?}");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn bulk_move_moves_every_ecc83_unit_and_no_neighbor() {
        let schematic = fixture_file();
        let before = instances(schematic.path());
        let result = call(
            schematic.path(),
            "bulk_move_schematic_components",
            json!({ "references": ["U1"], "dx": 12.7, "dy": 2.54 }),
        )
        .await;
        assert_eq!(result["moved_count"], 1);
        assert_eq!(result["moved"][0]["units"], 3);

        let after = instances(schematic.path());
        for old in before.iter().filter(|instance| instance.reference == "U1") {
            let new = after
                .iter()
                .find(|instance| instance.uuid == old.uuid)
                .expect("every ECC83 unit remains placed");
            assert!((new.x - old.x - 12.7).abs() < 1e-9, "unit {} x", old.unit);
            assert!((new.y - old.y - 2.54).abs() < 1e-9, "unit {} y", old.unit);
        }
        let old_r1 = before
            .iter()
            .find(|instance| instance.reference == "R1")
            .unwrap();
        let new_r1 = after
            .iter()
            .find(|instance| instance.reference == "R1")
            .unwrap();
        assert_eq!((new_r1.x, new_r1.y), (old_r1.x, old_r1.y));
    }

    #[tokio::test]
    async fn batch_edit_updates_every_ecc83_unit() {
        let schematic = fixture_file();
        let original_r1 = instances(schematic.path())
            .into_iter()
            .find(|instance| instance.reference == "R1")
            .unwrap()
            .value;
        let result = call(
            schematic.path(),
            "batch_edit_schematic_components",
            json!({
                "edits": [{
                    "reference": "U1",
                    "value": "ECC83-TEST",
                    "footprint": "Package_DIP:DIP-9_W7.62mm"
                }]
            }),
        )
        .await;
        assert_eq!(result["updated_count"], 1);

        let after = instances(schematic.path());
        let units: Vec<_> = after
            .iter()
            .filter(|instance| instance.reference == "U1")
            .collect();
        assert_eq!(units.len(), 3);
        assert!(units.iter().all(|instance| instance.value == "ECC83-TEST"));
        assert!(units
            .iter()
            .all(|instance| instance.footprint == "Package_DIP:DIP-9_W7.62mm"));
        assert_eq!(
            after
                .iter()
                .find(|instance| instance.reference == "R1")
                .unwrap()
                .value,
            original_r1
        );
        assert_eq!(result["updated_unit_copies"], 6);
        assert_eq!(result["created_unit_copies"], 0);
        assert_eq!(result["updated"][0]["fields"][0]["updated_units"], 3);
        assert_eq!(result["updated"][0]["fields"][1]["updated_units"], 3);
    }

    #[tokio::test]
    async fn batch_edit_missing_custom_field_is_opt_in_and_multi_unit_safe() {
        let schematic = fixture_file();
        let original = std::fs::read_to_string(schematic.path()).unwrap();
        let refused = call(
            schematic.path(),
            "batch_edit_schematic_components",
            json!({
                "edits": [{"reference": "U1", "fields": {"Tolerance": "1%"}}]
            }),
        )
        .await;
        assert_eq!(refused["updated_count"], 0);
        assert_eq!(refused["created_unit_copies"], 0);
        assert_eq!(
            refused["errors"],
            json!(["Field 'Tolerance' not found on 'U1'"])
        );
        assert_eq!(std::fs::read_to_string(schematic.path()).unwrap(), original);

        let created = call(
            schematic.path(),
            "batch_edit_schematic_components",
            json!({
                "create_missing": true,
                "edits": [{"reference": "U1", "fields": {"Tolerance": "1%"}}]
            }),
        )
        .await;
        assert_eq!(created["updated_count"], 1);
        assert_eq!(created["updated_unit_copies"], 0);
        assert_eq!(created["created_unit_copies"], 3);
        assert_eq!(
            created["updated"][0]["created_fields"],
            json!(["Tolerance"])
        );
        assert_eq!(created["updated"][0]["fields"][0]["created_units"], 3);
        let committed = std::fs::read_to_string(schematic.path()).unwrap();
        assert_eq!(
            committed.matches(r#"(property "Tolerance" "1%""#).count(),
            3
        );
        let units = instances(schematic.path())
            .into_iter()
            .filter(|instance| instance.reference == "U1")
            .collect::<Vec<_>>();
        let blocks = find_all_symbol_instance_blocks(&committed, "U1");
        for ((start, end), unit) in blocks.into_iter().zip(units) {
            let block = &committed[start..end];
            let property = &block[block.find(r#"(property "Tolerance""#).unwrap()..];
            assert!(
                property.contains(&format!("(at {} {} 0)", unit.x, unit.y)),
                "unit {} field was not anchored at its own placement",
                unit.unit
            );
        }

        let repeated = call(
            schematic.path(),
            "batch_edit_schematic_components",
            json!({
                "create_missing": true,
                "edits": [{"reference": "U1", "fields": {"Tolerance": "2%"}}]
            }),
        )
        .await;
        assert_eq!(repeated["updated_unit_copies"], 3);
        assert_eq!(repeated["created_unit_copies"], 0);
        let committed = std::fs::read_to_string(schematic.path()).unwrap();
        assert_eq!(
            committed.matches(r#"(property "Tolerance" "2%""#).count(),
            3
        );
        assert!(!committed.contains(r#"(property "Tolerance" "1%""#));
    }

    #[tokio::test]
    async fn batch_edit_creates_escaped_fields_for_multiple_components() {
        let schematic = fixture_file();
        let result = call(
            schematic.path(),
            "batch_edit_schematic_components",
            json!({
                "create_missing": true,
                "edits": [
                    {"reference": "U1", "fields": {
                        "Vendor Note": "line 1\n\"quoted\"\\path",
                        "Tolerance": "5%"
                    }},
                    {"reference": "R1", "fields": {"Tolerance": "1%"}}
                ]
            }),
        )
        .await;
        assert_eq!(result["updated_count"], 2);
        assert_eq!(result["created_unit_copies"], 7);
        assert_eq!(result["errors"], json!([]));
        let committed = std::fs::read_to_string(schematic.path()).unwrap();
        assert_eq!(
            committed.matches(r#"(property "Tolerance" "5%""#).count(),
            3
        );
        assert_eq!(
            committed.matches(r#"(property "Tolerance" "1%""#).count(),
            1
        );
        assert_eq!(
            committed
                .matches(r#"(property "Vendor Note" "line 1\n\"quoted\"\\path""#)
                .count(),
            3
        );
        read_schematic(schematic.path()).expect("escaped property remains valid KiCad syntax");
    }

    #[tokio::test]
    async fn batch_edit_rejects_non_string_reference_and_duplicate_assignments() {
        let schematic = fixture_file();
        let original = std::fs::read_to_string(schematic.path()).unwrap();
        let result = call(
            schematic.path(),
            "batch_edit_schematic_components",
            json!({
                "create_missing": true,
                "edits": [
                    {"reference": "U1", "fields": {
                        "Count": 3,
                        "Reference": "U99",
                        "Value": "first"
                    }},
                    {"reference": "U1", "value": "second"}
                ]
            }),
        )
        .await;
        assert_eq!(result["updated_count"], 1);
        assert_eq!(result["created_unit_copies"], 0);
        assert_eq!(result["updated_unit_copies"], 3);
        let errors = result["errors"].as_array().unwrap();
        assert!(errors
            .iter()
            .any(|error| error.as_str().unwrap().contains("must have a string value")));
        assert!(errors
            .iter()
            .any(|error| error.as_str().unwrap().contains("cannot be edited")));
        assert!(errors
            .iter()
            .any(|error| error.as_str().unwrap().contains("Duplicate assignment")));

        let committed = std::fs::read_to_string(schematic.path()).unwrap();
        assert_eq!(
            committed.matches(r#"(property "Reference" "U1""#).count(),
            3
        );
        assert_eq!(committed.matches(r#"(property "Value" "first""#).count(), 3);
        assert!(!committed.contains(r#"(property "Value" "second""#));
        assert_ne!(
            committed, original,
            "the valid first assignment still applies"
        );
    }

    #[tokio::test]
    async fn batch_edit_repairs_a_partially_populated_multi_unit_field() {
        let schematic = fixture_file();
        call(
            schematic.path(),
            "batch_edit_schematic_components",
            json!({
                "create_missing": true,
                "edits": [{"reference": "U1", "fields": {"Tolerance": "1%"}}]
            }),
        )
        .await;

        let content = std::fs::read_to_string(schematic.path()).unwrap();
        let first_unit = find_all_symbol_instance_blocks(&content, "U1")[0];
        let relative = content[first_unit.0..first_unit.1]
            .find(r#"(property "Tolerance" "1%""#)
            .unwrap();
        let property_start = first_unit.0 + relative;
        let (_, property_end) = konnect_sexp::writer::find_balanced_block(&content, property_start)
            .expect("the inserted property is balanced");
        let partial = apply_edits(
            content,
            vec![SexpEdit::delete(property_start, property_end)],
        );
        std::fs::write(schematic.path(), partial).unwrap();

        let repaired = call(
            schematic.path(),
            "batch_edit_schematic_components",
            json!({
                "create_missing": true,
                "edits": [{"reference": "U1", "fields": {"Tolerance": "2%"}}]
            }),
        )
        .await;
        assert_eq!(repaired["updated_unit_copies"], 2);
        assert_eq!(repaired["created_unit_copies"], 1);
        assert_eq!(repaired["updated"][0]["fields"][0]["updated_units"], 2);
        assert_eq!(repaired["updated"][0]["fields"][0]["created_units"], 1);
        let committed = std::fs::read_to_string(schematic.path()).unwrap();
        assert_eq!(
            committed.matches(r#"(property "Tolerance" "2%""#).count(),
            3
        );
    }

    #[tokio::test]
    async fn batch_edit_preserves_crlf_when_creating_fields() {
        let mut schematic = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        let crlf = ECC83.replace('\n', "\r\n");
        schematic.write_all(crlf.as_bytes()).unwrap();
        schematic.flush().unwrap();

        let result = call(
            schematic.path(),
            "batch_edit_schematic_components",
            json!({
                "create_missing": true,
                "edits": [{"reference": "U1", "fields": {"Tolerance": "1%"}}]
            }),
        )
        .await;
        assert_eq!(result["created_unit_copies"], 3);
        let committed = std::fs::read_to_string(schematic.path()).unwrap();
        assert!(!committed.replace("\r\n", "").contains('\n'));
        assert_eq!(
            committed.matches("\r\n\t\t(property \"Tolerance\"").count(),
            3
        );
    }

    #[test]
    fn batch_edit_candidate_refuses_a_concurrent_revision() {
        let schematic = fixture_file();
        let expected = read_consistent(schematic.path()).unwrap();
        let (candidate, counts) =
            set_property_value(&expected, "U1", "Tolerance", "1%", true).unwrap();
        assert_eq!(counts.added, 3);

        let external = format!("{expected}\n; external edit\n");
        std::fs::write(schematic.path(), &external).unwrap();
        let error = write_atomic_if_unchanged(schematic.path(), &expected, &candidate).unwrap_err();
        assert!(matches!(error, konnect_sexp::SexpError::Conflict { .. }));
        assert_eq!(std::fs::read_to_string(schematic.path()).unwrap(), external);
    }

    #[tokio::test]
    async fn both_reference_delete_tools_remove_every_ecc83_unit() {
        for tool in ["batch_delete", "batch_delete_schematic_components"] {
            let schematic = fixture_file();
            let result = call(schematic.path(), tool, json!({ "references": ["U1"] })).await;
            assert_eq!(result["deleted_count"], 1, "{tool}: {result}");

            let after = instances(schematic.path());
            assert!(
                after.iter().all(|instance| instance.reference != "U1"),
                "{tool} left an ECC83 unit"
            );
            assert!(
                after.iter().any(|instance| instance.reference == "R1"),
                "{tool} removed a neighbor"
            );
            let content = std::fs::read_to_string(schematic.path()).unwrap();
            assert!(
                content.contains(r#"(symbol "ecc83-pp:ECC83""#),
                "{tool} removed the embedded library definition"
            );
        }
    }
}

#[cfg(test)]
mod add_text_placement_tests {
    use super::{schematic_text_justify, tools};
    use crate::mcp::protocol::CallToolResult;
    use crate::tools::ToolContext;
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    const SCH: &str = "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\")\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 100 75 0)\n\t\t)\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n";

    async fn add_text_inner(text: &str, justify: Option<&str>) -> (String, CallToolResult) {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let def = tools()
            .into_iter()
            .find(|t| t.name == "add_schematic_text")
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
        let router = Arc::new(crate::router::ToolRouter::new());
        let ctx = Arc::new(ToolContext::new(cfg, router));
        let mut args = json!({
            "schematic": f.path().to_str().unwrap(),
            "text": text, "x": 30.0, "y": 114.3
        });
        if let Some(j) = justify {
            args["justify"] = json!(j);
        }
        let result = (def.handler)(&args, ctx).await.unwrap();
        (std::fs::read_to_string(f.path()).unwrap(), result)
    }

    async fn add_text(text: &str) -> String {
        add_text_inner(text, None).await.0
    }

    async fn add_text_justified(text: &str, justify: &str) -> String {
        add_text_inner(text, Some(justify)).await.0
    }

    async fn add_text_result(text: &str, justify: &str) -> CallToolResult {
        add_text_inner(text, Some(justify)).await.1
    }

    /// The regression: the text was spliced in at the file's last `)`, which
    /// puts it *after* the symbol instances and `sheet_instances`. KiCad 10
    /// requires instances last and rejects the whole file — "Failed to load
    /// schematic", with no hint as to which element is misplaced.
    #[tokio::test]
    async fn text_goes_before_the_symbol_instances() {
        let out = add_text("hello").await;
        let text_at = out.find("(text \"hello\"").expect("text written");
        let sym_at = out.find("(symbol\n\t\t(lib_id").expect("instance present");
        let sheets_at = out
            .find("(sheet_instances")
            .expect("sheet_instances present");
        assert!(
            text_at < sym_at && text_at < sheets_at,
            "text must precede symbol instances (text {text_at}, symbol {sym_at})"
        );
        // and it must land after lib_symbols, not inside it
        assert!(text_at > out.find("(lib_symbols").unwrap());
    }

    /// The other half of the same incident: the content was written with the
    /// newline as a literal byte inside the quoted string. KiCad wants the
    /// two-character escape and refuses the file otherwise.
    #[tokio::test]
    async fn multiline_text_escapes_its_newlines() {
        let out = add_text("line one\nline two").await;
        let text_at = out
            .find(r#"(text "line one\nline two""#)
            .expect("newline must be written as an escape, not a raw byte");
        assert!(text_at < out.find("(symbol\n\t\t(lib_id").unwrap());
    }

    #[tokio::test]
    async fn quotes_backslashes_and_tabs_are_escaped() {
        let out = add_text("a \"b\" c\\d\te").await;
        assert!(out.contains(r#"(text "a \"b\" c\\d\te""#), "got:\n{out}");
    }

    async fn add_text_formatted(text: &str, extra: serde_json::Value) -> (String, CallToolResult) {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let def = tools()
            .into_iter()
            .find(|t| t.name == "add_schematic_text")
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
        let router = Arc::new(crate::router::ToolRouter::new());
        let ctx = Arc::new(ToolContext::new(cfg, router));
        let mut args = json!({
            "schematic": f.path().to_str().unwrap(),
            "text": text, "x": 30.0, "y": 114.3
        });
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        let result = (def.handler)(&args, ctx).await.unwrap();
        (std::fs::read_to_string(f.path()).unwrap(), result)
    }

    /// Nothing asked for, nothing written: a plain call keeps the font block it
    /// always had, so existing callers see no change.
    #[tokio::test]
    async fn an_unformatted_call_writes_only_the_size() {
        let out = add_text_formatted("plain", json!({})).await.0;
        assert!(
            out.contains("(effects (font (size 1.27 1.27)) (justify left bottom))"),
            "got:\n{out}"
        );
    }

    /// KiCad's own order inside `(font …)`: size, thickness, bold, italic,
    /// colour. Measured against its bundled demos.
    #[tokio::test]
    async fn the_font_tokens_are_written_in_kicads_order() {
        let out = add_text_formatted(
            "styled",
            json!({ "bold": true, "italic": true, "thickness": 0.4, "color": [0, 194, 194, 1] }),
        )
        .await
        .0;

        assert!(
            out.contains(
                "(font (size 1.27 1.27) (thickness 0.4) (bold yes) (italic yes) (color 0 194 194 1))"
            ),
            "got:\n{out}"
        );
    }

    /// KiCad writes the flag only when it is on, never `(bold no)`.
    #[tokio::test]
    async fn a_false_flag_writes_no_token() {
        let out = add_text_formatted("plain", json!({ "bold": false, "italic": false }))
            .await
            .0;
        assert!(!out.contains("bold"), "got:\n{out}");
        assert!(!out.contains("italic"), "got:\n{out}");
    }

    /// A colour without alpha is meant to be seen, so it defaults to opaque
    /// rather than to KiCad's transparent zero.
    #[tokio::test]
    async fn a_colour_without_alpha_is_opaque() {
        let out = add_text_formatted("red", json!({ "color": [255, 0, 0] }))
            .await
            .0;
        assert!(out.contains("(color 255 0 0 1)"), "got:\n{out}");
    }

    #[tokio::test]
    async fn a_malformed_colour_is_refused() {
        for bad in [
            json!({ "color": [255, 0] }),
            json!({ "color": [300, 0, 0] }),
            json!({ "color": [255, 0, 0, 4] }),
            json!({ "color": [1.5, 0, 0] }),
            json!({ "color": "red" }),
        ] {
            let (out, result) = add_text_formatted("bad", bad.clone()).await;
            assert!(result.is_error, "{bad} must be refused");
            assert!(
                !out.contains("(text \"bad\""),
                "a refused call must write nothing:\n{out}"
            );
        }
    }

    #[tokio::test]
    async fn a_non_positive_thickness_is_refused() {
        let (_, result) = add_text_formatted("bad", json!({ "thickness": 0.0 })).await;
        assert!(result.is_error);
    }

    /// The reported defect: with no `(justify ...)` KiCad centres the text on
    /// `x`, so a long line crosses the left page edge and vanishes from the PDF
    /// export while the .kicad_sch still reads as complete.
    #[tokio::test]
    async fn text_is_left_aligned_by_default() {
        let out = add_text("hello").await;
        assert!(
            out.contains("(effects (font (size 1.27 1.27)) (justify left bottom))"),
            "got:\n{out}"
        );
    }

    #[tokio::test]
    async fn justify_is_written_as_kicad_orders_it() {
        let out = add_text_justified("hello", "top right").await;
        assert!(
            out.contains("(justify right top)"),
            "horizontal token comes first, got:\n{out}"
        );
    }

    /// KiCad has no `center` token: centred text is text with no justify at
    /// all. This is the pre-change behaviour, still reachable on request.
    #[tokio::test]
    async fn center_writes_no_justify_token() {
        let out = add_text_justified("hello", "center").await;
        assert!(!out.contains("(justify"), "got:\n{out}");
        assert!(
            out.contains("(effects (font (size 1.27 1.27)))"),
            "got:\n{out}"
        );
    }

    #[tokio::test]
    async fn an_unknown_token_is_refused() {
        let result = add_text_result("hello", "middle").await;
        assert!(
            result.is_error,
            "an unknown alignment must not be guessed at"
        );
    }

    #[tokio::test]
    async fn two_tokens_on_one_axis_are_refused() {
        let result = add_text_result("hello", "left right").await;
        assert!(result.is_error);
    }

    #[test]
    fn justify_parsing_covers_the_shapes_kicad_writes() {
        // The five forms present in KiCad 10's own demo projects.
        for (input, expected) in [
            ("left bottom", " (justify left bottom)"),
            ("right bottom", " (justify right bottom)"),
            ("left", " (justify left)"),
            // One axis alone: the other is centred by omission.
            ("bottom", " (justify bottom)"),
            ("left top", " (justify left top)"),
            ("right", " (justify right)"),
        ] {
            assert_eq!(schematic_text_justify(input).unwrap(), expected, "{input}");
        }
        assert_eq!(schematic_text_justify("center").unwrap(), "");
        assert_eq!(
            schematic_text_justify("  BOTTOM  Left ").unwrap(),
            " (justify left bottom)"
        );
        assert!(schematic_text_justify("sideways").is_err());
        assert!(schematic_text_justify("top bottom").is_err());
    }
}

/// `add_schematic_text` was not the only handler splicing at the file's last
/// `)`. `batch_connect_to_net` and `connect_to_net` did the same, and a label
/// or wire written after the symbol instances breaks the file exactly as #156
/// described — KiCad reports only "Failed to load schematic", and because the
/// file no longer loads, `kicad-cli erc` leaves a stale report in place.
#[cfg(test)]
mod insert_order_tests {
    use crate::tools::sch_wiring::insert_before_close;

    const SCH: &str = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Device:R\")\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(uuid \"u1\")\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n";

    #[test]
    fn labels_land_before_the_symbol_instances() {
        let out = insert_before_close(SCH, "\n  (label \"NET\" (at 10 10 0))");
        let label = out.find("(label \"NET\"").expect("label written");
        let inst = out.find("(symbol\n\t\t(lib_id").expect("instance present");
        assert!(
            label < inst,
            "a label after the instances makes the file unloadable:\n{out}"
        );
        assert!(
            !out.contains(")(symbol"),
            "elements must not be glued: {out}"
        );
        assert!(
            !out.lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "insertion must consume the target line's indent: {out:?}"
        );
    }

    /// The old splice point, for contrast: the file's final `)` sits after
    /// everything, so anything inserted there lands last.
    #[test]
    fn the_old_final_paren_splice_would_land_after_the_instances() {
        let close = SCH.rfind(')').unwrap();
        let inst = SCH.find("(symbol\n\t\t(lib_id").unwrap();
        assert!(
            close > inst,
            "this test is meaningless if the last paren precedes the instances"
        );
    }
}

#[cfg(test)]
mod bulk_move_no_connect_tests {
    use super::*;
    use crate::mcp::{error::extract_error_kind, protocol::ToolContent};
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    const CARRY: &str = include_str!("../../tests/fixtures/no_connect_carry_kicad10.kicad_sch");
    const R2_MARKER: &str = "1a251b9a-30be-43fa-bf5f-04706ed82788";

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

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("carry.kicad_sch");
        std::fs::write(&path, CARRY).unwrap();
        (directory, path)
    }

    fn text(result: &CallToolResult) -> serde_json::Value {
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        serde_json::from_str(text).unwrap()
    }

    /// A bulk shift reconciles junctions like the single-component move, so it
    /// owes the markers the same trip (#626).
    #[tokio::test]
    async fn a_bulk_shift_carries_the_marker_and_adds_no_dot() {
        let (_directory, path) = fixture();
        // R2 pin 1 goes from (158.75, 156.21) to (154.94, 160.02), mid-span on
        // the NETB wire — the same arrival the single-component move lands on.
        let result = handle_bulk_move(
            &json!({ "schematic": path, "references": ["R2"], "dx": -3.81, "dy": 3.81 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        let response = text(&result);

        assert_eq!(response["no_connects_moved_count"], 1, "{response}");
        assert_eq!(
            response["no_connects_moved"][0]["uuid"], R2_MARKER,
            "{response}"
        );
        assert_eq!(
            response["no_connects_moved"][0]["to"],
            json!({ "x": 154.94, "y": 160.02 }),
            "{response}"
        );
        assert_eq!(response["junctions_added_count"], 0, "{response}");

        let committed = std::fs::read_to_string(&path).unwrap();
        assert!(committed.contains("(at 154.94 160.02)"), "{committed}");
    }

    #[tokio::test]
    async fn a_bulk_shift_that_cannot_follow_a_marker_writes_nothing() {
        let (_directory, path) = fixture();
        // R3 alone leaves R5 pin 1 behind under the marker they share.
        let result = handle_bulk_move(
            &json!({ "schematic": path, "references": ["R3"], "dx": -12.7, "dy": 0 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{result:?}");
        assert_eq!(
            extract_error_kind(&result),
            Some("ambiguous_target".to_string()),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CARRY);
    }
}

/// #202: `bulk_move` shifted only the symbol's own `(at …)`. Property `(at …)`
/// coordinates are absolute in `.kicad_sch`, so Reference and Value text
/// stayed at the old location while the symbol moved away. The typed path
/// (`move_schematic_component` → `Symbol::translate`) always translated the
/// properties too — this was the second, text-based implementation that never
/// got the fix.
#[cfg(test)]
mod bulk_move_field_tests {
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
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// One symbol with Reference and Value at eeschema-style offsets beside
    /// it. Reference carries a rotation, which must survive the move.
    const SCH: &str = "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 101.6 101.6 0)\n\t\t(unit 1)\n\t\t(uuid \"sym-1\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 105.232 100.33 90)\n\t\t\t(effects (font (size 1.27 1.27)))\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 105.232 102.87 0)\n\t\t)\n\t\t(instances\n\t\t\t(project \"p\"\n\t\t\t\t(path \"/root\" (reference \"R1\") (unit 1))\n\t\t\t)\n\t\t)\n\t)\n\t(sheet_instances (path \"/\" (page \"1\")))\n)\n";

    /// The placed symbol's `(at …)` and each property's, read back from the
    /// written file. Numeric, so a float-formatting change can't break the
    /// test and a wrong coordinate can't hide behind one.
    fn positions(sch: &str) -> (Vec<f64>, Vec<(String, Vec<f64>)>) {
        let tree = konnect_sexp::parse_sexp(sch).expect("parses");
        let symbol = tree
            .children()
            .unwrap()
            .iter()
            .find(|n| n.head() == Some("symbol") && n.find("lib_id").is_some())
            .expect("placed symbol");
        let at_of = |n: &konnect_sexp::SexpNode| -> Vec<f64> {
            let at = n.find("at").expect("(at …)");
            (1..at.children().unwrap().len())
                .filter_map(|i| at.get_f64(i))
                .collect()
        };
        let props = symbol
            .find_all("property")
            .into_iter()
            .map(|p| {
                (
                    p.get(1).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    at_of(p),
                )
            })
            .collect();
        (at_of(symbol), props)
    }

    async fn bulk_move(dx: f64, dy: f64) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("move.kicad_sch");
        std::fs::write(&path, SCH).unwrap();
        let result = handle_bulk_move(
            &json!({ "schematic": path.to_str().unwrap(),
                     "references": ["R1"], "dx": dx, "dy": dy }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        std::fs::read_to_string(&path).unwrap()
    }

    /// #120 end to end: a dot the pin vacates is pruned, and the response says
    /// so. #315's wire-carrying move is gated on exactly this judgement.
    ///
    /// Fixture is eeschema's own output (`kicad-cli sch upgrade`), not a
    /// hand-written sheet — R1's pin sits mid-span on a wire at
    /// (120.65, 139.7) and earns the dot there. Moving R1 away must strand it.
    #[tokio::test]
    async fn bulk_move_prunes_the_junction_its_pin_vacates() {
        const SHEET: &str = include_str!("../../tests/fixtures/junction_reconcile.kicad_sch");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.kicad_sch");
        std::fs::write(&path, SHEET).unwrap();
        let dots = |p: &std::path::Path| -> usize {
            std::fs::read_to_string(p)
                .unwrap()
                .matches("(junction")
                .count()
        };
        assert_eq!(dots(&path), 3, "fixture starts with three dots");

        let result = handle_bulk_move(
            &json!({ "schematic": path.to_str().unwrap(),
                     "references": ["R1"], "dx": 0.0, "dy": -20.32 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("(at 120.65 139.7)"),
            "the dot R1's pin left must be pruned: {after}"
        );
        // The T and the bus tee are untouched — only R1's point was in scope.
        assert_eq!(dots(&path), 2, "exactly one dot removed");
        assert!(after.contains("(at 120.65 170.18)"), "the T survives");
        assert!(after.contains("(at 260.35 140)"), "the bus tee survives");

        let body = format!("{:?}", result.content);
        assert!(
            body.contains("junctions_pruned_count"),
            "the response must report what it did: {body}"
        );
    }

    /// Every property keeps its offset from the symbol — which is the same as
    /// saying it moved by whatever the symbol actually moved.
    async fn assert_fields_follow(dx: f64, dy: f64) {
        let (before_sym, before_props) = positions(SCH);
        let after_src = bulk_move(dx, dy).await;
        let (after_sym, after_props) = positions(&after_src);

        // The handler snaps to the 1.27 grid, so the effective delta is not
        // necessarily the requested one — the fields must follow the real one.
        let (mdx, mdy) = (after_sym[0] - before_sym[0], after_sym[1] - before_sym[1]);
        assert_eq!(before_props.len(), after_props.len());
        for ((name, before), (after_name, after)) in before_props.iter().zip(&after_props) {
            assert_eq!(name, after_name, "property order preserved");
            assert!(
                (after[0] - (before[0] + mdx)).abs() < 1e-6
                    && (after[1] - (before[1] + mdy)).abs() < 1e-6,
                "'{name}' must move with the symbol (delta {mdx}, {mdy}): \
                 {before:?} -> {after:?}\n{after_src}"
            );
            // A property's own rotation is independent of a translation.
            assert_eq!(
                before.get(2),
                after.get(2),
                "'{name}' rotation must not change"
            );
        }
        assert!(konnect_sexp::parse_sexp(&after_src).is_ok());
    }

    #[tokio::test]
    async fn field_text_moves_with_the_symbol() {
        // On-grid delta: symbol lands exactly where asked.
        assert_fields_follow(12.7, 2.54).await;
    }

    #[tokio::test]
    async fn fields_follow_the_snapped_delta_not_the_requested_one() {
        // Off-grid delta: the symbol snaps, so the fields must move by the
        // snapped amount or they drift relative to the part.
        assert_fields_follow(1.0, 0.0).await;
    }

    /// A negative move exercises the same path in the other direction.
    #[tokio::test]
    async fn field_text_follows_a_negative_move() {
        assert_fields_follow(-25.4, -12.7).await;
    }
}

#[cfg(test)]
mod bulk_move_coordinate_text_tests {
    use super::*;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    const SCH: &str = include_str!("../../tests/fixtures/single_pin_nets.kicad_sch");

    /// #744: the snapped grid point is written as KiCad writes it. 132 × 1.27
    /// is `167.64000000000001` in `f64`, 110 × 1.27 is exactly `139.7`, so
    /// `#PWR002` exposed the noise and `R8` is the control. Expected text is
    /// what `kicad-cli sch upgrade --force` writes for the moved sheet.
    #[tokio::test]
    async fn bulk_move_writes_kicad_coordinate_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("move.kicad_sch");
        std::fs::write(&path, SCH).unwrap();
        let context = ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        );
        let result = handle_bulk_move(
            &json!({ "schematic": path.to_str().unwrap(),
                     "references": ["#PWR002", "R8"], "dx": -5.08, "dy": 0.0 }),
            &context,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        // The symbol's own (at …), the first in its block; its fields share
        // the same point and already went through `fmt_f64`.
        let symbol_at = |reference: &str| {
            let (start, end) = find_all_symbol_instance_blocks(&after, reference)[0];
            let block = &after[start..end];
            let at = block.find("(at ").unwrap();
            block[at..at + block[at..].find(')').unwrap() + 1].to_string()
        };
        assert_eq!(symbol_at("#PWR002"), "(at 45.72 167.64 0)");
        assert_eq!(symbol_at("R8"), "(at 54.61 139.7 90)");
        // KiCad never writes more than six decimals.
        let noisy: Vec<&str> = after
            .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
            .filter(|token| token.parse::<f64>().is_ok())
            .filter(|token| {
                token
                    .split_once('.')
                    .is_some_and(|(_, frac)| frac.len() > 6)
            })
            .collect();
        assert!(noisy.is_empty(), "unrounded coordinates written: {noisy:?}");

        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        let placement = |reference: &str| {
            body["moved"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["reference"] == reference)
                .unwrap()["placements"][0]
                .clone()
        };
        assert_eq!(
            placement("#PWR002"),
            json!({ "old_x": 50.8, "old_y": 167.64, "new_x": 45.72, "new_y": 167.64 })
        );
        assert_eq!(
            placement("R8"),
            json!({ "old_x": 59.69, "old_y": 139.7, "new_x": 54.61, "new_y": 139.7 })
        );
    }
}

#[cfg(test)]
mod power_symbol_connection_tests {
    use super::*;
    use crate::tools::{ServerConfig, ToolContext};
    use std::io::Write;
    use std::sync::Arc;

    /// A `power:GND` symbol dropped straight onto R1's pin 2 — no wire between
    /// them, which is how KiCad itself draws a decoupling ground. Pin 1 is
    /// genuinely unconnected.
    const SCH: &str = include_str!("../../tests/fixtures/power_symbol_on_pin.kicad_sch");

    /// The regression: the graph knew only labels, so a pin whose entire
    /// connection is a power symbol was reported unconnected.
    #[tokio::test]
    async fn a_pin_under_a_power_symbol_is_connected() {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let ctx = ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        );
        let result = handle_validate_component_connections(
            &json!({ "schematic": f.path().to_str().unwrap() }),
            &ctx,
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        let s: serde_json::Value = serde_json::from_str(text).unwrap();

        let unconnected: Vec<(&str, &str)> = s["unconnected_pins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["reference"].as_str().unwrap(), p["pin"].as_str().unwrap()))
            .collect();
        assert_eq!(unconnected, vec![("R1", "1")], "only pin 1 floats: {s}");
    }
}

#[cfg(test)]
mod layout_bounds_tests {
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
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// The retained nodes are from KiCad 10's stock Device:R definition; the
    /// surrounding schematic is reduced to what this read-only query needs.
    fn schematic() -> &'static str {
        r#"(kicad_sch
	(version 20260206)
	(generator "eeschema")
	(lib_symbols
		(symbol "Device:R"
			(symbol "R_0_1"
				(rectangle
					(start -1.016 -2.54)
					(end 1.016 2.54)
					(stroke (width 0.254) (type default))
					(fill (type none))
				)
			)
			(symbol "R_1_1"
				(pin passive line
					(at 0 3.81 270)
					(length 1.27)
					(name "" (effects (font (size 1.27 1.27))))
					(number "1" (effects (font (size 1.27 1.27))))
				)
				(pin passive line
					(at 0 -3.81 90)
					(length 1.27)
					(name "" (effects (font (size 1.27 1.27))))
					(number "2" (effects (font (size 1.27 1.27))))
				)
			)
		)
	)
	(symbol
		(lib_id "Device:R")
		(at 100 50 0)
		(unit 1)
		(uuid "r1")
		(property "Reference" "R1" (at 102 50 90))
		(property "Value" "10k" (at 100 50 90))
	)
)
"#
    }

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn schematic_layout_bounds_enclose_graphics_and_pin_tips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("layout.kicad_sch");
        std::fs::write(&path, schematic()).unwrap();

        let result = handle_get_layout(
            &json!({
                "schematic": path.to_string_lossy(),
                "include_wires": false,
                "include_labels": false
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let result = response_json(&result);

        assert_eq!(result["bounds_resolved"], 1);
        assert_eq!(result["bounds_unresolved"], json!([]));
        assert_eq!(result["bounding_box"]["x_min"], 98.984);
        assert_eq!(result["bounding_box"]["x_max"], 101.016);
        assert_eq!(result["bounding_box"]["y_min"], 46.19);
        assert_eq!(result["bounding_box"]["y_max"], 53.81);
        assert_eq!(result["components"][0]["bounds"], result["bounding_box"]);
        assert_ne!(
            result["bounding_box"]["x_min"], 100.0,
            "an origin-only box reproduces the old false result"
        );
    }

    /// A real eeschema save (KiCad's ecc83 demo): U1 is placed as three units
    /// of the embedded dual triode — two identical triode units and one
    /// heater unit with different library geometry. Every placement must get
    /// its own resolved bounds from its OWN unit's drawing, or a multi-unit
    /// component reports one unit's box three times.
    #[tokio::test]
    async fn every_placed_unit_gets_bounds_from_its_own_geometry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ecc83.kicad_sch");
        std::fs::write(
            &path,
            include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch"),
        )
        .unwrap();

        let result =
            handle_get_layout(&json!({ "schematic": path.to_string_lossy() }), &test_ctx())
                .await
                .unwrap();
        let result = response_json(&result);

        assert_eq!(result["bounds_unresolved"], json!([]), "{result}");
        let u1_boxes: Vec<(f64, f64)> = result["components"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|component| component["reference"] == "U1")
            .map(|component| {
                let bounds = &component["bounds"];
                assert!(
                    !bounds.is_null(),
                    "every U1 placement resolves bounds: {component}"
                );
                (
                    bounds["width"].as_f64().unwrap(),
                    bounds["height"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(u1_boxes.len(), 3, "three placed units of U1");
        let distinct: std::collections::BTreeSet<String> = u1_boxes
            .iter()
            .map(|(w, h)| format!("{w:.3}x{h:.3}"))
            .collect();
        assert!(
            distinct.len() >= 2,
            "the heater unit's geometry differs from the triodes', so one \
             shared box means unit selection is broken: {u1_boxes:?}"
        );
    }

    #[tokio::test]
    async fn unresolved_geometry_is_named_and_its_origin_remains_covered() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("unresolved.kicad_sch");
        let source = schematic().replace(
            "\n)\n",
            "\n\t(symbol\n\t\t(lib_id \"Missing:Part\")\n\t\t(at 120 60 0)\n\t\t(unit 1)\n\t\t(uuid \"u1\")\n\t\t(property \"Reference\" \"U1\")\n\t)\n)\n",
        );
        std::fs::write(&path, source).unwrap();

        let result = handle_get_layout(&json!({"schematic": path.to_string_lossy()}), &test_ctx())
            .await
            .unwrap();
        let result = response_json(&result);

        assert_eq!(result["bounds_resolved"], 1);
        assert_eq!(result["bounds_unresolved"], json!(["U1"]));
        assert_eq!(result["components"][1]["bounds"], serde_json::Value::Null);
        assert_eq!(result["bounding_box"]["x_max"], 120.0);
        assert_eq!(result["bounding_box"]["y_max"], 60.0);
    }
}

#[cfg(test)]
mod batch_connect_options_tests {
    //! `batch_connect_to_net`'s stub, direction and label options (#701),
    //! against a KiCad-resaved sheet. Expected positions are what `main`
    //! wrote before the change; see the fixture README.
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use konnect_sexp::schematic::LabelKind;
    use std::sync::Arc;

    const PINS: [(&str, &str); 5] = [
        ("U1", "5"),
        ("U1", "8"),
        ("U1", "4"),
        ("U2", "5"),
        ("R1", "1"),
    ];

    fn ctx() -> ToolContext {
        ToolContext::new(ServerConfig::default(), Arc::new(ToolRouter::new()))
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        fixture_named("batch_connect_kicad10.kicad_sch")
    }

    fn fixture_named(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sheet.kicad_sch");
        std::fs::copy(src, &path).unwrap();
        (dir, path)
    }

    fn pins() -> Value {
        json!(PINS
            .iter()
            .map(|(r, n)| json!({ "reference": r, "pin_number": n }))
            .collect::<Vec<_>>())
    }

    fn body(result: &CallToolResult) -> Value {
        serde_json::from_str(&error_text(result)).unwrap()
    }

    async fn batch(path: &std::path::Path, options: Value) -> CallToolResult {
        let mut args = json!({ "schematic": path.display().to_string(), "pins": pins() });
        for (k, v) in options.as_object().unwrap() {
            args[k] = v.clone();
        }
        handle_batch_connect_to_net(&args, &ctx()).await.unwrap()
    }

    /// Wires, labels (kind, position, rotation) and junctions, rounded and
    /// sorted, so two files compare by what they draw rather than by UUIDs.
    fn drawing(path: &std::path::Path) -> Vec<String> {
        let (_, tree) = read_schematic(path).unwrap();
        let r = |v: f64| format!("{:.3}", v);
        let mut out: Vec<String> = extract_wires(&tree)
            .iter()
            .map(|w| format!("wire {} {} {} {}", r(w.x1), r(w.y1), r(w.x2), r(w.y2)))
            .chain(extract_labels(&tree).iter().map(|l| {
                format!(
                    "{:?} {} {} {} {}",
                    l.kind,
                    l.net,
                    r(l.x),
                    r(l.y),
                    r(l.rotation)
                )
            }))
            .chain(
                konnect_sexp::schematic::extract_junctions(&tree)
                    .iter()
                    .map(|j| format!("junction {} {}", r(j.0), r(j.1))),
            )
            .collect();
        out.sort();
        out
    }

    fn keys(v: &Value) -> BTreeSet<&str> {
        v.as_object().unwrap().keys().map(|k| k.as_str()).collect()
    }

    fn close(a: &Value, b: f64) -> bool {
        (a.as_f64().unwrap() - b).abs() < 1e-6
    }

    /// Omitting every new option, or passing its default, writes what `main`
    /// wrote: a local label on each pin endpoint, no wire, and the same
    /// response keys.
    #[tokio::test]
    async fn defaults_keep_the_legacy_output() {
        // (x, y, rotation) of each pin's label, as main wrote them.
        let promised = [
            (116.84, 93.98, 0.0),
            (101.6, 86.36, 0.0),
            (101.6, 116.84, 0.0),
            (162.56, 93.98, 180.0),
            (59.69, 63.5, 180.0),
        ];
        for options in [
            json!({ "net_name": "N" }),
            json!({ "net_name": "N", "stub_length": 0, "direction": "auto", "label_type": "net_label" }),
        ] {
            let (_d, path) = fixture();
            let result = batch(&path, options.clone()).await;
            assert!(!result.is_error, "{result:?}");
            let body = body(&result);
            assert_eq!(
                keys(&body),
                BTreeSet::from(["net", "added", "added_count", "errors"]),
                "{options}"
            );
            for (entry, (x, y, rot)) in body["added"].as_array().unwrap().iter().zip(promised) {
                assert_eq!(
                    keys(entry),
                    BTreeSet::from(["reference", "pin", "x", "y", "rotation"])
                );
                assert!(close(&entry["x"], x) && close(&entry["y"], y), "{entry}");
                assert!(close(&entry["rotation"], rot), "{entry}");
            }

            let (_, tree) = read_schematic(&path).unwrap();
            // Only the fixture's own wire: no stub by default.
            assert_eq!(extract_wires(&tree).len(), 1, "{options}");
            let labels = extract_labels(&tree);
            assert_eq!(labels.len(), 5);
            assert!(labels.iter().all(|l| l.kind == LabelKind::NetLabel));
            for (x, y, rot) in promised {
                assert!(
                    labels
                        .iter()
                        .any(|l| points_coincident(l.x, l.y, x, y, 1e-6) && l.rotation == rot),
                    "{options}: no label at ({x}, {y}) rot {rot}"
                );
            }
        }
    }

    /// Each explicit option set draws exactly what one `connect_to_net`
    /// call per pin draws, and the positions are the ones main's
    /// `connect_to_net` wrote.
    #[tokio::test]
    async fn explicit_options_draw_what_connect_to_net_draws() {
        let single = crate::tools::sch_wiring::tools()
            .into_iter()
            .find(|t| t.name == "connect_to_net")
            .unwrap();
        // (options, first pin's promised stub end, label kind, junctions).
        // U1.5's 2.54 mm stub ends mid-span on the fixture's wire, which
        // needs a junction; the upward stub misses it.
        let cases = [
            (
                json!({ "stub_length": 2.54 }),
                (119.38, 93.98),
                LabelKind::NetLabel,
                1,
            ),
            (
                json!({ "stub_length": 2.54, "label_type": "global_label" }),
                (119.38, 93.98),
                LabelKind::GlobalLabel,
                1,
            ),
            (
                json!({ "stub_length": 5.08, "direction": "up" }),
                (116.84, 88.9),
                LabelKind::NetLabel,
                0,
            ),
        ];
        for (options, (ex, ey), kind, junctions) in cases {
            let (_d, batch_path) = fixture();
            let mut args = options.clone();
            args["net"] = json!("N");
            let result = batch(&batch_path, args.clone()).await;
            assert!(!result.is_error, "{result:?}");
            let body = body(&result);
            let first = &body["added"][0];
            assert!(
                close(&first["label"]["x"], ex) && close(&first["label"]["y"], ey),
                "{first}"
            );
            assert!(
                close(&first["wire"]["x2"], ex) && close(&first["wire"]["y2"], ey),
                "{first}"
            );

            let (_d2, single_path) = fixture();
            for (reference, pin_number) in PINS {
                let mut one = args.clone();
                one["schematic"] = json!(single_path.display().to_string());
                one["reference"] = json!(reference);
                one["pin_number"] = json!(pin_number);
                let r = (single.handler)(&one, Arc::new(ctx())).await.unwrap();
                assert!(!r.is_error, "{r:?}");
            }

            let drawn = drawing(&batch_path);
            assert_eq!(drawn, drawing(&single_path), "{options}");
            // The fixture's own wire plus one stub per pin.
            assert_eq!(
                drawn.iter().filter(|l| l.starts_with("wire ")).count(),
                6,
                "{options}"
            );
            let dots: Vec<_> = drawn
                .iter()
                .filter(|l| l.starts_with("junction "))
                .collect();
            assert_eq!(dots.len(), junctions, "{options}: {dots:?}");
            if junctions == 1 {
                assert_eq!(dots[0], "junction 119.380 93.980");
            }
            let (_, tree) = read_schematic(&batch_path).unwrap();
            assert!(
                extract_labels(&tree).iter().all(|l| l.kind == kind),
                "{options}"
            );
        }
    }

    /// A stub's label marks the spot, so running the batch again adds
    /// neither a second wire nor a second label.
    #[tokio::test]
    async fn rerunning_a_stubbed_batch_adds_nothing() {
        let (_d, path) = fixture();
        let options = json!({ "net_name": "N", "stub_length": 2.54 });
        assert!(!batch(&path, options.clone()).await.is_error);
        let before = std::fs::read_to_string(&path).unwrap();
        let result = batch(&path, options).await;
        assert!(!result.is_error, "{result:?}");
        assert!(body(&result)["added"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["deduplicated"] == json!(true) && e.get("wire").is_none()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// Two stubs can end on one spot, and a label of the net can already sit
    /// at a stub's end. Each pin still gets its own wire; only the label is
    /// shared. The wires are the ones main's `connect_to_net` drew, one call
    /// per pin, and KiCad's netlist puts all three pins on the net.
    #[tokio::test]
    async fn stubs_ending_on_one_label_each_get_their_wire() {
        let (_d, path) = fixture_named("batch_connect_facing_kicad10.kicad_sch");
        let args = json!({
            "schematic": path.display().to_string(),
            "net": "N",
            "stub_length": 2.54,
            "pins": [
                { "reference": "R1", "pin_number": "2" },
                { "reference": "R2", "pin_number": "1" },
                { "reference": "R2", "pin_number": "2" }
            ]
        });
        let result = handle_batch_connect_to_net(&args, &ctx()).await.unwrap();
        assert!(!result.is_error, "{result:?}");
        for entry in body(&result)["added"].as_array().unwrap() {
            assert!(entry.get("wire").is_some(), "{entry}");
            assert!(entry.get("deduplicated").is_none(), "{entry}");
        }

        let drawn = drawing(&path);
        let wires: Vec<_> = drawn.iter().filter(|l| l.starts_with("wire ")).collect();
        assert_eq!(
            wires,
            [
                "wire 67.310 63.500 69.850 63.500",
                "wire 72.390 63.500 69.850 63.500",
                "wire 80.010 63.500 82.550 63.500",
            ]
        );
        // The fixture's own label at (82.55, 63.5), and one shared at the
        // meeting point.
        let labels: Vec<_> = drawn
            .iter()
            .filter(|l| l.starts_with("NetLabel "))
            .collect();
        assert_eq!(
            labels,
            [
                "NetLabel N 69.850 63.500 0.000",
                "NetLabel N 82.550 63.500 0.000"
            ]
        );
    }

    #[tokio::test]
    async fn net_is_an_alias_that_must_agree_with_net_name() {
        let (_d, path) = fixture();
        let original = std::fs::read_to_string(&path).unwrap();
        for (options, field) in [
            (json!({ "net_name": "A", "net": "B" }), "net"),
            (json!({}), "net_name"),
        ] {
            let result = batch(&path, options.clone()).await;
            assert!(result.is_error, "{options}");
            let body = body(&result);
            assert_eq!(body["error"]["kind"], "invalid_argument", "{body}");
            assert_eq!(body["error"]["field"], field, "{body}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                original,
                "{options}"
            );
        }
        for options in [
            json!({ "net": "A" }),
            json!({ "net_name": "A", "net": "A" }),
        ] {
            let (_d, path) = fixture();
            let result = batch(&path, options.clone()).await;
            assert!(!result.is_error, "{options}: {result:?}");
            assert_eq!(body(&result)["net"], "A");
        }
    }

    #[test]
    fn schema_refuses_a_negative_stub() {
        let def = tools()
            .into_iter()
            .find(|t| t.name == "batch_connect_to_net")
            .unwrap();
        let args = |stub: f64| json!({ "schematic": "s.kicad_sch", "net": "N", "pins": [], "stub_length": stub });
        assert!(def.input_validator.is_valid(&args(0.0)));
        assert!(def.input_validator.is_valid(&args(2.54)));
        assert!(!def.input_validator.is_valid(&args(-1.0)));
    }
}
