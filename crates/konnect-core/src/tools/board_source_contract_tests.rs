//! `board_source` contract for `get_layer_list` and `get_netclasses` (#542).
//!
//! Driven through served `tools/call` rather than the handlers, so the
//! advertised schema, the dispatch gate and the response body are all under
//! test, not just the function bodies.
//!
//! Every case runs against `board_source_divergence_kicad10.kicad_pcb` and a
//! KiCad double whose answers share no layer name, copper count or net name
//! with it. A test that reads the wrong adapter therefore fails on a value,
//! not on a coincidence — see the fixture's README for both sides of the
//! divergence and the `kicad-cli` oracles for the saved half.

use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::CallToolResult;
use crate::test_support::MockIpcServer;
use crate::tools::pcb_board::board_mock::{board_document, spawn_kicad_holding_board};
use crate::tools::ServerConfig;
use konnect_ipc::gen::kiapi;
use konnect_ipc::gen::kiapi::board::types::BoardLayer;
use prost::Message;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ─── What the two sides say ──────────────────────────────────────────────────
//
// Restated here as literals rather than imported from the implementation or
// the fixture: these are the promise the tests hold the code to.

/// The saved stackup, from `kicad-cli pcb export gerbers` on the fixture (one
/// plot per enabled layer, named by the name KiCad shows).
const SAVED_COPPER_LAYER_COUNT: u64 = 4;
const SAVED_ONLY_LAYERS: [&str; 2] = ["In1.Cu", "In2.Cu"];
/// `F.Fab`'s user rename, which the gerber export writes as its filename.
const SAVED_FAB_DISPLAY_NAME: &str = "SavedOnlyFabName";
/// The saved nets, from `kicad-cli pcb export ipcd356` on the fixture.
const SAVED_NETS: [&str; 2] = ["SAVED_ONLY_A", "SAVED_ONLY_B"];

/// What the KiCad double answers instead. Nothing here appears in the file.
const LIVE_COPPER_LAYER_COUNT: u64 = 2;
const LIVE_LAYERS: [&str; 3] = ["F.Cu", "B.Cu", "Edge.Cuts"];
const LIVE_TOP_DISPLAY_NAME: &str = "LiveRenamedTop";
const LIVE_NETS: [&str; 2] = ["LIVE_ONLY_P", "LIVE_ONLY_Q"];

/// The layers the double enables, as KiCad's own `BoardLayer` enum rather
/// than hand-copied numbers — `board_types.proto` is vendored from KiCad and
/// resynced on upgrades, and a constant that silently stopped meaning `F.Cu`
/// would make the double answer about a different layer.
fn live_layer_ids() -> [i32; 3] {
    [
        BoardLayer::BlFCu as i32,
        BoardLayer::BlBCu as i32,
        BoardLayer::BlEdgeCuts as i32,
    ]
}

// ─── Fixture and server plumbing ─────────────────────────────────────────────

fn fixture_board(dir: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/board_source_divergence_kicad10.kicad_pcb");
    let board = dir.join("board_source_divergence_kicad10.kicad_pcb");
    std::fs::copy(&source, &board).expect("the fixture is committed beside the tests");
    board
}

