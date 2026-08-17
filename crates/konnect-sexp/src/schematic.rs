//! Higher-level schematic helpers built on the parser and writer.
//!
//! Provides typed query functions used by the tool implementations.

use crate::geometry::{transform_pin, PinTransform};
use crate::parser::{parse_sexp, SexpNode};
use crate::writer::read_consistent;
use crate::SexpError;
use std::path::Path;

// ─── Schematic file I/O ───────────────────────────────────────────────────────

/// Create a minimal standalone schematic with a fresh root UUID.
#[must_use]
pub fn format_blank_schematic() -> String {
    format!(
        "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(uuid \"{}\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n",
        crate::writer::new_uuid()
    )
}

pub fn read_schematic(path: &Path) -> Result<(String, SexpNode), SexpError> {
    let content = read_consistent(path)?;
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
    extract_schematic_lines(tree, "wire")
}

/// Extract all bus segments using the same point representation as wires.
pub fn extract_buses(tree: &SexpNode) -> Vec<Wire> {
    extract_schematic_lines(tree, "bus")
}

fn extract_schematic_lines(tree: &SexpNode, kind: &str) -> Vec<Wire> {
    tree.find_all(kind)
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

/// Extract all junction dot positions from a parsed schematic tree.
pub fn extract_junctions(tree: &SexpNode) -> Vec<(f64, f64)> {
    tree.find_all("junction")
        .iter()
        .filter_map(|node| {
            let at = node.find("at")?;
            Some((at.get_f64(1)?, at.get_f64(2)?))
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
    /// Name of the `lib_symbols` entry this instance actually resolves through,
    /// when KiCAD wrote a `(lib_name …)` because the sheet carries a locally
    /// edited copy of the library symbol. Resolve with
    /// [`SymbolInstance::lib_symbol_name`] or [`find_lib_symbol`] — matching on
    /// `lib_id` alone picks the wrong definition, or none (#143).
    pub lib_name: Option<String>,
    pub lib_id: String,
    pub x: f64,
    pub y: f64,
    pub rotation: f64,
    pub mirror_x: bool,
    pub mirror_y: bool,
    pub uuid: Option<String>,
    /// Selected unit of a multi-unit symbol (`(unit N)`, 1-based). Defaults
    /// to 1 when the instance carries no unit — eeschema always writes one.
    pub unit: u32,
}

impl SymbolInstance {
    /// The `lib_symbols` entry name this instance resolves through: `lib_name`
    /// when KiCAD wrote one, otherwise `lib_id`. Mirrors eeschema's
    /// `SCH_SYMBOL::GetSchSymbolLibraryName()`.
    pub fn lib_symbol_name(&self) -> &str {
        self.lib_name.as_deref().unwrap_or(&self.lib_id)
    }

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
            let lib_name = node
                .find("lib_name")
                .and_then(|n| n.get(1))
                .and_then(|n| n.as_str())
                .map(String::from);
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

            let unit = node.find_f64("unit").map(|u| u as u32).unwrap_or(1);

            Some(SymbolInstance {
                reference: prop("Reference"),
                value: prop("Value"),
                footprint: prop("Footprint"),
                lib_name,
                lib_id,
                x,
                y,
                rotation,
                mirror_x,
                mirror_y,
                uuid,
                unit,
            })
        })
        .collect()
}

/// Resolve an instance's embedded `lib_symbols` definition the way KiCAD does:
/// by `lib_name` when the instance carries one, otherwise by `lib_id`.
///
/// `lib_syms` is the `find_all("symbol")` list of the sheet's `lib_symbols`
/// node. Matching on `lib_id` alone silently returns the *base* definition for
/// a locally edited symbol — whose pins can sit at different coordinates — or
/// nothing at all when the base was never embedded (#143).
pub fn find_lib_symbol<'a>(
    lib_syms: &[&'a SexpNode],
    inst: &SymbolInstance,
) -> Option<&'a SexpNode> {
    let want = inst.lib_symbol_name();
    lib_syms
        .iter()
        .copied()
        .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(want))
}

// ─── Pin in library symbol ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LibPin {
    pub number: String,
    pub name: String,
    /// KiCad electrical pin type (`input`, `power_in`, `no_connect`, ...).
    pub electrical_type: String,
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

