//! Full MCP-tool regression against a running KiCad PCB Editor.
//!
//! The workflow opens a disposable board under Xvfb and supplies its socket.
//! This test intentionally crosses every layer: JSON-RPC stdio, tool routing,
//! platform footprint-library discovery, `.kicad_mod` preparation, and live IPC.

use konnect_ipc::client::KiCadIpcClient;
use konnect_ipc::gen::kiapi;
use konnect_sexp::{parse_sexp, SexpNode};
use prost::Message;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpProcess {
    fn spawn(socket: &str) -> Self {
        let config = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let kicad_cli = std::env::var("KICAD_CLI").unwrap_or_else(|_| "kicad-cli".to_string());
        std::fs::write(
            config.path(),
            serde_json::to_vec(&json!({"ipc_address": socket, "kicad_cli": kicad_cli})).unwrap(),
        )
        .unwrap();
        let (_, config_path) = config.keep().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_konnect"))
            .arg("--config")
            .arg(config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to start Konnect MCP server");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut process = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        process.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "live-kicad-tools", "version": "0"}
            }),
        );
        process
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        self.stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "Konnect exited before replying"
            );
            let response: Value = serde_json::from_str(line.trim()).unwrap();
            if response["id"] == id {
                return response;
            }
        }
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let result = &response["result"];
        assert_ne!(
            result["isError"], true,
            "tool {name} failed: {}",
            result["content"][0]["text"]
        );
        result.clone()
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn footprint<'a>(tree: &'a SexpNode, reference: &str) -> &'a SexpNode {
    tree.find_all("footprint")
        .into_iter()
        .find(|node| {
            node.find_all("property").into_iter().any(|property| {
                property.get(1).and_then(SexpNode::as_str) == Some("Reference")
                    && property.get(2).and_then(SexpNode::as_str) == Some(reference)
            })
        })
        .unwrap_or_else(|| panic!("placed footprint {reference} is missing from saved board"))
}

fn live_footprint(
    ipc: &KiCadIpcClient,
    board: &std::path::Path,
    reference: &str,
) -> kiapi::board::types::FootprintInstance {
    let document = ipc.find_open_board(board).unwrap();
    ipc.get_items_in(
        document,
        kiapi::common::types::KiCadObjectType::KotPcbFootprint,
    )
    .unwrap()
    .into_iter()
    .filter_map(|item| kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).ok())
    .find(|footprint| {
        footprint
            .reference_field
            .as_ref()
            .and_then(|field| field.text.as_ref())
            .and_then(|text| text.text.as_ref())
            .map(|text| text.text.as_str())
            == Some(reference)
    })
    .unwrap_or_else(|| panic!("live footprint {reference} is missing"))
}

fn footprint_pad_size(
    footprint: &kiapi::board::types::FootprintInstance,
    number: &str,
) -> (i64, i64) {
    footprint
        .definition
        .as_ref()
        .unwrap()
        .items
        .iter()
        .filter(|item| item.type_url.ends_with("kiapi.board.types.Pad"))
        .filter_map(|item| kiapi::board::types::Pad::decode(item.value.as_slice()).ok())
        .find(|pad| pad.number == number)
        .and_then(|pad| pad.pad_stack)
        .and_then(|stack| stack.copper_layers.into_iter().next())
        .and_then(|layer| layer.size)
        .map(|size| (size.x_nm, size.y_nm))
        .unwrap_or_else(|| panic!("pad {number} is missing its copper size"))
}

fn footprint_models(
    footprint: &kiapi::board::types::FootprintInstance,
) -> Vec<kiapi::board::types::Footprint3DModel> {
    footprint
        .definition
        .as_ref()
        .unwrap()
        .items
        .iter()
        .filter(|item| {
            item.type_url
                .ends_with("kiapi.board.types.Footprint3DModel")
        })
        .map(|item| kiapi::board::types::Footprint3DModel::decode(item.value.as_slice()).unwrap())
        .collect()
}

