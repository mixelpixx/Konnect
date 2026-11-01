//! `move_region`'s rigid block move (`include: "all"`, #896).
//!
//! Everything inside the box moves by one `(dx, dy)`, so connectivity inside
//! the box cannot change. What can change is the boundary, and none of
//! eeschema's drag rules (#315) are needed to guard it: a wire, bus, bus entry
//! or sheet that crosses the edge refuses the move, and the pin nets of the
//! whole sheet must come out of the move exactly as they went in.

use crate::tools::sch_connectivity::{net_graph_for, NetRoot, COINCIDENT_TOLERANCE};
use crate::tools::sch_wiring::NoConnectCarry;
use konnect_schematic_editor as cse;
use konnect_schematic_editor::sexp::{atom, SexpNode as RawNode};
use konnect_schematic_editor::types::{fmt_f64, translate_field_ats};
use konnect_sexp::geometry::points_coincident;
use konnect_sexp::schematic::{extract_all_net_labels, extract_wires, parse_at, pin_endpoint};
use konnect_sexp::SexpNode;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The selection box, edges included, as `within_rectangle` reads it.
#[derive(Clone, Copy)]
pub(crate) struct Region {
    xmin: f64,
    xmax: f64,
    ymin: f64,
    ymax: f64,
}

impl Region {
    pub(crate) fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Self {
        Region {
            xmin: x1.min(x2),
            xmax: x1.max(x2),
            ymin: y1.min(y2),
            ymax: y1.max(y2),
        }
    }

    fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= self.xmin && x <= self.xmax && y >= self.ymin && y <= self.ymax
    }

    /// For an item drawn through `points`, segment by segment: `Some(true)`
    /// when it lies wholly inside, `Some(false)` when wholly outside, and
    /// `None` when it crosses the edge, including a segment that passes
    /// through with both ends outside.
    fn holds(&self, points: &[(f64, f64)]) -> Option<bool> {
        let inside = points.iter().filter(|&&p| self.contains(p)).count();
        match inside {
            0 if points.windows(2).any(|ends| self.meets(ends[0], ends[1])) => None,
            0 => Some(false),
            n if n == points.len() => Some(true),
            _ => None,
        }
    }

    /// Whether the segment from `a` to `b` touches the box (Liang-Barsky).
    fn meets(&self, a: (f64, f64), b: (f64, f64)) -> bool {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let (mut enter, mut leave) = (0.0_f64, 1.0_f64);
        for (p, q) in [
            (-dx, a.0 - self.xmin),
            (dx, self.xmax - a.0),
            (-dy, a.1 - self.ymin),
            (dy, self.ymax - a.1),
        ] {
            if p == 0.0 {
                if q < 0.0 {
                    return false;
                }
            } else if p < 0.0 {
                enter = enter.max(q / p);
            } else {
                leave = leave.min(q / p);
            }
        }
        enter <= leave
    }
}

/// What a rigid move translated, apart from the symbols the caller moves.
pub(crate) struct BlockMove {
    /// Items moved, by kind, for the response.
    pub(crate) counts: BTreeMap<&'static str, usize>,
    /// Each moved item's UUID and the anchor the written file must hold:
    /// its `(at …)`, or its first `(xy …)` for a wire or bus.
    pub(crate) anchors: Vec<(String, (f64, f64))>,
    /// No-connects on no pin, moved with the box. Markers on a pin are left to
    /// `carry_no_connects`, which keeps them on that pin wherever it goes.
    pub(crate) free_no_connects: Vec<NoConnectCarry>,
}

