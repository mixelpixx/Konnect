//! Structural queries over a `.kicad_pcb` tree.
//!
//! These exist because `SexpNode::find_all` is **direct children only**, which
//! is easy to forget: `footprint`, `segment`, `via` and `net` *are* direct
//! children of `(kicad_pcb …)`, so `tree.find_all("footprint")` is right — and
//! `pad` is not, so `tree.find_all("pad")` silently returns 0 on every board
//! ever written. Design review reported `pads: 0` for the whole life of its
//! coverage block because of exactly that (#246).
//!
//! # Malformed-item policy
//!
//! A copper item missing a load-bearing field (a segment without an endpoint,
//! a via without a drill) is **skipped and counted**, never fabricated: a
//! coordinate defaulted to 0.0 places phantom copper at the board origin and
//! everything downstream — placement, test-point selection, voltage drop —
//! computes confidently from it. Every scan therefore returns a [`Scan`]
//! carrying the parsed items *and* how many matching nodes were dropped, so a
//! caller can refuse to trust a board that lost items rather than never
//! finding out.
//!
//! # Net identity
//!
//! Items name their net two ways depending on the file format (see
//! [`crate::net`]): KiCad 10 writes `(net "GND")` in place; KiCad ≤ 9 writes
//! `(net 2)` and declares `(net 2 "GND")` once at top level. The scans here
//! resolve both to the net **name**, using the board's own top-level table for
//! the numeric form, so the same physical net gets the same key whichever
//! format wrote the file. The unconnected pseudo-net (net 0 / the empty name)
//! resolves to `None` — copper with no net is a real state, not a net called
//! `""`.

use crate::net;
use crate::parser::SexpNode;
use std::collections::{BTreeMap, HashMap};

/// Every footprint on a board, in file order.
///
/// Footprints are direct children of `(kicad_pcb …)`, so this is a thin
/// wrapper — it exists so pad counting has an obvious partner and callers stop
/// reaching for `find_all` directly on the root.
pub fn footprints(tree: &SexpNode) -> Vec<&SexpNode> {
    tree.find_all("footprint")
}

/// Every pad on the board, across all footprints.
///
/// Pads live one level down, inside each `(footprint …)`. Call this rather
/// than `tree.find_all("pad")`, which cannot ever match.
pub fn pads(tree: &SexpNode) -> Vec<&SexpNode> {
    footprints(tree)
        .into_iter()
        .flat_map(|fp| fp.find_all("pad"))
        .collect()
}

/// How many pads the board has. Zero from a board that has footprints means
/// something is wrong with the board or the parse — it is not a normal state.
pub fn count_pads(tree: &SexpNode) -> usize {
    pads(tree).len()
}

/// The result of a lossy structural scan: everything that parsed, plus how
/// many nodes matched the tag but were too malformed to represent.
///
/// `skipped` is the module's alternative to silently dropping or — worse —
/// zero-filling broken items (see the module docs). `skipped > 0` on a
/// KiCad-authored board means the file or the parser is wrong; callers that
/// feed analysis tools should surface it, not ignore it.
#[derive(Debug, Clone, PartialEq)]
pub struct Scan<T> {
    /// Items that carried every load-bearing field, in file order.
    pub items: Vec<T>,
    /// Nodes with the right tag that were dropped for missing or non-finite
    /// load-bearing fields.
    pub skipped: usize,
}

/// One `(segment …)` — a straight copper track.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    /// Track width in mm. Guaranteed finite and > 0 — a segment claiming
    /// otherwise is skipped, because zero-width copper poisons every
    /// resistance/current computation built on it.
    pub width: f64,
    pub layer: String,
    /// Resolved net name (see the module docs); `None` is the unconnected
    /// pseudo-net.
    pub net: Option<String>,
    pub uuid: Option<String>,
}

/// One `(via …)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Via {
    pub x: f64,
    pub y: f64,
    /// Pad (annular) diameter in mm; finite and > 0.
    pub size: f64,
    /// Drill diameter in mm; finite and > 0. KiCad always writes it, so a via
    /// without one is malformed — the netclass default it once implied cannot
    /// be recovered from the board file alone.
    pub drill: f64,
    /// The copper layers the via spans, e.g. `["F.Cu", "B.Cu"]`.
    pub layers: Vec<String>,
    /// Resolved net name; `None` is the unconnected pseudo-net.
    pub net: Option<String>,
    pub uuid: Option<String>,
}

/// One authored element in a zone's ordered `(polygon (pts …))` path.
///
/// KiCad writes straight corners as `(xy …)` and curved portions as exact
/// three-point `(arc (start …) (mid …) (end …))` forms. Keeping that
/// distinction avoids replacing authored curves with invented straight
/// segments.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZoneOutlineElement {
    Point((f64, f64)),
    Arc {
        start: (f64, f64),
        mid: (f64, f64),
        end: (f64, f64),
    },
}

/// The authored outline of one `(zone …)` — the polygon the user drew, not
/// the filled copper (`filled_polygon`), which refills on every pour and can
/// be absent entirely on an unfilled zone.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneOutline {
    /// Resolved net name; `None` covers both the unconnected pseudo-net and
    /// net-less zones (keepouts).
    pub net: Option<String>,
    /// Layers the zone lives on: KiCad 10 writes `(layers …)` (plural), older
    /// single-layer zones `(layer …)`. Both shapes land here.
    pub layers: Vec<String>,
    /// Straight `(xy …)` vertices from the zone's first
    /// `(polygon (pts …))`, in file order. This retains the original
    /// point-only view; it is empty for an arc-only outline. Use
    /// [`Self::elements`] for the complete geometry.
    pub points: Vec<(f64, f64)>,
    /// Lossless ordered outline geometry. Unknown or incomplete elements make
    /// the zone unreadable rather than being silently dropped.
    pub elements: Vec<ZoneOutlineElement>,
}

/// Every routed track segment on the board.
///
/// Segments are direct children of `(kicad_pcb …)` in every format KiCad has
/// written — verified against the KiCad 9 and 10 demo corpus, not assumed
/// (#246). Curved tracks (`(arc …)`) are a different node and are *not*
/// included here.
pub fn tracks(tree: &SexpNode) -> Scan<Track> {
    let table = top_level_net_table(tree);
    let mut items = Vec::new();
    let mut skipped = 0usize;
    for seg in tree.find_all("segment") {
        let parsed = (|| {
            let (x1, y1) = point(seg, "start")?;
            let (x2, y2) = point(seg, "end")?;
            let width = seg
                .find_f64("width")
                .filter(|w| w.is_finite() && *w > 0.0)?;
            let layer = seg.find_str("layer")?.to_string();
            Some(Track {
                x1,
                y1,
                x2,
                y2,
                width,
                layer,
                net: resolve_net(seg, &table),
                uuid: seg.find_str("uuid").map(str::to_string),
            })
        })();
        match parsed {
            Some(t) => items.push(t),
            None => skipped += 1,
        }
    }
    Scan { items, skipped }
}

/// Every via on the board (direct children of `(kicad_pcb …)`).
pub fn vias(tree: &SexpNode) -> Scan<Via> {
    let table = top_level_net_table(tree);
    let mut items = Vec::new();
    let mut skipped = 0usize;
    for via in tree.find_all("via") {
        let parsed = (|| {
            let (x, y) = point(via, "at")?;
            let size = via.find_f64("size").filter(|v| v.is_finite() && *v > 0.0)?;
            let drill = via
                .find_f64("drill")
                .filter(|v| v.is_finite() && *v > 0.0)?;
            let layers_node = via.find("layers")?;
            let layers: Vec<String> = layers_node
                .children()?
                .iter()
                .skip(1)
                .filter_map(|c| c.as_str())
                .map(str::to_string)
                .collect();
            if layers.is_empty() {
                return None;
            }
            Some(Via {
                x,
                y,
                size,
                drill,
                layers,
                net: resolve_net(via, &table),
                uuid: via.find_str("uuid").map(str::to_string),
            })
        })();
        match parsed {
            Some(v) => items.push(v),
            None => skipped += 1,
        }
    }
    Scan { items, skipped }
}

