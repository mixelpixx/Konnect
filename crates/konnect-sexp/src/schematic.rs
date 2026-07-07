//! Higher-level schematic helpers built on the parser and writer.
//!
//! Provides typed query functions used by the tool implementations.

use crate::geometry::{transform_pin, PinTransform};
use crate::parser::{parse_sexp, SexpNode};
use crate::SexpError;
use std::path::Path;

// ─── Schematic file I/O ───────────────────────────────────────────────────────

pub fn read_schematic(path: &Path) -> Result<(String, SexpNode), SexpError> {
    let content = std::fs::read_to_string(path)?;
    let tree = parse_sexp(&content)?;
    Ok((content, tree))
}

// ─── Coordinate helpers ───────────────────────────────────────────────────────

/// Parse `(at X Y [ROT])` from a node.
pub fn parse_at(node: &SexpNode) -> Option<(f64, f64, f64)> {
    let at = node.find("at")?;
    let x = at.get_f64(1)?;
    let y = at.get_f64(2)?;
    let rot = at.get_f64(3).unwrap_or(0.0);
    Some((x, y, rot))
}

/// Parse `(start X Y)` from a node.
pub fn parse_start(node: &SexpNode) -> Option<(f64, f64)> {
    let s = node.find("start")?;
    Some((s.get_f64(1)?, s.get_f64(2)?))
}

/// Parse `(end X Y)` from a node.
pub fn parse_end(node: &SexpNode) -> Option<(f64, f64)> {
    let e = node.find("end")?;
    Some((e.get_f64(1)?, e.get_f64(2)?))
}

// ─── Wire ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Wire {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub uuid: Option<String>,
}

/// Extract all wires from a parsed schematic tree.
/// Handles both KiCAD 8/9 format `(start)(end)` and KiCAD 10 format `(pts (xy)(xy))`.
pub fn extract_wires(tree: &SexpNode) -> Vec<Wire> {
    tree.find_all("wire")
        .iter()
        .filter_map(|node| {
            // Try KiCAD 10 format first: (pts (xy X Y) (xy X Y))
            let (x1, y1, x2, y2) = if let Some(pts) = node.find("pts") {
                let xy_nodes = pts.find_all("xy");
                if xy_nodes.len() >= 2 {
                    let x1 = xy_nodes[0].get_f64(1)?;
                    let y1 = xy_nodes[0].get_f64(2)?;
                    let x2 = xy_nodes[1].get_f64(1)?;
                    let y2 = xy_nodes[1].get_f64(2)?;
                    (x1, y1, x2, y2)
                } else {
                    return None;
                }
            } else {
                // Fall back to KiCAD 8/9 format: (start X Y) (end X Y)
                let (x1, y1) = parse_start(node)?;
                let (x2, y2) = parse_end(node)?;
                (x1, y1, x2, y2)
            };
            let uuid = node
                .find("uuid")
                .and_then(|u| u.get(1))
                .and_then(|u| u.as_str())
                .map(String::from);
            Some(Wire {
                x1,
                y1,
                x2,
                y2,
                uuid,
            })
        })
        .collect()
}

// ─── Net label ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum LabelKind {
    NetLabel,
    GlobalLabel,
    HierarchicalLabel,
    PowerSymbol,
}

#[derive(Debug, Clone)]
pub struct Label {
    pub kind: LabelKind,
    pub net: String,
    pub x: f64,
    pub y: f64,
    pub rotation: f64,
    pub uuid: Option<String>,
}

pub fn extract_labels(tree: &SexpNode) -> Vec<Label> {
    let mut labels = Vec::new();

    for (kind_str, kind) in &[
        ("net_label", LabelKind::NetLabel),
        ("global_label", LabelKind::GlobalLabel),
        ("hierarchical_label", LabelKind::HierarchicalLabel),
    ] {
        for node in tree.find_all(kind_str) {
            let net = node
                .get(1)
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let (x, y, rotation) = parse_at(node).unwrap_or((0.0, 0.0, 0.0));
            let uuid = node
                .find("uuid")
                .and_then(|u| u.get(1))
                .and_then(|u| u.as_str())
                .map(String::from);
            labels.push(Label {
                kind: kind.clone(),
                net,
                x,
                y,
                rotation,
                uuid,
            });
        }
    }

    labels
}

// ─── Symbol instance ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SymbolInstance {
    pub reference: String,
    pub value: String,
    pub footprint: String,
    pub lib_id: String,
    pub x: f64,
    pub y: f64,
    pub rotation: f64,
    pub mirror_x: bool,
    pub mirror_y: bool,
    pub uuid: Option<String>,
}

impl SymbolInstance {
    pub fn pin_transform(&self) -> PinTransform {
        PinTransform {
            comp_x: self.x,
            comp_y: self.y,
            rotation_deg: self.rotation,
            mirror_x: self.mirror_x,
            mirror_y: self.mirror_y,
        }
    }
}

