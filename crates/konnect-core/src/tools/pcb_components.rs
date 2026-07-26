//! `pcb_components` toolset — place, move, rotate, query, and array footprints on the PCB.
//!
//! Most operations use the KiCAD IPC API so they integrate with KiCAD's undo/redo
//! system and don't require a separate file-sync step. `get_board_2d_view` uses
//! kicad-cli to render a PNG.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::library::{footprint_lib_nickname_for_dir, is_lib_id, resolve_footprint_path};
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::writer::{
    apply_edits, find_balanced_block, find_block_starts, new_uuid, write_atomic,
};
use konnect_sexp::SexpEdit;
use serde_json::json;

// ─── Library footprint → board footprint ──────────────────────────────────────

/// Build a board-ready `(footprint …)` block for `lib_id`.
///
/// A library `.kicad_mod` is a complete footprint definition sitting at the
/// origin with a `REF**` placeholder reference. Placing it on a board means
/// renaming it to the full `Library:Footprint` id, stamping in a position,
/// rotation and fresh UUID, and substituting the real reference designator.
///
/// KiCAD's own parser then handles the pads and graphics, which is why the
/// whole definition is forwarded rather than reconstructed.
fn board_footprint_sexp(
    lib_id: &str,
    x: f64,
    y: f64,
    rotation: f64,
    layer: &str,
    reference: Option<&str>,
) -> Result<String, String> {
    let path = resolve_footprint_path(lib_id)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read footprint {}: {}", path.display(), e))?;

    let name_span = footprint_name_span(&content).ok_or_else(|| {
        format!(
            "{} does not start with a (footprint \"NAME\" …) block",
            path.display()
        )
    })?;

    // Board footprints carry the full library id, not the bare footprint name.
    // The declared name is the span without its surrounding quotes.
    let declared = &content[name_span.start + 1..name_span.end - 1];
    let mut out = String::with_capacity(content.len() + 128);
    out.push_str(&content[..name_span.start]);
    out.push_str(&escape_sexp_string(&board_lib_id(lib_id, &path, declared)));
    out.push_str(&format!(
        "\n\t(at {x} {y} {rotation})\n\t(uuid \"{}\")",
        new_uuid()
    ));
    out.push_str(&content[name_span.end..]);

    if rotation != 0.0 {
        out = apply_rotation_to_children(&out, rotation);
    }
    if let Some(reference) = reference {
        out = replace_property_value(&out, "Reference", reference);
    }
    if layer != "F.Cu" {
        out = replace_footprint_layer(&out, layer);
    }

    Ok(out)
}