/// Unit-aware variant of [`extract_lib_pins`] for multi-unit symbols (#35).
///
/// KiCAD names unit sub-symbols `Name_N_M` where `N` is the unit number
/// (`0` = drawn on every unit) and `M` is the body style. The unit-agnostic
/// [`extract_lib_pins`] superimposes every unit's pins onto one placement —
/// for an LM2904 that reports both op-amps' pins at the same instance. This
/// variant keeps only pins from sub-symbols where `N == 0` or `N == unit`
/// (pins directly on the symbol node, or in sub-symbols without the `_N_M`
/// suffix, are always kept).
pub fn extract_lib_pins_for_unit(sym_node: &SexpNode, unit: u32) -> Vec<LibPin> {
    let mut out = Vec::new();
    for pin in sym_node.find_all("pin") {
        if let Some(lib_pin) = parse_lib_pin(pin) {
            out.push(lib_pin);
        }
    }
    for sub in sym_node.find_all("symbol") {
        let sub_unit = sub
            .get(1)
            .and_then(|n| n.as_str())
            .and_then(parse_subsymbol_unit);
        match sub_unit {
            Some(n) if n != 0 && n != unit => {} // another unit's pins: skip
            // n == 0 (common), n == unit, or an un-suffixed name: keep all.
            _ => collect_pins_recursive(sub, &mut out),
        }
    }
    out
}

/// Parse the unit number `N` out of a `Name_N_M` sub-symbol name. Returns
/// `None` when the name doesn't end in two `_`-separated integers (base names
/// may themselves contain underscores and digits, e.g. `R_Small_1_1` → 1).
pub fn parse_subsymbol_unit(name: &str) -> Option<u32> {
    let mut it = name.rsplitn(3, '_');
    let _style: u32 = it.next()?.parse().ok()?;
    let unit: u32 = it.next()?.parse().ok()?;
    it.next()?; // a base name must exist before the suffix
    Some(unit)
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
    let electrical_type = node
        .get(1)
        .and_then(|pin_type| pin_type.as_str())
        .unwrap_or("unspecified")
        .to_string();
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
        electrical_type,
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

/// The on-screen direction leading *away* from the symbol body at this pin —
/// where a label or wire stub belongs. One of 0/90/180/270.
///
/// A KiCad pin's own angle points from its tip toward the body (see
/// [`pin_endpoint`]), so outward is the opposite; [`transform_direction`]
/// then applies the instance's rotation and mirror.
pub fn pin_outward_direction(pin: &LibPin, t: PinTransform) -> f64 {
    crate::geometry::transform_direction(pin.rotation + 180.0, t)
}

/// The rotation a label at this pin's endpoint needs so its text reads away
/// from the symbol body instead of across it. Pairs with [`label_justify`].
///
/// Horizontal pins take the outward direction — not our convention: across the
/// 115 KiCad 10 demo schematics all 249 labels on a horizontal pin do this,
/// with no counter-examples (32 of 33 on mirrored instances included).
///
/// Vertical pins keep the text horizontal, since that corpus never rotates a
/// pin-anchored label to 90 or 270. It splits 6/4 on *which* horizontal, so
/// `0` is our tie-break, not eeschema's.
pub fn pin_label_rotation(pin: &LibPin, t: PinTransform) -> f64 {
    horizontal_label_rotation(pin_outward_direction(pin, t))
}

/// Keep a label's text horizontal: pass 0/180 through, fold 90/270 to 0.
///
/// Shared with the wire-stub paths, whose `direction` argument can also ask
/// for an up/down stub. See [`pin_label_rotation`] for the corpus evidence.
pub fn horizontal_label_rotation(direction: f64) -> f64 {
    if direction.rem_euclid(360.0) == 180.0 {
        180.0
    } else {
        0.0
    }
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
    format_schematic_line("wire", x1, y1, x2, y2)
}

pub fn format_bus(x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    format_schematic_line("bus", x1, y1, x2, y2)
}

fn format_schematic_line(kind: &str, x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    let uuid = crate::writer::new_uuid();
    format!(
        "({kind}\n\t\t(pts\n\t\t\t(xy {x1} {y1}) (xy {x2} {y2})\n\t\t)\n\t\t(stroke\n\t\t\t(width 0)\n\t\t\t(type default)\n\t\t)\n\t\t(uuid \"{uuid}\")\n\t)"
    )
}

pub fn format_junction(x: f64, y: f64) -> String {
    let uuid = crate::writer::new_uuid();
    format!(
        "\n  (junction\n    (at {x} {y})\n    (diameter 0)\n    (color 0 0 0 0)\n    (uuid \"{uuid}\")\n  )"
    )
}

pub fn format_no_connect(x: f64, y: f64) -> String {
    let uuid = crate::writer::new_uuid();
    format!("\n  (no_connect\n    (at {x} {y})\n    (uuid \"{uuid}\")\n  )")
}

/// Orientation of a graphical wire-to-bus entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusEntryDirection {
    DownRight,
    DownLeft,
    UpRight,
    UpLeft,
}

