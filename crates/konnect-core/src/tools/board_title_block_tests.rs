//! `set_board_title_block` through served `tools/call` (#714).
//!
//! The file half runs on pcbnew 10.0.6 saves (`konnect-sexp/tests/fixtures/
//! title_block/`): each expected file is KiCad's own save after the same edit,
//! so a passing test means Konnect wrote the bytes KiCad writes. The live half
//! runs on a KiCad double that keeps one title block, records what each
//! request names, and can answer as KiCad 10.0.0 does (no `SetTitleBlockInfo`)
//! or lose a field on the way in.

use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::CallToolResult;
use crate::test_support::MockIpcServer;
use crate::tools::pcb_board::board_mock::board_document;
use crate::tools::ServerConfig;
use konnect_ipc::builders::pack_any;
use konnect_ipc::gen::kiapi;
use prost::Message;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../konnect-sexp/tests/fixtures/title_block")
        .join(name);
    std::fs::read_to_string(path).expect("the title-block fixtures are committed")
}

async fn handler(address: &str) -> McpHandler {
    McpHandler::new(ServerConfig {
        kicad_cli: String::new(),
        kicad_binary: String::new(),
        ipc_address: address.to_string(),
        project_dir: None,
        jlcpcb_db_path: None,
        auto_load_toolsets: true,
        eager_toolsets: true,
    })
    .await
    .expect("handler builds")
}

async fn call(handler: &McpHandler, board: &Path, mut arguments: Value) -> CallToolResult {
    arguments["board"] = json!(board.to_string_lossy());
    let response = handler
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 714, "method": "tools/call",
            "params": { "name": "set_board_title_block", "arguments": arguments }
        }))
        .await
        .expect("tools/call receives a response");
    assert!(response.error.is_none(), "tool errors are MCP results");
    serde_json::from_value(response.result.expect("a result")).expect("an MCP result")
}

fn body(result: &CallToolResult) -> Value {
    match result.content.first() {
        Some(crate::mcp::protocol::ToolContent::Text { text }) => {
            serde_json::from_str(text).unwrap_or_else(|_| json!({ "message": text }))
        }
        other => panic!("expected text content, got {other:?}"),
    }
}

fn board_with(dir: &Path, fixture_name: &str) -> PathBuf {
    let board = dir.join("board.kicad_pcb");
    std::fs::write(&board, fixture(fixture_name)).unwrap();
    board
}

/// Every field of `board.kicad_pcb`, as pcbnew's `GetTitleBlock()` reads it.
fn fixture_block() -> Value {
    json!({
        "title": "Rig \"Interface\" ${PROJECTNAME}", "date": "2026-10-20",
        "revision": "B", "company": "C:\\new\\tab",
        "comment_1": "first", "comment_2": "", "comment_3": "third",
        "comment_4": "", "comment_5": "", "comment_6": "", "comment_7": "",
        "comment_8": "", "comment_9": "ninth"
    })
}

// ─── File path ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn closed_board_edit_writes_what_kicad_writes() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_with(dir.path(), "board.kicad_pcb");

    let result = call(
        &handler("").await,
        &board,
        json!({ "revision": "C", "comment_2": "second", "comment_3": "" }),
    )
    .await;

    assert!(!result.is_error, "{}", body(&result));
    assert_eq!(
        std::fs::read_to_string(&board).unwrap(),
        fixture("board_edited.kicad_pcb")
    );
    let body = body(&result);
    let mut expected = fixture_block();
    expected["revision"] = json!("C");
    expected["comment_2"] = json!("second");
    expected["comment_3"] = json!("");
    assert_eq!(body["title_block"], expected);
    assert_eq!(
        body["changed"],
        json!(["revision", "comment_2", "comment_3"])
    );
    assert_eq!(body["source"], "file");
    assert_eq!(body["saved"], true);
    assert_eq!(body["fallback_reason"]["kind"], "transport_unreachable");
}