/// The name a board entry should carry for a footprint read from `path`.
///
/// `resolve_footprint_path` also accepts a bare filesystem path, which is
/// convenient for a caller holding a `.kicad_mod` directly. That path must not
/// reach the board file: `(footprint "C:\…\R_0805_2012Metric.kicad_mod")` is
/// not a library identifier, and KiCad reports the placed part as a broken
/// library link. This function is therefore total — every branch returns
/// something that is not a path.
///
/// Preference order, most authoritative first:
///
/// 1. The caller already gave a `Library:Footprint` id — use it verbatim.
/// 2. The fp-lib-table maps a nickname to the containing directory. Only the
///    table can answer this: KiCad lets any nickname point at any path, so
///    `MyParts` may well live in `vendor.pretty`, and guessing from the
///    directory would silently mislink the part.
/// 3. The conventional `<nickname>.pretty/` layout. The library is not
///    registered, so the link will be broken either way, but this is the
///    nickname the user gets when they do register it.
/// 4. Neither — fall back to a bare footprint name, which links to nothing but
///    is at least a valid name. The library file's own is used when it is not
///    itself path-like; otherwise the file stem, which cannot contain a
///    separator.
fn board_lib_id(reference: &str, path: &std::path::Path, declared: &str) -> String {
    if is_lib_id(reference) {
        return reference.to_string();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    if let Some(dir) = path.parent() {
        if let Some(nick) = footprint_lib_nickname_for_dir(dir) {
            return format!("{nick}:{stem}");
        }
        if let Some(nick) = pretty_dir_nickname(dir) {
            return format!("{nick}:{stem}");
        }
    }

    if declared.is_empty() || declared.contains('/') || declared.contains('\\') {
        stem
    } else {
        declared.to_string()
    }
}

/// The nickname a conventional `<nickname>.pretty` directory implies.
///
/// Matched case-insensitively: KiCad's own libraries are lowercase `.pretty`,
/// but Windows and macOS filesystems are case-insensitive, so a `.Pretty` on
/// disk is the same directory to KiCad and should not change the answer.
fn pretty_dir_nickname(dir: &std::path::Path) -> Option<String> {
    let name = dir.file_name()?.to_string_lossy().into_owned();
    let cut = name.len().checked_sub(".pretty".len())?;
    name[cut..]
        .eq_ignore_ascii_case(".pretty")
        .then(|| name[..cut].to_string())
        .filter(|nick| !nick.is_empty())
}

/// Fold the footprint's placement rotation into its pads and text items.
///
/// KiCad stores each pad's and text item's *absolute* orientation while their
/// positions stay in unrotated footprint-local coordinates — a `C_0603` placed
/// at -90° keeps `(at -0.775 0 270)` on pad 1. Omitting this leaves the pad
/// shapes unrotated relative to the body and makes KiCad's
/// `lib_footprint_mismatch` check fire.
///
/// Text is additionally kept readable: KiCad flips an angle that would leave a
/// label upside down by 180°, so a -90° footprint carries `90` on its reference.
fn apply_rotation_to_children(content: &str, rotation: f64) -> String {
    let mut out = content.to_string();

    for tag in ["pad", "property", "fp_text"] {
        let readable = tag != "pad";
        // Rewrite back-to-front so earlier byte offsets stay valid.
        let starts: Vec<usize> = find_block_starts(&out, tag);
        for start in starts.into_iter().rev() {
            let Some((bstart, bend)) = find_balanced_block(&out, start) else {
                continue;
            };
            // The block's own `(at …)` is its first — nested ones (a pad's
            // `(primitives …)`, for instance) come later.
            let Some(at_start) = find_block_starts(&out[bstart..bend], "at")
                .first()
                .map(|i| bstart + i)
            else {
                continue;
            };
            let Some((astart, aend)) = find_balanced_block(&out, at_start) else {
                continue;
            };
            let Some(rewritten) = rotate_at_block(&out[astart..aend], rotation, readable) else {
                continue;
            };
            out.replace_range(astart..aend, &rewritten);
        }
    }
    out
}

/// Rewrite `(at x y [angle])`, adding `rotation` to the angle.
///
/// Returns `None` when the block does not look like a positional `at`.
fn rotate_at_block(block: &str, rotation: f64, readable: bool) -> Option<String> {
    let inner = block.strip_prefix('(')?.strip_suffix(')')?;
    let mut parts = inner.split_whitespace();
    if parts.next()? != "at" {
        return None;
    }
    let x: f64 = parts.next()?.parse().ok()?;
    let y: f64 = parts.next()?.parse().ok()?;
    let existing: f64 = parts.next().and_then(|a| a.parse().ok()).unwrap_or(0.0);
    if parts.next().is_some() {
        return None; // `(at …)` with unexpected extra tokens — leave alone.
    }

    let mut angle = (existing + rotation).rem_euclid(360.0);
    if readable && angle > 90.0 && angle <= 270.0 {
        angle -= 180.0;
    }
    Some(format_at(x, y, angle))
}

/// Render `(at x y angle)`, dropping a zero angle as KiCad's writer does and
/// trimming trailing zeros from the decimals.
fn format_at(x: f64, y: f64, angle: f64) -> String {
    let n = |v: f64| {
        let s = format!("{v:.6}");
        let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        if s == "-0" {
            "0".to_string()
        } else {
            s
        }
    };
    if angle == 0.0 {
        format!("(at {} {})", n(x), n(y))
    } else {
        format!("(at {} {} {})", n(x), n(y), n(angle))
    }
}

/// Byte range of the quoted name in the leading `(footprint "NAME"` header,
/// including the surrounding quotes.
fn footprint_name_span(content: &str) -> Option<std::ops::Range<usize>> {
    let block = *find_block_starts(content, "footprint").first()?;
    let after_tag = block + "(footprint".len();
    let rel = content[after_tag..].find('"')?;
    let start = after_tag + rel;
    let end = start + 1 + content[start + 1..].find('"')?;
    Some(start..end + 1)
}

/// Quote and escape `value` as an S-expression string literal.
fn escape_sexp_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Replace the value of the first `(property "<key>" "<value>" …)` entry.
fn replace_property_value(content: &str, key: &str, value: &str) -> String {
    let needle = format!("(property \"{key}\"");
    let Some(prop) = find_block_starts(content, "property")
        .into_iter()
        .find(|&i| content[i..].starts_with(&needle))
    else {
        return content.to_string();
    };
    let after_key = prop + needle.len();
    let Some(rel) = content[after_key..].find('"') else {
        return content.to_string();
    };
    let vstart = after_key + rel;
    let Some(rel_end) = content[vstart + 1..].find('"') else {
        return content.to_string();
    };
    let vend = vstart + 1 + rel_end + 1;

    let mut out = String::with_capacity(content.len());
    out.push_str(&content[..vstart]);
    out.push_str(&escape_sexp_string(value));
    out.push_str(&content[vend..]);
    out
}

/// Replace the footprint's own `(layer "…")` — the first `layer` block that is a
/// direct child of the footprint, not one belonging to a pad or graphic.
///
/// Note this only retargets the footprint; a true F.Cu↔B.Cu flip would also
/// have to mirror every child item, which KiCAD does itself when the user flips
/// a placed footprint.
fn replace_footprint_layer(content: &str, layer: &str) -> String {
    let Some(name) = footprint_name_span(content) else {
        return content.to_string();
    };
    let Some(start) = find_block_starts(content, "layer")
        .into_iter()
        .find(|&i| i > name.end)
    else {
        return content.to_string();
    };
    let Some((bstart, bend)) = find_balanced_block(content, start) else {
        return content.to_string();
    };

    let mut out = String::with_capacity(content.len());
    out.push_str(&content[..bstart]);
    out.push_str(&format!("(layer {})", escape_sexp_string(layer)));
    out.push_str(&content[bend..]);
    out
}

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
    let reference = args["reference"].as_str().map(str::to_string);

    let board_path = get_path(args, "board")?;
    let sexp = match board_footprint_sexp(&footprint, x, y, rotation, &layer, reference.as_deref())
    {
        Ok(s) => s,
        Err(msg) => return Ok(CallToolResult::error(msg)),
    };

    // Try IPC first, then fall back to editing the board file — the same
    // strategy `pcb_board`'s tools use. KiCad 10.0 answers
    // ParseAndCreateItemsFromString with an empty CreateItemsResponse and
    // creates nothing, so in practice this always falls through; the IPC
    // attempt is kept so the tool starts working the moment KiCad implements
    // the command.
    let sexp_ipc = sexp.clone();
    let ipc_result = with_ipc(ctx.config.ipc_address.clone(), move |c| {
        c.place_footprint(&sexp_ipc)
    })
    .await?;

    if ipc_result.is_ok() {
        return Ok(CallToolResult::json(&json!({
            "placed": reference.unwrap_or_default(),
            "footprint": footprint,
            "x": x, "y": y, "rotation": rotation, "layer": layer,
            "source": "ipc"
        })));
    }

    // A "created no footprint" error means KiCad answered — so it is running and
    // may hold this board open, in which case it cannot see a file edit and will
    // overwrite it on its next save.
    let kicad_reachable = ipc_result
        .as_ref()
        .err()
        .is_some_and(|e| e.contains("created no footprint"));

    insert_into_board(&board_path, std::slice::from_ref(&sexp))?;

    let mut out = json!({
        "placed": reference.unwrap_or_default(),
        "footprint": footprint,
        "x": x, "y": y, "rotation": rotation, "layer": layer,
        "source": "file"
    });
    if kicad_reachable {
        out["warning"] = json!(
            "KiCAD is running and could not place this over IPC (KiCAD 10.0's \
             ParseAndCreateItemsFromString does nothing), so the board file was edited \
             directly. If this board is open in the PCB editor, close it without saving \
             and reopen it — otherwise KiCAD will overwrite this change."
        );
    }
    Ok(CallToolResult::json(&out))
}

