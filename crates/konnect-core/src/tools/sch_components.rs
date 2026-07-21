//! `sch_components` toolset — add, edit, move, rotate, delete schematic symbols.
//!
//! Simple CRUD operations use `konnect_schematic_editor` (cse) for structured
//! round-trip parsing.  Pin coordinate math still delegates to
//! `konnect_sexp::geometry::transform_pin`.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    find_symbol_instance_block, get_path, opt_f64, opt_str, project_name_for, require_f64,
    require_str, ToolContext, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::snap_point,
    schematic::{extract_lib_pins, extract_symbol_instances, pin_endpoint, read_schematic},
    writer::{apply_edits, new_uuid, write_atomic, SexpEdit},
};
use serde_json::json;

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "create_schematic",
            "Create a new blank .kicad_sch schematic file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full path for the new .kicad_sch file" }
                },
                "required": ["path"]
            }),
            |args, ctx| async move { handle_create_schematic(args, ctx).await }
        ),
        tool!(
            "add_schematic_component",
            "Add a symbol from a KiCAD library to the schematic. The symbol is snapped \
             to the 1.27mm schematic grid. Specify position in schematic mm coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "lib_id": { "type": "string", "description": "Library:Symbol (e.g. 'Device:R')" },
                    "x": { "type": "number", "description": "X position in mm" },
                    "y": { "type": "number", "description": "Y position in mm" },
                    "rotation": { "type": "number", "description": "Rotation in degrees (0/90/180/270)", "default": 0 },
                    "reference": { "type": "string", "description": "Optional override for reference designator" },
                    "value": { "type": "string", "description": "Optional override for value field" },
                    "unit": { "type": "integer", "description": "Unit number for multi-unit symbols (gate/part selection). Default 1.", "default": 1 }
                },
                "required": ["schematic", "lib_id", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_component(args, ctx).await }
        ),
        tool!(
            "delete_schematic_component",
            "Remove a symbol instance from the schematic by its reference designator.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator (e.g. 'R1')" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_delete_schematic_component(args, ctx).await }
        ),
        tool!(
            "edit_schematic_component",
            "Update fields (Reference, Value, Footprint, custom properties) of a symbol instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Current reference designator" },
                    "new_reference": { "type": "string", "description": "New reference designator (optional)" },
                    "value": { "type": "string", "description": "New value (optional)" },
                    "footprint": { "type": "string", "description": "New footprint (optional)" },
                    "datasheet": { "type": "string", "description": "New datasheet URL (optional)" },
                    "fields": {
                        "type": "object",
                        "description": "Additional property fields to set as key:value pairs"
                    }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_edit_schematic_component(args, ctx).await }
        ),
        tool!(
            "get_schematic_component",
            "Get all properties, position, and pin locations for a symbol instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_component(args, ctx).await }
        ),
        tool!(
            "list_schematic_components",
            "List all symbol instances in a schematic with their positions, values, \
             footprints, and pin locations.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_list_schematic_components(args, ctx).await }
        ),
        tool!(
            "move_schematic_component",
            "Move a symbol to a new position. Does NOT adjust connected wires.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "x": { "type": "number", "description": "New X position in mm" },
                    "y": { "type": "number", "description": "New Y position in mm" }
                },
                "required": ["schematic", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_schematic_component(args, ctx).await }
        ),
        tool!(
            "rotate_schematic_component",
            "Rotate a symbol by setting its absolute rotation angle (0/90/180/270).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "rotation": { "type": "number", "description": "Absolute rotation in degrees" }
                },
                "required": ["schematic", "reference", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_schematic_component(args, ctx).await }
        ),
        tool!(
            "move_connected",
            "Move a symbol and stretch/shrink connected wire stubs to preserve connections.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "x": { "type": "number" },
                    "y": { "type": "number" }
                },
                "required": ["schematic", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_connected(args, ctx).await }
        ),
        tool!(
            "move_region",
            "Move all symbols within a bounding box by a given offset.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x1": { "type": "number", "description": "Region bounding box min X" },
                    "y1": { "type": "number", "description": "Region bounding box min Y" },
                    "x2": { "type": "number", "description": "Region bounding box max X" },
                    "y2": { "type": "number", "description": "Region bounding box max Y" },
                    "dx": { "type": "number", "description": "X offset to move by" },
                    "dy": { "type": "number", "description": "Y offset to move by" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2", "dx", "dy"]
            }),
            |args, ctx| async move { handle_move_region(args, ctx).await }
        ),
        tool!(
            "annotate_schematic",
            "Run kicad-cli to auto-assign reference designators (R? → R1, U? → U1, etc.).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_annotate_schematic(args, ctx).await }
        ),
        tool!(
            "get_schematic_pin_locations",
            "Get the exact schematic-space (X,Y) coordinates of every pin on a symbol, \
             accounting for rotation and mirroring. Uses the canonical pin transform.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_pin_locations(args, ctx).await }
        ),
        tool!(
            "batch_get_schematic_pin_locations",
            "Get pin locations for multiple components in a single file read.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators"
                    }
                },
                "required": ["schematic", "references"]
            }),
            |args, ctx| async move { handle_batch_get_pin_locations(args, ctx).await }
        ),
        tool!(
            "add_component_annotation",
            "Add a custom property (annotation) to a symbol instance in the schematic.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'R1')" },
                    "key": { "type": "string", "description": "Property name" },
                    "value": { "type": "string", "description": "Property value" }
                },
                "required": ["schematic", "reference", "key", "value"]
            }),
            |args, ctx| async move { handle_add_component_annotation(args, ctx).await }
        ),
        tool!(
            "group_components",
            "Add a group property to multiple components in the schematic.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators to group"
                    },
                    "group_name": { "type": "string", "description": "Group name to assign" }
                },
                "required": ["schematic", "references", "group_name"]
            }),
            |args, ctx| async move { handle_group_components(args, ctx).await }
        ),
        tool!(
            "replace_component",
            "Replace a component's lib_id with a new library symbol (swap the component type).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'U1')" },
                    "new_lib_id": { "type": "string", "description": "New Library:Symbol identifier (e.g. 'Device:C')" }
                },
                "required": ["schematic", "reference", "new_lib_id"]
            }),
            |args, ctx| async move { handle_replace_component(args, ctx).await }
        ),
        tool!(
            "get_schematic_view",
            "Render the schematic to a PNG image (base64-encoded) via kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_schematic_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_create_schematic(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "path")?;
    // Build a minimal valid schematic and save via cse's atomic writer.
    let template = crate::tools::blank_schematic_template();
    // Write the template then immediately load/save through cse so the file
    // is normalised to cse's writer output format.
    write_atomic(&path, &template)?;
    let sch = cse::Schematic::load(&path)?;
    sch.overwrite()?;
    Ok(CallToolResult::json(
        &json!({ "created": path.display().to_string() }),
    ))
}

