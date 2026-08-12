//! `pcb_board` toolset — board setup, layers, outlines, zones, and board-level items.
//!
//! Most operations use S-expression file manipulation so they work without a running
//! KiCad instance. `get_board_info` and `get_board_extents` try the IPC API first,
//! falling back to parsing the file, and report which they used as `source` —
//! the file is the last save, so it disagrees with the IPC-backed writers here
//! whenever KiCad holds unsaved edits.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use konnect_ipc::builders;
use konnect_sexp::{
    parser::parse_sexp,
    writer::{apply_edits, new_uuid, write_atomic, SexpEdit},
};
use serde_json::json;

// Build the 4 Edge.Cuts segments forming a rectangle, packed as Any for create_items.
fn rect_outline_items(x1: f64, y1: f64, x2: f64, y2: f64, w: f64) -> Vec<prost_types::Any> {
    let sides = [
        (x1, y1, x2, y1),
        (x2, y1, x2, y2),
        (x2, y2, x1, y2),
        (x1, y2, x1, y1),
    ];
    sides
        .iter()
        .map(|&(a, b, c, d)| {
            builders::pack_any(
                &builders::board_segment("Edge.Cuts", w, a, b, c, d),
                "kiapi.board.types.BoardGraphicShape",
            )
        })
        .collect()
}

// ─── IPC helper ───────────────────────────────────────────────────────────────

async fn with_ipc<T, F>(addr: String, f: F) -> anyhow::Result<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&konnect_ipc::client::KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        let client = konnect_ipc::client::KiCadIpcClient::new(&addr);
        f(&client)
    })
    .await
    {
        Ok(Ok(r)) => Ok(Ok(r)),
        Ok(Err(e)) => Ok(Err(e.to_string())),
        Err(e) => Err(anyhow::anyhow!("Thread error: {}", e)),
    }
}

/// What a board-mutating tool should do after its IPC attempt.
pub(crate) enum BoardWrite<T = ()> {
    /// KiCAD applied the change; report `"source": "ipc"`. Carries whatever the
    /// IPC call returned, for tools that echo it back — the placed footprint,
    /// for instance.
    Ipc(T),
    /// No live KiCAD on this transport, so a direct file edit cannot race an
    /// editor; proceed with the S-expression path.
    File,
    /// KiCAD answered and refused. The caller must return this result and must
    /// NOT touch the file.
    Refused(CallToolResult),
}

/// Run `f` over IPC against the board named by `board_path`, deciding what the
/// caller may do next.
///
/// Two failure modes that used to look alike are kept apart here, both of which
/// silently corrupted work before:
///
/// * The board reached over IPC is whichever one KiCAD has open, so a request
///   naming a *different* board would edit the wrong one — `ensure_board_is_active`
///   rejects that up front (issue: `add_board_outline` writing into the open board).
/// * A file-only edit is invisible to a KiCAD holding this board open and is
///   discarded by its next save. So the fallback gate is the typed transport
///   classification, never a text match: only a request that never reached a live
///   KiCAD may edit the file; a KiCAD that answered — even with an error — fails
///   closed. `handle_add_mounting_hole` established that rule; this is it made
///   reusable, so every board-mutating tool decides the same way.
pub(crate) async fn attempt_ipc_write<T, F>(
    addr: String,
    board_path: &std::path::Path,
    what: &str,
    f: F,
) -> anyhow::Result<BoardWrite<T>>
where
    T: Send + 'static,
    F: FnOnce(&konnect_ipc::client::KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    let requested = board_path.to_path_buf();
    match crate::tools::pcb_components::with_ipc_classified(addr, move |c| {
        c.ensure_board_is_active(&requested)?;
        f(c)
    })
    .await?
    {
        Ok(value) => Ok(BoardWrite::Ipc(value)),
        Err(konnect_ipc::IpcFailure::Rejected(message)) => {
            Ok(BoardWrite::Refused(CallToolResult::error(format!(
                "KiCAD rejected the {what} over IPC: {message}. \
                 The board file was not modified — KiCAD is reachable and may hold this \
                 board open, so editing the file directly could be silently overwritten."
            ))))
        }
        Err(konnect_ipc::IpcFailure::Unreachable(_)) => Ok(BoardWrite::File),
    }
}

/// Refuse a direct file edit when KiCAD is reachable AND holds this very
/// board open: pcbnew saves from its in-memory state, so the file edit would
/// be silently discarded on its next save — success reported, nothing kept
/// (#192). For tools with no IPC implementation this guard is the honest
/// alternative to [`attempt_ipc_write`]'s fallback. A reachable KiCAD holding
/// a *different* board (or none) does not interfere with this file, and an
/// unreachable one cannot race it — both proceed.
pub(crate) async fn refuse_if_board_open_in_kicad(
    addr: String,
    board_path: &std::path::Path,
    what: &str,
) -> anyhow::Result<Option<CallToolResult>> {
    let requested = board_path.to_path_buf();
    match crate::tools::pcb_components::with_ipc_classified(addr, move |c| {
        c.ensure_board_is_active(&requested)
    })
    .await?
    {
        Ok(()) => Ok(Some(CallToolResult::error(format!(
            "KiCAD currently holds this board open, and a {what} written to the file would \
             be discarded by KiCAD's next save. Close the board in KiCAD (or make the edit \
             there) and retry — this tool has no IPC path for a live board yet."
        )))),
        Err(_) => Ok(None),
    }
}

// ─── S-expression format helpers ──────────────────────────────────────────────

fn format_gr_line(x1: f64, y1: f64, x2: f64, y2: f64, layer: &str, width: f64) -> String {
    let uuid = new_uuid();
    format!(
        "\n  (gr_line\n    (start {x1} {y1})\n    (end {x2} {y2})\n    \
         (stroke (width {width}) (type solid))\n    (layer \"{layer}\")\n    (uuid \"{uuid}\")\n  )"
    )
}

fn format_gr_text(text: &str, x: f64, y: f64, rot: f64, layer: &str, size: f64) -> String {
    let uuid = new_uuid();
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "\n  (gr_text \"{escaped}\"\n    (at {x} {y} {rot})\n    (layer \"{layer}\")\n    \
         (effects (font (size {size} {size}) (thickness 0.15)))\n    (uuid \"{uuid}\")\n  )"
    )
}

/// Library identifier a mounting hole is placed under. Shared by the IPC and
/// file paths so the two cannot drift.
fn mounting_hole_lib_id(drill_d: f64) -> String {
    format!("MountingHole:MountingHole_{drill_d:.1}mm")
}

/// Copper/mask annulus diameter around a `drill_d` mounting hole.
fn mounting_hole_pad_size(drill_d: f64) -> f64 {
    drill_d + 0.5
}

/// Footprint-local Y offset of the Reference/Value text of a mounting hole.
fn mounting_hole_text_offset(drill_d: f64) -> f64 {
    drill_d + 1.5
}

/// The single NPTH pad of a mounting hole, in footprint-local coordinates —
/// the IPC-path equivalent of the `(pad "" np_thru_hole …)` node that
/// [`format_npth_footprint`] writes.
fn mounting_hole_pad(drill_d: f64) -> konnect_ipc::IpcPadDefinition {
    let pad_size = mounting_hole_pad_size(drill_d);
    konnect_ipc::IpcPadDefinition {
        number: String::new(),
        pad_type: "np_thru_hole".to_string(),
        shape: "circle".to_string(),
        x: 0.0,
        y: 0.0,
        rotation: 0.0,
        size_x: pad_size,
        size_y: pad_size,
        drill_x: Some(drill_d),
        drill_y: Some(drill_d),
        drill_oval: false,
        layers: vec!["*.Cu".to_string(), "*.Mask".to_string()],
        roundrect_ratio: 0.0,
    }
}

