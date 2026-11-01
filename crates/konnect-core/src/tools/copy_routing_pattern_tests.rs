//! `copy_routing_pattern` on boards KiCad saved (#802).
//!
//! KiCad 10 indents with tabs and breaks the line after `(segment`, so the old
//! text scan found nothing on any board it wrote. Driven through served
//! `tools/call`, so dispatch, schema and handler are all under test.
//!
//! The boards are `specctra_two_resistors_locked{,_arc}.kicad_pcb`, saved by
//! KiCad 10.0.5 (see their README). Expected values are restated here as
//! literals from those files, not read from the code under test.

use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::{CallToolResult, ToolContent};
use crate::tools::pcb_board::board_mock::spawn_kicad_holding_boards;
use crate::tools::ServerConfig;
use konnect_sexp::writer::find_direct_child_blocks;
use konnect_sexp::{parse_sexp, SexpNode};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// The straight board: one segment (100.5, 50)→(105, 50) on GND and one via
/// at (105, 50), both locked.
const LOCKED: &str = "specctra_two_resistors_locked.kicad_pcb";
const SEGMENT_UUID: &str = "cb3de6bf-0cdb-4170-b26a-66acb0373995";
const VIA_UUID: &str = "db24f42e-63bc-4e53-a1a7-509928ef3cd6";

/// The same board plus one arc (99.5, 50)…(97.25, 47.75)…(95, 50) on VCC.
const LOCKED_ARC: &str = "specctra_two_resistors_locked_arc.kicad_pcb";
const ARC_UUID: &str = "ebe43b04-2de9-4c31-a95c-41d91ac54fd9";

/// KiCad's `ecc83-pp` demo in the 9.0 format, which numbers its nets.
const ECC83_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../konnect-sexp/tests/fixtures"
);
const ECC83: &str = "ecc83-pp.kicad_pcb";

/// A copy of one fixture board, kept alive for the test's duration.
struct Board {
    _dir: tempfile::TempDir,
    path: PathBuf,
    original: String,
}

impl Board {
    fn assert_untouched(&self) {
        assert!(
            std::fs::read_to_string(&self.path).unwrap() == self.original,
            "the board must not be modified"
        );
    }
}

fn board(fixture: &str) -> Board {
    board_from(FIXTURE_DIR, fixture)
}

fn board_from(dir: &str, fixture: &str) -> Board {
    let source = Path::new(dir).join(fixture);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(fixture);
    std::fs::copy(source, &path).unwrap();
    let original = std::fs::read_to_string(&path).unwrap();
    Board {
        _dir: dir,
        path,
        original,
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

async fn call_with(handler: &McpHandler, board: &Path, arguments: &Value) -> CallToolResult {
    let mut arguments = arguments.clone();
    arguments["board"] = json!(board.to_string_lossy());
    let response = handler
        .handle_message(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "copy_routing_pattern", "arguments": arguments }
        }))
        .await
        .expect("tools/call receives a response");
    assert!(response.error.is_none(), "tool errors are MCP results");
    serde_json::from_value(response.result.expect("a result"))
        .expect("result uses the advertised MCP shape")
}

/// No KiCad is reachable, so the saved board is authoritative.
async fn call(board: &Path, arguments: &Value) -> CallToolResult {
    call_with(&handler_talking_to("").await, board, arguments).await
}

fn text_of(result: &CallToolResult) -> &str {
    match result.content.first() {
        Some(ToolContent::Text { text }) => text,
        other => panic!("expected text content, got {other:?}"),
    }
}

fn json_of(result: &CallToolResult) -> Value {
    serde_json::from_str(text_of(result)).expect("JSON result")
}

/// The issue's region: it holds the segment and the via, and the destination
/// shifts everything by (+2.54, +5.08).
fn issue_region() -> Value {
    json!({
        "src_x1": 99.06, "src_y1": 48.26, "src_x2": 106, "src_y2": 51,
        "dest_x": 101.6, "dest_y": 53.34
    })
}