/// Insert `blocks` just inside the board's closing paren and write it back,
/// refusing to write anything that is not one complete `(kicad_pcb …)` form.
///
/// The insert point is `rfind(')')`, which is only the right place if the file
/// really is a single closed form. Checking the result before committing it
/// means a board that was already truncated — or a footprint block that was —
/// fails loudly instead of being written back over the user's file in a state
/// KiCad can no longer open.
///
/// Like the rest of `konnect-sexp`, this treats parens as syntax everywhere: a
/// `#`-commented paren would be miscounted. KiCad does not write comments into
/// `.kicad_pcb`, and no reader in this workspace understands them either, so
/// the assumption is at least consistent.
fn insert_into_board(board_path: &std::path::Path, blocks: &[String]) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let joined: String = blocks
        .iter()
        .map(|b| format!("\n{}", indent_block(b.trim_end(), "\t")))
        .collect();
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, joined)]);

    if let Err(why) = check_single_board_form(&new_content) {
        anyhow::bail!(
            "Refusing to write {}: {}. The board file was left untouched.",
            board_path.display(),
            why
        );
    }

    write_atomic(board_path, &new_content)?;
    Ok(())
}

/// Verify `content` is exactly one `(kicad_pcb …)` form and nothing else.
///
/// Checking only that *a* balanced block exists is too weak to back the promise
/// above: `find_balanced_block` skips whatever precedes the first paren, so
/// leading garbage would pass, as would a well-formed form that is not a board
/// at all.
fn check_single_board_form(content: &str) -> Result<(), String> {
    let trimmed = content.trim();
    let (start, end) = find_balanced_block(trimmed, 0)
        .ok_or_else(|| "the result is not a balanced S-expression".to_string())?;

    if start != 0 {
        return Err(format!(
            "{} bytes of content precede the opening paren",
            start
        ));
    }
    if end != trimmed.len() {
        return Err(format!(
            "{} bytes of content follow the closing paren",
            trimmed.len() - end
        ));
    }
    if !trimmed[1..].trim_start().starts_with("kicad_pcb") {
        return Err("the root expression is not (kicad_pcb …)".to_string());
    }
    Ok(())
}