/// The project file `get_netclasses` reads its definitions and patterns from.
///
/// One class with two exact-name patterns, one naming a live-only net and one
/// naming a saved-only net, so `matched_nets` alone says which net list the
/// answer was derived from.
fn write_project(board: &Path) {
    let pro = board.with_extension("kicad_pro");
    let settings = json!({
        "board": { "design_settings": { "rules": {} } },
        "net_settings": {
            "classes": [
                {
                    "name": "Default",
                    "priority": 2147483647,
                    "clearance": 0.2,
                    "track_width": 0.2,
                    "via_diameter": 0.6,
                    "via_drill": 0.3,
                    "microvia_diameter": 0.3,
                    "microvia_drill": 0.1,
                    "diff_pair_width": 0.2,
                    "diff_pair_gap": 0.25,
                    "diff_pair_via_gap": 0.25,
                    "wire_width": 6,
                    "bus_width": 12,
                    "line_style": 0,
                    "pcb_color": "rgba(0, 0, 0, 0.000)",
                    "schematic_color": "rgba(0, 0, 0, 0.000)"
                },
                { "name": "Rails", "priority": 1, "clearance": 0.5 }
            ],
            "meta": { "version": 4 },
            "netclass_patterns": [
                { "netclass": "Rails", "pattern": LIVE_NETS[0] },
                { "netclass": "Rails", "pattern": SAVED_NETS[0] }
            ]
        }
    });
    std::fs::write(&pro, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
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

/// A served Konnect, the fixture board, and whatever KiCad the case needs.
///
/// The temp dir is held for the scene's lifetime, so the board outlives every
/// call made against it.
struct Scene {
    _dir: tempfile::TempDir,
    board: PathBuf,
    kicad: Option<MockIpcServer>,
    handler: McpHandler,
}

impl Scene {
    /// KiCad holds the fixture and answers with the live half.
    async fn live() -> Self {
        Self::build(|board| Some(spawn_live_kicad(board))).await
    }

    /// KiCad holds the fixture and then declines to answer about it.
    async fn rejecting() -> Self {
        Self::build(|board| Some(spawn_kicad_rejecting_after_identification(board))).await
    }

    /// KiCad holds some other project, which is not this board's live source.
    async fn holding_another_board() -> Self {
        Self::build(|board| {
            let elsewhere = board.with_file_name("somebody_elses.kicad_pcb");
            std::fs::write(&elsewhere, "").unwrap();
            Some(spawn_live_kicad(&elsewhere))
        })
        .await
    }

    /// No KiCad is reachable at all.
    async fn offline() -> Self {
        Self::build(|_| None).await
    }

    /// KiCad is up with no PCB editor open.
    async fn without_a_board_editor() -> Self {
        Self::build(|_| Some(spawn_kicad_without_a_board_editor())).await
    }

    async fn not_implementing_open_documents() -> Self {
        Self::build(|_| Some(spawn_kicad_not_implementing_open_documents())).await
    }

    /// KiCad is reachable and refuses the open-document list with `status`,
    /// so no board is named and nothing says whether one is open.
    async fn refusing_before_identification(status: kiapi::common::ApiStatusCode) -> Self {
        Self::build(move |_| Some(spawn_kicad_refusing_everything_with(status))).await
    }

    async fn build(kicad: impl FnOnce(&Path) -> Option<MockIpcServer>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_board(dir.path());
        write_project(&board);
        let kicad = kicad(&board);
        let handler =
            handler_talking_to(kicad.as_ref().map_or("", |server| server.address())).await;
        Self {
            _dir: dir,
            board,
            kicad,
            handler,
        }
    }

    /// The endpoint this scene's server is configured to talk to. Fixed when
    /// the server was built, so a replacement KiCad has to claim this name.
    fn endpoint(&self) -> String {
        self.kicad
            .as_ref()
            .expect("a KiCad to take the endpoint from")
            .address()
            .to_string()
    }

    /// The editor goes away mid-session, after this process watched it hold
    /// the board. Dropping the guard releases its endpoint.
    fn lose_kicad(&mut self) {
        self.kicad = None;
    }

    /// Call one tool against this scene's board the way a client does: a
    /// `tools/call` JSON-RPC request.
    async fn call(&self, tool: &str, board_source: Option<&str>) -> CallToolResult {
        let mut arguments = json!({ "board": self.board.to_string_lossy() });
        if let Some(mode) = board_source {
            arguments["board_source"] = json!(mode);
        }
        self.call_with(tool, arguments).await
    }

    async fn call_with(&self, tool: &str, arguments: Value) -> CallToolResult {
        let response = self
            .handler
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

    /// The parsed body of a call that must succeed.
    async fn body(&self, tool: &str, board_source: Option<&str>) -> Value {
        let result = self.call(tool, board_source).await;
        assert!(!result.is_error, "{:?}", body_of(&result));
        body_of(&result)
    }
}

fn body_of(result: &CallToolResult) -> Value {
    let text = match result.content.first() {
        Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn kind_of(result: &CallToolResult) -> Option<String> {
    crate::mcp::error::extract_error_kind(result)
}

/// A KiCad holding `open` open and answering the layer and net queries with
/// the live half of the divergence.
fn spawn_live_kicad(open: &Path) -> MockIpcServer {
    spawn_kicad_holding_board(open, live_answer)
}

/// The same KiCad, except it declines the commands `skip` names — an `AS_OK`
/// with no body, which is how KiCad answers a command it will not serve.
fn spawn_live_kicad_skipping(open: &Path, skip: &'static str) -> MockIpcServer {
    spawn_kicad_holding_board(open, move |command| {
        if command.type_url.ends_with(skip) {
            return None;
        }
        live_answer(command)
    })
}

/// KiCad running with no PCB editor: the project manager answers, and refuses
/// `GetOpenDocuments` itself with `AS_UNHANDLED`. This is the state a user is
/// in between launching KiCad and opening a board.
fn spawn_kicad_without_a_board_editor() -> MockIpcServer {
    MockIpcServer::spawn("no-board-editor", no_board_editor_response)
}

/// A KiCad whose build does not implement the open-document command at all.
fn spawn_kicad_not_implementing_open_documents() -> MockIpcServer {
    MockIpcServer::spawn("open-documents-unimplemented", |request| {
        assert!(request.message.is_some(), "a command");
        kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsUnimplemented as i32,
                error_message: "kiapi.common.commands.GetOpenDocuments is not implemented"
                    .to_string(),
            }),
            header: None,
            message: None,
        }
    })
}

/// A KiCad that answers every command, `GetOpenDocuments` included, with
/// `status`: reachable, and refusing before it has named any board.
fn spawn_kicad_refusing_everything_with(status: kiapi::common::ApiStatusCode) -> MockIpcServer {
    MockIpcServer::spawn("refusing-before-identification", move |request| {
        assert!(request.message.is_some(), "a command");
        kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: status as i32,
                error_message: format!("the double answers {}", status.as_str_name()),
            }),
            header: None,
            message: None,
        }
    })
}

