//! One shared answer to "what is attached at (x, y)".
//!
//! The connectivity tools each used to carry their own tolerance and their own
//! set of items that count as a connection, so they disagreed on real sheets: a
//! wire ending on a hierarchical sheet pin was terminated for `find_orphan_items`
//! and floating for `validate_wire_connections`, and two pins placed directly on
//! each other were connected for the first and unconnected for
//! `validate_component_connections`. The net graph was a fourth answer again,
//! burying its attach tolerance in the function body and knowing nothing about
//! sheet pins. That tolerance is now a property of the [`WireIndex`] the seeder
//! is handed rather than a literal — the value every production caller uses is
//! still [`COINCIDENT_TOLERANCE`], so this is structure, not a behaviour change.
//!
//! One invariant is worth stating, because the two layers are not identical:
//! coincidence is a **superset** of the graph. A point is coincident within the
//! index's tolerance, while [`NetGraph`] joins nodes on exact [`pt_key`]
//! equality plus the attach step. So a wire ending 5 µm off a sheet pin is
//! *terminated* for the validators yet does not reach that pin's net. That is
//! the safe direction and it is deliberate: leniency suppresses a false
//! "floating end" from a rounding artefact, while strictness keeps the graph
//! from inventing a net connection KiCad's own netlister would not make.
//! KiCad snaps to grid, so exact agreement is the normal case.
//!
//! [`ConnectivityIndex`] is built once per tree and holds every item that can
//! terminate a point — wires, wire endpoints, labels, pins with the reference
//! that owns them, sheet pins, junctions and no-connects — under a single
//! tolerance, and the tools express policy over it. One [`seed_net_graph`] is
//! the only definition of the graph, so the ten read-only tools that want net
//! names cannot drift from the three that ask about attachment.

use konnect_sexp::{
    geometry::{point_on_segment, points_coincident},
    schematic::{
        extract_junctions, extract_no_connects, extract_sheet_pins, pin_endpoint, Label, LabelKind,
        LibPin, Wire,
    },
    SexpNode,
};
use std::collections::{HashMap, HashSet};

/// The coincidence tolerance the connectivity tools have always used, in mm.
/// `find_orphan_items` takes its own as an argument; the rest use this.
pub(crate) const COINCIDENT_TOLERANCE: f64 = 0.01;

// ─── Spatial indices ──────────────────────────────────────────────────────────

fn bucket(value: f64, tolerance: f64) -> i64 {
    (value / tolerance).floor() as i64
}

/// Points bucketed at the coincidence tolerance, so a lookup probes nine cells
/// instead of scanning every point. `points_coincident` compares an L∞ box of
/// side `tol`, which the 3×3 neighbourhood covers exactly.
struct PointIndex {
    tol: f64,
    buckets: HashMap<(i64, i64), Vec<(f64, f64)>>,
}

impl PointIndex {
    fn build(points: impl IntoIterator<Item = (f64, f64)>, tol: f64) -> Self {
        let mut index = PointIndex {
            tol,
            buckets: HashMap::new(),
        };
        for (x, y) in points {
            let key = index.cell(x, y);
            index.buckets.entry(key).or_default().push((x, y));
        }
        index
    }

    fn cell(&self, x: f64, y: f64) -> (i64, i64) {
        ((x / self.tol).floor() as i64, (y / self.tol).floor() as i64)
    }

    /// How many indexed points coincide with `(x, y)`.
    fn count_at(&self, x: f64, y: f64) -> usize {
        let (cx, cy) = self.cell(x, y);
        let mut found = 0;
        for dx in -1..=1 {
            for dy in -1..=1 {
                let Some(bucket) = self.buckets.get(&(cx + dx, cy + dy)) else {
                    continue;
                };
                found += bucket
                    .iter()
                    .filter(|(px, py)| points_coincident(x, y, *px, *py, self.tol))
                    .count();
            }
        }
        found
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        self.count_at(x, y) > 0
    }
}

/// Wires bucketed by the coordinate they hold constant: a horizontal wire can
/// only be met in its own row, a vertical one in its own column. Mirrors
/// `point_on_segment`, which answers `false` for anything diagonal.
struct WireIndex<'a> {
    tol: f64,
    rows: HashMap<i64, Vec<&'a Wire>>,
    columns: HashMap<i64, Vec<&'a Wire>>,
}

impl<'a> WireIndex<'a> {
    fn tolerance(&self) -> f64 {
        self.tol
    }

