//! `set_schematic_title_block` through served `tools/call` (#714).
//!
//! The fixture is KiCad's `complex_hierarchy` demo resaved by KiCad 10.0.6:
//! a root that places `ampli_ht.kicad_sch` twice. The expected title blocks
//! are what KiCad wrote and read back for the same edit; see
//! `tests/fixtures/title_block_hierarchy/README.md`.

use crate::mcp::handler::McpHandler;
use crate::mcp::protocol::CallToolResult;
use crate::tools::ServerConfig;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const ROOT: &str = "complex_hierarchy.kicad_sch";
const CHILD: &str = "ampli_ht.kicad_sch";

/// The title blocks as committed (KiCad 10.0.6 resaves of the demo).
const ROOT_BLOCK: &str =
    "\t(title_block\n\t\t(title \"Complex hierarchy: demo\")\n\t\t(date \"2017-01-15\")\n\t)\n";
const CHILD_BLOCK: &str = "\t(title_block\n\t\t(title \"Complex hierarchy: demo\")\n\t\t(date \"2017-01-15\")\n\t\t(rev \"1\")\n\t)\n";

/// Both files' title block after `EDIT`. KiCad 10.0.6 resaved Konnect's
/// output byte-for-byte unchanged, and its netlist reports these values on
/// all three sheet instances, with `${PROJECTNAME}` resolved.
const EDITED_BLOCK: &str = "\t(title_block\n\t\t(title \"Complex hierarchy: demo\")\n\t\t(date \"2026-10-20\")\n\t\t(rev \"B\")\n\t\t(company \"Precision \\\"Racing\\\"\")\n\t\t(comment 1 \"${PROJECTNAME}\")\n\t\t(comment 9 \"ninth\")\n\t)\n";

fn edit() -> Value {
    json!({
        "revision": "B", "date": "2026-10-20", "company": "Precision \"Racing\"",
        "comment_1": "${PROJECTNAME}", "comment_9": "ninth"
    })
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/title_block_hierarchy")
            .join(name),
    )
    .expect("the hierarchy fixture is committed")
}

fn project(dir: &Path) -> PathBuf {
    for name in [ROOT, CHILD, "complex_hierarchy.kicad_pro"] {
        std::fs::write(dir.join(name), fixture(name)).unwrap();
    }
    dir.join(ROOT)
}

async fn call(sheet: &Path, mut arguments: Value) -> CallToolResult {
    let handler = McpHandler::new(ServerConfig {
        auto_load_toolsets: true,
        eager_toolsets: true,
        ..Default::default()
    })
    .await
    .expect("handler builds");
    arguments["schematic"] = json!(sheet.to_string_lossy());
    let response = handler
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 714, "method": "tools/call",
            "params": { "name": "set_schematic_title_block", "arguments": arguments }
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

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

fn assert_untouched(dir: &Path) {
    assert_eq!(read(dir, ROOT), fixture(ROOT));
    assert_eq!(read(dir, CHILD), fixture(CHILD));
}

#[tokio::test]
async fn hierarchy_scope_edits_every_sheet_file_once_as_kicad_writes_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path());
    let mut arguments = edit();
    arguments["scope"] = json!("hierarchy");

    let result = call(&root, arguments).await;

    assert!(!result.is_error, "{}", body(&result));
    assert_eq!(
        read(dir.path(), ROOT),
        fixture(ROOT).replacen(ROOT_BLOCK, EDITED_BLOCK, 1)
    );
    assert_eq!(
        read(dir.path(), CHILD),
        fixture(CHILD).replacen(CHILD_BLOCK, EDITED_BLOCK, 1)
    );
    let body = body(&result);
    let sheets = body["sheets"].as_array().unwrap();
    // The root places the child twice; the file is edited, and listed, once.
    assert_eq!(sheets.len(), 2, "{body}");
    assert!(sheets[0]["file"].as_str().unwrap().ends_with(ROOT));
    assert!(sheets[1]["file"].as_str().unwrap().ends_with(CHILD));
    assert_eq!(
        sheets[0]["changed"],
        json!(["date", "revision", "company", "comment_1", "comment_9"])
    );
    assert_eq!(sheets[1]["title_block"]["company"], "Precision \"Racing\"");
    assert_eq!(sheets[1]["title_block"]["comment_1"], "${PROJECTNAME}");
    assert_eq!(body["source"], "file");
}

