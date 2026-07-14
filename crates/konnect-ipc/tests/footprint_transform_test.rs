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
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("tcp://127.0.0.1:{port}");

    let listen_url = url.clone();
    let thread = std::thread::spawn(move || {
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock rep socket");
        socket
            .set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(20)))
            .unwrap();
        socket.listen(&listen_url).expect("mock listen");
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
    let captured: CapturedUpdate = Arc::new(Mutex::new(None));
    let captured_in_mock = captured.clone();

    let mock = spawn_mock(move |req| {
        let msg = req.message.expect("request must pack a command");
        if msg.type_url.ends_with("GetOpenDocuments") {
            let resp = kiapi::common::commands::GetOpenDocumentsResponse {
                documents: vec![kiapi::common::types::DocumentSpecifier {
                    r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                    project: None,
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
                items: vec![builders::pack_any(
                    &fp,
                    "kiapi.board.types.FootprintInstance",
                )],
            };
            Some(reply_with(builders::pack_any(
                &resp,
                "kiapi.common.commands.GetItemsResponse",
            )))
        } else if msg.type_url.ends_with("UpdateItems") {
            let update =
                kiapi::common::commands::UpdateItems::decode(msg.value.as_slice()).unwrap();
            *captured_in_mock.lock().unwrap() = Some(update);
            Some(ok_response())
        } else {
            Some(ok_response())
        }
    });

    (mock, captured)
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