/// The same KiCad, taking over an endpoint a previous one has released.
fn spawn_kicad_without_a_board_editor_at(address: String) -> MockIpcServer {
    MockIpcServer::spawn_at(address, no_board_editor_response)
}

/// How KiCad answers a board command with no PCB editor behind the endpoint:
/// a completed round trip carrying `AS_UNHANDLED`, not a transport failure.
fn no_board_editor_response(request: kiapi::common::ApiRequest) -> kiapi::common::ApiResponse {
    assert!(request.message.is_some(), "a command");
    kiapi::common::ApiResponse {
        status: Some(kiapi::common::ApiResponseStatus {
            status: kiapi::common::ApiStatusCode::AsUnhandled as i32,
            error_message: "no handler available for request of type \
                            kiapi.common.commands.GetOpenDocuments"
                .to_string(),
        }),
        header: None,
        message: None,
    }
}

/// The same KiCad, refusing everything except the open-document list.
///
/// `spawn_kicad_holding_board` always answers `AS_OK`, so the one case that
/// needs a status is its own server: a board KiCad has positively identified
/// and then declines to answer about.
fn spawn_kicad_rejecting_after_identification(open: &Path) -> MockIpcServer {
    let documents = vec![board_document(&open.to_string_lossy())];
    MockIpcServer::spawn("board-source-busy", move |request| {
        let command = request.message.expect("a command");
        if command.type_url.ends_with("GetOpenDocuments") {
            return kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsOk as i32,
                    error_message: String::new(),
                }),
                header: None,
                message: Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetOpenDocumentsResponse {
                        documents: documents.clone(),
                    },
                    "kiapi.common.commands.GetOpenDocumentsResponse",
                )),
            };
        }
        kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsBusy as i32,
                error_message: "the board is busy".to_string(),
            }),
            header: None,
            message: None,
        }
    })
}

fn live_answer(command: &prost_types::Any) -> Option<prost_types::Any> {
    if command.type_url.ends_with("GetBoardEnabledLayers") {
        return Some(konnect_ipc::builders::pack_any(
            &kiapi::board::commands::BoardEnabledLayersResponse {
                copper_layer_count: LIVE_COPPER_LAYER_COUNT as u32,
                layers: live_layer_ids().to_vec(),
            },
            "kiapi.board.commands.BoardEnabledLayersResponse",
        ));
    }
    if command.type_url.ends_with("GetBoardLayerName") {
        let asked = kiapi::board::commands::GetBoardLayerName::decode(&command.value[..])
            .expect("a layer-name request");
        // Only the top copper layer is renamed, so a response that ignores the
        // requested layer cannot produce the expected answer.
        let [front, back, edge] = live_layer_ids();
        let name = match asked.layer {
            layer if layer == front => LIVE_TOP_DISPLAY_NAME,
            layer if layer == back => "B.Cu",
            layer if layer == edge => "Edge.Cuts",
            other => panic!("the double was asked for an unexpected layer {other}"),
        };
        return Some(konnect_ipc::builders::pack_any(
            &kiapi::board::commands::BoardLayerNameResponse {
                name: name.to_string(),
            },
            "kiapi.board.commands.BoardLayerNameResponse",
        ));
    }
    if command.type_url.ends_with("GetNets") {
        return Some(konnect_ipc::builders::pack_any(
            &kiapi::board::commands::NetsResponse {
                nets: LIVE_NETS
                    .iter()
                    .enumerate()
                    .map(|(index, name)| kiapi::board::types::Net {
                        code: Some(kiapi::board::types::NetCode {
                            value: index as i32 + 1,
                        }),
                        name: (*name).to_string(),
                    })
                    // KiCad also reports the unconnected pseudo-net, whose name
                    // is empty. It is not a net a user has.
                    .chain(std::iter::once(kiapi::board::types::Net {
                        code: Some(kiapi::board::types::NetCode { value: 0 }),
                        name: String::new(),
                    }))
                    .collect(),
            },
            "kiapi.board.commands.NetsResponse",
        ));
    }
    None
}

