//! `pcb_board` toolset — board setup, layers, outlines, zones, and board-level items.
//!
//! Most operations use S-expression file manipulation so they work without a running
//! KiCad instance. `get_board_info` and `get_board_extents` try the IPC API first,
//! falling back to parsing the file, and report which they used as `source` —
//! the file is the last save, so it disagrees with the IPC-backed writers here
//! whenever KiCad holds unsaved edits.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, opt_str_list, require_f64, require_str, with_board_ipc_classified, ToolContext,
    ToolDef,
};
use konnect_ipc::builders;
use konnect_sexp::{
    parser::{parse_sexp, SexpNode},
    writer::{
        apply_edits, find_block_with_leading_whitespace, find_direct_child_blocks, new_uuid,
        write_atomic, SexpEdit,
    },
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

#[derive(Debug, Clone, PartialEq)]
enum OutlinePrimitive {
    Line {
        start: (f64, f64),
        end: (f64, f64),
    },
    Arc {
        start: (f64, f64),
        mid: (f64, f64),
        end: (f64, f64),
    },
}

fn push_outline_line(primitives: &mut Vec<OutlinePrimitive>, start: (f64, f64), end: (f64, f64)) {
    if (start.0 - end.0).abs() > f64::EPSILON || (start.1 - end.1).abs() > f64::EPSILON {
        primitives.push(OutlinePrimitive::Line { start, end });
    }
}

fn rounded_rectangle_outline(
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    radius: f64,
) -> Result<Vec<OutlinePrimitive>, (&'static str, String)> {
    if ![x1, y1, x2, y2, radius].into_iter().all(f64::is_finite) {
        return Err((
            "corner_radius",
            "coordinates and corner_radius must be finite".to_string(),
        ));
    }
    let (left, right) = (x1.min(x2), x1.max(x2));
    let (top, bottom) = (y1.min(y2), y1.max(y2));
    let width = right - left;
    let height = bottom - top;
    if width <= 0.0 {
        return Err(("x2", "must differ from x1".to_string()));
    }
    if height <= 0.0 {
        return Err(("y2", "must differ from y1".to_string()));
    }
    if radius < 0.0 {
        return Err(("corner_radius", "must be zero or greater".to_string()));
    }
    let maximum = width.min(height) / 2.0;
    if radius > maximum {
        return Err((
            "corner_radius",
            format!("{radius} mm exceeds half the shorter side ({maximum} mm)"),
        ));
    }

    if radius == 0.0 {
        return Ok(vec![
            OutlinePrimitive::Line {
                start: (left, top),
                end: (right, top),
            },
            OutlinePrimitive::Line {
                start: (right, top),
                end: (right, bottom),
            },
            OutlinePrimitive::Line {
                start: (right, bottom),
                end: (left, bottom),
            },
            OutlinePrimitive::Line {
                start: (left, bottom),
                end: (left, top),
            },
        ]);
    }

    let diagonal_offset = radius * (1.0 - std::f64::consts::FRAC_1_SQRT_2);
    let mut primitives = Vec::with_capacity(8);

    push_outline_line(&mut primitives, (left + radius, top), (right - radius, top));
    primitives.push(OutlinePrimitive::Arc {
        start: (right - radius, top),
        mid: (right - diagonal_offset, top + diagonal_offset),
        end: (right, top + radius),
    });
    push_outline_line(
        &mut primitives,
        (right, top + radius),
        (right, bottom - radius),
    );
    primitives.push(OutlinePrimitive::Arc {
        start: (right, bottom - radius),
        mid: (right - diagonal_offset, bottom - diagonal_offset),
        end: (right - radius, bottom),
    });
    push_outline_line(
        &mut primitives,
        (right - radius, bottom),
        (left + radius, bottom),
    );
    primitives.push(OutlinePrimitive::Arc {
        start: (left + radius, bottom),
        mid: (left + diagonal_offset, bottom - diagonal_offset),
        end: (left, bottom - radius),
    });
    push_outline_line(
        &mut primitives,
        (left, bottom - radius),
        (left, top + radius),
    );
    primitives.push(OutlinePrimitive::Arc {
        start: (left, top + radius),
        mid: (left + diagonal_offset, top + diagonal_offset),
        end: (left + radius, top),
    });
    Ok(primitives)
}

fn outline_items(primitives: &[OutlinePrimitive], width: f64) -> Vec<prost_types::Any> {
    primitives
        .iter()
        .map(|primitive| {
            let shape = match primitive {
                OutlinePrimitive::Line { start, end } => {
                    builders::board_segment("Edge.Cuts", width, start.0, start.1, end.0, end.1)
                }
                OutlinePrimitive::Arc { start, mid, end } => builders::board_arc(
                    "Edge.Cuts",
                    width,
                    start.0,
                    start.1,
                    mid.0,
                    mid.1,
                    end.0,
                    end.1,
                ),
            };
            builders::pack_any(&shape, "kiapi.board.types.BoardGraphicShape")
        })
        .collect()
}

/// What a board-mutating tool should do after its IPC attempt.
pub(crate) enum BoardWrite<T = ()> {
    /// KiCAD applied the change; report `"source": "ipc"`. Carries whatever the
    /// IPC call returned, for tools that echo it back — the placed footprint,
    /// for instance.
    Ipc(T),
    /// No live KiCad on this transport and this board was not observed live
    /// during the current server session; proceed with the S-expression path.
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
///   classification, never a text match. A KiCad that answers — even with an
///   error — fails closed. An unreachable transport permits the file path only
///   when this server has never observed the requested board live.
pub(crate) async fn attempt_ipc_write<T, F>(
    ctx: &ToolContext,
    board_path: &std::path::Path,
    what: &str,
    f: F,
) -> anyhow::Result<BoardWrite<T>>
where
    T: Send + 'static,
    F: FnOnce(&konnect_ipc::client::KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match with_board_ipc_classified(ctx, board_path, f).await? {
        Ok(value) => Ok(BoardWrite::Ipc(value)),
        Err(konnect_ipc::IpcFailure::Rejected(message)) => {
            Ok(BoardWrite::Refused(CallToolResult::error(format!(
                "KiCAD rejected the {what} over IPC: {message}. \
                 The board file was not modified — KiCAD is reachable and may hold this \
                 board open, so editing the file directly could be silently overwritten."
            ))))
        }
        Err(konnect_ipc::IpcFailure::Target { error, .. }) => Ok(BoardWrite::Refused(
            crate::tools::ipc_target_error_result(&error),
        )),
        Err(konnect_ipc::IpcFailure::Unreachable(_)) => {
            if let Some(refusal) = stale_board_lock_refusal(board_path) {
                Ok(BoardWrite::Refused(refusal))
            } else if ctx.board_session.was_observed_live(board_path) {
                Ok(BoardWrite::Refused(unsafe_file_fallback(board_path)))
            } else {
                Ok(BoardWrite::File)
            }
        }
    }
}

fn unsafe_file_fallback(board_path: &std::path::Path) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::UnsafeFileFallback {
            path: board_path.display().to_string(),
        },
        "Konnect previously reached KiCad with this board open, but IPC is now unreachable. \
         The saved board file may be older than unsaved editor state, so Konnect did not \
         modify it. Reopen or recover the board in KiCad, reconcile it, and save the \
         authoritative state. If KiCad was deliberately closed cleanly, restart Konnect \
         only after confirming that the saved file is authoritative.",
    )
}

/// A KiCad sibling lock is persistent evidence that the saved board may not be
/// authoritative even when this server has never reached the editor (for
/// example, after a server restart or editor crash). KiCad's lock contents do
/// not carry a trustworthy PID or freshness token, so presence and inspection
/// failure both fail closed.
fn stale_board_lock_refusal(board_path: &std::path::Path) -> Option<CallToolResult> {
    stale_board_lock_refusal_with(board_path, |path| std::fs::symlink_metadata(path))
}

fn stale_board_lock_refusal_with(
    board_path: &std::path::Path,
    inspect: impl FnOnce(&std::path::Path) -> std::io::Result<std::fs::Metadata>,
) -> Option<CallToolResult> {
    let lock_path = konnect_sexp::writer::kicad_editor_lock_path(board_path)?;
    let reason = match inspect(&lock_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Ok(_) => format!(
            "KiCad sibling lock '{}' is present; its contents cannot prove whether an editor \
             still owns newer in-memory state",
            lock_path.display()
        ),
        Err(error) => format!(
            "KiCad sibling lock '{}' could not be inspected ({error}); absence cannot be proven",
            lock_path.display()
        ),
    };
    Some(CallToolResult::error_kind(
        ToolErrorKind::StaleTarget {
            target: board_path.display().to_string(),
            reason: reason.clone(),
        },
        format!(
            "The saved board '{}' may be stale: {}. Konnect did not modify it. Close or recover \
             the board in KiCad, reconcile any unsaved work, save the authoritative state, and \
             retry only after the lock is gone.",
            board_path.display(),
            reason
        ),
    ))
}

/// Refuse a direct file edit when KiCAD is reachable AND holds this very
/// board open: pcbnew saves from its in-memory state, so the file edit would
/// be silently discarded on its next save — success reported, nothing kept
/// (#192). For tools with no IPC implementation this guard is the honest
/// alternative to [`attempt_ipc_write`]'s fallback. A reachable KiCAD holding
/// a *different* board (or none) does not interfere with this file. An
/// unreachable transport proceeds only when this server has never observed
/// the requested board live.
pub(crate) async fn refuse_if_board_open_in_kicad(
    ctx: &ToolContext,
    board_path: &std::path::Path,
    what: &str,
) -> anyhow::Result<Option<CallToolResult>> {
    match with_board_ipc_classified(ctx, board_path, |_| Ok(())).await? {
        Ok(()) => Ok(Some(CallToolResult::error(format!(
            "KiCAD currently holds this board open, and a {what} written to the file would \
             be discarded by KiCAD's next save. Close the board in KiCAD (or make the edit \
             there) and retry — this tool has no IPC path for a live board yet."
        )))),
        Err(konnect_ipc::IpcFailure::Target {
            error:
                konnect_ipc::BoardTargetError::NoOpenDocuments { .. }
                | konnect_ipc::BoardTargetError::WrongDocument { .. },
            ..
        })
        | Err(konnect_ipc::IpcFailure::Rejected(_)) => Ok(None),
        Err(konnect_ipc::IpcFailure::Target { error, .. }) => {
            Ok(Some(crate::tools::ipc_target_error_result(&error)))
        }
        Err(konnect_ipc::IpcFailure::Unreachable(_)) => Ok(stale_board_lock_refusal(board_path)
            .or_else(|| {
                ctx.board_session
                    .was_observed_live(board_path)
                    .then(|| unsafe_file_fallback(board_path))
            })),
    }
}

// ─── Zone construction, shared by `add_zone` and its `add_copper_pour` alias ──