async fn handle_add_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let lib_id = match require_str(args, "lib_id") {
        Ok(s) => s.to_string(),
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
    let reference = opt_str(args, "reference");
    let value = opt_str(args, "value");
    let unit = opt_f64(args, "unit").unwrap_or(1.0) as u32;

    // Snap to 1.27mm grid
    let (x, y) = snap_point(x, y, 1.27);

    let ref_str = reference.unwrap_or("?");
    let val_str = value.unwrap_or(lib_id.split(':').next_back().unwrap_or("?"));

    // Load via konnect-schematic-editor
    let mut sch = cse::Schematic::load(&sch_path)?;

    // The instance path below must be "/<root-uuid>" — KiCAD's netlister
    // resolves instances against the root sheet UUID and silently forms no
    // wire-only nets for symbols whose path doesn't resolve.
    let root_uuid = crate::tools::ensure_root_uuid(&mut sch);
    let project_name = project_name_for(&sch_path);

    // Embed the library symbol definition
    if !cse::library::ensure_lib_symbol(&mut sch, &lib_id) {
        return Ok(crate::tools::lib_symbol_not_found_error(&lib_id));
    }

    // Build the Symbol struct
    let mut sym = cse::Symbol::new(&lib_id, x, y);
    sym.at.rotation = Some(rotation);
    sym.unit = unit;

    // Helper: build an effects sub-node  (font (size 1.27 1.27))  with optional (hide yes)
    let effects_node = |hide: bool| -> cse::sexp::SexpNode {
        let font = cse::sexp::SexpNode::List(vec![
            cse::sexp::atom("font"),
            cse::sexp::SexpNode::List(vec![
                cse::sexp::atom("size"),
                cse::sexp::atom("1.27"),
                cse::sexp::atom("1.27"),
            ]),
        ]);
        let mut children = vec![cse::sexp::atom("effects"), font];
        if hide {
            children.push(cse::sexp::SexpNode::List(vec![
                cse::sexp::atom("hide"),
                cse::sexp::atom("yes"),
            ]));
        }
        cse::sexp::SexpNode::List(children)
    };

    // Helper: build an (at X Y ROT) sub-node
    let at_node = |px: f64, py: f64, rot: f64| -> cse::sexp::SexpNode {
        cse::sexp::SexpNode::List(vec![
            cse::sexp::atom("at"),
            cse::sexp::atom(cse::types::fmt_f64(px)),
            cse::sexp::atom(cse::types::fmt_f64(py)),
            cse::sexp::atom(cse::types::fmt_f64(rot)),
        ])
    };

    // Offset Reference above component, Value below
    let ref_y = y - 3.81;
    let val_y = y + 3.81;

    // Reference property
    let mut ref_prop = cse::Property::new("Reference", ref_str);
    ref_prop.sub_nodes.push(at_node(x, ref_y, 0.0));
    ref_prop.sub_nodes.push(effects_node(false));
    sym.properties.push(ref_prop);

    // Value property
    let mut val_prop = cse::Property::new("Value", val_str);
    val_prop.sub_nodes.push(at_node(x, val_y, 0.0));
    val_prop.sub_nodes.push(effects_node(false));
    sym.properties.push(val_prop);

    // Footprint property (hidden)
    let mut fp_prop = cse::Property::new("Footprint", "");
    fp_prop.sub_nodes.push(at_node(x, y, 0.0));
    fp_prop.sub_nodes.push(effects_node(true));
    sym.properties.push(fp_prop);

    // Datasheet property (hidden)
    let mut ds_prop = cse::Property::new("Datasheet", "");
    ds_prop.sub_nodes.push(at_node(x, y, 0.0));
    ds_prop.sub_nodes.push(effects_node(true));
    sym.properties.push(ds_prop);

    // Instance entry, keyed to the root sheet UUID like eeschema writes it:
    // (instances (project "<name>" (path "/<root-uuid>" (reference ...) (unit 1))))
    sym.set_instance_path(&project_name, &format!("/{}", root_uuid), ref_str, unit);

    let uuid = sym.uuid.clone();
    sch.add_symbol(sym);
    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "added": lib_id,
        "reference": ref_str,
        "value": val_str,
        "x": x, "y": y,
        "unit": unit,
        "uuid": uuid
    })))
}

