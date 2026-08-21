//! `pcb_routing` toolset — traces, vias, copper pours, nets, netclasses, and diff pairs.
//!
//! Routing operations use the KiCAD IPC API; `add_net`, `create_netclass`, and
//! `add_copper_pour` use S-expression file manipulation.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, opt_f64, require_f64, require_str, ToolContext, ToolDef};
use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::writer::{apply_edits, new_uuid, write_atomic, SexpEdit};
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
    ($ctx:expr, $args:expr, |$c:ident| $body:expr) => {{
        let addr = $ctx.config.ipc_address.clone();
        let requested_board = get_path($args, "board")?;
        match with_ipc(addr, move |$c| {
            $c.ensure_board_is_active(&requested_board)?;
            $body
        })
        .await?
        {
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

// ─── S-expression helpers ─────────────────────────────────────────────────────

/// A zone S-expression in the same format the rest of the board uses: KiCad 10
/// gets `(net "GND")` and `(layers …)`, legacy boards keep the id +
/// `(net_name …)` pair and singular `(layer …)`. The net reference comes from
/// [`konnect_sexp::net::net_ref_for_write`] — resolved structurally, never by
/// string offset, which is how zones used to land on net 0 (#192).
fn format_zone(
    net: &konnect_sexp::net::NetRef,
    layer: &str,
    clearance: f64,
    min_w: f64,
    pts: &[(f64, f64)],
) -> String {
    let uuid = new_uuid();
    let pt_str: String = pts
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    format!(
        "\n  (zone {net_nodes} {layer_node} (uuid \"{uuid}\")\n    \
         (hatch edge 0.508)\n    (connect_pads (clearance {clearance}))\n    \
         (min_thickness {min_w})\n    (fill yes)\n    \
         (polygon (pts{pt_str}\n    ))\n  )",
        net_nodes = net.zone_net_nodes(),
        layer_node = net.zone_layer_node(layer),
    )
}

/// The refusal for a net name a legacy board's table does not declare.
fn unknown_net_error(net_name: &str, board: &std::path::Path) -> CallToolResult {
    CallToolResult::error(format!(
        "Net '{net_name}' is not declared in {}'s net table. On this legacy-format board a \
         zone must reference a declared net id — writing it anyway would attach the copper \
         to net 0, the unconnected pseudo-net (#192). Declare it first with add_net, or \
         check the name with get_nets_list.",
        board.display()
    ))
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_net",
            "Add a new net entry to the top-level net table of a pre-KiCad-10 board \
             (S-expression insert, no KiCAD IPC required). Fails on a KiCad 10 board, which \
             has no net table — there, name the net on copper (route_trace, add_via, \
             add_copper_pour) instead.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" }
                },
                "required": ["board", "net_name"]
            }),
            |args, ctx| async move { handle_add_net(args, ctx).await }
        ),
        tool!(
            "route_trace",
            "Route a trace segment between two points on a copper layer via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" },
                    "layer":    { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_trace(args, ctx).await }
        ),
        tool!(
            "route_pad_to_pad",
            "Route a direct trace between two pads of named components (L-bend routing) via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "net_name":    { "type": "string" },
                    "ref1":        { "type": "string", "description": "First component reference" },
                    "pad1":        { "type": "string", "description": "First pad number" },
                    "ref2":        { "type": "string", "description": "Second component reference" },
                    "pad2":        { "type": "string", "description": "Second pad number" },
                    "layer":       { "type": "string", "default": "F.Cu" },
                    "width":       { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "ref1", "pad1", "ref2", "pad2"]
            }),
            |args, ctx| async move { handle_route_pad_to_pad(args, ctx).await }
        ),
        tool!(
            "add_via",
            "Add a through-hole via at a given position and assign it to a net via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "net_name":  { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" },
                    "drill":     { "type": "number", "description": "Drill diameter in mm", "default": 0.4 },
                    "pad_size":  { "type": "number", "description": "Via pad diameter in mm", "default": 0.8 }
                },
                "required": ["board", "net_name", "x", "y"]
            }),
            |args, ctx| async move { handle_add_via(args, ctx).await }
        ),
        tool!(
            "add_copper_pour",
            "Add a copper fill zone polygon on a layer/net via S-expression file insert.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "net_name":  { "type": "string" },
                    "layer":     { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "points": {
                        "type": "array",
                        "items": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } } }
                    },
                    "clearance": { "type": "number", "default": 0.2 },
                    "min_width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "points"]
            }),
            |args, ctx| async move { handle_add_copper_pour(args, ctx).await }
        ),
        tool!(
            "delete_trace",
            "Delete a trace segment identified by its UUID via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "uuid":  { "type": "string", "description": "UUID of the track segment to delete" }
                },
                "required": ["board", "uuid"]
            }),
            |args, ctx| async move { handle_delete_trace(args, ctx).await }
        ),
        tool!(
            "query_traces",
            "List trace segments on the board, optionally filtered by net and/or layer. \
             Each result includes the track's UUID, which delete_trace takes.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string", "description": "Filter by net (optional)" },
                    "layer":    { "type": "string", "description": "Filter by layer (optional)" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_query_traces(args, ctx).await }
        ),
        tool!(
            "get_nets_list",
            "Return all nets defined on the PCB via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_nets_list(args, ctx).await }
        ),
        tool!(
            "modify_trace",
            "Modify a trace segment by deleting and re-adding it with new parameters.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "uuid":      { "type": "string" },
                    "net_name":  { "type": "string" },
                    "layer":     { "type": "string" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width":     { "type": "number", "default": 0.25 }
                },
                "required": ["board", "uuid", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_modify_trace(args, ctx).await }
        ),
        tool!(
            "create_netclass",
            "Create or update a netclass in the project's design rules. Writes \
             net_settings in the sibling .kicad_pro (where KiCad keeps netclasses \
             since v7); the board file is never touched. Requires the project file \
             to exist. An update changes only the settings you name. To see what a \
             class holds, call get_netclasses rather than this tool: naming a class \
             that does not exist here creates it with the defaults, so a call meant \
             as a look writes one instead — and its result is nearly \
             indistinguishable from a read of an existing class.",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_pro is edited" },
                    "name":         { "type": "string", "description": "Netclass name (e.g. 'Power')" },
                    "clearance":    { "type": "number", "description": "Clearance in mm", "default": 0.2 },
                    "trace_width":  { "type": "number", "description": "Default trace width in mm", "default": 0.25 },
                    "via_drill":    { "type": "number", "description": "Via drill diameter in mm", "default": 0.4 },
                    "via_diameter": { "type": "number", "description": "Via pad diameter in mm", "default": 0.8 }
                },
                "required": ["board", "name"]
            }),
            |args, ctx| async move { handle_create_netclass(args, ctx).await }
        ),
        tool!(
            "get_netclasses",
            "Read every netclass in the project's design rules, with its settings, \
             its netclass_patterns and the board nets those patterns match. Reads \
             the sibling .kicad_pro and the board file; KiCad need not be running. \
             Call before create_netclass to see what a class holds — an update \
             changes only the values you name, and this is the only way to see the \
             rest. A net can match several classes: KiCad takes each property from \
             the highest-priority class that sets it (lower number = higher \
             priority) and falls back to Default.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_pro is read" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_netclasses(args, ctx).await }
        ),
        tool!(
            "assign_net_to_class",
            "Assign a net to an existing netclass, as a netclass_patterns entry in \
             the sibling .kicad_pro. The class must already exist (create_netclass). \
             Reassigning moves the net's entry to the new class.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_pro is edited" },
                    "net_name":  { "type": "string", "description": "Net name to assign" },
                    "netclass":  { "type": "string", "description": "Netclass name to assign the net to" }
                },
                "required": ["board", "net_name", "netclass"]
            }),
            |args, ctx| async move { handle_assign_net_to_class(args, ctx).await }
        ),
        tool!(
            "route_differential_pair",
            "Route a differential pair (two parallel traces with a specified gap).",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_pos":  { "type": "string", "description": "Positive net name" },
                    "net_neg":  { "type": "string", "description": "Negative net name" },
                    "layer":    { "type": "string", "default": "F.Cu" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.1 },
                    "gap":   { "type": "number", "description": "Gap between pair traces in mm", "default": 0.1 }
                },
                "required": ["board", "net_pos", "net_neg", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_diff_pair(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_net(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parse_sexp(&content)?;

    if board_is_kicad_10(&tree) {
        return Ok(CallToolResult::error(format!(
            "Cannot add net '{net_name}' to this board: it is in the KiCad 10 format, which has \
             no top-level net table. A net exists only by being named on an item — \
             (net \"{net_name}\") on a pad, segment, via or zone — so there is nothing for a \
             file-level insert to add, and appending a net node would report success while \
             KiCad discarded it on load. Create the net by naming it on copper instead: \
             route_trace / add_via / add_copper_pour take a net_name, as does assigning a pad \
             in KiCad. get_nets_list reads the live net list over IPC."
        )));
    }

    // Pre-KiCad-10: the top-level table is real, so an insert is meaningful.
    // The next id is one past the highest in use — not the number of "(net "
    // occurrences in the file, which counted every reference on every pad,
    // segment and zone and so collided with existing ids almost immediately.
    let net_id = tree
        .find_all("net")
        .iter()
        .filter_map(|n| konnect_sexp::net::net_id(n))
        .filter_map(|id| id.parse::<i32>().ok())
        .max()
        .map(|max| max + 1)
        .unwrap_or(1);
    let net_sexp = format!("\n  (net {net_id} \"{net_name}\")");
    // Insert before the last closing paren
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, net_sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "net_id": net_id, "net_name": net_name }),
    ))
}

