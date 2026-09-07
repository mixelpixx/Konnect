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
    IpcEditorDocument, IpcEditorKind, IpcProjectIdentity, IpcSelectionObservationErrorKind,
    IpcSheetInstancePath,
};
use serde_json::json;

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
    use std::sync::Arc;
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

    #[test]
    fn public_tool_is_read_only_and_takes_no_required_arguments() {
        let definitions = tools();
        assert_eq!(definitions.len(), 2);
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
}