async fn handle_delete_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.remove_by_reference(&reference) {
        Some(_) => {
            sch.overwrite()?;
            Ok(CallToolResult::json(&json!({ "deleted": reference })))
        }
        None => Ok(CallToolResult::error(format!(
            "Component '{}' not found in schematic",
            reference
        ))),
    }
}

async fn handle_edit_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let mut content = std::fs::read_to_string(&sch_path)?;
    let mut changed = Vec::new();

    // Helper: update a property field value in-place within the symbol block
    // for `ref_`. Returns the reason on failure so the caller can report it
    // instead of silently claiming success.
    let update_field =
        |content: &str, ref_: &str, field: &str, new_val: &str| -> Result<String, String> {
            let (sym_start, sym_end) = find_symbol_instance_block(content, ref_)
                .ok_or_else(|| format!("symbol '{ref_}' not found in this schematic"))?;
            let sym_block = &content[sym_start..sym_end];
            let field_search = format!(r#"(property "{field}" ""#);
            let field_offset = sym_block
                .find(&field_search)
                .map(|o| sym_start + o + field_search.len())
                .ok_or_else(|| format!("'{ref_}' has no '{field}' property"))?;
            // Find the closing quote of the current value
            let val_end = content[field_offset..]
                .find('"')
                .map(|o| field_offset + o)
                .ok_or_else(|| format!("'{field}' property on '{ref_}' is malformed"))?;
            Ok(format!(
                "{}{}{}",
                &content[..field_offset],
                new_val,
                &content[val_end..]
            ))
        };

    let mut errors: Vec<String> = Vec::new();
    let mut apply = |content: &mut String, field: &str, new_val: &str| match update_field(
        content, &reference, field, new_val,
    ) {
        Ok(updated) => {
            *content = updated;
            changed.push(format!("{} → {}", field, new_val));
        }
        Err(why) => errors.push(format!("{field}: {why}")),
    };

    if let Some(new_ref) = opt_str(args, "new_reference") {
        apply(&mut content, "Reference", new_ref);
    }
    if let Some(val) = opt_str(args, "value") {
        apply(&mut content, "Value", val);
    }
    if let Some(fp) = opt_str(args, "footprint") {
        apply(&mut content, "Footprint", fp);
    }
    if let Some(ds) = opt_str(args, "datasheet") {
        apply(&mut content, "Datasheet", ds);
    }

    // A request that changed nothing is a failure, not a success — silently
    // reporting `"changes": []` is what let the tab-indentation bug hide.
    if changed.is_empty() && !errors.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No fields were updated on '{}': {}",
            reference,
            errors.join("; ")
        )));
    }

    if !changed.is_empty() {
        write_atomic(&sch_path, &content)?;
    }

    let mut result = json!({
        "reference": reference,
        "changes": changed
    });
    if !errors.is_empty() {
        result["errors"] = json!(errors);
    }
    Ok(CallToolResult::json(&result))
}

