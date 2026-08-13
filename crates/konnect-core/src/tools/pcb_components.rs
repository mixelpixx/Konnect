//! `pcb_components` toolset — place, move, rotate, query, and array footprints on the PCB.
//!
//! Most operations use the KiCAD IPC API so they integrate with KiCAD's undo/redo
//! system and don't require a separate file-sync step. `get_board_2d_view` uses
//! kicad-cli to render a PNG.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::library::{footprint_lib_nickname_for_dir, is_lib_id, resolve_footprint_path};
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use anyhow::Context;
use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::writer::{
    apply_edits, find_balanced_block, find_block_starts, new_uuid, write_atomic,
};
use konnect_sexp::SexpEdit;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ─── IPC helper ───────────────────────────────────────────────────────────────

async fn with_ipc<T, F>(addr: String, f: F) -> anyhow::Result<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(&KiCadIpcClient::new(&addr))).await {
        Ok(Ok(r)) => Ok(Ok(r)),
        Ok(Err(e)) => Ok(Err(format!("{e:#}"))),
        Err(e) => Err(anyhow::anyhow!("Thread error: {}", e)),
    }
}

/// As [`with_ipc`], but classifying a failure as transport-unreachable vs
/// KiCad-rejected via [`konnect_ipc::IpcFailure`] — the typed gate for the
/// file-editing fallback (never a text match on the error message).
pub(crate) async fn with_ipc_classified<T, F>(
    addr: String,
    f: F,
) -> anyhow::Result<Result<T, konnect_ipc::IpcFailure>>
where
    T: Send + 'static,
    F: FnOnce(&KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        f(&KiCadIpcClient::new(&addr)).map_err(konnect_ipc::IpcFailure::from_error)
    })
    .await
    {
        Ok(result) => Ok(result),
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

// ─── Footprint-library resolution ───────────────────────────────────────────

/// Read the library source of `lib_id` (`Library:Footprint`), resolving it
/// through the project's fp-lib-table (the board's directory), then the global
/// table, then the conventional KiCad library directories — the lookup that
/// `library::resolve_footprint_path` owns.
fn resolve_footprint_source(lib_id: &str, board: &Path) -> anyhow::Result<String> {
    let (nickname, entry) = lib_id.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("footprint must use Library:Footprint syntax, got '{lib_id}'")
    })?;
    if nickname.is_empty() || entry.is_empty() {
        anyhow::bail!("footprint must use a non-empty Library:Footprint identifier");
    }
    let path = super::library::resolve_footprint_path(lib_id, board.parent())
        .map_err(|message| anyhow::anyhow!(message))?;
    std::fs::read_to_string(&path)
        .map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display()))
}

/// Structured rejection for any back-side (`B.*`) placement layer.
///
/// Placing on the back is not a layer rename: KiCAD's flip mirrors the
/// footprint's geometry (pad X positions negate, every front layer swaps with
/// its back counterpart per item). Until Konnect implements that mirror,
/// pretending to support `B.Cu` silently produces wrong copper, so the layer
/// is refused up front — before anything is resolved, sent, or written.
fn back_side_layer_error(layer: &str) -> Option<CallToolResult> {
    if !layer.starts_with("B.") {
        return None;
    }
    Some(CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::InvalidArgument {
            field: "layer".to_string(),
            reason: format!("back-side placement on '{layer}' is not yet supported"),
        },
        format!(
            "Cannot place on '{layer}': back-side placement is not yet supported, \
             because a correct flip must mirror the footprint geometry rather than \
             just rename its layers. Place the footprint on F.Cu and flip it to the \
             back in KiCAD (select it and press F)."
        ),
    ))
}

fn escape_sexp_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn replace_quoted_after(source: &mut String, marker: &str, value: &str) -> anyhow::Result<()> {
    let start = source
        .find(marker)
        .map(|offset| offset + marker.len())
        .ok_or_else(|| anyhow::anyhow!("footprint library data is missing {marker}"))?;
    let bytes = source.as_bytes();
    let mut escaped = false;
    let end = (start..bytes.len())
        .find(|index| {
            let byte = bytes[*index];
            if escaped {
                escaped = false;
                false
            } else if byte == b'\\' {
                escaped = true;
                false
            } else {
                byte == b'"'
            }
        })
        .ok_or_else(|| anyhow::anyhow!("unterminated quoted value after {marker}"))?;
    source.replace_range(start..end, &escape_sexp_string(value));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_footprint_source(
    source: &str,
    lib_id: &str,
    reference: &str,
    value: Option<&str>,
    x: f64,
    y: f64,
    rotation: f64,
    layer: &str,
) -> anyhow::Result<String> {
    // No back-side placement: a correct F.Cu→B.Cu flip mirrors the geometry
    // (pad X positions negate, layers swap per item) the way KiCAD's own flip
    // does. A textual layer swap produces wrong copper, so it is refused
    // outright — see back_side_layer_error.
    if layer != "F.Cu" {
        anyhow::bail!(
            "footprints can only be placed on F.Cu (back-side placement is not yet \
             supported because a correct flip must mirror the geometry), got '{layer}'"
        );
    }
    let mut prepared = source.to_string();
    replace_quoted_after(&mut prepared, "(footprint \"", lib_id)?;
    replace_quoted_after(&mut prepared, "(property \"Reference\" \"", reference)?;
    if let Some(value) = value {
        replace_quoted_after(&mut prepared, "(property \"Value\" \"", value)?;
    }
    replace_quoted_after(&mut prepared, "(layer \"", layer)?;
    let layer_start = prepared
        .find("(layer \"")
        .context("footprint library data has no root layer")?;
    let layer_end = prepared[layer_start..]
        .find(')')
        .map(|offset| layer_start + offset + 1)
        .context("footprint root layer is unterminated")?;
    prepared.insert_str(layer_end, &format!("\n\t(at {x} {y} {rotation})"));
    konnect_sexp::parse_sexp(&prepared).context("prepared footprint is not valid S-expression")?;
    Ok(prepared)
}

fn extract_pad_definitions(source: &str) -> anyhow::Result<Vec<konnect_ipc::IpcPadDefinition>> {
    let footprint = konnect_sexp::parse_sexp(source)?;
    footprint
        .find_all("pad")
        .into_iter()
        .map(|pad| {
            let required = |index: usize, label: &str| {
                pad.get(index)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .ok_or_else(|| anyhow::anyhow!("footprint pad is missing {label}"))
            };
            let shape = required(3, "shape")?.to_string();
            if shape == "custom" {
                anyhow::bail!(
                    "custom-shape pads are not supported by KiCad 10's typed placement path"
                );
            }
            let at = pad
                .find("at")
                .context("footprint pad is missing its position")?;
            let size = pad
                .find("size")
                .context("footprint pad is missing its size")?;
            let layers = pad
                .find("layers")
                .context("footprint pad is missing its layer set")?
                .children()
                .unwrap_or_default()
                .iter()
                .skip(1)
                .filter_map(konnect_sexp::SexpNode::as_str)
                .map(str::to_string)
                .collect();
            let (drill_x, drill_y, drill_oval) = match pad.find("drill") {
                Some(drill)
                    if drill.get(1).and_then(konnect_sexp::SexpNode::as_str) == Some("oval") =>
                {
                    (
                        drill.get_f64(2),
                        drill.get_f64(3).or_else(|| drill.get_f64(2)),
                        true,
                    )
                }
                Some(drill) => (
                    drill.get_f64(1),
                    drill.get_f64(2).or_else(|| drill.get_f64(1)),
                    false,
                ),
                None => (None, None, false),
            };
            Ok(konnect_ipc::IpcPadDefinition {
                number: required(1, "number")?.to_string(),
                pad_type: required(2, "type")?.to_string(),
                shape,
                x: at
                    .get_f64(1)
                    .context("footprint pad has an invalid X position")?,
                y: at
                    .get_f64(2)
                    .context("footprint pad has an invalid Y position")?,
                rotation: at.get_f64(3).unwrap_or(0.0),
                size_x: size
                    .get_f64(1)
                    .context("footprint pad has an invalid width")?,
                size_y: size
                    .get_f64(2)
                    .context("footprint pad has an invalid height")?,
                drill_x,
                drill_y,
                drill_oval,
                layers,
                roundrect_ratio: pad.find_f64("roundrect_rratio").unwrap_or(0.0),
            })
        })
        .collect()
}

// ─── Footprint graphics extraction ───────────────────────────────────────────

/// `(start x y)`-style point child of a graphic node.
fn graphic_point(
    node: &konnect_sexp::SexpNode,
    tag: &str,
    kind: &str,
) -> anyhow::Result<(f64, f64)> {
    let point = node
        .find(tag)
        .ok_or_else(|| anyhow::anyhow!("footprint {kind} is missing its ({tag} …)"))?;
    Ok((
        point
            .get_f64(1)
            .ok_or_else(|| anyhow::anyhow!("footprint {kind} has an invalid {tag} X"))?,
        point
            .get_f64(2)
            .ok_or_else(|| anyhow::anyhow!("footprint {kind} has an invalid {tag} Y"))?,
    ))
}

fn graphic_layer(node: &konnect_sexp::SexpNode, kind: &str) -> anyhow::Result<String> {
    node.find_str("layer")
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("footprint {kind} is missing its layer"))
}