fn format_npth_footprint(x: f64, y: f64, drill_d: f64, reference: &str) -> String {
    let fp_uuid = new_uuid();
    let ref_uuid = new_uuid();
    let val_uuid = new_uuid();
    let pad_uuid = new_uuid();
    let pad_size = mounting_hole_pad_size(drill_d);
    let lib_id = mounting_hole_lib_id(drill_d);
    format!(
        "\n  (footprint \"{lib_id}\"\n    \
         (layer \"F.Cu\")\n    (at {x} {y})\n    \
         (attr exclude_from_pos_files)\n    \
         (property \"Reference\" \"{reference}\"\n      (at 0 {offset} 0)\n      (layer \"F.SilkS\")\n      (uuid \"{ref_uuid}\")\n    )\n    \
         (property \"Value\" \"MountingHole\"\n      (at 0 -{offset} 0)\n      (layer \"F.Fab\")\n      (uuid \"{val_uuid}\")\n    )\n    \
         (pad \"\" np_thru_hole circle (at 0 0) (size {pad_size} {pad_size})\n      \
         (drill {drill_d})\n      (layers \"*.Cu\" \"*.Mask\")\n      (uuid \"{pad_uuid}\")\n    )\n    \
         (uuid \"{fp_uuid}\")\n  )",
        offset = mounting_hole_text_offset(drill_d)
    )
}

/// A zone S-expression in the same format the rest of the board uses: KiCad 10
/// gets `(net "GND")` and `(layers …)`, legacy boards keep the id +
/// `(net_name …)` pair and singular `(layer …)`. The net reference comes from
/// [`konnect_sexp::net::net_ref_for_write`] — resolved structurally, never by
/// string offset, which is how zones used to land on net 0 (#192).
fn format_zone_polygon(
    net: &konnect_sexp::net::NetRef,
    layer: &str,
    clearance: f64,
    min_width: f64,
    points: &[(f64, f64)],
) -> String {
    let uuid = new_uuid();
    let pts: String = points
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    format!(
        "\n  (zone {net_nodes} {layer_node} (uuid \"{uuid}\")\n    \
         (hatch edge 0.508)\n    (connect_pads (clearance {clearance}))\n    \
         (min_thickness {min_width})\n    (fill yes (thermal_gap 0.5) (thermal_bridge_width 0.5))\n    \
         (polygon (pts{pts}\n    ))\n  )",
        net_nodes = net.zone_net_nodes(),
        layer_node = net.zone_layer_node(layer),
    )
}

/// A standalone filled polygon graphic (`gr_poly`), not tied to a net or zone
/// fill — used for imported artwork rather than copper pours.
fn format_gr_poly(points: &[(f64, f64)], layer: &str) -> String {
    let uuid = new_uuid();
    let pts: String = points
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    format!(
        "\n  (gr_poly\n    (pts{pts}\n    )\n    \
         (stroke (width 0) (type solid))\n    (fill solid)\n    \
         (layer \"{layer}\")\n    (uuid \"{uuid}\")\n  )"
    )
}

/// Byte offset of the `)` that closes the block opening at `open_pos`.
///
/// Balances parens while skipping quoted strings, so it is independent of how
/// the file is indented — KiCad 9 writes two spaces, KiCad 10 writes tabs, and
/// a probe for either is wrong on the other.
fn close_of_block(content: &str, open_pos: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in content[open_pos..].char_indices() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open_pos + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The leading whitespace of the first entry inside the block at `open_pos`,
/// so an inserted sibling matches the file it is written into.
fn entry_indent(content: &str, open_pos: usize) -> Option<String> {
    let after = &content[open_pos..];
    let nl = after.find('\n')?;
    let line = &after[nl + 1..];
    let indent: String = line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    (!indent.is_empty() && line[indent.len()..].starts_with('(')).then_some(indent)
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "set_board_size",
            "Set the PCB board outline to a rectangle of the given dimensions on the Edge.Cuts layer.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string", "description": "Path to .kicad_pcb file" },
                    "width":    { "type": "number", "description": "Board width in mm" },
                    "height":   { "type": "number", "description": "Board height in mm" },
                    "origin_x": { "type": "number", "description": "Left edge X coordinate", "default": 0 },
                    "origin_y": { "type": "number", "description": "Top edge Y coordinate", "default": 0 }
                },
                "required": ["board", "width", "height"]
            }),
            |args, ctx| async move { handle_set_board_size(args, ctx).await }
        ),
        tool!(
            "get_board_info",
            "Return metadata about the PCB: title, revision, company, layer count, paper size, \
             and the number of distinct nets (excluding the unconnected pseudo-net). \
             Reads the board open in KiCad when it is reachable, else the file — \
             'source' says which. Paper size always comes from the file.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_board_info(args, ctx).await }
        ),
        tool!(
            "get_board_extents",
            "Return the bounding box of all objects on the board (tries KiCAD IPC, falls back to file parse).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_board_extents(args, ctx).await }
        ),
        tool!(
            "get_layer_list",
            "Return all layers defined in the board with their names and types.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_layer_list(args, ctx).await }
        ),
        tool!(
            "add_layer",
            "Add a new inner copper or technical layer to the board layer stack.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "layer_name":  { "type": "string", "description": "KiCAD layer name (e.g. 'In1.Cu')" },
                    "layer_type":  { "type": "string", "description": "Type: 'signal', 'power', 'mixed', 'jumper'", "default": "signal" }
                },
                "required": ["board", "layer_name"]
            }),
            |args, ctx| async move { handle_add_layer(args, ctx).await }
        ),
        tool!(
            "set_active_layer",
            "Set the active layer recorded in the board file's setup section.",
            json!({
                "type": "object",
                "properties": {
                    "board":  { "type": "string" },
                    "layer":  { "type": "string", "description": "KiCAD layer name (e.g. 'F.Cu')" }
                },
                "required": ["board", "layer"]
            }),
            |args, ctx| async move { handle_set_active_layer(args, ctx).await }
        ),
        tool!(
            "add_board_outline",
            "Add a rectangular board outline on the Edge.Cuts layer at specified coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "board":          { "type": "string" },
                    "x1":             { "type": "number", "description": "Top-left X in mm" },
                    "y1":             { "type": "number", "description": "Top-left Y in mm" },
                    "x2":             { "type": "number", "description": "Bottom-right X in mm" },
                    "y2":             { "type": "number", "description": "Bottom-right Y in mm" },
                    "corner_radius":  { "type": "number", "description": "Corner radius in mm (0 = sharp)", "default": 0 }
                },
                "required": ["board", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_add_board_outline(args, ctx).await }
        ),
        tool!(
            "add_mounting_hole",
            "Add an NPTH mounting hole footprint at the specified position.",
            json!({
                "type": "object",
                "properties": {
                    "board":          { "type": "string" },
                    "x":              { "type": "number", "description": "X position in mm" },
                    "y":              { "type": "number", "description": "Y position in mm" },
                    "drill_diameter": { "type": "number", "description": "Drill diameter in mm", "default": 3.2 },
                    "reference":      { "type": "string", "description": "Designator for the hole (e.g. 'H1')", "default": "H1" }
                },
                "required": ["board", "x", "y"]
            }),
            |args, ctx| async move { handle_add_mounting_hole(args, ctx).await }
        ),
        tool!(
            "add_board_text",
            "Add a silkscreen or fabrication text string to the board.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "text":      { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" },
                    "layer":     { "type": "string", "description": "Layer name", "default": "F.SilkS" },
                    "size":      { "type": "number", "description": "Font size in mm", "default": 1.0 },
                    "rotation":  { "type": "number", "description": "Rotation in degrees", "default": 0 }
                },
                "required": ["board", "text", "x", "y"]
            }),
            |args, ctx| async move { handle_add_board_text(args, ctx).await }
        ),
        tool!(
            "add_zone",
            "Add a copper fill zone polygon on a specified layer and net.",
            json!({
                "type": "object",
                "properties": {
                    "board":      { "type": "string" },
                    "net_name":   { "type": "string", "description": "Net name (e.g. 'GND')" },
                    "layer":      { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "points": {
                        "type": "array",
                        "description": "Polygon vertices as [{x, y}]",
                        "items": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } } }
                    },
                    "clearance":  { "type": "number", "default": 0.2 },
                    "min_width":  { "type": "number", "default": 0.2 }
                },
                "required": ["board", "net_name", "layer", "points"]
            }),
            |args, ctx| async move { handle_add_zone(args, ctx).await }
        ),
        tool!(
            "import_svg_logo",
            "Import an SVG file as filled silkscreen or copper artwork (a logo, icon, or other \
             graphic). Curved paths are flattened into polygon outlines since KiCAD's board \
             format doesn't support Bezier curves in filled shapes. Tries KiCAD IPC first, \
             falls back to a direct file edit if KiCAD isn't running.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string", "description": "Path to .kicad_pcb file" },
                    "svg":       { "type": "string", "description": "Path to the .svg file to import" },
                    "width_mm":  { "type": "number", "description": "Target width in mm (aspect ratio preserved)" },
                    "x":         { "type": "number", "description": "X position of the artwork's top-left corner in mm", "default": 0 },
                    "y":         { "type": "number", "description": "Y position of the artwork's top-left corner in mm", "default": 0 },
                    "layer":     { "type": "string", "description": "Target layer", "default": "F.SilkS" }
                },
                "required": ["board", "svg", "width_mm"]
            }),
            |args, ctx| async move { handle_import_svg_logo(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_set_board_size(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let width = match require_f64(args, "width") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let height = match require_f64(args, "height") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let ox = args["origin_x"].as_f64().unwrap_or(0.0);
    let oy = args["origin_y"].as_f64().unwrap_or(0.0);

    let x2 = ox + width;
    let y2 = oy + height;
    let w = 0.05_f64;

    // Try IPC first (live board in KiCAD, undo-aware); fall through to file edit.
    // ponytail: 4 segments over a single BoardRectangle keeps one builder path;
    // switch to board_rectangle if a native rect proves less flaky.
    let items = rect_outline_items(ox, oy, x2, y2, w);
    match attempt_ipc_write(
        ctx.config.ipc_address.clone(),
        &board_path,
        "board size",
        move |c| c.create_items(items).map(|_| ()),
    )
    .await?
    {
        BoardWrite::Ipc(()) => {
            return Ok(CallToolResult::json(&json!({
                "width": width, "height": height,
                "x1": ox, "y1": oy, "x2": x2, "y2": y2,
                "source": "ipc"
            })))
        }
        BoardWrite::Refused(err) => return Ok(err),
        BoardWrite::File => {}
    }

    // Append 4 Edge.Cuts lines (top, right, bottom, left)
    let lines = format!(
        "{}{}{}{}",
        format_gr_line(ox, oy, x2, oy, "Edge.Cuts", w),
        format_gr_line(x2, oy, x2, y2, "Edge.Cuts", w),
        format_gr_line(x2, y2, ox, y2, "Edge.Cuts", w),
        format_gr_line(ox, y2, ox, oy, "Edge.Cuts", w),
    );

    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, lines)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "width": width, "height": height,
        "x1": ox, "y1": oy, "x2": x2, "y2": y2,
        "source": "file"
    })))
}

