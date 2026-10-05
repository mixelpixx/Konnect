//! What a library footprint carries besides its pads, drawings and fields,
//! read into the typed messages KiCad's API takes.
//!
//! Shared by `update_pcb_from_schematic`, which places new footprints, and
//! `update_footprints_from_library`, which refreshes placed ones, so both put
//! the same library data on the board. Before #789 only the refresh read it,
//! and every footprint the sync placed reached the board with no mounting
//! style, no exclusion flags, no description or keywords and no 3D model.

use anyhow::{bail, Context, Result};
use konnect_ipc::gen::kiapi;
use konnect_sexp::SexpNode;
use std::collections::BTreeSet;

/// The instance attributes a library footprint's `(attr …)` declares.
///
/// A footprint with no `(attr …)` gets KiCad's default: no mounting style,
/// which KiCad treats as unspecified. 17 of the 15,451 footprints KiCad 10.0.5
/// ships have none. An unknown token is refused rather than dropped, because
/// it would change what the board says about the part.
pub(crate) fn attributes(root: &SexpNode) -> Result<kiapi::board::types::FootprintAttributes> {
    use kiapi::board::types::FootprintMountingStyle;

    let mut attributes = kiapi::board::types::FootprintAttributes::default();
    let Some(attr) = root.find("attr") else {
        return Ok(attributes);
    };
    for value in attr.children().unwrap_or_default().iter().skip(1) {
        match value
            .as_str()
            .context("footprint attr contains a non-atom")?
        {
            "smd" => attributes.mounting_style = FootprintMountingStyle::FmsSmd as i32,
            "through_hole" => {
                attributes.mounting_style = FootprintMountingStyle::FmsThroughHole as i32
            }
            "board_only" => attributes.not_in_schematic = true,
            "exclude_from_pos_files" => attributes.exclude_from_position_files = true,
            "exclude_from_bom" => attributes.exclude_from_bill_of_materials = true,
            "allow_missing_courtyard" => attributes.exempt_from_courtyard_requirement = true,
            "dnp" => attributes.do_not_populate = true,
            "allow_soldermask_bridges" => attributes.allow_soldermask_bridges = true,
            unsupported => bail!("footprint attribute '{unsupported}' is not supported"),
        }
    }
    Ok(attributes)
}

/// The library description and keywords. KiCad keeps these on the footprint
/// definition, not the instance, in the same message type as the attributes.
pub(crate) fn description_and_keywords(
    root: &SexpNode,
) -> kiapi::board::types::FootprintAttributes {
    kiapi::board::types::FootprintAttributes {
        description: root.find_str("descr").unwrap_or_default().to_string(),
        keywords: root.find_str("tags").unwrap_or_default().to_string(),
        ..Default::default()
    }
}