/// The authored outline of every zone on the board.
///
/// Only the first `(polygon …)` of each zone is read — that is the outline
/// the user drew; a zone can additionally carry rule areas and per-layer
/// `filled_polygon`s that are derived data.
pub fn zones(tree: &SexpNode) -> Scan<ZoneOutline> {
    let table = top_level_net_table(tree);
    let mut items = Vec::new();
    let mut skipped = 0usize;
    for zone in tree.find_all("zone") {
        let parsed = (|| {
            // KiCad 10 zones write (layers …); legacy single-layer zones
            // write (layer …). Read whichever shape is present.
            let layers: Vec<String> = match zone.find("layers") {
                Some(node) => node
                    .children()?
                    .iter()
                    .skip(1)
                    .filter_map(|c| c.as_str())
                    .map(str::to_string)
                    .collect(),
                None => vec![zone.find_str("layer")?.to_string()],
            };
            if layers.is_empty() {
                return None;
            }
            let pts = zone.find("polygon")?.find("pts")?;
            let mut points = Vec::new();
            let mut elements = Vec::new();
            let mut defining_coordinate_count = 0usize;
            for node in pts.children()?.iter().skip(1) {
                match node.head()? {
                    "xy" => {
                        let point = coordinate_pair(node)?;
                        points.push(point);
                        elements.push(ZoneOutlineElement::Point(point));
                        defining_coordinate_count += 1;
                    }
                    "arc" => {
                        let start = point(node, "start")?;
                        let mid = point(node, "mid")?;
                        let end = point(node, "end")?;
                        elements.push(ZoneOutlineElement::Arc { start, mid, end });
                        defining_coordinate_count += 3;
                    }
                    _ => return None,
                }
            }
            if defining_coordinate_count < 3 {
                return None; // fewer than 3 coordinates cannot enclose area
            }
            Some(ZoneOutline {
                net: resolve_net(zone, &table),
                layers,
                points,
                elements,
            })
        })();
        match parsed {
            Some(z) => items.push(z),
            None => skipped += 1,
        }
    }
    Scan { items, skipped }
}

/// Bounding box `(min_x, min_y, max_x, max_y)` of the board outline: every
/// `Edge.Cuts` graphic that is a direct child of `(kicad_pcb …)` — `gr_line`,
/// `gr_rect`, `gr_arc`, `gr_circle`, `gr_curve`.
///
/// Arcs use exact extrema ([`crate::geometry::arc_bbox`]): a board whose
/// outline bulges through a fillet or a semicircular edge is wider than its
/// endpoints say. Bézier `gr_curve`s use the control-point hull, which is a
/// (tight enough) superset of the curve.
///
/// All-or-nothing: `None` when there are no Edge.Cuts graphics **or when any
/// of them is malformed**. A partial outline bbox looks exactly like a
/// finished one and silently mis-sizes the board, so a single broken edge
/// graphic invalidates the answer rather than shrinking it.
pub fn board_outline_bbox(tree: &SexpNode) -> Option<(f64, f64, f64, f64)> {
    const EDGE_TAGS: [&str; 5] = ["gr_line", "gr_rect", "gr_arc", "gr_circle", "gr_curve"];
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for child in tree.children().unwrap_or(&[]) {
        let Some(head) = child.head() else { continue };
        if !EDGE_TAGS.contains(&head) || child.find_str("layer") != Some("Edge.Cuts") {
            continue;
        }
        let bb = graphic_bbox(child, head)?;
        acc = Some(match acc {
            None => bb,
            Some((x0, y0, x1, y1)) => (x0.min(bb.0), y0.min(bb.1), x1.max(bb.2), y1.max(bb.3)),
        });
    }
    acc
}