impl BusEntryDirection {
    #[must_use]
    pub const fn size(self) -> (f64, f64) {
        match self {
            Self::DownRight => (2.54, 2.54),
            Self::DownLeft => (-2.54, 2.54),
            Self::UpRight => (2.54, -2.54),
            Self::UpLeft => (-2.54, -2.54),
        }
    }

    #[must_use]
    pub const fn rotated_clockwise(self) -> Self {
        match self {
            Self::DownRight => Self::DownLeft,
            Self::DownLeft => Self::UpLeft,
            Self::UpLeft => Self::UpRight,
            Self::UpRight => Self::DownRight,
        }
    }
}

pub fn format_bus_entry(x: f64, y: f64, direction: BusEntryDirection) -> String {
    let uuid = crate::writer::new_uuid();
    let (width, height) = direction.size();
    format!(
        "\n  (bus_entry\n    (at {x} {y})\n    (size {width} {height})\n    (stroke\n      (width 0)\n      (type default)\n    )\n    (uuid \"{uuid}\")\n  )"
    )
}

/// Data required for one parent-side hierarchical sheet reference.
#[derive(Debug, Clone, Copy)]
pub struct HierarchicalSheetSpec<'a> {
    pub name: &'a str,
    pub file: &'a str,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub project_name: &'a str,
    pub parent_instance_path: &'a str,
    pub page: &'a str,
}

pub fn format_hierarchical_sheet(spec: HierarchicalSheetSpec<'_>) -> String {
    let uuid = crate::writer::new_uuid();
    let name = escape_quoted_text(spec.name);
    let file = escape_quoted_text(spec.file);
    let project_name = escape_quoted_text(spec.project_name);
    let parent_instance_path = escape_quoted_text(spec.parent_instance_path);
    let page = escape_quoted_text(spec.page);
    let name_y = spec.y - 0.635;
    let file_y = spec.y + spec.height + 0.635;
    format!(
        r#"
  (sheet
    (at {x} {y})
    (size {width} {height})
    (fields_autoplaced yes)
    (stroke (width 0.1524) (type solid))
    (fill (color 0 0 0 0.0))
    (uuid "{uuid}")
    (property "Sheetname" "{name}"
      (at {x} {name_y} 0)
      (effects (font (size 1.27 1.27)) (justify left bottom))
    )
    (property "Sheetfile" "{file}"
      (at {x} {file_y} 0)
      (effects (font (size 1.27 1.27)) (justify left top))
    )
    (instances
      (project "{project_name}"
        (path "{parent_instance_path}" (page "{page}"))
      )
    )
  )"#,
        x = spec.x,
        y = spec.y,
        width = spec.width,
        height = spec.height,
    )
}

/// Electrical direction of a hierarchical-sheet pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SheetPinType {
    Input,
    Output,
    Bidirectional,
    TriState,
    Passive,
}

impl SheetPinType {
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::Bidirectional => "bidirectional",
            Self::TriState => "tri_state",
            Self::Passive => "passive",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Input => Self::Output,
            Self::Output => Self::Bidirectional,
            Self::Bidirectional => Self::TriState,
            Self::TriState => Self::Passive,
            Self::Passive => Self::Input,
        }
    }
}