    fn build(wires: &'a [Wire], tol: f64) -> Self {
        let mut index = WireIndex {
            tol,
            rows: HashMap::new(),
            columns: HashMap::new(),
        };
        for wire in wires {
            if (wire.x1 - wire.x2).abs() < tol {
                index
                    .columns
                    .entry(bucket(wire.x1, tol))
                    .or_default()
                    .push(wire);
            } else if (wire.y1 - wire.y2).abs() < tol {
                index
                    .rows
                    .entry(bucket(wire.y1, tol))
                    .or_default()
                    .push(wire);
            }
        }
        index
    }

    /// Every wire that could pass through `(x, y)`.
    fn candidates(&self, x: f64, y: f64) -> impl Iterator<Item = &&'a Wire> {
        let cell_x = bucket(x, self.tol);
        let cell_y = bucket(y, self.tol);
        (-1..=1).flat_map(move |delta| {
            let column = self.columns.get(&(cell_x + delta)).into_iter().flatten();
            let row = self.rows.get(&(cell_y + delta)).into_iter().flatten();
            column.chain(row)
        })
    }

    /// Every wire that actually passes through `(x, y)`.
    fn hits(&self, x: f64, y: f64) -> impl Iterator<Item = &&'a Wire> {
        self.candidates(x, y).filter(move |wire| {
            point_on_segment(x, y, wire.x1, wire.y1, wire.x2, wire.y2, self.tol)
        })
    }

    /// Lies anywhere on a wire, endpoints included.
    fn covers(&self, x: f64, y: f64) -> bool {
        self.hits(x, y).next().is_some()
    }

    /// Lies on the interior of a wire — a T-junction, which KiCAD connects
    /// without splitting the crossed wire.
    fn covers_interior(&self, x: f64, y: f64) -> bool {
        self.hits(x, y).any(|wire| {
            !points_coincident(x, y, wire.x1, wire.y1, self.tol)
                && !points_coincident(x, y, wire.x2, wire.y2, self.tol)
        })
    }
}

// ─── Union-find net graph ─────────────────────────────────────────────────────

/// A graph node's identity: coordinates quantized to 1 µm.
///
/// Deliberately *not* the index's tolerance. This is an equality key, and a
/// bucket boundary would separate two points closer together than two points it
/// keeps — so widening it does not make near-misses union, it only makes which
/// ones union depend on where the grid falls. Tolerance is applied by
/// [`seed_net_graph`]'s attach step, which is where a near-miss is resolved
/// against a wire it lies on.
pub(crate) fn pt_key(x: f64, y: f64) -> (i64, i64) {
    ((x * 1000.0).round() as i64, (y * 1000.0).round() as i64)
}

pub(crate) struct NetGraph {
    pub(crate) point_nets: HashMap<(i64, i64), String>,
    pub(crate) parent: HashMap<(i64, i64), (i64, i64)>,
}

impl NetGraph {
    pub(crate) fn new() -> Self {
        NetGraph {
            point_nets: HashMap::new(),
            parent: HashMap::new(),
        }
    }

    pub(crate) fn ensure(&mut self, k: (i64, i64)) {
        self.parent.entry(k).or_insert(k);
    }

    pub(crate) fn find(&mut self, k: (i64, i64)) -> (i64, i64) {
        self.ensure(k);
        let p = self.parent[&k];
        if p == k {
            return k;
        }
        let root = self.find(p);
        self.parent.insert(k, root);
        root
    }

    pub(crate) fn union(&mut self, a: (i64, i64), b: (i64, i64)) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent.insert(rb, ra);
        }
    }

    pub(crate) fn add_wire(&mut self, w: &Wire) {
        let a = pt_key(w.x1, w.y1);
        let b = pt_key(w.x2, w.y2);
        self.ensure(a);
        self.ensure(b);
        self.union(a, b);
    }

    pub(crate) fn add_label(&mut self, x: f64, y: f64, net: &str) {
        let k = pt_key(x, y);
        self.ensure(k);
        self.point_nets.insert(k, net.to_string());
    }

    pub(crate) fn net_at(&mut self, x: f64, y: f64) -> Option<String> {
        let k = pt_key(x, y);
        self.ensure(k);
        let root = self.find(k);
        let labels: Vec<_> = self.point_nets.clone().into_iter().collect();
        for (lk, net) in labels {
            if self.find(lk) == root {
                return Some(net);
            }
        }
        None
    }

    pub(crate) fn points_on_net(&mut self, net: &str) -> Vec<(i64, i64)> {
        // Collect keys first to avoid simultaneous borrow of point_nets and self.find()
        let net_keys: Vec<(i64, i64)> = self
            .point_nets
            .iter()
            .filter(|(_, n)| n.as_str() == net)
            .map(|(k, _)| *k)
            .collect();
        let net_roots: HashSet<(i64, i64)> = net_keys.iter().map(|k| self.find(*k)).collect();
        let all_keys: Vec<(i64, i64)> = self.parent.keys().cloned().collect();
        all_keys
            .into_iter()
            .filter(|k| net_roots.contains(&self.find(*k)))
            .collect()
    }
}

