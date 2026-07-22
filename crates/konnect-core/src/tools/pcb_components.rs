//! `pcb_components` toolset — place, move, rotate, query, and array footprints on the PCB.
//!
//! Most operations use the KiCAD IPC API so they integrate with KiCAD's undo/redo
//! system and don't require a separate file-sync step. `get_board_2d_view` uses
//! kicad-cli to render a PNG.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use konnect_ipc::client::KiCadIpcClient;
use serde_json::json;

// ─── IPC helper ───────────────────────────────────────────────────────────────

async fn with_ipc<T, F>(addr: String, f: F) -> anyhow::Result<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(&KiCadIpcClient::new(&addr))).await {
        Ok(Ok(r)) => Ok(Ok(r)),
        Ok(Err(e)) => Ok(Err(e.to_string())),
        Err(e) => Err(anyhow::anyhow!("Thread error: {}", e)),
    }
}

macro_rules! ipc {
    ($ctx:expr, |$c:ident| $body:expr) => {{
        let addr = $ctx.config.ipc_address.clone();
        match with_ipc(addr, move |$c| $body).await? {
            Ok(v) => v,
            Err(msg) => {
                return Ok(CallToolResult::error(format!(
                    "KiCAD must be running with the board loaded (IPC error: {})",
                    msg
                )))
            }
        }
    }};
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "place_component",
            "Place a footprint on the PCB at the given position and layer via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":      { "type": "string" },
                    "footprint":  { "type": "string", "description": "Library:Footprint (e.g. 'Resistor_SMD:R_0402')" },
                    "reference":  { "type": "string", "description": "Reference designator" },
                    "x":          { "type": "number" },
                    "y":          { "type": "number" },
                    "rotation":   { "type": "number", "default": 0 },
                    "layer":      { "type": "string", "default": "F.Cu" }
                },
                "required": ["board", "footprint", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_place_component(args, ctx).await }
        ),
        tool!(
            "move_component",
            "Move a placed footprint to a new X/Y position via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" }
                },
                "required": ["board", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_component(args, ctx).await }
        ),
        tool!(
            "rotate_component",
            "Set the rotation angle of a placed footprint via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "rotation":  { "type": "number", "description": "Rotation angle in degrees" }
                },
                "required": ["board", "reference", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_component(args, ctx).await }
        ),
        tool!(
            "delete_component",
            "Remove a footprint from the board via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_delete_component(args, ctx).await }
        ),
        tool!(
            "edit_component",
            "Update the value or other properties of a placed footprint via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" },
                    "value":     { "type": "string", "description": "New value string (optional)" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_edit_component(args, ctx).await }
        ),
        tool!(
            "find_component",
            "Find a footprint on the board by reference designator and return its position.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_find_component(args, ctx).await }
        ),
        tool!(
            "get_component_pads",
            "Return the pad positions and net assignments for a footprint.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_get_component_pads(args, ctx).await }
        ),
        tool!(
            "get_pad_position",
            "Return the schematic-space position of a specific pad number on a footprint.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "reference":   { "type": "string" },
                    "pad_number":  { "type": "string" }
                },
                "required": ["board", "reference", "pad_number"]
            }),
            |args, ctx| async move { handle_get_pad_position(args, ctx).await }
        ),
        tool!(
            "get_component_list",
            "List all footprints on the board with their positions, layers, and values.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_component_list(args, ctx).await }
        ),
        tool!(
            "place_component_array",
            "Place multiple copies of a footprint in a grid or line array via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string" },
                    "footprint":    { "type": "string" },
                    "start_x":      { "type": "number" },
                    "start_y":      { "type": "number" },
                    "count_x":      { "type": "integer", "description": "Number of columns" },
                    "count_y":      { "type": "integer", "description": "Number of rows", "default": 1 },
                    "spacing_x":    { "type": "number", "description": "Column spacing in mm" },
                    "spacing_y":    { "type": "number", "description": "Row spacing in mm", "default": 0 },
                    "ref_prefix":   { "type": "string", "description": "Reference prefix (e.g. 'R')", "default": "U" },
                    "ref_start":    { "type": "integer", "description": "Starting reference number", "default": 1 }
                },
                "required": ["board", "footprint", "start_x", "start_y", "count_x", "spacing_x"]
            }),
            |args, ctx| async move { handle_place_array(args, ctx).await }
        ),
        tool!(
            "align_components",
            "Align multiple footprints along a common X or Y axis via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "references":  { "type": "array", "items": { "type": "string" } },
                    "axis":        { "type": "string", "description": "'x' or 'y'", "default": "x" },
                    "value":       { "type": "number", "description": "Target coordinate to align to" }
                },
                "required": ["board", "references", "value"]
            }),
            |args, ctx| async move { handle_align_components(args, ctx).await }
        ),
        tool!(
            "duplicate_component",
            "Duplicate an existing footprint at a new position via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":         { "type": "string" },
                    "reference":     { "type": "string", "description": "Reference to duplicate" },
                    "new_reference": { "type": "string", "description": "New reference designator" },
                    "x":             { "type": "number" },
                    "y":             { "type": "number" }
                },
                "required": ["board", "reference", "new_reference", "x", "y"]
            }),
            |args, ctx| async move { handle_duplicate_component(args, ctx).await }
        ),
        tool!(
            "get_board_2d_view",
            "Render the PCB as a 2-D image using kicad-cli and return it as a base64 PNG.",
            json!({
                "type": "object",
                "properties": {
                    "board":  { "type": "string" },
                    "layers": {
                        "type": "array",
                        "description": "Layers to include (empty = default copper + silkscreen)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_board_2d_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_place_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let value = args["value"].as_str().unwrap_or("").to_string();
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();

    let reference_for_ipc = reference.clone();
    let value_for_ipc = value.clone();
    let fp = ipc!(ctx, |c| c.place_footprint(
        &footprint,
        &reference_for_ipc,
        &value_for_ipc,
        x,
        y,
        rotation,
        &layer
    ));
    Ok(CallToolResult::json(&json!({
        "placed": fp.reference,
        "footprint": fp.footprint,
        "x": fp.position.x, "y": fp.position.y,
        "rotation": fp.rotation, "layer": fp.layer
    })))
}

async fn handle_move_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
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

    let ref_ipc = reference.clone();
    ipc!(ctx, |c| c.move_footprint(&ref_ipc, x, y));
    Ok(CallToolResult::json(
        &json!({ "moved": reference, "x": x, "y": y }),
    ))
}

async fn handle_rotate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    ipc!(ctx, |c| c.rotate_footprint(&ref_ipc, rotation));
    Ok(CallToolResult::json(
        &json!({ "rotated": reference, "rotation": rotation }),
    ))
}

async fn handle_delete_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let ref_ipc = reference.clone();
    ipc!(ctx, |c| c.delete_footprint(&ref_ipc));
    Ok(CallToolResult::json(&json!({ "deleted": reference })))
}

async fn handle_edit_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    // IPC doesn't have a direct "set value" command; re-get the footprint and report
    // For now this is a query + informational response. Full field edits require S-expr.
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let fp = ipc!(ctx, |c| {
        c.get_footprint(&reference)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", reference))
    });
    Ok(CallToolResult::json(&json!({
        "reference": fp.reference,
        "value": fp.value,
        "footprint": fp.footprint,
        "note": "Field edits via IPC are not yet supported. Edit in the schematic (edit_schematic_component), then open the PCB in KiCAD and run Tools > Update PCB from Schematic."
    })))
}

