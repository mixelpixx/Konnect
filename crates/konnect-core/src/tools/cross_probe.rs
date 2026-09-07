//! Deterministic saved-structure cross-probe resolution.
//!
//! A KiCad PCB footprint carries the canonical schematic symbol path that
//! created it. This resolver uses that stable linkage in both directions and
//! requires the caller's explicit schematic hierarchy instance to agree.

use super::navigation_target::{
    resolve_navigation_target, NavigationTargetError, NavigationTargetRequest,
    ResolvedNavigationTarget,
};
use konnect_ipc::{IpcEditorKind, IpcProjectIdentity, IpcSheetInstancePath};
use konnect_sexp::SexpNode;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CrossProbeDirection {
    SchematicToPcb,
    PcbToSchematic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrossProbeRequest {
    pub project: IpcProjectIdentity,
    pub schematic_document_path: PathBuf,
    pub pcb_document_path: PathBuf,
    pub schematic_sheet_instance_path: IpcSheetInstancePath,
    pub source_editor: IpcEditorKind,
    pub source_object_kiid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CrossProbeEvidence {
    pub source: String,
    pub schematic_symbol_path: String,
    pub schematic_document_path: String,
    pub pcb_document_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CrossProbeResolution {
    pub direction: CrossProbeDirection,
    pub source: ResolvedNavigationTarget,
    pub destination: ResolvedNavigationTarget,
    pub evidence: CrossProbeEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CrossProbeError {
    Target(NavigationTargetError),
    UnresolvedDestination {
        source_kiid: String,
        reason: String,
        candidates: Vec<String>,
    },
}

impl std::fmt::Display for CrossProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Target(error) => error.fmt(formatter),
            Self::UnresolvedDestination {
                source_kiid,
                reason,
                ..
            } => write!(
                formatter,
                "cannot resolve cross-probe destination for '{source_kiid}': {reason}"
            ),
        }
    }
}

impl std::error::Error for CrossProbeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Target(error) => Some(error),
            Self::UnresolvedDestination { .. } => None,
        }
    }
}

impl From<NavigationTargetError> for CrossProbeError {
    fn from(error: NavigationTargetError) -> Self {
        Self::Target(error)
    }
}

pub(crate) fn resolve_cross_probe(
    request: &CrossProbeRequest,
) -> Result<CrossProbeResolution, CrossProbeError> {
    match request.source_editor {
        IpcEditorKind::Schematic => resolve_schematic_to_pcb(request),
        IpcEditorKind::Pcb => resolve_pcb_to_schematic(request),
    }
}

fn resolve_schematic_to_pcb(
    request: &CrossProbeRequest,
) -> Result<CrossProbeResolution, CrossProbeError> {
    let source = resolve_navigation_target(&NavigationTargetRequest {
        editor: IpcEditorKind::Schematic,
        project: request.project.clone(),
        document_path: request.schematic_document_path.clone(),
        sheet_instance_path: Some(request.schematic_sheet_instance_path.clone()),
        object_kiid: Some(request.source_object_kiid.clone()),
        human_reference: None,
    })?;
    if source.object.object_type != "schematic_symbol" {
        return Err(unresolved(
            request,
            "only an exact schematic symbol can cross-probe to a footprint",
            Vec::new(),
        ));
    }
    let symbol_path =
        expected_symbol_path(&request.schematic_sheet_instance_path, &source.object.kiid)?;
    let board = read_tree(&request.pcb_document_path, request)?;
    let matches = board
        .find_all("footprint")
        .into_iter()
        .filter(|footprint| footprint.find_str("path") == Some(symbol_path.as_str()))
        .filter_map(footprint_identity)
        .collect::<Vec<_>>();
    let (destination_kiid, destination_reference) = one_destination(request, &matches)?;
    if source.object.human_reference.as_deref() != Some(destination_reference.as_str()) {
        return Err(unresolved(
            request,
            "the linked footprint reference does not match the source symbol reference",
            matches
                .iter()
                .map(|(kiid, reference)| format!("{reference}:{kiid}"))
                .collect(),
        ));
    }
    let destination = resolve_navigation_target(&NavigationTargetRequest {
        editor: IpcEditorKind::Pcb,
        project: request.project.clone(),
        document_path: request.pcb_document_path.clone(),
        sheet_instance_path: None,
        object_kiid: Some(destination_kiid),
        human_reference: None,
    })?;
    Ok(resolution(
        CrossProbeDirection::SchematicToPcb,
        request,
        source,
        destination,
        symbol_path,
    ))
}

