//! Issue #23 regression tests: footprint moves must carry the children along.
//!
//! KiCAD serializes a FootprintInstance's pads/graphics/fields in ABSOLUTE
//! board coordinates and re-creates them verbatim from an UpdateItems message.
//! These tests stand up a mock KiCAD (same NNG rep0 approach as
//! mock_server_test.rs) serving a footprint at (100,100) and assert that
//! move/rotate requests arrive with every child transformed, not just the
//! anchor.

use konnect_ipc::builders;
use konnect_ipc::gen::kiapi;
use konnect_ipc::KiCadIpcClient;
use nng::options::Options;
use prost::Message;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct MockKicad {
    url: String,
    _thread: std::thread::JoinHandle<()>,
}

fn spawn_mock<F>(respond: F) -> MockKicad
where
    F: Fn(kiapi::common::ApiRequest) -> Option<kiapi::common::ApiResponse> + Send + 'static,
{
    // inproc:// needs no port, so there is no bind-a-TcpListener-then-relisten
    // TOCTOU window (that pattern intermittently died with AddressInUse on CI
    // when another process grabbed the probed port). The name only has to be
    // unique within this test process; a counter suffices.
    static NEXT_MOCK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let url = format!(
        "inproc://mock-kicad-fp-{}",
        NEXT_MOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );

    // Listen BEFORE returning (and before the receive thread spawns): the
    // client dials the moment spawn_mock returns, and NNG's dial fails
    // immediately if nothing is bound yet. Doing the listen inside the
    // thread raced the caller — flaky on slow CI runners.
    let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock rep socket");
    socket
        .set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(20)))
        .unwrap();
    socket.listen(&url).expect("mock listen");

    let thread = std::thread::spawn(move || {
        while let Ok(msg) = socket.recv() {
            let request = match kiapi::common::ApiRequest::decode(msg.as_slice()) {
                Ok(r) => r,
                Err(_) => break,
            };
            match respond(request) {
                Some(resp) => {
                    let out = nng::Message::from(resp.encode_to_vec().as_slice());
                    if socket.send(out).is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    });

    MockKicad {
        url,
        _thread: thread,
    }
}

fn ok_response() -> kiapi::common::ApiResponse {
    kiapi::common::ApiResponse {
        status: Some(kiapi::common::ApiResponseStatus {
            status: kiapi::common::ApiStatusCode::AsOk as i32,
            error_message: String::new(),
        }),
        header: None,
        message: None,
    }
}

fn reply_with(inner: prost_types::Any) -> kiapi::common::ApiResponse {
    kiapi::common::ApiResponse {
        message: Some(inner),
        ..ok_response()
    }
}

fn mk_field(name: &str, text: &str, x_mm: f64, y_mm: f64) -> kiapi::board::types::Field {
    kiapi::board::types::Field {
        name: name.to_string(),
        text: Some(kiapi::board::types::BoardText {
            text: Some(kiapi::common::types::Text {
                text: text.to_string(),
                position: Some(builders::vec2(x_mm, y_mm)),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn mk_pad(x_mm: f64, y_mm: f64) -> prost_types::Any {
    builders::pack_any(
        &kiapi::board::types::Pad {
            position: Some(builders::vec2(x_mm, y_mm)),
            pad_stack: Some(kiapi::board::types::PadStack {
                angle: Some(kiapi::common::types::Angle { value_degrees: 0.0 }),
                ..Default::default()
            }),
            ..Default::default()
        },
        "kiapi.board.types.Pad",
    )
}

/// An R1 footprint anchored at (100,100) with two pads, a silk segment, and
/// a reference field, all in absolute board coordinates like KiCAD sends.
fn mk_footprint_r1() -> kiapi::board::types::FootprintInstance {
    kiapi::board::types::FootprintInstance {
        position: Some(builders::vec2(100.0, 100.0)),
        orientation: Some(kiapi::common::types::Angle { value_degrees: 0.0 }),
        reference_field: Some(mk_field("Reference", "R1", 100.0, 98.0)),
        definition: Some(kiapi::board::types::Footprint {
            items: vec![
                mk_pad(99.0, 100.0),
                mk_pad(101.0, 100.0),
                builders::pack_any(
                    &builders::board_segment("F.SilkS", 0.12, 99.5, 99.0, 100.5, 99.0),
                    "kiapi.board.types.BoardGraphicShape",
                ),
            ],
            ..Default::default()
        }),
        ..Default::default()
    }
}

type CapturedUpdate = Arc<Mutex<Option<kiapi::common::commands::UpdateItems>>>;

/// Mock KiCAD serving `fp` for GetItems and recording the UpdateItems it
/// receives.
fn spawn_footprint_mock(fp: kiapi::board::types::FootprintInstance) -> (MockKicad, CapturedUpdate) {
    spawn_footprints_mock(vec![fp])
}

fn spawn_footprints_mock(
    footprints: Vec<kiapi::board::types::FootprintInstance>,
) -> (MockKicad, CapturedUpdate) {
    let captured: CapturedUpdate = Arc::new(Mutex::new(None));
    let captured_in_mock = captured.clone();
    let current = Arc::new(Mutex::new(footprints));
    let current_in_mock = current.clone();

    let mock = spawn_mock(move |req| {
        let msg = req.message.expect("request must pack a command");
        if msg.type_url.ends_with("GetOpenDocuments") {
            let resp = kiapi::common::commands::GetOpenDocumentsResponse {
                documents: vec![kiapi::common::types::DocumentSpecifier {
                    r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                    project: Some(kiapi::common::types::ProjectSpecifier {
                        name: "konnect-mock".to_string(),
                        path: mock_project_dir().to_string(),
                    }),
                    identifier: Some(
                        kiapi::common::types::document_specifier::Identifier::BoardFilename(
                            "test.kicad_pcb".to_string(),
                        ),
                    ),
                }],
            };
            Some(reply_with(builders::pack_any(
                &resp,
                "kiapi.common.commands.GetOpenDocumentsResponse",
            )))
        } else if msg.type_url.ends_with("GetItems") {
            let resp = kiapi::common::commands::GetItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                items: current_in_mock
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|footprint| {
                        builders::pack_any(footprint, "kiapi.board.types.FootprintInstance")
                    })
                    .collect(),
            };
            Some(reply_with(builders::pack_any(
                &resp,
                "kiapi.common.commands.GetItemsResponse",
            )))
        } else if msg.type_url.ends_with("BeginCommit") {
            Some(reply_with(builders::pack_any(
                &kiapi::common::commands::BeginCommitResponse {
                    id: Some(kiapi::common::types::Kiid {
                        value: "placement-commit".to_string(),
                    }),
                },
                "kiapi.common.commands.BeginCommitResponse",
            )))
        } else if msg.type_url.ends_with("EndCommit") {
            Some(reply_with(builders::pack_any(
                &kiapi::common::commands::EndCommitResponse {},
                "kiapi.common.commands.EndCommitResponse",
            )))
        } else if msg.type_url.ends_with("UpdateItems") {
            let update =
                kiapi::common::commands::UpdateItems::decode(msg.value.as_slice()).unwrap();
            let updated_items = update
                .items
                .iter()
                .cloned()
                .map(|item| kiapi::common::commands::ItemUpdateResult {
                    status: Some(kiapi::common::commands::ItemStatus {
                        code: kiapi::common::commands::ItemStatusCode::IscOk as i32,
                        error_message: String::new(),
                    }),
                    item: Some(item),
                })
                .collect();
            {
                // Keep the mock stateful: a later GetItems must observe what
                // UpdateItems wrote (the pad-readback test relies on it), and
                // a batch update replaces every matching footprint.
                let mut held = current_in_mock.lock().unwrap();
                for item in &update.items {
                    let incoming =
                        kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
                            .unwrap();
                    let reference = mock_reference(&incoming);
                    if let Some(slot) = held
                        .iter_mut()
                        .find(|existing| mock_reference(existing) == reference)
                    {
                        *slot = incoming;
                    }
                }
            }
            *captured_in_mock.lock().unwrap() = Some(update);
            Some(reply_with(builders::pack_any(
                &kiapi::common::commands::UpdateItemsResponse {
                    header: None,
                    status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                    updated_items,
                },
                "kiapi.common.commands.UpdateItemsResponse",
            )))
        } else {
            Some(ok_response())
        }
    });

    (mock, captured)
}

fn mock_reference(fp: &kiapi::board::types::FootprintInstance) -> String {
    fp.reference_field
        .as_ref()
        .and_then(|field| field.text.as_ref())
        .and_then(|board_text| board_text.text.as_ref())
        .map(|text| text.text.clone())
        .unwrap_or_default()
}

fn pad_positions_mm(fp: &kiapi::board::types::FootprintInstance) -> Vec<(f64, f64)> {
    fp.definition
        .as_ref()
        .unwrap()
        .items
        .iter()
        .filter(|i| i.type_url.ends_with("kiapi.board.types.Pad"))
        .map(|i| {
            let pad = kiapi::board::types::Pad::decode(i.value.as_slice()).unwrap();
            let p = pad.position.unwrap();
            (builders::nm_to_mm(p.x_nm), builders::nm_to_mm(p.y_nm))
        })
        .collect()
}

#[test]
fn move_footprint_translates_pads_graphics_and_fields() {
    let (mock, captured) = spawn_footprint_mock(mk_footprint_r1());
    let client = KiCadIpcClient::new(&mock.url);

    client.move_footprint("R1", 50.0, 50.0).unwrap();

    let update = captured.lock().unwrap().take().expect("UpdateItems sent");
    assert_eq!(update.items.len(), 1);
    let sent =
        kiapi::board::types::FootprintInstance::decode(update.items[0].value.as_slice()).unwrap();

    // Anchor moved.
    let pos = sent.position.unwrap();
    assert_eq!(builders::nm_to_mm(pos.x_nm), 50.0);
    assert_eq!(builders::nm_to_mm(pos.y_nm), 50.0);

    // The pads moved WITH it: this is the issue #23 regression check.
    assert_eq!(pad_positions_mm(&sent), vec![(49.0, 50.0), (51.0, 50.0)]);

    // Silkscreen segment translated too.
    let silk = sent
        .definition
        .as_ref()
        .unwrap()
        .items
        .iter()
        .find(|i| i.type_url.ends_with("BoardGraphicShape"))
        .unwrap();
    let shape = kiapi::board::types::BoardGraphicShape::decode(silk.value.as_slice()).unwrap();
    match shape.shape.unwrap().geometry.unwrap() {
        kiapi::common::types::graphic_shape::Geometry::Segment(s) => {
            assert_eq!(builders::nm_to_mm(s.start.unwrap().x_nm), 49.5);
            assert_eq!(builders::nm_to_mm(s.start.unwrap().y_nm), 49.0);
            assert_eq!(builders::nm_to_mm(s.end.unwrap().x_nm), 50.5);
        }
        other => panic!("expected segment, got {other:?}"),
    }

    // Reference text follows the footprint.
    let ref_pos = sent
        .reference_field
        .unwrap()
        .text
        .unwrap()
        .text
        .unwrap()
        .position
        .unwrap();
    assert_eq!(builders::nm_to_mm(ref_pos.x_nm), 50.0);
    assert_eq!(builders::nm_to_mm(ref_pos.y_nm), 48.0);
}

#[test]
fn rotate_footprint_rotates_children_around_anchor() {
    let (mock, captured) = spawn_footprint_mock(mk_footprint_r1());
    let client = KiCadIpcClient::new(&mock.url);

    client.rotate_footprint("R1", 90.0).unwrap();

    let update = captured.lock().unwrap().take().expect("UpdateItems sent");
    let sent =
        kiapi::board::types::FootprintInstance::decode(update.items[0].value.as_slice()).unwrap();

    assert_eq!(sent.orientation.unwrap().value_degrees, 90.0);

    // KiCAD-positive rotation is counterclockwise on screen (Y axis down):
    // pad at (99,100) rotates to (100,101); pad at (101,100) to (100,99).
    assert_eq!(pad_positions_mm(&sent), vec![(100.0, 101.0), (100.0, 99.0)]);

    // Pad orientations pick up the rotation delta.
    let pad = kiapi::board::types::Pad::decode(
        sent.definition.as_ref().unwrap().items[0].value.as_slice(),
    )
    .unwrap();
    assert_eq!(pad.pad_stack.unwrap().angle.unwrap().value_degrees, 90.0);
}

#[test]
fn footprint_pad_readback_observes_the_updated_live_state_after_a_move() {
    let (mock, _) = spawn_footprint_mock(mk_footprint_r1());
    let client = KiCadIpcClient::new(&mock.url);

    client.move_footprint("R1", 50.0, 50.0).unwrap();
    let document = client
        .find_open_board(&std::path::PathBuf::from(mock_project_dir()).join("test.kicad_pcb"))
        .expect("the mock holds test.kicad_pcb");
    let pads = client
        .get_footprint_pads_in(document, "R1")
        .expect("pad read")
        .expect("R1 is on the board");

    assert_eq!(
        pads.iter().map(|pad| (pad.x, pad.y)).collect::<Vec<_>>(),
        vec![(49.0, 50.0), (51.0, 50.0)]
    );
}

#[test]
fn placement_batch_moves_and_rotates_multiple_footprints_in_one_update() {
    let r1 = mk_footprint_r1();
    let mut r2 = mk_footprint_r1();
    konnect_ipc::transform::transform_footprint_children(
        &mut r2,
        &konnect_ipc::transform::Xform::Translate {
            dx_nm: 100_000_000,
            dy_nm: 0,
        },
    )
    .unwrap();
    r2.position = Some(builders::vec2(200.0, 100.0));
    r2.reference_field
        .as_mut()
        .unwrap()
        .text
        .as_mut()
        .unwrap()
        .text
        .as_mut()
        .unwrap()
        .text = "R2".to_string();

    let (mock, captured) = spawn_footprints_mock(vec![r1, r2]);
    let client = KiCadIpcClient::new(&mock.url);
    client
        .set_footprint_placements(&[
            konnect_ipc::types::IpcFootprintPlacement {
                reference: "R1".to_string(),
                x: 50.0,
                y: 50.0,
                rotation: 90.0,
            },
            konnect_ipc::types::IpcFootprintPlacement {
                reference: "R2".to_string(),
                x: 250.0,
                y: 150.0,
                rotation: 180.0,
            },
        ])
        .unwrap();

    let update = captured.lock().unwrap().take().expect("UpdateItems sent");
    assert_eq!(
        update.items.len(),
        2,
        "one request must carry both footprints"
    );
    let sent: Vec<_> = update
        .items
        .iter()
        .map(|item| kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap())
        .collect();
    let placements: Vec<_> = sent
        .iter()
        .map(|footprint| {
            let position = footprint.position.unwrap();
            (
                builders::nm_to_mm(position.x_nm),
                builders::nm_to_mm(position.y_nm),
                footprint.orientation.as_ref().unwrap().value_degrees,
            )
        })
        .collect();
    assert_eq!(placements, vec![(50.0, 50.0, 90.0), (250.0, 150.0, 180.0)]);
    assert_eq!(pad_positions_mm(&sent[0]), vec![(50.0, 51.0), (50.0, 49.0)]);
}

/// Absolute on the platform running the test — a POSIX-rooted path is not
/// absolute on Windows, and only an absolute project directory can place the
/// bare board filename KiCad sends.
fn mock_project_dir() -> &'static str {
    if cfg!(windows) {
        r"C:\konnect-mock-project"
    } else {
        "/konnect-mock-project"
    }
}