pub fn extract_symbol_instances(tree: &SexpNode) -> Vec<SymbolInstance> {
    tree.find_all("symbol")
        .iter()
        .filter_map(|node| {
            // Top-level symbols only have lib_id and at; filter out library definitions
            let lib_id = node.find("lib_id")?.get(1)?.as_str()?.to_string();
            let (x, y, rotation) = parse_at(node)?;

            let mirror_node = node.find("mirror");
            let mirror_x = mirror_node
                .and_then(|m| m.get(1))
                .and_then(|m| m.as_str())
                .map(|s| s == "x" || s == "xy")
                .unwrap_or(false);
            let mirror_y = mirror_node
                .and_then(|m| m.get(1))
                .and_then(|m| m.as_str())
                .map(|s| s == "y" || s == "xy")
                .unwrap_or(false);

            let prop = |name: &str| -> String {
                node.find_all("property")
                    .iter()
                    .find(|p| p.get(1).and_then(|n| n.as_str()) == Some(name))
                    .and_then(|p| p.get(2))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string()
            };

            let uuid = node
                .find("uuid")
                .and_then(|u| u.get(1))
                .and_then(|u| u.as_str())
                .map(String::from);

            Some(SymbolInstance {
                reference: prop("Reference"),
                value: prop("Value"),
                footprint: prop("Footprint"),
                lib_id,
                x,
                y,
                rotation,
                mirror_x,
                mirror_y,
                uuid,
            })
        })
        .collect()
}

// ─── Pin in library symbol ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LibPin {
    pub number: String,
    pub name: String,
    /// Position in symbol-local Y-up space (mm).
    pub local_x: f64,
    pub local_y: f64,
    pub rotation: f64,
    pub length: f64,
}

/// Parse pins from a library symbol definition node.
pub fn extract_lib_pins(sym_node: &SexpNode) -> Vec<LibPin> {
    sym_node
        .find_all("pin")
        .iter()
        .filter_map(|node| {
            let (x, y, rotation) = parse_at(node)?;
            let length = node
                .find("length")
                .and_then(|l| l.get_f64(1))
                .unwrap_or(0.0);
            let number = node
                .find("number")
                .and_then(|n| n.get(1))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let name = node
                .find("name")
                .and_then(|n| n.get(1))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            Some(LibPin {
                number,
                name,
                local_x: x,
                local_y: y,
                rotation,
                length,
            })
        })
        .collect()
}

/// Compute the schematic-space pin endpoint (where wires connect) for a lib pin
/// given a component's placement transform.
pub fn pin_endpoint(pin: &LibPin, t: PinTransform) -> (f64, f64) {
    // The wire-connection point is at pin_origin + length in pin direction
    let angle_rad = pin.rotation.to_radians();
    let tip_x = pin.local_x + pin.length * angle_rad.cos();
    let tip_y = pin.local_y + pin.length * angle_rad.sin();
    transform_pin(tip_x, tip_y, t)
}

// ─── T-Junction detection ─────────────────────────────────────────────────────

use crate::geometry::point_on_segment;

/// Given a set of wires, return all positions where a wire endpoint lies
/// strictly in the middle of another wire (T-junction), excluding existing
/// endpoints. These positions require a junction dot.
pub fn find_t_junctions(wires: &[Wire], tol: f64) -> Vec<(f64, f64)> {
    let mut junctions = Vec::new();

    for w1 in wires {
        // Check both endpoints of w1 against all other wires
        for (px, py) in [(w1.x1, w1.y1), (w1.x2, w1.y2)] {
            for w2 in wires {
                if std::ptr::eq(w1, w2) {
                    continue;
                }
                // Point is on w2 but NOT at its endpoints
                let at_endpoint = crate::geometry::points_coincident(px, py, w2.x1, w2.y1, tol)
                    || crate::geometry::points_coincident(px, py, w2.x2, w2.y2, tol);
                if !at_endpoint && point_on_segment(px, py, w2.x1, w2.y1, w2.x2, w2.y2, tol) {
                    // Avoid duplicate junction positions
                    if !junctions.iter().any(|(jx, jy): &(f64, f64)| {
                        crate::geometry::points_coincident(px, py, *jx, *jy, tol)
                    }) {
                        junctions.push((px, py));
                    }
                }
            }
        }
    }

    junctions
}

// ─── S-expression formatters for new elements ─────────────────────────────────

pub fn format_wire(x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    let uuid = crate::writer::new_uuid();
    format!(
        "(wire\n\t\t(pts\n\t\t\t(xy {} {}) (xy {} {})\n\t\t)\n\t\t(stroke\n\t\t\t(width 0)\n\t\t\t(type default)\n\t\t)\n\t\t(uuid \"{}\")\n\t)",
        x1, y1, x2, y2, uuid
    )
}

pub fn format_junction(x: f64, y: f64) -> String {
    let uuid = crate::writer::new_uuid();
    format!(
        "\n  (junction\n    (at {x} {y})\n    (diameter 0)\n    (color 0 0 0 0)\n    (uuid \"{uuid}\")\n  )"
    )
}

pub fn format_net_label(net: &str, x: f64, y: f64, rotation: f64) -> String {
    let uuid = crate::writer::new_uuid();
    format!(
        r#"
  (net_label "{net}"
    (at {x} {y} {rotation})
    (fields_autoplaced yes)
    (effects (font (size 1.27 1.27)) (justify left))
    (uuid "{uuid}")
  )"#
    )
}