fn layer_names(body: &Value) -> Vec<String> {
    body["layers"]
        .as_array()
        .expect("a layers array")
        .iter()
        .map(|layer| layer["name"].as_str().expect("a name").to_string())
        .collect()
}

/// The one netclass the fixture project defines beside `Default`.
fn rails_class(body: &Value) -> &Value {
    body["netclasses"]
        .as_array()
        .expect("netclasses")
        .iter()
        .find(|class| class["name"] == json!("Rails"))
        .unwrap_or_else(|| panic!("no Rails class in {body}"))
}

fn display_name_of(body: &Value, layer: &str) -> String {
    body["layers"]
        .as_array()
        .expect("a layers array")
        .iter()
        .find(|entry| entry["name"] == json!(layer))
        .unwrap_or_else(|| panic!("no layer '{layer}' in {body}"))["display_name"]
        .as_str()
        .expect("a display name")
        .to_string()
}

// ─── get_layer_list ──────────────────────────────────────────────────────────

/// The defect #542 names: a board open with an unsaved stackup change was
/// answered from its file. Under `auto` the live board wins outright.
#[tokio::test]
async fn auto_answers_the_live_stackup_over_a_divergent_saved_one() {
    let scene = Scene::live().await;

    let body = scene.body("get_layer_list", None).await;

    assert_eq!(layer_names(&body), LIVE_LAYERS, "{body}");
    assert_eq!(
        body["copper_layer_count"],
        json!(LIVE_COPPER_LAYER_COUNT),
        "the saved file declares {SAVED_COPPER_LAYER_COUNT}: {body}"
    );
    assert_eq!(display_name_of(&body, "F.Cu"), LIVE_TOP_DISPLAY_NAME);
    let rendered = body.to_string();
    for saved_only in SAVED_ONLY_LAYERS {
        assert!(
            !rendered.contains(saved_only),
            "a live answer must not carry the saved-only layer {saved_only}: {body}"
        );
    }
    assert!(!rendered.contains(SAVED_FAB_DISPLAY_NAME), "{body}");

    assert_eq!(body["sources"]["enabled_layers"], json!("ipc"), "{body}");
    assert_eq!(body["sources"]["layer_names"], json!("ipc"), "{body}");
    assert_eq!(
        body["sources"]["copper_layer_count"],
        json!("ipc"),
        "{body}"
    );
    assert_eq!(
        body["source_evidence"]["board_state"],
        json!("ipc"),
        "{body}"
    );
    assert_eq!(
        body["source_evidence"]["excludes_unsaved_editor_state"],
        json!(false),
        "{body}"
    );
}

/// The half of a live answer KiCad's IPC API cannot supply is reported as
/// file-backed instead of being passed off as live or dropped.
#[tokio::test]
async fn a_live_answer_reports_its_file_backed_fields_as_file_backed() {
    let scene = Scene::live().await;

    let body = scene.body("get_layer_list", None).await;

    assert_eq!(
        body["sources"]["layer_types"],
        json!("saved_board"),
        "{body}"
    );
    assert_eq!(body["sources"]["layer_ids"], json!("saved_board"), "{body}");
    // Joined on the canonical name from the fixture's own `(layers …)` table.
    let layers = body["layers"].as_array().unwrap();
    let edge = layers
        .iter()
        .find(|l| l["name"] == json!("Edge.Cuts"))
        .unwrap();
    assert_eq!(edge["type"], json!("user"), "{body}");
    assert_eq!(edge["id"], json!(25), "{body}");
    let front = layers.iter().find(|l| l["name"] == json!("F.Cu")).unwrap();
    assert_eq!(front["type"], json!("signal"), "{body}");
    assert_eq!(front["id"], json!(0), "{body}");
}

