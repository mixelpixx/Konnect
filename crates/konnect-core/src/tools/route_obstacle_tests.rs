//! #887: a routed wire joins nothing beyond its two endpoints' nets.
//!
//! The fixture and the expected wires are in
//! `tests/fixtures/route_obstacles_kicad10.README.md`, with KiCad's netlist of
//! each result.

use super::ServerConfig;
use crate::mcp::handler::McpHandler;
use konnect_sexp::schematic::{extract_junctions, extract_wires, read_schematic};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// R1.2 -> R2.1: the horizontal-first route crosses R3.1.
const AROUND: (&str, &str, &str, &str) = ("R1", "2", "R2", "1");
/// R11.2 -> R12.1: horizontal-first crosses R13.1 (net OTHER), vertical-first R14.1.
const BLOCKED: (&str, &str, &str, &str) = ("R11", "2", "R12", "1");
/// R21.2 -> R22.1: nothing in the way.
const CLEAR: (&str, &str, &str, &str) = ("R21", "2", "R22", "1");
/// R41.2 -> R42.1: the horizontal-first corner lands on sheet pin `IN`.
const SHEET: (&str, &str, &str, &str) = ("R41", "2", "R42", "1");

/// The vertical-first route around R3.1, as KiCad netlists it: R1.2 and R2.1 only.
const AROUND_PATH: [(f64, f64); 3] = [(101.6, 105.41), (101.6, 107.95), (127.0, 107.95)];
/// The horizontal-first route `main` already drew.
const CLEAR_PATH: [(f64, f64); 3] = [(101.6, 156.21), (127.0, 156.21), (127.0, 158.75)];
/// The vertical-first route around the sheet pin: R41.2 and R42.1 only.
const SHEET_PATH: [(f64, f64); 3] = [(177.8, 105.41), (177.8, 107.95), (203.2, 107.95)];
/// Where R53's wire ends mid-span on R51's, with no junction: two nets to KiCad.
const DOTLESS_TEE: (f64, f64) = (190.5, 156.21);

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "route_obstacles_kicad10.kicad_sch",
        "route_obstacles_sub_kicad10.kicad_sch",
    ] {
        std::fs::copy(src.join(name), dir.path().join(name)).unwrap();
    }
    let path = dir.path().join("route_obstacles_kicad10.kicad_sch");
    (dir, path)
}

async fn call(path: &Path, name: &str, mut arguments: Value) -> (bool, Value) {
    let handler = McpHandler::new(ServerConfig {
        eager_toolsets: true,
        ..ServerConfig::default()
    })
    .await
    .expect("handler builds");
    arguments["schematic"] = json!(path.display().to_string());
    let response = handler
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 887, "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .await
        .expect("tools/call receives a response");
    let result = response.result.expect("successful JSON-RPC response");
    let body = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    (result["isError"] == json!(true), body)
}

fn pair((ref1, pin1, ref2, pin2): (&str, &str, &str, &str)) -> Value {
    json!({ "ref1": ref1, "pin1": pin1, "ref2": ref2, "pin2": pin2 })
}

fn sheet_wires(path: &Path) -> Vec<[f64; 4]> {
    let (_, tree) = read_schematic(path).unwrap();
    extract_wires(&tree)
        .into_iter()
        .map(|w| [w.x1, w.y1, w.x2, w.y2])
        .collect()
}

/// The wires a call added, sorted, given the sheet's wires before it.
fn added_wires(path: &Path, before: &[[f64; 4]]) -> Vec<[f64; 4]> {
    let mut added: Vec<[f64; 4]> = sheet_wires(path)
        .into_iter()
        .filter(|w| !before.contains(w))
        .collect();
    added.sort_by(|a, b| a.partial_cmp(b).unwrap());
    added
}

/// The legs of each path, sorted, as `added_wires` reports them.
fn legs(paths: &[&[(f64, f64)]]) -> Vec<[f64; 4]> {
    let mut legs: Vec<[f64; 4]> = paths
        .iter()
        .flat_map(|p| p.windows(2).map(|l| [l[0].0, l[0].1, l[1].0, l[1].1]))
        .collect();
    legs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    legs
}

fn junctions(path: &Path) -> Vec<(f64, f64)> {
    extract_junctions(&read_schematic(path).unwrap().1)
}

