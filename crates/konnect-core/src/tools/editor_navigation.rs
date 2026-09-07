//! Semantic KiCad editor observation and navigation.
//!
//! Public names in this module are provisional while upstream design issue
//! #395 is under review. The implementation deliberately exposes only typed
//! semantic operations; KiCad's unstable raw `RunAction` strings are not part
//! of this surface.

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{ToolContext, ToolDef};
use serde_json::json;

pub fn tools() -> Vec<ToolDef> {
    vec![tool!(
        "get_editor_state",
        "Observe the configured KiCad IPC endpoint: running KiCad version, addressable schematic and PCB editors, exact open document identities, and semantic navigation capabilities. Active editor/document/sheet fields are null when KiCad has no stable typed query; open-document order is never treated as active state.",
        json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        |args, ctx| async move { handle_get_editor_state(args, ctx).await }
    )]
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

    #[test]
    fn public_tool_is_read_only_and_takes_no_required_arguments() {
        let definitions = tools();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "get_editor_state");
        assert_eq!(definitions[0].input_schema["required"], json!([]));
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
}