/// Seed a graph from the terminals that name and join nets. Both
/// [`net_graph_for`] and [`ConnectivityIndex::net_graph`] come through here, so
/// the two cannot answer differently.
///
/// Labels and junction dots connect anywhere along a wire, not only at an
/// endpoint, so each is unioned with every wire it lies on.
///
/// Sheet pins are only *registered*, never attached mid-span. A hierarchical
/// sheet pin is a pin, and KiCad connects a pin landing mid-wire only through a
/// junction dot (#104) — the rule [`ConnectivityIndex::attaches_pin`] states for
/// symbol pins. A wire that *ends* on a sheet pin is already unioned by
/// `add_wire`, and one that merely passes through its coordinates must not be:
/// a wire routed along a sheet edge would otherwise merge with whatever net
/// that pin carries, inventing a short KiCad does not see. Registering the
/// point is still worth doing, so a query at a sheet pin resolves its net.
fn seed_net_graph(
    wires: &[Wire],
    labels: &[Label],
    junctions: &[(f64, f64)],
    sheet_pins: &[(f64, f64)],
    on_wire: &WireIndex,
) -> NetGraph {
    let mut graph = NetGraph::new();
    for wire in wires {
        graph.add_wire(wire);
    }
    let attach = |graph: &mut NetGraph, x: f64, y: f64| {
        for wire in on_wire.hits(x, y) {
            graph.union(pt_key(x, y), pt_key(wire.x1, wire.y1));
        }
    };
    for label in labels {
        graph.add_label(label.x, label.y, &label.net);
        attach(&mut graph, label.x, label.y);
    }
    for &(x, y) in junctions {
        graph.ensure(pt_key(x, y));
        attach(&mut graph, x, y);
    }
    for &(x, y) in sheet_pins {
        graph.ensure(pt_key(x, y));
    }
    graph
}

/// Each label paired with the graph root it sits on.
///
/// Two tools read the one relation from opposite ends — `find_shorted_nets`
/// root-first, since more than one name on a root is a short, and
/// `find_single_pin_nets` name-first, pooling the roots one name reaches. The
/// walk lives here so a refinement to which label anchors which root cannot
/// land in only one of them.
pub(crate) fn label_roots<'a>(
    graph: &mut NetGraph,
    labels: &'a [Label],
) -> Vec<((i64, i64), &'a Label)> {
    labels
        .iter()
        .map(|label| (graph.find(pt_key(label.x, label.y)), label))
        .collect()
}

/// The net graph for a whole tree, at [`COINCIDENT_TOLERANCE`].
///
/// `labels` must be `extract_all_net_labels` — power symbols name nets too, and
/// a graph built from `extract_labels` alone reports every `power:` rail
/// unconnected.
///
/// This deliberately does not go through [`ConnectivityIndex`]: the graph reads
/// none of the pin geometry an index parses, and this is the entry point ten
/// read-only tools use.
pub(crate) fn net_graph_for(tree: &SexpNode, wires: &[Wire], labels: &[Label]) -> NetGraph {
    seed_net_graph(
        wires,
        labels,
        &extract_junctions(tree),
        &extract_sheet_pins(tree),
        &WireIndex::build(wires, COINCIDENT_TOLERANCE),
    )
}

// ─── The index ────────────────────────────────────────────────────────────────

/// A pin on the sheet, with the reference designator that owns it. Unit-aware
/// via `placed_pins_by_reference`, so a multi-unit symbol never contributes
/// another unit's pins as phantom connection points (#35).
pub(crate) struct PlacedPin {
    pub(crate) reference: String,
    /// The owning component's value, carried so a caller reporting a pin does
    /// not re-walk the instances this was built from.
    pub(crate) value: String,
    pub(crate) pin: LibPin,
    pub(crate) at: (f64, f64),
}

/// Every item that can terminate a point on one sheet, under one tolerance.
pub(crate) struct ConnectivityIndex<'a> {
    wires: &'a [Wire],
    labels: &'a [Label],
    on_wire: WireIndex<'a>,
    wire_ends: PointIndex,
    label_points: PointIndex,
    pin_points: PointIndex,
    sheet_pin_points: PointIndex,
    junction_points: PointIndex,
    no_connect_points: PointIndex,
    placed_pins: Vec<PlacedPin>,
}