/// Bbox of a single edge graphic; `None` when a load-bearing coordinate is
/// missing or non-finite.
fn graphic_bbox(node: &SexpNode, head: &str) -> Option<(f64, f64, f64, f64)> {
    match head {
        "gr_line" | "gr_rect" => {
            let (x1, y1) = point(node, "start")?;
            let (x2, y2) = point(node, "end")?;
            Some((x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)))
        }
        "gr_arc" => {
            let start = point(node, "start")?;
            let mid = point(node, "mid")?;
            let end = point(node, "end")?;
            Some(crate::geometry::arc_bbox(start, mid, end))
        }
        "gr_circle" => {
            let (cx, cy) = point(node, "center")?;
            let (ex, ey) = point(node, "end")?;
            let r = (ex - cx).hypot(ey - cy);
            Some((cx - r, cy - r, cx + r, cy + r))
        }
        "gr_curve" => {
            // Cubic Bézier: the control polygon contains the curve, so its
            // hull is a valid (if slightly loose) bbox.
            let pts = node.find("pts")?;
            let mut acc: Option<(f64, f64, f64, f64)> = None;
            for xy in pts.find_all("xy") {
                let (x, y) = (xy.get_f64(1)?, xy.get_f64(2)?);
                if !x.is_finite() || !y.is_finite() {
                    return None;
                }
                acc = Some(match acc {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
            acc
        }
        _ => None,
    }
}

/// `(tag x y)` as a finite coordinate pair, or `None` — never a zero-filled
/// stand-in (see the module docs).
fn coordinate_pair(node: &SexpNode) -> Option<(f64, f64)> {
    let (x, y) = (node.get_f64(1)?, node.get_f64(2)?);
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

fn point(node: &SexpNode, tag: &str) -> Option<(f64, f64)> {
    coordinate_pair(node.find(tag)?)
}

/// The board's top-level net table (`(net N "NAME")` direct children), which
/// only KiCad ≤ 9 writes. Keys are the numeric ids as written.
fn top_level_net_table(tree: &SexpNode) -> HashMap<String, String> {
    tree.find_all("net")
        .into_iter()
        .filter_map(|n| Some((net::net_id(n)?.to_string(), net::net_name(n)?.to_string())))
        .collect()
}

/// Resolve an item's `(net …)` child to a net name using both format shapes
/// (see [`crate::net`]). `None` means unconnected — net 0, the empty name, or
/// no net node at all. A numeric reference the table cannot resolve keeps the
/// id digits as its key: that is real identity from the file (the same
/// fallback [`net::collect_net_keys`] uses), unlike a fabricated name.
fn resolve_net(item: &SexpNode, table: &HashMap<String, String>) -> Option<String> {
    let node = item.find("net")?;
    if let Some(name) = net::net_name(node) {
        return (!name.is_empty()).then(|| name.to_string());
    }
    let id = net::net_id(node)?;
    if id == "0" {
        return None; // the unconnected pseudo-net
    }
    match table.get(id) {
        Some(name) if !name.is_empty() => Some(name.clone()),
        Some(_) => None, // declared as the unconnected pseudo-net
        None => Some(id.to_string()),
    }
}

// ─── Footprint courtyards ────────────────────────────────────────────────────

/// Which side of the board a footprint is mounted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Front,
    Back,
}

/// Where a [`FootprintCourtyard`]'s bbox came from — always stated, never
/// implied, so a placement tool can tell a real courtyard from a stand-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CourtyardSource {
    /// The footprint draws `F.CrtYd`/`B.CrtYd` artwork; the bbox bounds it.
    Courtyard,
    /// No courtyard artwork; the bbox bounds the footprint's pads (center ±
    /// rotated half-size). Smaller than a real courtyard would be — it has no
    /// body outline and no clearance margin — but never silently absent.
    PadsFallback,
    /// No courtyard artwork *and* no pads (a logo, a fiducial-less doc
    /// footprint): the bbox degenerates to the anchor point itself. Zero area,
    /// real position.
    AnchorOnly,
}

/// One footprint's courtyard extent in **board** coordinates.
///
/// # Transform convention (verified, not derived — see the fixture tests)
///
/// Footprint children are stored in footprint-local coordinates in the same
/// Y-down orientation as the board; the file bakes **no rotation and no
/// translation** into them. Board position of any child point is
/// `(at) + R(rot)·local` where `R` is KiCAD's Y-down screen-CCW rotation
/// ([`crate::geometry::transform_pad`] — note the non-textbook sign pattern).
///
/// A **back-side** footprint needs *no additional mirroring*: KiCAD's flip
/// rewrites the stored child coordinates themselves, so the same rigid-body
/// transform holds for both sides. Verified against routed copper in the
/// pic_programmer demo: JP1 (B.Cu) pad 1 (net `VCC`, local x = −0.725)
/// lands at board x = 147.357 next to the B.Cu VCC track end at 147.447 —
/// mirroring would put it at 148.807, 1.36 mm away on the wrong side.
#[derive(Debug, Clone, PartialEq)]
pub struct FootprintCourtyard {
    /// The reference designator (`property "Reference"`, or the legacy
    /// `fp_text reference`); `None` when the footprint carries neither.
    pub reference: Option<String>,
    /// The footprint anchor `(at x y …)` in board coordinates.
    pub at: (f64, f64),
    /// The root `(at … rot)` angle in degrees, 0 when absent.
    pub rotation_deg: f64,
    /// Board side, from the footprint's own `(layer …)`.
    pub layer_side: Side,
    /// `(min_x, min_y, max_x, max_y)` in board coordinates. Exact for
    /// cardinal rotations; for a non-cardinal angle it is the axis-aligned
    /// hull of the rotated local bbox — a superset of the artwork, never a
    /// subset.
    pub bbox: (f64, f64, f64, f64),
    /// What the bbox actually bounds.
    pub bbox_source: CourtyardSource,
    /// KiCad's own lock state for this footprint, from a direct `(locked yes)`
    /// child (or the legacy bare `locked` token). A locked footprint is one the
    /// user has pinned in the editor: automated placement must treat it as an
    /// obstacle, never as something to move.
    pub locked: bool,
}

/// Per-footprint courtyard bboxes in board coordinates, in file order.
///
/// Skipped-and-counted (never guessed): a footprint whose root `(at …)` is
/// missing/non-finite, whose `(layer …)` names no F./B. side, or whose
/// courtyard/pad geometry is partially unreadable. Partial geometry is
/// all-or-nothing for the same reason as [`board_outline_bbox`]: a bbox that
/// silently lost one courtyard line looks exactly like a complete one.
pub fn footprint_courtyards(tree: &SexpNode) -> Scan<FootprintCourtyard> {
    let mut items = Vec::new();
    let mut skipped = 0usize;
    for fp in footprints(tree) {
        match footprint_courtyard(fp) {
            Some(c) => items.push(c),
            None => skipped += 1,
        }
    }
    Scan { items, skipped }
}

/// Tags a footprint graphic child can carry.
const FP_SHAPE_TAGS: [&str; 5] = ["fp_line", "fp_rect", "fp_circle", "fp_arc", "fp_poly"];

fn footprint_courtyard(fp: &SexpNode) -> Option<FootprintCourtyard> {
    let (fx, fy, rot) = footprint_root_at(fp)?;
    let layer_side = footprint_side(fp)?;

    // Hull of the courtyard artwork in footprint-local coordinates.
    // All-or-nothing: one unreadable courtyard shape poisons the footprint.
    let mut local: Option<(f64, f64, f64, f64)> = None;
    for child in fp.children().unwrap_or(&[]) {
        let Some(head) = child.head() else { continue };
        if !FP_SHAPE_TAGS.contains(&head) {
            continue;
        }
        match child.find_str("layer") {
            Some("F.CrtYd") | Some("B.CrtYd") => {}
            _ => continue,
        }
        let bb = fp_shape_bbox(child, head)?;
        local = Some(merge_bbox(local, bb));
    }

    let (bbox, bbox_source) = match local {
        Some(lb) => (
            transform_local_bbox(lb, fx, fy, rot),
            CourtyardSource::Courtyard,
        ),
        None => {
            // Pad fallback, computed directly in board space: the stored pad
            // angle is the board-space angle (the file writes pad rotation
            // *including* the footprint's — R1 at −90° in the ecc83 demo
            // stores its pads at 270°), so center ± rotated half-size is
            // exact at any angle. All-or-nothing over the pads.
            let pads = fp.find_all("pad");
            if pads.is_empty() {
                ((fx, fy, fx, fy), CourtyardSource::AnchorOnly)
            } else {
                let mut acc: Option<(f64, f64, f64, f64)> = None;
                for pad in &pads {
                    let (lx, ly) = point(pad, "at")?;
                    let (cx, cy) = crate::geometry::transform_pad(lx, ly, fx, fy, rot);
                    let size = pad.find("size")?;
                    let (w, h) = (size.get_f64(1)?, size.get_f64(2)?);
                    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
                        return None;
                    }
                    let angle = pad.find("at").and_then(|a| a.get_f64(3)).unwrap_or(0.0);
                    let (s, c) = angle.to_radians().sin_cos();
                    let ext_x = (w / 2.0 * c).abs() + (h / 2.0 * s).abs();
                    let ext_y = (w / 2.0 * s).abs() + (h / 2.0 * c).abs();
                    acc = Some(merge_bbox(
                        acc,
                        (cx - ext_x, cy - ext_y, cx + ext_x, cy + ext_y),
                    ));
                }
                (acc?, CourtyardSource::PadsFallback)
            }
        }
    };

    Some(FootprintCourtyard {
        reference: footprint_reference(fp),
        at: (fx, fy),
        rotation_deg: rot,
        layer_side,
        bbox,
        bbox_source,
        locked: footprint_locked(fp),
    })
}

/// KiCad's lock state for a footprint, read from its **direct** children only.
///
/// Two forms exist and both mean locked:
/// - `(locked yes)` — what KiCad 10 writes.
/// - a bare `locked` token — the older inline form.
///
/// Depth matters more than it looks: `locked` appears tens of thousands of
/// times across the installed demo boards, almost all of it on pads and
/// graphics, so a text scan or a recursive search would report nearly every
/// footprint as locked. Only a direct child describes the footprint itself.
fn footprint_locked(fp: &SexpNode) -> bool {
    for child in fp.children().unwrap_or(&[]) {
        match child {
            // Legacy inline form: a bare `locked` token among the children.
            SexpNode::Atom(a) if a == "locked" => return true,
            _ => {}
        }
        if child.head() == Some("locked") {
            // `(locked yes)` locks; `(locked no)` is an explicit unlock.
            return matches!(child.get(1).and_then(|n| n.as_str()), Some("yes"));
        }
    }
    false
}

/// The footprint's root `(at x y [rot])`. The rotation defaults to 0 when
/// absent — that is the file's own convention — but x/y are load-bearing.
fn footprint_root_at(fp: &SexpNode) -> Option<(f64, f64, f64)> {
    let at = fp.find("at")?;
    let (x, y) = (at.get_f64(1)?, at.get_f64(2)?);
    let rot = at.get_f64(3).unwrap_or(0.0);
    (x.is_finite() && y.is_finite() && rot.is_finite()).then_some((x, y, rot))
}

/// Which side the footprint's own `(layer …)` puts it on. A footprint layer
/// is always `F.Cu` or `B.Cu` in KiCAD-authored files; anything else is
/// malformed rather than a third side.
fn footprint_side(fp: &SexpNode) -> Option<Side> {
    match fp.find_str("layer")? {
        "F.Cu" => Some(Side::Front),
        "B.Cu" => Some(Side::Back),
        _ => None,
    }
}

/// The reference designator: `(property "Reference" "R1" …)` in current
/// formats, `(fp_text reference "R1" …)` in legacy ones.
fn footprint_reference(fp: &SexpNode) -> Option<String> {
    for prop in fp.find_all("property") {
        if prop.get(1).and_then(|n| n.as_str()) == Some("Reference") {
            return prop.get(2).and_then(|n| n.as_str()).map(str::to_string);
        }
    }
    for text in fp.find_all("fp_text") {
        if text.get(1).and_then(|n| n.as_str()) == Some("reference") {
            return text.get(2).and_then(|n| n.as_str()).map(str::to_string);
        }
    }
    None
}

