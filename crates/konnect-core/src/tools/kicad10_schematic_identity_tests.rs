//! KiCad 10's schematic editor and the editor-navigation tools (#771).
//!
//! KiCad 10.x answers `GetOpenDocuments(DOCTYPE_SCHEMATIC)` with the sheet's
//! file name where a board's would go, and no project. Measured on KiCad 10.0.5
//! on Windows, from the project manager and from the standalone schematic
//! editor, and reported on 10.0.6 and 10.0.7-rc1 on Windows and Linux. KiCad
//! 11's development builds report the project and the sheet path. That shape
//! is a capability boundary, so it is refused as `unsupported_capability`
//! naming the version; a malformed identity on a supported path stays a
//! `stale_target`.
//!
//! Driven through served `tools/call` against a mock KiCad endpoint.

use crate::mcp::error::extract_error_kind;
use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::{CallToolResult, ToolContent};
use crate::tools::ServerConfig;
use konnect_ipc::builders;
use konnect_ipc::gen::kiapi;
use konnect_ipc::gen::kiapi::common::types::{
    document_specifier::Identifier, DocumentSpecifier, DocumentType, Kiid, ProjectSpecifier,
    SheetPath,
};
use nng::options::Options;
use prost::Message;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../konnect-sexp/tests/fixtures/placement"
);
const PROJECT: &str = "placement_fixture";
/// The fixture schematic's root sheet UUID.
const ROOT_SHEET: &str = "274835a9-5c23-4a9e-a000-24d0710ee122";
/// R1's symbol UUID in the fixture schematic.
const R1_SYMBOL: &str = "bea3fdee-e8c9-4d40-a48d-ba4ee9b5242d";

/// What the mock KiCad answers. `None` is `AS_UNHANDLED`, as KiCad answers a
/// command no open frame handles.
struct Endpoint {
    version: Option<(u32, u32, u32)>,
    schematic: Option<Vec<DocumentSpecifier>>,
    pcb: Option<Vec<DocumentSpecifier>>,
}

fn spawn(endpoint: Endpoint) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let url = format!(
        "inproc://kicad10-schematic-identity-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
    socket
        .set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(10)))
        .expect("timeout");
    socket.listen(&url).expect("listen");
    std::thread::spawn(move || {
        while let Ok(message) = socket.recv() {
            let request =
                kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
            let command = request.message.expect("command");
            let answer = if command.type_url.ends_with("GetVersion") {
                endpoint.version.map(|(major, minor, patch)| {
                    builders::pack_any(
                        &kiapi::common::commands::GetVersionResponse {
                            version: Some(kiapi::common::types::KiCadVersion {
                                major,
                                minor,
                                patch,
                                full_version: format!("{major}.{minor}.{patch}"),
                            }),
                        },
                        "kiapi.common.commands.GetVersionResponse",
                    )
                })
            } else if command.type_url.ends_with("GetOpenDocuments") {
                let query =
                    kiapi::common::commands::GetOpenDocuments::decode(command.value.as_slice())
                        .expect("open documents");
                let documents = if query.r#type == DocumentType::DoctypeSchematic as i32 {
                    endpoint.schematic.clone()
                } else {
                    endpoint.pcb.clone()
                };
                documents.map(|documents| {
                    builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse { documents },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )
                })
            } else if command.type_url.ends_with("GetSelection")
                || command.type_url.ends_with("ClearSelection")
            {
                Some(builders::pack_any(
                    &kiapi::common::commands::SelectionResponse { items: Vec::new() },
                    "kiapi.common.commands.SelectionResponse",
                ))
            } else {
                None
            };
            let (status, error_message) = if answer.is_some() {
                (kiapi::common::ApiStatusCode::AsOk, String::new())
            } else {
                (
                    kiapi::common::ApiStatusCode::AsUnhandled,
                    format!(
                        "no handler available for request of type {}",
                        command.type_url
                    ),
                )
            };
            let response = kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: status as i32,
                    error_message,
                }),
                header: None,
                message: answer,
            };
            if socket
                .send(nng::Message::from(response.encode_to_vec().as_slice()))
                .is_err()
            {
                break;
            }
        }
    });
    url
}