/// Stroke width in mm: modern `(stroke (width w) …)`, legacy bare `(width w)`.
/// KiCad's default silkscreen line width stands in when neither is present.
fn graphic_stroke_width(node: &konnect_sexp::SexpNode) -> f64 {
    node.find("stroke")
        .and_then(|stroke| stroke.find_f64("width"))
        .or_else(|| node.find_f64("width"))
        .unwrap_or(0.12)
}

/// `(fill yes)` (KiCad 8+) or legacy `(fill solid)`.
fn graphic_filled(node: &konnect_sexp::SexpNode) -> bool {
    matches!(node.find_str("fill"), Some("yes") | Some("solid"))
}

/// `(hide yes)` (modern) or a bare `hide` atom (legacy).
fn text_hidden(node: &konnect_sexp::SexpNode) -> bool {
    node.find_str("hide") == Some("yes")
        || node
            .children()
            .unwrap_or_default()
            .iter()
            .any(|child| child.as_str() == Some("hide"))
}

/// `(effects (font (size h w)))` glyph size, defaulting to KiCad's 1 mm.
fn text_size(node: &konnect_sexp::SexpNode) -> f64 {
    node.find("effects")
        .and_then(|effects| effects.find("font"))
        .and_then(|font| font.find("size"))
        .and_then(|size| size.get_f64(1))
        .unwrap_or(1.0)
}

/// Text position and angle from `(at x y [rot])`.
fn text_at(node: &konnect_sexp::SexpNode, kind: &str) -> anyhow::Result<((f64, f64), f64)> {
    let at = node
        .find("at")
        .ok_or_else(|| anyhow::anyhow!("footprint {kind} is missing its position"))?;
    Ok((
        (
            at.get_f64(1)
                .ok_or_else(|| anyhow::anyhow!("footprint {kind} has an invalid X position"))?,
            at.get_f64(2)
                .ok_or_else(|| anyhow::anyhow!("footprint {kind} has an invalid Y position"))?,
        ),
        at.get_f64(3).unwrap_or(0.0),
    ))
}

/// Parse a footprint's drawable children — `fp_line`, `fp_rect`, `fp_circle`,
/// `fp_arc`, `fp_poly` and visible `fp_text`/`property` texts — into
/// footprint-local [`konnect_ipc::IpcGraphicDefinition`]s.
///
/// The typed placement path previously shipped pads only, so a placed part had
/// no courtyard, silkscreen, or fab drawing: courtyard DRC had nothing to
/// check and KiCad's `lib_footprint_mismatch` flagged every placement.
///
/// `Reference` and `Value` properties are excluded — `build_footprint_item`
/// already carries those as first-class fields.
/// Footprint-local Reference/Value text anchors from the library source, so
/// placed parts keep the library's text layout (a synthesized offset put the
/// Reference on the part's own silkscreen — silk_overlap in live DRC).
fn extract_field_placement(source: &str) -> konnect_ipc::IpcFieldPlacement {
    let mut placement = konnect_ipc::IpcFieldPlacement::default();
    let Ok(footprint) = konnect_sexp::parse_sexp(source) else {
        return placement;
    };
    for prop in footprint.find_all("property") {
        let Some(name) = prop.get(1).and_then(|n| n.as_str()) else {
            continue;
        };
        let Some(at) = prop.find("at") else {
            continue;
        };
        let num = |i: usize| {
            at.get(i)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
        };
        let (x, y) = (num(1), num(2));
        let rot = num(3).unwrap_or(0.0);
        if let (Some(x), Some(y)) = (x, y) {
            match name {
                "Reference" => placement.reference_at = Some((x, y, rot)),
                "Value" => placement.value_at = Some((x, y, rot)),
                _ => {}
            }
        }
    }
    placement
}