#[tokio::test]
async fn sheet_scope_edits_only_the_named_file() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());

    let result = call(&dir.path().join(CHILD), json!({ "revision": "B" })).await;

    assert!(!result.is_error, "{}", body(&result));
    assert_eq!(read(dir.path(), ROOT), fixture(ROOT));
    let child = read(dir.path(), CHILD);
    assert_eq!(
        child,
        fixture(CHILD).replacen("(rev \"1\")", "(rev \"B\")", 1)
    );
    assert_eq!(body(&result)["sheets"][0]["changed"], json!(["revision"]));
}

/// Eeschema locks only the root, and saves every sheet from memory.
#[tokio::test]
async fn a_lock_beside_the_root_refuses_a_sub_sheet_edit() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    std::fs::write(dir.path().join(format!("~{ROOT}.lck")), "{}").unwrap();

    let result = call(&dir.path().join(CHILD), json!({ "revision": "B" })).await;

    assert!(result.is_error);
    let body = body(&result);
    assert_eq!(body["error"]["kind"], "conflict", "{body}");
    assert!(body["error"]["paths"].to_string().contains(ROOT), "{body}");
    assert_untouched(dir.path());
}

#[tokio::test]
async fn refusals_leave_every_sheet_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path());

    for (arguments, kind) in [
        (json!({}), "invalid_argument"),
        (json!({ "scope": "all", "title": "x" }), "invalid_argument"),
        (json!({ "rev": "B" }), "invalid_argument"),
    ] {
        let result = call(&root, arguments.clone()).await;
        assert!(result.is_error, "{arguments} was accepted");
        assert_eq!(body(&result)["error"]["kind"], kind, "{arguments}");
    }
    assert_untouched(dir.path());

    // One sheet the model cannot hold stops the whole hierarchy before any write.
    let repeated = fixture(CHILD).replacen("(rev \"1\")", "(rev \"1\")\n\t\t(rev \"2\")", 1);
    std::fs::write(dir.path().join(CHILD), &repeated).unwrap();
    let result = call(&root, json!({ "scope": "hierarchy", "revision": "B" })).await;
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
    assert_eq!(read(dir.path(), ROOT), fixture(ROOT));
    assert_eq!(read(dir.path(), CHILD), repeated);

    // So does a sheet file the hierarchy places but that does not exist.
    std::fs::remove_file(dir.path().join(CHILD)).unwrap();
    let result = call(&root, json!({ "scope": "hierarchy", "revision": "B" })).await;
    assert!(result.is_error);
    let body = body(&result);
    assert_eq!(body["error"]["kind"], "file_not_found", "{body}");
    assert!(body["error"]["path"].as_str().unwrap().ends_with(CHILD));
    assert_eq!(read(dir.path(), ROOT), fixture(ROOT));
}

#[tokio::test]
async fn values_already_stored_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = project(dir.path());
    let modified = |name: &str| {
        std::fs::metadata(dir.path().join(name))
            .unwrap()
            .modified()
            .unwrap()
    };
    let before = (modified(ROOT), modified(CHILD));

    let result = call(
        &root,
        json!({ "scope": "hierarchy", "title": "Complex hierarchy: demo" }),
    )
    .await;

    assert!(!result.is_error, "{}", body(&result));
    let body = body(&result);
    assert_eq!(body["sheets"][0]["changed"], json!([]));
    assert_eq!(body["sheets"][1]["changed"], json!([]));
    assert_eq!((modified(ROOT), modified(CHILD)), before);
}