/// The fixture project, copied so its path is this test's own.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for extension in ["kicad_pro", "kicad_sch", "kicad_pcb"] {
            std::fs::copy(
                Path::new(FIXTURE_DIR).join(format!("{PROJECT}.{extension}")),
                dir.path().join(format!("{PROJECT}.{extension}")),
            )
            .unwrap();
        }
        Self { dir }
    }

    fn path(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
    }

    fn file(&self, extension: &str) -> PathBuf {
        self.dir.path().join(format!("{PROJECT}.{extension}"))
    }

    fn identity(&self) -> ProjectSpecifier {
        ProjectSpecifier {
            name: PROJECT.to_string(),
            path: self.path(),
        }
    }
}

/// KiCad 10.x's schematic: the file name in the board-file slot, no project.
fn kicad_10_schematic() -> DocumentSpecifier {
    DocumentSpecifier {
        r#type: DocumentType::DoctypeSchematic as i32,
        project: None,
        identifier: Some(Identifier::BoardFilename(format!("{PROJECT}.kicad_sch"))),
    }
}

/// KiCad 11's schematic: the project and the sheet path.
fn kicad_11_schematic(project: &Project) -> DocumentSpecifier {
    DocumentSpecifier {
        r#type: DocumentType::DoctypeSchematic as i32,
        project: Some(project.identity()),
        identifier: Some(Identifier::SheetPath(SheetPath {
            path: vec![Kiid {
                value: ROOT_SHEET.to_string(),
            }],
            path_human_readable: "/".to_string(),
        })),
    }
}

fn pcb(project: &Project, filename: &str) -> DocumentSpecifier {
    DocumentSpecifier {
        r#type: DocumentType::DoctypePcb as i32,
        project: Some(project.identity()),
        identifier: Some(Identifier::BoardFilename(filename.to_string())),
    }
}

async fn call(address: &str, tool: &str, arguments: Value) -> CallToolResult {
    let handler = McpHandler::new(ServerConfig {
        ipc_address: address.to_string(),
        eager_toolsets: true,
        ..Default::default()
    })
    .await
    .expect("handler builds");
    let response = handler
        .handle_message(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }))
        .await
        .expect("tools/call receives a response");
    assert!(response.error.is_none(), "tool errors are MCP results");
    serde_json::from_value(response.result.expect("a result"))
        .expect("result uses the advertised MCP shape")
}

fn body(result: &CallToolResult) -> Value {
    let ToolContent::Text { text } = &result.content[0] else {
        panic!("expected a text result");
    };
    serde_json::from_str(text).unwrap_or_else(|_| json!({ "text": text }))
}

fn editor<'a>(state: &'a Value, name: &str) -> &'a Value {
    state["editors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|editor| editor["editor"] == name)
        .unwrap_or_else(|| panic!("no {name} editor in {state:#}"))
}

fn schematic_selection(project: &Project) -> Value {
    json!({
        "editor": "schematic",
        "project_name": PROJECT,
        "project_path": project.path(),
        "sheet_instance_path": [ROOT_SHEET]
    })
}

/// The refusal every schematic-side tool gives on KiCad 10.
fn assert_unsupported_identity(result: &CallToolResult, version: Value) {
    assert!(result.is_error, "{:#}", body(result));
    assert_eq!(
        extract_error_kind(result).as_deref(),
        Some("unsupported_capability"),
        "{:#}",
        body(result)
    );
    let refusal = body(result);
    assert_eq!(
        refusal["error"]["capability"], "schematic_editor_document_identity",
        "{refusal:#}"
    );
    assert_eq!(refusal["error"]["kicad_version"], version, "{refusal:#}");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("needs KiCad 11 or later"),
        "{refusal:#}"
    );
}