/// Translate every non-symbol item inside `region` by `(dx, dy)`, or return
/// why the box cannot move rigidly. Nothing is changed on refusal.
///
/// `placed` is every symbol's pins as loaded. A symbol moves when its origin
/// is in the box, so one with a pin on the other side of the edge is refused.
pub(crate) fn translate_block(
    sch: &mut cse::Schematic,
    region: Region,
    dx: f64,
    dy: f64,
    placed: &crate::tools::PlacedPinsByReference,
) -> Result<BlockMove, Vec<String>> {
    let mut crossings = Vec::new();
    let mut pins = Vec::new();
    for (inst, placed) in placed {
        let moves = region.contains((inst.x, inst.y));
        let ends: Vec<_> = placed
            .iter()
            .map(|(pin, transform)| pin_endpoint(pin, *transform))
            .collect();
        if ends.iter().any(|&end| region.contains(end) != moves) {
            crossings.push(format!(
                "symbol {} {} crosses the region edge",
                inst.reference,
                inst.uuid.as_deref().unwrap_or("")
            ));
        }
        pins.extend(ends);
    }
    for wire in sch.wires.iter() {
        if region.holds(&[wire.start, wire.end]).is_none() {
            crossings.push(format!(
                "wire {} from ({}, {}) to ({}, {}) crosses the region edge",
                wire.uuid, wire.start.0, wire.start.1, wire.end.0, wire.end.1
            ));
        }
    }
    for node in &sch.raw_other {
        let Some((_, points)) = raw_points(node) else {
            continue;
        };
        let uuid = node.get_value("uuid").unwrap_or("");
        let tag = node.tag().unwrap_or("");
        match points {
            // Geometry that cannot be read cannot be proven inside or out.
            None => crossings.push(format!("{tag} {uuid} has no readable position")),
            Some(points) if region.holds(&points).is_none() => {
                crossings.push(format!("{tag} {uuid} crosses the region edge"))
            }
            Some(_) => {}
        }
    }
    for sheet in sch.sheets.iter() {
        let (x0, y0) = (sheet.at.x, sheet.at.y);
        let (x1, y1) = (x0 + sheet.width, y0 + sheet.height);
        let overlaps =
            x0 <= region.xmax && x1 >= region.xmin && y0 <= region.ymax && y1 >= region.ymin;
        if overlaps && !(region.contains((x0, y0)) && region.contains((x1, y1))) {
            crossings.push(format!("sheet {} crosses the region edge", sheet.uuid));
        }
    }
    if !crossings.is_empty() {
        return Err(crossings);
    }

    let mut moved = BlockMove {
        counts: BTreeMap::new(),
        anchors: Vec::new(),
        free_no_connects: Vec::new(),
    };
    let mut record = |kind: &'static str, uuid: &str, anchor: (f64, f64)| {
        *moved.counts.entry(kind).or_default() += 1;
        // KiCad writes a UUID on every item; one without has nothing for the
        // readback to find it by.
        if !uuid.is_empty() {
            moved
                .anchors
                .push((uuid.to_owned(), (anchor.0 + dx, anchor.1 + dy)));
        }
    };

    for wire in sch.wires.iter_mut() {
        if region.contains(wire.start) {
            record("wires", &wire.uuid, wire.start);
            wire.translate(dx, dy);
        }
    }
    for node in &mut sch.raw_other {
        let Some((kind, Some(points))) = raw_points(node) else {
            continue;
        };
        if region.holds(&points) == Some(true) {
            record(kind, node.get_value("uuid").unwrap_or(""), points[0]);
            shift_raw(node, dx, dy);
        }
    }
    for label in sch.labels.iter_mut() {
        if region.contains(label.position()) {
            record("labels", &label.uuid, label.position());
            label.translate(dx, dy);
        }
    }
    for label in sch.global_labels.iter_mut() {
        if region.contains(label.position()) {
            record("global_labels", &label.uuid, label.position());
            label.translate(dx, dy);
        }
    }
    for label in sch.hierarchical_labels.iter_mut() {
        if region.contains(label.position()) {
            record("hierarchical_labels", &label.uuid, label.position());
            label.translate(dx, dy);
        }
    }
    for junction in &mut sch.junctions {
        if region.contains(junction.position()) {
            record("junctions", &junction.uuid, junction.position());
            junction.translate(dx, dy);
        }
    }
    for text in &mut sch.texts {
        if region.contains(text.position()) {
            record("texts", &text.uuid, text.position());
            text.translate(dx, dy);
        }
    }
    for sheet in sch.sheets.iter_mut() {
        if region.contains(sheet.position()) {
            record("sheets", &sheet.uuid, sheet.position());
            sheet.translate(dx, dy);
        }
    }
    for marker in &mut sch.no_connects {
        let from = (marker.x, marker.y);
        let on_pin = pins
            .iter()
            .any(|&(px, py)| points_coincident(px, py, from.0, from.1, COINCIDENT_TOLERANCE));
        if region.contains(from) && !on_pin {
            marker.x += dx;
            marker.y += dy;
            moved.free_no_connects.push(NoConnectCarry {
                uuid: marker.uuid.clone(),
                from,
                to: (marker.x, marker.y),
            });
        }
    }
    Ok(moved)
}