async fn handle_find_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let fp = ipc!(ctx, |c| {
        c.get_footprint(&reference)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", reference))
    });
    Ok(CallToolResult::json(&json!({
        "reference": fp.reference,
        "value": fp.value,
        "footprint": fp.footprint,
        "x": fp.position.x, "y": fp.position.y,
        "rotation": fp.rotation, "layer": fp.layer
    })))
}

async fn handle_get_component_pads(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    // Find the footprint with matching reference
    let fp_node = tree.find_all("footprint").into_iter().find(|fp| {
        fp.find_all("property").iter().any(|p| {
            p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                && p.get(2).and_then(|n| n.as_str()) == Some(reference.as_str())
        })
    });

    let fp_node = match fp_node {
        Some(n) => n,
        None => {
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' not found",
                reference
            )))
        }
    };

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pads: Vec<serde_json::Value> = fp_node
        .find_all("pad")
        .iter()
        .filter_map(|pad| {
            let number = pad.get(1)?.as_str()?.to_string();
            let pad_at = pad.find("at")?;
            let local_x = pad_at.get_f64(1)?;
            let local_y = pad_at.get_f64(2)?;
            // Transform local pad coords to board space (simplified: only rotation)
            let rad = fp_rot.to_radians();
            let board_x = fp_x + local_x * rad.cos() - local_y * rad.sin();
            let board_y = fp_y + local_x * rad.sin() + local_y * rad.cos();
            let net = pad
                .find("net")
                .and_then(|n| n.get(2))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            Some(json!({ "number": number, "x": board_x, "y": board_y, "net": net }))
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "reference": reference, "pad_count": pads.len(), "pads": pads }),
    ))
}

async fn handle_get_pad_position(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let pad_number = match require_str(args, "pad_number") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pads_result = handle_get_component_pads(args, ctx).await?;
    // Parse the result and filter for the specific pad number
    if let Some(crate::mcp::protocol::ToolContent::Text { text }) = pads_result.content.first() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(pads) = parsed["pads"].as_array() {
                if let Some(pad) = pads
                    .iter()
                    .find(|p| p["number"].as_str() == Some(&pad_number))
                {
                    return Ok(CallToolResult::json(pad));
                }
            }
        }
    }
    Ok(CallToolResult::error(format!(
        "Pad '{}' not found",
        pad_number
    )))
}

