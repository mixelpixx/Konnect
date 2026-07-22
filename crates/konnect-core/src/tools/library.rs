//! `library` toolset — create and manage footprints, symbols, and KiCAD library tables.
//!
//! Operations are file-based (S-expression manipulation + directory scanning).
//! No IPC or kicad-cli is required for most tools.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use konnect_sexp::writer::write_atomic;
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
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
                                "drill": { "type": "number", "description": "Drill diameter for thru-hole pads" }
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
                    "x": { "type": "number", "description": "New X position in mm (optional)" },
                    "y": { "type": "number", "description": "New Y position in mm (optional)" },
                    "width": { "type": "number", "description": "New pad width in mm (optional)" },
                    "height": { "type": "number", "description": "New pad height in mm (optional)" },
                    "shape": { "type": "string", "description": "New pad shape (optional)" },
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
                    "project": { "type": "string", "description": "Path to .kicad_pro file (required for project scope)" }
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
             op-amps or gate banks (via `units` + optional `common_pins`). Each unit gets a \
             rectangular body sized to its pins.",
            json!({
                "type": "object",
                "properties": {
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" },
                    "name": { "type": "string", "description": "Symbol name" },
                    "reference_prefix": { "type": "string", "description": "Default reference prefix (e.g. 'U')" },
                    "value": { "type": "string", "description": "Default value string" },
                    "pins": {
                        "type": "array",
                        "description": "Pin definitions",
                        "items": {
                            "type": "object",
                            "properties": {
                                "number": { "type": "string" },
                                "name": { "type": "string" },
                                "type": { "type": "string", "description": "'input', 'output', 'bidirectional', 'power_in', 'power_out', 'passive'" },
                                "x": { "type": "number" },
                                "y": { "type": "number" },
                                "angle": { "type": "number", "default": 0 },
                                "length": { "type": "number", "default": 2.54 }
                            },
                            "required": ["number", "name", "type", "x", "y"]
                        }
                    },
                    "show_pin_names": { "type": "boolean", "description": "Show pin names on the symbol (default true).", "default": true },
                    "show_pin_numbers": { "type": "boolean", "description": "Show pin numbers on the symbol (default true).", "default": true },
                    "units": {
                        "type": "array",
                        "description": "For MULTI-UNIT parts (dual/quad op-amps, gate banks, multi-bank connectors). Each element is one unit (becomes Unit A, B, C...) with its own pins and its own rectangular body. When given, `units` replaces `pins` (use `pins` for single-unit symbols instead). Each unit's pins use the same shape as `pins`.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "pins": {
                                    "type": "array",
                                    "description": "Pins for this unit",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "number": { "type": "string" },
                                            "name": { "type": "string" },
                                            "type": { "type": "string", "description": "'input', 'output', 'bidirectional', 'power_in', 'power_out', 'passive'" },
                                            "x": { "type": "number" },
                                            "y": { "type": "number" },
                                            "angle": { "type": "number", "default": 0 },
                                            "length": { "type": "number", "default": 2.54 }
                                        },
                                        "required": ["number", "name", "type", "x", "y"]
                                    }
                                }
                            },
                            "required": ["pins"]
                        }
                    },
                    "power_pins": {
                        "type": "array",
                        "description": "Shared power pins (V+/V-, VCC/GND). Only meaningful with `units`: they become a dedicated final 'power unit' (e.g. Unit C of a dual op-amp, Unit E of a quad gate) placed once, following KiCAD's own 74xx convention. This avoids drawing the power pins on every unit (which would each need wiring to pass ERC). Same shape as `pins`.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "number": { "type": "string" },
                                "name": { "type": "string" },
                                "type": { "type": "string" },
                                "x": { "type": "number" },
                                "y": { "type": "number" },
                                "angle": { "type": "number", "default": 0 },
                                "length": { "type": "number", "default": 2.54 }
                            },
                            "required": ["number", "name", "type", "x", "y"]
                        }
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
                    "library_path": { "type": "string", "description": "Path to .kicad_sym library file" }
                },
                "required": ["library_path"]
            }),
            |args, ctx| async move { handle_list_symbols_in_library(args, ctx).await }
        ),
        tool!(
            "register_symbol_library",
            "Register a .kicad_sym library file in the KiCAD global or project symbol table.",
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
                    "project": { "type": "string", "description": "Path to .kicad_pro file (required for project scope)" }
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
                    "limit": { "type": "integer", "description": "Maximum number of results to return", "default": 50 }
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
            "Return detailed information about a footprint: pad layout, courtyard, description.",
            json!({
                "type": "object",
                "properties": {
                    "footprint_path": { "type": "string", "description": "Path to .kicad_mod file, OR 'Library:Footprint' identifier" }
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
                    "lib_id": { "type": "string", "description": "Library:Symbol identifier (e.g. 'Device:R')" }
                },
                "required": ["lib_id"]
            }),
            |args, ctx| async move { handle_get_symbol_info(args, ctx).await }
        ),
    ]
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
    let name = args["name"].as_str().unwrap_or("Footprint");
    let description = args["description"].as_str().unwrap_or("");

    let pads_val = args["pads"].as_array().cloned().unwrap_or_default();
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

        let layers = if pad_type == "smd" {
            r#"(layers "F.Cu" "F.Paste" "F.Mask")"#
        } else {
            r#"(layers "*.Cu" "*.Mask")"#
        };

        let drill_sexp = if let Some(drill) = pad["drill"].as_f64() {
            format!("(drill {})", drill)
        } else {
            String::new()
        };

        pad_sexp.push_str(&format!(
            "\n  (pad \"{}\" {} {} (at {} {}) (size {} {}) {} {})",
            number, pad_type, shape, x, y, w, h, layers, drill_sexp
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
        serde_json::to_string_pretty(&json!({
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
    let pad_number = require_str(args, "pad_number").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let content = tokio::fs::read_to_string(&path).await?;

    // Find the pad block:  (pad "N" ... (at X Y) (size W H) ...)
    // We search for the at/size/drill atoms and replace them individually.
    let pad_pat = format!(r#"(pad "{}""#, pad_number);
    let pad_start = content
        .find(&pad_pat)
        .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found in footprint", pad_number))?;

    // Find the closing paren of this pad block (simple depth count)
    let pad_end = {
        let mut depth = 0i32;
        let mut end = pad_start;
        for (i, ch) in content[pad_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = pad_start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        end
    };
    let pad_block = &content[pad_start..pad_end];

    // Helper: replace or add a sub-expression within the pad block
    let mut new_pad = pad_block.to_string();

    if let Some(x) = args["x"].as_f64() {
        // Replace (at OLD_X OLD_Y [ROT]) → update X
        if let Some(at_pos) = new_pad.find("(at ") {
            let at_end = new_pad[at_pos..]
                .find(')')
                .map(|i| at_pos + i + 1)
                .unwrap_or(new_pad.len());
            let at_block = &new_pad[at_pos..at_end];
            // Parse existing values
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
            let new_at = format!("(at {} {}{})", x, old_y, rot);
            new_pad.replace_range(at_pos..at_end, &new_at);
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
            let new_at = format!("(at {} {}{})", old_x, y, rot);
            new_pad.replace_range(at_pos..at_end, &new_at);
        }
    }
    if let (Some(w), Some(h)) = (args["width"].as_f64(), args["height"].as_f64()) {
        if let Some(sz_pos) = new_pad.find("(size ") {
            let sz_end = new_pad[sz_pos..]
                .find(')')
                .map(|i| sz_pos + i + 1)
                .unwrap_or(new_pad.len());
            let new_size = format!("(size {} {})", w, h);
            new_pad.replace_range(sz_pos..sz_end, &new_size);
        }
    }
    if let Some(drill) = args["drill"].as_f64() {
        if let Some(dr_pos) = new_pad.find("(drill ") {
            let dr_end = new_pad[dr_pos..]
                .find(')')
                .map(|i| dr_pos + i + 1)
                .unwrap_or(new_pad.len());
            let new_drill = format!("(drill {})", drill);
            new_pad.replace_range(dr_pos..dr_end, &new_drill);
        } else {
            // Insert drill before closing paren of pad
            let insert_at = new_pad.rfind(')').unwrap_or(new_pad.len());
            new_pad.insert_str(insert_at, &format!(" (drill {})", drill));
        }
    }

    // Apply the pad block replacement
    let new_content = format!(
        "{}{}{}",
        &content[..pad_start],
        new_pad,
        &content[pad_end..]
    );
    write_atomic(&path, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "pad": pad_number
        }))
        .unwrap(),
    ))
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

/// Locate the KiCad install root by probing env vars, the running binary's
/// install location, and OS-standard paths.  Used to expand the `${KICAD10_*}`
/// variables that appear inside template fp/sym-lib-table files.
fn kicad_install_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("KICAD10_INSTALL_DIR") {
        let p = PathBuf::from(&dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(grandparent) = exe.parent().and_then(|p| p.parent()) {
            if grandparent.join("share").join("kicad").is_dir() {
                return Some(grandparent.to_path_buf());
            }
        }
    }
    let candidates: &[&str] = &[
        "/usr/share/kicad",
        "/usr/local/share/kicad",
        "/opt/kicad/share/kicad",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.is_dir() {
            return Some(p.parent().and_then(|p| p.parent())?.to_path_buf());
        }
    }
    None
}

fn kicad_share_dir() -> Option<PathBuf> {
    kicad_install_root().map(|r| r.join("share").join("kicad"))
}

/// Expand `${VAR}` references in a lib-table URI using KiCad's well-known
/// install-relative variables plus the process environment.  Unknown
/// variables are left untouched (KiCad also leaves them unresolved).
fn expand_kicad_uri(uri: &str) -> String {
    let mut out = String::with_capacity(uri.len());
    let bytes = uri.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = uri[i + 2..].find('}') {
                let var = &uri[i + 2..i + 2 + end];
                let resolved = resolve_kicad_var(var);
                out.push_str(&resolved);
                i = i + 2 + end + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn resolve_kicad_var(name: &str) -> String {
    if let Ok(v) = std::env::var(name) {
        return v;
    }
    let share = match kicad_share_dir() {
        Some(p) => p,
        None => return String::new(),
    };
    match name {
        "KICAD10_FOOTPRINT_DIR" | "KICAD8_FOOTPRINT_DIR" | "KICAD_FOOTPRINT_DIR" => {
            share.join("footprints").to_string_lossy().into_owned()
        }
        "KICAD10_SYMBOL_DIR" | "KICAD8_SYMBOL_DIR" | "KICAD_SYMBOL_DIR" => {
            share.join("symbols").to_string_lossy().into_owned()
        }
        "KICAD10_TEMPLATE_DIR" | "KICAD8_TEMPLATE_DIR" | "KICAD_TEMPLATE_DIR" => {
            share.join("template").to_string_lossy().into_owned()
        }
        "KICAD10_3RD_PARTY" | "KICAD8_3RD_PARTY" | "KICAD_3RD_PARTY" => super::kicad_config_dir()
            .join("3rdparty")
            .to_string_lossy()
            .into_owned(),
        "KICAD_CONFIG_HOME" | "KICAD10_CONFIG_HOME" => {
            super::kicad_config_dir().to_string_lossy().into_owned()
        }
        _ => String::new(),
    }
}

/// Recursively parse a lib-table file, expanding any `(lib ... (type "Table"))`
/// entries that point at another lib-table file.  KiCad's own LIBRARY_MANAGER
/// does this transparently when loading fp/sym-lib-table, so callers that
/// enumerate libraries (list/get/search) must mirror the behavior or they
/// cannot resolve entries from the bundled `template/*-lib-table`.
///
/// `visited` guards against reference cycles; `depth` bounds runaway recursion.
async fn parse_lib_table_recursive(
    table_path: &Path,
    scope: &str,
    visited: &mut HashSet<PathBuf>,
    depth: u32,
) -> Vec<serde_json::Value> {
    const MAX_DEPTH: u32 = 8;
    if depth > MAX_DEPTH || !table_path.exists() {
        return Vec::new();
    }
    let canonical = match table_path.canonicalize() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    if visited.contains(&canonical) {
        return Vec::new();
    }
    visited.insert(canonical.clone());

    let content = match tokio::fs::read_to_string(table_path).await {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for mut lib in parse_lib_table(&content) {
        let lib_type = lib["type"].as_str().unwrap_or("").to_string();
        let uri = lib["uri"].as_str().unwrap_or("").to_string();
        if lib_type == "Table" {
            let expanded = expand_kicad_uri(&uri);
            let child_path = PathBuf::from(&expanded);
            let children =
                Box::pin(parse_lib_table_recursive(&child_path, scope, visited, depth + 1)).await;
            out.extend(children);
        } else {
            lib["scope"] = json!(scope);
            out.push(lib);
        }
    }
    out
}

/// Pick the S-expression root element name for a new lib-table based on its
/// file name. `sym-lib-table` → `sym_lib_table`, `fp-lib-table` → `fp_lib_table`.
/// Anything else defaults to `sym_lib_table` to stay on the safe side for symbol use.
fn table_root_element(table_path: &Path) -> &'static str {
    match table_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
    {
        "fp-lib-table" => "fp_lib_table",
        "sym-lib-table" => "sym_lib_table",
        _ => "sym_lib_table",
    }
}

/// Find the next `(lib ` entry in `content` starting at `pos`, returning
/// (start_of_lib_paren, end_of_block_exclusive). Handles both TAB- and
/// space-indented tables, which KiCad emits either way depending on version.
fn find_lib_block(content: &str, pos: usize) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut i = pos;
    while i + 5 <= bytes.len() {
        if &bytes[i..i + 5] == b"(lib " {
            // Require a preceding whitespace character so we don't match
            // inside tokens like `sub_lib` or `lib_symbols`.
            let prev_is_ws = i == 0
                || bytes[i - 1] == b' '
                || bytes[i - 1] == b'\t'
                || bytes[i - 1] == b'\n'
                || bytes[i - 1] == b'\r';
            if prev_is_ws {
                let mut depth = 0i32;
                let mut j = i;
                for (k, &b) in bytes[i..].iter().enumerate() {
                    match b {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                j = i + k + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if depth == 0 {
                    return Some((i, j));
                }
            }
        }
        i += 1;
    }
    None
}

/// Find the next `(symbol "NAME"` block in `content` starting at `pos`,
/// returning (start_index, end_index_exclusive). Handles both TAB- and
/// space-indented entries, as well as the very first entry at offset 0.
fn find_symbol_block(content: &str, symbol_name: &str, pos: usize) -> Option<(usize, usize)> {
    let needle = format!("(symbol \"{}\"", symbol_name);
    let bytes = content.as_bytes();
    let mut search_from = pos;
    while let Some(rel) = content[search_from..].find(&needle) {
        let abs = search_from + rel;
        // Require a preceding whitespace (or start-of-file) so we don't match
        // `(sub_symbol "..."` or other tokens containing "symbol".
        let prev_ok = abs == 0
            || matches!(bytes[abs - 1], b' ' | b'\t' | b'\n' | b'\r');
        if prev_ok {
            let mut depth = 0i32;
            let mut end = abs;
            for (k, &b) in bytes[abs..].iter().enumerate() {
                match b {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = abs + k + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if depth == 0 {
                return Some((abs, end));
            }
        }
        search_from = abs + needle.len();
    }
    None
}

/// Find the next block in `content` starting at `pos` whose opening element is
/// `(prefix ` (e.g. `(pad `, `(pin `, `(lib `). Returns (start, end_exclusive)
/// with balanced parentheses. Matches both TAB- and space-indented layouts.
fn find_prefixed_block(content: &str, prefix: &[u8], pos: usize) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut i = pos;
    while i + prefix.len() <= bytes.len() {
        if &bytes[i..i + prefix.len()] == prefix {
            let prev_is_ws = i == 0
                || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r');
            if prev_is_ws {
                let mut depth = 0i32;
                let mut end = i;
                for (k, &b) in bytes[i..].iter().enumerate() {
                    match b {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = i + k + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if depth == 0 {
                    return Some((i, end));
                }
            }
        }
        i += 1;
    }
    None
}

/// Find the next `(pad ` block in `content` starting at `pos`.
fn find_pad_block(content: &str, pos: usize) -> Option<(usize, usize)> {
    find_prefixed_block(content, b"(pad ", pos)
}

/// Parse a lib-table S-expression and return list of (nickname, uri, type) tuples.
fn parse_lib_table(content: &str) -> Vec<serde_json::Value> {
    let mut libs = Vec::new();
    // Each entry: (lib (name "NICK") (type "...") (uri "...") (options "") (descr "..."))
    let mut pos = 0;
    while let Some((lib_start, lib_end)) = find_lib_block(content, pos) {
        let block = &content[lib_start..lib_end];

        let nickname = extract_sexp_string(block, "name").unwrap_or_default();
        let uri = extract_sexp_string(block, "uri").unwrap_or_default();
        let lib_type = extract_sexp_string(block, "type").unwrap_or_default();
        let descr = extract_sexp_string(block, "descr").unwrap_or_default();

        libs.push(json!({
            "nickname": nickname,
            "uri": uri,
            "type": lib_type,
            "description": descr
        }));
        pos = lib_end;
    }
    libs
}

/// Extract a quoted string value from `(key "value")` within a block.
fn extract_sexp_string(block: &str, key: &str) -> Option<String> {
    let pat = format!("({} \"", key);
    let start = block.find(&pat)? + pat.len();
    let end = block[start..].find('"')? + start;
    Some(block[start..end].to_string())
}

async fn handle_register_footprint_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let nickname = require_str(args, "nickname").map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let scope = args["scope"].as_str().unwrap_or("project");

    let table_path = if scope == "global" {
        global_fp_lib_table()
    } else if let Some(proj) = args["project"].as_str() {
        PathBuf::from(proj)
            .parent()
            .unwrap_or(Path::new("."))
            .join("fp-lib-table")
    } else {
        return Ok(CallToolResult::error(
            "For project scope, provide 'project' path to .kicad_pro file",
        ));
    };

    register_in_lib_table(
        &table_path,
        nickname,
        lib_path.to_str().unwrap_or(""),
        "KiCad",
    )
    .await?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "nickname": nickname,
            "scope": scope,
            "table": table_path.to_str().unwrap_or("")
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

    if scope == "global" || scope == "all" {
        let table = global_fp_lib_table();
        if table.exists() {
            let mut visited = HashSet::new();
            let libs = parse_lib_table_recursive(&table, "global", &mut visited, 0).await;
            all_libs.extend(libs);
        }
    }

    if (scope == "project" || scope == "all") && args["project"].is_string() {
        let proj = PathBuf::from(args["project"].as_str().unwrap());
        let table = proj.parent().unwrap_or(Path::new(".")).join("fp-lib-table");
        if table.exists() {
            let mut visited = HashSet::new();
            let libs = parse_lib_table_recursive(&table, "project", &mut visited, 0).await;
            all_libs.extend(libs);
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "count": all_libs.len(),
            "libraries": all_libs
        }))
        .unwrap(),
    ))
}

async fn handle_register_symbol_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let nickname = require_str(args, "nickname").map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let scope = args["scope"].as_str().unwrap_or("project");

    let table_path = if scope == "global" {
        global_sym_lib_table()
    } else if let Some(proj) = args["project"].as_str() {
        PathBuf::from(proj)
            .parent()
            .unwrap_or(Path::new("."))
            .join("sym-lib-table")
    } else {
        return Ok(CallToolResult::error(
            "For project scope, provide 'project' path to .kicad_pro file",
        ));
    };

    register_in_lib_table(
        &table_path,
        nickname,
        lib_path.to_str().unwrap_or(""),
        "KiCad",
    )
    .await?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "nickname": nickname,
            "scope": scope,
            "table": table_path.to_str().unwrap_or("")
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

    if scope == "global" || scope == "all" {
        let table = global_sym_lib_table();
        if table.exists() {
            let mut visited = HashSet::new();
            let libs = parse_lib_table_recursive(&table, "global", &mut visited, 0).await;
            all_libs.extend(libs);
        }
    }

    if (scope == "project" || scope == "all") && args["project"].is_string() {
        let proj = PathBuf::from(args["project"].as_str().unwrap());
        let table = proj
            .parent()
            .unwrap_or(Path::new("."))
            .join("sym-lib-table");
        if table.exists() {
            let mut visited = HashSet::new();
            let libs = parse_lib_table_recursive(&table, "project", &mut visited, 0).await;
            all_libs.extend(libs);
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "count": all_libs.len(),
            "libraries": all_libs
        }))
        .unwrap(),
    ))
}

/// Insert a new `(lib ...)` entry into a lib-table file (fp-lib-table or sym-lib-table).
/// Creates the file with minimal scaffolding if it doesn't exist. The root
/// element is chosen from the file name so a fresh `sym-lib-table` does not
/// accidentally get an `(fp_lib_table ...)` header.
async fn register_in_lib_table(
    table_path: &Path,
    nickname: &str,
    uri: &str,
    lib_type: &str,
) -> anyhow::Result<()> {
    let content = if table_path.exists() {
        tokio::fs::read_to_string(table_path).await?
    } else {
        let root = table_root_element(table_path);
        format!("({}\n  (version 7)\n)\n", root)
    };

    if content.contains(&format!("(name \"{}\")", nickname)) {
        return Ok(());
    }

    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let entry = format!(
        "\n  (lib (name \"{}\") (type \"{}\") (uri \"{}\") (options \"\") (descr \"\"))",
        nickname, lib_type, uri
    );

    let new_content = format!("{}{}\n)", &content[..insert_pos], entry);

    if let Some(parent) = table_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(table_path, &new_content)?;
    Ok(())
}

// ─── Symbol library tools ─────────────────────────────────────────────────────

/// Minimal pin geometry for deriving the symbol body.
#[derive(Debug, Clone, Copy)]
struct PinGeom {
    x: f64,
    y: f64,
    angle: f64,
    length: f64,
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

/// Body rectangle `(min_x, min_y, max_x, max_y)` for a symbol: edges that pins
/// attach to pass through those pins' roots (so each pin's far end touches the
/// border and its connection bulb sits outside), and edges with no pins are
/// pushed out by a margin so there is clear spacing beyond the outermost pins.
/// `None` when there are no pins.
fn symbol_body_rect(pins: &[PinGeom]) -> Option<(f64, f64, f64, f64)> {
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

    // Which edges have pins attaching, by orientation (Y up): a pin pointing
    // right (0) sits on the left edge, left (180) on the right edge, up (90) on
    // the bottom edge, down (270) on the top edge.
    let norm = |a: f64| ((a % 360.0) + 360.0) % 360.0;
    let near = |a: f64, t: f64| {
        let d = (norm(a) - t).abs();
        !(1.0..=359.0).contains(&d)
    };
    let (mut has_left, mut has_right, mut has_bottom, mut has_top) = (false, false, false, false);
    for p in pins {
        if near(p.angle, 0.0) {
            has_left = true;
        } else if near(p.angle, 180.0) {
            has_right = true;
        } else if near(p.angle, 90.0) {
            has_bottom = true;
        } else if near(p.angle, 270.0) {
            has_top = true;
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
    Some((min_x, min_y, max_x, max_y))
}

/// Build one unit's inner S-expression — an optional body rectangle (when
/// `with_body`) followed by its pins — and return it with the body rect (used
/// for reference/value placement). Shared by the single- and multi-unit paths.
fn build_symbol_unit(pins_val: &[serde_json::Value], with_body: bool) -> (String, SymbolRect) {
    let mut pins_sexp = String::new();
    let mut pin_geoms: Vec<PinGeom> = Vec::new();
    for pin in pins_val {
        let number = pin["number"].as_str().unwrap_or("1");
        let pin_name = pin["name"].as_str().unwrap_or("~");
        let pin_type = pin["type"].as_str().unwrap_or("passive");
        let x = pin["x"].as_f64().unwrap_or(0.0);
        let y = pin["y"].as_f64().unwrap_or(0.0);
        let angle = pin["angle"].as_f64().unwrap_or(0.0);
        let length = pin["length"].as_f64().unwrap_or(2.54);

        pin_geoms.push(PinGeom {
            x,
            y,
            angle,
            length,
        });
        pins_sexp.push_str(&format!(
            "\n    (pin {} line (at {} {} {})\n      (length {})\n      (name \"{}\" (effects (font (size 1.27 1.27))))\n      (number \"{}\" (effects (font (size 1.27 1.27))))\n    )",
            pin_type, x, y, angle, length, pin_name, number
        ));
    }
    let body = if with_body {
        symbol_body_rect(&pin_geoms)
    } else {
        None
    };
    let body_sexp = match body {
        Some((min_x, min_y, max_x, max_y)) => format!(
            "\n      (rectangle (start {:.4} {:.4}) (end {:.4} {:.4})\n        (stroke (width 0.254) (type default))\n        (fill (type background))\n      )",
            min_x, min_y, max_x, max_y
        ),
        None => String::new(),
    };
    (format!("{}{}", body_sexp, pins_sexp), body)
}

type SymbolRect = Option<(f64, f64, f64, f64)>;

async fn handle_create_symbol(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let name = args["name"].as_str().unwrap_or("Symbol");
    let ref_prefix = args["reference_prefix"].as_str().unwrap_or("U");
    let value_str = args["value"].as_str().unwrap_or(name);
    let show_names = args["show_pin_names"].as_bool().unwrap_or(true);
    let show_numbers = args["show_pin_numbers"].as_bool().unwrap_or(true);

    // Multi-unit when `units` is a non-empty array; otherwise the single-unit
    // `pins` path. Sub-symbols are named NAME_<unit>_1; unit 0 holds items drawn
    // on every unit (common power pins), units 1..N are the individual units.
    let units: Vec<Vec<serde_json::Value>> = args["units"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|u| u["pins"].as_array().cloned().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();
    let power_pins = args["power_pins"].as_array().cloned().unwrap_or_default();

    let mut units_sexp = String::new();
    let unit_count: usize;
    let ref_body: SymbolRect;
    if units.is_empty() {
        // Single unit: body + all pins live in NAME_0_1 (unchanged behavior).
        let pins_val = args["pins"].as_array().cloned().unwrap_or_default();
        let (inner, body) = build_symbol_unit(&pins_val, true);
        units_sexp.push_str(&format!("\n    (symbol \"{}_0_1\"{}\n    )", name, inner));
        unit_count = 1;
        ref_body = body;
    } else {
        // Multi-unit: each signal unit is NAME_1_1..NAME_N_1, and the power
        // pins (if any) become a dedicated FINAL unit rather than being drawn
        // on every unit. KiCAD's own multi-unit parts do this (e.g. 74LS00 has
        // the four gates as units 1..4 and VCC/GND as unit 5). It means the
        // power pins appear on exactly one placed unit instead of on every
        // unit, where each duplicate would otherwise need wiring to pass ERC.
        let mut first_body: SymbolRect = None;
        for (i, unit_pins) in units.iter().enumerate() {
            let (inner, body) = build_symbol_unit(unit_pins, true);
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
        let mut total = units.len();
        if !power_pins.is_empty() {
            let (inner, _) = build_symbol_unit(&power_pins, true);
            total += 1;
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

    let numbers_vis = if show_numbers { "" } else { " hide" };
    let names_vis = if show_names { "" } else { " hide" };

    let symbol_sexp = format!(
        "\n  (symbol \"{}\"\n    (pin_numbers{})\n    (pin_names (offset 1.016){})\n    (in_bom yes)\n    (on_board yes)\n    (property \"Reference\" \"{}\" (at 0 {:.4} 0) (effects (font (size 1.27 1.27))))\n    (property \"Value\" \"{}\" (at 0 {:.4} 0) (effects (font (size 1.27 1.27))))\n    (property \"Footprint\" \"\" (at 0 0 0) (effects (font (size 1.27 1.27)) hide))\n    (property \"Datasheet\" \"~\" (at 0 0 0) (effects (font (size 1.27 1.27)) hide)){}\n  )",
        name, numbers_vis, names_vis, ref_prefix, ref_y, value_str, value_y, units_sexp
    );

    // If file doesn't exist, create scaffold
    let content = if lib_path.exists() {
        tokio::fs::read_to_string(&lib_path).await?
    } else {
        "(kicad_symbol_lib\n  (version 20240108)\n  (generator \"konnect\")\n)\n".to_string()
    };

    // Insert before closing paren of root expression
    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = format!("{}{}\n)", &content[..insert_pos], symbol_sexp);

    if let Some(parent) = lib_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(&lib_path, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "symbol": name,
            "library": lib_path.to_str().unwrap_or(""),
            "unit_count": unit_count,
            "power_pin_count": power_pins.len()
        }))
        .unwrap(),
    ))
}

async fn handle_delete_symbol(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let symbol_name = require_str(args, "symbol_name").map_err(|e| anyhow::anyhow!("{:?}", e))?;

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
        serde_json::to_string_pretty(&json!({
            "success": true,
            "deleted": symbol_name
        }))
        .unwrap(),
    ))
}

async fn handle_list_symbols_in_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_path = get_path(args, "library_path")?;
    let content = tokio::fs::read_to_string(&lib_path).await?;

    // Match all top-level symbol names: `  (symbol "NAME"` at depth 1
    let mut symbols = Vec::new();
    let mut search = content.as_str();
    while let Some(pos) = search.find("\n  (symbol \"") {
        let after = &search[pos + 13..]; // skip `\n  (symbol "`
        if let Some(end) = after.find('"') {
            let sym_name = &after[..end];
            // Exclude sub-units like "NAME_0_1"
            if !sym_name.contains('_') || {
                // Allow symbols whose name contains underscores but are NOT sub-unit patterns
                let parts: Vec<&str> = sym_name.rsplitn(3, '_').collect();
                parts.len() < 3 || parts[0].parse::<u32>().is_err()
            } {
                symbols.push(sym_name.to_string());
            }
            search = &search[pos + 1..];
        } else {
            break;
        }
    }
    symbols.sort();
    symbols.dedup();

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "library": lib_path.to_str().unwrap_or(""),
            "count": symbols.len(),
            "symbols": symbols
        }))
        .unwrap(),
    ))
}

