//! Strict, provenance-bound Specctra SES import planning.
//!
//! This module performs no KiCad mutation. It fully validates the reverse
//! manifest, the Freerouting session, placement invariance, net/layer names,
//! via geometry, and every route primitive before returning an apply plan.

use anyhow::{bail, Context, Result};
use konnect_sexp::{parse_sexp, SexpNode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const UM_PER_MM: f64 = 1_000.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SesImportPlan {
    pub board_path: String,
    pub source_sha256: String,
    pub session_id: String,
    pub locked_track_count: usize,
    pub locked_via_count: usize,
    pub tracks: Vec<SesTrack>,
    pub arcs: Vec<SesArc>,
    pub vias: Vec<SesVia>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SesTrack {
    pub net_name: String,
    pub layer: String,
    pub width_mm: f64,
    pub x1_mm: f64,
    pub y1_mm: f64,
    pub x2_mm: f64,
    pub y2_mm: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SesArc {
    pub net_name: String,
    pub layer: String,
    pub width_mm: f64,
    pub start_x_mm: f64,
    pub start_y_mm: f64,
    pub mid_x_mm: f64,
    pub mid_y_mm: f64,
    pub end_x_mm: f64,
    pub end_y_mm: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SesVia {
    pub net_name: String,
    pub padstack_name: String,
    pub x_mm: f64,
    pub y_mm: f64,
    pub drill_mm: f64,
    pub size_mm: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestLayer {
    kicad_name: String,
    dsn_name: String,
    index: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestPin {
    pad_number: String,
    dsn_pin: String,
    net: Option<String>,
    padstack_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestNet {
    name: String,
    pins: Vec<String>,
    class_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestPadstack {
    name: String,
    purpose: String,
    shape: String,
    layers: Vec<String>,
    size_x_um: i64,
    size_y_um: i64,
    #[serde(default, rename = "corner_radius_um")]
    _corner_radius_um: Option<serde::de::IgnoredAny>,
    drill_um: Option<i64>,
}

pub(crate) fn parse_import_plan(
    board_path: &Path,
    board_source: &str,
    manifest_source: &str,
    ses_source: &str,
) -> Result<SesImportPlan> {
    let manifest: Manifest =
        serde_json::from_str(manifest_source).context("parse Specctra reverse manifest")?;
    validate_manifest(board_path, board_source, &manifest)?;

    let root = parse_sexp(ses_source).context("parse Specctra SES")?;
    require_head(&root, "session")?;
    require_direct_shape(&root, 1, &["base_design", "placement", "was_is", "routes"])?;
    let session_id = atom(&root, 1, "session id")?.to_string();

    let base_design = one_child(&root, "base_design")?;
    require_direct_shape(base_design, 1, &[])?;
    let expected_base = board_path
        .file_stem()
        .and_then(|value| value.to_str())
        .context("board path has no UTF-8 file stem")?;
    let actual_base = atom(base_design, 1, "base_design name")?;
    // Freerouting's native MCP stores the uploaded DSN under its generated job
    // id and writes that id as both the session name and base_design. The exact
    // target board remains bound by the manifest path and snapshot hash below;
    // accept only that self-consistent transport alias, never an unrelated
    // design name.
    if actual_base != expected_base && actual_base != session_id {
        bail!(
            "SES base_design '{actual_base}' matches neither board '{expected_base}' nor session '{session_id}'"
        );
    }

    let was_is = one_child(&root, "was_is")?;
    require_direct_shape(was_is, 0, &[])?;
    validate_placement(one_child(&root, "placement")?, &manifest)?;

    let routes = one_child(&root, "routes")?;
    require_direct_shape(
        routes,
        0,
        &["resolution", "parser", "library_out", "network_out"],
    )?;
    let route_resolution = parse_resolution(one_child(routes, "resolution")?)?;
    if let Some(parser) = optional_child(routes, "parser")? {
        validate_parser(parser)?;
    }
    validate_library(
        one_child(routes, "library_out")?,
        route_resolution,
        &manifest,
    )?;
    let (tracks, arcs, vias) = parse_network(
        one_child(routes, "network_out")?,
        route_resolution,
        &manifest,
    )?;
    if tracks.is_empty() && arcs.is_empty() && vias.is_empty() {
        bail!("SES contains no route primitives");
    }

    Ok(SesImportPlan {
        board_path: manifest.board_path,
        source_sha256: manifest.source_sha256,
        session_id,
        locked_track_count: manifest.supported_profile.locked_track_count,
        locked_via_count: manifest.supported_profile.locked_via_count,
        tracks,
        arcs,
        vias,
    })
}

fn validate_manifest(board_path: &Path, board_source: &str, manifest: &Manifest) -> Result<()> {
    if manifest.schema_version != 1 {
        bail!(
            "unsupported Specctra manifest schema {}",
            manifest.schema_version
        );
    }
    if manifest.coordinate_unit != "um" || manifest.resolution != 10 {
        bail!(
            "unsupported manifest coordinate system: {} resolution {}",
            manifest.coordinate_unit,
            manifest.resolution
        );
    }
    if manifest.supported_profile.copper_layers != 2
        || manifest.supported_profile.component_side != "front"
        || manifest.supported_profile.copper_zones
        || manifest.supported_profile.custom_rules
        || manifest.supported_profile.outline.is_empty()
        || manifest.supported_profile.pad_shapes.is_empty()
    {
        bail!("manifest does not describe the supported first routing profile");
    }
    let (locked_track_count, locked_via_count) = locked_routing_counts(board_source)?;
    let has_locked_routing = locked_track_count > 0 || locked_via_count > 0;
    if manifest.supported_profile.existing_routing != has_locked_routing
        || manifest.supported_profile.locked_track_count != locked_track_count
        || manifest.supported_profile.locked_via_count != locked_via_count
    {
        bail!("manifest locked-routing inventory does not match the live board snapshot");
    }
    let requested = canonical_existing(board_path)?;
    let recorded = canonical_existing(Path::new(&manifest.board_path))?;
    if requested != recorded {
        bail!(
            "manifest board '{}' does not match requested board '{}'",
            recorded.display(),
            requested.display()
        );
    }
    let actual_hash = sha256_hex(board_source.as_bytes());
    if actual_hash != manifest.source_sha256 {
        bail!(
            "live board revision does not match routing manifest (expected {}, got {})",
            manifest.source_sha256,
            actual_hash
        );
    }
    validate_manifest_relations(manifest)
}

fn locked_routing_counts(board_source: &str) -> Result<(usize, usize)> {
    let tree =
        parse_sexp(board_source).context("parse KiCad board for locked-routing inventory")?;
    if !tree.find_all("arc").is_empty() {
        bail!("live board contains an unsupported routed arc");
    }
    let tracks = tree.find_all("segment");
    if tracks
        .iter()
        .any(|track| track.find_str("locked") != Some("yes"))
    {
        bail!("live board contains an existing track segment that is not locked");
    }
    let vias = tree.find_all("via");
    if vias.iter().any(|via| via.find_str("locked") != Some("yes")) {
        bail!("live board contains an existing via that is not locked");
    }
    Ok((tracks.len(), vias.len()))
}

fn validate_manifest_relations(manifest: &Manifest) -> Result<()> {
    let mut layer_names = BTreeSet::new();
    let mut layer_indices = BTreeSet::new();
    for layer in &manifest.layers {
        if layer.kicad_name.is_empty()
            || layer.dsn_name.is_empty()
            || !layer_names.insert(layer.dsn_name.as_str())
            || !layer_indices.insert(layer.index)
        {
            bail!("manifest contains an invalid or duplicate layer mapping");
        }
    }
    if layer_names.len() != 2 {
        bail!("manifest must contain exactly two copper layer mappings");
    }

    let net_names = manifest
        .nets
        .iter()
        .map(|net| net.name.as_str())
        .collect::<BTreeSet<_>>();
    if net_names.len() != manifest.nets.len() || net_names.contains("") {
        bail!("manifest contains an empty or duplicate net name");
    }
    let padstack_names = manifest
        .padstacks
        .iter()
        .map(|padstack| padstack.name.as_str())
        .collect::<BTreeSet<_>>();
    if padstack_names.len() != manifest.padstacks.len() || padstack_names.contains("") {
        bail!("manifest contains an empty or duplicate padstack name");
    }
    let mut references = BTreeSet::new();
    for component in &manifest.components {
        if component.reference.is_empty()
            || component.kiid.is_empty()
            || component.image_name.is_empty()
            || !component.rotation_degrees.is_finite()
            || component.side != "front"
            || !references.insert(component.reference.as_str())
        {
            bail!("manifest contains invalid component placement metadata");
        }
        for pin in &component.pads {
            if pin.pad_number.is_empty()
                || pin.dsn_pin != format!("{}-{}", component.reference, pin.pad_number)
                || !padstack_names.contains(pin.padstack_name.as_str())
                || pin
                    .net
                    .as_deref()
                    .is_some_and(|net| !net_names.contains(net))
            {
                bail!(
                    "manifest contains invalid pin mapping for {}",
                    component.reference
                );
            }
        }
    }
    for net in &manifest.nets {
        if net.class_name.is_empty() || net.pins.is_empty() {
            bail!("manifest net '{}' is incomplete", net.name);
        }
    }
    Ok(())
}

fn validate_placement(node: &SexpNode, manifest: &Manifest) -> Result<()> {
    require_direct_shape(node, 0, &["resolution", "component"])?;
    let resolution = parse_resolution(one_child(node, "resolution")?)?;
    let expected = manifest
        .components
        .iter()
        .map(|component| (component.reference.as_str(), component))
        .collect::<BTreeMap<_, _>>();
    let mut observed = BTreeSet::new();
    for component_node in node.find_all("component") {
        require_direct_shape(component_node, 1, &["place"])?;
        let image_name = atom(component_node, 1, "placement component image")?;
        for place in component_node.find_all("place") {
            require_direct_shape(place, 5, &[])?;
            let reference = atom(place, 1, "place reference")?;
            let component = expected.get(reference).with_context(|| {
                format!("SES placement contains unknown component '{reference}'")
            })?;
            if image_name != component.image_name {
                bail!(
                    "SES image for '{reference}' changed from '{}' to '{image_name}'",
                    component.image_name
                );
            }
            if !observed.insert(reference.to_string()) {
                bail!("SES placement repeats component '{reference}'");
            }
            let x_um = scaled_i64(number(place, 2, "place x")?, resolution, "place x")?;
            let y_um = scaled_i64(number(place, 3, "place y")?, resolution, "place y")?;
            let side = atom(place, 4, "place side")?;
            let rotation = number(place, 5, "place rotation")?;
            if x_um != component.x_um
                || y_um != component.y_um
                || side != component.side
                || (rotation - component.rotation_degrees).abs() > 1e-9
            {
                bail!("SES changes placement of component '{reference}'");
            }
        }
    }
    if observed.len() != expected.len() {
        let missing = expected
            .keys()
            .filter(|reference| !observed.contains(**reference))
            .copied()
            .collect::<Vec<_>>();
        bail!("SES placement omits component(s): {}", missing.join(", "));
    }
    Ok(())
}

fn validate_parser(node: &SexpNode) -> Result<()> {
    require_direct_shape(
        node,
        0,
        &[
            "string_quote",
            "space_in_quoted_tokens",
            "host_cad",
            "host_version",
        ],
    )?;
    for child in node.children().unwrap_or(&[]).iter().skip(1) {
        require_direct_shape(child, 1, &[])?;
    }
    Ok(())
}

fn validate_library(node: &SexpNode, resolution: f64, manifest: &Manifest) -> Result<()> {
    require_direct_shape(node, 0, &["padstack"])?;
    let vias = manifest
        .padstacks
        .iter()
        .filter(|padstack| padstack.purpose == "via")
        .map(|padstack| (padstack.name.as_str(), padstack))
        .collect::<BTreeMap<_, _>>();
    let mut observed = BTreeSet::new();
    for padstack in node.find_all("padstack") {
        require_direct_shape(padstack, 1, &["shape", "attach"])?;
        let name = atom(padstack, 1, "library_out padstack name")?;
        let expected = vias
            .get(name)
            .with_context(|| format!("SES library contains unknown via padstack '{name}'"))?;
        if expected.shape != "circle"
            || expected.size_x_um != expected.size_y_um
            || expected.drill_um.is_none()
        {
            bail!("manifest via padstack '{name}' is not a supported round through via");
        }
        if !observed.insert(name.to_string()) {
            bail!("SES library repeats padstack '{name}'");
        }
        let mut shape_layers = BTreeSet::new();
        for shape in padstack.find_all("shape") {
            require_direct_shape(shape, 0, &["circle"])?;
            let circle = one_child(shape, "circle")?;
            require_direct_shape(circle, 4, &[])?;
            let layer = atom(circle, 1, "via circle layer")?;
            if !expected.layers.iter().any(|candidate| candidate == layer) {
                bail!("SES via padstack '{name}' has unexpected layer '{layer}'");
            }
            if !shape_layers.insert(layer) {
                bail!("SES via padstack '{name}' repeats layer '{layer}'");
            }
            let diameter_um = scaled_i64(
                number(circle, 2, "via diameter")?,
                resolution,
                "via diameter",
            )?;
            let x_offset = scaled_i64(
                number(circle, 3, "via x offset")?,
                resolution,
                "via x offset",
            )?;
            let y_offset = scaled_i64(
                number(circle, 4, "via y offset")?,
                resolution,
                "via y offset",
            )?;
            if diameter_um != expected.size_x_um || x_offset != 0 || y_offset != 0 {
                bail!("SES via padstack '{name}' geometry differs from the manifest");
            }
        }
        if shape_layers.len() != expected.layers.len() {
            bail!("SES via padstack '{name}' does not cover every expected copper layer");
        }
        if let Some(attach) = optional_child(padstack, "attach")? {
            require_direct_shape(attach, 1, &[])?;
            if atom(attach, 1, "attach value")? != "off" {
                bail!("SES via padstack '{name}' has unsupported attach mode");
            }
        }
    }
    Ok(())
}

fn parse_network(
    node: &SexpNode,
    resolution: f64,
    manifest: &Manifest,
) -> Result<(Vec<SesTrack>, Vec<SesArc>, Vec<SesVia>)> {
    require_direct_shape(node, 0, &["net"])?;
    let nets = manifest
        .nets
        .iter()
        .map(|net| net.name.as_str())
        .collect::<BTreeSet<_>>();
    let layers = manifest
        .layers
        .iter()
        .map(|layer| (layer.dsn_name.as_str(), layer.kicad_name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let vias_by_name = manifest
        .padstacks
        .iter()
        .filter(|padstack| padstack.purpose == "via")
        .map(|padstack| (padstack.name.as_str(), padstack))
        .collect::<BTreeMap<_, _>>();
    let mut tracks = Vec::new();
    let mut arcs = Vec::new();
    let mut vias = Vec::new();
    let mut seen_tracks = BTreeSet::new();
    let mut seen_arcs = BTreeSet::new();
    let mut seen_vias = BTreeSet::new();

    for net_node in node.find_all("net") {
        require_direct_shape(net_node, 1, &["wire", "via"])?;
        let net_name = atom(net_node, 1, "network_out net name")?;
        if !nets.contains(net_name) {
            bail!("SES route refers to unknown net '{net_name}'");
        }
        for wire in net_node.find_all("wire") {
            require_direct_shape(wire, 0, &["path", "qarc"])?;
            let path = optional_child(wire, "path")?;
            let qarc = optional_child(wire, "qarc")?;
            match (path, qarc) {
                (Some(path), None) => {
                    let data = atom_values(path)?;
                    if data.len() < 6 || (data.len() - 2) % 2 != 0 {
                        bail!(
                            "SES path for net '{net_name}' does not contain complete point pairs"
                        );
                    }
                    let layer = layers
                        .get(data[0])
                        .with_context(|| format!("SES path uses unknown layer '{}'", data[0]))?;
                    let width_um = scaled_positive(
                        number_text(data[1], "path width")?,
                        resolution,
                        "path width",
                    )?;
                    let width_mm = width_um / UM_PER_MM;
                    let points = data[2..]
                        .chunks_exact(2)
                        .map(|pair| {
                            let x_um =
                                scaled(number_text(pair[0], "path x")?, resolution, "path x")?;
                            let y_um =
                                scaled(number_text(pair[1], "path y")?, resolution, "path y")?;
                            Ok((x_um / UM_PER_MM, -y_um / UM_PER_MM))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    for pair in points.windows(2) {
                        let (x1_mm, y1_mm) = pair[0];
                        let (x2_mm, y2_mm) = pair[1];
                        if x1_mm == x2_mm && y1_mm == y2_mm {
                            bail!("SES path for net '{net_name}' contains a zero-length segment");
                        }
                        let mut endpoints = [
                            (ordered_f64(x1_mm), ordered_f64(y1_mm)),
                            (ordered_f64(x2_mm), ordered_f64(y2_mm)),
                        ];
                        endpoints.sort();
                        let key = (
                            net_name.to_string(),
                            (*layer).to_string(),
                            ordered_f64(width_mm),
                            endpoints,
                        );
                        if !seen_tracks.insert(key) {
                            bail!("SES repeats a route segment on net '{net_name}'");
                        }
                        tracks.push(SesTrack {
                            net_name: net_name.to_string(),
                            layer: (*layer).to_string(),
                            width_mm,
                            x1_mm,
                            y1_mm,
                            x2_mm,
                            y2_mm,
                        });
                    }
                }
                (None, Some(qarc)) => {
                    let data = atom_values(qarc)?;
                    if data.len() != 8 {
                        bail!("SES qarc for net '{net_name}' must contain layer, width, start, end, and center");
                    }
                    let layer = layers
                        .get(data[0])
                        .with_context(|| format!("SES qarc uses unknown layer '{}'", data[0]))?;
                    let width_mm = scaled_positive(
                        number_text(data[1], "qarc width")?,
                        resolution,
                        "qarc width",
                    )? / UM_PER_MM;
                    let point = |x: &str, y: &str, label: &str| -> Result<(f64, f64)> {
                        Ok((
                            scaled(number_text(x, &format!("{label} x"))?, resolution, label)?
                                / UM_PER_MM,
                            -scaled(number_text(y, &format!("{label} y"))?, resolution, label)?
                                / UM_PER_MM,
                        ))
                    };
                    let start = point(data[2], data[3], "qarc start")?;
                    let end = point(data[4], data[5], "qarc end")?;
                    let center = point(data[6], data[7], "qarc center")?;
                    let start_vector = (start.0 - center.0, start.1 - center.1);
                    let end_vector = (end.0 - center.0, end.1 - center.1);
                    let start_radius = start_vector.0.hypot(start_vector.1);
                    let end_radius = end_vector.0.hypot(end_vector.1);
                    let scale = start_radius.max(end_radius).max(1.0);
                    if start_radius <= f64::EPSILON
                        || (start_radius - end_radius).abs() > 1e-6 * scale
                        || (start_vector.0 * end_vector.0 + start_vector.1 * end_vector.1).abs()
                            > 1e-6 * start_radius * end_radius
                    {
                        bail!("SES qarc for net '{net_name}' is not a finite quarter-circle");
                    }
                    let bisector = (start_vector.0 + end_vector.0, start_vector.1 + end_vector.1);
                    let bisector_length = bisector.0.hypot(bisector.1);
                    if bisector_length <= f64::EPSILON {
                        bail!("SES qarc for net '{net_name}' has no unique midpoint");
                    }
                    let mid = (
                        center.0 + start_radius * bisector.0 / bisector_length,
                        center.1 + start_radius * bisector.1 / bisector_length,
                    );
                    let key = (
                        net_name.to_string(),
                        (*layer).to_string(),
                        ordered_f64(width_mm),
                        (ordered_f64(start.0), ordered_f64(start.1)),
                        (ordered_f64(end.0), ordered_f64(end.1)),
                        (ordered_f64(center.0), ordered_f64(center.1)),
                    );
                    if !seen_arcs.insert(key) {
                        bail!("SES repeats an arc on net '{net_name}'");
                    }
                    arcs.push(SesArc {
                        net_name: net_name.to_string(),
                        layer: (*layer).to_string(),
                        width_mm,
                        start_x_mm: start.0,
                        start_y_mm: start.1,
                        mid_x_mm: mid.0,
                        mid_y_mm: mid.1,
                        end_x_mm: end.0,
                        end_y_mm: end.1,
                    });
                }
                _ => bail!("SES wire for net '{net_name}' must contain exactly one path or qarc"),
            }
        }
        for via_node in net_node.find_all("via") {
            require_direct_shape(via_node, 3, &["net", "type"])?;
            let padstack_name = atom(via_node, 1, "via padstack")?;
            let padstack = vias_by_name
                .get(padstack_name)
                .with_context(|| format!("SES via uses unknown padstack '{padstack_name}'"))?;
            // KiCad's SES writer repeats `(net ...)` on a via; Freerouting's
            // native MCP output omits it because the via is already nested in
            // `network_out/net`. Inherit only that validated enclosing net.
            // When the redundant child is present, it must agree.
            if let Some(nested_net) = optional_child(via_node, "net")? {
                require_direct_shape(nested_net, 1, &[])?;
                if atom(nested_net, 1, "via net")? != net_name {
                    bail!("SES via net does not match enclosing net '{net_name}'");
                }
            }
            if let Some(kind) = optional_child(via_node, "type")? {
                require_direct_shape(kind, 1, &[])?;
                if atom(kind, 1, "via type")? != "protect" {
                    bail!("SES via has unsupported type");
                }
            }
            let x_um = scaled(number(via_node, 2, "via x")?, resolution, "via x")?;
            let y_um = scaled(number(via_node, 3, "via y")?, resolution, "via y")?;
            let x_mm = x_um / UM_PER_MM;
            let y_mm = -y_um / UM_PER_MM;
            let key = (net_name.to_string(), ordered_f64(x_mm), ordered_f64(y_mm));
            if !seen_vias.insert(key) {
                bail!("SES repeats a via on net '{net_name}'");
            }
            vias.push(SesVia {
                net_name: net_name.to_string(),
                padstack_name: padstack_name.to_string(),
                x_mm,
                y_mm,
                drill_mm: padstack.drill_um.context("manifest via has no drill")? as f64
                    / UM_PER_MM,
                size_mm: padstack.size_x_um as f64 / UM_PER_MM,
            });
        }
    }
    Ok((tracks, arcs, vias))
}

fn parse_resolution(node: &SexpNode) -> Result<f64> {
    require_direct_shape(node, 2, &[])?;
    if atom(node, 1, "resolution unit")? != "um" {
        bail!("only SES resolution unit 'um' is supported");
    }
    let resolution = number(node, 2, "resolution")?;
    if !resolution.is_finite() || resolution <= 0.0 || resolution.fract() != 0.0 {
        bail!("SES resolution must be a positive integer");
    }
    Ok(resolution)
}

fn require_head(node: &SexpNode, expected: &str) -> Result<()> {
    if node.head() != Some(expected) {
        bail!("expected ({expected} ...), got {:?}", node.head());
    }
    Ok(())
}

fn require_direct_shape(node: &SexpNode, atom_count: usize, allowed_lists: &[&str]) -> Result<()> {
    let children = node.children().context("expected SES list")?;
    let mut seen_list = false;
    for (index, child) in children.iter().skip(1).enumerate() {
        match child {
            SexpNode::Atom(_) | SexpNode::Str(_) if !seen_list && index < atom_count => {}
            SexpNode::List(_) => {
                seen_list = true;
                let tag = child.head().context("SES contains an empty list")?;
                if !allowed_lists.contains(&tag) {
                    bail!(
                        "unsupported SES field '{tag}' inside '{}'",
                        node.head().unwrap_or("<root>")
                    );
                }
            }
            _ => bail!(
                "unexpected SES value inside '{}'",
                node.head().unwrap_or("<root>")
            ),
        }
    }
    let actual_atoms = children
        .iter()
        .skip(1)
        .take_while(|child| child.as_str().is_some())
        .count();
    if actual_atoms != atom_count {
        bail!(
            "SES '{}' expects {atom_count} direct value(s), got {actual_atoms}",
            node.head().unwrap_or("<root>")
        );
    }
    Ok(())
}

fn one_child<'a>(node: &'a SexpNode, tag: &str) -> Result<&'a SexpNode> {
    let children = node.find_all(tag);
    if children.len() != 1 {
        bail!(
            "SES '{}' must contain exactly one '{tag}', got {}",
            node.head().unwrap_or("<root>"),
            children.len()
        );
    }
    Ok(children[0])
}

fn optional_child<'a>(node: &'a SexpNode, tag: &str) -> Result<Option<&'a SexpNode>> {
    let children = node.find_all(tag);
    if children.len() > 1 {
        bail!("SES '{}' repeats '{tag}'", node.head().unwrap_or("<root>"));
    }
    Ok(children.first().copied())
}

fn atom<'a>(node: &'a SexpNode, index: usize, label: &str) -> Result<&'a str> {
    node.get(index)
        .and_then(SexpNode::as_str)
        .with_context(|| format!("SES {label} is missing"))
}

fn number(node: &SexpNode, index: usize, label: &str) -> Result<f64> {
    number_text(atom(node, index, label)?, label)
}

fn number_text(value: &str, label: &str) -> Result<f64> {
    let number = value
        .parse::<f64>()
        .with_context(|| format!("SES {label} is not a number"))?;
    if !number.is_finite() {
        bail!("SES {label} is not finite");
    }
    Ok(number)
}

fn atom_values(node: &SexpNode) -> Result<Vec<&str>> {
    let children = node.children().context("expected SES list")?;
    children
        .iter()
        .skip(1)
        .map(|child| child.as_str().context("unexpected nested SES route field"))
        .collect()
}

fn scaled(value: f64, resolution: f64, label: &str) -> Result<f64> {
    let scaled = value / resolution;
    if !scaled.is_finite() || scaled.abs() > 1_000_000_000.0 {
        bail!("SES {label} is outside the supported coordinate range");
    }
    Ok(scaled)
}

fn scaled_positive(value: f64, resolution: f64, label: &str) -> Result<f64> {
    let value = scaled(value, resolution, label)?;
    if value <= 0.0 {
        bail!("SES {label} must be positive");
    }
    Ok(value)
}

fn scaled_i64(value: f64, resolution: f64, label: &str) -> Result<i64> {
    let value = scaled(value, resolution, label)?;
    if value.fract().abs() > 1e-9 {
        bail!("SES {label} does not resolve to a whole micrometre");
    }
    Ok(value as i64)
}

fn ordered_f64(value: f64) -> u64 {
    let bits = value.to_bits();
    if bits >> 63 == 0 {
        bits | (1 << 63)
    } else {
        !bits
    }
}

fn canonical_existing(path: &Path) -> Result<std::path::PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specctra::export_dsn;
    use konnect_ipc::{IpcEffectiveRoutingRules, IpcRoutingRules};

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

    fn sample_ses() -> &'static str {
        r#"(session board
  (base_design board)
  (placement
    (resolution um 10)
    (component konnect_image_R1 (place R1 1000000 -500000 front 0))
    (component konnect_image_R2 (place R2 1100000 -500000 front 0))
  )
  (was_is)
  (routes
    (resolution um 10)
    (parser (host_cad KiCad) (host_version 10.0.5))
    (library_out
      (padstack konnect_via_0001
        (shape (circle F.Cu 6000 0 0))
        (shape (circle B.Cu 6000 0 0))
        (attach off)
      )
    )
    (network_out
      (net GND
        (wire (path F.Cu 2500 995000 -500000 1050000 -500000 1050000 -510000))
        (via konnect_via_0001 1050000 -510000 (net GND) (type protect))
      )
    )
  )
)"#
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, String, String) {
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb").to_string();
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, &source).unwrap();
        let export = export_dsn(&board, &source, &rules()).unwrap();
        (dir, board, source, export.manifest)
    }

    #[test]
    fn valid_session_lowers_to_kicad_coordinates() {
        let (_dir, board, source, manifest) = fixture();
        let plan = parse_import_plan(&board, &source, &manifest, sample_ses()).unwrap();
        assert_eq!(plan.tracks.len(), 2);
        assert_eq!(plan.vias.len(), 1);
        assert!(plan.arcs.is_empty());
        assert_eq!(plan.tracks[0].width_mm, 0.25);
        assert_eq!(plan.tracks[0].x1_mm, 99.5);
        assert_eq!(plan.tracks[0].y1_mm, 50.0);
        assert_eq!(plan.vias[0].x_mm, 105.0);
        assert_eq!(plan.vias[0].y_mm, 51.0);
        assert_eq!(plan.vias[0].drill_mm, 0.3);
    }

    #[test]
    fn native_mcp_job_alias_is_accepted_as_the_transport_design_name() {
        let (_dir, board, source, manifest) = fixture();
        let ses = sample_ses()
            .replacen("(session board", "(session J-ABC123", 1)
            .replacen("(base_design board)", "(base_design J-ABC123)", 1);
        let plan = parse_import_plan(&board, &source, &manifest, &ses).unwrap();
        assert_eq!(plan.session_id, "J-ABC123");
    }

    #[test]
    fn unrelated_base_design_is_still_refused() {
        let (_dir, board, source, manifest) = fixture();
        let ses = sample_ses().replacen("(base_design board)", "(base_design other)", 1);
        let error = parse_import_plan(&board, &source, &manifest, &ses).unwrap_err();
        assert!(error.to_string().contains("matches neither"), "{error:#}");
    }

    #[test]
    fn native_mcp_via_inherits_its_validated_enclosing_net() {
        let (_dir, board, source, manifest) = fixture();
        let ses = sample_ses().replace(" (net GND) (type protect)", "");
        let plan = parse_import_plan(&board, &source, &manifest, &ses).unwrap();
        assert_eq!(plan.vias.len(), 1);
        assert_eq!(plan.vias[0].net_name, "GND");
    }

    #[test]
    fn redundant_via_net_must_match_its_enclosing_net() {
        let (_dir, board, source, manifest) = fixture();
        let ses = sample_ses().replace("(net GND) (type protect)", "(net VCC) (type protect)");
        let error = parse_import_plan(&board, &source, &manifest, &ses).unwrap_err();
        assert!(error.to_string().contains("enclosing net"), "{error:#}");
    }

    #[test]
    fn freerouting_owned_ses_corpus_parses() {
        let source = include_str!("../tests/fixtures/freerouting_issue368_no_gui_v2_3_0.ses");
        let root = parse_sexp(source).unwrap();
        require_head(&root, "session").unwrap();
        assert_eq!(
            atom(&root, 1, "session id").unwrap(),
            "corney_island_wireless"
        );
        assert_eq!(one_child(&root, "routes").unwrap().head(), Some("routes"));
    }

    #[test]
    fn stale_board_revision_is_refused() {
        let (_dir, board, mut source, manifest) = fixture();
        source.push(' ');
        let error = parse_import_plan(&board, &source, &manifest, sample_ses())
            .unwrap_err()
            .to_string();
        assert!(error.contains("revision"), "{error}");
    }

    #[test]
    fn placement_change_is_refused() {
        let (_dir, board, source, manifest) = fixture();
        let ses = sample_ses().replace("1000000 -500000", "1001000 -500000");
        let error = parse_import_plan(&board, &source, &manifest, &ses)
            .unwrap_err()
            .to_string();
        assert!(error.contains("changes placement"), "{error}");
    }

    #[test]
    fn quarter_arc_lowers_to_kicad_start_mid_end() {
        let (_dir, board, source, manifest) = fixture();
        let ses = sample_ses().replace(
            "(wire (path F.Cu 2500",
            "(wire (qarc F.Cu 2500 995000 -500000 1000000 -495000 1000000 -500000)) (wire (path F.Cu 2500",
        );
        let plan = parse_import_plan(&board, &source, &manifest, &ses).unwrap();
        assert_eq!(plan.arcs.len(), 1);
        assert_eq!(plan.arcs[0].mid_x_mm, 99.64644660940672);
        assert_eq!(plan.arcs[0].mid_y_mm, 49.64644660940672);
    }

    #[test]
    fn mixed_path_and_qarc_wire_is_refused() {
        let (_dir, board, source, manifest) = fixture();
        let ses = sample_ses().replace(
            "(wire (path F.Cu 2500",
            "(wire (qarc F.Cu 2500 995000 -500000 1000000 -495000 1000000 -500000) (path F.Cu 2500",
        );
        let error = parse_import_plan(&board, &source, &manifest, &ses)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exactly one path or qarc"), "{error}");
    }
}