/// Whether a board is in the KiCad 10 format, where nets are implicit.
///
/// The detection (shape first, version fallback) moved to
/// [`konnect_sexp::net::names_nets_in_place`] so the write side (#192) shares
/// it; this wrapper keeps the call sites readable.
fn board_is_kicad_10(tree: &konnect_sexp::SexpNode) -> bool {
    konnect_sexp::net::names_nets_in_place(tree)
}

async fn handle_route_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
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
    let width = args["width"].as_f64().unwrap_or(0.25);

    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, args, |c| c
        .add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2));
    Ok(CallToolResult::json(&json!({
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 }
    })))
}

async fn handle_route_pad_to_pad(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad1 = match require_str(args, "pad1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad2 = match require_str(args, "pad2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    let width = args["width"].as_f64().unwrap_or(0.25);

    // Look up pad positions from the PCB S-expression file
    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    let pos1 = find_pad_board_position(&tree, &ref1, &pad1)?;
    let pos2 = find_pad_board_position(&tree, &ref2, &pad2)?;

    // Route an L-bend: horizontal first, then vertical
    let (x1, y1) = pos1;
    let (x2, y2) = pos2;
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();

    if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
        // Already axis-aligned: single segment
        ipc!(ctx, args, |c| c
            .add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2));
    } else {
        // L-bend: horizontal then vertical
        let mid_x = x2;
        let mid_y = y1;
        let net_a = net_name.clone();
        let net_b = net_name.clone();
        let layer_a = layer.clone();
        let layer_b = layer.clone();
        ipc!(ctx, args, |c| {
            c.add_track(&net_a, &layer_a, width, x1, y1, mid_x, mid_y)?;
            c.add_track(&net_b, &layer_b, width, mid_x, mid_y, x2, y2)?;
            Ok(())
        });
    }

    Ok(CallToolResult::json(&json!({
        "routed": true,
        "net": net_name, "layer": layer, "width": width,
        "from": { "ref": ref1, "pad": pad1, "x": x1, "y": y1 },
        "to":   { "ref": ref2, "pad": pad2, "x": x2, "y": y2 }
    })))
}

/// Look up a pad's board-space (x, y) position from the parsed PCB S-expression tree.
fn find_pad_board_position(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
    pad_number: &str,
) -> anyhow::Result<(f64, f64)> {
    let fp_node = tree
        .find_all("footprint")
        .into_iter()
        .find(|fp| {
            fp.find_all("property").iter().any(|p| {
                p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                    && p.get(2).and_then(|n| n.as_str()) == Some(reference)
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found on board", reference))?;

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pad = fp_node
        .find_all("pad")
        .into_iter()
        .find(|p| p.get(1).and_then(|n| n.as_str()) == Some(pad_number))
        .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found on '{}'", pad_number, reference))?;

    let pad_at = pad
        .find("at")
        .ok_or_else(|| anyhow::anyhow!("Pad has no (at) node"))?;
    let local_x = pad_at.get_f64(1).unwrap_or(0.0);
    let local_y = pad_at.get_f64(2).unwrap_or(0.0);

    // Transform local pad coords to board space (rotation only).
    // Uses the canonical KiCAD transform — see konnect_sexp::geometry.
    Ok(konnect_sexp::geometry::transform_pad(
        local_x, local_y, fp_x, fp_y, fp_rot,
    ))
}

async fn handle_add_via(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
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
    let drill = args["drill"].as_f64().unwrap_or(0.4);
    let pad_size = args["pad_size"].as_f64().unwrap_or(0.8);

    let net_ipc = net_name.clone();
    ipc!(ctx, args, |c| c.add_via(&net_ipc, x, y, drill, pad_size));
    Ok(CallToolResult::json(
        &json!({ "net": net_name, "x": x, "y": y, "drill": drill, "pad_size": pad_size }),
    ))
}

async fn handle_add_copper_pour(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let clearance = args["clearance"].as_f64().unwrap_or(0.2);
    let min_w = args["min_width"].as_f64().unwrap_or(0.25);
    let pts_arr = match args["points"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'points' array")),
    };

    let pts: Vec<(f64, f64)> = pts_arr
        .iter()
        .filter_map(|p| Some((p["x"].as_f64()?, p["y"].as_f64()?)))
        .collect();
    if pts.len() < 3 {
        return Ok(CallToolResult::error("Zone requires at least 3 points"));
    }

    if let Some(refusal) = crate::tools::pcb_board::refuse_if_board_open_in_kicad(
        ctx.config.ipc_address.clone(),
        &board_path,
        "copper pour",
    )
    .await?
    {
        return Ok(refusal);
    }

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parse_sexp(&content)?;
    let Some(net) = konnect_sexp::net::net_ref_for_write(&tree, &net_name) else {
        return Ok(unknown_net_error(&net_name, &board_path));
    };
    let zone_s = format_zone(&net, &layer, clearance, min_w, &pts);
    let close = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close, zone_s)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "net": net_name, "layer": layer, "points": pts.len() }),
    ))
}

