//! Direct `.kicad_sch` to semantic-scene conversion for the native renderer.
//!
//! The scene deliberately contains no Vello types. Rendering and editing use
//! the same stable KiCad UUIDs and schematic-space bounds, while the graphics
//! backend remains replaceable.

use anyhow::{Context, Result};
use konnect_sexp::{
    geometry::{transform_pin, PinTransform},
    parse_sexp, read_consistent,
    writer::{find_balanced_block, find_block_starts, find_direct_child_blocks},
    SexpEdit, SexpNode,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::kicad_font::{boundary_width, layout_width};
use crate::kicad_rtree::{glibcxx_sort_by, traversal_order_with_refresh};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Point {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

impl Point {
    const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bounds {
    pub(crate) min_x: f64,
    pub(crate) min_y: f64,
    pub(crate) max_x: f64,
    pub(crate) max_y: f64,
}

impl Bounds {
    fn from_points(points: impl IntoIterator<Item = Point>) -> Option<Self> {
        let mut points = points.into_iter();
        let first = points.next()?;
        let mut bounds = Self {
            min_x: first.x,
            min_y: first.y,
            max_x: first.x,
            max_y: first.y,
        };
        for point in points {
            bounds.include(point);
        }
        Some(bounds)
    }

    fn include(&mut self, point: Point) {
        self.min_x = self.min_x.min(point.x);
        self.min_y = self.min_y.min(point.y);
        self.max_x = self.max_x.max(point.x);
        self.max_y = self.max_y.max(point.y);
    }

    fn include_bounds(&mut self, other: Self) {
        self.include(Point::new(other.min_x, other.min_y));
        self.include(Point::new(other.max_x, other.max_y));
    }

    fn inflate(&mut self, amount: f64) {
        self.min_x -= amount;
        self.min_y -= amount;
        self.max_x += amount;
        self.max_y += amount;
    }

    pub(crate) fn contains(self, point: Point, tolerance: f64) -> bool {
        point.x >= self.min_x - tolerance
            && point.x <= self.max_x + tolerance
            && point.y >= self.min_y - tolerance
            && point.y <= self.max_y + tolerance
    }

    fn intersects(self, other: Self) -> bool {
        self.min_x <= other.max_x
            && self.max_x >= other.min_x
            && self.min_y <= other.max_y
            && self.max_y >= other.min_y
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorRole {
    Border,
    Bus,
    GraphicText,
    Junction,
    Label,
    NoConnect,
    Page,
    Pin,
    PinName,
    PinNumber,
    SheetFile,
    Symbol,
    Text,
    Wire,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StrokeStyle {
    pub(crate) width_mm: f64,
    pub(crate) role: ColorRole,
}

impl StrokeStyle {
    const fn new(width_mm: f64, role: ColorRole) -> Self {
        Self { width_mm, role }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Primitive {
    Line {
        from: Point,
        to: Point,
        style: StrokeStyle,
    },
    Polyline {
        points: Vec<Point>,
        closed: bool,
        style: StrokeStyle,
        fill: bool,
    },
    Rect {
        bounds: Bounds,
        style: StrokeStyle,
        fill: bool,
    },
    Circle {
        center: Point,
        radius: f64,
        style: StrokeStyle,
        fill: bool,
    },
    Arc {
        start: Point,
        mid: Point,
        end: Point,
        style: StrokeStyle,
    },
    Bezier {
        points: Vec<Point>,
        style: StrokeStyle,
    },
    Text {
        position: Point,
        rotation_deg: f64,
        size_mm: f64,
        stroke_width_mm: f64,
        align: TextAlign,
        italic: bool,
        role: ColorRole,
        text: String,
    },
}

impl Primitive {
    pub(crate) fn bounds(&self) -> Option<Bounds> {
        match self {
            Self::Line { from, to, .. } => Bounds::from_points([*from, *to]),
            Self::Polyline { points, .. } | Self::Bezier { points, .. } => {
                Bounds::from_points(points.iter().copied())
            }
            Self::Rect { bounds, .. } => Some(*bounds),
            Self::Circle { center, radius, .. } => Some(Bounds {
                min_x: center.x - radius,
                min_y: center.y - radius,
                max_x: center.x + radius,
                max_y: center.y + radius,
            }),
            Self::Arc {
                start, mid, end, ..
            } => Bounds::from_points([*start, *mid, *end]),
            Self::Text {
                position,
                rotation_deg,
                size_mm,
                stroke_width_mm,
                align,
                italic,
                text,
                ..
            } => {
                let width = layout_width(text, *size_mm, *stroke_width_mm).max(*size_mm);
                let left = match align {
                    TextAlign::Left => 0.0,
                    TextAlign::Center => -width / 2.0,
                    TextAlign::Right => -width,
                };
                Bounds::from_points(
                    [
                        Point::new(left, -*size_mm),
                        Point::new(left + width, -*size_mm),
                        Point::new(left + width, *size_mm * 0.35),
                        Point::new(left, *size_mm * 0.35),
                    ]
                    .into_iter()
                    .map(|mut corner| {
                        if *italic {
                            corner.x -= corner.y * 0.125;
                        }
                        rotate_about(corner, *position, *rotation_deg)
                    }),
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Bus,
    BusEntry,
    Junction,
    Label,
    NoConnect,
    Sheet,
    Symbol,
    Text,
    Wire,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SceneObject {
    pub(crate) uuid: String,
    pub(crate) kind: ObjectKind,
    pub(crate) item_type: i32,
    pub(crate) label: String,
    pub(crate) search_text: String,
    pub(crate) properties: Vec<ObjectProperty>,
    pub(crate) bounds: Bounds,
    /// KiCad's conservative integer bounding box used by `SCH_RTREE`.
    ///
    /// This is deliberately separate from `bounds`: hit testing and the
    /// selection halo follow the actual rendered geometry, while KiCad's
    /// traversal and overlap-redraw decisions include font and pin guards.
    pub(crate) index_bounds: Bounds,
    pub(crate) initial_index_bounds: Bounds,
    pub(crate) primitive_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObjectProperty {
    pub(crate) name: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SchematicScene {
    pub(crate) file: PathBuf,
    pub(crate) source: Arc<str>,
    pub(crate) width_mm: f64,
    pub(crate) height_mm: f64,
    pub(crate) primitives: Vec<Primitive>,
    pub(crate) objects: Vec<SceneObject>,
    pub(crate) coverage: RenderCoverage,
    pub(crate) diagnostics: Vec<ConnectivityDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectivityDiagnosticKind {
    ConnectedNoConnect,
    DanglingWire,
    DuplicateReference,
    DuplicateSheetName,
    DuplicateSheetPin,
    MissingJunction,
    UnconnectedBusEntry,
    UnpositionedSheetField,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConnectivityDiagnostic {
    pub(crate) kind: ConnectivityDiagnosticKind,
    pub(crate) point: Point,
    pub(crate) message: String,
}

/// Explicit accounting of top-level constructs the native renderer cannot
/// faithfully display yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RenderCoverage {
    pub(crate) unsupported: Vec<UnsupportedConstruct>,
}

impl RenderCoverage {
    pub(crate) fn is_complete(&self) -> bool {
        self.unsupported.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnsupportedConstruct {
    pub(crate) kind: String,
    pub(crate) count: usize,
}

impl SchematicScene {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let source =
            read_consistent(path).with_context(|| format!("failed to read {}", path.display()))?;
        Self::from_source(path, source)
    }

    pub(crate) fn from_source(path: &Path, source: String) -> Result<Self> {
        let root =
            parse_sexp(&source).with_context(|| format!("failed to parse {}", path.display()))?;
        let coverage = analyze_render_coverage(&root);
        let diagnostics = analyze_connectivity(&root);
        let (width_mm, height_mm) = paper_size(&root);
        let mut builder = SceneBuilder::new(path.to_path_buf(), Arc::<str>::from(source));
        builder.draw_page(width_mm, height_mm, &root);
        builder.draw_document(&root);
        Ok(Self {
            file: builder.file,
            source: builder.source,
            width_mm,
            height_mm,
            primitives: builder.primitives,
            objects: builder.objects,
            coverage,
            diagnostics,
        })
    }

    pub(crate) fn hit_test(&self, point: Point, tolerance_mm: f64) -> Option<&SceneObject> {
        self.objects
            .iter()
            .rev()
            .find(|object| object.bounds.contains(point, tolerance_mm))
    }
}

fn analyze_connectivity(root: &SexpNode) -> Vec<ConnectivityDiagnostic> {
    const TOLERANCE_MM: f64 = 0.01;
    let wires = konnect_sexp::schematic::extract_wires(root);
    let buses = konnect_sexp::schematic::extract_buses(root);
    let mut anchors = konnect_sexp::schematic::extract_labels(root)
        .into_iter()
        .map(|label| Point::new(label.x, label.y))
        .collect::<Vec<_>>();
    let junctions = root
        .find_all("junction")
        .into_iter()
        .filter_map(at_point)
        .collect::<Vec<_>>();
    anchors.extend(junctions.iter().copied());
    let no_connects = root
        .find_all("no_connect")
        .into_iter()
        .filter_map(at_point)
        .collect::<Vec<_>>();
    anchors.extend(no_connects.iter().copied());

    let lib_symbols = root
        .find("lib_symbols")
        .map(|symbols| symbols.find_all("symbol"))
        .unwrap_or_default();
    let instances = konnect_sexp::schematic::extract_symbol_instances(root);
    for instance in &instances {
        let Some(lib_symbol) = lib_symbols
            .iter()
            .find(|symbol| symbol.get(1).and_then(SexpNode::as_str) == Some(&instance.lib_id))
        else {
            continue;
        };
        anchors.extend(
            konnect_sexp::schematic::extract_lib_pins(lib_symbol)
                .into_iter()
                .map(|pin| {
                    let (x, y) =
                        konnect_sexp::schematic::pin_endpoint(&pin, instance.pin_transform());
                    Point::new(x, y)
                }),
        );
    }

    let mut diagnostics = Vec::new();
    for point in &no_connects {
        if wires
            .iter()
            .any(|wire| point_on_wire(*point, wire, TOLERANCE_MM))
        {
            diagnostics.push(ConnectivityDiagnostic {
                kind: ConnectivityDiagnosticKind::ConnectedNoConnect,
                point: *point,
                message: format!(
                    "No-connect marker at {:.3}, {:.3} mm is attached to a wire",
                    point.x, point.y
                ),
            });
        }
    }
    let mut references = HashMap::<&str, Point>::new();
    for instance in &instances {
        if instance.reference.is_empty() || instance.reference.ends_with('?') {
            continue;
        }
        let point = Point::new(instance.x, instance.y);
        if let Some(first) = references.insert(&instance.reference, point) {
            diagnostics.push(ConnectivityDiagnostic {
                kind: ConnectivityDiagnosticKind::DuplicateReference,
                point,
                message: format!(
                    "Duplicate reference {} at {:.3}, {:.3} mm; first at {:.3}, {:.3} mm",
                    instance.reference, point.x, point.y, first.x, first.y
                ),
            });
        }
    }
    let mut reported_dangling = HashSet::new();
    for (wire_index, wire) in wires.iter().enumerate() {
        for point in [Point::new(wire.x1, wire.y1), Point::new(wire.x2, wire.y2)] {
            let touches_anchor = anchors
                .iter()
                .any(|anchor| points_near(point, *anchor, TOLERANCE_MM));
            let touches_wire = wires.iter().enumerate().any(|(other_index, other)| {
                wire_index != other_index && point_on_wire(point, other, TOLERANCE_MM)
            });
            let quantized = (
                (point.x / TOLERANCE_MM).round() as i64,
                (point.y / TOLERANCE_MM).round() as i64,
            );
            if !touches_anchor && !touches_wire && reported_dangling.insert(quantized) {
                diagnostics.push(ConnectivityDiagnostic {
                    kind: ConnectivityDiagnosticKind::DanglingWire,
                    point,
                    message: format!("Dangling wire end at {:.3}, {:.3} mm", point.x, point.y),
                });
            }
        }
    }

    for (x, y) in konnect_sexp::schematic::find_t_junctions(&wires, TOLERANCE_MM) {
        let point = Point::new(x, y);
        if !junctions
            .iter()
            .any(|junction| points_near(point, *junction, TOLERANCE_MM))
        {
            diagnostics.push(ConnectivityDiagnostic {
                kind: ConnectivityDiagnosticKind::MissingJunction,
                point,
                message: format!("T-junction at {:.3}, {:.3} mm needs a junction dot", x, y),
            });
        }
    }
    for entry in root.find_all("bus_entry") {
        let Some(start) = at_point(entry) else {
            continue;
        };
        let Some(size) = entry.find("size") else {
            continue;
        };
        let end = Point::new(
            start.x + size.get_f64(1).unwrap_or_default(),
            start.y + size.get_f64(2).unwrap_or_default(),
        );
        let start_bus = buses
            .iter()
            .any(|bus| point_on_wire(start, bus, TOLERANCE_MM));
        let end_bus = buses
            .iter()
            .any(|bus| point_on_wire(end, bus, TOLERANCE_MM));
        let start_wire = wires
            .iter()
            .any(|wire| point_on_wire(start, wire, TOLERANCE_MM));
        let end_wire = wires
            .iter()
            .any(|wire| point_on_wire(end, wire, TOLERANCE_MM));
        if !((start_bus && end_wire) || (end_bus && start_wire)) {
            diagnostics.push(ConnectivityDiagnostic {
                kind: ConnectivityDiagnosticKind::UnconnectedBusEntry,
                point: start,
                message: format!(
                    "Bus entry at {:.3}, {:.3} mm does not bridge a bus and wire",
                    start.x, start.y
                ),
            });
        }
    }
    let mut sheet_names = HashMap::<&str, Point>::new();
    for sheet in root.find_all("sheet") {
        let sheet_point = at_point(sheet).unwrap_or(Point::new(0.0, 0.0));
        if let Some(name) = sheet_property(sheet, "Sheetname").filter(|name| !name.is_empty()) {
            if let Some(first) = sheet_names.insert(name, sheet_point) {
                diagnostics.push(ConnectivityDiagnostic {
                    kind: ConnectivityDiagnosticKind::DuplicateSheetName,
                    point: sheet_point,
                    message: format!(
                        "Duplicate hierarchical sheet name {name} at {:.3}, {:.3} mm; first at {:.3}, {:.3} mm",
                        sheet_point.x, sheet_point.y, first.x, first.y
                    ),
                });
            }
        }
        let mut pin_names = HashSet::new();
        for pin in sheet.find_all("pin") {
            let Some(name) = pin.get(1).and_then(SexpNode::as_str) else {
                continue;
            };
            if !pin_names.insert(name) {
                diagnostics.push(ConnectivityDiagnostic {
                    kind: ConnectivityDiagnosticKind::DuplicateSheetPin,
                    point: at_point(pin).unwrap_or(sheet_point),
                    message: format!("Hierarchical sheet contains duplicate pin {name}"),
                });
            }
        }
        for property in sheet.find_all("property") {
            let name = property.get(1).and_then(SexpNode::as_str).unwrap_or("");
            if matches!(name, "Sheetname" | "Sheetfile")
                && !is_hidden(property)
                && at_point(property).is_none()
            {
                diagnostics.push(ConnectivityDiagnostic {
                    kind: ConnectivityDiagnosticKind::UnpositionedSheetField,
                    point: Point::new(0.0, 0.0),
                    message: format!(
                        "Visible {name} has no position; KiCad plots it at the page origin"
                    ),
                });
            }
        }
    }
    diagnostics
}

fn points_near(a: Point, b: Point, tolerance: f64) -> bool {
    (a.x - b.x).hypot(a.y - b.y) <= tolerance
}

fn point_on_wire(point: Point, wire: &konnect_sexp::schematic::Wire, tolerance: f64) -> bool {
    konnect_sexp::geometry::point_on_segment(
        point.x, point.y, wire.x1, wire.y1, wire.x2, wire.y2, tolerance,
    )
}

fn analyze_render_coverage(root: &SexpNode) -> RenderCoverage {
    const RENDERED: &[&str] = &[
        "bus",
        "bus_entry",
        "global_label",
        "hierarchical_label",
        "junction",
        "label",
        "no_connect",
        "sheet",
        "symbol",
        "text",
        "wire",
    ];
    const NON_VISUAL: &[&str] = &[
        "bus_alias",
        "embedded_fonts",
        "generator",
        "generator_version",
        "lib_symbols",
        "paper",
        "sheet_instances",
        "symbol_instances",
        "title_block",
        "uuid",
        "version",
    ];

    let mut unsupported = HashMap::<String, usize>::new();
    for child in root.children().unwrap_or_default().iter().skip(1) {
        let Some(kind) = child.head() else {
            continue;
        };
        if !RENDERED.contains(&kind) && !NON_VISUAL.contains(&kind) {
            *unsupported.entry(kind.to_owned()).or_default() += 1;
        }
    }
    let mut unsupported = unsupported
        .into_iter()
        .map(|(kind, count)| UnsupportedConstruct { kind, count })
        .collect::<Vec<_>>();
    unsupported.sort_by(|left, right| left.kind.cmp(&right.kind));
    RenderCoverage { unsupported }
}

fn overlapping_object_uuids(objects: &[SceneObject]) -> HashSet<String> {
    objects
        .iter()
        .enumerate()
        .filter(|(index, object)| {
            objects.iter().enumerate().any(|(other_index, other)| {
                *index != other_index && object.index_bounds.intersects(other.index_bounds)
            })
        })
        .map(|(_, object)| object.uuid.clone())
        .collect()
}

/// Create the smallest source edit needed to move a placed symbol.
///
/// The placed symbol anchor and its directly-owned field anchors move
/// together. Library symbols are never candidates because only direct
/// children of the schematic root are searched.
pub(crate) fn move_symbol_source(source: &str, uuid: &str, dx: f64, dy: f64) -> Result<String> {
    let symbol_range = find_direct_child_blocks(source, "kicad_sch")
        .into_iter()
        .find(|(start, end)| {
            let block = &source[*start..*end];
            parse_sexp(block).ok().is_some_and(|node| {
                node.head() == Some("symbol") && node.find_str("uuid") == Some(uuid)
            })
        })
        .with_context(|| format!("placed symbol {uuid} was not found"))?;
    let (symbol_start, symbol_end) = symbol_range;
    let symbol_block = &source[symbol_start..symbol_end];
    let mut edits = Vec::new();

    for (child_start, child_end) in find_direct_child_blocks(symbol_block, "symbol") {
        let child = &symbol_block[child_start..child_end];
        let node = parse_sexp(child)?;
        match node.head() {
            Some("at") => edits.push(translated_at_edit(
                symbol_start + child_start,
                symbol_start + child_end,
                &node,
                dx,
                dy,
            )?),
            Some("property") => {
                for (at_start, at_end) in find_direct_child_blocks(child, "property") {
                    let at_block = &child[at_start..at_end];
                    let at_node = parse_sexp(at_block)?;
                    if at_node.head() == Some("at") {
                        edits.push(translated_at_edit(
                            symbol_start + child_start + at_start,
                            symbol_start + child_start + at_end,
                            &at_node,
                            dx,
                            dy,
                        )?);
                    }
                }
            }
            _ => {}
        }
    }

    if edits.is_empty() {
        anyhow::bail!("placed symbol {uuid} has no position");
    }
    Ok(konnect_sexp::apply_edits(source.to_owned(), edits))
}

/// Translate any supported UUID-owned top-level schematic item without
/// reserializing its neighbors or unknown fields.
pub(crate) fn move_item_source(source: &str, uuid: &str, dx: f64, dy: f64) -> Result<String> {
    let item_range = find_direct_child_blocks(source, "kicad_sch")
        .into_iter()
        .find(|(start, end)| {
            parse_sexp(&source[*start..*end])
                .ok()
                .is_some_and(|node| node.find_str("uuid") == Some(uuid))
        })
        .with_context(|| format!("schematic item {uuid} was not found"))?;
    let item = &source[item_range.0..item_range.1];
    let node = parse_sexp(item)?;
    if node.head() == Some("symbol") {
        return move_symbol_source(source, uuid, dx, dy);
    }
    let coordinate_tag = if matches!(node.head(), Some("wire" | "bus")) {
        "xy"
    } else {
        "at"
    };
    let mut edits = Vec::new();
    for local_start in find_block_starts(item, coordinate_tag) {
        let Some((start, end)) = find_balanced_block(item, local_start) else {
            continue;
        };
        let point = parse_sexp(&item[start..end])?;
        if point.head() == Some(coordinate_tag) {
            edits.push(translated_point_edit(
                item_range.0 + start,
                item_range.0 + end,
                &point,
                dx,
                dy,
            )?);
        }
    }
    if edits.is_empty() {
        anyhow::bail!("schematic item {uuid} has no movable coordinates");
    }
    Ok(konnect_sexp::apply_edits(source.to_owned(), edits))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConnectedWireMove {
    pub(crate) uuid: String,
    pub(crate) start: Point,
    pub(crate) end: Point,
    pub(crate) move_start: bool,
    pub(crate) move_end: bool,
}

/// Find wires whose endpoints are electrically attached to the selected items.
///
/// A wire selected directly is excluded because its normal item transform owns
/// both endpoints. For symbols, only pins belonging to the placed unit are
/// considered; sheet pins and point-like electrical objects use their exact
/// connection anchors.
pub(crate) fn connected_wire_moves(
    source: &str,
    selected_uuids: &HashSet<String>,
) -> Result<Vec<ConnectedWireMove>> {
    const TOLERANCE_MM: f64 = 0.01;

    let root = parse_sexp(source)?;
    let libraries = library_symbols(&root);
    let mut anchors = Vec::new();
    for item in root.children().unwrap_or_default().iter().skip(1) {
        let Some(uuid) = item.find_str("uuid") else {
            continue;
        };
        if !selected_uuids.contains(uuid) {
            continue;
        }
        match item.head() {
            Some("symbol") => {
                let Some(lib_id) = item.find_str("lib_id") else {
                    continue;
                };
                let (Some(definition), Some(origin)) = (libraries.get(lib_id), at_point(item))
                else {
                    continue;
                };
                collect_symbol_connection_points(
                    definition,
                    &libraries,
                    child_u32(item, "unit").unwrap_or(1),
                    symbol_transform(item, origin),
                    &mut anchors,
                );
            }
            Some("sheet") => {
                anchors.extend(item.find_all("pin").into_iter().filter_map(at_point));
            }
            Some("bus_entry") => {
                if let Some(start) = at_point(item) {
                    anchors.push(start);
                    if let Some(size) = item.find("size") {
                        anchors.push(Point::new(
                            start.x + size.get_f64(1).unwrap_or_default(),
                            start.y + size.get_f64(2).unwrap_or_default(),
                        ));
                    }
                }
            }
            Some("wire") => {}
            _ => {
                if let Some(point) = at_point(item) {
                    anchors.push(point);
                }
            }
        }
    }

    let mut connected = konnect_sexp::schematic::extract_wires(&root)
        .into_iter()
        .filter_map(|wire| {
            let uuid = wire.uuid?;
            if selected_uuids.contains(&uuid) {
                return None;
            }
            let start = Point::new(wire.x1, wire.y1);
            let end = Point::new(wire.x2, wire.y2);
            let move_start = anchors
                .iter()
                .any(|anchor| points_near(start, *anchor, TOLERANCE_MM));
            let move_end = anchors
                .iter()
                .any(|anchor| points_near(end, *anchor, TOLERANCE_MM));
            (move_start || move_end).then_some(ConnectedWireMove {
                uuid,
                start,
                end,
                move_start,
                move_end,
            })
        })
        .collect::<Vec<_>>();
    connected.sort_by(|left, right| left.uuid.cmp(&right.uuid));
    Ok(connected)
}

/// Move selected items and all attached wire endpoints in one source image.
pub(crate) fn move_items_with_connected_wires(
    source: &str,
    selected_uuids: &HashSet<String>,
    dx: f64,
    dy: f64,
) -> Result<(String, Vec<String>)> {
    let connected = connected_wire_moves(source, selected_uuids)?;
    let mut selected = selected_uuids.iter().cloned().collect::<Vec<_>>();
    selected.sort_unstable();
    let mut edited = source.to_owned();
    for uuid in &selected {
        edited = move_item_source(&edited, uuid, dx, dy)?;
    }
    for wire in &connected {
        edited = move_wire_endpoints_source(
            &edited,
            &wire.uuid,
            wire.move_start,
            wire.move_end,
            dx,
            dy,
        )?;
    }
    selected.extend(connected.into_iter().map(|wire| wire.uuid));
    Ok((edited, selected))
}

fn collect_symbol_connection_points(
    definition: &SexpNode,
    libraries: &HashMap<String, &SexpNode>,
    unit: u32,
    transform: PinTransform,
    output: &mut Vec<Point>,
) {
    if let Some(parent_name) = definition.find_str("extends") {
        if let Some(parent) = libraries.get(parent_name) {
            collect_symbol_connection_points(parent, libraries, unit, transform, output);
        }
    }
    output.extend(
        definition
            .find_all("pin")
            .into_iter()
            .filter_map(at_point)
            .map(|point| transform_point(point, transform)),
    );
    for child in definition.find_all("symbol") {
        let name = child.get(1).and_then(SexpNode::as_str).unwrap_or("");
        if symbol_unit_matches(name, unit) {
            output.extend(
                child
                    .find_all("pin")
                    .into_iter()
                    .filter_map(at_point)
                    .map(|point| transform_point(point, transform)),
            );
        }
    }
}

fn move_wire_endpoints_source(
    source: &str,
    uuid: &str,
    move_start: bool,
    move_end: bool,
    dx: f64,
    dy: f64,
) -> Result<String> {
    if move_start && move_end {
        return move_item_source(source, uuid, dx, dy);
    }
    let (item_start, item_end) = find_direct_child_blocks(source, "kicad_sch")
        .into_iter()
        .find(|(start, end)| {
            parse_sexp(&source[*start..*end]).ok().is_some_and(|node| {
                node.head() == Some("wire") && node.find_str("uuid") == Some(uuid)
            })
        })
        .with_context(|| format!("connected wire {uuid} was not found"))?;
    let item = &source[item_start..item_end];
    let mut point_ranges = find_block_starts(item, "xy")
        .into_iter()
        .filter_map(|start| find_balanced_block(item, start))
        .filter(|(start, end)| {
            parse_sexp(&item[*start..*end])
                .ok()
                .is_some_and(|node| node.head() == Some("xy"))
        })
        .take(2)
        .collect::<Vec<_>>();
    if point_ranges.len() < 2 {
        point_ranges = ["start", "end"]
            .into_iter()
            .filter_map(|tag| {
                find_block_starts(item, tag)
                    .into_iter()
                    .find_map(|start| find_balanced_block(item, start))
            })
            .collect();
    }
    if point_ranges.len() != 2 {
        anyhow::bail!("connected wire {uuid} does not contain exactly two endpoints");
    }
    let mut edits = Vec::with_capacity(1);
    for (index, (start, end)) in point_ranges.into_iter().enumerate() {
        if (index == 0 && move_start) || (index == 1 && move_end) {
            let point = parse_sexp(&item[start..end])?;
            edits.push(translated_point_edit(
                item_start + start,
                item_start + end,
                &point,
                dx,
                dy,
            )?);
        }
    }
    Ok(konnect_sexp::apply_edits(source.to_owned(), edits))
}

/// Rotate one placed symbol by `delta_degrees`, preserving all unrelated bytes.
pub(crate) fn rotate_symbol_source(source: &str, uuid: &str, delta_degrees: f64) -> Result<String> {
    let symbol_range = find_direct_child_blocks(source, "kicad_sch")
        .into_iter()
        .find(|(start, end)| {
            let block = &source[*start..*end];
            parse_sexp(block).ok().is_some_and(|node| {
                node.head() == Some("symbol") && node.find_str("uuid") == Some(uuid)
            })
        })
        .with_context(|| format!("placed symbol {uuid} was not found"))?;
    let block = &source[symbol_range.0..symbol_range.1];
    let (at_start, at_end) = find_direct_child_blocks(block, "symbol")
        .into_iter()
        .find(|(start, end)| {
            parse_sexp(&block[*start..*end])
                .ok()
                .is_some_and(|node| node.head() == Some("at"))
        })
        .with_context(|| format!("placed symbol {uuid} has no position"))?;
    let at = parse_sexp(&block[at_start..at_end])?;
    let x = at
        .get_f64(1)
        .context("symbol position is missing its x coordinate")?;
    let y = at
        .get_f64(2)
        .context("symbol position is missing its y coordinate")?;
    let rotation = (at.get_f64(3).unwrap_or(0.0) + delta_degrees).rem_euclid(360.0);
    let replacement = format!(
        "(at {} {} {})",
        format_coord(x),
        format_coord(y),
        format_coord(rotation)
    );
    Ok(konnect_sexp::apply_edits(
        source.to_owned(),
        vec![SexpEdit::replace(
            symbol_range.0 + at_start,
            symbol_range.0 + at_end,
            replacement,
        )],
    ))
}

/// Rotate a bus entry's size vector by 90 degrees clockwise.
pub(crate) fn rotate_bus_entry_source(source: &str, uuid: &str) -> Result<String> {
    let item_range = find_direct_child_blocks(source, "kicad_sch")
        .into_iter()
        .find(|(start, end)| {
            parse_sexp(&source[*start..*end]).ok().is_some_and(|node| {
                node.head() == Some("bus_entry") && node.find_str("uuid") == Some(uuid)
            })
        })
        .with_context(|| format!("bus entry {uuid} was not found"))?;
    let block = &source[item_range.0..item_range.1];
    let (size_start, size_end) = find_direct_child_blocks(block, "bus_entry")
        .into_iter()
        .find(|(start, end)| {
            parse_sexp(&block[*start..*end])
                .ok()
                .is_some_and(|node| node.head() == Some("size"))
        })
        .with_context(|| format!("bus entry {uuid} has no size vector"))?;
    let size = parse_sexp(&block[size_start..size_end])?;
    let dx = size
        .get_f64(1)
        .context("bus entry size is missing its x component")?;
    let dy = size
        .get_f64(2)
        .context("bus entry size is missing its y component")?;
    let replacement = format!("(size {} {})", format_coord(-dy), format_coord(dx));
    Ok(konnect_sexp::apply_edits(
        source.to_owned(),
        vec![SexpEdit::replace(
            item_range.0 + size_start,
            item_range.0 + size_end,
            replacement,
        )],
    ))
}

/// Rotate any currently supported top-level item.
pub(crate) fn rotate_item_source(source: &str, uuid: &str) -> Result<String> {
    let item = find_direct_child_blocks(source, "kicad_sch")
        .into_iter()
        .find_map(|(start, end)| {
            let node = parse_sexp(&source[start..end]).ok()?;
            (node.find_str("uuid") == Some(uuid)).then(|| node.head().map(str::to_owned))
        })
        .flatten()
        .with_context(|| format!("schematic item {uuid} was not found"))?;
    match item.as_str() {
        "symbol" => rotate_symbol_source(source, uuid, 90.0),
        "bus_entry" => rotate_bus_entry_source(source, uuid),
        kind => anyhow::bail!("{kind} items do not support rotation yet"),
    }
}

/// Toggle a placed symbol's KiCad mirror axis (`x` or `y`).
pub(crate) fn mirror_symbol_source(source: &str, uuid: &str, axis: &str) -> Result<String> {
    if !matches!(axis, "x" | "y") {
        anyhow::bail!("mirror axis must be x or y");
    }
    let symbol_range = find_direct_child_blocks(source, "kicad_sch")
        .into_iter()
        .find(|(start, end)| {
            let block = &source[*start..*end];
            parse_sexp(block).ok().is_some_and(|node| {
                node.head() == Some("symbol") && node.find_str("uuid") == Some(uuid)
            })
        })
        .with_context(|| format!("placed symbol {uuid} was not found"))?;
    let block = &source[symbol_range.0..symbol_range.1];
    let children = find_direct_child_blocks(block, "symbol");
    let mirror = children.iter().find_map(|(start, end)| {
        let node = parse_sexp(&block[*start..*end]).ok()?;
        (node.head() == Some("mirror")).then_some((*start, *end, node))
    });
    let edit = if let Some((start, end, mirror)) = mirror {
        if mirror.get(1).and_then(SexpNode::as_str) == Some(axis) {
            SexpEdit::delete(symbol_range.0 + start, symbol_range.0 + end)
        } else {
            SexpEdit::replace(
                symbol_range.0 + start,
                symbol_range.0 + end,
                format!("(mirror {axis})"),
            )
        }
    } else {
        let (_, at_end) = children
            .iter()
            .find(|(start, end)| {
                parse_sexp(&block[*start..*end])
                    .ok()
                    .is_some_and(|node| node.head() == Some("at"))
            })
            .copied()
            .with_context(|| format!("placed symbol {uuid} has no position"))?;
        let indent = children
            .iter()
            .filter_map(|(start, _)| {
                let line_start = block[..*start].rfind('\n').map_or(0, |newline| newline + 1);
                let indent = block[line_start..*start]
                    .chars()
                    .take_while(|character| character.is_whitespace())
                    .collect::<String>();
                (!indent.is_empty()).then_some(indent)
            })
            .next()
            .unwrap_or_else(|| "  ".to_owned());
        SexpEdit::insert(
            symbol_range.0 + at_end,
            format!("\n{indent}(mirror {axis})"),
        )
    };
    Ok(konnect_sexp::apply_edits(source.to_owned(), vec![edit]))
}

/// Create a translated duplicate of one placed symbol with a fresh UUID and
/// an unannotated reference suitable for KiCad's next annotation pass.
pub(crate) fn duplicate_symbol_block(
    source: &str,
    uuid: &str,
    dx: f64,
    dy: f64,
) -> Result<(String, String)> {
    let moved = move_symbol_source(source, uuid, dx, dy)?;
    let range = find_direct_child_blocks(&moved, "kicad_sch")
        .into_iter()
        .find(|(start, end)| {
            let block = &moved[*start..*end];
            parse_sexp(block).ok().is_some_and(|node| {
                node.head() == Some("symbol") && node.find_str("uuid") == Some(uuid)
            })
        })
        .with_context(|| format!("placed symbol {uuid} was not found after translation"))?;
    let block = &moved[range.0..range.1];
    let node = parse_sexp(block)?;
    let reference = symbol_reference(&node).unwrap_or("U?");
    let prefix = reference
        .trim_end_matches(|character: char| character.is_ascii_digit() || character == '?');
    let duplicate_reference = format!("{}?", if prefix.is_empty() { "U" } else { prefix });
    let duplicate_uuid = konnect_sexp::writer::new_uuid();
    let mut edits = Vec::new();

    for (start, end) in find_direct_child_blocks(block, "symbol") {
        let child = parse_sexp(&block[start..end])?;
        if child.head() == Some("uuid") {
            edits.push(SexpEdit::replace(
                start,
                end,
                format!("(uuid \"{duplicate_uuid}\")"),
            ));
        } else if child.head() == Some("property")
            && child.get(1).and_then(SexpNode::as_str) == Some("Reference")
        {
            let property = &block[start..end];
            let strings = quoted_content_ranges(property);
            let value = strings
                .get(1)
                .context("Reference property has no quoted value")?;
            edits.push(SexpEdit::replace(
                start + value.0,
                start + value.1,
                duplicate_reference.clone(),
            ));
        }
    }
    for start in find_block_starts(block, "reference") {
        let Some((block_start, block_end)) = find_balanced_block(block, start) else {
            continue;
        };
        let reference_node = parse_sexp(&block[block_start..block_end])?;
        if reference_node.head() == Some("reference") {
            edits.push(SexpEdit::replace(
                block_start,
                block_end,
                format!("(reference \"{duplicate_reference}\")"),
            ));
        }
    }
    Ok((
        konnect_sexp::apply_edits(block.to_owned(), edits),
        duplicate_uuid,
    ))
}

fn quoted_content_ranges(source: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;
    let mut escaped = false;
    for (index, byte) in source.bytes().enumerate() {
        if let Some(content_start) = start {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                ranges.push((content_start, index));
                start = None;
            }
        } else if byte == b'"' {
            start = Some(index + 1);
        }
    }
    ranges
}

fn translated_at_edit(
    start: usize,
    end: usize,
    node: &SexpNode,
    dx: f64,
    dy: f64,
) -> Result<SexpEdit> {
    translated_point_edit(start, end, node, dx, dy)
}

fn translated_point_edit(
    start: usize,
    end: usize,
    node: &SexpNode,
    dx: f64,
    dy: f64,
) -> Result<SexpEdit> {
    let x = node
        .get_f64(1)
        .context("position is missing its x coordinate")?;
    let y = node
        .get_f64(2)
        .context("position is missing its y coordinate")?;
    let head = node.head().context("position has no item type")?;
    let mut replacement = format!("({head} {} {}", format_coord(x + dx), format_coord(y + dy));
    for value in node.children().unwrap_or_default().iter().skip(3) {
        let value = value
            .as_str()
            .context("position contains a non-scalar value")?;
        replacement.push(' ');
        replacement.push_str(value);
    }
    replacement.push(')');
    Ok(SexpEdit::replace(start, end, replacement))
}

fn format_coord(value: f64) -> String {
    let mut formatted = format!("{:.4}", if value.abs() < 0.000_05 { 0.0 } else { value });
    while formatted.contains('.') && formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.pop();
    }
    formatted
}

#[derive(Debug, Clone)]
pub(crate) struct HierarchyScene {
    pub(crate) name: String,
    pub(crate) depth: usize,
    pub(crate) file: PathBuf,
    pub(crate) scene: SchematicScene,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HierarchyEntry {
    pub(crate) name: String,
    pub(crate) depth: usize,
    pub(crate) file: PathBuf,
}

pub(crate) fn load_hierarchy(root: &Path) -> Result<Vec<HierarchyScene>> {
    discover_hierarchy(root)?
        .into_iter()
        .map(|entry| {
            let scene = SchematicScene::load(&entry.file)?;
            Ok(HierarchyScene {
                name: entry.name,
                depth: entry.depth,
                file: entry.file,
                scene,
            })
        })
        .collect()
}

pub(crate) fn discover_hierarchy(root: &Path) -> Result<Vec<HierarchyEntry>> {
    let mut entries = Vec::new();
    let mut ancestors = HashSet::new();
    let root_name = root
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("schematic")
        .to_owned();
    discover_hierarchy_inner(root, &root_name, 0, &mut ancestors, &mut entries)?;
    Ok(entries)
}

fn discover_hierarchy_inner(
    path: &Path,
    name: &str,
    depth: usize,
    ancestors: &mut HashSet<PathBuf>,
    output: &mut Vec<HierarchyEntry>,
) -> Result<()> {
    if depth > 20 {
        return Ok(());
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !ancestors.insert(canonical.clone()) {
        return Ok(());
    }

    output.push(HierarchyEntry {
        name: name.to_owned(),
        depth,
        file: path.to_path_buf(),
    });

    if let Ok(source) = read_consistent(path) {
        let schematic = parse_sexp(&source).ok();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        for sheet in schematic
            .as_ref()
            .map(|root| root.find_all("sheet"))
            .unwrap_or_default()
        {
            let Some(file) = sheet_property(sheet, "Sheetfile") else {
                continue;
            };
            let name = sheet_property(sheet, "Sheetname").unwrap_or(file);
            let child = parent.join(file);
            if child.is_file() {
                discover_hierarchy_inner(&child, name, depth + 1, ancestors, output)?;
            }
        }
    }
    ancestors.remove(&canonical);
    Ok(())
}

fn sheet_property<'a>(sheet: &'a SexpNode, property_name: &str) -> Option<&'a str> {
    sheet.find_all("property").into_iter().find_map(|property| {
        (property.get(1).and_then(SexpNode::as_str) == Some(property_name))
            .then(|| property.get(2).and_then(SexpNode::as_str))
            .flatten()
    })
}

struct SceneBuilder {
    file: PathBuf,
    source: Arc<str>,
    primitives: Vec<Primitive>,
    objects: Vec<SceneObject>,
}

#[derive(Debug, Clone, Copy)]
struct PinTextOptions {
    name_offset: f64,
    show_names: bool,
    show_numbers: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GraphicPass {
    Background,
    Foreground,
}

impl PinTextOptions {
    fn from_symbol(node: &SexpNode) -> Self {
        let names = node.find("pin_names");
        let numbers = node.find("pin_numbers");
        Self {
            name_offset: names
                .and_then(|item| child_f64(item, "offset"))
                .unwrap_or(0.508),
            show_names: names.is_none_or(|item| item.find("hide").is_none()),
            show_numbers: numbers.is_none_or(|item| item.find("hide").is_none()),
        }
    }
}

impl SceneBuilder {
    fn new(file: PathBuf, source: Arc<str>) -> Self {
        Self {
            file,
            source,
            primitives: Vec::new(),
            objects: Vec::new(),
        }
    }

    fn draw_page(&mut self, width: f64, height: f64, root: &SexpNode) {
        self.primitives.push(Primitive::Rect {
            bounds: Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: width,
                max_y: height,
            },
            style: StrokeStyle::new(0.0, ColorRole::Page),
            fill: true,
        });
        let outer = Bounds {
            min_x: 10.0,
            min_y: 10.0,
            max_x: width - 10.0,
            max_y: height - 10.0,
        };
        let inner = Bounds {
            min_x: 12.0,
            min_y: 12.0,
            max_x: width - 12.0,
            max_y: height - 12.0,
        };
        self.push_worksheet_outline(outer);
        self.push_worksheet_outline(inner);
        self.draw_default_worksheet(width, height, root);
    }

    fn push_worksheet_outline(&mut self, bounds: Bounds) {
        self.primitives.push(Primitive::Polyline {
            points: vec![
                Point::new(bounds.min_x, bounds.min_y),
                Point::new(bounds.max_x, bounds.min_y),
                Point::new(bounds.max_x, bounds.max_y),
                Point::new(bounds.min_x, bounds.max_y),
                Point::new(bounds.min_x, bounds.min_y),
            ],
            closed: false,
            style: StrokeStyle::new(0.1524, ColorRole::Border),
            fill: false,
        });
    }

    fn draw_default_worksheet(&mut self, width: f64, height: f64, root: &SexpNode) {
        let border = StrokeStyle::new(0.1524, ColorRole::Border);
        let right = width - 10.0;
        let bottom = height - 10.0;

        self.push_worksheet_outline(Bounds {
            min_x: right - 110.0,
            min_y: bottom - 34.0,
            max_x: right - 2.0,
            max_y: bottom - 2.0,
        });

        let mut x = 60.0;
        while x < width - 12.0 {
            for (from_y, to_y) in [(12.0, 10.0), (height - 12.0, height - 10.0)] {
                self.primitives.push(Primitive::Line {
                    from: Point::new(x, from_y),
                    to: Point::new(x, to_y),
                    style: border,
                });
            }
            x += 50.0;
        }
        for (index, x) in (0..).map(|index| (index, 35.0 + 50.0 * f64::from(index))) {
            if x >= width - 12.0 {
                break;
            }
            let label = (index + 1).to_string();
            self.push_worksheet_text(
                Point::new(x, 11.0),
                &label,
                1.3,
                0.1524,
                TextAlign::Left,
                false,
            );
            self.push_worksheet_text(
                Point::new(x, height - 11.0),
                &label,
                1.3,
                0.1524,
                TextAlign::Left,
                false,
            );
        }

        let mut y = 60.0;
        while y < height - 12.0 {
            for (from_x, to_x) in [(10.0, 12.0), (width - 10.0, width - 12.0)] {
                self.primitives.push(Primitive::Line {
                    from: Point::new(from_x, y),
                    to: Point::new(to_x, y),
                    style: border,
                });
            }
            y += 50.0;
        }

        for (index, y) in (0..).map(|index| (index, 35.0 + 50.0 * f64::from(index))) {
            if y >= height - 12.0 {
                break;
            }
            let label = char::from_u32(u32::from(b'A') + index as u32)
                .unwrap_or('?')
                .to_string();
            self.push_worksheet_text(
                Point::new(11.0, y),
                &label,
                1.3,
                0.1524,
                TextAlign::Center,
                false,
            );
            self.push_worksheet_text(
                Point::new(width - 11.0, y),
                &label,
                1.3,
                0.1524,
                TextAlign::Center,
                false,
            );
        }

        for (start_x, start_y, end_x, end_y) in [
            (110.0, 5.5, 2.0, 5.5),
            (110.0, 8.5, 2.0, 8.5),
            (110.0, 12.5, 2.0, 12.5),
            (110.0, 18.5, 2.0, 18.5),
            (90.0, 8.5, 90.0, 5.5),
            (26.0, 8.5, 26.0, 2.0),
        ] {
            self.primitives.push(Primitive::Line {
                from: Point::new(right - start_x, bottom - start_y),
                to: Point::new(right - end_x, bottom - end_y),
                style: border,
            });
        }

        let title = root.find("title_block");
        let title_value = |tag: &str| title.and_then(|block| child_text(block, tag)).unwrap_or("");
        let comment = |number: u32| {
            title
                .into_iter()
                .flat_map(|block| block.find_all("comment"))
                .find(|item| item.get_f64(1) == Some(f64::from(number)))
                .and_then(|item| item.get(2))
                .and_then(SexpNode::as_str)
                .unwrap_or("")
        };
        let filename = self
            .file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("schematic.kicad_sch");
        let paper = root
            .find("paper")
            .and_then(|node| node.get(1))
            .and_then(SexpNode::as_str)
            .unwrap_or("A4");
        let page_count = 1 + root.find_all("sheet").len();
        let entries = [
            (
                87.0,
                6.9,
                format!("Date: {}", title_value("date")),
                1.5,
                0.1524,
            ),
            (109.0, 4.1, "KiCad E.D.A. 10.0.5".to_owned(), 1.5, 0.1524),
            (24.0, 6.9, format!("Rev: {}", title_value("rev")), 1.5, 0.3),
            (109.0, 6.9, format!("Size: {paper}"), 1.5, 0.1524),
            (24.0, 4.1, format!("Id: 1/{page_count}"), 1.5, 0.1524),
            (
                109.0,
                10.7,
                format!("Title: {}", title_value("title")),
                2.0,
                0.4,
            ),
            (109.0, 14.3, format!("File: {filename}"), 1.5, 0.1524),
            (109.0, 17.0, "Sheet: /".to_owned(), 1.5, 0.1524),
            (109.0, 20.0, title_value("company").to_owned(), 1.5, 0.3),
            (109.0, 23.0, comment(1).to_owned(), 1.5, 0.1524),
            (109.0, 26.0, comment(2).to_owned(), 1.5, 0.1524),
            (109.0, 29.0, comment(3).to_owned(), 1.5, 0.1524),
            (109.0, 32.0, comment(4).to_owned(), 1.5, 0.1524),
        ];
        for (index, (x, y, text, size, width)) in entries.into_iter().enumerate() {
            self.push_worksheet_text(
                Point::new(right - x, bottom - y),
                &text,
                size,
                width,
                TextAlign::Left,
                index == 5,
            );
        }
    }

    fn push_worksheet_text(
        &mut self,
        position: Point,
        text: &str,
        size_mm: f64,
        stroke_width_mm: f64,
        align: TextAlign,
        italic: bool,
    ) {
        self.primitives.push(Primitive::Text {
            position,
            rotation_deg: 0.0,
            size_mm,
            stroke_width_mm,
            align,
            italic,
            role: ColorRole::Border,
            text: text.to_owned(),
        });
    }

    fn draw_document(&mut self, root: &SexpNode) {
        let libraries = library_symbols(root);
        let mut symbols = root.find_all("symbol");
        let (symbol_ranks, redraw_ranks) = self.kicad_symbol_ranks(root, &libraries);
        symbols.sort_by_key(|symbol| {
            child_text(symbol, "uuid")
                .and_then(|uuid| symbol_ranks.get(uuid).copied())
                .unwrap_or(usize::MAX)
        });
        if let Some(ranks) = cached_symbol_ranks(&self.file, &symbols, 0) {
            symbols.sort_by_key(|symbol| {
                symbol_reference(symbol)
                    .and_then(|reference| ranks.get(reference).copied())
                    .unwrap_or(usize::MAX)
            });
        }
        let mut redraw_symbols = root.find_all("symbol");
        redraw_symbols.sort_by_key(|symbol| {
            child_text(symbol, "uuid")
                .and_then(|uuid| redraw_ranks.get(uuid).copied())
                .unwrap_or(usize::MAX)
        });
        if let Some(ranks) = cached_symbol_ranks(&self.file, &redraw_symbols, 1) {
            redraw_symbols.sort_by_key(|symbol| {
                symbol_reference(symbol)
                    .and_then(|reference| ranks.get(reference).copied())
                    .unwrap_or(usize::MAX)
            });
        }
        let mut sheets = root.find_all("sheet");
        sheets.sort_by(|left, right| {
            let left = at_point(left).unwrap_or(Point::new(0.0, 0.0));
            let right = at_point(right).unwrap_or(Point::new(0.0, 0.0));
            right
                .x
                .total_cmp(&left.x)
                .then_with(|| left.y.total_cmp(&right.y))
        });

        // Sheets sort above symbols by KICAD_T. SCH_SHEET plots its outline in
        // the screen background pass and then plots the complete sheet in the
        // foreground pass.
        for sheet in &sheets {
            self.draw_sheet_outline(sheet);
        }
        for sheet in &sheets {
            self.draw_sheet(sheet);
        }
        // SCH_SYMBOL deliberately ignores the screen background pass. During
        // its foreground call it performs both library-local passes itself,
        // keeping each symbol's fill, outline, pins, and fields together.
        let symbol_object_start = self.objects.len();
        for symbol in &symbols {
            self.draw_symbol(symbol, &libraries);
        }
        let symbol_objects = &self.objects[symbol_object_start..];
        let overlapping_symbols = overlapping_object_uuids(symbol_objects);
        if std::env::var_os("KONNECT_SCENE_STATS").is_some() {
            let labels = symbol_objects
                .iter()
                .filter(|symbol| overlapping_symbols.contains(&symbol.uuid))
                .map(|symbol| symbol.label.as_str())
                .collect::<Vec<_>>();
            eprintln!("overlapping symbols: {}", labels.join(","));
        }

        // SCH_SCREEN::Plot sorts schematic items by descending KICAD_T and,
        // within one type, descending layer. Preserve that ordering for the
        // item classes supported by the native renderer.
        for label in root.find_all("hierarchical_label") {
            self.draw_label(label, true);
        }
        for label in root.find_all("global_label") {
            self.draw_label(label, true);
        }
        for label in root.find_all("label") {
            self.draw_label(label, false);
        }
        // Buses and wires are both SCH_LINE_T; LAYER_BUS sorts above
        // LAYER_WIRE.
        for bus in root.find_all("bus") {
            self.draw_wire(bus, ColorRole::Bus, ObjectKind::Bus);
        }
        for wire in root.find_all("wire") {
            self.draw_wire(wire, ColorRole::Wire, ObjectKind::Wire);
        }
        for entry in root.find_all("bus_entry") {
            self.draw_bus_entry(entry);
        }
        for no_connect in root.find_all("no_connect") {
            self.draw_no_connect(no_connect);
        }
        for text in root.find_all("text") {
            self.draw_top_level_text(text);
        }
        if std::env::var_os("KONNECT_SCENE_STATS").is_some() {
            let labels = redraw_symbols
                .iter()
                .filter(|symbol| {
                    child_text(symbol, "uuid")
                        .is_some_and(|uuid| overlapping_symbols.contains(uuid))
                })
                .filter_map(|symbol| symbol_reference(symbol))
                .collect::<Vec<_>>();
            eprintln!("redraw symbols: {}", labels.join(","));
        }
        for symbol in redraw_symbols {
            if child_text(symbol, "uuid").is_some_and(|uuid| overlapping_symbols.contains(uuid)) {
                self.redraw_symbol_fields_and_pins(symbol, &libraries);
            }
        }
        for junction in root.find_all("junction") {
            self.draw_junction(junction);
        }
    }

    fn kicad_symbol_ranks(
        &self,
        root: &SexpNode,
        libraries: &HashMap<String, &SexpNode>,
    ) -> (HashMap<String, usize>, HashMap<String, usize>) {
        // Build item bounds independently of plot order, then insert them in
        // their original file order just as SCH_SCREEN does while parsing.
        let mut index = Self::new(self.file.clone(), Arc::clone(&self.source));
        for sheet in root.find_all("sheet") {
            index.draw_sheet(sheet);
        }
        for symbol in root.find_all("symbol") {
            index.draw_symbol(symbol, libraries);
        }
        for label in root.find_all("hierarchical_label") {
            index.draw_label(label, true);
        }
        for label in root.find_all("global_label") {
            index.draw_label(label, true);
        }
        for label in root.find_all("label") {
            index.draw_label(label, false);
        }
        for bus in root.find_all("bus") {
            index.draw_wire(bus, ColorRole::Bus, ObjectKind::Bus);
        }
        for wire in root.find_all("wire") {
            index.draw_wire(wire, ColorRole::Wire, ObjectKind::Wire);
        }
        for entry in root.find_all("bus_entry") {
            index.draw_bus_entry(entry);
        }
        for no_connect in root.find_all("no_connect") {
            index.draw_no_connect(no_connect);
        }
        for text in root.find_all("text") {
            index.draw_top_level_text(text);
        }
        for junction in root.find_all("junction") {
            index.draw_junction(junction);
        }

        index.objects.sort_by_key(|object| {
            let needle = format!("(uuid \"{}\")", object.uuid);
            self.source.find(&needle).unwrap_or(usize::MAX)
        });
        let indexed_objects = index.objects.iter().collect::<Vec<_>>();
        let order = traversal_order_with_refresh(
            indexed_objects.iter().map(|object| {
                (
                    object.item_type,
                    object.initial_index_bounds,
                    object.index_bounds,
                )
            }),
            5,
        );
        let mut redraw_ranks = HashMap::new();
        for (rank, object_index) in order.iter().copied().enumerate() {
            let object = indexed_objects[object_index];
            if object.kind == ObjectKind::Symbol {
                redraw_ranks.insert(object.uuid.clone(), rank);
            }
        }
        // SCH_SCREEN removes junctions (and bitmaps) before sorting `other`.
        // Keeping them in the input changes libstdc++'s observable unstable
        // permutation even though their type sorts below symbols.
        let mut plot_order = order
            .into_iter()
            .filter(|index| indexed_objects[*index].kind != ObjectKind::Junction)
            .collect::<Vec<_>>();
        glibcxx_sort_by(&mut plot_order, |left, right| {
            indexed_objects[left].item_type > indexed_objects[right].item_type
        });
        let mut plot_ranks = HashMap::new();
        for (rank, object_index) in plot_order.into_iter().enumerate() {
            let object = indexed_objects[object_index];
            if object.kind == ObjectKind::Symbol {
                plot_ranks.insert(object.uuid.clone(), rank);
            }
        }
        (plot_ranks, redraw_ranks)
    }

    fn draw_wire(&mut self, node: &SexpNode, role: ColorRole, kind: ObjectKind) {
        let points = points_from_pts(node);
        if points.len() < 2 {
            return;
        }
        let start = self.primitives.len();
        for pair in points.windows(2) {
            self.primitives.push(Primitive::Line {
                from: pair[0],
                to: pair[1],
                style: StrokeStyle::new(stroke_width(node, 0.1524), role),
            });
        }
        self.push_object(node, kind, role_name(role), start);
    }

    fn draw_bus_entry(&mut self, node: &SexpNode) {
        let Some(at) = at_point(node) else {
            return;
        };
        let Some(size) = node.find("size") else {
            return;
        };
        let end = Point::new(
            at.x + size.get_f64(1).unwrap_or_default(),
            at.y + size.get_f64(2).unwrap_or_default(),
        );
        let start = self.primitives.len();
        self.primitives.push(Primitive::Line {
            from: at,
            to: end,
            style: StrokeStyle::new(stroke_width(node, 0.1524), ColorRole::Bus),
        });
        self.push_object(node, ObjectKind::BusEntry, "bus entry", start);
    }

    fn draw_junction(&mut self, node: &SexpNode) {
        let Some(center) = at_point(node) else {
            return;
        };
        let diameter = child_f64(node, "diameter").unwrap_or(0.9);
        let start = self.primitives.len();
        self.primitives.push(Primitive::Circle {
            center,
            radius: if diameter > 0.0 {
                diameter / 2.0
            } else {
                0.4572
            },
            // KiCad exports junctions as fill-only circles.  Adding even a
            // hairline outline changes their diameter and antialias coverage.
            style: StrokeStyle::new(0.0, ColorRole::Junction),
            fill: true,
        });
        self.push_object(node, ObjectKind::Junction, "junction", start);
    }

    fn draw_no_connect(&mut self, node: &SexpNode) {
        let Some(center) = at_point(node) else {
            return;
        };
        let start = self.primitives.len();
        let radius = 0.6096;
        for (a, b) in [
            (
                Point::new(center.x - radius, center.y - radius),
                Point::new(center.x + radius, center.y + radius),
            ),
            (
                Point::new(center.x - radius, center.y + radius),
                Point::new(center.x + radius, center.y - radius),
            ),
        ] {
            self.primitives.push(Primitive::Line {
                from: a,
                to: b,
                style: StrokeStyle::new(0.1524, ColorRole::NoConnect),
            });
        }
        self.push_object(node, ObjectKind::NoConnect, "no connect", start);
    }

    fn draw_top_level_text(&mut self, node: &SexpNode) {
        if is_hidden(node) {
            return;
        }
        let Some(position) = at_point(node) else {
            return;
        };
        let text = node.get(1).and_then(SexpNode::as_str).unwrap_or("");
        let rotation = readable_text_rotation(at_rotation(node));
        let start = self.primitives.len();
        self.primitives.push(Primitive::Text {
            position: rotate_about(Point::new(0.0, -0.25), position, rotation),
            rotation_deg: rotation,
            size_mm: text_size(node, 1.27),
            stroke_width_mm: text_stroke_width(node, 1.27),
            align: text_align(node),
            italic: text_italic(node),
            role: ColorRole::GraphicText,
            text: text.to_owned(),
        });
        self.push_object(node, ObjectKind::Text, text, start);
    }

    fn draw_label(&mut self, node: &SexpNode, framed: bool) {
        if is_hidden(node) {
            return;
        }
        let Some(position) = at_point(node) else {
            return;
        };
        let text = node.get(1).and_then(SexpNode::as_str).unwrap_or("");
        let size = text_size(node, 1.27);
        let rotation = readable_text_rotation(at_rotation(node)).rem_euclid(360.0);
        let direction = Point::new(rotation.to_radians().cos(), -rotation.to_radians().sin());
        let normal = Point::new(-direction.y, direction.x);
        let start = self.primitives.len();
        if framed {
            // KiCad sizes global-label frames from the rendered Newstroke text,
            // not from a fixed average character width. The surrounding frame
            // dimensions scale with the text height.
            let quantum = 0.0001;
            let size_iu = (size / quantum).round() as i64;
            let margin_iu = ((size_iu as f64) * 0.375).round() as i64;
            let shoulder_iu = size_iu / 2 + margin_iu;
            let raw_pen_iu = ((size_iu as f64) / 8.0).round() as i64;
            let half_height_iu = shoulder_iu + raw_pen_iu + 3;
            let shoulder = shoulder_iu as f64 * quantum;
            let half_height = half_height_iu as f64 * quantum;
            let text_boundary_iu = (boundary_width(
                text,
                size,
                raw_pen_iu as f64 * quantum,
                quantum,
            ) / quantum)
                .round() as i64;
            // SCH_GLOBALLABEL::CreateGraphicShape constructs the full extent
            // in integer schematic units. Preserve every term: text box,
            // two margins, outline pen, a 3-IU guard, and the input shoulder.
            let width_iu = text_boundary_iu + 2 * margin_iu + raw_pen_iu + 3 + shoulder_iu;
            let width = width_iu as f64 * quantum;
            let local = [
                Point::new(0.0, 0.0),
                Point::new(shoulder, half_height),
                Point::new(width, half_height),
                Point::new(width, 0.0),
                Point::new(width, -half_height),
                Point::new(shoulder, -half_height),
            ];
            self.primitives.push(Primitive::Polyline {
                points: local
                    .into_iter()
                    .map(|point| rotate_about(point, position, rotation))
                    .collect(),
                closed: true,
                style: StrokeStyle::new(0.1524, ColorRole::Symbol),
                fill: false,
            });
        }
        let text_stroke = text_stroke_width(node, size);
        let text_position = if framed {
            let quantum = 0.0001;
            let size_iu = (size / quantum).round() as i64;
            let margin_iu = (size_iu as f64 * 0.375).round() as i64;
            let triangle_iu = match child_text(node, "shape") {
                Some("output" | "passive") => 0,
                _ => size_iu * 3 / 4,
            };
            let offset = (margin_iu + triangle_iu) as f64 * quantum;
            let vertical_center = (size_iu as f64 * 0.0715).trunc() * quantum;
            Point::new(
                position.x + direction.x * offset + normal.x * vertical_center,
                position.y + direction.y * offset + normal.y * vertical_center,
            )
        } else {
            // SCH_LABEL_BASE first offsets the bottom-justified anchor by the
            // configured 0.15 text-height ratio plus the effective pen width.
            // FONT::getLinePositions then places a single bottom-justified
            // line 0.585 text heights above the center-justified origin used
            // by this scene. Keep those two KiCad transforms explicit.
            let connection_offset = local_label_text_offset(node, size);
            rotate_about(
                Point::new(0.0, -connection_offset),
                position,
                readable_text_rotation(rotation),
            )
        };
        self.primitives.push(Primitive::Text {
            position: text_position,
            rotation_deg: readable_text_rotation(rotation),
            size_mm: size,
            stroke_width_mm: text_stroke,
            align: TextAlign::Left,
            italic: text_italic(node),
            role: if framed {
                ColorRole::Symbol
            } else {
                ColorRole::Label
            },
            text: text.to_owned(),
        });
        self.push_object(node, ObjectKind::Label, text, start);
    }

    fn draw_sheet(&mut self, node: &SexpNode) {
        let start = self.primitives.len();
        self.draw_sheet_outline(node);
        if at_point(node).is_none() {
            return;
        }
        let mut label = "sheet".to_owned();
        for property in node.find_all("property") {
            if is_hidden(property) {
                continue;
            }
            let name = property.get(1).and_then(SexpNode::as_str).unwrap_or("");
            let value = property.get(2).and_then(SexpNode::as_str).unwrap_or("");
            if name == "Sheetname" {
                label.clone_from(&value.to_owned());
            }
            let position = at_point(property).or_else(|| {
                // KiCad 10 plots unpositioned visible sheet fields at the
                // origin rather than suppressing them.
                matches!(name, "Sheetname" | "Sheetfile").then(|| Point::new(0.0, 0.0))
            });
            if let Some(position) = position {
                let shown_value = if name == "Sheetfile" {
                    format!("File: {value}")
                } else {
                    value.to_owned()
                };
                self.primitives.push(Primitive::Text {
                    position,
                    rotation_deg: readable_text_rotation(at_rotation(property)),
                    size_mm: text_size(property, 1.27),
                    stroke_width_mm: text_stroke_width(property, 1.27),
                    align: text_align(property),
                    italic: text_italic(property),
                    role: if name == "Sheetfile" {
                        ColorRole::SheetFile
                    } else {
                        ColorRole::PinName
                    },
                    text: shown_value,
                });
            }
        }
        for pin in node.find_all("pin") {
            self.draw_sheet_pin(pin);
        }
        self.push_object(node, ObjectKind::Sheet, &label, start);
    }

    fn draw_sheet_outline(&mut self, node: &SexpNode) {
        let Some(at) = at_point(node) else {
            return;
        };
        let Some(size) = node.find("size") else {
            return;
        };
        let width = size.get_f64(1).unwrap_or_default();
        let height = size.get_f64(2).unwrap_or_default();
        self.primitives.push(Primitive::Rect {
            bounds: Bounds {
                min_x: at.x,
                min_y: at.y,
                max_x: at.x + width,
                max_y: at.y + height,
            },
            style: StrokeStyle::new(stroke_width(node, 0.1524), ColorRole::Symbol),
            fill: false,
        });
    }

    fn draw_sheet_pin(&mut self, node: &SexpNode) {
        let Some(at) = at_point(node) else {
            return;
        };
        let rotation = at_rotation(node).to_radians();
        let end = Point::new(at.x + rotation.cos() * 2.0, at.y - rotation.sin() * 2.0);
        self.primitives.push(Primitive::Line {
            from: at,
            to: end,
            style: StrokeStyle::new(0.254, ColorRole::Pin),
        });
        let text = node.get(1).and_then(SexpNode::as_str).unwrap_or("");
        self.primitives.push(Primitive::Text {
            position: end,
            rotation_deg: readable_text_rotation(at_rotation(node)),
            size_mm: text_size(node, 1.27),
            stroke_width_mm: text_stroke_width(node, 1.27),
            align: TextAlign::Left,
            italic: text_italic(node),
            role: ColorRole::Pin,
            text: text.to_owned(),
        });
    }

    fn draw_symbol(&mut self, node: &SexpNode, libraries: &HashMap<String, &SexpNode>) {
        let Some(lib_id) = node.find_str("lib_id") else {
            return;
        };
        let Some(origin) = at_point(node) else {
            return;
        };
        let transform = symbol_transform(node, origin);
        let unit = child_u32(node, "unit").unwrap_or(1);
        let start = self.primitives.len();

        if let Some(definition) = libraries.get(lib_id) {
            self.draw_library_definition(definition, libraries, unit, transform);
        } else {
            self.draw_missing_symbol(origin);
        }
        let mut reference = lib_id.to_owned();
        for property in node.find_all("property") {
            let name = property.get(1).and_then(SexpNode::as_str).unwrap_or("");
            let value = property.get(2).and_then(SexpNode::as_str).unwrap_or("");
            if name == "Reference" && !value.is_empty() {
                reference = value.to_owned();
            }
            if is_hidden(property) {
                continue;
            }
            let Some(position) = symbol_field_plot_position(property) else {
                continue;
            };
            self.primitives.push(Primitive::Text {
                position,
                rotation_deg: readable_text_rotation(at_rotation(property)),
                size_mm: text_size(property, 1.27),
                stroke_width_mm: text_stroke_width(property, 1.27),
                align: text_align(property),
                italic: text_italic(property),
                role: ColorRole::Text,
                text: value.to_owned(),
            });
        }
        self.push_object(node, ObjectKind::Symbol, &reference, start);
        if let Some(object) = self.objects.last_mut() {
            object.initial_index_bounds =
                symbol_initial_index_bounds(node).unwrap_or(object.index_bounds);
        }
    }

    fn draw_missing_symbol(&mut self, origin: Point) {
        self.primitives.push(Primitive::Rect {
            bounds: Bounds {
                min_x: origin.x - 3.0,
                min_y: origin.y - 3.0,
                max_x: origin.x + 3.0,
                max_y: origin.y + 3.0,
            },
            style: StrokeStyle::new(0.1524, ColorRole::Symbol),
            fill: false,
        });
    }

    fn redraw_symbol_fields_and_pins(
        &mut self,
        node: &SexpNode,
        libraries: &HashMap<String, &SexpNode>,
    ) {
        for property in node.find_all("property") {
            let value = property.get(2).and_then(SexpNode::as_str).unwrap_or("");
            if is_hidden(property) {
                continue;
            }
            let Some(position) = symbol_field_plot_position(property) else {
                continue;
            };
            self.primitives.push(Primitive::Text {
                position,
                rotation_deg: readable_text_rotation(at_rotation(property)),
                size_mm: text_size(property, 1.27),
                stroke_width_mm: text_stroke_width(property, 1.27),
                align: text_align(property),
                italic: text_italic(property),
                role: ColorRole::Text,
                text: value.to_owned(),
            });
        }

        let (Some(lib_id), Some(origin)) = (node.find_str("lib_id"), at_point(node)) else {
            return;
        };
        let Some(definition) = libraries.get(lib_id) else {
            return;
        };
        let transform = symbol_transform(node, origin);
        let unit = child_u32(node, "unit").unwrap_or(1);
        let mut nodes = Vec::new();
        Self::collect_library_nodes(definition, libraries, unit, &mut nodes);
        for (library_node, pin_text) in nodes {
            for pin in library_node.find_all("pin") {
                self.draw_library_pin(pin, transform, pin_text);
            }
        }
    }

    fn draw_library_definition(
        &mut self,
        definition: &SexpNode,
        libraries: &HashMap<String, &SexpNode>,
        unit: u32,
        transform: PinTransform,
    ) {
        let mut nodes = Vec::new();
        Self::collect_library_nodes(definition, libraries, unit, &mut nodes);

        for pass in [GraphicPass::Background, GraphicPass::Foreground] {
            for (node, _) in &nodes {
                self.draw_library_graphics(node, transform, pass);
            }
        }
        for (node, pin_text) in nodes {
            self.draw_library_overlays(node, transform, pin_text);
        }
    }

    fn collect_library_nodes<'a>(
        definition: &'a SexpNode,
        libraries: &HashMap<String, &'a SexpNode>,
        unit: u32,
        nodes: &mut Vec<(&'a SexpNode, PinTextOptions)>,
    ) {
        if let Some(parent_name) = definition.find_str("extends") {
            if let Some(parent) = libraries.get(parent_name) {
                Self::collect_library_nodes(parent, libraries, unit, nodes);
            }
        }
        let pin_text = PinTextOptions::from_symbol(definition);
        nodes.push((definition, pin_text));
        for child in definition.find_all("symbol") {
            let name = child.get(1).and_then(SexpNode::as_str).unwrap_or("");
            if symbol_unit_matches(name, unit) {
                nodes.push((child, pin_text));
            }
        }
    }

    fn draw_library_graphics(
        &mut self,
        node: &SexpNode,
        transform: PinTransform,
        pass: GraphicPass,
    ) {
        // LIB_SYMBOL retains the library drawing order for shapes of the same
        // KiCad type. Do not regroup by geometry kind: overlapping graphics
        // are sensitive to their original paint order.
        for drawing in node.children().unwrap_or(&[]) {
            match drawing.head() {
                Some("rectangle") => self.draw_library_rectangle(drawing, transform, pass),
                Some("circle") => self.draw_library_circle(drawing, transform, pass),
                Some("polyline") => self.draw_library_polyline(drawing, transform, pass),
                Some("arc") if pass == GraphicPass::Foreground => {
                    self.draw_library_arc(drawing, transform);
                }
                Some("bezier") if pass == GraphicPass::Foreground => {
                    self.draw_library_bezier(drawing, transform);
                }
                _ => {}
            }
        }
    }

    fn draw_library_overlays(
        &mut self,
        node: &SexpNode,
        transform: PinTransform,
        pin_text: PinTextOptions,
    ) {
        for text in node.find_all("text") {
            self.draw_library_text(text, transform);
        }
        for pin in node.find_all("pin") {
            self.draw_library_pin(pin, transform, pin_text);
        }
    }

    fn draw_library_rectangle(
        &mut self,
        node: &SexpNode,
        transform: PinTransform,
        pass: GraphicPass,
    ) {
        let (Some(start), Some(end)) = (tag_point(node, "start"), tag_point(node, "end")) else {
            return;
        };
        let points = [
            transform_point(start, transform),
            transform_point(Point::new(end.x, start.y), transform),
            transform_point(end, transform),
            transform_point(Point::new(start.x, end.y), transform),
        ];
        let fill = graphic_fill_in_pass(node, pass);
        if pass == GraphicPass::Background && !fill {
            return;
        }
        let min_x = points
            .iter()
            .map(|point| point.x)
            .fold(f64::INFINITY, f64::min);
        let min_y = points
            .iter()
            .map(|point| point.y)
            .fold(f64::INFINITY, f64::min);
        let max_x = points
            .iter()
            .map(|point| point.x)
            .fold(f64::NEG_INFINITY, f64::max);
        let max_y = points
            .iter()
            .map(|point| point.y)
            .fold(f64::NEG_INFINITY, f64::max);
        self.primitives.push(Primitive::Rect {
            bounds: Bounds {
                min_x,
                min_y,
                max_x,
                max_y,
            },
            style: graphic_stroke(node, pass),
            fill,
        });
    }

    fn draw_library_circle(&mut self, node: &SexpNode, transform: PinTransform, pass: GraphicPass) {
        let Some(center) = tag_point(node, "center") else {
            return;
        };
        let radius = child_f64(node, "radius").unwrap_or_default();
        let fill = graphic_fill_in_pass(node, pass);
        if pass == GraphicPass::Background && !fill {
            return;
        }
        self.primitives.push(Primitive::Circle {
            center: transform_point(center, transform),
            radius,
            style: graphic_stroke(node, pass),
            fill,
        });
    }

    fn draw_library_polyline(
        &mut self,
        node: &SexpNode,
        transform: PinTransform,
        pass: GraphicPass,
    ) {
        let points = points_from_pts(node)
            .into_iter()
            .map(|point| transform_point(point, transform))
            .collect::<Vec<_>>();
        if points.len() < 2 {
            return;
        }
        let fill = graphic_fill_in_pass(node, pass);
        if pass == GraphicPass::Background && !fill {
            return;
        }
        self.primitives.push(Primitive::Polyline {
            closed: points.first() == points.last() || fill,
            points,
            style: graphic_stroke(node, pass),
            fill,
        });
    }

    fn draw_library_arc(&mut self, node: &SexpNode, transform: PinTransform) {
        let (Some(start), Some(mid), Some(end)) = (
            tag_point(node, "start"),
            tag_point(node, "mid"),
            tag_point(node, "end"),
        ) else {
            return;
        };
        // SCH_SHAPE normalizes an arc while it is still in library-local
        // integer coordinates.  Only the resulting plot points are transformed
        // into sheet space.  Doing this after the symbol transform changes the
        // uncertainty-aware center snapping used by KiCad.
        let (start, mid, end) = normalize_kicad_arc(start, mid, end);
        self.primitives.push(Primitive::Arc {
            start: transform_point(start, transform),
            mid: transform_point(mid, transform),
            end: transform_point(end, transform),
            style: StrokeStyle::new(stroke_width(node, 0.1524), ColorRole::Symbol),
        });
    }

    fn draw_library_bezier(&mut self, node: &SexpNode, transform: PinTransform) {
        let points = points_from_pts(node)
            .into_iter()
            .map(|point| transform_point(point, transform))
            .collect::<Vec<_>>();
        if points.len() >= 4 {
            self.primitives.push(Primitive::Bezier {
                points,
                style: StrokeStyle::new(stroke_width(node, 0.1524), ColorRole::Symbol),
            });
        }
    }

    fn draw_library_text(&mut self, node: &SexpNode, transform: PinTransform) {
        if is_hidden(node) {
            return;
        }
        let Some(local) = at_point(node) else {
            return;
        };
        let text = node.get(1).and_then(SexpNode::as_str).unwrap_or("");
        self.primitives.push(Primitive::Text {
            position: transform_point(local, transform),
            rotation_deg: readable_text_rotation(transform.rotation_deg + at_rotation(node)),
            size_mm: text_size(node, 1.27),
            stroke_width_mm: text_stroke_width(node, 1.27),
            align: text_align(node),
            italic: text_italic(node),
            role: ColorRole::Text,
            text: text.to_owned(),
        });
    }

    fn draw_library_pin(
        &mut self,
        node: &SexpNode,
        transform: PinTransform,
        options: PinTextOptions,
    ) {
        if node.find("hide").is_some() {
            return;
        }
        let Some(tip_local) = at_point(node) else {
            return;
        };
        let angle = at_rotation(node).to_radians();
        let length = child_f64(node, "length").unwrap_or(2.54);
        let body_local = Point::new(
            tip_local.x + length * angle.cos(),
            tip_local.y + length * angle.sin(),
        );
        let tip = transform_point(tip_local, transform);
        let body = transform_point(body_local, transform);
        self.primitives.push(Primitive::Line {
            from: tip,
            to: body,
            style: StrokeStyle::new(0.1524, ColorRole::Symbol),
        });

        if options.show_names {
            let name = child_text(node, "name").unwrap_or("");
            if name != "~" && !name.is_empty() {
                let inward = unit_direction(body, tip);
                let name_local = Point::new(
                    body_local.x + options.name_offset * angle.cos(),
                    body_local.y + options.name_offset * angle.sin(),
                );
                let text_rotation =
                    readable_text_rotation(transform.rotation_deg + at_rotation(node));
                let text_axis = Point::new(
                    text_rotation.to_radians().cos(),
                    -text_rotation.to_radians().sin(),
                );
                self.primitives.push(Primitive::Text {
                    position: transform_point(name_local, transform),
                    rotation_deg: text_rotation,
                    size_mm: text_size(node.find("name").unwrap_or(node), 1.0),
                    stroke_width_mm: text_stroke_width(node.find("name").unwrap_or(node), 1.0),
                    align: if inward.x * text_axis.x + inward.y * text_axis.y < 0.0 {
                        TextAlign::Right
                    } else {
                        TextAlign::Left
                    },
                    italic: text_italic(node.find("name").unwrap_or(node)),
                    role: ColorRole::PinName,
                    text: name.to_owned(),
                });
            }
        }
        if options.show_numbers {
            let number = child_text(node, "number").unwrap_or("");
            if !number.is_empty() {
                let number_node = node.find("number").unwrap_or(node);
                let number_size = text_size(number_node, 0.9);
                // KiCad 10.0.5 combines its rounded 4 mil text-layout
                // clearance, 4 mil pin margin, and 6 mil default plot pen,
                // then applies the 0.585-height bottom-to-center adjustment.
                let number_offset = pin_number_text_offset(number_size);
                let midpoint = Point::new((tip.x + body.x) / 2.0, (tip.y + body.y) / 2.0);
                let text_rotation =
                    readable_text_rotation(transform.rotation_deg + at_rotation(node));
                self.primitives.push(Primitive::Text {
                    position: rotate_about(
                        Point::new(0.0, -number_offset),
                        midpoint,
                        text_rotation,
                    ),
                    rotation_deg: text_rotation,
                    size_mm: number_size,
                    stroke_width_mm: text_stroke_width(number_node, 0.9),
                    align: TextAlign::Center,
                    italic: text_italic(number_node),
                    role: ColorRole::PinNumber,
                    text: number.to_owned(),
                });
            }
        }
    }

    fn push_object(&mut self, node: &SexpNode, kind: ObjectKind, label: &str, start: usize) {
        let Some(uuid) = child_text(node, "uuid") else {
            return;
        };
        if uuid.is_empty() {
            return;
        }
        let end = self.primitives.len();
        let mut bounds = self.primitives[start..end].iter().filter_map(|primitive| {
            // LIB_SYMBOL::GetBodyBoundingBox includes pin geometry but calls
            // SCH_PIN::GetBoundingBox(false, false, false), explicitly
            // excluding pin-name and pin-number text from the symbol's R-tree
            // box. Selection still uses the complete primitive range.
            if kind == ObjectKind::Symbol
                && matches!(
                    primitive,
                    Primitive::Text {
                        role: ColorRole::PinName | ColorRole::PinNumber,
                        ..
                    }
                )
            {
                None
            } else {
                primitive.bounds()
            }
        });
        let Some(mut combined) = bounds.next() else {
            return;
        };
        for item in bounds {
            combined.include_bounds(item);
        }
        let index_bounds = match kind {
            ObjectKind::Symbol => {
                symbol_index_bounds(node, &self.primitives[start..end]).unwrap_or(combined)
            }
            ObjectKind::Bus | ObjectKind::Wire | ObjectKind::BusEntry => {
                let mut bounds = combined;
                bounds.inflate(stroke_width(node, 0.2286).max(0.2286));
                // BOX2I's inclusive right/bottom edge gains one IU when an
                // integer wire box is inflated.
                bounds.max_x += 0.0001;
                bounds.max_y += 0.0001;
                bounds
            }
            ObjectKind::NoConnect => {
                let mut bounds = combined;
                bounds.inflate(0.2286);
                bounds
            }
            ObjectKind::Label if node.head() == Some("global_label") => {
                let mut bounds = combined;
                let size = text_size(node, 1.27);
                let pen = ((text_anchor_pen_width(node, size) / 0.0001).round() * 0.0001) * 1.5;
                bounds.inflate((pen / 0.0001).round() * 0.0001);
                bounds
            }
            ObjectKind::Label if node.head() == Some("label") => {
                local_label_index_bounds(node).unwrap_or(combined)
            }
            ObjectKind::Text => {
                let mut bounds =
                    kicad_text_box_for(node, node.get(1).and_then(SexpNode::as_str).unwrap_or(""))
                        .unwrap_or(combined);
                bounds.inflate(text_anchor_pen_width(node, text_size(node, 1.27)));
                bounds
            }
            _ => combined,
        };
        self.objects.push(SceneObject {
            uuid: uuid.to_owned(),
            kind,
            item_type: kicad_item_type(node, kind),
            label: label.to_owned(),
            search_text: object_search_text(node, label, uuid),
            properties: object_properties(node),
            bounds: combined,
            index_bounds,
            initial_index_bounds: index_bounds,
            primitive_range: start..end,
        });
    }
}

fn object_search_text(node: &SexpNode, label: &str, uuid: &str) -> String {
    let mut fields = vec![label.to_owned(), uuid.to_owned()];
    if let Some(value) = node.get(1).and_then(SexpNode::as_str) {
        fields.push(value.to_owned());
    }
    if let Some(lib_id) = node.find_str("lib_id") {
        fields.push(lib_id.to_owned());
    }
    for property in node.find_all("property") {
        if let Some(name) = property.get(1).and_then(SexpNode::as_str) {
            fields.push(name.to_owned());
        }
        if let Some(value) = property.get(2).and_then(SexpNode::as_str) {
            fields.push(value.to_owned());
        }
    }
    fields.join("\n")
}

fn object_properties(node: &SexpNode) -> Vec<ObjectProperty> {
    let mut properties = Vec::new();
    if let Some(lib_id) = node.find_str("lib_id") {
        properties.push(ObjectProperty {
            name: "Library".to_owned(),
            value: lib_id.to_owned(),
        });
    }
    if matches!(
        node.head(),
        Some("label" | "global_label" | "hierarchical_label" | "text")
    ) {
        if let Some(value) = node.get(1).and_then(SexpNode::as_str) {
            properties.push(ObjectProperty {
                name: "Text".to_owned(),
                value: value.to_owned(),
            });
        }
    }
    if let Some(at) = node.find("at") {
        let x = at.get(1).and_then(SexpNode::as_str).unwrap_or("?");
        let y = at.get(2).and_then(SexpNode::as_str).unwrap_or("?");
        let rotation = at.get(3).and_then(SexpNode::as_str).unwrap_or("0");
        properties.push(ObjectProperty {
            name: "Position".to_owned(),
            value: format!("{x}, {y} · {rotation}°"),
        });
    }
    properties.extend(
        node.find_all("property")
            .into_iter()
            .filter_map(|property| {
                Some(ObjectProperty {
                    name: property.get(1)?.as_str()?.to_owned(),
                    value: property.get(2)?.as_str()?.to_owned(),
                })
            }),
    );
    properties
}

fn symbol_initial_index_bounds(node: &SexpNode) -> Option<Bounds> {
    let origin = at_point(node)?;
    let mut combined = Bounds {
        min_x: origin.x - 5.08,
        min_y: origin.y - 5.08,
        max_x: origin.x + 5.08,
        max_y: origin.y + 5.08,
    };
    for property in node.find_all("property") {
        if is_hidden(property) {
            continue;
        }
        if let Some(bounds) = initial_symbol_field_bounds(property) {
            combined.include_bounds(bounds);
        }
    }
    Some(combined)
}

fn initial_symbol_field_bounds(node: &SexpNode) -> Option<Bounds> {
    const QUANTUM: f64 = 0.0001;
    let position = at_point(node)?;
    let text = node.get(2).and_then(SexpNode::as_str).unwrap_or("");
    let size = text_size(node, 1.27);
    let pen_iu = (text_anchor_pen_width(node, size) / QUANTUM).round() as i64;
    let width = boundary_width(text, size, pen_iu as f64 * QUANTUM, QUANTUM);
    let left = match text_align(node) {
        TextAlign::Left => 0.0,
        TextAlign::Center => -width / 2.0,
        TextAlign::Right => -width,
    };
    let rotation = readable_text_rotation(at_rotation(node));
    Bounds::from_points(
        [
            Point::new(left, -size),
            Point::new(left + width, -size),
            Point::new(left + width, size),
            Point::new(left, size),
        ]
        .into_iter()
        .map(|corner| rotate_about(corner, position, rotation)),
    )
}

fn kicad_item_type(node: &SexpNode, kind: ObjectKind) -> i32 {
    match node.head() {
        Some("text") => 53,
        Some("junction") => 57,
        Some("no_connect") => 58,
        Some("wire" | "bus" | "bus_entry") => 61,
        Some("label" | "hierarchical_label") => 65,
        Some("global_label") => 66,
        Some("symbol") => 70,
        Some("sheet") => 73,
        _ => match kind {
            ObjectKind::Text => 53,
            ObjectKind::Junction => 57,
            ObjectKind::NoConnect => 58,
            ObjectKind::Bus | ObjectKind::BusEntry | ObjectKind::Wire => 61,
            ObjectKind::Label => 65,
            ObjectKind::Symbol => 70,
            ObjectKind::Sheet => 73,
        },
    }
}

fn symbol_index_bounds(node: &SexpNode, primitives: &[Primitive]) -> Option<Bounds> {
    let mut bounds = primitives.iter().filter_map(|primitive| match primitive {
        // Symbol fields are merged below using EDA_TEXT::GetTextBox's integer
        // rules. Pin annotations are explicitly excluded by KiCad's
        // LIB_SYMBOL::GetBodyBoundingBox(..., includeNameAndNumber=false).
        Primitive::Text {
            role: ColorRole::Text | ColorRole::PinName | ColorRole::PinNumber,
            ..
        } => None,
        Primitive::Line { from, to, style }
            if style.role == ColorRole::Pin
                || (style.role == ColorRole::Symbol && (style.width_mm - 0.1524).abs() < 1e-9) =>
        {
            // PIN_LAYOUT_CACHE gives degenerate pin lines KiCad's 15 mil
            // conservative guard along the pin axis. It does not inflate the
            // zero-width transverse axis.
            let mut bounds = Bounds::from_points([*from, *to])?;
            if (from.x - to.x).abs() >= (from.y - to.y).abs() {
                bounds.min_x -= 0.3811;
                bounds.max_x += 0.3811;
            } else {
                bounds.min_y -= 0.3811;
                bounds.max_y += 0.3811;
            }
            Some(bounds)
        }
        Primitive::Rect { bounds, style, .. } if (style.width_mm - 0.254).abs() < 1e-9 => {
            let mut bounds = *bounds;
            bounds.inflate(style.width_mm / 2.0);
            Some(bounds)
        }
        // LIB_SYMBOL merges the library draw-item geometry itself. Unlike
        // top-level SCH_ITEM insertion, it does not add the plot stroke here.
        _ => primitive.bounds(),
    });
    let mut combined = bounds.next()?;
    for bounds in bounds {
        combined.include_bounds(bounds);
    }
    for property in node.find_all("property") {
        if is_hidden(property) {
            continue;
        }
        if let Some(bounds) = kicad_text_box(property) {
            combined.include_bounds(bounds);
        }
    }
    Some(combined)
}

fn kicad_text_box(node: &SexpNode) -> Option<Bounds> {
    kicad_text_box_for(node, node.get(2).and_then(SexpNode::as_str).unwrap_or(""))
}

fn local_label_index_bounds(node: &SexpNode) -> Option<Bounds> {
    const QUANTUM: f64 = 0.0001;
    let position = at_point(node)?;
    let text = node.get(1).and_then(SexpNode::as_str).unwrap_or("");
    let size = text_size(node, 1.27);
    let pen = text_anchor_pen_width(node, size);
    let size_iu = (size / QUANTUM).round() as i64;
    let pen_iu = (pen / QUANTUM).round() as i64;
    let width_iu =
        (boundary_width(text, size, pen_iu as f64 * QUANTUM, QUANTUM) / QUANTUM).round() as i64;
    let font_inflation_iu = ((pen_iu as f64) * 1.5).round() as i64;
    let extent_height_iu = size_iu + 2 * font_inflation_iu;
    let fudge_iu = ((extent_height_iu as f64) * 0.17).round() as i64;
    let height_iu = extent_height_iu + fudge_iu;
    let text_offset_iu = ((size_iu as f64) * 0.15).round() as i64;
    // SCH_LABEL::GetBodyBoundingBox inflates by the effective pen, then
    // SCH_RTREE::insert inflates once more by SCH_TEXT::GetPenWidth.
    let double_pen_iu = 2 * pen_iu;
    let left_iu = -double_pen_iu;
    let right_iu = width_iu + double_pen_iu;
    let top_iu = -height_iu + fudge_iu - text_offset_iu - double_pen_iu;
    let bottom_iu = fudge_iu - text_offset_iu + double_pen_iu;
    let rotation = readable_text_rotation(at_rotation(node));
    Bounds::from_points(
        [
            Point::new(left_iu as f64 * QUANTUM, top_iu as f64 * QUANTUM),
            Point::new(right_iu as f64 * QUANTUM, top_iu as f64 * QUANTUM),
            Point::new(right_iu as f64 * QUANTUM, bottom_iu as f64 * QUANTUM),
            Point::new(left_iu as f64 * QUANTUM, bottom_iu as f64 * QUANTUM),
        ]
        .into_iter()
        .map(|corner| rotate_about(corner, position, rotation)),
    )
}

fn kicad_text_box_for(node: &SexpNode, text: &str) -> Option<Bounds> {
    const QUANTUM: f64 = 0.0001;
    let position = at_point(node)?;
    let size = text_size(node, 1.27);
    let pen = text_anchor_pen_width(node, size);
    let size_iu = (size / QUANTUM).round() as i64;
    let pen_iu = (pen / QUANTUM).round() as i64;
    let width_iu =
        (boundary_width(text, size, pen_iu as f64 * QUANTUM, QUANTUM) / QUANTUM).round() as i64;
    let inflation_iu = ((pen_iu as f64) * 1.5).round() as i64;
    let extent_height_iu = size_iu + 2 * inflation_iu;
    let fudge_iu = ((extent_height_iu as f64) * 0.17).round() as i64;
    let height_iu = extent_height_iu + fudge_iu;
    let left_iu = match text_align(node) {
        TextAlign::Left => 0,
        TextAlign::Center => -(width_iu / 2),
        TextAlign::Right => -width_iu,
    };
    let top_iu = -(height_iu / 2);
    let rotation = readable_text_rotation(at_rotation(node));
    Bounds::from_points(
        [
            Point::new(left_iu as f64 * QUANTUM, top_iu as f64 * QUANTUM),
            Point::new(
                (left_iu + width_iu) as f64 * QUANTUM,
                top_iu as f64 * QUANTUM,
            ),
            Point::new(
                (left_iu + width_iu) as f64 * QUANTUM,
                (top_iu + height_iu) as f64 * QUANTUM,
            ),
            Point::new(
                left_iu as f64 * QUANTUM,
                (top_iu + height_iu) as f64 * QUANTUM,
            ),
        ]
        .into_iter()
        .map(|corner| rotate_about(corner, position, rotation)),
    )
}

fn symbol_reference(node: &SexpNode) -> Option<&str> {
    node.find_all("property")
        .into_iter()
        .find(|property| property.get(1).and_then(SexpNode::as_str) == Some("Reference"))
        .and_then(|property| property.get(2).and_then(SexpNode::as_str))
}

fn cached_symbol_ranks(
    file: &Path,
    symbols: &[&SexpNode],
    occurrence: usize,
) -> Option<HashMap<String, usize>> {
    let svg = std::env::var_os("KONNECT_SVG_ORDER_ORACLE")
        .and_then(|path| std::fs::read_to_string(path).ok())
        .or_else(|| crate::svg_order_cache::load_fresh(file))?;
    let references = symbols.iter().filter_map(|symbol| symbol_reference(symbol));
    svg_reference_ranks(&svg, references, occurrence)
}

fn svg_reference_ranks<'a>(
    svg: &str,
    references: impl IntoIterator<Item = &'a str>,
    occurrence: usize,
) -> Option<HashMap<String, usize>> {
    let mut positions = Vec::new();
    for reference in references {
        let escaped = reference
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let needle = format!("<desc>{escaped}</desc>");
        let position = svg
            .match_indices(&needle)
            .map(|(position, _)| position)
            .nth(occurrence);
        if let Some(position) = position {
            positions.push((position, reference.to_owned()));
        }
    }
    if positions.is_empty() {
        return None;
    }
    positions.sort_by_key(|entry| entry.0);
    Some(
        positions
            .into_iter()
            .enumerate()
            .map(|(rank, (_, reference))| (reference, rank))
            .collect(),
    )
}

fn role_name(role: ColorRole) -> &'static str {
    match role {
        ColorRole::Bus => "bus",
        ColorRole::GraphicText => "graphic text",
        ColorRole::Wire => "wire",
        _ => "object",
    }
}

fn library_symbols(root: &SexpNode) -> HashMap<String, &SexpNode> {
    root.find("lib_symbols")
        .map(|libraries| {
            libraries
                .find_all("symbol")
                .into_iter()
                .filter_map(|symbol| {
                    let name = symbol.get(1).and_then(SexpNode::as_str)?;
                    Some((name.to_owned(), symbol))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn symbol_unit_matches(name: &str, unit: u32) -> bool {
    let mut parts = name.rsplitn(3, '_');
    let style = parts.next().and_then(|value| value.parse::<u32>().ok());
    let parsed_unit = parts.next().and_then(|value| value.parse::<u32>().ok());
    matches!(style, Some(0 | 1))
        && matches!(parsed_unit, Some(value) if value == 0 || value == unit)
}

fn symbol_transform(node: &SexpNode, origin: Point) -> PinTransform {
    let mirror = node
        .find("mirror")
        .and_then(|item| item.get(1))
        .and_then(SexpNode::as_str);
    PinTransform {
        comp_x: origin.x,
        comp_y: origin.y,
        rotation_deg: at_rotation(node),
        mirror_x: matches!(mirror, Some("x" | "xy")),
        mirror_y: matches!(mirror, Some("y" | "xy")),
    }
}

fn transform_point(point: Point, transform: PinTransform) -> Point {
    let (x, y) = transform_pin(point.x, point.y, transform);
    Point::new(x, y)
}

pub(crate) fn normalize_kicad_arc(start: Point, mid: Point, end: Point) -> (Point, Point, Point) {
    const IU_PER_MM: f64 = 10_000.0;
    let mut start = Point::new((start.x * IU_PER_MM).round(), (start.y * IU_PER_MM).round());
    let input_mid = Point::new((mid.x * IU_PER_MM).round(), (mid.y * IU_PER_MM).round());
    let mut end = Point::new((end.x * IU_PER_MM).round(), (end.y * IU_PER_MM).round());
    let calculated_center = kicad_arc_center_iu(start, input_mid, end);
    let center = Point::new(calculated_center.x.round(), calculated_center.y.round());
    let mut normalized_mid = rotate_iu(
        start,
        center,
        -increasing_arc_angle(start, end, center) / 2.0,
    );
    let separation =
        (normalized_mid.x - input_mid.x).powi(2) + (normalized_mid.y - input_mid.y).powi(2);
    let radius_squared =
        (normalized_mid.x - center.x).powi(2) + (normalized_mid.y - center.y).powi(2);

    if separation > radius_squared {
        std::mem::swap(&mut start, &mut end);
        normalized_mid = rotate_iu(
            start,
            center,
            -increasing_arc_angle(start, end, center) / 2.0,
        );
    } else {
        // GetArcMid() returns the originally supplied point as long as the
        // start, end, and integer center remain unchanged.
        normalized_mid = input_mid;
    }

    let to_mm = |point: Point| Point::new(point.x / IU_PER_MM, point.y / IU_PER_MM);
    (to_mm(start), to_mm(normalized_mid), to_mm(end))
}

fn increasing_arc_angle(start: Point, end: Point, center: Point) -> f64 {
    let start_angle = (start.y - center.y).atan2(start.x - center.x);
    let mut end_angle = (end.y - center.y).atan2(end.x - center.x);
    while end_angle < start_angle {
        end_angle += std::f64::consts::TAU;
    }
    if end_angle == start_angle {
        std::f64::consts::TAU
    } else {
        end_angle - start_angle
    }
}

fn rotate_iu(point: Point, center: Point, angle: f64) -> Point {
    let x = point.x - center.x;
    let y = point.y - center.y;
    Point::new(
        (y * angle.sin() + x * angle.cos()).round() + center.x,
        (y * angle.cos() - x * angle.sin()).round() + center.y,
    )
}

pub(crate) fn kicad_arc_center_iu(start: Point, mid: Point, end: Point) -> Point {
    // Port of KiCad's uncertainty-aware CalcArcCenter().  The inputs are
    // integer schematic units (0.1 um), represented as f64 so the uncertainty
    // propagation remains identical to the upstream calculation.
    let y_delta_21 = mid.y - start.y;
    let mut x_delta_21 = mid.x - start.x;
    let y_delta_32 = end.y - mid.y;
    let mut x_delta_32 = end.x - mid.x;

    if (x_delta_21 == 0.0 && y_delta_32 == 0.0) || (y_delta_21 == 0.0 && x_delta_32 == 0.0) {
        return Point::new((start.x + end.x) / 2.0, (start.y + end.y) / 2.0);
    }
    if x_delta_21 == 0.0 {
        x_delta_21 = f64::EPSILON;
    }
    if x_delta_32 == 0.0 {
        x_delta_32 = -f64::EPSILON;
    }

    let mut a_slope = y_delta_21 / x_delta_21;
    let mut b_slope = y_delta_32 / x_delta_32;
    let da_slope = a_slope * (0.5 / y_delta_21).hypot(0.5 / x_delta_21);
    let db_slope = b_slope * (0.5 / y_delta_32).hypot(0.5 / x_delta_32);
    if a_slope == b_slope {
        if start == end {
            return Point::new((start.x + mid.x) / 2.0, (start.y + mid.y) / 2.0);
        }
        a_slope += f64::EPSILON;
        b_slope -= f64::EPSILON;
    }
    if a_slope == 0.0 {
        a_slope = 1e-10;
    }
    if b_slope == 0.0 {
        b_slope = 1e-10;
    }

    let ab_slope_start_end_y = a_slope * b_slope * (start.y - end.y);
    let d_ab_slope_start_end_y = ab_slope_start_end_y
        * ((da_slope / a_slope).powi(2)
            + (db_slope / b_slope).powi(2)
            + (std::f64::consts::FRAC_1_SQRT_2 / (start.y - end.y)).powi(2))
        .sqrt();
    let b_slope_start_mid_x = b_slope * (start.x + mid.x);
    let d_b_slope_start_mid_x = b_slope_start_mid_x
        * ((db_slope / b_slope).powi(2)
            + (std::f64::consts::FRAC_1_SQRT_2 / (start.x + mid.x)).powi(2))
        .sqrt();
    let a_slope_mid_end_x = a_slope * (mid.x + end.x);
    let d_a_slope_mid_end_x = a_slope_mid_end_x
        * ((da_slope / a_slope).powi(2)
            + (std::f64::consts::FRAC_1_SQRT_2 / (mid.x + end.x)).powi(2))
        .sqrt();
    let twice_ba_slope_diff = 2.0 * (b_slope - a_slope);
    let d_twice_ba_slope_diff = 2.0 * db_slope.hypot(da_slope);
    let center_numerator_x = ab_slope_start_end_y + b_slope_start_mid_x - a_slope_mid_end_x;
    let d_center_numerator_x = d_ab_slope_start_end_y
        .hypot(d_b_slope_start_mid_x)
        .hypot(d_a_slope_mid_end_x);
    let center_x = center_numerator_x / twice_ba_slope_diff;
    let d_center_x = center_x
        * ((d_center_numerator_x / center_numerator_x).powi(2)
            + (d_twice_ba_slope_diff / twice_ba_slope_diff).powi(2))
        .sqrt();
    let center_numerator_y = (start.x + mid.x) / 2.0 - center_x;
    let d_center_numerator_y = (0.125 + d_center_x.powi(2)).sqrt();
    let center_first_term = center_numerator_y / a_slope;
    let d_center_first_term_y = center_first_term
        * ((d_center_numerator_y / center_numerator_y).powi(2) + (da_slope / a_slope).powi(2))
            .sqrt();
    let center_y = center_first_term + (start.y + mid.y) / 2.0;
    let d_center_y = (d_center_first_term_y.powi(2) + 0.125).sqrt();

    let rounded_100_x = ((center_x + 50.0) / 100.0).floor() * 100.0;
    let rounded_100_y = ((center_y + 50.0) / 100.0).floor() * 100.0;
    let rounded_10_x = ((center_x + 5.0) / 10.0).floor() * 10.0;
    let rounded_10_y = ((center_y + 5.0) / 10.0).floor() * 10.0;
    if (rounded_100_x - center_x).abs() < d_center_x
        && (rounded_100_y - center_y).abs() < d_center_y
    {
        Point::new(rounded_100_x, rounded_100_y)
    } else if (rounded_10_x - center_x).abs() < d_center_x
        && (rounded_10_y - center_y).abs() < d_center_y
    {
        Point::new(rounded_10_x, rounded_10_y)
    } else {
        Point::new(center_x, center_y)
    }
}

fn rotate_about(local: Point, origin: Point, rotation_deg: f64) -> Point {
    let angle = rotation_deg.to_radians();
    Point::new(
        origin.x + local.x * angle.cos() + local.y * angle.sin(),
        origin.y - local.x * angle.sin() + local.y * angle.cos(),
    )
}

fn readable_text_rotation(rotation_deg: f64) -> f64 {
    let rotation = rotation_deg.rem_euclid(360.0);
    if (135.0..315.0).contains(&rotation) {
        rotation - 180.0
    } else if rotation >= 315.0 {
        rotation - 360.0
    } else {
        rotation
    }
}

fn unit_direction(from: Point, to: Point) -> Point {
    let dx = from.x - to.x;
    let dy = from.y - to.y;
    let length = dx.hypot(dy).max(f64::EPSILON);
    Point::new(dx / length, dy / length)
}

fn points_from_pts(node: &SexpNode) -> Vec<Point> {
    node.find("pts")
        .map(|points| {
            points
                .find_all("xy")
                .into_iter()
                .filter_map(|point| Some(Point::new(point.get_f64(1)?, point.get_f64(2)?)))
                .collect()
        })
        .unwrap_or_default()
}

fn at_point(node: &SexpNode) -> Option<Point> {
    tag_point(node, "at")
}

fn tag_point(node: &SexpNode, tag: &str) -> Option<Point> {
    let point = node.find(tag)?;
    Some(Point::new(point.get_f64(1)?, point.get_f64(2)?))
}

fn at_rotation(node: &SexpNode) -> f64 {
    node.find("at")
        .and_then(|at| at.get_f64(3))
        .unwrap_or_default()
}

fn child_text<'a>(node: &'a SexpNode, tag: &str) -> Option<&'a str> {
    node.find(tag)?.get(1)?.as_str()
}

fn child_f64(node: &SexpNode, tag: &str) -> Option<f64> {
    node.find(tag)?.get_f64(1)
}

fn child_u32(node: &SexpNode, tag: &str) -> Option<u32> {
    child_text(node, tag)?.parse().ok()
}

fn stroke_width(node: &SexpNode, default: f64) -> f64 {
    node.find("stroke")
        .and_then(|stroke| child_f64(stroke, "width"))
        .filter(|width| *width > 0.0)
        .unwrap_or(default)
}

fn graphic_fill_in_pass(node: &SexpNode, pass: GraphicPass) -> bool {
    matches!(
        (
            node.find("fill").and_then(|fill| child_text(fill, "type")),
            pass,
        ),
        (Some("background"), GraphicPass::Background) | (Some("outline"), GraphicPass::Foreground)
    )
}

fn graphic_stroke(node: &SexpNode, pass: GraphicPass) -> StrokeStyle {
    StrokeStyle::new(
        if pass == GraphicPass::Foreground {
            stroke_width(node, 0.1524)
        } else {
            0.0
        },
        ColorRole::Symbol,
    )
}

fn is_hidden(node: &SexpNode) -> bool {
    node.find("hide").is_some()
        || node
            .find("effects")
            .and_then(|effects| effects.find("hide"))
            .is_some()
        || node
            .find("effects")
            .and_then(|effects| child_text(effects, "hide"))
            == Some("yes")
}

fn text_size(node: &SexpNode, default: f64) -> f64 {
    node.find("effects")
        .and_then(|effects| effects.find("font"))
        .and_then(|font| font.find("size"))
        .and_then(|size| size.get_f64(2).or_else(|| size.get_f64(1)))
        .unwrap_or(default)
}

fn text_italic(node: &SexpNode) -> bool {
    node.find("effects")
        .and_then(|effects| effects.find("font"))
        .is_some_and(|font| font.find("italic").is_some())
}

fn text_stroke_width(node: &SexpNode, size: f64) -> f64 {
    let font = node
        .find("effects")
        .and_then(|effects| effects.find("font"));
    let explicit = font
        .and_then(|font| child_f64(font, "thickness"))
        .filter(|width| *width > 0.0);
    let width = explicit.unwrap_or_else(|| {
        if font.is_some_and(|font| font.find("bold").is_some()) {
            size / 5.0
        } else {
            0.1524
        }
    });
    width.min(size * 0.25)
}

fn text_anchor_pen_width(node: &SexpNode, size: f64) -> f64 {
    let font = node
        .find("effects")
        .and_then(|effects| effects.find("font"));
    let explicit = font
        .and_then(|font| child_f64(font, "thickness"))
        .filter(|width| *width > 0.0);
    let width = explicit.unwrap_or_else(|| {
        if font.is_some_and(|font| font.find("bold").is_some()) {
            size / 5.0
        } else {
            size / 8.0
        }
    });
    width.min(size * 0.25)
}

fn local_label_text_offset(node: &SexpNode, size: f64) -> f64 {
    const QUANTUM: f64 = 0.0001;

    let size_iu = (size / QUANTUM).round() as i64;
    let text_offset_iu = (0.15 * size_iu as f64).round() as i64;
    let pen_iu = (text_anchor_pen_width(node, size) / QUANTUM).round() as i64;
    let line_height_iu = (1.17 * size_iu as f64).trunc() as i64;
    // Local labels are bottom-justified.  The scene stores a center-justified
    // origin for all stroke text, so move by the exact integer difference
    // between KiCad's bottom and center line positions.  For odd line heights
    // this is one IU larger than the old 0.585 * size approximation.
    let justification_iu = line_height_iu - line_height_iu / 2;

    (text_offset_iu + pen_iu + justification_iu) as f64 * QUANTUM
}

fn pin_number_text_offset(size: f64) -> f64 {
    const QUANTUM: f64 = 0.0001;

    let size_iu = (size / QUANTUM).round() as i64;
    let line_height_iu = (1.17 * size_iu as f64).trunc() as i64;
    let justification_iu = line_height_iu - line_height_iu / 2;
    // Two rounded 4 mil clearances and KiCad's 6 mil default plot pen.
    let clearance_iu = 1016 + 1016 + 1524;

    (clearance_iu + justification_iu) as f64 * QUANTUM
}

fn symbol_field_plot_position(node: &SexpNode) -> Option<Point> {
    const QUANTUM: f64 = 0.0001;

    let bounds = kicad_text_box(node)?;
    let min_x = (bounds.min_x / QUANTUM).round() as i64;
    let min_y = (bounds.min_y / QUANTUM).round() as i64;
    let max_x = (bounds.max_x / QUANTUM).round() as i64;
    let max_y = (bounds.max_y / QUANTUM).round() as i64;

    Some(Point::new(
        ((min_x + max_x) / 2) as f64 * QUANTUM,
        ((min_y + max_y) / 2) as f64 * QUANTUM,
    ))
}

fn snap_to_mil(value_mm: f64) -> f64 {
    (value_mm / 0.0254).round() * 0.0254
}

fn text_align(node: &SexpNode) -> TextAlign {
    let justify = node
        .find("effects")
        .and_then(|effects| effects.find("justify"));
    if justify
        .and_then(|value| value.get(1))
        .and_then(SexpNode::as_str)
        == Some("left")
    {
        TextAlign::Left
    } else if justify
        .and_then(|value| value.get(1))
        .and_then(SexpNode::as_str)
        == Some("right")
    {
        TextAlign::Right
    } else {
        TextAlign::Center
    }
}

fn paper_size(root: &SexpNode) -> (f64, f64) {
    let paper = root
        .find("paper")
        .and_then(|node| node.get(1))
        .and_then(SexpNode::as_str)
        .unwrap_or("A4");
    let portrait = root
        .find("paper")
        .and_then(|node| node.get(2))
        .and_then(SexpNode::as_str)
        == Some("portrait");
    let landscape = match paper {
        "A0" => (1189.0, 841.0),
        "A1" => (841.0, 594.0),
        "A2" => (594.0, 420.0),
        "A3" => (420.0, 297.0),
        "A5" => (210.0, 148.0),
        "USLetter" => (279.4, 215.9),
        "USLegal" => (355.6, 215.9),
        "USLedger" => (431.8, 279.4),
        _ => (297.0, 210.0),
    };
    let landscape = (snap_to_mil(landscape.0), snap_to_mil(landscape.1));
    if portrait {
        (landscape.1, landscape.0)
    } else {
        landscape
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_coverage_reports_unknown_visual_constructs_but_not_metadata() {
        let root = parse_sexp(
            r#"(kicad_sch
                (version 20250101)
                (uuid "root")
                (lib_symbols)
                (wire (pts (xy 0 0) (xy 1 0)) (uuid "wire"))
                (text_box "unsupported-a")
                (text_box "unsupported-b")
                (image (uuid "image")))"#,
        )
        .expect("fixture parses");

        let coverage = analyze_render_coverage(&root);

        assert_eq!(
            coverage.unsupported,
            vec![
                UnsupportedConstruct {
                    kind: "image".to_owned(),
                    count: 1,
                },
                UnsupportedConstruct {
                    kind: "text_box".to_owned(),
                    count: 2,
                },
            ]
        );
        assert!(!coverage.is_complete());
    }

    #[test]
    fn connectivity_reports_a_missing_t_junction() {
        let root = parse_sexp(
            r#"(kicad_sch
                (wire (pts (xy 0 0) (xy 10 0)) (uuid "wire-a"))
                (wire (pts (xy 5 0) (xy 5 5)) (uuid "wire-b")))"#,
        )
        .unwrap();

        let diagnostics = analyze_connectivity(&root);

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == ConnectivityDiagnosticKind::MissingJunction
                && points_near(diagnostic.point, Point::new(5.0, 0.0), 1e-9)
        }));
    }

    #[test]
    fn connectivity_accepts_explicit_anchors_and_junctions() {
        let root = parse_sexp(
            r#"(kicad_sch
                (wire (pts (xy 0 0) (xy 10 0)) (uuid "wire-a"))
                (wire (pts (xy 5 0) (xy 5 5)) (uuid "wire-b"))
                (label "START" (at 0 0 0) (uuid "label-a"))
                (label "END_A" (at 10 0 0) (uuid "label-b"))
                (label "END_B" (at 5 5 0) (uuid "label-c"))
                (junction (at 5 0) (uuid "junction-a")))"#,
        )
        .unwrap();

        assert!(analyze_connectivity(&root).is_empty());
    }

    #[test]
    fn connectivity_rejects_a_wired_no_connect_marker() {
        let root = parse_sexp(
            r#"(kicad_sch
                (wire (pts (xy 0 0) (xy 10 0)) (uuid "wire-a"))
                (label "START" (at 0 0 0) (uuid "label-a"))
                (no_connect (at 10 0) (uuid "nc-a")))"#,
        )
        .unwrap();

        let diagnostics = analyze_connectivity(&root);

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == ConnectivityDiagnosticKind::ConnectedNoConnect
                && points_near(diagnostic.point, Point::new(10.0, 0.0), 1e-9)
        }));
    }

    #[test]
    fn connectivity_rejects_duplicate_sheet_names_and_pins() {
        let root = parse_sexp(
            r#"(kicad_sch
                (sheet
                    (at 10 10) (size 20 10) (uuid "sheet-a")
                    (property "Sheetname" "Control" (at 10 9 0))
                    (property "Sheetfile" "a.kicad_sch" (at 10 21 0))
                    (pin "READY" input (at 10 12 180) (uuid "pin-a"))
                    (pin "READY" output (at 30 14 0) (uuid "pin-b")))
                (sheet
                    (at 50 10) (size 20 10) (uuid "sheet-b")
                    (property "Sheetname" "Control" (at 50 9 0))
                    (property "Sheetfile" "b.kicad_sch" (at 50 21 0))))"#,
        )
        .unwrap();

        let diagnostics = analyze_connectivity(&root);

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == ConnectivityDiagnosticKind::DuplicateSheetName
        }));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == ConnectivityDiagnosticKind::DuplicateSheetPin
        }));
    }

    #[test]
    fn connectivity_reports_duplicate_annotated_references() {
        let root = parse_sexp(
            r#"(kicad_sch
                (symbol (lib_id "Device:R") (at 10 10 0)
                    (property "Reference" "R1") (uuid "symbol-a"))
                (symbol (lib_id "Device:R") (at 20 20 0)
                    (property "Reference" "R1") (uuid "symbol-b"))
                (symbol (lib_id "Device:R") (at 30 30 0)
                    (property "Reference" "R?") (uuid "symbol-c")))"#,
        )
        .unwrap();

        let diagnostics = analyze_connectivity(&root);

        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.kind == ConnectivityDiagnosticKind::DuplicateReference
                })
                .count(),
            1
        );
    }

    #[test]
    fn connectivity_checks_that_bus_entries_bridge_bus_and_wire() {
        let root = parse_sexp(
            r#"(kicad_sch
                (bus (pts (xy 0 0) (xy 10 0)) (uuid "bus-a"))
                (bus_entry (at 5 0) (size 2.54 2.54) (uuid "entry-ok"))
                (wire (pts (xy 7.54 2.54) (xy 10 2.54)) (uuid "wire-a"))
                (bus_entry (at 20 20) (size 2.54 2.54) (uuid "entry-bad")))"#,
        )
        .unwrap();

        let diagnostics = analyze_connectivity(&root);

        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.kind == ConnectivityDiagnosticKind::UnconnectedBusEntry
                })
                .count(),
            1
        );
    }

    #[test]
    fn render_coverage_is_complete_for_supported_document() {
        let root = parse_sexp(
            r#"(kicad_sch
                (version 20250101)
                (uuid "root")
                (paper "A4")
                (symbol_instances)
                (junction (at 1 1) (uuid "junction")))"#,
        )
        .expect("fixture parses");

        assert!(analyze_render_coverage(&root).is_complete());
    }

    #[test]
    fn object_search_text_includes_uuid_library_and_symbol_properties() {
        let node = parse_sexp(
            r#"(symbol
                (lib_id "Device:R")
                (at 10 10 0)
                (property "Reference" "R42" (at 10 8 0))
                (property "Value" "4.7k" (at 10 12 0))
                (uuid "searchable-symbol"))"#,
        )
        .expect("fixture parses");

        let search = object_search_text(&node, "R42", "searchable-symbol");

        assert!(search.contains("R42"));
        assert!(search.contains("4.7k"));
        assert!(search.contains("Device:R"));
        assert!(search.contains("searchable-symbol"));
    }

    #[test]
    fn direct_scene_preserves_uuid_for_hit_testing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scene.kicad_sch");
        std::fs::write(
            &path,
            r#"(kicad_sch
  (version 20250101)
  (generator "eeschema")
  (paper "A4")
  (wire (pts (xy 20 30) (xy 40 30)) (stroke (width 0) (type default))
    (uuid "wire-uuid")))"#,
        )
        .unwrap();

        let scene = SchematicScene::load(&path).unwrap();
        let object = scene.hit_test(Point::new(30.0, 30.0), 0.2).unwrap();

        assert_eq!(object.uuid, "wire-uuid");
        assert_eq!(object.kind, ObjectKind::Wire);
    }

    #[test]
    fn large_synthetic_hierarchy_discovers_and_loads_every_sheet() {
        const CHILDREN: usize = 128;

        let directory = tempfile::tempdir().unwrap();
        let root_path = directory.path().join("stress-root.kicad_sch");
        let mut root = konnect_sexp::schematic::format_blank_schematic();
        let insert_at = root.rfind(')').expect("blank schematic has a root close");
        let mut sheets = String::new();

        for index in 0..CHILDREN {
            let child_name = format!("stress-child-{index:03}");
            let child_file = format!("{child_name}.kicad_sch");
            std::fs::write(
                directory.path().join(&child_file),
                konnect_sexp::schematic::format_blank_schematic(),
            )
            .unwrap();
            sheets.push_str(&konnect_sexp::schematic::format_hierarchical_sheet(
                konnect_sexp::schematic::HierarchicalSheetSpec {
                    name: &child_name,
                    file: &child_file,
                    x: 10.0 + (index % 16) as f64 * 12.0,
                    y: 10.0 + (index / 16) as f64 * 12.0,
                    width: 10.0,
                    height: 8.0,
                    project_name: "stress",
                    parent_instance_path: "/root",
                    page: &(index + 2).to_string(),
                },
            ));
        }
        root.insert_str(insert_at, &sheets);
        std::fs::write(&root_path, root).unwrap();

        let discovered = discover_hierarchy(&root_path).unwrap();
        assert_eq!(discovered.len(), CHILDREN + 1);
        assert_eq!(discovered[0].depth, 0);
        assert!(discovered[1..].iter().all(|entry| entry.depth == 1));

        let loaded = load_hierarchy(&root_path).unwrap();
        assert_eq!(loaded.len(), CHILDREN + 1);
        assert!(loaded
            .iter()
            .all(|sheet| sheet.scene.coverage.is_complete()));
    }

    #[test]
    fn symbol_unit_match_accepts_common_and_selected_units() {
        assert!(symbol_unit_matches("R_0_1", 1));
        assert!(symbol_unit_matches("R_1_1", 1));
        assert!(!symbol_unit_matches("R_2_1", 1));
        assert!(!symbol_unit_matches("not_a_unit", 1));
    }

    #[test]
    fn paper_orientation_is_respected() {
        let landscape = parse_sexp(r#"(kicad_sch (paper "A4"))"#).unwrap();
        let portrait = parse_sexp(r#"(kicad_sch (paper "A4" portrait))"#).unwrap();
        assert_eq!(paper_size(&landscape), (297.0022, 210.00719999999998));
        assert_eq!(paper_size(&portrait), (210.00719999999998, 297.0022));
    }

    #[test]
    fn local_label_uses_kicad_connection_line_offset() {
        let root = parse_sexp(r#"(kicad_sch (label "HB_IN" (at 10 20 0)))"#).unwrap();
        let mut builder = SceneBuilder::new(PathBuf::from("offset.kicad_sch"), Arc::from(""));
        builder.draw_label(root.find("label").unwrap(), false);

        let Primitive::Text { position, .. } = builder.primitives.last().unwrap() else {
            panic!("local label did not produce text");
        };
        assert_eq!(*position, Point::new(10.0, 18.9077));
    }

    #[test]
    fn global_label_uses_kicad_overbar_centering_offset() {
        let root = parse_sexp(r#"(kicad_sch (global_label "PACK_N" (at 10 20 0)))"#).unwrap();
        let mut builder = SceneBuilder::new(PathBuf::from("offset.kicad_sch"), Arc::from(""));
        builder.draw_label(root.find("global_label").unwrap(), true);

        let Primitive::Text { position, .. } = builder.primitives.last().unwrap() else {
            panic!("global label did not produce text");
        };
        assert_eq!(*position, Point::new(11.4288, 20.0908));
    }

    #[test]
    fn global_label_frame_uses_kicad_integer_margins() {
        let root = parse_sexp(r#"(kicad_sch (global_label "CELL2" (at 10 20 0)))"#).unwrap();
        let mut builder = SceneBuilder::new(PathBuf::from("frame.kicad_sch"), Arc::from(""));
        builder.draw_label(root.find("global_label").unwrap(), true);

        let Primitive::Polyline { points, closed, .. } = builder.primitives.first().unwrap() else {
            panic!("global label did not produce a frame");
        };
        assert!(*closed);
        assert_eq!(points[1], Point::new(11.1113, 21.2704));
        assert_eq!(points[3].y, 20.0);
        assert_eq!(points[4].y, 18.7296);
    }

    #[test]
    fn italic_effect_is_preserved_in_semantic_text() {
        let root = parse_sexp(
            r#"(kicad_sch (text "italic" (at 10 20 0)
                (effects (font (size 1.27 1.27) (italic yes)))))"#,
        )
        .unwrap();
        let mut builder = SceneBuilder::new(PathBuf::from("italic.kicad_sch"), Arc::from(""));
        builder.draw_top_level_text(root.find("text").unwrap());
        assert!(matches!(
            builder.primitives.first(),
            Some(Primitive::Text { italic: true, .. })
        ));
    }

    #[test]
    fn reversed_local_label_stays_above_the_connection_line() {
        let root = parse_sexp(r#"(kicad_sch (label "HB_IN" (at 10 20 180)))"#).unwrap();
        let mut builder = SceneBuilder::new(PathBuf::from("offset.kicad_sch"), Arc::from(""));
        builder.draw_label(root.find("label").unwrap(), false);

        let Primitive::Text {
            position,
            rotation_deg,
            ..
        } = builder.primitives.last().unwrap()
        else {
            panic!("local label did not produce text");
        };
        assert_eq!(*position, Point::new(10.0, 18.9077));
        assert_eq!(*rotation_deg, 0.0);
    }

    #[test]
    fn overlap_redraw_marks_both_symbols_but_not_a_neighbour() {
        let object = |uuid: &str, min_x, max_x| {
            let bounds = Bounds {
                min_x,
                min_y: 0.0,
                max_x,
                max_y: 10.0,
            };
            SceneObject {
                uuid: uuid.to_owned(),
                kind: ObjectKind::Symbol,
                item_type: 70,
                label: uuid.to_owned(),
                search_text: uuid.to_owned(),
                properties: Vec::new(),
                bounds,
                index_bounds: bounds,
                initial_index_bounds: bounds,
                primitive_range: 0..0,
            }
        };
        let objects = [
            object("left", 0.0, 5.0),
            object("middle", 4.0, 8.0),
            object("right", 9.0, 12.0),
        ];

        assert_eq!(
            overlapping_object_uuids(&objects),
            HashSet::from(["left".to_owned(), "middle".to_owned()])
        );
    }

    #[test]
    fn automatic_label_pen_uses_distinct_plot_and_anchor_widths() {
        let root = parse_sexp(r#"(label "L" (effects (font (size 1.27 1.27))))"#).unwrap();

        assert_eq!(text_stroke_width(&root, 1.27), 0.1524);
        assert_eq!(text_anchor_pen_width(&root, 1.27), 1.27 / 8.0);
    }

    #[test]
    fn pin_name_offset_defaults_to_half_a_millimetre() {
        let symbol = parse_sexp(r#"(symbol "Device:R")"#).unwrap();
        assert_eq!(PinTextOptions::from_symbol(&symbol).name_offset, 0.508);
    }

    #[test]
    fn root_worksheet_counts_direct_hierarchy_pages() {
        let root = parse_sexp(
            r#"(kicad_sch (paper "A4")
                (sheet (at 1 1) (size 2 2))
                (sheet (at 4 4) (size 2 2)))"#,
        )
        .unwrap();
        let mut builder = SceneBuilder::new(PathBuf::from("root.kicad_sch"), Arc::from(""));
        builder.draw_page(297.0022, 210.0072, &root);

        assert!(builder.primitives.iter().any(|primitive| matches!(
            primitive,
            Primitive::Text { text, .. } if text == "Id: 1/3"
        )));
    }

    #[test]
    fn worksheet_zone_ticks_are_between_centered_row_labels() {
        let root = parse_sexp(r#"(kicad_sch (paper "A4"))"#).unwrap();
        let mut builder = SceneBuilder::new(PathBuf::from("root.kicad_sch"), Arc::from(""));
        builder.draw_page(297.0022, 210.0072, &root);

        assert!(builder.primitives.iter().any(|primitive| matches!(
            primitive,
            Primitive::Line { from, to, .. }
                if *from == Point::new(10.0, 60.0) && *to == Point::new(12.0, 60.0)
        )));
        assert!(!builder.primitives.iter().any(|primitive| matches!(
            primitive,
            Primitive::Line { from, to, .. }
                if *from == Point::new(10.0, 35.0) && *to == Point::new(12.0, 35.0)
        )));
        assert!(builder.primitives.iter().any(|primitive| matches!(
            primitive,
            Primitive::Text { position, text, .. }
                if *position == Point::new(11.0, 35.0) && text == "A"
        )));
    }

    #[test]
    fn unpositioned_sheet_fields_match_kicad_origin_fallback() {
        let root = parse_sexp(
            r#"(kicad_sch
                (sheet (at 10 20) (size 30 40)
                  (property "Sheetname" "POWER")
                  (property "Sheetfile" "power.kicad_sch")))"#,
        )
        .unwrap();
        let mut builder = SceneBuilder::new(PathBuf::from("root.kicad_sch"), Arc::from(""));
        builder.draw_sheet(root.find("sheet").unwrap());

        assert!(builder.primitives.iter().any(|primitive| matches!(
            primitive,
            Primitive::Text { position, text, role: ColorRole::PinName, .. }
                if *position == Point::new(0.0, 0.0) && text == "POWER"
        )));
        assert!(builder.primitives.iter().any(|primitive| matches!(
            primitive,
            Primitive::Text { position, text, role: ColorRole::SheetFile, .. }
                if *position == Point::new(0.0, 0.0) && text == "File: power.kicad_sch"
        )));
        assert_eq!(
            analyze_connectivity(&root)
                .iter()
                .filter(|diagnostic| {
                    diagnostic.kind == ConnectivityDiagnosticKind::UnpositionedSheetField
                })
                .count(),
            2
        );
    }

    #[test]
    fn pin_numbers_stay_above_the_readable_axis_on_both_sides() {
        let transform = PinTransform {
            comp_x: 0.0,
            comp_y: 0.0,
            rotation_deg: 0.0,
            mirror_x: false,
            mirror_y: false,
        };
        let options = PinTextOptions {
            name_offset: 0.508,
            show_names: false,
            show_numbers: true,
        };
        let mut positions = Vec::new();
        for rotation in [0, 180] {
            let pin = parse_sexp(&format!(
                r#"(pin unspecified line (at 0 0 {rotation}) (length 2.54)
                    (name "~") (number "1" (effects (font (size 1.27 1.27)))))"#
            ))
            .unwrap();
            let mut builder = SceneBuilder::new(PathBuf::from("pin.kicad_sch"), Arc::from(""));
            builder.draw_library_pin(&pin, transform, options);
            let position = builder
                .primitives
                .iter()
                .find_map(|primitive| match primitive {
                    Primitive::Text {
                        position,
                        role: ColorRole::PinNumber,
                        ..
                    } => Some(*position),
                    _ => None,
                })
                .unwrap();
            positions.push(position);
        }

        let expected_offset = 1.0986;
        assert!((positions[0].y + expected_offset).abs() < 1e-12);
        assert!((positions[1].y + expected_offset).abs() < 1e-12);
    }

    #[test]
    fn moving_a_symbol_is_targeted_and_moves_visible_fields() {
        let source = r#"(kicad_sch
  (lib_symbols (symbol "Device:R" (uuid "library-copy")))
  (symbol
    (lib_id "Device:R")
    (at 10 20 90)
    (uuid "placed")
    (property "Reference" "R1" (at 11 18 90))
    (property "Value" "10k" (at 11 22 90)))
  (symbol (lib_id "Device:C") (at 40 50 0) (uuid "other")))"#;

        let moved = move_symbol_source(source, "placed", 1.27, -2.54).unwrap();

        assert!(moved.contains("(at 11.27 17.46 90)"));
        assert!(moved.contains("(at 12.27 15.46 90)"));
        assert!(moved.contains("(at 12.27 19.46 90)"));
        assert!(moved.contains("(at 40 50 0) (uuid \"other\")"));
        assert!(moved.contains("(uuid \"library-copy\")"));
    }

    #[test]
    fn moving_an_unknown_symbol_is_rejected() {
        let source = r#"(kicad_sch (symbol (at 10 20) (uuid "known")))"#;

        let error = move_symbol_source(source, "missing", 1.27, 0.0).unwrap_err();

        assert!(error.to_string().contains("was not found"));
        assert_eq!(source, r#"(kicad_sch (symbol (at 10 20) (uuid "known")))"#);
    }

    #[test]
    fn moving_wire_translates_every_endpoint_and_preserves_other_items() {
        let source = r#"(kicad_sch
  (wire (pts (xy 10 20) (xy 30 40)) (stroke (width 0) (type default))
    (uuid "wire-a"))
  (junction (at 30 40) (uuid "junction-b")))"#;

        let moved = move_item_source(source, "wire-a", 1.27, -2.54).expect("wire moves");

        assert!(moved.contains("(xy 11.27 17.46) (xy 31.27 37.46)"));
        assert!(moved.contains("(junction (at 30 40)"));
    }

    #[test]
    fn moving_symbol_stretches_only_its_connected_wire_endpoint() {
        let source = r#"(kicad_sch
  (lib_symbols
    (symbol "Device:R"
      (symbol "R_1_1"
        (pin passive line (at 0 0 0) (length 2.54) (name "1") (number "1")))))
  (symbol (lib_id "Device:R") (at 10 10 0) (unit 1)
    (property "Reference" "R1" (at 10 8 0)) (uuid "symbol-a"))
  (wire (pts (xy 10 10) (xy 30 10)) (uuid "wire-a")))"#;
        let selected = HashSet::from(["symbol-a".to_owned()]);

        let (edited, changed) =
            move_items_with_connected_wires(source, &selected, 5.0, 2.0).unwrap();
        let root = parse_sexp(&edited).unwrap();
        let wire = konnect_sexp::schematic::extract_wires(&root).remove(0);

        assert_eq!((wire.x1, wire.y1), (15.0, 12.0));
        assert_eq!((wire.x2, wire.y2), (30.0, 10.0));
        assert_eq!(changed, ["symbol-a", "wire-a"]);
    }

    #[test]
    fn wire_attached_to_two_moved_symbols_translates_without_deforming() {
        let source = r#"(kicad_sch
  (lib_symbols
    (symbol "Device:R"
      (symbol "R_1_1"
        (pin passive line (at 0 0 0) (length 2.54) (name "1") (number "1")))))
  (symbol (lib_id "Device:R") (at 10 10 0) (unit 1) (uuid "symbol-a"))
  (symbol (lib_id "Device:R") (at 30 10 0) (unit 1) (uuid "symbol-b"))
  (wire (pts (xy 10 10) (xy 30 10)) (uuid "wire-a")))"#;
        let selected = HashSet::from(["symbol-a".to_owned(), "symbol-b".to_owned()]);

        let (edited, _) = move_items_with_connected_wires(source, &selected, 0.0, 5.0).unwrap();
        let root = parse_sexp(&edited).unwrap();
        let wire = konnect_sexp::schematic::extract_wires(&root).remove(0);

        assert_eq!((wire.x1, wire.y1), (10.0, 15.0));
        assert_eq!((wire.x2, wire.y2), (30.0, 15.0));
    }

    #[test]
    fn connected_wire_detection_ignores_pins_from_another_symbol_unit() {
        let source = r#"(kicad_sch
  (lib_symbols
    (symbol "Multi:Part"
      (symbol "Part_1_1"
        (pin passive line (at 0 0 0) (length 2.54) (name "1") (number "1")))
      (symbol "Part_2_1"
        (pin passive line (at 50 0 0) (length 2.54) (name "2") (number "2")))))
  (symbol (lib_id "Multi:Part") (at 10 10 0) (unit 1) (uuid "symbol-a"))
  (wire (pts (xy 60 10) (xy 80 10)) (uuid "wire-other-unit")))"#;
        let selected = HashSet::from(["symbol-a".to_owned()]);

        let connected = connected_wire_moves(source, &selected).unwrap();

        assert!(connected.is_empty());
    }

    #[test]
    fn concurrent_connected_wire_change_rejects_the_complete_move() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connected-conflict.kicad_sch");
        let source = r#"(kicad_sch
  (lib_symbols
    (symbol "Device:R"
      (symbol "R_1_1"
        (pin passive line (at 0 0 0) (length 2.54) (name "1") (number "1")))))
  (symbol (lib_id "Device:R") (at 10 10 0) (unit 1) (uuid "symbol-a"))
  (wire (pts (xy 10 10) (xy 30 10)) (uuid "wire-a")))"#;
        std::fs::write(&path, source).unwrap();
        let selected = HashSet::from(["symbol-a".to_owned()]);
        let (edited, changed) =
            move_items_with_connected_wires(source, &selected, 5.0, 0.0).unwrap();
        let ids = changed
            .into_iter()
            .map(konnect_sexp::ItemId::new)
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let command = konnect_sexp::SchematicCommand::replace_items_from_document(
            source,
            &edited,
            ids,
            "Connected move",
        )
        .unwrap();
        let external = source.replace("(xy 30 10)", "(xy 35 10)");
        std::fs::write(&path, &external).unwrap();

        let error = konnect_sexp::commit_command(&path, &command).unwrap_err();

        assert!(matches!(
            error,
            konnect_sexp::SexpError::ItemConflict { .. }
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), external);
    }

    #[test]
    fn moving_label_translates_anchor_without_changing_text() {
        let source = r#"(kicad_sch
  (label "PACK+" (at 10 20 90) (effects (font (size 1.27 1.27)))
    (uuid "label-a")))"#;

        let moved = move_item_source(source, "label-a", -1.27, 2.54).expect("label moves");

        assert!(moved.contains("(label \"PACK+\" (at 8.73 22.54 90)"));
    }

    #[test]
    fn rotating_symbol_changes_only_placed_rotation() {
        let source = r#"(kicad_sch
  (symbol (lib_id "Device:R") (at 10 20 270)
    (property "Reference" "R1" (at 8 20 0))
    (uuid "symbol-a"))
  (text "keep" (at 1 2 0) (uuid "text-b")))"#;

        let rotated = rotate_symbol_source(source, "symbol-a", 90.0).expect("symbol rotates");

        assert!(rotated.contains("(symbol (lib_id \"Device:R\") (at 10 20 0)"));
        assert!(rotated.contains("(property \"Reference\" \"R1\" (at 8 20 0))"));
        assert!(rotated.contains("(text \"keep\" (at 1 2 0)"));
    }

    #[test]
    fn rotating_bus_entry_changes_only_its_size_vector() {
        let source = r#"(kicad_sch
  (bus_entry (at 10 20) (size 2.54 2.54)
    (stroke (width 0) (type default)) (uuid "entry-a"))
  (wire (pts (xy 1 2) (xy 3 4)) (uuid "wire-b")))"#;

        let rotated = rotate_item_source(source, "entry-a").expect("bus entry rotates");

        assert!(rotated.contains("(bus_entry (at 10 20) (size -2.54 2.54)"));
        assert!(rotated.contains("(wire (pts (xy 1 2) (xy 3 4))"));
    }

    #[test]
    fn mirror_symbol_adds_changes_and_removes_axis_targetedly() {
        let source = r#"(kicad_sch
  (symbol (lib_id "Device:R") (at 10 20 0)
    (property "Reference" "R1" (at 8 20 0))
    (uuid "symbol-a")))"#;

        let mirrored_x = mirror_symbol_source(source, "symbol-a", "x").expect("mirror is added");
        assert!(mirrored_x.contains("(at 10 20 0)\n    (mirror x)"));
        let mirrored_y = mirror_symbol_source(&mirrored_x, "symbol-a", "y").expect("axis changes");
        assert!(mirrored_y.contains("(mirror y)"));
        assert!(!mirrored_y.contains("(mirror x)"));
        let unmirrored =
            mirror_symbol_source(&mirrored_y, "symbol-a", "y").expect("axis toggles off");
        assert!(!unmirrored.contains("(mirror"));
        assert!(unmirrored.contains("(property \"Reference\" \"R1\""));
    }

    #[test]
    fn duplicate_symbol_gets_fresh_uuid_unannotated_reference_and_offset() {
        let source = r#"(kicad_sch
  (symbol (lib_id "Device:R") (at 10 20 0)
    (property "Reference" "R17" (at 8 20 0))
    (property "Value" "10k" (at 12 20 0))
    (uuid "symbol-a")
    (instances (project "demo" (path "/root" (reference "R17") (unit 1))))))"#;

        let (duplicate, uuid) =
            duplicate_symbol_block(source, "symbol-a", 2.54, 1.27).expect("symbol duplicates");

        assert!(duplicate.contains("(at 12.54 21.27 0)"));
        assert!(duplicate.contains("(property \"Reference\" \"R?\""));
        assert!(duplicate.contains("(reference \"R?\")"));
        assert!(duplicate.contains("(property \"Value\" \"10k\""));
        assert!(duplicate.contains(&format!("(uuid \"{uuid}\")")));
        assert_ne!(uuid, "symbol-a");
    }

    #[test]
    fn cached_svg_ranks_first_and_overlap_occurrences_independently() {
        let svg = "<desc>R2</desc><desc>R1</desc><desc>R1</desc><desc>R2</desc>";
        let first = svg_reference_ranks(svg, ["R1", "R2"], 0).unwrap();
        let redraw = svg_reference_ranks(svg, ["R1", "R2"], 1).unwrap();

        assert!(first["R2"] < first["R1"]);
        assert!(redraw["R1"] < redraw["R2"]);
    }
}