/// Top-level routing items of a board, parsed.
fn routing_items(content: &str) -> Vec<SexpNode> {
    find_direct_child_blocks(content, "kicad_pcb")
        .into_iter()
        .map(|(start, end)| parse_sexp(&content[start..end]).unwrap())
        .filter(|node| matches!(node.head(), Some("segment" | "arc" | "via")))
        .collect()
}

fn uuid_of(node: &SexpNode) -> &str {
    node.find_str("uuid")
        .or_else(|| node.find_str("tstamp"))
        .expect("routing items carry a uuid or tstamp")
}

/// The written text of a coordinate child, so a test sees what KiCad will
/// read, not an `f64` that compares equal after parsing.
fn written_xy(block: &str, head: &str) -> String {
    let node = parse_sexp(block).unwrap();
    let child = node.find(head).unwrap_or_else(|| panic!("no ({head} …)"));
    format!(
        "{} {}",
        child.get(1).and_then(SexpNode::as_str).unwrap(),
        child.get(2).and_then(SexpNode::as_str).unwrap()
    )
}

/// The written blocks of the routing items that are not in `before`.
fn new_blocks(before: &str, after: &str) -> Vec<String> {
    let old: Vec<String> = routing_items(before)
        .iter()
        .map(|n| uuid_of(n).to_string())
        .collect();
    find_direct_child_blocks(after, "kicad_pcb")
        .into_iter()
        .map(|(start, end)| after[start..end].to_string())
        .filter(|block| {
            let node = parse_sexp(block).unwrap();
            matches!(node.head(), Some("segment" | "arc" | "via"))
                && !old.iter().any(|u| u == uuid_of(&node))
        })
        .collect()
}

/// The issue's reproduction: on the board KiCad saved, the segment and the
/// via are both copied, shifted by exactly the anchor offset, with every
/// other attribute kept and a fresh uuid each.
#[tokio::test]
async fn copies_the_segment_and_the_via_kicad_saved() {
    let board = board(LOCKED);
    let result = call(&board.path, &issue_region()).await;
    assert!(!result.is_error, "{}", text_of(&result));
    let reply = json_of(&result);
    assert_eq!(reply["copied"], 2, "{reply}");
    assert_eq!(
        reply["copied_by_kind"],
        json!({ "segment": 1, "arc": 0, "via": 1 })
    );
    assert_eq!(reply["dx"], json!(2.54));
    assert_eq!(reply["dy"], json!(5.08));

    let written = std::fs::read_to_string(&board.path).unwrap();
    let copies = new_blocks(&board.original, &written);
    assert_eq!(copies.len(), 2, "{written}");
    let segment = copies.iter().find(|b| b.starts_with("(segment")).unwrap();
    let via = copies.iter().find(|b| b.starts_with("(via")).unwrap();

    // KiCad writes 103.04, not the f64 sum 103.03999999999999.
    assert_eq!(written_xy(segment, "start"), "103.04 55.08");
    assert_eq!(written_xy(segment, "end"), "107.54 55.08");
    assert_eq!(written_xy(via, "at"), "107.54 55.08");

    for (block, kept) in [
        (
            segment,
            &[
                "(width 0.25)",
                "(locked yes)",
                "(layer \"F.Cu\")",
                "(net \"GND\")",
            ][..],
        ),
        (
            via,
            &[
                "(size 0.6)",
                "(drill 0.3)",
                "(layers \"F.Cu\" \"B.Cu\")",
                "(locked yes)",
                "(net \"GND\")",
            ][..],
        ),
    ] {
        for attribute in kept {
            assert!(block.contains(attribute), "{attribute} lost in {block}");
        }
    }

    let new_uuids: Vec<String> = copies
        .iter()
        .map(|b| uuid_of(&parse_sexp(b).unwrap()).to_string())
        .collect();
    assert_ne!(new_uuids[0], new_uuids[1]);
    for uuid in &new_uuids {
        assert!(uuid != SEGMENT_UUID && uuid != VIA_UUID, "{uuid} reused");
        assert!(written.matches(uuid.as_str()).count() == 1);
    }
    let reported: BTreeSet<&str> = reply["uuids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap())
        .collect();
    let expected: BTreeSet<&str> = new_uuids.iter().map(String::as_str).collect();
    assert_eq!(reported, expected, "the reply names the copies it wrote");

    // Nothing else moved: taking the inserted copies out gives back the
    // original byte for byte.
    let mut stripped = written.clone();
    for block in &copies {
        let at = stripped.find(block.as_str()).unwrap();
        // The fixture is CRLF; the copy's line break is too.
        let eol_start = stripped[..at].rfind("\r\n").unwrap();
        stripped.replace_range(eol_start..at + block.len(), "");
    }
    assert!(stripped == board.original, "the copy changed other bytes");
}

/// An arc is copied with all three of its points shifted, including `mid`.
#[tokio::test]
async fn copies_an_arc_with_its_midpoint() {
    let board = board(LOCKED_ARC);
    let result = call(
        &board.path,
        &json!({
            "src_x1": 94, "src_y1": 47, "src_x2": 100, "src_y2": 50.5,
            "dest_x": 104, "dest_y": 57
        }),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));
    let reply = json_of(&result);
    assert_eq!(
        reply["copied_by_kind"],
        json!({ "segment": 0, "arc": 1, "via": 0 }),
        "{reply}"
    );

    let written = std::fs::read_to_string(&board.path).unwrap();
    let copies = new_blocks(&board.original, &written);
    assert_eq!(copies.len(), 1);
    let arc = &copies[0];
    // dx = 104 - 94 = 10, dy = 57 - 47 = 10.
    assert_eq!(written_xy(arc, "start"), "109.5 60");
    assert_eq!(written_xy(arc, "mid"), "107.25 57.75");
    assert_eq!(written_xy(arc, "end"), "105 60");
    assert!(arc.contains("(net \"VCC\")") && arc.contains("(width 0.25)"));
    assert_ne!(uuid_of(&parse_sexp(arc).unwrap()), ARC_UUID);
}