#[tokio::test]
async fn closed_board_insert_and_clear_match_kicad() {
    let dir = tempfile::tempdir().unwrap();
    let handler = handler("").await;

    let board = board_with(dir.path(), "board_empty.kicad_pcb");
    let result = call(
        &handler,
        &board,
        json!({ "title": "Fresh", "comment_4": "fourth" }),
    )
    .await;
    assert!(!result.is_error, "{}", body(&result));
    assert_eq!(
        std::fs::read_to_string(&board).unwrap(),
        fixture("board_inserted.kicad_pcb")
    );

    let board = board_with(dir.path(), "board.kicad_pcb");
    let cleared: serde_json::Map<_, _> = konnect_sexp::title_block::FIELDS
        .iter()
        .map(|field| (field.to_string(), json!("")))
        .collect();
    let result = call(&handler, &board, Value::Object(cleared)).await;
    assert!(!result.is_error, "{}", body(&result));
    assert_eq!(
        std::fs::read_to_string(&board).unwrap(),
        fixture("board_cleared.kicad_pcb")
    );
    assert_eq!(
        body(&result)["changed"],
        json!([
            "title",
            "date",
            "revision",
            "company",
            "comment_1",
            "comment_3",
            "comment_9"
        ])
    );
}

#[tokio::test]
async fn a_value_already_stored_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_with(dir.path(), "board.kicad_pcb");
    let modified = std::fs::metadata(&board).unwrap().modified().unwrap();

    let result = call(&handler("").await, &board, json!({ "revision": "B" })).await;

    assert!(!result.is_error, "{}", body(&result));
    assert_eq!(body(&result)["changed"], json!([]));
    assert_eq!(body(&result)["title_block"], fixture_block());
    assert_eq!(
        std::fs::metadata(&board).unwrap().modified().unwrap(),
        modified
    );
}

#[tokio::test]
async fn refusals_leave_the_board_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_with(dir.path(), "board.kicad_pcb");
    let handler = handler("").await;
    let original = fixture("board.kicad_pcb");

    for (arguments, field) in [
        (json!({}), "title_block"),
        (json!({ "revision": 3 }), "revision"),
        (json!({ "comment_1": null }), "comment_1"),
        (json!({ "rev": "C" }), "rev"),
        (json!({ "comment_10": "x" }), "comment_10"),
    ] {
        let result = call(&handler, &board, arguments.clone()).await;
        assert!(result.is_error, "{arguments} was accepted");
        let body = body(&result);
        assert_eq!(
            body["error"]["kind"], "invalid_argument",
            "{arguments}: {body}"
        );
        let named = body["error"]["field"].as_str().unwrap_or_default();
        assert!(named.contains(field), "{arguments} named {named}: {body}");
    }
    assert_eq!(std::fs::read_to_string(&board).unwrap(), original);

    // A field the model cannot hold is refused, never dropped.
    let repeated = original.replace("\t\t(rev \"B\")\n", "\t\t(rev \"B\")\n\t\t(rev \"B2\")\n");
    std::fs::write(&board, &repeated).unwrap();
    let result = call(&handler, &board, json!({ "title": "x" })).await;
    assert!(result.is_error);
    let refusal = body(&result);
    assert_eq!(
        refusal["error"]["capability"], "unrepresentable_title_block",
        "{refusal}"
    );
    assert!(refusal["message"]
        .as_str()
        .unwrap()
        .contains("repeats revision"));
    assert_eq!(std::fs::read_to_string(&board).unwrap(), repeated);
}

#[tokio::test]
async fn a_kicad_lock_refuses_the_file_write() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_with(dir.path(), "board.kicad_pcb");
    std::fs::write(dir.path().join("~board.kicad_pcb.lck"), "{}").unwrap();

    let result = call(&handler("").await, &board, json!({ "revision": "C" })).await;

    assert!(result.is_error);
    let body = body(&result);
    assert_eq!(body["error"]["kind"], "unsafe_file_fallback", "{body}");
    assert_eq!(body["error"]["reason"], "kicad_lock_present");
    assert_eq!(
        std::fs::read_to_string(&board).unwrap(),
        fixture("board.kicad_pcb")
    );
}