/// The refusal is read off what `commit_file_transaction` really returns for
/// a sheet that changed after it was read.
#[test]
fn a_sheet_changed_after_it_was_read_is_a_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let sheet = dir.path().join(CHILD);
    std::fs::write(&sheet, "(kicad_sch (version 1))").unwrap();
    let error = konnect_sexp::commit_file_transaction(
        dir.path(),
        vec![konnect_sexp::FileTransition::replace(
            &sheet,
            "(kicad_sch (version 0))",
            "(kicad_sch (version 2))",
        )],
    )
    .unwrap_err();

    let refusal = super::sch_hierarchy::stale_sheet_refusal(&error).expect("a conflict");
    let body = body(&refusal);
    assert_eq!(body["error"]["kind"], "conflict", "{body}");
    assert!(body["error"]["paths"].to_string().contains(CHILD), "{body}");
    assert_eq!(read(dir.path(), CHILD), "(kicad_sch (version 1))");

    // A journal left behind means a write may have happened: not this refusal.
    let journal = dir.path().join(".konnect-transaction-left.json");
    std::fs::write(&journal, "{}").unwrap();
    let kept = konnect_sexp::SexpError::TransactionConflict {
        path: sheet,
        journal,
        reason: "replacement did not remain durable".into(),
    };
    assert!(super::sch_hierarchy::stale_sheet_refusal(&kept).is_none());
}

/// The journal sits beside the project root, so a child sheet in a
/// subdirectory is inside it, and one outside the project is refused.
#[tokio::test]
async fn sheets_in_subdirectories_join_the_transaction_and_outside_ones_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(project_dir.join("sub")).unwrap();
    let root = project(&project_dir);
    std::fs::rename(project_dir.join(CHILD), project_dir.join("sub").join(CHILD)).unwrap();
    let placed = fixture(ROOT).replace(
        "(property \"Sheetfile\" \"ampli_ht.kicad_sch\"",
        "(property \"Sheetfile\" \"sub/ampli_ht.kicad_sch\"",
    );
    std::fs::write(&root, &placed).unwrap();

    let result = call(&root, json!({ "scope": "hierarchy", "revision": "B" })).await;
    assert!(!result.is_error, "{}", body(&result));
    assert!(read(&project_dir.join("sub"), CHILD).contains("(rev \"B\")"));

    let outside = fixture(ROOT).replace(
        "(property \"Sheetfile\" \"ampli_ht.kicad_sch\"",
        "(property \"Sheetfile\" \"../ampli_ht.kicad_sch\"",
    );
    std::fs::write(&root, &outside).unwrap();
    std::fs::write(dir.path().join(CHILD), fixture(CHILD)).unwrap();
    let result = call(&root, json!({ "scope": "hierarchy", "revision": "C" })).await;
    assert!(result.is_error);
    let body = body(&result);
    assert_eq!(body["error"]["kind"], "unsupported_capability", "{body}");
    assert_eq!(read(&project_dir, ROOT), outside);
    assert_eq!(read(dir.path(), CHILD), fixture(CHILD));
}

/// A hierarchy deeper than the walk's limit is refused, not truncated.
#[tokio::test]
async fn a_hierarchy_deeper_than_the_limit_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let depth = super::sch_hierarchy::MAX_HIERARCHY_DEPTH + 2;
    for level in 0..depth {
        let child = format!(
            "(sheet (property \"Sheetfile\" \"s{}.kicad_sch\"))",
            level + 1
        );
        std::fs::write(
            dir.path().join(format!("s{level}.kicad_sch")),
            format!("(kicad_sch\n\t(paper \"A4\")\n\t{child}\n)\n"),
        )
        .unwrap();
    }
    std::fs::write(
        dir.path().join(format!("s{depth}.kicad_sch")),
        "(kicad_sch\n\t(paper \"A4\")\n)\n",
    )
    .unwrap();

    let top = dir.path().join("s0.kicad_sch");
    let result = call(&top, json!({ "scope": "hierarchy", "revision": "B" })).await;

    assert!(result.is_error);
    let body = body(&result);
    assert_eq!(
        body["error"]["capability"], "sheet_hierarchy_deeper_than_limit",
        "{body}"
    );
    assert!(!read(dir.path(), "s0.kicad_sch").contains("title_block"));
}