/// Every `(model …)`, with its offset, scale, rotation, opacity and
/// visibility.
///
/// A hidden model is written two ways: the bare `hide` atom, and the
/// `(hide yes)` list KiCad 10 writes. 16 models in KiCad 10.0.5's own library
/// use the list form. Both are read; any other clause is refused.
///
/// The vector values go into the `*_nm` fields unconverted, because that is
/// how KiCad's own serializer packs a 3D model's millimetre offset, scale and
/// degrees. An absent opacity is KiCad's default, fully opaque; a present one
/// that is not a number is refused rather than read as that default.
pub(crate) fn models(root: &SexpNode) -> Result<Vec<kiapi::board::types::Footprint3DModel>> {
    root.find_all("model")
        .into_iter()
        .map(|model| {
            let mut hidden = false;
            for child in model.children().unwrap_or_default().iter().skip(2) {
                let Some(tag) = child.head() else {
                    if child.as_str() == Some("hide") {
                        hidden = true;
                        continue;
                    }
                    bail!("3D model contains an unsupported atom");
                };
                match tag {
                    "offset" | "scale" | "rotate" | "opacity" => {}
                    "hide" => {
                        hidden = match child.get(1).and_then(SexpNode::as_str) {
                            Some("yes") => true,
                            Some("no") => false,
                            _ => bail!("3D model hide must be yes or no"),
                        }
                    }
                    _ => bail!("3D model clause '{tag}' is not supported"),
                }
            }
            let vector = |tag: &str, default: [f64; 3]| -> Result<kiapi::common::types::Vector3D> {
                let Some(wrapper) = model.find(tag) else {
                    return Ok(kiapi::common::types::Vector3D {
                        x_nm: default[0],
                        y_nm: default[1],
                        z_nm: default[2],
                    });
                };
                let xyz = wrapper
                    .find("xyz")
                    .with_context(|| format!("3D model {tag} is missing xyz"))?;
                Ok(kiapi::common::types::Vector3D {
                    x_nm: xyz
                        .get_f64(1)
                        .with_context(|| format!("3D model {tag}.x is invalid"))?,
                    y_nm: xyz
                        .get_f64(2)
                        .with_context(|| format!("3D model {tag}.y is invalid"))?,
                    z_nm: xyz
                        .get_f64(3)
                        .with_context(|| format!("3D model {tag}.z is invalid"))?,
                })
            };
            Ok(kiapi::board::types::Footprint3DModel {
                filename: model
                    .get(1)
                    .and_then(SexpNode::as_str)
                    .context("3D model is missing its filename")?
                    .to_string(),
                scale: Some(vector("scale", [1.0, 1.0, 1.0])?),
                rotation: Some(vector("rotate", [0.0, 0.0, 0.0])?),
                offset: Some(vector("offset", [0.0, 0.0, 0.0])?),
                visible: !hidden,
                opacity: match model.find("opacity") {
                    None => 1.0,
                    Some(opacity) => opacity
                        .get_f64(1)
                        .context("3D model opacity is not a number")?,
                },
            })
        })
        .collect()
}

/// A library footprint's properties as KiCad's typed fields, in the footprint's
/// own coordinates.
///
/// The mandatory four keep their own slots; every other property is a custom
/// field. A mandatory field missing its position, layer or effects has no
/// typed form, and only its value is kept.
#[derive(Debug, Clone, Default)]
pub(crate) struct LibraryProperties {
    pub(crate) reference: Option<kiapi::board::types::Field>,
    pub(crate) value: Option<kiapi::board::types::Field>,
    pub(crate) datasheet: Option<kiapi::board::types::Field>,
    pub(crate) description: Option<kiapi::board::types::Field>,
    pub(crate) datasheet_value: Option<String>,
    pub(crate) description_value: Option<String>,
    pub(crate) custom: Vec<kiapi::board::types::Field>,
}

/// Read every `(property …)` of a library footprint into [`LibraryProperties`].
///
/// Every clause is either represented or refused: accepting a property while
/// dropping part of its authored presentation would place it differently from
/// the library. A name that appears twice is refused too, because KiCad's own
/// reader keeps one of them and which one is not something to guess.
pub(crate) fn properties(root: &SexpNode) -> Result<LibraryProperties> {
    let mut names = BTreeSet::new();
    let mut properties = LibraryProperties::default();
    for node in root.find_all("property") {
        let name = node
            .get(1)
            .and_then(SexpNode::as_str)
            .context("property is missing its name")?;
        if !names.insert(name.to_string()) {
            bail!("property '{name}' appears more than once in the library footprint");
        }

        // Mandatory and custom properties share one lossless clause validator.
        // The mandatory values keep their existing first-class IPC fields; the
        // shared parser proves that none of their authored clauses would be
        // silently ignored without requiring a typed custom Field.
        let mandatory = matches!(name, "Reference" | "Value" | "Datasheet" | "Description");
        let parsed = property(node, !mandatory)?;
        let value = node
            .get(2)
            .and_then(SexpNode::as_str)
            .with_context(|| format!("property '{name}' is missing its value"))?;
        match name {
            "Reference" => properties.reference = parsed,
            "Value" => properties.value = parsed,
            "Datasheet" => {
                properties.datasheet = parsed;
                properties.datasheet_value = Some(value.to_string());
            }
            "Description" => {
                properties.description = parsed;
                properties.description_value = Some(value.to_string());
            }
            _ => properties
                .custom
                .push(parsed.context("custom property did not produce a typed field")?),
        }
    }
    Ok(properties)
}