/// KiCad's own defaults for a freshly drawn zone. Shared by both tools so the
/// two cannot hand out different copper for the same request — they used to
/// disagree on `min_width` (0.2 vs 0.25).
pub(crate) const DEFAULT_ZONE_CLEARANCE_MM: f64 = 0.2;
pub(crate) const DEFAULT_ZONE_MIN_WIDTH_MM: f64 = 0.2;

/// What the caller gets back when no live KiCAD could be reached and the zone
/// went into the file instead.
pub(crate) const FILE_FALLBACK_WARNING: &str =
    "KiCad IPC was unreachable and this board has not been observed live during the current \
     Konnect server session, and no KiCad sibling lock was present, so Konnect edited the saved \
     board file directly. If KiCad crashed or was force-quit before this server started without \
     leaving a lock, reconcile any unsaved work before relying on this change.";

/// The `pad_connection` argument in both the representations it needs: the IPC
/// enum and the token KiCad's `(connect_pads …)` takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PadConnection {
    Solid,
    Thermal,
    None,
}

impl PadConnection {
    /// `thermal` is KiCad's default for a new zone, so it is ours.
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("thermal") {
            "solid" => Ok(Self::Solid),
            "thermal" => Ok(Self::Thermal),
            "none" => Ok(Self::None),
            other => Err(format!(
                "Unknown pad_connection '{other}'. Expected 'solid', 'thermal' or 'none'."
            )),
        }
    }

    fn as_ipc(self) -> konnect_ipc::gen::kiapi::board::types::ZoneConnectionStyle {
        use konnect_ipc::gen::kiapi::board::types::ZoneConnectionStyle;
        match self {
            Self::Solid => ZoneConnectionStyle::ZcsFull,
            Self::Thermal => ZoneConnectionStyle::ZcsThermal,
            Self::None => ZoneConnectionStyle::ZcsNone,
        }
    }

    /// The token between `connect_pads` and its `(clearance …)`. Thermal is
    /// spelled by writing nothing at all — that is how the file format says
    /// "the default".
    fn sexp_token(self) -> &'static str {
        match self {
            Self::Solid => " yes",
            Self::Thermal => "",
            Self::None => " no",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Solid => "solid",
            Self::Thermal => "thermal",
            Self::None => "none",
        }
    }
}

/// The JSON schema both zone tools advertise. One definition so `add_zone` and
/// `add_copper_pour` cannot drift apart again.
pub(crate) fn zone_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "board":      { "type": "string", "description": "Path to the .kicad_pcb file" },
            "net_name":   { "type": "string", "description": "Net name (e.g. 'GND')" },
            "layer":      { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
            "points": {
                "type": "array",
                "description": "Polygon vertices as [{x, y}], in mm",
                "items": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } } }
            },
            "clearance":  { "type": "number", "description": "Zone clearance in mm", "default": DEFAULT_ZONE_CLEARANCE_MM },
            "min_width":  { "type": "number", "description": "Minimum fill width in mm", "default": DEFAULT_ZONE_MIN_WIDTH_MM },
            "name":       { "type": "string", "description": "Optional zone name, as shown in pcbnew's zone properties" },
            "priority":   { "type": "integer", "description": "Higher priority wins where zones overlap", "default": 0, "minimum": 0 },
            "pad_connection": {
                "type": "string",
                "description": "How the pour attaches to pads on its net",
                "enum": ["solid", "thermal", "none"],
                "default": "thermal"
            }
        },
        "required": ["board", "net_name", "layer", "points"]
    })
}

// ─── Board graphics ───────────────────────────────────────────────────────────

/// The graphic kinds `delete_graphics` names, in the vocabulary both the live
/// and the file reader answer in.
///
/// `shape` is the live path's fallback for a shape KiCad sends with no
/// geometry (`konnect_ipc::client::shape_kind_and_origin`); it is in the list
/// so that everything the tool can *report* is also something a `types` filter
/// can *select* — otherwise such an item would be undeletable by kind.
const GRAPHIC_KINDS: [&str; 10] = [
    "line",
    "rect",
    "arc",
    "circle",
    "poly",
    "curve",
    "shape",
    "text",
    "textbox",
    "dimension",
];

/// The kind name for a board file's top-level graphic block, or `None` for
/// anything that is not a graphic — footprints, zones, tracks, the setup
/// block. `(image …)` is deliberately absent: KiCad 10's `ReferenceImage`
/// message is an empty placeholder, so the live path cannot see one and
/// deleting it from the file only would make the two paths disagree.
fn graphic_kind(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "gr_line" => "line",
        "gr_rect" => "rect",
        "gr_arc" => "arc",
        "gr_circle" => "circle",
        "gr_poly" => "poly",
        "gr_curve" => "curve",
        "gr_text" => "text",
        "gr_text_box" => "textbox",
        "dimension" => "dimension",
        _ => return None,
    })
}

/// The head tag of a `(tag …)` block, without parsing it.
fn block_tag(block: &str) -> Option<&str> {
    let after_paren = block.strip_prefix('(')?;
    let end = after_paren
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(after_paren.len());
    Some(&after_paren[..end])
}

/// A graphic read out of a board file, with the byte range to cut to delete it.
struct FileGraphic {
    uuid: String,
    kind: &'static str,
    layer: String,
    origin: Option<(f64, f64)>,
    /// Byte range of the block including its leading whitespace, so deleting
    /// it leaves no blank line behind.
    span: (usize, usize),
}

/// The first defining point of a graphic block: a segment's `start`, a
/// circle's `center`, a text's `at`, or a polygon's first vertex.
fn block_origin(node: &SexpNode) -> Option<(f64, f64)> {
    for tag in ["start", "center", "at"] {
        if let Some(point) = node.find(tag) {
            if let (Some(x), Some(y)) = (point.get_f64(1), point.get_f64(2)) {
                return Some((x, y));
            }
        }
    }
    let xy = node.find("pts")?.find("xy")?;
    Some((xy.get_f64(1)?, xy.get_f64(2)?))
}

/// Every top-level graphic in a board file, in file order.
///
/// Only direct children of `(kicad_pcb …)` count: a `gr_line` inside a
/// footprint belongs to that footprint, and this tool must never cut one out.
fn read_file_graphics(content: &str) -> Vec<FileGraphic> {
    find_direct_child_blocks(content, "kicad_pcb")
        .into_iter()
        .filter_map(|(start, end)| {
            // Read the head tag out of the slice before parsing: a board's
            // top-level blocks are overwhelmingly footprints, segments, vias,
            // and zones, and building an AST for each of those just to throw
            // it away parses most of the file for nothing.
            let kind = graphic_kind(block_tag(&content[start..end])?)?;
            let node = parse_sexp(&content[start..end]).ok()?;
            Some(FileGraphic {
                uuid: node.find_str("uuid").unwrap_or_default().to_string(),
                kind,
                layer: node.find_str("layer").unwrap_or_default().to_string(),
                origin: block_origin(&node),
                span: find_block_with_leading_whitespace(content, start).unwrap_or((start, end)),
            })
        })
        .collect()
}

/// The `delete_graphics` filter: a graphic matches when it satisfies every
/// filter given (unset filters match everything).
#[derive(Clone)]
struct GraphicFilter {
    uuids: Option<Vec<String>>,
    kinds: Option<Vec<String>>,
    layer: Option<String>,
}

impl GraphicFilter {
    fn matches(&self, uuid: &str, kind: &str, layer: &str) -> bool {
        self.uuids
            .as_ref()
            .is_none_or(|wanted| wanted.iter().any(|w| w == uuid))
            && self
                .kinds
                .as_ref()
                .is_none_or(|wanted| wanted.iter().any(|w| w == kind))
            && self.layer.as_ref().is_none_or(|wanted| wanted == layer)
    }