async fn handle_delete_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let uuid_ipc = uuid.clone();
    ipc!(ctx, args, |c| c.delete_track(&uuid_ipc));
    Ok(CallToolResult::json(&json!({ "deleted_uuid": uuid })))
}

async fn handle_query_traces(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net = args["net_name"].as_str().map(String::from);
    let layer = args["layer"].as_str().map(String::from);

    let tracks = ipc!(ctx, args, |c| {
        c.get_tracks(net.as_deref(), layer.as_deref())
    });

    let items: Vec<serde_json::Value> = tracks
        .iter()
        .map(|t| {
            json!({
                "uuid": t.uuid,
                "net": t.net_name, "layer": t.layer, "width": t.width,
                "x1": t.start.x, "y1": t.start.y,
                "x2": t.end.x,   "y2": t.end.y
            })
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "traces": items }),
    ))
}

async fn handle_get_nets_list(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let nets = ipc!(ctx, args, |c| c.get_nets());
    let items: Vec<serde_json::Value> = nets
        .iter()
        .map(|n| json!({ "name": n.name, "netcode": n.netcode }))
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "nets": items }),
    ))
}

async fn handle_modify_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
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
    let width = args["width"].as_f64().unwrap_or(0.25);

    let uuid_ipc = uuid.clone();
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, args, |c| {
        c.delete_track(&uuid_ipc)?;
        c.add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2)
    });
    Ok(CallToolResult::json(&json!({
        "modified_uuid": uuid,
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 }
    })))
}

/// The sibling `<project>.kicad_pro`, which is where KiCad ≥ 7 keeps net
/// classes. The board file has no netclass container at all — the pre-#190
/// code inserted `(netclass …)` as a direct child of `(kicad_pcb`, a token
/// pcbnew's parser rejects, so the board no longer loaded.
fn project_settings_path(board_path: &std::path::Path) -> std::path::PathBuf {
    board_path.with_extension("kicad_pro")
}

/// Load the project JSON, refusing (rather than inventing a file KiCad never
/// reads) when it is absent.
fn load_project_settings(
    board_path: &std::path::Path,
) -> anyhow::Result<Result<(std::path::PathBuf, serde_json::Value), CallToolResult>> {
    let pro = project_settings_path(board_path);
    if !pro.exists() {
        return Ok(Err(CallToolResult::error(format!(
            "No project file at {} — net classes live in the .kicad_pro since KiCad 7, \
             and a class written anywhere else is never read. Create the project \
             (KiCad: File > Save a Copy, or place the board inside a project) and retry.",
            pro.display()
        ))));
    }
    let settings: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&pro)?)
        .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", pro.display()))?;
    Ok(Ok((pro, settings)))
}

fn save_project_settings(
    pro: &std::path::Path,
    settings: &serde_json::Value,
) -> anyhow::Result<()> {
    // KiCad's own writer emits 2-space-indented JSON with alphabetical keys;
    // serde_json's pretty printer matches both, so the diff stays minimal.
    write_atomic(
        pro,
        &format!("{}\n", serde_json::to_string_pretty(settings)?),
    )?;
    Ok(())
}

async fn handle_create_netclass(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let name = match require_str(args, "name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    // KiCad's key, this tool's argument name, and the value a *new* class
    // takes when the caller says nothing. The defaults belong to creation
    // only: folding them in before an update turned "widen HV's track" into a
    // silent reset of the clearance, drill and via size the caller had tuned.
    const FIELDS: [(&str, &str, f64); 4] = [
        ("clearance", "clearance", 0.2),
        ("track_width", "trace_width", 0.25),
        ("via_drill", "via_drill", 0.4),
        ("via_diameter", "via_diameter", 0.8),
    ];

    let (pro, mut settings) = match load_project_settings(&board_path)? {
        Ok(v) => v,
        Err(refusal) => return Ok(refusal),
    };

    let net_settings = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: top level is not a JSON object", pro.display()))?
        .entry("net_settings")
        .or_insert_with(
            || json!({ "classes": [], "meta": { "version": 5 }, "netclass_patterns": [] }),
        );
    let classes = net_settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: net_settings is not an object", pro.display()))?
        .entry("classes")
        .or_insert_with(|| json!([]));
    let classes = classes.as_array_mut().ok_or_else(|| {
        anyhow::anyhow!("{}: net_settings.classes is not an array", pro.display())
    })?;

    // KiCad keys classes by name; a second entry with the same name is
    // undefined in its dialog, so an existing class is updated in place.
    let mut changed = true;
    let updated = if let Some(class) = classes.iter_mut().find(|c| c["name"] == json!(name)) {
        let before = class.clone();
        for (key, arg, _) in FIELDS {
            if let Some(value) = opt_f64(args, arg) {
                class[key] = json!(value);
            }
        }
        changed = *class != before;
        true
    } else {
        let mut class = json!({ "name": name, "priority": 0 });
        for (key, arg, default) in FIELDS {
            class[key] = json!(opt_f64(args, arg).unwrap_or(default));
        }
        classes.push(class);
        false
    };
    // Report the class as it now stands rather than the arguments that came
    // in: on an update most of it was never named by the caller.
    let stored = classes
        .iter()
        .find(|c| c["name"] == json!(name))
        .cloned()
        .unwrap_or_else(|| json!({}));
    // Naming no value at all leaves the class exactly as it was, and so does
    // passing the values it already holds. Saving anyway would rewrite the
    // whole project file — the serialiser re-emits the document rather than
    // patching it — for a call that decided nothing.
    if changed {
        save_project_settings(&pro, &settings)?;
    }

    Ok(CallToolResult::json(&json!({
        "created_netclass": name,
        "updated_existing": updated,
        "clearance": stored["clearance"], "trace_width": stored["track_width"],
        "via_drill": stored["via_drill"], "via_diameter": stored["via_diameter"],
        "file": pro.display().to_string(),
        "note": "Netclasses live in the project file; assign nets with assign_net_to_class. \
                 KiCad reads the change on next project open."
    })))
}

/// KiCad's netclass patterns are wildcard expressions: `*` stands for any run
/// of characters including none, `?` for exactly one. A pattern carrying
/// neither is an exact name — which is the shape `assign_net_to_class` writes.
///
/// Iterative rather than recursive: a pattern is user data, and `*`-heavy input
/// makes the naive recursion blow the stack on names a real board can carry.
fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = name.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have consumed too little.
    let (mut star, mut retry) = (None, 0usize);

    while t < txt.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            retry = t;
            p += 1;
        } else if let Some(s) = star {
            // Backtrack: let the last `*` swallow one more character.
            p = s + 1;
            retry += 1;
            t = retry;
        } else {
            return false;
        }
    }
    // Trailing `*`s may still match the empty rest.
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

