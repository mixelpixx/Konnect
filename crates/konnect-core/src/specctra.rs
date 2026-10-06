//! Fail-closed KiCad PCB snapshot to Specctra DSN lowering.
//!
//! The input is the exact string returned by KiCad's IPC
//! `SaveDocumentToString`; this module never reads or edits the live board
//! file. The first supported profile is intentionally narrow and rejects
//! every feature it cannot represent without approximation.

use anyhow::{bail, Context, Result};
use konnect_ipc::IpcEffectiveRoutingRules;
use konnect_sexp::{parse_sexp, SexpNode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specctra::read::{ListTokenizer, ReadDsn};
use specctra::structure as dsn;
use specctra::write::{ListWriter, WriteSes};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Cursor};
use std::path::Path;

const UM_PER_MM: f64 = 1_000.0;
const DSN_RESOLUTION: f32 = 10.0;

#[derive(Debug, Clone)]
pub(crate) struct ExportBundle {
    pub dsn: String,
    pub manifest: String,
    pub source_sha256: String,
    pub component_count: usize,
    pub pad_count: usize,
    pub net_count: usize,
    pub class_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PadShape {
    Circle,
    Rect,
    #[serde(rename = "roundrect")]
    RoundRect,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PadstackKey {
    shape: PadShape,
    layers: Vec<String>,
    size_x_um: i64,
    size_y_um: i64,
    corner_radius_um: Option<i64>,
    drill_um: Option<i64>,
}

#[derive(Debug, Clone)]
struct PadModel {
    number: String,
    x_um: i64,
    y_um: i64,
    rotation_degrees: f64,
    net: Option<String>,
    padstack: PadstackKey,
}

#[derive(Debug, Clone)]
struct FootprintModel {
    reference: String,
    kiid: String,
    image_name: String,
    x_um: i64,
    y_um: i64,
    rotation_degrees: f64,
    pads: Vec<PadModel>,
}

#[derive(Debug, Clone)]
struct LockedTrackModel {
    start_x_um: i64,
    start_y_um: i64,
    end_x_um: i64,
    end_y_um: i64,
    width_um: i64,
    layer: String,
    net: String,
}

#[derive(Debug, Clone)]
struct LockedViaModel {
    x_um: i64,
    y_um: i64,
    diameter_um: i64,
    drill_um: i64,
    net: String,
}

#[derive(Debug, Clone, Default)]
struct LockedRouting {
    tracks: Vec<LockedTrackModel>,
    vias: Vec<LockedViaModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RuleKey {
    track_width_um: i64,
    clearance_um: i64,
    via_diameter_um: i64,
    via_drill_um: i64,
}

type RuleGroups = BTreeMap<RuleKey, Vec<String>>;
type NetClassNames = BTreeMap<String, String>;

#[derive(Debug, Deserialize, Serialize)]
struct Manifest {
    schema_version: u32,
    board_path: String,
    source_sha256: String,
    coordinate_unit: String,
    resolution: u32,
    supported_profile: SupportedProfile,
    layers: Vec<ManifestLayer>,
    components: Vec<ManifestComponent>,
    nets: Vec<ManifestNet>,
    padstacks: Vec<ManifestPadstack>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SupportedProfile {
    copper_layers: u32,
    component_side: String,
    pad_shapes: Vec<String>,
    existing_routing: bool,
    #[serde(default)]
    locked_track_count: usize,
    #[serde(default)]
    locked_via_count: usize,
    copper_zones: bool,
    custom_rules: bool,
    outline: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ManifestLayer {
    kicad_name: String,
    dsn_name: String,
    index: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct ManifestComponent {
    reference: String,
    kiid: String,
    image_name: String,
    x_um: i64,
    y_um: i64,
    rotation_degrees: f64,
    side: String,
    pads: Vec<ManifestPin>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ManifestPin {
    pad_number: String,
    dsn_pin: String,
    net: Option<String>,
    padstack_name: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ManifestNet {
    name: String,
    pins: Vec<String>,
    class_name: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ManifestPadstack {
    name: String,
    purpose: String,
    shape: PadShape,
    layers: Vec<String>,
    size_x_um: i64,
    size_y_um: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    corner_radius_um: Option<i64>,
    drill_um: Option<i64>,
}

pub(crate) fn export_dsn(
    board_path: &Path,
    board_source: &str,
    effective_rules: &IpcEffectiveRoutingRules,
) -> Result<ExportBundle> {
    let tree = parse_sexp(board_source).context("parse KiCad IPC board snapshot")?;
    if tree.head() != Some("kicad_pcb") {
        bail!("KiCad IPC snapshot root is not 'kicad_pcb'");
    }

    let copper_layers = copper_layers(&tree)?;
    let outline = simple_closed_outline(&tree)?;
    let net_table = top_level_net_table(&tree);
    let locked_routing = locked_routing(&tree, &net_table, &copper_layers)?;
    let footprints = footprints(&tree, &net_table)?;
    if footprints.is_empty() {
        bail!("supported routing profile requires at least one footprint");
    }

    let mut net_pins: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for footprint in &footprints {
        for pad in &footprint.pads {
            if let Some(net) = &pad.net {
                net_pins
                    .entry(net.clone())
                    .or_default()
                    .push(format!("{}-{}", footprint.reference, pad.number));
            }
        }
    }
    if net_pins.is_empty() {
        bail!("supported routing profile requires at least one connected pad");
    }
    for pins in net_pins.values_mut() {
        pins.sort();
        pins.dedup();
    }

    let (class_nets, net_classes) = normalize_rules(&net_pins, effective_rules)?;

    let mut padstack_keys = BTreeSet::new();
    for footprint in &footprints {
        for pad in &footprint.pads {
            padstack_keys.insert(pad.padstack.clone());
        }
    }
    let padstack_names: BTreeMap<PadstackKey, String> = padstack_keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| (key, format!("konnect_pad_{:04}", index + 1)))
        .collect();

    let via_keys: BTreeSet<RuleKey> = class_nets.keys().cloned().collect();
    let via_names: BTreeMap<RuleKey, String> = via_keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| (key, format!("konnect_via_{:04}", index + 1)))
        .collect();

    let pcb = build_pcb(
        board_path,
        &copper_layers,
        &outline,
        &footprints,
        &net_pins,
        &class_nets,
        &net_classes,
        &padstack_names,
        &via_names,
        &locked_routing,
    )?;
    let dsn = serialize_and_validate(pcb)?;
    let source_sha256 = sha256_hex(board_source.as_bytes());
    let manifest = build_manifest(
        board_path,
        &source_sha256,
        &copper_layers,
        &footprints,
        &net_pins,
        &net_classes,
        &padstack_names,
        &via_names,
        &locked_routing,
    )?;

    Ok(ExportBundle {
        dsn,
        manifest,
        source_sha256,
        component_count: footprints.len(),
        pad_count: footprints
            .iter()
            .map(|footprint| footprint.pads.len())
            .sum(),
        net_count: net_pins.len(),
        class_count: class_nets.len(),
    })
}

/// Replace Konnect's deterministic DSN with a KiCad-native DSN while keeping
/// the same fail-closed board profile and rewriting the reverse manifest to
/// the exact identifiers KiCad emitted. This lets the strict SES importer
/// remain authoritative instead of trusting a second, looser return path.
pub(crate) fn adopt_native_dsn(
    mut baseline: ExportBundle,
    native_dsn: String,
) -> Result<ExportBundle> {
    validate_dsn_syntax(&native_dsn, "KiCad native DSN")?;
    let baseline_tree = parse_dsn_sexp(&baseline.dsn).context("parse Konnect baseline DSN")?;
    let native_tree = parse_dsn_sexp(&native_dsn).context("parse KiCad native DSN")?;
    let baseline_identity = dsn_identity(&baseline_tree)?;
    let native_identity = dsn_identity(&native_tree)?;
    let mut manifest: Manifest =
        serde_json::from_str(&baseline.manifest).context("parse baseline routing manifest")?;

    let layer_names = correlate_layers(&baseline_identity, &native_identity)?;
    let (class_names, via_names) = correlate_classes(&baseline_identity, &native_identity)?;
    let roundrect_models = manifest
        .padstacks
        .iter()
        .filter(|padstack| padstack.shape == PadShape::RoundRect)
        .map(|padstack| (padstack.name.as_str(), padstack))
        .collect();
    let padstack_names =
        correlate_components(&baseline_identity, &native_identity, &roundrect_models)?;
    validate_native_nets(&baseline_identity, &native_identity)?;

    for layer in &mut manifest.layers {
        layer.dsn_name = layer_names
            .get(&layer.dsn_name)
            .with_context(|| format!("native DSN omitted layer '{}'", layer.dsn_name))?
            .clone();
    }
    for component in &mut manifest.components {
        let native = native_identity
            .components
            .get(&component.reference)
            .with_context(|| format!("native DSN omitted component '{}'", component.reference))?;
        component.image_name = native.image_name.clone();
        for pin in &mut component.pads {
            pin.padstack_name = padstack_names
                .get(&pin.padstack_name)
                .with_context(|| {
                    format!(
                        "native DSN omitted padstack mapping for {}-{}",
                        component.reference, pin.pad_number
                    )
                })?
                .clone();
        }
    }
    for net in &mut manifest.nets {
        net.class_name = class_names
            .get(&net.class_name)
            .with_context(|| format!("native DSN omitted class mapping for '{}'", net.name))?
            .clone();
    }
    for padstack in &mut manifest.padstacks {
        let mapping = if padstack.purpose == "via" {
            &via_names
        } else {
            &padstack_names
        };
        padstack.name = mapping
            .get(&padstack.name)
            .with_context(|| format!("native DSN omitted padstack '{}'", padstack.name))?
            .clone();
    }
    manifest
        .padstacks
        .sort_by(|left, right| left.name.cmp(&right.name));
    baseline.manifest =
        serde_json::to_string_pretty(&manifest).context("serialize native routing manifest")?;
    baseline.dsn = native_dsn;
    Ok(baseline)
}

#[derive(Debug, Clone)]
struct DsnPlacementIdentity {
    image_name: String,
    x: i64,
    y: i64,
    side: String,
    rotation: OrderedF64,
    pins: BTreeMap<String, DsnPinIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DsnPinIdentity {
    padstack: String,
    x: i64,
    y: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OrderedF64(u64);

impl OrderedF64 {
    fn new(value: f64) -> Result<Self> {
        if !value.is_finite() {
            bail!("DSN contains a non-finite number");
        }
        Ok(Self(value.to_bits()))
    }

    fn value(self) -> f64 {
        f64::from_bits(self.0)
    }
}

#[derive(Debug, Clone)]
struct DsnClassIdentity {
    nets: BTreeSet<String>,
    via_name: String,
    width: i64,
    clearance: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DsnShapeIdentity {
    Circle {
        layer: String,
        diameter: i64,
    },
    Rect {
        layer: String,
        x1: i64,
        y1: i64,
        x2: i64,
        y2: i64,
    },
    Polygon {
        layer: String,
        points: Vec<(OrderedF64, OrderedF64)>,
    },
}

#[derive(Debug, Clone)]
struct DsnIdentity {
    layers: BTreeMap<usize, String>,
    components: BTreeMap<String, DsnPlacementIdentity>,
    nets: BTreeMap<String, BTreeSet<String>>,
    classes: BTreeMap<String, DsnClassIdentity>,
    padstacks: BTreeMap<String, Vec<DsnShapeIdentity>>,
}

fn validate_dsn_syntax(source: &str, label: &str) -> Result<()> {
    let cursor = Cursor::new(source.as_bytes());
    let mut tokenizer = ListTokenizer::new(BufReader::new(cursor));
    dsn::DsnFile::read_dsn(&mut tokenizer)
        .map_err(|error| anyhow::anyhow!("{label} failed Specctra parser validation: {error}"))?;
    Ok(())
}

fn parse_dsn_sexp(source: &str) -> Result<SexpNode> {
    // Specctra represents the quote delimiter as the bare token
    // `(string_quote ")`. KiCad's S-expression parser correctly treats the
    // opening quote as a string delimiter, so give only this metadata token an
    // escaped representation for structural inspection. The original DSN is
    // retained byte-for-byte and independently validated by Topola's parser.
    let compatible = source.replace("(string_quote \")", "(string_quote \"\\\"\")");
    parse_sexp(&compatible).context("parse normalized Specctra S-expression")
}

fn dsn_identity(root: &SexpNode) -> Result<DsnIdentity> {
    if root.head() != Some("pcb") {
        bail!("Specctra export root is not 'pcb'");
    }
    let structure = root.find("structure").context("DSN has no structure")?;
    let mut layers = BTreeMap::new();
    for layer in structure.find_all("layer") {
        let name = dsn_atom(layer, 1, "layer name")?.to_string();
        let index = layer
            .find("property")
            .and_then(|property| property.find("index"))
            .and_then(|index| index.get(1))
            .and_then(SexpNode::as_str)
            .context("DSN layer has no index")?
            .parse::<usize>()
            .context("DSN layer index is not an integer")?;
        if layers.insert(index, name).is_some() {
            bail!("DSN repeats a layer index");
        }
    }

    let library = root.find("library").context("DSN has no library")?;
    let mut image_pins = BTreeMap::<String, BTreeMap<String, DsnPinIdentity>>::new();
    for image in library.find_all("image") {
        let name = dsn_atom(image, 1, "image name")?.to_string();
        let mut pins = BTreeMap::new();
        for pin in image.find_all("pin") {
            let padstack = dsn_atom(pin, 1, "pin padstack")?.to_string();
            let number = dsn_atom(pin, 2, "pin number")?.to_string();
            let identity = DsnPinIdentity {
                padstack,
                x: dsn_integer(pin, 3, "pin x")?,
                y: dsn_integer(pin, 4, "pin y")?,
            };
            if pins.insert(number, identity).is_some() {
                bail!("DSN image '{name}' repeats a pin number");
            }
        }
        if image_pins.insert(name.clone(), pins).is_some() {
            bail!("DSN repeats image '{name}'");
        }
    }
    let mut padstacks = BTreeMap::new();
    for padstack in library.find_all("padstack") {
        let name = dsn_atom(padstack, 1, "padstack name")?.to_string();
        let shapes = padstack
            .find_all("shape")
            .into_iter()
            .map(|shape| {
                let geometry = shape.get(1).context("DSN shape has no geometry")?;
                match geometry.head() {
                    Some("circle") => Ok(DsnShapeIdentity::Circle {
                        layer: dsn_atom(geometry, 1, "circle layer")?.to_string(),
                        diameter: dsn_integer(geometry, 2, "circle diameter")?,
                    }),
                    Some("rect") => Ok(DsnShapeIdentity::Rect {
                        layer: dsn_atom(geometry, 1, "rect layer")?.to_string(),
                        x1: dsn_integer(geometry, 2, "rect x1")?,
                        y1: dsn_integer(geometry, 3, "rect y1")?,
                        x2: dsn_integer(geometry, 4, "rect x2")?,
                        y2: dsn_integer(geometry, 5, "rect y2")?,
                    }),
                    Some("polygon") => dsn_polygon(geometry),
                    other => bail!("DSN padstack has unsupported shape {other:?}"),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        if shapes.is_empty() || padstacks.insert(name.clone(), shapes).is_some() {
            bail!("DSN contains an empty or duplicate padstack '{name}'");
        }
    }

    let placement = root.find("placement").context("DSN has no placement")?;
    let mut components = BTreeMap::new();
    for component in placement.find_all("component") {
        let image_name = dsn_atom(component, 1, "component image")?.to_string();
        let pins = image_pins
            .get(&image_name)
            .with_context(|| format!("DSN placement uses unknown image '{image_name}'"))?
            .clone();
        for place in component.find_all("place") {
            let reference = dsn_atom(place, 1, "place reference")?.to_string();
            let x = dsn_integer(place, 2, "place x")?;
            let y = dsn_integer(place, 3, "place y")?;
            let side = dsn_atom(place, 4, "place side")?.to_string();
            let rotation = OrderedF64::new(dsn_number(place, 5, "place rotation")?)?;
            let identity = DsnPlacementIdentity {
                image_name: image_name.clone(),
                x,
                y,
                side,
                rotation,
                pins: pins.clone(),
            };
            if components.insert(reference.clone(), identity).is_some() {
                bail!("DSN repeats component '{reference}'");
            }
        }
    }

    let network = root.find("network").context("DSN has no network")?;
    let mut nets = BTreeMap::new();
    for net in network.find_all("net") {
        let name = dsn_atom(net, 1, "net name")?.to_string();
        let pins = net
            .find("pins")
            .context("DSN net has no pins")?
            .children()
            .context("DSN pins is not a list")?
            .iter()
            .skip(1)
            .map(|pin| {
                pin.as_str()
                    .map(str::to_string)
                    .context("DSN pins contains a list")
            })
            .collect::<Result<BTreeSet<_>>>()?;
        if pins.is_empty() || nets.insert(name.clone(), pins).is_some() {
            bail!("DSN contains an empty or duplicate net '{name}'");
        }
    }
    let mut classes = BTreeMap::new();
    for class in network.find_all("class") {
        let name = dsn_atom(class, 1, "class name")?.to_string();
        let children = class.children().context("DSN class is not a list")?;
        let nets = children
            .iter()
            .skip(2)
            .take_while(|child| child.as_str().is_some())
            .map(|child| child.as_str().unwrap().to_string())
            .collect::<BTreeSet<_>>();
        let via_name = class
            .find("circuit")
            .and_then(|circuit| circuit.find("use_via"))
            .and_then(|via| via.get(1))
            .and_then(SexpNode::as_str)
            .context("DSN class has no use_via")?
            .to_string();
        let rule = class.find("rule").context("DSN class has no rule")?;
        let width = rule.find("width").context("DSN class has no width")?;
        let clearance = rule
            .find("clearance")
            .context("DSN class has no clearance")?;
        let width = dsn_integer(width, 1, "class width")?;
        let clearance = dsn_integer(clearance, 1, "class clearance")?;
        if nets.is_empty()
            || !nets.iter().all(|net| {
                network
                    .find_all("net")
                    .iter()
                    .any(|node| node.get(1).and_then(SexpNode::as_str) == Some(net))
            })
            || classes
                .insert(
                    name.clone(),
                    DsnClassIdentity {
                        nets,
                        via_name,
                        width,
                        clearance,
                    },
                )
                .is_some()
        {
            bail!("DSN contains an invalid or duplicate class '{name}'");
        }
    }
    Ok(DsnIdentity {
        layers,
        components,
        nets,
        classes,
        padstacks,
    })
}

fn correlate_layers(
    baseline: &DsnIdentity,
    native: &DsnIdentity,
) -> Result<BTreeMap<String, String>> {
    if baseline.layers.len() != native.layers.len() {
        bail!("native DSN changed the copper-layer count");
    }
    let mappings = baseline
        .layers
        .iter()
        .map(|(index, baseline_name)| {
            let native_name = native
                .layers
                .get(index)
                .with_context(|| format!("native DSN omitted layer index {index}"))?;
            Ok((baseline_name.clone(), native_name.clone()))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    if mappings.iter().any(|(baseline, native)| baseline != native) {
        bail!("native DSN renamed a KiCad copper layer");
    }
    Ok(mappings)
}

fn correlate_components(
    baseline: &DsnIdentity,
    native: &DsnIdentity,
    roundrect_models: &BTreeMap<&str, &ManifestPadstack>,
) -> Result<BTreeMap<String, String>> {
    if baseline.components.len() != native.components.len() {
        bail!("native DSN changed the component count");
    }
    let mut padstacks = BTreeMap::new();
    for (reference, baseline_component) in &baseline.components {
        let native_component = native
            .components
            .get(reference)
            .with_context(|| format!("native DSN omitted component '{reference}'"))?;
        if baseline_component.x != native_component.x
            || baseline_component.y != native_component.y
            || baseline_component.side != native_component.side
            || baseline_component.rotation != native_component.rotation
            || baseline_component.pins.keys().collect::<Vec<_>>()
                != native_component.pins.keys().collect::<Vec<_>>()
        {
            bail!("native DSN changed placement or pins for component '{reference}'");
        }
        for (pin, baseline_pin) in &baseline_component.pins {
            let native_pin = native_component.pins.get(pin).unwrap();
            if baseline_pin.x != native_pin.x || baseline_pin.y != native_pin.y {
                bail!("native DSN changed pin position for component '{reference}' pin '{pin}'");
            }
            let baseline_shape = baseline
                .padstacks
                .get(&baseline_pin.padstack)
                .with_context(|| {
                    format!("baseline DSN omitted padstack '{}'", baseline_pin.padstack)
                })?;
            let native_shape = native
                .padstacks
                .get(&native_pin.padstack)
                .with_context(|| {
                    format!("native DSN omitted padstack '{}'", native_pin.padstack)
                })?;
            // Each padstack pair is judged once; later pins only need the same pair.
            let already_matched =
                padstacks.get(&baseline_pin.padstack) == Some(&native_pin.padstack);
            if !already_matched
                && !pad_geometry_matches(
                    baseline_shape,
                    native_shape,
                    roundrect_models
                        .get(baseline_pin.padstack.as_str())
                        .copied(),
                )
            {
                bail!("native DSN changed pad geometry for component '{reference}' pin '{pin}'");
            }
            if padstacks
                .insert(baseline_pin.padstack.clone(), native_pin.padstack.clone())
                .is_some_and(|previous| previous != native_pin.padstack)
            {
                bail!("native DSN maps one pad geometry to inconsistent padstacks");
            }
        }
    }
    Ok(padstacks)
}

/// A rounded rectangle is judged against its pad; every other shape must be
/// the one Konnect wrote.
fn pad_geometry_matches(
    baseline: &[DsnShapeIdentity],
    native: &[DsnShapeIdentity],
    roundrect: Option<&ManifestPadstack>,
) -> bool {
    match roundrect {
        Some(model) => polygons_trace_roundrect(native, model),
        None => baseline == native,
    }
}

fn validate_native_nets(baseline: &DsnIdentity, native: &DsnIdentity) -> Result<()> {
    if baseline.nets != native.nets {
        bail!("native DSN changed net or pin membership");
    }
    Ok(())
}

fn correlate_classes(
    baseline: &DsnIdentity,
    native: &DsnIdentity,
) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    if baseline.classes.len() != native.classes.len() {
        bail!("native DSN changed the routing-class count");
    }
    let mut class_names = BTreeMap::new();
    let mut via_names = BTreeMap::new();
    for (baseline_name, baseline_class) in &baseline.classes {
        let matches = native
            .classes
            .iter()
            .filter(|(_, native_class)| native_class.nets == baseline_class.nets)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            bail!("native DSN has no unique class for '{baseline_name}'");
        }
        let (native_name, native_class) = matches[0];
        if baseline_class.width != native_class.width
            || baseline_class.clearance != native_class.clearance
        {
            bail!("native DSN changed width or clearance for class '{baseline_name}'");
        }
        let baseline_via = baseline
            .padstacks
            .get(&baseline_class.via_name)
            .with_context(|| format!("baseline DSN omitted via '{}'", baseline_class.via_name))?;
        let native_via = native
            .padstacks
            .get(&native_class.via_name)
            .with_context(|| format!("native DSN omitted via '{}'", native_class.via_name))?;
        if baseline_via != native_via {
            bail!("native DSN changed via geometry for class '{baseline_name}'");
        }
        class_names.insert(baseline_name.clone(), native_name.clone());
        if via_names
            .insert(
                baseline_class.via_name.clone(),
                native_class.via_name.clone(),
            )
            .is_some_and(|previous| previous != native_class.via_name)
        {
            bail!("native DSN maps one via rule to inconsistent padstacks");
        }
    }
    Ok((class_names, via_names))
}

fn dsn_polygon(geometry: &SexpNode) -> Result<DsnShapeIdentity> {
    let layer = dsn_atom(geometry, 1, "polygon layer")?.to_string();
    if dsn_number(geometry, 2, "polygon aperture width")? != 0.0 {
        bail!("DSN pad polygon has a non-zero aperture width");
    }
    let count = geometry.children().map_or(0, <[SexpNode]>::len);
    if count <= 3 {
        bail!("DSN pad polygon has no coordinates");
    }
    if !(count - 3).is_multiple_of(2) {
        bail!("DSN pad polygon has an odd number of coordinates");
    }
    let points = (3..count)
        .step_by(2)
        .map(|index| {
            Ok((
                OrderedF64::new(dsn_number(geometry, index, "polygon x")?)?,
                OrderedF64::new(dsn_number(geometry, index + 1, "polygon y")?)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    if points.len() < 4 || points.first() != points.last() {
        bail!("DSN pad polygon is not a closed outline");
    }
    Ok(DsnShapeIdentity::Polygon { layer, points })
}

/// Arc error KiCad allows by default (`max_error`, 0.005 mm). KiCad keeps its
/// polygon within this of the arc on the inside.
const KICAD_DEFAULT_MAX_ERROR_UM: f64 = 5.0;

/// Whether `shapes` draw the rounded rectangle `model` describes on each of its
/// layers. The two exporters approximate the corners differently, so a polygon
/// is judged against the pad, not against the other exporter's vertices.
fn polygons_trace_roundrect(shapes: &[DsnShapeIdentity], model: &ManifestPadstack) -> bool {
    let Some(radius_um) = model.corner_radius_um else {
        return false;
    };
    shapes.len() == model.layers.len()
        && shapes.iter().zip(&model.layers).all(|(shape, layer)| {
            matches!(shape, DsnShapeIdentity::Polygon { layer: shape_layer, points }
                if shape_layer == layer
                    && polygon_traces_roundrect(points, model.size_x_um, model.size_y_um, radius_um))
        })
}

fn polygon_traces_roundrect(
    points: &[(OrderedF64, OrderedF64)],
    size_x_um: i64,
    size_y_um: i64,
    radius_um: i64,
) -> bool {
    let radius = radius_um as f64;
    let (half_x, half_y) = (size_x_um as f64 / 2.0, size_y_um as f64 / 2.0);
    // KiCad grows the radius by r * (1 - cos(pi / 36)) and puts its vertices on
    // that larger arc, so its outline stands up to that far outside the pad.
    // Its chords sag inside by up to the arc error. The extra 1 um covers
    // `corner_radius_um` and the pad size being rounded to the micrometre.
    let outside = radius * (1.0 - (std::f64::consts::PI / 36.0).cos()) + 1.0;
    let inside = KICAD_DEFAULT_MAX_ERROR_UM + 1.0;
    // Signed distance from the rounded rectangle's outline, positive outside.
    let off_outline = |x: f64, y: f64| {
        let qx = x.abs() - (half_x - radius);
        let qy = y.abs() - (half_y - radius);
        let distance = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
        !(-inside..=outside).contains(&distance)
    };
    let (mut low_x, mut high_x, mut low_y, mut high_y) = (
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
    );
    for edge in points.windows(2) {
        let (start_x, start_y) = (edge[0].0.value(), edge[0].1.value());
        let (end_x, end_y) = (edge[1].0.value(), edge[1].1.value());
        if off_outline(start_x, start_y)
            || off_outline((start_x + end_x) / 2.0, (start_y + end_y) / 2.0)
        {
            return false;
        }
        (low_x, high_x) = (low_x.min(start_x), high_x.max(start_x));
        (low_y, high_y) = (low_y.min(start_y), high_y.max(start_y));
    }
    // Each side reaches the pad's edge: never more than rounding short of it.
    [
        -half_x - low_x,
        high_x - half_x,
        -half_y - low_y,
        high_y - half_y,
    ]
    .iter()
    .all(|reach| (-1.0..=outside).contains(reach))
}

fn dsn_atom<'a>(node: &'a SexpNode, index: usize, label: &str) -> Result<&'a str> {
    node.get(index)
        .and_then(SexpNode::as_str)
        .with_context(|| format!("DSN is missing {label}"))
}

fn dsn_number(node: &SexpNode, index: usize, label: &str) -> Result<f64> {
    let value = dsn_atom(node, index, label)?
        .parse::<f64>()
        .with_context(|| format!("DSN {label} is not numeric"))?;
    if !value.is_finite() {
        bail!("DSN {label} is not finite");
    }
    Ok(value)
}

fn dsn_integer(node: &SexpNode, index: usize, label: &str) -> Result<i64> {
    let value = dsn_number(node, index, label)?;
    if value.fract() != 0.0 || value < i64::MIN as f64 || value > i64::MAX as f64 {
        bail!("DSN {label} is not a supported integer");
    }
    Ok(value as i64)
}

fn locked_routing(
    tree: &SexpNode,
    net_table: &BTreeMap<String, String>,
    copper_layers: &[String],
) -> Result<LockedRouting> {
    let zone_count = tree.find_all("zone").len();
    if zone_count > 0 {
        bail!(
            "unsupported first routing profile: board contains {zone_count} copper zone or rule area(s)"
        );
    }
    let arc_count = tree.find_all("arc").len();
    if arc_count > 0 {
        let locked = tree
            .find_all("arc")
            .iter()
            .filter(|arc| arc.find_str("locked") == Some("yes"))
            .count();
        if locked > 0 {
            bail!(
                "unsupported first routing profile: board contains {locked} locked routed arc(s), which cannot be represented without approximation"
            );
        }
        bail!(
            "unsupported first routing profile: board contains {arc_count} existing routed arc(s)"
        );
    }

    let mut routing = LockedRouting::default();
    for segment in tree.find_all("segment") {
        if segment.find_str("locked") != Some("yes") {
            bail!("unsupported first routing profile: board contains an existing track segment that is not locked");
        }
        let layer = segment
            .find_str("layer")
            .context("locked track segment has no layer")?
            .to_string();
        if !copper_layers.contains(&layer) {
            bail!("locked track segment uses unsupported layer '{layer}'");
        }
        let net = segment
            .find("net")
            .and_then(|node| resolve_net(node, net_table))
            .context("locked track segment has no connected net")?;
        let (start_x_um, start_y_um) = point_um(segment, "start")?;
        let (end_x_um, end_y_um) = point_um(segment, "end")?;
        if start_x_um == end_x_um && start_y_um == end_y_um {
            bail!("locked track segment has zero length");
        }
        routing.tracks.push(LockedTrackModel {
            start_x_um,
            start_y_um,
            end_x_um,
            end_y_um,
            width_um: positive_um(
                segment.find("width").and_then(|width| width.get_f64(1)),
                "locked track width",
            )?,
            layer,
            net,
        });
    }
    for via in tree.find_all("via") {
        if via.find_str("locked") != Some("yes") {
            bail!("unsupported first routing profile: board contains an existing via that is not locked");
        }
        if let Some(kind) = via.find_str("type") {
            bail!("unsupported locked via type '{kind}'; only through vias are supported");
        }
        let layers = via
            .find("layers")
            .and_then(SexpNode::children)
            .unwrap_or(&[])
            .iter()
            .skip(1)
            .filter_map(SexpNode::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        if layers != copper_layers {
            bail!(
                "unsupported locked via layer span [{}]; expected [{}]",
                layers.join(", "),
                copper_layers.join(", ")
            );
        }
        let net = via
            .find("net")
            .and_then(|node| resolve_net(node, net_table))
            .context("locked via has no connected net")?;
        let (x_um, y_um) = point_um(via, "at")?;
        let diameter_um = positive_um(
            via.find("size").and_then(|size| size.get_f64(1)),
            "locked via diameter",
        )?;
        let drill = via.find("drill").context("locked via has no drill")?;
        if drill.get(1).and_then(SexpNode::as_str) == Some("oval") {
            bail!("unsupported locked via with oval drill");
        }
        let drill_um = positive_um(drill.get_f64(1), "locked via drill")?;
        if drill_um >= diameter_um {
            bail!("locked via drill is not smaller than its diameter");
        }
        routing.vias.push(LockedViaModel {
            x_um,
            y_um,
            diameter_um,
            drill_um,
            net,
        });
    }
    routing.tracks.sort_by(|left, right| {
        (
            &left.net,
            &left.layer,
            left.start_x_um,
            left.start_y_um,
            left.end_x_um,
            left.end_y_um,
        )
            .cmp(&(
                &right.net,
                &right.layer,
                right.start_x_um,
                right.start_y_um,
                right.end_x_um,
                right.end_y_um,
            ))
    });
    routing.vias.sort_by(|left, right| {
        (&left.net, left.x_um, left.y_um).cmp(&(&right.net, right.x_um, right.y_um))
    });
    Ok(routing)
}

fn copper_layers(tree: &SexpNode) -> Result<Vec<String>> {
    let layers = tree.find("layers").context("board has no layers table")?;
    let mut copper = layers
        .children()
        .unwrap_or(&[])
        .iter()
        .skip(1)
        // Layer table rows are `(numeric-id "name" type)`, so the layer name
        // is the first data item even though the numeric id is the list head.
        .filter_map(|layer| layer.get(1).and_then(SexpNode::as_str))
        .filter(|name| name.ends_with(".Cu"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    copper.dedup();
    if copper != ["F.Cu".to_string(), "B.Cu".to_string()] {
        bail!(
            "unsupported first routing profile: expected exactly F.Cu and B.Cu, got [{}]",
            copper.join(", ")
        );
    }
    Ok(copper)
}

fn simple_closed_outline(tree: &SexpNode) -> Result<Vec<(i64, i64)>> {
    let mut edges = Vec::new();
    for node in tree.children().unwrap_or(&[]) {
        if node.find_str("layer") != Some("Edge.Cuts") {
            continue;
        }
        match node.head() {
            Some("gr_line") => {
                let start = point_um(node, "start").context("Edge.Cuts line has no start")?;
                let end = point_um(node, "end").context("Edge.Cuts line has no end")?;
                if start == end {
                    bail!("unsupported outline: zero-length Edge.Cuts line");
                }
                edges.push((start, end));
            }
            Some(tag) if tag.starts_with("gr_") => {
                bail!("unsupported first routing profile: Edge.Cuts '{tag}' is not a straight line")
            }
            _ => {}
        }
    }
    if edges.len() < 3 {
        bail!("unsupported outline: need at least three Edge.Cuts lines");
    }

    let mut adjacency: BTreeMap<(i64, i64), Vec<(i64, i64)>> = BTreeMap::new();
    for (start, end) in &edges {
        adjacency.entry(*start).or_default().push(*end);
        adjacency.entry(*end).or_default().push(*start);
    }
    for (point, neighbours) in &mut adjacency {
        neighbours.sort();
        neighbours.dedup();
        if neighbours.len() != 2 {
            bail!(
                "unsupported outline: vertex ({}, {}) has degree {}, expected 2",
                point.0,
                point.1,
                neighbours.len()
            );
        }
    }

    let start = *adjacency.keys().next().context("outline has no vertices")?;
    let mut ordered = vec![start];
    let mut previous = None;
    let mut current = start;
    loop {
        let neighbours = &adjacency[&current];
        let next = match previous {
            None => neighbours[0],
            Some(previous) if neighbours[0] == previous => neighbours[1],
            Some(_) => neighbours[0],
        };
        if next == start {
            ordered.push(start);
            break;
        }
        if ordered.contains(&next) {
            bail!("unsupported outline: Edge.Cuts contains more than one loop");
        }
        ordered.push(next);
        previous = Some(current);
        current = next;
    }
    if ordered.len() != edges.len() + 1 {
        bail!("unsupported outline: Edge.Cuts is not one closed loop");
    }
    Ok(ordered)
}

fn footprints(
    tree: &SexpNode,
    net_table: &BTreeMap<String, String>,
) -> Result<Vec<FootprintModel>> {
    let mut output = Vec::new();
    let mut references = BTreeSet::new();
    for footprint in tree.find_all("footprint") {
        let layer = footprint
            .find_str("layer")
            .context("footprint has no layer")?;
        if layer != "F.Cu" {
            bail!("unsupported first routing profile: footprint on '{layer}'");
        }
        if footprint.find("clearance").is_some() {
            bail!("unsupported first routing profile: footprint has a local clearance override");
        }
        let reference = property_value(footprint, "Reference")
            .filter(|value| !value.is_empty())
            .context("footprint has no Reference property")?
            .to_string();
        if !references.insert(reference.clone()) {
            bail!("duplicate footprint reference '{reference}'");
        }
        let kiid = footprint
            .find_str("uuid")
            .context("footprint has no UUID")?
            .to_string();
        let at = footprint.find("at").context("footprint has no position")?;
        let x_um = finite_um(at.get_f64(1), "footprint x")?;
        let y_um = -finite_um(at.get_f64(2), "footprint y")?;
        let rotation_degrees = finite_number(at.get_f64(3).or(Some(0.0)), "footprint rotation")?;
        let image_name = format!("konnect_image_{reference}");

        let mut pads = Vec::new();
        let mut pad_numbers = BTreeSet::new();
        for pad in footprint.find_all("pad") {
            let number = pad
                .get(1)
                .and_then(SexpNode::as_str)
                .filter(|number| !number.is_empty())
                .context("footprint contains an unnumbered pad")?
                .to_string();
            if !pad_numbers.insert(number.clone()) {
                bail!("footprint '{reference}' has duplicate pad number '{number}'");
            }
            let pad_type = pad
                .get(2)
                .and_then(SexpNode::as_str)
                .context("pad has no type")?;
            if !matches!(pad_type, "smd" | "thru_hole") {
                bail!(
                    "unsupported first routing profile: pad {reference}-{number} has type '{pad_type}'"
                );
            }
            let shape = match pad.get(3).and_then(SexpNode::as_str) {
                Some("circle") => PadShape::Circle,
                Some("rect") => PadShape::Rect,
                Some("roundrect") => PadShape::RoundRect,
                Some(other) => bail!(
                    "unsupported first routing profile: pad {reference}-{number} has shape '{other}'"
                ),
                None => bail!("pad {reference}-{number} has no shape"),
            };
            let at = pad.find("at").context("pad has no position")?;
            let x_um = finite_um(at.get_f64(1), "pad x")?;
            let y_um = -finite_um(at.get_f64(2), "pad y")?;
            let rotation_degrees = finite_number(at.get_f64(3).or(Some(0.0)), "pad rotation")?;
            let size = pad.find("size").context("pad has no size")?;
            let size_x_um = positive_um(size.get_f64(1), "pad width")?;
            let size_y_um = positive_um(size.get_f64(2), "pad height")?;
            if shape == PadShape::Circle && size_x_um != size_y_um {
                bail!("circle pad {reference}-{number} does not have equal X/Y size");
            }
            let corner_radius_um = (shape == PadShape::RoundRect)
                .then(|| {
                    roundrect_corner_radius_um(
                        pad,
                        size_x_um,
                        size_y_um,
                        &format!("{reference}-{number}"),
                    )
                })
                .transpose()?;
            if pad.find("clearance").is_some() {
                bail!(
                    "unsupported first routing profile: pad {reference}-{number} has a local clearance override"
                );
            }
            if pad.find_str("remove_unused_layers") == Some("yes") {
                bail!(
                    "unsupported first routing profile: pad {reference}-{number} removes copper on unused layers"
                );
            }
            if let Some(offset) = pad.find("offset") {
                let offset_x_um = finite_um(offset.get_f64(1), "pad offset x")?;
                let offset_y_um = finite_um(offset.get_f64(2), "pad offset y")?;
                if offset_x_um != 0 || offset_y_um != 0 {
                    bail!(
                        "unsupported first routing profile: pad {reference}-{number} has a non-zero shape offset"
                    );
                }
            }
            let layers = pad
                .find("layers")
                .and_then(SexpNode::children)
                .unwrap_or(&[])
                .iter()
                .skip(1)
                .filter_map(SexpNode::as_str)
                .filter(|layer| layer.ends_with(".Cu") || *layer == "*.Cu")
                .map(str::to_string)
                .collect::<Vec<_>>();
            let (layers, drill_um) = if pad_type == "smd" {
                if layers != ["F.Cu".to_string()] {
                    bail!(
                        "unsupported SMD pad {reference}-{number}: copper layers are [{}]",
                        layers.join(", ")
                    );
                }
                (layers, None)
            } else {
                if !(layers == ["*.Cu".to_string()]
                    || layers == ["F.Cu".to_string(), "B.Cu".to_string()])
                {
                    bail!(
                        "unsupported through-hole pad {reference}-{number}: copper layers are [{}]",
                        layers.join(", ")
                    );
                }
                let drill = pad.find("drill").context("through-hole pad has no drill")?;
                if drill.get(1).and_then(SexpNode::as_str) == Some("oval") {
                    bail!("unsupported through-hole pad {reference}-{number}: oval drill");
                }
                let drill_um = positive_um(drill.get_f64(1), "pad drill")?;
                if drill_um >= size_x_um.min(size_y_um) {
                    bail!(
                        "pad {reference}-{number} drill {drill_um} um is not smaller than its copper size"
                    );
                }
                (vec!["F.Cu".to_string(), "B.Cu".to_string()], Some(drill_um))
            };
            let net = pad
                .find("net")
                .and_then(|node| resolve_net(node, net_table));
            pads.push(PadModel {
                number,
                x_um,
                y_um,
                rotation_degrees,
                net,
                padstack: PadstackKey {
                    shape,
                    layers,
                    size_x_um,
                    size_y_um,
                    corner_radius_um,
                    drill_um,
                },
            });
        }
        if pads.is_empty() {
            bail!("footprint '{reference}' has no pads");
        }
        pads.sort_by(|left, right| left.number.cmp(&right.number));
        output.push(FootprintModel {
            reference,
            kiid,
            image_name,
            x_um,
            y_um,
            rotation_degrees,
            pads,
        });
    }
    output.sort_by(|left, right| left.reference.cmp(&right.reference));
    Ok(output)
}

/// KiCad's corner radius, `min(width, height) * roundrect_rratio` rounded to
/// the nanometre, then down to the micrometre: a smaller radius only moves the
/// exported corner outward. KiCad writes a chamfered pad as `roundrect` too; its
/// cut corners are not represented, so it is refused.
fn roundrect_corner_radius_um(
    pad: &SexpNode,
    size_x_um: i64,
    size_y_um: i64,
    label: &str,
) -> Result<i64> {
    if pad
        .find("chamfer")
        .and_then(SexpNode::children)
        .is_some_and(|corners| corners.len() > 1)
    {
        bail!("unsupported first routing profile: pad {label} has chamfered corners");
    }
    let ratio = finite_number(
        pad.find("roundrect_rratio")
            .and_then(|node| node.get_f64(1)),
        "pad roundrect_rratio",
    )?;
    if !(0.0..=0.5).contains(&ratio) {
        bail!("pad {label} roundrect_rratio {ratio} is outside 0..=0.5");
    }
    let radius_nm = (size_x_um.min(size_y_um) as f64 * 1_000.0 * ratio).round() as i64;
    Ok(radius_nm / 1_000)
}

fn normalize_rules(
    net_pins: &BTreeMap<String, Vec<String>>,
    effective_rules: &IpcEffectiveRoutingRules,
) -> Result<(RuleGroups, NetClassNames)> {
    let mut class_nets: BTreeMap<RuleKey, Vec<String>> = BTreeMap::new();
    for net in net_pins.keys() {
        let rules = effective_rules.get(net).with_context(|| {
            format!("KiCad returned no effective routing rules for net '{net}'")
        })?;
        let key = RuleKey {
            track_width_um: positive_um(rules.track_width_mm, "track width")?,
            clearance_um: non_negative_um(rules.clearance_mm, "clearance")?,
            via_diameter_um: positive_um(rules.via_diameter_mm, "via diameter")?,
            via_drill_um: positive_um(rules.via_drill_mm, "via drill")?,
        };
        if key.via_drill_um >= key.via_diameter_um {
            bail!(
                "net '{net}' has via drill {} um not smaller than diameter {} um",
                key.via_drill_um,
                key.via_diameter_um
            );
        }
        class_nets.entry(key).or_default().push(net.clone());
    }
    for nets in class_nets.values_mut() {
        nets.sort();
    }
    let mut net_classes = BTreeMap::new();
    for (index, nets) in class_nets.values().enumerate() {
        let name = format!("konnect_class_{:04}", index + 1);
        for net in nets {
            net_classes.insert(net.clone(), name.clone());
        }
    }
    Ok((class_nets, net_classes))
}

#[allow(clippy::too_many_arguments)]
fn build_pcb(
    board_path: &Path,
    copper_layers: &[String],
    outline: &[(i64, i64)],
    footprints: &[FootprintModel],
    net_pins: &BTreeMap<String, Vec<String>>,
    class_nets: &BTreeMap<RuleKey, Vec<String>>,
    net_classes: &BTreeMap<String, String>,
    padstack_names: &BTreeMap<PadstackKey, String>,
    via_names: &BTreeMap<RuleKey, String>,
    locked_routing: &LockedRouting,
) -> Result<dsn::Pcb> {
    let default_rule = class_nets
        .keys()
        .next()
        .context("routing model has no rule class")?;
    let layers = copper_layers
        .iter()
        .enumerate()
        .map(|(index, name)| dsn::Layer {
            name: name.clone(),
            r#type: "signal".to_string(),
            property: Some(dsn::Property { index }),
        })
        .collect();
    let boundary = dsn::Boundary::Path(dsn::Path {
        layer: "pcb".to_string(),
        width: 0.0,
        coords: outline
            .iter()
            .map(|(x, y)| dsn::Point {
                x: *x as f64,
                y: *y as f64,
            })
            .collect(),
    });

    let components = footprints
        .iter()
        .map(|footprint| dsn::Component {
            name: footprint.image_name.clone(),
            places: vec![dsn::Place {
                name: footprint.reference.clone(),
                x: footprint.x_um as f64,
                y: footprint.y_um as f64,
                side: "front".to_string(),
                rotation: footprint.rotation_degrees,
                PN: None,
            }],
        })
        .collect();
    let images = footprints
        .iter()
        .map(|footprint| dsn::Image {
            name: footprint.image_name.clone(),
            outlines: Vec::new(),
            pins: footprint
                .pads
                .iter()
                .map(|pad| dsn::Pin {
                    name: padstack_names[&pad.padstack].clone(),
                    rotate: (pad.rotation_degrees != 0.0).then_some(pad.rotation_degrees),
                    id: pad.number.clone(),
                    x: pad.x_um as f64,
                    y: pad.y_um as f64,
                })
                .collect(),
            keepouts: dsn::Keepouts(Vec::new()),
        })
        .collect();

    let mut padstacks = padstack_names
        .iter()
        .map(|(key, name)| padstack(name, key))
        .collect::<Vec<_>>();
    padstacks.extend(via_names.iter().map(|(rule, name)| {
        dsn::Padstack {
            name: name.clone(),
            shapes: copper_layers
                .iter()
                .map(|layer| {
                    dsn::Shape::Circle(dsn::Circle {
                        layer: layer.clone(),
                        diameter: rule.via_diameter_um as f64,
                        offset: None,
                    })
                })
                .collect(),
            attach: Some(false),
        }
    }));
    padstacks.sort_by(|left, right| left.name.cmp(&right.name));

    let nets = net_pins
        .iter()
        .map(|(name, pins)| dsn::NetPinAssignments {
            name: name.clone(),
            pins: Some(dsn::Pins {
                names: pins.clone(),
            }),
        })
        .collect();
    let classes = class_nets
        .iter()
        .enumerate()
        .map(|(index, (rule, nets))| dsn::Class {
            name: format!("konnect_class_{:04}", index + 1),
            nets: nets.clone(),
            circuit: dsn::Circuit {
                use_via: via_names[rule].clone(),
            },
            rule: dsn::Rule {
                width: rule.track_width_um as f32,
                clearances: vec![dsn::Clearance {
                    value: rule.clearance_um as f32,
                    r#type: None,
                }],
            },
        })
        .collect();
    debug_assert_eq!(net_classes.len(), net_pins.len());

    let net_rules = class_nets
        .iter()
        .flat_map(|(rule, nets)| nets.iter().map(move |net| (net, rule)))
        .collect::<BTreeMap<_, _>>();
    let wires = locked_routing
        .tracks
        .iter()
        .map(|track| {
            if !net_rules.contains_key(&track.net) {
                bail!("locked track uses unknown routing net '{}'", track.net);
            }
            Ok(dsn::Wire {
                path: dsn::Path {
                    layer: track.layer.clone(),
                    width: track.width_um as f64,
                    coords: vec![
                        dsn::Point {
                            x: track.start_x_um as f64,
                            y: track.start_y_um as f64,
                        },
                        dsn::Point {
                            x: track.end_x_um as f64,
                            y: track.end_y_um as f64,
                        },
                    ],
                },
                net: track.net.clone(),
                r#type: "fix".to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let fixed_vias = locked_routing
        .vias
        .iter()
        .map(|via| {
            let rule = net_rules
                .get(&via.net)
                .with_context(|| format!("locked via uses unknown routing net '{}'", via.net))?;
            if via.diameter_um != rule.via_diameter_um || via.drill_um != rule.via_drill_um {
                bail!(
                    "locked via on net '{}' is {}:{} um, but its effective via rule is {}:{} um",
                    via.net,
                    via.diameter_um,
                    via.drill_um,
                    rule.via_diameter_um,
                    rule.via_drill_um
                );
            }
            Ok(dsn::Via {
                name: via_names[*rule].clone(),
                x: via.x_um as f64,
                y: via.y_um as f64,
                net: via.net.clone(),
                r#type: Some("fix".to_string()),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(dsn::Pcb {
        name: board_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("board.kicad_pcb")
            .to_string(),
        parser: Some(dsn::Parser {
            string_quote: Some('"'),
            space_in_quoted_tokens: Some(true),
            host_cad: Some("Konnect".to_string()),
            host_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
        resolution: dsn::Resolution {
            unit: "um".to_string(),
            value: DSN_RESOLUTION,
        },
        unit: Some("um".to_string()),
        structure: dsn::Structure {
            layers,
            boundary,
            place_boundary: None,
            planes: Vec::new(),
            keepouts: dsn::Keepouts(Vec::new()),
            via: dsn::ViaNames {
                names: via_names.values().cloned().collect(),
            },
            grids: Vec::new(),
            rules: vec![dsn::StructureRule {
                width: Some(default_rule.track_width_um as f32),
                clearances: vec![dsn::Clearance {
                    value: default_rule.clearance_um as f32,
                    r#type: None,
                }],
            }],
        },
        placement: dsn::Placement { components },
        library: dsn::Library { images, padstacks },
        network: dsn::Network { nets, classes },
        wiring: dsn::Wiring {
            wires,
            vias: fixed_vias,
        },
    })
}

fn padstack(name: &str, key: &PadstackKey) -> dsn::Padstack {
    let shapes = key
        .layers
        .iter()
        .map(|layer| match key.shape {
            PadShape::Circle => dsn::Shape::Circle(dsn::Circle {
                layer: layer.clone(),
                diameter: key.size_x_um as f64,
                offset: None,
            }),
            PadShape::Rect => dsn::Shape::Rect(dsn::Rect {
                layer: layer.clone(),
                x1: -(key.size_x_um as f64) / 2.0,
                y1: -(key.size_y_um as f64) / 2.0,
                x2: key.size_x_um as f64 / 2.0,
                y2: key.size_y_um as f64 / 2.0,
            }),
            // Specctra has no rounded rectangle, so KiCad writes a polygon too.
            PadShape::RoundRect => dsn::Shape::Polygon(dsn::Polygon {
                layer: layer.clone(),
                width: 0.0,
                coords: roundrect_outline(
                    key.size_x_um,
                    key.size_y_um,
                    key.corner_radius_um.unwrap_or(0),
                )
                .into_iter()
                .map(|(x, y)| dsn::Point { x, y })
                .collect(),
            }),
        })
        .collect();
    dsn::Padstack {
        name: name.to_string(),
        shapes,
        attach: Some(false),
    }
}

/// Furthest a rounded corner's straight edges may stand outside its arc.
const ROUNDRECT_MAX_ERROR_UM: f64 = 1.0;

/// A closed, counter-clockwise outline of a rounded rectangle centred on the
/// origin. Each corner's edges are tangent to its arc, so the outline encloses
/// it and stands at most `ROUNDRECT_MAX_ERROR_UM` outside, before rounding to
/// the DSN's 0.1 um resolution.
fn roundrect_outline(size_x_um: i64, size_y_um: i64, radius_um: i64) -> Vec<(f64, f64)> {
    use std::f64::consts::FRAC_PI_2;
    let radius = radius_um as f64;
    let half_x = size_x_um as f64 / 2.0 - radius;
    let half_y = size_y_um as f64 / 2.0 - radius;
    let segments = if radius_um == 0 {
        1
    } else {
        let max_half_step = (radius / (radius + ROUNDRECT_MAX_ERROR_UM)).acos();
        (FRAC_PI_2 / (2.0 * max_half_step)).ceil() as usize
    };
    let step = FRAC_PI_2 / segments as f64;
    let reach = radius / (step / 2.0).cos();
    let resolution = f64::from(DSN_RESOLUTION);
    // `+ 0.0` writes -0 as 0.
    let snap = |value: f64| (value * resolution).round() / resolution + 0.0;
    let mut points = Vec::new();
    for (quadrant, (centre_x, centre_y)) in [
        (half_x, half_y),
        (-half_x, half_y),
        (-half_x, -half_y),
        (half_x, -half_y),
    ]
    .into_iter()
    .enumerate()
    {
        for segment in 0..segments {
            let angle = quadrant as f64 * FRAC_PI_2 + (segment as f64 + 0.5) * step;
            points.push((
                snap(centre_x + reach * angle.cos()),
                snap(centre_y + reach * angle.sin()),
            ));
        }
    }
    points.dedup();
    points.push(points[0]);
    points
}

fn serialize_and_validate(pcb: dsn::Pcb) -> Result<String> {
    let file = dsn::DsnFile { pcb };
    let mut bytes = Vec::new();
    {
        let mut writer = ListWriter::new(&mut bytes);
        file.write_dsn(&mut writer)
            .context("serialize Specctra DSN")?;
    }
    let mut text = String::from_utf8(bytes).context("Specctra writer emitted non-UTF-8")?;
    if text.starts_with('\n') {
        text.remove(0);
    }
    text.push('\n');

    let cursor = Cursor::new(text.as_bytes());
    let mut tokenizer = ListTokenizer::new(BufReader::new(cursor));
    dsn::DsnFile::read_dsn(&mut tokenizer)
        .map_err(|error| anyhow::anyhow!("generated DSN failed parser round-trip: {error}"))?;
    Ok(text)
}

#[allow(clippy::too_many_arguments)]
fn build_manifest(
    board_path: &Path,
    source_sha256: &str,
    copper_layers: &[String],
    footprints: &[FootprintModel],
    net_pins: &BTreeMap<String, Vec<String>>,
    net_classes: &BTreeMap<String, String>,
    padstack_names: &BTreeMap<PadstackKey, String>,
    via_names: &BTreeMap<RuleKey, String>,
    locked_routing: &LockedRouting,
) -> Result<String> {
    let components = footprints
        .iter()
        .map(|footprint| ManifestComponent {
            reference: footprint.reference.clone(),
            kiid: footprint.kiid.clone(),
            image_name: footprint.image_name.clone(),
            x_um: footprint.x_um,
            y_um: footprint.y_um,
            rotation_degrees: footprint.rotation_degrees,
            side: "front".to_string(),
            pads: footprint
                .pads
                .iter()
                .map(|pad| ManifestPin {
                    pad_number: pad.number.clone(),
                    dsn_pin: format!("{}-{}", footprint.reference, pad.number),
                    net: pad.net.clone(),
                    padstack_name: padstack_names[&pad.padstack].clone(),
                })
                .collect(),
        })
        .collect();
    let nets = net_pins
        .iter()
        .map(|(name, pins)| ManifestNet {
            name: name.clone(),
            pins: pins.clone(),
            class_name: net_classes[name].clone(),
        })
        .collect();
    let mut padstacks = padstack_names
        .iter()
        .map(|(key, name)| ManifestPadstack {
            name: name.clone(),
            purpose: "pad".to_string(),
            shape: key.shape,
            layers: key.layers.clone(),
            size_x_um: key.size_x_um,
            size_y_um: key.size_y_um,
            corner_radius_um: key.corner_radius_um,
            drill_um: key.drill_um,
        })
        .collect::<Vec<_>>();
    padstacks.extend(via_names.iter().map(|(rule, name)| ManifestPadstack {
        name: name.clone(),
        purpose: "via".to_string(),
        shape: PadShape::Circle,
        layers: copper_layers.to_vec(),
        size_x_um: rule.via_diameter_um,
        size_y_um: rule.via_diameter_um,
        corner_radius_um: None,
        drill_um: Some(rule.via_drill_um),
    }));
    padstacks.sort_by(|left, right| left.name.cmp(&right.name));

    serde_json::to_string_pretty(&Manifest {
        schema_version: 1,
        board_path: board_path.display().to_string(),
        source_sha256: source_sha256.to_string(),
        coordinate_unit: "um".to_string(),
        resolution: DSN_RESOLUTION as u32,
        supported_profile: SupportedProfile {
            copper_layers: 2,
            component_side: "front".to_string(),
            pad_shapes: ["circle", "rect", "roundrect"].map(String::from).to_vec(),
            existing_routing: !locked_routing.tracks.is_empty() || !locked_routing.vias.is_empty(),
            locked_track_count: locked_routing.tracks.len(),
            locked_via_count: locked_routing.vias.len(),
            copper_zones: false,
            custom_rules: false,
            outline: "one closed loop of straight Edge.Cuts lines".to_string(),
        },
        layers: copper_layers
            .iter()
            .enumerate()
            .map(|(index, layer)| ManifestLayer {
                kicad_name: layer.clone(),
                dsn_name: layer.clone(),
                index,
            })
            .collect(),
        components,
        nets,
        padstacks,
    })
    .context("serialize routing manifest")
}

fn top_level_net_table(tree: &SexpNode) -> BTreeMap<String, String> {
    tree.find_all("net")
        .into_iter()
        .filter_map(|net| {
            let id = net.get(1)?.as_str()?;
            let name = net.get(2)?.as_str()?;
            Some((id.to_string(), name.to_string()))
        })
        .collect()
}

fn resolve_net(net: &SexpNode, table: &BTreeMap<String, String>) -> Option<String> {
    if let Some(name) = net.get(2).and_then(SexpNode::as_str) {
        return (!name.is_empty()).then(|| name.to_string());
    }
    let value = net.get(1)?.as_str()?;
    if let Some(name) = table.get(value).filter(|name| !name.is_empty()) {
        return Some(name.clone());
    }
    // KiCad 10 stores the net name directly as `(net "NAME")`. Older board
    // files used `(net id "NAME")`, handled by the branch above.
    (!value.is_empty()).then(|| value.to_string())
}

fn property_value<'a>(node: &'a SexpNode, property_name: &str) -> Option<&'a str> {
    node.find_all("property")
        .into_iter()
        .find(|property| property.get(1).and_then(SexpNode::as_str) == Some(property_name))?
        .get(2)?
        .as_str()
}

fn point_um(node: &SexpNode, tag: &str) -> Result<(i64, i64)> {
    let point = node.find(tag).with_context(|| format!("missing '{tag}'"))?;
    Ok((
        finite_um(point.get_f64(1), tag)?,
        -finite_um(point.get_f64(2), tag)?,
    ))
}

fn finite_number(value: Option<f64>, label: &str) -> Result<f64> {
    let value = value.with_context(|| format!("missing {label}"))?;
    if !value.is_finite() {
        bail!("{label} is not finite");
    }
    Ok(value)
}

fn finite_um(value: Option<f64>, label: &str) -> Result<i64> {
    let value = finite_number(value, label)? * UM_PER_MM;
    if value < i64::MIN as f64 || value > i64::MAX as f64 {
        bail!("{label} is outside the supported coordinate range");
    }
    Ok(value.round() as i64)
}

fn positive_um(value: Option<f64>, label: &str) -> Result<i64> {
    let value = finite_um(value, label)?;
    if value <= 0 {
        bail!("{label} must be greater than zero");
    }
    Ok(value)
}

fn non_negative_um(value: Option<f64>, label: &str) -> Result<i64> {
    let value = finite_um(value, label)?;
    if value < 0 {
        bail!("{label} must not be negative");
    }
    Ok(value)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_ipc::IpcRoutingRules;

    fn rules() -> IpcEffectiveRoutingRules {
        ["GND", "VCC"]
            .into_iter()
            .map(|net| {
                (
                    net.to_string(),
                    IpcRoutingRules {
                        class_name: "Default".to_string(),
                        constituents: vec!["Default".to_string()],
                        track_width_mm: Some(0.25),
                        clearance_mm: Some(0.2),
                        via_diameter_mm: Some(0.6),
                        via_drill_mm: Some(0.3),
                    },
                )
            })
            .collect()
    }

    fn native_fixture_rules() -> IpcEffectiveRoutingRules {
        let mut rules = rules();
        for rule in rules.values_mut() {
            rule.track_width_mm = Some(0.2);
        }
        rules
    }

    const STOCK_ROUNDRECT: &str =
        include_str!("../tests/fixtures/specctra_stock_roundrect.kicad_pcb");
    const STOCK_ROUNDRECT_NATIVE: &str =
        include_str!("../tests/fixtures/specctra_stock_roundrect.native-kicad-10.dsn");

    /// KiCad's default netclass, which every net of the stock fixture uses.
    fn stock_rules() -> IpcEffectiveRoutingRules {
        let mut rules = native_fixture_rules();
        let mut sig = rules["VCC"].clone();
        sig.class_name = "Default".to_string();
        rules.insert("SIG".to_string(), sig);
        rules
    }

    /// KiCad's export of the stock fixture without its courtyard
    /// `(outline (polygon ...))` lines, which the Specctra parser behind
    /// `validate_dsn_syntax` cannot read (#841). Pads and padstacks are
    /// KiCad's, unchanged.
    fn stock_roundrect_native() -> String {
        let native = STOCK_ROUNDRECT_NATIVE
            .lines()
            .filter(|line| !line.trim_start().starts_with("(outline (polygon "))
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        assert_eq!(
            STOCK_ROUNDRECT_NATIVE.lines().count() - native.lines().count(),
            3,
            "one courtyard polygon per image"
        );
        native
    }

    fn stock_export() -> ExportBundle {
        export_dsn(
            Path::new("board.kicad_pcb"),
            STOCK_ROUNDRECT,
            &stock_rules(),
        )
        .unwrap()
    }

    fn manifest_pads(manifest: &str) -> Vec<ManifestPadstack> {
        let manifest: Manifest = serde_json::from_str(manifest).unwrap();
        manifest
            .padstacks
            .into_iter()
            .filter(|padstack| padstack.purpose == "pad")
            .collect()
    }

    #[test]
    fn stock_roundrect_pads_export_as_polygons() {
        let export = stock_export();

        // Pad sizes and radii KiCad 10.0.6 named in its own export of this
        // board (`RoundRect[T]Pad_<w>x<h>_<r + inflation>_um`), see the README.
        let pads = manifest_pads(&export.manifest)
            .iter()
            .map(|pad| {
                (
                    pad.shape,
                    pad.size_x_um,
                    pad.size_y_um,
                    pad.corner_radius_um,
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            pads,
            BTreeSet::from([
                (PadShape::RoundRect, 540, 640, Some(135)),
                (PadShape::RoundRect, 800, 950, Some(200)),
                (PadShape::RoundRect, 900, 950, Some(225)),
            ])
        );
        let manifest: serde_json::Value = serde_json::from_str(&export.manifest).unwrap();
        assert_eq!(
            manifest["supported_profile"]["pad_shapes"],
            serde_json::json!(["circle", "rect", "roundrect"])
        );
        assert_eq!(export.pad_count, 6);
        assert!(export.dsn.contains("(polygon F.Cu 0"), "{}", export.dsn);
    }

    #[test]
    fn roundrect_outline_encloses_the_pad_within_its_error() {
        for (size_x, size_y, radius) in [(540, 640, 135), (1025, 1400, 250), (600, 600, 300)] {
            let outline = roundrect_outline(size_x, size_y, radius);
            let model = ManifestPadstack {
                name: String::new(),
                purpose: "pad".to_string(),
                shape: PadShape::RoundRect,
                layers: vec!["F.Cu".to_string()],
                size_x_um: size_x,
                size_y_um: size_y,
                corner_radius_um: Some(radius),
                drill_um: None,
            };
            let (half_x, half_y, r) = (size_x as f64 / 2.0, size_y as f64 / 2.0, radius as f64);
            // Every point of the copper outline is inside the polygon, up to
            // the 0.05 um the DSN's resolution can move a vertex.
            for step in 0..3600 {
                let angle = f64::from(step).to_radians() / 10.0;
                let (cos, sin) = (angle.cos(), angle.sin());
                let copper = (
                    (half_x - r).copysign(cos) + r * cos,
                    (half_y - r).copysign(sin) + r * sin,
                );
                let outside = outline.windows(2).any(|edge| {
                    let (a, b) = (edge[0], edge[1]);
                    let cross = (b.0 - a.0) * (copper.1 - a.1) - (b.1 - a.1) * (copper.0 - a.0);
                    cross < -0.05 * (b.0 - a.0).hypot(b.1 - a.1)
                });
                assert!(!outside, "{copper:?} outside {size_x}x{size_y} r{radius}");
            }
            let shape = DsnShapeIdentity::Polygon {
                layer: "F.Cu".to_string(),
                points: outline
                    .iter()
                    .map(|(x, y)| (OrderedF64::new(*x).unwrap(), OrderedF64::new(*y).unwrap()))
                    .collect(),
            };
            assert!(polygons_trace_roundrect(&[shape], &model));
        }
    }

    #[test]
    fn native_kicad_roundrect_polygons_are_adopted() {
        let baseline = stock_export();
        let native = stock_roundrect_native();
        let adopted = adopt_native_dsn(baseline, native.clone()).unwrap();

        assert_eq!(adopted.dsn, native);
        let names = manifest_pads(&adopted.manifest)
            .into_iter()
            .map(|pad| (pad.name, pad.corner_radius_um))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                (
                    "RoundRect[T]Pad_540.000000x640.000000_135.514000_um_0.000000_0".to_string(),
                    Some(135)
                ),
                (
                    "RoundRect[T]Pad_800.000000x950.000000_200.761000_um_0.000000_0".to_string(),
                    Some(200)
                ),
                (
                    "RoundRect[T]Pad_900.000000x950.000000_225.856000_um_0.000000_0".to_string(),
                    Some(225)
                ),
            ]
        );
    }

    /// Freerouting 2.3.0 routed both exports of the stock board completely;
    /// the strict import planner accepts each against its own manifest.
    #[test]
    fn freerouting_sessions_of_stock_roundrect_pads_pass_the_import_planner() {
        let temp = tempfile::tempdir().unwrap();
        let board_path = temp.path().join("specctra_stock_roundrect.kicad_pcb");
        std::fs::write(&board_path, STOCK_ROUNDRECT).unwrap();
        let rust = export_dsn(&board_path, STOCK_ROUNDRECT, &stock_rules()).unwrap();
        let native = adopt_native_dsn(rust.clone(), stock_roundrect_native()).unwrap();
        for (manifest, ses) in [
            (
                &rust.manifest,
                include_str!("../tests/fixtures/specctra_stock_roundrect.freerouting-2.3.0.ses"),
            ),
            (
                &native.manifest,
                include_str!(
                    "../tests/fixtures/specctra_stock_roundrect.native-kicad-10.freerouting-2.3.0.ses"
                ),
            ),
        ] {
            let plan =
                crate::specctra_ses::parse_import_plan(&board_path, STOCK_ROUNDRECT, manifest, ses)
                    .unwrap();
            let nets = plan
                .tracks
                .iter()
                .map(|track| track.net_name.as_str())
                .collect::<BTreeSet<_>>();
            assert_eq!(nets, BTreeSet::from(["GND", "SIG", "VCC"]));
        }
    }

    /// KiCad 10.0.6 named the stock 0805 padstack (1025 x 1400 um,
    /// `roundrect_rratio 0.243902`) `RoundRect[T]Pad_..._250.951000_um`:
    /// radius 250 um plus 0.951 um of growth. A plain floor of 249.99955
    /// would give 249.
    #[test]
    fn roundrect_corner_radius_follows_kicad_rounding() {
        let pad = |ratio: &str| {
            parse_sexp(&format!(
                "(pad \"1\" smd roundrect (size 1.025 1.4) (roundrect_rratio {ratio}))"
            ))
            .unwrap()
        };
        assert_eq!(
            roundrect_corner_radius_um(&pad("0.243902"), 1025, 1400, "R2-1").unwrap(),
            250
        );
        let error = roundrect_corner_radius_um(&pad("0.6"), 1025, 1400, "R2-1")
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside 0..=0.5"), "{error}");
    }

    /// KiCad's own polygon for the 0402 pad traces that pad, and no pad a few
    /// micrometres larger, smaller, or rounder.
    #[test]
    fn kicad_roundrect_polygon_traces_only_its_own_pad() {
        let native = dsn_identity(&parse_dsn_sexp(&stock_roundrect_native()).unwrap()).unwrap();
        let shapes =
            &native.padstacks["RoundRect[T]Pad_540.000000x640.000000_135.514000_um_0.000000_0"];
        let model = |size_x_um, size_y_um, corner_radius_um| ManifestPadstack {
            name: String::new(),
            purpose: "pad".to_string(),
            shape: PadShape::RoundRect,
            layers: vec!["F.Cu".to_string()],
            size_x_um,
            size_y_um,
            corner_radius_um: Some(corner_radius_um),
            drill_um: None,
        };
        assert!(polygons_trace_roundrect(shapes, &model(540, 640, 135)));
        assert!(!polygons_trace_roundrect(shapes, &model(550, 650, 137)));
        assert!(!polygons_trace_roundrect(shapes, &model(530, 630, 132)));
        // Within the arc error everywhere, but 1.5 um short of each side.
        assert!(!polygons_trace_roundrect(shapes, &model(544, 644, 135)));
        // KiCad's sharper corners stand about 4 um outside a 145 um corner.
        assert!(!polygons_trace_roundrect(shapes, &model(540, 640, 145)));
    }

    #[test]
    fn native_roundrect_of_another_size_is_refused() {
        let baseline = stock_export();
        // R1's pins point at the 0603 padstack in the native file.
        let native = stock_roundrect_native();
        let moved = native.replace(
            "(pin RoundRect[T]Pad_540.000000x640.000000_135.514000_um_0.000000_0 1",
            "(pin RoundRect[T]Pad_900.000000x950.000000_225.856000_um_0.000000_0 1",
        );
        assert_ne!(moved, native);
        let error = adopt_native_dsn(baseline, moved).unwrap_err().to_string();
        assert!(
            error.contains("changed pad geometry for component 'R1' pin '1'"),
            "{error}"
        );
    }

    #[test]
    fn chamfered_roundrect_pad_is_refused() {
        let source = STOCK_ROUNDRECT.replacen(
            "(roundrect_rratio 0.25)",
            "(roundrect_rratio 0.25)\n\t\t\t(chamfer_ratio 0.2)\n\t\t\t(chamfer top_left)",
            1,
        );
        assert_ne!(source, STOCK_ROUNDRECT);
        let error = export_dsn(Path::new("board.kicad_pcb"), &source, &stock_rules())
            .unwrap_err()
            .to_string();
        assert!(error.contains("chamfered corners"), "{error}");
    }

    #[test]
    fn deterministic_export_round_trips_through_specctra_parser() {
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb");
        let first = export_dsn(Path::new("board.kicad_pcb"), source, &rules()).unwrap();
        let second = export_dsn(Path::new("board.kicad_pcb"), source, &rules()).unwrap();

        assert_eq!(first.dsn, second.dsn);
        assert_eq!(first.manifest, second.manifest);
        assert_eq!(first.component_count, 2);
        assert_eq!(first.pad_count, 4);
        assert_eq!(first.net_count, 2);
        assert!(first.dsn.contains("(pcb board.kicad_pcb"));
        assert!(first.dsn.contains("(boundary"));
        assert!(first.dsn.contains("(net GND"));
        assert!(first.dsn.contains("R1-1"));
        let manifest: serde_json::Value = serde_json::from_str(&first.manifest).unwrap();
        assert_eq!(manifest["components"][0]["reference"], "R1");
        assert_eq!(manifest["components"][0]["x_um"], 100_000);
        assert_eq!(manifest["components"][0]["y_um"], -50_000);
        assert_eq!(manifest["components"][0]["rotation_degrees"], 0.0);
        assert_eq!(manifest["components"][0]["side"], "front");
    }

    #[test]
    fn freerouting_owned_dsn_corpus_parses() {
        let source = include_str!("../tests/fixtures/freerouting_issue269_minimal_v2_3_0.dsn");
        let mut tokenizer = ListTokenizer::new(BufReader::new(Cursor::new(source.as_bytes())));
        dsn::DsnFile::read_dsn(&mut tokenizer)
            .expect("Freerouting v2.3.0 corpus fixture must remain parseable");
    }

    #[test]
    fn native_kicad_dsn_rewrites_manifest_identifiers_without_changing_semantics() {
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb");
        let native = include_str!("../tests/fixtures/specctra_two_resistors.native-kicad-10.dsn");
        let baseline = export_dsn(
            Path::new("board.kicad_pcb"),
            source,
            &native_fixture_rules(),
        )
        .unwrap();
        let adopted = adopt_native_dsn(baseline, native.to_string()).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&adopted.manifest).unwrap();

        assert_eq!(adopted.dsn, native);
        assert_eq!(
            manifest["components"][0]["image_name"],
            "Resistor_SMD:R_0402"
        );
        assert_eq!(
            manifest["components"][0]["pads"][0]["padstack_name"],
            "Rect[T]Pad_600.000000x500.000000_um"
        );
        assert_eq!(manifest["nets"][0]["class_name"], "kicad_default");
        assert!(manifest["padstacks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|padstack| padstack["name"] == "Via[0-1]_600:300_um"));
    }

    /// Optional local parity check against the Freerouting engine. CI does not
    /// install Java or Freerouting; maintainers can opt in with
    /// `FREEROUTING_JAR=/path/to/freerouting.jar cargo test -p konnect-core
    /// freerouting_accepts_exported_fixture -- --ignored`.
    #[test]
    #[ignore = "requires Java and FREEROUTING_JAR"]
    fn freerouting_accepts_exported_fixture() {
        let jar = std::env::var_os("FREEROUTING_JAR").expect("set FREEROUTING_JAR");
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb");
        let temp = tempfile::tempdir().expect("tempdir");
        let board_path = temp.path().join("board.kicad_pcb");
        let dsn_path = temp.path().join("board.dsn");
        let ses_path = temp.path().join("board.ses");
        std::fs::write(&board_path, source).expect("write board fixture");
        let export = export_dsn(&board_path, source, &rules()).unwrap();
        std::fs::write(&dsn_path, export.dsn).expect("write DSN");

        let output = std::process::Command::new("java")
            .arg("-jar")
            .arg(jar)
            .arg("-de")
            .arg(&dsn_path)
            .arg("-do")
            .arg(&ses_path)
            .arg("-mp")
            .arg("2")
            .output()
            .expect("launch Freerouting");

        assert!(
            output.status.success(),
            "Freerouting refused generated DSN:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            std::fs::metadata(&ses_path)
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false),
            "Freerouting did not produce a non-empty SES"
        );
        let ses = std::fs::read_to_string(&ses_path).expect("read Freerouting SES");
        crate::specctra_ses::parse_import_plan(&board_path, source, &export.manifest, &ses)
            .expect("Freerouting SES must pass Konnect's strict import planner");
    }

    /// Real-engine parity for KiCad 10's native exporter identifiers plus the
    /// rewritten revision manifest consumed by the strict SES planner.
    #[test]
    #[ignore = "requires Java and FREEROUTING_JAR"]
    fn freerouting_round_trips_native_kicad_export() {
        let jar = std::env::var_os("FREEROUTING_JAR").expect("set FREEROUTING_JAR");
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb");
        let native = include_str!("../tests/fixtures/specctra_two_resistors.native-kicad-10.dsn");
        let temp = tempfile::tempdir().expect("tempdir");
        let board_path = temp.path().join("board.kicad_pcb");
        let dsn_path = temp.path().join("board.dsn");
        let ses_path = temp.path().join("board.ses");
        std::fs::write(&board_path, source).expect("write board fixture");
        let baseline = export_dsn(&board_path, source, &native_fixture_rules()).unwrap();
        let export = adopt_native_dsn(baseline, native.to_string()).unwrap();
        std::fs::write(&dsn_path, &export.dsn).expect("write native DSN");

        let output = std::process::Command::new("java")
            .arg("-jar")
            .arg(jar)
            .arg("-de")
            .arg(&dsn_path)
            .arg("-do")
            .arg(&ses_path)
            .arg("-mp")
            .arg("2")
            .output()
            .expect("run Freerouting");
        assert!(
            output.status.success(),
            "Freerouting failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let ses = std::fs::read_to_string(&ses_path).expect("read SES");
        let plan =
            crate::specctra_ses::parse_import_plan(&board_path, source, &export.manifest, &ses)
                .expect("strict SES planner accepts native KiCad identifiers");
        assert!(!plan.tracks.is_empty() || !plan.vias.is_empty());
    }

    #[test]
    fn incomplete_effective_rules_are_refused() {
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb");
        let mut rules = rules();
        rules.get_mut("GND").unwrap().via_drill_mm = None;
        let error = export_dsn(Path::new("board.kicad_pcb"), source, &rules)
            .unwrap_err()
            .to_string();
        assert!(error.contains("via drill"), "{error}");
    }

    #[test]
    fn unlocked_routing_is_refused_before_export() {
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb").replace(
            "\n)",
            "\n  (segment (start 1 1) (end 2 2) (width 0.2) (layer \"F.Cu\") (net 1))\n)",
        );
        let error = export_dsn(Path::new("board.kicad_pcb"), &source, &rules())
            .unwrap_err()
            .to_string();
        assert!(error.contains("existing track segment"), "{error}");
    }

    #[test]
    fn locked_tracks_and_vias_are_exported_as_fixed_wiring() {
        let source = include_str!("../tests/fixtures/specctra_two_resistors_locked.kicad_pcb");
        let export = export_dsn(Path::new("board.kicad_pcb"), source, &rules()).unwrap();
        assert!(export.dsn.contains("(type fix)"));
        assert!(export.dsn.contains("(path F.Cu 250"));
        assert!(export.dsn.contains("(via konnect_via_"));
        let manifest: serde_json::Value = serde_json::from_str(&export.manifest).unwrap();
        assert_eq!(manifest["supported_profile"]["existing_routing"], true);
        assert_eq!(manifest["supported_profile"]["locked_track_count"], 1);
        assert_eq!(manifest["supported_profile"]["locked_via_count"], 1);
    }

    #[test]
    fn freerouting_session_preserves_locked_routing_outside_the_import_plan() {
        let source = include_str!("../tests/fixtures/specctra_two_resistors_locked.kicad_pcb");
        let native =
            include_str!("../tests/fixtures/specctra_two_resistors_locked.native-kicad-10.dsn");
        let ses =
            include_str!("../tests/fixtures/specctra_two_resistors_locked.freerouting-2.3.0.ses");
        let temp = tempfile::tempdir().unwrap();
        let board_path = temp.path().join("specctra_two_resistors_locked.kicad_pcb");
        std::fs::write(&board_path, source).unwrap();
        let baseline = export_dsn(&board_path, source, &native_fixture_rules()).unwrap();
        let adopted = adopt_native_dsn(baseline, native.to_string()).unwrap();
        let plan =
            crate::specctra_ses::parse_import_plan(&board_path, source, &adopted.manifest, ses)
                .unwrap();
        assert_eq!(plan.locked_track_count, 1);
        assert_eq!(plan.locked_via_count, 1);
        assert!(!plan.tracks.is_empty() || !plan.vias.is_empty());
    }

    #[test]
    fn locked_arcs_are_refused_instead_of_approximated() {
        let source = include_str!("../tests/fixtures/specctra_two_resistors_locked_arc.kicad_pcb");
        let error = export_dsn(Path::new("board.kicad_pcb"), source, &rules())
            .unwrap_err()
            .to_string();
        assert!(error.contains("locked routed arc"), "{error}");
        assert!(error.contains("without approximation"), "{error}");
    }

    #[test]
    fn branched_outline_is_refused() {
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb")
            .replace(
                "\n)",
                "\n  (gr_line (start 80 30) (end 90 40) (stroke (width 0.05) (type default)) (layer \"Edge.Cuts\") (uuid \"branch\"))\n)",
            );
        let error = export_dsn(Path::new("board.kicad_pcb"), &source, &rules())
            .unwrap_err()
            .to_string();
        assert!(error.contains("degree"), "{error}");
    }

    #[test]
    fn nonzero_pad_shape_offset_is_refused() {
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb").replacen(
            "(size 0.6 0.5)",
            "(size 0.6 0.5)\n\t\t\t(offset 0.1 0)",
            1,
        );
        let error = export_dsn(Path::new("board.kicad_pcb"), &source, &rules())
            .unwrap_err()
            .to_string();
        assert!(error.contains("shape offset"), "{error}");
    }
}