    fn is_empty(&self) -> bool {
        self.uuids.is_none() && self.kinds.is_none() && self.layer.is_none()
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

fn format_gr_arc(
    start: (f64, f64),
    mid: (f64, f64),
    end: (f64, f64),
    layer: &str,
    width: f64,
) -> String {
    let uuid = new_uuid();
    format!(
        "\n  (gr_arc\n    (start {} {})\n    (mid {} {})\n    (end {} {})\n    \
         (stroke (width {width}) (type solid))\n    (layer \"{layer}\")\n    (uuid \"{uuid}\")\n  )",
        start.0, start.1, mid.0, mid.1, end.0, end.1
    )
}

fn format_outline(primitives: &[OutlinePrimitive], layer: &str, width: f64) -> String {
    primitives
        .iter()
        .map(|primitive| match primitive {
            OutlinePrimitive::Line { start, end } => {
                format_gr_line(start.0, start.1, end.0, end.1, layer, width)
            }
            OutlinePrimitive::Arc { start, mid, end } => {
                format_gr_arc(*start, *mid, *end, layer, width)
            }
        })
        .collect::<Vec<_>>()
        .join("")
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
///
/// Only the fallback path writes this; the live-KiCad path goes through
/// [`konnect_ipc::builders::build_zone`], and the two are kept in step by
/// sharing the thermal-relief and hatch-pitch constants.
#[allow(clippy::too_many_arguments)]
fn format_zone_polygon(
    net: &konnect_sexp::net::NetRef,
    layer: &str,
    clearance: f64,
    min_width: f64,
    points: &[(f64, f64)],
    name: &str,
    priority: u32,
    connection: PadConnection,
) -> String {
    let uuid = new_uuid();
    let pts: String = points
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    // KiCad omits both of these at their defaults, and so do we: an explicit
    // `(priority 0)` is legal but is not what pcbnew writes.
    let name_node = if name.is_empty() {
        String::new()
    } else {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\n    (name \"{escaped}\")")
    };
    let priority_node = if priority == 0 {
        String::new()
    } else {
        format!("\n    (priority {priority})")
    };
    let connect = connection.sexp_token();
    let thermal = konnect_ipc::builders::ZONE_THERMAL_RELIEF_MM;
    let hatch_pitch = konnect_ipc::builders::ZONE_BORDER_PITCH_MM;
    format!(
        "\n  (zone {net_nodes} {layer_node}{name_node} (uuid \"{uuid}\"){priority_node}\n    \
         (hatch edge {hatch_pitch})\n    (connect_pads{connect} (clearance {clearance}))\n    \
         (min_thickness {min_width})\n    \
         (fill yes (thermal_gap {thermal}) (thermal_bridge_width {thermal}))\n    \
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
            "Add a rectangular board outline of the given dimensions on the Edge.Cuts layer. \
             This appends: on a board that already has an outline it leaves two overlapping \
             rectangles and a DRC failure, so resizing means calling \
             delete_graphics(layer='Edge.Cuts') first.",
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
        )
        .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
        tool!(
            "get_board_info",
            "Return metadata about the PCB: title, revision, company, layer count, paper size, \
             and the number of distinct nets (excluding the unconnected pseudo-net). \
             Reads the board open in KiCad when it is reachable, else the file — \
             'source' says which. Paper size always comes from the file, and is null \
             when the file cannot be read. A custom (User) size also reports its \
             dimensions in millimetres under 'paper_size_mm' on both paths.",
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
        )
        .with_board_access(crate::tools::BoardAccess::ClosedBoardOnly),
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
        )
        .with_board_access(crate::tools::BoardAccess::ClosedBoardOnly),
        tool!(
            "add_board_outline",
            "Add a rectangular board outline on Edge.Cuts, optionally using circular rounded \
             corners. This appends: on a board that already has an outline it leaves two \
             overlapping rectangles and a DRC failure, so replacing one means calling \
             delete_graphics(layer='Edge.Cuts') first.",
            json!({
                "type": "object",
                "properties": {
                    "board":          { "type": "string" },
                    "x1":             { "type": "number", "description": "Top-left X in mm" },
                    "y1":             { "type": "number", "description": "Top-left Y in mm" },
                    "x2":             { "type": "number", "description": "Bottom-right X in mm" },
                    "y2":             { "type": "number", "description": "Bottom-right Y in mm" },
                    "corner_radius":  { "type": "number", "minimum": 0, "description": "Circular corner radius in mm; must not exceed half the shorter side (0 = sharp)", "default": 0 }
                },
                "required": ["board", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_add_board_outline(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
        tool!(
            "delete_graphics",
            "Delete board graphics — lines, rectangles, arcs, circles, polygons, curves, text, \
             text boxes, and dimensions — that match every filter given. At least one of \
             'uuids', 'layer', or 'types' is required; 'dry_run' lists what would go without \
             deleting anything, and the UUIDs it reports can be passed straight back as 'uuids'. \
             Footprints, zones, tracks, vias, and graphics belonging to a footprint are never \
             touched, and neither are reference images (KiCad's API cannot identify one). \
             This is how a board outline is resized: add_board_outline and set_board_size \
             append, so calling one twice without deleting the old Edge.Cuts graphics first \
             leaves two overlapping outlines. Acts on the board open in KiCad when it is \
             reachable, else on the file — 'source' says which.",
            json!({
                "type": "object",
                "properties": {
                    "board":   { "type": "string", "description": "Path to .kicad_pcb file" },
                    "layer":   { "type": "string", "description": "Only graphics on this layer (e.g. 'Edge.Cuts')" },
                    "uuids": {
                        "type": "array",
                        "description": "Only graphics with these UUIDs",
                        "items": { "type": "string" }
                    },
                    "types": {
                        "type": "array",
                        "description": "Only graphics of these kinds",
                        "items": { "type": "string", "enum": GRAPHIC_KINDS }
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "List the matches without deleting them",
                        "default": false
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_delete_graphics(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
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
        )
        .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
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
        )
        .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
        tool!(
            "add_zone",
            "Add a copper fill zone polygon on a specified layer and net. Tries KiCAD IPC \
             first — with KiCAD live on this board the zone is created through the API and \
             refilled, so it shows up at once and is undoable there. Only when no live KiCAD \
             answers does it fall back to inserting the (zone …) S-expression into the file, \
             and that result carries a warning, because a file-only edit is invisible to an \
             open pcbnew and is lost on its next save. Refuses a net the board does not \
             declare rather than binding copper to net 0.",
            zone_schema(),
            |args, ctx| async move { handle_add_zone(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
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
        )
        .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// What replacing the board outline over IPC found and did.
enum OutlineIpcOutcome {
    /// Outline replaced; carries how many old Edge.Cuts segments were removed.
    Replaced(usize),
    /// The existing outline holds something a rectangle cannot honestly
    /// replace (an arc, polygon, rounded rect…). Nothing was written.
    NotPlainSegments(String),
}

/// Partition a live board's `KotPcbShape` items into Edge.Cuts segment KIIDs
/// and the kinds of any non-segment Edge.Cuts shapes.
///
/// Pure so it can be tested without a live KiCAD; the decode is guarded by the
/// type URL per the #244 rule — a pad-shaped `Any` must not be mistaken for a
/// shape.
fn partition_edge_cuts_shapes(items: &[prost_types::Any]) -> (Vec<String>, Vec<&'static str>) {
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    let mut segment_ids = Vec::new();
    let mut other_kinds = Vec::new();
    for item in items {
        if !builders::any_is(item, "kiapi.board.types.BoardGraphicShape") {
            continue;
        }
        let Ok(shape) = kiapi::board::types::BoardGraphicShape::decode(item.value.as_slice())
        else {
            continue;
        };
        if shape.layer != kiapi::board::types::BoardLayer::BlEdgeCuts as i32 {
            continue;
        }
        use kiapi::common::types::graphic_shape::Geometry;
        match shape.shape.as_ref().and_then(|s| s.geometry.as_ref()) {
            Some(Geometry::Segment(_)) => {
                if let Some(id) = shape.id.as_ref().filter(|id| !id.value.is_empty()) {
                    segment_ids.push(id.value.clone());
                }
            }
            Some(Geometry::Rectangle(_)) => other_kinds.push("rectangle"),
            Some(Geometry::Arc(_)) => other_kinds.push("arc"),
            Some(Geometry::Circle(_)) => other_kinds.push("circle"),
            Some(Geometry::Polygon(_)) => other_kinds.push("polygon"),
            Some(Geometry::Bezier(_)) => other_kinds.push("bezier"),
            None => other_kinds.push("unknown shape"),
        }
    }
    (segment_ids, other_kinds)
}

/// The refusal both write paths share when the outline is not plain segments.
fn outline_not_replaceable(kinds: &str) -> CallToolResult {
    CallToolResult::error(format!(
        "The existing board outline includes {kinds} on Edge.Cuts, which a plain \
         rectangle cannot honestly replace — Konnect refused before writing \
         anything. Edit the outline in pcbnew, or delete the old outline first \
         if the rectangle is really what you want."
    ))
}

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

    // Try IPC first (live board in KiCAD, undo-aware); fall through to file
    // edit. Both paths REPLACE the existing outline: since the first release
    // this tool only ever appended, so every resize added a second rectangle
    // to Edge.Cuts and the board failed DRC with a self-intersecting outline
    // while the tool reported success (#314).
    let items = rect_outline_items(ox, oy, x2, y2, w);
    match attempt_ipc_write(ctx, &board_path, "board size", move |c| {
        let existing =
            c.get_items(konnect_ipc::gen::kiapi::common::types::KiCadObjectType::KotPcbShape)?;
        let (segment_ids, other_kinds) = partition_edge_cuts_shapes(&existing);
        if !other_kinds.is_empty() {
            return Ok(OutlineIpcOutcome::NotPlainSegments(other_kinds.join(", ")));
        }
        let removed = segment_ids.len();
        c.run_commit("Set board size", |c| {
            c.delete_items(segment_ids.clone())?;
            c.create_items(items.clone()).map(|_| ())
        })?;
        Ok(OutlineIpcOutcome::Replaced(removed))
    })
    .await?
    {
        BoardWrite::Ipc(OutlineIpcOutcome::Replaced(removed)) => {
            return Ok(CallToolResult::json(&json!({
                "width": width, "height": height,
                "x1": ox, "y1": oy, "x2": x2, "y2": y2,
                "replaced_segments": removed,
                "source": "ipc"
            })))
        }
        // The refusal is Konnect's, not KiCAD's — do not route it through the
        // Refused arm, whose wording attributes the rejection to KiCAD (#230).
        BoardWrite::Ipc(OutlineIpcOutcome::NotPlainSegments(kinds)) => {
            return Ok(outline_not_replaceable(&kinds))
        }
        BoardWrite::Refused(err) => return Ok(err),
        BoardWrite::File => {}
    }

    let content = std::fs::read_to_string(&board_path)?;

    // Locate every existing top-level Edge.Cuts graphic. Plain `gr_line`s are
    // replaced; anything else refuses, because silently deleting an arc or
    // polygon outline would be guessing at design intent.
    let mut edits = Vec::new();
    let mut removed = 0usize;
    let mut other_kinds: Vec<String> = Vec::new();
    for (start, end) in find_direct_child_blocks(&content, "kicad_pcb") {
        let block = &content[start..end];
        let tag = block
            .trim_start_matches('(')
            .split_whitespace()
            .next()
            .unwrap_or("");
        if !tag.starts_with("gr_") || !block.contains("\"Edge.Cuts\"") {
            continue;
        }
        if tag == "gr_line" {
            let (ws_start, _) =
                find_block_with_leading_whitespace(&content, start).unwrap_or((start, end));
            edits.push(SexpEdit::replace(ws_start, end, String::new()));
            removed += 1;
        } else {
            other_kinds.push(tag.trim_start_matches("gr_").to_string());
        }
    }
    if !other_kinds.is_empty() {
        return Ok(outline_not_replaceable(&other_kinds.join(", ")));
    }

    let lines = format!(
        "{}{}{}{}",
        format_gr_line(ox, oy, x2, oy, "Edge.Cuts", w),
        format_gr_line(x2, oy, x2, y2, "Edge.Cuts", w),
        format_gr_line(x2, y2, ox, y2, "Edge.Cuts", w),
        format_gr_line(ox, y2, ox, oy, "Edge.Cuts", w),
    );
    let close_pos = content.rfind(')').unwrap_or(content.len());
    edits.push(SexpEdit::insert(close_pos, lines));
    let new_content = apply_edits(content, edits);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "width": width, "height": height,
        "x1": ox, "y1": oy, "x2": x2, "y2": y2,
        "replaced_segments": removed,
        "source": "file",
        "warning": FILE_FALLBACK_WARNING
    })))
}

/// The board file, read and parsed — `None` when either fails. The paper
/// helpers take an already-parsed tree so every field of one
/// `get_board_info` answer comes from a single parse of the file.
///
/// The two failure modes are not the same answer: a board that parses and
/// carries no `(paper …)` really is A4, which is KiCad's default, while a board
/// we could not read tells us nothing. Returning `"A4"` for the second is the
/// "plausible answer rather than a failure" this change refuses elsewhere, so
/// `paper` reports `null` instead — and `paper_size_mm` with it.
fn parsed_board(board_path: &std::path::Path) -> Option<SexpNode> {
    let content = std::fs::read_to_string(board_path).ok()?;
    parse_sexp(&content).ok()
}

/// The paper size named in an already-parsed board tree — `"A4"` when there is
/// no `(paper …)` at all, which is KiCad's default.
fn paper_name(tree: &SexpNode) -> String {
    tree.find("paper")
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("A4")
        .to_string()
}

/// The page dimensions in millimetres from an already-parsed tree, for sizes
/// whose name does not imply them (`(paper "User" 431.8 279.4)`). `None` for
/// named sizes and for an absent `(paper …)` — a named size is its own answer,
/// and a missing node has nothing to measure. Taking the tree rather than the
/// path keeps this on the caller's parse: the file branch of
/// `handle_get_board_info` already holds one, and re-reading here could race
/// the read that produced it.
fn paper_size_mm(tree: &SexpNode) -> Option<(f64, f64)> {
    let paper = tree.find("paper")?;
    Some((paper.get_f64(2)?, paper.get_f64(3)?))
}

async fn handle_get_board_info(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;

    // The board open in KiCad first (#207). Reading only the file reported the
    // state of the last save — on a board with unsaved edits it disagreed with
    // the IPC-backed writers in this toolset, most visibly as layer_count 0 /
    // net_count 0 on a board KiCad was showing fully populated.
    let ipc_board = board_path.clone();
    if let Ok((title_block, enabled, nets)) =
        with_board_ipc_classified(ctx, &board_path, move |c| {
            let document = c.find_open_board(&ipc_board)?;
            Ok((
                c.get_title_block_in(document.clone())?,
                c.get_enabled_layers_in(document.clone())?,
                // Net 0 is KiCad's unconnected pseudo-net and GetNets returns it.
                // The tool description promises a count without it and the file
                // path already excludes it (konnect_sexp::net::count_distinct_nets),
                // so a board with nothing wired must not read as one net.
                c.get_nets_in(document)?
                    .iter()
                    .filter(|net| net.netcode != 0)
                    .count(),
            ))
        })
        .await?
    {
        // Paper always comes from the file — KiCad's API exposes no page
        // settings — through the same parsed-tree helpers the file path uses,
        // so a custom User size reports its dimensions on both paths (#219).
        let paper_tree = parsed_board(&board_path);
        return Ok(CallToolResult::json(&json!({
            "file": board_path.display().to_string(),
            "title": title_block.title,
            "date": title_block.date,
            "revision": title_block.revision,
            "company": title_block.company,
            "paper": paper_tree.as_ref().map(paper_name),
            "paper_size_mm": paper_tree
                .as_ref()
                .and_then(paper_size_mm)
                .map(|(w, h)| json!({"width": w, "height": h})),
            "layer_count": enabled.layers.len(),
            "copper_layer_count": enabled.copper_layer_count,
            "net_count": nets,
            "source": "ipc"
        })));
    }

    // One read+parse feeds every field of the file answer. An unreadable or
    // unparseable file is a hard error here: unlike the paper fields, which
    // report null rather than guess A4 (see parsed_board), the rest of the
    // response has no honest answer at all.
    let tree = match parsed_board(&board_path) {
        Some(tree) => tree,
        None => anyhow::bail!("could not read or parse {}", board_path.display()),
    };

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
    let paper = paper_name(&tree);

    // Not find_all("net"): that counts only direct children of (kicad_pcb …),
    // i.e. the top-level net table — which KiCad 10 does not write at all, so
    // every KiCad 10 board reported 0. Collect from wherever the nets actually
    // are and de-duplicate; see konnect_sexp::net.
    let net_count = konnect_sexp::net::count_distinct_nets(&tree);

    Ok(CallToolResult::json(&json!({
        "file": board_path.display().to_string(),
        "title": title, "date": date, "revision": rev, "company": company,
        "paper": paper,
        "paper_size_mm": paper_size_mm(&tree)
            .map(|(w, h)| json!({"width": w, "height": h})),
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
    if let Ok(ext) = with_board_ipc_classified(ctx, &board_path, move |c| {
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
    let corner_radius = match args.get("corner_radius") {
        None | Some(serde_json::Value::Null) => 0.0,
        Some(value) => match value.as_f64() {
            Some(radius) => radius,
            None => {
                return Ok(CallToolResult::error_kind(
                    crate::mcp::error::ToolErrorKind::InvalidArgument {
                        field: "corner_radius".to_string(),
                        reason: "must be a number".to_string(),
                    },
                    "Argument 'corner_radius' must be a number",
                ));
            }
        },
    };
    let primitives = match rounded_rectangle_outline(x1, y1, x2, y2, corner_radius) {
        Ok(primitives) => primitives,
        Err((field, reason)) => {
            return Ok(CallToolResult::error_kind(
                crate::mcp::error::ToolErrorKind::InvalidArgument {
                    field: field.to_string(),
                    reason: reason.clone(),
                },
                format!("Argument '{field}' is invalid: {reason}"),
            ));
        }
    };
    let w = 0.05_f64;
    let line_count = primitives
        .iter()
        .filter(|primitive| matches!(primitive, OutlinePrimitive::Line { .. }))
        .count();
    let arc_count = primitives.len() - line_count;

    let items = outline_items(&primitives, w);
    match attempt_ipc_write(ctx, &board_path, "board outline", move |c| {
        c.create_items(items).map(|_| ())
    })
    .await?
    {
        BoardWrite::Ipc(()) => {
            return Ok(CallToolResult::json(&json!({
                "x1": x1, "y1": y1, "x2": x2, "y2": y2,
                "width": (x2-x1).abs(), "height": (y2-y1).abs(),
                "corner_radius": corner_radius,
                "line_count": line_count, "arc_count": arc_count,
                "source": "ipc"
            })))
        }
        BoardWrite::Refused(err) => return Ok(err),
        BoardWrite::File => {}
    }

    let outline = format_outline(&primitives, "Edge.Cuts", w);

    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, outline)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "x1": x1, "y1": y1, "x2": x2, "y2": y2,
        "width": (x2-x1).abs(), "height": (y2-y1).abs(),
        "corner_radius": corner_radius,
        "line_count": line_count, "arc_count": arc_count,
        "source": "file",
        "warning": FILE_FALLBACK_WARNING
    })))
}

fn graphic_json(
    uuid: &str,
    kind: &str,
    layer: &str,
    origin: Option<(f64, f64)>,
) -> serde_json::Value {
    json!({
        "uuid": uuid,
        "type": kind,
        "layer": layer,
        "x": origin.map(|(x, _)| x),
        "y": origin.map(|(_, y)| y),
    })
}

async fn handle_delete_graphics(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let uuids = match opt_str_list(args, "uuids") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let kinds = match opt_str_list(args, "types") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().map(str::to_string);
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);

    if let Some(unknown) = kinds
        .iter()
        .flatten()
        .find(|kind| !GRAPHIC_KINDS.contains(&kind.as_str()))
    {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "types".to_string(),
                reason: format!("unknown graphic type '{unknown}'"),
            },
            format!(
                "Unknown graphic type '{unknown}'. Valid types: {}.",
                GRAPHIC_KINDS.join(", ")
            ),
        ));
    }

    let filter = GraphicFilter {
        uuids,
        kinds,
        layer,
    };
    // An unfiltered call would wipe every graphic on the board. That is never
    // what a caller means by omitting the arguments, so it has to be spelled
    // out — layer by layer, or by UUID.
    if filter.is_empty() {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "filter".to_string(),
                reason: "at least one of 'uuids', 'layer', or 'types' is required".to_string(),
            },
            "delete_graphics needs a filter: pass 'layer' (e.g. 'Edge.Cuts'), 'uuids', \
             or 'types'. Run it with dry_run to see what a filter would match."
                .to_string(),
        ));
    }

    // The board KiCad holds first. The fallback gate is the typed transport
    // classification plus session memory: file mode is available only when
    // this server has never observed the requested board live.
    let ipc_board = board_path.clone();
    let ipc_filter = filter.clone();
    let attempt = attempt_ipc_write(ctx, &board_path, "graphic deletion", move |c| {
        let document = c.find_open_board(&ipc_board)?;
        let matched: Vec<konnect_ipc::IpcGraphic> = c
            .get_board_graphics_in(document.clone())?
            .into_iter()
            .filter(|g| ipc_filter.matches(&g.uuid, &g.kind, &g.layer))
            .collect();
        if !dry_run {
            if let Some(anonymous) = matched.iter().find(|g| g.uuid.is_empty()) {
                anyhow::bail!(
                    "KiCad returned a {} on {} with no identifier, so it cannot be deleted; \
                     nothing was deleted",
                    anonymous.kind,
                    anonymous.layer
                );
            }
            c.delete_items_in(document, matched.iter().map(|g| g.uuid.clone()).collect())?;
        }
        Ok(matched)
    })
    .await?;

    let (graphics, source) = match attempt {
        BoardWrite::Ipc(matched) => (
            matched
                .iter()
                .map(|g| {
                    graphic_json(
                        &g.uuid,
                        &g.kind,
                        &g.layer,
                        g.origin.as_ref().map(|p| (p.x, p.y)),
                    )
                })
                .collect::<Vec<_>>(),
            "ipc",
        ),
        BoardWrite::Refused(result) => return Ok(result),
        BoardWrite::File => {
            let content = std::fs::read_to_string(&board_path)?;
            let matched: Vec<FileGraphic> = read_file_graphics(&content)
                .into_iter()
                .filter(|g| filter.matches(&g.uuid, g.kind, &g.layer))
                .collect();
            let graphics = matched
                .iter()
                .map(|g| graphic_json(&g.uuid, g.kind, &g.layer, g.origin))
                .collect::<Vec<_>>();

            if !dry_run && !matched.is_empty() {
                let edits = matched
                    .iter()
                    .map(|g| SexpEdit::delete(g.span.0, g.span.1))
                    .collect();
                write_atomic(&board_path, &apply_edits(content, edits))?;
            }
            (graphics, "file")
        }
    };

    Ok(CallToolResult::json(&json!({
        "count": graphics.len(),
        "deleted": if dry_run { 0 } else { graphics.len() },
        "dry_run": dry_run,
        "graphics": graphics,
        "source": source,
        "warning": if source == "file" && !dry_run {
            Some(FILE_FALLBACK_WARNING)
        } else {
            None
        }
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
    let attempt = attempt_ipc_write(ctx, &board_path, "mounting hole", move |c| {
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
    })
    .await?;

    match attempt {
        BoardWrite::Ipc(fp) => Ok(CallToolResult::json(&json!({
            "reference": fp.reference, "x": fp.position.x, "y": fp.position.y,
            "drill_diameter": drill_d, "footprint": fp.footprint,
            "source": "ipc"
        }))),
        BoardWrite::Refused(err) => Ok(err),
        BoardWrite::File => {
            // No live KiCad now, and this board was not observed live during
            // the current server session: use the guarded file path.
            let fp_sexp = format_npth_footprint(x, y, drill_d, &reference);
            let content = std::fs::read_to_string(&board_path)?;
            let close_pos = content.rfind(')').unwrap_or(content.len());
            let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, fp_sexp)]);
            write_atomic(&board_path, &new_content)?;

            Ok(CallToolResult::json(&json!({
                "reference": reference, "x": x, "y": y, "drill_diameter": drill_d,
                "footprint": lib_id,
                "source": "file",
                "warning": FILE_FALLBACK_WARNING
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
    match attempt_ipc_write(ctx, &board_path, "board text", move |c| {
        let bt = builders::board_text(&layer_ipc, &text_ipc, x, y, size, rotation, false);
        let any = builders::pack_any(&bt, "kiapi.board.types.BoardText");
        c.create_items(vec![any]).map(|_| ())
    })
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
        "source": "file",
        "warning": FILE_FALLBACK_WARNING
    })))
}

/// Shared implementation of `add_zone` and its `add_copper_pour` alias.
///
/// IPC first: with KiCAD live on the board, the zone is created through the
/// API and refilled, so it appears immediately and is part of the user's undo
/// stack. Only when no live KiCAD answers does this fall back to inserting the
/// `(zone …)` S-expression, which is reported with an explicit warning — a
/// file-only edit is invisible to an open pcbnew and is discarded by its next
/// save (#192), and that is exactly what `add_zone` used to do unconditionally.
pub(crate) async fn add_zone_impl(
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
    let clearance = args["clearance"]
        .as_f64()
        .unwrap_or(DEFAULT_ZONE_CLEARANCE_MM);
    let min_width = args["min_width"]
        .as_f64()
        .unwrap_or(DEFAULT_ZONE_MIN_WIDTH_MM);
    let zone_name = args["name"].as_str().unwrap_or("").to_string();
    let priority = args["priority"].as_u64().unwrap_or(0) as u32;
    let connection = match PadConnection::parse(args["pad_connection"].as_str()) {
        Ok(v) => v,
        Err(message) => return Ok(CallToolResult::error(message)),
    };
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

    let describe = || {
        json!({
            "net": net_name,
            "layer": layer,
            "point_count": points.len(),
            "name": zone_name,
            "priority": priority,
            "pad_connection": connection.as_str(),
        })
    };

    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    let name_ipc = zone_name.clone();
    let points_ipc = points.clone();
    let ipc_attempt = attempt_ipc_write(ctx, &board_path, "zone", move |c| {
        c.add_zone(&konnect_ipc::builders::ZoneSpec {
            layer: &layer_ipc,
            net_name: &net_ipc,
            points: &points_ipc,
            clearance_mm: clearance,
            min_thickness_mm: min_width,
            name: &name_ipc,
            priority,
            connection: connection.as_ipc(),
        })
    })
    .await?;

    match ipc_attempt {
        BoardWrite::Refused(err) => return Ok(err),
        BoardWrite::Ipc(zone_id) => {
            let mut body = describe();
            body["source"] = json!("ipc");
            body["zone_id"] = match zone_id {
                Some(id) => json!(id),
                None => serde_json::Value::Null,
            };
            return Ok(CallToolResult::json(&body));
        }
        BoardWrite::File => {}
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
    let zone_sexp = format_zone_polygon(
        &net, &layer, clearance, min_width, &points, &zone_name, priority, connection,
    );

    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, zone_sexp)]);
    write_atomic(&board_path, &new_content)?;

    let mut body = describe();
    body["source"] = json!("file");
    body["warning"] = json!(FILE_FALLBACK_WARNING);
    Ok(CallToolResult::json(&body))
}

async fn handle_add_zone(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    add_zone_impl(args, ctx).await
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
    let ipc_attempt = attempt_ipc_write(ctx, &board_path, "SVG logo", move |c| {
        let shape = builders::board_polygon(&layer_ipc, 0.0, true, &placed_ipc);
        let any = builders::pack_any(&shape, "kiapi.board.types.BoardGraphicShape");
        c.create_items(vec![any]).map(|_| ())
    })
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
        "source": "file",
        "warning": FILE_FALLBACK_WARNING
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

/// Shared scaffolding for this module's tests: a `ToolContext` pointed at a
/// given IPC address, and a mock KiCad that answers `GetOpenDocuments` with
/// one board and delegates every other command to the test.
#[cfg(test)]
mod board_mock {
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use konnect_ipc::gen::kiapi;
    use prost::Message;
    use std::sync::Arc;

    /// An empty `address` classifies as transport-unreachable, which is the
    /// file-editing path — no live KiCad needed.
    pub fn ctx_talking_to(address: String) -> ToolContext {
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

    /// A rep0 endpoint playing a KiCad that holds `board` open. `respond`
    /// answers the commands the test cares about and returns `None` for the
    /// rest; the socket, the envelope, and the `GetOpenDocuments` answer are
    /// handled here so each test only writes its own command arms.
    pub fn spawn_kicad_holding_board(
        board: &std::path::Path,
        respond: impl Fn(&prost_types::Any) -> Option<prost_types::Any> + Send + 'static,
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
                } else {
                    respond(&command)
                };
                let response = kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    header: None,
                    message: body,
                };
                if socket
                    .send(nng::Message::from(response.encode_to_vec().as_slice()))
                    .is_err()
                {
                    break;
                }
            }
        });
        url
    }
}

#[cfg(test)]
mod board_session_safety_tests {
    use super::board_mock::{ctx_talking_to, spawn_kicad_holding_board};
    use super::*;
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    fn spawn_one_document_response(
        board: &std::path::Path,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("tcp://127.0.0.1:{port}");
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock rep socket");
        socket.listen(&url).expect("mock listen");
        let board = board.display().to_string();
        let handle = std::thread::spawn(move || {
            let request = socket.recv().expect("GetOpenDocuments request");
            let request = kiapi::common::ApiRequest::decode(request.as_slice()).unwrap();
            assert!(request
                .message
                .as_ref()
                .is_some_and(|message| message.type_url.ends_with("GetOpenDocuments")));
            let document = kiapi::common::types::DocumentSpecifier {
                r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                project: None,
                identifier: Some(
                    kiapi::common::types::document_specifier::Identifier::BoardFilename(board),
                ),
            };
            let response = kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsOk as i32,
                    error_message: String::new(),
                }),
                header: None,
                message: Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetOpenDocumentsResponse {
                        documents: vec![document],
                    },
                    "kiapi.common.commands.GetOpenDocumentsResponse",
                )),
            };
            socket
                .send(nng::Message::from(response.encode_to_vec().as_slice()))
                .expect("document response");
        });
        (url, handle)
    }

    #[tokio::test]
    async fn live_then_dead_blocks_a_file_fallback_and_preserves_the_board() {
        let dir = tempfile::tempdir().unwrap();
        let board = super::mounting_hole_tests::blank_board(dir.path());
        let before = std::fs::read(&board).unwrap();
        let (address, server) = spawn_one_document_response(&board);
        let ctx = ctx_talking_to(address);

        let observation = with_board_ipc_classified(&ctx, &board, |_| Ok(()))
            .await
            .unwrap();
        assert!(observation.is_ok());
        server.join().unwrap();

        let result = super::handle_add_mounting_hole(
            &json!({
                "board": board.to_string_lossy(),
                "x": 5.0,
                "y": 6.0,
                "reference": "H1"
            }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("unsafe_file_fallback")
        );
        let text = super::mounting_hole_tests::result_text(&result);
        assert!(text.contains("Konnect previously reached KiCad"), "{text}");
        assert!(text.contains("did not modify"), "{text}");
        assert_eq!(std::fs::read(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn an_offline_board_never_observed_live_still_allows_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let board = super::mounting_hole_tests::blank_board(dir.path());
        let ctx = ctx_talking_to(String::new());

        let outcome = attempt_ipc_write(&ctx, &board, "test write", |_| Ok(()))
            .await
            .unwrap();

        assert!(matches!(outcome, BoardWrite::File));
    }

    #[tokio::test]
    async fn an_observed_board_does_not_taint_a_different_offline_board() {
        let dir = tempfile::tempdir().unwrap();
        let board_a = dir.path().join("a.kicad_pcb");
        let board_b = dir.path().join("b.kicad_pcb");
        std::fs::write(&board_a, "").unwrap();
        std::fs::write(&board_b, "").unwrap();
        let ctx = ctx_talking_to(String::new());
        ctx.board_session.observe_live(&board_a);

        let outcome = attempt_ipc_write(&ctx, &board_b, "test write", |_| Ok(()))
            .await
            .unwrap();

        assert!(matches!(outcome, BoardWrite::File));
    }

    #[tokio::test]
    async fn any_exact_board_lock_refuses_offline_file_mode_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let board = super::mounting_hole_tests::blank_board(dir.path());
        let before = std::fs::read(&board).unwrap();
        let lock = konnect_sexp::writer::kicad_editor_lock_path(&board).unwrap();
        let ctx = ctx_talking_to(String::new());

        for body in [
            r#"{"username":"test","hostname":"host"}"#,
            r#"{"username":"former","hostname":"retired"}"#,
            "not parseable lock data",
        ] {
            std::fs::write(&lock, body).unwrap();
            let outcome = attempt_ipc_write(&ctx, &board, "test write", |_| Ok(()))
                .await
                .unwrap();

            let BoardWrite::Refused(result) = outcome else {
                panic!("an observed sibling lock must refuse file mode")
            };
            assert_eq!(
                crate::mcp::error::extract_error_kind(&result).as_deref(),
                Some("stale_target")
            );
            assert_eq!(std::fs::read(&board).unwrap(), before);
        }

        let guarded = refuse_if_board_open_in_kicad(&ctx, &board, "file-only test write")
            .await
            .unwrap()
            .expect("the file-only gate must honor the same exact lock");
        assert_eq!(
            crate::mcp::error::extract_error_kind(&guarded).as_deref(),
            Some("stale_target")
        );
    }

    #[tokio::test]
    async fn a_lock_for_another_board_does_not_taint_the_requested_board() {
        let dir = tempfile::tempdir().unwrap();
        let requested = super::mounting_hole_tests::blank_board(dir.path());
        let other = dir.path().join("other.kicad_pcb");
        std::fs::write(&other, "(kicad_pcb)\n").unwrap();
        let other_lock = konnect_sexp::writer::kicad_editor_lock_path(&other).unwrap();
        std::fs::write(other_lock, "locked").unwrap();
        let ctx = ctx_talking_to(String::new());

        let outcome = attempt_ipc_write(&ctx, &requested, "test write", |_| Ok(()))
            .await
            .unwrap();

        assert!(matches!(outcome, BoardWrite::File));
    }

    #[test]
    fn an_uninspectable_exact_board_lock_fails_closed() {
        let board = std::path::Path::new("design.kicad_pcb");
        let result = stale_board_lock_refusal_with(board, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "mock access denied",
            ))
        })
        .expect("inspection failure must refuse");

        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("stale_target")
        );
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text,
            other => panic!("expected text, got {other:?}"),
        };
        assert!(text.contains("could not be inspected"), "{text}");
    }

    #[tokio::test]
    async fn a_wrong_open_document_is_structured_and_never_reaches_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let requested = super::mounting_hole_tests::blank_board(dir.path());
        let other = dir.path().join("other.kicad_pcb");
        std::fs::write(&other, "(kicad_pcb)\n").unwrap();
        let before = std::fs::read(&requested).unwrap();
        let address = spawn_kicad_holding_board(&other, |_| None);
        let ctx = ctx_talking_to(address);
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_in_write = called.clone();

        let outcome = attempt_ipc_write(&ctx, &requested, "test write", move |_| {
            called_in_write.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();

        let BoardWrite::Refused(result) = outcome else {
            panic!("wrong document must be a refusal")
        };
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("wrong_document")
        );
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(std::fs::read(&requested).unwrap(), before);
        assert!(!ctx.board_session.was_observed_live(&requested));
    }

    #[tokio::test]
    async fn a_file_only_guard_blocks_a_previously_live_board_after_transport_loss() {
        let dir = tempfile::tempdir().unwrap();
        let board = super::mounting_hole_tests::blank_board(dir.path());
        let ctx = ctx_talking_to(String::new());
        ctx.board_session.observe_live(&board);

        let result = refuse_if_board_open_in_kicad(&ctx, &board, "test edit")
            .await
            .unwrap()
            .expect("the edit must be refused");

        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("unsafe_file_fallback")
        );
    }

    #[tokio::test]
    async fn an_operation_rejected_after_identification_still_protects_the_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let board = super::mounting_hole_tests::blank_board(dir.path());
        let (address, server) = spawn_one_document_response(&board);
        let ctx = ctx_talking_to(address);

        let rejected = with_board_ipc_classified(&ctx, &board, |_| -> anyhow::Result<()> {
            anyhow::bail!("mock mutation rejected after board identification")
        })
        .await
        .unwrap();
        assert!(matches!(
            rejected,
            Err(konnect_ipc::IpcFailure::Rejected(_))
        ));
        server.join().unwrap();

        let next = attempt_ipc_write(&ctx, &board, "next write", |_| Ok(()))
            .await
            .unwrap();
        let BoardWrite::Refused(result) = next else {
            panic!("the next file fallback must be refused")
        };
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("unsafe_file_fallback")
        );
    }

    #[tokio::test]
    async fn rejection_before_board_identification_does_not_create_an_observation() {
        let dir = tempfile::tempdir().unwrap();
        let board = super::mounting_hole_tests::blank_board(dir.path());
        let ctx = ctx_talking_to(super::mounting_hole_tests::spawn_rejecting_kicad());

        let result = with_board_ipc_classified(&ctx, &board, |_| Ok(()))
            .await
            .unwrap();

        assert!(matches!(result, Err(konnect_ipc::IpcFailure::Rejected(_))));
        assert!(!ctx.board_session.was_observed_live(&board));
    }
}

