//! Semantic KiCad editor observation and navigation.
//!
//! Public names in this module are provisional while upstream design issue
//! #395 is under review. The implementation deliberately exposes only typed
//! semantic operations; KiCad's unstable raw `RunAction` strings are not part
//! of this surface.

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{invalid_arg, opt_str, require_array, require_str, ToolContext, ToolDef};
use konnect_ipc::{
    IpcEditorDocument, IpcEditorKind, IpcProjectIdentity, IpcSelectionMutation,
    IpcSelectionMutationErrorKind, IpcSelectionObservationErrorKind, IpcSheetInstancePath,
};
use serde_json::json;
use std::path::PathBuf;

use super::cross_probe::{resolve_cross_probe, CrossProbeError, CrossProbeRequest};
use super::navigation_target::{
    resolve_navigation_target, NavigationTargetErrorKind, NavigationTargetRequest,
};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "get_editor_state",
            "Observe the configured KiCad IPC endpoint: running KiCad version, addressable schematic and PCB editors, exact open document identities, and semantic navigation capabilities. Active editor/document/sheet fields are null when KiCad has no stable typed query; open-document order is never treated as active state.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            |args, ctx| async move { handle_get_editor_state(args, ctx).await }
        ),
        tool!(
            "get_editor_selection",
            "Read the current selection from one exact KiCad editor, project, document, and schematic sheet instance. Returns stable KIID/UUID identities and refuses stale, ambiguous, cross-project, cross-document, or unsupported selected state instead of retargeting.",
            json!({
                "type": "object",
                "properties": {
                    "editor": { "type": "string", "enum": ["schematic", "pcb"] },
                    "project_name": { "type": "string", "description": "Exact project name returned by get_editor_state" },
                    "project_path": { "type": "string", "description": "Exact project path returned by get_editor_state" },
                    "document_path": { "type": "string", "description": "Exact PCB path; required only for editor=pcb" },
                    "sheet_instance_path": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Canonical root-to-leaf sheet KIIIDs; required only for editor=schematic"
                    },
                    "sheet_path_human_readable": { "type": "string", "description": "Optional display path returned by get_editor_state; not used as identity" }
                },
                "required": ["editor", "project_name", "project_path"]
            }),
            |args, ctx| async move { handle_get_editor_selection(args, ctx).await }
        ),
        tool!(
            "resolve_navigation_target",
            "Deterministically resolve one exact saved KiCad object in an explicitly open project, document, editor, and schematic hierarchy instance. Stable KIID/UUID identity is primary; a human reference is accepted only when it resolves uniquely and ambiguity is returned with candidates.",
            json!({
                "type": "object",
                "properties": {
                    "editor": { "type": "string", "enum": ["schematic", "pcb"] },
                    "project_name": { "type": "string" },
                    "project_path": { "type": "string" },
                    "document_path": { "type": "string", "description": "Exact saved .kicad_sch or .kicad_pcb document" },
                    "sheet_instance_path": { "type": "array", "items": { "type": "string" } },
                    "sheet_path_human_readable": { "type": "string" },
                    "object_kiid": { "type": "string", "description": "Preferred stable KIID/UUID" },
                    "human_reference": { "type": "string", "description": "Fallback reference such as C10; must resolve uniquely" }
                },
                "required": ["editor", "project_name", "project_path", "document_path"]
            }),
            |args, ctx| async move { handle_resolve_navigation_target(args, ctx).await }
        ),
        tool!(
            "mutate_editor_selection",
            "Clear, add to, or remove from one exact KiCad editor selection. Every non-clear KIID is first resolved in the explicit saved project/document/sheet, and success is derived only from a fresh exact GetSelection readback.",
            json!({
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "enum": ["clear", "add", "remove"] },
                    "editor": { "type": "string", "enum": ["schematic", "pcb"] },
                    "project_name": { "type": "string" },
                    "project_path": { "type": "string" },
                    "document_path": { "type": "string", "description": "Exact saved .kicad_sch or .kicad_pcb document" },
                    "sheet_instance_path": { "type": "array", "items": { "type": "string" } },
                    "sheet_path_human_readable": { "type": "string" },
                    "object_kiids": { "type": "array", "items": { "type": "string" }, "description": "Empty for clear; one or more stable KIIIDs for add/remove" }
                },
                "required": ["operation", "editor", "project_name", "project_path", "document_path", "object_kiids"]
            }),
            |args, ctx| async move { handle_mutate_editor_selection(args, ctx).await }
        ),
        tool!(
            "resolve_cross_probe_target",
            "Resolve an exact schematic-symbol/PCB-footprint relationship in either direction from stable saved KiCad symbol-path linkage, while proving both explicit live document contexts. This resolve-only operation does not activate or select; pin/pad/net expansion remains unsupported unless a unique stable destination can be modeled.",
            json!({
                "type": "object",
                "properties": {
                    "source_editor": { "type": "string", "enum": ["schematic", "pcb"] },
                    "project_name": { "type": "string" },
                    "project_path": { "type": "string" },
                    "schematic_document_path": { "type": "string" },
                    "pcb_document_path": { "type": "string" },
                    "schematic_sheet_instance_path": { "type": "array", "items": { "type": "string" } },
                    "sheet_path_human_readable": { "type": "string" },
                    "source_object_kiid": { "type": "string" }
                },
                "required": ["source_editor", "project_name", "project_path", "schematic_document_path", "pcb_document_path", "schematic_sheet_instance_path", "source_object_kiid"]
            }),
            |args, ctx| async move { handle_resolve_cross_probe_target(args, ctx).await }
        ),
    ]
}