impl<'a> ConnectivityIndex<'a> {
    /// `labels` should be `extract_all_net_labels`: a power symbol names a net
    /// exactly as a label does, and [`net_graph`](Self::net_graph) needs both.
    /// Its points coincide with the power symbol's own pin, so passing them
    /// changes no coincidence answer — only the names the graph can reach.
    pub(crate) fn build(
        tree: &SexpNode,
        wires: &'a [Wire],
        labels: &'a [Label],
        tolerance: f64,
    ) -> Self {
        let placed_pins: Vec<PlacedPin> = crate::tools::placed_pins_by_reference(tree)
            .into_iter()
            .flat_map(|(inst, pins)| {
                pins.into_iter().map(move |(pin, transform)| PlacedPin {
                    reference: inst.reference.clone(),
                    value: inst.value.clone(),
                    at: pin_endpoint(&pin, transform),
                    pin,
                })
            })
            .collect();
        let junctions = extract_junctions(tree);
        let sheet_pins = extract_sheet_pins(tree);

        ConnectivityIndex {
            wires,
            labels,
            on_wire: WireIndex::build(wires, tolerance),
            wire_ends: PointIndex::build(
                wires
                    .iter()
                    .flat_map(|wire| [(wire.x1, wire.y1), (wire.x2, wire.y2)]),
                tolerance,
            ),
            // Power symbols excluded on purpose. `extract_power_symbol_labels`
            // places a pseudo-label *on the symbol's own pin*, so indexing it
            // here would make every power pin see a label at its own position
            // and report itself attached — hiding the unwired GND that is one
            // of the commonest real mistakes on a sheet. A wire arriving at a
            // power symbol is terminated by its pin, which `pin_points` covers.
            label_points: PointIndex::build(
                labels
                    .iter()
                    .filter(|label| label.kind != LabelKind::PowerSymbol)
                    .map(|label| (label.x, label.y)),
                tolerance,
            ),
            pin_points: PointIndex::build(placed_pins.iter().map(|p| p.at), tolerance),
            sheet_pin_points: PointIndex::build(sheet_pins, tolerance),
            junction_points: PointIndex::build(junctions, tolerance),
            no_connect_points: PointIndex::build(extract_no_connects(tree), tolerance),
            placed_pins,
        }
    }

    pub(crate) fn placed_pins(&self) -> &[PlacedPin] {
        &self.placed_pins
    }

    pub(crate) fn labels(&self) -> &[Label] {
        self.labels
    }

    /// How many wire endpoints lie at `(x, y)`. A point that is itself a wire
    /// endpoint counts itself, so two or more means wires meet here.
    pub(crate) fn wire_ends_at(&self, x: f64, y: f64) -> usize {
        self.wire_ends.count_at(x, y)
    }

    pub(crate) fn has_wire_end(&self, x: f64, y: f64) -> bool {
        self.wire_ends.contains(x, y)
    }

    /// Lies anywhere on a wire, endpoints included.
    pub(crate) fn on_wire(&self, x: f64, y: f64) -> bool {
        self.on_wire.covers(x, y)
    }

    /// Lies on a wire's interior — a T-junction KiCAD connects without
    /// splitting the crossed wire.
    pub(crate) fn on_wire_interior(&self, x: f64, y: f64) -> bool {
        self.on_wire.covers_interior(x, y)
    }

    /// How many wires lie under `(x, y)` — endpoint and interior alike. The
    /// booleans above cannot tell one wire passing from two wires crossing,
    /// and the junction reconciler needs that distinction: a dot on a lone
    /// wire connects nothing, a dot where two wires cross joins two nets.
    pub(crate) fn wires_at(&self, x: f64, y: f64) -> usize {
        self.wires
            .iter()
            .filter(|w| point_on_segment(x, y, w.x1, w.y1, w.x2, w.y2, self.on_wire.tolerance()))
            .count()
    }

    pub(crate) fn has_label(&self, x: f64, y: f64) -> bool {
        self.label_points.contains(x, y)
    }

    /// How many pins lie at `(x, y)`. A point that is itself a pin counts
    /// itself, so two or more means pins are stacked — a legal connection.
    pub(crate) fn pins_at(&self, x: f64, y: f64) -> usize {
        self.pin_points.count_at(x, y)
    }

    pub(crate) fn has_pin(&self, x: f64, y: f64) -> bool {
        self.pin_points.contains(x, y)
    }