/// The page size, which is only ever read from the file: KiCad's API exposes
/// no page settings, so even the live path answers this one field from disk.
fn paper_from_file(board_path: &std::path::Path) -> String {
    let Ok(content) = std::fs::read_to_string(board_path) else {
        return "A4".to_string();
    };
    let Ok(tree) = parse_sexp(&content) else {
        return "A4".to_string();
    };
    tree.find("paper")
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("A4")
        .to_string()
}

async fn handle_get_board_info(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;

    // The board open in KiCad first. Reading only the file reported the state
    // of the last save — on a board with unsaved edits it disagreed with the
    // IPC-backed writers in this toolset, most visibly as layer_count 0 /
    // net_count 0 on a board KiCad was showing fully populated.
    let ipc_board = board_path.clone();
    if let Ok((title_block, enabled, nets)) = with_ipc(ctx.config.ipc_address.clone(), move |c| {
        let document = c.find_open_board(&ipc_board)?;
        Ok((
            c.get_title_block_in(document.clone())?,
            c.get_enabled_layers_in(document.clone())?,
            c.get_nets_in(document)?.len(),
        ))
    })
    .await?
    {
        // The copper count is KiCad's own field, not a tally of layer names
        // ending in `.Cu` — the two agree on an ordinary stackup, and that is
        // the kind of agreement that stops holding on an unusual one.
        return Ok(CallToolResult::json(&json!({
            "file": board_path.display().to_string(),
            "title": title_block.title,
            "date": title_block.date,
            "revision": title_block.revision,
            "company": title_block.company,
            "paper": paper_from_file(&board_path),
            "layer_count": enabled.layers.len(),
            "copper_layer_count": enabled.copper_layer_count,
            "net_count": nets,
            "source": "ipc"
        })));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let tree = parse_sexp(&content)?;

    let tb = tree.find("title_block");
    let title = tb
        .and_then(|t| t.find("title"))
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let date = tb
        .and_then(|t| t.find("date"))
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let rev = tb
        .and_then(|t| t.find("rev"))
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let company = tb
        .and_then(|t| t.find("company"))
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();

    // A layer is `(0 "F.Cu" signal)`, keyed by its ordinal rather than by a
    // tag, so find_all("") — which matches on the head — never matched one and
    // this was always 0. See konnect_sexp::layers.
    let stack = konnect_sexp::layers::layers(&tree);
    let layer_count = stack.len();
    let copper_layer_count = konnect_sexp::layers::copper(&stack).len();
    let paper = tree
        .find("paper")
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("A4")
        .to_string();

    // Not find_all("net"): that counts only direct children of (kicad_pcb …),
    // i.e. the top-level net table — which KiCad 10 does not write at all, so
    // every KiCad 10 board reported 0. Collect from wherever the nets actually
    // are and de-duplicate; see konnect_sexp::net.
    let net_count = konnect_sexp::net::count_distinct_nets(&tree);

    Ok(CallToolResult::json(&json!({
        "file": board_path.display().to_string(),
        "title": title, "date": date, "revision": rev, "company": company,
        "paper": paper,
        "layer_count": layer_count,
        "copper_layer_count": copper_layer_count,
        "net_count": net_count,
        "source": "file"
    })))
}

async fn handle_get_board_extents(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;

    // Try IPC first; fall through to file-based computation on error.
    // Addressed to the requested board, not the first open one — with two
    // boards open, first-document targeting silently measures the other, and
    // ensure_board_is_active only checks it is open somewhere.
    let ipc_board = board_path.clone();
    if let Ok(ext) = with_ipc(ctx.config.ipc_address.clone(), move |c| {
        c.get_board_extents_in(c.find_open_board(&ipc_board)?)
    })
    .await?
    {
        return Ok(CallToolResult::json(&json!({
            "x_min": ext.min.x, "y_min": ext.min.y,
            "x_max": ext.max.x, "y_max": ext.max.y,
            "width": ext.max.x - ext.min.x,
            "height": ext.max.y - ext.min.y,
            "source": "ipc"
        })));
    }

    // File-based fallback: collect all coordinates from gr_lines and footprint positions
    let content = std::fs::read_to_string(&board_path)?;
    let tree = parse_sexp(&content)?;

    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
    let mut update = |x: f64, y: f64| {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    };

    for line in tree.find_all("gr_line") {
        if let (Some(s), Some(e)) = (line.find("start"), line.find("end")) {
            if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                (s.get_f64(1), s.get_f64(2), e.get_f64(1), e.get_f64(2))
            {
                update(x1, y1);
                update(x2, y2);
            }
        }
    }
    for fp in tree.find_all("footprint") {
        if let Some(at) = fp.find("at") {
            if let (Some(x), Some(y)) = (at.get_f64(1), at.get_f64(2)) {
                update(x, y);
            }
        }
    }

    if min_x == f64::MAX {
        return Ok(CallToolResult::json(
            &json!({ "x_min": 0, "y_min": 0, "x_max": 0, "y_max": 0, "width": 0, "height": 0, "source": "empty" }),
        ));
    }

    Ok(CallToolResult::json(&json!({
        "x_min": min_x, "y_min": min_y,
        "x_max": max_x, "y_max": max_y,
        "width": max_x - min_x,
        "height": max_y - min_y,
        "source": "file"
    })))
}

async fn handle_get_layer_list(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let content = std::fs::read_to_string(&board_path)?;
    let tree = parse_sexp(&content)?;

    if tree.find("layers").is_none() {
        return Ok(CallToolResult::error(
            "No (layers) section found in board file",
        ));
    }

    // Each child of layers looks like: (0 "F.Cu" signal). The ordinal is the
    // head of the list, so the fields sit one place earlier than the accessors
    // used to assume — and find_all("") never returned any of them anyway.
    let layers: Vec<serde_json::Value> = konnect_sexp::layers::layers(&tree)
        .into_iter()
        .map(|l| {
            json!({
                "id": l.id,
                "name": l.name,
                "type": l.kind,
                "user_name": l.user_name,
                "copper": l.is_copper(),
            })
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "count": layers.len(), "layers": layers }),
    ))
}