/// Prefix every non-empty line with `indent`.
fn indent_block(block: &str, indent: &str) -> String {
    block
        .lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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
            // Transform local pad coords to board space (rotation only).
            // Uses the canonical KiCAD transform — see konnect_sexp::geometry.
            let (board_x, board_y) =
                konnect_sexp::geometry::transform_pad(local_x, local_y, fp_x, fp_y, fp_rot);
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

    let board_path = get_path(args, "board")?;

    // Build every block up front so a bad footprint id fails before anything is
    // written or sent.
    let mut placed = Vec::new();
    let mut blocks = Vec::new();
    let mut n = ref_start;
    for row in 0..count_y {
        for col in 0..count_x {
            let x = start_x + col as f64 * spacing_x;
            let y = start_y + row as f64 * spacing_y;
            let reference = format!("{prefix}{n}");
            match board_footprint_sexp(&footprint, x, y, 0.0, "F.Cu", Some(&reference)) {
                Ok(s) => blocks.push(s),
                Err(msg) => return Ok(CallToolResult::error(msg)),
            }
            placed.push(json!({ "reference": reference, "x": x, "y": y }));
            n += 1;
        }
    }

    // Same IPC-then-file strategy as place_component.
    let ipc_blocks = blocks.clone();
    let ipc_result = with_ipc(ctx.config.ipc_address.clone(), move |c| {
        for b in &ipc_blocks {
            c.place_footprint(b)?;
        }
        Ok(())
    })
    .await?;

    if ipc_result.is_ok() {
        return Ok(CallToolResult::json(&json!({
            "placed_count": placed.len(), "components": placed, "source": "ipc"
        })));
    }

    let kicad_reachable = ipc_result
        .as_ref()
        .err()
        .is_some_and(|e| e.contains("created no footprint"));

    insert_into_board(&board_path, &blocks)?;

    let mut out = json!({
        "placed_count": placed.len(), "components": placed, "source": "file"
    });
    if kicad_reachable {
        out["warning"] = json!(
            "KiCAD is running but cannot place footprints over IPC on KiCAD 10.0, so the \
             board file was edited directly. If this board is open in the PCB editor, close \
             it without saving and reopen it — otherwise KiCAD will overwrite this change."
        );
    }
    Ok(CallToolResult::json(&out))
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

    let sexp = match board_footprint_sexp(
        &src.footprint,
        x,
        y,
        src.rotation,
        &src.layer,
        Some(&new_reference),
    ) {
        Ok(s) => s,
        Err(msg) => return Ok(CallToolResult::error(msg)),
    };

    ipc!(ctx, |c| c.place_footprint(&sexp));
    Ok(CallToolResult::json(&json!({
        "duplicated_from": reference,
        "new_reference": new_reference,
        "footprint": src.footprint,
        "x": x, "y": y, "rotation": src.rotation, "layer": src.layer
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

#[cfg(test)]
mod footprint_sexp_tests {
    use super::*;
    use std::path::Path;

    /// A library footprint in the exact shape KiCad ships: TAB-indented, name
    /// without a library prefix, `REF**` placeholder, no `(at …)`.
    fn library_footprint() -> String {
        [
            "(footprint \"R_0805_2012Metric\"",
            "\t(version 20260206)",
            "\t(generator \"kicad-footprint-generator\")",
            "\t(layer \"F.Cu\")",
            "\t(descr \"Resistor SMD 0805\")",
            "\t(property \"Reference\" \"REF**\"",
            "\t\t(at 0 -1.65 0)",
            "\t\t(layer \"F.SilkS\")",
            "\t)",
            "\t(property \"Value\" \"R_0805_2012Metric\"",
            "\t\t(at 0 1.65 0)",
            "\t\t(layer \"F.Fab\")",
            "\t)",
            "\t(pad \"1\" smd roundrect",
            "\t\t(at -0.9125 0)",
            "\t\t(size 1.025 1.4)",
            "\t\t(layers \"F.Cu\" \"F.Paste\" \"F.Mask\")",
            "\t)",
            ")",
            "",
        ]
        .join("\r\n")
    }

    fn test_ctx() -> ToolContext {
        // No IPC address: the handler's IPC attempt fails immediately and it
        // takes the file path, which is what KiCad 10.0 forces in practice
        // anyway (ParseAndCreateItemsFromString creates nothing).
        ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    const EMPTY_BOARD: &str = "(kicad_pcb
	(version 20260206)
	(generator \"pcbnew\")
	(net 0 \"\")
)
";

    /// Write a library footprint and an empty board into `dir`.
    /// A `.pretty` library holding one footprint, plus an empty board.
    ///
    /// The footprint lives in `Resistor_SMD.pretty/` the way KiCad lays
    /// libraries out, so passing its path exercises the same nickname
    /// derivation a real `Resistor_SMD:R_0805_2012Metric` id would produce —
    /// while still skipping the fp-lib-table, which keeps these tests hermetic.
    fn fixture(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let pretty = dir.join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        let modfile = pretty.join("R_0805_2012Metric.kicad_mod");
        std::fs::write(&modfile, library_footprint()).unwrap();
        let board = dir.join("b.kicad_pcb");
        std::fs::write(&board, EMPTY_BOARD).unwrap();
        (modfile, board)
    }

    #[tokio::test]
    async fn place_component_puts_a_real_footprint_on_the_board() {
        // Regression: place_component reported success while the board stayed
        // empty. It sent a body-less stub — (footprint "Lib:Fp") with a layer
        // and a position and nothing else — and discarded KiCad's response, so
        // two "successful" placements followed by a save produced a board file
        // containing zero footprints.
        let tmp = tempfile::tempdir().unwrap();
        let (modfile, board) = fixture(tmp.path());

        let args = json!({
            "board": board.to_string_lossy(),
            // A direct path skips the fp-lib-table, keeping this test hermetic.
            "footprint": modfile.to_string_lossy(),
            "reference": "R7",
            "x": 50.0, "y": 60.0,
        });
        let res = handle_place_component(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);

        let out = std::fs::read_to_string(&board).unwrap();
        assert_eq!(
            out.matches("(footprint \"").count(),
            1,
            "no footprint:
{out}"
        );
        assert!(
            out.contains("(at 50 60 0)"),
            "placement missing:
{out}"
        );
        assert!(out.contains("(property \"Reference\" \"R7\""), "{out}");
        // The heart of the bug: the payload must carry the real definition.
        assert!(
            out.contains("(pad \"1\" smd roundrect"),
            "pads missing:
{out}"
        );
        assert!(out.contains("(size 1.025 1.4)"), "pad geometry missing");
        assert!(out.contains("(uuid \""), "board items need a uuid");
        // The board must name a library, not the file it was read from.
        assert!(
            out.contains("(footprint \"Resistor_SMD:R_0805_2012Metric\""),
            "board should carry a Library:Footprint id:
{out}"
        );
        assert!(
            !out.contains(".kicad_mod"),
            "a filesystem path leaked into the board:
{out}"
        );
        assert_eq!(
            count_parens(&out),
            0,
            "board is no longer balanced:
{out}"
        );
    }

    /// `board_lib_id` for a path, with the library file's declared name.
    fn id_for(path: &str, declared: &str) -> String {
        board_lib_id(path, Path::new(path), declared)
    }

    #[test]
    fn board_lib_id_never_yields_a_filesystem_path() {
        // A Library:Footprint id is already what the board wants.
        assert_eq!(
            board_lib_id("Resistor_SMD:R_0805", Path::new("/ignored"), "R_0805"),
            "Resistor_SMD:R_0805"
        );
        // A path in a .pretty library takes the nickname from its directory.
        assert_eq!(
            id_for(
                "/usr/share/kicad/footprints/Resistor_SMD.pretty/R_0805.kicad_mod",
                "R_0805"
            ),
            "Resistor_SMD:R_0805"
        );
        // Loose file: no nickname to recover, so it keeps the name the library
        // file declares — unlinked, but a valid name rather than a path.
        assert_eq!(
            id_for("/tmp/scratch/R_0805.kicad_mod", "R_0805_2012Metric"),
            "R_0805_2012Metric"
        );
    }

    #[test]
    fn a_path_like_declared_name_falls_back_to_the_file_stem() {
        // A malformed library file naming itself with a path must not smuggle
        // that path into the board through the fallback branch.
        assert_eq!(
            id_for("/tmp/scratch/R_0805.kicad_mod", "/tmp/other/R.kicad_mod"),
            "R_0805"
        );
        assert_eq!(
            id_for("/tmp/scratch/R_0805.kicad_mod", r"C:\x\R.kicad_mod"),
            "R_0805"
        );
        // An empty declared name is no better than a path.
        assert_eq!(id_for("/tmp/scratch/R_0805.kicad_mod", ""), "R_0805");
    }

    #[test]
    fn windows_paths_are_not_mistaken_for_library_ids() {
        // The drive letter's colon is why is_lib_id cannot just look for one.
        assert_eq!(
            id_for(
                r"C:\KiCad\footprints\Resistor_SMD.pretty\R_0805.kicad_mod",
                "R_0805"
            ),
            "Resistor_SMD:R_0805"
        );
    }

    #[test]
    fn pretty_suffix_matching_ignores_case() {
        // Windows and macOS filesystems are case-insensitive, so Foo.Pretty and
        // Foo.pretty are the same directory to KiCad.
        assert_eq!(
            pretty_dir_nickname(Path::new("/libs/Resistor_SMD.Pretty")),
            Some("Resistor_SMD".into())
        );
        assert_eq!(
            pretty_dir_nickname(Path::new("/libs/Resistor_SMD.pretty")),
            Some("Resistor_SMD".into())
        );
        // A bare ".pretty" leaves no nickname behind.
        assert_eq!(pretty_dir_nickname(Path::new("/libs/.pretty")), None);
        assert_eq!(pretty_dir_nickname(Path::new("/libs/plain")), None);
    }

    #[test]
    fn a_board_edit_must_stay_one_kicad_pcb_form() {
        assert!(check_single_board_form("(kicad_pcb (version 20241229))").is_ok());
        assert!(check_single_board_form("\n  (kicad_pcb (version 1))\n\n").is_ok());

        // Truncated — the bug this guard exists for.
        assert!(check_single_board_form("(kicad_pcb (version 1)").is_err());
        // Leading garbage would otherwise be skipped by find_balanced_block.
        assert!(check_single_board_form("garbage(kicad_pcb (version 1))").is_err());
        // A second form after the root is not one board.
        assert!(check_single_board_form("(kicad_pcb (version 1))(extra)").is_err());
        // Well-formed, but not a board.
        assert!(check_single_board_form("(not_a_board (version 1))").is_err());
    }

    #[tokio::test]
    async fn a_truncated_board_is_refused_rather_than_rewritten() {
        // rfind(')') picks the insert point, so a board that is not one closed
        // (kicad_pcb …) form would silently gain a footprint outside the root
        // expression. Nothing should be written in that case.
        let tmp = tempfile::tempdir().unwrap();
        let (modfile, board) = fixture(tmp.path());
        let truncated = "(kicad_pcb (version 20241229) (generator \"test\")";
        std::fs::write(&board, truncated).unwrap();

        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": modfile.to_string_lossy(),
            "reference": "R1", "x": 1.0, "y": 2.0,
        });
        let err = handle_place_component(&args, &test_ctx())
            .await
            .expect_err("a malformed board must not be written back");
        assert!(
            err.to_string().contains("balanced"),
            "error should explain why: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&board).unwrap(),
            truncated,
            "board must be left exactly as it was"
        );
    }

    #[tokio::test]
    async fn place_component_reports_which_path_it_took() {
        let tmp = tempfile::tempdir().unwrap();
        let (modfile, board) = fixture(tmp.path());
        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": modfile.to_string_lossy(),
            "reference": "R1", "x": 1.0, "y": 2.0,
        });
        let res = handle_place_component(&args, &test_ctx()).await.unwrap();
        let out: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();
        assert_eq!(out["source"], "file");
        assert_eq!(out["placed"], "R1");
        // KiCad was never reachable here, so no reopen warning is warranted.
        assert!(out.get("warning").is_none(), "unexpected warning: {out}");
    }

    #[tokio::test]
    async fn place_component_rejects_an_unknown_footprint_without_touching_the_board() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, board) = fixture(tmp.path());
        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": tmp.path().join("does_not_exist.kicad_mod").to_string_lossy(),
            "reference": "R1", "x": 1.0, "y": 2.0,
        });
        let res = handle_place_component(&args, &test_ctx()).await.unwrap();
        assert!(res.is_error, "a missing footprint must be an error");
        assert_eq!(
            std::fs::read_to_string(&board).unwrap(),
            EMPTY_BOARD,
            "board must be left untouched"
        );
    }

    #[tokio::test]
    async fn placed_rotation_reaches_the_pads() {
        // A rotated placement whose pads keep angle 0 trips KiCad's own
        // lib_footprint_mismatch check, so the rotation has to reach them.
        let tmp = tempfile::tempdir().unwrap();
        let (modfile, board) = fixture(tmp.path());
        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": modfile.to_string_lossy(),
            "reference": "R1", "x": 10.0, "y": 20.0, "rotation": -90.0,
        });
        let res = handle_place_component(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "{:?}", res.content);

        let out = std::fs::read_to_string(&board).unwrap();
        assert!(
            out.contains("(at 10 20 -90)"),
            "footprint angle:
{out}"
        );
        assert!(
            out.contains("(at -0.9125 0 270)"),
            "pad angle:
{out}"
        );
        assert!(
            out.contains("(at 0 -1.65 90)"),
            "readable text angle:
{out}"
        );
    }

    #[tokio::test]
    async fn place_component_array_writes_every_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let (modfile, board) = fixture(tmp.path());
        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": modfile.to_string_lossy(),
            "start_x": 10.0, "start_y": 10.0,
            "count_x": 3, "count_y": 2, "spacing_x": 5.0,
            "ref_prefix": "R", "ref_start": 1,
        });
        let res = handle_place_array(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "{:?}", res.content);

        let out = std::fs::read_to_string(&board).unwrap();
        assert_eq!(out.matches("(footprint \"").count(), 6, "{out}");
        for r in ["R1", "R2", "R3", "R4", "R5", "R6"] {
            assert!(
                out.contains(&format!("(property \"Reference\" \"{r}\"")),
                "missing {r}"
            );
        }
        assert_eq!(count_parens(&out), 0, "board is no longer balanced");
    }

    /// Net paren depth, ignoring anything inside quoted strings.
    fn count_parens(s: &str) -> i32 {
        let (mut depth, mut in_str, mut esc) = (0i32, false, false);
        for ch in s.chars() {
            match ch {
                _ if esc => esc = false,
                '\\' if in_str => esc = true,
                '"' => in_str = !in_str,
                '(' if !in_str => depth += 1,
                ')' if !in_str => depth -= 1,
                _ => {}
            }
        }
        depth
    }

    fn result_text(res: &CallToolResult) -> String {
        match res.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn name_span_covers_the_quoted_header_name() {
        let c = library_footprint();
        let span = footprint_name_span(&c).expect("header not found");
        assert_eq!(&c[span], "\"R_0805_2012Metric\"");
    }

    #[test]
    fn name_span_is_none_without_a_footprint_block() {
        assert!(footprint_name_span("(kicad_pcb (version 20250610))").is_none());
    }

    #[test]
    fn reference_substitution_targets_the_reference_property_only() {
        let out = replace_property_value(&library_footprint(), "Reference", "R42");
        assert!(out.contains("(property \"Reference\" \"R42\""), "{out}");
        assert!(
            out.contains("(property \"Value\" \"R_0805_2012Metric\""),
            "Value must be untouched:\n{out}"
        );
        assert!(!out.contains("REF**"));
    }

    #[test]
    fn reference_substitution_is_a_no_op_when_the_property_is_absent() {
        let c = "(footprint \"X\"\n\t(layer \"F.Cu\")\n)";
        assert_eq!(replace_property_value(c, "Reference", "R1"), c);
    }

    #[test]
    fn layer_override_retargets_the_footprint_not_its_pads() {
        let out = replace_footprint_layer(&library_footprint(), "B.Cu");
        assert!(out.contains("(layer \"B.Cu\")"), "{out}");
        // The pad's layer list and the silkscreen text layer must survive.
        assert!(out.contains("(layers \"F.Cu\" \"F.Paste\" \"F.Mask\")"));
        assert!(out.contains("(layer \"F.SilkS\")"));
        assert_eq!(
            out.matches("(layer \"B.Cu\")").count(),
            1,
            "only the footprint's own layer should change:\n{out}"
        );
    }

    #[test]
    fn pad_angles_absorb_the_footprint_rotation() {
        // KiCad stores each pad's absolute orientation: a footprint placed at
        // -90 carries 270 on its pads, while pad positions stay in unrotated
        // footprint-local coordinates.
        let out = apply_rotation_to_children(&library_footprint(), -90.0);
        assert!(out.contains("(at -0.9125 0 270)"), "{out}");
        // Position is unchanged; only the angle was added.
        assert!(
            !out.contains("(at 0 -0.9125"),
            "pad position must not rotate"
        );
    }

    #[test]
    fn pad_angles_wrap_into_zero_to_360() {
        let out = apply_rotation_to_children(&library_footprint(), 270.0);
        assert!(out.contains("(at -0.9125 0 270)"), "{out}");
        // 270 + 270 = 540 -> 180
        let twice = apply_rotation_to_children(&out, 270.0);
        assert!(twice.contains("(at -0.9125 0 180)"), "{twice}");
    }

    #[test]
    fn text_angles_are_kept_readable() {
        // A -90 footprint would put text at 270, which reads upside down, so
        // KiCad flips it by 180 to 90 — matching what eeschema/pcbnew write.
        let out = apply_rotation_to_children(&library_footprint(), -90.0);
        assert!(
            out.contains("(at 0 -1.65 90)"),
            "reference text:
{out}"
        );
        assert!(
            out.contains("(at 0 1.65 90)"),
            "value text:
{out}"
        );
    }

    #[test]
    fn zero_rotation_is_written_without_an_angle() {
        assert_eq!(format_at(1.5, -2.0, 0.0), "(at 1.5 -2)");
        assert_eq!(format_at(0.0, 0.0, 90.0), "(at 0 0 90)");
    }

    #[test]
    fn rotate_at_block_rejects_non_positional_at() {
        assert!(rotate_at_block("(at)", 90.0, false).is_none());
        assert!(rotate_at_block("(atomic 1 2)", 90.0, false).is_none());
        assert!(rotate_at_block("(at 1 2 3 4)", 90.0, false).is_none());
    }

    #[test]
    fn indent_block_prefixes_each_line_and_leaves_blanks_alone() {
        assert_eq!(
            indent_block(
                "a

b", "	"
            ),
            "	a

	b"
        );
    }

    #[test]
    fn sexp_strings_are_escaped() {
        // Input characters:  a " b \ c
        // Expected output:   " a \ " b \ \ c "
        let input = ['a', '"', 'b', '\\', 'c'].iter().collect::<String>();
        let expected = ['"', 'a', '\\', '"', 'b', '\\', '\\', 'c', '"']
            .iter()
            .collect::<String>();
        assert_eq!(escape_sexp_string(&input), expected);
        assert_eq!(escape_sexp_string("plain"), "\"plain\"");
    }

    /// The end-to-end transform, exercised through a real library file on disk.
    #[test]
    fn board_footprint_carries_the_definition_position_and_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let pretty = tmp.path().join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        std::fs::write(
            pretty.join("R_0805_2012Metric.kicad_mod"),
            library_footprint(),
        )
        .unwrap();

        // resolve_footprint_path takes a plain path when it isn't a lib id.
        let direct = pretty.join("R_0805_2012Metric.kicad_mod");
        let content = std::fs::read_to_string(&direct).unwrap();
        let span = footprint_name_span(&content).unwrap();

        // Reproduce board_footprint_sexp's transform on the resolved content so
        // the assertions do not depend on the machine's fp-lib-table.
        let mut out = String::new();
        out.push_str(&content[..span.start]);
        out.push_str(&escape_sexp_string("Resistor_SMD:R_0805_2012Metric"));
        out.push_str("\n\t(at 50 60 90)\n\t(uuid \"fixed-uuid\")");
        out.push_str(&content[span.end..]);
        let out = replace_property_value(&out, "Reference", "R7");

        // The library id replaces the bare name.
        assert!(
            out.starts_with("(footprint \"Resistor_SMD:R_0805_2012Metric\""),
            "{out}"
        );
        // Placement is stamped in.
        assert!(out.contains("(at 50 60 90)"));
        assert!(out.contains("(uuid \"fixed-uuid\")"));
        // The reference designator is real, not the placeholder.
        assert!(out.contains("(property \"Reference\" \"R7\""));
        // Critically, the pad definition survives — a body-less stub is exactly
        // what made KiCad create nothing.
        assert!(out.contains("(pad \"1\" smd roundrect"));
        assert!(out.contains("(size 1.025 1.4)"));
    }
}