    pub(crate) fn has_sheet_pin(&self, x: f64, y: f64) -> bool {
        self.sheet_pin_points.contains(x, y)
    }

    pub(crate) fn has_junction(&self, x: f64, y: f64) -> bool {
        self.junction_points.contains(x, y)
    }

    pub(crate) fn has_no_connect(&self, x: f64, y: f64) -> bool {
        self.no_connect_points.contains(x, y)
    }

    /// Whether a wire endpoint at `(x, y)` is terminated. Everything but the
    /// endpoint itself terminates it: a pin, a label, a hierarchical sheet pin,
    /// a junction dot, a no-connect flag, another wire's endpoint, or the
    /// interior of a wire it lands mid-span on.
    pub(crate) fn terminates_wire_end(&self, x: f64, y: f64) -> bool {
        self.has_pin(x, y)
            || self.has_label(x, y)
            || self.has_sheet_pin(x, y)
            || self.has_junction(x, y)
            || self.has_no_connect(x, y)
            || self.wire_ends_at(x, y) >= 2
            || self.on_wire_interior(x, y)
    }

    /// Whether a pin at `(x, y)` is attached to anything. A wire ending on it,
    /// a label naming it, a hierarchical sheet pin meeting it, or a second pin
    /// stacked on it all connect. A pin landing mid-wire connects only through
    /// a junction dot: KiCAD's netlister registers the unsplit wire at a
    /// junction point, so the dot alone is enough (#104).
    pub(crate) fn attaches_pin(&self, x: f64, y: f64) -> bool {
        self.has_wire_end(x, y)
            || self.has_label(x, y)
            || self.has_sheet_pin(x, y)
            || self.pins_at(x, y) >= 2
            || (self.has_junction(x, y) && self.on_wire(x, y))
    }

    /// Every wire endpoint that nothing terminates, as `(x, y, wire uuid)`.
    /// Both tools that report floating ends read this, so a refinement to what
    /// counts as terminated cannot land in only one of them.
    pub(crate) fn floating_wire_ends(&self) -> Vec<(f64, f64, Option<&str>)> {
        self.wires
            .iter()
            .flat_map(|wire| {
                [(wire.x1, wire.y1), (wire.x2, wire.y2)].map(|(x, y)| (x, y, wire.uuid.as_deref()))
            })
            .filter(|&(x, y, _)| !self.terminates_wire_end(x, y))
            .collect()
    }
}

