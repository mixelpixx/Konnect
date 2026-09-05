//! Placement acceptance for #383, using the KiCad-authored complex-hierarchy demo.

use super::*;
use konnect_schematic_editor::{sexp::SexpNode, Schematic};
use serde_json::json;
use std::path::{Path, PathBuf};

const PATH_A: &str = "/5b9623a5-6d01-41fc-9865-e1bc779418c8/00000000-0000-0000-0000-00004b3a1333";
const PATH_B: &str = "/5b9623a5-6d01-41fc-9865-e1bc779418c8/00000000-0000-0000-0000-00004b3a13a4";
const PLACERS: [&str; 3] = [
    "add_schematic_component",
    "batch_place_components",
    "add_power_symbol",
];

fn fixture(reused: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let (root, child) = schematic_target_tests::native_deep_project(directory.path());
    let mut schematic = Schematic::load(&child).unwrap();
    if !reused {
        // Derive a unique-child case by removing one native sheet and its saved
        // per-symbol entries. Do not synthesize the remaining instance metadata.
        let mut parent = Schematic::load(&root).unwrap();
        parent
            .sheets
            .remove_by_uuid("00000000-0000-0000-0000-00004b3a13a4")
            .unwrap();
        parent.overwrite().unwrap();
        for symbol in schematic.symbols.iter_mut() {
            let project = symbol
                .raw_sub_nodes
                .iter_mut()
                .find(|node| node.tag() == Some("instances"))
                .unwrap()
                .find_mut("project")
                .unwrap();
            let SexpNode::List(children) = project else {
                unreachable!()
            };
            children.retain(|node| node.tag() != Some("path") || node.value() != Some(PATH_B));
        }
    }
    // Alias the demo's embedded GND definition under the power tool's library
    // name. Geometry and properties remain KiCad-authored; no host library is needed.
    let library = schematic
        .raw_other
        .iter_mut()
        .find(|node| node.tag() == Some("lib_symbols"))
        .unwrap();
    // Unit validation also consults the library source. Export the native
    // embedded definitions into a temporary project library, keeping their
    // unit sub-symbols intact and only stripping the library namespace.
    let mut exported = library
        .find_all("symbol")
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    for entry in &mut exported {
        let name = entry
            .value()
            .unwrap()
            .split(':')
            .next_back()
            .unwrap()
            .to_string();
        let SexpNode::List(fields) = entry else {
            unreachable!()
        };
        fields[1] = SexpNode::Str(name);
    }
    let mut library_nodes = vec![
        konnect_schematic_editor::sexp::atom("kicad_symbol_lib"),
        konnect_schematic_editor::sexp::tagged(
            "version",
            vec![konnect_schematic_editor::sexp::atom("20241209")],
        ),
        konnect_schematic_editor::sexp::tagged(
            "generator",
            vec![SexpNode::Str("kicad_symbol_editor".to_string())],
        ),
    ];
    library_nodes.extend(exported);
    std::fs::write(
        child.parent().unwrap().join("fixture.kicad_sym"),
        konnect_schematic_editor::sexp::writer::write(&SexpNode::List(library_nodes)),
    )
    .unwrap();
    std::fs::write(child.parent().unwrap().join("sym-lib-table"),
        r#"(sym_lib_table (lib (name "complex_hierarchy") (type "KiCad") (uri "${KIPRJMOD}/fixture.kicad_sym") (options "") (descr "KiCad demo definitions")))"#).unwrap();
    let mut power = library
        .find_all("symbol")
        .into_iter()
        .find(|symbol| symbol.value() == Some("complex_hierarchy:GND"))
        .unwrap()
        .clone();
    let SexpNode::List(power_children) = &mut power else {
        unreachable!()
    };
    power_children[1] = SexpNode::Str("power:GND".to_string());
    let SexpNode::List(library_children) = library else {
        unreachable!()
    };
    library_children.push(power);
    schematic.overwrite().unwrap();
    (directory, root, child)
}

async fn place(name: &str, child: &Path) -> CallToolResult {
    let component = json!({
        "lib_id": "complex_hierarchy:LM358N", "reference": "U999",
        "value": "placement readback", "x": 100.1, "y": 80.2,
        "rotation": 90.0, "unit": 2
    });
    let mut args = match name {
        "add_schematic_component" => component,
        "batch_place_components" => json!({"components": [
            component,
            {"lib_id": "complex_hierarchy:R", "reference": "R999", "x": 110.1, "y": 80.2}
        ]}),
        "add_power_symbol" => json!({"power_net": "GND", "x": 100.1, "y": 80.2, "rotation": 90.0}),
        _ => unreachable!(),
    };
    args["schematic"] = json!(child.display().to_string());
    let context = Arc::new(ToolContext::new(
        ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        },
        Arc::new(ToolRouter::new()),
    ));
    let mut tools = sch_components::tools();
    tools.extend(sch_batch::tools());
    tools.extend(sch_wiring::tools());
    let tool = tools.iter().find(|tool| tool.name == name).unwrap();
    (tool.handler)(&args, context).await.unwrap()
}

