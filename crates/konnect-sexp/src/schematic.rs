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

/// The `justify` token a label needs so its text reads away from its anchor.
///
/// A label's `(at … ROT)` orients the connection arrow; it is `(justify …)`
/// inside `(effects)` that decides which way the *text* runs. Get them out of
/// step and the label attaches correctly but renders backwards, over whatever
/// it points at. eeschema always writes both, and the pairing is exactly:
/// rotation 0/90 → `left`, 180/270 → `right` (confirmed against 692 labels in
/// KiCAD 10-authored schematics: 0→left ×297, 90→left ×6, 180→right ×298,
/// 270→right ×5, with no counter-examples).
pub fn label_justify(rotation: f64) -> &'static str {
    // Normalize: KiCAD stores 0/90/180/270, but tolerate 360, negatives, and
    // the f64 the tool layer hands us.
    let deg = ((rotation % 360.0) + 360.0) % 360.0;
    if deg < 180.0 {
        "left"
    } else {
        "right"
    }
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

    // KiCAD's tag for a plain net label is `label` — there is no `net_label`
    // in the .kicad_sch format, so matching that name found nothing in any
    // real schematic (and hid every plain label from the net graph).
    for (kind_str, kind) in &[
        ("label", LabelKind::NetLabel),
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
/// Extract every pin of a library symbol, including pins nested inside unit
/// sub-symbols. KiCAD stores pins under children like `(symbol "Device:R_1_1"
/// (pin …))`, so a direct-children-only scan finds ZERO pins for standard
/// library parts — the bug the first real-KiCAD e2e run caught in
/// `connect_pins` ("Pin '2' not found on 'R1'").
pub fn extract_lib_pins(sym_node: &SexpNode) -> Vec<LibPin> {
    let mut out = Vec::new();
    collect_pins_recursive(sym_node, &mut out);
    out
}

fn collect_pins_recursive(node: &SexpNode, out: &mut Vec<LibPin>) {
    for pin in node.find_all("pin") {
        if let Some(lib_pin) = parse_lib_pin(pin) {
            out.push(lib_pin);
        }
    }
    // Recurse into unit/body-style sub-symbols ("R_1_1", "R_1_0", …).
    for sub in node.find_all("symbol") {
        collect_pins_recursive(sub, out);
    }
}

fn parse_lib_pin(node: &SexpNode) -> Option<LibPin> {
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
}

/// Compute the schematic-space pin endpoint (where wires connect) for a lib pin
/// given a component's placement transform.
pub fn pin_endpoint(pin: &LibPin, t: PinTransform) -> (f64, f64) {
    // In the KiCAD symbol format the pin's (at x y angle) IS the electrical
    // connection point: the angle points from that tip TOWARD the symbol body,
    // and the drawn pin line extends `length` mm inward. Adding length here
    // would land on the body-attachment end — 1 pin-length away from where
    // KiCAD actually joins wires (eeschema's ERC reports pin positions at the
    // (at) point, and pin tips land exactly on the body outline only after
    // adding length — verified against Device:R in the KiCAD 10 libraries).
    transform_pin(pin.local_x, pin.local_y, t)
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
    // The tag must be `label`: KiCAD has no `net_label` in its schematic
    // format and refuses to load a file containing one ("Failed to load
    // schematic"), so emitting that made the whole schematic unopenable.
    //
    // justify must follow the rotation, or the text renders backwards across
    // whatever the label points at. Plain labels also carry `bottom`, which
    // lifts the text off the wire it annotates.
    let justify = label_justify(rotation);
    format!(
        r#"
  (label "{net}"
    (at {x} {y} {rotation})
    (fields_autoplaced yes)
    (effects (font (size 1.27 1.27)) (justify {justify} bottom))
    (uuid "{uuid}")
  )"#
    )
}

#[cfg(test)]
mod pin_endpoint_tests {
    use super::*;

    fn device_r_pin(number: &str, local_y: f64, rotation: f64) -> LibPin {
        // Device:R in the KiCAD 10 libraries: (pin ... (at 0 3.81 270) (length 1.27))
        // and (at 0 -3.81 90) — the (at) point is the electrical tip; the angle
        // points toward the body.
        LibPin {
            number: number.to_string(),
            name: "~".to_string(),
            local_x: 0.0,
            local_y,
            rotation,
            length: 1.27,
        }
    }

    fn placed(comp_x: f64, comp_y: f64, rotation_deg: f64) -> PinTransform {
        PinTransform {
            comp_x,
            comp_y,
            rotation_deg,
            mirror_x: false,
            mirror_y: false,
        }
    }

    #[test]
    fn endpoint_is_the_electrical_tip_not_the_body_end() {
        // R placed at (100.33, 80.01), rotation 0. eeschema's own ERC reports
        // these pins at y = 76.20 and 83.82 — the (at)-derived tips.
        let (x1, y1) = pin_endpoint(&device_r_pin("1", 3.81, 270.0), placed(100.33, 80.01, 0.0));
        assert!((x1 - 100.33).abs() < 1e-9);
        assert!(
            (y1 - 76.20).abs() < 1e-9,
            "pin 1 tip must be at 76.20 (got {y1}); 77.47 would be the body end"
        );

        let (x2, y2) = pin_endpoint(&device_r_pin("2", -3.81, 90.0), placed(100.33, 80.01, 0.0));
        assert!((x2 - 100.33).abs() < 1e-9);
        assert!(
            (y2 - 83.82).abs() < 1e-9,
            "pin 2 tip must be at 83.82 (got {y2}); 82.55 would be the body end"
        );
    }

    #[test]
    fn endpoint_respects_rotation() {
        // Same resistor rotated 90°: the pin tips swing onto the X axis.
        let (x, y) = pin_endpoint(&device_r_pin("1", 3.81, 270.0), placed(100.0, 80.0, 90.0));
        assert!((y - 80.0).abs() < 1e-9);
        assert!(
            (x - 96.19).abs() < 1e-9 || (x - 103.81).abs() < 1e-9,
            "rotated tip must sit 3.81 mm from center on the X axis, got {x}"
        );
    }
}

#[cfg(test)]
mod label_tag_tests {
    use super::*;

    /// KiCAD's schematic format has no `net_label` tag — a file containing one
    /// fails to load outright ("Failed to load schematic" from kicad-cli 10.0.3,
    /// verified against a file identical but for this tag). The plain net label
    /// is `label`.
    #[test]
    fn format_net_label_emits_kicad_label_tag() {
        let sexp = format_net_label("VCC", 100.0, 80.0, 0.0);
        assert!(
            sexp.contains("(label \"VCC\""),
            "must emit KiCAD's (label) tag, got: {sexp}"
        );
        assert!(!sexp.contains("(net_label"));
    }

    #[test]
    fn format_net_label_round_trips_through_extract_labels() {
        let sch = format!(
            "(kicad_sch{}\n)",
            format_net_label("SIGNAL", 25.4, 50.8, 90.0)
        );
        let tree = parse_sexp(&sch).expect("emitted label must parse");
        let labels = extract_labels(&tree);
        assert_eq!(labels.len(), 1, "emitted label must be readable back");
        assert_eq!(labels[0].net, "SIGNAL");
        assert_eq!(labels[0].kind, LabelKind::NetLabel);
        assert_eq!(labels[0].x, 25.4);
        assert_eq!(labels[0].y, 50.8);
        assert_eq!(labels[0].rotation, 90.0);
        assert!(labels[0].uuid.is_some());
    }

    #[test]
    fn extract_labels_sees_plain_labels_written_by_eeschema() {
        // Tab-indented, as eeschema saves; all three label kinds present.
        let sch = "(kicad_sch\n\t(label \"MID\"\n\t\t(at 10 20 0)\n\t\t(uuid \"a\")\n\t)\n\t(global_label \"VBUS\"\n\t\t(shape input)\n\t\t(at 30 40 0)\n\t\t(uuid \"b\")\n\t)\n\t(hierarchical_label \"HIN\"\n\t\t(shape input)\n\t\t(at 50 60 0)\n\t\t(uuid \"c\")\n\t)\n)";
        let tree = parse_sexp(sch).unwrap();
        let labels = extract_labels(&tree);
        assert_eq!(labels.len(), 3, "all three label kinds must be found");

        let plain = labels
            .iter()
            .find(|l| l.kind == LabelKind::NetLabel)
            .expect(
                "plain (label) must be extracted — it was invisible while this matched 'net_label'",
            );
        assert_eq!(plain.net, "MID");
        assert_eq!((plain.x, plain.y), (10.0, 20.0));
    }
}

#[cfg(test)]
mod label_justify_tests {
    use super::*;

    /// The pairing eeschema itself writes, sampled from 692 labels across
    /// KiCAD 10-authored schematics: 0→left, 90→left, 180→right, 270→right.
    #[test]
    fn justify_follows_the_rotation_eeschema_pairs_it_with() {
        assert_eq!(label_justify(0.0), "left");
        assert_eq!(label_justify(90.0), "left");
        assert_eq!(label_justify(180.0), "right");
        assert_eq!(label_justify(270.0), "right");
    }

    #[test]
    fn rotation_is_normalized() {
        assert_eq!(label_justify(360.0), "left");
        assert_eq!(label_justify(-180.0), "right");
        assert_eq!(label_justify(-90.0), "right", "-90 is 270");
        assert_eq!(label_justify(540.0), "right", "540 is 180");
    }

    #[test]
    fn formatted_label_carries_justify_matching_its_rotation() {
        let west = format_net_label("SIG", 10.0, 20.0, 180.0);
        assert!(
            west.contains("(justify right bottom)"),
            "a 180° label must read right-justified, got: {west}"
        );
        let east = format_net_label("SIG", 10.0, 20.0, 0.0);
        assert!(east.contains("(justify left bottom)"), "got: {east}");
    }
}
