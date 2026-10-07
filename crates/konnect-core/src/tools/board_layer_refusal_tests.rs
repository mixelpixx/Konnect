//! Board writers refuse a layer KiCad has no board layer for (#844).
//!
//! `add_board_text`, `import_svg_logo` and `add_zone`/`add_copper_pour` used to
//! send an unknown name to KiCad as `BL_UNDEFINED`, or write it into the file.
//! Each case runs through served `tools/call`, once against a KiCad double
//! holding the board and once with no KiCad, on a board KiCad 10 saved (see
//! `tests/fixtures/six_layer_power_planes_kicad10.README.md` for the oracle).

use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::CallToolResult;
use crate::test_support::MockIpcServer;
use crate::tools::pcb_board::board_mock::spawn_kicad_holding_board;
use crate::tools::ServerConfig;
use konnect_ipc::gen::kiapi;
use konnect_ipc::gen::kiapi::board::types::BoardLayer;
use prost::Message;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Every command the double received after `GetOpenDocuments`, with the
/// layers of each item a `CreateItems` carried.
type Received = Arc<Mutex<Vec<(String, Vec<i32>)>>>;

struct Scene {
    _dir: tempfile::TempDir,
    board: PathBuf,
    svg: PathBuf,
    _kicad: Option<MockIpcServer>,
    received: Received,
    handler: McpHandler,
}

impl Scene {
    async fn new(live: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("six_layer.kicad_pcb");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/six_layer_power_planes_kicad10.kicad_pcb"),
            &board,
        )
        .unwrap();
        let svg = dir.path().join("logo.svg");
        std::fs::write(
            &svg,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><path d="M0 0 L10 0 L10 10 L0 10 Z"/></svg>"#,
        )
        .unwrap();
        let received = Received::default();
        let kicad = live.then(|| kicad_recording(&board, received.clone()));
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            // An endpoint nothing listens on: an empty address would read
            // `KICAD_API_SOCKET` and could reach a real KiCad.
            ipc_address: kicad.as_ref().map_or_else(
                || format!("ipc://{}", dir.path().join("no-kicad-here.sock").display()),
                |k| k.address().to_string(),
            ),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: true,
            eager_toolsets: false,
        })
        .await
        .expect("handler builds");
        Self {
            _dir: dir,
            board,
            svg,
            _kicad: kicad,
            received,
            handler,
        }
    }

    async fn call(&self, tool: &str, layer: Option<&str>) -> CallToolResult {
        let board = self.board.to_string_lossy();
        let mut arguments = match tool {
            "add_board_text" => json!({ "board": board, "text": "REV A", "x": 5.0, "y": 5.0 }),
            "import_svg_logo" => json!({
                "board": board, "svg": self.svg.to_string_lossy(), "width_mm": 4.0
            }),
            _ => json!({
                "board": board, "net_name": "GND",
                "points": [{ "x": 0.0, "y": 0.0 }, { "x": 10.0, "y": 0.0 }, { "x": 10.0, "y": 10.0 }]
            }),
        };
        if let Some(layer) = layer {
            arguments["layer"] = json!(layer);
        }
        let response = self
            .handler
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 844, "method": "tools/call",
                "params": { "name": tool, "arguments": arguments }
            }))
            .await
            .expect("tools/call receives a response");
        assert!(response.error.is_none(), "tool errors are MCP results");
        serde_json::from_value(response.result.expect("a result")).expect("an MCP result")
    }

    /// What the double received since the last call to this.
    fn take_received(&self) -> Vec<(String, Vec<i32>)> {
        std::mem::take(&mut *self.received.lock().unwrap())
    }
}

/// A KiCad holding `board` that records what it is asked, creates whatever
/// `CreateItems` names, and knows one net, `GND`.
fn kicad_recording(board: &Path, received: Received) -> MockIpcServer {
    spawn_kicad_holding_board(board, move |command| {
        let name = command.type_url.rsplit('.').next().unwrap().to_string();
        if name != "CreateItems" {
            let nets = name == "GetNets";
            received.lock().unwrap().push((name, Vec::new()));
            return nets.then(|| {
                konnect_ipc::builders::pack_any(
                    &kiapi::board::commands::NetsResponse {
                        nets: vec![konnect_ipc::builders::net("GND", 1)],
                    },
                    "kiapi.board.commands.NetsResponse",
                )
            });
        }
        let request =
            kiapi::common::commands::CreateItems::decode(command.value.as_slice()).unwrap();
        let layers = request.items.iter().flat_map(item_layers).collect();
        received.lock().unwrap().push((name, layers));
        Some(konnect_ipc::builders::pack_any(
            &kiapi::common::commands::CreateItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                created_items: request
                    .items
                    .into_iter()
                    .map(|item| kiapi::common::commands::ItemCreationResult {
                        status: Some(kiapi::common::commands::ItemStatus {
                            code: kiapi::common::commands::ItemStatusCode::IscOk as i32,
                            error_message: String::new(),
                        }),
                        item: Some(item),
                    })
                    .collect(),
            },
            "kiapi.common.commands.CreateItemsResponse",
        ))
    })
}

