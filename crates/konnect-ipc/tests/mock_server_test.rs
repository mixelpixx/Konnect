//! IPC client tests against a mock KiCAD NNG server — no KiCAD required.
//!
//! A rep0 socket on inproc://<unique-name> plays KiCAD: it decodes the
//! ApiRequest envelope and returns canned ApiResponse messages. This lets CI
//! exercise the full encode → transport → decode → error-mapping path that
//! previously only ran against a live KiCAD session.

use konnect_ipc::builders;
use konnect_ipc::gen::kiapi;
use konnect_ipc::KiCadIpcClient;
use nng::options::Options;
use prost::Message;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A rep0 server answering each request via `respond`.
/// Returns the inproc:// URL to dial. The server thread exits when the socket
/// errors (i.e. when `_socket_keepalive` is dropped by the returned guard).
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
        "inproc://mock-kicad-{}",
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
                None => {
                    // Simulate a wedged KiCAD: never reply. The rep socket
                    // can't take another request until it replies, so just
                    // park until the test ends.
                    std::thread::sleep(Duration::from_secs(20));
                    break;
                }
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

fn open_board_response() -> kiapi::common::ApiResponse {
    let response = kiapi::common::commands::GetOpenDocumentsResponse {
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
    reply_with(builders::pack_any(
        &response,
        "kiapi.common.commands.GetOpenDocumentsResponse",
    ))
}

#[test]
fn save_document_to_string_targets_the_named_open_board() {
    let mock = spawn_mock(|request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            return Some(open_board_response());
        }
        if message.type_url.ends_with("SaveDocumentToString") {
            let command =
                kiapi::common::commands::SaveDocumentToString::decode(message.value.as_slice())
                    .expect("decode SaveDocumentToString");
            let document = command.document.expect("target document");
            assert_eq!(
                document.identifier,
                Some(
                    kiapi::common::types::document_specifier::Identifier::BoardFilename(
                        "test.kicad_pcb".to_string()
                    )
                )
            );
            let response = kiapi::common::commands::SavedDocumentResponse {
                document: Some(document),
                contents: "(kicad_pcb (version 20260206))".to_string(),
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.SavedDocumentResponse",
            )));
        }
        panic!("unexpected command {}", message.type_url);
    });

    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("test.kicad_pcb"))
        .expect("the mock holds test.kicad_pcb");
    let snapshot = client
        .save_document_to_string_in(document)
        .expect("live board snapshot");
    assert_eq!(snapshot, "(kicad_pcb (version 20260206))");
}

#[test]
fn effective_routing_rules_preserve_complete_kicad_values() {
    let mock = spawn_mock(|request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            return Some(open_board_response());
        }
        if message.type_url.ends_with("GetNets") {
            let response = kiapi::board::commands::NetsResponse {
                nets: vec![kiapi::board::types::Net {
                    code: Some(kiapi::board::types::NetCode { value: 7 }),
                    name: "GND".to_string(),
                }],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.board.commands.NetsResponse",
            )));
        }
        if message.type_url.ends_with("GetNetClassForNets") {
            let command =
                kiapi::board::commands::GetNetClassForNets::decode(message.value.as_slice())
                    .expect("decode GetNetClassForNets");
            assert_eq!(command.net.len(), 1);
            assert_eq!(command.net[0].name, "GND");
            assert_eq!(command.net[0].code.as_ref().map(|code| code.value), Some(7));

            let via_stack = kiapi::board::types::PadStack {
                drill: Some(kiapi::board::types::DrillProperties {
                    diameter: Some(kiapi::common::types::Vector2 {
                        x_nm: 600_000,
                        y_nm: 600_000,
                    }),
                    ..Default::default()
                }),
                copper_layers: vec![kiapi::board::types::PadStackLayer {
                    size: Some(kiapi::common::types::Vector2 {
                        x_nm: 1_200_000,
                        y_nm: 1_200_000,
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let class = kiapi::common::project::NetClass {
                name: "Default".to_string(),
                board: Some(kiapi::common::project::NetClassBoardSettings {
                    clearance: Some(kiapi::common::types::Distance { value_nm: 200_000 }),
                    track_width: Some(kiapi::common::types::Distance { value_nm: 250_000 }),
                    via_stack: Some(via_stack),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let response = kiapi::board::commands::NetClassForNetsResponse {
                classes: [("GND".to_string(), class)].into_iter().collect(),
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.board.commands.NetClassForNetsResponse",
            )));
        }
        panic!("unexpected command {}", message.type_url);
    });

    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("test.kicad_pcb"))
        .expect("the mock holds test.kicad_pcb");
    let rules = client
        .get_effective_routing_rules_in(document)
        .expect("effective rules");
    let gnd = rules.get("GND").expect("GND rules");
    assert_eq!(gnd.class_name, "Default");
    assert_eq!(gnd.track_width_mm, Some(0.25));
    assert_eq!(gnd.clearance_mm, Some(0.2));
    assert_eq!(gnd.via_diameter_mm, Some(1.2));
    assert_eq!(gnd.via_drill_mm, Some(0.6));
}

#[test]
fn ping_roundtrips_through_mock() {
    let mock = spawn_mock(|req| {
        // The envelope must carry a client name and a packed command.
        assert!(req.header.is_some());
        let header = req.header.unwrap();
        assert!(header.client_name.starts_with("konnect-"));
        let msg = req.message.expect("request must pack a command");
        assert!(
            msg.type_url.ends_with("kiapi.common.commands.Ping"),
            "unexpected type_url: {}",
            msg.type_url
        );
        Some(ok_response())
    });

    let client = KiCadIpcClient::new(&mock.url);
    assert!(client.ping().unwrap());
}

#[test]
fn explicit_kicad_token_is_sent_in_request_header() {
    let mock = spawn_mock(|req| {
        let header = req.header.expect("request header");
        assert_eq!(header.kicad_token, "linux-instance-token");
        Some(ok_response())
    });

    let client = KiCadIpcClient::new_with_token(&mock.url, "linux-instance-token");
    assert!(client.ping().unwrap());
}

#[test]
fn kicad_error_status_maps_to_err() {
    let mock = spawn_mock(|_req| {
        Some(kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsBadRequest as i32,
                error_message: "no board open".to_string(),
            }),
            header: None,
            message: None,
        })
    });

    let client = KiCadIpcClient::new(&mock.url);
    // ping() swallows errors into Ok(false) by design — that's the
    // "KiCAD unreachable" UX. It must not be Ok(true) and must not hang.
    assert!(!client.ping().unwrap());

    // A typed call surfaces the error text.
    let err = client.get_open_documents().unwrap_err().to_string();
    assert!(err.contains("no board open"), "unexpected error: {err}");
}