#[cfg(test)]
mod agreement_tests {
    use super::*;
    use crate::tools::{ServerConfig, ToolContext};
    use konnect_sexp::schematic::{extract_all_net_labels, extract_wires, read_schematic};
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    /// Run a registered tool from either schematic toolset against a temporary
    /// file, exactly as the MCP dispatch layer does after selecting its
    /// `ToolDef`. Both toolsets are searched so one test can ask two tools the
    /// same question — which is the whole point of this module.
    async fn call(
        tool_name: &str,
        schematic: &str,
        mut args: serde_json::Value,
    ) -> serde_json::Value {
        let mut file = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        file.write_all(schematic.as_bytes()).unwrap();
        file.flush().unwrap();
        args["schematic"] = json!(file.path().to_str().unwrap());

        let definition = crate::tools::sch_analysis::tools()
            .into_iter()
            .chain(crate::tools::sch_batch::tools())
            .find(|tool| tool.name == tool_name)
            .unwrap_or_else(|| panic!("no tool named {tool_name}"));
        let context = ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        );
        let result = (definition.handler)(&args, Arc::new(context))
            .await
            .unwrap();
        assert!(!result.is_error, "{tool_name} failed: {:?}", result.content);
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content from {tool_name}");
        };
        serde_json::from_str(text).unwrap()
    }

    /// A one-pin symbol whose connection point is the placement origin.
    const LIB: &str = "\t(lib_symbols\n\t\t(symbol \"Test:P1\"\n\t\t\t(symbol \"P1_1_1\"\n\t\t\t\t(pin passive line (at 0 0 0) (length 2.54)\n\t\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n";

    fn symbol(reference: &str, uuid: &str, x: f64, y: f64) -> String {
        format!(
            "\t(symbol\n\t\t(lib_id \"Test:P1\")\n\t\t(at {x} {y} 0)\n\t\t(unit 1)\n\t\t(uuid \"{uuid}\")\n\t\t(property \"Reference\" \"{reference}\"\n\t\t\t(at {x} {y} 0)\n\t\t)\n\t)\n"
        )
    }

    fn sheet(x: f64, y: f64, pin_x: f64, pin_y: f64) -> String {
        format!(
            "\t(sheet\n\t\t(at {x} {y})\n\t\t(size 20 20)\n\t\t(uuid \"sh1\")\n\t\t(pin \"OUT\" input\n\t\t\t(at {pin_x} {pin_y} 180)\n\t\t\t(uuid \"sp1\")\n\t\t)\n\t)\n"
        )
    }

    fn schematic(body: &str) -> String {
        format!("(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n{LIB}{body}\t(sheet_instances (path \"/\" (page \"1\")))\n)\n")
    }

    /// U1 —— wire —— sheet pin. Both ends of the wire are terminated, and every
    /// tool has to say so: `validate_wire_connections` used to report the
    /// sheet-pin end as floating because sheet pins were not in its item set.
    #[tokio::test]
    async fn a_sheet_pin_terminates_a_wire_end_for_every_tool() {
        let sch = schematic(&format!(
            "\t(wire\n\t\t(pts (xy 100 80) (xy 120 80))\n\t\t(uuid \"w1\")\n\t)\n{}{}",
            symbol("U1", "u1", 100.0, 80.0),
            sheet(120.0, 70.0, 120.0, 80.0),
        ));

        let orphans = call("find_orphan_items", &sch, json!({})).await;
        assert_eq!(orphans["orphan_count"], 0, "{orphans}");

        let wires = call("validate_wire_connections", &sch, json!({})).await;
        assert_eq!(wires["floating_count"], 0, "{wires}");
    }

    /// Two pins placed on each other are a legal connection with no wire at
    /// all. `find_orphan_items` has always counted it; the component validator
    /// used to demand a wire endpoint and report both pins unconnected.
    #[tokio::test]
    async fn stacked_pins_are_connected_for_every_tool() {
        let sch = schematic(&format!(
            "{}{}",
            symbol("U1", "u1", 100.0, 80.0),
            symbol("U2", "u2", 100.0, 80.0),
        ));

        let orphans = call("find_orphan_items", &sch, json!({})).await;
        assert_eq!(orphans["orphan_count"], 0, "{orphans}");

        let components = call("validate_component_connections", &sch, json!({})).await;
        assert_eq!(components["unconnected_count"], 0, "{components}");
    }

    /// A lone pin is unconnected for both tools — the agreement has to hold in
    /// the direction that still reports a fault, or the fix above is just a
    /// blanket "everything is connected".
    #[tokio::test]
    async fn an_isolated_pin_is_unconnected_for_every_tool() {
        let sch = schematic(&symbol("U1", "u1", 100.0, 80.0));

        let orphans = call("find_orphan_items", &sch, json!({})).await;
        assert_eq!(orphans["orphan_count"], 1, "{orphans}");
        assert_eq!(orphans["orphans"][0]["type"], "unconnected_pin");

        let components = call("validate_component_connections", &sch, json!({})).await;
        assert_eq!(components["unconnected_count"], 1, "{components}");
        assert_eq!(components["unconnected_pins"][0]["reference"], "U1");
    }

    /// The component validator still reports the value it always has, which now
    /// comes from a lookup rather than from the instance being iterated.
    #[tokio::test]
    async fn an_unconnected_pin_still_reports_its_component_value() {
        let sch = schematic(&symbol("U1", "u1", 100.0, 80.0).replace(
            "(property \"Reference\" \"U1\"",
            "(property \"Value\" \"10k\"\n\t\t\t(at 100 80 0)\n\t\t)\n\t\t(property \"Reference\" \"U1\"",
        ));

        let components = call("validate_component_connections", &sch, json!({})).await;
        assert_eq!(
            components["unconnected_pins"][0]["value"], "10k",
            "{components}"
        );
    }

    /// A graph seeded exactly as `net_graph_for` seeds one, but at a chosen
    /// tolerance, so a test can vary the only knob the seeder takes.
    fn graph_at(
        tree: &konnect_sexp::SexpNode,
        wires: &[Wire],
        labels: &[Label],
        tol: f64,
    ) -> NetGraph {
        seed_net_graph(
            wires,
            labels,
            &extract_junctions(tree),
            &extract_sheet_pins(tree),
            &WireIndex::build(wires, tol),
        )
    }

    fn index_for(sch: &str) -> (konnect_sexp::SexpNode, Vec<Wire>, Vec<Label>) {
        let mut file = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        file.write_all(sch.as_bytes()).unwrap();
        file.flush().unwrap();
        let (_, tree) = read_schematic(file.path()).unwrap();
        let wires = extract_wires(&tree);
        let labels = extract_all_net_labels(&tree);
        (tree, wires, labels)
    }

    /// The graph now attaches at the index's tolerance instead of a hardcoded
    /// 0.01, so a label 0.03 mm off a wire joins that wire's net when the
    /// caller asked for 0.05 and stays an island when it asked for 0.01.
    #[tokio::test]
    async fn the_graph_attaches_at_the_index_tolerance() {
        let sch = schematic(
            "\t(wire\n\t\t(pts (xy 100 80) (xy 120 80))\n\t\t(uuid \"w1\")\n\t)\n\t(label \"NETA\"\n\t\t(at 110 80.03 0)\n\t\t(uuid \"l1\")\n\t)\n",
        );
        let (tree, wires, labels) = index_for(&sch);

        for (tolerance, expected) in [(0.05, Some("NETA".to_string())), (0.01, None)] {
            let mut graph = graph_at(&tree, &wires, &labels, tolerance);
            assert_eq!(
                graph.net_at(100.0, 80.0),
                expected,
                "tolerance {tolerance} should have given {expected:?}"
            );
        }
    }

    /// A wire *ending* on a sheet pin reaches it, so the net does not stop at
    /// the sheet boundary — the blindness this index was built to remove.
    #[tokio::test]
    async fn a_wire_ending_on_a_sheet_pin_joins_its_net() {
        let sch = schematic(&format!(
            "\t(wire\n\t\t(pts (xy 100 80) (xy 120 80))\n\t\t(uuid \"w1\")\n\t)\n\t(label \"NETA\"\n\t\t(at 100 80 0)\n\t\t(uuid \"l1\")\n\t)\n{}",
            sheet(120.0, 70.0, 120.0, 80.0),
        ));
        let (tree, wires, labels) = index_for(&sch);
        let index = ConnectivityIndex::build(&tree, &wires, &labels, COINCIDENT_TOLERANCE);

        assert!(index.has_sheet_pin(120.0, 80.0));
        let mut graph = net_graph_for(&tree, &wires, &labels);
        assert_eq!(graph.net_at(120.0, 80.0), Some("NETA".to_string()));
    }

    /// A sheet pin a wire merely passes through is a pin landing mid-wire, and
    /// KiCad connects one of those only through a junction dot (#104). Merging
    /// without the dot would invent a short between the net on the wire and
    /// whatever the sheet pin carries.
    #[tokio::test]
    async fn a_sheet_pin_mid_wire_joins_its_net_only_through_a_junction() {
        for (junction, expected) in [
            ("", None),
            (
                "\t(junction (at 120 80) (diameter 0) (color 0 0 0 0) (uuid \"j1\"))\n",
                Some("NETA".to_string()),
            ),
        ] {
            let sch = schematic(&format!(
                "\t(wire\n\t\t(pts (xy 100 80) (xy 140 80))\n\t\t(uuid \"w1\")\n\t)\n\t(label \"NETA\"\n\t\t(at 100 80 0)\n\t\t(uuid \"l1\")\n\t)\n{junction}{}",
                sheet(110.0, 70.0, 120.0, 80.0),
            ));
            let (tree, wires, labels) = index_for(&sch);
            let index = ConnectivityIndex::build(&tree, &wires, &labels, COINCIDENT_TOLERANCE);

            assert!(index.has_sheet_pin(120.0, 80.0));
            let mut graph = net_graph_for(&tree, &wires, &labels);
            assert_eq!(
                graph.net_at(120.0, 80.0),
                expected,
                "junction present: {}",
                !junction.is_empty()
            );
        }
    }
    fn power_symbol(reference: &str, value: &str, x: f64, y: f64) -> String {
        format!(
            "\t(symbol\n\t\t(lib_id \"power:GND\")\n\t\t(at {x} {y} 0)\n\t\t(unit 1)\n\t\t(uuid \"p1\")\n\t\t(property \"Reference\" \"{reference}\"\n\t\t\t(at {x} {y} 0)\n\t\t)\n\t\t(property \"Value\" \"{value}\"\n\t\t\t(at {x} {y} 0)\n\t\t)\n\t)\n"
        )
    }

    /// A power symbol's net name is synthesised as a pseudo-label sitting on
    /// the symbol's own pin. Index that as a label and every power pin sees a
    /// label at its own position and reports itself attached — so an unwired
    /// GND, one of the commonest real mistakes, becomes invisible.
    #[tokio::test]
    async fn an_unwired_power_symbol_is_still_an_orphan() {
        let sch = format!(
            "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"power:GND\"\n\t\t\t(power)\n\t\t\t(symbol \"GND_0_1\"\n\t\t\t\t(pin power_in line (at 0 0 270) (length 0)\n\t\t\t\t\t(name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t)\n{}\t(sheet_instances (path \"/\" (page \"1\")))\n)\n",
            power_symbol("#PWR01", "GND", 100.0, 80.0)
        );

        let orphans = call("find_orphan_items", &sch, json!({})).await;
        assert_eq!(orphans["orphan_count"], 1, "{orphans}");
        assert_eq!(orphans["orphans"][0]["type"], "unconnected_pin");
        assert_eq!(orphans["orphans"][0]["reference"], "#PWR01");
    }

    /// Values are read from the instance that placed each pin, not looked up by
    /// reference: on a pre-annotation sheet every part is `R?`, and a
    /// reference-keyed map would report one arbitrary value for all of them.
    #[tokio::test]
    async fn unannotated_components_keep_their_own_values() {
        let with_value = |reference: &str, uuid: &str, value: &str, x: f64| {
            symbol(reference, uuid, x, 80.0).replace(
                "(property \"Reference\"",
                &format!("(property \"Value\" \"{value}\"\n\t\t\t(at {x} 80 0)\n\t\t)\n\t\t(property \"Reference\""),
            )
        };
        let sch = schematic(&format!(
            "{}{}",
            with_value("R?", "r1", "1k", 100.0),
            with_value("R?", "r2", "10k", 120.0),
        ));

        let body = call("validate_component_connections", &sch, json!({})).await;
        let mut values: Vec<&str> = body["unconnected_pins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pin| pin["value"].as_str().unwrap())
            .collect();
        values.sort_unstable();
        assert_eq!(values, ["10k", "1k"], "{body}");
    }

    /// The power-symbol trap again, one layer down. A pseudo-label is kept out
    /// of the index's label points, but the *graph* still holds it — so a
    /// reachability fallback answered "connected" for a power symbol attached
    /// to nothing, disagreeing with `find_orphan_items` on the same sheet.
    ///
    /// There is no such fallback now: every point the graph can name is a wire
    /// endpoint, label, junction or sheet pin, and `attaches_pin` already
    /// covers all four at tolerance rather than at exact `pt_key` equality.
    #[tokio::test]
    async fn an_unwired_power_symbol_is_unconnected_for_every_tool() {
        let sch = format!(
            "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"power:GND\"\n\t\t\t(power)\n\t\t\t(symbol \"GND_0_1\"\n\t\t\t\t(pin power_in line (at 0 0 270) (length 0)\n\t\t\t\t\t(name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t)\n{}\t(sheet_instances (path \"/\" (page \"1\")))\n)\n",
            power_symbol("#PWR01", "GND", 100.0, 80.0)
        );

        let orphans = call("find_orphan_items", &sch, json!({})).await;
        assert_eq!(orphans["orphan_count"], 1, "{orphans}");
        assert_eq!(orphans["orphans"][0]["reference"], "#PWR01");

        let components = call("validate_component_connections", &sch, json!({})).await;
        assert_eq!(components["unconnected_count"], 1, "{components}");
        assert_eq!(components["unconnected_pins"][0]["reference"], "#PWR01");
    }

    /// A power symbol wired to something is connected for both — the agreement
    /// has to hold in the direction that reports no fault, too.
    #[tokio::test]
    async fn a_wired_power_symbol_is_connected_for_every_tool() {
        let sch = format!(
            "(kicad_sch\n\t(version 20260306)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"power:GND\"\n\t\t\t(power)\n\t\t\t(symbol \"GND_0_1\"\n\t\t\t\t(pin power_in line (at 0 0 270) (length 0)\n\t\t\t\t\t(name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t\t(symbol \"Test:P1\"\n\t\t\t(symbol \"P1_1_1\"\n\t\t\t\t(pin passive line (at 0 0 0) (length 2.54)\n\t\t\t\t\t(name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t)\n\t(wire\n\t\t(pts (xy 100 80) (xy 120 80))\n\t\t(uuid \"w1\")\n\t)\n{}{}\t(sheet_instances (path \"/\" (page \"1\")))\n)\n",
            power_symbol("#PWR01", "GND", 100.0, 80.0),
            symbol("U1", "u1", 120.0, 80.0),
        );

        let orphans = call("find_orphan_items", &sch, json!({})).await;
        assert_eq!(orphans["orphan_count"], 0, "{orphans}");

        let components = call("validate_component_connections", &sch, json!({})).await;
        assert_eq!(components["unconnected_count"], 0, "{components}");
    }
}