#[cfg(test)]
mod svg_logo_tests {
    use super::board_mock::ctx_talking_to;
    use super::*;

    fn test_ctx() -> ToolContext {
        ctx_talking_to(String::new())
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

/// `add_board_outline` and `set_board_size` only ever appended, so an outline
/// was write-once: a second call left two overlapping rectangles and a DRC
/// failure, and shrinking a board meant deleting the old edges by hand in
/// KiCad. `delete_graphics` is the missing delete verb.
#[cfg(test)]
mod delete_graphics_tests {
    use super::board_mock::{ctx_talking_to, spawn_kicad_holding_board};
    use super::*;
    use konnect_ipc::gen::kiapi;
    use prost::Message;
    use std::sync::{Arc, Mutex};

    /// Tab-indented like KiCad 10's own writer, with a graphic *inside* a
    /// footprint that no filter may ever reach.
    const BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(paper \"A4\")\n\
        \t(gr_line\n\t\t(start 0 0)\n\t\t(end 100 0)\n\t\t(layer \"Edge.Cuts\")\n\t\t(uuid \"edge-top\")\n\t)\n\
        \t(gr_line\n\t\t(start 100 0)\n\t\t(end 100 60)\n\t\t(layer \"Edge.Cuts\")\n\t\t(uuid \"edge-right\")\n\t)\n\
        \t(gr_text \"REV A\"\n\t\t(at 10 10 0)\n\t\t(layer \"F.SilkS\")\n\t\t(uuid \"silk-text\")\n\t)\n\
        \t(gr_circle\n\t\t(center 5 5)\n\t\t(end 7 5)\n\t\t(layer \"F.SilkS\")\n\t\t(uuid \"silk-dot\")\n\t)\n\
        \t(footprint \"R_0402\"\n\t\t(at 20 20)\n\
        \t\t(gr_line\n\t\t\t(start 0 0)\n\t\t\t(end 1 0)\n\t\t\t(layer \"Edge.Cuts\")\n\t\t\t(uuid \"inside-footprint\")\n\t\t)\n\
        \t\t(uuid \"fp1\")\n\t)\n\
        \t(zone (net 1) (layer \"F.Cu\") (uuid \"zone1\"))\n\
        )\n";

    async fn delete_graphics(
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> (serde_json::Value, bool) {
        let result = handle_delete_graphics(&args, ctx)
            .await
            .expect("handler should succeed");
        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        (serde_json::from_str(&body).unwrap(), result.is_error)
    }

    fn offline() -> ToolContext {
        ctx_talking_to(String::new())
    }

    fn board_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, BOARD).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn clearing_edge_cuts_leaves_the_rest_of_the_board() {
        let (_dir, board) = board_file();

        let (body, is_error) = delete_graphics(
            &offline(),
            json!({
                "board": board.to_str().unwrap(),
                "layer": "Edge.Cuts"
            }),
        )
        .await;

        assert!(!is_error, "{body}");
        assert_eq!(body["deleted"], json!(2));
        assert_eq!(body["source"], json!("file"));

        let updated = std::fs::read_to_string(&board).unwrap();
        assert!(!updated.contains("edge-top"));
        assert!(!updated.contains("edge-right"));
        // A footprint's own graphics are the footprint's, whatever layer they
        // claim — cutting one out would corrupt the part.
        assert!(updated.contains("inside-footprint"));
        assert!(updated.contains("silk-text"));
        assert!(updated.contains("zone1"));
        parse_sexp(&updated).expect("the board still parses");
    }

    /// The whole point of the tool: place an outline, clear it, place a
    /// smaller one, and end up with exactly one rectangle.
    #[tokio::test]
    async fn an_outline_can_be_replaced_by_clearing_it_first() {
        let (_dir, board) = board_file();
        let ctx = offline();
        let args = json!({ "board": board.to_str().unwrap() });

        delete_graphics(
            &ctx,
            json!({ "board": board.to_str().unwrap(), "layer": "Edge.Cuts" }),
        )
        .await;
        let mut outline = args.clone();
        outline["x1"] = json!(0.0);
        outline["y1"] = json!(0.0);
        outline["x2"] = json!(50.0);
        outline["y2"] = json!(30.0);
        handle_add_board_outline(&outline, &ctx).await.unwrap();

        let updated = std::fs::read_to_string(&board).unwrap();
        let edges = read_file_graphics(&updated)
            .into_iter()
            .filter(|g| g.layer == "Edge.Cuts")
            .count();
        assert_eq!(edges, 4, "one rectangle, not two overlapping ones");
    }

    #[tokio::test]
    async fn a_dry_run_reports_the_matches_and_changes_nothing() {
        let (_dir, board) = board_file();

        let (body, is_error) = delete_graphics(
            &offline(),
            json!({
                "board": board.to_str().unwrap(),
                "layer": "Edge.Cuts",
                "dry_run": true
            }),
        )
        .await;

        assert!(!is_error, "{body}");
        assert_eq!(body["count"], json!(2));
        assert_eq!(body["deleted"], json!(0));
        assert_eq!(body["graphics"][0]["uuid"], json!("edge-top"));
        assert_eq!(body["graphics"][0]["type"], json!("line"));
        assert_eq!(body["graphics"][0]["x"], json!(0.0));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }

    #[tokio::test]
    async fn a_uuid_filter_deletes_exactly_that_graphic() {
        let (_dir, board) = board_file();

        let (body, _) = delete_graphics(
            &offline(),
            json!({
                "board": board.to_str().unwrap(),
                "uuids": ["silk-text"]
            }),
        )
        .await;

        assert_eq!(body["deleted"], json!(1));
        let updated = std::fs::read_to_string(&board).unwrap();
        assert!(!updated.contains("silk-text"));
        assert!(updated.contains("silk-dot"));
    }

    #[tokio::test]
    async fn filters_combine() {
        let (_dir, board) = board_file();

        let (body, _) = delete_graphics(
            &offline(),
            json!({
                "board": board.to_str().unwrap(),
                "layer": "F.SilkS",
                "types": ["circle"]
            }),
        )
        .await;

        assert_eq!(body["deleted"], json!(1));
        assert_eq!(body["graphics"][0]["uuid"], json!("silk-dot"));
        assert!(std::fs::read_to_string(&board)
            .unwrap()
            .contains("silk-text"));
    }

    /// Omitting every filter would wipe the board's artwork, which no caller
    /// means by leaving the arguments out.
    #[tokio::test]
    async fn a_call_with_no_filter_is_refused() {
        let (_dir, board) = board_file();

        let (body, is_error) =
            delete_graphics(&offline(), json!({ "board": board.to_str().unwrap() })).await;

        assert!(is_error);
        assert_eq!(body["error"]["kind"], json!("invalid_argument"));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }

    #[tokio::test]
    async fn an_unknown_type_names_the_valid_ones() {
        let (_dir, board) = board_file();

        let (body, is_error) = delete_graphics(
            &offline(),
            json!({
                "board": board.to_str().unwrap(),
                "types": ["gr_line"]
            }),
        )
        .await;

        assert!(is_error);
        assert_eq!(body["error"]["kind"], json!("invalid_argument"));
        assert!(body["message"].as_str().unwrap().contains("line, rect"));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }

    /// A KiCad holding `board` with `shapes` (uuid, layer) on it, recording
    /// every UUID a DeleteItems request asks for.
    fn spawn_kicad_with_shapes(
        board: &std::path::Path,
        shapes: Vec<(&'static str, &'static str)>,
        deleted: Arc<Mutex<Vec<String>>>,
    ) -> String {
        spawn_kicad_holding_board(board, move |command| {
            if command.type_url.ends_with("GetItems") {
                let items = shapes
                    .iter()
                    .map(|(uuid, layer)| {
                        let mut shape =
                            konnect_ipc::builders::board_segment(layer, 0.05, 0.0, 0.0, 10.0, 0.0);
                        shape.id = Some(kiapi::common::types::Kiid {
                            value: uuid.to_string(),
                        });
                        konnect_ipc::builders::pack_any(
                            &shape,
                            "kiapi.board.types.BoardGraphicShape",
                        )
                    })
                    .collect();
                Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items,
                    },
                    "kiapi.common.commands.GetItemsResponse",
                ))
            } else if command.type_url.ends_with("DeleteItems") {
                let delete =
                    kiapi::common::commands::DeleteItems::decode(command.value.as_slice()).unwrap();
                deleted
                    .lock()
                    .unwrap()
                    .extend(delete.item_ids.iter().map(|id| id.value.clone()));
                Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::DeleteItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        deleted_items: vec![],
                    },
                    "kiapi.common.commands.DeleteItemsResponse",
                ))
            } else {
                None
            }
        })
    }

    /// The live board wins over the file, the same way the writers in this
    /// toolset act on the board KiCad holds.
    #[tokio::test]
    async fn a_live_board_is_edited_over_ipc_and_the_file_is_left_alone() {
        let (_dir, board) = board_file();
        let deleted = Arc::new(Mutex::new(Vec::new()));
        let address = spawn_kicad_with_shapes(
            &board,
            vec![("live-edge", "Edge.Cuts"), ("live-silk", "F.SilkS")],
            deleted.clone(),
        );
        let ctx = ctx_talking_to(address);

        let (body, is_error) = delete_graphics(
            &ctx,
            json!({ "board": board.to_str().unwrap(), "layer": "Edge.Cuts" }),
        )
        .await;

        assert!(!is_error, "{body}");
        assert_eq!(body["source"], json!("ipc"));
        assert_eq!(body["deleted"], json!(1));
        assert_eq!(*deleted.lock().unwrap(), vec!["live-edge".to_string()]);
        // The file is the last save; KiCad owns the board, so it stays as-is.
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }

    #[tokio::test]
    async fn a_filter_matching_nothing_deletes_nothing() {
        let (_dir, board) = board_file();

        let (body, is_error) = delete_graphics(
            &offline(),
            json!({
                "board": board.to_str().unwrap(),
                "layer": "B.SilkS"
            }),
        )
        .await;

        assert!(!is_error, "{body}");
        assert_eq!(body["count"], json!(0));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }
}