fn body(result: &CallToolResult) -> Value {
    let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
        panic!("expected text result")
    };
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn native_placement_preserves_unique_and_reused_paths_with_committed_readback() {
    for reused in [false, true] {
        for name in PLACERS {
            let (_directory, root, child) = fixture(reused);
            let root_before = std::fs::read(&root).unwrap();
            let project_before = std::fs::read(root.with_extension("kicad_pro")).unwrap();
            let existing = Schematic::load(&child).unwrap().symbols.into_vec();
            let result = place(name, &child).await;
            assert!(!result.is_error, "{name}, reused={reused}: {result:?}");
            let response = body(&result);
            let placed = if name == "batch_place_components" {
                assert_eq!(response["placed_count"], 2);
                assert_eq!(response["errors"], json!([]));
                response["placed"].as_array().unwrap().clone()
            } else {
                vec![response]
            };
            let committed = Schematic::load(&child).unwrap();
            assert_eq!(committed.symbols.len(), existing.len() + placed.len());
            let expected = if reused {
                vec![PATH_A, PATH_B]
            } else {
                vec![PATH_A]
            };
            for entry in placed {
                let symbol = committed
                    .symbols
                    .iter()
                    .find(|symbol| Some(symbol.uuid.as_str()) == entry["uuid"].as_str())
                    .unwrap();
                let mut identities = symbol.instance_paths();
                identities.sort();
                let observed_project = identities[0].0.clone();
                assert_eq!(
                    identities,
                    expected
                        .iter()
                        .map(|path| ("complex_hierarchy".to_string(), path.to_string()))
                        .collect::<Vec<_>>()
                );
                assert_eq!(entry["instance_paths"], json!(expected));
                assert_eq!(
                    entry["schematic"],
                    committed.filepath().display().to_string()
                );
                assert_eq!(entry["project"], observed_project);
                assert_eq!(entry["added"], symbol.lib_id);
                assert_eq!(entry["reference"], symbol.reference().unwrap());
                assert_eq!(entry["value"], symbol.value_str().unwrap());
                assert_eq!(entry["x"].as_f64(), Some(symbol.at.x));
                assert_eq!(entry["y"].as_f64(), Some(symbol.at.y));
                assert_eq!(entry["rotation"].as_f64(), symbol.at.rotation);
                assert_eq!(entry["unit"], symbol.unit);
                if symbol.reference() == Some("U999") {
                    assert_eq!(symbol.unit, 2);
                    assert_ne!(
                        symbol.at.x, 100.1,
                        "response must report grid-snapped coordinates"
                    );
                }
                if name == "add_power_symbol" {
                    assert_eq!(entry["added_power"], symbol.value_str().unwrap());
                }
            }
            for original in existing {
                let saved = committed
                    .symbols
                    .iter()
                    .find(|symbol| symbol.uuid == original.uuid)
                    .unwrap();
                assert_eq!(
                    saved.to_sexp(),
                    original.to_sexp(),
                    "existing symbol changed"
                );
            }
            assert_eq!(std::fs::read(&root).unwrap(), root_before);
            assert_eq!(
                std::fs::read(root.with_extension("kicad_pro")).unwrap(),
                project_before
            );
        }
    }
}