fn extract_graphic_definitions(
    source: &str,
) -> anyhow::Result<Vec<konnect_ipc::IpcGraphicDefinition>> {
    use konnect_ipc::IpcGraphicDefinition as Graphic;
    let footprint = konnect_sexp::parse_sexp(source)?;
    let mut graphics = Vec::new();

    for line in footprint.find_all("fp_line") {
        graphics.push(Graphic::Line {
            start: graphic_point(line, "start", "fp_line")?,
            end: graphic_point(line, "end", "fp_line")?,
            layer: graphic_layer(line, "fp_line")?,
            width: graphic_stroke_width(line),
        });
    }
    for rect in footprint.find_all("fp_rect") {
        graphics.push(Graphic::Rect {
            start: graphic_point(rect, "start", "fp_rect")?,
            end: graphic_point(rect, "end", "fp_rect")?,
            layer: graphic_layer(rect, "fp_rect")?,
            width: graphic_stroke_width(rect),
            filled: graphic_filled(rect),
        });
    }
    for circle in footprint.find_all("fp_circle") {
        graphics.push(Graphic::Circle {
            center: graphic_point(circle, "center", "fp_circle")?,
            end: graphic_point(circle, "end", "fp_circle")?,
            layer: graphic_layer(circle, "fp_circle")?,
            width: graphic_stroke_width(circle),
            filled: graphic_filled(circle),
        });
    }
    for arc in footprint.find_all("fp_arc") {
        graphics.push(Graphic::Arc {
            start: graphic_point(arc, "start", "fp_arc")?,
            mid: graphic_point(arc, "mid", "fp_arc")?,
            end: graphic_point(arc, "end", "fp_arc")?,
            layer: graphic_layer(arc, "fp_arc")?,
            width: graphic_stroke_width(arc),
        });
    }
    for poly in footprint.find_all("fp_poly") {
        let pts = poly
            .find("pts")
            .ok_or_else(|| anyhow::anyhow!("footprint fp_poly is missing its (pts …)"))?;
        let points = pts
            .children()
            .unwrap_or_default()
            .iter()
            .filter(|node| node.head() == Some("xy"))
            .map(|node| {
                Ok((
                    node.get_f64(1)
                        .ok_or_else(|| anyhow::anyhow!("footprint fp_poly has an invalid X"))?,
                    node.get_f64(2)
                        .ok_or_else(|| anyhow::anyhow!("footprint fp_poly has an invalid Y"))?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        graphics.push(Graphic::Poly {
            points,
            layer: graphic_layer(poly, "fp_poly")?,
            width: graphic_stroke_width(poly),
            filled: graphic_filled(poly),
        });
    }
    for text in footprint.find_all("fp_text") {
        if text_hidden(text) {
            continue;
        }
        let content = text
            .get(2)
            .and_then(konnect_sexp::SexpNode::as_str)
            .ok_or_else(|| anyhow::anyhow!("footprint fp_text is missing its text"))?;
        let (position, rotation) = text_at(text, "fp_text")?;
        graphics.push(Graphic::Text {
            text: content.to_string(),
            position,
            rotation,
            layer: graphic_layer(text, "fp_text")?,
            size: text_size(text),
        });
    }
    for property in footprint.find_all("property") {
        let name = property.get(1).and_then(konnect_sexp::SexpNode::as_str);
        // Reference and Value travel as first-class fields; hidden built-ins
        // (Footprint, Datasheet, …) are not drawn.
        if matches!(name, Some("Reference") | Some("Value")) || text_hidden(property) {
            continue;
        }
        let Some(content) = property.get(2).and_then(konnect_sexp::SexpNode::as_str) else {
            continue;
        };
        let Ok((position, rotation)) = text_at(property, "property") else {
            continue;
        };
        let Ok(layer) = graphic_layer(property, "property") else {
            continue;
        };
        graphics.push(Graphic::Text {
            text: content.to_string(),
            position,
            rotation,
            layer,
            size: text_size(property),
        });
    }
    Ok(graphics)
}

// ─── Library footprint → board footprint (file-editing fallback) ─────────────
//
// Used ONLY when the IPC transport is unreachable (unconfigured socket or
// failed dial/send): a live KiCad must never have the board file edited
// behind its back. Ported from emolitor's PR #66.

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
    project_dir: Option<&Path>,
) -> Result<String, String> {
    let path = resolve_footprint_path(lib_id, project_dir)?;
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
    out.push_str(&quote_sexp_string(&board_lib_id(lib_id, &path, declared)));
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
fn board_lib_id(reference: &str, path: &Path, declared: &str) -> String {
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
fn pretty_dir_nickname(dir: &Path) -> Option<String> {
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
fn quote_sexp_string(value: &str) -> String {
    format!("\"{}\"", escape_sexp_string(value))
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
    out.push_str(&quote_sexp_string(value));
    out.push_str(&content[vend..]);
    out
}

/// Replace the footprint's own `(layer "…")` — the first `layer` block that is a
/// direct child of the footprint, not one belonging to a pad or graphic.
///
/// Note this only retargets the footprint; a true F.Cu↔B.Cu flip would also
/// have to mirror every child item, which is why back-side placement is
/// rejected before this code can run (see `back_side_layer_error`).
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
    out.push_str(&format!("(layer {})", quote_sexp_string(layer)));
    out.push_str(&content[bend..]);
    out
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
fn insert_into_board(board_path: &Path, blocks: &[String]) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(board_path)?;
    // KiCad writes these files CRLF on Windows — its bundled .kicad_mod
    // libraries are CRLF throughout — so an inserted block joined with bare LF
    // would leave the board with two conventions in it.
    let eol = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let joined: String = blocks
        .iter()
        .map(|b| format!("{eol}{}", indent_block(b.trim_end(), "\t", eol)))
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

/// Prefix every non-empty line with `indent`, joining them with `eol`.
fn indent_block(block: &str, indent: &str, eol: &str) -> String {
    // `lines()` strips a trailing \r along with the \n, so rejoining with `eol`
    // re-imposes one convention on a block that may have arrived with another —
    // a CRLF library footprint going into an LF board, or the reverse.
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
        .join(eol)
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
            "list_board_footprint_graphics",
            "List the graphic items inside a footprint placed on the board — silkscreen, fabrication, and courtyard artwork — with the UUID needed to edit one. Points are footprint-local millimetres, as the .kicad_mod shows them. Each item reports 'editable', plus 'outlines' and 'holes' for polygons: 'points' covers the first outline only, so an item with more than one outline or any holes is reported but cannot be edited here. Requires KiCAD running with the board open.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator, e.g. 'J2'" },
                    "layer":     { "type": "string", "description": "Only list items on this layer, e.g. 'F.SilkS' (optional)" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_list_board_footprint_graphics(args, ctx).await }
        ),
        tool!(
            "edit_board_footprint_graphic",
            "Replace the vertices of a polygon inside a footprint placed on the board, selected by UUID. Points are footprint-local millimetres, as the .kicad_mod shows them. Use this to bring one placed instance in line with a library change without re-placing the part. Only a single-outline polygon with no holes can be replaced; anything else is refused by name rather than flattened. Requires KiCAD running with the board open.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator, e.g. 'J2'" },
                    "uuid":      { "type": "string", "description": "UUID of the graphic item, from list_board_footprint_graphics" },
                    "points": {
                        "type": "array",
                        "description": "Replacement vertices in footprint-local millimetres; at least 3.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "x": { "type": "number" },
                                "y": { "type": "number" }
                            },
                            "required": ["x", "y"]
                        }
                    }
                },
                "required": ["board", "reference", "uuid", "points"]
            }),
            |args, ctx| async move { handle_edit_board_footprint_graphic(args, ctx).await }
        ),
        tool!(
            "get_component_pads",
            "Return the pad positions and net assignments for a footprint. \
             A pad's 'net' is its net name, \"\" if the pad carries no net node \
             (unconnected), or null if the node is present but unreadable — \
             treat null as an error, not as an unconnected pad.",
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
            "Render the board with kicad-cli and return it as a base64 PNG. Note this is              kicad-cli's 3-D board render viewed from the top, not a layer plot -- there is              no layer selection. Use export_svg for layer-aware 2-D output.",
            json!({
                "type": "object",
                "properties": {
                    "board":  { "type": "string" },
                    "width":  { "type": "integer", "default": 800, "description": "Render width in pixels, clamped to 100-4000 (kept small since the image lands in LLM context, raise it when detail matters)" },
                    "height": { "type": "integer", "default": 600, "description": "Render height in pixels, clamped to 100-4000" }
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
    let board = get_path(args, "board")?;
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
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
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    if let Some(rejection) = back_side_layer_error(&layer) {
        return Ok(rejection);
    }
    let source = match resolve_footprint_source(&footprint, &board) {
        Ok(source) => source,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let prepared = match prepare_footprint_source(
        &source, &footprint, &reference, None, x, y, rotation, &layer,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let pads = match extract_pad_definitions(&prepared) {
        Ok(pads) => pads,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let graphics = match extract_graphic_definitions(&prepared) {
        Ok(graphics) => graphics,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let fields = extract_field_placement(&prepared);

    let value = footprint
        .split_once(':')
        .map(|(_, entry)| entry)
        .unwrap_or(&footprint)
        .to_string();

    // Try IPC first. The fallback gate is the typed transport classification:
    // only when the request never reached a live KiCad (unconfigured socket,
    // failed dial/send) is it safe to edit the board file directly. A KiCad
    // that answered — even with an error — may hold this board open, and a
    // file edited behind a live editor is silently overwritten on its next
    // save, so a rejection fails closed with no fallback.
    let requested_board = board.clone();
    let footprint_ipc = footprint.clone();
    let reference_ipc = reference.clone();
    let layer_ipc = layer.clone();
    let attempt = with_ipc_classified(ctx.config.ipc_address.clone(), move |c| {
        c.place_footprint(
            &requested_board,
            &footprint_ipc,
            &reference_ipc,
            &value,
            &pads,
            &graphics,
            &fields,
            x,
            y,
            rotation,
            &layer_ipc,
        )
    })
    .await?;

    match attempt {
        Ok(fp) => Ok(CallToolResult::json(&json!({
            "placed": fp.reference,
            "footprint": fp.footprint,
            "x": fp.position.x, "y": fp.position.y,
            "rotation": fp.rotation, "layer": fp.layer,
            "source": "ipc"
        }))),
        Err(konnect_ipc::IpcFailure::Rejected(message)) => Ok(CallToolResult::error(format!(
            "KiCAD rejected the placement over IPC: {message}. \
             The board file was not modified — KiCAD is reachable and may hold this \
             board open, so editing the file directly could be silently overwritten."
        ))),
        Err(konnect_ipc::IpcFailure::Unreachable(_)) => {
            // No live KiCad on the other end of this transport: fall back to
            // editing the board file directly.
            let sexp = match board_footprint_sexp(
                &footprint,
                x,
                y,
                rotation,
                &layer,
                Some(&reference),
                board.parent(),
            ) {
                Ok(sexp) => sexp,
                Err(message) => return Ok(CallToolResult::error(message)),
            };
            insert_into_board(&board, std::slice::from_ref(&sexp))?;
            Ok(CallToolResult::json(&json!({
                "placed": reference,
                "footprint": footprint,
                "x": x, "y": y, "rotation": rotation, "layer": layer,
                "source": "file",
                "warning": "KiCAD IPC was not reachable, so the board file was edited \
                            directly. KiCAD will show this footprint when it next loads \
                            the board."
            })))
        }
    }
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
    ipc!(ctx, args, |c| c.move_footprint(&ref_ipc, x, y));
    Ok(CallToolResult::json(
        &json!({ "moved": reference, "x": x, "y": y }),
    ))
}

async fn handle_list_board_footprint_graphics(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer_filter = args["layer"].as_str().map(|s| s.to_string());

    let ref_ipc = reference.clone();
    let graphics: Vec<konnect_ipc::types::IpcFootprintGraphic> =
        ipc!(ctx, args, |c| c.list_footprint_graphics(&ref_ipc));

    let graphics: Vec<_> = graphics
        .into_iter()
        .filter(|g| layer_filter.as_deref().is_none_or(|l| g.layer == l))
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": graphics.len(),
        "reference": reference,
        "graphics": graphics,
    })))
}

async fn handle_edit_board_footprint_graphic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let points: Vec<(f64, f64)> = args["points"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("'points' must be an array of {{x, y}}"))?
        .iter()
        .map(|p| {
            let x = p["x"]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("each point needs a numeric 'x'"))?;
            let y = p["y"]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("each point needs a numeric 'y'"))?;
            Ok::<_, anyhow::Error>((x, y))
        })
        .collect::<Result<_, _>>()?;

    let (ref_ipc, uuid_ipc, pts) = (reference.clone(), uuid.clone(), points.clone());
    let kind: String = ipc!(ctx, args, |c| c
        .set_footprint_graphic_points(&ref_ipc, &uuid_ipc, &pts));

    Ok(CallToolResult::json(&json!({
        "edited": reference,
        "uuid": uuid,
        "kind": kind,
        "points": points.len(),
    })))
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
    ipc!(ctx, args, |c| c.rotate_footprint(&ref_ipc, rotation));
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
    ipc!(ctx, args, |c| c.delete_footprint(&ref_ipc));
    Ok(CallToolResult::json(&json!({ "deleted": reference })))
}

