//! The extent of a board's items, measured the way KiCad measures them (#688).
//!
//! KiCad answers `GetBoundingBox` for a live board item by item, in
//! `BBM_ITEM_ONLY` mode. This module computes the same boxes from a saved
//! `.kicad_pcb`, so that a read of the saved board means the same thing as a
//! read of the live one. Each rule below reproduces KiCad 10's own code and was
//! checked item by item against KiCad 10.0.5's live answers on its demo boards:
//! the goldens under `tests/fixtures/board_bounds/` are those answers.
//!
//! - **Tracks:** the segment's box, inflated by half the width.
//! - **Arc tracks:** the arc's exact extent inflated by the **full** width.
//!   KiCad inflates the arc's effective shape, which already carries half the
//!   width, by half the width again (`PCB_TRACK::GetBoundingBox`).
//! - **Vias:** the centre inflated by half the largest diameter on any layer.
//! - **Graphic shapes:** the geometry's box inflated by half the stroke width.
//! - **Text boxes:** the same, on the box's corners, as `start`/`end` or as the
//!   four `pts` KiCad writes for a turned one. The stroke counts with or
//!   without a drawn border, and text that overflows the box does not.
//! - **Zones:** the outline, arcs included. Fill is not part of the box.
//! - **Footprints** (`FOOTPRINT::GetBoundingBox` without text): the anchor
//!   inflated by 0.25 mm, merged with every pad, zone and point, and with every
//!   graphic and text box except those on `Cmts.User`, `Dwgs.User`,
//!   `Eco1.User`, `Eco2.User` or a private layer. Text, fields and dimensions
//!   are left out.
//!
//! What a saved file cannot be measured against without KiCad's own font and
//! layout code — text, dimensions, and the classes no KiCad sample exercises —
//! is counted in [`BoardItemBounds::not_measured`] rather than guessed at or
//! dropped without a word.

use crate::parser::SexpNode;
use std::collections::BTreeMap;

/// `(min_x, min_y, max_x, max_y)` in millimetres.
pub type Bbox = (f64, f64, f64, f64);

/// The kinds of board item this module measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemClass {
    Footprint,
    Shape,
    Track,
    Arc,
    Via,
    Zone,
    TextBox,
}

impl ItemClass {
    /// The name a response reports this class under.
    pub fn key(self) -> &'static str {
        match self {
            ItemClass::Footprint => "footprints",
            ItemClass::Shape => "shapes",
            ItemClass::Track => "tracks",
            ItemClass::Arc => "arcs",
            ItemClass::Via => "vias",
            ItemClass::Zone => "zones",
            ItemClass::TextBox => "text_boxes",
        }
    }
}

/// One measured board item.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemBox {
    pub class: ItemClass,
    /// The item's `(uuid …)`, which is also its KiCad KIID.
    pub uuid: Option<String>,
    pub bbox: Bbox,
}

/// Every top-level item of a board, measured or accounted for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoardItemBounds {
    /// The items that were measured.
    pub items: Vec<ItemBox>,
    /// Items of a measured class that could not be read (a missing or
    /// non-finite coordinate), counted rather than guessed at.
    pub skipped: usize,
    /// What the board carries that this read did not measure, by name.
    pub not_measured: BTreeMap<&'static str, usize>,
}

impl BoardItemBounds {
    /// The union of every measured item; `None` when nothing was measured.
    pub fn union(&self) -> Option<Bbox> {
        self.items
            .iter()
            .fold(None, |acc, item| Some(merge(acc, item.bbox)))
    }

    /// How many items of each class were measured, by [`ItemClass::key`].
    pub fn measured(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for item in &self.items {
            *counts.entry(item.class.key()).or_insert(0) += 1;
        }
        counts
    }
}

/// Board-level items whose extent needs KiCad's own font or layout code, or
/// that no KiCad sample exercises, by the name a response reports them under.
const NOT_MEASURED: [(&str, &str); 6] = [
    ("gr_text", "text"),
    ("dimension", "dimensions"),
    ("barcode", "barcodes"),
    ("image", "reference_images"),
    ("table", "tables"),
    ("target", "targets"),
];