#[tokio::test]
async fn native_placement_refuses_stale_instance_metadata_without_writing() {
    for corruption in [
        "missing",
        "foreign",
        "duplicate",
        "obsolete",
        "malformed",
        "missing-reference",
        "wrong-reference",
        "missing-unit",
        "wrong-unit",
    ] {
        for name in PLACERS {
            let (_directory, root, child) = fixture(true);
            let mut schematic = Schematic::load(&child).unwrap();
            let symbol = schematic.symbols.get_mut(0).unwrap();
            let instances = symbol
                .raw_sub_nodes
                .iter_mut()
                .find(|node| node.tag() == Some("instances"))
                .unwrap();
            let project = instances.find_mut("project").unwrap();
            let path = project.find("path").unwrap().clone();
            let SexpNode::List(children) = project else {
                unreachable!()
            };
            let index = children
                .iter()
                .position(|node| node.tag() == Some("path"))
                .unwrap();
            match corruption {
                "missing" => {
                    children.remove(index);
                }
                "foreign" => children[1] = SexpNode::Str("foreign-project".to_string()),
                "duplicate" => children.push(path),
                "obsolete" => {
                    let SexpNode::List(fields) = &mut children[index] else {
                        unreachable!()
                    };
                    fields[1] = SexpNode::Str("/obsolete/sheet".to_string());
                }
                "malformed" => {
                    let SexpNode::List(fields) = &mut children[index] else {
                        unreachable!()
                    };
                    fields.remove(1);
                }
                "missing-reference" => {
                    let SexpNode::List(fields) = &mut children[index] else {
                        unreachable!()
                    };
                    fields.retain(|field| field.tag() != Some("reference"));
                }
                "wrong-reference" => {
                    let SexpNode::List(fields) = children[index].find_mut("reference").unwrap()
                    else {
                        unreachable!()
                    };
                    fields[1] = SexpNode::Str("R999".to_string());
                }
                "missing-unit" => {
                    let SexpNode::List(fields) = &mut children[index] else {
                        unreachable!()
                    };
                    fields.retain(|field| field.tag() != Some("unit"));
                }
                "wrong-unit" => {
                    let SexpNode::List(fields) = children[index].find_mut("unit").unwrap() else {
                        unreachable!()
                    };
                    fields[1] = SexpNode::Atom("99".to_string());
                }
                _ => unreachable!(),
            }
            schematic.overwrite().unwrap();
            let before = std::fs::read(&child).unwrap();
            let root_before = std::fs::read(&root).unwrap();
            let result = place(name, &child).await;
            assert!(result.is_error, "{name}/{corruption}: {result:?}");
            assert_eq!(
                body(&result)["error"]["kind"],
                "stale_target",
                "{name}/{corruption}"
            );
            assert_eq!(
                std::fs::read(&child).unwrap(),
                before,
                "{name}/{corruption} wrote the target"
            );
            assert_eq!(std::fs::read(&root).unwrap(), root_before);
        }
    }
}

#[tokio::test]
async fn native_placement_refuses_ambiguous_ownership_without_writing() {
    for name in PLACERS {
        let (directory, root, child) = fixture(true);
        let competing = directory.path().join("competing.kicad_sch");
        std::fs::copy(&root, &competing).unwrap();
        std::fs::copy(
            root.with_extension("kicad_pro"),
            competing.with_extension("kicad_pro"),
        )
        .unwrap();
        let before = std::fs::read(&child).unwrap();
        let root_before = std::fs::read(&root).unwrap();
        let result = place(name, &child).await;
        assert!(result.is_error, "{name}: {result:?}");
        assert_eq!(body(&result)["error"]["kind"], "conflict");
        assert_eq!(std::fs::read(&child).unwrap(), before);
        assert_eq!(std::fs::read(&root).unwrap(), root_before);
        assert_eq!(std::fs::read(&competing).unwrap(), root_before);
    }
}