/// `get_editor_state` on KiCad 10 with the schematic editor and a board open:
/// the schematic editor is reported, unaddressable, with the reason and the
/// version; the PCB editor is observed as before. It used to refuse the whole
/// observation as a `stale_target`.
#[tokio::test]
async fn kicad_10s_schematic_editor_is_reported_unsupported_beside_its_pcb_editor() {
    let project = Project::new();
    let address = spawn(Endpoint {
        version: Some((10, 0, 6)),
        schematic: Some(vec![kicad_10_schematic()]),
        pcb: Some(vec![pcb(&project, &format!("{PROJECT}.kicad_pcb"))]),
    });

    let result = call(&address, "get_editor_state", json!({})).await;

    assert!(!result.is_error, "{:#}", body(&result));
    let state = body(&result);
    let schematic = editor(&state, "schematic");
    assert_eq!(schematic["addressable"], false, "{state:#}");
    assert_eq!(schematic["documents"], json!([]), "{state:#}");
    let reason = schematic["unavailable_reason"].as_str().unwrap();
    assert!(
        reason.contains("KiCad 10.0.6 reports no project or sheet identity")
            && reason.contains("needs KiCad 11 or later"),
        "{reason}"
    );
    for capability in [
        "observe_documents",
        "read_selection",
        "mutate_selection",
        "cross_probe",
    ] {
        let capability = &schematic["capabilities"][capability];
        assert_eq!(capability["availability"], "unsupported", "{state:#}");
        assert_eq!(capability["reason"], reason, "{state:#}");
    }

    let board = editor(&state, "pcb");
    assert_eq!(board["addressable"], true, "{state:#}");
    assert_eq!(
        board["documents"][0]["project"]["name"], PROJECT,
        "{state:#}"
    );
    assert_eq!(
        board["capabilities"]["read_selection"]["availability"], "available",
        "{state:#}"
    );
}

/// Every schematic-side tool refuses KiCad 10's schematic editor as an
/// unsupported capability naming the version, without guessing an identity
/// from the caller's project, the saved file or the board. Each used to be a
/// `stale_target` that read as a Konnect-side race.
#[tokio::test]
async fn every_schematic_tool_refuses_kicad_10s_schematic_editor_as_unsupported() {
    let project = Project::new();
    let endpoint = || Endpoint {
        version: Some((10, 0, 6)),
        schematic: Some(vec![kicad_10_schematic()]),
        pcb: Some(vec![pcb(&project, &format!("{PROJECT}.kicad_pcb"))]),
    };
    let schematic = project.file("kicad_sch").to_string_lossy().into_owned();
    let board = project.file("kicad_pcb").to_string_lossy().into_owned();

    let selection = call(
        &spawn(endpoint()),
        "get_editor_selection",
        schematic_selection(&project),
    )
    .await;
    assert_unsupported_identity(&selection, json!("10.0.6"));

    let mut clear = schematic_selection(&project);
    clear["operation"] = json!("clear");
    clear["document_path"] = json!(schematic);
    clear["object_kiids"] = json!([]);
    let mutation = call(&spawn(endpoint()), "mutate_editor_selection", clear).await;
    assert_unsupported_identity(&mutation, json!("10.0.6"));

    let mut navigation = schematic_selection(&project);
    navigation["document_path"] = json!(schematic);
    navigation["object_kiid"] = json!(R1_SYMBOL);
    let target = call(&spawn(endpoint()), "resolve_navigation_target", navigation).await;
    assert_unsupported_identity(&target, json!("10.0.6"));

    let cross_probe = call(
        &spawn(endpoint()),
        "resolve_cross_probe_target",
        json!({
            "source_editor": "schematic",
            "project_name": PROJECT,
            "project_path": project.path(),
            "schematic_document_path": schematic,
            "pcb_document_path": board,
            "schematic_sheet_instance_path": [ROOT_SHEET],
            "source_object_kiid": R1_SYMBOL
        }),
    )
    .await;
    assert_unsupported_identity(&cross_probe, json!("10.0.6"));
}