/// The selection rule is whole-item containment. A segment with one end
/// inside the box is not copied by half: it is named in the reply, and the
/// board is not touched.
#[tokio::test]
async fn names_an_item_that_crosses_the_region_and_writes_nothing() {
    let board = board(LOCKED);
    let result = call(
        &board.path,
        &json!({
            "src_x1": 99, "src_y1": 49, "src_x2": 102, "src_y2": 51,
            "dest_x": 99, "dest_y": 60
        }),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));
    let reply = json_of(&result);
    assert_eq!(reply["copied"], 0, "{reply}");
    assert_eq!(
        reply["excluded_crossing"],
        json!([{ "kind": "segment", "uuid": SEGMENT_UUID }])
    );
    board.assert_untouched();
}

/// `net_map` renames the copy's net and leaves the original's alone.
#[tokio::test]
async fn net_map_renames_only_the_copies() {
    let board = board(LOCKED);
    let mut arguments = issue_region();
    arguments["net_map"] = json!({ "GND": "VCC" });
    let result = call(&board.path, &arguments).await;
    assert!(!result.is_error, "{}", text_of(&result));

    let written = std::fs::read_to_string(&board.path).unwrap();
    for block in new_blocks(&board.original, &written) {
        assert!(block.contains("(net \"VCC\")"), "{block}");
        assert!(!block.contains("(net \"GND\")"), "{block}");
    }
    for node in routing_items(&written) {
        if [SEGMENT_UUID, VIA_UUID].contains(&uuid_of(&node)) {
            assert_eq!(
                node.find("net")
                    .and_then(|n| n.get(1))
                    .and_then(SexpNode::as_str),
                Some("GND")
            );
        }
    }
}