#[test]
fn unreachable_endpoint_errors_fast() {
    // Nothing listens here; dial must fail with an error, not hang.
    let client = KiCadIpcClient::new("tcp://127.0.0.1:1");
    let start = std::time::Instant::now();
    let result = client.get_open_documents();
    assert!(result.is_err());
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "dial to dead endpoint took {:?}",
        start.elapsed()
    );
}

#[test]
fn empty_socket_path_is_configuration_error() {
    // Clear KICAD_API_SOCKET influence by passing explicit empty and hoping
    // the env var isn't set in CI; if it is, skip.
    if std::env::var("KICAD_API_SOCKET").is_ok() {
        eprintln!("SKIP: KICAD_API_SOCKET set in environment");
        return;
    }
    let client = KiCadIpcClient::new("");
    let err = client.get_open_documents().unwrap_err().to_string();
    assert!(
        err.contains("socket path not configured"),
        "unexpected error: {err}"
    );
    assert!(
        err.contains("TROUBLESHOOTING"),
        "error should link the troubleshooting guide: {err}"
    );
}

// ─── CreateItems outcome handling ─────────────────────────────────────────────
//
// Ported from emolitor's PR #66 (which exercised these against the
// ParseAndCreateItemsFromString path) and adapted to the typed create_items
// API: the per-item accounting bugs they guard are identical.

/// A mock KiCAD with one board open that answers every CreateItems with
/// `results`.
fn spawn_mock_creating(results: Vec<kiapi::common::commands::ItemCreationResult>) -> MockKicad {
    spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            Some(open_board_response())
        } else {
            assert!(message.type_url.ends_with("CreateItems"));
            let response = kiapi::common::commands::CreateItemsResponse {
                header: None,
                // IRS_OK even though nothing may have been created — the
                // response shape the proto explicitly warns about.
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                created_items: results.clone(),
            };
            Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.CreateItemsResponse",
            )))
        }
    })
}

fn creation_result(
    code: kiapi::common::commands::ItemStatusCode,
    message: &str,
) -> kiapi::common::commands::ItemCreationResult {
    kiapi::common::commands::ItemCreationResult {
        status: Some(kiapi::common::commands::ItemStatus {
            code: code as i32,
            error_message: message.to_string(),
        }),
        item: None,
    }
}

fn any_item() -> prost_types::Any {
    builders::pack_any(&kiapi::common::commands::Ping {}, "test.Item")
}

#[test]
fn a_rejected_item_is_not_counted_as_created() {
    // The regression this guards: created_items is documented as "status of
    // each item TO BE created", so a rejection still occupies a slot. Counting
    // the vector's length would call this a success and put the phantom back.
    let mock = spawn_mock_creating(vec![creation_result(
        kiapi::common::commands::ItemStatusCode::IscInvalidData,
        "footprint has no pads",
    )]);

    let client = KiCadIpcClient::new(&mock.url);
    let err = client
        .create_items(vec![any_item()])
        .unwrap_err()
        .to_string();

    assert!(err.contains("created 0 of 1"), "must report failure: {err}");
    // The per-item reason is what makes this diagnosable.
    assert!(
        err.contains("ISC_INVALID_DATA") && err.contains("footprint has no pads"),
        "must surface KiCAD's own reason: {err}"
    );
}