fn resolve_pcb_to_schematic(
    request: &CrossProbeRequest,
) -> Result<CrossProbeResolution, CrossProbeError> {
    let source = resolve_navigation_target(&NavigationTargetRequest {
        editor: IpcEditorKind::Pcb,
        project: request.project.clone(),
        document_path: request.pcb_document_path.clone(),
        sheet_instance_path: None,
        object_kiid: Some(request.source_object_kiid.clone()),
        human_reference: None,
    })?;
    if source.object.object_type != "pcb_footprint" {
        return Err(unresolved(
            request,
            "only an exact PCB footprint can cross-probe to a symbol",
            Vec::new(),
        ));
    }
    let board = read_tree(&request.pcb_document_path, request)?;
    let footprint = board
        .find_all("footprint")
        .into_iter()
        .filter(|footprint| footprint.find_str("uuid") == Some(source.object.kiid.as_str()))
        .collect::<Vec<_>>();
    let [footprint] = footprint.as_slice() else {
        return Err(unresolved(
            request,
            "the exact source footprint is missing or duplicated in saved structure",
            footprint
                .iter()
                .filter_map(|item| item.find_str("uuid").map(str::to_string))
                .collect(),
        ));
    };
    let symbol_path = footprint.find_str("path").ok_or_else(|| {
        unresolved(
            request,
            "the source footprint has no stable schematic symbol path",
            Vec::new(),
        )
    })?;
    let symbol_kiid = symbol_path
        .split('/')
        .rfind(|part| !part.is_empty())
        .ok_or_else(|| {
            unresolved(
                request,
                "the source footprint carries an empty schematic symbol path",
                Vec::new(),
            )
        })?;
    let expected = expected_symbol_path(&request.schematic_sheet_instance_path, symbol_kiid)?;
    if expected != symbol_path {
        return Err(unresolved(
            request,
            "the footprint linkage belongs to another hierarchical sheet instance",
            vec![symbol_path.to_string(), expected],
        ));
    }
    let destination = resolve_navigation_target(&NavigationTargetRequest {
        editor: IpcEditorKind::Schematic,
        project: request.project.clone(),
        document_path: request.schematic_document_path.clone(),
        sheet_instance_path: Some(request.schematic_sheet_instance_path.clone()),
        object_kiid: Some(symbol_kiid.to_string()),
        human_reference: None,
    })?;
    let source_reference = footprint_reference(footprint);
    if source_reference.as_deref() != destination.object.human_reference.as_deref() {
        return Err(unresolved(
            request,
            "the linked symbol reference does not match the source footprint reference",
            source_reference.into_iter().collect(),
        ));
    }
    Ok(resolution(
        CrossProbeDirection::PcbToSchematic,
        request,
        source,
        destination,
        symbol_path.to_string(),
    ))
}

fn resolution(
    direction: CrossProbeDirection,
    request: &CrossProbeRequest,
    source: ResolvedNavigationTarget,
    destination: ResolvedNavigationTarget,
    symbol_path: String,
) -> CrossProbeResolution {
    CrossProbeResolution {
        direction,
        source,
        destination,
        evidence: CrossProbeEvidence {
            source: "saved_kicad_footprint_symbol_path".to_string(),
            schematic_symbol_path: symbol_path,
            schematic_document_path: request.schematic_document_path.display().to_string(),
            pcb_document_path: request.pcb_document_path.display().to_string(),
        },
    }
}

fn read_tree(path: &PathBuf, request: &CrossProbeRequest) -> Result<SexpNode, CrossProbeError> {
    let source = std::fs::read_to_string(path).map_err(|error| {
        unresolved(
            request,
            &format!("cannot read destination document: {error}"),
            Vec::new(),
        )
    })?;
    konnect_sexp::parse_sexp(&source).map_err(|error| {
        unresolved(
            request,
            &format!("cannot parse destination document: {error}"),
            Vec::new(),
        )
    })
}

fn expected_symbol_path(
    sheet_instance: &IpcSheetInstancePath,
    symbol_kiid: &str,
) -> Result<String, CrossProbeError> {
    if sheet_instance.kiids.is_empty() || symbol_kiid.is_empty() {
        return Err(CrossProbeError::UnresolvedDestination {
            source_kiid: symbol_kiid.to_string(),
            reason: "schematic instance path and symbol KIID must be non-empty".to_string(),
            candidates: Vec::new(),
        });
    }
    let mut parts = sheet_instance
        .kiids
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<_>>();
    parts.push(symbol_kiid.to_string());
    Ok(format!("/{}", parts.join("/")))
}