/// Convert a footprint property into the typed `Field` shape carried by
/// KiCad's IPC model. Unknown clauses refuse here: accepting a property while
/// dropping part of its authored presentation would place it differently from
/// the library even when its value survived.
///
/// `(unlocked yes)` is KiCad's own clause for a field that moves independently
/// of its footprint; its footprint editor writes it, and 3,353 of KiCad
/// 10.0.5's 15,451 stock footprints carry it. Without it a field is locked, as
/// the library authors it.
///
/// With `require_typed_field` off, a property missing its position, layer or
/// effects is accepted with no typed form.
fn property(
    property: &SexpNode,
    require_typed_field: bool,
) -> Result<Option<kiapi::board::types::Field>> {
    use kiapi::common::types::LockedState;

    let name = property
        .get(1)
        .and_then(SexpNode::as_str)
        .context("property is missing its name")?;
    let value = property
        .get(2)
        .and_then(SexpNode::as_str)
        .with_context(|| format!("property '{name}' is missing its value"))?;
    let mut position = None;
    let mut rotation = 0.0;
    let mut layer = None;
    let mut hidden = None;
    let mut knockout = None;
    let mut unlocked = None;
    let mut attributes = None;
    let mut identifier = None;

    for clause in property.children().unwrap_or_default().iter().skip(3) {
        let tag = clause
            .head()
            .with_context(|| format!("property '{name}' contains an unsupported atom"))?;
        match tag {
            "at" => {
                if position.is_some() {
                    bail!("property '{name}' contains duplicate 'at' clauses");
                }
                let count = clause.children().map_or(0, |children| children.len());
                if !matches!(count, 3 | 4) {
                    bail!("property '{name}' 'at' must contain x, y, and optional rotation");
                }
                let x = clause
                    .get_f64(1)
                    .with_context(|| format!("property '{name}' has an invalid X position"))?;
                let y = clause
                    .get_f64(2)
                    .with_context(|| format!("property '{name}' has an invalid Y position"))?;
                rotation = clause.get_f64(3).unwrap_or(0.0);
                if !x.is_finite() || !y.is_finite() || !rotation.is_finite() {
                    bail!("property '{name}' position and rotation must be finite");
                }
                position = Some(konnect_ipc::builders::vec2(x, y));
            }
            "layer" => {
                if layer.is_some() {
                    bail!("property '{name}' contains duplicate 'layer' clauses");
                }
                let layer_name = clause
                    .get(1)
                    .and_then(SexpNode::as_str)
                    .filter(|name| !name.is_empty())
                    .with_context(|| format!("property '{name}' has no layer name"))?;
                if clause.children().map_or(0, |children| children.len()) != 2 {
                    bail!("property '{name}' 'layer' must name exactly one layer");
                }
                layer = Some(
                    konnect_ipc::builders::try_layer_from_name(layer_name)
                        .with_context(|| format!("property '{name}' has an unsupported layer"))?
                        as i32,
                );
            }
            "hide" => {
                if hidden.is_some() {
                    bail!("property '{name}' contains duplicate 'hide' clauses");
                }
                hidden = Some(yes_no(clause, name, "hide")?);
            }
            "knockout" => {
                if knockout.is_some() {
                    bail!("property '{name}' contains duplicate 'knockout' clauses");
                }
                knockout = Some(yes_no(clause, name, "knockout")?);
            }
            "unlocked" => {
                if unlocked.is_some() {
                    bail!("property '{name}' contains duplicate 'unlocked' clauses");
                }
                unlocked = Some(yes_no(clause, name, "unlocked")?);
            }
            "uuid" | "tstamp" => {
                if let Some(previous) = identifier {
                    bail!(
                        "property '{name}' contains multiple identifier clauses ('{previous}' and '{tag}')"
                    );
                }
                identifier = Some(tag);
                if clause.children().map_or(0, |children| children.len()) != 2
                    || clause
                        .get(1)
                        .and_then(SexpNode::as_str)
                        .is_none_or(str::is_empty)
                {
                    bail!("property '{name}' '{tag}' must contain exactly one identifier");
                }
            }
            "effects" => {
                if attributes.is_some() {
                    bail!("property '{name}' contains duplicate 'effects' clauses");
                }
                attributes = Some(property_effects(clause, name)?);
            }
            unsupported => {
                bail!("property '{name}' clause '{unsupported}' is not supported losslessly")
            }
        }
    }

    if !require_typed_field && (position.is_none() || layer.is_none() || attributes.is_none()) {
        return Ok(None);
    }

    let position =
        position.with_context(|| format!("property '{name}' is missing its 'at' clause"))?;
    let layer =
        layer.with_context(|| format!("property '{name}' is missing its 'layer' clause"))?;
    let mut attributes =
        attributes.with_context(|| format!("property '{name}' is missing its 'effects' clause"))?;
    attributes.angle = Some(kiapi::common::types::Angle {
        value_degrees: rotation,
    });
    Ok(Some(kiapi::board::types::Field {
        id: None,
        name: name.to_string(),
        text: Some(kiapi::board::types::BoardText {
            // A library child's UUID is definition-local and cannot be reused
            // across placed instances. Let KiCad assign the board child ID.
            id: None,
            text: Some(kiapi::common::types::Text {
                position: Some(position),
                attributes: Some(attributes),
                text: value.to_string(),
                hyperlink: String::new(),
            }),
            layer,
            knockout: knockout.unwrap_or(false),
            locked: if unlocked.unwrap_or(false) {
                LockedState::LsUnlocked
            } else {
                LockedState::LsLocked
            } as i32,
            parent: None,
        }),
        visible: !hidden.unwrap_or(false),
    }))
}

