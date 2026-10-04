//! The `.kicad_pro` writers while KiCad holds the board (#791) or the
//! project (#804).
//!
//! Saving the board in KiCad rewrites the project file from KiCad's own copy.
//! So `set_design_rules`, `create_netclass` and `assign_net_to_class` must not
//! write while KiCad holds the board: the write would be reported and then
//! reverted. Measured on KiCad 10.0.5 before the fix: all three reported
//! success, and `run_drc` with `sync_live_board` then put every value back.
//!
//! Eeschema rewrites the project file on save as well, and holds no board, so
//! the writers also refuse while a KiCad program holds the project lock,
//! `~<project>.kicad_pro.lck`. Measured on KiCad 10.0.5: pcbnew, Eeschema and
//! the project manager each create it, and an Eeschema save reverted all three
//! #791 writers' values.
//!
//! Driven through served `tools/call`, so dispatch, schema and handler are all
//! under test. The project is the KiCad-written placement fixture
//! (`konnect-sexp/tests/fixtures/placement/`).

use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::{CallToolResult, ToolContent};
use crate::tools::pcb_board::board_mock::spawn_kicad_holding_boards;
use crate::tools::ServerConfig;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../konnect-sexp/tests/fixtures/placement"
);

/// A copy of the fixture project, kept alive for the test's duration.
struct Project {
    _dir: tempfile::TempDir,
    board: PathBuf,
    project_file: PathBuf,
}

fn project() -> Project {
    let dir = tempfile::tempdir().unwrap();
    for extension in ["kicad_pcb", "kicad_pro"] {
        std::fs::copy(
            Path::new(FIXTURE_DIR).join(format!("placement_fixture.{extension}")),
            dir.path().join(format!("placement_fixture.{extension}")),
        )
        .unwrap();
    }
    Project {
        board: dir.path().join("placement_fixture.kicad_pcb"),
        project_file: dir.path().join("placement_fixture.kicad_pro"),
        _dir: dir,
    }
}