fn path_of(body: &Value) -> Vec<(f64, f64)> {
    body["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
        .collect()
}

/// One reason per bend, each naming what is in the way.
fn assert_names_both_obstacles(reasons: &[&str]) {
    assert_eq!(
        reasons,
        [
            "the horizontal-first route would join R13 pin 1 and label OTHER at (114.3, 54.61) on net OTHER",
            "the vertical-first route would join R14 pin 1 at (120.65, 57.15)",
        ]
    );
}

fn reasons(body: &Value) -> Vec<&str> {
    body["error"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn connect_pins_routes_around_a_pin_on_its_first_bend() {
    let (_dir, path) = fixture();
    let before = sheet_wires(&path);
    let (is_error, body) = call(&path, "connect_pins", pair(AROUND)).await;
    assert!(!is_error, "{body}");
    assert_eq!(path_of(&body), AROUND_PATH);
    assert_eq!(added_wires(&path, &before), legs(&[&AROUND_PATH]));
    assert_eq!(junctions(&path), [], "R3.1 gets no junction");
}

#[tokio::test]
async fn connect_pins_routes_around_a_sheet_pin_at_its_first_corner() {
    let (_dir, path) = fixture();
    let before = sheet_wires(&path);
    let (is_error, body) = call(&path, "connect_pins", pair(SHEET)).await;
    assert!(!is_error, "{body}");
    assert_eq!(path_of(&body), SHEET_PATH);
    assert_eq!(added_wires(&path, &before), legs(&[&SHEET_PATH]));
}

#[tokio::test]
async fn connect_pins_leaves_a_dotless_tee_elsewhere_undotted() {
    let (_dir, path) = fixture();
    let (is_error, body) = call(&path, "connect_pins", pair(CLEAR)).await;
    assert!(!is_error, "{body}");
    assert_eq!(path_of(&body), CLEAR_PATH);
    assert!(
        !junctions(&path).contains(&DOTLESS_TEE),
        "R53.2 stays off R51's net"
    );
}

#[tokio::test]
async fn add_schematic_connection_taps_a_wire_mid_span() {
    let (_dir, path) = fixture();
    let args = json!({ "x1": 184.15, "y1": 143.51, "x2": 184.15, "y2": 156.21 });
    let (is_error, body) = call(&path, "add_schematic_connection", args).await;
    assert!(!is_error, "{body}");
    assert_eq!(
        junctions(&path),
        [(184.15, 156.21)],
        "R54.2 joins R51's net"
    );
}

#[tokio::test]
async fn connect_pins_refuses_when_both_bends_join_another_net() {
    let (_dir, path) = fixture();
    let before = std::fs::read(&path).unwrap();
    let (is_error, body) = call(&path, "connect_pins", pair(BLOCKED)).await;
    assert!(is_error, "{body}");
    assert_eq!(body["error"]["kind"], "plan_blocked", "{body}");
    assert_eq!(body["error"]["operation"], "connect_pins", "{body}");
    assert_names_both_obstacles(&reasons(&body));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a refusal writes nothing"
    );
}

#[tokio::test]
async fn add_schematic_connection_refuses_when_both_bends_join_another_net() {
    let (_dir, path) = fixture();
    let before = std::fs::read(&path).unwrap();
    let args = json!({ "x1": 101.6, "y1": 54.61, "x2": 127.0, "y2": 57.15 });
    let (is_error, body) = call(&path, "add_schematic_connection", args).await;
    assert!(is_error, "{body}");
    assert_eq!(body["error"]["kind"], "plan_blocked", "{body}");
    assert_names_both_obstacles(&reasons(&body));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a refusal writes nothing"
    );
}

#[tokio::test]
async fn a_route_refuses_when_pins_cannot_be_resolved() {
    let (_dir, path) = fixture();
    // Without the embedded Device:R, no pin on the sheet can be located.
    let text = std::fs::read_to_string(&path).unwrap();
    let start = text.find("(lib_symbols").unwrap();
    let end = konnect_sexp::writer::find_balanced_block(&text, start)
        .unwrap()
        .1;
    std::fs::write(
        &path,
        format!("{}(lib_symbols){}", &text[..start], &text[end..]),
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();

    let args = json!({ "x1": 101.6, "y1": 105.41, "x2": 127.0, "y2": 107.95 });
    let (is_error, body) = call(&path, "add_schematic_connection", args).await;
    assert!(is_error, "{body}");
    assert_eq!(body["error"]["kind"], "plan_blocked", "{body}");
    // The promised wording, restated rather than read from the code under test.
    assert_eq!(
        reasons(&body),
        ["cannot check what the route would join: one or more placed symbols have unresolved library pin geometry"]
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn batch_connect_pins_skips_only_the_blocked_route() {
    let (_dir, path) = fixture();
    let before = sheet_wires(&path);
    let connections = json!([pair(AROUND), pair(BLOCKED), pair(CLEAR), pair(SHEET)]);
    let (is_error, body) = call(
        &path,
        "batch_connect_pins",
        json!({ "connections": connections }),
    )
    .await;
    assert!(!is_error, "{body}");
    assert_eq!(body["connected_count"], 3, "{body}");
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{body}");
    let error = errors[0].as_str().unwrap();
    let reasons = error
        .strip_prefix("R11.2 -> R12.1: ")
        .unwrap_or_else(|| panic!("{error}"));
    assert_names_both_obstacles(&reasons.split("; ").collect::<Vec<_>>());

    assert_eq!(
        added_wires(&path, &before),
        legs(&[&AROUND_PATH, &CLEAR_PATH, &SHEET_PATH])
    );
    assert_eq!(junctions(&path), []);
}

#[tokio::test]
async fn batch_connect_pins_with_every_route_blocked_is_an_error_and_writes_nothing() {
    let (_dir, path) = fixture();
    let before = std::fs::read(&path).unwrap();
    let (is_error, body) = call(
        &path,
        "batch_connect_pins",
        json!({ "connections": [pair(BLOCKED)] }),
    )
    .await;
    assert!(is_error, "{body}");
    assert_eq!(body["connected_count"], 0, "{body}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