fn footprint_identity(footprint: &SexpNode) -> Option<(String, String)> {
    Some((
        footprint.find_str("uuid")?.to_string(),
        footprint_reference(footprint)?,
    ))
}

fn footprint_reference(footprint: &SexpNode) -> Option<String> {
    footprint
        .find_all("property")
        .into_iter()
        .find(|property| property.get(1).and_then(SexpNode::as_str) == Some("Reference"))
        .and_then(|property| property.get(2))
        .and_then(SexpNode::as_str)
        .map(str::to_string)
        .or_else(|| {
            footprint
                .find_all("fp_text")
                .into_iter()
                .find(|text| text.get(1).and_then(SexpNode::as_str) == Some("reference"))
                .and_then(|text| text.get(2))
                .and_then(SexpNode::as_str)
                .map(str::to_string)
        })
}

fn one_destination(
    request: &CrossProbeRequest,
    matches: &[(String, String)],
) -> Result<(String, String), CrossProbeError> {
    match matches {
        [destination] => Ok(destination.clone()),
        [] => Err(unresolved(
            request,
            "no footprint carries the exact schematic symbol path",
            Vec::new(),
        )),
        many => Err(unresolved(
            request,
            "more than one footprint carries the exact schematic symbol path",
            many.iter()
                .map(|(kiid, reference)| format!("{reference}:{kiid}"))
                .collect(),
        )),
    }
}