#[cfg(test)]
mod net_count_tests {
    use super::board_mock::ctx_talking_to;
    use super::*;

    fn test_ctx() -> ToolContext {
        ctx_talking_to(String::new())
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

#[cfg(test)]
mod rounded_outline_tests {
    use super::*;

    fn endpoints(primitive: &OutlinePrimitive) -> ((f64, f64), (f64, f64)) {
        match primitive {
            OutlinePrimitive::Line { start, end } | OutlinePrimitive::Arc { start, end, .. } => {
                (*start, *end)
            }
        }
    }

    #[test]
    fn rounded_rectangle_is_a_closed_four_line_four_arc_path() {
        let outline = rounded_rectangle_outline(10.0, 20.0, 50.0, 60.0, 5.0).unwrap();
        assert_eq!(
            outline
                .iter()
                .filter(|item| matches!(item, OutlinePrimitive::Line { .. }))
                .count(),
            4
        );
        assert_eq!(
            outline
                .iter()
                .filter(|item| matches!(item, OutlinePrimitive::Arc { .. }))
                .count(),
            4
        );
        for index in 0..outline.len() {
            let (_, end) = endpoints(&outline[index]);
            let (next_start, _) = endpoints(&outline[(index + 1) % outline.len()]);
            assert_eq!(end, next_start, "outline gap after primitive {index}");
        }

        let OutlinePrimitive::Arc { start, mid, end } = &outline[1] else {
            panic!("top-right primitive should be an arc");
        };
        assert_eq!(*start, (45.0, 20.0));
        assert_eq!(*end, (50.0, 25.0));
        let center = (45.0, 25.0);
        let mid_radius = ((mid.0 - center.0).powi(2) + (mid.1 - center.1).powi(2)).sqrt();
        assert!((mid_radius - 5.0).abs() < 1e-9);
    }

    #[test]
    fn capsule_outline_omits_zero_length_sides() {
        let outline = rounded_rectangle_outline(0.0, 0.0, 30.0, 10.0, 5.0).unwrap();
        assert_eq!(
            outline
                .iter()
                .filter(|item| matches!(item, OutlinePrimitive::Line { .. }))
                .count(),
            2
        );
        assert_eq!(outline.len(), 6);
    }

    #[test]
    fn corner_radius_cannot_overlap_itself() {
        let error = rounded_rectangle_outline(0.0, 0.0, 20.0, 10.0, 5.1).unwrap_err();
        assert_eq!(error.0, "corner_radius");
        assert!(error.1.contains("half the shorter side"));
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
    async fn rounded_outline_file_fallback_writes_real_kicad_arcs() {
        let dir = tempfile::tempdir().unwrap();
        let board = blank_board(dir.path());
        let ctx = ctx_with_ipc(String::new());
        let mut args = board_args(&board);
        args["corner_radius"] = json!(3.0);

        let result = handle_add_board_outline(&args, &ctx).await.unwrap();
        assert!(!result.is_error, "handler errored: {:?}", result.content);
        let response: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(response["corner_radius"], 3.0);
        assert_eq!(response["line_count"], 4);
        assert_eq!(response["arc_count"], 4);

        let updated = std::fs::read_to_string(&board).unwrap();
        assert_eq!(updated.matches("(gr_line").count(), 4, "{updated}");
        assert_eq!(updated.matches("(gr_arc").count(), 4, "{updated}");
        parse_sexp(&updated).expect("rounded outline remains valid KiCad S-expression");
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

    fn body_of(r: &CallToolResult) -> serde_json::Value {
        serde_json::from_str(&text_of(r)).expect("tool results are JSON")
    }

    async fn zone_with(board: &str, extra: serde_json::Value) -> (CallToolResult, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board).unwrap();
        let mut args = json!({
            "board": path.to_str().unwrap(), "net_name": "GND", "layer": "B.Cu",
            "points": [ {"x": 0.0, "y": 0.0}, {"x": 10.0, "y": 0.0}, {"x": 10.0, "y": 10.0} ]
        });
        for (key, value) in extra.as_object().expect("object").clone() {
            args[key] = value;
        }
        let result = handle_add_zone(&args, &test_ctx()).await.unwrap();
        (result, std::fs::read_to_string(&path).unwrap())
    }

    /// The defect this replaced: with no IPC attempt at all, `add_zone` wrote
    /// the file and reported plain success, so a zone added while KiCad held
    /// the board open was invisible and was dropped by KiCad's next save
    /// (#192). The file path is still there — it is the right thing to do when
    /// nothing is holding the board — but it now says so.
    #[tokio::test]
    async fn the_file_fallback_names_itself_and_warns() {
        let (result, _) = zone_with(KICAD_10, json!({})).await;
        assert!(!result.is_error, "{}", text_of(&result));
        let body = body_of(&result);
        assert_eq!(body["source"], json!("file"));
        let warning = body["warning"].as_str().expect("a fallback must warn");
        assert!(
            warning.contains("current Konnect server session"),
            "{warning}"
        );
        assert!(warning.contains("crashed or was force-quit"), "{warning}");
    }

    #[tokio::test]
    async fn the_new_fields_reach_the_s_expression() {
        let (result, after) = zone_with(
            KICAD_10,
            json!({ "name": "ground pour", "priority": 3, "pad_connection": "solid" }),
        )
        .await;
        assert!(!result.is_error, "{}", text_of(&result));
        let z = &after[after.find("(zone").expect("zone written")..];
        assert!(z.contains("(name \"ground pour\")"), "{z}");
        assert!(z.contains("(priority 3)"), "{z}");
        assert!(z.contains("(connect_pads yes (clearance"), "{z}");
        assert!(konnect_sexp::parse_sexp(&after).is_ok(), "still parses");

        let body = body_of(&result);
        assert_eq!(body["name"], json!("ground pour"));
        assert_eq!(body["priority"], json!(3));
        assert_eq!(body["pad_connection"], json!("solid"));
    }

    /// KiCad spells "thermal" and "priority 0" by writing nothing at all, so
    /// the defaults must not start emitting nodes pcbnew never writes.
    #[tokio::test]
    async fn the_defaults_write_what_pcbnew_writes() {
        let (result, after) = zone_with(KICAD_10, json!({})).await;
        assert!(!result.is_error, "{}", text_of(&result));
        let z = &after[after.find("(zone").expect("zone written")..];
        assert!(z.contains("(connect_pads (clearance 0.2))"), "{z}");
        assert!(z.contains("(min_thickness 0.2)"), "{z}");
        assert!(!z.contains("(priority"), "priority 0 is implicit: {z}");
        assert!(
            !z.contains("(name"),
            "an unnamed zone gets no name node: {z}"
        );
    }

    #[tokio::test]
    async fn pad_connection_none_writes_the_no_token() {
        let (_, after) = zone_with(KICAD_10, json!({ "pad_connection": "none" })).await;
        let z = &after[after.find("(zone").unwrap()..];
        assert!(z.contains("(connect_pads no (clearance"), "{z}");
    }

    #[tokio::test]
    async fn an_unknown_pad_connection_is_refused_before_anything_is_written() {
        let (result, after) =
            zone_with(KICAD_10, json!({ "pad_connection": "thermal_relief" })).await;
        assert!(result.is_error, "{}", text_of(&result));
        assert!(text_of(&result).contains("'solid', 'thermal' or 'none'"));
        assert_eq!(after, KICAD_10, "file must be untouched");
    }

    /// A KiCad that answers — even to say no — may be holding this board, so
    /// the file must not be edited behind it. This is `attempt_ipc_write`'s
    /// fail-closed rule; before this change `add_zone` reached the same
    /// outcome through `refuse_if_board_open_in_kicad`, which had no IPC path
    /// to offer instead.
    #[tokio::test]
    async fn a_rejecting_kicad_never_touches_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, KICAD_10).unwrap();
        let ctx = super::mounting_hole_tests::ctx_with_ipc(
            super::mounting_hole_tests::spawn_rejecting_kicad(),
        );
        let result = handle_add_zone(
            &json!({
                "board": path.to_str().unwrap(), "net_name": "GND", "layer": "B.Cu",
                "points": [ {"x": 0.0, "y": 0.0}, {"x": 10.0, "y": 0.0}, {"x": 10.0, "y": 10.0} ]
            }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error, "{}", text_of(&result));
        assert!(
            text_of(&result).contains("board file was not modified"),
            "{}",
            text_of(&result)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), KICAD_10);
    }
}

#[cfg(test)]
mod board_size_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn ctx() -> ToolContext {
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