/// The four settings `create_netclass` writes, as (KiCad's key, this API's
/// name). Reported per class so a caller can see what a class holds before
/// overwriting it — the gap that made #220 hard to review.
const NETCLASS_FIELDS: [(&str, &str); 4] = [
    ("clearance", "clearance"),
    ("track_width", "trace_width"),
    ("via_drill", "via_drill"),
    ("via_diameter", "via_diameter"),
];

async fn handle_get_netclasses(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let (pro, settings) = match load_project_settings(&board_path)? {
        Ok(v) => v,
        Err(refusal) => return Ok(refusal),
    };

    let classes = settings["net_settings"]["classes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let patterns = settings["net_settings"]["netclass_patterns"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    // The nets come from the board file rather than IPC: this is a read-only
    // query that must answer with KiCad closed. They are read with
    // `collect_net_keys`, not `find_all("net")` — KiCad 10 writes no top-level
    // net table at all, so a direct-children scan finds zero nets on every
    // current board and every pattern would match nothing. A board held open
    // with unsaved edits reads as last saved, which the payload says out loud.
    let (net_names, nets_note): (Vec<String>, String) = match std::fs::read_to_string(&board_path) {
        Ok(text) => match konnect_sexp::parse_sexp(&text) {
            Ok(tree) => {
                let mut names: Vec<String> = konnect_sexp::net::collect_net_keys(&tree)
                    .into_iter()
                    .collect();
                names.sort();
                (
                    names,
                    "board file as last saved; unsaved edits in a running KiCad are not visible"
                        .to_string(),
                )
            }
            Err(e) => (
                Vec::new(),
                format!("board could not be parsed ({e}); nets unavailable"),
            ),
        },
        Err(e) => (
            Vec::new(),
            format!("board could not be read ({e}); nets unavailable"),
        ),
    };

    let mut out = Vec::new();
    for class in &classes {
        let name = class["name"].as_str().unwrap_or_default().to_string();
        // KiCad's own fallback class. Its clearance explains DRC results that
        // no explicit class accounts for, so it is reported, not filtered.
        let is_default = name == "Default";

        let mine: Vec<&serde_json::Value> = patterns
            .iter()
            .filter(|p| p["netclass"].as_str() == Some(name.as_str()))
            .collect();
        let pattern_strings: Vec<String> = mine
            .iter()
            .filter_map(|p| p["pattern"].as_str().map(String::from))
            .collect();

        let mut matched: Vec<String> = net_names
            .iter()
            .filter(|net| pattern_strings.iter().any(|pat| wildcard_matches(pat, net)))
            .cloned()
            .collect();
        matched.sort();
        matched.dedup();

        let mut entry = json!({
            "name": name,
            "is_default": is_default,
            "priority": class["priority"],
            "patterns": pattern_strings,
            "matched_nets": matched,
        });
        for (key, api) in NETCLASS_FIELDS {
            entry[api] = class[key].clone();
        }
        out.push(entry);
    }

    // A pattern naming a class that does not exist does nothing in KiCad, and
    // is invisible in its dialog. Surfacing it here is the whole point of a
    // read tool: it is the state assign_net_to_class refuses to create but a
    // hand-edited or third-party project file can still hold.
    let orphan_patterns: Vec<serde_json::Value> = patterns
        .iter()
        .filter(|p| {
            let target = p["netclass"].as_str().unwrap_or_default();
            !classes.iter().any(|c| c["name"] == json!(target))
        })
        .cloned()
        .collect();

    // Netclass membership is many-to-many: KiCad forms an aggregate class per
    // net, taking each property from the highest-priority class that sets it,
    // with Default filling what is left. Naming one winning class per net
    // would be a fiction, so the mapping is reported as it is.
    Ok(CallToolResult::json(&json!({
        "file": pro.display().to_string(),
        "count": out.len(),
        "netclasses": out,
        "nets_on_board": net_names.len(),
        "nets_source": nets_note,
        "orphan_patterns": orphan_patterns,
        "note": "A net can match several classes; KiCad then takes each property from the \
                 highest-priority class that sets it and falls back to Default. Lower \
                 priority numbers rank higher.",
    })))
}

async fn handle_assign_net_to_class(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let netclass = match require_str(args, "netclass") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let (pro, mut settings) = match load_project_settings(&board_path)? {
        Ok(v) => v,
        Err(refusal) => return Ok(refusal),
    };

    // The class must exist — a pattern naming an unknown class silently does
    // nothing in KiCad, which is exactly the failure shape #190 removed.
    let known: Vec<String> = settings["net_settings"]["classes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !known.iter().any(|n| n == &netclass) {
        return Ok(CallToolResult::error(format!(
            "Netclass '{}' not found in {} — available: {}. Create it with create_netclass.",
            netclass,
            pro.display(),
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        )));
    }

    // Membership is a netclass_patterns entry; the exact net name is a valid
    // pattern. One pattern maps to one class, so a re-assignment moves the
    // entry rather than adding a competing one.
    let patterns = settings["net_settings"]
        .as_object_mut()
        .expect("checked above")
        .entry("netclass_patterns")
        .or_insert_with(|| json!([]));
    let patterns = patterns.as_array_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "{}: net_settings.netclass_patterns is not an array",
            pro.display()
        )
    })?;

    let mut previous_class: Option<String> = None;
    if let Some(entry) = patterns
        .iter_mut()
        .find(|p| p["pattern"] == json!(net_name))
    {
        if entry["netclass"] == json!(netclass) {
            return Ok(CallToolResult::json(&json!({
                "already_assigned": true,
                "net_name": net_name,
                "netclass": netclass,
                "file": pro.display().to_string()
            })));
        }
        previous_class = entry["netclass"].as_str().map(String::from);
        entry["netclass"] = json!(netclass);
    } else {
        patterns.push(json!({ "netclass": netclass, "pattern": net_name }));
    }
    save_project_settings(&pro, &settings)?;

    Ok(CallToolResult::json(&json!({
        "assigned": true,
        "net_name": net_name,
        "netclass": netclass,
        "previous_class": previous_class,
        "file": pro.display().to_string()
    })))
}

async fn handle_route_diff_pair(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net_pos = match require_str(args, "net_pos") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_neg = match require_str(args, "net_neg") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
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
    let width = args["width"].as_f64().unwrap_or(0.1);
    let gap = args["gap"].as_f64().unwrap_or(0.1);
    let offset = (gap + width) / 2.0;

    // Route two parallel traces offset perpendicular to the direction
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt().max(1e-9);
    let perp_x = -dy / len * offset;
    let perp_y = dx / len * offset;

    let np_ipc = net_pos.clone();
    let nn_ipc = net_neg.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, args, |c| {
        c.add_track(
            &np_ipc,
            &layer_ipc,
            width,
            x1 + perp_x,
            y1 + perp_y,
            x2 + perp_x,
            y2 + perp_y,
        )?;
        c.add_track(
            &nn_ipc,
            &layer_ipc,
            width,
            x1 - perp_x,
            y1 - perp_y,
            x2 - perp_x,
            y2 - perp_y,
        )
    });

    Ok(CallToolResult::json(&json!({
        "net_pos": net_pos, "net_neg": net_neg,
        "layer": layer, "width": width, "gap": gap
    })))
}

