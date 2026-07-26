//! IPC client tests against a mock KiCAD NNG server — no KiCAD required.
//!
//! A rep0 socket on tcp://127.0.0.1:<port> plays KiCAD: it decodes the
//! ApiRequest envelope and returns canned ApiResponse messages. This lets CI
//! exercise the full encode → transport → decode → error-mapping path that
//! previously only ran against a live KiCAD session.

use konnect_ipc::gen::kiapi;
use konnect_ipc::KiCadIpcClient;
use nng::options::Options;
use prost::Message;
use std::time::Duration;

/// A rep0 server answering each request via `respond`.
/// Returns the tcp:// URL to dial. The server thread exits when the socket
/// errors (i.e. when `_socket_keepalive` is dropped by the returned guard).
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

// ─── ParseAndCreateItemsFromString outcome handling ───────────────────────────

/// A mock KiCAD with one board open that answers every paste with `results`.
///
/// `place_footprint` first asks for the open document, so the mock has to
/// answer two different commands.
fn spawn_mock_pasting(results: Vec<kiapi::common::commands::ItemCreationResult>) -> MockKicad {
    spawn_mock(move |req| {
        let msg = req.message.expect("request must pack a command");
        let body = if msg.type_url.ends_with("GetOpenDocuments") {
            pack(
                &kiapi::common::commands::GetOpenDocumentsResponse {
                    documents: vec![kiapi::common::types::DocumentSpecifier::default()],
                },
                "kiapi.common.commands.GetOpenDocumentsResponse",
            )
        } else {
            pack(
                &kiapi::common::commands::CreateItemsResponse {
                    header: None,
                    // IRS_OK even though nothing was created — the response
                    // shape the proto explicitly warns about.
                    status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                    created_items: results.clone(),
                },
                "kiapi.common.commands.CreateItemsResponse",
            )
        };
        Some(kiapi::common::ApiResponse {
            message: Some(body),
            ..ok_response()
        })
    })
}

fn pack<M: Message>(msg: &M, type_name: &str) -> prost_types::Any {
    prost_types::Any {
        type_url: format!("type.googleapis.com/{type_name}"),
        value: msg.encode_to_vec(),
    }
}

fn result_with(
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

#[test]
fn a_rejected_item_is_not_counted_as_created() {
    // The regression this guards: created_items is documented as "status of
    // each item TO BE created", so a rejection still occupies a slot. Counting
    // the vector's length would call this a success and put the phantom back.
    let mock = spawn_mock_pasting(vec![result_with(
        kiapi::common::commands::ItemStatusCode::IscInvalidData,
        "footprint has no pads",
    )]);

    let client = KiCadIpcClient::new(&mock.url);
    let err = client
        .place_footprint("(footprint \"Lib:Fp\")")
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("created no footprint"),
        "must report failure: {err}"
    );
    // The per-item reason is what makes this diagnosable.
    assert!(
        err.contains("IscInvalidData") && err.contains("footprint has no pads"),
        "must surface KiCAD's own reason: {err}"
    );
}

#[test]
fn an_empty_result_list_still_reports_failure() {
    // KiCAD 10.0's actual behaviour: an empty CreateItemsResponse.
    let mock = spawn_mock_pasting(vec![]);
    let client = KiCadIpcClient::new(&mock.url);
    let err = client
        .place_footprint("(footprint \"Lib:Fp\")")
        .unwrap_err()
        .to_string();
    assert!(err.contains("created no footprint"), "unexpected: {err}");
    assert!(err.contains("no items at all"), "unexpected: {err}");
}

#[test]
fn a_created_item_counts() {
    let mock = spawn_mock_pasting(vec![result_with(
        kiapi::common::commands::ItemStatusCode::IscOk,
        "",
    )]);
    let client = KiCadIpcClient::new(&mock.url);
    assert_eq!(client.place_footprint("(footprint \"Lib:Fp\")").unwrap(), 1);
}

#[test]
fn a_defaulted_status_counts_only_when_an_item_came_back() {
    // Protobuf cannot distinguish "unset" from "explicitly zero", so an
    // ISC_UNKNOWN status is only evidence of success if an item is attached.
    let with_item = kiapi::common::commands::ItemCreationResult {
        status: None,
        item: Some(prost_types::Any::default()),
    };
    let mock = spawn_mock_pasting(vec![with_item]);
    let client = KiCadIpcClient::new(&mock.url);
    assert_eq!(
        client.place_footprint("(footprint \"Lib:Fp\")").unwrap(),
        1,
        "an item with a defaulted status was still created"
    );

    // The same defaulted status with nothing attached created nothing.
    let without_item = kiapi::common::commands::ItemCreationResult {
        status: Some(kiapi::common::commands::ItemStatus {
            code: kiapi::common::commands::ItemStatusCode::IscUnknown as i32,
            error_message: String::new(),
        }),
        item: None,
    };
    let mock = spawn_mock_pasting(vec![without_item]);
    let client = KiCadIpcClient::new(&mock.url);
    assert!(
        client.place_footprint("(footprint \"Lib:Fp\")").is_err(),
        "a bare ISC_UNKNOWN with no item must not count as created"
    );
}

#[test]
fn a_mixed_response_counts_only_the_successes() {
    let mock = spawn_mock_pasting(vec![
        result_with(kiapi::common::commands::ItemStatusCode::IscOk, ""),
        result_with(
            kiapi::common::commands::ItemStatusCode::IscInvalidData,
            "bad",
        ),
    ]);
    let client = KiCadIpcClient::new(&mock.url);
    let outcome = client
        .parse_and_create_items("(footprint \"Lib:Fp\")")
        .unwrap();
    assert_eq!(outcome.created, 1);
    assert_eq!(outcome.failures.len(), 1);
}