/// Local (footprint-frame) bbox of one `fp_*` graphic; `None` when a
/// load-bearing coordinate is missing or non-finite. Centerline geometry —
/// stroke width is not expanded, matching what the courtyard outline means.
fn fp_shape_bbox(node: &SexpNode, head: &str) -> Option<(f64, f64, f64, f64)> {
    match head {
        "fp_line" | "fp_rect" => {
            let (x1, y1) = point(node, "start")?;
            let (x2, y2) = point(node, "end")?;
            Some((x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)))
        }
        "fp_arc" => {
            let start = point(node, "start")?;
            let mid = point(node, "mid")?;
            let end = point(node, "end")?;
            Some(crate::geometry::arc_bbox(start, mid, end))
        }
        "fp_circle" => {
            let (cx, cy) = point(node, "center")?;
            let (ex, ey) = point(node, "end")?;
            let r = (ex - cx).hypot(ey - cy);
            Some((cx - r, cy - r, cx + r, cy + r))
        }
        "fp_poly" => {
            let pts = node.find("pts")?;
            let mut acc: Option<(f64, f64, f64, f64)> = None;
            for xy in pts.find_all("xy") {
                let (x, y) = (xy.get_f64(1)?, xy.get_f64(2)?);
                if !x.is_finite() || !y.is_finite() {
                    return None;
                }
                acc = Some(merge_bbox(acc, (x, y, x, y)));
            }
            acc
        }
        _ => None,
    }
}

fn merge_bbox(acc: Option<(f64, f64, f64, f64)>, bb: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    match acc {
        None => bb,
        Some((x0, y0, x1, y1)) => (x0.min(bb.0), y0.min(bb.1), x1.max(bb.2), y1.max(bb.3)),
    }
}

/// Push a local-frame bbox through the footprint's rigid-body transform:
/// rotate its four corners ([`crate::geometry::transform_pad`]) and hull.
/// Exact for cardinal rotations; a superset for any other angle.
fn transform_local_bbox(
    (lx0, ly0, lx1, ly1): (f64, f64, f64, f64),
    fx: f64,
    fy: f64,
    rot: f64,
) -> (f64, f64, f64, f64) {
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for (x, y) in [(lx0, ly0), (lx1, ly0), (lx0, ly1), (lx1, ly1)] {
        let (bx, by) = crate::geometry::transform_pad(x, y, fx, fy, rot);
        acc = Some(merge_bbox(acc, (bx, by, bx, by)));
    }
    acc.expect("four corners were merged")
}

// ─── Connectivity index ──────────────────────────────────────────────────────

/// One pad, located in board coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct PadSite {
    /// The owning footprint's reference designator.
    pub reference: String,
    /// The pad's own number/name as written (`"1"`, `"A7"`, `"SH"`, or `""`
    /// on mechanical pads). Not unique within a footprint: multi-hole pads
    /// share a number.
    pub pad_number: String,
    /// Pad anchor in board coordinates — the pad's local `(at …)` pushed
    /// through the footprint's rigid-body transform (see
    /// [`FootprintCourtyard`] for the convention and its verification).
    pub at: (f64, f64),
    /// The **footprint's** mounting side. A through-hole pad is physically
    /// reachable from both sides regardless; this records where its
    /// footprint sits.
    pub layer_side: Side,
}

/// Net-keyed index over a board's pads and routed segments.
///
/// # Net identity
///
/// Keys are net **names**, never numeric codes — codes are renumbered on
/// every save, so an index keyed by them goes stale the moment KiCAD touches
/// the file. Both file formats resolve through the same rules as the scans
/// (see the module docs). Pads on the unconnected pseudo-net are indexed
/// separately and stay reachable via [`pads_without_net`] — test-point
/// tooling needs mechanical/TH pads whether or not they carry a net.
///
/// # Loss accounting
///
/// The constructor never guesses: a pad without a readable number or
/// position, or one whose footprint root is unreadable, is dropped and
/// counted in [`skipped_pads`]; segments inherit [`tracks`]' policy via
/// [`skipped_tracks`]. Non-zero counts on a KiCAD-authored board mean the
/// parse cannot be trusted, not that the board is odd.
///
/// [`pads_without_net`]: PcbConnectivityIndex::pads_without_net
/// [`skipped_pads`]: PcbConnectivityIndex::skipped_pads
/// [`skipped_tracks`]: PcbConnectivityIndex::skipped_tracks
#[derive(Debug, Clone, PartialEq)]
pub struct PcbConnectivityIndex {
    pads_by_net: BTreeMap<String, Vec<PadSite>>,
    unconnected_pads: Vec<PadSite>,
    segments_by_net: BTreeMap<String, Vec<Track>>,
    /// `(reference, pad_number) → net name`, first occurrence wins for the
    /// (shared-number) multi-hole case — all KiCAD-authored duplicates share
    /// one net.
    net_by_pad: HashMap<(String, String), String>,
    skipped_pads: usize,
    skipped_tracks: usize,
}

impl PcbConnectivityIndex {
    /// Build the index from a parsed `.kicad_pcb` tree.
    pub fn build(tree: &SexpNode) -> Self {
        let table = top_level_net_table(tree);
        let mut pads_by_net: BTreeMap<String, Vec<PadSite>> = BTreeMap::new();
        let mut unconnected_pads = Vec::new();
        let mut net_by_pad = HashMap::new();
        let mut skipped_pads = 0usize;

        for fp in footprints(tree) {
            let root = footprint_root_at(fp).zip(footprint_side(fp));
            let reference = footprint_reference(fp);
            for pad in fp.find_all("pad") {
                let site = (|| {
                    let ((fx, fy, rot), layer_side) = root?;
                    let reference = reference.clone()?;
                    let pad_number = pad.get(1)?.as_str()?.to_string();
                    let (lx, ly) = point(pad, "at")?;
                    let at = crate::geometry::transform_pad(lx, ly, fx, fy, rot);
                    Some(PadSite {
                        reference,
                        pad_number,
                        at,
                        layer_side,
                    })
                })();
                let Some(site) = site else {
                    skipped_pads += 1;
                    continue;
                };
                match resolve_net(pad, &table) {
                    Some(net) => {
                        net_by_pad
                            .entry((site.reference.clone(), site.pad_number.clone()))
                            .or_insert_with(|| net.clone());
                        pads_by_net.entry(net).or_default().push(site);
                    }
                    None => unconnected_pads.push(site),
                }
            }
        }

        let scan = tracks(tree);
        let mut segments_by_net: BTreeMap<String, Vec<Track>> = BTreeMap::new();
        for track in scan.items {
            if let Some(net) = track.net.clone() {
                segments_by_net.entry(net).or_default().push(track);
            }
        }

        PcbConnectivityIndex {
            pads_by_net,
            unconnected_pads,
            segments_by_net,
            net_by_pad,
            skipped_pads,
            skipped_tracks: scan.skipped,
        }
    }

    /// Every pad on `net`, in file order; empty for a net the board never
    /// pads (including unknown names — asking is not an error).
    pub fn pads_of_net(&self, net: &str) -> &[PadSite] {
        self.pads_by_net.get(net).map_or(&[], Vec::as_slice)
    }

    /// Every routed segment on `net`, in file order.
    pub fn segments_of_net(&self, net: &str) -> &[Track] {
        self.segments_by_net.get(net).map_or(&[], Vec::as_slice)
    }