#[test]
fn an_empty_result_list_still_reports_failure() {
    // KiCAD 10.0's actual behaviour: an empty CreateItemsResponse with IRS_OK.
    let mock = spawn_mock_creating(vec![]);
    let client = KiCadIpcClient::new(&mock.url);
    let err = client
        .create_items(vec![any_item()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("created no items"), "unexpected: {err}");
    assert!(err.contains("no items at all"), "unexpected: {err}");
}

#[test]
fn a_created_item_counts() {
    let mock = spawn_mock_creating(vec![creation_result(
        kiapi::common::commands::ItemStatusCode::IscOk,
        "",
    )]);
    let client = KiCadIpcClient::new(&mock.url);
    client
        .create_items(vec![any_item()])
        .expect("an ISC_OK result is a created item");
}

#[test]
fn a_defaulted_status_counts_only_when_an_item_came_back() {
    // Protobuf cannot distinguish "unset" from "explicitly zero", so an
    // ISC_UNKNOWN status is only evidence of success if an item is attached.
    let with_item = kiapi::common::commands::ItemCreationResult {
        status: None,
        item: Some(prost_types::Any::default()),
    };
    let mock = spawn_mock_creating(vec![with_item]);
    let client = KiCadIpcClient::new(&mock.url);
    client
        .create_items(vec![any_item()])
        .expect("an item with a defaulted status was still created");

    // The same defaulted status with nothing attached created nothing.
    let without_item = kiapi::common::commands::ItemCreationResult {
        status: Some(kiapi::common::commands::ItemStatus {
            code: kiapi::common::commands::ItemStatusCode::IscUnknown as i32,
            error_message: String::new(),
        }),
        item: None,
    };
    let mock = spawn_mock_creating(vec![without_item]);
    let client = KiCadIpcClient::new(&mock.url);
    assert!(
        client.create_items(vec![any_item()]).is_err(),
        "a bare ISC_UNKNOWN with no item must not count as created"
    );
}

#[test]
fn a_mixed_response_counts_only_the_successes() {
    let mock = spawn_mock_creating(vec![
        creation_result(kiapi::common::commands::ItemStatusCode::IscOk, ""),
        creation_result(
            kiapi::common::commands::ItemStatusCode::IscInvalidData,
            "bad",
        ),
    ]);
    let client = KiCadIpcClient::new(&mock.url);
    let err = client
        .create_items(vec![any_item(), any_item()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("created 1 of 2"), "unexpected: {err}");
    assert!(
        err.contains("item 1") && err.contains("ISC_INVALID_DATA") && err.contains("bad"),
        "the rejected slot must be identified: {err}"
    );
}

/// End-to-end placement through the mock: the FootprintInstance sent over the
/// wire must carry the footprint's graphics (courtyard/silk/fab) as children
/// alongside its pads — a pads-only instance trips lib_footprint_mismatch and
/// makes courtyard DRC meaningless.
#[test]
fn place_footprint_sends_graphics_children() {
    use konnect_ipc::{IpcGraphicDefinition, IpcPadDefinition};

    let captured: Arc<Mutex<Option<kiapi::common::commands::CreateItems>>> =
        Arc::new(Mutex::new(None));
    let captured_in_mock = captured.clone();
    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            Some(open_board_response())
        } else if message.type_url.ends_with("GetItems") {
            // Before creation the board is empty; afterwards it holds exactly
            // what CreateItems carried, so the client's verification pass sees
            // its own footprint.
            let items = captured_in_mock
                .lock()
                .unwrap()
                .as_ref()
                .map(|create| create.items.clone())
                .unwrap_or_default();
            Some(reply_with(builders::pack_any(
                &kiapi::common::commands::GetItemsResponse {
                    header: None,
                    status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                    items,
                },
                "kiapi.common.commands.GetItemsResponse",
            )))
        } else {
            assert!(message.type_url.ends_with("CreateItems"));
            let create =
                kiapi::common::commands::CreateItems::decode(message.value.as_slice()).unwrap();
            let created_items = create
                .items
                .iter()
                .cloned()
                .map(|item| kiapi::common::commands::ItemCreationResult {
                    status: Some(kiapi::common::commands::ItemStatus {
                        code: kiapi::common::commands::ItemStatusCode::IscOk as i32,
                        error_message: String::new(),
                    }),
                    item: Some(item),
                })
                .collect();
            *captured_in_mock.lock().unwrap() = Some(create);
            Some(reply_with(builders::pack_any(
                &kiapi::common::commands::CreateItemsResponse {
                    header: None,
                    status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                    created_items,
                },
                "kiapi.common.commands.CreateItemsResponse",
            )))
        }
    });

    let pads = vec![IpcPadDefinition {
        number: "1".to_string(),
        pad_type: "smd".to_string(),
        shape: "roundrect".to_string(),
        x: -0.5,
        y: 0.0,
        rotation: 0.0,
        size_x: 0.5,
        size_y: 0.5,
        drill_x: None,
        drill_y: None,
        drill_oval: false,
        layers: vec!["F.Cu".to_string()],
        roundrect_ratio: 0.25,
    }];
    let graphics = vec![
        IpcGraphicDefinition::Rect {
            start: (-0.8, -0.7),
            end: (0.8, 0.7),
            layer: "F.CrtYd".to_string(),
            width: 0.05,
            filled: false,
        },
        IpcGraphicDefinition::Line {
            start: (-0.6, -0.5),
            end: (0.6, -0.5),
            layer: "F.SilkS".to_string(),
            width: 0.12,
        },
        IpcGraphicDefinition::Text {
            text: "R_0402".to_string(),
            position: (0.0, 1.17),
            rotation: 0.0,
            layer: "F.Fab".to_string(),
            size: 0.26,
            stroke_width_mm: 0.04,
        },
    ];

    let client = KiCadIpcClient::new(&mock.url);
    let placed = client
        .place_footprint(
            std::path::Path::new("test.kicad_pcb"),
            "Resistor_SMD:R_0402",
            "R1",
            "R_0402",
            &pads,
            &graphics,
            &konnect_ipc::IpcFieldPlacement::default(),
            10.0,
            20.0,
            0.0,
            "F.Cu",
        )
        .expect("placement through the mock should succeed");
    assert_eq!(placed.reference, "R1");

    let create = captured.lock().unwrap().take().expect("CreateItems sent");
    assert_eq!(create.items.len(), 1);
    let footprint =
        kiapi::board::types::FootprintInstance::decode(create.items[0].value.as_slice())
            .expect("sent item must be a FootprintInstance");
    let children = footprint.definition.expect("definition").items;
    let pads_sent = children
        .iter()
        .filter(|any| any.type_url.ends_with("kiapi.board.types.Pad"))
        .count();
    let shapes_sent = children
        .iter()
        .filter(|any| any.type_url.ends_with("BoardGraphicShape"))
        .count();
    let texts_sent = children
        .iter()
        .filter(|any| any.type_url.ends_with("BoardText"))
        .count();
    assert_eq!(pads_sent, 1, "pad child missing");
    assert_eq!(shapes_sent, 2, "courtyard rect + silk line must be sent");
    assert_eq!(texts_sent, 1, "fab text must be sent");
}

#[test]
fn create_items_requires_a_typed_response_payload() {
    let mock = spawn_mock(|request| {
        let message = request.message.expect("request message");
        if message.type_url.ends_with("GetOpenDocuments") {
            Some(open_board_response())
        } else {
            assert!(message.type_url.ends_with("CreateItems"));
            Some(ok_response())
        }
    });
    let client = KiCadIpcClient::new(&mock.url);
    let item = builders::pack_any(&kiapi::common::commands::Ping {}, "test.Item");

    let error = client.create_items(vec![item]).unwrap_err().to_string();

    assert!(error.contains("no CreateItems response payload"), "{error}");
}

