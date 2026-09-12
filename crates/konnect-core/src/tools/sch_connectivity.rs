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
use std::collections::{BTreeSet, HashMap, HashSet};

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

/// A name a label puts on a point, with the kind of label that put it there.
///
/// The kind is what makes [`NetGraph::name_of_root`] deterministic: one net can
/// carry several names, and which one it is *called* follows KiCad's own
/// precedence rather than whichever the map happened to yield first.
#[derive(Clone)]
struct NetName {
    name: String,
    kind: LabelKind,
}

/// KiCad's net-name precedence, highest first: a global label outranks a power
/// symbol, which outranks a local label, which outranks a hierarchical one.
///
/// Verified against KiCad 10.0.5 by netlisting one net carrying each pair:
/// `+3V3` power symbol with a `LOCAL_VCC` label nets as `+3V3`, either with a
/// `GLOBAL_SYS` global label nets as `GLOBAL_SYS`, and a local label beats a
/// hierarchical one. It is `CONNECTION_SUBGRAPH`'s driver priority, minus the
/// sheet-pin and pin ranks below it — neither names a net here.
fn driver_priority(kind: LabelKind) -> u8 {
    match kind {
        LabelKind::GlobalLabel => 3,
        LabelKind::PowerSymbol => 2,
        LabelKind::NetLabel => 1,
        LabelKind::HierarchicalLabel => 0,
    }
}

/// Whether `a` names the net in preference to `b`. Equal priority is broken on
/// the name, ascending, which is KiCad's own tie-break (two local labels on one
/// net gave `AAA` over `LOCAL_VCC`, and `LOCAL_VCC` over `ZZZ`).
fn names_ahead_of(a: &NetName, b: &NetName) -> bool {
    let (pa, pb) = (driver_priority(a.kind), driver_priority(b.kind));
    pa > pb || (pa == pb && a.name < b.name)
}

/// A net's identity: the union-find root every point on that net resolves to.
///
/// Distinct from a net *name*, which a net may have several of or none at all.
pub(crate) type NetRoot = (i64, i64);

/// A graph of points joined into nets, and the name each net carries.
///
/// Built only by [`seed_net_graph`], which is why every mutator below is
/// private: once seeding calls [`NetGraph::name_nets`] the graph is frozen, and
/// `root_names` cannot go stale behind a later union. Callers get the two
/// questions they actually ask — [`root_at`](Self::root_at) for a net's
/// identity, [`name_of_root`](Self::name_of_root) for what it is called.
pub(crate) struct NetGraph {
    /// Every name at a point, not just the winning one. A rail label written
    /// over the power symbol that names it puts two names on one coordinate,
    /// and dropping the loser would keep it out of
    /// [`merge_named_nets`](Self::merge_named_nets) — so a point carrying
    /// `AAA` and `ZZZ` would never join a disconnected `ZZZ` segment.
    point_nets: HashMap<(i64, i64), Vec<NetName>>,
    parent: HashMap<(i64, i64), NetRoot>,
    /// The winning name per root, resolved once at the end of seeding.
    root_names: HashMap<NetRoot, String>,
    /// Every name on each root, sorted. A caller classifying a net — is this a
    /// rail? is it ground? — must read all of them: the winning name can be a
    /// global label that says nothing about the `+3V3` alias beside it.
    root_aliases: HashMap<NetRoot, BTreeSet<String>>,
}

impl NetGraph {
    fn new() -> Self {
        NetGraph {
            point_nets: HashMap::new(),
            parent: HashMap::new(),
            root_names: HashMap::new(),
            root_aliases: HashMap::new(),
        }
    }

    fn ensure(&mut self, k: (i64, i64)) {
        self.parent.entry(k).or_insert(k);
    }

    fn find(&mut self, k: (i64, i64)) -> NetRoot {
        self.ensure(k);
        let p = self.parent[&k];
        if p == k {
            return k;
        }
        let root = self.find(p);
        self.parent.insert(k, root);
        root
    }