type Points = Vec<(f64, f64)>;

/// The points of an item the typed model keeps only as a raw node: a bus, a
/// bus entry, a directive label or a text box. `None` for any other node, and
/// no points when they cannot be read.
fn raw_points(node: &RawNode) -> Option<(&'static str, Option<Points>)> {
    let xy = |node: &RawNode| -> Option<(f64, f64)> {
        let args = node.scalar_args();
        Some((args.first()?.parse().ok()?, args.get(1)?.parse().ok()?))
    };
    match node.tag()? {
        "bus" => Some((
            "buses",
            node.find("pts").and_then(|pts| {
                let points: Option<Vec<_>> = pts.find_all("xy").into_iter().map(xy).collect();
                points.filter(|points| !points.is_empty())
            }),
        )),
        "bus_entry" => Some((
            "bus_entries",
            node.find("at")
                .and_then(xy)
                .zip(node.find("size").and_then(xy))
                .map(|(at, size)| vec![at, (at.0 + size.0, at.1 + size.1)]),
        )),
        // Placed by their anchor, as labels and texts are.
        tag @ ("netclass_flag" | "text_box") => Some((
            if tag == "text_box" {
                "text_boxes"
            } else {
                "directive_labels"
            },
            node.find("at").and_then(xy).map(|at| vec![at]),
        )),
        _ => None,
    }
}

/// Shift a bus's `(xy …)` points, or another item's `(at …)` and its fields'.
/// A `(size …)` is relative and stays.
fn shift_raw(node: &mut RawNode, dx: f64, dy: f64) {
    if let Some(RawNode::List(points)) = node.find_mut("pts") {
        for point in points.iter_mut().skip(1) {
            let RawNode::List(items) = point else {
                continue;
            };
            for (index, delta) in [(1, dx), (2, dy)] {
                if let Some(value) = items.get(index).and_then(|n| n.text()?.parse::<f64>().ok()) {
                    items[index] = atom(fmt_f64(value + delta));
                }
            }
        }
    }
    if let RawNode::List(children) = node {
        translate_field_ats(children, dx, dy);
        for field in children
            .iter_mut()
            .filter(|child| child.tag() == Some("property"))
        {
            if let RawNode::List(parts) = field {
                translate_field_ats(parts, dx, dy);
            }
        }
    }
}

/// A pin by the identity a move preserves: its owner's UUID, unit, and its
/// place in the owner's pin list (both trees embed the same definitions).
type PinKey = (String, u32, usize);

/// Each pin's net root, net name, and how a refusal names the pin.
type PinNets = BTreeMap<PinKey, (NetRoot, Option<String>, String)>;

/// Every symbol and sheet pin's net root and net name, with how a refusal
/// names it. `None` when a symbol's library pins do not resolve, since an
/// unseen pin cannot be shown to keep its net.
fn pin_nets(tree: &SexpNode) -> Option<PinNets> {
    let wires = extract_wires(tree);
    let labels = extract_all_net_labels(tree);
    let mut graph = net_graph_for(tree, &wires, &labels);
    let mut nets = BTreeMap::new();
    let mut add = |key: PinKey, (x, y): (f64, f64), label: String| {
        let root = graph.root_at(x, y);
        nets.insert(key, (root, graph.name_of_root(root), label));
    };
    for (inst, pins) in crate::tools::resolved_placed_pins_by_reference(tree)? {
        let symbol = inst.uuid.clone().unwrap_or_else(|| inst.reference.clone());
        for (ordinal, (pin, transform)) in pins.into_iter().enumerate() {
            let label = format!("{} pin {}", inst.reference, pin.number);
            add(
                (symbol.clone(), inst.unit, ordinal),
                pin_endpoint(&pin, transform),
                label,
            );
        }
    }
    // A sheet pin carries a net into its child, so a sheet net with no symbol
    // pin on this sheet still counts.
    for sheet in tree.find_all("sheet") {
        let uuid = sheet.find_str("uuid").unwrap_or("").to_owned();
        for (ordinal, pin) in sheet.find_all("pin").into_iter().enumerate() {
            let Some((x, y, _)) = parse_at(pin) else {
                continue;
            };
            let name = pin.get(1).and_then(|name| name.as_str()).unwrap_or("");
            add(
                (uuid.clone(), 0, ordinal),
                (x, y),
                format!("sheet {uuid} pin {name}"),
            );
        }
    }
    Some(nets)
}