pub fn format_sheet_pin(
    name: &str,
    pin_type: SheetPinType,
    x: f64,
    y: f64,
    rotation: f64,
) -> String {
    let name = escape_quoted_text(name);
    let uuid = crate::writer::new_uuid();
    format!(
        "(pin \"{name}\" {}\n\t(at {x} {y} {rotation})\n\t(uuid \"{uuid}\")\n)",
        pin_type.keyword()
    )
}

fn escape_quoted_text(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn format_net_label(net: &str, x: f64, y: f64, rotation: f64) -> String {
    let uuid = crate::writer::new_uuid();
    let net = escape_quoted_text(net);
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
mod unit_pin_tests {
    use super::*;

    /// A 2-unit op-amp shaped like LM2904: unit 1 has pins 1-3, unit 2 has
    /// pins 5-7, and the power pins 4/8 live in a `_0_1` sub-symbol common to
    /// all units.
    fn two_unit_symbol() -> SexpNode {
        let pin = |num: &str, y: f64| {
            format!(
                "(pin passive line (at -7.62 {y} 0) (length 2.54)\n  (name \"~\" (effects (font (size 1.27 1.27))))\n  (number \"{num}\" (effects (font (size 1.27 1.27))))\n)"
            )
        };
        let sch = format!(
            "(kicad_symbol_lib\n  (symbol \"OP_DUAL\"\n    (symbol \"OP_DUAL_0_1\"\n      {}{}\n    )\n    (symbol \"OP_DUAL_1_1\"\n      {}{}{}\n    )\n    (symbol \"OP_DUAL_2_1\"\n      {}{}{}\n    )\n  )\n)",
            pin("4", -10.16),
            pin("8", 10.16),
            pin("1", 0.0),
            pin("2", 2.54),
            pin("3", 5.08),
            pin("5", 0.0),
            pin("6", 2.54),
            pin("7", 5.08),
        );
        parse_sexp(&sch).unwrap()
    }

    fn numbers(pins: &[LibPin]) -> Vec<String> {
        let mut n: Vec<String> = pins.iter().map(|p| p.number.clone()).collect();
        n.sort();
        n
    }

    #[test]
    fn for_unit_keeps_own_and_common_pins_only() {
        let root = two_unit_symbol();
        let sym = root.find("symbol").unwrap();

        let u1 = extract_lib_pins_for_unit(sym, 1);
        assert_eq!(
            numbers(&u1),
            vec!["1", "2", "3", "4", "8"],
            "unit 1 = its own pins + the _0_1 commons"
        );

        let u2 = extract_lib_pins_for_unit(sym, 2);
        assert_eq!(
            numbers(&u2),
            vec!["4", "5", "6", "7", "8"],
            "unit 2 = its own pins + the _0_1 commons"
        );

        // The signal pin sets are disjoint — before this function, both units
        // reported all 8 pins superimposed (#35).
        assert!(!numbers(&u1).contains(&"5".to_string()));
        assert!(!numbers(&u2).contains(&"1".to_string()));
    }

    #[test]
    fn unit_agnostic_extraction_still_returns_everything() {
        let root = two_unit_symbol();
        let sym = root.find("symbol").unwrap();
        assert_eq!(
            numbers(&extract_lib_pins(sym)),
            vec!["1", "2", "3", "4", "5", "6", "7", "8"]
        );
    }

    #[test]
    fn extraction_preserves_the_electrical_pin_type() {
        let root = parse_sexp(
            r#"(kicad_symbol_lib
  (symbol "TEST"
    (pin no_connect line (at 0 0 0) (length 0)
      (name "NC" (effects (font (size 1.27 1.27))))
      (number "1" (effects (font (size 1.27 1.27)))))))"#,
        )
        .unwrap();
        let pin = extract_lib_pins(root.find("symbol").unwrap())
            .into_iter()
            .next()
            .unwrap();

        assert_eq!(pin.electrical_type, "no_connect");
    }

    #[test]
    fn underscored_base_names_parse_their_unit_suffix() {
        assert_eq!(parse_subsymbol_unit("R_Small_1_1"), Some(1));
        assert_eq!(parse_subsymbol_unit("OP_DUAL_2_1"), Some(2));
        assert_eq!(parse_subsymbol_unit("X_0_1"), Some(0));
        // No trailing _N_M suffix → None (pins are then always kept).
        assert_eq!(parse_subsymbol_unit("R"), None);
        assert_eq!(parse_subsymbol_unit("R_1"), None);
        assert_eq!(parse_subsymbol_unit("Name_A_1"), None);
    }
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
            electrical_type: "passive".to_string(),
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

    #[test]
    fn format_no_connect_emits_a_parseable_uuid_item() {
        let sexp = format_no_connect(12.7, 25.4);
        let tree = parse_sexp(&format!("(kicad_sch{sexp}\n)")).unwrap();
        let item = tree.find("no_connect").expect("no-connect item");
        assert_eq!(parse_at(item), Some((12.7, 25.4, 0.0)));
        assert!(item.find("uuid").is_some());
    }

    #[test]
    fn format_bus_entry_uses_typed_direction_and_parseable_geometry() {
        let sexp = format_bus_entry(10.16, 20.32, BusEntryDirection::UpLeft);
        let tree = parse_sexp(&format!("(kicad_sch{sexp}\n)")).unwrap();
        let entry = tree.find("bus_entry").expect("bus entry");
        assert_eq!(parse_at(entry), Some((10.16, 20.32, 0.0)));
        let size = entry.find("size").expect("entry size");
        assert_eq!(size.get_f64(1), Some(-2.54));
        assert_eq!(size.get_f64(2), Some(-2.54));
        assert!(entry.find("uuid").is_some());
    }

    #[test]
    fn format_bus_emits_a_parseable_uuid_line() {
        let sexp = format_bus(1.27, 2.54, 25.4, 2.54);
        let tree = parse_sexp(&format!("(kicad_sch\n\t{sexp}\n)")).unwrap();
        let bus = tree.find("bus").expect("bus line");
        assert!(bus.find("pts").is_some());
        assert!(bus.find("uuid").is_some());
    }

    #[test]
    fn format_hierarchical_sheet_positions_fields_and_escapes_metadata() {
        let sexp = format_hierarchical_sheet(HierarchicalSheetSpec {
            name: "Power \\\"A\\\"",
            file: "power_a.kicad_sch",
            x: 25.4,
            y: 50.8,
            width: 76.2,
            height: 50.8,
            project_name: "Pack",
            parent_instance_path: "/root-uuid",
            page: "2",
        });
        let tree = parse_sexp(&format!("(kicad_sch{sexp}\n)")).unwrap();
        let sheet = tree.find("sheet").expect("sheet");
        assert_eq!(parse_at(sheet), Some((25.4, 50.8, 0.0)));
        assert_eq!(sheet.find_all("property").len(), 2);
        assert!(sheet
            .find_all("property")
            .iter()
            .all(|property| property.find("at").is_some()));
        assert_eq!(
            sheet.find_all("property")[0]
                .get(2)
                .and_then(SexpNode::as_str),
            Some("Power \\\"A\\\"")
        );
        assert_eq!(
            sheet
                .find("instances")
                .and_then(|instances| instances.find("project"))
                .and_then(|project| project.find("path"))
                .and_then(|path| path.find("page"))
                .and_then(|page| page.get(1))
                .and_then(SexpNode::as_str),
            Some("2")
        );
    }

    #[test]
    fn format_sheet_pin_uses_a_typed_direction_and_escapes_its_name() {
        let source = format_sheet_pin("DATA \"A\"", SheetPinType::Bidirectional, 10.0, 20.0, 180.0);
        let pin = parse_sexp(&source).expect("sheet pin parses");
        assert_eq!(pin.head(), Some("pin"));
        assert_eq!(pin.get(1).and_then(SexpNode::as_str), Some("DATA \"A\""));
        assert_eq!(pin.get(2).and_then(SexpNode::as_str), Some("bidirectional"));
        assert_eq!(parse_at(&pin), Some((10.0, 20.0, 180.0)));
        assert!(pin.find("uuid").is_some());
    }

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
    fn format_net_label_escapes_user_text() {
        let sch = format!("(kicad_sch{}\n)", format_net_label("A\\\"B", 1.0, 2.0, 0.0));
        let tree = parse_sexp(&sch).expect("escaped label parses");
        assert_eq!(extract_labels(&tree)[0].net, "A\\\"B");
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

#[cfg(test)]
mod pin_label_rotation_tests {
    use super::*;

    /// One pin per edge of a square IC body, in the KiCad convention where a
    /// pin's angle points from its tip toward the body.
    fn edge_pin(angle: f64) -> LibPin {
        let rad = angle.to_radians();
        LibPin {
            number: "1".into(),
            name: "PIN".into(),
            electrical_type: "passive".into(),
            // Tip sits 10 mm out from the origin, opposite the way it points.
            local_x: -10.0 * rad.cos(),
            local_y: -10.0 * rad.sin(),
            rotation: angle,
            length: 2.54,
        }
    }

    fn placed(rotation_deg: f64, mirror_x: bool, mirror_y: bool) -> PinTransform {
        PinTransform {
            comp_x: 100.0,
            comp_y: 100.0,
            rotation_deg,
            mirror_x,
            mirror_y,
        }
    }

    #[test]
    fn horizontal_pins_point_their_label_away_from_the_body() {
        let t = placed(0.0, false, false);
        // Left-edge pin (angle 0, body to its right): text must run left.
        assert_eq!(pin_label_rotation(&edge_pin(0.0), t), 180.0);
        // Right-edge pin (angle 180, body to its left): text runs right.
        assert_eq!(pin_label_rotation(&edge_pin(180.0), t), 0.0);
    }

    #[test]
    fn vertical_pins_keep_their_label_horizontal() {
        let t = placed(0.0, false, false);
        // Outward is north and south respectively, but no pin-anchored label
        // in the KiCad demo corpus is rotated 90 or 270.
        assert_eq!(pin_outward_direction(&edge_pin(270.0), t), 90.0);
        assert_eq!(pin_label_rotation(&edge_pin(270.0), t), 0.0);
        assert_eq!(pin_outward_direction(&edge_pin(90.0), t), 270.0);
        assert_eq!(pin_label_rotation(&edge_pin(90.0), t), 0.0);
    }

    #[test]
    fn rotating_the_symbol_carries_the_label_with_it() {
        // A left-edge pin on a symbol turned 180° is now on the right.
        assert_eq!(
            pin_label_rotation(&edge_pin(0.0), placed(180.0, false, false)),
            0.0
        );
        // Turned 90°, the pin is vertical, so the text stays horizontal.
        assert_eq!(
            pin_label_rotation(&edge_pin(0.0), placed(90.0, false, false)),
            0.0
        );
    }

    #[test]
    fn mirroring_the_symbol_flips_left_and_right() {
        let mirrored = placed(0.0, false, true);
        assert_eq!(pin_label_rotation(&edge_pin(0.0), mirrored), 0.0);
        assert_eq!(pin_label_rotation(&edge_pin(180.0), mirrored), 180.0);
        // Mirroring about the other axis leaves a horizontal pin alone.
        assert_eq!(
            pin_label_rotation(&edge_pin(0.0), placed(0.0, true, false)),
            180.0
        );
    }

    /// The thing the user actually sees: a left-edge pin's label must be
    /// right-justified, so its text runs away from the pin names inside the body.
    #[test]
    fn justify_pairs_with_the_derived_rotation() {
        let t = placed(0.0, false, false);
        assert_eq!(
            label_justify(pin_label_rotation(&edge_pin(0.0), t)),
            "right"
        );
        assert_eq!(
            label_justify(pin_label_rotation(&edge_pin(180.0), t)),
            "left"
        );
    }

    #[test]
    fn folding_a_direction_keeps_only_west() {
        assert_eq!(horizontal_label_rotation(0.0), 0.0);
        assert_eq!(horizontal_label_rotation(180.0), 180.0);
        assert_eq!(horizontal_label_rotation(90.0), 0.0);
        assert_eq!(horizontal_label_rotation(270.0), 0.0);
        assert_eq!(horizontal_label_rotation(-180.0), 180.0);
    }
}