/// Measure every top-level item of `tree`, a parsed `(kicad_pcb …)`.
pub fn board_item_bounds(tree: &SexpNode) -> BoardItemBounds {
    let mut out = BoardItemBounds::default();
    for child in tree.children().unwrap_or(&[]) {
        let Some(head) = child.head() else { continue };
        let measured = match head {
            "footprint" => Some((ItemClass::Footprint, footprint_bbox(child, &mut out))),
            "gr_line" | "gr_rect" | "gr_circle" | "gr_arc" | "gr_poly" | "gr_curve" => {
                Some((ItemClass::Shape, shape_bbox(child, None)))
            }
            "segment" => Some((ItemClass::Track, track_bbox(child))),
            "arc" => Some((ItemClass::Arc, arc_track_bbox(child))),
            "via" => Some((ItemClass::Via, via_bbox(child))),
            "zone" => Some((ItemClass::Zone, zone_bbox(child))),
            "gr_text_box" => Some((ItemClass::TextBox, shape_bbox(child, None))),
            _ => None,
        };
        match measured {
            Some((class, Some(bbox))) => out.items.push(ItemBox {
                class,
                uuid: child.find_str("uuid").map(str::to_owned),
                bbox,
            }),
            Some((_, None)) => out.skipped += 1,
            None => {
                if let Some((_, name)) = NOT_MEASURED.iter().find(|(tag, _)| *tag == head) {
                    *out.not_measured.entry(name).or_insert(0) += 1;
                }
            }
        }
    }
    out
}

// ─── Items ───────────────────────────────────────────────────────────────────

fn track_bbox(node: &SexpNode) -> Option<Bbox> {
    let (x1, y1) = point(node, "start")?;
    let (x2, y2) = point(node, "end")?;
    let width = finite(node.find_f64("width")?)?;
    Some(inflate(
        (x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)),
        width / 2.0,
    ))
}

fn arc_track_bbox(node: &SexpNode) -> Option<Bbox> {
    let arc = crate::geometry::arc_bbox(
        point(node, "start")?,
        point(node, "mid")?,
        point(node, "end")?,
    );
    // The full width, not half: see the module documentation.
    Some(inflate(arc, finite(node.find_f64("width")?)?))
}

fn via_bbox(node: &SexpNode) -> Option<Bbox> {
    let (x, y) = point(node, "at")?;
    // A via with per-layer sizes is as wide as its widest layer.
    let mut diameter = finite(node.find_f64("size")?)?;
    if let Some(stack) = node.find("padstack") {
        for layer in stack.find_all("layer") {
            if let Some(size) = layer.find("size") {
                diameter = diameter.max(finite(size.get_f64(1)?)?);
            }
        }
    }
    Some(inflate((x, y, x, y), diameter / 2.0))
}

/// A zone's outline, arcs included. KiCad stores a footprint's zones in board
/// coordinates, so the same reading serves both.
fn zone_bbox(node: &SexpNode) -> Option<Bbox> {
    let mut acc = None;
    for polygon in node.find_all("polygon") {
        acc = Some(merge(acc, pts_bbox(polygon.find("pts")?, None)?));
    }
    acc
}

/// A `gr_*`, `fp_*` or pad-primitive graphic: its geometry inflated by half
/// its stroke width. `to_board` places footprint- or pad-local coordinates.
fn shape_bbox(node: &SexpNode, to_board: Option<&dyn Fn(f64, f64) -> (f64, f64)>) -> Option<Bbox> {
    let place = |p: (f64, f64)| match to_board {
        Some(f) => f(p.0, p.1),
        None => p,
    };
    let head = node.head()?;
    let kind = head.split_once('_').map_or(head, |(_, kind)| kind);
    let geometry = match kind {
        "line" => hull([place(point(node, "start")?), place(point(node, "end")?)]),
        "rect" => {
            let (x1, y1) = point(node, "start")?;
            let (x2, y2) = point(node, "end")?;
            hull([(x1, y1), (x2, y1), (x2, y2), (x1, y2)].map(place))
        }
        "circle" => {
            let (cx, cy) = place(point(node, "center")?);
            let (ex, ey) = place(point(node, "end")?);
            let r = (ex - cx).hypot(ey - cy);
            (cx - r, cy - r, cx + r, cy + r)
        }
        "arc" => crate::geometry::arc_bbox(
            place(point(node, "start")?),
            place(point(node, "mid")?),
            place(point(node, "end")?),
        ),
        "poly" | "curve" => pts_bbox(node.find("pts")?, to_board)?,
        "text_box" => match node.find("pts") {
            Some(pts) => pts_bbox(pts, to_board)?,
            None => {
                let (x1, y1) = point(node, "start")?;
                let (x2, y2) = point(node, "end")?;
                hull([(x1, y1), (x2, y1), (x2, y2), (x1, y2)].map(place))
            }
        },
        _ => return None,
    };
    // KiCad writes a hairline as a tiny negative width; its integer half is 0.
    Some(inflate(geometry, (stroke_width(node)? / 2.0).max(0.0)))
}

/// The stroke width, from `(stroke (width w))` or the older bare `(width w)`.
/// A shape that states neither has none.
fn stroke_width(node: &SexpNode) -> Option<f64> {
    match node
        .find("stroke")
        .and_then(|stroke| stroke.find("width"))
        .or_else(|| node.find("width"))
    {
        Some(width) => finite(width.get_f64(1)?),
        None => Some(0.0),
    }
}