#[test]
fn native_placement_readback_refuses_wrong_document_and_missing_evidence() {
    for missing in [
        "document",
        "symbol",
        "Reference",
        "Value",
        "instances",
        "stale",
    ] {
        let (_directory, _root, child) = fixture(true);
        let mut schematic = Schematic::load(&child).unwrap();
        let context = sheet_instance_context(&child, &mut schematic).unwrap();
        let uuid = schematic.symbols.get(0).unwrap().uuid.clone();
        let symbol = schematic.symbols.get(0).unwrap();
        let expected = sch_components::ComponentTargetUnit::placement(
            &uuid,
            &context,
            &symbol.lib_id,
            symbol.at.x,
            symbol.at.y,
            symbol.at.rotation.unwrap_or(0.0),
            symbol.reference().unwrap(),
            symbol.value_str(),
            symbol.unit,
        );
        match missing {
            "symbol" => {
                schematic.symbols.remove_by_uuid(&uuid).unwrap();
            }
            "Reference" | "Value" => schematic
                .symbols
                .get_mut(0)
                .unwrap()
                .remove_property(missing),
            "instances" => schematic
                .symbols
                .get_mut(0)
                .unwrap()
                .raw_sub_nodes
                .retain(|node| node.tag() != Some("instances")),
            "stale" => schematic
                .symbols
                .get_mut(0)
                .unwrap()
                .set_instance_path("foreign", "/foreign", "U999", 1),
            "document" => {}
            _ => unreachable!(),
        }
        schematic.overwrite().unwrap();
        let other = child.with_file_name("other.kicad_sch");
        std::fs::copy(&child, &other).unwrap();
        let committed = Schematic::load(if missing == "document" {
            &other
        } else {
            &child
        })
        .unwrap();
        let result =
            sch_components::placed_component_readback(&child, &committed, &expected, &context)
                .expect_err("incomplete readback must not return success");
        assert_eq!(body(&result)["error"]["kind"], "stale_target", "{missing}");
        assert!(!body(&result)["message"]
            .as_str()
            .unwrap()
            .contains("did not modify"));
    }
}

#[test]
fn native_placement_readback_requires_requested_values() {
    for mismatch in ["unit", "lib_id", "x", "y", "rotation", "Value"] {
        let (_directory, _root, child) = fixture(true);
        let mut schematic = Schematic::load(&child).unwrap();
        let context = sheet_instance_context(&child, &mut schematic).unwrap();
        let symbol = schematic.symbols.get(0).unwrap();
        let expected = sch_components::ComponentTargetUnit::placement(
            &symbol.uuid,
            &context,
            if mismatch == "lib_id" {
                "Device:WRONG"
            } else {
                &symbol.lib_id
            },
            symbol.at.x + if mismatch == "x" { 1.27 } else { 0.0 },
            symbol.at.y + if mismatch == "y" { 1.27 } else { 0.0 },
            symbol.at.rotation.unwrap_or(0.0) + if mismatch == "rotation" { 90.0 } else { 0.0 },
            symbol.reference().unwrap(),
            if mismatch == "Value" {
                Some("wrong-value")
            } else {
                symbol.value_str()
            },
            symbol.unit + u32::from(mismatch == "unit"),
        );
        let committed = Schematic::load(&child).unwrap();
        let error =
            sch_components::placed_component_readback(&child, &committed, &expected, &context)
                .unwrap_err();
        assert_eq!(body(&error)["error"]["kind"], "stale_target", "{mismatch}");
    }
}