fn yes_no(clause: &SexpNode, name: &str, tag: &str) -> Result<bool> {
    if clause.children().map_or(0, |children| children.len()) != 2 {
        bail!("property '{name}' '{tag}' must contain exactly one yes/no value");
    }
    match clause.get(1).and_then(SexpNode::as_str) {
        Some("yes") => Ok(true),
        Some("no") => Ok(false),
        _ => bail!("property '{name}' '{tag}' must be yes or no"),
    }
}

fn property_effects(
    effects: &SexpNode,
    name: &str,
) -> Result<kiapi::common::types::TextAttributes> {
    use kiapi::common::types::{HorizontalAlignment, VerticalAlignment};

    let mut font = None;
    let mut horizontal = HorizontalAlignment::HaCenter;
    let mut vertical = VerticalAlignment::VaCenter;
    let mut mirrored = false;
    for clause in effects.children().unwrap_or_default().iter().skip(1) {
        let tag = clause
            .head()
            .with_context(|| format!("property '{name}' effects contain an unsupported atom"))?;
        match tag {
            "font" => {
                if font.replace(clause).is_some() {
                    bail!("property '{name}' contains duplicate font clauses");
                }
            }
            "justify" => {
                for value in clause.children().unwrap_or_default().iter().skip(1) {
                    match value.as_str().with_context(|| {
                        format!("property '{name}' justify contains a non-atom")
                    })? {
                        "left" if horizontal == HorizontalAlignment::HaCenter => {
                            horizontal = HorizontalAlignment::HaLeft
                        }
                        "right" if horizontal == HorizontalAlignment::HaCenter => {
                            horizontal = HorizontalAlignment::HaRight
                        }
                        "top" if vertical == VerticalAlignment::VaCenter => {
                            vertical = VerticalAlignment::VaTop
                        }
                        "bottom" if vertical == VerticalAlignment::VaCenter => {
                            vertical = VerticalAlignment::VaBottom
                        }
                        "mirror" if !mirrored => mirrored = true,
                        "left" | "right" => bail!(
                            "property '{name}' has conflicting horizontal justification"
                        ),
                        "top" | "bottom" => {
                            bail!("property '{name}' has conflicting vertical justification")
                        }
                        "mirror" => bail!("property '{name}' repeats mirrored justification"),
                        unsupported => bail!(
                            "property '{name}' justification '{unsupported}' is not supported losslessly"
                        ),
                    }
                }
            }
            unsupported => bail!(
                "property '{name}' effects clause '{unsupported}' is not supported losslessly"
            ),
        }
    }
    let font =
        font.with_context(|| format!("property '{name}' effects are missing the font clause"))?;

    let mut font_name = String::new();
    let mut size = None;
    let mut thickness = None;
    let mut bold = None;
    let mut italic = None;
    let mut line_spacing = None;
    for clause in font.children().unwrap_or_default().iter().skip(1) {
        let tag = clause
            .head()
            .with_context(|| format!("property '{name}' font contains an unsupported atom"))?;
        match tag {
            "face" => {
                if !font_name.is_empty() {
                    bail!("property '{name}' contains duplicate font face clauses");
                }
                font_name = clause
                    .get(1)
                    .and_then(SexpNode::as_str)
                    .filter(|face| !face.is_empty())
                    .with_context(|| format!("property '{name}' font face is invalid"))?
                    .to_string();
            }
            "size" => {
                if size.is_some() {
                    bail!("property '{name}' contains duplicate font size clauses");
                }
                if clause.children().map_or(0, |children| children.len()) != 3 {
                    bail!("property '{name}' font size must contain width and height");
                }
                let width = clause
                    .get_f64(1)
                    .with_context(|| format!("property '{name}' font width is invalid"))?;
                let height = clause
                    .get_f64(2)
                    .with_context(|| format!("property '{name}' font height is invalid"))?;
                if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                    bail!("property '{name}' font size must be finite and positive");
                }
                size = Some((width, height));
            }
            "thickness" => {
                if thickness.is_some() {
                    bail!("property '{name}' contains duplicate font thickness clauses");
                }
                let value = clause
                    .get_f64(1)
                    .with_context(|| format!("property '{name}' font thickness is invalid"))?;
                if clause.children().map_or(0, |children| children.len()) != 2
                    || !value.is_finite()
                    || value <= 0.0
                {
                    bail!("property '{name}' font thickness must be finite and positive");
                }
                thickness = Some(value);
            }
            "bold" => {
                if bold.is_some() {
                    bail!("property '{name}' contains duplicate font 'bold' clauses");
                }
                bold = Some(yes_no(clause, name, "font bold")?);
            }
            "italic" => {
                if italic.is_some() {
                    bail!("property '{name}' contains duplicate font 'italic' clauses");
                }
                italic = Some(yes_no(clause, name, "font italic")?);
            }
            "line_spacing" => {
                if line_spacing.is_some() {
                    bail!("property '{name}' contains duplicate font 'line_spacing' clauses");
                }
                let value = clause
                    .get_f64(1)
                    .with_context(|| format!("property '{name}' line spacing is invalid"))?;
                if clause.children().map_or(0, |children| children.len()) != 2
                    || !value.is_finite()
                    || value <= 0.0
                {
                    bail!("property '{name}' line spacing must be finite and positive");
                }
                line_spacing = Some(value);
            }
            unsupported => {
                bail!("property '{name}' font clause '{unsupported}' is not supported losslessly")
            }
        }
    }
    let (width, height) =
        size.with_context(|| format!("property '{name}' font is missing its size"))?;
    Ok(kiapi::common::types::TextAttributes {
        font_name,
        horizontal_alignment: horizontal as i32,
        vertical_alignment: vertical as i32,
        angle: None,
        line_spacing: line_spacing.unwrap_or(1.0),
        stroke_width: Some(konnect_ipc::builders::distance(
            thickness.unwrap_or(width * 0.15),
        )),
        italic: italic.unwrap_or(false),
        bold: bold.unwrap_or(false),
        underlined: false,
        visible: true,
        mirrored,
        multiline: false,
        keep_upright: false,
        size: Some(konnect_ipc::builders::vec2(width, height)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiapi::board::types::FootprintMountingStyle;

    const C_0603: &str = include_str!("../../tests/fixtures/c_0603_1608metric_kicad10.kicad_mod");

    fn root(source: &str) -> SexpNode {
        konnect_sexp::parse_sexp(source).unwrap()
    }

    /// The KiCad-written 0603 capacitor: `(attr smd)`, its description and
    /// tags, and one visible model with identity transforms.
    #[test]
    fn a_kicad_library_footprint_reads_whole() {
        let root = root(C_0603);

        let attributes = attributes(&root).unwrap();
        assert_eq!(
            attributes.mounting_style,
            FootprintMountingStyle::FmsSmd as i32
        );
        assert!(!attributes.exclude_from_position_files);
        assert!(!attributes.do_not_populate);

        let text = description_and_keywords(&root);
        assert!(text
            .description
            .starts_with("Capacitor SMD 0603 (1608 Metric)"));
        assert_eq!(text.keywords, "capacitor");

        let models = models(&root).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].filename,
            "${KICAD10_3DMODEL_DIR}/Capacitor_SMD.3dshapes/C_0603_1608Metric.step"
        );
        assert!(models[0].visible);
        assert_eq!(models[0].opacity, 1.0);
        let scale = models[0].scale.as_ref().unwrap();
        assert_eq!((scale.x_nm, scale.y_nm, scale.z_nm), (1.0, 1.0, 1.0));
    }

    /// Where KiCad's own footprint libraries are installed, if they are.
    fn installed_footprint_dir() -> Option<std::path::PathBuf> {
        ["KICAD10_FOOTPRINT_DIR", "KICAD_FOOTPRINT_DIR"]
            .iter()
            .filter_map(std::env::var_os)
            .map(std::path::PathBuf::from)
            .chain(
                [
                    r"C:\KiCad\10.0\share\kicad\footprints",
                    r"C:\Program Files\KiCad\10.0\share\kicad\footprints",
                    "/usr/share/kicad/footprints",
                    "/usr/local/share/kicad/footprints",
                    "/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints",
                ]
                .iter()
                .map(std::path::PathBuf::from),
            )
            .find(|dir| dir.is_dir())
    }

    /// KiCad 10 hides a model with `(hide yes)`. Read from the installed
    /// library rather than committed, because the library is CC BY-SA (the
    /// same choice as #738's demo sheet). Skips without an installed KiCad.
    #[test]
    fn a_model_hidden_the_kicad_10_way_reads_as_hidden() {
        let Some(dir) = installed_footprint_dir() else {
            eprintln!("SKIP: no installed KiCad footprint library");
            return;
        };
        let path = dir.join("Package_SO.pretty/Texas_S-PDSO-G8_3x3mm_P0.65mm.kicad_mod");
        let Ok(source) = std::fs::read_to_string(&path) else {
            eprintln!("SKIP: {} is not installed", path.display());
            return;
        };
        let models = models(&root(&source)).unwrap();
        assert_eq!(models.len(), 1);
        assert!(
            !models[0].visible,
            "{} is hidden in the library",
            models[0].filename
        );
    }

    /// Opacity defaults only when the library leaves it out. A value that is
    /// there and readable is kept; one that is there and unreadable refuses the
    /// model, where it used to become full opacity without a word.
    #[test]
    fn opacity_defaults_only_when_absent() {
        let with = |opacity: &str| {
            root(&format!(
                "(footprint \"X\" (model \"m.step\" (offset (xyz 0 0 0)) {opacity}))"
            ))
        };
        assert_eq!(models(&with("")).unwrap()[0].opacity, 1.0);
        assert_eq!(models(&with("(opacity 0.4)")).unwrap()[0].opacity, 0.4);
        let refusal = models(&with("(opacity dim)")).unwrap_err();
        assert!(
            format!("{refusal:#}").contains("3D model opacity is not a number"),
            "{refusal:#}"
        );
        assert!(models(&with("(opacity)")).is_err());
    }

    /// The KiCad-written 0603's properties: Reference and Value with the
    /// library's own layer, visibility and font, and its one custom property.
    /// None carries `(unlocked …)`, so each is locked, as the library has it.
    #[test]
    fn a_kicad_library_footprint_reads_its_properties_whole() {
        use kiapi::board::types::BoardLayer;
        use kiapi::common::types::LockedState;

        let properties = properties(&root(C_0603)).unwrap();

        let layer_visible_locked = |field: &kiapi::board::types::Field| {
            let text = field.text.as_ref().unwrap();
            (text.layer, field.visible, text.locked)
        };
        let reference = properties.reference.as_ref().expect("a typed Reference");
        assert_eq!(
            layer_visible_locked(reference),
            (
                BoardLayer::BlFSilkS as i32,
                true,
                LockedState::LsLocked as i32
            )
        );
        let value = properties.value.as_ref().expect("a typed Value");
        assert_eq!(
            layer_visible_locked(value),
            (
                BoardLayer::BlFFab as i32,
                true,
                LockedState::LsLocked as i32
            )
        );
        let font = value.text.as_ref().unwrap().text.as_ref().unwrap();
        let attributes = font.attributes.as_ref().unwrap();
        assert_eq!(attributes.stroke_width.as_ref().unwrap().value_nm, 150_000);
        assert_eq!(attributes.line_spacing, 1.0);

        assert_eq!(properties.custom.len(), 1);
        let generator = &properties.custom[0];
        assert_eq!(generator.name, "KiLib_Generator");
        assert_eq!(
            generator.text.as_ref().unwrap().text.as_ref().unwrap().text,
            "SMD_2terminal_chip_molded"
        );
        assert_eq!(
            layer_visible_locked(generator),
            (
                BoardLayer::BlFSilkS as i32,
                false,
                LockedState::LsLocked as i32
            )
        );
        assert!(properties.datasheet.is_none() && properties.description.is_none());
    }

    /// `(unlocked yes)`, as KiCad's footprint editor writes it, is read as an
    /// unlocked field rather than refused; `(unlocked no)` is locked; anything
    /// else is refused.
    #[test]
    fn unlocked_is_read_as_the_fields_locked_state() {
        use kiapi::common::types::LockedState;

        let with = |unlocked: &str| {
            C_0603.replace(
                "(property \"KiLib_Generator\" \"SMD_2terminal_chip_molded\"",
                &format!("(property \"KiLib_Generator\" \"SMD_2terminal_chip_molded\" {unlocked}"),
            )
        };
        let locked_state = |source: &str| {
            let properties = properties(&root(source))?;
            Ok::<_, anyhow::Error>(properties.custom[0].text.as_ref().unwrap().locked)
        };
        assert_ne!(with("(unlocked yes)"), C_0603);
        assert_eq!(
            locked_state(&with("(unlocked yes)")).unwrap(),
            LockedState::LsUnlocked as i32
        );
        assert_eq!(
            locked_state(&with("(unlocked no)")).unwrap(),
            LockedState::LsLocked as i32
        );
        let refusal = locked_state(&with("(unlocked maybe)")).unwrap_err();
        assert!(format!("{refusal:#}").contains("unlocked"), "{refusal:#}");
    }

    #[test]
    fn hide_no_is_visible_and_anything_else_is_refused() {
        let with = |hide: &str| {
            root(&format!(
                "(footprint \"X\" (model \"m.step\" (offset (xyz 0 0 0)) {hide}))"
            ))
        };
        assert!(models(&with("(hide no)")).unwrap()[0].visible);
        assert!(!models(&with("(hide yes)")).unwrap()[0].visible);
        assert!(!models(&with("hide")).unwrap()[0].visible);
        assert!(models(&with("(hide maybe)")).is_err());
    }
}