    /// A KiCad-10-shaped board (tab indentation) already carrying a 20x10
    /// outline at the origin, the way pcbnew saves one.
    const BOARD_WITH_OUTLINE: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(31 \"B.Cu\" signal)\n\t)\n\
        \t(gr_line\n\t\t(start 0 0)\n\t\t(end 20 0)\n\t\t(stroke (width 0.05) (type default))\n\t\t(layer \"Edge.Cuts\")\n\t)\n\
        \t(gr_line\n\t\t(start 20 0)\n\t\t(end 20 10)\n\t\t(stroke (width 0.05) (type default))\n\t\t(layer \"Edge.Cuts\")\n\t)\n\
        \t(gr_line\n\t\t(start 20 10)\n\t\t(end 0 10)\n\t\t(stroke (width 0.05) (type default))\n\t\t(layer \"Edge.Cuts\")\n\t)\n\
        \t(gr_line\n\t\t(start 0 10)\n\t\t(end 0 0)\n\t\t(stroke (width 0.05) (type default))\n\t\t(layer \"Edge.Cuts\")\n\t)\n\
        )";

    async fn resize(board_text: &str, w: f64, h: f64) -> (CallToolResult, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board_text).unwrap();
        let result = handle_set_board_size(
            &serde_json::json!({
                "board": path.to_str().unwrap(),
                "width": w,
                "height": h
            }),
            &ctx(),
        )
        .await
        .unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        (result, after)
    }

