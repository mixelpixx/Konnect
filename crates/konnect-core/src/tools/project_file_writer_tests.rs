//! The three `.kicad_pro` writers while KiCad holds the board (#791).
//!
//! Saving the board in KiCad rewrites the project file from KiCad's own copy.
//! So `set_design_rules`, `create_netclass` and `assign_net_to_class` must not
//! write while KiCad holds the board: the write would be reported and then
//! reverted. Measured on KiCad 10.0.5 before the fix: all three reported
//! success, and `run_drc` with `sync_live_board` then put every value back.
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
pub(super) struct Project {
    _dir: tempfile::TempDir,
    pub(super) board: PathBuf,
    pub(super) project_file: PathBuf,
}

pub(super) fn project() -> Project {
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

pub(super) async fn handler_talking_to(address: &str) -> McpHandler {
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

pub(super) async fn call(
    handler: &McpHandler,
    tool: &str,
    board: &Path,
    arguments: &Value,
) -> CallToolResult {
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

pub(super) fn text_of(result: &CallToolResult) -> &str {
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