/// `saved` is an instruction, not a fallback: KiCad is right there holding the
/// board and is still not consulted, and the answer says what it excludes.
#[tokio::test]
async fn saved_inspects_the_file_even_while_kicad_holds_the_board() {
    let scene = Scene::live().await;

    let body = scene.body("get_layer_list", Some("saved")).await;

    assert_eq!(
        body["copper_layer_count"],
        json!(SAVED_COPPER_LAYER_COUNT),
        "{body}"
    );
    let names = layer_names(&body);
    for saved_only in SAVED_ONLY_LAYERS {
        assert!(names.iter().any(|name| name == saved_only), "{body}");
    }
    assert_eq!(display_name_of(&body, "F.Fab"), SAVED_FAB_DISPLAY_NAME);
    assert!(
        !body.to_string().contains(LIVE_TOP_DISPLAY_NAME),
        "the live editor must not have been consulted: {body}"
    );

    assert_eq!(
        body["sources"]["enabled_layers"],
        json!("saved_board"),
        "{body}"
    );
    assert_eq!(
        body["source_evidence"]["excludes_unsaved_editor_state"],
        json!(true),
        "{body}"
    );
    assert_eq!(
        body["source_evidence"]["reason"],
        json!("explicitly_requested"),
        "{body}"
    );
}

/// A reachable KiCad holding some other board is not this board's live source.
/// `live` says so, naming the boards KiCad does hold; `auto` reads the file and
/// explains why it was allowed to.
#[tokio::test]
async fn a_kicad_holding_another_board_refuses_live_and_explains_auto() {
    let scene = Scene::holding_another_board().await;

    let refused = scene.call("get_layer_list", Some("live")).await;
    assert!(refused.is_error);
    assert_eq!(kind_of(&refused).as_deref(), Some("wrong_document"));
    let refusal = body_of(&refused).to_string();
    for saved_only in SAVED_ONLY_LAYERS {
        assert!(
            !refusal.contains(saved_only),
            "a refusal must not leak the saved stackup: {refusal}"
        );
    }

    let body = scene.body("get_layer_list", None).await;
    assert_eq!(
        body["copper_layer_count"],
        json!(SAVED_COPPER_LAYER_COUNT),
        "{body}"
    );
    assert_eq!(
        body["source_evidence"]["reason"],
        json!("board_not_open_in_kicad"),
        "{body}"
    );
    assert_eq!(
        body["source_evidence"]["excludes_unsaved_editor_state"],
        json!(true),
        "{body}"
    );
}

/// `live` with no KiCad at all is a refusal, not a quiet file read.
#[tokio::test]
async fn live_refuses_when_no_kicad_is_reachable() {
    let scene = Scene::offline().await;

    let result = scene.call("get_layer_list", Some("live")).await;

    assert!(result.is_error);
    assert_eq!(kind_of(&result).as_deref(), Some("editor_unavailable"));
    let refusal = body_of(&result);
    assert!(
        !refusal.to_string().contains(SAVED_ONLY_LAYERS[0]),
        "{refusal}"
    );
}

/// The non-negotiable rule: once KiCad has identified the board as live, a
/// failed query is a failure. It never becomes a saved-file success.
#[tokio::test]
async fn a_failure_after_identification_is_not_answered_from_the_file() {
    let scene = Scene::rejecting().await;

    for mode in [Some("auto"), Some("live")] {
        let result = scene.call("get_layer_list", mode).await;
        assert!(result.is_error, "{mode:?}: {:?}", body_of(&result));
        assert_eq!(
            kind_of(&result).as_deref(),
            Some("editor_unavailable"),
            "{mode:?}"
        );
        let refusal = body_of(&result).to_string();
        assert!(
            !refusal.contains(SAVED_ONLY_LAYERS[0]) && !refusal.contains(SAVED_FAB_DISPLAY_NAME),
            "{mode:?}: the saved stackup must not be reported as this board's state: {refusal}"
        );
    }
}