    fn edge_cuts_lines(content: &str) -> usize {
        find_direct_child_blocks(content, "kicad_pcb")
            .into_iter()
            .filter(|&(s, e)| {
                let b = &content[s..e];
                b.starts_with("(gr_line") && b.contains("\"Edge.Cuts\"")
            })
            .count()
    }

    fn text_of_result(result: &CallToolResult) -> String {
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// #314: since the first release this tool appended a second rectangle on
    /// every call — the board accumulated outlines and failed DRC with a
    /// self-intersecting Edge.Cuts while the tool reported success.
    #[tokio::test]
    async fn resizing_replaces_the_outline_instead_of_stacking_a_second_one() {
        let (result, after) = resize(BOARD_WITH_OUTLINE, 45.0, 30.0).await;
        assert!(!result.is_error);

        assert_eq!(
            edge_cuts_lines(&after),
            4,
            "exactly one rectangle must remain: {after}"
        );
        assert!(after.contains("(end 45"), "new width missing: {after}");
        assert!(
            !after.contains("(end 20 0)"),
            "old outline survived: {after}"
        );

        // And the response says what actually happened, not what was asked.
        let output: serde_json::Value = serde_json::from_str(&text_of_result(&result)).unwrap();
        assert_eq!(output["replaced_segments"], 4);
        assert_eq!(output["source"], "file");
    }

    /// Two resizes in a row must not accumulate either — the second replaces
    /// the first rectangle.
    #[tokio::test]
    async fn a_second_resize_still_leaves_one_rectangle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, BOARD_WITH_OUTLINE).unwrap();
        for (w, h) in [(45.0, 30.0), (60.0, 40.0)] {
            let result = handle_set_board_size(
                &serde_json::json!({
                    "board": path.to_str().unwrap(),
                    "width": w,
                    "height": h
                }),
                &ctx(),
            )
            .await
            .unwrap();
            assert!(!result.is_error);
        }
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edge_cuts_lines(&after), 4, "{after}");
        assert!(after.contains("(end 60"), "{after}");
    }

    /// An empty board still gets its first outline (the old behavior that was
    /// actually correct).
    #[tokio::test]
    async fn a_board_without_an_outline_gains_one() {
        let bare = "(kicad_pcb\n\t(version 20260206)\n\t(generator \"pcbnew\")\n)";
        let (result, after) = resize(bare, 30.0, 20.0).await;
        assert!(!result.is_error);
        assert_eq!(edge_cuts_lines(&after), 4, "{after}");
    }

    /// An outline containing anything but plain segments refuses untouched:
    /// silently deleting an arc or polygon outline would be guessing at
    /// design intent.
    #[tokio::test]
    async fn a_curved_outline_refuses_and_the_file_is_untouched() {
        let curved = "(kicad_pcb\n\
            \t(version 20260206)\n\
            \t(gr_line\n\t\t(start 0 0)\n\t\t(end 20 0)\n\t\t(layer \"Edge.Cuts\")\n\t)\n\
            \t(gr_arc\n\t\t(start 20 0)\n\t\t(mid 25 5)\n\t\t(end 20 10)\n\t\t(layer \"Edge.Cuts\")\n\t)\n\
            )";
        let (result, after) = resize(curved, 45.0, 30.0).await;
        assert!(result.is_error);
        assert_eq!(after, curved, "a refusal must not modify the file");
        let text = text_of_result(&result);
        assert!(text.contains("arc"), "the refusal names the shape: {text}");
    }

    /// The live-path classifier: a pad-typed `Any` must not be counted, a
    /// non-Edge.Cuts segment must not be deleted, and non-segment Edge.Cuts
    /// geometry is reported by kind (#244's type-URL rule applied here).
    #[test]
    fn partitioning_live_shapes_is_layer_and_type_exact() {
        use konnect_ipc::gen::kiapi;

        let edge_segment = builders::pack_any(
            &builders::board_segment("Edge.Cuts", 0.05, 0.0, 0.0, 20.0, 0.0),
            "kiapi.board.types.BoardGraphicShape",
        );
        let silk_segment = builders::pack_any(
            &builders::board_segment("F.SilkS", 0.12, 0.0, 0.0, 5.0, 0.0),
            "kiapi.board.types.BoardGraphicShape",
        );
        let mut arc = builders::board_segment("Edge.Cuts", 0.05, 0.0, 0.0, 1.0, 1.0);
        arc.shape = Some(kiapi::common::types::GraphicShape {
            geometry: Some(kiapi::common::types::graphic_shape::Geometry::Arc(
                kiapi::common::types::GraphicArcAttributes::default(),
            )),
            ..arc.shape.unwrap_or_default()
        });
        let edge_arc = builders::pack_any(&arc, "kiapi.board.types.BoardGraphicShape");
        // A pad whose bytes would happily decode as a shape (#244).
        let pad = builders::pack_any(
            &kiapi::board::types::Pad::default(),
            "kiapi.board.types.Pad",
        );

        let (ids, other) = partition_edge_cuts_shapes(&[edge_segment, silk_segment, edge_arc, pad]);
        // The plain Edge.Cuts segment has no KIID here (builder does not set
        // one), so ids stays empty — but the arc is still detected by kind.
        assert!(ids.is_empty());
        assert_eq!(other, vec!["arc"]);
    }
}

