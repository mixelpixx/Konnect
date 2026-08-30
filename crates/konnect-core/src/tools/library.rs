//! `library` toolset — create and manage footprints, symbols, and KiCAD library tables.
//!
//! Operations are file-based (S-expression manipulation + directory scanning).
//! No IPC or kicad-cli is required for most tools.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_array, require_str, ToolContext, ToolDef};
use konnect_schematic_editor::types::fmt_f64;
use konnect_sexp::parser::{parse_sexp, SexpNode};
use konnect_sexp::writer::{
    apply_edits, find_balanced_block, find_block_starts, find_direct_child_blocks, read_consistent,
    write_atomic, write_atomic_if_unchanged, write_new_atomic, SexpEdit,
};
use serde_json::json;
use std::path::{Path, PathBuf};

// ─── Tool definitions ─────────────────────────────────────────────────────────

/// The pin-item object schema (number/name/type/style/x/y/angle/length) shared
/// by `pins`, `units[].pins`, and `power_pins` in the create_symbol schema
/// below. `type_desc` parameterizes the one wording difference between call
/// sites. `require_xy` is false where a `glyph` may auto-place the pins (so x/y
/// become optional) and true for the always-rectangular `power_pins`.
fn pin_item_schema(type_desc: &str, require_xy: bool) -> serde_json::Value {
    let mut required = vec!["number", "name", "type"];
    if require_xy {
        required.push("x");
        required.push("y");
    }
    json!({
        "type": "object",
        "properties": {
            "number": { "type": "string" },
            "name": { "type": "string" },
            "type": {
                "type": "string",
                "enum": ["input", "output", "bidirectional", "tri_state", "passive", "free", "unspecified", "power_in", "power_out", "open_collector", "open_emitter", "no_connect"],
                "description": type_desc
            },
            "style": {
                "type": "string",
                "enum": ["line", "inverted", "clock", "inverted_clock", "input_low", "clock_low", "output_low", "edge_clock_high", "non_logic"],
                "description": "Pin graphic style (default 'line'). 'inverted' = active-low bubble, 'clock' = clock input, etc. Works with any body shape."
            },
            "x": {
                "type": "number",
                "description": "Starting position, not a fixed one. For a rectangle body the box is sized to fit the pin names and every pin whose angle names an edge is then aligned to that edge, so a pin can end up further out than requested. A `glyph` body ignores x/y entirely. The response reports where each pin actually ended up under `units[].pins`, with `requested` alongside whenever it differs."
            },
            "y": {
                "type": "number",
                "description": "Starting position, not a fixed one — see `x`."
            },
            "angle": { "type": "number", "default": 0 },
            "length": { "type": "number", "default": 2.54 }
        },
        "required": required
    })
}