#[cfg(test)]
mod add_net_format_tests {
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

    /// Runs add_net against a throwaway copy of `board` and returns the
    /// handler result together with the file as it stands afterwards, so a
    /// test can assert both what the caller was told and what was written.
    async fn add_net_to(board: &str) -> (CallToolResult, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board).unwrap();
        let result = handle_add_net(
            &json!({ "board": path.to_str().unwrap(), "net_name": "NEWNET" }),
            &test_ctx(),
        )
        .await
        .expect("handler should return");
        let after = std::fs::read_to_string(&path).unwrap();
        (result, after)
    }

    fn text_of(result: &CallToolResult) -> String {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        }
    }

    /// A KiCad 10 board has no net table at all, so there is nothing an insert
    /// can add. Writing one anyway is the "reports success, does nothing"
    /// pattern — the board would be unchanged in KiCad's eyes while the caller
    /// was told the net existed.
    #[tokio::test]
    async fn a_kicad_10_board_is_refused_rather_than_silently_edited() {
        let board = "(kicad_pcb\n\t(version 20260206)\n\
            \t(segment (start 0 0) (end 1 0) (net \"GND\"))\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(result.is_error, "must fail closed: {}", text_of(&result));
        let msg = text_of(&result);
        assert!(msg.contains("KiCad 10"), "{msg}");
        assert!(msg.contains("route_trace"), "must point somewhere: {msg}");
        assert_eq!(after, board, "the board must not be touched");
    }

    /// Even with no net named anywhere, the format version still identifies a
    /// KiCad 10 board, and refusing is the safe direction when structure alone
    /// cannot say.
    #[tokio::test]
    async fn a_blank_kicad_10_board_is_refused_on_its_version() {
        let board = "(kicad_pcb\n\t(version 20260306)\n\t(generator \"pcbnew\")\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(result.is_error, "{}", text_of(&result));
        assert_eq!(after, board);
    }

    #[tokio::test]
    async fn a_legacy_board_still_gets_its_net() {
        let board = "(kicad_pcb\n  (version 20241229)\n  (net 0 \"\")\n  (net 1 \"GND\")\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert!(after.contains("(net 2 \"NEWNET\")"), "{after}");
    }

    /// The old id was `content.matches("(net ").count()`, which counted every
    /// reference on every pad, segment and zone — so on any real board the
    /// "next" id collided with ids already in use.
    #[tokio::test]
    async fn the_next_id_is_one_past_the_highest_not_a_count_of_occurrences() {
        let board = "(kicad_pcb\n  (version 20241229)\n  (net 0 \"\")\n  (net 1 \"GND\")\n  \
            (net 7 \"VCC\")\n  (segment (start 0 0) (end 1 0) (net 7))\n  \
            (segment (start 1 0) (end 2 0) (net 7))\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert!(after.contains("(net 8 \"NEWNET\")"), "{after}");
    }

    /// A 9.99 development build wrote the legacy shape with a 2025 version
    /// number; treating it as KiCad 10 on the version alone would refuse a
    /// board that an insert works perfectly well on.
    #[tokio::test]
    async fn a_9_99_development_format_is_treated_as_legacy() {
        let board = "(kicad_pcb\n  (version 20250610)\n  (net 0 \"\")\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert!(after.contains("(net 1 \"NEWNET\")"), "{after}");
    }
}

/// Netclasses live in `<project>.kicad_pro` since KiCad 7, not the board.
/// The old handlers inserted a `(netclass …)` node into the `.kicad_pcb` —
/// as a direct child of `(kicad_pcb` on any modern board, a token pcbnew's
/// parser rejects outright, so the board no longer loaded (#190).
#[cfg(test)]
mod netclass_tests {
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

    const BOARD: &str = "(kicad_pcb\n\t(version 20250610)\n\t(generator \"pcbnew\")\n)\n";

    /// A board plus, optionally, the sibling `.kicad_pro` KiCad writes.
    fn fixture(with_project: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("demo.kicad_pcb");
        std::fs::write(&board, BOARD).unwrap();
        if with_project {
            std::fs::write(
                dir.path().join("demo.kicad_pro"),
                serde_json::to_string_pretty(&json!({
                    "board": { "design_settings": {} },
                    "meta": { "filename": "demo.kicad_pro", "version": 3 }
                }))
                .unwrap(),
            )
            .unwrap();
        }
        (dir, board)
    }

    fn text_of(r: &CallToolResult) -> String {
        match r.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn project_json(board: &std::path::Path) -> serde_json::Value {
        let pro = board.with_extension("kicad_pro");
        serde_json::from_str(&std::fs::read_to_string(pro).unwrap()).unwrap()
    }

    async fn create(board: &std::path::Path, args: serde_json::Value) -> CallToolResult {
        let mut args = args;
        args["board"] = json!(board.to_str().unwrap());
        handle_create_netclass(&args, &test_ctx()).await.unwrap()
    }

    async fn assign(board: &std::path::Path, net: &str, class: &str) -> CallToolResult {
        handle_assign_net_to_class(
            &json!({ "board": board.to_str().unwrap(), "net_name": net, "netclass": class }),
            &test_ctx(),
        )
        .await
        .unwrap()
    }

    /// The board file is data KiCad refuses if a netclass node lands in it;
    /// the class must go into the project file and the board must not change
    /// by a single byte.
    #[tokio::test]
    async fn create_netclass_writes_the_project_file_and_leaves_the_board_alone() {
        let (_dir, board) = fixture(true);
        let result = create(
            &board,
            json!({ "name": "HV", "clearance": 0.5, "trace_width": 0.3 }),
        )
        .await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);

        let pro = project_json(&board);
        let classes = pro["net_settings"]["classes"].as_array().unwrap();
        let hv = classes
            .iter()
            .find(|c| c["name"] == "HV")
            .expect("HV class in net_settings.classes");
        assert_eq!(hv["clearance"], json!(0.5));
        assert_eq!(hv["track_width"], json!(0.3));
        assert_eq!(hv["via_diameter"], json!(0.8));
        assert_eq!(hv["via_drill"], json!(0.4));
        // The existing project content survives the edit.
        assert_eq!(pro["meta"]["filename"], json!("demo.kicad_pro"));
    }