async fn handle_get_editor_state(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }

    let result = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).observe_editor_state()
    })
    .await?;
    match result {
        Ok(observation) => Ok(CallToolResult::json(&observation)),
        Err(error) => {
            if let Some(document_error) = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<konnect_ipc::IpcDocumentObservationError>())
            {
                return Ok(CallToolResult::error_kind(
                    ToolErrorKind::StaleTarget {
                        target: format!("{} editor document state", document_error.editor.as_str()),
                        reason: document_error.reason.clone(),
                    },
                    document_error.to_string(),
                ));
            }
            if let Some(status) = konnect_ipc::ApiStatusError::from_error(&error) {
                if status.is_unsupported() {
                    return Ok(CallToolResult::error_kind(
                        ToolErrorKind::UnsupportedCapability {
                            capability: "editor_state_observation".to_string(),
                            kicad_version: None,
                        },
                        "The running KiCad endpoint does not support editor-state observation.",
                    ));
                }
                return Ok(CallToolResult::error_kind(
                    ToolErrorKind::StaleTarget {
                        target: "configured KiCad IPC endpoint".to_string(),
                        reason: status.code_name.clone(),
                    },
                    "The configured KiCad endpoint responded but could not provide a stable editor-state observation.",
                ));
            }
            match konnect_ipc::IpcFailure::from_error(error) {
                konnect_ipc::IpcFailure::Unreachable(_) => Ok(editor_unavailable(
                    "the configured KiCad IPC endpoint is unreachable",
                )),
                _ => Ok(CallToolResult::error_kind(
                    ToolErrorKind::StaleTarget {
                        target: "configured KiCad IPC endpoint".to_string(),
                        reason: "the endpoint did not return a complete typed observation"
                            .to_string(),
                    },
                    "The configured KiCad endpoint did not return a complete typed editor-state observation.",
                )),
            }
        }
    }
}

fn editor_unavailable(reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::EditorUnavailable {
            editor: "configured_endpoint".to_string(),
            reason: reason.to_string(),
        },
        format!("KiCad editor state is unavailable: {reason}."),
    )
}

async fn handle_get_editor_selection(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let target = match parse_selection_target(args) {
        Ok(target) => target,
        Err(result) => return Ok(result),
    };
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }
    let result = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).observe_selection(&target)
    })
    .await?;
    match result {
        Ok(observation) => Ok(CallToolResult::json(&observation)),
        Err(error) => Ok(selection_error_result(error)),
    }
}

fn parse_selection_target(args: &serde_json::Value) -> Result<IpcEditorDocument, CallToolResult> {
    let editor = match require_str(args, "editor")? {
        "schematic" => IpcEditorKind::Schematic,
        "pcb" => IpcEditorKind::Pcb,
        _ => return Err(invalid_arg("editor", "expected 'schematic' or 'pcb'")),
    };
    let project_name = require_str(args, "project_name")?;
    let project_path = require_str(args, "project_path")?;
    if project_name.is_empty() {
        return Err(invalid_arg("project_name", "must not be empty"));
    }
    if project_path.is_empty() {
        return Err(invalid_arg("project_path", "must not be empty"));
    }
    let project = Some(IpcProjectIdentity {
        name: project_name.to_string(),
        path: project_path.to_string(),
    });
    match editor {
        IpcEditorKind::Pcb => {
            let document_path = require_str(args, "document_path")?;
            if document_path.is_empty() {
                return Err(invalid_arg("document_path", "must not be empty"));
            }
            // Read the schematic-only arguments too so schema/handler drift
            // checks can prove every advertised parameter is intentional.
            if !args["sheet_instance_path"].is_null()
                || !args["sheet_path_human_readable"].is_null()
            {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "schematic sheet identity is not valid for editor=pcb",
                ));
            }
            Ok(IpcEditorDocument {
                editor,
                project,
                document_path: Some(document_path.to_string()),
                sheet_instance_path: None,
            })
        }
        IpcEditorKind::Schematic => {
            if !args["document_path"].is_null() {
                return Err(invalid_arg(
                    "document_path",
                    "PCB document paths are not schematic sheet identity",
                ));
            }
            let ids = require_array(args, "sheet_instance_path")?
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    invalid_arg("sheet_instance_path", "every entry must be a string")
                })?;
            if ids.is_empty() || ids.iter().any(String::is_empty) {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "must contain non-empty root-to-leaf KIIIDs",
                ));
            }
            Ok(IpcEditorDocument {
                editor,
                project,
                document_path: None,
                sheet_instance_path: Some(IpcSheetInstancePath {
                    kiids: ids,
                    human_readable: opt_str(args, "sheet_path_human_readable")
                        .unwrap_or("")
                        .to_string(),
                }),
            })
        }
    }
}

fn selection_error_result(error: anyhow::Error) -> CallToolResult {
    if let Some(selection) = konnect_ipc::IpcSelectionObservationError::from_error(&error) {
        let kind = match selection.kind {
            IpcSelectionObservationErrorKind::WrongProject => ToolErrorKind::WrongProject {
                requested: selection.requested.clone(),
                open_projects: selection.candidates.clone(),
            },
            IpcSelectionObservationErrorKind::WrongDocument => ToolErrorKind::WrongDocument {
                requested: selection.requested.clone(),
                open_documents: selection.candidates.clone(),
            },
            IpcSelectionObservationErrorKind::WrongSheetInstance => {
                ToolErrorKind::WrongSheetInstance {
                    requested: selection.requested.clone(),
                    open_sheet_instances: selection.candidates.clone(),
                }
            }
            IpcSelectionObservationErrorKind::AmbiguousDocument => ToolErrorKind::AmbiguousTarget {
                target: selection.requested.clone(),
                candidates: selection.candidates.clone(),
            },
            IpcSelectionObservationErrorKind::UnsupportedObjectType => {
                ToolErrorKind::UnsupportedCapability {
                    capability: format!("decode selected object type {}", selection.requested),
                    kicad_version: None,
                }
            }
            IpcSelectionObservationErrorKind::StaleEditorState
            | IpcSelectionObservationErrorKind::MalformedSelectedObject => {
                ToolErrorKind::StaleTarget {
                    target: selection.requested.clone(),
                    reason: selection.reason.clone(),
                }
            }
        };
        return CallToolResult::error_kind(kind, selection.to_string());
    }
    if let Some(status) = konnect_ipc::ApiStatusError::from_error(&error) {
        if status.is_unsupported() {
            return CallToolResult::error_kind(
                ToolErrorKind::UnsupportedCapability {
                    capability: "selection_observation".to_string(),
                    kicad_version: None,
                },
                "The running KiCad endpoint does not support typed selection observation.",
            );
        }
    }
    match konnect_ipc::IpcFailure::from_error(error) {
        konnect_ipc::IpcFailure::Unreachable(_) => {
            editor_unavailable("the configured KiCad IPC endpoint is unreachable")
        }
        _ => CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: "requested editor selection".to_string(),
                reason: "KiCad did not return a complete typed selection readback".to_string(),
            },
            "KiCad did not return a complete typed selection readback.",
        ),
    }
}