fn footprint_text_stroke_widths(footprint: &kiapi::board::types::FootprintInstance) -> Vec<i64> {
    footprint
        .definition
        .as_ref()
        .unwrap()
        .items
        .iter()
        .filter(|item| item.type_url.ends_with("kiapi.board.types.BoardText"))
        .map(|item| kiapi::board::types::BoardText::decode(item.value.as_slice()).unwrap())
        .filter_map(|text| {
            text.text
                .and_then(|text| text.attributes)
                .and_then(|attributes| attributes.stroke_width)
                .map(|width| width.value_nm)
        })
        .collect()
}

#[test]
#[ignore = "requires a running KiCad GUI, API socket, and standard footprint libraries"]
fn place_component_loads_real_library_geometry() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let lib_id = std::env::var("KONNECT_LIVE_KICAD_FOOTPRINT")
        .unwrap_or_else(|_| "Resistor_SMD:R_0402_1005Metric".into());
    let reference =
        std::env::var("KONNECT_LIVE_KICAD_PLACE_REFERENCE").unwrap_or_else(|_| "R900".into());

    let ipc = KiCadIpcClient::new(&socket);
    let ready = (0..100).any(|_| {
        if ipc.ping().unwrap_or(false) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
        false
    });
    assert!(ready, "KiCad IPC socket never became ready");

    let mut mcp = McpProcess::spawn(&socket);
    mcp.tool("load_toolset", json!({"name": "pcb_components"}));
    let placed = mcp.tool(
        "place_component",
        json!({
            "board": board,
            "footprint": lib_id,
            "reference": reference,
            "x": 42.0,
            "y": 37.0,
            "rotation": 90.0,
            "layer": "F.Cu"
        }),
    );
    let body: Value = serde_json::from_str(placed["content"][0]["text"].as_str().unwrap())
        .expect("place_component did not return JSON");
    assert_eq!(body["placed"], reference);
    assert_eq!(body["footprint"], lib_id);

    let wrong_board = std::path::Path::new(&board)
        .with_file_name("not-the-active-board.kicad_pcb")
        .to_string_lossy()
        .into_owned();
    let rejected = mcp.request(
        "tools/call",
        json!({
            "name": "move_component",
            "arguments": {
                "board": wrong_board,
                "reference": reference,
                "x": 99.0,
                "y": 99.0
            }
        }),
    );
    assert_eq!(
        rejected["result"]["isError"], true,
        "wrong-board mutation was not rejected: {rejected}"
    );

    let edited = mcp.tool(
        "edit_component",
        json!({"board": board, "reference": reference, "value": "10k"}),
    );
    let edited: Value = serde_json::from_str(edited["content"][0]["text"].as_str().unwrap())
        .expect("edit_component did not return JSON");
    assert_eq!(edited["value"], "10k");

    let array = mcp.tool(
        "place_component_array",
        json!({
            "board": board,
            "footprint": lib_id,
            "start_x": 30.0,
            "start_y": 50.0,
            "count_x": 2,
            "count_y": 1,
            "spacing_x": 5.0,
            "ref_prefix": "R",
            "ref_start": 910
        }),
    );
    let array: Value = serde_json::from_str(array["content"][0]["text"].as_str().unwrap())
        .expect("place_component_array did not return JSON");
    assert_eq!(array["placed_count"], 2);

    let aligned = mcp.tool(
        "align_components",
        json!({
            "board": board,
            "references": ["R910", "R911"],
            "axis": "y",
            "value": 55.0
        }),
    );
    let aligned: Value = serde_json::from_str(aligned["content"][0]["text"].as_str().unwrap())
        .expect("align_components did not return JSON");
    assert_eq!(aligned["aligned_count"], 2);

    KiCadIpcClient::new(&socket)
        .save_board()
        .expect("failed to save board after placement");
    let tree = parse_sexp(&std::fs::read_to_string(&board).unwrap()).unwrap();
    let placed = footprint(&tree, &reference);
    assert!(
        placed.find_all("pad").len() >= 2,
        "placed library footprint lost its pads"
    );
    assert!(placed.find_all("property").into_iter().any(|property| {
        property.get(1).and_then(SexpNode::as_str) == Some("Value")
            && property.get(2).and_then(SexpNode::as_str) == Some("10k")
    }));
    let at = placed
        .find("at")
        .expect("placed footprint has no board position");
    assert!((at.get_f64(1).unwrap() - 42.0).abs() < 1e-6);
    assert!((at.get_f64(2).unwrap() - 37.0).abs() < 1e-6);
    assert!((at.get_f64(3).unwrap() - 90.0).abs() < 1e-6);

    for array_reference in ["R910", "R911"] {
        let array_footprint = footprint(&tree, array_reference);
        let at = array_footprint
            .find("at")
            .expect("array footprint has no board position");
        assert!((at.get_f64(2).unwrap() - 55.0).abs() < 1e-6);
    }
}