/// Malformed arguments are refused by name, and nothing is written: a
/// net_map value that is not a net name used to be dropped, an inverted box
/// used to select nothing and report success, and a destination net that no
/// pad or track carries would leave the copy unconnected.
#[tokio::test]
async fn refuses_malformed_arguments_by_name() {
    for (key, value, field) in [
        ("net_map", json!({ "GND": 3 }), "net_map.GND"),
        ("src_x2", json!(90), "src_x2"),
        ("src_y2", json!(40), "src_y2"),
        ("net_map", json!({ "GND": "GDN" }), "net_map.GND"),
    ] {
        let board = board(LOCKED);
        let mut arguments = issue_region();
        arguments[key] = value;
        let result = call(&board.path, &arguments).await;
        assert!(result.is_error, "{field}");
        let reply = json_of(&result);
        assert_eq!(reply["error"]["kind"], "invalid_argument", "{field}");
        assert_eq!(reply["error"]["field"], field);
        board.assert_untouched();
    }
}

/// A routing item this tool cannot place is refused by uuid instead of being
/// skipped, which would report a selection smaller than the region holds.
#[tokio::test]
async fn refuses_a_routing_item_without_readable_coordinates() {
    let board = board(LOCKED);
    let broken = board.original.replacen("(at 105 50)", "(at 105)", 1);
    assert_ne!(broken, board.original);
    std::fs::write(&board.path, &broken).unwrap();

    let result = call(&board.path, &issue_region()).await;
    assert!(result.is_error);
    assert!(text_of(&result).contains(VIA_UUID), "{}", text_of(&result));
    assert_eq!(std::fs::read_to_string(&board.path).unwrap(), broken);
}

/// KiCad holds this very board: a copy written to the file would be
/// discarded by its next save, so nothing is written.
#[tokio::test]
async fn refuses_while_kicad_holds_the_board() {
    let board = board(LOCKED);
    let kicad = spawn_kicad_holding_boards(&[&board.path], |_| None);
    let handler = handler_talking_to(kicad.address()).await;

    let result = call_with(&handler, &board.path, &issue_region()).await;
    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("currently holds this board open"),
        "{}",
        text_of(&result)
    );
    board.assert_untouched();
}

/// KiCad's sibling lock is present though IPC is unreachable: the saved
/// board cannot be proven authoritative, so nothing is written.
#[tokio::test]
async fn refuses_while_kicad_lock_is_present() {
    let board = board(LOCKED);
    let lock = board.path.with_file_name(format!("~{LOCKED}.lck"));
    std::fs::write(&lock, "{}").unwrap();

    let result = call(&board.path, &issue_region()).await;
    assert!(result.is_error, "{}", text_of(&result));
    assert_eq!(json_of(&result)["error"]["kind"], "unsafe_file_fallback");
    board.assert_untouched();
}

/// The board changed between the read and the write: the copy is refused as
/// a conflict and the newer file is kept as it is.
#[test]
fn a_board_changed_after_the_read_is_not_replaced() {
    use super::verification::{commit_routing_copy, plan_routing_copy, RoutingCopy};

    let board = board(LOCKED);
    let copy = RoutingCopy {
        region: [99.06, 48.26, 106.0, 51.0],
        dx: 2.54,
        dy: 5.08,
        net_map: Default::default(),
    };
    let planned = plan_routing_copy(&board.original, &copy).expect("the fixture plans");
    let newer = board.original.replacen("(width 0.25)", "(width 0.3)", 1);
    std::fs::write(&board.path, &newer).unwrap();

    let result = commit_routing_copy(&board.path, &board.original, &copy, &planned)
        .expect("a conflict is a result, not an error");
    assert!(result.is_error);
    assert_eq!(json_of(&result)["error"]["kind"], "conflict");
    assert_eq!(std::fs::read_to_string(&board.path).unwrap(), newer);
}