async fn handle_add_layer(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let layer_name = match require_str(args, "layer_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer_type = args["layer_type"].as_str().unwrap_or("signal");

    // Fail closed on a name KiCad does not define. The layer set is closed, and
    // a board carrying an unknown name does not open at all — so writing one
    // returns success and hands back a file the user cannot load. Verified
    // against KiCAD 10: `(53 "User.8" user)` loads, `(53 "TestLayer" user)` is
    // refused with "Failed to load board".
    if !konnect_sexp::layers::is_canonical_name(&layer_name) {
        return Ok(CallToolResult::error(format!(
            "'{layer_name}' is not a KiCAD layer name, and a board containing one \
             cannot be opened. Names are fixed: F.Cu, B.Cu, In1.Cu..In30.Cu, \
             User.1..User.45, and the technical layers (Edge.Cuts, F.Mask, …). \
             To give a layer your own label, add the canonical layer and set its \
             user name — `(53 \"User.8\" user \"{layer_name}\")`."
        )));
    }

    let content = std::fs::read_to_string(&board_path)?;

    // Find the (layers ...) block and insert before its closing paren
    let layers_pos = match content.find("(layers") {
        Some(p) => p,
        None => return Ok(CallToolResult::error("No (layers) section found")),
    };

    // Determine the next available inner copper ID (first unused ID in 1-30 range).
    // The ids have to be read by shape — see konnect_sexp::layers. Reading them
    // with find_all("") returned nothing, so every call allocated id 1 and
    // duplicated In1.Cu on any board that already had an inner layer.
    let tree = parse_sexp(&content)?;
    let used_ids: std::collections::HashSet<i32> = konnect_sexp::layers::layers(&tree)
        .iter()
        .map(|l| l.id)
        .collect();
    let new_id = match (1..=30).find(|id| !used_ids.contains(id)) {
        Some(id) => id,
        None => {
            return Ok(CallToolResult::error(
                "No free inner copper layer id: 1-30 are all in use",
            ))
        }
    };

    // Close of the layers block, by paren balance. The previous probe looked for
    // a literal "\n  )", which a tab-indented KiCad 10 file never contains; the
    // fallback then found the first ')' in the block — the close of the *first
    // layer entry* — and the new layer was written inside it.
    let close = match close_of_block(&content, layers_pos) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(
                "Unbalanced (layers) block; refusing to write",
            ))
        }
    };
    // Insert after the last entry rather than immediately before the close, so
    // the newline and indent that already sit in front of `)` stay in front of
    // it and the block keeps KiCad's own layout.
    let insert_pos = content[..close].trim_end().len();

    // Match whatever the file already indents entries with, rather than
    // hardcoding spaces into a file that may be tab-indented.
    let indent = entry_indent(&content, layers_pos).unwrap_or_else(|| "    ".to_string());
    let new_layer = format!("\n{indent}({new_id} \"{layer_name}\" {layer_type})");
    let new_content = apply_edits(content, vec![SexpEdit::insert(insert_pos, new_layer)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added_layer": layer_name, "id": new_id, "type": layer_type
    })))
}

async fn handle_set_active_layer(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let new_content = if let Some(pos) = content.find("(active_layer ") {
        let after = pos + "(active_layer ".len();
        let close = content[after..].find(')').unwrap_or(0);
        let layer_end = after + close;
        apply_edits(
            content,
            vec![SexpEdit::replace(after, layer_end, format!("\"{layer}\""))],
        )
    } else {
        // Insert into setup block
        let setup_close = content
            .find("(setup")
            .and_then(|p| content[p..].find('\n').map(|off| p + off))
            .unwrap_or(content.rfind(')').unwrap_or(content.len()));
        apply_edits(
            content,
            vec![SexpEdit::insert(
                setup_close,
                format!("\n    (active_layer \"{layer}\")"),
            )],
        )
    };
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({ "active_layer": layer })))
}

async fn handle_add_board_outline(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
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
    let w = 0.05_f64;

    let items = rect_outline_items(x1, y1, x2, y2, w);
    match attempt_ipc_write(
        ctx.config.ipc_address.clone(),
        &board_path,
        "board outline",
        move |c| c.create_items(items).map(|_| ()),
    )
    .await?
    {
        BoardWrite::Ipc(()) => {
            return Ok(CallToolResult::json(&json!({
                "x1": x1, "y1": y1, "x2": x2, "y2": y2,
                "width": (x2-x1).abs(), "height": (y2-y1).abs(),
                "source": "ipc"
            })))
        }
        BoardWrite::Refused(err) => return Ok(err),
        BoardWrite::File => {}
    }

    let lines = format!(
        "{}{}{}{}",
        format_gr_line(x1, y1, x2, y1, "Edge.Cuts", w),
        format_gr_line(x2, y1, x2, y2, "Edge.Cuts", w),
        format_gr_line(x2, y2, x1, y2, "Edge.Cuts", w),
        format_gr_line(x1, y2, x1, y1, "Edge.Cuts", w),
    );

    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, lines)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "x1": x1, "y1": y1, "x2": x2, "y2": y2,
        "width": (x2-x1).abs(), "height": (y2-y1).abs(),
        "source": "file"
    })))
}