async fn handle_search_symbols(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    // Walk all global symbol libraries
    let table = global_sym_lib_table();
    let mut results = Vec::new();

    if table.exists() {
        let mut visited = HashSet::new();
        let libs = parse_lib_table_recursive(&table, "global", &mut visited, 0).await;

        'outer: for lib in &libs {
            let uri = lib["uri"].as_str().unwrap_or("");
            let uri = expand_kicad_uri(uri);
            if uri.is_empty() {
                continue;
            }
            let lib_path = PathBuf::from(&uri);
            if !lib_path.exists() {
                continue;
            }
            let lib_content = match tokio::fs::read_to_string(&lib_path).await {
                Ok(c) => c,
                Err(_) => continue,
            };

            let nickname = lib["nickname"].as_str().unwrap_or("");
            let mut search = lib_content.as_str();
            while let Some(pos) = search.find("\n  (symbol \"") {
                let after = &search[pos + 13..];
                if let Some(end) = after.find('"') {
                    let sym_name = &after[..end];
                    if sym_name.to_lowercase().contains(&query) && !sym_name.contains('_') {
                        results.push(json!({
                            "library": nickname,
                            "name": sym_name,
                            "id": format!("{}:{}", nickname, sym_name)
                        }));
                        if results.len() >= limit {
                            break 'outer;
                        }
                    }
                    search = &search[pos + 1..];
                } else {
                    break;
                }
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
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
    let library_path_str =
        require_str(args, "library_path").map_err(|e| anyhow::anyhow!("{:?}", e))?;
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
        serde_json::to_string_pretty(&json!({
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
    let fp_path_str =
        require_str(args, "footprint_path").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    // Resolve "Library:Footprint" form via global fp-lib-table
    let path =
        if fp_path_str.contains(':') && !fp_path_str.contains('/') && !fp_path_str.contains('\\') {
            let parts: Vec<&str> = fp_path_str.splitn(2, ':').collect();
            let (nick, fp_name) = (parts[0], parts[1]);
            let table = global_fp_lib_table();
            if table.exists() {
                let mut visited = HashSet::new();
                let libs = parse_lib_table_recursive(&table, "global", &mut visited, 0).await;
                if let Some(lib) = libs.iter().find(|l| l["nickname"].as_str() == Some(nick)) {
                    let uri = lib["uri"].as_str().unwrap_or("");
                    let uri = expand_kicad_uri(uri);
                    PathBuf::from(uri).join(format!("{}.kicad_mod", fp_name))
                } else {
                    return Ok(CallToolResult::error(format!(
                        "Library '{}' not found in fp-lib-table",
                        nick
                    )));
                }
            } else {
                return Ok(CallToolResult::error("Global fp-lib-table not found"));
            }
        } else {
            PathBuf::from(fp_path_str)
        };

    let content = tokio::fs::read_to_string(&path).await?;

    // Parse basic info: description, pads
    let description = extract_sexp_string(&content, "descr").unwrap_or_default();
    let fp_name = extract_sexp_string(&content, "footprint").unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });

    let mut pad_count = 0usize;
    let mut scan_pos = 0usize;
    while let Some((_, end)) = find_pad_block(&content, scan_pos) {
        pad_count += 1;
        scan_pos = end;
    }

    // Extract courtyard bbox (gr_poly on B.CrtYd or F.CrtYd) — simplified
    let has_courtyard = content.contains("B.CrtYd") || content.contains("F.CrtYd");
    let has_3d = content.contains("(model ");

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "name": fp_name,
            "description": description,
            "pad_count": pad_count,
            "has_courtyard": has_courtyard,
            "has_3d_model": has_3d,
            "path": path.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

// ─── search_footprints (moved from verification toolset) ─────────────────────

async fn handle_search_footprints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    // Walk global fp-lib-table
    let fp_lib_table_path = super::kicad_config_dir().join("fp-lib-table");

    let mut results = Vec::new();

    if fp_lib_table_path.exists() {
        let mut visited = HashSet::new();
        let libs =
            parse_lib_table_recursive(&fp_lib_table_path, "global", &mut visited, 0).await;

        'outer: for lib in &libs {
            let nickname = lib["nickname"].as_str().unwrap_or_default();
            let uri = lib["uri"].as_str().unwrap_or_default();
            let uri = expand_kicad_uri(uri);
            if uri.is_empty() {
                continue;
            }
            let dir = PathBuf::from(&uri);
            if dir.is_dir() {
                if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
                    while let Ok(Some(entry)) = rd.next_entry().await {
                        let fname = entry.file_name();
                        let fname_str = fname.to_string_lossy();
                        if fname_str.ends_with(".kicad_mod") {
                            let fp_name = fname_str.trim_end_matches(".kicad_mod");
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
                }
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
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
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let lib_id = require_str(args, "lib_id").map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Ok(CallToolResult::error(
            "lib_id must be in 'Library:Symbol' format (e.g. 'Device:R')",
        ));
    }
    let (lib_nick, sym_name) = (parts[0], parts[1]);

    // Look up library path from global sym-lib-table
    let sym_lib_table_path = super::kicad_config_dir().join("sym-lib-table");

    let lib_path = if sym_lib_table_path.exists() {
        let tc = tokio::fs::read_to_string(&sym_lib_table_path).await?;
        // Parse lib entries: (lib (name "NICK") ... (uri "PATH") ...)
        let pat = format!(r#"(name "{}")"#, lib_nick);
        if let Some(block_start) = tc.find(&pat) {
            let block_end = tc[block_start..]
                .find(")\n")
                .map(|i| block_start + i + 2)
                .unwrap_or(tc.len());
            let block = &tc[block_start..block_end];
            if let Some(uri_pos) = block.find("(uri \"") {
                let after = &block[uri_pos + 6..];
                after.find('"').map(|end| PathBuf::from(&after[..end]))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let lib_path = match lib_path {
        Some(p) if !p.to_str().unwrap_or("").starts_with("${") => p,
        _ => {
            return Ok(CallToolResult::error(format!(
                "Library '{}' not found or path uses unresolved env var",
                lib_nick
            )));
        }
    };

    let content = tokio::fs::read_to_string(&lib_path).await?;

    let (sym_start, sym_end) = find_symbol_block(&content, sym_name, 0).ok_or_else(|| {
        anyhow::anyhow!("Symbol '{}' not found in library '{}'", sym_name, lib_nick)
    })?;
    let sym_block = &content[sym_start..sym_end];

    let mut pins = Vec::new();
    let mut search_from = 0usize;
    while let Some((pin_start, pin_end)) = find_prefixed_block(sym_block, b"(pin ", search_from) {
        let inner = &sym_block[pin_start..pin_end];
        let pin_type = inner.split_whitespace().nth(1).unwrap_or("").to_string();
        let pin_name = extract_sexp_string(inner, "name").unwrap_or_default();
        let pin_num = extract_sexp_string(inner, "number").unwrap_or_default();
        let (px, py) = extract_at_xy(inner).unwrap_or((0.0, 0.0));
        pins.push(json!({
            "number": pin_num,
            "name": pin_name,
            "type": pin_type,
            "x": px,
            "y": py
        }));
        search_from = pin_end;
    }

    // Extract properties
    let mut properties = serde_json::Map::new();
    let mut search_from = 0usize;
    while let Some((prop_start, prop_end)) =
        find_prefixed_block(sym_block, b"(property \"", search_from)
    {
        let block = &sym_block[prop_start..prop_end];
        let inner = &block["(property \"".len()..];
        if let Some(key_end) = inner.find('"') {
            let key = &inner[..key_end];
            let val_start = inner[key_end + 1..]
                .find('"')
                .map(|i| key_end + 1 + i + 1)
                .unwrap_or(0);
            let val_end = inner[val_start..]
                .find('"')
                .map(|i| val_start + i)
                .unwrap_or(0);
            if val_end > val_start {
                properties.insert(
                    key.to_string(),
                    json!(inner[val_start..val_end].to_string()),
                );
            }
        }
        search_from = prop_end;
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
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

fn extract_at_xy(block: &str) -> Option<(f64, f64)> {
    let pos = block.find("(at ")?;
    let after = &block[pos + 4..];
    let end = after.find(')')?;
    let parts: Vec<&str> = after[..end].split_whitespace().collect();
    let x = parts.first()?.parse::<f64>().ok()?;
    let y = parts.get(1)?.parse::<f64>().ok()?;
    Some((x, y))
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
            },
            Arc::new(ToolRouter::new()),
        )
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

    #[test]
    fn symbol_body_rect_touches_side_pins_and_spaces_the_ends() {
        // Three pins on the left (point right), two on the right (point left).
        let pins = vec![
            PinGeom {
                x: -10.16,
                y: 2.54,
                angle: 0.0,
                length: 2.54,
            },
            PinGeom {
                x: -10.16,
                y: 0.0,
                angle: 0.0,
                length: 2.54,
            },
            PinGeom {
                x: -10.16,
                y: -2.54,
                angle: 0.0,
                length: 2.54,
            },
            PinGeom {
                x: 10.16,
                y: 2.54,
                angle: 180.0,
                length: 2.54,
            },
            PinGeom {
                x: 10.16,
                y: -2.54,
                angle: 180.0,
                length: 2.54,
            },
        ];
        let (min_x, min_y, max_x, max_y) = symbol_body_rect(&pins).unwrap();
        // Left/right edges pass through the pin roots (pins touch the border).
        assert!((min_x - -7.62).abs() < 1e-9, "left edge {min_x}");
        assert!((max_x - 7.62).abs() < 1e-9, "right edge {max_x}");
        // Connection bulbs at x = ±10.16 stay outside the body.
        assert!(min_x > -10.16 && max_x < 10.16);
        // Top/bottom edges have no pins → spacing beyond the outermost pins.
        assert!(max_y >= 2.54 + 2.5, "top spacing {max_y}");
        assert!(min_y <= -2.54 - 2.5, "bottom spacing {min_y}");
        assert!(symbol_body_rect(&[]).is_none());
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
        assert!(c.contains("(pin_numbers)"), "pin numbers should be shown");
        assert!(!c.contains("(pin_numbers hide)"));
        assert!(
            konnect_sexp::parser::parse_sexp(&c).is_ok(),
            "generated symbol doesn't parse"
        );
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
}