#[test]
fn update_items_rejects_missing_per_item_results() {
    let mock = spawn_mock(|request| {
        let message = request.message.expect("request message");
        if message.type_url.ends_with("GetOpenDocuments") {
            Some(open_board_response())
        } else {
            assert!(message.type_url.ends_with("UpdateItems"));
            let response = kiapi::common::commands::UpdateItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                updated_items: vec![],
            };
            Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.UpdateItemsResponse",
            )))
        }
    });
    let client = KiCadIpcClient::new(&mock.url);
    let item = builders::pack_any(&kiapi::common::commands::Ping {}, "test.Item");

    let error = client.update_items(vec![item]).unwrap_err().to_string();

    assert!(
        error.contains("0 update results for 1 requested"),
        "{error}"
    );
}

#[test]
fn delete_items_surfaces_per_item_failure() {
    let mock = spawn_mock(|request| {
        let message = request.message.expect("request message");
        if message.type_url.ends_with("GetOpenDocuments") {
            Some(open_board_response())
        } else {
            assert!(message.type_url.ends_with("DeleteItems"));
            let response = kiapi::common::commands::DeleteItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                deleted_items: vec![kiapi::common::commands::ItemDeletionResult {
                    id: Some(kiapi::common::types::Kiid {
                        value: "missing-id".to_string(),
                    }),
                    status: kiapi::common::commands::ItemDeletionStatus::IdsNonexistent as i32,
                }],
            };
            Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.DeleteItemsResponse",
            )))
        }
    });
    let client = KiCadIpcClient::new(&mock.url);

    let error = client
        .delete_items(vec!["missing-id".to_string()])
        .unwrap_err()
        .to_string();

    assert!(error.contains("IDS_NONEXISTENT"), "{error}");
    assert!(error.contains("missing-id"), "{error}");
}

/// KiCad 10 builds per-item deletion results and never attaches them, so a
/// successful delete comes back with an empty `deleted_items`. Treating that
/// as failure is what made delete_component report "0 deletion results" for
/// deletions that had actually happened (#116).
#[test]
fn an_empty_deletion_result_list_is_not_a_failure() {
    let mock = spawn_mock(|request| {
        let message = request.message.expect("request message");
        if message.type_url.ends_with("GetOpenDocuments") {
            Some(open_board_response())
        } else {
            assert!(message.type_url.ends_with("DeleteItems"));
            let response = kiapi::common::commands::DeleteItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                deleted_items: vec![],
            };
            Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.DeleteItemsResponse",
            )))
        }
    });
    let client = KiCadIpcClient::new(&mock.url);

    client
        .delete_items(vec!["some-id".to_string()])
        .expect("an empty result list means KiCad said nothing, not that it failed");
}

#[test]
fn failed_multi_step_commit_is_dropped() {
    let actions = Arc::new(Mutex::new(Vec::new()));
    let captured_actions = actions.clone();
    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request message");
        if message.type_url.ends_with("BeginCommit") {
            let response = kiapi::common::commands::BeginCommitResponse {
                id: Some(kiapi::common::types::Kiid {
                    value: "commit-1".to_string(),
                }),
            };
            Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.BeginCommitResponse",
            )))
        } else {
            assert!(message.type_url.ends_with("EndCommit"));
            let command =
                kiapi::common::commands::EndCommit::decode(message.value.as_slice()).unwrap();
            captured_actions.lock().unwrap().push(command.action());
            Some(reply_with(builders::pack_any(
                &kiapi::common::commands::EndCommitResponse {},
                "kiapi.common.commands.EndCommitResponse",
            )))
        }
    });
    let client = KiCadIpcClient::new(&mock.url);

    let error = client
        .run_commit::<()>("test transaction", |_| anyhow::bail!("second step failed"))
        .unwrap_err()
        .to_string();

    assert!(error.contains("changes dropped"), "{error}");
    assert_eq!(
        *actions.lock().unwrap(),
        vec![kiapi::common::commands::CommitAction::CmaDrop]
    );
}

// ─── IpcFailure classification ────────────────────────────────────────────────
//
// The file-editing fallback in konnect-core is gated on this classification:
// only a transport that never delivered the request may fall back, and the
// decision must come from the typed marker, never from matching error text
// (Copilot flagged substring matching three times on PR #66).

#[test]
fn an_unconfigured_socket_classifies_as_unreachable() {
    if std::env::var("KICAD_API_SOCKET").is_ok() {
        eprintln!("SKIP: KICAD_API_SOCKET set in environment");
        return;
    }
    let client = KiCadIpcClient::new("");
    let failure = konnect_ipc::IpcFailure::from_error(client.get_open_documents().unwrap_err());
    assert!(
        matches!(failure, konnect_ipc::IpcFailure::Unreachable(_)),
        "unexpected classification: {failure:?}"
    );
}

#[test]
fn a_dead_endpoint_classifies_as_unreachable() {
    let client = KiCadIpcClient::new("tcp://127.0.0.1:1");
    let failure = konnect_ipc::IpcFailure::from_error(client.get_open_documents().unwrap_err());
    assert!(
        matches!(failure, konnect_ipc::IpcFailure::Unreachable(_)),
        "unexpected classification: {failure:?}"
    );
}

#[test]
fn a_live_kicad_that_says_no_classifies_as_rejected() {
    let mock = spawn_mock(|_req| {
        Some(kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsBadRequest as i32,
                error_message: "no board open".to_string(),
            }),
            header: None,
            message: None,
        })
    });
    let client = KiCadIpcClient::new(&mock.url);
    let failure = konnect_ipc::IpcFailure::from_error(client.get_open_documents().unwrap_err());
    assert!(
        matches!(failure, konnect_ipc::IpcFailure::Rejected(_)),
        "a completed round-trip must never classify as unreachable: {failure:?}"
    );
    assert!(failure.message().contains("no board open"), "{failure:?}");
}