    /// No project file means nowhere KiCad would ever read the class from;
    /// inventing one risks orphan settings, so the tool refuses instead.
    #[tokio::test]
    async fn create_netclass_without_a_project_file_refuses_and_writes_nothing() {
        let (dir, board) = fixture(false);
        let result = create(&board, json!({ "name": "HV" })).await;
        assert!(result.is_error, "{}", text_of(&result));
        assert!(
            text_of(&result).contains("kicad_pro"),
            "{}",
            text_of(&result)
        );
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
        assert!(!dir.path().join("demo.kicad_pro").exists());
    }

    /// Same name twice updates in place — KiCad keys classes by name and two
    /// entries with one name is undefined behaviour in its dialog.
    #[tokio::test]
    async fn create_netclass_updates_an_existing_class_in_place() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV", "clearance": 0.3 })).await;
        let second = create(&board, json!({ "name": "HV", "clearance": 0.6 })).await;
        assert!(!second.is_error, "{}", text_of(&second));

        let pro = project_json(&board);
        let classes = pro["net_settings"]["classes"].as_array().unwrap();
        assert_eq!(
            classes.iter().filter(|c| c["name"] == "HV").count(),
            1,
            "{classes:?}"
        );
        assert_eq!(classes[0]["clearance"], json!(0.6));
    }

    /// Re-running the tool is how a caller adjusts one setting of a class it
    /// already tuned. Every argument carries a schema default, so applying
    /// those defaults on an update silently reset the three settings the
    /// caller did not name — the clearance a board was routed to, gone on a
    /// call that only meant to widen a track.
    #[tokio::test]
    async fn create_netclass_leaves_settings_the_caller_did_not_name_alone() {
        let (_dir, board) = fixture(true);
        create(
            &board,
            json!({ "name": "HV", "clearance": 1.5, "trace_width": 0.5,
                    "via_drill": 0.45, "via_diameter": 0.85 }),
        )
        .await;
        let second = create(&board, json!({ "name": "HV", "trace_width": 0.9 })).await;
        assert!(!second.is_error, "{}", text_of(&second));

        let pro = project_json(&board);
        let hv = pro["net_settings"]["classes"][0].clone();
        assert_eq!(hv["track_width"], json!(0.9), "the named value changes");
        assert_eq!(hv["clearance"], json!(1.5), "{hv}");
        assert_eq!(hv["via_drill"], json!(0.45), "{hv}");
        assert_eq!(hv["via_diameter"], json!(0.85), "{hv}");

        // The result echoes the stored class, not the one argument passed.
        let echoed: serde_json::Value = serde_json::from_str(&text_of(&second)).unwrap();
        assert_eq!(echoed["clearance"], json!(1.5));
        assert_eq!(echoed["trace_width"], json!(0.9));
    }

    /// With the defaults gone from the update path, a call that names no value
    /// decides nothing — so it must not write. `save_project_settings`
    /// re-serialises the whole document rather than patching it, so saving
    /// anyway rewrites every line of the project file for a call that is, in
    /// effect, a read.
    #[tokio::test]
    async fn a_call_that_changes_nothing_leaves_the_project_file_untouched() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV", "clearance": 1.5 })).await;

        // Re-written by hand in a shape the serialiser would not produce, so
        // any save at all is visible in the bytes.
        let pro = board.with_extension("kicad_pro");
        let compact = serde_json::to_string(
            &serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&pro).unwrap())
                .unwrap(),
        )
        .unwrap();
        std::fs::write(&pro, &compact).unwrap();

        // Naming no value at all: a read.
        let result = create(&board, json!({ "name": "HV" })).await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert_eq!(std::fs::read_to_string(&pro).unwrap(), compact);
        // It still reports what the class holds.
        let echoed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(echoed["clearance"], json!(1.5));
        assert_eq!(echoed["updated_existing"], json!(true));

        // Naming the values it already holds: also nothing to decide.
        create(&board, json!({ "name": "HV", "clearance": 1.5 })).await;
        assert_eq!(std::fs::read_to_string(&pro).unwrap(), compact);

        // A real change still writes.
        create(&board, json!({ "name": "HV", "clearance": 0.9 })).await;
        assert_ne!(std::fs::read_to_string(&pro).unwrap(), compact);
    }

    /// A new class still gets the documented defaults for whatever the caller
    /// leaves out — the fix above must not turn creation into a partial class.
    #[tokio::test]
    async fn a_new_class_is_still_created_with_the_documented_defaults() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV" })).await;

        let hv = project_json(&board)["net_settings"]["classes"][0].clone();
        assert_eq!(hv["clearance"], json!(0.2), "{hv}");
        assert_eq!(hv["track_width"], json!(0.25), "{hv}");
        assert_eq!(hv["via_drill"], json!(0.4), "{hv}");
        assert_eq!(hv["via_diameter"], json!(0.8), "{hv}");
    }

    /// Membership is a netclass_patterns entry keyed by the exact net name.
    #[tokio::test]
    async fn assign_net_adds_a_pattern_once_and_can_move_it() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV" })).await;
        create(&board, json!({ "name": "LV" })).await;

        let first = assign(&board, "GND", "HV").await;
        assert!(!first.is_error, "{}", text_of(&first));
        let pro = project_json(&board);
        let patterns = pro["net_settings"]["netclass_patterns"].as_array().unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0]["netclass"], json!("HV"));
        assert_eq!(patterns[0]["pattern"], json!("GND"));

        // Idempotent.
        let again = assign(&board, "GND", "HV").await;
        let body: serde_json::Value = serde_json::from_str(&text_of(&again)).unwrap();
        assert_eq!(body["already_assigned"], json!(true));
        let pro = project_json(&board);
        assert_eq!(
            pro["net_settings"]["netclass_patterns"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Reassigning moves the one entry rather than adding a second.
        let moved = assign(&board, "GND", "LV").await;
        let body: serde_json::Value = serde_json::from_str(&text_of(&moved)).unwrap();
        assert_eq!(body["previous_class"], json!("HV"), "{body}");
        let pro = project_json(&board);
        let patterns = pro["net_settings"]["netclass_patterns"].as_array().unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0]["netclass"], json!("LV"));

        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }

    /// A board whose nets are named the way KiCad 10 writes them: in place, on
    /// the items themselves, with no top-level net table anywhere.
    ///
    /// This shape is the whole point of the fixture. A board carrying the
    /// pre-10 `(net 1 "GND")` table would pass even a collector that only
    /// scans direct children of `(kicad_pcb …)` — and that collector reports
    /// zero nets on every board KiCad 10 saves.
    fn fixture_with_nets(nets: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, board) = fixture(true);
        let mut text = String::from("(kicad_pcb\n\t(version 20260206)\n\t(generator \"pcbnew\")\n");
        text.push_str("\t(footprint \"R_0402\"\n\t\t(layer \"F.Cu\")\n");
        for (i, n) in nets.iter().enumerate() {
            text.push_str(&format!(
                "\t\t(pad \"{}\" smd roundrect\n\t\t\t(at {} 0)\n\t\t\t(net \"{}\")\n\t\t)\n",
                i + 1,
                i,
                n
            ));
        }
        text.push_str("\t)\n");
        // The same nets appear again on copper — KiCad repeats the reference on
        // every item, and a collector that counts occurrences over-reports.
        for n in nets {
            text.push_str(&format!(
                "\t(segment (start 0 0) (end 1 0) (width 0.2) (layer \"F.Cu\") (net \"{n}\"))\n"
            ));
        }
        text.push_str(")\n");
        std::fs::write(&board, text).unwrap();
        (dir, board)
    }

    async fn get_classes(board: &std::path::Path) -> serde_json::Value {
        let result =
            handle_get_netclasses(&json!({ "board": board.to_str().unwrap() }), &test_ctx())
                .await
                .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        serde_json::from_str(&text_of(&result)).unwrap()
    }

    #[test]
    fn wildcards_match_the_way_kicad_documents_them() {
        // An exact pattern — what assign_net_to_class writes.
        assert!(wildcard_matches("GND", "GND"));
        assert!(!wildcard_matches("GND", "GNDA"));
        // `*` spans any run, including none.
        assert!(wildcard_matches("HV_*", "HV_IN"));
        assert!(wildcard_matches("HV_*", "HV_"));
        assert!(!wildcard_matches("HV_*", "LV_IN"));
        assert!(wildcard_matches("*", "anything"));
        assert!(wildcard_matches("/Power/*", "/Power/VBUS"));
        // `?` is exactly one character.
        assert!(wildcard_matches("D?", "D0"));
        assert!(!wildcard_matches("D?", "D"));
        assert!(!wildcard_matches("D?", "D12"));
        // Backtracking: the first `*` must give characters back.
        assert!(wildcard_matches("*_P", "USB_D_P"));
        assert!(!wildcard_matches("*_P", "USB_D_N"));
        // A run of stars must not blow up or mis-answer.
        assert!(wildcard_matches("**a**", "xxaxx"));
    }

    /// The read path is the whole point of #222: what a class holds must be
    /// visible before create_netclass overwrites part of it.
    #[tokio::test]
    async fn get_netclasses_reports_settings_patterns_and_matching_nets() {
        let (_dir, board) = fixture_with_nets(&["GND", "HV_IN", "HV_OUT"]);
        create(&board, json!({ "name": "HV", "clearance": 0.5 })).await;
        assign(&board, "GND", "HV").await;

        let body = get_classes(&board).await;
        assert_eq!(body["count"], json!(1));
        let hv = &body["netclasses"][0];
        assert_eq!(hv["name"], json!("HV"));
        assert_eq!(hv["clearance"], json!(0.5));
        // Values the caller never named are reported from the file, not echoed
        // back from the arguments — the gap that made #220 unreviewable.
        assert_eq!(hv["trace_width"], json!(0.25));
        assert_eq!(hv["patterns"], json!(["GND"]));
        assert_eq!(hv["matched_nets"], json!(["GND"]));
        assert_eq!(hv["is_default"], json!(false));
        // Each net is named on a pad *and* on a segment; they are one net each.
        assert_eq!(body["nets_on_board"], json!(3));
    }

    /// The regression this tool would otherwise ship with: KiCad 10 writes no
    /// top-level net table, so reading nets as direct children of
    /// `(kicad_pcb …)` finds nothing and every pattern silently matches
    /// nothing — a plausible-looking empty answer on every current board.
    #[tokio::test]
    async fn nets_named_in_place_are_found_on_a_kicad_10_board() {
        let (_dir, board) = fixture_with_nets(&["GND", "HV_IN"]);
        // No net table at all, the way KiCad 10 saves.
        let text = std::fs::read_to_string(&board).unwrap();
        let tree = konnect_sexp::parse_sexp(&text).unwrap();
        assert_eq!(
            tree.find_all("net").len(),
            0,
            "fixture must not carry a top-level net table"
        );

        create(&board, json!({ "name": "HV" })).await;
        assign(&board, "HV_IN", "HV").await;

        let body = get_classes(&board).await;
        assert_eq!(body["nets_on_board"], json!(2));
        assert_eq!(body["netclasses"][0]["matched_nets"], json!(["HV_IN"]));
    }

    /// The pre-10 shape must keep working: there the name follows a numeric id,
    /// and the same net is repeated as a bare `(net 1)` on every item.
    #[tokio::test]
    async fn the_pre_kicad_10_net_table_still_resolves() {
        let (_dir, board) = fixture(true);
        std::fs::write(
            &board,
            "(kicad_pcb\n\t(version 20250610)\n\t(generator \"pcbnew\")\n\
             \t(net 0 \"\")\n\t(net 1 \"GND\")\n\t(net 2 \"HV_IN\")\n\
             \t(segment (start 0 0) (end 1 0) (net 1))\n)\n",
        )
        .unwrap();
        create(&board, json!({ "name": "HV" })).await;
        assign(&board, "HV_IN", "HV").await;

        let body = get_classes(&board).await;
        // Net 0 is the unconnected pseudo-net and is not a net a user has.
        assert_eq!(body["nets_on_board"], json!(2));
        assert_eq!(body["netclasses"][0]["matched_nets"], json!(["HV_IN"]));
    }

    /// A wildcard pattern is the case a per-net lookup cannot answer: one
    /// class matches many nets, and nothing in the project file lists them.
    #[tokio::test]
    async fn a_wildcard_pattern_resolves_against_the_board_nets() {
        let (_dir, board) = fixture_with_nets(&["HV_IN", "HV_OUT", "GND"]);
        create(&board, json!({ "name": "HV" })).await;
        // assign_net_to_class only writes exact names, so write the wildcard
        // the way a user's KiCad dialog would.
        let pro = board.with_extension("kicad_pro");
        let mut settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pro).unwrap()).unwrap();
        settings["net_settings"]["netclass_patterns"] =
            json!([{ "pattern": "HV_*", "netclass": "HV" }]);
        std::fs::write(&pro, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        assert_eq!(
            body["netclasses"][0]["matched_nets"],
            json!(["HV_IN", "HV_OUT"])
        );
    }

    /// A net may sit in several classes at once — KiCad aggregates them by
    /// priority. Reporting a single winning class per net would be a fiction.
    #[tokio::test]
    async fn one_net_can_belong_to_several_classes() {
        let (_dir, board) = fixture_with_nets(&["USB_DP"]);
        create(&board, json!({ "name": "HighSpeed" })).await;
        create(&board, json!({ "name": "Wide" })).await;
        let pro = board.with_extension("kicad_pro");
        let mut settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pro).unwrap()).unwrap();
        settings["net_settings"]["netclass_patterns"] = json!([
            { "pattern": "USB_*", "netclass": "HighSpeed" },
            { "pattern": "*_DP",  "netclass": "Wide" },
        ]);
        std::fs::write(&pro, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        for class in body["netclasses"].as_array().unwrap() {
            assert_eq!(
                class["matched_nets"],
                json!(["USB_DP"]),
                "both classes claim the net: {class}"
            );
        }
    }

    /// A pattern naming a class that does not exist does nothing in KiCad and
    /// is invisible in its dialog. It is exactly what a read tool is for.
    #[tokio::test]
    async fn a_pattern_naming_no_class_is_reported_as_an_orphan() {
        let (_dir, board) = fixture_with_nets(&["GND"]);
        create(&board, json!({ "name": "HV" })).await;
        let pro = board.with_extension("kicad_pro");
        let mut settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pro).unwrap()).unwrap();
        settings["net_settings"]["netclass_patterns"] =
            json!([{ "pattern": "GND", "netclass": "Vanished" }]);
        std::fs::write(&pro, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        assert_eq!(body["orphan_patterns"][0]["netclass"], json!("Vanished"));
        assert_eq!(body["netclasses"][0]["matched_nets"], json!([]));
    }

    /// Default is KiCad's fallback and its clearance explains DRC results no
    /// explicit class accounts for, so it is reported and marked.
    #[tokio::test]
    async fn the_default_class_is_reported_and_marked() {
        let (_dir, board) = fixture_with_nets(&["GND"]);
        let pro = board.with_extension("kicad_pro");
        let mut settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pro).unwrap()).unwrap();
        settings["net_settings"] = json!({
            "classes": [{ "name": "Default", "clearance": 0.2, "track_width": 0.25,
                          "priority": 2147483647i64 }],
            "netclass_patterns": []
        });
        std::fs::write(&pro, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        assert_eq!(body["netclasses"][0]["is_default"], json!(true));
        assert_eq!(body["netclasses"][0]["priority"], json!(2147483647i64));
    }

    /// Reading must not write. The project file is byte-identical afterwards —
    /// #220's second half was create_netclass rewriting it for a no-op call.
    #[tokio::test]
    async fn reading_leaves_both_files_untouched() {
        let (_dir, board) = fixture_with_nets(&["GND"]);
        create(&board, json!({ "name": "HV" })).await;
        let pro = board.with_extension("kicad_pro");
        let before_pro = std::fs::read_to_string(&pro).unwrap();
        let before_board = std::fs::read_to_string(&board).unwrap();

        get_classes(&board).await;

        assert_eq!(std::fs::read_to_string(&pro).unwrap(), before_pro);
        assert_eq!(std::fs::read_to_string(&board).unwrap(), before_board);
    }

    /// Without a project file there are no netclasses to read, and the refusal
    /// must say where they actually live.
    #[tokio::test]
    async fn reading_without_a_project_file_refuses_with_the_reason() {
        let (_dir, board) = fixture(false);
        let result =
            handle_get_netclasses(&json!({ "board": board.to_str().unwrap() }), &test_ctx())
                .await
                .unwrap();
        assert!(result.is_error);
        assert!(
            text_of(&result).contains("kicad_pro"),
            "{}",
            text_of(&result)
        );
    }

    /// A project with no net_settings at all is a normal new project, not an
    /// error — it simply has no classes yet.
    #[tokio::test]
    async fn a_project_without_net_settings_reads_as_empty() {
        let (_dir, board) = fixture_with_nets(&["GND"]);
        let body = get_classes(&board).await;
        assert_eq!(body["count"], json!(0));
        assert_eq!(body["netclasses"], json!([]));
        assert_eq!(body["nets_on_board"], json!(1));
    }

    /// Assigning to a class that doesn't exist names the ones that do.
    #[tokio::test]
    async fn assign_net_to_a_missing_class_errors_naming_the_available_ones() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV" })).await;
        let result = assign(&board, "GND", "NOPE").await;
        assert!(result.is_error);
        let msg = text_of(&result);
        assert!(msg.contains("HV"), "{msg}");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }
}