/// A board this server watched KiCad hold, whose editor then vanished: the
/// saved file may be older than state that was lost, so `auto` refuses. An
/// explicit `saved` request may still inspect it, and must say so.
#[tokio::test]
async fn a_board_observed_live_then_lost_refuses_auto_and_discloses_under_saved() {
    let mut scene = Scene::live().await;
    scene.body("get_layer_list", None).await;

    scene.lose_kicad();

    let refused = scene.call("get_layer_list", None).await;
    assert!(refused.is_error, "{:?}", body_of(&refused));
    assert_eq!(kind_of(&refused).as_deref(), Some("unsafe_file_fallback"));

    let inspected = scene.body("get_layer_list", Some("saved")).await;
    assert_eq!(
        inspected["copper_layer_count"],
        json!(SAVED_COPPER_LAYER_COUNT),
        "{inspected}"
    );
    assert_eq!(
        inspected["source_evidence"]["reason"],
        json!("explicitly_requested_after_live_observation"),
        "{inspected}"
    );
    assert_eq!(
        inspected["source_evidence"]["excludes_unsaved_editor_state"],
        json!(true),
        "{inspected}"
    );
}

/// A layer whose name KiCad declines to give is one cosmetic lookup, not a
/// reason to fail a whole live read — it falls back to the canonical name.
/// The enabled set KiCad did give is still the answer.
#[tokio::test]
async fn a_declined_layer_name_does_not_fail_the_live_read() {
    let dir = tempfile::tempdir().unwrap();
    let board = fixture_board(dir.path());
    let server = spawn_live_kicad_skipping(&board, "GetBoardLayerName");
    let handler = handler_talking_to(server.address()).await;
    let scene = Scene {
        _dir: dir,
        board,
        kicad: Some(server),
        handler,
    };

    let body = scene.body("get_layer_list", None).await;

    assert_eq!(layer_names(&body), LIVE_LAYERS, "{body}");
    assert_eq!(
        body["copper_layer_count"],
        json!(LIVE_COPPER_LAYER_COUNT),
        "{body}"
    );
    // The rename is what was lost, so the canonical spelling stands in.
    assert_eq!(display_name_of(&body, "F.Cu"), "F.Cu");
    assert_eq!(
        body["source_evidence"]["board_state"],
        json!("ipc"),
        "{body}"
    );
}

/// No board has zero enabled layers — Edge.Cuts and the courtyards cannot be
/// disabled. An `AS_OK` carrying no layer set is KiCad declining to answer,
/// and reporting it as a live stackup of nothing would be a confident lie
/// about a board whose real stackup is sitting in the file.
#[tokio::test]
async fn an_empty_enabled_layer_reply_is_not_reported_as_a_live_stackup() {
    let dir = tempfile::tempdir().unwrap();
    let board = fixture_board(dir.path());
    let server = spawn_live_kicad_skipping(&board, "GetBoardEnabledLayers");
    let handler = handler_talking_to(server.address()).await;
    let scene = Scene {
        _dir: dir,
        board,
        kicad: Some(server),
        handler,
    };

    let result = scene.call("get_layer_list", None).await;

    assert!(result.is_error, "{:?}", body_of(&result));
    assert_eq!(kind_of(&result).as_deref(), Some("editor_unavailable"));
    let refusal = body_of(&result);
    assert!(
        !refusal.to_string().contains("\"count\": 0"),
        "an empty stackup must not be handed back as the answer: {refusal}"
    );
}

/// `AS_UNHANDLED` and `AS_UNIMPLEMENTED` both leave Konnect with no live
/// board, and both fall back to the saved file — but only the first is
/// evidence about KiCad's editors. A build that does not implement the
/// open-document command may well have a board open with unsaved changes, so
/// the answer must not carry the "no PCB editor at this endpoint" claim.
#[tokio::test]
async fn an_unimplemented_open_document_command_claims_nothing_about_editors() {
    let scene = Scene::not_implementing_open_documents().await;

    for tool in ["get_layer_list", "get_netclasses"] {
        let body = scene.body(tool, None).await;
        assert_eq!(
            body["source_evidence"]["reason"],
            json!("open_documents_unimplemented_at_endpoint"),
            "{tool}: {body}"
        );
        let detail = body["source_evidence"]["detail"]
            .as_str()
            .unwrap_or_default();
        assert!(
            detail.contains("does not implement"),
            "{tool}: the detail has to say what KiCad actually answered: {detail}"
        );
        assert!(
            !detail.contains("no PCB editor"),
            "{tool}: an unimplemented command is not evidence that no editor is open: {detail}"
        );
        // The file answer still excludes whatever an editor might hold; that
        // is a property of reading the file, not a claim about KiCad.
        assert_eq!(
            body["source_evidence"]["excludes_unsaved_editor_state"],
            json!(true),
            "{tool}: {body}"
        );
    }

    // `live` still refuses: nothing was identified, whichever way KiCad said so.
    let refused = scene.call("get_layer_list", Some("live")).await;
    assert!(refused.is_error, "{:?}", body_of(&refused));
    assert_eq!(kind_of(&refused).as_deref(), Some("editor_unavailable"));
}