async fn handle_resolve_navigation_target(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let request = match parse_navigation_target_request(args) {
        Ok(request) => request,
        Err(result) => return Ok(result),
    };
    let live_document = IpcEditorDocument {
        editor: request.editor,
        project: Some(request.project.clone()),
        document_path: (request.editor == IpcEditorKind::Pcb)
            .then(|| request.document_path.display().to_string()),
        sheet_instance_path: request.sheet_instance_path.clone(),
    };
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }
    let observed = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).observe_exact_open_document(&live_document)
    })
    .await?;
    let observed = match observed {
        Ok(observed) => observed,
        Err(error) => return Ok(selection_error_result(error)),
    };

    let resolved = tokio::task::spawn_blocking(move || resolve_navigation_target(&request)).await?;
    match resolved {
        Ok(target) => Ok(CallToolResult::json(&json!({
            "target": target,
            "live_context_evidence": {
                "source": "kicad_ipc_get_open_documents",
                "document": observed
            }
        }))),
        Err(error) => Ok(navigation_target_error_result(error)),
    }
}

fn parse_navigation_target_request(
    args: &serde_json::Value,
) -> Result<NavigationTargetRequest, CallToolResult> {
    let editor = match require_str(args, "editor")? {
        "schematic" => IpcEditorKind::Schematic,
        "pcb" => IpcEditorKind::Pcb,
        _ => return Err(invalid_arg("editor", "expected 'schematic' or 'pcb'")),
    };
    let project_name = require_str(args, "project_name")?;
    let project_path = require_str(args, "project_path")?;
    let document_path = require_str(args, "document_path")?;
    if project_name.is_empty() || project_path.is_empty() || document_path.is_empty() {
        return Err(invalid_arg(
            "project_name",
            "project and document identity strings must not be empty",
        ));
    }

    let sheet_instance_path = match editor {
        IpcEditorKind::Pcb => {
            if !args["sheet_instance_path"].is_null()
                || !args["sheet_path_human_readable"].is_null()
            {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "PCB targets cannot carry schematic sheet identity",
                ));
            }
            None
        }
        IpcEditorKind::Schematic => {
            let ids = require_array(args, "sheet_instance_path")?
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    invalid_arg("sheet_instance_path", "every entry must be a string")
                })?;
            if ids.is_empty() || ids.iter().any(String::is_empty) {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "must contain non-empty root-to-leaf KIIIDs",
                ));
            }
            Some(IpcSheetInstancePath {
                kiids: ids,
                human_readable: opt_str(args, "sheet_path_human_readable")
                    .unwrap_or("")
                    .to_string(),
            })
        }
    };
    let object_kiid = opt_str(args, "object_kiid").map(str::to_string);
    let human_reference = opt_str(args, "human_reference").map(str::to_string);
    if object_kiid.as_deref().is_some_and(str::is_empty)
        || human_reference.as_deref().is_some_and(str::is_empty)
        || object_kiid.is_some() == human_reference.is_some()
    {
        return Err(invalid_arg(
            "object_kiid",
            "provide exactly one non-empty object_kiid or human_reference",
        ));
    }
    Ok(NavigationTargetRequest {
        editor,
        project: IpcProjectIdentity {
            name: project_name.to_string(),
            path: project_path.to_string(),
        },
        document_path: PathBuf::from(document_path),
        sheet_instance_path,
        object_kiid,
        human_reference,
    })
}

fn navigation_target_error_result(
    error: super::navigation_target::NavigationTargetError,
) -> CallToolResult {
    let kind = match error.kind {
        NavigationTargetErrorKind::WrongProject => ToolErrorKind::WrongProject {
            requested: error.target.clone(),
            open_projects: error.candidates.clone(),
        },
        NavigationTargetErrorKind::WrongDocument => ToolErrorKind::WrongDocument {
            requested: error.target.clone(),
            open_documents: error.candidates.clone(),
        },
        NavigationTargetErrorKind::WrongSheetInstance => ToolErrorKind::WrongSheetInstance {
            requested: error.target.clone(),
            open_sheet_instances: error.candidates.clone(),
        },
        NavigationTargetErrorKind::AmbiguousTarget => ToolErrorKind::AmbiguousTarget {
            target: error.target.clone(),
            candidates: error.candidates.clone(),
        },
        NavigationTargetErrorKind::StaleTarget => ToolErrorKind::StaleTarget {
            target: error.target.clone(),
            reason: error.reason.clone(),
        },
    };
    CallToolResult::error_kind(kind, error.to_string())
}

#[derive(Debug)]
struct SelectionMutationRequest {
    operation: IpcSelectionMutation,
    live_document: IpcEditorDocument,
    object_kiids: Vec<String>,
    structural_targets: Vec<NavigationTargetRequest>,
}

async fn handle_mutate_editor_selection(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let request = match parse_selection_mutation_request(args) {
        Ok(request) => request,
        Err(result) => return Ok(result),
    };
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let resolved_targets = request
            .structural_targets
            .iter()
            .map(resolve_navigation_target)
            .collect::<Result<Vec<_>, _>>()?;
        let mutation = konnect_ipc::KiCadIpcClient::new(address).mutate_selection(
            &request.live_document,
            request.operation,
            &request.object_kiids,
        )?;
        Ok((resolved_targets, mutation))
    })
    .await?;
    match result {
        Ok((resolved_targets, mutation)) => Ok(CallToolResult::json(&json!({
            "resolved_targets": resolved_targets,
            "mutation": mutation
        }))),
        Err(error) => Ok(selection_mutation_error_result(error)),
    }
}