pub fn tools() -> Vec<ToolDef> {
    let mut tools = vec![
        tool!(
            "create_footprint",
            "Create a new footprint (.kicad_mod) file from a pad layout description.",
            json!({
                "type": "object",
                "properties": {
                    "output": { "type": "string", "description": "Output .kicad_mod file path" },
                    "name": { "type": "string", "description": "Footprint name" },
                    "description": { "type": "string", "description": "Footprint description (optional)" },
                    "pads": {
                        "type": "array",
                        "description": "Pad definitions",
                        "items": {
                            "type": "object",
                            "properties": {
                                "number": { "type": "string" },
                                "type": { "type": "string", "description": "'smd', 'thru_hole', 'np_thru_hole'" },
                                "shape": { "type": "string", "description": "'rect', 'oval', 'circle', 'roundrect'" },
                                "x": { "type": "number" },
                                "y": { "type": "number" },
                                "width": { "type": "number" },
                                "height": { "type": "number" },
                                "drill": { "type": "number", "description": "Drill diameter for thru-hole pads" },
                                "layers": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Explicit canonical KiCAD pad layers. Defaults to F.Cu/F.Paste/F.Mask for SMD and *.Cu/*.Mask otherwise."
                                },
                                "rotation": {
                                    "type": "number",
                                    "description": "Pad rotation in degrees (default 0)"
                                },
                                "roundrect_rratio": {
                                    "type": "number",
                                    "minimum": 0,
                                    "maximum": 0.5,
                                    "description": "Rounded-rectangle corner radius ratio (0..0.5)"
                                }
                            },
                            "required": ["number", "type", "shape", "x", "y", "width", "height"]
                        }
                    },
                    "body_width": { "type": "number", "description": "Physical component body width in mm (optional; used for silk/fab outlines). Falls back to the pad envelope if omitted." },
                    "body_height": { "type": "number", "description": "Physical component body height in mm (optional)." },
                    "package_type": { "type": "string", "description": "'smd' (0.25mm courtyard), 'through_hole' (0.5mm), 'small' (0.15mm, <0603), or 'bga' (1.0mm). Sets courtyard clearance when courtyard_clearance is not given." },
                    "courtyard_clearance": { "type": "number", "description": "Explicit courtyard clearance in mm (overrides package_type / auto-detection)." },
                    "model": {
                        "type": "object",
                        "description": "Optional 3D model to associate with the footprint.",
                        "properties": {
                            "path": { "type": "string", "description": "Path to the 3D model file (.step/.wrl); absolute or a KiCAD env-var path like ${KICAD9_3DMODEL_DIR}/..." },
                            "offset": { "type": "object", "description": "{x,y,z} in mm (default 0,0,0)" },
                            "scale": { "type": "object", "description": "{x,y,z} (default 1,1,1)" },
                            "rotate": { "type": "object", "description": "{x,y,z} in degrees (default 0,0,0)" }
                        },
                        "required": ["path"]
                    }
                },
                "required": ["output", "name", "pads"]
            }),
            |args, ctx| async move { handle_create_footprint(args, ctx).await }
        ),
        tool!(
            "edit_footprint_pad",
            "Edit the size, shape, or position of a pad in an existing .kicad_mod footprint file.",
            json!({
                "type": "object",
                "properties": {
                    "footprint_path": { "type": "string", "description": "Path to .kicad_mod file" },
                    "pad_number": { "type": "string", "description": "Pad number to edit" },
                    "new_number": { "type": "string", "description": "New pad number (optional; duplicate destination numbers are allowed)" },
                    "match_all": { "type": "boolean", "description": "Edit every direct-child pad with pad_number instead of only the first match", "default": false },
                    "x": { "type": "number", "description": "New X position in mm (optional)" },
                    "y": { "type": "number", "description": "New Y position in mm (optional)" },
                    "width": { "type": "number", "description": "New pad width in mm (optional)" },
                    "height": { "type": "number", "description": "New pad height in mm (optional)" },
                    "shape": {
                        "type": "string",
                        "enum": ["circle", "rect", "oval", "roundrect"],
                        "description": "New standard pad shape (optional). roundrect gets a valid default corner ratio when needed."
                    },
                    "drill": { "type": "number", "description": "New drill diameter in mm (optional)" }
                },
                "required": ["footprint_path", "pad_number"]
            }),
            |args, ctx| async move { handle_edit_footprint_pad(args, ctx).await }
        ),
        tool!(
            "register_footprint_library",
            "Register a local footprint library directory in the KiCAD global or project library table.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .pretty directory" },
                    "nickname": { "type": "string", "description": "Library nickname" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global' or 'project'",
                        "default": "project"
                    },
                    "project": { "type": "string", "description": "Path to the .kicad_pro file, or to the project directory that holds it (required for project scope)" },
                    "replace_existing": {
                        "type": "boolean",
                        "description": "Replace the URI of an existing nickname instead of leaving it unchanged",
                        "default": false
                    }
                },
                "required": ["library_path", "nickname"]
            }),
            |args, ctx| async move { handle_register_footprint_library(args, ctx).await }
        ),
        tool!(
            "list_footprint_libraries",
            "List all registered footprint libraries (global and optionally project-level).",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to .kicad_pro to include project libraries (optional)" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global', 'project', or 'all'",
                        "default": "all"
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_list_footprint_libraries(args, ctx).await }
        ),
        tool!(
            "create_symbol",
            "Create a new KiCAD schematic symbol and append it to a .kicad_sym library file. \
             Supports single-unit symbols (via `pins`) and multi-unit parts like dual/quad \
             op-amps or gate banks (via `units` + optional `power_pins`). By default each unit \
             gets a rectangular body sized to its pins; set `glyph` (symbol-level and/or per \
             unit) to draw a conventional op-amp triangle or logic-gate body instead. With a \
             glyph, pins auto-place by their `type` (inputs left in the order listed top-to- \
             bottom, output right, power top/bottom) and their x/y are ignored.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" },
                    "name": { "type": "string", "description": "Symbol name" },
                    "reference_prefix": { "type": "string", "description": "Default reference prefix (e.g. 'U')" },
                    "value": { "type": "string", "description": "Default value string" },
                    "datasheet": { "type": "string", "description": "Datasheet URL or path (default empty). A '~' is written as an empty string — carrying it fails ERC's library-match check." },
                    "glyph": {
                        "type": "string",
                        "enum": ["rectangle", "opamp", "buffer", "inverter", "schmitt", "schmitt_inverter", "and", "nand", "or", "nor", "xor", "xnor"],
                        "description": "Symbol-level default body shape. 'rectangle' (default) uses the pin x/y you supply; the others draw a fixed conventional shape and auto-place pins by type (x/y ignored). Op-amp/gate inputs are placed in the order listed, top-to-bottom (KiCAD's op-amp convention is + on top, - on bottom). Inverting glyphs (inverter/schmitt_inverter/nand/nor/xnor) draw the same body as their base and put the inversion bubble on the output pin. If a glyph's pins don't fit (wrong input count, not exactly one output), it falls back to a rectangle and reports a warning."
                    },
                    "pins": {
                        "type": "array",
                        "description": "Pin definitions. x/y size and position the rectangle body rather than fixing the pins: see the per-pin `x`. They are ignored entirely when a `glyph` is set.",
                        "items": pin_item_schema("Pin electrical type — exactly one of KiCAD's 12 values. Note: NC pins are 'no_connect' (not 'not_connected').", false)
                    },
                    "show_pin_names": { "type": "boolean", "description": "Show pin names on the symbol (default true).", "default": true },
                    "show_pin_numbers": { "type": "boolean", "description": "Show pin numbers on the symbol (default true).", "default": true },
                    "units": {
                        "type": "array",
                        "description": "For MULTI-UNIT parts (dual/quad op-amps, gate banks, multi-bank connectors). Each element is one unit (becomes Unit A, B, C...) with its own pins and body. When given, `units` replaces `pins` (use `pins` for single-unit symbols instead). Each unit may set its own `glyph`, overriding the symbol-level default.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "glyph": {
                                    "type": "string",
                                    "enum": ["rectangle", "opamp", "buffer", "inverter", "schmitt", "schmitt_inverter", "and", "nand", "or", "nor", "xor", "xnor"],
                                    "description": "Body shape for this unit, overriding the symbol-level `glyph`."
                                },
                                "pins": {
                                    "type": "array",
                                    "description": "Pins for this unit. x/y are ignored when the unit has a `glyph`.",
                                    "items": pin_item_schema("Pin electrical type — exactly one of KiCAD's 12 values. Note: NC pins are 'no_connect' (not 'not_connected').", false)
                                }
                            },
                            "required": ["pins"]
                        }
                    },
                    "power_pins": {
                        "type": "array",
                        "description": "Shared power pins (V+/V-, VCC/GND). Only meaningful with `units`: they become a dedicated final 'power unit' (e.g. Unit C of a dual op-amp, Unit E of a quad gate) placed once, following KiCAD's own 74xx convention. This avoids drawing the power pins on every unit (which would each need wiring to pass ERC). The power unit is always a rectangle.",
                        "items": pin_item_schema("Pin electrical type — exactly one of KiCAD's 12 values (power pins are usually 'power_in'). Note: NC pins are 'no_connect' (not 'not_connected').", true)
                    }
                },
                "required": ["library_path", "name", "reference_prefix"]
            }),
            |args, ctx| async move { handle_create_symbol(args, ctx).await }
        ),
        tool!(
            "delete_symbol",
            "Delete a symbol definition from a .kicad_sym library file.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" },
                    "symbol_name": { "type": "string", "description": "Name of the symbol to delete" }
                },
                "required": ["library_path", "symbol_name"]
            }),
            |args, ctx| async move { handle_delete_symbol(args, ctx).await }
        ),
        tool!(
            "list_symbols_in_library",
            "List all symbol names defined in a .kicad_sym library file.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" },
                    "limit": { "type": "integer", "description": "Maximum number of symbols to return", "default": 100 }
                },
                "required": ["library_path"]
            }),
            |args, ctx| async move { handle_list_symbols_in_library(args, ctx).await }
        ),
        tool!(
            "register_symbol_library",
            "Register a .kicad_sym library file in the KiCAD global or project symbol table. \
             Reports whether the entry was inserted, left unchanged, or updated — an existing \
             nickname is kept as-is unless replace_existing is set.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym file" },
                    "nickname": { "type": "string", "description": "Library nickname" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global' or 'project'",
                        "default": "project"
                    },
                    "project": { "type": "string", "description": "Path to the .kicad_pro file, or to the project directory that holds it (required for project scope)" },
                    "replace_existing": {
                        "type": "boolean",
                        "description": "Replace the URI of an existing nickname instead of leaving it unchanged",
                        "default": false
                    }
                },
                "required": ["library_path", "nickname"]
            }),
            |args, ctx| async move { handle_register_symbol_library(args, ctx).await }
        ),
        tool!(
            "list_symbol_libraries",
            "List all registered symbol libraries (global and optionally project-level).",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to .kicad_pro to include project libraries (optional)" },
                    "scope": {
                        "type": "string",
                        "description": "Scope: 'global', 'project', or 'all'",
                        "default": "all"
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_list_symbol_libraries(args, ctx).await }
        ),
        tool!(
            "search_symbols",
            "Search for symbols across all registered libraries by name or keyword.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search string (partial name or keyword match)" },
                    "limit": { "type": "integer", "description": "Maximum number of results to return", "default": 50 },
                    "project_dir": { "type": "string", "description": "Project directory whose sym-lib-table is also searched. Defaults to the configured project_dir." }
                },
                "required": ["query"]
            }),
            |args, ctx| async move { handle_search_symbols(args, ctx).await }
        ),
        tool!(
            "list_library_footprints",
            "List all footprints in a specific registered footprint library (.pretty directory).",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .pretty directory (or nickname to look up)" }
                },
                "required": ["library_path"]
            }),
            |args, ctx| async move { handle_list_library_footprints(args, ctx).await }
        ),
        tool!(
            "get_footprint_info",
            "Return detailed information about a footprint: pad layout, courtyard, description, \
             and optionally its supported graphical primitives.",
            json!({
                "type": "object",
                "properties": {
                    "footprint_path": { "type": "string", "description": "Path to .kicad_mod file, OR 'Library:Footprint' identifier" },
                    "project": { "type": "string", "description": "Path to a .kicad_pro used to resolve project libraries (optional)" },
                    "include_graphics": { "type": "boolean", "description": "Include supported top-level footprint graphics in the response", "default": false },
                    "graphics_layer": { "type": "string", "description": "Return graphics only from this canonical KiCad layer; implies include_graphics" }
                },
                "required": ["footprint_path"]
            }),
            |args, ctx| async move { handle_get_footprint_info(args, ctx).await }
        ),
        tool!(
            "search_footprints",
            "Search for footprints across all registered libraries by name or keyword.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search string (partial name or keyword)" },
                    "limit": { "type": "integer", "description": "Maximum number of results to return", "default": 50 }
                },
                "required": ["query"]
            }),
            |args, ctx| async move { handle_search_footprints(args, ctx).await }
        ),
        tool!(
            "get_symbol_info",
            "Return detailed information about a schematic symbol: pins, properties, description.",
            json!({
                "type": "object",
                "properties": {
                    "lib_id": { "type": "string", "description": "Library:Symbol identifier (e.g. 'Device:R')" },
                    "project_dir": { "type": "string", "description": "Project directory to resolve project-scoped libraries. Defaults to the configured project_dir." }
                },
                "required": ["lib_id"]
            }),
            |args, ctx| async move { handle_get_symbol_info(args, ctx).await }
        ),
    ];
    // Grouped after `create_footprint` so the footprint-editing tools read as
    // one family in `tools/list`. Anchored on that tool's name rather than a
    // literal index: the list above is edited often, and a positional insert
    // silently reorders the catalogue the first time a tool moves.
    let after_create_footprint = tools
        .iter()
        .position(|t| t.name == "create_footprint")
        .map(|i| i + 1)
        .unwrap_or(tools.len());
    for (offset, tool) in [
        super::footprint_graphics::tool(),
        super::footprint_metadata::tool(),
        super::footprint_models::tool(),
    ]
    .into_iter()
    .enumerate()
    {
        tools.insert(after_create_footprint + offset, tool);
    }
    tools
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

// ─── Footprint / symbol geometry (pure, unit-tested) ──────────────────────────

/// Minimal pad geometry needed to derive outlines, courtyards, and pin 1.
#[derive(Debug, Clone)]
struct PadGeom {
    number: String,
    pad_type: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Axis-aligned bounding box `(min_x, min_y, max_x, max_y)` over pad extents.
fn pads_bbox(pads: &[PadGeom]) -> (f64, f64, f64, f64) {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in pads {
        min_x = min_x.min(p.x - p.w / 2.0);
        min_y = min_y.min(p.y - p.h / 2.0);
        max_x = max_x.max(p.x + p.w / 2.0);
        max_y = max_y.max(p.y + p.h / 2.0);
    }
    (min_x, min_y, max_x, max_y)
}

/// Courtyard clearance per the contributor's rule: an explicit value wins, else
/// `package_type`, else auto-detect (through-hole 0.5 mm, sub-0603 body 0.15 mm,
/// otherwise SMT 0.25 mm). BGA (1.0 mm) is opt-in via `package_type` because an
/// area array can't be reliably auto-detected from pads alone.
fn courtyard_clearance(
    explicit: Option<f64>,
    package_type: Option<&str>,
    pads: &[PadGeom],
    body: Option<(f64, f64)>,
) -> f64 {
    if let Some(c) = explicit {
        return c;
    }
    match package_type {
        Some("bga") => return 1.0,
        Some("small") => return 0.15,
        Some("through_hole") | Some("th") => return 0.5,
        Some("smd") => return 0.25,
        _ => {}
    }
    if pads.iter().any(|p| p.pad_type.contains("thru")) {
        return 0.5;
    }
    if let Some((bw, bh)) = body {
        // 0603 imperial body is 1.6 x 0.8 mm; anything shorter is "smaller".
        if bw.max(bh) < 1.6 {
            return 0.15;
        }
    }
    0.25
}

/// Index of pin 1: the pad numbered "1", else the first pad. `None` if no pads.
fn pin1_index(pads: &[PadGeom]) -> Option<usize> {
    if pads.is_empty() {
        return None;
    }
    Some(pads.iter().position(|p| p.number == "1").unwrap_or(0))
}

/// The rectangle corner (of the four) nearest point `(px, py)`.
fn nearest_corner(min_x: f64, min_y: f64, max_x: f64, max_y: f64, px: f64, py: f64) -> (f64, f64) {
    let cx = if (px - min_x).abs() <= (max_x - px).abs() {
        min_x
    } else {
        max_x
    };
    let cy = if (py - min_y).abs() <= (max_y - py).abs() {
        min_y
    } else {
        max_y
    };
    (cx, cy)
}

fn point_toward(from: (f64, f64), toward: (f64, f64), d: f64) -> (f64, f64) {
    let dx = toward.0 - from.0;
    let dy = toward.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-9 {
        return from;
    }
    (from.0 + dx / len * d, from.1 + dy / len * d)
}

/// Ordered vertices of a rectangle outline whose corner nearest `(px, py)` is
/// chamfered by `chamfer` mm (clamped to 40% of the shorter side) — the F.Fab
/// pin-1 marker. Clockwise, KiCAD footprint Y-down.
fn chamfered_rect_points(
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    px: f64,
    py: f64,
    chamfer: f64,
) -> Vec<(f64, f64)> {
    let ch = chamfer
        .min(0.4 * (max_x - min_x).min(max_y - min_y))
        .max(0.0);
    let corners = [
        (min_x, min_y),
        (max_x, min_y),
        (max_x, max_y),
        (min_x, max_y),
    ];
    let (tcx, tcy) = nearest_corner(min_x, min_y, max_x, max_y, px, py);
    let mut pts = Vec::new();
    for (i, &(cx, cy)) in corners.iter().enumerate() {
        if (cx - tcx).abs() < 1e-9 && (cy - tcy).abs() < 1e-9 && ch > 0.0 {
            let prev = corners[(i + 3) % 4];
            let next = corners[(i + 1) % 4];
            pts.push(point_toward((cx, cy), prev, ch));
            pts.push(point_toward((cx, cy), next, ch));
        } else {
            pts.push((cx, cy));
        }
    }
    pts
}

/// Emit the `(model ...)` block when a `model` object with a non-empty `path`
/// is present. Path is passed through verbatim (absolute or KiCAD env-var).
fn build_model_sexp(args: &serde_json::Value) -> String {
    let model = match args.get("model") {
        Some(m) if m.is_object() => m,
        _ => return String::new(),
    };
    let path = match model["path"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => return String::new(),
    };
    let xyz = |key: &str, default: f64| -> (f64, f64, f64) {
        let o = &model[key];
        (
            o["x"].as_f64().unwrap_or(default),
            o["y"].as_f64().unwrap_or(default),
            o["z"].as_f64().unwrap_or(default),
        )
    };
    let (ox, oy, oz) = xyz("offset", 0.0);
    let (sx, sy, sz) = xyz("scale", 1.0);
    let (rx, ry, rz) = xyz("rotate", 0.0);
    format!(
        "\n  (model \"{}\"\n    (offset (xyz {} {} {}))\n    (scale (xyz {} {} {}))\n    (rotate (xyz {} {} {}))\n  )",
        path, ox, oy, oz, sx, sy, sz, rx, ry, rz
    )
}

/// Build the courtyard, silkscreen, fab outline, reference/value text, and the
/// pin-1 marker (silk dot + fab chamfer) for a footprint from its pad geometry.
fn build_footprint_graphics(args: &serde_json::Value, name: &str, pads: &[PadGeom]) -> String {
    let (pmin_x, pmin_y, pmax_x, pmax_y) = pads_bbox(pads);

    let body = match (args["body_width"].as_f64(), args["body_height"].as_f64()) {
        (Some(bw), Some(bh)) => Some((bw, bh)),
        _ => None,
    };
    let clearance = courtyard_clearance(
        args["courtyard_clearance"].as_f64(),
        args["package_type"].as_str(),
        pads,
        body,
    );

    // Courtyard: pad envelope + clearance.
    let (cmin_x, cmin_y, cmax_x, cmax_y) = (
        pmin_x - clearance,
        pmin_y - clearance,
        pmax_x + clearance,
        pmax_y + clearance,
    );

    // Silk: just outside the pad envelope so it clears pads (avoids the
    // silk-over-pad DRC violation) regardless of the body outline.
    let silk_margin = 0.15;
    let (smin_x, smin_y, smax_x, smax_y) = (
        pmin_x - silk_margin,
        pmin_y - silk_margin,
        pmax_x + silk_margin,
        pmax_y + silk_margin,
    );

    // Fab: the component body when given, else the pad envelope. May overlap
    // pads — fab is a documentation layer, not subject to silk-over-pad rules.
    let (fmin_x, fmin_y, fmax_x, fmax_y) = match body {
        Some((bw, bh)) => {
            let cx = (pmin_x + pmax_x) / 2.0;
            let cy = (pmin_y + pmax_y) / 2.0;
            (cx - bw / 2.0, cy - bh / 2.0, cx + bw / 2.0, cy + bh / 2.0)
        }
        None => (pmin_x, pmin_y, pmax_x, pmax_y),
    };

    let mut s = String::new();

    // Courtyard rectangle (F.CrtYd) — required for DRC.
    s.push_str(&format!(
        "\n  (fp_rect (start {:.4} {:.4}) (end {:.4} {:.4}) (stroke (width 0.05) (type solid)) (fill none) (layer \"F.CrtYd\"))",
        cmin_x, cmin_y, cmax_x, cmax_y
    ));
    // Silkscreen outline (F.SilkS).
    s.push_str(&format!(
        "\n  (fp_rect (start {:.4} {:.4}) (end {:.4} {:.4}) (stroke (width 0.12) (type solid)) (fill none) (layer \"F.SilkS\"))",
        smin_x, smin_y, smax_x, smax_y
    ));

    if let Some(i1) = pin1_index(pads) {
        let p1 = &pads[i1];

        // Fab outline with the pin-1 corner chamfered.
        let chamfer = (0.25 * (fmax_x - fmin_x).min(fmax_y - fmin_y)).clamp(0.3, 1.0);
        let pts = chamfered_rect_points(fmin_x, fmin_y, fmax_x, fmax_y, p1.x, p1.y, chamfer);
        let pts_str: String = pts
            .iter()
            .map(|(x, y)| format!("(xy {:.4} {:.4}) ", x, y))
            .collect();
        s.push_str(&format!(
            "\n  (fp_poly (pts {}) (stroke (width 0.1) (type solid)) (fill none) (layer \"F.Fab\"))",
            pts_str.trim()
        ));

        // Silk pin-1 dot just outside the silk outline, aligned with pin 1's
        // pad — NOT at the footprint corner, where a dot is ambiguous between
        // pin 1 and the last pin that shares the same corner. It sits directly
        // beside pin 1 so the mark is unmistakable.
        let bcx = (pmin_x + pmax_x) / 2.0;
        let bcy = (pmin_y + pmax_y) / 2.0;
        let (dx, dy) = if (p1.x - bcx).abs() >= (p1.y - bcy).abs() {
            // Pin 1 is on a left/right edge: dot outside that edge, at pin 1's y.
            let sign = if p1.x < bcx { -1.0 } else { 1.0 };
            let edge = if sign < 0.0 { smin_x } else { smax_x };
            (edge + sign * 0.4, p1.y)
        } else {
            // Pin 1 is on a top/bottom edge: dot outside that edge, at pin 1's x.
            let sign = if p1.y < bcy { -1.0 } else { 1.0 };
            let edge = if sign < 0.0 { smin_y } else { smax_y };
            (p1.x, edge + sign * 0.4)
        };
        s.push_str(&format!(
            "\n  (fp_circle (center {:.4} {:.4}) (end {:.4} {:.4}) (stroke (width 0.1) (type solid)) (fill solid) (layer \"F.SilkS\"))",
            dx, dy, dx + 0.15, dy
        ));
    } else {
        // No pads to mark pin 1 against — plain fab rectangle.
        s.push_str(&format!(
            "\n  (fp_rect (start {:.4} {:.4}) (end {:.4} {:.4}) (stroke (width 0.1) (type solid)) (fill none) (layer \"F.Fab\"))",
            fmin_x, fmin_y, fmax_x, fmax_y
        ));
    }

    // Reference (F.SilkS, above) and value (F.Fab, below).
    let cx = (pmin_x + pmax_x) / 2.0;
    s.push_str(&format!(
        "\n  (fp_text reference \"REF**\" (at {:.4} {:.4} 0) (layer \"F.SilkS\") (effects (font (size 1 1) (thickness 0.15))))",
        cx, cmin_y - 1.0
    ));
    s.push_str(&format!(
        "\n  (fp_text value \"{}\" (at {:.4} {:.4} 0) (layer \"F.Fab\") (effects (font (size 1 1) (thickness 0.15))))",
        name, cx, cmax_y + 1.0
    ));

    s
}

async fn handle_create_footprint(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let output = get_path(args, "output")?;
    // Both are schema-required. Defaulting them meant a call with neither
    // wrote `(footprint "Footprint" …)` with no pads, no courtyard, no
    // silkscreen and no fab outline — through `write_atomic`, which replaces
    // unconditionally — over whatever `.kicad_mod` was already at `output`,
    // and returned `{"success": true, "pad_count": 0}` (#218).
    let name = match require_str(args, "name") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let description = args["description"].as_str().unwrap_or("");

    let pads_val = match require_array(args, "pads") {
        Ok(v) => v.clone(),
        Err(e) => return Ok(e),
    };
    let mut pad_geoms: Vec<PadGeom> = Vec::new();
    let mut pad_sexp = String::new();
    for pad in &pads_val {
        let number = pad["number"].as_str().unwrap_or("1").to_string();
        let pad_type = pad["type"].as_str().unwrap_or("smd").to_string();
        let shape = pad["shape"].as_str().unwrap_or("rect");
        let x = pad["x"].as_f64().unwrap_or(0.0);
        let y = pad["y"].as_f64().unwrap_or(0.0);
        let w = pad["width"].as_f64().unwrap_or(1.0);
        let h = pad["height"].as_f64().unwrap_or(1.0);

        let layer_names = if let Some(layers) = pad["layers"].as_array() {
            let mut names = Vec::with_capacity(layers.len());
            for layer in layers {
                let layer = layer
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("pad layers must contain strings"))?;
                if !matches!(
                    layer,
                    "F.Cu"
                        | "B.Cu"
                        | "F.Paste"
                        | "B.Paste"
                        | "F.Mask"
                        | "B.Mask"
                        | "*.Cu"
                        | "*.Mask"
                ) {
                    anyhow::bail!("invalid pad layer: {layer}");
                }
                names.push(layer);
            }
            if names.is_empty() {
                anyhow::bail!("pad layers must not be empty");
            }
            names
        } else if pad_type == "smd" {
            vec!["F.Cu", "F.Paste", "F.Mask"]
        } else {
            vec!["*.Cu", "*.Mask"]
        };
        let layers = format!(
            "(layers {})",
            layer_names
                .iter()
                .map(|layer| format!("\"{layer}\""))
                .collect::<Vec<_>>()
                .join(" ")
        );

        let rotation = pad["rotation"].as_f64().unwrap_or(0.0);
        let at = if rotation == 0.0 {
            format!("(at {x} {y})")
        } else {
            format!("(at {x} {y} {rotation})")
        };

        let roundrect_ratio = if let Some(ratio) = pad["roundrect_rratio"].as_f64() {
            if !(0.0..=0.5).contains(&ratio) {
                anyhow::bail!("roundrect_rratio must be between 0 and 0.5");
            }
            format!("(roundrect_rratio {ratio})")
        } else {
            String::new()
        };

        let drill_sexp = if let Some(drill) = pad["drill"].as_f64() {
            format!("(drill {})", drill)
        } else {
            String::new()
        };

        pad_sexp.push_str(&format!(
            "\n  (pad \"{}\" {} {} {} (size {} {}) {} {} {})",
            number, pad_type, shape, at, w, h, layers, drill_sexp, roundrect_ratio
        ));
        pad_geoms.push(PadGeom {
            number,
            pad_type,
            x,
            y,
            w,
            h,
        });
    }

    // Courtyard, silk, fab, text, and pin-1 marker, derived from pad geometry.
    let graphics = if pad_geoms.is_empty() {
        String::new()
    } else {
        build_footprint_graphics(args, name, &pad_geoms)
    };
    let model_sexp = build_model_sexp(args);

    let attr = if pad_geoms.iter().any(|p| p.pad_type == "smd") {
        "smd"
    } else {
        "through_hole"
    };

    let content = format!(
        "(footprint \"{}\"\n  (version 20240108)\n  (generator \"konnect\")\n  (layer \"F.Cu\")\n  (descr \"{}\")\n  (attr {}){}{}{}\n)",
        name, description, attr, pad_sexp, graphics, model_sexp
    );

    // Ensure parent directory exists
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(&output, &content)?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "footprint": name,
            "output": output.to_str().unwrap_or(""),
            "pad_count": pad_geoms.len(),
            "courtyard": !pad_geoms.is_empty(),
            "pin1_marked": !pad_geoms.is_empty(),
            "model": args.get("model").and_then(|m| m["path"].as_str()).unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_edit_footprint_pad(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "footprint_path")?;
    let pad_number = match require_str(args, "pad_number") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let new_number = match args.get("new_number") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => match value.as_str() {
            Some(number) => Some(number),
            None => {
                return Ok(invalid_library_argument(
                    "new_number",
                    "must be a string when supplied",
                ))
            }
        },
    };
    let match_all = match args.get("match_all") {
        None | Some(serde_json::Value::Null) => false,
        Some(value) => match value.as_bool() {
            Some(match_all) => match_all,
            None => {
                return Ok(invalid_library_argument(
                    "match_all",
                    "must be a boolean when supplied",
                ))
            }
        },
    };
    let shape = match args.get("shape") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => match value.as_str() {
            Some(shape) if matches!(shape, "circle" | "rect" | "oval" | "roundrect") => {
                Some(shape)
            }
            Some(shape) => {
                return Ok(invalid_library_argument(
                    "shape",
                    format!(
                        "unsupported standard shape '{shape}'; expected circle, rect, oval, or roundrect"
                    ),
                ))
            }
            None => {
                return Ok(invalid_library_argument(
                    "shape",
                    "must be a string when supplied",
                ))
            }
        },
    };

    let content = read_consistent(&path)?;
    match parse_sexp(&content) {
        Ok(root) if root.head() == Some("footprint") => {}
        Ok(root) => {
            return Ok(invalid_library_argument(
                "footprint_path",
                crate::tools::footprint_root_reason(root.head()),
            ))
        }
        Err(error) => {
            return Ok(invalid_library_argument(
                "footprint_path",
                format!("footprint is malformed: {error}"),
            ))
        }
    }

    let mut edits = Vec::new();
    let mut matched_count = 0usize;
    for (start, end) in find_direct_child_blocks(&content, "footprint") {
        let block = &content[start..end];
        let Ok(node) = parse_sexp(block) else {
            return Ok(invalid_library_argument(
                "footprint_path",
                "footprint contains a malformed direct child",
            ));
        };
        if node.head() != Some("pad") || node.get(1).and_then(SexpNode::as_str) != Some(pad_number)
        {
            continue;
        }

        matched_count += 1;
        let edited = match edit_footprint_pad_block(block, args, new_number, shape) {
            Ok(edited) => edited,
            Err(reason) => return Ok(invalid_library_argument("shape", reason)),
        };
        edits.push(SexpEdit::replace(start, end, edited));
        if !match_all {
            break;
        }
    }

    if matched_count == 0 {
        return Ok(invalid_library_argument(
            "pad_number",
            format!("pad '{pad_number}' was not found"),
        ));
    }

    let new_content = apply_edits(content.clone(), edits);
    write_atomic_if_unchanged(&path, &content, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "pad": pad_number,
            "shape": shape,
            "matched_count": matched_count,
            "updated_count": matched_count
        }))
        .unwrap(),
    ))
}

fn invalid_library_argument(field: &str, reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.clone(),
        },
        format!("Argument '{field}' is invalid: {reason}"),
    )
}

fn skip_pad_header_token(source: &str, mut index: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    let start = index;
    if bytes.get(index) == Some(&b'"') {
        index += 1;
        while index < bytes.len() {
            if bytes[index] == b'\\' {
                index += 2;
            } else if bytes[index] == b'"' {
                return Some((start, index + 1));
            } else {
                index += 1;
            }
        }
        return None;
    }
    while bytes
        .get(index)
        .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b')')
    {
        index += 1;
    }
    (index > start).then_some((start, index))
}

fn pad_shape_range(pad_block: &str) -> Option<(usize, usize)> {
    let open = pad_block.find('(')? + 1;
    let (_, pad_end) = skip_pad_header_token(pad_block, open)?;
    if &pad_block[open..pad_end] != "pad" {
        return None;
    }
    let (_, number_end) = skip_pad_header_token(pad_block, pad_end)?;
    let (_, type_end) = skip_pad_header_token(pad_block, number_end)?;
    skip_pad_header_token(pad_block, type_end)
}

fn remove_pad_shape_children(pad_block: String, keep_roundrect_ratio: bool) -> String {
    let mut edits = Vec::new();
    for (start, end) in find_direct_child_blocks(&pad_block, "pad") {
        let Ok(child) = parse_sexp(&pad_block[start..end]) else {
            continue;
        };
        let remove = match child.head() {
            Some("roundrect_rratio") => !keep_roundrect_ratio,
            Some("chamfer_ratio" | "chamfer" | "rect_delta" | "options" | "primitives") => true,
            _ => false,
        };
        if remove {
            edits.push(SexpEdit::delete(start, end));
        }
    }
    apply_edits(pad_block, edits)
}

fn has_direct_pad_child(pad_block: &str, head: &str) -> bool {
    find_direct_child_blocks(pad_block, "pad")
        .into_iter()
        .filter_map(|(start, end)| parse_sexp(&pad_block[start..end]).ok())
        .any(|child| child.head() == Some(head))
}

fn edit_footprint_pad_block(
    pad_block: &str,
    args: &serde_json::Value,
    new_number: Option<&str>,
    shape: Option<&str>,
) -> Result<String, String> {
    let mut new_pad = pad_block.to_string();

    if let Some(number) = new_number {
        if let Some(number_start) = new_pad.find('"') {
            if let Some(number_end) = new_pad[number_start + 1..].find('"') {
                let number_end = number_start + 1 + number_end;
                new_pad.replace_range(number_start + 1..number_end, &escape_library_string(number));
            }
        }
    }

    if let Some(shape) = shape {
        let (shape_start, shape_end) = pad_shape_range(&new_pad)
            .ok_or_else(|| "could not locate the pad shape token".to_string())?;
        new_pad.replace_range(shape_start..shape_end, shape);
        let is_roundrect = shape == "roundrect";
        new_pad = remove_pad_shape_children(new_pad, is_roundrect);
        if is_roundrect && !has_direct_pad_child(&new_pad, "roundrect_rratio") {
            let insert_at = new_pad
                .rfind(')')
                .ok_or_else(|| "pad block has no closing parenthesis".to_string())?;
            let ratio = if new_pad.contains('\n') {
                "\n    (roundrect_rratio 0.25)"
            } else {
                " (roundrect_rratio 0.25)"
            };
            new_pad.insert_str(insert_at, ratio);
        }
    }

    if let Some(x) = args["x"].as_f64() {
        if let Some(at_pos) = new_pad.find("(at ") {
            let at_end = new_pad[at_pos..]
                .find(')')
                .map(|i| at_pos + i + 1)
                .unwrap_or(new_pad.len());
            let at_block = &new_pad[at_pos..at_end];
            let parts: Vec<&str> = at_block
                .trim_start_matches("(at ")
                .trim_end_matches(')')
                .split_whitespace()
                .collect();
            let old_y = parts
                .get(1)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let rot = parts.get(2).map(|s| format!(" {}", s)).unwrap_or_default();
            new_pad.replace_range(at_pos..at_end, &format!("(at {} {}{})", x, old_y, rot));
        }
    }
    if let Some(y) = args["y"].as_f64() {
        if let Some(at_pos) = new_pad.find("(at ") {
            let at_end = new_pad[at_pos..]
                .find(')')
                .map(|i| at_pos + i + 1)
                .unwrap_or(new_pad.len());
            let at_block = &new_pad[at_pos..at_end];
            let parts: Vec<&str> = at_block
                .trim_start_matches("(at ")
                .trim_end_matches(')')
                .split_whitespace()
                .collect();
            let old_x = parts
                .first()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let rot = parts.get(2).map(|s| format!(" {}", s)).unwrap_or_default();
            new_pad.replace_range(at_pos..at_end, &format!("(at {} {}{})", old_x, y, rot));
        }
    }
    if args["width"].as_f64().is_some() || args["height"].as_f64().is_some() {
        if let Some(size_pos) = new_pad.find("(size ") {
            let size_end = new_pad[size_pos..]
                .find(')')
                .map(|i| size_pos + i + 1)
                .unwrap_or(new_pad.len());
            let size_block = &new_pad[size_pos..size_end];
            let parts: Vec<&str> = size_block
                .trim_start_matches("(size ")
                .trim_end_matches(')')
                .split_whitespace()
                .collect();
            let old_width = parts
                .first()
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(0.0);
            let old_height = parts
                .get(1)
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(0.0);
            let width = args["width"].as_f64().unwrap_or(old_width);
            let height = args["height"].as_f64().unwrap_or(old_height);
            new_pad.replace_range(size_pos..size_end, &format!("(size {width} {height})"));
        }
    }
    if let Some(drill) = args["drill"].as_f64() {
        if let Some(drill_pos) = new_pad.find("(drill ") {
            let drill_end = new_pad[drill_pos..]
                .find(')')
                .map(|i| drill_pos + i + 1)
                .unwrap_or(new_pad.len());
            new_pad.replace_range(drill_pos..drill_end, &format!("(drill {drill})"));
        } else {
            let insert_at = new_pad.rfind(')').unwrap_or(new_pad.len());
            new_pad.insert_str(insert_at, &format!(" (drill {drill})"));
        }
    }

    Ok(new_pad)
}

fn escape_library_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

// ─── Library table helpers ────────────────────────────────────────────────────

/// Returns the path to the global fp-lib-table file.
fn global_fp_lib_table() -> PathBuf {
    super::kicad_config_dir().join("fp-lib-table")
}

/// Returns the path to the global sym-lib-table file.
fn global_sym_lib_table() -> PathBuf {
    super::kicad_config_dir().join("sym-lib-table")
}

/// Directory of the structurally proven project that owns `file`, falling back
/// to the file's own directory when it belongs to no project — a loose
/// schematic keeps resolving against the tables beside it.
///
/// The project file is found by scanning for the extension rather than by name:
/// a sheet's filename says nothing about what the project is called.
///
/// A library table sitting beside `file` is the most specific answer. An exact
/// sibling `<stem>.kicad_pro` is also authoritative. Otherwise a candidate
/// ancestor project is accepted only when its parsed root schematic reaches
/// this file. Multiple owners are returned as a typed ambiguity rather than a
/// directory-enumeration-order guess (#189).
pub(crate) fn project_root_for(
    file: &Path,
) -> Result<Option<PathBuf>, crate::tools::SchematicTargetError> {
    let Some(start) = file.parent() else {
        return Ok(None);
    };
    if holds_lib_table(start) {
        return Ok(Some(start.to_path_buf()));
    }
    if file.with_extension("kicad_pro").is_file() {
        return Ok(Some(start.to_path_buf()));
    }
    Ok(crate::tools::resolve_schematic_ownership(file)?
        .and_then(|ownership| ownership.project_file.parent().map(Path::to_path_buf))
        .or_else(|| Some(start.to_path_buf())))
}

/// Whether `dir` carries a library table of its own.
fn holds_lib_table(dir: &Path) -> bool {
    dir.join("sym-lib-table").is_file() || dir.join("fp-lib-table").is_file()
}

/// Symbol libraries resolved as KiCad does: project `sym-lib-table` (shadowing
/// same-nickname global entries), then global, then the conventional
/// `<nickname>.kicad_symdir` / `.kicad_sym` layout. Same order as
/// [`resolve_footprint_path`]; the last step covers a missing global table.
///
/// Both the flattened tables and the install dirs are memoised: placing one
/// component asks for candidates at least twice, and the default global table
/// is a `(type "Table")` indirection to ~200 bundled entries, each of which
/// re-probes the install roots when `${KICAD*_DIR}` is unset.
pub(crate) struct KiCadSymbolSource {
    project_dir: Option<PathBuf>,
    tables: std::sync::OnceLock<Vec<serde_json::Value>>,
    install_dirs: std::sync::OnceLock<Vec<PathBuf>>,
}

impl KiCadSymbolSource {
    pub(crate) fn new(project_dir: Option<PathBuf>) -> Self {
        Self {
            project_dir,
            tables: std::sync::OnceLock::new(),
            install_dirs: std::sync::OnceLock::new(),
        }
    }

    /// For a `.kicad_sch` or `.kicad_pcb` — `sym-lib-table` sits at the project
    /// root, which is the nearest ancestor holding a `.kicad_pro`. A
    /// hierarchical sheet under `<proj>/sheets/` therefore still resolves
    /// against `<proj>/sym-lib-table`: KiCad anchors `KIPRJMOD` at the project,
    /// not at the sheet.
    pub(crate) fn for_file(file: &Path) -> Result<Self, crate::tools::SchematicTargetError> {
        Ok(Self::new(project_root_for(file)?))
    }

    /// Project entries first so they shadow same-nickname global ones.
    fn tables(&self) -> &[serde_json::Value] {
        self.tables.get_or_init(|| {
            let mut libs = Vec::new();
            if let Some(dir) = &self.project_dir {
                libs.extend(read_flat_lib_table(&dir.join("sym-lib-table")));
            }
            libs.extend(read_flat_lib_table(&global_sym_lib_table()));
            libs
        })
    }
}

impl konnect_schematic_editor::library::SymbolLibrarySource for KiCadSymbolSource {
    fn candidates(&self, nickname: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();

        for lib in self
            .tables()
            .iter()
            .filter(|l| l["nickname"].as_str() == Some(nickname))
        {
            if let Some(path) = lib["path"].as_str() {
                out.push(PathBuf::from(path));
            }
        }

        let install_dirs = self
            .install_dirs
            .get_or_init(|| super::find_kicad_library_dirs("symbols"));
        for base in install_dirs {
            out.push(base.join(format!("{}.kicad_symdir", nickname)));
            out.push(base.join(format!("{}.kicad_sym", nickname)));
        }
        out
    }
}

/// Parse a lib-table S-expression and return list of (nickname, uri, type) tuples.
///
/// Indentation-agnostic: KiCad's own writers emit tab-indented, CRLF-terminated
/// tables while this crate's writer uses two spaces, so a fixed literal such as
/// `"\n  (lib "` silently matches nothing in a real `fp-lib-table`.
///
/// Each entry is located textually and then *parsed*, so a field is read from
/// the tree rather than scraped out of the block: an escaped quote inside a
/// `descr` no longer truncates the value, and a field KiCad wrote across a line
/// break is still found. Locating entries textually keeps a malformed one from
/// discarding the rest of the table.
fn parse_lib_table(content: &str) -> Vec<serde_json::Value> {
    let mut libs = Vec::new();
    // Each entry: (lib (name "NICK") (type "...") (uri "...") (options "") (descr "..."))
    for start in find_block_starts(content, "lib") {
        let Some((block_start, block_end)) = find_balanced_block(content, start) else {
            continue;
        };
        let block = &content[block_start..block_end];
        let Ok(entry) = parse_sexp(block) else {
            tracing::warn!("lib-table entry at byte {block_start} does not parse — skipped");
            continue;
        };

        let field = |tag: &str| entry.find_str(tag).unwrap_or_default().to_string();

        libs.push(json!({
            "nickname": field("name"),
            "uri": field("uri"),
            "type": field("type"),
            "description": field("descr")
        }));
    }
    libs
}

/// Resolve a lib-table URI to a concrete path, expanding a leading
/// `${KICAD*_DIR}` reference.
///
/// KiCad's shipped tables address bundled libraries as
/// `${KICAD10_FOOTPRINT_DIR}/Resistor_SMD.pretty`. An exported environment
/// variable wins; otherwise the variable's kind is inferred from its name and
/// the known install locations are searched.
/// User path variables from `kicad_common.json` (Preferences → Configure
/// Paths).
///
/// These are KiCad's own variables, not process environment variables — KiCad
/// stores them in its config and substitutes them itself when reading a
/// lib-table, so `std::env::var` never finds them. A table written against one
/// is perfectly normal and is the recommended way to keep a table portable
/// across machines.
fn kicad_user_path_vars() -> std::collections::HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(super::kicad_config_dir().join("kicad_common.json"))
    else {
        return std::collections::HashMap::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return std::collections::HashMap::new();
    };
    json.get("environment")
        .and_then(|e| e.get("vars"))
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn expand_lib_uri(uri: &str, kiprjmod: Option<&Path>) -> Option<PathBuf> {
    let Some(rest) = uri.strip_prefix("${") else {
        return (!uri.is_empty()).then(|| PathBuf::from(uri));
    };
    let close = rest.find('}')?;
    let var = &rest[..close];
    let tail = rest[close + 1..].trim_start_matches(['/', '\\']);

    // ${KIPRJMOD} is the project directory — resolved from the table's own
    // location, not the environment: KiCad sets it per open project at
    // runtime, so an exported value (if any) may belong to a different
    // project than the table being read. Project-scoped registrations are
    // the default for register_footprint_library, so this is the common
    // case for user-registered libraries, not an edge.
    if var == "KIPRJMOD" {
        let p = kiprjmod?.join(tail);
        return p.exists().then_some(p);
    }

    // var_os, not var: `var` treats a non-Unicode value as absent, which would
    // send a perfectly good ${KICAD*_DIR} down the install-root guess path.
    if let Some(base) = std::env::var_os(var) {
        let p = PathBuf::from(base).join(tail);
        if p.exists() {
            return Some(p);
        }
    }

    // User path variables (Preferences → Configure Paths) are stored in
    // kicad_common.json and are NOT process environment variables, so the
    // var_os lookup above can never see them. They are the normal way to write
    // a portable lib-table — `${MY_PARTS}/house.kicad_sym` — and without this
    // every such entry resolved to None and the library was invisible.
    // Credit to @JYPochez (#172) for finding this.
    if let Some(base) = kicad_user_path_vars().get(var) {
        let p = PathBuf::from(base).join(tail);
        if p.exists() {
            return Some(p);
        }
    }

    // e.g. KICAD10_FOOTPRINT_DIR -> "footprints"
    let kind = if var.ends_with("_FOOTPRINT_DIR") {
        "footprints"
    } else if var.ends_with("_SYMBOL_DIR") {
        "symbols"
    } else if var.ends_with("_3DMODEL_DIR") {
        "3dmodels"
    } else {
        return None;
    };

    super::find_kicad_library_dirs(kind)
        .into_iter()
        .map(|base| base.join(tail))
        .find(|p| p.exists())
}

/// Maximum depth when following nested `(type "Table")` lib-table references.
const MAX_LIB_TABLE_DEPTH: usize = 4;

/// Parse a lib-table and return concrete libraries, following nested tables.
///
/// KiCad 10 no longer copies its ~155 bundled libraries into the user's table.
/// The default global table instead holds a single indirection entry —
/// `(lib (name "KiCad") (type "Table") (uri ".../template/fp-lib-table"))` —
/// pointing at the shipped template table. Treating that entry as a library
/// makes every bundled library invisible, so it is followed here.
///
/// Each returned entry carries the original `uri` plus a resolved `path`
/// whenever [`expand_lib_uri`] yields one: a `${KICAD*_DIR}` URI resolves only
/// if the expansion exists on disk, while a plain URI is passed through as
/// written. The target may be a directory (`.pretty`) or a file
/// (`.kicad_sym`), so the presence of `path` is not a promise that the library
/// is readable — only that the URI was understood.
fn flatten_lib_table(
    content: &str,
    depth: usize,
    kiprjmod: Option<&Path>,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();

    for mut entry in parse_lib_table(content) {
        let uri = entry["uri"].as_str().unwrap_or("").to_string();
        let is_nested = entry["type"].as_str() == Some("Table");

        if is_nested {
            if depth >= MAX_LIB_TABLE_DEPTH {
                tracing::warn!(
                    "lib-table nesting deeper than {} levels at '{}' — not followed",
                    MAX_LIB_TABLE_DEPTH,
                    uri
                );
                continue;
            }
            match expand_lib_uri(&uri, kiprjmod).map(std::fs::read_to_string) {
                Some(Ok(nested)) => out.extend(flatten_lib_table(&nested, depth + 1, kiprjmod)),
                _ => tracing::warn!("nested lib-table '{}' could not be read", uri),
            }
            continue;
        }

        if let Some(path) = expand_lib_uri(&uri, kiprjmod) {
            entry["path"] = json!(path.to_string_lossy());
        }
        out.push(entry);
    }

    out
}

/// Read a lib-table file from disk and flatten it, reporting a table that is
/// present but unreadable.
///
/// An absent table is normal and yields an empty list: a project without its
/// own fp-lib-table simply has none, and every caller checks both the global
/// and project tables. Anything else — a permissions problem, a truncated
/// file — is not normal, and must not be folded into the same empty list. The
/// symptom that produces is a bare `{"count": 0}`, which is precisely what the
/// bug this module fixes looked like, so silence here would make a real
/// failure indistinguishable from a regression.
fn read_lib_table_checked(path: &Path) -> Result<Vec<serde_json::Value>, String> {
    match std::fs::read_to_string(path) {
        // ${KIPRJMOD} is the directory the project's lib-table lives in, so
        // the table's own parent IS the correct expansion base for a project
        // table. For the global table the parent is KiCad's config dir, where
        // a ${KIPRJMOD} entry would be authoring error to begin with — the
        // expansion then simply fails its exists() check.
        Ok(content) => Ok(flatten_lib_table(&content, 0, path.parent())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("Cannot read lib-table {}: {}", path.display(), e)),
    }
}

/// As [`read_lib_table_checked`], for callers with nowhere to put an error.
///
/// The failure is logged rather than dropped in silence. Handlers that can
/// surface it to the user should call `read_lib_table_checked` directly.
fn read_flat_lib_table(path: &Path) -> Vec<serde_json::Value> {
    match read_lib_table_checked(path) {
        Ok(libs) => libs,
        Err(msg) => {
            tracing::warn!("{msg}");
            Vec::new()
        }
    }
}

/// Whether a footprint reference is KiCad's `Library:Footprint` form rather
/// than a filesystem path.
///
/// "Contains a colon" is not enough, because Windows paths contain one too.
/// `C:\libs\R.kicad_mod` is caught by the separator test, but the
/// drive-*relative* form `C:R.kicad_mod` — meaning `R.kicad_mod` in the current
/// directory of drive C — carries no separator and is otherwise shaped exactly
/// like a lib id.
///
/// A one-letter prefix is therefore read as a drive letter rather than a
/// nickname. Nothing distinguishes the two, so this is a choice: a drive letter
/// is much the likelier reading, and guessing the other way means silently
/// hunting for a library named "C". The cost is that a single-letter nickname
/// cannot be written in this form — it is still reachable by path — and the
/// rule is applied on every platform so the behaviour does not change under
/// the caller's feet.
pub(crate) fn is_lib_id(reference: &str) -> bool {
    let Some((nick, _)) = reference.split_once(':') else {
        return false;
    };
    if reference.contains('/') || reference.contains('\\') {
        return false;
    }
    !(nick.len() == 1 && nick.as_bytes()[0].is_ascii_alphabetic())
}

/// The nickname the fp-lib-table gives to the library living in `dir`, if any.
///
/// This is the inverse of `resolve_footprint_path` and exists because a
/// nickname is *not* derivable from the directory name: KiCad lets a table map
/// any nickname to any path, so `MyParts` may well point at `vendor.pretty`,
/// and two nicknames may share one directory. Only the table can answer it.
///
/// Paths are compared canonicalised so a symlinked or non-normalised entry
/// still matches, falling back to a literal comparison when canonicalisation
/// fails (a directory that no longer exists, say).
pub(crate) fn footprint_lib_nickname_for_dir(dir: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(dir).ok();
    let same = |candidate: &Path| -> bool {
        match (&canonical, std::fs::canonicalize(candidate).ok()) {
            (Some(a), Some(b)) => a == &b,
            _ => candidate == dir,
        }
    };

    read_flat_lib_table(&global_fp_lib_table())
        .into_iter()
        .find(|lib| lib["path"].as_str().is_some_and(|p| same(Path::new(p))))
        .and_then(|lib| lib["nickname"].as_str().map(str::to_string))
}

/// Resolve a footprint reference to an on-disk `.kicad_mod` path.
///
/// Accepts either a direct filesystem path or KiCad's `Library:Footprint`
/// form. Returns a human-readable message on failure so callers can surface it
/// verbatim.
///
/// A lib id is looked up in `project_dir`'s fp-lib-table first, then the
/// global one, and finally the conventional `<nickname>.pretty` layout under
/// the bundled library directories. Project-first matches KiCad, where a
/// project entry shadows a global one of the same nickname, and it is the only
/// order that makes `register_footprint_library` useful — it writes to the
/// project table by default, so a global-only lookup cannot see anything it
/// registers. The `.pretty` fallback covers a stock install whose global
/// table is missing or unreadable.
///
/// Symbols resolve the same way and in the same order, via
/// [`KiCadSymbolSource`] and `resolve_symbol_lib_path`.
pub(crate) fn resolve_footprint_path(
    reference: &str,
    project_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    if !is_lib_id(reference) {
        // Check here rather than leaving it to the caller's read: an unchecked
        // path reaches the reader as a bare io::Error, which surfaces as
        // "The system cannot find the file specified. (os error 2)" with no
        // mention of what was being looked for.
        let path = PathBuf::from(reference);
        if !path.is_file() {
            return Err(format!(
                "Footprint file not found: {}. Pass either a path to a .kicad_mod \
                 file or a Library:Footprint id (e.g. 'Resistor_SMD:R_0402').",
                path.display()
            ));
        }
        return Ok(path);
    }

    let (nick, fp_name) = reference.split_once(':').expect("checked above");
    let filename = format!("{fp_name}.kicad_mod");

    // Project table first: its entries shadow same-nickname global ones.
    let mut libs = Vec::new();
    if let Some(project) = project_dir.map(|d| d.join("fp-lib-table")) {
        libs.extend(read_flat_lib_table(&project));
    }
    libs.extend(read_flat_lib_table(&global_fp_lib_table()));

    if let Some(lib) = libs.iter().find(|l| l["nickname"].as_str() == Some(nick)) {
        let Some(dir) = lib["path"].as_str() else {
            return Err(format!(
                "Library '{}' has an unresolvable URI '{}'",
                nick,
                lib["uri"].as_str().unwrap_or("")
            ));
        };
        let path = PathBuf::from(dir).join(&filename);
        if !path.is_file() {
            return Err(format!(
                "Footprint '{}' not found in library '{}' (looked for {})",
                fp_name,
                nick,
                path.display()
            ));
        }
        return Ok(path);
    }

    // Not in any table — fall back to the conventional `<nickname>.pretty`
    // layout under the discovered KiCad library directories.
    let attempted: Vec<PathBuf> = super::find_kicad_library_dirs("footprints")
        .into_iter()
        .map(|base| base.join(format!("{nick}.pretty")).join(&filename))
        .collect();
    if let Some(path) = attempted.iter().find(|p| p.is_file()) {
        return Ok(path.clone());
    }

    let known: Vec<&str> = libs
        .iter()
        .filter_map(|l| l["nickname"].as_str())
        .take(12)
        .collect();
    let attempted_list = if attempted.is_empty() {
        "no KiCad library directories were found — set KICAD10_FOOTPRINT_DIR for a \
         non-standard install"
            .to_string()
    } else {
        format!(
            "also looked for {}",
            attempted
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Err(format!(
        "Library '{}' not found in the project or global fp-lib-table ({} libraries known{}); {}",
        nick,
        libs.len(),
        if known.is_empty() {
            String::new()
        } else {
            format!(", e.g. {}", known.join(", "))
        },
        attempted_list
    ))
}

async fn handle_register_footprint_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let nickname = match require_str(args, "nickname") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let scope = args["scope"].as_str().unwrap_or("project");
    let replace_existing = match args.get("replace_existing") {
        None | Some(serde_json::Value::Null) => false,
        Some(value) => match value.as_bool() {
            Some(value) => value,
            None => {
                return Ok(invalid_library_argument(
                    "replace_existing",
                    "must be a boolean when supplied",
                ))
            }
        },
    };

    let (table_path, uri) = match lib_table_target(
        scope,
        args["project"].as_str(),
        &lib_path,
        global_fp_lib_table(),
        "fp-lib-table",
    ) {
        Ok(target) => target,
        Err(e) => return Ok(e),
    };

    let registration = match register_in_lib_table_with_policy(
        &table_path,
        nickname,
        &uri,
        "KiCad",
        replace_existing,
    )
    .await?
    {
        Ok(registration) => registration,
        Err(error) => return Ok(invalid_library_argument(&error.field, error.reason)),
    };

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "nickname": nickname,
            "scope": scope,
            "table": table_path.to_str().unwrap_or(""),
            "uri": uri,
            "state": registration.state.as_str(),
            "repaired_table_root": registration.repaired_root
        }))
        .unwrap(),
    ))
}

async fn handle_list_footprint_libraries(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let scope = args["scope"].as_str().unwrap_or("all");
    let mut all_libs = Vec::new();

    // A table that exists but cannot be read is reported rather than counted
    // as zero libraries — "0" is the symptom of the bug this PR fixes, so the
    // two must not look alike.
    if scope == "global" || scope == "all" {
        let mut libs = match read_lib_table_checked(&global_fp_lib_table()) {
            Ok(libs) => libs,
            Err(msg) => return Ok(CallToolResult::error(msg)),
        };
        for lib in &mut libs {
            lib["scope"] = json!("global");
        }
        all_libs.extend(libs);
    }

    if (scope == "project" || scope == "all") && args["project"].is_string() {
        let proj = PathBuf::from(args["project"].as_str().unwrap());
        let table = proj.parent().unwrap_or(Path::new(".")).join("fp-lib-table");
        let mut libs = match read_lib_table_checked(&table) {
            Ok(libs) => libs,
            Err(msg) => return Ok(CallToolResult::error(msg)),
        };
        for lib in &mut libs {
            lib["scope"] = json!("project");
        }
        all_libs.extend(libs);
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "count": all_libs.len(),
            "libraries": all_libs
        }))
        .unwrap(),
    ))
}

/// A lib-table path written the way KiCad writes one: forward slashes on every
/// platform, and without the Windows verbatim prefix.
///
/// `canonicalize` on Windows returns `\\?\C:\…`. That prefix is an OS-level
/// escape for the 260-character limit, not part of the path — KiCad neither
/// writes nor expects it, and a table carrying one is not portable. Backslashes
/// are normalised for the same reason: a URI written on Windows has to still
/// resolve when the project is opened on Linux or macOS.
/// Credit to @anyn99 (#163), whose PR carried this and the lexical fallback
/// below.
fn portable_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    stripped.replace('\\', "/")
}

/// A lib-table URI for `lib_path`: `${KIPRJMOD}/…` when it lives inside the
/// project, so the project survives being moved or cloned. Anything outside
/// keeps its absolute path.
fn project_relative_uri(lib_path: &Path, table_dir: &Path) -> String {
    // An empty table dir must never reach `strip_prefix`: it succeeds and
    // returns the whole path, which would emit a `${KIPRJMOD}//abs/path` that
    // resolves nowhere on read.
    if table_dir.as_os_str().is_empty() {
        return portable_uri(lib_path);
    }

    let relative = |rel: &Path| format!("${{KIPRJMOD}}/{}", portable_uri(rel));

    // Canonicalising both sides is the accurate comparison — it resolves
    // symlinks, `..` and case differences — but it only works for paths that
    // already exist. Registering a library before creating it is normal, so a
    // failure here falls through to a lexical compare rather than giving up on
    // portability.
    if let (Ok(lib), Ok(dir)) = (
        std::fs::canonicalize(lib_path),
        std::fs::canonicalize(table_dir),
    ) {
        if let Ok(rel) = lib.strip_prefix(&dir) {
            return relative(rel);
        }
        // Canonicalised and genuinely outside the project.
        return portable_uri(&lib);
    }

    match lib_path.strip_prefix(table_dir) {
        Ok(rel) => relative(rel),
        Err(_) => portable_uri(lib_path),
    }
}

fn repository_relative_uri(
    lib_path: &Path,
    table_dir: &Path,
    project_path: &Path,
) -> Option<String> {
    if !project_path.is_file()
        || project_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("kicad_pro")
    {
        return None;
    }
    let repository = table_dir
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())?;
    let repository = std::fs::canonicalize(repository).ok()?;
    let table_dir = std::fs::canonicalize(table_dir).ok()?;
    let library = std::fs::canonicalize(lib_path).ok()?;
    if !table_dir.starts_with(&repository) || !library.starts_with(&repository) {
        return None;
    }
    let relative = lexical_relative_path(&table_dir, &library)?;
    Some(format!("${{KIPRJMOD}}/{}", portable_uri(&relative)))
}

fn lexical_relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let from_components: Vec<_> = from.components().collect();
    let to_components: Vec<_> = to.components().collect();
    if from_components.first() != to_components.first() {
        return None;
    }
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in &from_components[common..] {
        if matches!(component, Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &to_components[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

/// Which lib-table a `register_*` call writes to, and the URI it records.
///
/// Shared by the symbol and footprint registrars so the two cannot drift: both
/// need the same `${KIPRJMOD}` portability and the same empty-parent guard.
/// Resolve the caller's `project` argument to the directory whose library table
/// KiCad actually reads.
///
/// The schema documents a `.kicad_pro` path, but every other tool on this
/// surface takes a project directory and callers pass one. Taking `parent()`
/// unconditionally then lands one level *above* the project — a directory that
/// holds no `.kicad_pro` and that KiCad never consults. The write succeeds
/// there and reports `inserted`, so two projects sharing a parent end up
/// sharing one stray table that neither of them reads.
///
/// A path that is neither an existing directory nor recognisable as a file is
/// refused rather than guessed at, because guessing is what produced the stray
/// table.
fn project_table_dir(project: &str) -> Result<PathBuf, CallToolResult> {
    let path = PathBuf::from(project);
    if path.is_dir() {
        return Ok(path);
    }
    let looks_like_a_file =
        path.is_file() || path.extension().is_some_and(|ext| ext == "kicad_pro");
    if looks_like_a_file {
        // `Path::new("board.kicad_pro").parent()` is Some("") — an empty path,
        // not None — so the `.` default needs an explicit emptiness check.
        return Ok(path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf());
    }
    Err(CallToolResult::error(format!(
        "'project' must be a .kicad_pro file or an existing project directory, got '{project}'"
    )))
}

fn lib_table_target(
    scope: &str,
    project: Option<&str>,
    lib_path: &Path,
    global_table: PathBuf,
    table_filename: &str,
) -> Result<(PathBuf, String), CallToolResult> {
    if scope == "global" {
        return Ok((global_table, portable_uri(lib_path)));
    }
    let Some(proj) = project else {
        return Err(CallToolResult::error(
            "For project scope, provide 'project' path to .kicad_pro file",
        ));
    };
    let table_dir = project_table_dir(proj)?;
    let uri = project_relative_uri(lib_path, &table_dir);
    let uri = if uri.starts_with("${KIPRJMOD}") {
        uri
    } else {
        repository_relative_uri(lib_path, &table_dir, Path::new(proj)).unwrap_or(uri)
    };
    Ok((table_dir.join(table_filename), uri))
}

async fn handle_register_symbol_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let nickname = match require_str(args, "nickname") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let scope = args["scope"].as_str().unwrap_or("project");
    let replace_existing = match args.get("replace_existing") {
        None | Some(serde_json::Value::Null) => false,
        Some(value) => match value.as_bool() {
            Some(value) => value,
            None => {
                return Ok(invalid_library_argument(
                    "replace_existing",
                    "must be a boolean when supplied",
                ))
            }
        },
    };

    let (table_path, uri) = match lib_table_target(
        scope,
        args["project"].as_str(),
        &lib_path,
        global_sym_lib_table(),
        "sym-lib-table",
    ) {
        Ok(target) => target,
        Err(e) => return Ok(e),
    };

    // Same contract as register_footprint_library: report which of
    // inserted/unchanged/updated happened, rather than a bare "success" that
    // cannot be told apart from a silent no-op on an existing nickname (#211).
    let registration = match register_in_lib_table_with_policy(
        &table_path,
        nickname,
        &uri,
        "KiCad",
        replace_existing,
    )
    .await?
    {
        Ok(registration) => registration,
        Err(error) => return Ok(invalid_library_argument(&error.field, error.reason)),
    };

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "nickname": nickname,
            "scope": scope,
            "table": table_path.to_str().unwrap_or(""),
            "uri": uri,
            "state": registration.state.as_str(),
            "repaired_table_root": registration.repaired_root
        }))
        .unwrap(),
    ))
}

async fn handle_list_symbol_libraries(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let scope = args["scope"].as_str().unwrap_or("all");
    let mut all_libs = Vec::new();

    // Same as the footprint listing: an unreadable table is an error, not a
    // zero count.
    if scope == "global" || scope == "all" {
        let mut libs = match read_lib_table_checked(&global_sym_lib_table()) {
            Ok(libs) => libs,
            Err(msg) => return Ok(CallToolResult::error(msg)),
        };
        for lib in &mut libs {
            lib["scope"] = json!("global");
        }
        all_libs.extend(libs);
    }

    if (scope == "project" || scope == "all") && args["project"].is_string() {
        let proj = PathBuf::from(args["project"].as_str().unwrap());
        let table = proj
            .parent()
            .unwrap_or(Path::new("."))
            .join("sym-lib-table");
        let mut libs = match read_lib_table_checked(&table) {
            Ok(libs) => libs,
            Err(msg) => return Ok(CallToolResult::error(msg)),
        };
        for lib in &mut libs {
            lib["scope"] = json!("project");
        }
        all_libs.extend(libs);
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "count": all_libs.len(),
            "libraries": all_libs
        }))
        .unwrap(),
    ))
}

/// Root S-expression element for a lib-table file, decided by its filename:
/// `sym-lib-table` uses `sym_lib_table`, everything else (`fp-lib-table`)
/// uses `fp_lib_table`. Credit: first diagnosed in PR #54 (presire) — the
/// hardcoded `fp_lib_table` scaffold produced symbol tables KiCad rejects.
fn table_root_element(table_path: &Path) -> &'static str {
    let is_sym = table_path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains("sym"));
    if is_sym {
        "sym_lib_table"
    } else {
        "fp_lib_table"
    }
}

/// Correct a lib-table whose root element is the wrong kind, returning the
/// content and whether anything changed.
///
/// A `sym-lib-table` whose root says `(fp_lib_table` — or the reverse — is
/// rejected outright: *"Library table … has type FOOTPRINT but expected SYMBOL;
/// skipping"*. #54 stopped Konnect from **creating** such a file, but a table
/// an older build already wrote stays broken forever, because every later
/// registration appends into whatever root it finds. Repairing on write is what
/// makes the earlier fix reach existing projects.
///
/// Only a root element is rewritten: the wrong token has to be the first thing
/// in the file, so the same words appearing inside a `(descr …)` are left alone.
fn repair_table_root(content: String, expected_root: &str) -> (String, bool) {
    let wrong_root = if expected_root == "sym_lib_table" {
        "fp_lib_table"
    } else {
        "sym_lib_table"
    };
    let opening = format!("({wrong_root}");
    match content.find(&opening) {
        Some(pos) if content[..pos].trim().is_empty() => {
            let repaired = format!(
                "{}({expected_root}{}",
                &content[..pos],
                &content[pos + opening.len()..]
            );
            (repaired, true)
        }
        _ => (content, false),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibTableRegistrationState {
    Inserted,
    Unchanged,
    Updated,
}

impl LibTableRegistrationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::Unchanged => "unchanged",
            Self::Updated => "updated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LibTableRegistration {
    state: LibTableRegistrationState,
    repaired_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LibTableRegistrationError {
    field: String,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedLibTableRegistration {
    content: String,
    registration: LibTableRegistration,
}

/// Register without the replace policy, reporting only whether the table's
/// root element needed repairing.
///
/// Both handlers call `register_in_lib_table_with_policy` directly now that
/// each reports its own registration state (#211), so this is only the
/// narrower shape the root-repair tests are written against.
#[cfg(test)]
async fn register_in_lib_table(
    table_path: &Path,
    nickname: &str,
    uri: &str,
    lib_type: &str,
) -> anyhow::Result<bool> {
    let registration =
        register_in_lib_table_with_policy(table_path, nickname, uri, lib_type, false)
            .await?
            .map_err(|error| anyhow::anyhow!(error.reason))?;
    Ok(registration.repaired_root)
}

async fn register_in_lib_table_with_policy(
    table_path: &Path,
    nickname: &str,
    uri: &str,
    lib_type: &str,
    replace_existing: bool,
) -> anyhow::Result<Result<LibTableRegistration, LibTableRegistrationError>> {
    let root = table_root_element(table_path);
    let existing = table_path.exists();
    let source = if existing {
        read_consistent(table_path)?
    } else {
        format!("({root}\n  (version 7)\n)\n")
    };
    let prepared = match prepare_lib_table_registration(
        &source,
        root,
        nickname,
        uri,
        lib_type,
        replace_existing,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(Err(error)),
    };
    if prepared.registration.repaired_root {
        tracing::warn!(
            "Repaired the root element of {} — it declared the wrong table kind, so KiCad was skipping the whole table",
            table_path.display()
        );
    }

    if prepared.content != source {
        if let Some(parent) = table_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if existing {
            persist_lib_table_registration(table_path, &source, &prepared.content)?;
        } else {
            write_new_atomic(table_path, &prepared.content)?;
        }
    }
    Ok(Ok(prepared.registration))
}

fn prepare_lib_table_registration(
    source: &str,
    expected_root: &str,
    nickname: &str,
    uri: &str,
    lib_type: &str,
    replace_existing: bool,
) -> Result<PreparedLibTableRegistration, LibTableRegistrationError> {
    let (content, repaired_root) = repair_table_root(source.to_string(), expected_root);
    let parsed = parse_sexp(&content).map_err(|error| LibTableRegistrationError {
        field: "table".to_string(),
        reason: format!("invalid library table S-expression: {error}"),
    })?;
    if parsed.head() != Some(expected_root) {
        return Err(LibTableRegistrationError {
            field: "table".to_string(),
            reason: format!("root must be {expected_root}"),
        });
    }

    let mut matches = Vec::new();
    for (start, end) in find_direct_child_blocks(&content, expected_root) {
        let block = &content[start..end];
        let node = parse_sexp(block).map_err(|_| LibTableRegistrationError {
            field: "table".to_string(),
            reason: "contains a malformed direct child".to_string(),
        })?;
        if node.head() == Some("lib") && node.find_str("name") == Some(nickname) {
            matches.push((start, end));
        }
    }
    if matches.len() > 1 {
        return Err(LibTableRegistrationError {
            field: "nickname".to_string(),
            reason: format!("library table contains duplicate nickname '{nickname}'"),
        });
    }

    let (content, state) = if let Some((start, end)) = matches.first().copied() {
        if !replace_existing {
            (content, LibTableRegistrationState::Unchanged)
        } else {
            let block = &content[start..end];
            let current_uri = parse_sexp(block)
                .ok()
                .and_then(|node| node.find_str("uri").map(str::to_string))
                .ok_or_else(|| LibTableRegistrationError {
                    field: "table".to_string(),
                    reason: format!("entry '{nickname}' has no valid uri"),
                })?;
            if current_uri == uri {
                (content, LibTableRegistrationState::Unchanged)
            } else {
                let uri_range = find_direct_child_blocks(block, "lib")
                    .into_iter()
                    .find(|(child_start, child_end)| {
                        parse_sexp(&block[*child_start..*child_end])
                            .is_ok_and(|node| node.head() == Some("uri"))
                    })
                    .ok_or_else(|| LibTableRegistrationError {
                        field: "table".to_string(),
                        reason: format!("entry '{nickname}' has no uri block"),
                    })?;
                let replacement = format!("(uri {})", quote_lib_table_string(uri));
                let updated = apply_edits(
                    content,
                    vec![SexpEdit::replace(
                        start + uri_range.0,
                        start + uri_range.1,
                        replacement,
                    )],
                );
                (updated, LibTableRegistrationState::Updated)
            }
        }
    } else {
        let root_end = find_balanced_block(&content, 0)
            .map(|range| range.1 - 1)
            .ok_or_else(|| LibTableRegistrationError {
                field: "table".to_string(),
                reason: "unbalanced library table root".to_string(),
            })?;
        let entry = format!(
            "\n  (lib (name {}) (type {}) (uri {}) (options \"\") (descr \"\"))",
            quote_lib_table_string(nickname),
            quote_lib_table_string(lib_type),
            quote_lib_table_string(uri)
        );
        (
            apply_edits(content, vec![SexpEdit::insert(root_end, entry)]),
            LibTableRegistrationState::Inserted,
        )
    };

    parse_sexp(&content).map_err(|error| LibTableRegistrationError {
        field: "table".to_string(),
        reason: format!("updated library table does not parse: {error}"),
    })?;
    Ok(PreparedLibTableRegistration {
        content,
        registration: LibTableRegistration {
            state,
            repaired_root,
        },
    })
}

fn quote_lib_table_string(value: &str) -> String {
    format!("\"{}\"", escape_library_string(value))
}

fn persist_lib_table_registration(
    table_path: &Path,
    expected: &str,
    replacement: &str,
) -> Result<(), konnect_sexp::SexpError> {
    write_atomic_if_unchanged(table_path, expected, replacement)
}

// ─── Symbol library tools ─────────────────────────────────────────────────────

/// Minimal pin geometry for deriving the symbol body.
#[derive(Debug, Clone)]
struct PinGeom {
    x: f64,
    y: f64,
    angle: f64,
    length: f64,
    name: String,
}

/// The point where a pin meets the symbol body. In KiCAD symbols the pin's
/// connection endpoint (the "bulb", where wires attach) is at `(x, y)` and the
/// pin extends by `length` in its orientation to reach the body outline. Angles
/// are 0=E, 90=N, 180=W, 270=S with Y up, so the body-attach point (root) is
/// `(x + length*cos, y + length*sin)` — on the far side of the bulb.
fn pin_root(x: f64, y: f64, angle_deg: f64, length: f64) -> (f64, f64) {
    let a = angle_deg.to_radians();
    (x + length * a.cos(), y + length * a.sin())
}

/// The body edge a pin attaches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PinEdge {
    Left,
    Right,
    Bottom,
    Top,
}

/// Which edge a pin sits on, from its orientation (Y up): a pin pointing right
/// (0) sits on the left edge, left (180) on the right edge, up (90) on the
/// bottom edge, down (270) on the top edge. A degree of tolerance absorbs
/// callers whose 360 arithmetic lands just off. `None` for anything diagonal.
fn pin_edge(angle_deg: f64) -> Option<PinEdge> {
    let a = angle_deg.rem_euclid(360.0);
    let near = |t: f64| {
        let d = (a - t).abs();
        !(1.0..=359.0).contains(&d)
    };
    if near(0.0) {
        Some(PinEdge::Left)
    } else if near(180.0) {
        Some(PinEdge::Right)
    } else if near(90.0) {
        Some(PinEdge::Bottom)
    } else if near(270.0) {
        Some(PinEdge::Top)
    } else {
        None
    }
}

/// Body rectangle `(min_x, min_y, max_x, max_y)` for a symbol: edges that pins
/// attach to pass through those pins' roots (so each pin's far end touches the
/// border and its connection bulb sits outside), and edges with no pins are
/// pushed out by a margin so there is clear spacing beyond the outermost pins.
/// `None` when there are no pins.
fn symbol_body_rect(pins: &[PinGeom], show_names: bool) -> Option<(f64, f64, f64, f64)> {
    if pins.is_empty() {
        return None;
    }
    let roots: Vec<(f64, f64)> = pins
        .iter()
        .map(|p| pin_root(p.x, p.y, p.angle, p.length))
        .collect();
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for &(x, y) in &roots {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }

    // Which edges have pins attaching.
    let (mut has_left, mut has_right, mut has_bottom, mut has_top) = (false, false, false, false);
    for p in pins {
        match pin_edge(p.angle) {
            Some(PinEdge::Left) => has_left = true,
            Some(PinEdge::Right) => has_right = true,
            Some(PinEdge::Bottom) => has_bottom = true,
            Some(PinEdge::Top) => has_top = true,
            None => {}
        }
    }

    // Spacing beyond the last pin on any edge without attachments (~1 grid).
    let margin = 2.54;
    if !has_left {
        min_x -= margin;
    }
    if !has_right {
        max_x += margin;
    }
    if !has_bottom {
        min_y -= margin;
    }
    if !has_top {
        max_y += margin;
    }

    // Minimum visible body.
    let min_size = 2.54;
    if max_x - min_x < min_size {
        let c = (min_x + max_x) / 2.0;
        min_x = c - min_size / 2.0;
        max_x = c + min_size / 2.0;
    }
    if max_y - min_y < min_size {
        let c = (min_y + max_y) / 2.0;
        min_y = c - min_size / 2.0;
        max_y = c + min_size / 2.0;
    }

    // A box that only touches the pin roots is too small for the names KiCad
    // draws inside it: with long names the two columns run into each other.
    if show_names {
        grow_to(&mut min_x, &mut max_x, names_span(pins, Axis::Horizontal));
        grow_to(&mut min_y, &mut max_y, names_span(pins, Axis::Vertical));
    }
    Some((min_x, min_y, max_x, max_y))
}

/// The pair of facing edges a name span is measured across.
#[derive(Debug, Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

/// Outer size needed so the pin names on two facing edges never meet.
///
/// Pins level with each other share a row, which needs both names plus a gap.
/// Horizontally that pairs the left and right edges with rows keyed by Y;
/// vertically the bottom and top edges with rows keyed by X.
fn names_span(pins: &[PinGeom], axis: Axis) -> f64 {
    let (a, b) = match axis {
        Axis::Horizontal => (PinEdge::Left, PinEdge::Right),
        Axis::Vertical => (PinEdge::Bottom, PinEdge::Top),
    };
    let mut rows: std::collections::HashMap<i64, (f64, f64)> = std::collections::HashMap::new();
    for p in pins {
        let Some(edge) = pin_edge(p.angle).filter(|e| *e == a || *e == b) else {
            continue;
        };
        let w = pin_name_width(&p.name, PIN_TEXT);
        if w == 0.0 {
            continue;
        }
        let across = match axis {
            Axis::Horizontal => p.y,
            Axis::Vertical => p.x,
        };
        let row = rows
            .entry((across * 1000.0).round() as i64)
            .or_insert((0.0, 0.0));
        let col = if edge == a { &mut row.0 } else { &mut row.1 };
        *col = col.max(w);
    }
    let widest = rows
        .values()
        .map(|&(a, b)| {
            if a > 0.0 && b > 0.0 {
                a + b + PIN_NAME_GAP
            } else {
                a + b
            }
        })
        .fold(0.0, f64::max);
    if widest == 0.0 {
        0.0
    } else {
        widest + 2.0 * PIN_NAME_OFFSET
    }
}

/// Widen `lo..hi` symmetrically to at least `needed`, keeping the new edges on
/// the schematic grid so pins pushed out to meet them stay on it too.
fn grow_to(lo: &mut f64, hi: &mut f64, needed: f64) {
    if needed <= *hi - *lo {
        return;
    }
    let centre = (*lo + *hi) / 2.0;
    let half = needed / 2.0;
    // Snap each edge outwards on its own: rounding the half-extent instead only
    // lands on the grid when the centre already does. `+ 0.0` turns the `-0.0`
    // floor/ceil produce at the origin back into `0`.
    *lo = ((centre - half) / SYMBOL_GRID).floor() * SYMBOL_GRID + 0.0;
    *hi = ((centre + half) / SYMBOL_GRID).ceil() * SYMBOL_GRID + 0.0;
}

/// KiCAD's 12 valid pin electrical types — the first token of a
/// `(pin TYPE line …)` S-expression. Anything else makes eeschema refuse to
/// load the library ("Failed to load schematic"-class parse error), so the
/// value is validated instead of interpolated verbatim (#55).
const ALLOWED_PIN_ELECTRICAL_TYPES: [&str; 12] = [
    "input",
    "output",
    "bidirectional",
    "tri_state",
    "passive",
    "free",
    "unspecified",
    "power_in",
    "power_out",
    "open_collector",
    "open_emitter",
    "no_connect",
];

/// Error when any pin's electrical type is not one of KiCAD's 12 valid values
/// (#55) — eeschema refuses to load a library with a bad type, so nothing must
/// be written in that case. Shared by the rectangle and glyph paths.
fn validate_pin_types(pins_val: &[serde_json::Value]) -> anyhow::Result<()> {
    for pin in pins_val {
        let pin_type = pin["type"].as_str().unwrap_or("passive");
        if !ALLOWED_PIN_ELECTRICAL_TYPES.contains(&pin_type) {
            let number = pin["number"].as_str().unwrap_or("1");
            // The one mistake seen in the wild (#55) gets a targeted hint.
            let hint = if pin_type == "not_connected" {
                " (did you mean \"no_connect\"?)"
            } else {
                ""
            };
            anyhow::bail!(
                "invalid pin electrical type \"{}\" on pin \"{}\"{} — KiCAD accepts exactly one of: {}",
                pin_type,
                number,
                hint,
                ALLOWED_PIN_ELECTRICAL_TYPES.join(", ")
            );
        }
    }
    Ok(())
}

/// A conventional body shape for a symbol unit. `Rectangle` is the default (a
/// derived box around caller-positioned pins); the others draw a fixed op-amp or
/// logic-gate glyph copied from KiCAD's stock libraries and auto-place the pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Glyph {
    Rectangle,
    Opamp,
    Buffer,
    Inverter,
    Schmitt,
    SchmittInverter,
    And,
    Nand,
    Or,
    Nor,
    Xor,
    Xnor,
}

impl Glyph {
    fn parse(s: &str) -> Option<Glyph> {
        Some(match s {
            "rectangle" => Glyph::Rectangle,
            "opamp" => Glyph::Opamp,
            "buffer" => Glyph::Buffer,
            "inverter" => Glyph::Inverter,
            "schmitt" => Glyph::Schmitt,
            "schmitt_inverter" => Glyph::SchmittInverter,
            "and" => Glyph::And,
            "nand" => Glyph::Nand,
            "or" => Glyph::Or,
            "nor" => Glyph::Nor,
            "xor" => Glyph::Xor,
            "xnor" => Glyph::Xnor,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Glyph::Rectangle => "rectangle",
            Glyph::Opamp => "opamp",
            Glyph::Buffer => "buffer",
            Glyph::Inverter => "inverter",
            Glyph::Schmitt => "schmitt",
            Glyph::SchmittInverter => "schmitt_inverter",
            Glyph::And => "and",
            Glyph::Nand => "nand",
            Glyph::Or => "or",
            Glyph::Nor => "nor",
            Glyph::Xor => "xor",
            Glyph::Xnor => "xnor",
        }
    }

    /// Inverting glyphs draw the same body as their non-inverting base and mark
    /// the inversion with an `inverted` output pin (matching KiCAD's own gates,
    /// which carry the bubble on the pin rather than as a body circle).
    fn is_inverting(self) -> bool {
        matches!(
            self,
            Glyph::Inverter | Glyph::SchmittInverter | Glyph::Nand | Glyph::Nor | Glyph::Xnor
        )
    }

    /// How many input pins the fixed geometry has room for.
    fn input_count(self) -> usize {
        match self {
            Glyph::Buffer | Glyph::Inverter | Glyph::Schmitt | Glyph::SchmittInverter => 1,
            _ => 2,
        }
    }

    /// The narrow triangle-bodied glyphs (op-amp and the buffer family). Their
    /// apex leaves no room for power-pin names, so a single-unit triangular
    /// symbol that carries power pins puts them on a separate rectangular power
    /// unit instead (the gate glyphs have a flat back with room, so they keep
    /// their power pins on the body).
    fn is_triangular(self) -> bool {
        matches!(
            self,
            Glyph::Opamp
                | Glyph::Buffer
                | Glyph::Inverter
                | Glyph::Schmitt
                | Glyph::SchmittInverter
        )
    }
}

/// Whether a pin is a supply pin (belongs on a power unit).
fn is_power_pin(p: &serde_json::Value) -> bool {
    matches!(p["type"].as_str(), Some("power_in") | Some("power_out"))
}

/// Lay out power pins for a standalone rectangular power unit: vertical, V+/V-
/// style — even-indexed pins enter from the top (pointing down), odd from the
/// bottom (pointing up), matching KiCAD's multi-unit op-amp power unit. Any
/// caller x/y is replaced. Same spread (bulbs at y = ±7.62) as the multi-unit
/// `power_pins` path so a single op-amp's power unit matches a dual's.
fn layout_power_unit(power: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let n_top = power.len().div_ceil(2);
    let n_bot = power.len() / 2;
    let mut top_i = 0usize;
    let mut bot_i = 0usize;
    power
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut q = p.clone();
            if let Some(obj) = q.as_object_mut() {
                if i % 2 == 0 {
                    let x = (top_i as f64 - (n_top as f64 - 1.0) / 2.0) * 2.54;
                    obj.insert("x".into(), json!(x));
                    obj.insert("y".into(), json!(7.62));
                    obj.insert("angle".into(), json!(270));
                    obj.insert("length".into(), json!(2.54));
                    top_i += 1;
                } else {
                    let x = (bot_i as f64 - (n_bot as f64 - 1.0) / 2.0) * 2.54;
                    obj.insert("x".into(), json!(x));
                    obj.insert("y".into(), json!(-7.62));
                    obj.insert("angle".into(), json!(90));
                    obj.insert("length".into(), json!(2.54));
                    bot_i += 1;
                }
            }
            q
        })
        .collect()
}

/// Normalize a caller-supplied pin graphic style to a valid KiCAD token,
/// defaulting to `line`.
fn pin_style_token(s: Option<&str>) -> &'static str {
    match s {
        Some("inverted") => "inverted",
        Some("clock") => "clock",
        Some("inverted_clock") => "inverted_clock",
        Some("input_low") => "input_low",
        Some("clock_low") => "clock_low",
        Some("output_low") => "output_low",
        Some("edge_clock_high") => "edge_clock_high",
        Some("non_logic") => "non_logic",
        _ => "line",
    }
}

/// Default pin name/number text height (KiCAD's default).
const PIN_TEXT: f64 = 1.27;
/// Smaller pin-name height for glyph units. The fixed op-amp/gate bodies are
/// compact (KiCAD's own gates keep pin names empty on them), so real pin names
/// at the default 1.27 mm collide; 0.762 mm (KiCAD's standard small text) fits
/// them without enlarging the body and breaking the library-matching shape.
const GLYPH_PIN_NAME_TEXT: f64 = 0.762;

/// Average advance per character of KiCad's stroke font, as a multiple of the
/// text height. Measured off a 400 dpi render of KiCad 10 output: an
/// 18-character name at 1.27 mm spans 24.03 mm. The font is proportional, so
/// this is an average — [`PIN_NAME_GAP`] absorbs the variance.
const STROKE_ADVANCE_RATIO: f64 = 1.05;
/// Clear space kept between the two columns of pin names inside a body.
const PIN_NAME_GAP: f64 = 2.54;
/// Gap KiCad leaves between the body outline and the start of a pin name,
/// emitted as `(pin_names (offset …))`.
const PIN_NAME_OFFSET: f64 = 1.016;
/// Schematic grid. Body edges land on it so pins pushed out to meet them do too.
const SYMBOL_GRID: f64 = 1.27;

/// Width of a pin name as KiCad draws it. `~{FOO}` is an overbar rather than
/// four extra glyphs, and a bare `~` means "unnamed" and draws nothing.
fn pin_name_width(name: &str, size: f64) -> f64 {
    if name == "~" {
        return 0.0;
    }
    let glyphs = name.chars().filter(|c| !"~{}".contains(*c)).count();
    glyphs as f64 * size * STROKE_ADVANCE_RATIO
}

/// One `(pin …)` S-expression. `name_font` sets the pin-name text height;
/// numbers stay at the default (they sit outside the body and don't crowd).
///
/// Coordinates go through [`fmt_f64`] so binary floating-point artifacts stay
/// out of the file: `25.4 + 5.08` is `30.479999999999997` and `{}` prints it in
/// full.
#[allow(clippy::too_many_arguments)]
fn emit_pin(
    pin_type: &str,
    style: &str,
    x: f64,
    y: f64,
    angle: f64,
    length: f64,
    name: &str,
    number: &str,
    name_font: f64,
) -> String {
    format!(
        "\n    (pin {} {} (at {} {} {})\n      (length {})\n      (name \"{}\" (effects (font (size {} {}))))\n      (number \"{}\" (effects (font (size {} {}))))\n    )",
        pin_type,
        style,
        fmt_f64(x),
        fmt_f64(y),
        fmt_f64(angle),
        fmt_f64(length),
        name,
        name_font,
        name_font,
        number,
        PIN_TEXT,
        PIN_TEXT
    )
}

// ── Glyph geometry (coordinates copied verbatim from KiCAD 10's stock symbols)

fn fmt_pts(pts: &[(f64, f64)]) -> String {
    pts.iter()
        .map(|(x, y)| format!("(xy {} {})", x, y))
        .collect::<Vec<_>>()
        .join(" ")
}

fn g_polyline(pts: &[(f64, f64)], width: f64, fill: &str) -> String {
    format!(
        "\n      (polyline (pts {}) (stroke (width {}) (type default)) (fill (type {})))",
        fmt_pts(pts),
        width,
        fill
    )
}

fn g_arc(s: (f64, f64), m: (f64, f64), e: (f64, f64), fill: &str) -> String {
    format!(
        "\n      (arc (start {} {}) (mid {} {}) (end {} {}) (stroke (width 0.254) (type default)) (fill (type {})))",
        s.0, s.1, m.0, m.1, e.0, e.1, fill
    )
}

/// The OR/NOR body (also the base of XOR/XNOR): a concave back arc, two back
/// stubs, two front arcs meeting at the apex, and KiCAD's fill-outline polyline.
fn or_body() -> String {
    let mut b = g_arc((-3.81, 3.81), (-2.589, 0.0), (-3.81, -3.81), "none");
    b.push_str(&g_polyline(
        &[(-3.81, 3.81), (-0.635, 3.81)],
        0.254,
        "background",
    ));
    b.push_str(&g_polyline(
        &[(-3.81, -3.81), (-0.635, -3.81)],
        0.254,
        "background",
    ));
    b.push_str(&g_arc(
        (3.81, 0.0),
        (2.1855, -2.584),
        (-0.6096, -3.81),
        "background",
    ));
    b.push_str(&g_arc(
        (-0.6096, 3.81),
        (2.1928, 2.5924),
        (3.81, 0.0),
        "background",
    ));
    b.push_str(&g_polyline(
        &[
            (-0.635, 3.81),
            (-3.81, 3.81),
            (-3.81, 3.81),
            (-3.556, 3.4036),
            (-3.0226, 2.2606),
            (-2.6924, 1.0414),
            (-2.6162, -0.254),
            (-2.7686, -1.4986),
            (-3.175, -2.7178),
            (-3.81, -3.81),
            (-3.81, -3.81),
            (-0.635, -3.81),
        ],
        -25.4,
        "background",
    ));
    b
}

/// Fixed body + pin anchors for a glyph. Input anchors are ordered
/// top-to-bottom; the caller's pin order maps onto them in that order.
/// `power_top`/`power_bottom` are the points *on the body outline* where a
/// power pin's root should land (so the pin visually touches the shape, not the
/// bounding box).
struct GlyphGeom {
    body: String,
    inputs: Vec<(f64, f64, f64, f64)>,
    output: (f64, f64, f64, f64),
    power_top: (f64, f64),
    power_bottom: (f64, f64),
    rect: (f64, f64, f64, f64),
}

fn glyph_geom(g: Glyph) -> GlyphGeom {
    match g {
        Glyph::Rectangle => unreachable!("rectangle is handled by the rectangle path"),
        Glyph::Opamp => GlyphGeom {
            body: g_polyline(
                &[(-5.08, 5.08), (5.08, 0.0), (-5.08, -5.08), (-5.08, 5.08)],
                0.254,
                "background",
            ),
            inputs: vec![(-7.62, 2.54, 0.0, 2.54), (-7.62, -2.54, 0.0, 2.54)],
            output: (7.62, 0.0, 180.0, 2.54),
            // Centered on the triangle top/bottom edges (at x = 0, y = ±2.54),
            // so the power names clear the +/- input names on the left.
            power_top: (0.0, 2.54),
            power_bottom: (0.0, -2.54),
            rect: (-5.08, -5.08, 5.08, 5.08),
        },
        Glyph::Buffer | Glyph::Inverter => GlyphGeom {
            body: g_polyline(
                &[(-3.81, 3.81), (-3.81, -3.81), (3.81, 0.0), (-3.81, 3.81)],
                0.254,
                "background",
            ),
            inputs: vec![(-7.62, 0.0, 0.0, 3.81)],
            output: (7.62, 0.0, 180.0, 3.81),
            // Centered on the triangle top/bottom edges (x = 0, y = ±1.905).
            power_top: (0.0, 1.905),
            power_bottom: (0.0, -1.905),
            rect: (-3.81, -3.81, 3.81, 3.81),
        },
        Glyph::Schmitt | Glyph::SchmittInverter => {
            let mut body = g_polyline(
                &[(-3.81, 3.81), (-3.81, -3.81), (3.81, 0.0), (-3.81, 3.81)],
                0.254,
                "background",
            );
            // Hysteresis mark (from KiCAD's 74HC14).
            body.push_str(&g_polyline(
                &[(-2.54, -1.27), (-0.635, -1.27), (-0.635, 1.27), (0.0, 1.27)],
                0.254,
                "none",
            ));
            body.push_str(&g_polyline(
                &[(-1.905, -1.27), (-1.905, 1.27), (-0.635, 1.27)],
                0.254,
                "none",
            ));
            GlyphGeom {
                body,
                inputs: vec![(-7.62, 0.0, 0.0, 3.81)],
                output: (7.62, 0.0, 180.0, 3.81),
                // Centered (x = 0, y = ±1.905); the hysteresis mark sits at x <= 0.
                power_top: (0.0, 1.905),
                power_bottom: (0.0, -1.905),
                rect: (-3.81, -3.81, 3.81, 3.81),
            }
        }
        Glyph::And | Glyph::Nand => {
            let mut body = g_arc((0.0, 3.81), (3.7934, 0.0), (0.0, -3.81), "background");
            body.push_str(&g_polyline(
                &[(0.0, 3.81), (-3.81, 3.81), (-3.81, -3.81), (0.0, -3.81)],
                0.254,
                "background",
            ));
            GlyphGeom {
                body,
                inputs: vec![(-7.62, 2.54, 0.0, 3.81), (-7.62, -2.54, 0.0, 3.81)],
                output: (7.62, 0.0, 180.0, 3.81),
                // Right end of the flat back edges (x = 0, y = ±3.81), away from
                // the input names on the left.
                power_top: (0.0, 3.81),
                power_bottom: (0.0, -3.81),
                rect: (-3.81, -3.81, 3.81, 3.81),
            }
        }
        Glyph::Or | Glyph::Nor => GlyphGeom {
            body: or_body(),
            // Longer than the gates above: the back is a *concave* arc that sits
            // at x ≈ -3.10 at the input height, so length 4.52 puts the roots on
            // the curve (3.81 would leave a visible gap).
            inputs: vec![(-7.62, 2.54, 0.0, 4.52), (-7.62, -2.54, 0.0, 4.52)],
            output: (7.62, 0.0, 180.0, 3.81),
            // Rightmost point of the flat back stubs (y = ±3.81, x = -0.635),
            // away from the input names on the left.
            power_top: (-0.635, 3.81),
            power_bottom: (-0.635, -3.81),
            rect: (-3.81, -3.81, 3.81, 3.81),
        },
        Glyph::Xor | Glyph::Xnor => {
            // OR body plus a second offset back arc and two input stubs.
            let mut body = g_arc((-4.4196, 3.81), (-3.2033, 0.0), (-4.4196, -3.81), "none");
            body.push_str(&or_body());
            body.push_str(&g_polyline(
                &[(-3.81, 2.54), (-3.175, 2.54)],
                0.254,
                "background",
            ));
            body.push_str(&g_polyline(
                &[(-3.81, -2.54), (-3.175, -2.54)],
                0.254,
                "background",
            ));
            GlyphGeom {
                body,
                inputs: vec![(-7.62, 2.54, 0.0, 4.445), (-7.62, -2.54, 0.0, 4.445)],
                output: (7.62, 0.0, 180.0, 3.81),
                // Rightmost flat point of the back stubs (x = -0.635, y = ±3.81).
                power_top: (-0.635, 3.81),
                power_bottom: (-0.635, -3.81),
                rect: (-4.4196, -3.81, 3.81, 3.81),
            }
        }
    }
}

/// Build a glyph unit: the fixed body plus auto-placed pins. Returns `Err(msg)`
/// when the unit's pins don't fit the glyph (wrong input count, not exactly one
/// output, or unsupported pin types) so the caller can fall back to a rectangle.
fn build_glyph_unit(
    pins_val: &[serde_json::Value],
    g: Glyph,
) -> Result<(String, SymbolRect, Vec<ResolvedPin>), String> {
    let mut inputs: Vec<&serde_json::Value> = Vec::new();
    let mut outputs: Vec<&serde_json::Value> = Vec::new();
    let mut powers: Vec<&serde_json::Value> = Vec::new();
    let mut others = 0usize;
    for p in pins_val {
        match p["type"].as_str().unwrap_or("passive") {
            "input" => inputs.push(p),
            "output" | "tri_state" | "open_collector" | "open_emitter" => outputs.push(p),
            "power_in" | "power_out" => powers.push(p),
            _ => others += 1,
        }
    }

    let want = g.input_count();
    if inputs.len() != want {
        return Err(format!(
            "glyph '{}' expects {} input pin(s) but {} were given; drew a rectangle instead",
            g.name(),
            want,
            inputs.len()
        ));
    }
    if outputs.len() != 1 {
        return Err(format!(
            "glyph '{}' expects exactly 1 output pin but {} were given; drew a rectangle instead",
            g.name(),
            outputs.len()
        ));
    }
    if others > 0 {
        return Err(format!(
            "glyph '{}' only supports input/output/power pins; drew a rectangle instead",
            g.name()
        ));
    }

    let geom = glyph_geom(g);
    let mut sexp = geom.body.clone();
    let mut resolved = Vec::with_capacity(pins_val.len());

    // Inputs map onto the glyph anchors in the caller's order (top-to-bottom).
    for (p, &(x, y, angle, length)) in inputs.iter().zip(geom.inputs.iter()) {
        resolved.push(ResolvedPin::new(p, x, y));
        let number = p["number"].as_str().unwrap_or("1");
        let name = p["name"].as_str().unwrap_or("~");
        let style = pin_style_token(p["style"].as_str());
        sexp.push_str(&emit_pin(
            "input",
            style,
            x,
            y,
            angle,
            length,
            name,
            number,
            GLYPH_PIN_NAME_TEXT,
        ));
    }

    // The single output sits at the apex; inverting glyphs default to an
    // inverted pin (the bubble), but the caller can override via `style`.
    let out = outputs[0];
    let out_number = out["number"].as_str().unwrap_or("1");
    let out_name = out["name"].as_str().unwrap_or("~");
    let out_type = out["type"].as_str().unwrap_or("output");
    let out_style = match out["style"].as_str() {
        Some(s) => pin_style_token(Some(s)),
        None if g.is_inverting() => "inverted",
        None => "line",
    };
    let (ox, oy, oa, ol) = geom.output;
    resolved.push(ResolvedPin::new(out, ox, oy));
    sexp.push_str(&emit_pin(
        out_type,
        out_style,
        ox,
        oy,
        oa,
        ol,
        out_name,
        out_number,
        GLYPH_PIN_NAME_TEXT,
    ));

    // Power pins (e.g. a single op-amp's V+/V-) enter vertically, alternating
    // top/bottom, with their roots on the body outline so they touch the shape.
    for (i, p) in powers.iter().enumerate() {
        let number = p["number"].as_str().unwrap_or("1");
        let name = p["name"].as_str().unwrap_or("~");
        let ptype = p["type"].as_str().unwrap_or("power_in");
        let style = pin_style_token(p["style"].as_str());
        let length = 2.54;
        let (x, y, angle) = if i % 2 == 0 {
            let (ax, ay) = geom.power_top;
            (ax, ay + length, 270.0) // bulb above, root on the top edge
        } else {
            let (ax, ay) = geom.power_bottom;
            (ax, ay - length, 90.0) // bulb below, root on the bottom edge
        };
        resolved.push(ResolvedPin::new(p, x, y));
        sexp.push_str(&emit_pin(
            ptype,
            style,
            x,
            y,
            angle,
            length,
            name,
            number,
            GLYPH_PIN_NAME_TEXT,
        ));
    }

    Ok((sexp, Some(geom.rect), resolved))
}

/// Build one unit's inner S-expression — an optional body (a rectangle, or a
/// conventional `glyph` shape) followed by its pins — and return it with the
/// body rect (used for reference/value placement) and an optional warning (e.g.
/// a glyph that didn't fit its pins and fell back to a rectangle). Shared by the
/// single- and multi-unit paths.
///
/// Errors (#55) when a pin's electrical type is not one of KiCAD's 12 valid
/// values — the caller must not write anything to disk in that case.
/// Glyph units keep their fixed library-matching shape, so `show_names` only
/// reaches the rectangle path.
fn build_symbol_unit(
    pins_val: &[serde_json::Value],
    glyph: Option<Glyph>,
    show_names: bool,
) -> anyhow::Result<BuiltUnit> {
    validate_pin_types(pins_val)?;
    if let Some(g) = glyph {
        if g != Glyph::Rectangle {
            match build_glyph_unit(pins_val, g) {
                Ok((sexp, rect, pins)) => {
                    return Ok(BuiltUnit {
                        sexp,
                        rect,
                        warning: None,
                        body: g.name(),
                        pins,
                    })
                }
                Err(reason) => {
                    let (sexp, rect, pins) = build_rect_unit(pins_val, show_names);
                    return Ok(BuiltUnit {
                        sexp,
                        rect,
                        warning: Some(reason),
                        body: "rectangle",
                        pins,
                    });
                }
            }
        }
    }
    let (sexp, rect, pins) = build_rect_unit(pins_val, show_names);
    Ok(BuiltUnit {
        sexp,
        rect,
        warning: None,
        body: "rectangle",
        pins,
    })
}

/// The default rectangle body + caller-positioned pins. Pin types are assumed
/// already validated by `build_symbol_unit`.
fn build_rect_unit(
    pins_val: &[serde_json::Value],
    show_names: bool,
) -> (String, SymbolRect, Vec<ResolvedPin>) {
    let mut pin_geoms: Vec<PinGeom> = Vec::new();
    for pin in pins_val {
        pin_geoms.push(PinGeom {
            x: pin["x"].as_f64().unwrap_or(0.0),
            y: pin["y"].as_f64().unwrap_or(0.0),
            angle: pin["angle"].as_f64().unwrap_or(0.0),
            length: pin["length"].as_f64().unwrap_or(2.54),
            name: pin["name"].as_str().unwrap_or("~").to_string(),
        });
    }
    let body = symbol_body_rect(&pin_geoms, show_names);
    // Fitting the names can push the body past the pins that defined it.
    // Slide them back out, keeping the length the caller asked for.
    if let Some((min_x, min_y, max_x, max_y)) = body {
        for g in &mut pin_geoms {
            match pin_edge(g.angle) {
                Some(PinEdge::Left) => g.x = min_x - g.length,
                Some(PinEdge::Right) => g.x = max_x + g.length,
                Some(PinEdge::Bottom) => g.y = min_y - g.length,
                Some(PinEdge::Top) => g.y = max_y + g.length,
                None => {}
            }
        }
    }

    let mut pins_sexp = String::new();
    let mut resolved = Vec::with_capacity(pins_val.len());
    for (pin, g) in pins_val.iter().zip(&pin_geoms) {
        resolved.push(ResolvedPin::new(pin, g.x, g.y));
        pins_sexp.push_str(&emit_pin(
            pin["type"].as_str().unwrap_or("passive"),
            pin_style_token(pin["style"].as_str()),
            g.x,
            g.y,
            g.angle,
            g.length,
            &g.name,
            pin["number"].as_str().unwrap_or("1"),
            PIN_TEXT,
        ));
    }
    let body_sexp = match body {
        Some((min_x, min_y, max_x, max_y)) => format!(
            "\n      (rectangle (start {:.4} {:.4}) (end {:.4} {:.4})\n        (stroke (width 0.254) (type default))\n        (fill (type background))\n      )",
            min_x, min_y, max_x, max_y
        ),
        None => String::new(),
    };
    (format!("{}{}", body_sexp, pins_sexp), body, resolved)
}

type SymbolRect = Option<(f64, f64, f64, f64)>;

/// Where a pin ended up, next to where the caller asked for it.
///
/// `create_symbol` treats a pin's `x`/`y` as a starting point, not a fixed
/// position: the body is sized to fit the pin names and every pin on an edge is
/// then aligned to that edge. Callers cannot plan wiring from the coordinates
/// they sent, so the resolved ones are reported back.
struct ResolvedPin {
    number: String,
    name: String,
    requested: Option<(f64, f64)>,
    x: f64,
    y: f64,
}

impl ResolvedPin {
    fn new(pin: &serde_json::Value, x: f64, y: f64) -> Self {
        let requested = match (pin["x"].as_f64(), pin["y"].as_f64()) {
            (Some(rx), Some(ry)) => Some((rx, ry)),
            _ => None,
        };
        ResolvedPin {
            number: pin["number"].as_str().unwrap_or("1").to_string(),
            name: pin["name"].as_str().unwrap_or("~").to_string(),
            requested,
            x,
            y,
        }
    }

    /// How far the pin was moved, or `None` when the caller named no position.
    fn displacement(&self) -> Option<f64> {
        let (rx, ry) = self.requested?;
        let moved = ((self.x - rx).powi(2) + (self.y - ry).powi(2)).sqrt();
        (moved > POSITION_EPSILON).then_some(moved)
    }

    fn to_json(&self) -> serde_json::Value {
        let mut out = json!({
            "number": self.number,
            "name": self.name,
            "x": self.x,
            "y": self.y
        });
        // Only when it differs, so the common case stays compact and a moved
        // pin stands out.
        if let (Some((rx, ry)), Some(_)) = (self.requested, self.displacement()) {
            out["requested"] = json!({ "x": rx, "y": ry });
        }
        out
    }
}

/// Below this a difference is float noise from the body arithmetic, not a move.
const POSITION_EPSILON: f64 = 1e-6;

/// One unit's rendered body, and what became of the pins the caller placed.
struct BuiltUnit {
    sexp: String,
    rect: SymbolRect,
    /// A glyph that did not fit its pins and fell back to a rectangle.
    warning: Option<String>,
    /// `"rectangle"` or the glyph's name — a glyph places pins by type, so a
    /// moved pin there is intended rather than a surprise.
    body: &'static str,
    pins: Vec<ResolvedPin>,
}

impl BuiltUnit {
    /// A per-unit summary of the auto-size override, or `None` when nothing
    /// moved. Per pin would be a dozen lines of noise on a normal symbol, and a
    /// glyph unit moves every pin by design.
    fn displacement_warning(&self, unit_label: &str) -> Option<String> {
        if self.body != "rectangle" {
            return None;
        }
        let moved: Vec<f64> = self
            .pins
            .iter()
            .filter_map(ResolvedPin::displacement)
            .collect();
        let furthest = moved.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (!moved.is_empty()).then(|| {
            format!(
                "{unit_label}: the body was sized to fit the pin names, moving {} of {} pins by up to {:.2} mm; the resolved positions are in `units`",
                moved.len(),
                self.pins.len(),
                furthest
            )
        })
    }

    fn to_json(&self, unit: usize) -> serde_json::Value {
        json!({
            "unit": unit,
            "body": self.body,
            "pins": self.pins.iter().map(ResolvedPin::to_json).collect::<Vec<_>>()
        })
    }
}

async fn handle_create_symbol(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    // Schema-required. Defaulting them appended `(symbol "Symbol" …)` with
    // reference `U` to the library — and there is no duplicate-name guard, so
    // repeated calls stacked identically-named entries in one file (#218).
    let name = match require_str(args, "name") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let ref_prefix = match require_str(args, "reference_prefix") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let value_str = args["value"].as_str().unwrap_or(name);
    // `~` is KiCAD's legacy "no datasheet" placeholder. Its library loader
    // normalises it to the empty string and its schematic lib_symbols loader
    // does not, so a symbol carrying `~` never matches its own library copy in
    // ERC. Write the normalised form on both paths.
    let datasheet = match args["datasheet"].as_str().unwrap_or("") {
        "~" => "",
        s => s,
    };
    let show_names = args["show_pin_names"].as_bool().unwrap_or(true);
    let show_numbers = args["show_pin_numbers"].as_bool().unwrap_or(true);

    // Optional conventional body shape. `glyph` may be set at the symbol level
    // (a default for every unit) and/or per unit (overriding the default).
    let mut warnings: Vec<String> = Vec::new();
    // Where each unit's pins actually ended up. The caller's x/y are a starting
    // point, so this is the only way to plan wiring without re-measuring the
    // finished symbol with batch_get_schematic_pin_locations.
    let mut units_report: Vec<serde_json::Value> = Vec::new();
    let sym_glyph = match args["glyph"].as_str() {
        None => None,
        Some(s) => match Glyph::parse(s) {
            Some(g) => Some(g),
            None => {
                warnings.push(format!("unknown glyph '{}'; used a rectangle", s));
                None
            }
        },
    };

    // Multi-unit when `units` is a non-empty array; otherwise the single-unit
    // `pins` path. Sub-symbols are named NAME_<unit>_1; units 1..N are the
    // individual units, and shared `power_pins` become a dedicated final unit.
    let unit_objs: Vec<serde_json::Value> = args["units"].as_array().cloned().unwrap_or_default();
    let power_pins = args["power_pins"].as_array().cloned().unwrap_or_default();

    let mut units_sexp = String::new();
    let unit_count: usize;
    let ref_body: SymbolRect;
    if unit_objs.is_empty() {
        let pins_val = args["pins"].as_array().cloned().unwrap_or_default();
        // A single-unit triangular glyph (op-amp/buffer/inverter/schmitt) has no
        // room for power-pin names on its narrow apex, so if it carries power
        // pins, split them onto a dedicated rectangular power unit (like KiCAD's
        // multi-unit op-amps) instead of drawing them on the triangle.
        let split_power =
            matches!(sym_glyph, Some(g) if g.is_triangular()) && pins_val.iter().any(is_power_pin);
        if split_power {
            let signal: Vec<serde_json::Value> = pins_val
                .iter()
                .filter(|p| !is_power_pin(p))
                .cloned()
                .collect();
            let power: Vec<serde_json::Value> = pins_val
                .iter()
                .filter(|p| is_power_pin(p))
                .cloned()
                .collect();
            // Unit 1: the triangle with its signal pins.
            let unit1 = match build_symbol_unit(&signal, sym_glyph, show_names) {
                Ok(v) => v,
                Err(e) => return Ok(CallToolResult::error(e.to_string())),
            };
            warnings.extend(unit1.warning.clone());
            warnings.extend(unit1.displacement_warning("unit 1"));
            units_report.push(unit1.to_json(1));
            let (inner1, body1) = (unit1.sexp, unit1.rect);
            units_sexp.push_str(&format!("\n    (symbol \"{}_1_1\"{}\n    )", name, inner1));
            // Unit 2: a rectangular power unit.
            let power_laid = layout_power_unit(&power);
            let unit2 = match build_symbol_unit(&power_laid, None, show_names) {
                Ok(v) => v,
                Err(e) => return Ok(CallToolResult::error(e.to_string())),
            };
            warnings.extend(unit2.warning.clone());
            warnings.extend(unit2.displacement_warning("unit 2"));
            units_report.push(unit2.to_json(2));
            let inner2 = unit2.sexp;
            units_sexp.push_str(&format!("\n    (symbol \"{}_2_1\"{}\n    )", name, inner2));
            unit_count = 2;
            ref_body = body1;
        } else {
            // Single unit: body + all pins live in NAME_0_1 (unchanged behavior).
            let unit = match build_symbol_unit(&pins_val, sym_glyph, show_names) {
                Ok(v) => v,
                Err(e) => return Ok(CallToolResult::error(e.to_string())),
            };
            warnings.extend(unit.warning.clone());
            warnings.extend(unit.displacement_warning("unit 1"));
            units_report.push(unit.to_json(1));
            let (inner, body) = (unit.sexp, unit.rect);
            units_sexp.push_str(&format!("\n    (symbol \"{}_0_1\"{}\n    )", name, inner));
            unit_count = 1;
            ref_body = body;
        }
    } else {
        // Multi-unit: each signal unit is NAME_1_1..NAME_N_1, and the power
        // pins (if any) become a dedicated FINAL unit rather than being drawn
        // on every unit. KiCAD's own multi-unit parts do this (e.g. 74LS00 has
        // the four gates as units 1..4 and VCC/GND as unit 5). It means the
        // power pins appear on exactly one placed unit instead of on every
        // unit, where each duplicate would otherwise need wiring to pass ERC.
        let mut first_body: SymbolRect = None;
        for (i, u) in unit_objs.iter().enumerate() {
            let unit_pins = u["pins"].as_array().cloned().unwrap_or_default();
            // A per-unit `glyph` overrides the symbol-level default.
            let unit_glyph = match u["glyph"].as_str() {
                None => sym_glyph,
                Some(s) => match Glyph::parse(s) {
                    Some(g) => Some(g),
                    None => {
                        warnings.push(format!(
                            "unit {}: unknown glyph '{}'; used a rectangle",
                            i + 1,
                            s
                        ));
                        Some(Glyph::Rectangle)
                    }
                },
            };
            let unit = match build_symbol_unit(&unit_pins, unit_glyph, show_names) {
                Ok(v) => v,
                Err(e) => return Ok(CallToolResult::error(e.to_string())),
            };
            if let Some(w) = unit.warning.clone() {
                warnings.push(format!("unit {}: {}", i + 1, w));
            }
            warnings.extend(unit.displacement_warning(&format!("unit {}", i + 1)));
            units_report.push(unit.to_json(i + 1));
            let (inner, body) = (unit.sexp, unit.rect);
            if i == 0 {
                first_body = body;
            }
            units_sexp.push_str(&format!(
                "\n    (symbol \"{}_{}_1\"{}\n    )",
                name,
                i + 1,
                inner
            ));
        }
        let mut total = unit_objs.len();
        if !power_pins.is_empty() {
            // The power unit is always a rectangle.
            let unit = match build_symbol_unit(&power_pins, None, show_names) {
                Ok(v) => v,
                Err(e) => return Ok(CallToolResult::error(e.to_string())),
            };
            total += 1;
            warnings.extend(unit.displacement_warning(&format!("unit {total}")));
            units_report.push(unit.to_json(total));
            let inner = unit.sexp;
            units_sexp.push_str(&format!(
                "\n    (symbol \"{}_{}_1\"{}\n    )",
                name, total, inner
            ));
        }
        unit_count = total;
        ref_body = first_body;
    }

    // Reference/value placement above/below the (first) unit body (Y-up).
    let (ref_y, value_y) = match ref_body {
        Some((_, min_y, _, max_y)) => (max_y + 2.54, min_y - 2.54),
        None => (2.54, -2.54),
    };

    let numbers = if show_numbers {
        String::new()
    } else {
        "\n    (pin_numbers hide)".to_string()
    };
    let names = if show_names {
        format!(
            "\n    (pin_names\n      (offset {}))",
            fmt_f64(PIN_NAME_OFFSET)
        )
    } else {
        format!(
            "\n    (pin_names\n      (offset {})\n      (hide yes))",
            fmt_f64(PIN_NAME_OFFSET)
        )
    };
    let visible_property = |name: &str, value: &str, y: f64| {
        format!(
            "\n    (property \"{name}\" \"{value}\" (at 0 {y:.4} 0) \
             (show_name no) (do_not_autoplace no) \
             (effects (font (size 1.27 1.27))))"
        )
    };
    let hidden_property = |name: &str, value: &str| {
        format!(
            "\n    (property \"{name}\" \"{value}\" (at 0 0 0) \
             (show_name no) (do_not_autoplace no) (hide yes) \
             (effects (font (size 1.27 1.27))))"
        )
    };

    let symbol_sexp = format!(
        "\n  (symbol \"{}\"{}{}\n    (exclude_from_sim no)\n    (in_bom yes)\n    (on_board yes)\n    (in_pos_files yes)\n    (duplicate_pin_numbers_are_jumpers no){}{}{}{}{}{}\n    (embedded_fonts no)\n  )",
        name,
        numbers,
        names,
        visible_property("Reference", ref_prefix, ref_y),
        visible_property("Value", value_str, value_y),
        hidden_property("Footprint", ""),
        hidden_property("Datasheet", datasheet),
        hidden_property("Description", ""),
        units_sexp
    );

    // If file doesn't exist, create scaffold
    let content = if lib_path.exists() {
        let mut content = tokio::fs::read_to_string(&lib_path).await?;
        content = content.replace("(version 20240108)", "(version 20251024)");
        if !content.contains("(generator_version ") {
            if let Some(position) = content.find("(generator \"") {
                if let Some(end) = content[position..].find(')') {
                    let insert_at = position + end + 1;
                    content.insert_str(insert_at, "\n  (generator_version \"10.0\")");
                }
            }
        }
        content
    } else {
        "(kicad_symbol_lib\n  (version 20251024)\n  (generator \"konnect\")\n  \
         (generator_version \"10.0\")\n)\n"
            .to_string()
    };

    // Insert before closing paren of root expression
    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = format!("{}{}\n)", &content[..insert_pos], symbol_sexp);

    if let Some(parent) = lib_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(&lib_path, &new_content)?;

    let mut result = json!({
        "success": true,
        "symbol": name,
        "library": lib_path.to_str().unwrap_or(""),
        "unit_count": unit_count,
        "power_pin_count": power_pins.len()
    });
    result["units"] = json!(units_report);
    if !warnings.is_empty() {
        result["warnings"] = json!(warnings);
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&result).unwrap(),
    ))
}

async fn handle_delete_symbol(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let symbol_name = match require_str(args, "symbol_name") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = tokio::fs::read_to_string(&lib_path).await?;

    // Find `  (symbol "NAME"` block
    let pat = format!(r#"  (symbol "{}""#, symbol_name);
    let start = content
        .find(&pat)
        .ok_or_else(|| anyhow::anyhow!("Symbol '{}' not found in library", symbol_name))?;

    // Walk back to find preceding newline
    let block_start = content[..start].rfind('\n').map(|i| i + 1).unwrap_or(start);

    // Walk forward to find end of block (depth count)
    let mut depth = 0i32;
    let mut end = start;
    for (i, ch) in content[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    // Skip trailing newline
    let end = if content[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };

    let new_content = format!("{}{}", &content[..block_start], &content[end..]);
    write_atomic(&lib_path, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "deleted": symbol_name
        }))
        .unwrap(),
    ))
}

/// Extract the names of every top-level symbol defined in a `.kicad_sym`
/// library body, sorted and de-duplicated.
///
/// KiCad writes these files with CRLF line endings (on Windows) and TAB
/// indentation, so a fixed string search such as `\n  (symbol "` does not work
/// — it returned 0 symbols for every real library (KiCad 10, format version
/// 20251024). Instead we parse the S-expression structurally and read the
/// **direct** children of the `(kicad_symbol_lib …)` root whose head is
/// `symbol`. Nested unit sub-symbols (`NAME_0_1`, `NAME_1_1`, …) live one
/// level deeper, so they are excluded automatically — no name-pattern
/// heuristics required, and names containing underscores are preserved
/// verbatim.
fn top_level_symbol_names(content: &str) -> anyhow::Result<Vec<String>> {
    let root = parse_sexp(content)
        .map_err(|e| anyhow::anyhow!("failed to parse .kicad_sym library: {e}"))?;
    let mut names: Vec<String> = root
        .find_all("symbol")
        .into_iter()
        .filter_map(|sym| sym.get(1).and_then(|n| n.as_str()).map(str::to_owned))
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// The `.kicad_sym` file defining `nick:sym_name`, found exactly the way symbol
/// placement finds it — via [`KiCadSymbolSource`], so a lookup here and a
/// placement of the same lib_id can never disagree.
fn resolve_symbol_lib_path(
    nick: &str,
    sym_name: &str,
    project_dir: Option<&Path>,
) -> Option<PathBuf> {
    use konnect_schematic_editor::library::SymbolLibrarySource;
    KiCadSymbolSource::new(project_dir.map(Path::to_path_buf))
        .candidates(nick)
        .iter()
        .find_map(|candidate| {
            konnect_schematic_editor::library::symbol_file_in(candidate, sym_name)
        })
}

/// Recursively collect every descendant `SexpNode::List` whose head matches
/// `head` (depth-first, document order). Pins live inside nested unit
/// sub-symbols `(symbol "NAME_N_M" …)`, not as direct children of the top-level
/// symbol, so a direct-children lookup is not enough.
fn descendants_with_head<'a>(node: &'a SexpNode, head: &str) -> Vec<&'a SexpNode> {
    fn walk<'a>(node: &'a SexpNode, head: &str, out: &mut Vec<&'a SexpNode>) {
        for child in node.children().unwrap_or(&[]) {
            if child.head() == Some(head) {
                out.push(child);
            }
            walk(child, head, out);
        }
    }
    let mut out = Vec::new();
    walk(node, head, &mut out);
    out
}

/// Resolve the effective pins of a symbol, following `(extends "BASE")` so
/// derived symbols inherit pins from their base. Walks from the most-derived
/// symbol (`sym_node`) up through each base found among `root`'s top-level
/// symbols, collecting pin nodes with most-derived precedence (a pin number
/// declared on a derived symbol shadows the same number on a base). A visited
/// set guards against cyclic `extends`; a missing base stops the walk
/// gracefully and returns whatever pins were collected.
fn resolve_symbol_pins<'a>(root: &'a SexpNode, sym_node: &'a SexpNode) -> Vec<&'a SexpNode> {
    // Build the chain [sym_node, base, base-of-base, ...] (most-derived first).
    let mut chain: Vec<&SexpNode> = Vec::new();
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut current = sym_node;
    while let Some(name) = current.get(1).and_then(|n| n.as_str()) {
        if !visited.insert(name) {
            break; // cycle guard: name already seen
        }
        chain.push(current);
        let Some(base_name) = current.find_str("extends") else {
            break; // terminal base (no extends)
        };
        let Some(base) = root
            .find_all("symbol")
            .into_iter()
            .find(|s| s.get(1).and_then(|n| n.as_str()) == Some(base_name))
        else {
            break; // missing base — stop gracefully
        };
        current = base;
    }

    // Collect pins most-derived first, dedup by number.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut pins: Vec<&SexpNode> = Vec::new();
    for sym in &chain {
        for pin in descendants_with_head(sym, "pin") {
            let number = pin.find_str("number").unwrap_or("").to_owned();
            if seen.insert(number) {
                pins.push(pin);
            }
        }
    }
    pins
}

/// Search one library body for top-level symbols whose name contains `query`
/// (case-insensitive), returning result objects shaped like `search_symbols`.
fn search_lib_symbols(nickname: &str, content: &str, query: &str) -> Vec<serde_json::Value> {
    let Ok(names) = top_level_symbol_names(content) else {
        return Vec::new();
    };
    names
        .into_iter()
        .filter(|n| n.to_lowercase().contains(query))
        .map(|sym_name| {
            json!({
                "library": nickname,
                "name": sym_name,
                "id": format!("{}:{}", nickname, sym_name)
            })
        })
        .collect()
}

async fn handle_list_symbols_in_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let content = tokio::fs::read_to_string(&lib_path).await?;

    let symbols = top_level_symbol_names(&content)?;
    let limit = args["limit"].as_u64().unwrap_or(100) as usize;
    let truncated = symbols.len() > limit;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "library": lib_path.to_str().unwrap_or(""),
            "count": symbols.len(),
            "truncated": truncated,
            "symbols": symbols.into_iter().take(limit).collect::<Vec<_>>()
        }))
        .unwrap(),
    ))
}

async fn handle_search_symbols(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let query = match require_str(args, "query") {
        Ok(v) => v.to_lowercase(),
        Err(e) => return Ok(e),
    };
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    let project_dir = args["project_dir"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| ctx.config.project_dir.clone());

    // Gather (nickname, path) entries from the global sym-lib-table and, when a
    // project dir is supplied, the project's own sym-lib-table too — this is
    // what makes project-attached libraries searchable. Nested `(type "Table")`
    // references are followed and `${KICAD*_DIR}` URIs expanded, so the
    // libraries KiCad ships are included.
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut tables = vec![global_sym_lib_table()];
    if let Some(pd) = &project_dir {
        tables.push(pd.join("sym-lib-table"));
    }
    for table in &tables {
        for lib in read_flat_lib_table(table) {
            if let (Some(nick), Some(path)) = (lib["nickname"].as_str(), lib["path"].as_str()) {
                entries.push((nick.to_string(), path.to_string()));
            }
        }
    }

    let mut results = Vec::new();
    // `entries` holds resolved filesystem paths, not the raw uris they came
    // from — read_flat_lib_table does that expansion now.
    'outer: for (nickname, resolved) in entries {
        let lib_path = PathBuf::from(&resolved);
        if !lib_path.exists() {
            continue;
        }
        let Ok(lib_content) = tokio::fs::read_to_string(&lib_path).await else {
            continue;
        };
        for hit in search_lib_symbols(&nickname, &lib_content, &query) {
            results.push(hit);
            if results.len() >= limit {
                break 'outer;
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "query": query,
            "count": results.len(),
            "results": results
        }))
        .unwrap(),
    ))
}

async fn handle_list_library_footprints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let library_path_str = match require_str(args, "library_path") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let lib_dir = PathBuf::from(library_path_str);

    if !lib_dir.is_dir() {
        return Ok(CallToolResult::error(format!(
            "Not a directory: {}",
            library_path_str
        )));
    }

    let mut footprints = Vec::new();
    let mut rd = tokio::fs::read_dir(&lib_dir).await?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".kicad_mod") {
            footprints.push(name_str.trim_end_matches(".kicad_mod").to_string());
        }
    }
    footprints.sort();

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "library": library_path_str,
            "count": footprints.len(),
            "footprints": footprints
        }))
        .unwrap(),
    ))
}

async fn handle_get_footprint_info(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let fp_path_str = match require_str(args, "footprint_path") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    // Resolve "Library:Footprint" against the project's fp-lib-table as well
    // as the global one, when the caller says which project they mean.
    let project_dir = args["project"]
        .as_str()
        .map(PathBuf::from)
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let path = match resolve_footprint_path(fp_path_str, project_dir.as_deref()) {
        Ok(p) => p,
        Err(msg) => return Ok(CallToolResult::error(msg)),
    };

    let content = tokio::fs::read_to_string(&path).await?;
    let footprint = parse_sexp(&content)?;

    // KiCad controls indentation and line endings, so every field here is read
    // from the parsed footprint rather than inferred from source text — a
    // `descr` mentioning a pad must not be counted as one. The name is the
    // root node's first datum; pads, models and graphics are its direct
    // children.
    let description = footprint.find_str("descr").unwrap_or_default().to_string();
    let fp_name = footprint
        .get(1)
        .and_then(|node| node.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });
    let pad_count = footprint
        .children()
        .unwrap_or(&[])
        .iter()
        .filter(|node| node.head() == Some("pad"))
        .count();
    let has_courtyard = footprint
        .children()
        .unwrap_or(&[])
        .iter()
        .any(|node| matches!(node.find_str("layer"), Some("B.CrtYd" | "F.CrtYd")));
    let has_3d = footprint.find("model").is_some();

    let mut response = json!({
        "name": fp_name,
        "description": description,
        "pad_count": pad_count,
        "has_courtyard": has_courtyard,
        "has_3d_model": has_3d,
        "path": path.to_str().unwrap_or("")
    });
    let graphics_layer = args["graphics_layer"].as_str();
    let include_graphics =
        args["include_graphics"].as_bool().unwrap_or(false) || graphics_layer.is_some();
    if include_graphics {
        let graphics = match super::footprint_graphics::inspect_graphics(&content, graphics_layer) {
            Ok(graphics) => graphics,
            Err(super::footprint_graphics::FootprintGraphicsError::InvalidArgument {
                field,
                reason,
            }) => {
                return Ok(CallToolResult::error_kind(
                    crate::mcp::error::ToolErrorKind::InvalidArgument {
                        field: field.clone(),
                        reason: reason.clone(),
                    },
                    format!("Argument '{field}' is invalid: {reason}"),
                ));
            }
            Err(super::footprint_graphics::FootprintGraphicsError::Conflict(reason)) => {
                return Ok(CallToolResult::error(reason));
            }
        };
        response["graphic_count"] = json!(graphics.len());
        response["graphics"] = serde_json::to_value(graphics)?;
    }

    Ok(CallToolResult::json(&response))
}

// ─── search_footprints (moved from verification toolset) ─────────────────────

async fn handle_search_footprints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let query = match require_str(args, "query") {
        Ok(v) => v.to_lowercase(),
        Err(e) => return Ok(e),
    };
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    // Walk global fp-lib-table
    let fp_lib_table_path = super::kicad_config_dir().join("fp-lib-table");

    let mut results = Vec::new();

    'outer: for lib in read_flat_lib_table(&fp_lib_table_path) {
        let nickname = lib["nickname"].as_str().unwrap_or("").to_string();
        let Some(dir) = lib["path"].as_str().map(PathBuf::from) else {
            continue;
        };
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();
            let Some(fp_name) = fname_str.strip_suffix(".kicad_mod") else {
                continue;
            };
            if fp_name.to_lowercase().contains(&query) {
                results.push(json!({
                    "library": nickname,
                    "name": fp_name,
                    "id": format!("{}:{}", nickname, fp_name)
                }));
                if results.len() >= limit {
                    break 'outer;
                }
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "query": args["query"].as_str().unwrap_or(""),
            "count": results.len(),
            "results": results
        }))
        .unwrap(),
    ))
}

// ─── get_symbol_info (moved from verification toolset) ───────────────────────

async fn handle_get_symbol_info(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_id = match require_str(args, "lib_id") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Ok(CallToolResult::error(
            "lib_id must be in 'Library:Symbol' format (e.g. 'Device:R')",
        ));
    }
    let (lib_nick, sym_name) = (parts[0], parts[1]);

    // Project dir is optional: an explicit arg wins, else the server default.
    let project_dir = args["project_dir"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| ctx.config.project_dir.clone());

    let lib_path = match resolve_symbol_lib_path(lib_nick, sym_name, project_dir.as_deref()) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Symbol '{}' not found: no library '{}' in the project or global \
                 sym-lib-table, nor in the installed KiCad symbol libraries",
                lib_id, lib_nick
            )));
        }
    };

    let content = tokio::fs::read_to_string(&lib_path).await?;
    let root = parse_sexp(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse .kicad_sym library '{lib_nick}': {e}"))?;

    // Top-level symbol with the exact name (the lib_id suffix). Nested unit
    // sub-symbols (NAME_N_M) are one level deeper, so they are skipped here.
    let sym_node = root
        .find_all("symbol")
        .into_iter()
        .find(|s| s.get(1).and_then(|n| n.as_str()) == Some(sym_name));
    let sym_node = match sym_node {
        Some(n) => n,
        None => {
            return Ok(CallToolResult::error(format!(
                "Symbol '{}' not found in library '{}'",
                sym_name, lib_nick
            )));
        }
    };

    // Pins live inside nested unit sub-symbols, so recurse to collect them all.
    // Derived symbols (`(extends …)`) inherit pins from their base; the helper
    // walks the extends chain so derived symbols report their inherited pins.
    let pins: Vec<serde_json::Value> = resolve_symbol_pins(&root, sym_node)
        .into_iter()
        .map(|pin| {
            let pin_type = pin.get(1).and_then(|n| n.as_str()).unwrap_or("");
            let (px, py) = pin
                .find("at")
                .and_then(|a| Some((a.get_f64(1)?, a.get_f64(2)?)))
                .unwrap_or((0.0, 0.0));
            json!({
                "number": pin.find("number").and_then(|n| n.get(1)).and_then(|n| n.as_str()).unwrap_or(""),
                "name": pin.find("name").and_then(|n| n.get(1)).and_then(|n| n.as_str()).unwrap_or(""),
                "type": pin_type,
                "x": px,
                "y": py
            })
        })
        .collect();

    // Properties are direct children of the top-level symbol.
    let mut properties = serde_json::Map::new();
    for prop in sym_node.find_all("property") {
        if let (Some(key), Some(val)) = (
            prop.get(1).and_then(|n| n.as_str()),
            prop.get(2).and_then(|n| n.as_str()),
        ) {
            properties.insert(key.to_string(), json!(val));
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "lib_id": lib_id,
            "name": sym_name,
            "library": lib_nick,
            "pin_count": pins.len(),
            "pins": pins,
            "properties": properties
        }))
        .unwrap(),
    ))
}

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
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// A lib-table in the exact shape KiCad writes it: CRLF-terminated and
    /// TAB-indented.
    ///
    /// Field values are escaped the way `quote_lib_table_string` writes them,
    /// because a tempdir path is interpolated here and on Windows it contains
    /// backslashes: an unescaped `…\template-fp-lib-table` reads back with a
    /// TAB, so the entry pointed at a file that does not exist.
    fn kicad_style_table(kind: &str, entries: &[(&str, &str, &str)]) -> String {
        let body: String = entries
            .iter()
            .map(|(nick, ty, uri)| {
                let (nick, ty, uri) = (
                    escape_library_string(nick),
                    escape_library_string(ty),
                    escape_library_string(uri),
                );
                format!(
                    "\t(lib (name \"{nick}\") (type \"{ty}\") (uri \"{uri}\") (options \"\") (descr \"\"))\r\n"
                )
            })
            .collect();
        format!("({kind}\r\n\t(version 7)\r\n{body})\r\n")
    }

    /// Serializes tests that set KICAD10_FOOTPRINT_DIR (process-wide env), the
    /// way `sch_components`' `SYMBOL_DIR_ENV` does for the symbol equivalent.
    static FOOTPRINT_DIR_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point `KICAD10_FOOTPRINT_DIR` at `dir` for as long as the returned guard
    /// lives.
    ///
    /// Rust runs tests in threads of one process, so two tests setting this to
    /// their own tempdir would race. Holding the lock serializes them, and
    /// restoring the previous value keeps a developer's real KiCad environment
    /// intact for whatever runs next.
    fn footprint_dir_env(dir: &Path) -> FootprintDirEnv {
        let guard = FOOTPRINT_DIR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        // var_os, not var: a value this process cannot decode as UTF-8 is still
        // one the developer set, and `var` would report it as absent, leaving
        // the restore to silently delete it.
        let previous = std::env::var_os("KICAD10_FOOTPRINT_DIR");
        std::env::set_var("KICAD10_FOOTPRINT_DIR", dir);
        FootprintDirEnv {
            _guard: guard,
            previous,
        }
    }

    struct FootprintDirEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl Drop for FootprintDirEnv {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var("KICAD10_FOOTPRINT_DIR", v),
                None => std::env::remove_var("KICAD10_FOOTPRINT_DIR"),
            }
        }
    }

    #[tokio::test]
    async fn list_footprint_libraries_reads_a_table_kicad_wrote() {
        // End-to-end regression for the user-visible symptom: on a stock KiCad
        // 10 install every library listing returned {"count": 0}, which left
        // place_component unable to resolve any Library:Footprint id. Drive the
        // real handler with a table in the exact shape KiCad writes.
        let tmp = tempfile::tempdir().unwrap();
        let pretty = tmp.path().join("MyParts.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        let table = kicad_style_table(
            "fp_lib_table",
            &[("MyParts", "KiCad", &pretty.to_string_lossy())],
        );
        assert!(
            !table.contains("\n  (lib "),
            "fixture must be in KiCad's tab format, not the old needle's"
        );
        std::fs::write(tmp.path().join("fp-lib-table"), table).unwrap();

        let args = json!({
            "project": tmp.path().join("board.kicad_pro").to_string_lossy(),
            "scope": "project",
        });
        let res = handle_list_footprint_libraries(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);

        let out: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();
        assert_eq!(out["count"], 1, "library not found: {out}");
        assert_eq!(out["libraries"][0]["nickname"], "MyParts");
        assert_eq!(
            out["libraries"][0]["path"].as_str().map(PathBuf::from),
            Some(pretty),
            "the resolved directory should be reported alongside the raw uri"
        );
    }

    #[tokio::test]
    async fn list_footprint_libraries_expands_a_nested_table_of_env_var_uris() {
        // The two things that kept KiCad's ~155 bundled libraries invisible even
        // once the table parsed: a `(type "Table")` indirection, and entries
        // addressed as ${KICAD10_FOOTPRINT_DIR}/Foo.pretty.
        let tmp = tempfile::tempdir().unwrap();
        let shipped = tmp.path().join("share");
        let pretty = shipped.join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        let _env = footprint_dir_env(&shipped);

        let nested = tmp.path().join("template-fp-lib-table");
        std::fs::write(
            &nested,
            kicad_style_table(
                "fp_lib_table",
                &[(
                    "Resistor_SMD",
                    "KiCad",
                    "${KICAD10_FOOTPRINT_DIR}/Resistor_SMD.pretty",
                )],
            ),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("fp-lib-table"),
            kicad_style_table(
                "fp_lib_table",
                &[("KiCad", "Table", &nested.to_string_lossy())],
            ),
        )
        .unwrap();

        let args = json!({
            "project": tmp.path().join("board.kicad_pro").to_string_lossy(),
            "scope": "project",
        });
        let res = handle_list_footprint_libraries(&args, &test_ctx())
            .await
            .unwrap();
        let out: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();

        assert_eq!(out["count"], 1, "nested table not expanded: {out}");
        assert_eq!(out["libraries"][0]["nickname"], "Resistor_SMD");
        assert_eq!(
            out["libraries"][0]["path"].as_str().map(PathBuf::from),
            Some(pretty),
            "env-var URI should resolve to a real directory"
        );
    }

    #[test]
    fn parse_lib_table_reads_kicad10_crlf_tab_format() {
        // Regression: parse_lib_table hard-coded the needle `\n  (lib ` (LF +
        // exactly 2 spaces). KiCad writes these tables CRLF-terminated and
        // TAB-indented, so the needle never matched and every library listing
        // came back empty — which in turn made footprint placement unable to
        // resolve any `Library:Footprint` id.
        let content = kicad_style_table(
            "fp_lib_table",
            &[
                ("OpenDongle", "KiCad", "/tmp/OpenDongle"),
                ("wch-antenna", "KiCad", "/tmp/wch.pretty"),
            ],
        );
        assert!(
            !content.contains("\n  (lib "),
            "fixture must not contain the old LF/2-space needle"
        );

        let libs = parse_lib_table(&content);
        assert_eq!(libs.len(), 2, "parsed: {libs:?}");
        assert_eq!(libs[0]["nickname"], "OpenDongle");
        assert_eq!(libs[1]["uri"], "/tmp/wch.pretty");
    }

    #[test]
    fn parse_lib_table_still_reads_two_space_indentation() {
        // konnect's own writer emits two-space indentation; both must work.
        let content = "(fp_lib_table\n  (version 7)\n  (lib (name \"Local\") (type \"KiCad\") (uri \"/tmp/local.pretty\") (options \"\") (descr \"\"))\n)\n";
        let libs = parse_lib_table(content);
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0]["nickname"], "Local");
    }

    #[test]
    fn parse_lib_table_keeps_an_escaped_quote_in_a_description() {
        // Regression: fields were scraped with a substring scan that stopped at
        // the first raw `"`, so an escaped quote truncated the value — a descr
        // of `0.1" pitch` came back as `0.1\`. The parser decodes the escape.
        let content = "(fp_lib_table\n\t(lib (name \"Headers\") (type \"KiCad\") \
             (uri \"/tmp/h.pretty\") (options \"\") (descr \"0.1\\\" pitch headers\"))\n)\n";

        let libs = parse_lib_table(content);
        assert_eq!(libs.len(), 1, "parsed: {libs:?}");
        assert_eq!(libs[0]["description"], "0.1\" pitch headers");
        assert_eq!(libs[0]["uri"], "/tmp/h.pretty");
    }

    #[test]
    fn parse_lib_table_reads_a_field_split_across_lines() {
        // The scan required the literal `(name "` — one space, quote next. A
        // table written with the value on its own line matched nothing and the
        // entry came back with every field empty.
        let content = concat!(
            "(fp_lib_table\n",
            "  (lib\n",
            "    (name\n",
            "      \"Split\")\n",
            "    (type \"KiCad\")\n",
            "    (uri \"/tmp/split.pretty\"))\n",
            ")\n",
        );

        let libs = parse_lib_table(content);
        assert_eq!(libs.len(), 1, "parsed: {libs:?}");
        assert_eq!(libs[0]["nickname"], "Split");
        assert_eq!(libs[0]["uri"], "/tmp/split.pretty");
    }

    #[test]
    fn parse_lib_table_reads_an_escaped_windows_uri() {
        // A Windows URI is a string full of escape introducers, and the one
        // that bites is a path component starting with `t`: written escaped —
        // which is how KiCad and `quote_lib_table_string` write it — it must
        // come back as a backslash and a `t`, not a TAB.
        let content = concat!(
            "(fp_lib_table\r\n",
            "\t(lib (name \"Parts\") (type \"Table\")",
            " (uri \"C:\\\\Users\\\\me\\\\template-fp-lib-table\")",
            " (options \"\") (descr \"\"))\r\n",
            ")\r\n",
        );

        let libs = parse_lib_table(content);
        assert_eq!(libs.len(), 1, "parsed: {libs:?}");
        assert_eq!(libs[0]["uri"], r"C:\Users\me\template-fp-lib-table");
    }

    #[test]
    fn parse_lib_table_skips_only_the_entry_that_does_not_parse() {
        // Entries are located textually so one unbalanced block cannot discard
        // the rest of the table, which a whole-file parse would.
        let content = concat!(
            "(fp_lib_table\n",
            "  (lib (name \"Good\") (type \"KiCad\") (uri \"/tmp/good.pretty\"))\n",
            "  (lib (name \"Bad\" (type \"KiCad\")\n",
            ")\n",
        );

        let libs = parse_lib_table(content);
        assert_eq!(libs.len(), 1, "parsed: {libs:?}");
        assert_eq!(libs[0]["nickname"], "Good");
    }

    #[test]
    fn flatten_lib_table_follows_nested_table_entries() {
        // KiCad 10's default global table does not copy the ~155 bundled
        // libraries; it holds one `(type "Table")` entry pointing at the
        // template table that KiCad ships. Treating that as a library makes
        // every bundled library invisible.
        let tmp = tempfile::tempdir().unwrap();
        let leaf_dir = tmp.path().join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&leaf_dir).unwrap();

        let nested = tmp.path().join("template-fp-lib-table");
        std::fs::write(
            &nested,
            kicad_style_table(
                "fp_lib_table",
                &[("Resistor_SMD", "KiCad", &leaf_dir.to_string_lossy())],
            ),
        )
        .unwrap();

        let root = kicad_style_table(
            "fp_lib_table",
            &[("KiCad", "Table", &nested.to_string_lossy())],
        );

        let libs = flatten_lib_table(&root, 0, None);
        assert_eq!(libs.len(), 1, "nested table not followed: {libs:?}");
        assert_eq!(libs[0]["nickname"], "Resistor_SMD");
        assert_eq!(
            libs[0]["path"].as_str().map(PathBuf::from),
            Some(leaf_dir),
            "resolved path missing"
        );
    }

    #[test]
    fn flatten_lib_table_stops_at_a_self_referencing_table() {
        // A table that points at itself must not recurse forever.
        let tmp = tempfile::tempdir().unwrap();
        let table = tmp.path().join("fp-lib-table");
        std::fs::write(
            &table,
            kicad_style_table(
                "fp_lib_table",
                &[("Loop", "Table", &table.to_string_lossy())],
            ),
        )
        .unwrap();

        let content = std::fs::read_to_string(&table).unwrap();
        assert!(flatten_lib_table(&content, 0, None).is_empty());
    }

    #[test]
    fn is_lib_id_separates_library_ids_from_paths() {
        assert!(is_lib_id("Resistor_SMD:R_0402"));
        assert!(is_lib_id("MyParts:Weird:Name")); // only the first colon splits

        // Paths, by separator.
        assert!(!is_lib_id(r"C:\KiCad\R.kicad_mod"));
        assert!(!is_lib_id("/usr/share/kicad/R.kicad_mod"));
        assert!(!is_lib_id("Resistor_SMD.pretty/R.kicad_mod"));
        // No colon at all.
        assert!(!is_lib_id("R_0402.kicad_mod"));
    }

    #[test]
    fn a_windows_drive_relative_path_is_not_a_library_id() {
        // `C:R.kicad_mod` means R.kicad_mod in drive C's current directory. It
        // has a colon and no separator, so it is shaped exactly like a lib id;
        // the one-letter prefix is what gives it away.
        assert!(!is_lib_id("C:R_0402.kicad_mod"));
        assert!(!is_lib_id("d:board.kicad_mod"));
        // Two letters is a nickname again — no drive is named "Ab".
        assert!(is_lib_id("Ab:R_0402"));
    }

    #[test]
    fn an_absent_lib_table_is_not_an_error() {
        // Every caller checks both the global and project tables, and a project
        // without its own is the normal case.
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("fp-lib-table");
        assert_eq!(read_lib_table_checked(&absent), Ok(Vec::new()));
    }

    #[test]
    fn an_unreadable_lib_table_is_an_error_not_an_empty_list() {
        // Reading a directory as a file fails with something other than
        // NotFound on every platform, which is the case that must not be
        // folded into "0 libraries" — that is the symptom of the very bug this
        // module fixes.
        let tmp = tempfile::tempdir().unwrap();
        let dir_as_table = tmp.path().join("fp-lib-table");
        std::fs::create_dir(&dir_as_table).unwrap();

        let err = read_lib_table_checked(&dir_as_table)
            .expect_err("a table that exists but cannot be read must be reported");
        assert!(err.contains("fp-lib-table"), "must name the table: {err}");
    }

    #[tokio::test]
    async fn list_footprint_libraries_reports_an_unreadable_table() {
        // The handler-level half: this used to surface a read error via `?`
        // before the table read was centralised, and must still.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("fp-lib-table")).unwrap();

        let args = json!({
            "project": tmp.path().join("board.kicad_pro").to_string_lossy(),
            "scope": "project",
        });
        let res = handle_list_footprint_libraries(&args, &test_ctx())
            .await
            .unwrap();
        assert!(
            res.is_error,
            "an unreadable table must not report zero libraries: {:?}",
            res.content
        );
    }

    #[test]
    fn a_missing_footprint_path_names_itself() {
        // Without the existence check the caller's read fails with a bare
        // "os error 2" that never mentions the file, so the message is the
        // point of the test.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.kicad_mod");
        let err = resolve_footprint_path(&missing.to_string_lossy(), None)
            .expect_err("a nonexistent path must not resolve");
        assert!(err.contains("nope.kicad_mod"), "must name the file: {err}");
        assert!(
            err.contains("Library:Footprint"),
            "should say what the alternative is: {err}"
        );
    }

    #[test]
    fn a_directory_is_not_a_footprint() {
        // is_file, not exists — a .pretty directory would otherwise resolve and
        // fail confusingly at read time.
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_footprint_path(&tmp.path().to_string_lossy(), None).is_err());
    }

    #[test]
    fn an_existing_footprint_path_resolves_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("R_0805.kicad_mod");
        std::fs::write(&file, "(footprint \"R_0805\")").unwrap();
        assert_eq!(
            resolve_footprint_path(&file.to_string_lossy(), None).unwrap(),
            file
        );
    }

    #[test]
    fn a_project_registered_library_resolves() {
        // register_footprint_library writes to the project fp-lib-table by
        // default, so a global-only lookup could not see anything it
        // registered — the default workflow resolved to "library not found".
        let tmp = tempfile::tempdir().unwrap();
        let pretty = tmp.path().join("MyProjLib.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        std::fs::write(pretty.join("Foo.kicad_mod"), "(footprint \"Foo\")").unwrap();
        std::fs::write(
            tmp.path().join("fp-lib-table"),
            kicad_style_table(
                "fp_lib_table",
                &[("MyProjLib", "KiCad", &pretty.to_string_lossy())],
            ),
        )
        .unwrap();

        assert_eq!(
            resolve_footprint_path("MyProjLib:Foo", Some(tmp.path())).unwrap(),
            pretty.join("Foo.kicad_mod")
        );
        // Without the project dir it is invisible, which is the bug.
        assert!(resolve_footprint_path("MyProjLib:Foo", None).is_err());
    }

    #[test]
    fn an_unregistered_nickname_falls_back_to_the_conventional_pretty_dir() {
        // A stock install whose global table is missing or unreadable can
        // still serve Resistor_SMD:R_0402 from <libdir>/Resistor_SMD.pretty.
        let tmp = tempfile::tempdir().unwrap();
        let pretty = tmp.path().join("Fallback_Lib.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        std::fs::write(pretty.join("R_1.kicad_mod"), "(footprint \"R_1\")").unwrap();
        let _env = footprint_dir_env(tmp.path());

        assert_eq!(
            resolve_footprint_path("Fallback_Lib:R_1", None).unwrap(),
            pretty.join("R_1.kicad_mod")
        );
    }

    #[test]
    fn a_missing_library_error_names_the_nickname_and_attempted_locations() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = footprint_dir_env(tmp.path());
        let err = resolve_footprint_path("NoSuchLib:R_1", Some(tmp.path()))
            .expect_err("an unknown nickname must not resolve");
        assert!(err.contains("NoSuchLib"), "must name the library: {err}");
        assert!(
            err.contains("libraries known"),
            "should count the known libraries: {err}"
        );
        assert!(
            err.contains("NoSuchLib.pretty"),
            "should list the attempted fallback location: {err}"
        );
    }

    #[test]
    fn expand_lib_uri_expands_a_kicad_env_var() {
        let tmp = tempfile::tempdir().unwrap();
        let pretty = tmp.path().join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        let _env = footprint_dir_env(tmp.path());

        assert_eq!(
            expand_lib_uri("${KICAD10_FOOTPRINT_DIR}/Resistor_SMD.pretty", None),
            Some(pretty)
        );
        assert_eq!(
            expand_lib_uri("/plain/path", None),
            Some(PathBuf::from("/plain/path")),
            "a non-variable URI must pass through untouched"
        );
    }

    #[test]
    fn kiprjmod_resolves_against_the_tables_own_directory() {
        // The default register_footprint_library scope is "project", which
        // writes ${KIPRJMOD}/… entries — the common case, not an edge (#61
        // repro case 1 was exactly this).
        let tmp = tempfile::tempdir().unwrap();
        let pretty = tmp.path().join("MyParts.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        let table = tmp.path().join("fp-lib-table");
        std::fs::write(
            &table,
            "(fp_lib_table\n\t(version 7)\n\t(lib (name \"MyParts\") (type \"KiCad\") (uri \"${KIPRJMOD}/MyParts.pretty\") (options \"\") (descr \"\"))\n)\n",
        )
        .unwrap();

        let libs = read_lib_table_checked(&table).unwrap();
        assert_eq!(libs.len(), 1);
        assert_eq!(
            libs[0]["path"].as_str().map(PathBuf::from),
            Some(pretty),
            "a project-scoped ${{KIPRJMOD}} URI must resolve via the table's directory"
        );

        // Without a project context (direct call, no table), it must not
        // resolve rather than guess.
        assert_eq!(expand_lib_uri("${KIPRJMOD}/MyParts.pretty", None), None);
    }

    #[test]
    fn table_root_element_matches_the_table_kind() {
        // Credit: PR #54 — the scaffold was hardcoded to fp_lib_table, so
        // registering a symbol library on a machine with no global
        // sym-lib-table wrote a file KiCad rejects.
        assert_eq!(
            table_root_element(Path::new("sym-lib-table")),
            "sym_lib_table"
        );
        assert_eq!(
            table_root_element(Path::new("C:/proj/sym-lib-table")),
            "sym_lib_table"
        );
        assert_eq!(
            table_root_element(Path::new("fp-lib-table")),
            "fp_lib_table"
        );
    }

    #[tokio::test]
    async fn registering_a_symbol_library_scaffolds_a_sym_root() {
        let tmp = tempfile::tempdir().unwrap();
        let table = tmp.path().join("sym-lib-table");
        let repaired = register_in_lib_table(&table, "MySyms", "${KIPRJMOD}/my.kicad_sym", "KiCad")
            .await
            .unwrap();
        assert!(!repaired, "a fresh scaffold has nothing to repair");
        let content = std::fs::read_to_string(&table).unwrap();
        assert!(
            content.starts_with("(sym_lib_table"),
            "scaffold root must match the table kind, got: {content}"
        );
        assert!(content.contains("\"MySyms\""));
    }

    /// A sym-lib-table an older build wrote with an `(fp_lib_table` root is
    /// rejected wholesale by KiCad ("has type FOOTPRINT but expected SYMBOL;
    /// skipping"). #54 stopped new files being written that way; without a
    /// repair the existing ones stay broken through every later registration.
    const BROKEN_SYM_TABLE: &str = "(fp_lib_table\n  (version 7)\n  (lib (name \"Old\") (type \"KiCad\") (uri \"/abs/old.kicad_sym\") (options \"\") (descr \"\"))\n)\n";

    #[tokio::test]
    async fn an_existing_sym_table_with_an_fp_root_is_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let table = tmp.path().join("sym-lib-table");
        std::fs::write(&table, BROKEN_SYM_TABLE).unwrap();

        let repaired = register_in_lib_table(&table, "MySyms", "/abs/my.kicad_sym", "KiCad")
            .await
            .unwrap();

        assert!(
            repaired,
            "the wrong root must be reported, not fixed mutely"
        );
        let content = std::fs::read_to_string(&table).unwrap();
        assert!(
            content.starts_with("(sym_lib_table"),
            "root must be repaired, got: {content}"
        );
        assert!(!content.contains("fp_lib_table"), "{content}");
        assert!(
            content.contains("\"Old\""),
            "existing entry lost: {content}"
        );
        assert!(content.contains("\"MySyms\""), "{content}");
    }

    /// The idempotent "already registered" early return used to skip the write
    /// entirely, so re-registering the same nickname left the bad root in place.
    #[tokio::test]
    async fn a_repair_still_lands_when_the_nickname_is_already_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let table = tmp.path().join("sym-lib-table");
        std::fs::write(&table, BROKEN_SYM_TABLE).unwrap();

        let repaired = register_in_lib_table(&table, "Old", "/abs/old.kicad_sym", "KiCad")
            .await
            .unwrap();

        assert!(repaired);
        let content = std::fs::read_to_string(&table).unwrap();
        assert!(content.starts_with("(sym_lib_table"), "{content}");
        assert_eq!(
            content.matches("(lib ").count(),
            1,
            "an already-registered nickname must not be duplicated: {content}"
        );
    }

    #[test]
    fn register_footprint_library_schema_exposes_opt_in_replacement() {
        let registration = tools()
            .into_iter()
            .find(|tool| tool.name == "register_footprint_library")
            .expect("library must expose register_footprint_library");
        let replace = &registration.input_schema["properties"]["replace_existing"];

        assert_eq!(replace["type"], "boolean");
        assert_eq!(replace["default"], false);
        assert_eq!(
            registration.input_schema["required"],
            json!(["library_path", "nickname"])
        );
    }

    /// #211: registering a nickname that already exists silently reported
    /// success while changing nothing, so a stale project URI could not be
    /// corrected and the caller could not tell. The footprint half gained a
    /// reported state and `replace_existing` in #205; the symbol half kept
    /// the old always-`true` shape, which left the API asymmetric.
    #[tokio::test]
    async fn register_symbol_library_reports_that_it_changed_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("test").join("board.kicad_pro");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        let table = project.parent().unwrap().join("sym-lib-table");
        let original = "(sym_lib_table
  (version 7)
  (lib (name \"Parts\") (type \"KiCad\") (uri \"C:/stale/Parts.kicad_sym\") (options \"keep=1\") (descr \"keep me\"))
)
";
        std::fs::write(&table, original).unwrap();

        let result = handle_register_symbol_library(
            &json!({
                "library_path": tmp.path().join("lib").join("Parts.kicad_sym"),
                "nickname": "Parts",
                "project": project
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(
            output["state"], "unchanged",
            "a no-op must say so rather than reporting a bare success: {output}"
        );
        assert_eq!(std::fs::read_to_string(&table).unwrap(), original);
    }

    #[tokio::test]
    async fn register_symbol_library_reports_a_new_entry_as_inserted() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("test").join("board.kicad_pro");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();

        let result = handle_register_symbol_library(
            &json!({
                "library_path": tmp.path().join("lib").join("Parts.kicad_sym"),
                "nickname": "Parts",
                "project": project
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["state"], "inserted", "{output}");
    }

    /// The point of the issue: a stale URI must be correctable, and the entry's
    /// own options/descr must survive the correction.
    #[tokio::test]
    async fn register_symbol_library_replaces_a_stale_uri_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("test").join("board.kicad_pro");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, "{}").unwrap();
        let library = tmp.path().join("test").join("Parts.kicad_sym");
        std::fs::write(&library, "(kicad_symbol_lib)").unwrap();
        let table = project.parent().unwrap().join("sym-lib-table");
        std::fs::write(
            &table,
            "(sym_lib_table
  (version 7)
  (lib (name \"Parts\") (type \"KiCad\") (uri \"C:/stale/Parts.kicad_sym\") (options \"keep=1\") (descr \"keep me\"))
)
",
        )
        .unwrap();

        let result = handle_register_symbol_library(
            &json!({
                "library_path": library,
                "nickname": "Parts",
                "project": project,
                "replace_existing": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["state"], "updated", "{output}");
        let written = std::fs::read_to_string(&table).unwrap();
        assert!(
            !written.contains("C:/stale"),
            "stale URI must be gone:
{written}"
        );
        assert!(
            written.contains("keep=1") && written.contains("keep me"),
            "the entry's own metadata must survive:
{written}"
        );
    }

    #[tokio::test]
    async fn register_symbol_library_rejects_a_non_boolean_replace_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("test").join("board.kicad_pro");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        let result = handle_register_symbol_library(
            &json!({
                "library_path": tmp.path().join("lib").join("Parts.kicad_sym"),
                "nickname": "Parts",
                "project": project,
                "replace_existing": "yes"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{:?}", result.content);
    }

    #[tokio::test]
    async fn register_footprint_library_keeps_existing_uri_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("test").join("board.kicad_pro");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        let table = project.parent().unwrap().join("fp-lib-table");
        let original = "(fp_lib_table\n  (version 7)\n  (lib (name \"Parts\") (type \"KiCad\") (uri \"C:/stale/Parts.pretty\") (options \"keep=1\") (descr \"keep me\"))\n)\n";
        std::fs::write(&table, original).unwrap();

        let result = handle_register_footprint_library(
            &json!({
                "library_path": tmp.path().join("lib").join("Parts.pretty"),
                "nickname": "Parts",
                "project": project
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["state"], "unchanged");
        assert_eq!(std::fs::read_to_string(table).unwrap(), original);
    }

    #[tokio::test]
    async fn register_footprint_library_replaces_uri_portably_and_preserves_entry_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let project = tmp.path().join("test").join("board.kicad_pro");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, "{}").unwrap();
        let library = tmp.path().join("lib").join("Parts.pretty");
        std::fs::create_dir_all(&library).unwrap();
        let table = project.parent().unwrap().join("fp-lib-table");
        let original = "(fp_lib_table\n\t(version 7)\n\t(lib (name \"Other\") (type \"KiCad\") (uri \"${KIPRJMOD}/other.pretty\") (options \"\") (descr \"other\"))\n\t(lib (name \"Parts\") (type \"KiCad\") (uri \"C:/stale/Parts.pretty\") (options \"keep=1\") (descr \"keep me\"))\n)\n";
        std::fs::write(&table, original).unwrap();

        let result = handle_register_footprint_library(
            &json!({
                "library_path": library,
                "nickname": "Parts",
                "project": project,
                "replace_existing": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["state"], "updated");
        assert_eq!(output["uri"], "${KIPRJMOD}/../lib/Parts.pretty");

        let updated = std::fs::read_to_string(table).unwrap();
        assert!(updated.starts_with("(fp_lib_table\n\t(version 7)"));
        assert!(updated
            .contains("(name \"Other\") (type \"KiCad\") (uri \"${KIPRJMOD}/other.pretty\")"));
        assert!(updated.contains("(name \"Parts\") (type \"KiCad\") (uri \"${KIPRJMOD}/../lib/Parts.pretty\") (options \"keep=1\") (descr \"keep me\")"));
        assert!(!updated.contains("C:/stale"));
    }

    /// The whole reported failure, end to end: two projects side by side under
    /// one parent, the same nickname registered in each. Before, both resolved
    /// to a stray table in the parent, so the second call reported `unchanged`
    /// against the first project's entry and neither project ever saw one.
    #[tokio::test]
    async fn registering_by_project_directory_keeps_two_projects_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let alpha = tmp.path().join("alpha");
        let beta = tmp.path().join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let library = tmp.path().join("shared.kicad_sym");
        std::fs::write(&library, "(kicad_symbol_lib)\n").unwrap();

        let register = |project: std::path::PathBuf| {
            let library = library.clone();
            async move {
                handle_register_symbol_library(
                    &json!({
                        "library_path": library,
                        "nickname": "Shared",
                        "project": project
                    }),
                    &test_ctx(),
                )
                .await
                .unwrap()
            }
        };

        let first = register(alpha.clone()).await;
        let second = register(beta.clone()).await;

        assert!(!first.is_error, "{:?}", first.content);
        assert!(!second.is_error, "{:?}", second.content);
        let first: serde_json::Value = serde_json::from_str(&result_text(&first)).unwrap();
        let second: serde_json::Value = serde_json::from_str(&result_text(&second)).unwrap();
        assert_eq!(first["state"], "inserted");
        assert_eq!(
            second["state"], "inserted",
            "the second project must get its own entry, not `unchanged`"
        );

        assert!(
            alpha.join("sym-lib-table").exists(),
            "alpha must carry its own table"
        );
        assert!(
            beta.join("sym-lib-table").exists(),
            "beta must carry its own table"
        );
        assert!(
            !tmp.path().join("sym-lib-table").exists(),
            "nothing may be written above the projects"
        );
    }

    /// A project argument that names nothing is refused instead of resolving to
    /// its parent and reporting success there.
    #[tokio::test]
    async fn registering_against_a_missing_project_refuses_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let library = tmp.path().join("parts.kicad_sym");
        std::fs::write(&library, "(kicad_symbol_lib)\n").unwrap();

        let result = handle_register_symbol_library(
            &json!({
                "library_path": library,
                "nickname": "Parts",
                "project": tmp.path().join("no-such-project")
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error, "{:?}", result.content);
        assert!(!tmp.path().join("sym-lib-table").exists());
    }

    #[tokio::test]
    async fn register_footprint_library_rejects_duplicate_nicknames_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("board.kicad_pro");
        let table = tmp.path().join("fp-lib-table");
        let duplicate = "(fp_lib_table\n  (version 7)\n  (lib (name \"Parts\") (type \"KiCad\") (uri \"/one\") (options \"\") (descr \"\"))\n  (lib (name \"Parts\") (type \"KiCad\") (uri \"/two\") (options \"\") (descr \"\"))\n)\n";
        std::fs::write(&table, duplicate).unwrap();

        let result = handle_register_footprint_library(
            &json!({
                "library_path": tmp.path().join("Parts.pretty"),
                "nickname": "Parts",
                "project": project,
                "replace_existing": true
            }),
            &test_ctx(),
        )
        .await
        .expect("a malformed table is a tool error, not a transport error");

        assert!(result.is_error);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["error"]["kind"], "invalid_argument");
        assert_eq!(output["error"]["field"], "nickname");
        assert_eq!(std::fs::read_to_string(table).unwrap(), duplicate);
    }

    #[test]
    fn stale_library_table_source_is_rejected_without_overwriting_newer_content() {
        let tmp = tempfile::tempdir().unwrap();
        let table = tmp.path().join("fp-lib-table");
        let original = "(fp_lib_table\n  (version 7)\n)\n";
        std::fs::write(&table, original).unwrap();
        let prepared = "(fp_lib_table\n  (version 7)\n  (lib (name \"Parts\") (type \"KiCad\") (uri \"/parts\") (options \"\") (descr \"\"))\n)\n";
        let newer = "(fp_lib_table\n  (version 7)\n  (lib (name \"Other\") (type \"KiCad\") (uri \"/other\") (options \"\") (descr \"\"))\n)\n";
        std::fs::write(&table, newer).unwrap();

        let error = persist_lib_table_registration(&table, original, prepared)
            .expect_err("a stale expected table must conflict");
        assert!(matches!(error, konnect_sexp::SexpError::Conflict { .. }));
        assert_eq!(std::fs::read_to_string(table).unwrap(), newer);
    }

    const DUPLICATE_PAD_FOOTPRINT: &str = r#"(footprint "DualSocket"
  (version 20240108)
  (generator "pcbnew")
  (layer "F.Cu")
  (descr "preserve me")
  (fp_line (start 0 0) (end 1 1) (stroke (width 0.1) (type default)) (layer "F.SilkS"))
  (pad "3" thru_hole circle (at 1 2) (size 2 2) (drill 1) (layers "*.Cu" "*.Mask"))
  (pad "3" thru_hole oval (at 3 4 90) (size 3 2) (drill oval 1 2) (layers "*.Cu" "*.Mask"))
  (pad "1" thru_hole circle (at 5 6) (size 2 2) (drill 1) (layers "*.Cu" "*.Mask"))
  (model "../models/keep.step"
    (offset (xyz 0 0 0))
    (scale (xyz 1 1 1))
    (rotate (xyz 0 0 0)))
)
"#;

    #[tokio::test]
    async fn edit_footprint_pad_renumbers_only_the_first_duplicate_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("DualSocket.kicad_mod");
        std::fs::write(&path, DUPLICATE_PAD_FOOTPRINT).unwrap();

        let result = handle_edit_footprint_pad(
            &json!({
                "footprint_path": path,
                "pad_number": "3",
                "new_number": "1"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["matched_count"], 1);
        assert_eq!(output["updated_count"], 1);

        let updated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(updated.matches("(pad \"3\"").count(), 1);
        assert_eq!(updated.matches("(pad \"1\"").count(), 2);
        assert!(
            updated.contains("(pad \"1\" thru_hole circle (at 1 2) (size 2 2) (drill 1)"),
            "the first duplicate should be renumbered: {updated}"
        );
        assert!(
            updated.contains("(pad \"3\" thru_hole oval (at 3 4 90) (size 3 2) (drill oval 1 2)"),
            "the second duplicate should remain unchanged: {updated}"
        );
    }

    #[tokio::test]
    async fn edit_footprint_pad_persists_a_valid_shape_change() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("DualSocket.kicad_mod");
        std::fs::write(&path, DUPLICATE_PAD_FOOTPRINT).unwrap();

        let result = handle_edit_footprint_pad(
            &json!({
                "footprint_path": path,
                "pad_number": "3",
                "shape": "roundrect"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let updated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(updated.matches("thru_hole roundrect").count(), 1);
        assert_eq!(updated.matches("(roundrect_rratio 0.25)").count(), 1);
        parse_sexp(&updated).expect("edited footprint stays parseable");
    }

    #[tokio::test]
    async fn edit_footprint_pad_renumbers_and_resizes_every_duplicate_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("DualSocket.kicad_mod");
        std::fs::write(&path, DUPLICATE_PAD_FOOTPRINT).unwrap();

        let result = handle_edit_footprint_pad(
            &json!({
                "footprint_path": path,
                "pad_number": "3",
                "new_number": "1",
                "match_all": true,
                "x": 9.5,
                "width": 4.0,
                "height": 5.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["matched_count"], 2);
        assert_eq!(output["updated_count"], 2);

        let updated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(updated.matches("(pad \"3\"").count(), 0);
        assert_eq!(updated.matches("(pad \"1\"").count(), 3);
        assert_eq!(updated.matches("(at 9.5 ").count(), 2);
        assert_eq!(updated.matches("(size 4 5)").count(), 2);
        assert!(updated.contains("(descr \"preserve me\")"));
        assert!(updated.contains("(fp_line (start 0 0) (end 1 1)"));
        assert!(updated.contains("(model \"../models/keep.step\""));
        parse_sexp(&updated).expect("the edited footprint must remain parseable");
    }

    #[tokio::test]
    async fn edit_footprint_pad_missing_match_returns_structured_error_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("DualSocket.kicad_mod");
        std::fs::write(&path, DUPLICATE_PAD_FOOTPRINT).unwrap();

        let result = handle_edit_footprint_pad(
            &json!({
                "footprint_path": path,
                "pad_number": "404",
                "new_number": "1",
                "match_all": true
            }),
            &test_ctx(),
        )
        .await
        .expect("a missing pad is a tool error, not a transport error");

        assert!(result.is_error);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["error"]["kind"], "invalid_argument");
        assert_eq!(output["error"]["field"], "pad_number");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            DUPLICATE_PAD_FOOTPRINT
        );
    }

    #[tokio::test]
    async fn edit_footprint_pad_rejects_non_string_new_number_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("DualSocket.kicad_mod");
        std::fs::write(&path, DUPLICATE_PAD_FOOTPRINT).unwrap();

        let result = handle_edit_footprint_pad(
            &json!({
                "footprint_path": path,
                "pad_number": "3",
                "new_number": 1
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["error"]["kind"], "invalid_argument");
        assert_eq!(output["error"]["field"], "new_number");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            DUPLICATE_PAD_FOOTPRINT
        );
    }

    #[test]
    fn edit_footprint_pad_schema_exposes_batch_renumbering_compatibly() {
        let tool = tools()
            .into_iter()
            .find(|tool| tool.name == "edit_footprint_pad")
            .expect("library must expose edit_footprint_pad");
        let properties = &tool.input_schema["properties"];

        assert_eq!(properties["new_number"]["type"], "string");
        assert_eq!(properties["match_all"]["type"], "boolean");
        assert_eq!(properties["match_all"]["default"], false);
        assert_eq!(
            properties["shape"]["enum"],
            json!(["circle", "rect", "oval", "roundrect"])
        );
        assert_eq!(
            tool.input_schema["required"],
            json!(["footprint_path", "pad_number"])
        );
    }

    #[test]
    fn pad_shape_change_adds_and_removes_shape_specific_children() {
        let circle = r#"(pad "1" smd circle (at 0 0) (size 2 1) (layers "F.Cu" "F.Mask"))"#;
        let rounded =
            edit_footprint_pad_block(circle, &json!({}), None, Some("roundrect")).unwrap();
        assert!(rounded.contains(r#"(pad "1" smd roundrect"#), "{rounded}");
        assert!(rounded.contains("(roundrect_rratio 0.25)"), "{rounded}");
        parse_sexp(&rounded).unwrap();

        let chamfered = r#"(pad "1" smd roundrect
  (at 0 0)
  (size 2 1)
  (layers "F.Cu" "F.Mask")
  (roundrect_rratio 0)
  (chamfer_ratio 0.2)
  (chamfer top_left)
)"#;
        let oval = edit_footprint_pad_block(chamfered, &json!({}), None, Some("oval")).unwrap();
        assert!(oval.contains(r#"(pad "1" smd oval"#), "{oval}");
        for removed in ["roundrect_rratio", "chamfer_ratio", "(chamfer "] {
            assert!(!oval.contains(removed), "left {removed} in {oval}");
        }
        parse_sexp(&oval).unwrap();
    }

    #[test]
    fn changing_one_pad_dimension_preserves_the_other() {
        let pad = r#"(pad "1" smd rect (at 0 0) (size 2 1) (layers "F.Cu"))"#;
        let wider = edit_footprint_pad_block(pad, &json!({ "width": 3.5 }), None, None).unwrap();
        assert!(wider.contains("(size 3.5 1)"), "{wider}");

        let taller = edit_footprint_pad_block(pad, &json!({ "height": 4.5 }), None, None).unwrap();
        assert!(taller.contains("(size 2 4.5)"), "{taller}");
    }

    #[tokio::test]
    async fn unsupported_pad_shape_is_rejected_before_reading_or_writing() {
        let result = handle_edit_footprint_pad(
            &json!({
                "footprint_path": "/does/not/exist.kicad_mod",
                "pad_number": "1",
                "shape": "custom"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let output: serde_json::Value = serde_json::from_str(&result_text(&result)).unwrap();
        assert_eq!(output["error"]["field"], "shape");
    }

    #[test]
    fn a_correct_root_and_a_mention_inside_a_descr_are_left_alone() {
        let ok = "(fp_lib_table\n  (version 7)\n)\n".to_string();
        assert_eq!(
            repair_table_root(ok.clone(), "fp_lib_table"),
            (ok, false),
            "a correct table must not be rewritten"
        );

        // The wrong token appears, but not as the root element.
        let nested =
            "(sym_lib_table\n  (lib (name \"X\") (descr \"copied from fp_lib_table\"))\n)\n"
                .to_string();
        assert_eq!(
            repair_table_root(nested.clone(), "sym_lib_table"),
            (nested, false),
            "only the root element may be rewritten"
        );
    }

    fn pad(number: &str, t: &str, x: f64, y: f64, w: f64, h: f64) -> PadGeom {
        PadGeom {
            number: number.into(),
            pad_type: t.into(),
            x,
            y,
            w,
            h,
        }
    }

    #[test]
    fn pads_bbox_covers_pad_extents() {
        let pads = vec![
            pad("1", "smd", -1.0, 0.0, 0.4, 0.6),
            pad("2", "smd", 1.0, 0.0, 0.4, 0.6),
        ];
        let (min_x, min_y, max_x, max_y) = pads_bbox(&pads);
        assert!((min_x - -1.2).abs() < 1e-9); // -1.0 - 0.4/2
        assert!((max_x - 1.2).abs() < 1e-9);
        assert!((min_y - -0.3).abs() < 1e-9);
        assert!((max_y - 0.3).abs() < 1e-9);
    }

    #[test]
    fn courtyard_clearance_follows_the_rule() {
        let smd = vec![pad("1", "smd", 0.0, 0.0, 0.4, 0.6)];
        let th = vec![pad("1", "thru_hole", 0.0, 0.0, 1.5, 1.5)];
        // Explicit wins over everything.
        assert_eq!(
            courtyard_clearance(Some(0.42), Some("bga"), &smd, None),
            0.42
        );
        // package_type mapping.
        assert_eq!(courtyard_clearance(None, Some("bga"), &smd, None), 1.0);
        assert_eq!(courtyard_clearance(None, Some("small"), &smd, None), 0.15);
        assert_eq!(
            courtyard_clearance(None, Some("through_hole"), &smd, None),
            0.5
        );
        assert_eq!(courtyard_clearance(None, Some("smd"), &smd, None), 0.25);
        // Auto: through-hole pad present.
        assert_eq!(courtyard_clearance(None, None, &th, None), 0.5);
        // Auto: sub-0603 body (1.0 x 0.5 mm).
        assert_eq!(
            courtyard_clearance(None, None, &smd, Some((1.0, 0.5))),
            0.15
        );
        // Auto: 0603 itself and larger stay at the SMT default.
        assert_eq!(
            courtyard_clearance(None, None, &smd, Some((1.6, 0.8))),
            0.25
        );
        assert_eq!(courtyard_clearance(None, None, &smd, None), 0.25);
    }

    #[test]
    fn pin1_index_prefers_pad_numbered_one() {
        let pads = vec![
            pad("2", "smd", 0.0, 0.0, 1.0, 1.0),
            pad("1", "smd", 2.0, 0.0, 1.0, 1.0),
        ];
        assert_eq!(pin1_index(&pads), Some(1));
        // No pad numbered "1" falls back to the first pad.
        let pads2 = vec![pad("A1", "smd", 0.0, 0.0, 1.0, 1.0)];
        assert_eq!(pin1_index(&pads2), Some(0));
        assert_eq!(pin1_index(&[]), None);
    }

    #[test]
    fn chamfered_rect_cuts_the_pin1_corner() {
        // Rectangle (0,0)-(10,10), pin 1 nearest the top-left corner.
        let pts = chamfered_rect_points(0.0, 0.0, 10.0, 10.0, 0.0, 0.0, 1.0);
        assert_eq!(pts.len(), 5, "one corner chamfered adds a vertex: {pts:?}");
        // The sharp corner is gone, replaced by two edge points.
        assert!(!pts.iter().any(|&(x, y)| x.abs() < 1e-9 && y.abs() < 1e-9));
        assert!(pts
            .iter()
            .any(|&(x, y)| (x - 0.0).abs() < 1e-9 && (y - 1.0).abs() < 1e-9));
        assert!(pts
            .iter()
            .any(|&(x, y)| (x - 1.0).abs() < 1e-9 && (y - 0.0).abs() < 1e-9));
    }

    #[test]
    fn pin_root_is_on_the_body_side_of_the_connection() {
        // Left pin (points right): bulb on the left, root to its right (body).
        let (lx, ly) = pin_root(-10.16, 0.0, 0.0, 2.54);
        assert!(
            (lx - -7.62).abs() < 1e-9 && ly.abs() < 1e-9,
            "left {lx},{ly}"
        );
        // Right pin (points left): root to the left of the bulb.
        let (rx, ry) = pin_root(10.16, 0.0, 180.0, 2.54);
        assert!(
            (rx - 7.62).abs() < 1e-9 && ry.abs() < 1e-9,
            "right {rx},{ry}"
        );
        // Up pin (points up, Y-up): root above the bulb.
        let (ux, uy) = pin_root(0.0, -5.0, 90.0, 2.54);
        assert!(ux.abs() < 1e-9 && (uy - -2.46).abs() < 1e-9, "up {ux},{uy}");
    }

    /// A pin for the body-sizing tests. `"~"` is KiCAD's "unnamed" sentinel, so
    /// it contributes no name width.
    fn test_pin(x: f64, y: f64, angle: f64, length: f64, name: &str) -> PinGeom {
        PinGeom {
            x,
            y,
            angle,
            length,
            name: name.into(),
        }
    }

    #[test]
    fn symbol_body_rect_touches_side_pins_and_spaces_the_ends() {
        let pin = |x, y, angle| test_pin(x, y, angle, 2.54, "~");
        // Three pins on the left (point right), two on the right (point left).
        let pins = vec![
            pin(-10.16, 2.54, 0.0),
            pin(-10.16, 0.0, 0.0),
            pin(-10.16, -2.54, 0.0),
            pin(10.16, 2.54, 180.0),
            pin(10.16, -2.54, 180.0),
        ];
        let (min_x, min_y, max_x, max_y) = symbol_body_rect(&pins, true).unwrap();
        // Left/right edges pass through the pin roots (pins touch the border).
        assert!((min_x - -7.62).abs() < 1e-9, "left edge {min_x}");
        assert!((max_x - 7.62).abs() < 1e-9, "right edge {max_x}");
        // Connection bulbs at x = ±10.16 stay outside the body.
        assert!(min_x > -10.16 && max_x < 10.16);
        // Top/bottom edges have no pins → spacing beyond the outermost pins.
        assert!(max_y >= 2.54 + 2.5, "top spacing {max_y}");
        assert!(min_y <= -2.54 - 2.5, "bottom spacing {min_y}");
        assert!(symbol_body_rect(&[], true).is_none());
    }

    /// A box that only touches the pin roots leaves the two columns of pin
    /// names overlapping: here a 26-character name facing an 8-character one.
    #[test]
    fn body_widens_so_facing_pin_names_do_not_collide() {
        let pin = |x, angle, name| test_pin(x, 0.0, angle, 5.08, name);
        let pins = vec![
            pin(-25.4, 0.0, "LONG/MULTI/FUNCTION/NAME/X"),
            pin(25.4, 180.0, "SHORT/NM"),
        ];
        let roots_only = symbol_body_rect(&pins, false).unwrap();
        assert!(
            (roots_only.2 - roots_only.0 - 40.64).abs() < 1e-9,
            "without names the body still hugs the pin roots: {roots_only:?}"
        );

        let (min_x, _, max_x, _) = symbol_body_rect(&pins, true).unwrap();
        let inner = max_x - min_x - 2.0 * PIN_NAME_OFFSET;
        let text = pin_name_width("LONG/MULTI/FUNCTION/NAME/X", PIN_TEXT)
            + pin_name_width("SHORT/NM", PIN_TEXT);
        assert!(
            inner >= text + PIN_NAME_GAP,
            "body {:.2} mm leaves {:.2} mm for {:.2} mm of names",
            max_x - min_x,
            inner,
            text
        );
        // Edges stay on the schematic grid so the pins meeting them do too.
        assert!(
            (min_x / SYMBOL_GRID).fract().abs() < 1e-9,
            "off grid {min_x}"
        );
    }

    /// Name width counts drawn glyphs, not the markup around them.
    #[test]
    fn body_width_ignores_unnamed_and_overbar_markup() {
        assert_eq!(pin_name_width("~", PIN_TEXT), 0.0);
        // `~{RST}` is RST with an overbar: three glyphs, not six.
        assert!(
            (pin_name_width("~{RST}", PIN_TEXT) - pin_name_width("RST", PIN_TEXT)).abs() < 1e-9
        );
    }

    /// A row with only one name needs room for that name, not for a gap too.
    #[test]
    fn a_row_with_one_name_reserves_no_column_gap() {
        let name = "SOLO/PIN/NAME";
        let one_sided = names_span(&[test_pin(-25.4, 0.0, 0.0, 5.08, name)], Axis::Horizontal);
        assert!(
            (one_sided - (pin_name_width(name, PIN_TEXT) + 2.0 * PIN_NAME_OFFSET)).abs() < 1e-9,
            "one name reserved {one_sided} mm"
        );
        // The same two names on separate rows never share one, so neither does.
        let staggered = names_span(
            &[
                test_pin(-25.4, 0.0, 0.0, 5.08, name),
                test_pin(25.4, 2.54, 180.0, 5.08, name),
            ],
            Axis::Horizontal,
        );
        assert!(
            (staggered - one_sided).abs() < 1e-9,
            "staggered {staggered}"
        );
    }

    /// Vertical pins get the same treatment on height — their names are drawn
    /// across it just as horizontal pins' names are drawn across the width.
    #[test]
    fn body_widens_vertically_for_facing_top_and_bottom_names() {
        let pins = vec![
            test_pin(0.0, -25.4, 90.0, 5.08, "LONG/MULTI/FUNCTION/NAME/X"),
            test_pin(0.0, 25.4, 270.0, 5.08, "SHORT/NM"),
        ];
        let (_, min_y, _, max_y) = symbol_body_rect(&pins, true).unwrap();
        let inner = max_y - min_y - 2.0 * PIN_NAME_OFFSET;
        let text = pin_name_width("LONG/MULTI/FUNCTION/NAME/X", PIN_TEXT)
            + pin_name_width("SHORT/NM", PIN_TEXT);
        assert!(
            inner >= text + PIN_NAME_GAP,
            "body {:.2} mm tall leaves {:.2} mm for {:.2} mm of names",
            max_y - min_y,
            inner,
            text
        );
    }

    /// `grow_to` snaps both edges to the grid even when the box it widens is
    /// centred off it — otherwise the pins slid out to meet them land off grid.
    #[test]
    fn grow_to_lands_on_grid_from_an_off_grid_centre() {
        let (mut lo, mut hi) = (-20.32, 16.51);
        grow_to(&mut lo, &mut hi, 50.0);
        assert!(hi - lo >= 50.0, "grew to {lo}..{hi}");
        for edge in [lo, hi] {
            assert!((edge / SYMBOL_GRID).fract().abs() < 1e-9, "off grid {edge}");
        }
    }

    /// Pins that defined the original box must slide out to meet the widened
    /// one, or they float inside the body, off its outline.
    #[tokio::test]
    async fn widening_the_body_slides_pins_out_to_meet_it() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("w.kicad_sym");
        handle_create_symbol(
            &json!({
                "library_path": lib.to_string_lossy(),
                "name": "WIDE",
                // Required by the schema; this test predates its enforcement
                // and relied on the handler's "U" default (#218).
                "reference_prefix": "U",
                "pins": [
                    {"number":"1","name":"LONG/MULTI/FUNCTION/NAME/X","type":"bidirectional",
                     "x":-25.4,"y":0.0,"angle":0,"length":5.08},
                    {"number":"2","name":"SHORT/NM","type":"bidirectional",
                     "x":25.4,"y":0.0,"angle":180,"length":5.08}
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let out = std::fs::read_to_string(&lib).unwrap();
        let tree = konnect_sexp::parse_sexp(&out).unwrap();
        // kicad_symbol_lib → (symbol "WIDE") → (symbol "WIDE_0_1") → body + pins
        let unit = tree.find("symbol").unwrap().find("symbol").unwrap();
        let rect = unit.find("rectangle").unwrap();
        let (sx, _) = konnect_sexp::schematic::parse_start(rect).unwrap();
        let (ex, _) = konnect_sexp::schematic::parse_end(rect).unwrap();
        for pin in unit.find_all("pin") {
            let (px, _, angle) = konnect_sexp::schematic::parse_at(pin).unwrap();
            let root = if angle == 0.0 { px + 5.08 } else { px - 5.08 };
            let edge = if angle == 0.0 { sx } else { ex };
            assert!(
                (root - edge).abs() < 1e-6,
                "pin at {px} (angle {angle}) roots at {root}, body edge is {edge}"
            );
        }
    }

    #[test]
    fn model_sexp_only_with_path() {
        assert_eq!(build_model_sexp(&json!({})), "");
        assert_eq!(build_model_sexp(&json!({ "model": {} })), "");
        let s = build_model_sexp(&json!({ "model": { "path": "x.wrl", "rotate": { "z": 90.0 } } }));
        assert!(s.contains("(model \"x.wrl\""));
        assert!(s.contains("(rotate (xyz 0 0 90)"));
        assert!(s.contains("(scale (xyz 1 1 1)"));
    }

    #[tokio::test]
    async fn create_footprint_emits_courtyard_pin1_and_model() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("TEST.kicad_mod");
        let args = json!({
            "output": out.to_string_lossy(),
            "name": "TEST_QFN",
            "pads": [
                {"number":"1","type":"smd","shape":"roundrect","x":-1.0,"y":-1.0,"width":0.3,"height":0.6},
                {"number":"2","type":"smd","shape":"roundrect","x":-1.0,"y":1.0,"width":0.3,"height":0.6},
                {"number":"3","type":"smd","shape":"roundrect","x":1.0,"y":0.0,"width":0.3,"height":0.6}
            ],
            "body_width": 2.0, "body_height": 2.0,
            "model": { "path": "${KICAD9_3DMODEL_DIR}/Package.3dshapes/TEST_QFN.wrl" }
        });
        let res = handle_create_footprint(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error);
        let c = std::fs::read_to_string(&out).unwrap();
        assert!(c.contains("F.CrtYd"), "missing courtyard:\n{c}");
        assert!(c.contains("F.SilkS"));
        assert!(c.contains("(fp_poly"), "missing fab chamfer outline");
        assert!(c.contains("(fp_circle"), "missing pin-1 silk dot");
        assert!(c.contains("(fp_text reference \"REF**\""));
        assert!(c.contains("(fp_text value \"TEST_QFN\""));
        assert!(c.contains("(model \"${KICAD9_3DMODEL_DIR}/Package.3dshapes/TEST_QFN.wrl\""));
        // Round-trips through the S-expression parser.
        assert!(
            konnect_sexp::parser::parse_sexp(&c).is_ok(),
            "generated footprint doesn't parse"
        );
    }

    #[test]
    fn create_footprint_schema_exposes_pad_layers_rotation_and_roundrect_ratio() {
        let tool = tools()
            .into_iter()
            .find(|tool| tool.name == "create_footprint")
            .expect("library must expose create_footprint");
        let pad_properties = &tool.input_schema["properties"]["pads"]["items"]["properties"];
        assert_eq!(pad_properties["layers"]["items"]["type"], json!("string"));
        assert_eq!(pad_properties["rotation"]["type"], json!("number"));
        assert_eq!(pad_properties["roundrect_rratio"]["maximum"], json!(0.5));
    }

    #[tokio::test]
    async fn create_footprint_emits_bottom_layer_rotated_roundrect_pad() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("BOTTOM.kicad_mod");
        let args = json!({
            "output": out.to_string_lossy(),
            "name": "BOTTOM",
            "pads": [{
                "number": "1",
                "type": "smd",
                "shape": "roundrect",
                "x": -8.075,
                "y": 4.7,
                "width": 2.5,
                "height": 2.55,
                "layers": ["B.Cu", "B.Paste", "B.Mask"],
                "rotation": 180.0,
                "roundrect_rratio": 0.2
            }]
        });

        let result = handle_create_footprint(&args, &test_ctx()).await.unwrap();
        assert!(!result.is_error);
        let content = std::fs::read_to_string(&out).unwrap();
        assert!(
            content.contains("(at -8.075 4.7 180)"),
            "missing pad rotation:\n{content}"
        );
        assert!(
            content.contains("(layers \"B.Cu\" \"B.Paste\" \"B.Mask\")"),
            "missing bottom pad layers:\n{content}"
        );
        assert!(
            content.contains("(roundrect_rratio 0.2)"),
            "missing roundrect ratio:\n{content}"
        );
    }

    #[tokio::test]
    async fn create_footprint_legacy_pad_payload_remains_front_side() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("LEGACY.kicad_mod");
        let args = json!({
            "output": out.to_string_lossy(),
            "name": "LEGACY",
            "pads": [{
                "number": "1",
                "type": "smd",
                "shape": "roundrect",
                "x": 0.0,
                "y": 0.0,
                "width": 1.0,
                "height": 1.0
            }]
        });

        let result = handle_create_footprint(&args, &test_ctx()).await.unwrap();
        assert!(!result.is_error);
        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.contains("(at 0 0)"), "{content}");
        assert!(
            content.contains("(layers \"F.Cu\" \"F.Paste\" \"F.Mask\")"),
            "{content}"
        );
        assert!(!content.contains("(at 0 0 0)"), "{content}");
    }

    #[tokio::test]
    async fn create_footprint_rejects_invalid_pad_layer_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("INVALID_LAYER.kicad_mod");
        let args = json!({
            "output": out.to_string_lossy(),
            "name": "INVALID_LAYER",
            "pads": [{
                "number": "1",
                "type": "smd",
                "shape": "rect",
                "x": 0.0,
                "y": 0.0,
                "width": 1.0,
                "height": 1.0,
                "layers": ["User.MadeUp"]
            }]
        });

        let error = handle_create_footprint(&args, &test_ctx())
            .await
            .expect_err("invalid layer must be rejected");
        assert!(error.to_string().contains("invalid pad layer"));
        assert!(!out.exists());
    }

    #[tokio::test]
    async fn create_footprint_rejects_invalid_roundrect_ratio_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("INVALID_RATIO.kicad_mod");
        let args = json!({
            "output": out.to_string_lossy(),
            "name": "INVALID_RATIO",
            "pads": [{
                "number": "1",
                "type": "smd",
                "shape": "roundrect",
                "x": 0.0,
                "y": 0.0,
                "width": 1.0,
                "height": 1.0,
                "roundrect_rratio": 0.75
            }]
        });

        let error = handle_create_footprint(&args, &test_ctx())
            .await
            .expect_err("invalid roundrect ratio must be rejected");
        assert!(error.to_string().contains("roundrect_rratio"));
        assert!(!out.exists());
    }

    #[tokio::test]
    async fn get_footprint_info_includes_graphics_only_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("inspect.kicad_mod");
        std::fs::write(
            &path,
            r#"(footprint "Inspect" (version 20240108) (generator "pcbnew")
  (layer "F.Cu")
  (descr "Inspection fixture")
  (fp_line (start 0 0) (end 1 0)
    (stroke (width 0.1) (type solid)) (layer "F.SilkS"))
  (fp_poly
    (pts (xy 0 0) (xy 2 0) (xy 2 1))
    (stroke (width 0.05) (type solid)) (fill none) (layer "B.CrtYd"))
  (pad "1" thru_hole circle (at 0 0) (size 1.8 1.8) (drill 1)
    (layers "*.Cu" "*.Mask"))
)
"#,
        )
        .unwrap();

        let default = handle_get_footprint_info(
            &json!({"footprint_path": path.to_string_lossy()}),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &default.content[0] else {
            panic!("expected text");
        };
        let default: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(default["name"], "Inspect");
        assert_eq!(default["pad_count"], 1);
        assert!(
            default.get("graphics").is_none(),
            "default response must stay compact: {default}"
        );
        assert!(default.get("graphic_count").is_none());

        let detailed = handle_get_footprint_info(
            &json!({
                "footprint_path": path.to_string_lossy(),
                "include_graphics": true,
                "graphics_layer": "B.CrtYd"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &detailed.content[0] else {
            panic!("expected text");
        };
        let detailed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(detailed["graphic_count"], 1);
        assert_eq!(detailed["graphics"][0]["type"], "poly");
        assert_eq!(detailed["graphics"][0]["layer"], "B.CrtYd");
        assert_eq!(detailed["graphics"][0]["point_count"], 3);
        assert_eq!(detailed["graphics"][0]["closed"], true);
        assert_eq!(detailed["graphics"][0]["stroke_width_mm"], 0.05);
        assert_eq!(detailed["graphics"][0]["fill"], "none");

        let invalid = handle_get_footprint_info(
            &json!({
                "footprint_path": path.to_string_lossy(),
                "graphics_layer": "BottomCourtyard"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(invalid.is_error);
        let crate::mcp::protocol::ToolContent::Text { text } = &invalid.content[0] else {
            panic!("expected text");
        };
        let invalid: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(invalid["error"]["kind"], "invalid_argument");
        assert_eq!(invalid["error"]["field"], "graphics_layer");

        let schema = tools()
            .into_iter()
            .find(|tool| tool.name == "get_footprint_info")
            .unwrap()
            .input_schema;
        assert_eq!(schema["properties"]["include_graphics"]["type"], "boolean");
        assert_eq!(schema["properties"]["graphics_layer"]["type"], "string");
    }

    #[tokio::test]
    async fn get_footprint_info_reads_kicad_style_layout_structurally() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("USB_C_Receptacle_HRO_TYPE-C-31-M-12.kicad_mod");
        // Reduced from KiCad 10's stock Connector_USB footprint without
        // rewriting the retained nodes. CRLF is intentional: together with
        // KiCad's tabs it reproduces both reported formatting assumptions.
        const KICAD_STYLE_FOOTPRINT: &str = concat!(
            "(footprint \"USB_C_Receptacle_HRO_TYPE-C-31-M-12\"\r\n",
            "\t(version 20260206)\r\n",
            "\t(generator \"pcbnew\")\r\n",
            "\t(generator_version \"10.0\")\r\n",
            "\t(layer \"F.Cu\")\r\n",
            "\t(descr \"USB Type-C receptacle for USB 2.0 and PD, http://www.krhro.com/uploads/soft/180320/1-1P320120243.pdf\")\r\n",
            "\t(tags \"usb usb-c 2.0 pd\")\r\n",
            "\t(fp_line\r\n",
            "\t\t(start -5.32 -5.27)\r\n",
            "\t\t(end -5.32 4.15)\r\n",
            "\t\t(stroke\r\n",
            "\t\t\t(width 0.05)\r\n",
            "\t\t\t(type solid)\r\n",
            "\t\t)\r\n",
            "\t\t(layer \"F.CrtYd\")\r\n",
            "\t\t(uuid \"d939342c-cab3-429e-ad71-738d34173267\")\r\n",
            "\t)\r\n",
            "\t(pad \"\" np_thru_hole circle\r\n",
            "\t\t(at -2.89 -2.6)\r\n",
            "\t\t(size 0.65 0.65)\r\n",
            "\t\t(drill 0.65)\r\n",
            "\t\t(layers \"*.Cu\" \"*.Mask\")\r\n",
            "\t\t(uuid \"e13b4b37-788a-41d5-87ca-c66be43ee32d\")\r\n",
            "\t)\r\n",
            "\t(pad \"\" np_thru_hole circle\r\n",
            "\t\t(at 2.89 -2.6)\r\n",
            "\t\t(size 0.65 0.65)\r\n",
            "\t\t(drill 0.65)\r\n",
            "\t\t(layers \"*.Cu\" \"*.Mask\")\r\n",
            "\t\t(uuid \"4558f1f0-2fa3-46e1-a220-284fa2707bb5\")\r\n",
            "\t)\r\n",
            "\t(pad \"A1\" smd roundrect\r\n",
            "\t\t(at -3.25 -4.045)\r\n",
            "\t\t(size 0.6 1.45)\r\n",
            "\t\t(layers \"F.Cu\" \"F.Mask\" \"F.Paste\")\r\n",
            "\t\t(roundrect_rratio 0.25)\r\n",
            "\t\t(uuid \"245fae56-8b58-4bd8-bbf1-36624dc3fa3e\")\r\n",
            "\t)\r\n",
            "\t(pad \"B12\" smd roundrect\r\n",
            "\t\t(at -3.25 -4.045)\r\n",
            "\t\t(size 0.6 1.45)\r\n",
            "\t\t(layers \"F.Cu\" \"F.Mask\" \"F.Paste\")\r\n",
            "\t\t(roundrect_rratio 0.25)\r\n",
            "\t\t(uuid \"eda87c04-3a59-4e2c-a9bd-f59ed31672aa\")\r\n",
            "\t)\r\n",
            "\t(pad \"SH\" thru_hole oval\r\n",
            "\t\t(at -4.32 -3.13)\r\n",
            "\t\t(size 1 2.1)\r\n",
            "\t\t(drill oval 0.6 1.7)\r\n",
            "\t\t(property pad_prop_mechanical)\r\n",
            "\t\t(layers \"*.Cu\" \"*.Mask\")\r\n",
            "\t\t(remove_unused_layers no)\r\n",
            "\t\t(uuid \"69c3dc88-a37e-49ef-ae4d-497d3191ad5b\")\r\n",
            "\t)\r\n",
            "\t(pad \"SH\" thru_hole oval\r\n",
            "\t\t(at -4.32 1.05)\r\n",
            "\t\t(size 1 1.6)\r\n",
            "\t\t(drill oval 0.6 1.2)\r\n",
            "\t\t(property pad_prop_mechanical)\r\n",
            "\t\t(layers \"*.Cu\" \"*.Mask\")\r\n",
            "\t\t(remove_unused_layers no)\r\n",
            "\t\t(uuid \"5991ca6f-c5c1-4692-8fe9-cf4b7ca50c4b\")\r\n",
            "\t)\r\n",
            "\t(embedded_fonts no)\r\n",
            "\t(model \"${KICAD10_3DMODEL_DIR}/Connector_USB.3dshapes/USB_C_Receptacle_HRO_TYPE-C-31-M-12.step\"\r\n",
            "\t\t(offset\r\n",
            "\t\t\t(xyz 0 0 0)\r\n",
            "\t\t)\r\n",
            "\t\t(scale\r\n",
            "\t\t\t(xyz 1 1 1)\r\n",
            "\t\t)\r\n",
            "\t\t(rotate\r\n",
            "\t\t\t(xyz 0 0 0)\r\n",
            "\t\t)\r\n",
            "\t)\r\n",
            ")\r\n",
        );
        std::fs::write(&path, KICAD_STYLE_FOOTPRINT).unwrap();

        let result = handle_get_footprint_info(
            &json!({"footprint_path": path.to_string_lossy()}),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text");
        };
        let result: serde_json::Value = serde_json::from_str(text).unwrap();

        assert_eq!(result["name"], "USB_C_Receptacle_HRO_TYPE-C-31-M-12");
        assert_eq!(result["pad_count"], 6);
        assert_eq!(result["has_courtyard"], true);
        assert_eq!(result["has_3d_model"], true);
    }

    #[tokio::test]
    async fn get_footprint_info_ignores_metadata_that_looks_like_structure() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metadata-only.kicad_mod");
        std::fs::write(
            &path,
            concat!(
                "(footprint \"MetadataOnly\"\r\n",
                "\t(version 20240108)\r\n",
                "\t(generator \"pcbnew\")\r\n",
                "\t(layer \"F.Cu\")\r\n",
                "\t(descr \"mentions (pad fake), F.CrtYd, and (model fake)\")\r\n",
                ")\r\n",
            ),
        )
        .unwrap();

        let result = handle_get_footprint_info(
            &json!({"footprint_path": path.to_string_lossy()}),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text");
        };
        let result: serde_json::Value = serde_json::from_str(text).unwrap();

        assert_eq!(result["pad_count"], 0);
        assert_eq!(result["has_courtyard"], false);
        assert_eq!(result["has_3d_model"], false);
    }

    #[tokio::test]
    async fn create_symbol_emits_body_and_shows_pins() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("test.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "TEST_IC",
            "reference_prefix": "U",
            "pins": [
                {"number":"1","name":"IN","type":"input","x":-7.62,"y":2.54,"angle":0,"length":2.54},
                {"number":"2","name":"GND","type":"power_in","x":-7.62,"y":-2.54,"angle":0,"length":2.54},
                {"number":"3","name":"OUT","type":"output","x":7.62,"y":0.0,"angle":180,"length":2.54}
            ]
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error);
        let c = std::fs::read_to_string(&lib).unwrap();
        assert!(
            c.contains("(rectangle"),
            "missing symbol body rectangle:\n{c}"
        );
        assert!(
            c.contains("(generator \"konnect\")"),
            "stale generator string"
        );
        assert!(
            !c.contains("(pin_numbers hide)"),
            "KiCad 10 shows pin numbers by omitting the hide override"
        );
        assert!(
            konnect_sexp::parser::parse_sexp(&c).is_ok(),
            "generated symbol doesn't parse"
        );
    }

    #[tokio::test]
    async fn create_symbol_emits_kicad10_library_match_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("modern.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "Modern",
            "reference_prefix": "U",
            "pins": [
                {"number":"1","name":"IN","type":"input","x":-7.62,"y":0.0,"angle":0}
            ]
        });

        let result = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(!result.is_error);
        let output = std::fs::read_to_string(&lib).unwrap();

        assert!(output.contains("(version 20251024)"), "{output}");
        assert!(output.contains("(generator_version \"10.0\")"), "{output}");
        assert!(output.contains("(exclude_from_sim no)"), "{output}");
        assert!(output.contains("(in_pos_files yes)"), "{output}");
        assert!(
            output.contains("(duplicate_pin_numbers_are_jumpers no)"),
            "{output}"
        );
        assert!(
            output.contains("(property \"Description\" \"\""),
            "{output}"
        );
        assert!(output.contains("(show_name no)"), "{output}");
        assert!(output.contains("(do_not_autoplace no)"), "{output}");
        assert!(output.contains("(hide yes)"), "{output}");
        assert!(output.contains("(embedded_fonts no)"), "{output}");
    }

    #[tokio::test]
    async fn create_symbol_writes_datasheet_and_normalizes_the_placeholder() {
        // `~` must never reach the file: KiCad's library loader normalises it to
        // "" and its lib_symbols loader does not, so a symbol carrying it fails
        // ERC's library-match check for as long as it exists.
        let tmp = tempfile::tempdir().unwrap();
        let pins = json!([{"number":"1","name":"IN","type":"input","x":-7.62,"y":0.0,"angle":0}]);

        for (i, (given, written)) in [
            (
                json!("https://example.com/ds.pdf"),
                "https://example.com/ds.pdf",
            ),
            (json!("~"), ""),
            (serde_json::Value::Null, ""),
        ]
        .into_iter()
        .enumerate()
        {
            let lib = tmp.path().join(format!("ds{i}.kicad_sym"));
            let mut args = json!({
                "library_path": lib.to_string_lossy(),
                "name": "DS",
                "reference_prefix": "U",
                "pins": pins,
            });
            if !given.is_null() {
                args["datasheet"] = given;
            }

            let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
            assert!(!res.is_error);
            let output = std::fs::read_to_string(&lib).unwrap();
            assert!(
                output.contains(&format!("(property \"Datasheet\" \"{written}\"")),
                "{output}"
            );
        }
    }

    #[tokio::test]
    async fn create_symbol_single_unit_uses_unit_0_only() {
        // Regression: without `units`, a symbol is one sub-symbol NAME_0_1 and
        // creates no NAME_1_1 unit (unchanged from before multi-unit support).
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("s.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "SINGLE",
            "reference_prefix": "U",
            "pins": [{"number":"1","name":"A","type":"passive","x":-5.08,"y":0.0,"angle":0,"length":2.54}]
        });
        handle_create_symbol(&args, &test_ctx()).await.unwrap();
        let c = std::fs::read_to_string(&lib).unwrap();
        assert!(
            c.contains("(symbol \"SINGLE_0_1\""),
            "single unit lives in _0_1:\n{c}"
        );
        assert!(
            !c.contains("SINGLE_1_1"),
            "single unit must not create a _1_1 unit"
        );
    }

    #[tokio::test]
    async fn list_symbols_parses_kicad10_crlf_tab_format() {
        // Regression: konnect 0.2.0 hard-coded the needle `\n  (symbol "` (LF +
        // exactly 2 spaces) and so returned 0 symbols for every real KiCad
        // library. On disk those files are CRLF-terminated and TAB-indented
        // (KiCad 10, format version 20251024), so the needle never matched.
        // Build a fixture in that exact on-disk shape and confirm we now find
        // the top-level symbols and skip the nested `_N_M` sub-units.
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("kicad10.kicad_sym");
        let unit = |name: &str| {
            format!("\t(symbol \"{name}\"\r\n\t\t(symbol \"{name}_0_1\"\r\n\t\t)\r\n\t)\r\n")
        };
        let content = format!(
            "(kicad_symbol_lib\r\n\t(version 20251024)\r\n\t(generator \"kicad_symbol_editor\")\r\n{}{})\r\n",
            unit("R_ohm"),
            unit("LED"),
        );
        // Sanity: the fixture really is CRLF + TAB and lacks the old needle.
        assert!(content.contains("\r\n"));
        assert!(
            !content.contains("\n  (symbol \""),
            "fixture must not contain the old LF/2-space needle"
        );
        std::fs::write(&lib, content).unwrap();

        let args = json!({ "library_path": lib.to_string_lossy() });
        let res = handle_list_symbols_in_library(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);
        let text = match res.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        let out: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            out["count"], 2,
            "expected 2 top-level symbols (R_ohm, LED), got: {text}"
        );
        let names: Vec<String> = serde_json::from_value(out["symbols"].clone()).unwrap();
        assert!(names.contains(&"R_ohm".to_string()), "names={names:?}");
        assert!(names.contains(&"LED".to_string()), "names={names:?}");
        assert!(
            !names.iter().any(|n| n.ends_with("_0_1")),
            "sub-units must not leak into the listing: {names:?}"
        );
    }

    fn result_text(res: &CallToolResult) -> String {
        match res.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// Build a temp "project dir" containing a `sym-lib-table` that references a
    /// single `.kicad_sym` library, returning the project dir path. The URI is
    /// absolute (not `${KICAD_*}`) so it resolves without KiCad env vars.
    fn write_project_sym_lib(tmp: &tempfile::TempDir, nick: &str, lib_body: &str) -> PathBuf {
        let lib_file = tmp.path().join(format!("{nick}.kicad_sym"));
        std::fs::write(&lib_file, lib_body).unwrap();
        let uri = lib_file.to_string_lossy().replace('\\', "/");
        let table = format!(
            "(sym_lib_table\n  (lib (name \"{nick}\") (type \"Normal\") (uri \"{uri}\") (options \"\") (descr \"\"))\n)\n",
        );
        std::fs::write(tmp.path().join("sym-lib-table"), table).unwrap();
        tmp.path().to_path_buf()
    }

    #[tokio::test]
    async fn get_symbol_info_parses_kicad10_pins_and_props() {
        // Regression: get_symbol_info hard-coded `  (symbol "NAME"` / `\n    (pin `
        // string searches and only consulted the GLOBAL table, so it returned
        // "not found" for every real KiCad 10 symbol (CRLF + TAB files) and could
        // never resolve project libraries. Fixture is a KiCad-10-shaped (CRLF +
        // TAB) library resolved via a project sym-lib-table; we expect pins +
        // properties read from the tree, with the nested _1_1 unit's pins
        // collected recursively.
        let tmp = tempfile::tempdir().unwrap();
        let body = concat!(
            "(kicad_symbol_lib\r\n",
            "\t(version 20251024)\r\n",
            "\t(generator \"kicad_symbol_editor\")\r\n",
            "\t(symbol \"T1\"\r\n",
            "\t\t(property \"Reference\" \"Q\" (at 0 5.08 0))\r\n",
            "\t\t(property \"Value\" \"T1\" (at 0 -5.08 0))\r\n",
            "\t\t(symbol \"T1_1_1\"\r\n",
            "\t\t\t(pin input line (at -5.08 2.54 0) (length 2.54) (name \"G\") (number \"1\"))\r\n",
            "\t\t\t(pin output line (at 5.08 0 180) (length 2.54) (name \"S\") (number \"3\"))\r\n",
            "\t\t)\r\n",
            "\t)\r\n",
            ")\r\n",
        );
        let proj = write_project_sym_lib(&tmp, "testlib", body);

        let args = json!({
            "lib_id": "testlib:T1",
            "project_dir": proj.to_string_lossy(),
        });
        let res = handle_get_symbol_info(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);
        let out: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();
        assert_eq!(out["pin_count"], 2, "full result: {out}");
        let numbers: Vec<&str> = out["pins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["number"].as_str().unwrap_or(""))
            .collect();
        assert!(numbers.contains(&"1"), "pins: {out}");
        assert!(numbers.contains(&"3"), "pins: {out}");
        let g_pin = out["pins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["number"] == "1")
            .unwrap();
        assert_eq!(g_pin["type"], "input", "{g_pin}");
        assert_eq!(g_pin["name"], "G", "{g_pin}");
        assert_eq!(out["properties"]["Reference"], "Q", "{out}");
        assert_eq!(out["properties"]["Value"], "T1", "{out}");
    }

    const EXTENDS_DERIVED_LIB: &str = "\
(kicad_symbol_lib
  (version 20251024)
  (symbol \"Base\"
    (symbol \"Base_1_1\"
      (pin input line (at -5.08 2.54 0) (length 2.54) (name \"G\") (number \"1\"))
      (pin output line (at 5.08 0 180) (length 2.54) (name \"S\") (number \"3\"))
    )
  )
  (symbol \"Derived\"
    (extends \"Base\")
    (property \"Reference\" \"U\" (at 0 5.08 0))
    (property \"Value\" \"Derived\" (at 0 -5.08 0))
  )
)
";

    #[test]
    fn resolve_symbol_pins_inherits_from_base() {
        let root = parse_sexp(EXTENDS_DERIVED_LIB).unwrap();
        let derived = root
            .find_all("symbol")
            .into_iter()
            .find(|s| s.get(1).and_then(|n| n.as_str()) == Some("Derived"))
            .unwrap();
        let pins = resolve_symbol_pins(&root, derived);
        let numbers: Vec<&str> = pins
            .iter()
            .map(|p| p.find_str("number").unwrap_or(""))
            .collect();
        assert_eq!(
            pins.len(),
            2,
            "derived symbol should inherit base pins: {numbers:?}"
        );
        assert!(numbers.contains(&"1"), "{numbers:?}");
        assert!(numbers.contains(&"3"), "{numbers:?}");
    }

    #[tokio::test]
    async fn get_symbol_info_resolves_extends_pins() {
        // Derived symbol (extends Base) has no own pins; get_symbol_info must
        // follow the extends chain and report the base's pins.
        let tmp = tempfile::tempdir().unwrap();
        let proj = write_project_sym_lib(&tmp, "testlib", EXTENDS_DERIVED_LIB);
        let args = json!({
            "lib_id": "testlib:Derived",
            "project_dir": proj.to_string_lossy(),
        });
        let res = handle_get_symbol_info(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "handler errored: {:?}", res.content);
        let out: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();
        assert_eq!(
            out["pin_count"], 2,
            "derived symbol should inherit 2 base pins: {out}"
        );
        let numbers: Vec<&str> = out["pins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["number"].as_str().unwrap_or(""))
            .collect();
        assert!(numbers.contains(&"1"), "pins: {out}");
        assert!(numbers.contains(&"3"), "pins: {out}");
        assert_eq!(out["properties"]["Reference"], "U", "{out}");
    }

    #[test]
    fn resolve_symbol_pins_follows_multilevel_chain() {
        let src = "\
(kicad_symbol_lib
  (symbol \"C\"
    (symbol \"C_1_1\"
      (pin passive line (at 0 5.08 0) (length 2.54) (name \"C1\") (number \"1\"))
    )
  )
  (symbol \"B\" (extends \"C\"))
  (symbol \"A\" (extends \"B\"))
)";
        let root = parse_sexp(src).unwrap();
        let a = root
            .find_all("symbol")
            .into_iter()
            .find(|s| s.get(1).and_then(|n| n.as_str()) == Some("A"))
            .unwrap();
        let pins = resolve_symbol_pins(&root, a);
        let numbers: Vec<&str> = pins
            .iter()
            .map(|p| p.find_str("number").unwrap_or(""))
            .collect();
        assert_eq!(numbers, vec!["1"], "A→B→C should resolve to C's pin");
    }

    #[test]
    fn resolve_symbol_pins_handles_cycle() {
        let src = "\
(kicad_symbol_lib
  (symbol \"A\"
    (extends \"B\")
    (symbol \"A_1_1\"
      (pin passive line (at 0 5.08 0) (length 2.54) (name \"A1\") (number \"1\"))
    )
  )
  (symbol \"B\"
    (extends \"A\")
    (symbol \"B_1_1\"
      (pin passive line (at 0 -5.08 0) (length 2.54) (name \"B2\") (number \"2\"))
    )
  )
)";
        let root = parse_sexp(src).unwrap();
        let a = root
            .find_all("symbol")
            .into_iter()
            .find(|s| s.get(1).and_then(|n| n.as_str()) == Some("A"))
            .unwrap();
        let pins = resolve_symbol_pins(&root, a);
        let numbers: Vec<&str> = pins
            .iter()
            .map(|p| p.find_str("number").unwrap_or(""))
            .collect();
        // Terminates (no hang); collects A's pin "1" then B's pin "2".
        assert!(numbers.contains(&"1"), "{numbers:?}");
        assert!(numbers.contains(&"2"), "{numbers:?}");
    }

    #[test]
    fn resolve_symbol_pins_missing_base_falls_back() {
        let src = "\
(kicad_symbol_lib
  (symbol \"Orphan\"
    (extends \"NoSuch\")
    (symbol \"Orphan_1_1\"
      (pin passive line (at 0 5.08 0) (length 2.54) (name \"P\") (number \"7\"))
    )
  )
)";
        let root = parse_sexp(src).unwrap();
        let orphan = root
            .find_all("symbol")
            .into_iter()
            .find(|s| s.get(1).and_then(|n| n.as_str()) == Some("Orphan"))
            .unwrap();
        let pins = resolve_symbol_pins(&root, orphan);
        let numbers: Vec<&str> = pins
            .iter()
            .map(|p| p.find_str("number").unwrap_or(""))
            .collect();
        // Missing base: walk stops, returns Orphan's own pin (no panic).
        assert_eq!(numbers, vec!["7"]);
    }

    #[test]
    fn resolve_symbol_pins_derived_shadows_base() {
        let src = "\
(kicad_symbol_lib
  (symbol \"Base\"
    (symbol \"Base_1_1\"
      (pin input line (at 0 5.08 0) (length 2.54) (name \"BASE_G\") (number \"1\"))
    )
  )
  (symbol \"Derived\"
    (extends \"Base\")
    (symbol \"Derived_1_1\"
      (pin output line (at 0 -5.08 0) (length 2.54) (name \"DERIVED_G\") (number \"1\"))
    )
  )
)";
        let root = parse_sexp(src).unwrap();
        let derived = root
            .find_all("symbol")
            .into_iter()
            .find(|s| s.get(1).and_then(|n| n.as_str()) == Some("Derived"))
            .unwrap();
        let pins = resolve_symbol_pins(&root, derived);
        // Derived's own pin "1" shadows base's pin "1": one pin, derived's name.
        assert_eq!(pins.len(), 1, "{pins:?}");
        assert_eq!(pins[0].find_str("name"), Some("DERIVED_G"));
        assert_eq!(pins[0].find_str("number"), Some("1"));
    }

    #[tokio::test]
    async fn search_lib_symbols_matches_underscore_names_and_skips_units() {
        // Pure check of the per-library matcher factored out of search_symbols:
        // top-level symbols with underscores must be returned verbatim, and the
        // nested _0_1 unit sub-symbols must not leak into results.
        let body = concat!(
            "(kicad_symbol_lib\r\n\t(version 20251024)\r\n",
            "\t(symbol \"FOO_BAR\"\r\n\t\t(symbol \"FOO_BAR_0_1\")\r\n\t)\r\n",
            "\t(symbol \"LED\"\r\n\t\t(symbol \"LED_0_1\")\r\n\t)\r\n",
            ")\r\n",
        );
        let results = search_lib_symbols("projlib", body, "foo");
        let names: Vec<&str> = results
            .iter()
            .map(|r| r["name"].as_str().unwrap_or(""))
            .collect();
        assert!(names.contains(&"FOO_BAR"), "names={names:?}");
        assert_eq!(results[0]["library"], "projlib");
        assert_eq!(results[0]["id"], "projlib:FOO_BAR");
        assert!(
            !names.iter().any(|n| n.ends_with("_0_1")),
            "sub-units leaked: {names:?}"
        );
    }

    /// A 14-pin module with pins declared on two facing columns, as the report
    /// describes it. `names` decides how wide the body has to be.
    async fn module_response(names: &[&str], x: f64) -> serde_json::Value {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("mod.kicad_sym");
        let pins: Vec<serde_json::Value> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                // Facing columns: each row carries a left and a right pin, which
                // is what makes two long names run into each other across the
                // body and forces it wider.
                let left = i % 2 == 0;
                json!({
                    "number": (i + 1).to_string(),
                    "name": name,
                    "type": "bidirectional",
                    "x": if left { -x } else { x },
                    "y": (i / 2) as f64 * 2.54,
                    "angle": if left { 0 } else { 180 },
                    "length": 2.54
                })
            })
            .collect();
        let res = handle_create_symbol(
            &json!({
                "library_path": lib.to_string_lossy(),
                "name": "MODULE",
                "reference_prefix": "U",
                "pins": pins
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{:?}", res.content);
        serde_json::from_str(&result_text(&res)).unwrap()
    }

    fn pin_x(report: &serde_json::Value, number: &str) -> f64 {
        report["units"][0]["pins"]
            .as_array()
            .expect("units[0].pins is an array")
            .iter()
            .find(|p| p["number"] == number)
            .expect("pin present")["x"]
            .as_f64()
            .expect("x is a number")
    }

    /// The reported case: long pin names widen the body, the pins slide out to
    /// meet it, and the response now says where they went.
    #[tokio::test]
    async fn long_pin_names_move_the_pins_and_the_response_reports_it() {
        let long = ["D4/A4/SDA/P0.04"; 14];
        let report = module_response(&long, 19.05).await;

        let first = &report["units"][0]["pins"][0];
        assert_eq!(report["units"][0]["body"], "rectangle");
        assert!(
            first["requested"].is_object(),
            "a moved pin carries the position that was asked for: {first}"
        );
        assert_eq!(first["requested"]["x"], json!(-19.05));
        assert!(
            pin_x(&report, "1") < -19.05,
            "the pin must sit further out than requested, got {}",
            pin_x(&report, "1")
        );

        let warnings = report["warnings"]
            .as_array()
            .expect("a move is worth a warning");
        assert_eq!(warnings.len(), 1, "one summary per unit, not one per pin");
        let text = warnings[0].as_str().unwrap();
        assert!(text.contains("14 of 14 pins"), "{text}");
        assert!(text.contains("unit 1"), "{text}");
    }

    /// The other row of the report's table: short names need no extra width, so
    /// the pins stay where they were put and nothing is warned about.
    #[tokio::test]
    async fn short_pin_names_leave_the_pins_alone() {
        let short = ["D4/SDA"; 14];
        let report = module_response(&short, 19.05).await;

        assert_eq!(pin_x(&report, "1"), -19.05);
        assert!(
            report["units"][0]["pins"][0]["requested"].is_null(),
            "an unmoved pin needs no `requested`: {}",
            report["units"][0]["pins"][0]
        );
        assert!(
            report["warnings"].is_null(),
            "nothing moved, so nothing to warn about: {}",
            report["warnings"]
        );
    }

    /// Two symbols with identical pin coordinates but different name lengths
    /// come out with different connection points. That is the surprise the
    /// report is about, and the response is what makes it visible.
    #[tokio::test]
    async fn identical_coordinates_can_still_resolve_differently() {
        let long = module_response(&["D4/A4/SDA/P0.04"; 14], 19.05).await;
        let short = module_response(&["D4/SDA"; 14], 19.05).await;

        assert_ne!(
            pin_x(&long, "1"),
            pin_x(&short, "1"),
            "same request, different result — the caller has to be told"
        );
    }

    /// Every pin on an edge is aligned to it, names or no names. A ragged
    /// column is squared up even with `show_pin_names` off, so the response has
    /// to report that too.
    #[tokio::test]
    async fn pins_are_aligned_to_the_body_edge_even_without_names() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("ragged.kicad_sym");
        let res = handle_create_symbol(
            &json!({
                "library_path": lib.to_string_lossy(),
                "name": "RAGGED",
                "reference_prefix": "U",
                "show_pin_names": false,
                "pins": [
                    {"number":"1","name":"A","type":"input","x":-7.62,"y":0.0,"angle":0,"length":2.54},
                    {"number":"2","name":"B","type":"input","x":-12.7,"y":2.54,"angle":0,"length":2.54}
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{:?}", res.content);
        let report: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();

        assert_eq!(
            pin_x(&report, "1"),
            pin_x(&report, "2"),
            "both pins end up on the same edge"
        );
        let moved = report["units"][0]["pins"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["requested"].is_object())
            .count();
        assert_eq!(moved, 1, "the pin that was pulled out is marked: {report}");
    }

    /// A glyph places its pins by type — documented, intended, and reported
    /// without a warning, or every correct call would carry one.
    #[tokio::test]
    async fn a_glyph_unit_reports_its_placement_without_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("gate.kicad_sym");
        let res = handle_create_symbol(
            &json!({
                "library_path": lib.to_string_lossy(),
                "name": "GATE",
                "reference_prefix": "U",
                "glyph": "inverter",
                "pins": [
                    {"number":"1","name":"A","type":"input","x":100.0,"y":100.0},
                    {"number":"2","name":"Y","type":"output","x":200.0,"y":200.0}
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{:?}", res.content);
        let report: serde_json::Value = serde_json::from_str(&result_text(&res)).unwrap();

        assert_eq!(report["units"][0]["body"], "inverter");
        assert_ne!(
            pin_x(&report, "1"),
            100.0,
            "a glyph ignores the requested position"
        );
        assert!(
            report["warnings"].is_null(),
            "a glyph move is intended, not a surprise: {}",
            report["warnings"]
        );
    }

    #[tokio::test]
    async fn create_symbol_accepts_all_12_kicad_pin_types() {
        // One pin per valid electrical type; the generated library must carry
        // each type verbatim and still parse (#55).
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("types.kicad_sym");
        let pins: Vec<serde_json::Value> = ALLOWED_PIN_ELECTRICAL_TYPES
            .iter()
            .enumerate()
            .map(|(i, t)| {
                json!({
                    "number": (i + 1).to_string(),
                    "name": format!("P{}", i + 1),
                    "type": t,
                    "x": -7.62, "y": (i as f64) * 2.54, "angle": 0, "length": 2.54
                })
            })
            .collect();
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "ALL_TYPES",
            "reference_prefix": "U",
            "pins": pins
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(
            !res.is_error,
            "all valid types must pass: {:?}",
            res.content
        );
        let c = std::fs::read_to_string(&lib).unwrap();
        for t in ALLOWED_PIN_ELECTRICAL_TYPES {
            assert!(
                c.contains(&format!("(pin {} line", t)),
                "missing pin type {t}:\n{c}"
            );
        }
        assert!(
            konnect_sexp::parser::parse_sexp(&c).is_ok(),
            "generated symbol doesn't parse"
        );
    }

    #[tokio::test]
    async fn create_symbol_rejects_not_connected_with_suggestion() {
        // KiCAD's enum is `no_connect`; `not_connected` used to be interpolated
        // verbatim, producing a library eeschema refuses to load (#55).
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("nc.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "BAD_NC",
            "reference_prefix": "U",
            "pins": [
                {"number":"1","name":"NC","type":"not_connected","x":-5.08,"y":0.0}
            ]
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(res.is_error, "not_connected must be rejected");
        let text = result_text(&res);
        assert!(
            text.contains("not_connected"),
            "error must name the invalid token: {text}"
        );
        assert!(
            text.contains("no_connect"),
            "error must suggest the valid spelling: {text}"
        );
        assert!(
            !lib.exists(),
            "nothing may be written when validation fails"
        );
    }

    #[tokio::test]
    async fn create_symbol_rejects_dual_electrical_type() {
        // "output bidirectional" is two types in one string — KiCAD expects
        // exactly one (#55, bug 2).
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("dual_type.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "BAD_DUAL",
            "reference_prefix": "U",
            "pins": [
                {"number":"1","name":"IO","type":"output bidirectional","x":-5.08,"y":0.0}
            ]
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(res.is_error, "dual electrical type must be rejected");
        let text = result_text(&res);
        assert!(
            text.contains("output bidirectional"),
            "error must name the invalid token: {text}"
        );
        assert!(!lib.exists(), "nothing may be written on failure");
    }

    #[tokio::test]
    async fn create_symbol_invalid_type_in_multi_unit_writes_nothing() {
        // The multi-unit and power-pin paths validate too, and an existing
        // library file must be left untouched on failure.
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("existing.kicad_sym");
        let before = "(kicad_symbol_lib\n  (version 20240108)\n  (generator \"konnect\")\n)\n";
        std::fs::write(&lib, before).unwrap();
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "BAD_MULTI",
            "reference_prefix": "U",
            "units": [
                { "pins": [{"number":"1","name":"A","type":"input","x":-5.08,"y":0.0}] },
                { "pins": [{"number":"2","name":"B","type":"totem_pole","x":-5.08,"y":0.0}] }
            ],
            "power_pins": [
                {"number":"3","name":"VCC","type":"power_in","x":0.0,"y":5.08,"angle":270}
            ]
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(res.is_error, "invalid type in unit 2 must be rejected");
        assert!(result_text(&res).contains("totem_pole"));
        assert_eq!(
            std::fs::read_to_string(&lib).unwrap(),
            before,
            "existing library must be untouched on failure"
        );
    }

    #[tokio::test]
    async fn create_symbol_multi_unit_emits_units_and_common() {
        // A dual op-amp: two signal units + power pins as a dedicated 3rd unit.
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("dual.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "DUAL_OPAMP",
            "reference_prefix": "U",
            "value": "DUAL_OPAMP",
            "units": [
                { "pins": [
                    {"number":"3","name":"+","type":"input","x":-10.16,"y":2.54,"angle":0,"length":2.54},
                    {"number":"2","name":"-","type":"input","x":-10.16,"y":-2.54,"angle":0,"length":2.54},
                    {"number":"1","name":"~","type":"output","x":10.16,"y":0.0,"angle":180,"length":2.54}
                ]},
                { "pins": [
                    {"number":"5","name":"+","type":"input","x":-10.16,"y":2.54,"angle":0,"length":2.54},
                    {"number":"6","name":"-","type":"input","x":-10.16,"y":-2.54,"angle":0,"length":2.54},
                    {"number":"7","name":"~","type":"output","x":10.16,"y":0.0,"angle":180,"length":2.54}
                ]}
            ],
            "power_pins": [
                {"number":"8","name":"V+","type":"power_in","x":0.0,"y":7.62,"angle":270,"length":2.54},
                {"number":"4","name":"V-","type":"power_in","x":0.0,"y":-7.62,"angle":90,"length":2.54}
            ]
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error);
        let c = std::fs::read_to_string(&lib).unwrap();
        // Two signal units + a dedicated power unit (unit 3). No common _0_1,
        // and the power pins are NOT drawn on every unit.
        assert!(
            !c.contains("DUAL_OPAMP_0_1"),
            "multi-unit must not use a common _0_1:\n{c}"
        );
        assert!(
            c.contains("(symbol \"DUAL_OPAMP_1_1\""),
            "missing signal unit 1"
        );
        assert!(
            c.contains("(symbol \"DUAL_OPAMP_2_1\""),
            "missing signal unit 2"
        );
        assert!(
            c.contains("(symbol \"DUAL_OPAMP_3_1\""),
            "missing dedicated power unit 3"
        );
        assert!(
            !c.contains("DUAL_OPAMP_4_1"),
            "should be exactly three units"
        );
        // The power pins appear once (in the power unit), not per signal unit.
        assert_eq!(
            c.matches("\"V+\"").count(),
            1,
            "V+ must appear exactly once"
        );
        assert_eq!(
            c.matches("\"V-\"").count(),
            1,
            "V- must appear exactly once"
        );
        // A body rectangle per unit (2 signal + 1 power).
        assert_eq!(c.matches("(rectangle").count(), 3, "one body per unit");
        assert!(
            konnect_sexp::parser::parse_sexp(&c).is_ok(),
            "multi-unit symbol doesn't parse"
        );
    }

    async fn make_symbol(glyph: &str, pins: serde_json::Value) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("g.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "G",
            "reference_prefix": "U",
            "glyph": glyph,
            "pins": pins,
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error, "{glyph} create_symbol errored");
        let c = std::fs::read_to_string(&lib).unwrap();
        assert!(
            konnect_sexp::parser::parse_sexp(&c).is_ok(),
            "{glyph} output doesn't parse:\n{c}"
        );
        c
    }

    #[tokio::test]
    async fn glyph_opamp_draws_triangle_and_orders_inputs_top_to_bottom() {
        // Inputs are placed in the order listed, top first. Passing + then -
        // gives KiCAD's convention (+ on top, - on bottom).
        let c = make_symbol(
            "opamp",
            json!([
                {"number":"3","name":"+","type":"input"},
                {"number":"2","name":"-","type":"input"},
                {"number":"1","name":"OUT","type":"output"}
            ]),
        )
        .await;
        assert!(c.contains("(polyline"), "op-amp draws a triangle:\n{c}");
        assert!(
            !c.contains("(rectangle"),
            "op-amp must not draw a rectangle"
        );
        // Caller x/y are ignored; pins land on the fixed anchors.
        let top = c.find("(at -7.62 2.54 0)").expect("top input anchor");
        let bot = c.find("(at -7.62 -2.54 0)").expect("bottom input anchor");
        assert!(top < bot, "first-listed input (+) is emitted on top");
        // Non-inverting output at the apex.
        assert!(
            c.contains("(pin output line (at 7.62 0 180)"),
            "op-amp output is a plain line at the apex:\n{c}"
        );
    }

    #[tokio::test]
    async fn glyph_opamp_with_power_splits_into_a_rect_power_unit() {
        // A single op-amp carrying its own supply: the triangle has no room for
        // power-pin names, so V+/V- go to a dedicated rectangular power unit
        // (unit 2), like KiCAD's multi-unit op-amps.
        let c = make_symbol(
            "opamp",
            json!([
                {"number":"3","name":"+","type":"input"},
                {"number":"2","name":"-","type":"input"},
                {"number":"6","name":"OUT","type":"output"},
                {"number":"7","name":"V+","type":"power_in"},
                {"number":"4","name":"V-","type":"power_in"}
            ]),
        )
        .await;
        // Two units: G_1_1 (triangle) + G_2_1 (rect power). No single _0_1.
        assert!(
            !c.contains("G_0_1"),
            "a split symbol must not use _0_1:\n{c}"
        );
        assert!(c.contains("(symbol \"G_1_1\""), "signal triangle is unit 1");
        assert!(
            c.contains("(symbol \"G_2_1\""),
            "power is a separate unit 2"
        );
        // Exactly one triangle (the op-amp) and one rectangle (the power unit).
        assert_eq!(c.matches("(polyline").count(), 1, "one triangle body:\n{c}");
        assert_eq!(
            c.matches("(rectangle").count(),
            1,
            "one rectangular power unit"
        );
        // The supply pins appear once, and on the power unit at full-size text.
        assert_eq!(c.matches("\"V+\"").count(), 1, "V+ appears exactly once");
        assert_eq!(c.matches("\"V-\"").count(), 1, "V- appears exactly once");
        assert!(
            c.contains("(name \"V+\" (effects (font (size 1.27 1.27))))"),
            "power-unit names use the full 1.27 font (it's a rectangle):\n{c}"
        );
        // The triangle keeps its signal pins at the compact glyph font.
        assert!(c.contains("(name \"+\" (effects (font (size 0.762 0.762))))"));
        assert!(c.contains("(pin output line (at 7.62 0 180)"));
    }

    #[tokio::test]
    async fn glyph_and_nand_share_body_and_differ_by_output_bubble() {
        let pins = json!([
            {"number":"1","name":"A","type":"input"},
            {"number":"2","name":"B","type":"input"},
            {"number":"3","name":"Y","type":"output"}
        ]);
        let and = make_symbol("and", pins.clone()).await;
        let nand = make_symbol("nand", pins).await;
        // Same AND body (an arc), no rectangle.
        for (g, c) in [("and", &and), ("nand", &nand)] {
            assert!(c.contains("(arc"), "{g} has the AND arc:\n{c}");
            assert!(!c.contains("(rectangle"), "{g} must not draw a rectangle");
        }
        // The only difference is the output pin: AND plain, NAND inverted bubble.
        assert!(
            and.contains("(pin output line (at 7.62 0 180)"),
            "AND output line"
        );
        assert!(
            nand.contains("(pin output inverted (at 7.62 0 180)"),
            "NAND output carries the bubble via an inverted pin:\n{nand}"
        );
        assert!(!nand.contains("(pin output line (at 7.62 0 180)"));
    }

    #[tokio::test]
    async fn glyph_buffer_and_inverter_share_triangle() {
        let pins = json!([
            {"number":"1","name":"A","type":"input"},
            {"number":"2","name":"Y","type":"output"}
        ]);
        let buffer = make_symbol("buffer", pins.clone()).await;
        let inverter = make_symbol("inverter", pins).await;
        // Single input centered on the left, plain vs inverted output.
        assert!(
            buffer.contains("(pin input line (at -7.62 0 0)"),
            "buffer input centered"
        );
        assert!(
            buffer.contains("(pin output line (at 7.62 0 180)"),
            "buffer output line"
        );
        assert!(
            inverter.contains("(pin output inverted (at 7.62 0 180)"),
            "inverter output inverted:\n{inverter}"
        );
    }

    #[tokio::test]
    async fn glyph_schmitt_has_hysteresis_mark_and_optional_bubble() {
        let pins = json!([
            {"number":"1","name":"A","type":"input"},
            {"number":"2","name":"Y","type":"output"}
        ]);
        let schmitt = make_symbol("schmitt", pins.clone()).await;
        let schmitt_inv = make_symbol("schmitt_inverter", pins).await;
        // The hysteresis mark (from KiCAD's 74HC14) is present on both.
        for (g, c) in [("schmitt", &schmitt), ("schmitt_inverter", &schmitt_inv)] {
            assert!(
                c.contains("(xy -1.905 -1.27)") && c.contains("(xy -1.905 1.27)"),
                "{g} draws the hysteresis mark:\n{c}"
            );
        }
        // Non-inverting Schmitt keeps a plain output; the inverter adds the bubble.
        assert!(schmitt.contains("(pin output line (at 7.62 0 180)"));
        assert!(schmitt_inv.contains("(pin output inverted (at 7.62 0 180)"));
    }

    #[tokio::test]
    async fn glyph_or_and_xor_differ_by_the_extra_back_arc() {
        let pins = json!([
            {"number":"1","name":"A","type":"input"},
            {"number":"2","name":"B","type":"input"},
            {"number":"3","name":"Y","type":"output"}
        ]);
        let or = make_symbol("or", pins.clone()).await;
        let xor = make_symbol("xor", pins.clone()).await;
        let nor = make_symbol("nor", pins.clone()).await;
        let xnor = make_symbol("xnor", pins).await;
        // Both have the OR concave back arc; XOR/XNOR add a second offset arc.
        for (g, c) in [("or", &or), ("xor", &xor)] {
            assert!(
                c.contains("(start -3.81 3.81)"),
                "{g} has the OR back arc:\n{c}"
            );
        }
        assert!(
            !or.contains("(start -4.4196 3.81)"),
            "OR has no second back arc"
        );
        assert!(
            xor.contains("(start -4.4196 3.81)"),
            "XOR adds the offset back arc:\n{xor}"
        );
        // Inverting variants carry the output bubble.
        assert!(nor.contains("(pin output inverted (at 7.62 0 180)"));
        assert!(xnor.contains("(pin output inverted (at 7.62 0 180)"));
        assert!(or.contains("(pin output line (at 7.62 0 180)"));
    }

    #[tokio::test]
    async fn pin_style_applies_on_glyph_and_rectangle() {
        // A clock input on a buffer glyph emits the clock pin style.
        let c = make_symbol(
            "buffer",
            json!([
                {"number":"1","name":"CLK","type":"input","style":"clock"},
                {"number":"2","name":"Y","type":"output"}
            ]),
        )
        .await;
        assert!(
            c.contains("(pin input clock (at -7.62 0 0)"),
            "clock style on a glyph input:\n{c}"
        );

        // On the rectangle path, a per-pin style is honored too.
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("r.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "R",
            "reference_prefix": "U",
            "pins": [
                {"number":"1","name":"~RST","type":"input","style":"inverted","x":-7.62,"y":0.0,"angle":0,"length":2.54}
            ]
        });
        handle_create_symbol(&args, &test_ctx()).await.unwrap();
        let rc = std::fs::read_to_string(&lib).unwrap();
        // Position is not asserted: the body is sized to fit the pin name, which
        // can slide the pin out to meet it (see symbol_body_rect).
        assert!(
            rc.contains("(pin input inverted (at "),
            "inverted style on a rectangle pin:\n{rc}"
        );
    }

    #[tokio::test]
    async fn glyph_falls_back_to_rectangle_on_incompatible_pins() {
        // A NAND glyph given 3 inputs can't be drawn as a 2-input gate; it falls
        // back to a rectangle and reports a warning instead of misrepresenting.
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("fb.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "FB",
            "reference_prefix": "U",
            "glyph": "nand",
            "pins": [
                {"number":"1","name":"A","type":"input","x":-7.62,"y":2.54,"angle":0,"length":2.54},
                {"number":"2","name":"B","type":"input","x":-7.62,"y":0.0,"angle":0,"length":2.54},
                {"number":"3","name":"C","type":"input","x":-7.62,"y":-2.54,"angle":0,"length":2.54},
                {"number":"4","name":"Y","type":"output","x":7.62,"y":0.0,"angle":180,"length":2.54}
            ]
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        let c = std::fs::read_to_string(&lib).unwrap();
        assert!(
            c.contains("(rectangle"),
            "fell back to a rectangle body:\n{c}"
        );
        assert!(!c.contains("(arc"), "must not draw the AND arc on fallback");
        let text = result_text(&res);
        assert!(
            text.contains("warnings") && text.contains("rectangle instead"),
            "fallback reports a warning:\n{text}"
        );
    }

    #[tokio::test]
    async fn glyph_default_applies_to_units_and_quad_nand_layout() {
        // Symbol-level glyph "nand" applies to every signal unit that doesn't
        // override it; power pins stay a rectangular power unit.
        let unit = json!({ "pins": [
            {"number":"1","name":"A","type":"input"},
            {"number":"2","name":"B","type":"input"},
            {"number":"3","name":"Y","type":"output"}
        ]});
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("quad.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(),
            "name": "QUAD_NAND",
            "reference_prefix": "U",
            "glyph": "nand",
            "units": [unit.clone(), unit.clone(), unit.clone(), unit.clone()],
            "power_pins": [
                {"number":"14","name":"VCC","type":"power_in","x":0.0,"y":7.62,"angle":270,"length":2.54},
                {"number":"7","name":"GND","type":"power_in","x":0.0,"y":-7.62,"angle":90,"length":2.54}
            ]
        });
        let res = handle_create_symbol(&args, &test_ctx()).await.unwrap();
        assert!(!res.is_error);
        let c = std::fs::read_to_string(&lib).unwrap();
        // Four NAND gate units (each an AND arc + inverted output) ...
        assert_eq!(
            c.matches("(arc").count(),
            4,
            "one AND body arc per gate:\n{c}"
        );
        assert_eq!(
            c.matches("(pin output inverted").count(),
            4,
            "four inverted NAND outputs"
        );
        // ... plus a fifth, rectangular power unit.
        assert!(
            c.contains("(symbol \"QUAD_NAND_5_1\""),
            "power unit is unit 5"
        );
        assert!(!c.contains("QUAD_NAND_6_1"), "exactly five units");
        assert_eq!(
            c.matches("(rectangle").count(),
            1,
            "only the power unit is a rectangle"
        );
        assert!(konnect_sexp::parser::parse_sexp(&c).is_ok());
    }

    #[tokio::test]
    async fn glyph_pin_names_use_the_smaller_font_numbers_stay_default() {
        // Glyph bodies are compact, so pin names use the 0.762 mm text to keep
        // them from overlapping; numbers (outside the body) stay at 1.27 mm.
        let c = make_symbol(
            "nand",
            json!([
                {"number":"1","name":"A","type":"input"},
                {"number":"2","name":"B","type":"input"},
                {"number":"3","name":"Y","type":"output"}
            ]),
        )
        .await;
        assert!(
            c.contains("(name \"A\" (effects (font (size 0.762 0.762))))"),
            "glyph pin names use the compact 0.762 font:\n{c}"
        );
        assert!(
            c.contains("(number \"1\" (effects (font (size 1.27 1.27))))"),
            "glyph pin numbers keep the default 1.27 font"
        );

        // The rectangle path is unchanged (names stay at 1.27).
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("r.kicad_sym");
        let args = json!({
            "library_path": lib.to_string_lossy(), "name": "R", "reference_prefix": "U",
            "pins": [{"number":"1","name":"IN","type":"input","x":-7.62,"y":0.0,"angle":0,"length":2.54}]
        });
        handle_create_symbol(&args, &test_ctx()).await.unwrap();
        let rc = std::fs::read_to_string(&lib).unwrap();
        assert!(
            rc.contains("(name \"IN\" (effects (font (size 1.27 1.27))))"),
            "rectangle pin names keep the default 1.27 font:\n{rc}"
        );
    }
}

#[cfg(test)]
mod symbol_source_tests {
    use super::*;
    use konnect_schematic_editor::library::SymbolLibrarySource;

    /// The bug this fixes: a library registered in the project table was
    /// invisible to placement, which only scanned the KiCad install dirs.
    #[test]
    fn project_table_entry_is_offered_before_the_install_dirs() {
        let proj = tempfile::tempdir().unwrap();
        let lib = proj.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let file = lib.join("vendor-parts.kicad_sym");
        std::fs::write(&file, "(kicad_symbol_lib)\n").unwrap();

        std::fs::write(
            proj.path().join("sym-lib-table"),
            format!(
                "(sym_lib_table\n  (version 7)\n  (lib (name \"MyParts\") (type \"KiCad\") (uri \"{}\") (options \"\") (descr \"\"))\n)\n",
                file.display()
            ),
        )
        .unwrap();

        let src = KiCadSymbolSource::new(Some(proj.path().to_path_buf()));
        let candidates = src.candidates("MyParts");

        assert_eq!(
            candidates.first(),
            Some(&file),
            "the project table entry must be tried first, got {candidates:?}"
        );
    }

    /// `${KIPRJMOD}` resolves against the table's own directory, not the env.
    #[test]
    fn kiprjmod_uri_resolves_against_the_project_dir() {
        let proj = tempfile::tempdir().unwrap();
        let lib = proj.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let file = lib.join("parts.kicad_sym");
        std::fs::write(&file, "(kicad_symbol_lib)\n").unwrap();

        std::fs::write(
            proj.path().join("sym-lib-table"),
            "(sym_lib_table\n  (version 7)\n  (lib (name \"Proj\") (type \"KiCad\") (uri \"${KIPRJMOD}/lib/parts.kicad_sym\") (options \"\") (descr \"\"))\n)\n",
        )
        .unwrap();

        let src = KiCadSymbolSource::new(Some(proj.path().to_path_buf()));
        assert!(
            src.candidates("Proj").contains(&file),
            "${{KIPRJMOD}} must expand to the project dir"
        );
    }

    /// A stock install keeps working with no table entry at all. Points
    /// KICAD10_SYMBOL_DIR at a tempdir rather than asserting against whatever
    /// KiCad is installed — CI has none, so the fallback would have no
    /// directory to derive from and the assertion would be vacuous at best.
    #[test]
    fn unregistered_nickname_falls_back_to_the_conventional_layout() {
        let _env = crate::tools::KICAD_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let install = tempfile::tempdir().unwrap();
        std::env::set_var("KICAD10_SYMBOL_DIR", install.path());

        let src = KiCadSymbolSource::new(None);
        let candidates = src.candidates("Device");
        assert!(
            candidates.contains(&install.path().join("Device.kicad_symdir")),
            "expected the symdir fallback, got {candidates:?}"
        );
        assert!(
            candidates.contains(&install.path().join("Device.kicad_sym")),
            "expected the single-file fallback, got {candidates:?}"
        );
    }

    /// Reported on #136: a sheet under `<proj>/sheets/` saw no project library,
    /// because the table was looked for beside the sheet rather than at the
    /// project root. `add_hierarchical_sheet` accepts such a `sheet_file`, so
    /// this is reachable through Konnect alone.
    #[test]
    fn a_sheet_in_a_subdirectory_resolves_against_the_project_table() {
        let proj = tempfile::tempdir().unwrap();
        let file = proj.path().join("parts.kicad_sym");
        std::fs::write(&file, "(kicad_symbol_lib)\n").unwrap();
        std::fs::write(proj.path().join("board.kicad_pro"), "{}\n").unwrap();
        std::fs::write(
            proj.path().join("sym-lib-table"),
            "(sym_lib_table\n  (version 7)\n  (lib (name \"MyLib\") (type \"KiCad\") (uri \"${KIPRJMOD}/parts.kicad_sym\") (options \"\") (descr \"\"))\n)\n",
        )
        .unwrap();

        let sheets = proj.path().join("sheets");
        std::fs::create_dir_all(&sheets).unwrap();
        let child = sheets.join("child.kicad_sch");
        std::fs::write(&child, "(kicad_sch)\n").unwrap();
        std::fs::write(
            proj.path().join("board.kicad_sch"),
            r#"(kicad_sch
	(version 20250610)
	(generator "eeschema")
	(uuid "root-uuid")
	(paper "A4")
	(lib_symbols)
	(sheet
		(at 20 20)
		(size 40 20)
		(uuid "child-instance")
		(property "Sheetname" "Child" (at 20 19.365 0))
		(property "Sheetfile" "sheets/child.kicad_sch" (at 20 40.635 0))
	)
	(sheet_instances (path "/" (page "1")))
)
"#,
        )
        .unwrap();

        let candidates = KiCadSymbolSource::for_file(&child)
            .unwrap()
            .candidates("MyLib");
        assert!(
            candidates.contains(&file),
            "a sub-sheet must see the project table at the root, got {candidates:?}"
        );
    }

    /// The fallback the walk must not break: a schematic belonging to no
    /// project still resolves against the tables sitting beside it.
    #[test]
    fn a_schematic_with_no_project_falls_back_to_its_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("parts.kicad_sym");
        std::fs::write(&file, "(kicad_symbol_lib)\n").unwrap();
        std::fs::write(
            dir.path().join("sym-lib-table"),
            "(sym_lib_table\n  (version 7)\n  (lib (name \"Loose\") (type \"KiCad\") (uri \"${KIPRJMOD}/parts.kicad_sym\") (options \"\") (descr \"\"))\n)\n",
        )
        .unwrap();
        let sch = dir.path().join("loose.kicad_sch");
        std::fs::write(&sch, "(kicad_sch)\n").unwrap();

        let candidates = KiCadSymbolSource::for_file(&sch)
            .unwrap()
            .candidates("Loose");
        assert!(
            candidates.contains(&file),
            "a projectless schematic must still use the table beside it, got {candidates:?}"
        );
    }

    /// The walk is unbounded, so an unrelated `.kicad_pro` in any ancestor used
    /// to capture the search and send every lookup to the wrong `KIPRJMOD`. A
    /// project nested inside another project's folder is the realistic case;
    /// the one that actually bit was a stray `.kicad_pro` in the system temp
    /// directory, which quietly defeated the hermetic fixtures of two tests —
    /// they passed on CI, where temp is clean, and failed on any machine that
    /// had accumulated one.
    ///
    /// A table beside the file is the more specific statement and must win.
    #[test]
    fn a_table_beside_the_file_beats_an_unrelated_project_further_up() {
        let outer = tempfile::tempdir().unwrap();
        // An unrelated project sitting above — the interloper.
        std::fs::write(outer.path().join("Unrelated.kicad_pro"), "{}\n").unwrap();
        std::fs::write(
            outer.path().join("sym-lib-table"),
            "(sym_lib_table\n  (version 7)\n  (lib (name \"Shared\") (type \"KiCad\") (uri \"${KIPRJMOD}/wrong.kicad_sym\") (options \"\") (descr \"\"))\n)\n",
        )
        .unwrap();

        let inner = outer.path().join("nested");
        std::fs::create_dir(&inner).unwrap();
        let want = inner.join("right.kicad_sym");
        std::fs::write(&want, "(kicad_symbol_lib)\n").unwrap();
        std::fs::write(
            inner.join("sym-lib-table"),
            "(sym_lib_table\n  (version 7)\n  (lib (name \"Shared\") (type \"KiCad\") (uri \"${KIPRJMOD}/right.kicad_sym\") (options \"\") (descr \"\"))\n)\n",
        )
        .unwrap();
        let sch = inner.join("nested.kicad_sch");
        std::fs::write(&sch, "(kicad_sch)\n").unwrap();

        let candidates = KiCadSymbolSource::for_file(&sch)
            .unwrap()
            .candidates("Shared");
        assert!(
            candidates.contains(&want),
            "the table beside the schematic must win, got {candidates:?}"
        );
        assert!(
            !candidates.contains(&outer.path().join("wrong.kicad_sym")),
            "the outer project's table must not be consulted, got {candidates:?}"
        );
    }

    /// `Path::new("board.kicad_sch").parent()` is `Some("")`, not `None`. The
    /// walk must leave that as-is so a bare relative path keeps resolving
    /// against the working directory.
    #[test]
    fn a_bare_relative_schematic_keeps_an_empty_project_dir() {
        assert_eq!(
            project_root_for(Path::new("board.kicad_sch")).unwrap(),
            Some(PathBuf::new())
        );
    }

    #[test]
    fn project_scoped_registration_writes_a_portable_uri() {
        let proj = tempfile::tempdir().unwrap();
        let lib = proj.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let file = lib.join("parts.kicad_sym");
        std::fs::write(&file, "(kicad_symbol_lib)\n").unwrap();

        assert_eq!(
            project_relative_uri(&file, proj.path()),
            "${KIPRJMOD}/lib/parts.kicad_sym"
        );
    }

    /// `Path::new("board.kicad_pro").parent()` is `Some("")`, and
    /// `strip_prefix("")` returns the whole path — which would emit
    /// `${KIPRJMOD}//abs/path`, resolving nowhere on read.
    #[test]
    fn an_empty_table_dir_does_not_produce_a_rooted_kiprjmod_uri() {
        let other = tempfile::tempdir().unwrap();
        let file = other.path().join("parts.kicad_sym");
        std::fs::write(&file, "(kicad_symbol_lib)\n").unwrap();

        let uri = project_relative_uri(&file, Path::new(""));
        assert!(
            !uri.contains("KIPRJMOD"),
            "empty project dir must fall back to an absolute uri: {uri}"
        );
        assert_eq!(uri, portable_uri(&file));
    }

    /// A library outside the project keeps an absolute URI, but still written
    /// with forward slashes — a backslash URI does not survive the project
    /// being opened on Linux or macOS.
    #[test]
    fn a_library_outside_the_project_stays_absolute_with_forward_slashes() {
        let proj = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let file = other.path().join("parts.kicad_sym");
        std::fs::write(&file, "(kicad_symbol_lib)\n").unwrap();

        let uri = project_relative_uri(&file, proj.path());
        assert!(!uri.contains("KIPRJMOD"), "outside the project: {uri}");
        assert!(!uri.contains('\\'), "uri must be slash-separated: {uri}");
    }

    /// Global scope is never relativized, and never carries the Windows
    /// verbatim prefix or backslashes.
    #[test]
    fn global_scope_writes_a_plain_forward_slash_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("parts.kicad_sym");
        std::fs::write(&file, "(kicad_symbol_lib)\n").unwrap();

        let (table, uri) = lib_table_target(
            "global",
            None,
            &file,
            PathBuf::from("/cfg/sym-lib-table"),
            "sym-lib-table",
        )
        .expect("global scope needs no project");

        assert_eq!(table, PathBuf::from("/cfg/sym-lib-table"));
        assert!(!uri.contains("KIPRJMOD"), "global is never relative: {uri}");
        assert!(!uri.starts_with(r"\\?\"), "verbatim prefix leaked: {uri}");
        assert!(!uri.contains('\\'), "uri must be slash-separated: {uri}");
    }

    /// Registering a library before it exists on disk is normal — the file is
    /// often created by the very next call. `canonicalize` fails for a path
    /// that is not there yet, so without a lexical fallback the URI silently
    /// came out absolute and the project stopped being portable.
    #[test]
    fn a_library_not_yet_on_disk_still_gets_a_portable_uri() {
        let proj = tempfile::tempdir().unwrap();
        let file = proj.path().join("lib").join("not-created-yet.kicad_sym");

        assert_eq!(
            project_relative_uri(&file, proj.path()),
            "${KIPRJMOD}/lib/not-created-yet.kicad_sym"
        );
    }

    /// Both registrars share one target helper, so footprints get the same
    /// portable URI and the same empty-parent guard as symbols.
    /// The report: passing the project *directory* resolved one level above it,
    /// so the table landed where KiCad never looks and the call still said
    /// `inserted`.
    #[test]
    fn a_project_directory_resolves_to_its_own_table() {
        let parent = tempfile::tempdir().unwrap();
        let proj = parent.path().join("board");
        std::fs::create_dir_all(proj.join("lib")).unwrap();
        let sym = proj.join("lib/parts.kicad_sym");
        std::fs::write(&sym, "(kicad_symbol_lib)\n").unwrap();

        let (table, uri) = lib_table_target(
            "project",
            proj.to_str(),
            &sym,
            global_sym_lib_table(),
            "sym-lib-table",
        )
        .expect("a project directory is a usable project argument");

        assert_eq!(table, proj.join("sym-lib-table"));
        assert_ne!(
            table,
            parent.path().join("sym-lib-table"),
            "the table must not land above the project"
        );
        assert_eq!(uri, "${KIPRJMOD}/lib/parts.kicad_sym");
    }

    /// Both spellings of the same project must name the same table, or a caller
    /// gets a different answer depending on which one it happened to pass.
    #[test]
    fn a_project_file_and_its_directory_agree() {
        let proj = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join("lib")).unwrap();
        let sym = proj.path().join("lib/parts.kicad_sym");
        std::fs::write(&sym, "(kicad_symbol_lib)\n").unwrap();
        let proj_file = proj.path().join("board.kicad_pro");
        std::fs::write(&proj_file, "{}\n").unwrap();

        let from_file = lib_table_target(
            "project",
            proj_file.to_str(),
            &sym,
            global_sym_lib_table(),
            "sym-lib-table",
        )
        .expect("project file");
        let from_dir = lib_table_target(
            "project",
            proj.path().to_str(),
            &sym,
            global_sym_lib_table(),
            "sym-lib-table",
        )
        .expect("project directory");

        assert_eq!(from_file, from_dir);
    }

    /// The follow-on failure in the report: with both projects resolving to the
    /// same stray table, registering a nickname for the second one came back
    /// `unchanged` because the handler was reading the first one's entry.
    #[test]
    fn two_projects_under_one_parent_get_separate_tables() {
        let parent = tempfile::tempdir().unwrap();
        let one = parent.path().join("alpha");
        let two = parent.path().join("beta");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let lib = parent.path().join("shared.kicad_sym");
        std::fs::write(&lib, "(kicad_symbol_lib)\n").unwrap();

        let (table_one, _) = lib_table_target(
            "project",
            one.to_str(),
            &lib,
            global_sym_lib_table(),
            "sym-lib-table",
        )
        .expect("first project");
        let (table_two, _) = lib_table_target(
            "project",
            two.to_str(),
            &lib,
            global_sym_lib_table(),
            "sym-lib-table",
        )
        .expect("second project");

        assert_ne!(
            table_one, table_two,
            "two projects must not share one table"
        );
        assert_eq!(table_one, one.join("sym-lib-table"));
        assert_eq!(table_two, two.join("sym-lib-table"));
    }

    /// A path that is neither a directory nor recognisable as a file is refused
    /// rather than resolved to its parent — guessing is what produced the stray
    /// table in the first place.
    #[test]
    fn an_unusable_project_argument_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("no-such-project");
        let lib = parent.path().join("parts.kicad_sym");
        std::fs::write(&lib, "(kicad_symbol_lib)\n").unwrap();

        let result = lib_table_target(
            "project",
            missing.to_str(),
            &lib,
            global_sym_lib_table(),
            "sym-lib-table",
        );

        assert!(
            result.is_err(),
            "a path that names nothing must not resolve"
        );
        assert!(
            !parent.path().join("sym-lib-table").exists(),
            "nothing may be written above the project"
        );
    }

    /// A `.kicad_pro` that does not exist yet still resolves — the project file
    /// is often written by the very next call.
    #[test]
    fn a_project_file_not_yet_on_disk_still_resolves() {
        let proj = tempfile::tempdir().unwrap();
        let lib = proj.path().join("parts.kicad_sym");
        std::fs::write(&lib, "(kicad_symbol_lib)\n").unwrap();
        let not_yet = proj.path().join("board.kicad_pro");

        let (table, _) = lib_table_target(
            "project",
            not_yet.to_str(),
            &lib,
            global_sym_lib_table(),
            "sym-lib-table",
        )
        .expect("a .kicad_pro path resolves whether or not it exists yet");

        assert_eq!(table, proj.path().join("sym-lib-table"));
    }

    /// The footprint registrar shares `lib_table_target`, so the directory form
    /// has to reach it too.
    #[test]
    fn the_footprint_registrar_takes_a_project_directory_as_well() {
        let parent = tempfile::tempdir().unwrap();
        let proj = parent.path().join("board");
        std::fs::create_dir_all(proj.join("lib/parts.pretty")).unwrap();
        let fp = proj.join("lib/parts.pretty");

        let (table, uri) = lib_table_target(
            "project",
            proj.to_str(),
            &fp,
            global_fp_lib_table(),
            "fp-lib-table",
        )
        .expect("footprint target");

        assert_eq!(table, proj.join("fp-lib-table"));
        assert_eq!(uri, "${KIPRJMOD}/lib/parts.pretty");
    }

    #[test]
    fn both_registrars_agree_on_the_project_table_target() {
        let proj = tempfile::tempdir().unwrap();
        let lib = proj.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let sym = lib.join("parts.kicad_sym");
        std::fs::write(&sym, "(kicad_symbol_lib)\n").unwrap();
        let fp = lib.join("parts.pretty");
        std::fs::create_dir_all(&fp).unwrap();

        let proj_file = proj.path().join("board.kicad_pro");
        let proj_arg = proj_file.to_str();

        let (sym_table, sym_uri) = lib_table_target(
            "project",
            proj_arg,
            &sym,
            global_sym_lib_table(),
            "sym-lib-table",
        )
        .expect("symbol target");
        let (fp_table, fp_uri) = lib_table_target(
            "project",
            proj_arg,
            &fp,
            global_fp_lib_table(),
            "fp-lib-table",
        )
        .expect("footprint target");

        assert_eq!(sym_uri, "${KIPRJMOD}/lib/parts.kicad_sym");
        assert_eq!(fp_uri, "${KIPRJMOD}/lib/parts.pretty");
        assert_eq!(sym_table, proj.path().join("sym-lib-table"));
        assert_eq!(fp_table, proj.path().join("fp-lib-table"));
    }

    #[test]
    fn a_library_outside_the_project_keeps_its_absolute_uri() {
        let proj = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let file = other.path().join("shared.kicad_sym");
        std::fs::write(&file, "(kicad_symbol_lib)\n").unwrap();

        let uri = project_relative_uri(&file, proj.path());
        assert!(
            !uri.contains("KIPRJMOD"),
            "nothing portable to say about an out-of-project library: {uri}"
        );
    }
}

/// A missing argument must stay a structured `invalid_argument` naming the
/// field, rather than collapsing into a generic `handler_error`.
///
/// Seven handlers here did `map_err(|e| anyhow!("{:?}", e))?` on the
/// `CallToolResult` that `require_str` returns, debug-formatting a typed error
/// into a string and losing the taxonomy on the way out. `pcb_components`
/// already returned the result directly. This is what lets a caller tell "you
/// forgot an argument" from "the tool tried and failed".
#[cfg(test)]
mod argument_error_kind_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use serde_json::json;
    use std::sync::Arc;

    fn ctx() -> Arc<ToolContext> {
        Arc::new(ToolContext::new(
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
        ))
    }

    #[tokio::test]
    async fn missing_required_arguments_report_invalid_argument_with_the_field() {
        // (tool, args that satisfy everything *except* the field under test,
        //  the field it should name)
        //
        // The path arguments are still supplied deliberately: this test is
        // about `require_str`, and a handler that reads a path first would
        // otherwise fail on that instead, making the assertion vacuous. Since
        // #194 those path failures are themselves `invalid_argument`, so the
        // wrong one would still *pass* the kind check while naming a different
        // field — which is exactly why the field is asserted too. Path
        // arguments get their own coverage in
        // `mcp::handler::path_argument_taxonomy_tests`.
        let cases = [
            ("get_symbol_info", json!({}), "lib_id"),
            ("get_footprint_info", json!({}), "footprint_path"),
            (
                "register_footprint_library",
                json!({ "library_path": "/tmp/x.pretty", "scope": "global" }),
                "nickname",
            ),
            (
                "register_symbol_library",
                json!({ "library_path": "/tmp/x.kicad_sym", "scope": "global" }),
                "nickname",
            ),
        ];

        for (tool_name, args, field) in cases {
            let def = tools()
                .into_iter()
                .find(|t| t.name == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} is registered"));

            // The old pattern turned this into an Err, so the unwrap itself is
            // part of the assertion.
            let result = (def.handler)(&args, ctx())
                .await
                .unwrap_or_else(|e| panic!("{tool_name} must not bubble an anyhow error: {e}"));

            assert!(result.is_error, "{tool_name}: missing argument must fail");

            let text = match result.content.first() {
                Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
                other => panic!("{tool_name}: expected text, got {other:?}"),
            };
            let parsed: serde_json::Value =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool_name}: {e}: {text}"));

            assert_eq!(
                parsed["error"]["kind"], "invalid_argument",
                "{tool_name} must report a typed argument error, not a \
                 debug-formatted handler_error: {text}"
            );
            assert_eq!(
                parsed["error"]["field"], field,
                "{tool_name} must name the missing field: {text}"
            );
        }
    }
}

/// A tool that declares an argument required must refuse the call when it is
/// absent — not substitute a value, do the work, and report success.
///
/// `create_footprint` is the reason this module exists. With `pads` omitted it
/// produced a footprint with no pads, no courtyard, no silkscreen and no fab
/// outline, named `"Footprint"` because `name` defaulted too, and committed it
/// with `write_atomic` — an unconditional replace with no if-unchanged check.
/// Measured against a real 0402 resistor footprint: **805 bytes and 2 pads
/// became 121 bytes and 0 pads**, and the call returned
/// `{"success": true, "pad_count": 0}` (#218).
#[cfg(test)]
mod required_argument_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use serde_json::json;
    use std::sync::Arc;

    fn ctx() -> Arc<ToolContext> {
        Arc::new(ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(crate::router::ToolRouter::new()),
        ))
    }

    fn error_field(result: &CallToolResult) -> String {
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
        assert_eq!(
            parsed["error"]["kind"], "invalid_argument",
            "must be a typed argument error: {text}"
        );
        parsed["error"]["field"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    async fn call(tool_name: &str, args: serde_json::Value) -> CallToolResult {
        let def = tools()
            .into_iter()
            .find(|t| t.name == tool_name)
            .unwrap_or_else(|| panic!("{tool_name} is registered"));
        (def.handler)(&args, ctx())
            .await
            .unwrap_or_else(|e| panic!("{tool_name} must not bubble anyhow: {e}"))
    }

    /// The property that matters: a refused call leaves the target file exactly
    /// as it was. Asserting only the error would still pass if the write
    /// happened first.
    #[tokio::test]
    async fn create_footprint_without_pads_refuses_and_leaves_the_file_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("R_0402_1005Metric.kicad_mod");
        let original = "(footprint \"R_0402_1005Metric\"\n\t(layer \"F.Cu\")\n\t\
             (pad \"1\" smd roundrect (at -0.51 0) (size 0.54 0.64) \
             (layers \"F.Cu\" \"F.Paste\" \"F.Mask\"))\n\t\
             (pad \"2\" smd roundrect (at 0.51 0) (size 0.54 0.64) \
             (layers \"F.Cu\" \"F.Paste\" \"F.Mask\"))\n)\n";
        std::fs::write(&path, original).unwrap();

        let result = call(
            "create_footprint",
            json!({ "output": path.display().to_string(), "name": "R_0402_1005Metric" }),
        )
        .await;

        assert!(result.is_error, "a footprint with no pads must be refused");
        assert_eq!(error_field(&result), "pads");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the existing footprint must survive a refused call untouched"
        );
    }

    #[tokio::test]
    async fn create_footprint_without_a_name_refuses_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.kicad_mod");
        let result = call(
            "create_footprint",
            json!({ "output": path.display().to_string(), "pads": [] }),
        )
        .await;
        assert!(result.is_error);
        assert_eq!(error_field(&result), "name");
        assert!(
            !path.exists(),
            "nothing should be created for a refused call"
        );
    }

    /// An empty `pads` array is a different thing from an absent one: the
    /// caller said "no pads", which is a coherent request for a mechanical
    /// footprint. Only the absent case is a mistake.
    #[tokio::test]
    async fn an_explicitly_empty_pads_array_is_still_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mech.kicad_mod");
        let result = call(
            "create_footprint",
            json!({ "output": path.display().to_string(), "name": "Mech", "pads": [] }),
        )
        .await;
        assert!(!result.is_error, "an explicit empty pad list is a request");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn create_symbol_refuses_a_missing_name_or_reference_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.kicad_sym");
        std::fs::write(&path, "(kicad_symbol_lib\n)\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        for (args, field) in [
            (json!({ "reference_prefix": "R" }), "name"),
            (json!({ "name": "MyPart" }), "reference_prefix"),
        ] {
            let mut args = args;
            args["library_path"] = json!(path.display().to_string());
            let result = call("create_symbol", args).await;
            assert!(result.is_error, "must refuse when {field} is absent");
            assert_eq!(error_field(&result), field);
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "no symbol should have been appended"
        );
    }
}