async fn handler_talking_to(address: &str) -> McpHandler {
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

async fn call(handler: &McpHandler, tool: &str, board: &Path, arguments: &Value) -> CallToolResult {
    let mut arguments = arguments.clone();
    arguments["board"] = json!(board.to_string_lossy());
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

fn text_of(result: &CallToolResult) -> &str {
    match result.content.first() {
        Some(ToolContent::Text { text }) => text,
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Arguments that change something the fixture's project file does not
/// already hold (its rules say 0.2, it has only `Default`, and no net is
/// assigned), so a write that happened would show in the file.
fn arguments_for(tool: &str) -> Value {
    match tool {
        "set_design_rules" => json!({ "min_trace_width": 0.35 }),
        "create_netclass" => json!({ "name": "Power", "trace_width": 0.5 }),
        "assign_net_to_class" => json!({ "net_name": "VCC", "netclass": "Default" }),
        // The fixture's palette is empty, so any list is a change.
        "set_predefined_sizes" => json!({ "track_widths": [0.3] }),
        other => panic!("no arguments for {other}"),
    }
}

/// KiCad holds this very board: the tool refuses, and neither the project
/// file nor the board changes by a byte.
async fn assert_refused_while_kicad_holds_the_board(tool: &str) {
    let project = project();
    let project_before = std::fs::read(&project.project_file).unwrap();
    let board_before = std::fs::read(&project.board).unwrap();
    let kicad = spawn_kicad_holding_boards(&[&project.board], |_| None);
    let handler = handler_talking_to(kicad.address()).await;

    let result = call(&handler, tool, &project.board, &arguments_for(tool)).await;

    assert!(
        result.is_error,
        "{tool} must refuse while KiCad holds the board: {}",
        text_of(&result)
    );
    assert!(
        text_of(&result).contains("currently holds this board open"),
        "{tool} must say why it refused: {}",
        text_of(&result)
    );
    assert_eq!(
        std::fs::read(&project.project_file).unwrap(),
        project_before,
        "{tool} wrote the project file while KiCad held the board"
    );
    assert_eq!(std::fs::read(&project.board).unwrap(), board_before);
}

#[tokio::test]
async fn set_design_rules_refuses_while_kicad_holds_the_board() {
    assert_refused_while_kicad_holds_the_board("set_design_rules").await;
}

#[tokio::test]
async fn create_netclass_refuses_while_kicad_holds_the_board() {
    assert_refused_while_kicad_holds_the_board("create_netclass").await;
}

#[tokio::test]
async fn assign_net_to_class_refuses_while_kicad_holds_the_board() {
    assert_refused_while_kicad_holds_the_board("assign_net_to_class").await;
}

/// The control: a KiCad holding some other board does not own this project
/// file, so each writer still writes, and the file says so.
#[tokio::test]
async fn each_writer_still_writes_while_kicad_holds_a_different_board() {
    let elsewhere = tempfile::tempdir().unwrap();
    let other_board = elsewhere.path().join("other.kicad_pcb");
    std::fs::copy(
        Path::new(FIXTURE_DIR).join("placement_fixture.kicad_pcb"),
        &other_board,
    )
    .unwrap();

    for tool in ["set_design_rules", "create_netclass", "assign_net_to_class"] {
        let project = project();
        let kicad = spawn_kicad_holding_boards(&[&other_board], |_| None);
        let handler = handler_talking_to(kicad.address()).await;

        let result = call(&handler, tool, &project.board, &arguments_for(tool)).await;
        assert!(!result.is_error, "{tool}: {}", text_of(&result));

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&project.project_file).unwrap()).unwrap();
        let net_settings = &written["net_settings"];
        match tool {
            "set_design_rules" => assert_eq!(
                written["board"]["design_settings"]["rules"]["min_track_width"],
                json!(0.35)
            ),
            "create_netclass" => assert!(net_settings["classes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|class| class["name"] == "Power" && class["track_width"] == json!(0.5))),
            "assign_net_to_class" => assert_eq!(
                net_settings["netclass_patterns"],
                json!([{ "netclass": "Default", "pattern": "VCC" }])
            ),
            _ => unreachable!(),
        }
    }
}

/// Every tool that writes the board's project file.
const PROJECT_FILE_WRITERS: [&str; 4] = [
    "set_design_rules",
    "set_predefined_sizes",
    "create_netclass",
    "assign_net_to_class",
];

/// KiCad's project lock, where pcbnew, Eeschema and the project manager each
/// create it. Its contents name a host and user; only its presence is read.
fn lock_project(project: &Project) -> PathBuf {
    let lock = project
        .project_file
        .with_file_name("~placement_fixture.kicad_pro.lck");
    std::fs::write(&lock, r#"{"hostname":"HOST","username":"user"}"#).unwrap();
    lock
}

fn body_of(result: &CallToolResult) -> Value {
    serde_json::from_str(text_of(result)).expect("a structured refusal")
}

/// No KiCad answers over IPC, as when only Eeschema or the project manager is
/// running, but a KiCad program holds the project: the tool refuses, names
/// the lock, and the project file does not change by a byte.
async fn assert_refused_while_kicad_holds_the_project(tool: &str) {
    let project = project();
    let lock = lock_project(&project);
    let before = std::fs::read(&project.project_file).unwrap();
    let handler = handler_talking_to("").await;

    let result = call(&handler, tool, &project.board, &arguments_for(tool)).await;

    assert!(result.is_error, "{tool}: {}", text_of(&result));
    let body = body_of(&result);
    assert_eq!(
        body["error"]["kind"], "unsafe_file_fallback",
        "{tool}: {body}"
    );
    assert_eq!(
        body["error"]["reason"], "kicad_project_lock_present",
        "{tool}: {body}"
    );
    assert_eq!(
        body["error"]["path"],
        project.project_file.display().to_string(),
        "{tool}: {body}"
    );
    let message = body["message"].as_str().unwrap();
    let lock_name = lock.file_name().unwrap().to_string_lossy();
    assert!(
        message.contains(lock_name.as_ref()),
        "{tool} must name the lock: {message}"
    );
    assert_eq!(
        std::fs::read(&project.project_file).unwrap(),
        before,
        "{tool} wrote the project file while KiCad held the project"
    );
}

#[tokio::test]
async fn set_design_rules_refuses_while_kicad_holds_the_project() {
    assert_refused_while_kicad_holds_the_project("set_design_rules").await;
}

#[tokio::test]
async fn set_predefined_sizes_refuses_while_kicad_holds_the_project() {
    assert_refused_while_kicad_holds_the_project("set_predefined_sizes").await;
}

#[tokio::test]
async fn create_netclass_refuses_while_kicad_holds_the_project() {
    assert_refused_while_kicad_holds_the_project("create_netclass").await;
}

#[tokio::test]
async fn assign_net_to_class_refuses_while_kicad_holds_the_project() {
    assert_refused_while_kicad_holds_the_project("assign_net_to_class").await;
}

/// The control: the same project with no lock and no KiCad is written by
/// every one of the four.
#[tokio::test]
async fn each_writer_writes_when_no_kicad_program_holds_the_project() {
    for tool in PROJECT_FILE_WRITERS {
        let project = project();
        let before = std::fs::read(&project.project_file).unwrap();
        let handler = handler_talking_to("").await;

        let result = call(&handler, tool, &project.board, &arguments_for(tool)).await;

        assert!(!result.is_error, "{tool}: {}", text_of(&result));
        assert_ne!(
            std::fs::read(&project.project_file).unwrap(),
            before,
            "{tool} did not write"
        );
    }
}