fn parse_selection_mutation_request(
    args: &serde_json::Value,
) -> Result<SelectionMutationRequest, CallToolResult> {
    let operation = match require_str(args, "operation")? {
        "clear" => IpcSelectionMutation::Clear,
        "add" => IpcSelectionMutation::Add,
        "remove" => IpcSelectionMutation::Remove,
        _ => {
            return Err(invalid_arg(
                "operation",
                "expected 'clear', 'add', or 'remove'",
            ))
        }
    };
    let editor = match require_str(args, "editor")? {
        "schematic" => IpcEditorKind::Schematic,
        "pcb" => IpcEditorKind::Pcb,
        _ => return Err(invalid_arg("editor", "expected 'schematic' or 'pcb'")),
    };
    let project_name = require_str(args, "project_name")?;
    let project_path = require_str(args, "project_path")?;
    let document_path = require_str(args, "document_path")?;
    if project_name.is_empty() || project_path.is_empty() || document_path.is_empty() {
        return Err(invalid_arg(
            "project_name",
            "project and document identity strings must not be empty",
        ));
    }
    let object_kiids = require_array(args, "object_kiids")?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| invalid_arg("object_kiids", "every entry must be a string"))?;
    if object_kiids.iter().any(String::is_empty) {
        return Err(invalid_arg("object_kiids", "KIIIDs must not be empty"));
    }
    let unique = object_kiids
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    if unique.len() != object_kiids.len() {
        return Err(invalid_arg(
            "object_kiids",
            "duplicate KIIIDs are not allowed",
        ));
    }
    match operation {
        IpcSelectionMutation::Clear if !object_kiids.is_empty() => {
            return Err(invalid_arg("object_kiids", "clear requires an empty array"));
        }
        IpcSelectionMutation::Add | IpcSelectionMutation::Remove if object_kiids.is_empty() => {
            return Err(invalid_arg(
                "object_kiids",
                "add and remove require at least one KIID",
            ));
        }
        _ => {}
    }

    let project = IpcProjectIdentity {
        name: project_name.to_string(),
        path: project_path.to_string(),
    };
    let sheet_instance_path = match editor {
        IpcEditorKind::Pcb => {
            if !args["sheet_instance_path"].is_null()
                || !args["sheet_path_human_readable"].is_null()
            {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "PCB targets cannot carry schematic sheet identity",
                ));
            }
            None
        }
        IpcEditorKind::Schematic => {
            let ids = require_array(args, "sheet_instance_path")?
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    invalid_arg("sheet_instance_path", "every entry must be a string")
                })?;
            if ids.is_empty() || ids.iter().any(String::is_empty) {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "must contain non-empty root-to-leaf KIIIDs",
                ));
            }
            Some(IpcSheetInstancePath {
                kiids: ids,
                human_readable: opt_str(args, "sheet_path_human_readable")
                    .unwrap_or("")
                    .to_string(),
            })
        }
    };
    let saved_document = PathBuf::from(document_path);
    let structural_targets = object_kiids
        .iter()
        .map(|kiid| NavigationTargetRequest {
            editor,
            project: project.clone(),
            document_path: saved_document.clone(),
            sheet_instance_path: sheet_instance_path.clone(),
            object_kiid: Some(kiid.clone()),
            human_reference: None,
        })
        .collect();
    Ok(SelectionMutationRequest {
        operation,
        live_document: IpcEditorDocument {
            editor,
            project: Some(project),
            document_path: (editor == IpcEditorKind::Pcb)
                .then(|| saved_document.display().to_string()),
            sheet_instance_path,
        },
        object_kiids,
        structural_targets,
    })
}

fn selection_mutation_error_result(error: anyhow::Error) -> CallToolResult {
    if let Some(target) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<super::navigation_target::NavigationTargetError>())
    {
        return navigation_target_error_result(target.clone());
    }
    if let Some(mutation) = konnect_ipc::IpcSelectionMutationError::from_error(&error) {
        let kind = match mutation.kind {
            IpcSelectionMutationErrorKind::InvalidRequest => ToolErrorKind::InvalidArgument {
                field: "object_kiids".to_string(),
                reason: mutation.reason.clone(),
            },
            IpcSelectionMutationErrorKind::ReadbackMismatch => ToolErrorKind::ReadbackMismatch {
                operation: mutation.operation.as_str().to_string(),
                requested_kiids: mutation.requested_kiids.clone(),
                before_kiids: mutation.before_kiids.clone(),
                after_kiids: mutation.after_kiids.clone(),
            },
        };
        return CallToolResult::error_kind(kind, mutation.to_string());
    }
    if konnect_ipc::IpcSelectionObservationError::from_error(&error).is_some() {
        return selection_error_result(error);
    }
    if let Some(status) = konnect_ipc::ApiStatusError::from_error(&error) {
        if status.is_unsupported() {
            return CallToolResult::error_kind(
                ToolErrorKind::UnsupportedCapability {
                    capability: "selection_mutation".to_string(),
                    kicad_version: None,
                },
                "The running KiCad endpoint does not support typed selection mutation.",
            );
        }
    }
    match konnect_ipc::IpcFailure::from_error(error) {
        konnect_ipc::IpcFailure::Unreachable(_) => {
            editor_unavailable("the configured KiCad IPC endpoint is unreachable")
        }
        _ => CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: "requested editor selection mutation".to_string(),
                reason: "KiCad did not return a complete typed mutation/readback sequence"
                    .to_string(),
            },
            "KiCad did not return a complete typed selection mutation/readback sequence.",
        ),
    }
}