async fn handle_get_component_list(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let fps = ipc!(ctx, |c| c.list_footprints());
    let items: Vec<serde_json::Value> = fps
        .iter()
        .map(|fp| {
            json!({
                "reference": fp.reference,
                "value": fp.value,
                "footprint": fp.footprint,
                "x": fp.position.x, "y": fp.position.y,
                "rotation": fp.rotation, "layer": fp.layer
            })
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "components": items }),
    ))
}

async fn handle_place_array(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let start_x = match require_f64(args, "start_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let start_y = match require_f64(args, "start_y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let count_x = args["count_x"].as_u64().unwrap_or(1) as usize;
    let count_y = args["count_y"].as_u64().unwrap_or(1) as usize;
    let spacing_x = match require_f64(args, "spacing_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let spacing_y = args["spacing_y"].as_f64().unwrap_or(spacing_x);
    let prefix = args["ref_prefix"].as_str().unwrap_or("U").to_string();
    let ref_start = args["ref_start"].as_u64().unwrap_or(1) as usize;

    let mut placed = Vec::new();
    let mut n = ref_start;
    for row in 0..count_y {
        for col in 0..count_x {
            let x = start_x + col as f64 * spacing_x;
            let y = start_y + row as f64 * spacing_y;
            let reference = format!("{prefix}{n}");
            let fp_id = footprint.clone();
            let ref_for_ipc = reference.clone();
            let ref_for_result = reference.clone();
            match with_ipc(ctx.config.ipc_address.clone(), move |c| {
                c.place_footprint(&fp_id, &ref_for_ipc, "", x, y, 0.0, "F.Cu")
            })
            .await?
            {
                Ok(fp) => placed.push(
                    json!({ "reference": ref_for_result, "x": fp.position.x, "y": fp.position.y }),
                ),
                Err(e) => {
                    return Ok(CallToolResult::error(format!(
                        "IPC error placing {}: {}",
                        reference, e
                    )))
                }
            }
            n += 1;
        }
    }
    Ok(CallToolResult::json(
        &json!({ "placed_count": placed.len(), "components": placed }),
    ))
}

async fn handle_align_components(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let refs = args["references"].as_array().cloned().unwrap_or_default();
    let axis = args["axis"].as_str().unwrap_or("x").to_string();
    let value = match require_f64(args, "value") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut aligned = Vec::new();
    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r.to_string(),
            None => continue,
        };
        let ref2 = reference.clone();
        let axis_clone = axis.clone();
        let res = with_ipc(ctx.config.ipc_address.clone(), move |c| {
            let fp = c
                .get_footprint(&ref2)?
                .ok_or_else(|| anyhow::anyhow!("not found"))?;
            let (nx, ny) = if axis_clone == "y" {
                (fp.position.x, value)
            } else {
                (value, fp.position.y)
            };
            c.move_footprint(&ref2, nx, ny)?;
            Ok((nx, ny))
        })
        .await?;
        match res {
            Ok((nx, ny)) => aligned.push(json!({ "reference": reference, "x": nx, "y": ny })),
            Err(e) => return Ok(CallToolResult::error(format!("IPC error: {}", e))),
        }
    }
    Ok(CallToolResult::json(
        &json!({ "aligned_count": aligned.len(), "components": aligned }),
    ))
}

async fn handle_duplicate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_reference = match require_str(args, "new_reference") {
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

    // Get the source footprint's footprint ID and rotation
    let ref_ipc = reference.clone();
    let src = ipc!(ctx, |c| {
        c.get_footprint(&ref_ipc)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", ref_ipc))
    });

    let new_ref_ipc = new_reference.clone();
    let new_value_ipc = src.value.clone();
    let fp = ipc!(ctx, |c| c.place_footprint(
        &src.footprint,
        &new_ref_ipc,
        &new_value_ipc,
        x,
        y,
        src.rotation,
        &src.layer
    ));
    Ok(CallToolResult::json(&json!({
        "duplicated_from": reference,
        "new_reference": fp.reference,
        "x": fp.position.x, "y": fp.position.y
    })))
}

async fn handle_get_board_2d_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    use base64::Engine;
    let board_path = get_path(args, "board")?;
    let layers: Vec<String> = args["layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "F.Cu".into(),
                "B.Cu".into(),
                "F.SilkS".into(),
                "B.SilkS".into(),
                "Edge.Cuts".into(),
            ]
        });

    let tmp = board_path.with_extension("render.png");
    let layer_refs: Vec<&str> = layers.iter().map(String::as_str).collect();
    super::cli::render_pcb_png(&ctx.config.kicad_cli, &board_path, &tmp, &layer_refs).await?;
    let bytes = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;

    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(CallToolResult::image(b64, "image/png"))
}