/// The box of a `(pts …)` list: its vertices, and the exact extent of any
/// `(arc …)` segment in it.
///
/// A Bézier `gr_curve` stores its control points here. Their hull contains the
/// curve, so it bounds it from outside; KiCad measures its flattened curve,
/// which no KiCad sample exercises, so this is the one shape rule that is a
/// superset rather than a reproduction.
fn pts_bbox(pts: &SexpNode, to_board: Option<&dyn Fn(f64, f64) -> (f64, f64)>) -> Option<Bbox> {
    let place = |p: (f64, f64)| match to_board {
        Some(f) => f(p.0, p.1),
        None => p,
    };
    let mut acc = None;
    for element in pts.children()?.iter().skip(1) {
        let bb = match element.head() {
            Some("xy") => {
                let p = place(pair(element)?);
                (p.0, p.1, p.0, p.1)
            }
            Some("arc") => crate::geometry::arc_bbox(
                place(point(element, "start")?),
                place(point(element, "mid")?),
                place(point(element, "end")?),
            ),
            _ => return None,
        };
        acc = Some(merge(acc, bb));
    }
    acc
}

// ─── Footprints and pads ─────────────────────────────────────────────────────

/// Layers whose footprint graphics KiCad leaves out of a footprint's box.
const ANNOTATION_LAYERS: [&str; 4] = ["Cmts.User", "Dwgs.User", "Eco1.User", "Eco2.User"];

/// A footprint's box as `FOOTPRINT::GetBoundingBox(false)` computes it.
///
/// What it cannot reproduce is counted on `out`: a footprint text box, a pad
/// with per-layer shapes beyond its default one, and a footprint with no
/// graphic, pad or zone at all (KiCad then measures its text instead).
fn footprint_bbox(fp: &SexpNode, out: &mut BoardItemBounds) -> Option<Bbox> {
    let at = fp.find("at")?;
    let (fx, fy) = (finite(at.get_f64(1)?)?, finite(at.get_f64(2)?)?);
    let rot = optional_finite_child(at, 3, 0.0)?;
    let to_board = move |x: f64, y: f64| crate::geometry::transform_pad(x, y, fx, fy, rot);

    let private: Vec<&str> = fp
        .find("private_layers")
        .map(|layers| {
            layers
                .children()
                .unwrap_or(&[])
                .iter()
                .skip(1)
                .filter_map(SexpNode::as_str)
                .collect()
        })
        .unwrap_or_default();

    let mut bbox = inflate((fx, fy, fx, fy), 0.25);
    let mut geometry = false;
    for child in fp.children().unwrap_or(&[]) {
        let Some(head) = child.head() else { continue };
        let bb = match head {
            "fp_line" | "fp_rect" | "fp_circle" | "fp_arc" | "fp_poly" | "fp_curve"
            | "fp_text_box" => {
                geometry = true;
                let layer = child.find_str("layer");
                if layer.is_some_and(|l| ANNOTATION_LAYERS.contains(&l) || private.contains(&l)) {
                    continue;
                }
                shape_bbox(child, Some(&to_board))?
            }
            "pad" => {
                geometry = true;
                if child
                    .find("padstack")
                    .is_some_and(|s| s.find("layer").is_some())
                {
                    *out.not_measured.entry("pad_layer_shapes").or_insert(0) += 1;
                }
                pad_bbox(child, fx, fy, rot)?
            }
            "zone" => {
                geometry = true;
                zone_bbox(child)?
            }
            // KiCad writes a footprint's points in board coordinates, as it
            // does its zones: measured against KiCad on pic_programmer, where
            // reading them as footprint-local moves seven footprints by metres.
            "point" => {
                let (x, y) = point(child, "at")?;
                let size = match child.find("size") {
                    None => 1.0,
                    Some(size) => finite(size.get_f64(1)?)?,
                };
                inflate((x, y, x, y), size / 2.0)
            }
            // Drawings KiCad leaves out of the box, but whose presence still
            // keeps it from falling back to measuring the footprint's text.
            "fp_text" | "dimension" | "image" | "barcode" => {
                geometry = true;
                continue;
            }
            _ => continue,
        };
        bbox = merge(Some(bbox), bb);
    }
    if !geometry {
        *out.not_measured.entry("text_only_footprints").or_insert(0) += 1;
    }
    Some(bbox)
}