/// The other side of the classifier. Only `AS_UNHANDLED` and
/// `AS_UNIMPLEMENTED` on the open-document list mean that no board was served.
/// Any other refusal of that list comes from a KiCad that may hold this board:
/// busy with an operation, or still starting with the board open. The default
/// therefore refuses rather than answering from a file that may be older than
/// the editor, exactly as a refusal after identification does. `saved` still
/// answers, because it asked for the file.
///
/// Every other failure status in KiCad's `envelope.proto` is covered, so a
/// classifier widened to "any status refusing the list means no editor"
/// fails here rather than reading a busy editor's board from disk.
#[tokio::test]
async fn any_other_refusal_of_the_open_document_list_is_not_read_as_no_editor() {
    use kiapi::common::ApiStatusCode as Status;

    for status in [
        Status::AsBusy,
        Status::AsNotReady,
        Status::AsTimeout,
        Status::AsBadRequest,
        Status::AsTokenMismatch,
    ] {
        let scene = Scene::refusing_before_identification(status).await;

        for tool in ["get_layer_list", "get_netclasses"] {
            for mode in [None, Some("auto"), Some("live")] {
                let refused = scene.call(tool, mode).await;
                assert!(
                    refused.is_error,
                    "{status:?} {tool} {mode:?} was answered: {}",
                    body_of(&refused)
                );
                assert_eq!(
                    kind_of(&refused).as_deref(),
                    Some("editor_unavailable"),
                    "{status:?} {tool} {mode:?}: {}",
                    body_of(&refused)
                );
            }

            let saved = scene.body(tool, Some("saved")).await;
            assert_eq!(
                saved["source_evidence"]["reason"],
                json!("explicitly_requested"),
                "{status:?} {tool}: {saved}"
            );
        }
    }
}

/// The regression the maintainer caught on a real KiCad: with only the project
/// manager running, `GetOpenDocuments` is refused outright, so no board was
/// ever identified — and #574's rule is scoped to *after* identification.
/// Refusing here would take the default answer away from every user who has
/// launched KiCad and not yet opened a board.
#[tokio::test]
async fn a_kicad_with_no_board_editor_still_answers_by_default() {
    let scene = Scene::without_a_board_editor().await;

    for tool in ["get_layer_list", "get_netclasses"] {
        for mode in [None, Some("auto")] {
            let body = scene.body(tool, mode).await;
            assert_eq!(
                body["source_evidence"]["reason"],
                json!("no_pcb_editor_at_endpoint"),
                "{tool} / {mode:?}: {body}"
            );
            assert_eq!(
                body["source_evidence"]["excludes_unsaved_editor_state"],
                json!(true),
                "{tool} / {mode:?}: {body}"
            );
        }
    }

    let body = scene.body("get_layer_list", None).await;
    assert_eq!(
        body["copper_layer_count"],
        json!(SAVED_COPPER_LAYER_COUNT),
        "{body}"
    );
}

/// `live` is still a request for a live board, and there is not one.
#[tokio::test]
async fn a_kicad_with_no_board_editor_refuses_a_live_request() {
    let scene = Scene::without_a_board_editor().await;

    let result = scene.call("get_layer_list", Some("live")).await;

    assert!(result.is_error, "{:?}", body_of(&result));
    assert_eq!(kind_of(&result).as_deref(), Some("editor_unavailable"));
}

/// The exception: an editor this session watched hold the board is gone, and
/// the saved file may be older than what it held.
#[tokio::test]
async fn a_board_seen_live_before_the_editor_closed_still_refuses() {
    let mut scene = Scene::live().await;
    scene.body("get_layer_list", None).await;

    // The PCB editor closes and the project manager takes over the same
    // endpoint. The server is the one that saw the board live, so its session
    // memory is the thing under test — a fresh handler would not have it.
    let endpoint = scene.endpoint();
    scene.lose_kicad();
    scene.kicad = Some(spawn_kicad_without_a_board_editor_at(endpoint));

    let refused = scene.call("get_layer_list", None).await;

    assert!(refused.is_error, "{:?}", body_of(&refused));
    assert_eq!(kind_of(&refused).as_deref(), Some("unsafe_file_fallback"));
}