async fn handle_add_mounting_hole(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let drill_d = args["drill_diameter"].as_f64().unwrap_or(3.2);
    let reference = args["reference"].as_str().unwrap_or("H1").to_string();

    // A mounting hole is a footprint, so the same rule as every other
    // board-mutating tool applies (see `attempt_ipc_write`): the request must
    // name the board KiCAD has open, and only an IPC transport that was never
    // reached may fall back to editing the file.
    let requested_board = board_path.clone();
    let lib_id = mounting_hole_lib_id(drill_d);
    let lib_id_ipc = lib_id.clone();
    let reference_ipc = reference.clone();
    let text_offset = mounting_hole_text_offset(drill_d);
    let attempt = attempt_ipc_write(
        ctx.config.ipc_address.clone(),
        &board_path,
        "mounting hole",
        move |c| {
            c.place_footprint(
                &requested_board,
                &lib_id_ipc,
                &reference_ipc,
                "MountingHole",
                std::slice::from_ref(&mounting_hole_pad(drill_d)),
                &[],
                &konnect_ipc::IpcFieldPlacement {
                    reference_at: Some((0.0, text_offset, 0.0)),
                    value_at: Some((0.0, -text_offset, 0.0)),
                },
                x,
                y,
                0.0,
                "F.Cu",
            )
        },
    )
    .await?;

    match attempt {
        BoardWrite::Ipc(fp) => Ok(CallToolResult::json(&json!({
            "reference": fp.reference, "x": fp.position.x, "y": fp.position.y,
            "drill_diameter": drill_d, "footprint": fp.footprint,
            "source": "ipc"
        }))),
        BoardWrite::Refused(err) => Ok(err),
        BoardWrite::File => {
            // No live KiCad on the other end of this transport: editing the
            // board file directly cannot race an editor.
            let fp_sexp = format_npth_footprint(x, y, drill_d, &reference);
            let content = std::fs::read_to_string(&board_path)?;
            let close_pos = content.rfind(')').unwrap_or(content.len());
            let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, fp_sexp)]);
            write_atomic(&board_path, &new_content)?;

            Ok(CallToolResult::json(&json!({
                "reference": reference, "x": x, "y": y, "drill_diameter": drill_d,
                "footprint": lib_id,
                "source": "file",
                "warning": "KiCAD IPC was not reachable, so the board file was edited \
                            directly. KiCAD will show this mounting hole when it next \
                            loads the board."
            })))
        }
    }
}

async fn handle_add_board_text(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
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
    let layer = args["layer"].as_str().unwrap_or("F.SilkS").to_string();
    let size = args["size"].as_f64().unwrap_or(1.0);
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);

    let text_ipc = text.clone();
    let layer_ipc = layer.clone();
    match attempt_ipc_write(
        ctx.config.ipc_address.clone(),
        &board_path,
        "board text",
        move |c| {
            let bt = builders::board_text(&layer_ipc, &text_ipc, x, y, size, rotation, false);
            let any = builders::pack_any(&bt, "kiapi.board.types.BoardText");
            c.create_items(vec![any]).map(|_| ())
        },
    )
    .await?
    {
        BoardWrite::Ipc(()) => {
            return Ok(CallToolResult::json(&json!({
                "text": text, "x": x, "y": y, "layer": layer, "size": size,
                "source": "ipc"
            })))
        }
        BoardWrite::Refused(err) => return Ok(err),
        BoardWrite::File => {}
    }

    let gr_text = format_gr_text(&text, x, y, rotation, &layer, size);
    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, gr_text)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "text": text, "x": x, "y": y, "layer": layer, "size": size,
        "source": "file"
    })))
}

async fn handle_add_zone(
    args: &serde_json::Value,
    _ctx: &ToolContext,
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
    let min_width = args["min_width"].as_f64().unwrap_or(0.2);
    let pts_arr = match args["points"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'points' array")),
    };

    let points: Vec<(f64, f64)> = pts_arr
        .iter()
        .filter_map(|p| Some((p["x"].as_f64()?, p["y"].as_f64()?)))
        .collect();

    if points.len() < 3 {
        return Ok(CallToolResult::error("Zone requires at least 3 points"));
    }

    if let Some(refusal) =
        refuse_if_board_open_in_kicad(_ctx.config.ipc_address.clone(), &board_path, "zone").await?
    {
        return Ok(refusal);
    }

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parse_sexp(&content)?;
    let Some(net) = konnect_sexp::net::net_ref_for_write(&tree, &net_name) else {
        return Ok(CallToolResult::error(format!(
            "Net '{net_name}' is not declared in {}'s net table. On this legacy-format board \
             a zone must reference a declared net id — writing it anyway would attach the \
             copper to net 0, the unconnected pseudo-net (#192). Declare it first with \
             add_net, or check the name with get_nets_list.",
            board_path.display()
        )));
    };
    let zone_sexp = format_zone_polygon(&net, &layer, clearance, min_width, &points);

    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, zone_sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net_name, "layer": layer,
        "point_count": points.len()
    })))
}

async fn handle_import_svg_logo(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let svg_path = get_path(args, "svg")?;
    let width_mm = match require_f64(args, "width_mm") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x = args["x"].as_f64().unwrap_or(0.0);
    let y = args["y"].as_f64().unwrap_or(0.0);
    let layer = args["layer"].as_str().unwrap_or("F.SilkS").to_string();

    let svg_content = std::fs::read_to_string(&svg_path)?;
    let logo = crate::tools::svg_import::extract_polygons(&svg_content)?;
    if logo.polygons.is_empty() {
        return Ok(CallToolResult::error(
            "No fillable paths found in the SVG (only <path> elements are supported).",
        ));
    }

    let placed =
        crate::tools::svg_import::scale_and_place(&logo.polygons, logo.width, width_mm, x, y);

    let layer_ipc = layer.clone();
    let placed_ipc = placed.clone();
    let ipc_attempt = attempt_ipc_write(
        ctx.config.ipc_address.clone(),
        &board_path,
        "SVG logo",
        move |c| {
            let shape = builders::board_polygon(&layer_ipc, 0.0, true, &placed_ipc);
            let any = builders::pack_any(&shape, "kiapi.board.types.BoardGraphicShape");
            c.create_items(vec![any]).map(|_| ())
        },
    )
    .await?;
    if let BoardWrite::Refused(err) = ipc_attempt {
        return Ok(err);
    }
    if matches!(ipc_attempt, BoardWrite::Ipc(())) {
        return Ok(CallToolResult::json(&json!({
            "polygon_count": placed.len(),
            "layer": layer,
            "width_mm": width_mm,
            "source": "ipc"
        })));
    }

    let mut sexp = String::new();
    for polygon in &placed {
        sexp.push_str(&format_gr_poly(polygon, &layer));
    }
    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "polygon_count": placed.len(),
        "layer": layer,
        "width_mm": width_mm,
        "source": "file"
    })))
}

#[cfg(test)]
mod layers_block_tests {
    use super::*;

    // Both indent styles, same content: KiCad 9 writes two spaces, 10 writes tabs.
    const SPACES: &str =
        "(kicad_pcb\n  (layers\n    (0 \"F.Cu\" signal)\n    (2 \"B.Cu\" signal)\n  )\n)";
    const TABS: &str =
        "(kicad_pcb\n\t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(2 \"B.Cu\" signal)\n\t)\n)";

    fn layers_close(content: &str) -> usize {
        close_of_block(content, content.find("(layers").unwrap()).unwrap()
    }

    #[test]
    fn close_of_block_finds_the_same_close_under_either_indent() {
        for content in [SPACES, TABS] {
            let close = layers_close(content);
            // Everything up to the close balances, and the block ends after the
            // last entry rather than inside the first one.
            assert_eq!(&content[close..close + 1], ")");
            assert!(content[..close].contains("B.Cu"));
        }
    }