fn item_layers(item: &prost_types::Any) -> Vec<i32> {
    let bytes = item.value.as_slice();
    if item.type_url.ends_with("BoardText") {
        vec![kiapi::board::types::BoardText::decode(bytes).unwrap().layer]
    } else if item.type_url.ends_with("BoardGraphicShape") {
        vec![
            kiapi::board::types::BoardGraphicShape::decode(bytes)
                .unwrap()
                .layer,
        ]
    } else if item.type_url.ends_with("Zone") {
        kiapi::board::types::Zone::decode(bytes).unwrap().layers
    } else {
        panic!("unexpected item {}", item.type_url)
    }
}

fn body(result: &CallToolResult) -> Value {
    match result.content.first() {
        Some(crate::mcp::protocol::ToolContent::Text { text }) => serde_json::from_str(text)
            .unwrap_or_else(|_| panic!("expected a JSON body, got {text}")),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Names KiCad 10.0.6 refuses to load a board item on (fixture README): the
/// issue's typo, and a name that is no layer at all.
const UNKNOWN_LAYERS: [&str; 2] = ["F.Silk", "Not.A.Layer"];

/// `tool` refuses each unknown layer as `invalid_argument` naming `layer`, with
/// KiCad live and without, sending KiCad nothing and leaving the board alone.
async fn assert_refuses_unknown_layers(tool: &str) {
    for live in [true, false] {
        let scene = Scene::new(live).await;
        let before = std::fs::read(&scene.board).unwrap();
        for layer in UNKNOWN_LAYERS {
            let result = scene.call(tool, Some(layer)).await;

            let case = format!("{tool} {layer} live={live}");
            assert!(result.is_error, "{case}: {:?}", result.content);
            let error = &body(&result)["error"];
            assert_eq!(error["kind"], "invalid_argument", "{case}");
            assert_eq!(error["field"], "layer", "{case}");
            assert!(
                error["reason"].as_str().unwrap().contains(layer),
                "{case}: {error}"
            );
            assert_eq!(scene.take_received(), Vec::new(), "{case}: KiCad was asked");
            assert_eq!(
                std::fs::read(&scene.board).unwrap(),
                before,
                "{case}: the board changed"
            );
        }
    }
}

#[tokio::test]
async fn add_board_text_refuses_an_unknown_layer() {
    assert_refuses_unknown_layers("add_board_text").await;
}

#[tokio::test]
async fn import_svg_logo_refuses_an_unknown_layer() {
    assert_refuses_unknown_layers("import_svg_logo").await;
}

#[tokio::test]
async fn add_zone_refuses_an_unknown_layer() {
    assert_refuses_unknown_layers("add_zone").await;
}

#[tokio::test]
async fn add_copper_pour_refuses_an_unknown_layer() {
    assert_refuses_unknown_layers("add_copper_pour").await;
}

#[tokio::test]
async fn a_known_layer_still_reaches_kicad_as_that_layer() {
    let cases = [
        ("add_board_text", None, BoardLayer::BlFSilkS),
        ("add_board_text", Some("B.SilkS"), BoardLayer::BlBSilkS),
        ("import_svg_logo", None, BoardLayer::BlFSilkS),
        ("import_svg_logo", Some("F.Cu"), BoardLayer::BlFCu),
        ("add_zone", Some("In1.Cu"), BoardLayer::BlIn1Cu),
        ("add_copper_pour", Some("B.Cu"), BoardLayer::BlBCu),
    ];
    let scene = Scene::new(true).await;
    let before = std::fs::read(&scene.board).unwrap();
    for (tool, layer, expected) in cases {
        let result = scene.call(tool, layer).await;

        let case = format!("{tool} {layer:?}");
        assert!(!result.is_error, "{case}: {:?}", result.content);
        assert_eq!(body(&result)["source"], "ipc", "{case}");
        let created: Vec<_> = scene
            .take_received()
            .into_iter()
            .filter(|(name, _)| name == "CreateItems")
            .collect();
        assert_eq!(created.len(), 1, "{case}");
        assert_eq!(created[0].1, vec![expected as i32], "{case}");
        assert_eq!(std::fs::read(&scene.board).unwrap(), before, "{case}");
    }
}

#[tokio::test]
async fn a_known_layer_is_still_written_to_the_file() {
    let scene = Scene::new(false).await;
    for (tool, layer, written) in [
        ("add_board_text", None, "\"F.SilkS\""),
        ("import_svg_logo", Some("B.SilkS"), "\"B.SilkS\""),
        ("add_zone", Some("In1.Cu"), "\"In1.Cu\""),
    ] {
        let before = std::fs::read_to_string(&scene.board).unwrap();

        let result = scene.call(tool, layer).await;

        assert!(!result.is_error, "{tool}: {:?}", result.content);
        assert_eq!(body(&result)["source"], "file", "{tool}");
        let after = std::fs::read_to_string(&scene.board).unwrap();
        assert_eq!(
            after.matches(written).count(),
            before.matches(written).count() + 1,
            "{tool}"
        );
    }
}
