//! Pure deterministic navigation-target resolution.
//!
//! This module reads saved KiCad structure only. Live editor/document proof is
//! performed separately by `editor_navigation` so results never blur file
//! evidence with IPC evidence.

use konnect_ipc::{IpcEditorKind, IpcProjectIdentity, IpcSheetInstancePath};
use konnect_sexp::SexpNode;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NavigationTargetRequest {
    pub editor: IpcEditorKind,
    pub project: IpcProjectIdentity,
    pub document_path: PathBuf,
    pub sheet_instance_path: Option<IpcSheetInstancePath>,
    pub object_kiid: Option<String>,
    pub human_reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResolvedNavigationObject {
    pub kiid: String,
    pub object_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human_reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct NavigationStructuralEvidence {
    pub source: String,
    pub project_file: String,
    pub document_path: String,
    pub resolver: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResolvedNavigationTarget {
    pub project: IpcProjectIdentity,
    pub editor: IpcEditorKind,
    pub document_path: String,
    pub sheet_instance_path: Option<IpcSheetInstancePath>,
    pub object: ResolvedNavigationObject,
    pub structural_evidence: NavigationStructuralEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NavigationTargetErrorKind {
    WrongProject,
    WrongDocument,
    WrongSheetInstance,
    AmbiguousTarget,
    StaleTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NavigationTargetError {
    pub kind: NavigationTargetErrorKind,
    pub target: String,
    pub candidates: Vec<String>,
    pub reason: String,
}

impl std::fmt::Display for NavigationTargetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cannot resolve navigation target '{}': {}",
            self.target, self.reason
        )
    }
}

impl std::error::Error for NavigationTargetError {}

pub(crate) fn resolve_navigation_target(
    request: &NavigationTargetRequest,
) -> Result<ResolvedNavigationTarget, NavigationTargetError> {
    let project_file = PathBuf::from(&request.project.path)
        .join(&request.project.name)
        .with_extension("kicad_pro");
    if !project_file.is_file() {
        return Err(target_error(
            NavigationTargetErrorKind::WrongProject,
            &request.project.name,
            Vec::new(),
            format!(
                "the explicit project file '{}' does not exist",
                project_file.display()
            ),
        ));
    }
    if !request.document_path.is_file() {
        return Err(target_error(
            NavigationTargetErrorKind::StaleTarget,
            &request.document_path.display().to_string(),
            Vec::new(),
            "the saved document no longer exists".to_string(),
        ));
    }

    let (tree, instance_path) = match request.editor {
        IpcEditorKind::Schematic => {
            if !has_extension(&request.document_path, "kicad_sch") {
                return Err(wrong_document(request, "expected a .kicad_sch document"));
            }
            let requested_instance = request.sheet_instance_path.as_ref().ok_or_else(|| {
                target_error(
                    NavigationTargetErrorKind::WrongSheetInstance,
                    &request.document_path.display().to_string(),
                    Vec::new(),
                    "schematic resolution requires an explicit instance path".to_string(),
                )
            })?;
            validate_schematic_context(request, &project_file, requested_instance)?;
            let (_, tree) = konnect_sexp::schematic::read_schematic(&request.document_path)
                .map_err(|error| {
                    target_error(
                        NavigationTargetErrorKind::StaleTarget,
                        &request.document_path.display().to_string(),
                        Vec::new(),
                        format!("saved schematic cannot be parsed: {error}"),
                    )
                })?;
            (tree, Some(requested_instance.clone()))
        }
        IpcEditorKind::Pcb => {
            if !has_extension(&request.document_path, "kicad_pcb") {
                return Err(wrong_document(request, "expected a .kicad_pcb document"));
            }
            if request.sheet_instance_path.is_some() {
                return Err(target_error(
                    NavigationTargetErrorKind::WrongSheetInstance,
                    &request.document_path.display().to_string(),
                    Vec::new(),
                    "PCB targets cannot carry a schematic instance path".to_string(),
                ));
            }
            let source = std::fs::read_to_string(&request.document_path).map_err(|error| {
                target_error(
                    NavigationTargetErrorKind::StaleTarget,
                    &request.document_path.display().to_string(),
                    Vec::new(),
                    format!("saved PCB cannot be read: {error}"),
                )
            })?;
            let tree = konnect_sexp::parse_sexp(&source).map_err(|error| {
                target_error(
                    NavigationTargetErrorKind::StaleTarget,
                    &request.document_path.display().to_string(),
                    Vec::new(),
                    format!("saved PCB cannot be parsed: {error}"),
                )
            })?;
            (tree, None)
        }
    };

    let object = resolve_object(request, &tree)?;
    Ok(ResolvedNavigationTarget {
        project: request.project.clone(),
        editor: request.editor,
        document_path: request.document_path.display().to_string(),
        sheet_instance_path: instance_path,
        object,
        structural_evidence: NavigationStructuralEvidence {
            source: "saved_kicad_structure".to_string(),
            project_file: project_file.display().to_string(),
            document_path: request.document_path.display().to_string(),
            resolver: if request.object_kiid.is_some() {
                "exact_kiid".to_string()
            } else {
                "unique_human_reference".to_string()
            },
        },
    })
}

fn validate_schematic_context(
    request: &NavigationTargetRequest,
    project_file: &Path,
    requested_instance: &IpcSheetInstancePath,
) -> Result<(), NavigationTargetError> {
    let ownership = super::resolve_schematic_ownership(&request.document_path)
        .map_err(|error| map_schematic_ownership_error(&request.document_path, error))?;
    if let Some(owner) = &ownership {
        if canonical(&owner.project_file) != canonical(project_file) {
            return Err(target_error(
                NavigationTargetErrorKind::WrongProject,
                &request.project.name,
                vec![owner.project_file.display().to_string()],
                "the saved hierarchy belongs to another project".to_string(),
            ));
        }
    } else if canonical(&project_file.with_extension("kicad_sch"))
        != canonical(&request.document_path)
    {
        return Err(wrong_document(
            request,
            "the schematic is not reachable from the explicit project root",
        ));
    }

    let mut schematic =
        konnect_schematic_editor::Schematic::load(&request.document_path).map_err(|error| {
            target_error(
                NavigationTargetErrorKind::StaleTarget,
                &request.document_path.display().to_string(),
                Vec::new(),
                format!("saved schematic cannot be loaded: {error}"),
            )
        })?;
    let context =
        super::sheet_instance_context(&request.document_path, &mut schematic).map_err(|error| {
            target_error(
                NavigationTargetErrorKind::StaleTarget,
                &request.document_path.display().to_string(),
                Vec::new(),
                error.to_string(),
            )
        })?;
    if context.project_name != request.project.name {
        return Err(target_error(
            NavigationTargetErrorKind::WrongProject,
            &request.project.name,
            vec![context.project_name],
            "schematic instance metadata names another project".to_string(),
        ));
    }
    super::validate_sheet_instance_state(&request.document_path, &schematic, &context).map_err(
        |error| {
            target_error(
                NavigationTargetErrorKind::StaleTarget,
                &request.document_path.display().to_string(),
                Vec::new(),
                error.to_string(),
            )
        },
    )?;

    let requested = format!("/{}", requested_instance.kiids.join("/"));
    if !context.instance_paths.contains(&requested) {
        return Err(target_error(
            NavigationTargetErrorKind::WrongSheetInstance,
            &requested,
            context.instance_paths,
            "the requested hierarchy instance does not own this schematic document".to_string(),
        ));
    }
    Ok(())
}

fn map_schematic_ownership_error(
    target: &Path,
    error: super::SchematicTargetError,
) -> NavigationTargetError {
    let reason = error.to_string();
    let (kind, candidates) = match error {
        super::SchematicTargetError::ProjectConflict { roots, .. } => (
            NavigationTargetErrorKind::AmbiguousTarget,
            roots
                .into_iter()
                .map(|root| root.display().to_string())
                .collect(),
        ),
        super::SchematicTargetError::DiscoveryFailed { .. }
        | super::SchematicTargetError::StaleTarget { .. } => {
            (NavigationTargetErrorKind::StaleTarget, Vec::new())
        }
    };
    target_error(kind, &target.display().to_string(), candidates, reason)
}

fn resolve_object(
    request: &NavigationTargetRequest,
    tree: &SexpNode,
) -> Result<ResolvedNavigationObject, NavigationTargetError> {
    match (&request.object_kiid, &request.human_reference) {
        (Some(kiid), None) => resolve_by_kiid(request, tree, kiid),
        (None, Some(reference)) => resolve_by_reference(request, tree, reference),
        _ => Err(target_error(
            NavigationTargetErrorKind::StaleTarget,
            &request.document_path.display().to_string(),
            Vec::new(),
            "provide exactly one of object_kiid or human_reference".to_string(),
        )),
    }
}

fn resolve_by_kiid(
    request: &NavigationTargetRequest,
    tree: &SexpNode,
    kiid: &str,
) -> Result<ResolvedNavigationObject, NavigationTargetError> {
    let mut candidates = Vec::new();
    collect_uuid_objects(tree, request.editor, &mut candidates);
    let matches = candidates
        .into_iter()
        .filter(|object| object.kiid == kiid)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [target] => Ok(target.clone()),
        [] => Err(target_error(
            NavigationTargetErrorKind::StaleTarget,
            kiid,
            Vec::new(),
            "no object with this KIID exists in the exact saved document".to_string(),
        )),
        many => Err(target_error(
            NavigationTargetErrorKind::AmbiguousTarget,
            kiid,
            many.iter()
                .map(|object| format!("{}:{}", object.object_type, object.kiid))
                .collect(),
            "the saved document contains a duplicate KIID".to_string(),
        )),
    }
}

fn resolve_by_reference(
    request: &NavigationTargetRequest,
    tree: &SexpNode,
    reference: &str,
) -> Result<ResolvedNavigationObject, NavigationTargetError> {
    let mut candidates = match request.editor {
        IpcEditorKind::Schematic => konnect_sexp::schematic::extract_symbol_instances(tree)
            .into_iter()
            .filter(|symbol| symbol.reference == reference)
            .map(|symbol| ResolvedNavigationObject {
                kiid: symbol.uuid.unwrap_or_default(),
                object_type: "schematic_symbol".to_string(),
                human_reference: Some(symbol.reference),
            })
            .collect::<Vec<_>>(),
        IpcEditorKind::Pcb => konnect_sexp::board::footprints(tree)
            .into_iter()
            .filter_map(|footprint| {
                let observed = footprint_reference(footprint)?;
                (observed == reference).then(|| ResolvedNavigationObject {
                    kiid: footprint.find_str("uuid").unwrap_or_default().to_string(),
                    object_type: "pcb_footprint".to_string(),
                    human_reference: Some(observed),
                })
            })
            .collect::<Vec<_>>(),
    };
    candidates.sort_by(|left, right| left.kiid.cmp(&right.kiid));
    if candidates.iter().any(|candidate| candidate.kiid.is_empty()) {
        return Err(target_error(
            NavigationTargetErrorKind::StaleTarget,
            reference,
            Vec::new(),
            "a matching saved object has no stable KIID".to_string(),
        ));
    }
    match candidates.as_slice() {
        [target] => Ok(target.clone()),
        [] => Err(target_error(
            NavigationTargetErrorKind::StaleTarget,
            reference,
            Vec::new(),
            "the human reference does not exist in the exact saved document".to_string(),
        )),
        many => Err(target_error(
            NavigationTargetErrorKind::AmbiguousTarget,
            reference,
            many.iter()
                .map(|object| format!("{}:{}", object.object_type, object.kiid))
                .collect(),
            "the human reference resolves to more than one object".to_string(),
        )),
    }
}

fn collect_uuid_objects(
    node: &SexpNode,
    editor: IpcEditorKind,
    output: &mut Vec<ResolvedNavigationObject>,
) {
    let Some(children) = node.children() else {
        return;
    };
    let head = node.head().unwrap_or("");
    if head == "lib_symbols" {
        return;
    }
    if !matches!(head, "kicad_sch" | "kicad_pcb") {
        if let Some(kiid) = node.find_str("uuid").filter(|id| !id.is_empty()) {
            output.push(ResolvedNavigationObject {
                kiid: kiid.to_string(),
                object_type: object_type(editor, head),
                human_reference: match editor {
                    IpcEditorKind::Schematic if head == "symbol" => node
                        .find_all("property")
                        .into_iter()
                        .find(|property| {
                            property.get(1).and_then(SexpNode::as_str) == Some("Reference")
                        })
                        .and_then(|property| property.get(2))
                        .and_then(SexpNode::as_str)
                        .map(str::to_string),
                    IpcEditorKind::Pcb if head == "footprint" => footprint_reference(node),
                    _ => None,
                },
            });
        }
    }
    for child in children.iter().skip(1) {
        if child.head() != Some("uuid") {
            collect_uuid_objects(child, editor, output);
        }
    }
}

fn object_type(editor: IpcEditorKind, head: &str) -> String {
    let prefix = editor.as_str();
    let kind = match (editor, head) {
        (IpcEditorKind::Schematic, "symbol") => "symbol",
        (IpcEditorKind::Pcb, "footprint") => "footprint",
        (_, "property") => "field",
        (_, other) if !other.is_empty() => other,
        _ => "object",
    };
    format!("{prefix}_{kind}")
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

fn wrong_document(request: &NavigationTargetRequest, reason: &str) -> NavigationTargetError {
    target_error(
        NavigationTargetErrorKind::WrongDocument,
        &request.document_path.display().to_string(),
        Vec::new(),
        reason.to_string(),
    )
}

fn target_error(
    kind: NavigationTargetErrorKind,
    target: &str,
    candidates: Vec<String>,
    reason: String,
) -> NavigationTargetError {
    NavigationTargetError {
        kind,
        target: target.to_string(),
        candidates,
        reason,
    }
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, source: &str) {
        fs::write(path, source).expect("write fixture");
    }

    #[test]
    fn schematic_ownership_failures_preserve_current_conflict_and_discovery_semantics() {
        let target = PathBuf::from("child.kicad_sch");
        let conflict = map_schematic_ownership_error(
            &target,
            super::super::SchematicTargetError::ProjectConflict {
                target: target.clone(),
                roots: vec![PathBuf::from("a.kicad_sch"), PathBuf::from("b.kicad_sch")],
            },
        );
        assert_eq!(conflict.kind, NavigationTargetErrorKind::AmbiguousTarget);
        assert_eq!(conflict.candidates.len(), 2);

        let discovery = map_schematic_ownership_error(
            &target,
            super::super::SchematicTargetError::DiscoveryFailed {
                directory: PathBuf::from("."),
                reason: "unreadable".to_string(),
            },
        );
        assert_eq!(discovery.kind, NavigationTargetErrorKind::StaleTarget);
        assert!(discovery.candidates.is_empty());
    }

    fn project(root: &Path) -> IpcProjectIdentity {
        IpcProjectIdentity {
            name: "nav".to_string(),
            path: root.display().to_string(),
        }
    }

    fn schematic_request(root: &Path) -> NavigationTargetRequest {
        NavigationTargetRequest {
            editor: IpcEditorKind::Schematic,
            project: project(root),
            document_path: root.join("nav.kicad_sch"),
            sheet_instance_path: Some(IpcSheetInstancePath {
                kiids: vec!["root".to_string()],
                human_readable: "/".to_string(),
            }),
            object_kiid: Some("sym-c10".to_string()),
            human_reference: None,
        }
    }

    fn fixture(root: &Path, symbols: &str) {
        write(&root.join("nav.kicad_pro"), "{}");
        write(
            &root.join("nav.kicad_sch"),
            &format!(
                "(kicad_sch (uuid \"root\") {symbols} (sheet_instances (path \"/\" (page \"1\"))))"
            ),
        );
    }

    fn symbol(reference: &str, uuid: &str) -> String {
        format!(
            "(symbol (lib_id \"Device:C\") (at 10 10 0) (uuid \"{uuid}\") \
             (property \"Reference\" \"{reference}\") \
             (instances (project \"nav\" (path \"/root\" (reference \"{reference}\") (unit 1)))))"
        )
    }

    #[test]
    fn exact_schematic_kiid_resolves_with_separate_structural_evidence() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path(), &symbol("C10", "sym-c10"));
        let target = resolve_navigation_target(&schematic_request(temp.path())).unwrap();
        assert_eq!(target.object.kiid, "sym-c10");
        assert_eq!(target.object.object_type, "schematic_symbol");
        assert_eq!(target.structural_evidence.source, "saved_kicad_structure");
        assert_eq!(target.structural_evidence.resolver, "exact_kiid");
    }

    #[test]
    fn unique_human_reference_resolves_but_duplicates_return_candidates() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path(), &symbol("C10", "sym-c10"));
        let mut request = schematic_request(temp.path());
        request.object_kiid = None;
        request.human_reference = Some("C10".to_string());
        assert_eq!(
            resolve_navigation_target(&request).unwrap().object.kiid,
            "sym-c10"
        );

        fixture(
            temp.path(),
            &format!("{} {}", symbol("C10", "sym-a"), symbol("C10", "sym-b")),
        );
        let error = resolve_navigation_target(&request).unwrap_err();
        assert_eq!(error.kind, NavigationTargetErrorKind::AmbiguousTarget);
        assert_eq!(error.candidates.len(), 2);
    }

    #[test]
    fn missing_stale_wrong_project_and_wrong_sheet_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        fixture(temp.path(), &symbol("C10", "sym-c10"));

        let mut missing = schematic_request(temp.path());
        missing.object_kiid = Some("gone".to_string());
        assert_eq!(
            resolve_navigation_target(&missing).unwrap_err().kind,
            NavigationTargetErrorKind::StaleTarget
        );

        let mut wrong_project = schematic_request(temp.path());
        wrong_project.project.name = "other".to_string();
        assert_eq!(
            resolve_navigation_target(&wrong_project).unwrap_err().kind,
            NavigationTargetErrorKind::WrongProject
        );

        let mut wrong_sheet = schematic_request(temp.path());
        wrong_sheet.sheet_instance_path = Some(IpcSheetInstancePath {
            kiids: vec!["other-root".to_string()],
            human_readable: "/other".to_string(),
        });
        assert_eq!(
            resolve_navigation_target(&wrong_sheet).unwrap_err().kind,
            NavigationTargetErrorKind::WrongSheetInstance
        );
    }

    #[test]
    fn exact_board_footprint_and_unique_reference_resolve() {
        let temp = tempfile::tempdir().unwrap();
        write(&temp.path().join("nav.kicad_pro"), "{}");
        let board = temp.path().join("layout.kicad_pcb");
        write(
            &board,
            "(kicad_pcb (footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"fp-c10\") (property \"Reference\" \"C10\")))",
        );
        let mut request = NavigationTargetRequest {
            editor: IpcEditorKind::Pcb,
            project: project(temp.path()),
            document_path: board,
            sheet_instance_path: None,
            object_kiid: Some("fp-c10".to_string()),
            human_reference: None,
        };
        assert_eq!(
            resolve_navigation_target(&request)
                .unwrap()
                .object
                .object_type,
            "pcb_footprint"
        );
        request.object_kiid = None;
        request.human_reference = Some("C10".to_string());
        assert_eq!(
            resolve_navigation_target(&request).unwrap().object.kiid,
            "fp-c10"
        );
    }
}