    #[test]
    fn close_of_block_is_not_the_first_paren_in_the_block() {
        // The old probe fell back to the first ')' — the close of entry one —
        // and wrote the new layer inside it.
        let content = TABS;
        let start = content.find("(layers").unwrap();
        let first = start + content[start..].find(')').unwrap();
        assert_ne!(layers_close(content), first);
    }

    #[test]
    fn close_of_block_ignores_parens_inside_strings() {
        let content = "(kicad_pcb\n\t(layers\n\t\t(0 \"F.Cu)(\" signal)\n\t)\n)";
        let close = layers_close(content);
        assert!(content[..close].contains("F.Cu)("));
    }

    #[test]
    fn close_of_block_refuses_an_unbalanced_block() {
        assert_eq!(close_of_block("(layers\n\t(0 \"F.Cu\" signal)", 0), None);
    }

    #[test]
    fn entry_indent_matches_the_file() {
        assert_eq!(
            entry_indent(SPACES, SPACES.find("(layers").unwrap()).as_deref(),
            Some("    ")
        );
        assert_eq!(
            entry_indent(TABS, TABS.find("(layers").unwrap()).as_deref(),
            Some("\t\t")
        );
    }

    #[test]
    fn entry_indent_declines_an_empty_block_rather_than_guessing() {
        let empty = "(kicad_pcb\n\t(layers\n\t)\n)";
        assert_eq!(entry_indent(empty, empty.find("(layers").unwrap()), None);
    }

    #[test]
    fn layers_canonical_names_match_kicads_own_enum() {
        // Guards konnect_sexp::layers::is_canonical_name against drift: the
        // authority is KiCAD's BoardLayer enum, shipped in the API protos.
        // Variant name -> file name is `BL_` off, remaining `_` to `.`.
        use konnect_ipc::gen::kiapi::board::types::BoardLayer;
        let sentinels = ["BL_UNKNOWN", "BL_UNDEFINED", "BL_UNSELECTED"];
        let mut checked = 0;
        for i in 0..=200i32 {
            let Ok(layer) = BoardLayer::try_from(i) else {
                continue;
            };
            let variant = layer.as_str_name();
            if sentinels.contains(&variant) {
                continue;
            }
            let name = variant.trim_start_matches("BL_").replacen('_', ".", 1);
            assert!(
                konnect_sexp::layers::is_canonical_name(&name),
                "{variant} maps to '{name}', which is_canonical_name rejects"
            );
            checked += 1;
        }
        // Cheap guard against the loop silently matching nothing.
        assert!(checked > 90, "only {checked} layers checked");
    }

    #[test]
    fn ids_in_use_are_seen_so_a_new_layer_does_not_collide() {
        // The regression this PR is about: with the ids unreadable, the free-id
        // search always returned 1 and duplicated an existing In1.Cu.
        let four_layer = "(kicad_pcb\n\t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(1 \"In1.Cu\" signal)\n\t\t(2 \"B.Cu\" signal)\n\t)\n)";
        let tree = parse_sexp(four_layer).unwrap();
        let used: std::collections::HashSet<i32> = konnect_sexp::layers::layers(&tree)
            .iter()
            .map(|l| l.id)
            .collect();
        assert!(used.contains(&1));
        assert_eq!((1..=30).find(|id| !used.contains(id)), Some(3));
    }
}

#[cfg(test)]
mod svg_logo_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            // Deliberately empty ipc_address: with_ipc fails fast against it,
            // exercising the file-fallback path without needing live KiCAD.
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

    fn blank_board() -> &'static str {
        "(kicad_pcb\n  (version 20250610)\n  (generator \"konnect\")\n  (paper \"A4\")\n  (net 0 \"\")\n)\n"
    }

    fn rect_svg() -> &'static str {
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <path d="M0 0 L100 0 L100 100 L0 100 Z" fill="black"/>
        </svg>"##
    }

    #[test]
    fn format_gr_poly_contains_layer_fill_and_points() {
        let sexp = format_gr_poly(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)], "F.SilkS");
        assert!(sexp.contains("(gr_poly"));
        assert!(sexp.contains("(fill solid)"));
        assert!(sexp.contains("(layer \"F.SilkS\")"));
        assert!(sexp.contains("(xy 1 0)") || sexp.contains("(xy 1.0 0)"));
    }

    #[tokio::test]
    async fn import_svg_logo_file_fallback_places_polygon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board_path = dir.path().join("board.kicad_pcb");
        let svg_path = dir.path().join("logo.svg");
        std::fs::write(&board_path, blank_board()).unwrap();
        std::fs::write(&svg_path, rect_svg()).unwrap();

        let ctx = test_ctx();
        let args = json!({
            "board": board_path.to_str().unwrap(),
            "svg": svg_path.to_str().unwrap(),
            "width_mm": 10.0
        });

        let result = handle_import_svg_logo(&args, &ctx)
            .await
            .expect("handler should succeed");
        assert!(!result.is_error);

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["polygon_count"], json!(1));
        assert_eq!(parsed["source"], json!("file"));
        assert_eq!(parsed["layer"], json!("F.SilkS"));

        let updated = std::fs::read_to_string(&board_path).unwrap();
        assert!(updated.contains("(gr_poly"));
    }

    #[tokio::test]
    async fn import_svg_logo_rejects_svg_with_no_fillable_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board_path = dir.path().join("board.kicad_pcb");
        let svg_path = dir.path().join("empty.svg");
        std::fs::write(&board_path, blank_board()).unwrap();
        std::fs::write(
            &svg_path,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"##,
        )
        .unwrap();

        let ctx = test_ctx();
        let args = json!({
            "board": board_path.to_str().unwrap(),
            "svg": svg_path.to_str().unwrap(),
            "width_mm": 10.0
        });

        let result = handle_import_svg_logo(&args, &ctx).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn import_svg_logo_missing_width_mm_returns_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board_path = dir.path().join("board.kicad_pcb");
        let svg_path = dir.path().join("logo.svg");
        std::fs::write(&board_path, blank_board()).unwrap();
        std::fs::write(&svg_path, rect_svg()).unwrap();

        let ctx = test_ctx();
        let args = json!({
            "board": board_path.to_str().unwrap(),
            "svg": svg_path.to_str().unwrap()
        });

        let result = handle_import_svg_logo(&args, &ctx).await.unwrap();
        assert!(result.is_error);
    }
}

#[cfg(test)]
mod net_count_tests {
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

    async fn net_count_of(board: &str) -> i64 {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board).unwrap();
        let result =
            handle_get_board_info(&json!({ "board": path.to_str().unwrap() }), &test_ctx())
                .await
                .expect("handler should succeed");
        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        parsed["net_count"].as_i64().expect("net_count")
    }

    /// KiCad 10 has no top-level net table, so the old
    /// `find_all("net").len().saturating_sub(1)` — direct children only —
    /// reported 0 for every board saved by KiCad 10, however many nets it had.
    #[tokio::test]
    async fn a_kicad_10_board_with_no_net_table_still_counts_its_nets() {
        let board = "(kicad_pcb\n\
            \t(version 20260206)\n\
            \t(footprint \"R\"\n\t\t(pad \"1\" smd rect (at 0 0) (net \"GND\"))\n\
            \t\t(pad \"2\" smd rect (at 1 0) (net \"VCC\"))\n\t)\n\
            \t(segment (start 0 0) (end 1 0) (net \"GND\"))\n\
            )\n";
        assert_eq!(net_count_of(board).await, 2);
    }

    /// A KiCad ≤ 9 net is declared once and referenced many times; the old
    /// code happened to be right here only because it looked at the table.
    #[tokio::test]
    async fn a_kicad_9_board_counts_each_net_once() {
        let board = "(kicad_pcb\n\
            \t(version 20241229)\n\
            \t(net 0 \"\")\n\t(net 1 \"GND\")\n\t(net 2 \"VCC\")\n\
            \t(segment (start 0 0) (end 1 0) (net 1))\n\
            \t(via (at 1 0) (net 1))\n\
            )\n";
        assert_eq!(net_count_of(board).await, 2);
    }

    #[tokio::test]
    async fn a_board_with_only_the_unconnected_pseudo_net_counts_zero() {
        assert_eq!(
            net_count_of("(kicad_pcb\n  (version 20250610)\n  (net 0 \"\")\n)\n").await,
            0
        );
    }
}