async fn handle_edit_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    if let Some(value) = args["value"].as_str() {
        let reference_for_ipc = reference.clone();
        let value_for_ipc = value.to_string();
        ipc!(ctx, args, |c| c
            .set_footprint_value(&reference_for_ipc, &value_for_ipc));
    }
    let lookup_reference = reference.clone();
    let fp = ipc!(ctx, args, |c| {
        c.get_footprint(&lookup_reference)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", lookup_reference))
    });
    Ok(CallToolResult::json(&json!({
        "reference": fp.reference,
        "value": fp.value,
        "footprint": fp.footprint
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
    let fp = ipc!(ctx, args, |c| {
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
            // Three outcomes, deliberately distinguishable. No (net …) node at
            // all is an unconnected pad, and "" says so. A node we can read
            // gives its name. A node that is present but unreadable gives
            // null — previously it gave "" too, so a fully connected KiCad 10
            // pad was indistinguishable from an unconnected one. See
            // konnect_sexp::net for the two shapes.
            let net = match pad.find("net") {
                None => json!(""),
                Some(node) => match konnect_sexp::net::net_name(node) {
                    Some(name) => json!(name),
                    None => serde_json::Value::Null,
                },
            };
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
    let fps = ipc!(ctx, _args, |c| c.list_footprints());
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
    let board = get_path(args, "board")?;
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
    let count_x = args["count_x"].as_u64().unwrap_or(1);
    let count_y = args["count_y"].as_u64().unwrap_or(1);
    let Some(total_count) = count_x.checked_mul(count_y) else {
        return Ok(CallToolResult::error("Array dimensions overflow."));
    };
    if count_x == 0 || count_y == 0 || total_count > 10_000 {
        return Ok(CallToolResult::error(
            "Array dimensions must be non-zero and contain at most 10,000 components.",
        ));
    }
    let count_x = count_x as usize;
    let count_y = count_y as usize;
    let spacing_x = match require_f64(args, "spacing_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let spacing_y = args["spacing_y"].as_f64().unwrap_or(spacing_x);
    let prefix = args["ref_prefix"].as_str().unwrap_or("U").to_string();
    let ref_start = args["ref_start"].as_u64().unwrap_or(1);
    if ref_start.checked_add(total_count - 1).is_none() {
        return Ok(CallToolResult::error("Reference number overflow."));
    }
    let source = match resolve_footprint_source(&footprint, &board) {
        Ok(source) => source,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    // Graphics are footprint-local and identical for every array instance, so
    // one extraction serves the whole batch.
    let graphics = match extract_graphic_definitions(&source) {
        Ok(graphics) => graphics,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let fields = extract_field_placement(&source);

    let value = footprint
        .split_once(':')
        .map(|(_, entry)| entry)
        .unwrap_or(&footprint)
        .to_string();
    let mut planned = Vec::with_capacity(total_count as usize);
    for row in 0..count_y {
        for col in 0..count_x {
            let x = start_x + col as f64 * spacing_x;
            let y = start_y + row as f64 * spacing_y;
            let reference = format!("{prefix}{}", ref_start + planned.len() as u64);
            let prepared = match prepare_footprint_source(
                &source, &footprint, &reference, None, x, y, 0.0, "F.Cu",
            ) {
                Ok(prepared) => prepared,
                Err(error) => return Ok(CallToolResult::error(error.to_string())),
            };
            let pads = match extract_pad_definitions(&prepared) {
                Ok(pads) => pads,
                Err(error) => return Ok(CallToolResult::error(error.to_string())),
            };
            planned.push((reference, pads, x, y));
        }
    }

    let requested_board = board.clone();
    let footprint_id = footprint.clone();
    let placed = match with_ipc(ctx.config.ipc_address.clone(), move |c| {
        c.ensure_board_is_active(&requested_board)?;
        let existing = c
            .list_footprints()?
            .into_iter()
            .map(|footprint| footprint.reference)
            .collect::<HashSet<_>>();
        let conflicts = planned
            .iter()
            .filter(|(reference, _, _, _)| existing.contains(reference))
            .map(|(reference, _, _, _)| reference.as_str())
            .collect::<Vec<_>>();
        if !conflicts.is_empty() {
            anyhow::bail!(
                "footprint references already exist on the board: {}",
                conflicts.join(", ")
            );
        }

        let items = planned
            .iter()
            .map(|(reference, pads, x, y)| {
                c.build_footprint_item(
                    &footprint_id,
                    reference,
                    &value,
                    pads,
                    &graphics,
                    &fields,
                    *x,
                    *y,
                    0.0,
                    "F.Cu",
                )
                .with_context(|| format!("failed to prepare {reference}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        c.run_commit("Place footprint array", |c| c.create_items(items))?;

        let mut created = c
            .list_footprints()?
            .into_iter()
            .map(|footprint| (footprint.reference.clone(), footprint))
            .collect::<HashMap<_, _>>();
        planned
            .into_iter()
            .map(|(reference, _, _, _)| {
                let footprint = created.remove(&reference).with_context(|| {
                    format!("committed footprint '{reference}' was not found on the board")
                })?;
                Ok(json!({
                    "reference": reference,
                    "x": footprint.position.x,
                    "y": footprint.position.y
                }))
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await?
    {
        Ok(placed) => placed,
        Err(error) => return Ok(CallToolResult::error(format!("IPC array error: {error:#}"))),
    };
    Ok(CallToolResult::json(
        &json!({ "placed_count": placed.len(), "components": placed }),
    ))
}

async fn handle_align_components(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let refs = match args["references"].as_array() {
        Some(references) if !references.is_empty() => references,
        _ => {
            return Ok(CallToolResult::error(
                "'references' must be a non-empty array.",
            ))
        }
    };
    let references = match refs
        .iter()
        .map(|reference| reference.as_str().map(String::from))
        .collect::<Option<Vec<_>>>()
    {
        Some(references) => references,
        None => return Ok(CallToolResult::error("Every reference must be a string.")),
    };
    let axis = args["axis"].as_str().unwrap_or("x").to_string();
    if axis != "x" && axis != "y" {
        return Ok(CallToolResult::error("'axis' must be either 'x' or 'y'."));
    }
    let value = match require_f64(args, "value") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let requested_board = board.clone();
    let aligned = match with_ipc(ctx.config.ipc_address.clone(), move |c| {
        c.ensure_board_is_active(&requested_board)?;
        c.run_commit("Align footprints", |c| {
            references
                .iter()
                .map(|reference| {
                    let footprint = c
                        .get_footprint(reference)?
                        .with_context(|| format!("footprint '{reference}' not found"))?;
                    let (x, y) = if axis == "y" {
                        (footprint.position.x, value)
                    } else {
                        (value, footprint.position.y)
                    };
                    c.move_footprint(reference, x, y)?;
                    Ok(json!({ "reference": reference, "x": x, "y": y }))
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
    })
    .await?
    {
        Ok(aligned) => aligned,
        Err(error) => return Ok(CallToolResult::error(format!("IPC align error: {error:#}"))),
    };
    Ok(CallToolResult::json(
        &json!({ "aligned_count": aligned.len(), "components": aligned }),
    ))
}

async fn handle_duplicate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
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
    let src = ipc!(ctx, args, |c| {
        c.get_footprint(&ref_ipc)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", ref_ipc))
    });
    if let Some(rejection) = back_side_layer_error(&src.layer) {
        return Ok(rejection);
    }
    let source = match resolve_footprint_source(&src.footprint, &board) {
        Ok(source) => source,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let prepared = match prepare_footprint_source(
        &source,
        &src.footprint,
        &new_reference,
        Some(&src.value),
        x,
        y,
        src.rotation,
        &src.layer,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let ipc_reference = new_reference.clone();
    let fp_id = src.footprint.clone();
    let fp_value = src.value.clone();
    let fp_layer = src.layer.clone();
    let fp_rotation = src.rotation;
    let pads = match extract_pad_definitions(&prepared) {
        Ok(pads) => pads,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let graphics = match extract_graphic_definitions(&prepared) {
        Ok(graphics) => graphics,
        Err(error) => return Ok(CallToolResult::error(error.to_string())),
    };
    let fields = extract_field_placement(&prepared);
    let dup_board = board.clone();
    let fp = ipc!(ctx, args, |c| c.place_footprint(
        &dup_board,
        &fp_id,
        &ipc_reference,
        &fp_value,
        &pads,
        &graphics,
        &fields,
        x,
        y,
        fp_rotation,
        &fp_layer
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
    let width = args["width"].as_u64().unwrap_or(800).clamp(100, 4000) as u32;
    let height = args["height"].as_u64().unwrap_or(600).clamp(100, 4000) as u32;

    let tmp = board_path.with_extension("render.png");
    super::cli::render_pcb_png(&ctx.config.kicad_cli, &board_path, &tmp, width, height).await?;
    let bytes = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;

    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(CallToolResult::image(b64, "image/png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOOTPRINT: &str = r#"(footprint "R_0402"
  (version 20240108)
  (generator pcbnew)
  (layer "F.Cu")
  (property "Reference" "REF**" (at 0 -1 0) (layer "F.SilkS"))
  (property "Value" "R_0402" (at 0 1 0) (layer "F.Fab"))
  (pad "1" smd roundrect (at -0.5 0) (size 0.5 0.5)
    (layers "F.Cu" "F.Paste" "F.Mask")))"#;

    #[test]
    fn prepares_complete_front_footprint() {
        let prepared = prepare_footprint_source(
            FOOTPRINT,
            "Resistor_SMD:R_0402",
            "R17",
            None,
            12.5,
            8.25,
            90.0,
            "F.Cu",
        )
        .unwrap();
        assert!(prepared.starts_with("(footprint \"Resistor_SMD:R_0402\""));
        assert!(prepared.contains("(property \"Reference\" \"R17\""));
        assert!(prepared.contains("(at 12.5 8.25 90)"));
        assert!(prepared.contains("(pad \"1\""));
        assert!(prepared.contains("(layers \"F.Cu\" \"F.Paste\" \"F.Mask\")"));
        let pads = extract_pad_definitions(&prepared).unwrap();
        assert_eq!(pads.len(), 1);
        assert_eq!(pads[0].number, "1");
        assert_eq!(pads[0].shape, "roundrect");
        assert_eq!(pads[0].layers, ["F.Cu", "F.Paste", "F.Mask"]);
    }

    #[test]
    fn back_side_placement_is_rejected_not_string_swapped() {
        // The old implementation did a blind "F. → "B. text swap over the whole
        // footprint, which corrupted property values starting with "F." and
        // left pad X positions unmirrored — wrong geometry presented as
        // success. Until a real mirror flip exists, B.Cu must be refused.
        let error = prepare_footprint_source(
            FOOTPRINT,
            "Resistor_SMD:R_0402",
            "R18",
            Some("10k"),
            1.0,
            2.0,
            0.0,
            "B.Cu",
        )
        .unwrap_err();
        assert!(error.to_string().contains("not yet supported"), "{error}");
    }

    #[test]
    fn rejects_non_outer_copper_layer() {
        let error = prepare_footprint_source(
            FOOTPRINT,
            "Resistor_SMD:R_0402",
            "R19",
            None,
            0.0,
            0.0,
            0.0,
            "In1.Cu",
        )
        .unwrap_err();
        assert!(error.to_string().contains("F.Cu"));
    }

    /// A footprint with the graphics KiCad's own libraries ship: courtyard
    /// rect, silkscreen lines, a fab outline and text, plus hidden built-in
    /// properties that must not be drawn.
    const GRAPHIC_FOOTPRINT: &str = r#"(footprint "R_0402"
  (version 20240108)
  (generator pcbnew)
  (layer "F.Cu")
  (property "Reference" "REF**" (at 0 -1 0) (layer "F.SilkS"))
  (property "Value" "R_0402" (at 0 1 0) (layer "F.Fab"))
  (property "Datasheet" "" (at 0 0 0) (layer "F.Fab") (hide yes))
  (fp_line (start -0.6 -0.5) (end 0.6 -0.5) (stroke (width 0.12) (type solid)) (layer "F.SilkS"))
  (fp_line (start -0.6 0.5) (end 0.6 0.5) (stroke (width 0.12) (type solid)) (layer "F.SilkS"))
  (fp_rect (start -0.8 -0.7) (end 0.8 0.7) (stroke (width 0.05) (type default)) (fill no) (layer "F.CrtYd"))
  (fp_circle (center 0 0) (end 0.25 0) (stroke (width 0.1) (type solid)) (fill yes) (layer "F.Fab"))
  (fp_arc (start -0.3 0) (mid 0 -0.3) (end 0.3 0) (stroke (width 0.12) (type solid)) (layer "F.SilkS"))
  (fp_poly (pts (xy -0.2 -0.2) (xy 0.2 -0.2) (xy 0.2 0.2)) (stroke (width 0.1) (type solid)) (fill yes) (layer "F.Fab"))
  (fp_text user "${REFERENCE}" (at 0 1.17 0) (layer "F.Fab") (effects (font (size 0.26 0.26) (thickness 0.04))))
  (fp_text user "secret" (at 0 0 0) (layer "F.Fab") (hide yes) (effects (font (size 0.26 0.26))))
  (pad "1" smd roundrect (at -0.5 0) (size 0.5 0.5)
    (layers "F.Cu" "F.Paste" "F.Mask")))"#;

    #[test]
    fn extracts_all_drawable_graphics_with_layers_and_widths() {
        use konnect_ipc::IpcGraphicDefinition as Graphic;
        let graphics = extract_graphic_definitions(GRAPHIC_FOOTPRINT).unwrap();

        let lines: Vec<_> = graphics
            .iter()
            .filter(|g| matches!(g, Graphic::Line { .. }))
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(matches!(
            lines[0],
            Graphic::Line { layer, width, start, .. }
                if layer == "F.SilkS" && *width == 0.12 && *start == (-0.6, -0.5)
        ));

        let rect = graphics
            .iter()
            .find(|g| matches!(g, Graphic::Rect { .. }))
            .unwrap();
        assert!(matches!(
            rect,
            Graphic::Rect { layer, width, filled, start, end }
                if layer == "F.CrtYd" && *width == 0.05 && !*filled
                    && *start == (-0.8, -0.7) && *end == (0.8, 0.7)
        ));

        let circle = graphics
            .iter()
            .find(|g| matches!(g, Graphic::Circle { .. }))
            .unwrap();
        assert!(matches!(
            circle,
            Graphic::Circle { layer, filled, end, .. }
                if layer == "F.Fab" && *filled && *end == (0.25, 0.0)
        ));

        assert!(graphics
            .iter()
            .any(|g| matches!(g, Graphic::Arc { layer, mid, .. }
                if layer == "F.SilkS" && *mid == (0.0, -0.3))));

        let poly = graphics
            .iter()
            .find(|g| matches!(g, Graphic::Poly { .. }))
            .unwrap();
        assert!(matches!(
            poly,
            Graphic::Poly { points, filled, .. } if points.len() == 3 && *filled
        ));

        // Exactly one visible text: the fab ${REFERENCE}. The hidden fp_text,
        // the hidden Datasheet property, and the Reference/Value properties
        // (carried as first-class fields) are all excluded.
        let texts: Vec<_> = graphics
            .iter()
            .filter(|g| matches!(g, Graphic::Text { .. }))
            .collect();
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(matches!(
            texts[0],
            Graphic::Text { text, layer, size, position, .. }
                if text == "${REFERENCE}" && layer == "F.Fab" && *size == 0.26
                    && *position == (0.0, 1.17)
        ));
    }

    #[test]
    fn a_bare_pads_only_footprint_extracts_no_graphics() {
        assert!(extract_graphic_definitions(FOOTPRINT).unwrap().is_empty());
    }

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                // No IPC address: any handler that reaches the IPC layer fails
                // with the socket-path configuration error, so a different
                // error proves the handler rejected before trying IPC.
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn result_text(res: &CallToolResult) -> String {
        match res.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    // ─── File-editing fallback (ported from emolitor's PR #66) ────────────────

    /// A library footprint in the exact shape KiCad ships: TAB-indented, name
    /// without a library prefix, `REF**` placeholder, no `(at …)`. CRLF, the
    /// way KiCad's bundled libraries are written.
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

    const EMPTY_BOARD: &str = "(kicad_pcb
\t(version 20260206)
\t(generator \"pcbnew\")
\t(net 0 \"\")
)
";

    /// A project directory holding a registered `Resistor_SMD.pretty` library
    /// with one footprint, plus an empty board. The project fp-lib-table makes
    /// `Resistor_SMD:R_0805_2012Metric` resolve hermetically — no global
    /// table, no environment.
    fn fallback_fixture(dir: &Path) -> std::path::PathBuf {
        let pretty = dir.join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        std::fs::write(
            pretty.join("R_0805_2012Metric.kicad_mod"),
            library_footprint(),
        )
        .unwrap();
        std::fs::write(
            dir.join("fp-lib-table"),
            format!(
                "(fp_lib_table\r\n\t(version 7)\r\n\t(lib (name \"Resistor_SMD\") (type \"KiCad\") (uri \"{}\") (options \"\") (descr \"\"))\r\n)\r\n",
                pretty.to_string_lossy()
            ),
        )
        .unwrap();
        let board = dir.join("b.kicad_pcb");
        std::fs::write(&board, EMPTY_BOARD).unwrap();
        board
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

    #[tokio::test]
    async fn unreachable_ipc_falls_back_to_writing_the_board_file() {
        // ipc_address is empty in test_ctx, which classifies as
        // transport-unreachable — the one condition under which editing the
        // board file directly cannot race a live editor.
        let tmp = tempfile::tempdir().unwrap();
        let board = fallback_fixture(tmp.path());

        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": "Resistor_SMD:R_0805_2012Metric",
            "reference": "R7",
            "x": 50.0, "y": 60.0,
        });
        let res = handle_place_component(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);

        let out: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();
        assert_eq!(out["source"], "file");
        assert_eq!(out["placed"], "R7");
        assert!(
            out["warning"]
                .as_str()
                .is_some_and(|w| w.contains("edited") && w.contains("loads")),
            "the fallback must warn that the file was edited directly: {out}"
        );

        let written = std::fs::read_to_string(&board).unwrap();
        assert_eq!(
            written.matches("(footprint \"").count(),
            1,
            "no footprint:\n{written}"
        );
        assert!(
            written.contains("(footprint \"Resistor_SMD:R_0805_2012Metric\""),
            "board should carry the Library:Footprint id:\n{written}"
        );
        assert!(
            written.contains("(at 50 60 0)"),
            "placement missing:\n{written}"
        );
        assert!(
            written.contains("(property \"Reference\" \"R7\""),
            "{written}"
        );
        assert!(
            written.contains("(pad \"1\" smd roundrect"),
            "the full definition must be carried:\n{written}"
        );
        assert!(written.contains("(uuid \""), "board items need a uuid");
        assert_eq!(
            count_parens(&written),
            0,
            "board is no longer balanced:\n{written}"
        );
    }

    #[tokio::test]
    async fn fallback_placement_rotation_reaches_the_pads() {
        // A rotated placement whose pads keep angle 0 trips KiCad's own
        // lib_footprint_mismatch check, so the rotation has to reach them.
        let tmp = tempfile::tempdir().unwrap();
        let board = fallback_fixture(tmp.path());
        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": "Resistor_SMD:R_0805_2012Metric",
            "reference": "R1", "x": 10.0, "y": 20.0, "rotation": -90.0,
        });
        let res = handle_place_component(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "{:?}", res.content);

        let out = std::fs::read_to_string(&board).unwrap();
        assert!(out.contains("(at 10 20 -90)"), "footprint angle:\n{out}");
        assert!(out.contains("(at -0.9125 0 270)"), "pad angle:\n{out}");
        assert!(
            out.contains("(at 0 -1.65 90)"),
            "readable text angle:\n{out}"
        );
    }

    #[tokio::test]
    async fn a_truncated_board_is_refused_rather_than_rewritten() {
        // rfind(')') picks the insert point, so a board that is not one closed
        // (kicad_pcb …) form would silently gain a footprint outside the root
        // expression. Nothing should be written in that case.
        let tmp = tempfile::tempdir().unwrap();
        let board = fallback_fixture(tmp.path());
        let truncated = "(kicad_pcb (version 20241229) (generator \"test\")";
        std::fs::write(&board, truncated).unwrap();

        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": "Resistor_SMD:R_0805_2012Metric",
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

    /// Force LF, whatever the checkout did to this source file's literals.
    fn lf(s: &str) -> String {
        s.replace("\r\n", "\n")
    }

    /// Force CRLF, likewise.
    fn crlf(s: &str) -> String {
        lf(s).replace('\n', "\r\n")
    }

    #[tokio::test]
    async fn a_crlf_board_stays_crlf() {
        // KiCad writes these files CRLF on Windows, so placing into a CRLF
        // board must not leave two conventions in it.
        let tmp = tempfile::tempdir().unwrap();
        let board = fallback_fixture(tmp.path());
        std::fs::write(&board, crlf(EMPTY_BOARD)).unwrap();

        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": "Resistor_SMD:R_0805_2012Metric",
            "reference": "R1", "x": 1.0, "y": 2.0,
        });
        let res = handle_place_component(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);

        let out = std::fs::read_to_string(&board).unwrap();
        assert!(
            out.contains("(pad \"1\" smd roundrect"),
            "footprint missing"
        );
        let bare_lf = out
            .match_indices('\n')
            .filter(|(i, _)| *i == 0 || out.as_bytes()[i - 1] != b'\r')
            .count();
        assert_eq!(
            bare_lf, 0,
            "a CRLF board gained {bare_lf} bare LF line endings:\n{out:?}"
        );
    }

    #[tokio::test]
    async fn an_lf_board_stays_lf() {
        // The reverse: a CRLF library footprint must not drag \r into an LF
        // board, which is the common case on Linux and macOS.
        let tmp = tempfile::tempdir().unwrap();
        let board = fallback_fixture(tmp.path());
        std::fs::write(&board, lf(EMPTY_BOARD)).unwrap();

        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": "Resistor_SMD:R_0805_2012Metric",
            "reference": "R1", "x": 1.0, "y": 2.0,
        });
        let res = handle_place_component(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);

        let out = std::fs::read_to_string(&board).unwrap();
        assert!(
            out.contains("(pad \"1\" smd roundrect"),
            "footprint missing"
        );
        assert!(
            !out.contains('\r'),
            "a CRLF library footprint dragged \\r into an LF board:\n{out:?}"
        );
    }

    /// A rep0 endpoint that completes every round-trip with an error status —
    /// a live KiCAD saying no. Placement must fail closed: error out, and
    /// leave the board file alone.
    fn spawn_rejecting_kicad() -> String {
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

    #[tokio::test]
    async fn a_reachable_kicad_that_rejects_never_touches_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let board = fallback_fixture(tmp.path());
        let board_before = std::fs::read_to_string(&board).unwrap();

        let ctx = ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: spawn_rejecting_kicad(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        );
        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": "Resistor_SMD:R_0805_2012Metric",
            "reference": "R1", "x": 1.0, "y": 2.0,
        });
        let res = handle_place_component(&args, &ctx).await.unwrap();
        assert!(res.is_error, "a rejection must not be reported as success");
        let text = result_text(&res);
        assert!(
            text.contains("rejected the placement") && text.contains("not modified"),
            "the error must say the file was left alone: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(&board).unwrap(),
            board_before,
            "a reachable KiCAD that says no must never trigger the file fallback"
        );
    }

    // ─── board_lib_id / helpers (ported from PR #66) ──────────────────────────

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
                "/nonexistent/kicad/footprints/Resistor_SMD.pretty/R_0805.kicad_mod",
                "R_0805"
            ),
            "Resistor_SMD:R_0805"
        );
        // Loose file: no nickname to recover, so it keeps the name the library
        // file declares — unlinked, but a valid name rather than a path.
        assert_eq!(
            id_for("/nonexistent/scratch/R_0805.kicad_mod", "R_0805_2012Metric"),
            "R_0805_2012Metric"
        );
    }

    #[test]
    fn a_path_like_declared_name_falls_back_to_the_file_stem() {
        // A malformed library file naming itself with a path must not smuggle
        // that path into the board through the fallback branch.
        assert_eq!(
            id_for(
                "/nonexistent/scratch/R_0805.kicad_mod",
                "/tmp/other/R.kicad_mod"
            ),
            "R_0805"
        );
        assert_eq!(
            id_for("/nonexistent/scratch/R_0805.kicad_mod", r"C:\x\R.kicad_mod"),
            "R_0805"
        );
        // An empty declared name is no better than a path.
        assert_eq!(
            id_for("/nonexistent/scratch/R_0805.kicad_mod", ""),
            "R_0805"
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
    fn text_angles_are_kept_readable_in_file_fallback() {
        // A -90 footprint would put text at 270, which reads upside down, so
        // KiCad flips it by 180 to 90 — matching what pcbnew writes.
        let out = apply_rotation_to_children(&library_footprint(), -90.0);
        assert!(out.contains("(at 0 -1.65 90)"), "reference text:\n{out}");
        assert!(out.contains("(at 0 1.65 90)"), "value text:\n{out}");
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
    fn indent_block_reimposes_one_line_ending() {
        // A CRLF library footprint going into an LF board and the reverse:
        // whichever the destination uses is what comes out.
        assert_eq!(indent_block("a\r\nb", "\t", "\n"), "\ta\n\tb");
        assert_eq!(indent_block("a\nb", "\t", "\r\n"), "\ta\r\n\tb");
    }

    #[test]
    fn sexp_strings_are_escaped_and_quoted() {
        // Input characters:  a " b \ c
        let input = ['a', '"', 'b', '\\', 'c'].iter().collect::<String>();
        let expected = ['"', 'a', '\\', '"', 'b', '\\', '\\', 'c', '"']
            .iter()
            .collect::<String>();
        assert_eq!(quote_sexp_string(&input), expected);
        assert_eq!(quote_sexp_string("plain"), "\"plain\"");
    }

    #[test]
    fn name_span_covers_the_quoted_header_name() {
        let content = library_footprint();
        let span = footprint_name_span(&content).expect("header not found");
        assert_eq!(&content[span], "\"R_0805_2012Metric\"");
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

    #[tokio::test]
    async fn place_component_rejects_back_copper_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let board = tmp.path().join("b.kicad_pcb");
        let board_content = "(kicad_pcb\n\t(version 20240108)\n\t(generator \"pcbnew\")\n)\n";
        std::fs::write(&board, board_content).unwrap();

        let args = json!({
            "board": board.to_string_lossy(),
            "footprint": "Resistor_SMD:R_0402",
            "reference": "R1",
            "x": 10.0, "y": 20.0,
            "layer": "B.Cu",
        });
        let res = handle_place_component(&args, &test_ctx()).await.unwrap();
        assert!(res.is_error, "B.Cu placement must be refused");
        assert_eq!(
            crate::mcp::error::extract_error_kind(&res).as_deref(),
            Some("invalid_argument"),
            "rejection should be a structured invalid_argument error"
        );
        let text = result_text(&res);
        assert!(
            text.contains("back-side placement is not yet supported"),
            "must say why: {text}"
        );
        assert!(
            text.contains("F.Cu") && text.contains("flip"),
            "must suggest the workaround: {text}"
        );
        // Rejection happens before any resolution, IPC round-trip, or file
        // write — the board is untouched and no IPC error ever surfaced.
        assert!(
            !text.contains("socket path not configured"),
            "the handler must not have reached the IPC layer: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(&board).unwrap(),
            board_content,
            "board file must be left untouched"
        );
    }
}

#[cfg(test)]
mod field_placement_tests {
    use super::*;

    #[test]
    fn field_anchors_come_from_the_library_footprint() {
        // R_0603-style: Reference above the silk at -1.43, Value below at 1.43.
        let source = "(footprint \"R_0603\"
	(property \"Reference\" \"REF**\"
		(at 0 -1.43 0)
		(layer \"F.SilkS\")
	)
	(property \"Value\" \"R_0603\"
		(at 0 1.43 0)
		(layer \"F.Fab\")
	)
)";
        let placement = extract_field_placement(source);
        assert_eq!(placement.reference_at, Some((0.0, -1.43, 0.0)));
        assert_eq!(placement.value_at, Some((0.0, 1.43, 0.0)));
    }

    #[test]
    fn missing_fields_leave_defaults() {
        let placement = extract_field_placement("(footprint \"bare\")");
        assert_eq!(placement.reference_at, None);
        assert_eq!(placement.value_at, None);
    }
}

#[cfg(test)]
mod pad_net_shape_tests {
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

    async fn pads_of(board: &str) -> serde_json::Value {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board).unwrap();
        let result = handle_get_component_pads(
            &json!({ "board": path.to_str().unwrap(), "reference": "R1" }),
            &test_ctx(),
        )
        .await
        .expect("handler should succeed");
        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        serde_json::from_str(&body).unwrap()
    }

    fn board_with_pads(pads: &str) -> String {
        format!(
            "(kicad_pcb\n\t(version 20260206)\n\t(footprint \"TestPad\"\n\t\t(layer \"F.Cu\")\n\t\t(at 100 100)\n\
             \t\t(property \"Reference\" \"R1\" (at 0 0 0) (layer \"F.SilkS\"))\n{pads}\t)\n)\n"
        )
    }

    /// The reported bug: KiCad 10 puts the name at index 1, the old reader
    /// took index 2, and `.unwrap_or("")` turned that miss into a plausible
    /// empty string — so a fully connected pad looked exactly like an
    /// unconnected one.
    #[tokio::test]
    async fn kicad_10_pads_report_their_net_names() {
        let pads = pads_of(&board_with_pads(
            "\t\t(pad \"1\" smd rect (at -1 0) (size 1 1) (net \"GND\"))\n\
             \t\t(pad \"2\" smd rect (at 1 0) (size 1 1) (net \"VCC\"))\n",
        ))
        .await;
        assert_eq!(pads["pads"][0]["net"], json!("GND"));
        assert_eq!(pads["pads"][1]["net"], json!("VCC"));
    }

    #[tokio::test]
    async fn legacy_pads_still_report_their_net_names() {
        let pads = pads_of(&board_with_pads(
            "\t\t(pad \"1\" smd rect (at -1 0) (size 1 1) (net 1 \"GND\"))\n\
             \t\t(pad \"2\" smd rect (at 1 0) (size 1 1) (net 2 \"VCC\"))\n",
        ))
        .await;
        assert_eq!(pads["pads"][0]["net"], json!("GND"));
        assert_eq!(pads["pads"][1]["net"], json!("VCC"));
    }

    /// A pad with no net node is genuinely unconnected, and "" says so. This
    /// is the one case where the empty string is the right answer, which is
    /// why the unreadable case must not share it.
    #[tokio::test]
    async fn a_pad_with_no_net_node_reports_an_empty_name() {
        let pads = pads_of(&board_with_pads(
            "\t\t(pad \"1\" smd rect (at -1 0) (size 1 1))\n",
        ))
        .await;
        assert_eq!(pads["pads"][0]["net"], json!(""));
    }

    /// Present but unreadable is now loud. A bare `(net 1)` reference names no
    /// net, and null forces a caller to notice rather than concluding the pad
    /// is floating.
    #[tokio::test]
    async fn a_net_node_we_cannot_read_reports_null_not_empty() {
        let pads = pads_of(&board_with_pads(
            "\t\t(pad \"1\" smd rect (at -1 0) (size 1 1) (net 1))\n\
             \t\t(pad \"2\" smd rect (at 1 0) (size 1 1) (net))\n",
        ))
        .await;
        assert_eq!(pads["pads"][0]["net"], serde_json::Value::Null);
        assert_eq!(pads["pads"][1]["net"], serde_json::Value::Null);
    }

    /// The unconnected pseudo-net is named, and its name is empty — that is a
    /// real answer, not a failure, so it must not become null.
    #[tokio::test]
    async fn the_unconnected_pseudo_net_reports_an_empty_name() {
        let pads = pads_of(&board_with_pads(
            "\t\t(pad \"1\" smd rect (at -1 0) (size 1 1) (net 0 \"\"))\n",
        ))
        .await;
        assert_eq!(pads["pads"][0]["net"], json!(""));
    }
}