async fn handle_get_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference(&reference) {
        Some(sym) => {
            let (x, y) = sym.position();
            let rotation = sym.at.rotation.unwrap_or(0.0);
            let mirror = sym.mirror.as_deref().unwrap_or("");
            Ok(CallToolResult::json(&json!({
                "reference": sym.reference().unwrap_or("?"),
                "value": sym.value_str().unwrap_or(""),
                "footprint": sym.footprint().unwrap_or(""),
                "lib_id": sym.lib_id,
                "x": x,
                "y": y,
                "rotation": rotation,
                "mirror_x": mirror.contains('x'),
                "mirror_y": mirror.contains('y'),
                "uuid": sym.uuid
            })))
        }
        None => Ok(CallToolResult::error(format!(
            "Component '{}' not found",
            reference
        ))),
    }
}

async fn handle_list_schematic_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;

    let items: Vec<serde_json::Value> = sch
        .symbols
        .iter()
        .map(|sym| {
            let (x, y) = sym.position();
            let rotation = sym.at.rotation.unwrap_or(0.0);
            let mirror = sym.mirror.as_deref().unwrap_or("");
            json!({
                "reference": sym.reference().unwrap_or("?"),
                "value": sym.value_str().unwrap_or(""),
                "footprint": sym.footprint().unwrap_or(""),
                "lib_id": sym.lib_id,
                "x": x,
                "y": y,
                "rotation": rotation,
                "mirror_x": mirror.contains('x'),
                "mirror_y": mirror.contains('y')
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "components": items
    })))
}

async fn handle_move_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let new_y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let (new_x, new_y) = snap_point(new_x, new_y, 1.27);

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference_mut(&reference) {
        Some(sym) => {
            sym.move_to(new_x, new_y);
            sch.overwrite()?;
            Ok(CallToolResult::json(
                &json!({ "moved": reference, "x": new_x, "y": new_y }),
            ))
        }
        None => Err(anyhow::anyhow!("Component '{}' not found", reference)),
    }
}

async fn handle_rotate_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference_mut(&reference) {
        Some(sym) => {
            sym.set_rotation(rotation);
            sch.overwrite()?;
            Ok(CallToolResult::json(
                &json!({ "rotated": reference, "rotation": rotation }),
            ))
        }
        None => Err(anyhow::anyhow!("Component '{}' not found", reference)),
    }
}

async fn handle_move_connected(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    // For now: delegate to simple move. Wire adjustment is a Phase 2 enhancement.
    handle_move_schematic_component(args, ctx).await
}

async fn handle_move_region(
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
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    // Collect references of symbols within the bounding box
    let refs_to_move: Vec<String> = sch
        .symbols
        .within_rectangle(x1, y1, x2, y2)
        .iter()
        .filter_map(|s| s.reference().map(String::from))
        .collect();

    let mut moved = Vec::new();
    for reference in &refs_to_move {
        if let Some(sym) = sch.symbols.by_reference_mut(reference) {
            let (ox, oy) = sym.position();
            let (nx, ny) = snap_point(ox + dx, oy + dy, 1.27);
            sym.move_to(nx, ny);
            moved.push(reference.clone());
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved
    })))
}