/// KiCad answers and holds another board, so the gate allows the file path;
/// the lock beside this board must still stop the write itself.
#[tokio::test]
async fn a_lock_beside_a_board_kicad_does_not_hold_refuses_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_with(dir.path(), "board.kicad_pcb");
    let other = dir.path().join("other.kicad_pcb");
    let kicad = crate::tools::pcb_board::board_mock::spawn_kicad_holding_board(&other, |_| None);
    std::fs::write(dir.path().join("~board.kicad_pcb.lck"), "{}").unwrap();

    let result = call(
        &handler(kicad.address()).await,
        &board,
        json!({ "revision": "C" }),
    )
    .await;

    assert!(result.is_error);
    let body = body(&result);
    assert_eq!(body["error"]["kind"], "unsafe_file_fallback", "{body}");
    assert_eq!(body["error"]["reason"], "kicad_lock_present");
    assert_eq!(
        std::fs::read_to_string(&board).unwrap(),
        fixture("board.kicad_pcb")
    );
}

// ─── Live board ──────────────────────────────────────────────────────────────

/// How the double handles `SetTitleBlockInfo`.
#[derive(Clone, Copy)]
enum SetBehaviour {
    /// Store the block, as KiCad 10.0.1 and later do.
    Store,
    /// Answer `AS_UNHANDLED`, as KiCad 10.0.0's PCB editor does.
    Unhandled,
    /// Store the block without comment 9.
    DropComment9,
}

struct LiveKiCad {
    _server: MockIpcServer,
    block: Arc<Mutex<kiapi::common::types::TitleBlockInfo>>,
    /// Each title-block request: its command and the board it names.
    requests: Arc<Mutex<Vec<(&'static str, String)>>>,
}

fn named_board(document: Option<kiapi::common::types::DocumentSpecifier>) -> String {
    match document.and_then(|document| document.identifier) {
        Some(kiapi::common::types::document_specifier::Identifier::BoardFilename(name)) => name,
        other => format!("{other:?}"),
    }
}

fn live_kicad(board: &Path, behaviour: SetBehaviour) -> LiveKiCad {
    let block = Arc::new(Mutex::new(kiapi::common::types::TitleBlockInfo {
        title: "Live title".into(),
        revision: "B".into(),
        comment9: "keep me".into(),
        ..Default::default()
    }));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let documents = vec![board_document(&board.to_string_lossy())];
    let (stored, log) = (block.clone(), requests.clone());
    let server = MockIpcServer::spawn("title-block", move |request| {
        let command = request.message.expect("a command");
        let ok = |message| kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsOk as i32,
                error_message: String::new(),
            }),
            header: None,
            message,
        };
        let name = command.type_url.rsplit('.').next().unwrap_or_default();
        match name {
            "GetOpenDocuments" => ok(Some(pack_any(
                &kiapi::common::commands::GetOpenDocumentsResponse {
                    documents: documents.clone(),
                },
                "kiapi.common.commands.GetOpenDocumentsResponse",
            ))),
            "GetVersion" => ok(Some(pack_any(
                &kiapi::common::commands::GetVersionResponse {
                    version: Some(kiapi::common::types::KiCadVersion {
                        major: 10,
                        minor: 0,
                        patch: 0,
                        full_version: "10.0.0".into(),
                    }),
                },
                "kiapi.common.commands.GetVersionResponse",
            ))),
            "GetTitleBlockInfo" => {
                let asked =
                    kiapi::common::commands::GetTitleBlockInfo::decode(command.value.as_slice())
                        .unwrap();
                log.lock()
                    .unwrap()
                    .push(("get", named_board(asked.document)));
                ok(Some(pack_any(
                    &*stored.lock().unwrap(),
                    "kiapi.common.types.TitleBlockInfo",
                )))
            }
            "SetTitleBlockInfo" => {
                let asked =
                    kiapi::common::commands::SetTitleBlockInfo::decode(command.value.as_slice())
                        .unwrap();
                log.lock()
                    .unwrap()
                    .push(("set", named_board(asked.document)));
                let mut sent = asked.title_block.unwrap_or_default();
                match behaviour {
                    SetBehaviour::Unhandled => {
                        return kiapi::common::ApiResponse {
                            status: Some(kiapi::common::ApiResponseStatus {
                                status: kiapi::common::ApiStatusCode::AsUnhandled as i32,
                                error_message: "no handler available for request".into(),
                            }),
                            header: None,
                            message: None,
                        }
                    }
                    SetBehaviour::DropComment9 => sent.comment9.clear(),
                    SetBehaviour::Store => {}
                }
                *stored.lock().unwrap() = sent;
                ok(None)
            }
            _ => ok(None),
        }
    });
    LiveKiCad {
        _server: server,
        block,
        requests,
    }
}