    /// Every net the index knows — any net with a pad or a routed segment —
    /// sorted by name.
    pub fn nets(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .pads_by_net
            .keys()
            .chain(self.segments_by_net.keys())
            .map(String::as_str)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The net of pad `pad_number` on footprint `reference`; `None` when the
    /// pad is unknown *or* unconnected — [`Self::pads_without_net`]
    /// distinguishes the two.
    pub fn net_of_pad(&self, reference: &str, pad_number: &str) -> Option<&str> {
        self.net_by_pad
            .get(&(reference.to_string(), pad_number.to_string()))
            .map(String::as_str)
    }

    /// Pads on the unconnected pseudo-net (mounting holes, shields,
    /// mechanical pads), in file order. Indexed under no net but never
    /// dropped.
    pub fn pads_without_net(&self) -> &[PadSite] {
        &self.unconnected_pads
    }

    /// Pads dropped for missing load-bearing fields. See the type docs.
    pub fn skipped_pads(&self) -> usize {
        self.skipped_pads
    }

    /// Segments dropped by the underlying [`tracks`] scan.
    pub fn skipped_tracks(&self) -> usize {
        self.skipped_tracks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_sexp;

    /// A board carrying two footprints of two pads each, in KiCad's own
    /// tab-indented layout.
    const BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(footprint \"R_0402\"\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(pad \"1\" smd roundrect\n\
        \t\t\t(at -0.51 0)\n\
        \t\t\t(size 0.54 0.64)\n\
        \t\t)\n\
        \t\t(pad \"2\" smd roundrect\n\
        \t\t\t(at 0.51 0)\n\
        \t\t\t(size 0.54 0.64)\n\
        \t\t)\n\
        \t)\n\
        \t(footprint \"C_0402\"\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(pad \"1\" smd roundrect\n\
        \t\t\t(at -0.51 0)\n\
        \t\t)\n\
        \t\t(pad \"2\" smd roundrect\n\
        \t\t\t(at 0.51 0)\n\
        \t\t)\n\
        \t)\n\
        )";

    /// The bug this module exists to prevent: `find_all` does not recurse, so
    /// asking the root for pads is not merely inaccurate, it is always zero.
    #[test]
    fn pads_are_nested_so_the_root_never_sees_them() {
        let tree = parse_sexp(BOARD).unwrap();

        assert_eq!(
            tree.find_all("pad").len(),
            0,
            "if this ever becomes non-zero, find_all started recursing and \
             every caller needs rechecking"
        );
        assert_eq!(count_pads(&tree), 4);
        assert_eq!(footprints(&tree).len(), 2);
    }

    #[test]
    fn a_board_with_no_footprints_has_no_pads() {
        let tree = parse_sexp("(kicad_pcb\n\t(version 20260206)\n)").unwrap();
        assert_eq!(count_pads(&tree), 0);
        assert_eq!(footprints(&tree).len(), 0);
    }

    /// KiCad ≤ 9 shapes, node-for-node as pcbnew 9 writes them (taken from the
    /// ecc83 and RoyalBlue54L-NFC-Antenna demo boards): top-level net table,
    /// numeric net references, single-layer zone with `(net_name …)`.
    const KICAD9_BOARD: &str = "(kicad_pcb\n\
        \t(version 20241229)\n\
        \t(generator \"pcbnew\")\n\
        \t(net 0 \"\")\n\
        \t(net 1 \"GND\")\n\
        \t(net 2 \"Net-(P3-P1)\")\n\
        \t(segment\n\
        \t\t(start 139.573 99.695)\n\
        \t\t(end 141.605 99.695)\n\
        \t\t(width 0.8)\n\
        \t\t(layer \"B.Cu\")\n\
        \t\t(net 2)\n\
        \t\t(uuid \"1d6285fd-2d49-4956-932f-458079ff628a\")\n\
        \t)\n\
        \t(segment\n\
        \t\t(start 0 0)\n\
        \t\t(end 1 0)\n\
        \t\t(width 0.5)\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(net 0)\n\
        \t)\n\
        \t(via\n\
        \t\t(at 152.65011 73.152695)\n\
        \t\t(size 1.27)\n\
        \t\t(drill 0.7112)\n\
        \t\t(layers \"F.Cu\" \"B.Cu\")\n\
        \t\t(tenting front back)\n\
        \t\t(net 1)\n\
        \t\t(uuid \"44daf9b6-a0ef-4d27-aaa3-1cdd2fffc238\")\n\
        \t)\n\
        \t(zone\n\
        \t\t(net 1)\n\
        \t\t(net_name \"GND\")\n\
        \t\t(layer \"B.Cu\")\n\
        \t\t(hatch edge 0.508)\n\
        \t\t(polygon\n\
        \t\t\t(pts\n\
        \t\t\t\t(xy 172.085 135.89) (xy 172.085 91.313) (xy 122.555 91.44) (xy 122.555 135.89)\n\
        \t\t\t)\n\
        \t\t)\n\
        \t)\n\
        )";

    /// KiCad 10 shapes (from the pic_programmer demo): no net table, names in
    /// place on every item.
    const KICAD10_BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(segment\n\
        \t\t(start 110.49 124.155)\n\
        \t\t(end 110.49 119.38)\n\
        \t\t(width 0.8)\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(net \"VCC\")\n\
        \t\t(uuid \"234aa39b-64ad-4fb9-b7a9-cdc8cbfb4541\")\n\
        \t)\n\
        \t(via\n\
        \t\t(at 189.865 110.49)\n\
        \t\t(size 1.6)\n\
        \t\t(drill 0.6)\n\
        \t\t(layers \"F.Cu\" \"B.Cu\")\n\
        \t\t(net \"/CLOCK-RB6\")\n\
        \t\t(uuid \"00c62925-76e5-4da4-9776-805e7e214afd\")\n\
        \t)\n\
        \t(zone\n\
        \t\t(net \"GND\")\n\
        \t\t(layer \"B.Cu\")\n\
        \t\t(polygon\n\
        \t\t\t(pts\n\
        \t\t\t\t(xy 223.52 138.43) (xy 232.41 128.905) (xy 232.41 53.975)\n\
        \t\t\t)\n\
        \t\t)\n\
        \t)\n\
        )";

    /// Reduced from the two arc-only zone outlines in KiCad 10's
    /// RoyalBlue54L-Feather demo board. The full board is intentionally not a
    /// fixture: these are the only authored forms the zone scan rejected.
    const KICAD10_ARC_ONLY_ZONES: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(net 15 \"+BATT\")\n\
        \t(net 35 \"VSYS\")\n\
        \t(zone\n\
        \t\t(net 35)\n\
        \t\t(net_name \"VSYS\")\n\
        \t\t(layer \"B.Cu\")\n\
        \t\t(polygon\n\
        \t\t\t(pts\n\
        \t\t\t\t(arc (start 149.28 105.91) (mid 149.206777 106.086777) (end 149.03 106.16))\n\
        \t\t\t\t(arc (start 147.13 106.16) (mid 146.953223 106.233223) (end 146.88 106.41))\n\
        \t\t\t\t(arc (start 146.88 108.81) (mid 146.953223 108.986777) (end 147.13 109.06))\n\
        \t\t\t\t(arc (start 149.93 109.06) (mid 150.106777 108.986777) (end 150.18 108.81))\n\
        \t\t\t\t(arc (start 150.18 99.41) (mid 150.106777 99.233223) (end 149.93 99.16))\n\
        \t\t\t\t(arc (start 147.83 99.16) (mid 147.653223 99.233223) (end 147.58 99.41))\n\
        \t\t\t\t(arc (start 147.58 102.61) (mid 147.653223 102.786777) (end 147.83 102.86))\n\
        \t\t\t\t(arc (start 149.03 102.86) (mid 149.206777 102.933223) (end 149.28 103.11))\n\
        \t\t\t)\n\
        \t\t)\n\
        \t)\n\
        \t(zone\n\
        \t\t(net 15)\n\
        \t\t(net_name \"+BATT\")\n\
        \t\t(layer \"In2.Cu\")\n\
        \t\t(polygon\n\
        \t\t\t(pts\n\
        \t\t\t\t(arc (start 144.26 95.23) (mid 144.552893 95.937107) (end 145.26 96.23))\n\
        \t\t\t\t(arc (start 148.545786 96.23) (mid 148.928469 96.30612) (end 149.252893 96.522893))\n\
        \t\t\t\t(arc (start 150.167107 97.437107) (mid 150.38388 97.76153) (end 150.46 98.144214))\n\
        \t\t\t\t(arc (start 150.46 102.13) (mid 150.167107 102.837107) (end 149.46 103.13))\n\
        \t\t\t\t(arc (start 138.36 103.13) (mid 137.652893 102.837107) (end 137.36 102.13))\n\
        \t\t\t\t(arc (start 137.36 94.73) (mid 137.652893 94.022893) (end 138.36 93.73))\n\
        \t\t\t\t(arc (start 143.26 93.73) (mid 143.967107 94.022893) (end 144.26 94.73))\n\
        \t\t\t)\n\
        \t\t)\n\
        \t)\n\
        )";

    #[test]
    fn tracks_resolve_numeric_nets_through_the_table() {
        let tree = parse_sexp(KICAD9_BOARD).unwrap();
        let scan = tracks(&tree);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items.len(), 2);

        let t = &scan.items[0];
        assert_eq!((t.x1, t.y1, t.x2, t.y2), (139.573, 99.695, 141.605, 99.695));
        assert_eq!(t.width, 0.8);
        assert_eq!(t.layer, "B.Cu");
        assert_eq!(t.net.as_deref(), Some("Net-(P3-P1)"));
        assert_eq!(
            t.uuid.as_deref(),
            Some("1d6285fd-2d49-4956-932f-458079ff628a")
        );

        // (net 0) is the unconnected pseudo-net, not a net named "0".
        assert_eq!(scan.items[1].net, None);
        assert_eq!(scan.items[1].uuid, None);
    }

    #[test]
    fn tracks_read_kicad_10_names_in_place() {
        let tree = parse_sexp(KICAD10_BOARD).unwrap();
        let scan = tracks(&tree);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items.len(), 1);
        assert_eq!(scan.items[0].net.as_deref(), Some("VCC"));
        assert_eq!(scan.items[0].width, 0.8);
    }

    #[test]
    fn vias_read_both_net_shapes() {
        let k9 = parse_sexp(KICAD9_BOARD).unwrap();
        let scan = vias(&k9);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items.len(), 1);
        let v = &scan.items[0];
        assert_eq!((v.x, v.y), (152.65011, 73.152695));
        assert_eq!((v.size, v.drill), (1.27, 0.7112));
        assert_eq!(v.layers, vec!["F.Cu", "B.Cu"]);
        assert_eq!(v.net.as_deref(), Some("GND"));

        let k10 = parse_sexp(KICAD10_BOARD).unwrap();
        let scan = vias(&k10);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items[0].net.as_deref(), Some("/CLOCK-RB6"));
    }

    #[test]
    fn zones_take_the_authored_outline() {
        let k9 = parse_sexp(KICAD9_BOARD).unwrap();
        let scan = zones(&k9);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items.len(), 1);
        let z = &scan.items[0];
        assert_eq!(z.net.as_deref(), Some("GND"));
        assert_eq!(z.layers, vec!["B.Cu"]);
        assert_eq!(
            z.points,
            vec![
                (172.085, 135.89),
                (172.085, 91.313),
                (122.555, 91.44),
                (122.555, 135.89),
            ]
        );
        assert_eq!(
            z.elements,
            vec![
                ZoneOutlineElement::Point((172.085, 135.89)),
                ZoneOutlineElement::Point((172.085, 91.313)),
                ZoneOutlineElement::Point((122.555, 91.44)),
                ZoneOutlineElement::Point((122.555, 135.89)),
            ]
        );

        let k10 = parse_sexp(KICAD10_BOARD).unwrap();
        assert_eq!(zones(&k10).items[0].net.as_deref(), Some("GND"));
    }

    #[test]
    fn a_kicad_10_zone_may_span_several_layers() {
        let tree = parse_sexp(
            "(kicad_pcb\n\t(zone\n\t\t(net \"GND\")\n\t\t(layers \"F.Cu\" \"B.Cu\")\n\
             \t\t(polygon (pts (xy 0 0) (xy 10 0) (xy 10 10)))\n\t))",
        )
        .unwrap();
        let scan = zones(&tree);
        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items[0].layers, vec!["F.Cu", "B.Cu"]);
    }