async fn handle_resolve_cross_probe_target(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let request = match parse_cross_probe_request(args) {
        Ok(request) => request,
        Err(result) => return Ok(result),
    };
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let resolution = resolve_cross_probe(&request)?;
        let source_live = live_cross_probe_document(&request, request.source_editor);
        let destination_editor = match request.source_editor {
            IpcEditorKind::Schematic => IpcEditorKind::Pcb,
            IpcEditorKind::Pcb => IpcEditorKind::Schematic,
        };
        let destination_live = live_cross_probe_document(&request, destination_editor);
        let client = konnect_ipc::KiCadIpcClient::new(address);
        let source_observed = client.observe_exact_open_document(&source_live)?;
        let destination_observed = client.observe_exact_open_document(&destination_live)?;
        Ok((resolution, source_observed, destination_observed))
    })
    .await?;
    match result {
        Ok((resolution, source_observed, destination_observed)) => {
            Ok(CallToolResult::json(&json!({
                "resolution": resolution,
                "live_context_evidence": {
                    "source": "kicad_ipc_get_open_documents",
                    "source_document": source_observed,
                    "destination_document": destination_observed
                },
                "destination_navigation": {
                    "attempted": false,
                    "status": "not_attempted",
                    "reason": "exact activation/reveal is unsupported by the bundled stable protocol; resolution does not imply a state change"
                }
            })))
        }
        Err(error) => Ok(cross_probe_error_result(error)),
    }
}

fn parse_cross_probe_request(
    args: &serde_json::Value,
) -> Result<CrossProbeRequest, CallToolResult> {
    let source_editor = match require_str(args, "source_editor")? {
        "schematic" => IpcEditorKind::Schematic,
        "pcb" => IpcEditorKind::Pcb,
        _ => {
            return Err(invalid_arg(
                "source_editor",
                "expected 'schematic' or 'pcb'",
            ))
        }
    };
    let project_name = require_str(args, "project_name")?;
    let project_path = require_str(args, "project_path")?;
    let schematic_document_path = require_str(args, "schematic_document_path")?;
    let pcb_document_path = require_str(args, "pcb_document_path")?;
    let source_object_kiid = require_str(args, "source_object_kiid")?;
    if [
        project_name,
        project_path,
        schematic_document_path,
        pcb_document_path,
        source_object_kiid,
    ]
    .iter()
    .any(|value| value.is_empty())
    {
        return Err(invalid_arg(
            "source_object_kiid",
            "project, document, and source KIID strings must not be empty",
        ));
    }
    let sheet_kiids = require_array(args, "schematic_sheet_instance_path")?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            invalid_arg(
                "schematic_sheet_instance_path",
                "every entry must be a string",
            )
        })?;
    if sheet_kiids.is_empty() || sheet_kiids.iter().any(String::is_empty) {
        return Err(invalid_arg(
            "schematic_sheet_instance_path",
            "must contain non-empty root-to-leaf KIIIDs",
        ));
    }
    Ok(CrossProbeRequest {
        project: IpcProjectIdentity {
            name: project_name.to_string(),
            path: project_path.to_string(),
        },
        schematic_document_path: PathBuf::from(schematic_document_path),
        pcb_document_path: PathBuf::from(pcb_document_path),
        schematic_sheet_instance_path: IpcSheetInstancePath {
            kiids: sheet_kiids,
            human_readable: opt_str(args, "sheet_path_human_readable")
                .unwrap_or("")
                .to_string(),
        },
        source_editor,
        source_object_kiid: source_object_kiid.to_string(),
    })
}

fn live_cross_probe_document(
    request: &CrossProbeRequest,
    editor: IpcEditorKind,
) -> IpcEditorDocument {
    IpcEditorDocument {
        editor,
        project: Some(request.project.clone()),
        document_path: (editor == IpcEditorKind::Pcb)
            .then(|| request.pcb_document_path.display().to_string()),
        sheet_instance_path: (editor == IpcEditorKind::Schematic)
            .then(|| request.schematic_sheet_instance_path.clone()),
    }
}