/// KiCad 10's standalone schematic editor does not answer `GetVersion`. The
/// refusal is the same; it just cannot name the version.
#[tokio::test]
async fn a_standalone_kicad_10_schematic_editor_is_refused_without_a_version() {
    let project = Project::new();
    let address = spawn(Endpoint {
        version: None,
        schematic: Some(vec![kicad_10_schematic()]),
        pcb: None,
    });

    let selection = call(
        &address,
        "get_editor_selection",
        schematic_selection(&project),
    )
    .await;

    assert_unsupported_identity(&selection, Value::Null);
    assert!(
        body(&selection)["message"]
            .as_str()
            .unwrap()
            .starts_with("This KiCad reports no project or sheet identity"),
        "{:#}",
        body(&selection)
    );
}

/// A complete schematic identity, as KiCad 11's development builds report it,
/// is addressable, and its selection is read.
#[tokio::test]
async fn a_complete_kicad_11_schematic_identity_is_addressable() {
    let project = Project::new();
    let endpoint = || Endpoint {
        version: Some((10, 99, 0)),
        schematic: Some(vec![kicad_11_schematic(&project)]),
        pcb: None,
    };

    let state = call(&spawn(endpoint()), "get_editor_state", json!({})).await;
    assert!(!state.is_error, "{:#}", body(&state));
    let state = body(&state);
    let schematic = editor(&state, "schematic");
    assert_eq!(schematic["addressable"], true, "{state:#}");
    assert_eq!(
        schematic["documents"][0]["sheet_instance_path"]["kiids"],
        json!([ROOT_SHEET]),
        "{state:#}"
    );

    let selection = call(
        &spawn(endpoint()),
        "get_editor_selection",
        schematic_selection(&project),
    )
    .await;
    assert!(!selection.is_error, "{:#}", body(&selection));
    assert_eq!(body(&selection)["selected_objects"], json!([]));
}

/// A malformed identity on a supported path is still a `stale_target`: a PCB
/// with no file name, and the two schematic shapes that are not KiCad 10's,
/// one with a project but no sheet path and one with nothing at all.
#[tokio::test]
async fn a_malformed_identity_is_still_a_stale_target() {
    let project = Project::new();
    let schematic_with_project = DocumentSpecifier {
        project: Some(project.identity()),
        ..kicad_10_schematic()
    };
    let schematic_with_nothing = DocumentSpecifier {
        r#type: DocumentType::DoctypeSchematic as i32,
        project: None,
        identifier: Some(Identifier::BoardFilename(String::new())),
    };
    for (label, endpoint) in [
        (
            "a PCB with no file name",
            Endpoint {
                version: Some((10, 0, 6)),
                schematic: None,
                pcb: Some(vec![pcb(&project, "")]),
            },
        ),
        (
            "a schematic with a project but no sheet path",
            Endpoint {
                version: Some((10, 0, 6)),
                schematic: Some(vec![schematic_with_project]),
                pcb: None,
            },
        ),
        (
            "a schematic with neither",
            Endpoint {
                version: Some((10, 0, 6)),
                schematic: Some(vec![schematic_with_nothing]),
                pcb: None,
            },
        ),
    ] {
        let result = call(&spawn(endpoint), "get_editor_state", json!({})).await;
        assert!(result.is_error, "{label}: {:#}", body(&result));
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("stale_target"),
            "{label}: {:#}",
            body(&result)
        );
        assert_eq!(
            body(&result)["error"]["reason"],
            "missing or empty document identifier",
            "{label}"
        );
    }

    // The selection tools give the identity's own reason too, where they used
    // to say only that the readback was incomplete.
    let selection = call(
        &spawn(Endpoint {
            version: Some((10, 0, 6)),
            schematic: Some(vec![DocumentSpecifier {
                project: Some(project.identity()),
                ..kicad_10_schematic()
            }]),
            pcb: None,
        }),
        "get_editor_selection",
        schematic_selection(&project),
    )
    .await;
    assert_eq!(
        extract_error_kind(&selection).as_deref(),
        Some("stale_target"),
        "{:#}",
        body(&selection)
    );
    assert_eq!(
        body(&selection)["error"]["reason"],
        "missing or empty document identifier",
        "{:#}",
        body(&selection)
    );
}