#[cfg(test)]
mod mounting_hole_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    pub(super) fn ctx_with_ipc(ipc_address: String) -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address,
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    pub(super) fn blank_board(dir: &std::path::Path) -> std::path::PathBuf {
        let board = dir.join("board.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb\n  (version 20250610)\n  (generator \"konnect\")\n  (paper \"A4\")\n  (net 0 \"\")\n)\n",
        )
        .unwrap();
        board
    }

    pub(super) fn result_text(res: &CallToolResult) -> String {
        match res.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// A rep0 endpoint that completes every round-trip with an error status —
    /// a live KiCAD saying no. Mirrors the helper of the same name in
    /// `pcb_components`, which guards `place_component`'s fallback.
    pub(super) fn spawn_rejecting_kicad() -> String {
        use nng::options::Options;
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let url = format!("tcp://127.0.0.1:{port}");
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock rep socket");
        socket
            .set_opt::<nng::options::RecvTimeout>(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        socket.listen(&url).expect("mock listen");
        std::thread::spawn(move || {
            use prost::Message;
            while socket.recv().is_ok() {
                let response = konnect_ipc::gen::kiapi::common::ApiResponse {
                    status: Some(konnect_ipc::gen::kiapi::common::ApiResponseStatus {
                        status: konnect_ipc::gen::kiapi::common::ApiStatusCode::AsBadRequest as i32,
                        error_message: "mock rejects everything".to_string(),
                    }),
                    header: None,
                    message: None,
                };
                let out = nng::Message::from(response.encode_to_vec().as_slice());
                if socket.send(out).is_err() {
                    break;
                }
            }
        });
        url
    }

    #[test]
    fn mounting_hole_pad_is_an_unplated_hole_with_drill_and_annulus() {
        let pad = mounting_hole_pad(3.45);
        assert_eq!(pad.pad_type, "np_thru_hole");
        assert_eq!(pad.shape, "circle");
        assert_eq!(pad.drill_x, Some(3.45));
        assert_eq!(pad.drill_y, Some(3.45));
        assert!(!pad.drill_oval);
        // Annulus matches the (size …) the file path writes, so a hole placed
        // over IPC and one written to the file are the same hole.
        assert_eq!(pad.size_x, 3.95);
        assert_eq!(pad.size_y, 3.95);
        assert_eq!(pad.layers, ["*.Cu", "*.Mask"]);
        assert_eq!(pad.x, 0.0);
        assert_eq!(pad.y, 0.0);
    }

    /// The bug: `add_mounting_hole` only ever edited the board file. Against a
    /// KiCAD holding the board open, the hole never appeared in the session and
    /// the next save discarded it — three calls, a success JSON each time, zero
    /// footprints on the board. A reachable KiCAD must now fail closed.
    #[tokio::test]
    async fn a_reachable_kicad_that_rejects_never_touches_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = blank_board(dir.path());
        let before = std::fs::read_to_string(&board).unwrap();

        let ctx = ctx_with_ipc(spawn_rejecting_kicad());
        let args = json!({
            "board": board.to_str().unwrap(),
            "x": 5.0, "y": 6.0, "drill_diameter": 3.45, "reference": "H1"
        });
        let res = handle_add_mounting_hole(&args, &ctx).await.unwrap();

        assert!(res.is_error, "a rejection must not be reported as success");
        let text = result_text(&res);
        assert!(
            text.contains("rejected the mounting hole") && text.contains("not modified"),
            "the error must say the file was left alone: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(&board).unwrap(),
            before,
            "a reachable KiCAD that says no must never trigger the file fallback"
        );
    }

    #[tokio::test]
    async fn an_unreachable_kicad_still_falls_back_to_the_board_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = blank_board(dir.path());

        // Empty ipc_address is classified TransportUnreachable, so no live
        // KiCAD can be holding this board.
        let ctx = ctx_with_ipc(String::new());
        let args = json!({
            "board": board.to_str().unwrap(),
            "x": 5.0, "y": 6.0, "drill_diameter": 3.45, "reference": "H1"
        });
        let res = handle_add_mounting_hole(&args, &ctx).await.unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);

        let parsed: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();
        assert_eq!(parsed["source"], json!("file"));
        assert_eq!(parsed["reference"], json!("H1"));

        let updated = std::fs::read_to_string(&board).unwrap();
        assert!(
            updated.contains("(pad \"\" np_thru_hole circle"),
            "{updated}"
        );
        assert!(updated.contains("(drill 3.45)"), "{updated}");
        assert!(updated.contains("\"H1\""), "{updated}");
    }
}

/// The board-graphics tools (`set_board_size`, `add_board_outline`,
/// `add_board_text`, `import_svg_logo`) went to IPC on `with_ipc(..).is_ok()`,
/// which conflated "no KiCAD there" with "KiCAD said no" and ignored the
/// `board` argument entirely. Both halves are covered here: a reachable KiCAD
/// that refuses must leave the file alone, and an unreachable one must still
/// produce the file edit.
#[cfg(test)]
mod board_write_gate_tests {
    use super::mounting_hole_tests::{
        blank_board, ctx_with_ipc, result_text, spawn_rejecting_kicad,
    };
    use super::*;

    fn board_args(board: &std::path::Path) -> serde_json::Value {
        json!({
            "board": board.to_str().unwrap(),
            "x1": 10.0, "y1": 10.0, "x2": 30.0, "y2": 25.0
        })
    }

    #[tokio::test]
    async fn outline_on_a_rejecting_kicad_never_touches_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = blank_board(dir.path());
        let before = std::fs::read_to_string(&board).unwrap();

        let ctx = ctx_with_ipc(spawn_rejecting_kicad());
        let res = handle_add_board_outline(&board_args(&board), &ctx)
            .await
            .unwrap();

        assert!(res.is_error, "a rejection must not be reported as success");
        assert!(
            result_text(&res).contains("board file was not modified"),
            "{}",
            result_text(&res)
        );
        assert_eq!(
            std::fs::read_to_string(&board).unwrap(),
            before,
            "a reachable KiCAD refused, so the file must be untouched"
        );
    }

    #[tokio::test]
    async fn outline_on_an_unreachable_kicad_edits_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = blank_board(dir.path());

        // Empty ipc_address classifies as TransportUnreachable: no live KiCAD
        // can be holding this board, so the file edit is safe.
        let ctx = ctx_with_ipc(String::new());
        let res = handle_add_board_outline(&board_args(&board), &ctx)
            .await
            .unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);

        let parsed: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();
        assert_eq!(parsed["source"], json!("file"));

        let updated = std::fs::read_to_string(&board).unwrap();
        assert_eq!(
            updated.matches("Edge.Cuts").count(),
            4,
            "expected four outline segments: {updated}"
        );
    }

    #[tokio::test]
    async fn board_text_on_a_rejecting_kicad_never_touches_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = blank_board(dir.path());
        let before = std::fs::read_to_string(&board).unwrap();

        let ctx = ctx_with_ipc(spawn_rejecting_kicad());
        let args = json!({
            "board": board.to_str().unwrap(),
            "text": "REV A", "x": 5.0, "y": 5.0
        });
        let res = handle_add_board_text(&args, &ctx).await.unwrap();

        assert!(res.is_error, "a rejection must not be reported as success");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn set_board_size_on_a_rejecting_kicad_never_touches_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = blank_board(dir.path());
        let before = std::fs::read_to_string(&board).unwrap();

        let ctx = ctx_with_ipc(spawn_rejecting_kicad());
        let args = json!({
            "board": board.to_str().unwrap(),
            "width": 20.0, "height": 15.0
        });
        let res = handle_set_board_size(&args, &ctx).await.unwrap();

        assert!(res.is_error, "a rejection must not be reported as success");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), before);
    }
}

