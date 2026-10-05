//! JLCPCB's coordinate conversion uses KiCad's own board-space pad boxes.
//! No pad-shape approximation or local-box rotation lives here.
use anyhow::{Context, Result};
use konnect_ipc::{gen::kiapi, KiCadIpcClient};
use prost::Message;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize)]
pub(super) struct Midpoint {
    pub designator: String,
    pub anchor_x_mm: f64,
    pub anchor_y_mm: f64,
    pub midpoint_x_mm: f64,
    pub midpoint_y_mm: f64,
    pub algorithm: &'static str,
}

pub(super) struct Snapshot {
    pub contents: String,
    pub midpoints: BTreeMap<String, Midpoint>,
}

pub(super) fn capture(
    client: &KiCadIpcClient,
    document: kiapi::common::types::DocumentSpecifier,
) -> Result<Snapshot> {
    let contents = client.save_document_to_string_in(document.clone())?;
    let items = client.get_items_in(
        document.clone(),
        kiapi::common::types::KiCadObjectType::KotPcbFootprint,
    )?;
    let mut midpoints = BTreeMap::new();
    for item in items {
        let fp = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())?;
        if fp
            .attributes
            .as_ref()
            .is_some_and(|attrs| attrs.do_not_populate || attrs.exclude_from_position_files)
        {
            continue;
        }
        let reference = fp
            .reference_field
            .as_ref()
            .and_then(|field| field.text.as_ref())
            .and_then(|text| text.text.as_ref())
            .context("footprint missing reference text")?
            .text
            .clone();
        anyhow::ensure!(!reference.is_empty(), "footprint has empty designator");
        let anchor = fp.position.context("footprint missing anchor")?;
        let definition = fp.definition.context("footprint missing definition")?;
        let mut ids = Vec::new();
        for item in definition.items {
            if item.type_url.ends_with("kiapi.board.types.Pad") {
                let pad = kiapi::board::types::Pad::decode(item.value.as_slice())?;
                let id = pad.id.context("pad missing UUID")?.value;
                anyhow::ensure!(!id.is_empty(), "pad has empty UUID");
                ids.push(id);
            }
        }
        let boxes = client.get_item_boxes_in(document.clone(), &ids)?;
        let (x, y) = if boxes.is_empty() {
            (anchor.x_nm as f64 / 1e6, anchor.y_nm as f64 / 1e6)
        } else {
            union_center(boxes.values())?
        };
        let midpoint = Midpoint {
            designator: reference.clone(),
            anchor_x_mm: anchor.x_nm as f64 / 1e6,
            anchor_y_mm: anchor.y_nm as f64 / 1e6,
            midpoint_x_mm: x,
            midpoint_y_mm: y,
            algorithm: if ids.is_empty() {
                "no_pads_observed_anchor"
            } else {
                "kicad_native_board_pad_boxes_union_center"
            },
        };
        anyhow::ensure!(
            midpoints.insert(reference, midpoint).is_none(),
            "duplicate footprint designator"
        );
    }
    anyhow::ensure!(
        client.save_document_to_string_in(document)? == contents,
        "board changed while reading JLCPCB midpoint geometry; retry after edits stop"
    );
    Ok(Snapshot {
        contents,
        midpoints,
    })
}

fn union_center<'a>(
    boxes: impl Iterator<Item = &'a kiapi::common::types::Box2>,
) -> Result<(f64, f64)> {
    let (mut left, mut top, mut right, mut bottom) = (i128::MAX, i128::MAX, i128::MIN, i128::MIN);
    for bounds in boxes {
        let p = bounds.position.as_ref().context("box missing position")?;
        let size = bounds.size.as_ref().context("box missing size")?;
        anyhow::ensure!(size.x_nm >= 0 && size.y_nm >= 0, "negative box size");
        left = left.min(i128::from(p.x_nm));
        top = top.min(i128::from(p.y_nm));
        right = right.max(i128::from(p.x_nm) + i128::from(size.x_nm));
        bottom = bottom.max(i128::from(p.y_nm) + i128::from(size.y_nm));
    }
    anyhow::ensure!(left != i128::MAX, "empty box union");
    // BOX2I::GetCenter uses integer half-size; preserve KiCad's nm rounding.
    Ok((
        (left + (right - left) / 2) as f64 / 1e6,
        (top + (bottom - top) / 2) as f64 / 1e6,
    ))
}