    fn union(&mut self, a: (i64, i64), b: (i64, i64)) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent.insert(rb, ra);
        }
    }

    fn add_wire(&mut self, w: &Wire) {
        let a = pt_key(w.x1, w.y1);
        let b = pt_key(w.x2, w.y2);
        self.ensure(a);
        self.ensure(b);
        self.union(a, b);
    }

    fn add_label(&mut self, x: f64, y: f64, net: &str, kind: LabelKind) {
        let k = pt_key(x, y);
        self.ensure(k);
        let held = self.point_nets.entry(k).or_default();
        // Kept whole, and ranked only when a name has to be *chosen*: every
        // name at this point still has to reach `merge_named_nets`, or a net
        // joined to this one by the losing name would stay a net of its own.
        if !held.iter().any(|name| name.name == net) {
            held.push(NetName {
                name: net.to_string(),
                kind,
            });
        }
    }

    /// The graph node a point belongs to. Two points share a root exactly when
    /// they are on one net — through wire and junction geometry, and through
    /// [`merge_named_nets`](Self::merge_named_nets) for the pieces KiCad joins
    /// by name alone.
    ///
    /// This is the comparison a caller wants wherever it would otherwise
    /// compare net *names*: a net carrying two names has one root and would
    /// fail a name comparison against itself.
    pub(crate) fn root_at(&mut self, x: f64, y: f64) -> NetRoot {
        self.find(pt_key(x, y))
    }

    /// What the net at `root` is called, or `None` when no label names it.
    ///
    /// A net can carry more than one name — a rail named by a `+3V3` power
    /// symbol that also has a label on it, a local label on a net that also
    /// carries a global one. The answer is the one KiCad's netlister would
    /// choose (see [`driver_priority`]), not whichever the map yielded first:
    /// that was `HashMap` iteration order, so the name moved between runs of
    /// the same binary and the audits reported the losing name as a rail of its
    /// own.
    pub(crate) fn name_of_root(&self, root: NetRoot) -> Option<String> {
        self.root_names.get(&root).cloned()
    }

    /// Every name on the net at `root`, sorted, empty when nothing names it.
    ///
    /// [`name_of_root`](Self::name_of_root) answers what the net is *called*;
    /// this answers what it is *known as*, which is the question a classifier
    /// asks. A rail whose winning name is the global label `SYS` is still the
    /// `+3V3` rail, and reading only the winner made it neither a rail to
    /// decouple nor a pull-up destination.
    pub(crate) fn aliases_of_root(&self, root: NetRoot) -> BTreeSet<String> {
        self.root_aliases.get(&root).cloned().unwrap_or_default()
    }

    /// Join the points that carry the same name.
    ///
    /// KiCad nets a sheet by name as well as by wire: two segments each
    /// carrying a `SIG` label are one net, and every `GND` power symbol on the
    /// sheet is the same rail. Wires and junctions alone answer for one piece
    /// of copper, so without this a root is a *segment* rather than a net —
    /// and a decoupling capacitor drawn the normal way, on its own stub with
    /// its own `+3V3` symbol, shares no root with the pin it decouples.
    ///
    /// Anchoring each name at the first point that carries it and unioning the
    /// rest into it makes every same-named point one root, whatever order the
    /// map yields them in.
    fn merge_named_nets(&mut self) {
        let mut anchors: HashMap<String, (i64, i64)> = HashMap::new();
        let named: Vec<((i64, i64), Vec<String>)> = self
            .point_nets
            .iter()
            .map(|(k, names)| (*k, names.iter().map(|name| name.name.clone()).collect()))
            .collect();
        for (k, names) in named {
            for name in names {
                match anchors.get(&name) {
                    Some(&anchor) => self.union(anchor, k),
                    None => {
                        anchors.insert(name, k);
                    }
                }
            }
        }
    }

    /// Resolve one winning name per root. Called once, by [`seed_net_graph`],
    /// after the last union — a name is a property of the finished net, so
    /// there is nothing to resolve until the graph stops moving.
    fn name_nets(&mut self) {
        let mut best: HashMap<NetRoot, NetName> = HashMap::new();
        let mut aliases: HashMap<NetRoot, BTreeSet<String>> = HashMap::new();
        for k in self.point_nets.keys().copied().collect::<Vec<_>>() {
            let root = self.find(k);
            for candidate in self.point_nets[&k].clone() {
                aliases
                    .entry(root)
                    .or_default()
                    .insert(candidate.name.clone());
                let wins = match best.get(&root) {
                    Some(held) => names_ahead_of(&candidate, held),
                    None => true,
                };
                if wins {
                    best.insert(root, candidate);
                }
            }
        }
        self.root_names = best
            .into_iter()
            .map(|(root, name)| (root, name.name))
            .collect();
        self.root_aliases = aliases;
    }

    /// The name of the net at a point — [`root_at`](Self::root_at) then
    /// [`name_of_root`](Self::name_of_root). For reporting a single point only:
    /// a caller asking whether two points are on one net wants their roots, not
    /// their names, since a net can carry more than one.
    pub(crate) fn net_at(&mut self, x: f64, y: f64) -> Option<String> {
        let root = self.root_at(x, y);
        self.name_of_root(root)
    }

    pub(crate) fn points_on_net(&mut self, net: &str) -> Vec<(i64, i64)> {
        // Collect keys first to avoid simultaneous borrow of point_nets and self.find()
        let net_keys: Vec<(i64, i64)> = self
            .point_nets
            .iter()
            .filter(|(_, names)| names.iter().any(|name| name.name == net))
            .map(|(k, _)| *k)
            .collect();
        let net_roots: HashSet<NetRoot> = net_keys.iter().map(|k| self.find(*k)).collect();
        self.points_on_roots(&net_roots)
    }

    /// Every point on any of `roots` — the same walk [`points_on_net`] does,
    /// for a caller that already holds identities rather than a name. Asking
    /// once for several nets also walks the graph once instead of per net.
    ///
    /// [`points_on_net`]: Self::points_on_net
    pub(crate) fn points_on_roots(&mut self, roots: &HashSet<NetRoot>) -> Vec<(i64, i64)> {
        let all_keys: Vec<(i64, i64)> = self.parent.keys().cloned().collect();
        all_keys
            .into_iter()
            .filter(|k| roots.contains(&self.find(*k)))
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
        graph.add_label(label.x, label.y, &label.net, label.kind);
        attach(&mut graph, label.x, label.y);
    }
    for &(x, y) in junctions {
        graph.ensure(pt_key(x, y));
        attach(&mut graph, x, y);
    }
    for &(x, y) in sheet_pins {
        graph.ensure(pt_key(x, y));
    }
    graph.merge_named_nets();
    graph.name_nets();
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
        .map(|label| (graph.root_at(label.x, label.y), label))
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

    /// The real KiCad fixture, at the graph level: `TP7` reaches the `+3V3`
    /// rail only through the `ALT` label sitting on the second `+3V3` power
    /// symbol's own pin. KiCad nets them together (`+3V3` has five pins), so a
    /// graph that keeps one name per point loses that join — and with it the
    /// alias every classifier reads.
    ///
    /// Provenance and KiCad's own net table: `two_name_nets.README.md`.
    #[tokio::test]
    async fn an_alias_on_a_power_symbol_pin_joins_its_segment() {
        let sch = include_str!("../../tests/fixtures/two_name_nets.kicad_sch");
        let (tree, wires, labels) = index_for(sch);
        let mut graph = net_graph_for(&tree, &wires, &labels);

        // TP7's pin, and U1's VCC pin at the other end of the rail.
        let stub = graph.root_at(30.48, 88.9);
        let rail = graph.root_at(110.49, 72.39);
        assert_eq!(stub, rail, "TP7 joins the rail through the ALT alias");
        assert_eq!(graph.name_of_root(rail), Some("+3V3".to_string()));
        assert!(
            graph.aliases_of_root(rail).contains("ALT"),
            "the alias survives to the classifiers: {:?}",
            graph.aliases_of_root(rail)
        );

        // The same join without an alias: two `+3V3` power symbols on separate
        // stubs are one rail, and C1's pin reaches U1's through it.
        assert_eq!(graph.root_at(59.69, 96.52), rail);

        // And a plain label doing it: `U1.5` and `R1.2` carry `SDA` on segments
        // that share no wire. KiCad nets both (`/SDA` has two pins).
        assert_eq!(graph.root_at(120.65, 77.47), graph.root_at(160.02, 63.5));
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