async fn handle_annotate_schematic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    crate::tools::cli::annotate_schematic(&ctx.config.kicad_cli, &sch_path).await?;
    Ok(CallToolResult::text("Annotation complete."))
}

async fn handle_get_schematic_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let inst = match instances.iter().find(|i| i.reference == reference) {
        Some(i) => i,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Find the library symbol definition within the schematic's lib_symbols section
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let lib_sym = lib_syms
        .iter()
        .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));

    // A missing embedded definition is an error, not an empty pin list —
    // silently returning [] hid every bad-lib_id component until wiring or
    // netlisting failed much later (#34).
    let Some(sym) = lib_sym else {
        return Ok(CallToolResult::error(format!(
            "Component '{}' has no embedded definition for '{}' in this \
             schematic's lib_symbols — it was likely added with a lib_id that \
             doesn't exist in the installed libraries, so it is invisible to \
             KiCAD's netlister. Re-add it with a valid lib_id \
             (delete_schematic_component + add_schematic_component).",
            reference, inst.lib_id
        )));
    };
    let lib_pins = extract_lib_pins(sym);
    let t = inst.pin_transform();
    let pins: Vec<serde_json::Value> = lib_pins
        .iter()
        .map(|p| {
            let (sx, sy) = pin_endpoint(p, t);
            json!({
                "number": p.number,
                "name": p.name,
                "x": sx,
                "y": sy
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "component_x": inst.x,
        "component_y": inst.y,
        "rotation": inst.rotation,
        "pins": pins
    })))
}

async fn handle_batch_get_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let (_, tree) = read_schematic(&sch_path)?; // single read
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let results: Vec<serde_json::Value> = refs
        .iter()
        .map(|reference| {
            let inst = match instances.iter().find(|i| &i.reference == reference) {
                Some(i) => i,
                None => return json!({ "reference": reference, "error": "not found" }),
            };
            let lib_sym = lib_syms
                .iter()
                .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
            // Per-entry error rather than a silent empty pin list (#34).
            let Some(sym) = lib_sym else {
                return json!({
                    "reference": reference,
                    "error": format!(
                        "no embedded definition for '{}' in lib_symbols — \
                         likely added with a nonexistent lib_id",
                        inst.lib_id
                    )
                });
            };
            let t = inst.pin_transform();
            let pins: Vec<serde_json::Value> = extract_lib_pins(sym)
                .iter()
                .map(|p| {
                    let (sx, sy) = pin_endpoint(p, t);
                    json!({ "number": p.number, "name": p.name, "x": sx, "y": sy })
                })
                .collect();
            json!({ "reference": reference, "x": inst.x, "y": inst.y, "pins": pins })
        })
        .collect();

    Ok(CallToolResult::json(&json!({ "components": results })))
}

async fn handle_get_schematic_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tmp_dir = std::env::temp_dir().join(format!("konnect_{}", new_uuid()));
    tokio::fs::create_dir_all(&tmp_dir).await?;

    // KiCAD 10 CLI only supports SVG export for schematics (no bitmap)
    let svg_path =
        crate::tools::cli::render_schematic_svg(&ctx.config.kicad_cli, &sch_path, &tmp_dir).await?;

    let svg_content = tokio::fs::read_to_string(&svg_path).await?;
    tokio::fs::remove_dir_all(&tmp_dir).await.ok();

    // Return as text content (SVG is XML text, not a raster image)
    Ok(crate::mcp::protocol::CallToolResult {
        content: vec![crate::mcp::protocol::ToolContent::Text {
            text: format!("SVG schematic rendered. {} bytes.\n\nNote: KiCAD 10 CLI exports schematics as SVG only (no bitmap). \
                          The SVG file has been generated. Use export_schematic_pdf for a PDF version.", svg_content.len()),
        }],
        is_error: false,
    })
}