/// How the move would change the sheet's pin nets, one line per net; empty
/// when it changes none.
///
/// Pins are compared by the net they share with other pins and by the net's
/// name, never by position, since every pin in the box moves.
pub(crate) fn pin_net_changes(before: &SexpNode, after: &SexpNode) -> Vec<String> {
    let (Some(before), Some(after)) = (pin_nets(before), pin_nets(after)) else {
        return vec![crate::tools::UNRESOLVED_PIN_GEOMETRY.to_owned()];
    };
    if before.keys().ne(after.keys()) {
        return vec!["the move changes which pins the sheet places".to_owned()];
    }
    // By key, not by label: on a sheet not yet annotated every resistor is
    // `R?`, and two pins sharing a label must still count as two.
    let peers = |nets: &PinNets| {
        let mut by_root: HashMap<NetRoot, BTreeSet<PinKey>> = HashMap::new();
        for (key, (root, _, _)) in nets {
            by_root.entry(*root).or_default().insert(key.clone());
        }
        by_root
    };
    let (before_peers, after_peers) = (peers(&before), peers(&after));
    let list = |pins: Vec<&PinKey>| {
        let mut labels: Vec<&str> = pins.iter().map(|key| before[*key].2.as_str()).collect();
        labels.sort_unstable();
        labels.join(", ")
    };
    // One line per net, not per pin: every pin on a net that changed says so.
    let mut changes = BTreeSet::new();
    let mut seen = std::collections::HashSet::new();
    for (key, (root, name, _)) in &before {
        if !seen.insert(*root) {
            continue;
        }
        let (after_root, after_name, _) = &after[key];
        let (was, now) = (&before_peers[root], &after_peers[after_root]);
        let net = list(was.iter().collect());
        if name != after_name {
            changes.insert(format!(
                "the net of [{net}] would be renamed from {} to {}",
                name.as_deref().unwrap_or("(unnamed)"),
                after_name.as_deref().unwrap_or("(unnamed)")
            ));
        }
        let gained = list(now.difference(was).collect());
        if !gained.is_empty() {
            changes.insert(format!("the net of [{net}] would gain [{gained}]"));
        }
        let lost = list(was.difference(now).collect());
        if !lost.is_empty() {
            changes.insert(format!("the net of [{net}] would lose [{lost}]"));
        }
    }
    changes.into_iter().collect()
}

/// An item's `(at …)`, or its first `(xy …)` for a wire or bus.
fn item_anchor(node: &SexpNode) -> Option<(f64, f64)> {
    match node.find("pts").and_then(|pts| pts.find("xy")) {
        Some(xy) => xy.get_f64(1).zip(xy.get_f64(2)),
        None => parse_at(node).map(|(x, y, _)| (x, y)),
    }
}