/// The regression the recv timeout exists for: a server that accepts the
/// request and never replies. The predecessor project hung >600 s here; the
/// client must give up at its recv timeout instead.
///
/// Ignored by default: it necessarily takes the full 30 s recv timeout.
/// Run explicitly with: cargo test -p konnect-ipc -- --ignored
#[test]
#[ignore = "takes ~30s (full recv timeout) by design"]
fn wedged_server_times_out_instead_of_hanging() {
    let mock = spawn_mock(|_req| None); // accept, never respond

    let client = KiCadIpcClient::new(&mock.url);
    let start = std::time::Instant::now();
    let result = client.get_open_documents();
    assert!(result.is_err(), "expected timeout error");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_secs(25) && elapsed < Duration::from_secs(60),
        "expected ~30s recv timeout, got {elapsed:?}"
    );
}

// ─── Multi-board document targeting ──────────────────────────────────────────
//
// Live verification caught this: with the user's own project focused and the
// target board open behind it, first-document targeting either fails or
// mutates the wrong board. place_footprint must address the document whose
// path matches the request.

#[test]
fn placement_targets_the_named_board_among_several_open() {
    use std::sync::{Arc, Mutex};
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_in_mock = captured.clone();

    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            let response = kiapi::common::commands::GetOpenDocumentsResponse {
                documents: vec![
                    doc_for("other-project.kicad_pcb"),
                    doc_for("target.kicad_pcb"),
                ],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetOpenDocumentsResponse",
            )));
        }
        if message.type_url.ends_with("GetItems") {
            let request =
                kiapi::common::commands::GetItems::decode(message.value.as_slice()).unwrap();
            record_doc(&captured_in_mock, &request.header);
            let response = kiapi::common::commands::GetItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                items: vec![],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetItemsResponse",
            )));
        }
        if message.type_url.ends_with("CreateItems") {
            let request =
                kiapi::common::commands::CreateItems::decode(message.value.as_slice()).unwrap();
            record_doc(&captured_in_mock, &request.header);
            // Fail fast after capturing the header; the assertion below is
            // about WHICH board the create addressed, not the outcome.
            return Some(kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsBadRequest as i32,
                    error_message: "stop here".to_string(),
                }),
                header: None,
                message: None,
            });
        }
        Some(ok_response())
    });

    let client = KiCadIpcClient::new(&mock.url);
    let _ = client.place_footprint(
        std::path::Path::new("target.kicad_pcb"),
        "Resistor_SMD:R_0402",
        "R1",
        "R_0402",
        &[],
        &[],
        &konnect_ipc::IpcFieldPlacement::default(),
        10.0,
        20.0,
        0.0,
        "F.Cu",
    );

    let addressed = captured
        .lock()
        .unwrap()
        .take()
        .expect("a command carried a document");
    assert_eq!(
        addressed, "target.kicad_pcb",
        "commands must address the requested board, not the first open one"
    );
}

fn doc_for(filename: &str) -> kiapi::common::types::DocumentSpecifier {
    kiapi::common::types::DocumentSpecifier {
        r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
        project: None,
        identifier: Some(
            kiapi::common::types::document_specifier::Identifier::BoardFilename(
                filename.to_string(),
            ),
        ),
    }
}

#[test]
fn generic_read_and_write_helpers_keep_the_bound_named_board() {
    let captured = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let captured_in_mock = captured.clone();
    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            let response = kiapi::common::commands::GetOpenDocumentsResponse {
                documents: vec![doc_for("other.kicad_pcb"), doc_for("target.kicad_pcb")],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetOpenDocumentsResponse",
            )));
        }
        if message.type_url.ends_with("GetItems") {
            let command =
                kiapi::common::commands::GetItems::decode(message.value.as_slice()).unwrap();
            let document = command.header.unwrap().document.unwrap();
            captured_in_mock
                .lock()
                .unwrap()
                .push(("read".to_string(), board_filename(&document)));
            let response = kiapi::common::commands::GetItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                items: Vec::new(),
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetItemsResponse",
            )));
        }
        if message.type_url.ends_with("CreateItems") {
            let command =
                kiapi::common::commands::CreateItems::decode(message.value.as_slice()).unwrap();
            let document = command.header.unwrap().document.unwrap();
            captured_in_mock
                .lock()
                .unwrap()
                .push(("write".to_string(), board_filename(&document)));
            let response = kiapi::common::commands::CreateItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                created_items: vec![creation_result(
                    kiapi::common::commands::ItemStatusCode::IscOk,
                    "",
                )],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.CreateItemsResponse",
            )));
        }
        panic!("unexpected command {}", message.type_url);
    });

    let client = KiCadIpcClient::new(&mock.url);
    client
        .find_open_board(std::path::Path::new("target.kicad_pcb"))
        .expect("bind target");
    client
        .get_items(kiapi::common::types::KiCadObjectType::KotPcbFootprint)
        .expect("generic read");
    client
        .create_items(vec![any_item()])
        .expect("generic write");

    assert_eq!(
        *captured.lock().unwrap(),
        [
            ("read".to_string(), "target.kicad_pcb".to_string()),
            ("write".to_string(), "target.kicad_pcb".to_string()),
        ]
    );
}