/// `add_zone`'s twin of `pcb_routing::zone_net_format_tests` — same #192
/// defect, second copy of the broken lookup.
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

    const KICAD_10: &str = "(kicad_pcb\n\t(version 20260206)\n\t(generator \"pcbnew\")\n\t(segment\n\t\t(start 10 10)\n\t\t(end 20 10)\n\t\t(width 0.2)\n\t\t(layer \"F.Cu\")\n\t\t(net \"GND\")\n\t)\n)\n";
    const LEGACY: &str = "(kicad_pcb\n  (version 20240108)\n  (generator \"pcbnew\")\n  (net 0 \"\")\n  (net 7 \"GND\")\n)\n";

    async fn zone(board: &str, net: &str) -> (CallToolResult, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board).unwrap();
        let result = handle_add_zone(
            &json!({
                "board": path.to_str().unwrap(), "net_name": net, "layer": "B.Cu",
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
        let (result, after) = zone(KICAD_10, "GND").await;
        assert!(!result.is_error, "{}", text_of(&result));
        let zone_at = after.find("(zone").expect("zone written");
        let z = &after[zone_at..];
        assert!(z.contains("(net \"GND\")"), "{z}");
        assert!(!z.contains("(net 0)"), "{z}");
        assert!(!z.contains("net_name"), "{z}");
        assert!(z.contains("(layers \"B.Cu\")"), "{z}");
    }

    #[tokio::test]
    async fn a_legacy_zone_keeps_the_declared_id_and_net_name_pair() {
        let (result, after) = zone(LEGACY, "GND").await;
        assert!(!result.is_error, "{}", text_of(&result));
        let z = &after[after.find("(zone").unwrap()..];
        assert!(z.contains("(net 7) (net_name \"GND\")"), "{z}");
        assert!(z.contains("(layer \"B.Cu\")"), "{z}");
    }

    #[tokio::test]
    async fn an_undeclared_net_on_a_legacy_board_is_refused_not_zeroed() {
        let (result, after) = zone(LEGACY, "PWR").await;
        assert!(result.is_error, "{}", text_of(&result));
        assert_eq!(after, LEGACY);
    }
}

/// `get_board_info` used to read only the file — the last save — while every
/// writer in this toolset acts on the board KiCad holds. On a board with
/// unsaved edits the two disagreed completely, most visibly as layer_count 0
/// and net_count 0 for a board KiCad was showing fully populated.
#[cfg(test)]
mod board_info_source_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use konnect_ipc::gen::kiapi;
    use prost::Message;
    use std::sync::Arc;

    /// A board saved before anything was placed on it: the empty stub the
    /// file-only reader kept reporting.
    const EMPTY_STUB: &str = "(kicad_pcb\n\t(version 20260206)\n\t(paper \"A3\")\n)\n";

    fn ctx_talking_to(address: String) -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: address,
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// A rep0 endpoint playing a KiCad holding `board` open with `layers`
    /// enabled, `copper` of them copper, and `nets` named — none of it saved
    /// to the file.
    fn spawn_kicad_holding(
        board: &std::path::Path,
        layers: usize,
        copper: u32,
        nets: usize,
    ) -> String {
        use nng::options::Options;

        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let url = format!("tcp://127.0.0.1:{port}");
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock rep socket");
        socket
            .set_opt::<nng::options::RecvTimeout>(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        socket.listen(&url).expect("mock listen");

        let board = board.to_string_lossy().to_string();
        std::thread::spawn(move || {
            while let Ok(message) = socket.recv() {
                let request = kiapi::common::ApiRequest::decode(message.as_slice()).unwrap();
                let command = request.message.expect("a command");
                let body = if command.type_url.ends_with("GetOpenDocuments") {
                    Some(konnect_ipc::builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![kiapi::common::types::DocumentSpecifier {
                                r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                                project: None,
                                identifier: Some(
                                    kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                        board.clone(),
                                    ),
                                ),
                            }],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    ))
                } else if command.type_url.ends_with("GetTitleBlockInfo") {
                    Some(konnect_ipc::builders::pack_any(
                        &kiapi::common::types::TitleBlockInfo {
                            title: "Live title".to_string(),
                            revision: "B".to_string(),
                            ..Default::default()
                        },
                        "kiapi.common.types.TitleBlockInfo",
                    ))
                } else if command.type_url.ends_with("GetBoardEnabledLayers") {
                    Some(konnect_ipc::builders::pack_any(
                        &kiapi::board::commands::BoardEnabledLayersResponse {
                            copper_layer_count: copper,
                            layers: (0..layers as i32).collect(),
                        },
                        "kiapi.board.commands.BoardEnabledLayersResponse",
                    ))
                } else if command.type_url.ends_with("GetNets") {
                    Some(konnect_ipc::builders::pack_any(
                        &kiapi::board::commands::NetsResponse {
                            nets: (0..nets)
                                .map(|index| kiapi::board::types::Net {
                                    code: None,
                                    name: format!("N{index}"),
                                })
                                .collect(),
                        },
                        "kiapi.board.commands.NetsResponse",
                    ))
                } else {
                    None
                };
                let response = kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    header: None,
                    message: body,
                };
                let out = nng::Message::from(response.encode_to_vec().as_slice());
                if socket.send(out).is_err() {
                    break;
                }
            }
        });
        url
    }

    async fn board_info(board: &std::path::Path, ctx: &ToolContext) -> serde_json::Value {
        let result = handle_get_board_info(&json!({ "board": board.to_str().unwrap() }), ctx)
            .await
            .expect("handler should succeed");
        assert!(!result.is_error, "{:?}", result.content);
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_live_board_is_reported_instead_of_the_last_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, EMPTY_STUB).unwrap();
        // Six copper layers among 27 enabled. Ids 3..26 are all `*.Cu`, so a
        // tally of layer names would say 24 — the response field says 6.
        let address = spawn_kicad_holding(&board, 27, 6, 99);

        let info = board_info(&board, &ctx_talking_to(address)).await;

        assert_eq!(info["source"], json!("ipc"));
        assert_eq!(info["layer_count"], json!(27));
        assert_eq!(info["copper_layer_count"], json!(6));
        assert_eq!(info["net_count"], json!(99));
        assert_eq!(info["title"], json!("Live title"));
        assert_eq!(info["revision"], json!("B"));
        // Page size has no IPC equivalent, so it stays a file reading.
        assert_eq!(info["paper"], json!("A3"));
    }

    #[tokio::test]
    async fn an_offline_session_still_reads_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, EMPTY_STUB).unwrap();

        let info = board_info(&board, &ctx_talking_to(String::new())).await;

        assert_eq!(info["source"], json!("file"));
        assert_eq!(info["net_count"], json!(0));
        assert_eq!(info["paper"], json!("A3"));
    }
}