fn cross_probe_error_result(error: anyhow::Error) -> CallToolResult {
    if let Some(cross_probe) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<CrossProbeError>())
    {
        return match cross_probe {
            CrossProbeError::Target(target) => navigation_target_error_result(target.clone()),
            CrossProbeError::UnresolvedDestination {
                source_kiid,
                reason,
                candidates,
            } => CallToolResult::error_kind(
                ToolErrorKind::UnresolvedCrossProbeDestination {
                    source_kiid: source_kiid.clone(),
                    candidates: candidates.clone(),
                    reason: reason.clone(),
                },
                cross_probe.to_string(),
            ),
        };
    }
    if konnect_ipc::IpcSelectionObservationError::from_error(&error).is_some() {
        return selection_error_result(error);
    }
    match konnect_ipc::IpcFailure::from_error(error) {
        konnect_ipc::IpcFailure::Unreachable(_) => {
            editor_unavailable("a requested cross-probe editor endpoint is unreachable")
        }
        _ => CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: "requested cross-probe context".to_string(),
                reason: "live or structural evidence was incomplete".to_string(),
            },
            "Cross-probe resolution did not return complete live and structural evidence.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use crate::mcp::protocol::ToolContent;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use konnect_ipc::builders;
    use konnect_ipc::gen::kiapi;
    use nng::options::Options;
    use prost::Message;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn context(ipc_address: String) -> ToolContext {
        ToolContext::new(
            ServerConfig {
                ipc_address,
                ..Default::default()
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn selection_project_path() -> &'static str {
        if cfg!(windows) {
            r"C:\design"
        } else {
            "/design"
        }
    }

    fn spawn_observation_mock() -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://editor-state-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket
            .set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(10)))
            .expect("timeout");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            for _ in 0..3 {
                let message = socket.recv().expect("request");
                let request =
                    kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
                let command = request.message.expect("command");
                let response_any = if command.type_url.ends_with("GetVersion") {
                    builders::pack_any(
                        &kiapi::common::commands::GetVersionResponse {
                            version: Some(kiapi::common::types::KiCadVersion {
                                major: 10,
                                minor: 0,
                                patch: 5,
                                full_version: "10.0.5".to_string(),
                            }),
                        },
                        "kiapi.common.commands.GetVersionResponse",
                    )
                } else {
                    builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: Vec::new(),
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )
                };
                let response = kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    header: None,
                    message: Some(response_any),
                };
                socket
                    .send(nng::Message::from(response.encode_to_vec().as_slice()))
                    .expect("response");
            }
        });
        url
    }

    fn spawn_selection_mock() -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://editor-selection-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            for _ in 0..3 {
                let message = socket.recv().expect("request");
                let request =
                    kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
                let command = request.message.expect("command");
                let response_any = if command.type_url.ends_with("GetOpenDocuments") {
                    builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![kiapi::common::types::DocumentSpecifier {
                                r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                                identifier: Some(
                                    kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                        "navigation.kicad_pcb".to_string(),
                                    ),
                                ),
                                project: Some(kiapi::common::types::ProjectSpecifier {
                                    name: "navigation".to_string(),
                                    path: selection_project_path().to_string(),
                                }),
                            }],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )
                } else {
                    let footprint = kiapi::board::types::FootprintInstance {
                        id: Some(kiapi::common::types::Kiid {
                            value: "footprint-kiid".to_string(),
                        }),
                        ..Default::default()
                    };
                    builders::pack_any(
                        &kiapi::common::commands::SelectionResponse {
                            items: vec![builders::pack_any(
                                &footprint,
                                "kiapi.board.types.FootprintInstance",
                            )],
                        },
                        "kiapi.common.commands.SelectionResponse",
                    )
                };
                socket
                    .send(nng::Message::from(
                        kiapi::common::ApiResponse {
                            status: Some(kiapi::common::ApiResponseStatus {
                                status: kiapi::common::ApiStatusCode::AsOk as i32,
                                error_message: String::new(),
                            }),
                            header: None,
                            message: Some(response_any),
                        }
                        .encode_to_vec()
                        .as_slice(),
                    ))
                    .expect("response");
            }
        });
        url
    }

    fn spawn_open_board_mock(project_path: String) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://navigation-resolver-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            let message = socket.recv().expect("request");
            let request =
                kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
            assert!(request
                .message
                .as_ref()
                .is_some_and(|message| message.type_url.ends_with("GetOpenDocuments")));
            let response = kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsOk as i32,
                    error_message: String::new(),
                }),
                header: None,
                message: Some(builders::pack_any(
                    &kiapi::common::commands::GetOpenDocumentsResponse {
                        documents: vec![kiapi::common::types::DocumentSpecifier {
                            r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                            identifier: Some(
                                kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                    "layout.kicad_pcb".to_string(),
                                ),
                            ),
                            project: Some(kiapi::common::types::ProjectSpecifier {
                                name: "nav".to_string(),
                                path: project_path,
                            }),
                        }],
                    },
                    "kiapi.common.commands.GetOpenDocumentsResponse",
                )),
            };
            socket
                .send(nng::Message::from(response.encode_to_vec().as_slice()))
                .expect("response");
        });
        url
    }

    fn spawn_add_selection_mock(project_path: String, apply_mutation: bool) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://selection-mutation-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            let mut selected = false;
            for _ in 0..8 {
                let message = socket.recv().expect("request");
                let request =
                    kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
                let command = request.message.expect("command");
                let response_any = if command.type_url.ends_with("GetOpenDocuments") {
                    builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![kiapi::common::types::DocumentSpecifier {
                                r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                                identifier: Some(
                                    kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                        "layout.kicad_pcb".to_string(),
                                    ),
                                ),
                                project: Some(kiapi::common::types::ProjectSpecifier {
                                    name: "nav".to_string(),
                                    path: project_path.clone(),
                                }),
                            }],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )
                } else {
                    if command.type_url.ends_with("AddToSelection") && apply_mutation {
                        let add = kiapi::common::commands::AddToSelection::decode(
                            command.value.as_slice(),
                        )
                        .expect("add selection");
                        assert_eq!(add.items[0].value, "fp-c10");
                        selected = true;
                    }
                    let items = selected
                        .then(|| {
                            builders::pack_any(
                                &kiapi::board::types::FootprintInstance {
                                    id: Some(kiapi::common::types::Kiid {
                                        value: "fp-c10".to_string(),
                                    }),
                                    ..Default::default()
                                },
                                "kiapi.board.types.FootprintInstance",
                            )
                        })
                        .into_iter()
                        .collect();
                    builders::pack_any(
                        &kiapi::common::commands::SelectionResponse { items },
                        "kiapi.common.commands.SelectionResponse",
                    )
                };
                let response = kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    header: None,
                    message: Some(response_any),
                };
                socket
                    .send(nng::Message::from(response.encode_to_vec().as_slice()))
                    .expect("response");
            }
        });
        url
    }

    fn spawn_cross_probe_context_mock(
        project_path: String,
        commands: Arc<Mutex<Vec<String>>>,
    ) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://cross-probe-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            for _ in 0..2 {
                let message = socket.recv().expect("request");
                let request =
                    kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
                let command = request.message.expect("command");
                commands.lock().unwrap().push(command.type_url.clone());
                let query =
                    kiapi::common::commands::GetOpenDocuments::decode(command.value.as_slice())
                        .expect("open documents");
                let document = if query.r#type
                    == kiapi::common::types::DocumentType::DoctypeSchematic as i32
                {
                    kiapi::common::types::DocumentSpecifier {
                        r#type: query.r#type,
                        identifier: Some(
                            kiapi::common::types::document_specifier::Identifier::SheetPath(
                                kiapi::common::types::SheetPath {
                                    path: vec![kiapi::common::types::Kiid {
                                        value: "root".to_string(),
                                    }],
                                    path_human_readable: "/".to_string(),
                                },
                            ),
                        ),
                        project: Some(kiapi::common::types::ProjectSpecifier {
                            name: "nav".to_string(),
                            path: project_path.clone(),
                        }),
                    }
                } else {
                    kiapi::common::types::DocumentSpecifier {
                        r#type: query.r#type,
                        identifier: Some(
                            kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                "nav.kicad_pcb".to_string(),
                            ),
                        ),
                        project: Some(kiapi::common::types::ProjectSpecifier {
                            name: "nav".to_string(),
                            path: project_path.clone(),
                        }),
                    }
                };
                let response = kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    header: None,
                    message: Some(builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![document],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )),
                };
                socket
                    .send(nng::Message::from(response.encode_to_vec().as_slice()))
                    .expect("response");
            }
        });
        url
    }

    #[test]
    fn public_tool_is_read_only_and_takes_no_required_arguments() {
        let definitions = tools();
        assert_eq!(definitions.len(), 5);
        let state = definitions
            .iter()
            .find(|tool| tool.name == "get_editor_state")
            .expect("state tool");
        assert_eq!(state.input_schema["required"], json!([]));
        let selection = definitions
            .iter()
            .find(|tool| tool.name == "get_editor_selection")
            .expect("selection tool");
        assert_eq!(
            selection.input_schema["required"],
            json!(["editor", "project_name", "project_path"])
        );
        let resolver = definitions
            .iter()
            .find(|tool| tool.name == "resolve_navigation_target")
            .expect("resolver tool");
        assert_eq!(
            resolver.input_schema["required"],
            json!(["editor", "project_name", "project_path", "document_path"])
        );
        let mutation = definitions
            .iter()
            .find(|tool| tool.name == "mutate_editor_selection")
            .expect("selection mutation tool");
        assert_eq!(
            mutation.input_schema["required"],
            json!([
                "operation",
                "editor",
                "project_name",
                "project_path",
                "document_path",
                "object_kiids"
            ])
        );
        let cross_probe = definitions
            .iter()
            .find(|tool| tool.name == "resolve_cross_probe_target")
            .expect("cross-probe resolver tool");
        assert_eq!(
            cross_probe.input_schema["required"],
            json!([
                "source_editor",
                "project_name",
                "project_path",
                "schematic_document_path",
                "pcb_document_path",
                "schematic_sheet_instance_path",
                "source_object_kiid"
            ])
        );
    }

    #[tokio::test]
    async fn unconfigured_endpoint_is_a_structured_editor_unavailable_refusal() {
        let result = handle_get_editor_state(&json!({}), &context(String::new()))
            .await
            .expect("handler result");
        assert!(result.is_error);
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("editor_unavailable")
        );
    }

    #[tokio::test]
    async fn public_result_is_derived_from_typed_ipc_observation() {
        let result = handle_get_editor_state(&json!({}), &context(spawn_observation_mock()))
            .await
            .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).expect("json result");
        assert_eq!(body["kicad_version"]["full_version"], "10.0.5");
        assert_eq!(body["evidence_source"], "kicad_ipc");
        assert_eq!(body["editors"].as_array().map(Vec::len), Some(2));
        assert!(body["active_editor"].is_null());
        assert!(body["active_document"].is_null());
        assert!(body["active_sheet_instance"].is_null());
    }

    #[tokio::test]
    async fn public_selection_result_is_derived_from_exact_document_readback() {
        let project_path = selection_project_path();
        let result = handle_get_editor_selection(
            &json!({
                "editor": "pcb",
                "project_name": "navigation",
                "project_path": project_path,
                "document_path": std::path::Path::new(project_path).join("navigation.kicad_pcb")
            }),
            &context(spawn_selection_mock()),
        )
        .await
        .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).expect("json result");
        assert_eq!(body["editor"], "pcb");
        assert_eq!(body["selected_objects"][0]["kiid"], "footprint-kiid");
        assert_eq!(
            body["evidence_source"],
            "kicad_ipc_get_selection_with_document_readback"
        );
    }

    #[test]
    fn public_selection_parser_refuses_cross_editor_identity_fields() {
        let pcb_with_sheet = parse_selection_target(&json!({
            "editor": "pcb",
            "project_name": "navigation",
            "project_path": r"C:\design",
            "document_path": r"C:\design\navigation.kicad_pcb",
            "sheet_instance_path": ["root"]
        }))
        .expect_err("PCB must not accept a schematic identity");
        assert_eq!(
            extract_error_kind(&pcb_with_sheet).as_deref(),
            Some("invalid_argument")
        );

        let schematic_with_board = parse_selection_target(&json!({
            "editor": "schematic",
            "project_name": "navigation",
            "project_path": r"C:\design",
            "document_path": r"C:\design\navigation.kicad_sch",
            "sheet_instance_path": ["root"]
        }))
        .expect_err("schematic must not accept a PCB path");
        assert_eq!(
            extract_error_kind(&schematic_with_board).as_deref(),
            Some("invalid_argument")
        );
    }

    #[test]
    fn wrong_sheet_instance_maps_to_the_public_structured_refusal() {
        let result = selection_error_result(anyhow::Error::new(
            konnect_ipc::IpcSelectionObservationError {
                kind: IpcSelectionObservationErrorKind::WrongSheetInstance,
                editor: IpcEditorKind::Schematic,
                requested: "/root/requested".to_string(),
                candidates: vec!["/root/other".to_string()],
                reason: "wrong sheet".to_string(),
            },
        ));
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("wrong_sheet_instance")
        );
    }

    #[test]
    fn resolver_parser_requires_exactly_one_identifier() {
        let base = json!({
            "editor": "pcb",
            "project_name": "navigation",
            "project_path": r"C:\design",
            "document_path": r"C:\design\navigation.kicad_pcb"
        });
        let missing = parse_navigation_target_request(&base).expect_err("identifier required");
        assert_eq!(
            extract_error_kind(&missing).as_deref(),
            Some("invalid_argument")
        );

        let mut both = base;
        both["object_kiid"] = json!("id");
        both["human_reference"] = json!("C10");
        let ambiguous = parse_navigation_target_request(&both).expect_err("one identifier only");
        assert_eq!(
            extract_error_kind(&ambiguous).as_deref(),
            Some("invalid_argument")
        );
    }

    #[tokio::test]
    async fn public_resolver_keeps_live_and_structural_evidence_separate() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nav.kicad_pro"), "{}").unwrap();
        let board = temp.path().join("layout.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb (footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"fp-c10\") (property \"Reference\" \"C10\")))",
        )
        .unwrap();
        let project_path = temp.path().display().to_string();
        let result = handle_resolve_navigation_target(
            &json!({
                "editor": "pcb",
                "project_name": "nav",
                "project_path": project_path.clone(),
                "document_path": board.display().to_string(),
                "human_reference": "C10"
            }),
            &context(spawn_open_board_mock(project_path)),
        )
        .await
        .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["target"]["object"]["kiid"], "fp-c10");
        assert_eq!(
            body["target"]["structural_evidence"]["source"],
            "saved_kicad_structure"
        );
        assert_eq!(
            body["live_context_evidence"]["source"],
            "kicad_ipc_get_open_documents"
        );
    }

    #[test]
    fn mutation_parser_requires_operation_appropriate_unique_kiids() {
        let base = json!({
            "operation": "clear",
            "editor": "pcb",
            "project_name": "navigation",
            "project_path": r"C:\design",
            "document_path": r"C:\design\navigation.kicad_pcb",
            "object_kiids": []
        });
        assert!(parse_selection_mutation_request(&base).is_ok());

        for object_kiids in [json!(["a"]), json!(["a", "a"])] {
            let mut invalid = base.clone();
            invalid["object_kiids"] = object_kiids;
            if invalid["object_kiids"].as_array().map(Vec::len) == Some(2) {
                invalid["operation"] = json!("add");
            }
            let result = parse_selection_mutation_request(&invalid).expect_err("invalid KIIIDs");
            assert_eq!(
                extract_error_kind(&result).as_deref(),
                Some("invalid_argument")
            );
        }
    }

    #[tokio::test]
    async fn public_selection_mutation_success_is_derived_from_readback() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nav.kicad_pro"), "{}").unwrap();
        let board = temp.path().join("layout.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb (footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"fp-c10\") (property \"Reference\" \"C10\")))",
        )
        .unwrap();
        let project_path = temp.path().display().to_string();
        let result = handle_mutate_editor_selection(
            &json!({
                "operation": "add",
                "editor": "pcb",
                "project_name": "nav",
                "project_path": project_path.clone(),
                "document_path": board.display().to_string(),
                "object_kiids": ["fp-c10"]
            }),
            &context(spawn_add_selection_mock(project_path, true)),
        )
        .await
        .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["resolved_targets"][0]["object"]["kiid"], "fp-c10");
        assert_eq!(
            body["mutation"]["after"]["selected_objects"][0]["kiid"],
            "fp-c10"
        );
        assert_eq!(
            body["mutation"]["evidence_source"],
            "kicad_ipc_selection_mutation_with_get_selection_readback"
        );
    }

    #[tokio::test]
    async fn public_selection_mutation_reports_readback_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nav.kicad_pro"), "{}").unwrap();
        let board = temp.path().join("layout.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb (footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"fp-c10\") (property \"Reference\" \"C10\")))",
        )
        .unwrap();
        let project_path = temp.path().display().to_string();
        let result = handle_mutate_editor_selection(
            &json!({
                "operation": "add",
                "editor": "pcb",
                "project_name": "nav",
                "project_path": project_path.clone(),
                "document_path": board.display().to_string(),
                "object_kiids": ["fp-c10"]
            }),
            &context(spawn_add_selection_mock(project_path, false)),
        )
        .await
        .expect("handler result");
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("readback_mismatch")
        );
    }

    #[test]
    fn unsupported_selection_mutation_maps_to_a_typed_capability_refusal() {
        let result =
            selection_mutation_error_result(anyhow::Error::new(konnect_ipc::ApiStatusError {
                code: kiapi::common::ApiStatusCode::AsUnhandled as i32,
                code_name: "AS_UNHANDLED".to_string(),
                message: "unsupported".to_string(),
            }));
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("unsupported_capability")
        );
    }

    #[tokio::test]
    async fn public_cross_probe_proves_both_live_contexts_without_state_change() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nav.kicad_pro"), "{}").unwrap();
        let schematic = temp.path().join("nav.kicad_sch");
        std::fs::write(
            &schematic,
            "(kicad_sch (uuid \"root\") \
             (symbol (lib_id \"Device:C\") (at 1 2) (uuid \"sym-c10\") \
               (property \"Reference\" \"C10\") \
               (instances (project \"nav\" (path \"/root\" (reference \"C10\") (unit 1))))) \
             (sheet_instances (path \"/\" (page \"1\"))))",
        )
        .unwrap();
        let board = temp.path().join("nav.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb (footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"fp-c10\") (property \"Reference\" \"C10\") (path \"/sym-c10\")))",
        )
        .unwrap();
        let project_path = temp.path().display().to_string();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let result = handle_resolve_cross_probe_target(
            &json!({
                "source_editor": "schematic",
                "project_name": "nav",
                "project_path": project_path.clone(),
                "schematic_document_path": schematic.display().to_string(),
                "pcb_document_path": board.display().to_string(),
                "schematic_sheet_instance_path": ["root"],
                "sheet_path_human_readable": "/",
                "source_object_kiid": "sym-c10"
            }),
            &context(spawn_cross_probe_context_mock(
                project_path,
                commands.clone(),
            )),
        )
        .await
        .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["resolution"]["source"]["object"]["kiid"], "sym-c10");
        assert_eq!(
            body["resolution"]["destination"]["object"]["kiid"],
            "fp-c10"
        );
        assert_eq!(
            body["resolution"]["evidence"]["source"],
            "saved_kicad_footprint_symbol_path"
        );
        assert_eq!(body["destination_navigation"]["attempted"], false);
        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 2);
        assert!(commands
            .iter()
            .all(|command| command.ends_with("GetOpenDocuments")));
    }

    #[test]
    fn unresolved_cross_probe_has_a_public_structured_refusal() {
        let result =
            cross_probe_error_result(anyhow::Error::new(CrossProbeError::UnresolvedDestination {
                source_kiid: "sym-c10".to_string(),
                reason: "duplicate linkage".to_string(),
                candidates: vec!["fp-a".to_string(), "fp-b".to_string()],
            }));
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("unresolved_cross_probe_destination")
        );
    }
}