/// `get_board_info` used to read only the file — the last save — while every
/// writer in this toolset acts on the board KiCad holds. On a board with
/// unsaved edits the two disagreed completely, most visibly as layer_count 0
/// and net_count 0 for a board KiCad was showing fully populated.
#[cfg(test)]
mod board_info_source_tests {
    use super::board_mock::{ctx_talking_to, spawn_kicad_holding_board};
    use super::*;
    use konnect_ipc::gen::kiapi;

    /// A board saved before anything was placed on it: the empty stub the
    /// file-only reader kept reporting.
    const EMPTY_STUB: &str = "(kicad_pcb\n\t(version 20260206)\n\t(paper \"A3\")\n)\n";

    /// A KiCad holding `board` open with `layers` enabled, `copper` of them
    /// copper, and `nets` real nets — none of it saved to the file.
    ///
    /// The net list carries KiCad's unconnected pseudo-net (code 0, empty
    /// name) ahead of the real ones, because `GetNets` returns it and the
    /// count must not.
    fn spawn_kicad_holding(
        board: &std::path::Path,
        layers: usize,
        copper: u32,
        nets: usize,
    ) -> String {
        spawn_kicad_holding_board(board, move |command| {
            if command.type_url.ends_with("GetTitleBlockInfo") {
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
                        nets: std::iter::once(kiapi::board::types::Net {
                            code: Some(kiapi::board::types::NetCode { value: 0 }),
                            name: String::new(),
                        })
                        .chain((1..=nets).map(|index| kiapi::board::types::Net {
                            code: Some(kiapi::board::types::NetCode {
                                value: index as i32,
                            }),
                            name: format!("N{index}"),
                        }))
                        .collect(),
                    },
                    "kiapi.board.commands.NetsResponse",
                ))
            } else {
                None
            }
        })
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
        // 99 real nets, not 100: GetNets also returned the pseudo-net.
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

    /// The pseudo-net is the only net a freshly-created board has, and both
    /// paths have to call that zero. The file path is covered by
    /// `a_board_with_only_the_unconnected_pseudo_net_counts_zero`; this is the
    /// live half, which used to report 1.
    #[tokio::test]
    async fn a_live_board_with_only_the_pseudo_net_counts_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, EMPTY_STUB).unwrap();
        let address = spawn_kicad_holding(&board, 2, 2, 0);

        let info = board_info(&board, &ctx_talking_to(address)).await;

        assert_eq!(info["source"], json!("ipc"));
        assert_eq!(info["net_count"], json!(0));
    }

    /// KiCad's API has no page settings, so paper comes from the file even on
    /// the live path — and when the file cannot be read there is no honest
    /// answer. Reporting A4 would invent a page size the board never stated.
    #[tokio::test]
    async fn an_unreadable_file_reports_no_paper_rather_than_a4() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, EMPTY_STUB).unwrap();
        let address = spawn_kicad_holding(&board, 2, 2, 3);
        std::fs::remove_file(&board).unwrap();

        let info = board_info(&board, &ctx_talking_to(address)).await;

        assert_eq!(info["source"], json!("ipc"));
        assert_eq!(info["paper"], serde_json::Value::Null);
        // The live half of the answer is unaffected by the missing file.
        assert_eq!(info["net_count"], json!(3));
    }
}
#[cfg(test)]
mod board_info_paper_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    /// Deliberately empty ipc_address: with_ipc fails fast against it, so the
    /// handler takes the file path this PR changes.
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

    /// A board saved before anything was placed on it: the empty stub the
    /// file-only reader kept reporting.
    const EMPTY_STUB: &str = "(kicad_pcb\n\t(version 20260206)\n\t(paper \"A3\")\n)\n";

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

    /// A custom size is `(paper "User" W H)`: the name alone answers nothing,
    /// so the dimensions ride along. This is the defect in #219 — the response
    /// used to carry only the token "User".
    #[tokio::test]
    async fn a_user_paper_size_reports_its_dimensions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb\n\t(version 20260206)\n\t(paper \"User\" 431.8 279.4)\n)\n",
        )
        .unwrap();

        let info = board_info(&board, &test_ctx()).await;

        assert_eq!(info["paper"], json!("User"));
        assert_eq!(
            info["paper_size_mm"],
            json!({"width": 431.8, "height": 279.4})
        );
    }

    /// A named size implies its dimensions, so `paper_size_mm` is null rather
    /// than a redundant copy of what every caller already knows about A3.
    #[tokio::test]
    async fn a_named_paper_size_reports_no_dimensions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, EMPTY_STUB).unwrap();

        let info = board_info(&board, &test_ctx()).await;

        assert_eq!(info["paper"], json!("A3"));
        assert_eq!(info["paper_size_mm"], serde_json::Value::Null);
    }
}