/// Zones must reference their net in the same shape the board uses — KiCad 10
/// by name, legacy by declared id. Both `add_copper_pour` here and `add_zone`
/// in `pcb_board.rs` used a string-offset id lookup that returned 0 on every
/// KiCad 10 board, silently attaching the pour to the unconnected pseudo-net
/// (#192).
#[cfg(test)]
mod zone_net_format_tests {
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

    fn text_of(r: &CallToolResult) -> String {
        match r.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// KiCad 10 names nets on copper; there is no table and no ids.
    const KICAD_10: &str = "(kicad_pcb\n\t(version 20260206)\n\t(generator \"pcbnew\")\n\t(segment\n\t\t(start 10 10)\n\t\t(end 20 10)\n\t\t(width 0.2)\n\t\t(layer \"F.Cu\")\n\t\t(net \"GND\")\n\t)\n)\n";
    /// Legacy: table at top level, items reference by id.
    const LEGACY: &str = "(kicad_pcb\n  (version 20240108)\n  (generator \"pcbnew\")\n  (net 0 \"\")\n  (net 7 \"GND\")\n  (segment (start 10 10) (end 20 10) (width 0.2) (layer \"F.Cu\") (net 7))\n)\n";

    async fn pour(board: &str, net: &str) -> (CallToolResult, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board).unwrap();
        let result = handle_add_copper_pour(
            &json!({
                "board": path.to_str().unwrap(), "net_name": net, "layer": "F.Cu",
                "points": [ {"x": 0.0, "y": 0.0}, {"x": 10.0, "y": 0.0}, {"x": 10.0, "y": 10.0} ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        (result, std::fs::read_to_string(&path).unwrap())
    }

    #[tokio::test]
    async fn a_kicad_10_zone_references_the_net_by_name() {
        let (result, after) = pour(KICAD_10, "GND").await;
        assert!(!result.is_error, "{}", text_of(&result));
        let zone_at = after.find("(zone").expect("zone written");
        let zone = &after[zone_at..];
        assert!(zone.contains("(net \"GND\")"), "{zone}");
        assert!(!zone.contains("(net 0)"), "{zone}");
        assert!(
            !zone.contains("net_name"),
            "no net_name token in KiCad 10: {zone}"
        );
        assert!(zone.contains("(layers \"F.Cu\")"), "plural layers: {zone}");
        // The #142 read helpers must see the pour on GND, not orphaned.
        let tree = konnect_sexp::parse_sexp(&after).unwrap();
        assert!(konnect_sexp::net::collect_net_keys(&tree).contains("GND"));
    }

    #[tokio::test]
    async fn a_legacy_zone_keeps_the_declared_id_and_net_name_pair() {
        let (result, after) = pour(LEGACY, "GND").await;
        assert!(!result.is_error, "{}", text_of(&result));
        let zone_at = after.find("(zone").expect("zone written");
        let zone = &after[zone_at..];
        assert!(zone.contains("(net 7) (net_name \"GND\")"), "{zone}");
        assert!(zone.contains("(layer \"F.Cu\")"), "singular layer: {zone}");
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    /// The old lookup fell back to 0 — the orphan. An unknown net on a legacy
    /// board must refuse and leave the file alone.
    #[tokio::test]
    async fn an_undeclared_net_on_a_legacy_board_is_refused_not_zeroed() {
        let (result, after) = pour(LEGACY, "PWR").await;
        assert!(result.is_error, "{}", text_of(&result));
        assert!(text_of(&result).contains("add_net"), "{}", text_of(&result));
        assert_eq!(after, LEGACY, "file must be untouched");
    }
}