#[test]
fn zero_wrong_and_duplicate_open_document_sets_are_typed_target_failures() {
    let cases = [
        (
            Vec::new(),
            "no_open",
            std::path::PathBuf::from("target.kicad_pcb"),
        ),
        (
            vec![doc_for("other.kicad_pcb")],
            "wrong",
            std::path::PathBuf::from("target.kicad_pcb"),
        ),
        (
            vec![doc_for("target.kicad_pcb"), doc_for("target.kicad_pcb")],
            "ambiguous",
            std::path::PathBuf::from("target.kicad_pcb"),
        ),
    ];

    for (documents, expected, requested) in cases {
        let mock = spawn_mock(move |request| {
            let message = request.message.expect("request must pack a command");
            assert!(message.type_url.ends_with("GetOpenDocuments"));
            let response = kiapi::common::commands::GetOpenDocumentsResponse {
                documents: documents.clone(),
            };
            Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetOpenDocumentsResponse",
            )))
        });
        let client = KiCadIpcClient::new(&mock.url);
        let failure = konnect_ipc::IpcFailure::from_error(
            client
                .find_open_board(&requested)
                .expect_err("target selection must refuse"),
        );
        match (expected, failure) {
            (
                "no_open",
                konnect_ipc::IpcFailure::Target {
                    error: konnect_ipc::BoardTargetError::NoOpenDocuments { .. },
                    ..
                },
            )
            | (
                "wrong",
                konnect_ipc::IpcFailure::Target {
                    error: konnect_ipc::BoardTargetError::WrongDocument { .. },
                    ..
                },
            )
            | (
                "ambiguous",
                konnect_ipc::IpcFailure::Target {
                    error: konnect_ipc::BoardTargetError::AmbiguousDocument { .. },
                    ..
                },
            ) => {}
            (_, other) => panic!("unexpected target classification: {other:?}"),
        }
    }
}

#[test]
fn a_bound_document_disappearing_before_a_generic_command_is_stale() {
    let observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observations_in_mock = observations.clone();
    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        assert!(message.type_url.ends_with("GetOpenDocuments"));
        let count = observations_in_mock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let response = kiapi::common::commands::GetOpenDocumentsResponse {
            documents: if count == 0 {
                vec![doc_for("target.kicad_pcb")]
            } else {
                vec![doc_for("other.kicad_pcb")]
            },
        };
        Some(reply_with(builders::pack_any(
            &response,
            "kiapi.common.commands.GetOpenDocumentsResponse",
        )))
    });

    let client = KiCadIpcClient::new(&mock.url);
    client
        .find_open_board(std::path::Path::new("target.kicad_pcb"))
        .expect("initial target binding");
    let failure = konnect_ipc::IpcFailure::from_error(
        client
            .get_items(kiapi::common::types::KiCadObjectType::KotPcbFootprint)
            .expect_err("closed bound document must refuse before GetItems"),
    );
    assert!(matches!(
        failure,
        konnect_ipc::IpcFailure::Target {
            error: konnect_ipc::BoardTargetError::StaleDocument { .. },
            ..
        }
    ));
    assert_eq!(observations.load(std::sync::atomic::Ordering::SeqCst), 2);
}

fn board_filename(document: &kiapi::common::types::DocumentSpecifier) -> String {
    match document.identifier.as_ref() {
        Some(kiapi::common::types::document_specifier::Identifier::BoardFilename(name)) => {
            name.clone()
        }
        other => panic!("expected board filename, got {other:?}"),
    }
}

fn record_doc(
    slot: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    header: &Option<kiapi::common::types::ItemHeader>,
) {
    if let Some(kiapi::common::types::document_specifier::Identifier::BoardFilename(name)) = header
        .as_ref()
        .and_then(|h| h.document.as_ref())
        .and_then(|d| d.identifier.as_ref())
    {
        let mut slot = slot.lock().unwrap();
        if slot.is_none() {
            *slot = Some(name.clone());
        }
    }
}

// --- #244: a pad must never be mistaken for a graphic --------------------

/// A footprint carrying one pad and one silkscreen line, both with KIIDs, as
/// KiCad would send it.
fn footprint_with_a_pad_and_a_line(pad_uuid: &str, line_uuid: &str) -> prost_types::Any {
    let item = KiCadIpcClient::build_footprint_item(
        "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
        "U1",
        "NE555",
        &[konnect_ipc::IpcPadDefinition {
            number: "1".to_string(),
            pad_type: "smd".to_string(),
            shape: "rect".to_string(),
            x: 0.0,
            y: 0.0,
            rotation: 0.0,
            size_x: 1.0,
            size_y: 1.0,
            drill_x: None,
            drill_y: None,
            drill_oval: false,
            layers: vec!["F.Cu".to_string()],
            roundrect_ratio: 0.0,
        }],
        &[konnect_ipc::IpcGraphicDefinition::Poly {
            points: vec![(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0)],
            layer: "F.SilkS".to_string(),
            width: 0.12,
            filled: false,
        }],
        &konnect_ipc::IpcFieldPlacement::default(),
        10.0,
        10.0,
        0.0,
        "F.Cu",
    )
    .unwrap();

    let mut fp = kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
    for child in &mut fp.definition.as_mut().unwrap().items {
        let id = Some(kiapi::common::types::Kiid {
            value: if builders::any_is(child, "kiapi.board.types.Pad") {
                pad_uuid.to_string()
            } else {
                line_uuid.to_string()
            },
        });
        if builders::any_is(child, "kiapi.board.types.Pad") {
            let mut pad = kiapi::board::types::Pad::decode(child.value.as_slice()).unwrap();
            pad.id = id;
            *child = builders::pack_any(&pad, "kiapi.board.types.Pad");
        } else if builders::any_is(child, "kiapi.board.types.BoardGraphicShape") {
            let mut shape =
                kiapi::board::types::BoardGraphicShape::decode(child.value.as_slice()).unwrap();
            shape.id = id;
            *child = builders::pack_any(&shape, "kiapi.board.types.BoardGraphicShape");
        }
    }
    builders::pack_any(&fp, "kiapi.board.types.FootprintInstance")
}