/// KiCad 9 references nets by number: `(net 2)` on the item, named in a
/// top-level table. A copy keeps the number, and `net_map`, which renames by
/// name, is refused rather than writing a name the table does not declare.
/// `ecc83-pp` is KiCad's own demo board in the 9.0 format.
#[tokio::test]
async fn numbered_nets_are_copied_as_is_and_refuse_net_map() {
    let board = board_from(ECC83_DIR, ECC83);
    // Exactly the segment (139.573, 99.695)→(141.605, 99.695) on net 2.
    let region = json!({
        "src_x1": 139.5, "src_y1": 99.69, "src_x2": 141.7, "src_y2": 99.7,
        "dest_x": 60, "dest_y": 60
    });

    let mut mapped = region.clone();
    mapped["net_map"] = json!({ "Net-(P3-P1)": "GND" });
    let result = call(&board.path, &mapped).await;
    assert!(result.is_error);
    let reply = json_of(&result);
    assert_eq!(reply["error"]["kind"], "invalid_argument");
    assert_eq!(reply["error"]["field"], "net_map");
    board.assert_untouched();

    let result = call(&board.path, &region).await;
    assert!(!result.is_error, "{}", text_of(&result));
    let written = std::fs::read_to_string(&board.path).unwrap();
    let copies = new_blocks(&board.original, &written);
    assert_eq!(copies.len(), 1, "{}", text_of(&result));
    assert!(copies[0].contains("(net 2)"), "{}", copies[0]);
    assert_eq!(written_xy(&copies[0], "start"), "60.073 60.005");
}

/// Boards from KiCad 7 and earlier identify items with `(tstamp …)`, not
/// `(uuid …)`. The copy gets a fresh one in the same tag. Derived from the
/// KiCad 10 fixture, since KiCad 10 cannot save the older format.
#[tokio::test]
async fn copies_items_identified_by_tstamp() {
    let board = board(LOCKED);
    let legacy = board.original.replace(
        &format!("(uuid \"{SEGMENT_UUID}\")"),
        &format!("(tstamp \"{SEGMENT_UUID}\")"),
    );
    std::fs::write(&board.path, &legacy).unwrap();

    let result = call(&board.path, &issue_region()).await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(json_of(&result)["copied"], 2);
    let written = std::fs::read_to_string(&board.path).unwrap();
    let segment = new_blocks(&legacy, &written)
        .into_iter()
        .find(|b| b.starts_with("(segment"))
        .expect("the segment is copied");
    let tstamp = parse_sexp(&segment).unwrap();
    let tstamp = tstamp.find_str("tstamp").expect("the copy keeps the tag");
    assert_ne!(tstamp, SEGMENT_UUID);
}

/// A destination equal to the source anchor would stack every copy on its
/// original.
#[tokio::test]
async fn refuses_a_zero_offset() {
    let board = board(LOCKED);
    let mut arguments = issue_region();
    arguments["dest_x"] = json!(99.06);
    arguments["dest_y"] = json!(48.26);
    let result = call(&board.path, &arguments).await;
    assert!(result.is_error);
    assert_eq!(json_of(&result)["error"]["field"], "dest_x");
    board.assert_untouched();
}

/// The schema gate refuses a non-string `net_map` value on a served call; the
/// handler refuses it too, for callers that reach it directly.
#[tokio::test]
async fn the_handler_refuses_a_net_map_value_that_is_not_a_string() {
    let board = board(LOCKED);
    let tool = super::verification::tools()
        .into_iter()
        .find(|t| t.name == "copy_routing_pattern")
        .expect("registered");
    let ctx = std::sync::Arc::new(crate::tools::ToolContext::new(
        ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        },
        std::sync::Arc::new(crate::router::ToolRouter::new()),
    ));
    let mut arguments = issue_region();
    arguments["board"] = json!(board.path.to_string_lossy());
    arguments["net_map"] = json!({ "GND": 3 });

    let result = (tool.handler)(&arguments, ctx)
        .await
        .expect("a refusal is a result");
    assert!(result.is_error);
    assert_eq!(json_of(&result)["error"]["field"], "net_map.GND");
    board.assert_untouched();
}