async fn handle_add_component_annotation(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let key = match require_str(args, "key") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let value = match require_str(args, "value") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&sch_path)?;

    // Find the symbol block for this reference
    let (sym_start, sym_end) = match find_symbol_instance_block(&content, &reference) {
        Some(r) => r,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Find the position just before (instances in the symbol block, or before closing paren
    let sym_block = &content[sym_start..sym_end];
    let insert_rel = sym_block
        .find("(instances")
        .unwrap_or(sym_block.rfind(')').unwrap_or(sym_block.len() - 1));
    let insert_abs = sym_start + insert_rel;

    // Build the property S-expression
    let prop_sexp = format!(
        "    (property \"{key}\" \"{value}\"\n      (at 0 0 0)\n      (effects (font (size 1.27 1.27)) (hide yes))\n    )\n    "
    );

    let new_content = apply_edits(content, vec![SexpEdit::insert(insert_abs, prop_sexp)]);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "added_property": key,
        "value": value
    })))
}

async fn handle_group_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let group_name = match require_str(args, "group_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let refs = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if refs.is_empty() {
        return Ok(CallToolResult::error("No references provided"));
    }

    let mut content = std::fs::read_to_string(&sch_path)?;
    let mut grouped = Vec::new();

    for reference in &refs {
        let (sym_start, sym_end) = match find_symbol_instance_block(&content, reference) {
            Some(r) => r,
            None => continue,
        };

        let sym_block = &content[sym_start..sym_end];
        let insert_rel = sym_block
            .find("(instances")
            .unwrap_or(sym_block.rfind(')').unwrap_or(sym_block.len() - 1));
        let insert_abs = sym_start + insert_rel;

        let prop_sexp = format!(
            "    (property \"Group\" \"{group_name}\"\n      (at 0 0 0)\n      (effects (font (size 1.27 1.27)) (hide yes))\n    )\n    "
        );

        content = apply_edits(content, vec![SexpEdit::insert(insert_abs, prop_sexp)]);
        grouped.push(reference.clone());
    }

    write_atomic(&sch_path, &content)?;

    Ok(CallToolResult::json(&json!({
        "group_name": group_name,
        "grouped_count": grouped.len(),
        "grouped": grouped
    })))
}

async fn handle_replace_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_lib_id = match require_str(args, "new_lib_id") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let mut content = std::fs::read_to_string(&sch_path)?;

    // Find the symbol block for this reference
    let (sym_start, sym_end) = match find_symbol_instance_block(&content, &reference) {
        Some(r) => r,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Find the (lib_id "OLD") and replace it — searching only within this
    // symbol's block, so a malformed instance can't reach into the next one.
    let sym_block = &content[sym_start..sym_end];
    let lib_id_pat = "(lib_id \"";
    let lib_id_rel = match sym_block.find(lib_id_pat) {
        Some(o) => o,
        None => {
            return Ok(CallToolResult::error(
                "Could not find lib_id in symbol block",
            ))
        }
    };
    let lib_id_abs = sym_start + lib_id_rel + lib_id_pat.len();
    let lib_id_end = match content[lib_id_abs..].find('"') {
        Some(o) => lib_id_abs + o,
        None => return Ok(CallToolResult::error("Malformed lib_id")),
    };

    let old_lib_id = content[lib_id_abs..lib_id_end].to_string();

    let new_content = apply_edits(
        content,
        vec![SexpEdit::replace(
            lib_id_abs,
            lib_id_end,
            new_lib_id.clone(),
        )],
    );
    content = new_content;

    // Ensure the new library symbol definition is present. Bail BEFORE writing:
    // a replace that can't embed its definition would leave the component
    // netlist-invisible (#34).
    if !super::ensure_lib_symbol_in_schematic(&mut content, &new_lib_id) {
        return Ok(crate::tools::lib_symbol_not_found_error(&new_lib_id));
    }
    write_atomic(&sch_path, &content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "old_lib_id": old_lib_id,
        "new_lib_id": new_lib_id
    })))
}

// Library symbol resolution moved to tools/mod.rs (shared with sch_wiring.rs)