/// Asking to edit a graphic by a *pad's* uuid must refuse, and must not send
/// an UpdateItems — while the real graphic on the same footprint stays
/// editable, so the refusal is selective and not a blanket failure.
///
/// Note what this does and does not prove. It passes with #244's type-URL
/// check removed, because `Pad.id` is tag 1 and `BoardGraphicShape.id` is
/// tag 4: a pad's KIID does not land in `shape.id`, so the uuid never matches
/// and the pad is unreachable by this path today anyway. The type check is
/// there so that stays true when KiCad renumbers a field, and this test pins
/// the caller-visible contract rather than the guard.
#[test]
fn a_pads_uuid_is_never_accepted_as_a_graphic_to_edit() {
    let updates: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let seen = updates.clone();
    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            return Some(open_board_response());
        }
        if message.type_url.ends_with("GetItems") {
            let response = kiapi::common::commands::GetItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                items: vec![footprint_with_a_pad_and_a_line("pad-kiid", "line-kiid")],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetItemsResponse",
            )));
        }
        if message.type_url.ends_with("UpdateItems") {
            *seen.lock().unwrap() += 1;
            let response = kiapi::common::commands::UpdateItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                updated_items: vec![kiapi::common::commands::ItemUpdateResult {
                    status: Some(kiapi::common::commands::ItemStatus {
                        code: kiapi::common::commands::ItemStatusCode::IscOk as i32,
                        error_message: String::new(),
                    }),
                    item: None,
                }],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.UpdateItemsResponse",
            )));
        }
        Some(ok_response())
    });

    let client = KiCadIpcClient::new(&mock.url);
    let error = client
        .set_footprint_graphic_points("U1", "pad-kiid", &[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)])
        .expect_err("a pad is not a graphic")
        .to_string();

    assert!(
        error.contains("pad-kiid") && error.contains("not found"),
        "must refuse by uuid: {error}"
    );
    assert_eq!(
        *updates.lock().unwrap(),
        0,
        "a refused edit must not write to the board"
    );

    // The real graphic on the same footprint still resolves, so the guard
    // refuses the pad rather than refusing everything.
    client
        .set_footprint_graphic_points("U1", "line-kiid", &[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)])
        .expect("the actual graphic is still editable");
    assert_eq!(*updates.lock().unwrap(), 1);
}

// ─── Reading pads back from the live board ───────────────────────────────────
//
// The board file is the last save, so a footprint placed through IPC has no
// pads on disk until the user presses Ctrl+S. Reading them over IPC is what
// lets a caller place a part and immediately measure it.

fn pad_at(number: &str, x_mm: f64, y_mm: f64, net: &str) -> prost_types::Any {
    builders::pack_any(
        &kiapi::board::types::Pad {
            number: number.to_string(),
            position: Some(builders::vec2(x_mm, y_mm)),
            net: Some(kiapi::board::types::Net {
                code: None,
                name: net.to_string(),
            }),
            pad_stack: Some(kiapi::board::types::PadStack {
                layers: vec![
                    kiapi::board::types::BoardLayer::BlFCu as i32,
                    kiapi::board::types::BoardLayer::BlFPaste as i32,
                    kiapi::board::types::BoardLayer::BlFMask as i32,
                ],
                ..Default::default()
            }),
            ..Default::default()
        },
        "kiapi.board.types.Pad",
    )
}

fn footprint_with_pads(reference: &str, pads: Vec<prost_types::Any>) -> prost_types::Any {
    let mut items = pads;
    // A non-pad child must be skipped rather than decoded as a pad.
    items.push(builders::pack_any(
        &builders::board_segment("F.SilkS", 0.12, 0.0, 0.0, 1.0, 0.0),
        "kiapi.board.types.BoardGraphicShape",
    ));
    builders::pack_any(
        &kiapi::board::types::FootprintInstance {
            position: Some(builders::vec2(100.0, 100.0)),
            reference_field: Some(kiapi::board::types::Field {
                name: "Reference".to_string(),
                text: Some(kiapi::board::types::BoardText {
                    text: Some(kiapi::common::types::Text {
                        text: reference.to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            definition: Some(kiapi::board::types::Footprint {
                items,
                ..Default::default()
            }),
            ..Default::default()
        },
        "kiapi.board.types.FootprintInstance",
    )
}

fn spawn_kicad_holding_items(items: Vec<prost_types::Any>) -> MockKicad {
    spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            return Some(open_board_response());
        }
        if message.type_url.ends_with("GetItems") {
            let response = kiapi::common::commands::GetItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                items: items.clone(),
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetItemsResponse",
            )));
        }
        Some(ok_response())
    })
}

#[test]
fn a_failed_item_read_is_an_error_not_an_empty_board() {
    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            return Some(open_board_response());
        }
        if message.type_url.ends_with("GetItems") {
            let response = kiapi::common::commands::GetItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsDocumentNotFound as i32,
                items: vec![],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetItemsResponse",
            )));
        }
        Some(ok_response())
    });

    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("test.kicad_pcb"))
        .expect("the mock holds test.kicad_pcb");

    let error = client
        .get_footprint_pads_in(document, "U1")
        .expect_err("a failed request must not read as a board with no footprints");
    assert!(
        error.to_string().contains("IRS_DOCUMENT_NOT_FOUND"),
        "the failure must name KiCad's status, got: {error}"
    );
}

#[test]
fn footprint_pads_come_back_in_board_coordinates_with_their_nets() {
    let mock = spawn_kicad_holding_items(vec![footprint_with_pads(
        "U1",
        vec![
            pad_at("A4", 101.155, 66.11, "/VBUS"),
            pad_at("A5", 102.155, 66.11, ""),
        ],
    )]);
    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("test.kicad_pcb"))
        .expect("the mock holds test.kicad_pcb");

    let pads = client
        .get_footprint_pads_in(document, "U1")
        .expect("pad read")
        .expect("U1 is on the board");

    assert_eq!(pads.len(), 2, "the silk segment must not decode as a pad");
    assert_eq!(pads[0].number, "A4");
    assert_eq!(pads[0].x, 101.155);
    assert_eq!(pads[0].y, 66.11);
    assert_eq!(pads[0].net, "/VBUS");
    assert_eq!(pads[0].layers, vec!["F.Cu", "F.Paste", "F.Mask"]);
    // KiCad names no net on an unconnected pad; "" is that, not a read failure.
    assert_eq!(pads[1].net, "");
}