// ─── get_netclasses ──────────────────────────────────────────────────────────

/// The mixed-source answer: definitions and patterns are project-file facts,
/// the nets are live, and the matches are derived from them. Each is named
/// separately rather than compressed into one source string.
#[tokio::test]
async fn netclasses_report_live_nets_beside_project_file_definitions() {
    let scene = Scene::live().await;

    let body = scene.body("get_netclasses", None).await;

    assert_eq!(
        body["sources"],
        json!({
            "definitions": "project_file",
            "patterns": "project_file",
            "board_nets": "ipc",
            "matched_nets": "derived",
        }),
        "{body}"
    );
    assert_eq!(
        body["source_evidence"]["board_state"],
        json!("ipc"),
        "{body}"
    );
    // The empty-named unconnected pseudo-net KiCad also reports is not a net.
    assert_eq!(body["nets_on_board"], json!(LIVE_NETS.len()), "{body}");

    let rails = rails_class(&body);
    // Both patterns are present; only the live-named net can match.
    assert_eq!(
        rails["patterns"],
        json!([LIVE_NETS[0], SAVED_NETS[0]]),
        "{body}"
    );
    assert_eq!(rails["matched_nets"], json!([LIVE_NETS[0]]), "{body}");
    assert_eq!(rails["clearance"], json!(0.5), "{body}");
}

/// The same call against the saved snapshot matches the other pattern, so the
/// derived field alone says which net list produced it.
#[tokio::test]
async fn netclasses_saved_mode_matches_the_saved_nets_and_discloses_it() {
    let scene = Scene::live().await;

    let body = scene.body("get_netclasses", Some("saved")).await;

    assert_eq!(
        body["sources"]["board_nets"],
        json!("saved_board"),
        "{body}"
    );
    assert_eq!(
        body["sources"]["definitions"],
        json!("project_file"),
        "{body}"
    );
    assert_eq!(body["nets_on_board"], json!(SAVED_NETS.len()), "{body}");
    assert_eq!(
        rails_class(&body)["matched_nets"],
        json!([SAVED_NETS[0]]),
        "{body}"
    );
    assert_eq!(
        body["source_evidence"]["excludes_unsaved_editor_state"],
        json!(true),
        "{body}"
    );
}

/// `get_netclasses` obeys the same live-identification rule as its sibling:
/// the project file is still readable, and that is not licence to report a
/// stale net list as the board's.
#[tokio::test]
async fn netclasses_refuse_after_a_failed_live_query() {
    let scene = Scene::rejecting().await;

    let result = scene.call("get_netclasses", None).await;

    assert!(result.is_error, "{:?}", body_of(&result));
    assert_eq!(kind_of(&result).as_deref(), Some("editor_unavailable"));
    let refusal = body_of(&result);
    assert!(!refusal.to_string().contains(SAVED_NETS[0]), "{refusal}");
}

// ─── The selector itself ─────────────────────────────────────────────────────

/// The advertised schema refuses a value neither tool can honour, before any
/// board is touched.
#[tokio::test]
async fn an_unknown_board_source_is_refused_by_name() {
    let scene = Scene::offline().await;

    for tool in ["get_layer_list", "get_netclasses"] {
        for value in [json!("ipc"), json!("file"), json!(true)] {
            let result = scene
                .call_with(
                    tool,
                    json!({ "board": scene.board.to_string_lossy(), "board_source": value }),
                )
                .await;
            assert!(result.is_error, "{tool} accepted {value}");
            assert_eq!(
                kind_of(&result).as_deref(),
                Some("invalid_argument"),
                "{tool} / {value}"
            );
            assert_eq!(
                body_of(&result)["error"]["field"],
                json!("board_source"),
                "{tool} / {value}"
            );
        }
    }
}

/// Omitting the selector keeps every existing caller working, and means
/// `auto`: with no KiCad reachable that is the saved file, disclosed.
#[tokio::test]
async fn omitting_the_selector_reads_the_saved_board_and_says_so() {
    let scene = Scene::offline().await;

    let body = scene.body("get_layer_list", None).await;

    assert_eq!(
        body["copper_layer_count"],
        json!(SAVED_COPPER_LAYER_COUNT),
        "{body}"
    );
    assert_eq!(
        body["source_evidence"]["reason"],
        json!("kicad_ipc_unreachable"),
        "{body}"
    );
    // The request is never echoed back as evidence that a source was used.
    assert!(body.get("board_source").is_none(), "{body}");
}