    #[test]
    fn kicad_10_arc_only_zone_outlines_scan_losslessly() {
        let tree = parse_sexp(KICAD10_ARC_ONLY_ZONES).unwrap();
        let scan = zones(&tree);

        assert_eq!(scan.skipped, 0);
        assert_eq!(scan.items.len(), 2);
        assert_eq!(scan.items[0].net.as_deref(), Some("VSYS"));
        assert_eq!(scan.items[0].layers, vec!["B.Cu"]);
        assert!(scan.items[0].points.is_empty());
        assert_eq!(scan.items[0].elements.len(), 8);
        assert_eq!(
            scan.items[0].elements.first(),
            Some(&ZoneOutlineElement::Arc {
                start: (149.28, 105.91),
                mid: (149.206777, 106.086777),
                end: (149.03, 106.16),
            })
        );
        assert_eq!(
            scan.items[0].elements.last(),
            Some(&ZoneOutlineElement::Arc {
                start: (149.03, 102.86),
                mid: (149.206777, 102.933223),
                end: (149.28, 103.11),
            })
        );
        assert_eq!(scan.items[1].net.as_deref(), Some("+BATT"));
        assert_eq!(scan.items[1].layers, vec!["In2.Cu"]);
        assert!(scan.items[1].points.is_empty());
        assert_eq!(scan.items[1].elements.len(), 7);
        assert_eq!(
            scan.items[1].elements.first(),
            Some(&ZoneOutlineElement::Arc {
                start: (144.26, 95.23),
                mid: (144.552893, 95.937107),
                end: (145.26, 96.23),
            })
        );
        assert_eq!(
            scan.items[1].elements.last(),
            Some(&ZoneOutlineElement::Arc {
                start: (143.26, 93.73),
                mid: (143.967107, 94.022893),
                end: (144.26, 94.73),
            })
        );
    }

    /// The module's malformed-item policy: broken nodes are dropped *and
    /// counted*, and are never zero-filled into phantom copper at the origin.
    #[test]
    fn malformed_items_are_counted_not_fabricated() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(segment (start 0 0) (width 0.5) (layer \"F.Cu\"))\n\
             \t(segment (start 0 0) (end 1 0) (width 0) (layer \"F.Cu\"))\n\
             \t(segment (start 0 0) (end 1 0) (width 0.5) (layer \"F.Cu\"))\n\
             \t(via (at 1 1) (size 0.8) (layers \"F.Cu\" \"B.Cu\"))\n\
             \t(zone (layer \"F.Cu\") (polygon (pts (xy 0 0) (xy 1 0))))\n\
             \t(zone (layer \"F.Cu\") (polygon (pts\n\
             \t\t(arc (start 0 0) (mid 1 1)))))\n\
             \t(zone (layer \"F.Cu\") (polygon (pts\n\
             \t\t(xy 0 0) (curve (start 1 0) (end 1 1)) (xy 0 1))))\n\
             )",
        )
        .unwrap();
        let t = tracks(&tree);
        assert_eq!((t.items.len(), t.skipped), (1, 2)); // no end; zero width
        let v = vias(&tree);
        assert_eq!((v.items.len(), v.skipped), (0, 1)); // no drill
        let z = zones(&tree);
        assert_eq!((z.items.len(), z.skipped), (0, 3));
        // Too few points, an incomplete arc, and an unknown outline element
        // are all unreadable rather than being fabricated or dropped.
    }