#[tokio::test]
async fn a_live_board_gets_the_merged_block_and_reports_its_readback() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_with(dir.path(), "board.kicad_pcb");
    let kicad = live_kicad(&board, SetBehaviour::Store);

    let result = call(
        &handler(kicad._server.address()).await,
        &board,
        json!({ "revision": "C", "comment_1": "${PROJECTNAME}" }),
    )
    .await;

    assert!(!result.is_error, "{}", body(&result));
    // The fields the call did not name survive KiCad's replace-all handler.
    let stored = kicad.block.lock().unwrap().clone();
    assert_eq!(
        (stored.title.as_str(), stored.revision.as_str()),
        ("Live title", "C")
    );
    assert_eq!(
        (stored.comment1.as_str(), stored.comment9.as_str()),
        ("${PROJECTNAME}", "keep me")
    );
    let body = body(&result);
    assert_eq!(body["source"], "ipc");
    assert_eq!(body["saved"], false);
    assert_eq!(body["changed"], json!(["revision", "comment_1"]));
    assert_eq!(body["title_block"]["comment_9"], "keep me");
    assert_eq!(body["title_block"]["revision"], "C");
    // Read, write, and a fresh read, each naming the requested board.
    let board_name = board.to_string_lossy().to_string();
    assert_eq!(
        *kicad.requests.lock().unwrap(),
        vec![
            ("get", board_name.clone()),
            ("set", board_name.clone()),
            ("get", board_name)
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&board).unwrap(),
        fixture("board.kicad_pcb")
    );
}

#[tokio::test]
async fn a_live_value_already_stored_sends_no_write() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_with(dir.path(), "board.kicad_pcb");
    let kicad = live_kicad(&board, SetBehaviour::Store);

    let result = call(
        &handler(kicad._server.address()).await,
        &board,
        json!({ "revision": "B" }),
    )
    .await;

    assert!(!result.is_error, "{}", body(&result));
    assert_eq!(body(&result)["changed"], json!([]));
    assert_eq!(body(&result)["undo"], Value::Null);
    assert!(kicad
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|(command, _)| *command == "get"));
}

#[tokio::test]
async fn a_kicad_without_the_command_refuses_without_touching_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_with(dir.path(), "board.kicad_pcb");
    let kicad = live_kicad(&board, SetBehaviour::Unhandled);

    let result = call(
        &handler(kicad._server.address()).await,
        &board,
        json!({ "revision": "C" }),
    )
    .await;

    assert!(result.is_error);
    let body = body(&result);
    assert_eq!(body["error"]["kind"], "unsupported_capability", "{body}");
    assert_eq!(body["error"]["capability"], "set_board_title_block");
    assert_eq!(body["error"]["kicad_version"], "10.0.0");
    assert_eq!(
        std::fs::read_to_string(&board).unwrap(),
        fixture("board.kicad_pcb")
    );
}

#[tokio::test]
async fn a_live_readback_that_differs_is_uncertain() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_with(dir.path(), "board.kicad_pcb");
    let kicad = live_kicad(&board, SetBehaviour::DropComment9);

    let result = call(
        &handler(kicad._server.address()).await,
        &board,
        json!({ "revision": "C" }),
    )
    .await;

    assert!(result.is_error);
    let body = body(&result);
    assert_eq!(
        body["error"]["kind"], "mutation_outcome_uncertain",
        "{body}"
    );
    assert!(
        body["error"]["reason"]
            .as_str()
            .unwrap()
            .contains("comment_9"),
        "{body}"
    );
    assert_eq!(
        std::fs::read_to_string(&board).unwrap(),
        fixture("board.kicad_pcb")
    );
}