#[cfg(test)]
mod tests {
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
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// Serializes tests that set KICAD10_SYMBOL_DIR (process-wide env).
    static SYMBOL_DIR_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A stub symbol library so component adds resolve without an installed
    /// KiCAD (CI has none): Device:R and Device:C_Polarized in the KiCAD 10
    /// symdir layout. Returns (tempdir guard, env lock).
    fn stub_symbol_dir() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = SYMBOL_DIR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let symdir = dir.path().join("Device.kicad_symdir");
        std::fs::create_dir_all(&symdir).unwrap();
        let symbol = |name: &str| {
            format!(
                "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"{name}\"\n\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t(property \"Value\" \"{name}\" (at 0 0 0))\n\t\t(symbol \"{name}_0_1\"\n\t\t\t(pin passive line (at 0 3.81 270) (length 1.27)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t\t(pin passive line (at 0 -3.81 90) (length 1.27)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"2\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t)\n\t)\n)\n"
            )
        };
        std::fs::write(symdir.join("R.kicad_sym"), symbol("R")).unwrap();
        std::fs::write(symdir.join("C_Polarized.kicad_sym"), symbol("C_Polarized")).unwrap();
        std::env::set_var("KICAD10_SYMBOL_DIR", dir.path());
        (dir, guard)
    }

    #[tokio::test]
    async fn create_schematic_writes_root_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.kicad_sch");
        let ctx = test_ctx();

        let result = handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(
            sch.uuid.is_some(),
            "root (uuid ...) is required for KiCAD's netlister to resolve instance paths"
        );
    }

    #[tokio::test]
    async fn add_component_writes_eeschema_style_instance_path() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("amp.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 100.0, "y": 80.0,
                "reference": "R1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        let root_uuid = sch.uuid.clone().expect("root uuid present");
        let sym = sch.symbols.by_reference("R1").unwrap();
        // KiCAD only forms wire-only nets when the instance path is exactly
        // "/<root-uuid>"; the project key mirrors eeschema (file stem).
        assert!(
            sym.has_instance_path("amp", &format!("/{}", root_uuid)),
            "instance path must be /<root-uuid> under the file-stem project name"
        );
    }

    #[tokio::test]
    async fn add_component_writes_requested_unit() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 100.0, "y": 80.0,
                "reference": "U1",
                "unit": 3
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch.symbols.by_reference("U1").unwrap();
        assert_eq!(sym.unit, 3, "symbol (unit N) must match the requested unit");
        let root_uuid = sch.uuid.clone().unwrap();
        // Instance entry must carry the same unit, not a hardcoded 1.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(&format!("/{}", root_uuid)));
        assert!(raw.contains("(unit 3)"), "instance unit must be 3");
    }

    #[tokio::test]
    async fn add_component_repairs_legacy_file_without_root_uuid() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.kicad_sch");
        // File shape produced by Konnect before root UUIDs were written.
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 50.0, "y": 50.0,
                "reference": "R1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        let root_uuid = sch.uuid.clone().expect("legacy file gains a root uuid");
        let sym = sch.symbols.by_reference("R1").unwrap();
        assert!(sym.has_instance_path("legacy", &format!("/{}", root_uuid)));
    }

    #[tokio::test]
    async fn add_component_with_nonexistent_lib_id_errors_with_suggestion() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Device:CP is the KiCAD ≤9 name; 10 renamed it to C_Polarized (#34).
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:CP",
                "x": 100.0, "y": 80.0,
                "reference": "C1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error, "nonexistent lib_id must be an error");
        let msg = format!("{:?}", result.content);
        assert!(msg.contains("Device:CP"), "names the bad lib_id: {msg}");
        assert!(
            msg.contains("C_Polarized"),
            "did-you-mean should surface the rename: {msg}"
        );

        // And nothing was written: no ghost instance in the file.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn add_component_with_unknown_library_says_so() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nolib.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Transistor_FET_xyzzy:IRF830",
                "x": 100.0, "y": 80.0
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let msg = format!("{:?}", result.content);
        assert!(
            msg.contains("Library 'Transistor_FET_xyzzy' not found"),
            "distinguishes missing library from missing symbol: {msg}"
        );
    }

    #[tokio::test]
    async fn pin_locations_error_when_definition_not_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noembed.kicad_sch");
        // A symbol instance whose lib_id has NO lib_symbols entry — the file
        // shape a ghost lib_id used to leave behind (#34).
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t)\n\t(symbol\n\t\t(lib_id \"Device:CP\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n\t\t(property \"Reference\" \"C1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let result = handle_get_schematic_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "C1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            result.is_error,
            "missing embedded definition must be an error, not pins: []"
        );
        let msg = format!("{:?}", result.content);
        assert!(msg.contains("Device:CP"));
        assert!(msg.contains("no embedded definition"));
    }
}