#[test]
#[ignore = "requires a running KiCad GUI, API socket, saved schematic, and matching open board"]
fn schematic_sync_apply_then_dry_run_is_noop() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let schematic = std::env::var("KONNECT_LIVE_KICAD_SCHEMATIC")
        .expect("KONNECT_LIVE_KICAD_SCHEMATIC must name the saved, closed root schematic");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let mut mcp = McpProcess::spawn(&socket);
    mcp.tool("load_toolset", json!({"name": "sch_export"}));

    let dry_run = mcp.tool(
        "update_pcb_from_schematic",
        json!({"schematic": schematic, "board": board, "dry_run": true}),
    );
    let dry_run: Value =
        serde_json::from_str(dry_run["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        dry_run["status"], "ready",
        "fixture must require a sync: {dry_run}"
    );
    let revision = dry_run["plan_revision"]
        .as_str()
        .expect("dry run returned no plan revision");

    let applied = mcp.tool(
        "update_pcb_from_schematic",
        json!({
            "schematic": schematic,
            "board": board,
            "dry_run": false,
            "expected_plan_revision": revision
        }),
    );
    let applied: Value =
        serde_json::from_str(applied["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(applied["status"], "applied", "{applied}");

    let after = mcp.tool(
        "update_pcb_from_schematic",
        json!({"schematic": schematic, "board": board, "dry_run": true}),
    );
    let after: Value = serde_json::from_str(after["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(after["status"], "noop", "apply did not converge: {after}");
}

#[test]
#[ignore = "requires a running KiCad GUI and a disposable open board"]
fn footprint_library_update_apply_then_dry_run_is_noop() {
    let board = std::path::PathBuf::from(
        std::env::var("KONNECT_LIVE_KICAD_BOARD")
            .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board"),
    );
    let project = board.with_extension("kicad_pro");
    assert!(
        project.is_file(),
        "the disposable board needs a sibling .kicad_pro"
    );
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let project_dir = board.parent().unwrap();
    let library_dir = project_dir.join("konnect-update-fixture.pretty");
    let footprint_path = library_dir.join("RefreshFixture.kicad_mod");
    let reference = "REFRESH900";
    let library_id = "konnect-update-fixture:RefreshFixture";

    let ipc = KiCadIpcClient::new(&socket);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match ipc.find_open_board(&board) {
            Ok(_) => break,
            Err(error)
                if error.to_string().contains("AS_NOT_READY")
                    && std::time::Instant::now() < deadline => {}
            Err(error) => panic!("KiCad did not open the disposable board: {error:#}"),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let mut mcp = McpProcess::spawn(&socket);
    mcp.tool(
        "load_toolset",
        json!({"name": ["library", "pcb_components"]}),
    );
    mcp.tool(
        "create_footprint",
        json!({
            "output": footprint_path,
            "name": "RefreshFixture",
            "description": "revision A",
            "pads": [
                {
                    "number": "1",
                    "type": "smd",
                    "shape": "rect",
                    "x": -1.0,
                    "y": 0.0,
                    "width": 1.0,
                    "height": 1.0
                },
                {
                    "number": "2",
                    "type": "smd",
                    "shape": "rect",
                    "x": 1.0,
                    "y": 0.0,
                    "width": 1.0,
                    "height": 1.0
                }
            ],
            "body_width": 3.0,
            "body_height": 2.0,
            "model": {
                "path": "../models/revision-a.step",
                "offset": {"x": 0.0, "y": 0.0, "z": 0.0},
                "scale": {"x": 1.0, "y": 1.0, "z": 1.0},
                "rotate": {"x": 0.0, "y": 0.0, "z": 0.0}
            }
        }),
    );
    let mut footprint_source = std::fs::read_to_string(&footprint_path).unwrap();
    let root_end = footprint_source
        .rfind("\n)")
        .expect("created footprint must have a closing root");
    footprint_source.insert_str(
        root_end,
        r#"
  (fp_text user "${REFERENCE}" (at 0 1.5 0) (layer "F.Fab")
    (uuid "f96c2efe-5925-4f74-81d2-f89a56f57e13")
    (effects (font (size 0.8 0.8) (thickness 0.11))))"#,
    );
    std::fs::write(&footprint_path, footprint_source).unwrap();
    mcp.tool(
        "register_footprint_library",
        json!({
            "library_path": library_dir,
            "nickname": "konnect-update-fixture",
            "scope": "project",
            "project": project
        }),
    );
    mcp.tool(
        "place_component",
        json!({
            "board": board,
            "footprint": library_id,
            "reference": reference,
            "x": 42.0,
            "y": 37.0,
            "rotation": 37.0,
            "layer": "F.Cu"
        }),
    );

    let before = live_footprint(&ipc, &board, reference);
    let before_id = before.id.clone();
    let before_position = before.position;
    let before_orientation = before.orientation;
    let before_layer = before.layer;
    let before_pad_one = footprint_pad_size(&before, "1");
    let before_models = footprint_models(&before);

    mcp.tool(
        "edit_footprint_pad",
        json!({
            "footprint_path": footprint_path,
            "pad_number": "1",
            "width": 2.0,
            "height": 1.5
        }),
    );
    mcp.tool(
        "set_footprint_graphics",
        json!({
            "footprint_path": footprint_path,
            "selector": {"layer": "F.CrtYd"},
            "mode": "replace",
            "graphics": [{
                "type": "rect",
                "start": {"x": -2.5, "y": -1.5},
                "end": {"x": 2.5, "y": 1.5},
                "stroke_width_mm": 0.05,
                "fill": "none"
            }]
        }),
    );
    mcp.tool(
        "set_footprint_metadata",
        json!({
            "footprint_path": footprint_path,
            "description": "revision B",
            "tags": ["konnect", "refresh"]
        }),
    );
    mcp.tool(
        "set_footprint_models",
        json!({
            "footprint_path": footprint_path,
            "mode": "replace",
            "models": [{
                "path": "../models/revision-b.step",
                "offset": {"x": 1.0, "y": 2.0, "z": 3.0},
                "scale": {"x": 1.0, "y": 1.0, "z": 1.0},
                "rotate": {"x": 90.0, "y": 0.0, "z": 45.0}
            }]
        }),
    );

    let dry_run = mcp.tool(
        "update_footprints_from_library",
        json!({
            "board": board,
            "references": [reference],
            "dry_run": true
        }),
    );
    let dry_run: Value =
        serde_json::from_str(dry_run["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(dry_run["status"], "ready", "{dry_run}");
    assert_eq!(dry_run["coverage"]["selected"]["planned"], 1);
    assert_eq!(dry_run["coverage"]["changed"]["planned"], 1);
    let revision = dry_run["plan_revision"].as_str().unwrap();

    let applied = mcp.tool(
        "update_footprints_from_library",
        json!({
            "board": board,
            "references": [reference],
            "dry_run": false,
            "expected_plan_revision": revision
        }),
    );
    let applied: Value =
        serde_json::from_str(applied["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(applied["status"], "applied", "{applied}");

    let after = live_footprint(&ipc, &board, reference);
    assert_eq!(after.id, before_id);
    assert_eq!(after.position, before_position);
    assert_eq!(after.orientation, before_orientation);
    assert_eq!(after.layer, before_layer);
    assert_ne!(footprint_pad_size(&after, "1"), before_pad_one);
    assert_eq!(footprint_pad_size(&after, "1"), (2_000_000, 1_500_000));
    assert_ne!(footprint_models(&after), before_models);
    assert_eq!(
        footprint_models(&after)[0].filename,
        "../models/revision-b.step"
    );
    assert!(
        footprint_text_stroke_widths(&after).contains(&110_000),
        "explicit fab text thickness was not preserved"
    );

    let converged = mcp.tool(
        "update_footprints_from_library",
        json!({
            "board": board,
            "references": [reference],
            "dry_run": true
        }),
    );
    let converged: Value =
        serde_json::from_str(converged["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(converged["status"], "noop", "{converged}");

    ipc.save_board().unwrap();
    let saved_converged = mcp.tool(
        "update_footprints_from_library",
        json!({
            "board": board,
            "references": [reference],
            "dry_run": true
        }),
    );
    let saved_converged: Value =
        serde_json::from_str(saved_converged["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        saved_converged["status"], "noop",
        "KiCad save changed the rebuilt fab text: {saved_converged}"
    );
    mcp.tool("load_toolset", json!({"name": "verification"}));
    let drc = mcp.tool(
        "run_drc",
        json!({
            "board": board,
            "severity": "info",
            "tests": ["lib_footprint_mismatch"],
            "limit": 100
        }),
    );
    let drc: Value = serde_json::from_str(drc["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        drc["violations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|violation| {
                !violation["description"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("doesn't match the copy in the library")
            }),
        "{drc}"
    );
}

/// What a real KiCad puts in `DocumentSpecifier`, recorded rather than assumed.
///
/// The ambiguity gate in `find_open_board` refuses whenever an open PCB
/// document cannot be placed on disk, and the shapes it refuses were derived
/// from the vendored proto's own contract — `board_filename` is documented as
/// "a PCB with a given filename, e.g. `board.kicad_pcb`", with
/// `ProjectSpecifier.path` supplying the directory. A gate built on that
/// reading is only as good as the reading, so this prints every field of every
/// open document and then asserts the property the gate depends on: a live
/// KiCad's open-document list is one Konnect can resolve in full.
///
/// Run it against a KiCad holding the disposable board:
///
/// ```text
/// KICAD_API_SOCKET=… KONNECT_LIVE_KICAD_BOARD=… \
///   cargo test -p konnect --test live_kicad_tools -- --ignored --nocapture \
///   real_kicad_open_documents_resolve_to_comparable_paths
/// ```
#[test]
#[ignore = "requires a running KiCad GUI with a board open and its API socket"]
fn real_kicad_open_documents_resolve_to_comparable_paths() {
    let board = std::path::PathBuf::from(
        std::env::var("KONNECT_LIVE_KICAD_BOARD")
            .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board"),
    );
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let ipc = KiCadIpcClient::new(&socket);

    let documents = ipc.get_open_documents().expect("KiCad answered");
    assert!(
        !documents.is_empty(),
        "open the disposable board in KiCad before running this"
    );
    for document in &documents {
        println!(
            "open PCB document: type={} identifier={:?} project={:?}",
            document.r#type, document.identifier, document.project
        );
    }

    // The property the gate rests on: against a real KiCad, the requested
    // board is positively identified rather than refused as ambiguous.
    ipc.find_open_board(&board)
        .unwrap_or_else(|error| panic!("KiCad's open-document list was not resolvable: {error:#}"));

    // And a board that is genuinely not open is reported as such, not as an
    // ambiguity — the distinction that decides whether a file write may run.
    let absent = board.with_file_name("konnect-not-open-probe.kicad_pcb");
    let error = ipc
        .find_open_board(&absent)
        .expect_err("that board is not open");
    assert!(
        matches!(
            konnect_ipc::IpcFailure::from_error(error),
            konnect_ipc::IpcFailure::BoardNotOpen(_)
        ),
        "a complete open-document list must prove absence, not merely fail to confirm it"
    );
}
