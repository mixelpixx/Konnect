//! `set_design_rules` / `get_design_rules` for the copper-to-edge clearance and
//! silkscreen text minima (#960).
//!
//! Driven through served `tools/call` against a copy of the KiCad-written
//! placement project, with no KiCad reachable. Its rules hold KiCad's own
//! defaults for all three: 0.5, 0.8 and 0.08 mm.

use super::project_file_writer_tests::{call, handler_talking_to, project, text_of, Project};
use crate::mcp::protocol::CallToolResult;
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// KiCad 10.0.6 loads these ranges and resets anything outside them to the
/// default (measured with pcbnew's loader, see the PR). They are the promise,
/// not a mirror of the code.
const RANGES: [(&str, f64, f64); 3] = [
    ("min_copper_edge_clearance", -0.01, 25.0),
    ("min_text_height", 0.0, 100.0),
    ("min_text_thickness", 0.0, 25.0),
];

fn body(result: &CallToolResult) -> Value {
    serde_json::from_str(text_of(result)).unwrap()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The project with the named rules removed, to compare what else changed.
fn without_rules(mut project: Value, keys: &[&str]) -> Value {
    let rules = project["board"]["design_settings"]["rules"]
        .as_object_mut()
        .unwrap();
    for key in keys {
        rules.remove(*key);
    }
    project
}

/// Backdate the file, so a rewrite with identical bytes still shows.
pub(super) fn backdate(path: &Path) -> SystemTime {
    let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(then)
        .unwrap();
    then
}

pub(super) fn assert_untouched(path: &Path, bytes: &[u8], mtime: SystemTime, why: &str) {
    assert_eq!(std::fs::read(path).unwrap(), bytes, "{why}");
    assert_eq!(
        std::fs::metadata(path).unwrap().modified().unwrap(),
        mtime,
        "{why}: the project file was rewritten"
    );
}

#[tokio::test]
async fn sets_all_three_minima_and_reads_them_back() {
    let project = project();
    let before = read_json(&project.project_file);
    let board_before = std::fs::read(&project.board).unwrap();
    let handler = handler_talking_to("").await;

    let result = call(
        &handler,
        "set_design_rules",
        &project.board,
        &json!({
            "min_copper_edge_clearance": 0.2,
            "min_text_height": 1.0,
            "min_text_thickness": 0.15
        }),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));

    // The response is the re-read project: the three new values, and the
    // fixture's adjacent minima as KiCad wrote them.
    let expected = json!({
        "min_clearance": 0.0,
        "min_trace_width": 0.2,
        "min_via_drill": 0.3,
        "min_via_size": 0.5,
        "min_hole_to_hole": 0.25,
        "min_copper_edge_clearance": 0.2,
        "min_text_height": 1.0,
        "min_text_thickness": 0.15
    });
    assert_eq!(body(&result)["rules"], expected);
    let read = call(&handler, "get_design_rules", &project.board, &json!({})).await;
    assert_eq!(body(&read)["rules"], expected);

    let keys = RANGES.map(|(field, _, _)| field);
    assert_eq!(
        without_rules(read_json(&project.project_file), &keys),
        without_rules(before, &keys),
        "only the three named rules may change"
    );
    assert_eq!(std::fs::read(&project.board).unwrap(), board_before);
}

#[tokio::test]
async fn an_omitted_minimum_keeps_its_saved_value() {
    let project = project();
    let handler = handler_talking_to("").await;

    let result = call(
        &handler,
        "set_design_rules",
        &project.board,
        &json!({ "min_text_height": 1.0 }),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(body(&result)["changed"], json!(["min_text_height = 1"]));

    let read = call(&handler, "get_design_rules", &project.board, &json!({})).await;
    let rules = &body(&read)["rules"];
    assert_eq!(rules["min_text_height"], json!(1.0));
    assert_eq!(rules["min_copper_edge_clearance"], json!(0.5));
    assert_eq!(rules["min_text_thickness"], json!(0.08));
}

#[tokio::test]
async fn kicads_range_boundaries_are_accepted() {
    let project = project();
    let handler = handler_talking_to("").await;
    for at_max in [true, false] {
        let bound = |min: f64, max: f64| if at_max { max } else { min };
        let mut arguments = json!({});
        for (field, min, max) in RANGES {
            arguments[field] = json!(bound(min, max));
        }
        let result = call(&handler, "set_design_rules", &project.board, &arguments).await;
        assert!(!result.is_error, "{}", text_of(&result));
        let rules = &body(&result)["rules"];
        for (field, min, max) in RANGES {
            assert_eq!(rules[field], json!(bound(min, max)), "{field}");
        }
    }
}

#[tokio::test]
async fn naming_only_saved_values_does_not_rewrite_the_project() {
    let project = project();
    let bytes = std::fs::read(&project.project_file).unwrap();
    let mtime = backdate(&project.project_file);
    let handler = handler_talking_to("").await;

    let result = call(
        &handler,
        "set_design_rules",
        &project.board,
        &json!({
            "min_copper_edge_clearance": 0.5,
            "min_text_height": 0.8,
            "min_text_thickness": 0.08
        }),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(body(&result)["changed"], json!([]));
    assert_eq!(body(&result)["rules"]["min_text_thickness"], json!(0.08));
    assert_untouched(&project.project_file, &bytes, mtime, "a no-op");
}

#[tokio::test]
async fn an_out_of_range_or_mistyped_minimum_is_refused_before_writing() {
    let cases = [
        ("min_copper_edge_clearance", json!(-0.02)),
        ("min_copper_edge_clearance", json!(25.01)),
        ("min_text_height", json!(-1)),
        ("min_text_height", json!(100.01)),
        ("min_text_height", json!("1.0")),
        ("min_text_thickness", json!(-0.02)),
        ("min_text_thickness", json!(25.01)),
        ("min_text_thickness", json!(true)),
    ];
    let Project {
        board,
        project_file,
        ..
    } = &project();
    let bytes = std::fs::read(project_file).unwrap();
    let mtime = backdate(project_file);
    let handler = handler_talking_to("").await;
    for (field, value) in cases {
        // A valid change beside the bad one, which must not land either.
        let result = call(
            &handler,
            "set_design_rules",
            board,
            &json!({ "min_trace_width": 0.35, field: value }),
        )
        .await;
        assert!(result.is_error, "{field} = {value} was accepted");
        let error = &body(&result)["error"];
        assert_eq!(error["kind"], "invalid_argument", "{field} = {value}");
        assert_eq!(error["field"], field, "{field} = {value}");
        assert_untouched(project_file, &bytes, mtime, &format!("{field} = {value}"));
    }
}

/// Clients see the range before calling.
#[test]
fn the_schema_advertises_kicads_ranges() {
    let tool = super::verification::tools()
        .into_iter()
        .find(|tool| tool.name == "set_design_rules")
        .unwrap();
    for (field, min, max) in RANGES {
        let property = &tool.input_schema["properties"][field];
        assert_eq!(property["minimum"].as_f64(), Some(min), "{field}");
        assert_eq!(property["maximum"].as_f64(), Some(max), "{field}");
    }
}