/// A pad's box in board coordinates.
///
/// The pad's `(at x y angle)` places its centre in the footprint frame, and its
/// angle is already the board-space angle: the file stores pad rotation with
/// the footprint's included.
pub fn pad_bbox(pad: &SexpNode, fx: f64, fy: f64, footprint_rot: f64) -> Option<Bbox> {
    let at = pad.find("at")?;
    let (px, py) = (finite(at.get_f64(1)?)?, finite(at.get_f64(2)?)?);
    let angle = optional_finite_child(at, 3, 0.0)?;
    let (cx, cy) = crate::geometry::transform_pad(px, py, fx, fy, footprint_rot);
    let size = pad.find("size")?;
    let (w, h) = (finite(size.get_f64(1)?)?, finite(size.get_f64(2)?)?);
    let (ox, oy) = match pad.find("drill").and_then(|drill| drill.find("offset")) {
        Some(offset) => pair(offset)?,
        None => (0.0, 0.0),
    };
    // A point in the pad's own frame, shape offset included, on the board.
    let place = move |x: f64, y: f64| crate::geometry::transform_pad(x + ox, y + oy, cx, cy, angle);
    let rect = [
        (-w / 2.0, -h / 2.0),
        (w / 2.0, -h / 2.0),
        (w / 2.0, h / 2.0),
        (-w / 2.0, h / 2.0),
    ];
    let circle = |(x, y): (f64, f64), r: f64| inflate((x, y, x, y), r);

    match pad.get(3)?.as_str()? {
        "circle" => Some(circle(place(0.0, 0.0), w / 2.0)),
        "rect" => Some(hull(rect.map(|(x, y)| place(x, y)))),
        "oval" => {
            let r = w.min(h) / 2.0;
            let (a, b) = if w >= h {
                ((-(w / 2.0 - r), 0.0), (w / 2.0 - r, 0.0))
            } else {
                ((0.0, -(h / 2.0 - r)), (0.0, h / 2.0 - r))
            };
            Some(merge(
                Some(circle(place(a.0, a.1), r)),
                circle(place(b.0, b.1), r),
            ))
        }
        "roundrect" => {
            let ratio = match pad.find("roundrect_rratio") {
                None => 0.25,
                Some(node) => finite(node.get_f64(1)?)?,
            };
            let r = ratio * w.min(h);
            let mut acc = None;
            for (x, y) in rect {
                let corner = (x - x.signum() * r, y - y.signum() * r);
                acc = Some(merge(acc, circle(place(corner.0, corner.1), r)));
            }
            acc
        }
        "trapezoid" => {
            let (dx, dy) = match pad.find("rect_delta") {
                Some(delta) => pair(delta)?,
                None => (0.0, 0.0),
            };
            let corners = [
                (-w / 2.0 - dy / 2.0, -h / 2.0 + dx / 2.0),
                (w / 2.0 + dy / 2.0, -h / 2.0 - dx / 2.0),
                (w / 2.0 - dy / 2.0, h / 2.0 + dx / 2.0),
                (-w / 2.0 + dy / 2.0, h / 2.0 - dx / 2.0),
            ];
            Some(hull(corners.map(|(x, y)| place(x, y))))
        }
        "custom" => {
            // The anchor shape, then every primitive in the pad's frame.
            let anchor = pad.find("options").and_then(|o| o.find_str("anchor"));
            let mut acc = Some(match anchor {
                Some("rect") => hull(rect.map(|(x, y)| place(x, y))),
                _ => circle(place(0.0, 0.0), w / 2.0),
            });
            if let Some(primitives) = pad.find("primitives") {
                for primitive in primitives.children()?.iter().skip(1) {
                    acc = Some(merge(acc, shape_bbox(primitive, Some(&place))?));
                }
            }
            acc
        }
        _ => None,
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

/// Read an optional numeric child without treating malformed present data as
/// omitted. KiCad omits optional angles when they are zero; a present value
/// that is not finite numeric data makes the containing item unmeasurable.
fn optional_finite_child(node: &SexpNode, index: usize, default: f64) -> Option<f64> {
    match node.get(index) {
        None => Some(default),
        Some(value) => finite(value.as_str()?.parse().ok()?),
    }
}

fn pair(node: &SexpNode) -> Option<(f64, f64)> {
    Some((finite(node.get_f64(1)?)?, finite(node.get_f64(2)?)?))
}

fn point(node: &SexpNode, tag: &str) -> Option<(f64, f64)> {
    pair(node.find(tag)?)
}

fn inflate((x0, y0, x1, y1): Bbox, d: f64) -> Bbox {
    (x0 - d, y0 - d, x1 + d, y1 + d)
}

fn merge(acc: Option<Bbox>, bb: Bbox) -> Bbox {
    match acc {
        None => bb,
        Some((x0, y0, x1, y1)) => (x0.min(bb.0), y0.min(bb.1), x1.max(bb.2), y1.max(bb.3)),
    }
}

fn hull<const N: usize>(points: [(f64, f64); N]) -> Bbox {
    points
        .iter()
        .fold(None, |acc, &(x, y)| Some(merge(acc, (x, y, x, y))))
        .expect("at least one point")
}