    #[test]
    fn every_parsed_track_has_positive_finite_width() {
        // "NaN" parses as a valid f64 — the finiteness filter must catch it.
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(segment (start 0 0) (end 1 0) (width NaN) (layer \"F.Cu\"))\n\
             \t(segment (start inf 0) (end 1 0) (width 0.5) (layer \"F.Cu\"))\n\
             )",
        )
        .unwrap();
        let t = tracks(&tree);
        assert_eq!((t.items.len(), t.skipped), (0, 2));
    }

    /// Edge.Cuts bbox over the ecc83 demo's outline shape: four `gr_line`s
    /// forming a rectangle (coordinates verbatim from the demo board).
    #[test]
    fn outline_bbox_of_a_rectangular_line_outline() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(gr_line (start 173.355 90.17) (end 173.355 136.525) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 121.285 90.17) (end 121.285 136.525) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 173.355 90.17) (end 121.285 90.17) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 121.285 136.525) (end 173.355 136.525) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 0 0) (end 500 500) (layer \"F.SilkS\"))\n\
             )",
        )
        .unwrap();
        // The silkscreen line must not leak into the outline.
        assert_eq!(
            board_outline_bbox(&tree),
            Some((121.285, 90.17, 173.355, 136.525))
        );
    }

    /// An arc that bulges past both of its endpoints must widen the bbox by
    /// its true extrema, not its endpoint hull: semicircular board edge from
    /// (0, 0) to (10, 0) bulging down through (5, -5).
    #[test]
    fn outline_bbox_uses_exact_arc_extrema() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(gr_line (start 0 0) (end 10 0) (layer \"Edge.Cuts\"))\n\
             \t(gr_arc (start 0 0) (mid 5 -5) (end 10 0) (layer \"Edge.Cuts\"))\n\
             )",
        )
        .unwrap();
        let (x0, y0, x1, y1) = board_outline_bbox(&tree).unwrap();
        assert!((x0 - 0.0).abs() < 1e-9 && (x1 - 10.0).abs() < 1e-9);
        assert!((y1 - 0.0).abs() < 1e-9);
        // The bulge: min_y is the arc's -Y extreme at -5, far below the
        // endpoint hull's 0.
        assert!((y0 - -5.0).abs() < 1e-9, "min_y = {y0}, expected -5");
    }

    #[test]
    fn outline_bbox_handles_circles_rects_and_curves() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(gr_circle (center 50 50) (end 53 54) (layer \"Edge.Cuts\"))\n\
             \t(gr_rect (start 60 60) (end 40 45) (layer \"Edge.Cuts\"))\n\
             \t(gr_curve (pts (xy 30 50) (xy 32 48) (xy 35 47) (xy 38 50)) (layer \"Edge.Cuts\"))\n\
             )",
        )
        .unwrap();
        // circle r = 5 about (50, 50) → (45, 45, 55, 55); rect corners are
        // unordered; curve hull reaches x = 30, y = 47.
        assert_eq!(board_outline_bbox(&tree), Some((30.0, 45.0, 60.0, 60.0)));
    }

    /// All-or-nothing: one malformed edge graphic poisons the whole bbox —
    /// a partial outline is indistinguishable from a complete one.
    #[test]
    fn outline_bbox_refuses_a_partially_readable_outline() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(gr_line (start 0 0) (end 10 0) (layer \"Edge.Cuts\"))\n\
             \t(gr_line (start 0 0) (layer \"Edge.Cuts\"))\n\
             )",
        )
        .unwrap();
        assert_eq!(board_outline_bbox(&tree), None);
    }

    #[test]
    fn outline_bbox_is_none_without_edge_cuts() {
        let tree = parse_sexp("(kicad_pcb\n\t(version 20260206)\n)").unwrap();
        assert_eq!(board_outline_bbox(&tree), None);
    }

    // ─── Courtyards ──────────────────────────────────────────────────────

    fn assert_bbox_close(got: (f64, f64, f64, f64), want: (f64, f64, f64, f64), label: &str) {
        let ok = (got.0 - want.0).abs() < 1e-9
            && (got.1 - want.1).abs() < 1e-9
            && (got.2 - want.2).abs() < 1e-9
            && (got.3 - want.3).abs() < 1e-9;
        assert!(ok, "{label}: got {got:?}, expected {want:?}");
    }

    /// Non-cardinal rotation, hand-computed. Footprint at (100, 100, 45°),
    /// courtyard rect local (−1, −2)…(1, 2). KiCAD's Y-down CCW rotation is
    /// (x, y) → (x·cosθ + y·sinθ, y·cosθ − x·sinθ); at 45° (c = s = √2/2)
    /// the corners map to (±3·√2/2, ±√2/2) and (±√2/2, ∓3·√2/2), so the
    /// hull is ±3·√2/2 = ±2.121320343… about the anchor on both axes.
    #[test]
    fn courtyard_bbox_at_45_degrees_hand_computed() {
        let tree = parse_sexp(
            "(kicad_pcb\n\t(footprint \"X\"\n\t\t(layer \"F.Cu\")\n\t\t(at 100 100 45)\n\
             \t\t(property \"Reference\" \"U9\")\n\
             \t\t(fp_rect (start -1 -2) (end 1 2) (layer \"F.CrtYd\"))\n\t))",
        )
        .unwrap();
        let scan = footprint_courtyards(&tree);
        assert_eq!(scan.skipped, 0);
        let c = &scan.items[0];
        assert_eq!(c.reference.as_deref(), Some("U9"));
        assert_eq!(c.rotation_deg, 45.0);
        assert_eq!(c.bbox_source, CourtyardSource::Courtyard);
        let e = 3.0 * std::f64::consts::SQRT_2 / 2.0; // 2.1213203435596424
        assert_bbox_close(c.bbox, (100.0 - e, 100.0 - e, 100.0 + e, 100.0 + e), "45°");
    }

    /// Pads fallback, hand-computed with a rotated pad: footprint at
    /// (10, 10, 90°), pad local (2, 0), size 2×1, stored pad angle 90° (the
    /// file writes pad angles *including* the footprint's). Center lands at
    /// (10 + 0·2, 10 − 2) = (10, 8); at 90° the 2×1 rect turns into 1×2, so
    /// extents are (0.5, 1).
    #[test]
    fn pads_fallback_bbox_hand_computed() {
        let tree = parse_sexp(
            "(kicad_pcb\n\t(footprint \"R\"\n\t\t(layer \"F.Cu\")\n\t\t(at 10 10 90)\n\
             \t\t(property \"Reference\" \"R7\")\n\
             \t\t(pad \"1\" smd rect (at 2 0 90) (size 2 1))\n\t))",
        )
        .unwrap();
        let scan = footprint_courtyards(&tree);
        assert_eq!(scan.skipped, 0);
        let c = &scan.items[0];
        assert_eq!(c.bbox_source, CourtyardSource::PadsFallback);
        assert_bbox_close(c.bbox, (9.5, 7.0, 10.5, 9.0), "pad fallback");
    }

    /// A footprint with neither courtyard nor pads (a logo) keeps its anchor
    /// as a zero-area bbox — stated as such, never silently dropped.
    #[test]
    fn courtyardless_padless_footprint_is_anchor_only() {
        let tree = parse_sexp(
            "(kicad_pcb\n\t(footprint \"Logo\"\n\t\t(layer \"F.Cu\")\n\t\t(at 5 6)\n\
             \t\t(fp_line (start 0 0) (end 1 1) (layer \"F.SilkS\"))\n\t))",
        )
        .unwrap();
        let scan = footprint_courtyards(&tree);
        assert_eq!(scan.skipped, 0);
        let c = &scan.items[0];
        assert_eq!(c.bbox_source, CourtyardSource::AnchorOnly);
        assert_eq!(c.bbox, (5.0, 6.0, 5.0, 6.0));
        assert_eq!(c.reference, None);
    }

    /// All-or-nothing over the courtyard artwork, same rationale as
    /// [`board_outline_bbox`]: a partially-read courtyard looks complete.
    #[test]
    fn courtyard_with_a_broken_shape_is_skipped_and_counted() {
        let tree = parse_sexp(
            "(kicad_pcb\n\
             \t(footprint \"A\"\n\t\t(layer \"F.Cu\")\n\t\t(at 0 0)\n\
             \t\t(fp_line (start 0 0) (layer \"F.CrtYd\"))\n\t)\n\
             \t(footprint \"B\"\n\t\t(layer \"F.Cu\")\n\
             \t\t(fp_rect (start 0 0) (end 1 1) (layer \"F.CrtYd\"))\n\t)\n\
             \t(footprint \"C\"\n\t\t(layer \"F.SilkS\")\n\t\t(at 0 0)\n\
             \t\t(fp_rect (start 0 0) (end 1 1) (layer \"F.CrtYd\"))\n\t)\n\
             )",
        )
        .unwrap();
        // A: courtyard line without an end. B: no root (at …). C: layer that
        // names no side. All dropped, all counted.
        let scan = footprint_courtyards(&tree);
        assert_eq!((scan.items.len(), scan.skipped), (0, 3));
    }

    /// A back-side footprint keeps the same rigid-body transform — the file
    /// stores its children already flipped (verified against pic_programmer
    /// copper in the fixture tests; this pins the unit-level behavior).
    #[test]
    fn back_side_footprint_is_not_mirrored_again() {
        let tree = parse_sexp(
            "(kicad_pcb\n\t(footprint \"J\"\n\t\t(layer \"B.Cu\")\n\t\t(at 100 50)\n\
             \t\t(property \"Reference\" \"JP9\")\n\
             \t\t(fp_rect (start -2 -1) (end 3 1) (layer \"B.CrtYd\"))\n\t))",
        )
        .unwrap();
        let scan = footprint_courtyards(&tree);
        let c = &scan.items[0];
        assert_eq!(c.layer_side, Side::Back);
        // Asymmetric on purpose: mirroring would report (97, 49, 102, 51).
        assert_bbox_close(c.bbox, (98.0, 49.0, 103.0, 51.0), "back side");
    }

    // ─── Connectivity index ──────────────────────────────────────────────

    /// KiCad ≤ 9 shapes: pads write `(net N "NAME")`, segments bare `(net N)`.
    const INDEX_K9: &str = "(kicad_pcb\n\
        \t(version 20241229)\n\
        \t(net 0 \"\")\n\t(net 1 \"GND\")\n\t(net 2 \"VCC\")\n\
        \t(footprint \"R\"\n\t\t(layer \"F.Cu\")\n\t\t(at 10 20 90)\n\
        \t\t(property \"Reference\" \"R1\")\n\
        \t\t(pad \"1\" thru_hole circle (at 0 0 90) (size 1.6 1.6) (net 1 \"GND\"))\n\
        \t\t(pad \"2\" thru_hole circle (at 7.62 0 90) (size 1.6 1.6) (net 2 \"VCC\"))\n\t)\n\
        \t(footprint \"H\"\n\t\t(layer \"B.Cu\")\n\t\t(at 50 50)\n\
        \t\t(property \"Reference\" \"H1\")\n\
        \t\t(pad \"\" np_thru_hole circle (at 0 0) (size 3 3) (net 0 \"\"))\n\t)\n\
        \t(segment (start 10 20) (end 10 12.38) (width 0.5) (layer \"F.Cu\") (net 1))\n\
        \t(segment (start 0 0) (end 1 0) (width 0.5) (layer \"F.Cu\") (net 0))\n\
        )";

    #[test]
    fn index_transforms_pads_and_keys_nets_by_name() {
        let tree = parse_sexp(INDEX_K9).unwrap();
        let ix = PcbConnectivityIndex::build(&tree);
        assert_eq!((ix.skipped_pads(), ix.skipped_tracks()), (0, 0));

        // R1 pad 2: local (7.62, 0) through (10, 20, 90°) → (10, 20 − 7.62).
        let vcc = ix.pads_of_net("VCC");
        assert_eq!(vcc.len(), 1);
        assert_eq!(vcc[0].reference, "R1");
        assert_eq!(vcc[0].pad_number, "2");
        assert!((vcc[0].at.0 - 10.0).abs() < 1e-9 && (vcc[0].at.1 - 12.38).abs() < 1e-9);
        assert_eq!(vcc[0].layer_side, Side::Front);

        assert_eq!(ix.net_of_pad("R1", "1"), Some("GND"));
        assert_eq!(ix.net_of_pad("R1", "2"), Some("VCC"));
        assert_eq!(ix.net_of_pad("R1", "3"), None);
        assert_eq!(ix.net_of_pad("H1", ""), None); // unconnected, not unknown

        // Nets are names, sorted; the pseudo-net never appears.
        assert_eq!(ix.nets(), vec!["GND", "VCC"]);

        // The unconnected mounting hole stays reachable.
        let free = ix.pads_without_net();
        assert_eq!(free.len(), 1);
        assert_eq!(free[0].reference, "H1");
        assert_eq!(free[0].layer_side, Side::Back);

        // Segments: keyed by resolved name; the net-0 one is not on a net.
        assert_eq!(ix.segments_of_net("GND").len(), 1);
        assert_eq!(ix.segments_of_net("VCC").len(), 0);
    }

    #[test]
    fn index_reads_kicad_10_names_in_place() {
        let tree = parse_sexp(
            "(kicad_pcb\n\t(version 20260206)\n\
             \t(footprint \"C\"\n\t\t(layer \"F.Cu\")\n\t\t(at 0 0)\n\
             \t\t(property \"Reference\" \"C1\")\n\
             \t\t(pad \"1\" smd rect (at -1 0) (size 1 1) (net \"VDD\"))\n\
             \t\t(pad \"2\" smd rect (at 1 0) (size 1 1) (net \"GND\"))\n\t)\n\
             \t(segment (start 0 0) (end 1 0) (width 0.2) (layer \"F.Cu\") (net \"VDD\"))\n\
             )",
        )
        .unwrap();
        let ix = PcbConnectivityIndex::build(&tree);
        assert_eq!(ix.nets(), vec!["GND", "VDD"]);
        assert_eq!(ix.net_of_pad("C1", "1"), Some("VDD"));
        assert_eq!(ix.segments_of_net("VDD").len(), 1);
    }

    /// A net that exists only as routed copper (no pads) is still a net the
    /// index reports — and vice versa.
    #[test]
    fn nets_is_the_union_of_pad_and_segment_nets() {
        let tree = parse_sexp(
            "(kicad_pcb\n\t(version 20260206)\n\
             \t(footprint \"T\"\n\t\t(layer \"F.Cu\")\n\t\t(at 0 0)\n\
             \t\t(property \"Reference\" \"TP1\")\n\
             \t\t(pad \"1\" smd rect (at 0 0) (size 1 1) (net \"PAD_ONLY\"))\n\t)\n\
             \t(segment (start 0 0) (end 1 0) (width 0.2) (layer \"F.Cu\") (net \"SEG_ONLY\"))\n\
             )",
        )
        .unwrap();
        let ix = PcbConnectivityIndex::build(&tree);
        assert_eq!(ix.nets(), vec!["PAD_ONLY", "SEG_ONLY"]);
        assert!(ix.pads_of_net("SEG_ONLY").is_empty());
        assert!(ix.segments_of_net("PAD_ONLY").is_empty());
    }

    /// Loss accounting: a pad without a position and a footprint without a
    /// readable root drop their pads into `skipped_pads`, never into a
    /// zero-filled site at the origin.
    #[test]
    fn index_counts_unreadable_pads() {
        let tree = parse_sexp(
            "(kicad_pcb\n\t(version 20260206)\n\
             \t(footprint \"A\"\n\t\t(layer \"F.Cu\")\n\t\t(at 1 1)\n\
             \t\t(property \"Reference\" \"U1\")\n\
             \t\t(pad \"1\" smd rect (size 1 1) (net \"N\"))\n\
             \t\t(pad \"2\" smd rect (at 0 0) (size 1 1) (net \"N\"))\n\t)\n\
             \t(footprint \"B\"\n\t\t(layer \"F.Cu\")\n\
             \t\t(property \"Reference\" \"U2\")\n\
             \t\t(pad \"1\" smd rect (at 0 0) (size 1 1) (net \"N\"))\n\t)\n\
             )",
        )
        .unwrap();
        let ix = PcbConnectivityIndex::build(&tree);
        assert_eq!(ix.skipped_pads(), 2);
        assert_eq!(ix.pads_of_net("N").len(), 1);
        assert_eq!(ix.pads_of_net("N")[0].reference, "U1");
    }
}
