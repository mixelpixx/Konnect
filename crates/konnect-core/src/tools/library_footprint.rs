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