pub(super) fn convert_positions(
    source: &str,
    midpoints: &BTreeMap<String, Midpoint>,
) -> Result<(String, Vec<Midpoint>)> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(source.as_bytes());
    let header = reader.headers()?.clone();
    let index = |name| {
        header
            .iter()
            .position(|field| field == name)
            .with_context(|| format!("missing position column {name}"))
    };
    let (reference, ix, iy) = (index("Ref")?, index("PosX")?, index("PosY")?);
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(&header)?;
    let mut evidence = Vec::new();
    for row in reader.records() {
        let row = row?;
        let designator = row.get(reference).context("missing designator")?;
        let midpoint = midpoints
            .get(designator)
            .with_context(|| format!("no native midpoint for {designator}"))?;
        let mut fields = row.iter().map(str::to_owned).collect::<Vec<_>>();
        // The native CLI wrapper uses board origin, mm, Y-up, no bottom-X negation.
        let x: f64 = row.get(ix).context("missing PosX")?.parse()?;
        let y: f64 = row.get(iy).context("missing PosY")?.parse()?;
        anyhow::ensure!(
            x.is_finite()
                && y.is_finite()
                && (x - midpoint.anchor_x_mm).abs() <= 0.000001
                && (y + midpoint.anchor_y_mm).abs() <= 0.000001,
            "CLI anchor disagrees with native snapshot for {designator}"
        );
        if (midpoint.midpoint_x_mm - midpoint.anchor_x_mm).abs() > 1e-9 {
            fields[ix] = format!("{:.6}", midpoint.midpoint_x_mm);
        }
        if (midpoint.midpoint_y_mm - midpoint.anchor_y_mm).abs() > 1e-9 {
            fields[iy] = format!("{:.6}", -midpoint.midpoint_y_mm);
        }
        writer.write_record(fields)?;
        evidence.push(midpoint.clone());
    }
    Ok((String::from_utf8(writer.into_inner()?)?, evidence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockIpcServer;
    use konnect_ipc::builders::pack_any;

    fn box_at(x: i64, y: i64, w: i64, h: i64) -> kiapi::common::types::Box2 {
        kiapi::common::types::Box2 {
            position: Some(kiapi::common::types::Vector2 { x_nm: x, y_nm: y }),
            size: Some(kiapi::common::types::Vector2 { x_nm: w, y_nm: h }),
        }
    }

    fn fixture_server(board: &std::path::Path, mode: &'static str) -> MockIpcServer {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let document =
            super::super::pcb_board::board_mock::board_document(&board.to_string_lossy());
        let reads = AtomicUsize::new(0);
        MockIpcServer::spawn("midpoint-capture", move |request| {
            let command = request.message.unwrap();
            let response = if command.type_url.ends_with("GetOpenDocuments") {
                pack_any(
                    &kiapi::common::commands::GetOpenDocumentsResponse {
                        documents: if mode == "closed" {
                            vec![]
                        } else {
                            vec![document.clone()]
                        },
                    },
                    "kiapi.common.commands.GetOpenDocumentsResponse",
                )
            } else if command.type_url.ends_with("SaveDocumentToString") {
                let command =
                    kiapi::common::commands::SaveDocumentToString::decode(command.value.as_slice())
                        .unwrap();
                assert_eq!(command.document.as_ref(), Some(&document));
                let mut contents =
                    include_str!("../../tests/fixtures/jlcpcb_midpoints/midpoints.kicad_pcb")
                        .to_string();
                if mode == "drift" && reads.fetch_add(1, Ordering::SeqCst) > 0 {
                    contents.push('\n');
                }
                pack_any(
                    &kiapi::common::commands::SavedDocumentResponse {
                        document: Some(document.clone()),
                        contents,
                    },
                    "kiapi.common.commands.SavedDocumentResponse",
                )
            } else if command.type_url.ends_with("GetItems") {
                let command =
                    kiapi::common::commands::GetItems::decode(command.value.as_slice()).unwrap();
                assert_eq!(command.header.unwrap().document.as_ref(), Some(&document));
                let mut items = kiapi::common::commands::GetItemsResponse::decode(
                    include_bytes!("../../tests/fixtures/jlcpcb_midpoints/footprints.pb")
                        .as_slice(),
                )
                .unwrap();
                if mode == "no-pads" {
                    items.items.truncate(1);
                    let mut fp = kiapi::board::types::FootprintInstance::decode(
                        items.items[0].value.as_slice(),
                    )
                    .unwrap();
                    fp.definition
                        .as_mut()
                        .unwrap()
                        .items
                        .retain(|item| !item.type_url.ends_with("kiapi.board.types.Pad"));
                    items.items[0].value = fp.encode_to_vec();
                }
                if matches!(
                    mode,
                    "excluded-with-pads" | "excluded-no-pads" | "dnp" | "duplicate" | "bom-only"
                ) {
                    // Change only attributes/reference on native-captured items;
                    // leave the pad geometry and UUID oracle untouched.
                    let first = kiapi::board::types::FootprintInstance::decode(
                        items.items[0].value.as_slice(),
                    )
                    .unwrap();
                    for item in &mut items.items[1..3] {
                        let mut fp =
                            kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
                                .unwrap();
                        fp.reference_field = first.reference_field.clone();
                        let attrs = fp.attributes.get_or_insert_default();
                        attrs.exclude_from_position_files = mode.starts_with("excluded-");
                        attrs.do_not_populate = mode == "dnp";
                        attrs.exclude_from_bill_of_materials = mode == "bom-only";
                        if mode == "excluded-no-pads" {
                            fp.definition
                                .as_mut()
                                .unwrap()
                                .items
                                .retain(|item| !item.type_url.ends_with("kiapi.board.types.Pad"));
                        }
                        item.value = fp.encode_to_vec();
                    }
                }
                pack_any(&items, "kiapi.common.commands.GetItemsResponse")
            } else if command.type_url.ends_with("GetBoundingBox") {
                let requested =
                    kiapi::common::commands::GetBoundingBox::decode(command.value.as_slice())
                        .unwrap();
                assert_eq!(requested.header.unwrap().document.as_ref(), Some(&document));
                let captured = kiapi::common::commands::GetBoundingBoxResponse::decode(
                    include_bytes!("../../tests/fixtures/jlcpcb_midpoints/boxes.pb").as_slice(),
                )
                .unwrap();
                let map: BTreeMap<_, _> = captured
                    .items
                    .into_iter()
                    .zip(captured.boxes)
                    .map(|(id, bounds)| (id.value, bounds))
                    .collect();
                let mut response = kiapi::common::commands::GetBoundingBoxResponse::default();
                for id in requested.items {
                    if mode != "missing-box" {
                        response.boxes.push(map[&id.value]);
                        response.items.push(id);
                    }
                }
                pack_any(&response, "kiapi.common.commands.GetBoundingBoxResponse")
            } else {
                panic!("unexpected command: {}", command.type_url)
            };
            kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsOk as i32,
                    error_message: String::new(),
                }),
                message: Some(response),
                ..Default::default()
            }
        })
    }

    #[test]
    fn position_exclusions_precede_duplicate_detection_and_geometry() {
        let board = std::path::Path::new("midpoints.kicad_pcb");
        let document =
            super::super::pcb_board::board_mock::board_document(&board.to_string_lossy());
        for mode in ["excluded-with-pads", "excluded-no-pads", "dnp"] {
            let mock = fixture_server(board, mode);
            let snapshot = capture(&KiCadIpcClient::new(mock.address()), document.clone()).unwrap();
            assert_eq!(snapshot.midpoints.len(), 9, "{mode}");
            // Two excluded items share the exported first item's reference.
            // They must neither refuse the export nor replace its geometry.
            let point = &snapshot.midpoints["JF0"];
            assert_eq!((point.midpoint_x_mm, point.midpoint_y_mm), (20.0, 21.27));
            let positions =
                "Ref,Val,Package,PosX,PosY,Rot,Side\nJF0,X,JST,20.000000,-20.000000,0,top\n";
            let (converted, evidence) = convert_positions(positions, &snapshot.midpoints).unwrap();
            assert!(converted.contains("20.000000,-21.270000"), "{mode}");
            assert_eq!(evidence.len(), 1, "{mode}");
            assert_eq!(
                snapshot.contents,
                include_str!("../../tests/fixtures/jlcpcb_midpoints/midpoints.kicad_pcb")
            );
        }
        for mode in ["duplicate", "bom-only"] {
            let mock = fixture_server(board, mode);
            let error = capture(&KiCadIpcClient::new(mock.address()), document.clone())
                .err()
                .expect("position-included duplicates must still refuse");
            assert!(
                error.to_string().contains("duplicate footprint designator"),
                "{mode}: {error}"
            );
        }
    }

    #[test]
    fn captured_native_geometry_matches_literal_board_space_oracle() {
        let board = std::path::Path::new("midpoints.kicad_pcb");
        let document =
            super::super::pcb_board::board_mock::board_document(&board.to_string_lossy());
        let mock = fixture_server(board, "good");
        let snapshot = capture(&KiCadIpcClient::new(mock.address()), document.clone()).unwrap();
        let expected = [
            (20.0, 21.27),
            (31.27, 20.0),
            (40.0, 18.73),
            (48.73, 20.0),
            (60.721984, 20.721984),
        ];
        assert_eq!(snapshot.midpoints.len(), 11);
        let centered = &snapshot.midpoints["R1"];
        assert_eq!(
            (centered.midpoint_x_mm, centered.midpoint_y_mm),
            (80.0, 20.0)
        );
        for side in ["F", "B"] {
            for (i, &point) in expected.iter().enumerate() {
                let observed = &snapshot.midpoints[&format!("J{side}{i}")];
                assert_eq!((observed.midpoint_x_mm, observed.midpoint_y_mm), point);
            }
        }
        for mode in ["drift", "missing-box"] {
            let mock = fixture_server(board, mode);
            assert!(
                capture(&KiCadIpcClient::new(mock.address()), document.clone()).is_err(),
                "{mode}"
            );
        }
        let mock = fixture_server(board, "no-pads");
        let snapshot = capture(&KiCadIpcClient::new(mock.address()), document).unwrap();
        let observed = snapshot.midpoints.values().next().unwrap();
        assert_eq!(observed.algorithm, "no_pads_observed_anchor");
        assert_eq!(observed.midpoint_x_mm, observed.anchor_x_mm);
        assert_eq!(observed.midpoint_y_mm, observed.anchor_y_mm);
    }

    #[tokio::test]
    async fn served_jlcpcb_refuses_closed_drifting_or_missing_geometry_without_output() {
        use crate::mcp::handler::McpHandler;
        use crate::tools::ServerConfig;
        for mode in ["closed", "drift", "missing-box", "duplicate", "bom-only"] {
            let dir = tempfile::tempdir().unwrap();
            let board = dir.path().join("midpoints.kicad_pcb");
            let before = include_str!("../../tests/fixtures/jlcpcb_midpoints/midpoints.kicad_pcb");
            std::fs::write(&board, before).unwrap();
            let output = dir.path().join("package");
            let mock = fixture_server(&board, mode);
            let handler = McpHandler::new(ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: mock.address().into(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: true,
            })
            .await
            .unwrap();
            let response = handler
                .handle_message(serde_json::json!({
                    "jsonrpc": "2.0", "id": 667, "method": "tools/call",
                    "params": {"name": "export_manufacturing_package", "arguments": {
                        "board": board, "output_dir": output, "fab_house": "jlcpcb"
                    }}
                }))
                .await
                .unwrap();
            let result = response.result.unwrap();
            assert_eq!(result["isError"], true, "{mode}: {result}");
            assert!(!output.exists(), "{mode}: refusal must not create output");
            assert_eq!(std::fs::read_to_string(&board).unwrap(), before);
        }
    }

    #[test]
    fn native_union_uses_board_boxes_and_integer_nanometres() {
        let boxes = [
            box_at(-1_000_000, -2_000_000, 2_000_000, 4_000_000),
            box_at(3_000_000, 6_000_000, 2_000_000, 2_000_000),
        ];
        assert_eq!(union_center(boxes.iter()).unwrap(), (2.0, 3.0));
        assert_eq!(
            union_center([box_at(-5, -3, 5, 5)].iter()).unwrap(),
            (-0.000003, -0.000001)
        );
        assert!(union_center([box_at(0, 0, -1, 1)].iter()).is_err());
        assert!(union_center([].iter()).is_err());
    }

    #[test]
    fn geometry_conversion_keeps_centred_values_and_refuses_missing_or_wrong_source() {
        let midpoint = Midpoint {
            designator: "J1".into(),
            anchor_x_mm: 15.0,
            anchor_y_mm: 6.0,
            midpoint_x_mm: 16.25,
            midpoint_y_mm: 6.0,
            algorithm: "kicad_native_board_pad_boxes_union_center",
        };
        let mut points = BTreeMap::from([("J1".into(), midpoint.clone())]);
        let source = "Ref,Val,Package,PosX,PosY,Rot,Side\nJ1,X,JST,15.000000,-6.000000,0,top\n";
        let (converted, evidence) = convert_positions(source, &points).unwrap();
        assert!(converted.contains("16.250000,-6.000000"));
        assert_eq!(evidence[0].anchor_x_mm, 15.0);
        points.get_mut("J1").unwrap().midpoint_x_mm = 15.0;
        assert_eq!(convert_positions(source, &points).unwrap().0, source);
        assert!(convert_positions(source, &BTreeMap::new()).is_err());
        assert!(convert_positions(&source.replace("15.000000", "14.000000"), &points).is_err());
    }

    #[test]
    fn native_box_ids_must_be_complete_unique_and_exact() {
        for mode in ["missing", "duplicate", "unexpected", "negative", "good"] {
            let mock = MockIpcServer::spawn("midpoint-boxes", move |request| {
                let command = request.message.unwrap();
                let requested =
                    kiapi::common::commands::GetBoundingBox::decode(command.value.as_slice())
                        .unwrap();
                assert_eq!(
                    requested
                        .items
                        .iter()
                        .map(|id| id.value.as_str())
                        .collect::<Vec<_>>(),
                    ["p1", "p2"]
                );
                let (ids, boxes) = match mode {
                    "missing" => (vec!["p1"], vec![box_at(0, 0, 1, 1)]),
                    "duplicate" => (vec!["p1", "p1"], vec![box_at(0, 0, 1, 1); 2]),
                    "unexpected" => (vec!["p1", "other"], vec![box_at(0, 0, 1, 1); 2]),
                    "negative" => (vec!["p1", "p2"], vec![box_at(0, 0, -1, 1); 2]),
                    _ => (
                        vec!["p2", "p1"],
                        vec![box_at(4, 0, 2, 2), box_at(0, 0, 2, 2)],
                    ),
                };
                kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    message: Some(pack_any(
                        &kiapi::common::commands::GetBoundingBoxResponse {
                            items: ids
                                .into_iter()
                                .map(|value| kiapi::common::types::Kiid {
                                    value: value.into(),
                                })
                                .collect(),
                            boxes,
                        },
                        "kiapi.common.commands.GetBoundingBoxResponse",
                    )),
                    ..Default::default()
                }
            });
            let client = KiCadIpcClient::new(mock.address());
            let result = client.get_item_boxes_in(Default::default(), &["p1".into(), "p2".into()]);
            assert_eq!(result.is_ok(), mode == "good", "{mode}: {result:?}");
        }
    }

    #[tokio::test]
    #[ignore = "requires the generated midpoint fixture open in KiCad IPC and kicad-cli"]
    async fn live_native_midpoint_acceptance() {
        let board = std::path::PathBuf::from(std::env::var("KONNECT_MIDPOINT_BOARD").unwrap());
        let client = KiCadIpcClient::new(konnect_ipc::detect_ipc_address().unwrap());
        let document = client.find_open_board(&board).unwrap();
        let snapshot = capture(&client, document.clone()).unwrap();
        let expected = [
            (20.0, 21.27),
            (31.27, 20.0),
            (40.0, 18.73),
            (48.73, 20.0),
            (60.721984, 20.721984),
        ];
        assert_eq!(snapshot.midpoints.len(), 11);
        for side in ["F", "B"] {
            for (i, &(x, y)) in expected.iter().enumerate() {
                let point = &snapshot.midpoints[&format!("J{side}{i}")];
                assert_eq!((point.midpoint_x_mm, point.midpoint_y_mm), (x, y));
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("midpoints.kicad_pcb");
        std::fs::write(&staged, &snapshot.contents).unwrap();
        let output = dir.path().join("CPL.csv");
        let cli = std::env::var("KICAD_CLI_PATH").unwrap_or_else(|_| "kicad-cli".into());
        let export = super::super::manufacturing::export_jlcpcb_cpl(
            &cli,
            &staged,
            &output,
            "both",
            None,
            &snapshot.midpoints,
        )
        .await
        .unwrap();
        let mut reader = csv::Reader::from_reader(export.bytes.as_slice());
        assert_eq!(reader.records().count(), 11);
        let mut reader = csv::Reader::from_reader(export.bytes.as_slice());
        for row in reader.records() {
            let row = row.unwrap();
            let reference = &row[0];
            if reference == "R1" {
                assert_eq!(&row[1], "80.000000");
                assert_eq!(&row[2], "-20.000000");
                continue;
            }
            let i: usize = reference[2..].parse().unwrap();
            assert_eq!(row[1].parse::<f64>().unwrap(), expected[i].0);
            assert_eq!(row[2].parse::<f64>().unwrap(), -expected[i].1);
            assert_eq!(
                &row[3],
                if reference.starts_with("JF") {
                    "top"
                } else {
                    "bottom"
                }
            );
        }
        let published = std::fs::read(&output).unwrap();
        let mut incomplete = snapshot.midpoints.clone();
        incomplete.remove("JF0");
        assert!(super::super::manufacturing::export_jlcpcb_cpl(
            &cli,
            &staged,
            &output,
            "both",
            None,
            &incomplete
        )
        .await
        .is_err());
        assert_eq!(
            std::fs::read(&output).unwrap(),
            published,
            "a failed conversion must preserve the previously published CPL"
        );
        // Capture real native protobuf fixtures for deterministic replay in CI.
        if let Ok(destination) = std::env::var("KONNECT_MIDPOINT_CAPTURE_DIR") {
            let destination = std::path::PathBuf::from(destination);
            std::fs::create_dir_all(&destination).unwrap();
            std::fs::write(destination.join("midpoints.kicad_pcb"), snapshot.contents).unwrap();
            let items = client
                .get_items_in(
                    document.clone(),
                    kiapi::common::types::KiCadObjectType::KotPcbFootprint,
                )
                .unwrap();
            let mut ids = Vec::new();
            for item in &items {
                let fp =
                    kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
                for item in fp.definition.unwrap().items {
                    if item.type_url.ends_with("kiapi.board.types.Pad") {
                        ids.push(
                            kiapi::board::types::Pad::decode(item.value.as_slice())
                                .unwrap()
                                .id
                                .unwrap()
                                .value,
                        );
                    }
                }
            }
            let boxes = client.get_item_boxes_in(document, &ids).unwrap();
            let response = kiapi::common::commands::GetBoundingBoxResponse {
                items: boxes
                    .keys()
                    .map(|value| kiapi::common::types::Kiid {
                        value: value.clone(),
                    })
                    .collect(),
                boxes: boxes.into_values().collect(),
            };
            std::fs::write(destination.join("boxes.pb"), response.encode_to_vec()).unwrap();
            let response = kiapi::common::commands::GetItemsResponse {
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                items,
                ..Default::default()
            };
            std::fs::write(destination.join("footprints.pb"), response.encode_to_vec()).unwrap();
        }
    }

    #[test]
    #[ignore = "requires disposable sole open KONNECT_MIDPOINT_BOARD and KONNECT_MIDPOINT_LIBRARY; KiCad IPC enabled"]
    fn live_native_midpoint_fixture() {
        let board = std::path::PathBuf::from(std::env::var("KONNECT_MIDPOINT_BOARD").unwrap());
        let library = std::path::PathBuf::from(std::env::var("KONNECT_MIDPOINT_LIBRARY").unwrap());
        let client = KiCadIpcClient::new(konnect_ipc::detect_ipc_address().unwrap());
        let document = client.find_open_board(&board).unwrap();
        assert_eq!(client.get_open_documents().unwrap(), vec![document.clone()]);
        let source = std::fs::read_to_string(library).unwrap();
        let pads = super::super::pcb_components::extract_pad_definitions(&source).unwrap();
        let graphics = super::super::pcb_components::extract_graphic_definitions(&source).unwrap();
        let fields = super::super::pcb_components::extract_field_placement(&source);
        let mut creates = Vec::new();
        for (i, angle) in [0.0, 90.0, 180.0, 270.0, 45.0].iter().enumerate() {
            for (side, layer) in [("F", "F.Cu"), ("B", "B.Cu")] {
                creates.push(
                    KiCadIpcClient::build_footprint_item(
                        "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
                        &format!("J{side}{i}"),
                        "header",
                        &pads,
                        &graphics,
                        &fields,
                        20.0 + i as f64 * 10.0,
                        20.0,
                        *angle,
                        layer,
                    )
                    .unwrap(),
                );
            }
        }
        let centered =
            std::fs::read_to_string(std::env::var("KONNECT_MIDPOINT_CENTERED_LIBRARY").unwrap())
                .unwrap();
        creates.push(
            KiCadIpcClient::build_footprint_item(
                "Resistor_SMD:R_0805_2012Metric",
                "R1",
                "10k",
                &super::super::pcb_components::extract_pad_definitions(&centered).unwrap(),
                &super::super::pcb_components::extract_graphic_definitions(&centered).unwrap(),
                &super::super::pcb_components::extract_field_placement(&centered),
                80.0,
                20.0,
                0.0,
                "F.Cu",
            )
            .unwrap(),
        );
        client
            .run_commit("midpoint acceptance fixture", |client| {
                client.create_items_in(document.clone(), creates)
            })
            .unwrap();
        let snapshot = capture(&client, document).unwrap();
        // Persist only to the explicitly disposable test file, not a user design.
        std::fs::write(&board, &snapshot.contents).unwrap();
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&snapshot.midpoints).unwrap()
        );
    }
}