#[test]
fn an_unreadable_live_pad_is_reported_instead_of_silently_dropped() {
    let mock = spawn_kicad_holding_items(vec![footprint_with_pads(
        "U1",
        vec![prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.Pad".to_string(),
            value: vec![0xff],
        }],
    )]);
    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("test.kicad_pcb"))
        .expect("the mock holds test.kicad_pcb");

    let error = client
        .get_footprint_pads_in(document, "U1")
        .expect_err("malformed pad data must fail the read");
    assert!(error.to_string().contains("unreadable pad"), "{error:#}");
}

#[test]
fn a_live_pad_without_a_position_is_reported_instead_of_fabricated_at_zero() {
    let pad = builders::pack_any(
        &kiapi::board::types::Pad {
            number: "1".to_string(),
            ..Default::default()
        },
        "kiapi.board.types.Pad",
    );
    let mock = spawn_kicad_holding_items(vec![footprint_with_pads("U1", vec![pad])]);
    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("test.kicad_pcb"))
        .expect("the mock holds test.kicad_pcb");

    let error = client
        .get_footprint_pads_in(document, "U1")
        .expect_err("missing coordinates must fail the read");
    assert!(error.to_string().contains("has no position"), "{error:#}");
}

#[test]
fn a_footprint_absent_from_the_live_board_reads_as_none() {
    let mock = spawn_kicad_holding_items(vec![footprint_with_pads(
        "U1",
        vec![pad_at("1", 1.0, 2.0, "GND")],
    )]);
    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("test.kicad_pcb"))
        .expect("the mock holds test.kicad_pcb");

    assert!(client
        .get_footprint_pads_in(document, "R99")
        .expect("pad read")
        .is_none());
}

#[test]
fn pad_reads_target_the_named_board_among_several_open() {
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_in_mock = captured.clone();

    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            let response = kiapi::common::commands::GetOpenDocumentsResponse {
                documents: vec![
                    doc_for("other-project.kicad_pcb"),
                    doc_for("target.kicad_pcb"),
                ],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetOpenDocumentsResponse",
            )));
        }
        if message.type_url.ends_with("GetItems") {
            let request =
                kiapi::common::commands::GetItems::decode(message.value.as_slice()).unwrap();
            record_doc(&captured_in_mock, &request.header);
            let response = kiapi::common::commands::GetItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                items: vec![],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetItemsResponse",
            )));
        }
        Some(ok_response())
    });

    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("target.kicad_pcb"))
        .expect("target.kicad_pcb is open");
    let _ = client.get_footprint_pads_in(document, "R1");

    let addressed = captured
        .lock()
        .unwrap()
        .take()
        .expect("a command carried a document");
    assert_eq!(
        addressed, "target.kicad_pcb",
        "a read must answer about the requested board, not the first open one"
    );
}

fn kiid(value: &str) -> kiapi::common::types::Kiid {
    kiapi::common::types::Kiid {
        value: value.to_string(),
    }
}

#[test]
fn board_graphics_come_back_with_their_kind_layer_and_identifier() {
    let mut segment = builders::board_segment("Edge.Cuts", 0.05, 0.0, 0.0, 100.0, 0.0);
    segment.id = Some(kiid("edge-top"));
    let mut text = builders::board_text("F.SilkS", "REV A", 10.0, 20.0, 1.0, 0.0, false);
    text.id = Some(kiid("silk-text"));

    // One request asks for four object types, so KiCad answers with a single
    // mixed list. protobuf decoding is lenient enough to turn the text into an
    // empty shape, so the reader has to dispatch on the type_url.
    let mock = spawn_kicad_holding_items(vec![
        builders::pack_any(&segment, "kiapi.board.types.BoardGraphicShape"),
        builders::pack_any(&text, "kiapi.board.types.BoardText"),
    ]);

    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("test.kicad_pcb"))
        .expect("the mock holds test.kicad_pcb");

    let graphics = client
        .get_board_graphics_in(document)
        .expect("graphics read");

    assert_eq!(graphics.len(), 2, "the text must be read once, as text");
    assert_eq!(graphics[0].uuid, "edge-top");
    assert_eq!(graphics[0].kind, "line");
    assert_eq!(graphics[0].layer, "Edge.Cuts");
    assert_eq!(
        graphics[0].origin.as_ref().map(|p| (p.x, p.y)),
        Some((0.0, 0.0))
    );
    assert_eq!(graphics[1].uuid, "silk-text");
    assert_eq!(graphics[1].kind, "text");
    assert_eq!(graphics[1].layer, "F.SilkS");
    assert_eq!(
        graphics[1].origin.as_ref().map(|p| (p.x, p.y)),
        Some((10.0, 20.0))
    );
}

#[test]
fn deletes_target_the_named_board_among_several_open() {
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_in_mock = captured.clone();

    let mock = spawn_mock(move |request| {
        let message = request.message.expect("request must pack a command");
        if message.type_url.ends_with("GetOpenDocuments") {
            let response = kiapi::common::commands::GetOpenDocumentsResponse {
                documents: vec![
                    doc_for("other-project.kicad_pcb"),
                    doc_for("target.kicad_pcb"),
                ],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.GetOpenDocumentsResponse",
            )));
        }
        if message.type_url.ends_with("DeleteItems") {
            let request =
                kiapi::common::commands::DeleteItems::decode(message.value.as_slice()).unwrap();
            record_doc(&captured_in_mock, &request.header);
            let response = kiapi::common::commands::DeleteItemsResponse {
                header: None,
                status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                deleted_items: vec![],
            };
            return Some(reply_with(builders::pack_any(
                &response,
                "kiapi.common.commands.DeleteItemsResponse",
            )));
        }
        Some(ok_response())
    });

    let client = KiCadIpcClient::new(&mock.url);
    let document = client
        .find_open_board(std::path::Path::new("target.kicad_pcb"))
        .expect("target.kicad_pcb is open");
    client
        .delete_items_in(document, vec!["edge-top".to_string()])
        .expect("delete");

    let addressed = captured
        .lock()
        .unwrap()
        .take()
        .expect("the delete carried a document");
    assert_eq!(
        addressed, "target.kicad_pcb",
        "a delete must act on the requested board, not the first open one"
    );
}