/// The first moved item the written file does not hold at its new anchor.
pub(crate) fn misplaced_item(content: &str, anchors: &[(String, (f64, f64))]) -> Option<String> {
    let Ok(tree) = konnect_sexp::parse_sexp(content) else {
        return Some("the written schematic cannot be parsed".to_owned());
    };
    let mut written = HashMap::new();
    for node in tree.children().unwrap_or(&[]) {
        let Some(uuid) = node.find_str("uuid") else {
            continue;
        };
        written.insert(uuid, item_anchor(node));
    }
    anchors
        .iter()
        .find_map(|(uuid, (x, y))| match written.get(uuid.as_str()) {
            Some(Some((wx, wy))) if points_coincident(*wx, *wy, *x, *y, COINCIDENT_TOLERANCE) => {
                None
            }
            Some(Some((wx, wy))) => Some(format!(
                "item {uuid} was written at ({wx}, {wy}), expected ({x}, {y})"
            )),
            _ => Some(format!("item {uuid} is missing from the written schematic")),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ServerConfig;
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};

    /// KiCad 10.0.6 resave; provenance and oracle in the fixture README.
    const FIXTURE: &str = include_str!("../../tests/fixtures/rigid_block_move_kicad10.kicad_sch");
    const CHILD: &str =
        include_str!("../../tests/fixtures/rigid_block_move_child_kicad10.kicad_sch");
    /// The closed block: R1, C1, R2, their GND symbol, wires, bus and labels.
    const BLOCK: [f64; 4] = [50.0, 40.0, 110.0, 92.0];
    const SHEET_PIN: &str = "sheet a9a16ec1-d102-41c0-b24c-f8bf2ba99f68 pin SP";

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rigid_block_move_kicad10.kicad_sch");
        std::fs::write(&path, FIXTURE).unwrap();
        std::fs::write(
            directory
                .path()
                .join("rigid_block_move_child_kicad10.kicad_sch"),
            CHILD,
        )
        .unwrap();
        (directory, path)
    }

    async fn served_move(path: &Path, area: [f64; 4], dx: f64, dy: f64) -> (bool, Value) {
        let handler = crate::mcp::handler::McpHandler::new(ServerConfig {
            auto_load_toolsets: true,
            ..ServerConfig::default()
        })
        .await
        .unwrap();
        let result = handler
            .handle_message(json!({"jsonrpc": "2.0", "id": 896, "method": "tools/call",
            "params": {"name": "move_region", "arguments": {
                "schematic": path.display().to_string(),
                "x1": area[0], "y1": area[1], "x2": area[2], "y2": area[3],
                "dx": dx, "dy": dy, "include": "all"
            }}}))
            .await
            .unwrap()
            .result
            .unwrap();
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let text = result["content"][0]["text"].as_str().unwrap();
        (is_error, serde_json::from_str(text).unwrap())
    }

    /// The `(at …)`, or first `(xy …)`, of the top-level item with `uuid`.
    fn anchor_of(content: &str, uuid: &str) -> (f64, f64) {
        let tree = konnect_sexp::parse_sexp(content).unwrap();
        let node = tree
            .children()
            .unwrap()
            .iter()
            .find(|node| node.find_str("uuid") == Some(uuid))
            .unwrap_or_else(|| panic!("{uuid} not in the file"));
        item_anchor(node).unwrap()
    }

    fn assert_at(content: &str, uuid: &str, expected: (f64, f64)) {
        let (x, y) = anchor_of(content, uuid);
        assert!(
            points_coincident(x, y, expected.0, expected.1, 0.001),
            "{uuid} at ({x}, {y}), expected {expected:?}"
        );
    }

    /// Each pin net as KiCad's netlist lists it: the net name, or `None` for
    /// KiCad's `unconnected-(…)` nets, against the pins on it.
    fn nets(content: &str) -> BTreeSet<(Option<String>, BTreeSet<String>)> {
        let tree = konnect_sexp::parse_sexp(content).unwrap();
        let mut by_root: HashMap<NetRoot, (Option<String>, BTreeSet<String>)> = HashMap::new();
        for (root, name, label) in pin_nets(&tree).unwrap().into_values() {
            by_root
                .entry(root)
                .or_insert_with(|| (name, BTreeSet::new()))
                .1
                .insert(label);
        }
        by_root.into_values().collect()
    }

    /// `kicad-cli sch export netlist` on the fixture, before and after the
    /// move (see the README), plus what KiCad's netlist leaves out: the GND
    /// symbols' own pins, and the sheet pin's net, which has no symbol pin.
    /// These are KiCad's nets, not a mirror of the code under test.
    fn kicad_nets() -> BTreeSet<(Option<String>, BTreeSet<String>)> {
        let net = |name: Option<&str>, pins: &[&str]| {
            (
                name.map(str::to_owned),
                pins.iter().map(|pin| pin.to_string()).collect(),
            )
        };
        BTreeSet::from([
            net(
                Some("GND"),
                &[
                    "C1 pin 2",
                    "R1 pin 2",
                    "R3 pin 2",
                    "#PWR01 pin 1",
                    "#PWR02 pin 1",
                ],
            ),
            net(
                Some("OUT"),
                &["C1 pin 1", "R1 pin 1", "R2 pin 1", "R3 pin 1"],
            ),
            net(None, &["R2 pin 2"]),
            net(None, &["R4 pin 1"]),
            net(None, &["R4 pin 2"]),
            net(Some("SP_NET"), &[SHEET_PIN]),
        ])
    }

    #[tokio::test]
    async fn rigid_move_carries_the_closed_block_and_keeps_kicads_nets() {
        let (_directory, path) = fixture();
        assert_eq!(nets(FIXTURE), kicad_nets());

        let (is_error, response) = served_move(&path, BLOCK, -12.7, -12.7).await;

        assert!(!is_error, "{response}");
        let moved: BTreeSet<&str> = response["moved"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reference| reference.as_str().unwrap())
            .collect();
        assert_eq!(moved, BTreeSet::from(["#PWR01", "C1", "R1", "R2"]));
        assert_eq!(
            response["items_moved"],
            json!({"buses": 1, "bus_entries": 1, "directive_labels": 1, "global_labels": 1,
                   "junctions": 2, "labels": 3, "text_boxes": 1, "wires": 10}),
            "{response}"
        );
        assert_eq!(response["junctions_added_count"], 0, "{response}");
        assert_eq!(response["junctions_pruned_count"], 0, "{response}");
        assert_eq!(response["no_connects_moved_count"], 1, "{response}");

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(nets(&written), kicad_nets());
        // One item of every kind, at exactly (dx, dy) from where KiCad put it.
        assert_at(
            &written,
            "c38adf10-6ec1-4be8-8043-005e13fbe29e",
            (50.8, 44.45),
        );
        assert_at(
            &written,
            "25b38bed-0c69-40bf-acbf-c8507f2f9168",
            (43.18, 71.12),
        );
        assert_at(
            &written,
            "4b0ba710-9bf8-462a-9abd-83bc52117637",
            (55.88, 68.58),
        );
        assert_at(
            &written,
            "b8a99aef-5051-41fe-86f3-708faf332bda",
            (66.04, 39.37),
        );
        assert_at(
            &written,
            "f607abfb-9e47-495e-9b04-14fbe5deb275",
            (81.28, 52.07),
        );
        assert_at(
            &written,
            "c1caa3e1-c3bc-4a1b-bb63-25ecc8436478",
            (88.9, 39.37),
        );
        // KiCad wrote the global label's Intersheetrefs field on its anchor;
        // left behind, it would sit 18 mm from the label it annotates.
        let sheetrefs = written.find("\"Intersheetrefs\"").unwrap();
        assert!(
            written[sheetrefs..]
                .starts_with("\"Intersheetrefs\" \"${INTERSHEET_REFS}\"\n\t\t\t(at 88.9 39.37 0)"),
            "{}",
            &written[sheetrefs..sheetrefs + 80]
        );
        // A local label's fields are absolute too: SIG's Netclass field.
        let netclass = written.find("\"Netclass\" \"Power\"").unwrap();
        assert!(
            written[netclass..].starts_with("\"Netclass\" \"Power\"\n\t\t\t(at 58.42 36.83 0)"),
            "{}",
            &written[netclass..netclass + 60]
        );
        // The directive label and text box the typed model keeps raw, and the
        // directive label's field.
        assert_at(
            &written,
            "5e6a0c1d-8f3b-4c2a-9d7e-1b2c3d4e5f60",
            (53.34, 57.15),
        );
        assert_at(
            &written,
            "5e6a0c1d-8f3b-4c2a-9d7e-1b2c3d4e5f61",
            (39.37, 29.21),
        );
        assert!(
            written.contains("(at 54.0385 54.61 0)"),
            "directive field left behind"
        );
        // Outside the box: untouched.
        assert_at(
            &written,
            "f061bf4b-1f02-4e26-8420-f743db8b4728",
            (139.7, 69.85),
        );
        assert_at(
            &written,
            "fab384f8-8f26-4d6c-9076-6aa00fd760d4",
            (63.5, 118.11),
        );
    }

    /// Half a grid step: snapping each symbol alone, as `include: "symbols"`
    /// does, would pull every pin off the wires that moved with it.
    #[tokio::test]
    async fn rigid_move_keeps_off_grid_offsets_exact() {
        let (_directory, path) = fixture();
        let (is_error, response) = served_move(&path, BLOCK, 0.635, 0.0).await;
        assert!(!is_error, "{response}");
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(nets(&written), kicad_nets());
        assert_at(
            &written,
            "c38adf10-6ec1-4be8-8043-005e13fbe29e",
            (64.135, 57.15),
        );
    }

    async fn assert_refused(area: [f64; 4], dx: f64, dy: f64, expected: &str) {
        assert_refused_on(FIXTURE, area, dx, dy, expected).await;
    }

    /// The move is refused with `expected` among its reasons, and `content`
    /// is still on disk byte for byte.
    async fn assert_refused_on(content: &str, area: [f64; 4], dx: f64, dy: f64, expected: &str) {
        let (_directory, path) = fixture();
        std::fs::write(&path, content).unwrap();
        let (is_error, response) = served_move(&path, area, dx, dy).await;
        assert!(is_error, "{response}");
        assert_eq!(response["error"]["kind"], "plan_blocked", "{response}");
        let reasons: Vec<&str> = response["error"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reason| reason.as_str().unwrap())
            .collect();
        assert!(reasons.contains(&expected), "{response}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }

    #[tokio::test]
    async fn rigid_move_refuses_a_wire_across_the_edge() {
        assert_refused(
            [120.0, 40.0, 150.0, 92.0],
            0.0,
            -12.7,
            "wire f061bf4b-1f02-4e26-8420-f743db8b4728 from (139.7, 69.85) to (160.02, 69.85) crosses the region edge",
        )
        .await;
    }

    #[tokio::test]
    async fn rigid_move_refuses_a_bus_across_the_edge() {
        assert_refused(
            [50.0, 75.0, 80.0, 92.0],
            0.0,
            -2.54,
            "bus 25b38bed-0c69-40bf-acbf-c8507f2f9168 crosses the region edge",
        )
        .await;
    }

    #[tokio::test]
    async fn rigid_move_refuses_a_sheet_across_the_edge() {
        assert_refused(
            [110.0, 95.0, 130.0, 130.0],
            0.0,
            2.54,
            "sheet a9a16ec1-d102-41c0-b24c-f8bf2ba99f68 crosses the region edge",
        )
        .await;
    }

    /// Every item crosses nothing, yet R1 pin 2 lands on the end of R4's
    /// dangling stub, which would put R4 pin 1 on GND.
    #[tokio::test]
    async fn rigid_move_refuses_a_pin_landing_on_another_net() {
        assert_refused(
            BLOCK,
            0.0,
            40.64,
            "the net of [#PWR01 pin 1, #PWR02 pin 1, C1 pin 2, R1 pin 2, R3 pin 2] would gain [R4 pin 1]",
        )
        .await;
    }

    /// The SP wire passes through the box with both ends outside it, so its
    /// label would leave it.
    #[tokio::test]
    async fn rigid_move_refuses_a_wire_passing_through() {
        assert_refused(
            [155.0, 100.0, 165.0, 108.0],
            0.0,
            -2.54,
            "wire 740ed2d5-f65d-4d87-9bb9-2fed4f90b62d from (147.32, 104.14) to (167.64, 104.14) crosses the region edge",
        )
        .await;
    }

    /// The block's D0 stub lands on the SP wire's free end. No symbol pin is on
    /// either net, so only the sheet pin shows that SP_NET becomes D0.
    #[tokio::test]
    async fn rigid_move_refuses_renaming_a_sheet_pin_net() {
        assert_refused(
            BLOCK,
            99.06,
            27.94,
            &format!("the net of [{SHEET_PIN}] would be renamed from SP_NET to D0"),
        )
        .await;
    }

    /// Unannotated, R2 and R4 are both `R?`. R2 lands on R4, stacking the two
    /// pin 2s, each alone and unnamed before: only their identity shows the
    /// merge. The no-connects go, so their carry does not refuse first.
    #[tokio::test]
    async fn rigid_move_tells_unannotated_pins_apart() {
        let mut content = FIXTURE
            .replace("\"R2\"", "\"R?\"")
            .replace("\"R4\"", "\"R?\"");
        while let Some(start) = content.find("\t(no_connect\n") {
            let end = start + content[start..].find("\n\t)\n").unwrap() + "\n\t)\n".len();
            content.replace_range(start..end, "");
        }
        assert_refused_on(
            &content,
            BLOCK,
            -30.48,
            53.34,
            "the net of [R? pin 2] would gain [R? pin 2]",
        )
        .await;
    }

    /// A pin that cannot be placed cannot be shown to keep its net. The
    /// no-connects go too, because their carry refuses such a sheet first.
    #[tokio::test]
    async fn rigid_move_refuses_a_symbol_with_unresolved_pins() {
        let mut content = FIXTURE.replace("(lib_id \"Device:C\")", "(lib_id \"Device:Missing\")");
        assert_ne!(content, FIXTURE);
        while let Some(start) = content.find("\t(no_connect\n") {
            let end = start + content[start..].find("\n\t)\n").unwrap() + "\n\t)\n".len();
            content.replace_range(start..end, "");
        }
        assert_refused_on(
            &content,
            BLOCK,
            -12.7,
            -12.7,
            crate::tools::UNRESOLVED_PIN_GEOMETRY,
        )
        .await;
    }

    /// A bus entry without its `(size …)` has no far end to test against the box.
    #[tokio::test]
    async fn rigid_move_refuses_a_bus_entry_it_cannot_measure() {
        let content = FIXTURE.replace("\t\t(size 2.54 2.54)\n", "");
        assert_ne!(content, FIXTURE);
        assert_refused_on(
            &content,
            BLOCK,
            -12.7,
            -12.7,
            "bus_entry 4b0ba710-9bf8-462a-9abd-83bc52117637 has no readable position",
        )
        .await;
    }

    #[tokio::test]
    async fn rigid_move_carries_a_sheet_inside_the_box() {
        let (_directory, path) = fixture();
        let area = [115.0, 95.0, 170.0, 120.0];
        let (is_error, response) = served_move(&path, area, 2.54, 0.0).await;
        assert!(!is_error, "{response}");
        assert_eq!(
            response["items_moved"],
            json!({"labels": 1, "sheets": 1, "wires": 1}),
            "{response}"
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert_at(
            &written,
            "a9a16ec1-d102-41c0-b24c-f8bf2ba99f68",
            (124.46, 99.06),
        );
        // The pin rides on the sheet's edge, still on the end of its wire.
        let pin = written.find("(pin \"SP\" bidirectional").unwrap();
        assert!(
            written[pin..].contains("(at 149.86 104.14 0)"),
            "{}",
            &written[pin..pin + 80]
        );
        assert_eq!(nets(&written), kicad_nets());
    }

    /// R4's origin is outside the box and its pin 2 inside, so the symbol
    /// would stay while anything on that pin moved.
    #[tokio::test]
    async fn rigid_move_refuses_a_symbol_across_the_edge() {
        assert_refused(
            [50.0, 116.0, 80.0, 125.0],
            2.54,
            0.0,
            "symbol R4 e796109b-1df5-4569-b57b-76625b20e182 crosses the region edge",
        )
        .await;
    }

    /// The readback the handler runs after its write, against a file that
    /// does not hold the move.
    #[test]
    fn misplaced_item_names_the_item_the_file_does_not_hold() {
        let wire = "c38adf10-6ec1-4be8-8043-005e13fbe29e".to_owned();
        assert_eq!(
            misplaced_item(FIXTURE, &[(wire.clone(), (63.5, 57.15))]),
            None
        );
        assert_eq!(
            misplaced_item(FIXTURE, &[(wire.clone(), (50.8, 44.45))]),
            Some(format!(
                "item {wire} was written at (63.5, 57.15), expected (50.8, 44.45)"
            ))
        );
        assert_eq!(
            misplaced_item(FIXTURE, &[("absent".to_owned(), (0.0, 0.0))]),
            Some("item absent is missing from the written schematic".to_owned())
        );
    }

    #[tokio::test]
    async fn an_unknown_include_is_an_argument_error() {
        let (_directory, path) = fixture();
        let context = crate::tools::ToolContext::new(
            ServerConfig::default(),
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        );
        let result = crate::tools::sch_components::handle_move_region(
            &json!({
                "schematic": path.display().to_string(),
                "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 1.0, "dx": 0.0, "dy": 0.0,
                "include": "wires"
            }),
            &context,
        )
        .await
        .unwrap();
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FIXTURE);
    }

    /// KiCad's own netlist of the moved block is the netlist it had before.
    #[tokio::test]
    #[ignore = "needs an installed KiCad 10 kicad-cli"]
    async fn kicad_netlist_is_unchanged_by_the_rigid_move() {
        let (_directory, path) = fixture();
        let cli = crate::kicad_install::find_cli("").expect("installed kicad-cli");
        let export = |output: PathBuf| {
            let (cli, path) = (cli.display().to_string(), path.clone());
            async move {
                crate::tools::cli::export_netlist(&cli, &path, &output, "kicadsexpr")
                    .await
                    .unwrap();
                let netlist = std::fs::read_to_string(output).unwrap();
                netlist[netlist.find("(nets").unwrap()..].to_owned()
            }
        };
        let before = export(path.with_extension("before.net")).await;
        let (is_error, response) = served_move(&path, BLOCK, -12.7, -12.7).await;
        assert!(!is_error, "{response}");
        let after = export(path.with_extension("after.net")).await;
        assert_eq!(before, after);
    }
}