fn unresolved(
    request: &CrossProbeRequest,
    reason: &str,
    candidates: Vec<String>,
) -> CrossProbeError {
    CrossProbeError::UnresolvedDestination {
        source_kiid: request.source_object_kiid.clone(),
        reason: reason.to_string(),
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &std::path::Path, footprint_body: &str) -> CrossProbeRequest {
        std::fs::write(root.join("nav.kicad_pro"), "{}").unwrap();
        std::fs::write(
            root.join("nav.kicad_sch"),
            "(kicad_sch (uuid \"root\") \
             (symbol (lib_id \"Device:C\") (at 1 2) (uuid \"sym-c10\") \
               (property \"Reference\" \"C10\") \
               (instances (project \"nav\" (path \"/root\" (reference \"C10\") (unit 1))))) \
             (sheet_instances (path \"/\" (page \"1\"))))",
        )
        .unwrap();
        std::fs::write(
            root.join("nav.kicad_pcb"),
            format!("(kicad_pcb {footprint_body})"),
        )
        .unwrap();
        CrossProbeRequest {
            project: IpcProjectIdentity {
                name: "nav".to_string(),
                path: root.display().to_string(),
            },
            schematic_document_path: root.join("nav.kicad_sch"),
            pcb_document_path: root.join("nav.kicad_pcb"),
            schematic_sheet_instance_path: IpcSheetInstancePath {
                kiids: vec!["root".to_string()],
                human_readable: "/".to_string(),
            },
            source_editor: IpcEditorKind::Schematic,
            source_object_kiid: "sym-c10".to_string(),
        }
    }

    fn footprint(uuid: &str, path: &str, reference: &str) -> String {
        format!(
            "(footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"{uuid}\") (property \"Reference\" \"{reference}\") (path \"{path}\"))"
        )
    }

    #[test]
    fn exact_symbol_and_footprint_cross_probe_in_both_directions() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = fixture(temp.path(), &footprint("fp-c10", "/sym-c10", "C10"));
        let forward = resolve_cross_probe(&request).unwrap();
        assert_eq!(forward.destination.object.kiid, "fp-c10");
        assert_eq!(forward.evidence.source, "saved_kicad_footprint_symbol_path");

        request.source_editor = IpcEditorKind::Pcb;
        request.source_object_kiid = "fp-c10".to_string();
        let reverse = resolve_cross_probe(&request).unwrap();
        assert_eq!(reverse.destination.object.kiid, "sym-c10");
        assert_eq!(reverse.direction, CrossProbeDirection::PcbToSchematic);
    }

    #[test]
    fn navigation_mvp_resolves_c10_then_its_exact_footprint_by_stable_linkage() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = fixture(temp.path(), &footprint("fp-c10", "/sym-c10", "C10"));
        let symbol = resolve_navigation_target(&NavigationTargetRequest {
            editor: IpcEditorKind::Schematic,
            project: request.project.clone(),
            document_path: request.schematic_document_path.clone(),
            sheet_instance_path: Some(request.schematic_sheet_instance_path.clone()),
            object_kiid: None,
            human_reference: Some("C10".to_string()),
        })
        .unwrap();
        assert_eq!(symbol.object.kiid, "sym-c10");
        assert_eq!(
            symbol.structural_evidence.resolver,
            "unique_human_reference"
        );

        request.source_object_kiid = symbol.object.kiid;
        let footprint = resolve_cross_probe(&request).unwrap();
        assert_eq!(footprint.destination.object.kiid, "fp-c10");
        assert_eq!(footprint.evidence.schematic_symbol_path, "/sym-c10");
    }

    #[test]
    fn hierarchical_symbol_path_includes_the_exact_sheet_instance() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nav.kicad_pro"), "{}").unwrap();
        std::fs::write(
            temp.path().join("nav.kicad_sch"),
            "(kicad_sch (uuid \"root\") \
             (sheet (at 1 1) (size 10 10) (uuid \"sheet-a\") \
               (property \"Sheetname\" \"Child\") \
               (property \"Sheetfile\" \"child.kicad_sch\")) \
             (sheet_instances (path \"/\" (page \"1\"))))",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("child.kicad_sch"),
            "(kicad_sch (uuid \"child-file\") \
             (symbol (lib_id \"Device:C\") (at 1 2) (uuid \"sym-c10\") \
               (property \"Reference\" \"C10\") \
               (instances (project \"nav\" (path \"/root/sheet-a\" (reference \"C10\") (unit 1))))) \
             (sheet_instances (path \"/root/sheet-a\" (page \"2\"))))",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("nav.kicad_pcb"),
            format!(
                "(kicad_pcb {})",
                footprint("fp-c10", "/sheet-a/sym-c10", "C10")
            ),
        )
        .unwrap();
        let request = CrossProbeRequest {
            project: IpcProjectIdentity {
                name: "nav".to_string(),
                path: temp.path().display().to_string(),
            },
            schematic_document_path: temp.path().join("child.kicad_sch"),
            pcb_document_path: temp.path().join("nav.kicad_pcb"),
            schematic_sheet_instance_path: IpcSheetInstancePath {
                kiids: vec!["root".to_string(), "sheet-a".to_string()],
                human_readable: "/Child".to_string(),
            },
            source_editor: IpcEditorKind::Schematic,
            source_object_kiid: "sym-c10".to_string(),
        };
        let resolved = resolve_cross_probe(&request).unwrap();
        assert_eq!(resolved.evidence.schematic_symbol_path, "/sheet-a/sym-c10");
        assert_eq!(resolved.destination.object.kiid, "fp-c10");
    }

    #[test]
    fn missing_duplicate_and_reference_mismatch_destinations_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let missing = fixture(temp.path(), &footprint("fp-c10", "/other", "C10"));
        assert!(matches!(
            resolve_cross_probe(&missing),
            Err(CrossProbeError::UnresolvedDestination { .. })
        ));

        let duplicate_body = format!(
            "{} {}",
            footprint("fp-a", "/sym-c10", "C10"),
            footprint("fp-b", "/sym-c10", "C10")
        );
        let duplicate = fixture(temp.path(), &duplicate_body);
        let Err(CrossProbeError::UnresolvedDestination { candidates, .. }) =
            resolve_cross_probe(&duplicate)
        else {
            panic!("duplicate linkage must be unresolved");
        };
        assert_eq!(candidates.len(), 2);

        let mismatch = fixture(temp.path(), &footprint("fp-c10", "/sym-c10", "C11"));
        assert!(matches!(
            resolve_cross_probe(&mismatch),
            Err(CrossProbeError::UnresolvedDestination { .. })
        ));
    }

    #[test]
    fn wrong_hierarchy_instance_and_stale_source_are_never_retargeted() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = fixture(temp.path(), &footprint("fp-c10", "/sym-c10", "C10"));
        request.schematic_sheet_instance_path.kiids = vec!["other-root".to_string()];
        assert!(matches!(
            resolve_cross_probe(&request),
            Err(CrossProbeError::Target(_))
        ));

        request.schematic_sheet_instance_path.kiids = vec!["root".to_string()];
        request.source_object_kiid = "gone".to_string();
        assert!(matches!(
            resolve_cross_probe(&request),
            Err(CrossProbeError::Target(_))
        ));
    }
}
