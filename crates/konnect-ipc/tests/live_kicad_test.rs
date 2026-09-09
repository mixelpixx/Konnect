//! Live KiCad GUI IPC regression tests.
//!
//! These tests are ignored by default; run them with `-- --ignored`, and with
//! `--test-threads=1`, since they all drive the one board KiCad has open.
//!
//! ## Point `KONNECT_LIVE_KICAD_BOARD` at a COPY
//!
//! These tests mutate the open board and call `save_board()` — the move test
//! relocates and rotates a mounting hole. Opening the tracked fixture directly
//! writes those mutations back into the repository, and KiCad also upgrades
//! the file format and drops a `.kicad_pro`, a `.kicad_prl`, a lock file and a
//! `.history/` beside it. That has already happened once: a live run left the
//! fixture format-upgraded with `MH1` moved, and `git add -A` swept the lot
//! into a commit. Copy it to a scratch directory and open the copy:
//!
//! ```sh
//! cp crates/konnect-ipc/tests/fixtures/live_ipc.kicad_pcb /tmp/live/
//! export KONNECT_LIVE_KICAD_BOARD=/tmp/live/live_ipc.kicad_pcb
//! ```
//!
//! ## The fixture
//!
//! `fixtures/live_ipc.kicad_pcb` is KiCad's GPL-licensed built-in
//! EuroCard160mmX100mm template — board outline (55, 45) to (215, 145), four
//! locked mounting holes, and two keepout zones along the guide-rail edges —
//! plus a `GND` and a `SIG1` net added for these tests. Those nets are
//! load-bearing: the stock template declares only the unnamed net 0, so
//! `get_nets` returns nothing usable and every net-attached test below fails
//! its own precondition before reaching the code under test. The board is in
//! the legacy (KiCad 9) format, where nets live in a top-level table and
//! copper references them by id; KiCad 10 upgrades it on open.
//!
//! Coordinates in these tests are absolute board coordinates, so they must sit
//! inside that outline — the template does not start at the origin, and a zone
//! placed outside the outline is clipped to nothing.
//!
//! ## Finding the socket (`KICAD_API_SOCKET`)
//!
//! KiCad exposes **one socket per frame**, not one per process, and the
//! well-known path is not usually the one you want. On macOS with KiCad 10 the
//! *manager* owns `/tmp/kicad/api.sock` and answers `GetOpenDocuments` with
//! `AS_UNHANDLED`, because it holds no board; a standalone pcbnew registers its
//! own `/tmp/kicad/api-<pid>.sock`, and that is the one to target. The suffix
//! changes every launch, so it cannot be hard-coded:
//!
//! ```sh
//! ls -t /tmp/kicad/api-*.sock | head -1
//! ```
//!
//! There is no CI job for this file. `e2e-kicad.yml` installs KiCad but runs
//! only the kicad-cli and mock-server suites, and sets none of the variables
//! these tests require — so nothing here has ever run automatically. Run it by
//! hand before tagging a release.

use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::{parse_sexp, SexpNode};
use std::path::Path;

/// Where `adding_a_via_actually_creates_it_on_the_board` puts its via, and the
/// outline `adding_a_zone_creates_it_on_the_live_board` pours. Both are
/// absolute board coordinates, and both are checked against the fixture's
/// outline by `the_live_fixture_satisfies_what_the_live_tests_assume` — the
/// via spot used to be (40, 40), which is off the board.
const VIA_SPOT: (f64, f64) = (100.0, 120.0);
const ZONE_OUTLINE: [(f64, f64); 4] =
    [(150.0, 95.0), (190.0, 95.0), (190.0, 115.0), (150.0, 115.0)];

fn footprint<'a>(tree: &'a SexpNode, reference: &str) -> &'a SexpNode {
    tree.find_all("footprint")
        .into_iter()
        .find(|node| {
            node.find_all("property").into_iter().any(|property| {
                property.get(1).and_then(SexpNode::as_str) == Some("Reference")
                    && property.get(2).and_then(SexpNode::as_str) == Some(reference)
            })
        })
        .unwrap_or_else(|| panic!("footprint {reference} not found in saved board"))
}

fn at(node: &SexpNode) -> (f64, f64) {
    let at = node.find("at").expect("item has no (at ...) position");
    (
        at.get_f64(1).expect("invalid X coordinate"),
        at.get_f64(2).expect("invalid Y coordinate"),
    )
}

fn footprint_at(node: &SexpNode) -> (f64, f64, f64) {
    let position = node.find("at").expect("footprint has no (at ...) position");
    (
        position.get_f64(1).expect("invalid footprint X"),
        position.get_f64(2).expect("invalid footprint Y"),
        position.get_f64(3).unwrap_or(0.0),
    )
}

fn collect_geometry(node: &SexpNode, output: &mut Vec<(String, f64, f64)>) {
    if matches!(
        node.head(),
        Some("at" | "start" | "mid" | "end" | "center" | "xy")
    ) {
        if let (Some(x), Some(y)) = (node.get_f64(1), node.get_f64(2)) {
            output.push((node.head().unwrap().to_string(), x, y));
        }
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_geometry(child, output);
        }
    }
}

/// Footprint-relative child coordinates, in a canonical order.
///
/// KiCad is free to re-serialize a footprint's graphics in a different order
/// when it rewrites the file — a rotate on a footprint with several silk and
/// courtyard segments reliably shuffles them. The invariant under test is that
/// no child coordinate *changed*, not that KiCad preserved its own ordering,
/// so compare as a sorted multiset.
fn child_geometry(footprint: &SexpNode) -> Vec<(String, f64, f64)> {
    let mut output = Vec::new();
    for child in footprint.children().unwrap_or_default() {
        // The footprint's own position is the only coordinate expected to
        // change. Every nested coordinate is footprint-relative on disk.
        if child.head() != Some("at") {
            collect_geometry(child, &mut output);
        }
    }
    output.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then(a.2.total_cmp(&b.2))
    });
    output
}

fn pad_offsets(footprint: &SexpNode) -> Vec<(f64, f64)> {
    footprint.find_all("pad").into_iter().map(at).collect()
}

fn load_board(path: &Path) -> SexpNode {
    let source = std::fs::read_to_string(path).expect("failed to read live KiCad board");
    parse_sexp(&source).expect("failed to parse live KiCad board")
}

#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn editor_state_observation_reports_real_version_and_honest_capabilities() {
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let observation = KiCadIpcClient::new(socket)
        .observe_editor_state()
        .expect("editor-state observation failed");
    assert!(
        observation.kicad_version.major >= 10,
        "navigation requires KiCad 10 or newer, got {:?}",
        observation.kicad_version
    );
    assert_eq!(observation.evidence_source, "kicad_ipc");
    assert_eq!(observation.editors.len(), 2);
    assert!(observation.active_editor.is_none());
    assert!(observation.active_document.is_none());
    assert!(observation.active_sheet_instance.is_none());
    assert!(observation.editors.iter().all(|editor| {
        editor.addressable
            || editor.unavailable_reason.as_deref().is_some_and(|reason| {
                reason.contains("AS_UNHANDLED") || reason.contains("AS_UNIMPLEMENTED")
            })
    }));
}

/// What does KiCad send for a footprint that has no schematic symbol?
///
/// `konnect-core`'s board-sync planner treats `symbol_path == None` as
/// "board-only" — a logo, a fiducial, a mounting hole — and #452 was the
/// planner reading *absence* as the shared identity `/`. The fix assumes
/// KiCad's IPC layer sends `symbol_path` **present but empty**
/// (`Some(SheetPath { path: [] })`) rather than absent (`None`) for such a
/// footprint. That assumption was read from the source and from the field's
/// own proto comment ("the associated symbol ... **if one exists**"); until
/// this test runs it has never been observed against a running editor.
///
/// It matters which one it is. If KiCad sends `None`, the empty-vector guard
/// in `pcb_sync`'s reader is dead code and the `/` identity comes from
/// somewhere else — so the failure message names the state it actually saw
/// rather than merely asserting.
///
/// The bundled fixture is enough: `live_ipc.kicad_pcb` is KiCad's own EuroCard
/// template, whose four mounting holes `MH1`–`MH4` carry no `(path ...)` line
/// at all. Two or more pathless footprints is also exactly the #452 collision,
/// so this covers the reported case and not a reduced one.
#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn kicad_reports_an_empty_sheet_path_for_a_board_only_footprint() {
    use kiapi::common::types::KiCadObjectType as ObjectType;
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);

    // The board on disk is the authority for *which* footprints are board-only:
    // no `(path ...)` child means no schematic symbol behind it. Deriving the
    // expectation from the file rather than hard-coding references keeps this
    // honest if the fixture gains a schematic-backed footprint later.
    //
    // Deliberately no `save_board()`, unlike the mutating tests below: this one
    // only observes, so it must not rewrite the open board. That keeps it safe
    // to run against the tracked fixture itself, and whether a footprint has a
    // `(path ...)` is a property of the design that no format upgrade changes.
    let tree = load_board(Path::new(&board));
    let pathless = tree
        .find_all("footprint")
        .into_iter()
        .filter(|node| node.find("path").is_none())
        .filter_map(|node| {
            node.find_all("property").into_iter().find_map(|property| {
                (property.get(1).and_then(SexpNode::as_str) == Some("Reference"))
                    .then(|| property.get(2).and_then(SexpNode::as_str))
                    .flatten()
                    .map(str::to_string)
            })
        })
        .collect::<Vec<_>>();
    assert!(
        pathless.len() >= 2,
        "this test needs at least two footprints with no (path ...) on the open \
         board — the bundled fixture's MH1-MH4 qualify; found {pathless:?}"
    );

    let document = client
        .find_open_board(Path::new(&board))
        .expect("KiCad has not got the requested board open");
    let items = client
        .get_items_in(document, ObjectType::KotPcbFootprint)
        .expect("footprint query failed");

    let mut seen = Vec::new();
    let mut captures = Vec::new();
    for item in items {
        let footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            .expect("KiCad returned an invalid footprint item");
        let reference = footprint
            .reference_field
            .as_ref()
            .and_then(|field| field.text.as_ref())
            .and_then(|text| text.text.as_ref())
            .map(|text| text.text.clone())
            .unwrap_or_default();
        if !pathless.contains(&reference) {
            continue;
        }
        let state = match footprint.symbol_path.as_ref() {
            None => "absent (None)".to_string(),
            Some(path) if path.path.is_empty() => "present and empty".to_string(),
            Some(path) => format!(
                "present with {} KIID(s): {:?}",
                path.path.len(),
                path.path
                    .iter()
                    .map(|k| k.value.as_str())
                    .collect::<Vec<_>>()
            ),
        };
        captures.push((reference.clone(), item.value.clone(), footprint.clone()));
        seen.push((reference, state));
    }

    // Regeneration + provenance inspection for the checked-in capture. Off by
    // default; the assertions below are what runs normally.
    if std::env::var("KONNECT_CAPTURE_IPC_FIXTURE").is_ok() {
        for (reference, raw, decoded) in &captures {
            eprintln!("--- {reference} ({} bytes) ---\n{decoded:#?}", raw.len());
        }
        let (reference, raw, _) = captures
            .first()
            .expect("no board-only footprint captured to write");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../konnect-core/tests/fixtures/board_only_footprint.ipc.bin");
        std::fs::write(&out, raw).expect("failed to write capture");
        eprintln!(
            "wrote {} bytes from {reference} to {}",
            raw.len(),
            out.display()
        );
    }

    assert_eq!(
        seen.len(),
        pathless.len(),
        "IPC did not return every board-only footprint the file carries: saw {seen:?}, \
         expected {pathless:?}"
    );
    let wrong = seen
        .iter()
        .filter(|(_, state)| state != "present and empty")
        .collect::<Vec<_>>();
    assert!(
        wrong.is_empty(),
        "a footprint with no (path ...) must arrive as a present-but-empty SheetPath, \
         because that is what pcb_sync's board_symbol_path guard normalises to None. \
         These did not: {wrong:?}. If they are 'absent (None)', the guard is dead code \
         and the `/` identity in #452 comes from somewhere else."
    );
}

/// Two board-only footprints on one board really do share the reference
/// `REF**`, and KiCad reports both that way over IPC.
///
/// `pcb_sync` narrows `duplicate_board_reference` to references the schematic
/// export actually names, so a board carrying repeated `REF**` graphics stops
/// blocking a sync. That narrowing rests on a claim about KiCad, not about
/// Konnect: that a footprint placed on the board without annotation keeps the
/// library's own `REF**`, and that a second one keeps it too — the duplicate
/// is normal, not a corrupt board.
///
/// `live_ipc.kicad_pcb` cannot witness it: its four mounting holes are
/// annotated `MH1`-`MH4`. `fixtures/board_only_shared_reference.kicad_pcb` is
/// a board KiCad itself wrote for this, holding two unannotated
/// `MountingHole:MountingHole_2.7mm_M2.5_DIN965` instances. Point
/// `KONNECT_LIVE_KICAD_SHARED_REFERENCE_BOARD` at a copy of it.
///
/// Observing only, like the test above: no `save_board()`, so it cannot
/// rewrite the board it is reading.
#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn kicad_reports_the_same_reference_for_two_board_only_footprints() {
    use kiapi::common::types::KiCadObjectType as ObjectType;
    use konnect_ipc::gen::kiapi;
    use prost::Message;
    use std::collections::BTreeMap;

    let board = std::env::var("KONNECT_LIVE_KICAD_SHARED_REFERENCE_BOARD").expect(
        "KONNECT_LIVE_KICAD_SHARED_REFERENCE_BOARD must name the open copy of \
         fixtures/board_only_shared_reference.kicad_pcb",
    );
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);

    // The file is the authority for which footprints are board-only and what
    // they are called, exactly as in the test above — derived, not hard-coded,
    // so a fixture that gained a schematic-backed footprint fails here rather
    // than quietly narrowing what the test covers.
    let tree = load_board(Path::new(&board));
    let mut pathless_by_reference: BTreeMap<String, usize> = BTreeMap::new();
    for node in tree.find_all("footprint") {
        if node.find("path").is_some() {
            continue;
        }
        let reference = node.find_all("property").into_iter().find_map(|property| {
            (property.get(1).and_then(SexpNode::as_str) == Some("Reference"))
                .then(|| property.get(2).and_then(SexpNode::as_str))
                .flatten()
                .map(str::to_string)
        });
        if let Some(reference) = reference {
            *pathless_by_reference.entry(reference).or_default() += 1;
        }
    }
    let (shared, count) = pathless_by_reference
        .iter()
        .find(|(_, count)| **count >= 2)
        .map(|(reference, count)| (reference.clone(), *count))
        .unwrap_or_else(|| {
            panic!(
                "this test needs two board-only footprints sharing one reference on the \
                 open board — that is the #452 case; found {pathless_by_reference:?}"
            )
        });
    assert_eq!(
        shared, "REF**",
        "the shared reference should be KiCad's own unannotated default, not one \
         an editing session happened to leave behind"
    );

    let document = client
        .find_open_board(Path::new(&board))
        .expect("KiCad has not got the requested board open");
    let items = client
        .get_items_in(document, ObjectType::KotPcbFootprint)
        .expect("footprint query failed");

    let mut seen = Vec::new();
    let mut captures = Vec::new();
    for item in items {
        let footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            .expect("KiCad returned an invalid footprint item");
        let reference = footprint
            .reference_field
            .as_ref()
            .and_then(|field| field.text.as_ref())
            .and_then(|text| text.text.as_ref())
            .map(|text| text.text.clone())
            .unwrap_or_default();
        if reference != shared {
            continue;
        }
        let empty_path = footprint
            .symbol_path
            .as_ref()
            .is_some_and(|path| path.path.is_empty());
        captures.push((reference.clone(), item.value.clone(), footprint.clone()));
        seen.push((reference, empty_path));
    }

    // Regeneration + provenance inspection for the checked-in capture. Off by
    // default; the assertions below are what runs normally.
    if std::env::var("KONNECT_CAPTURE_IPC_FIXTURE").is_ok() {
        for (reference, raw, decoded) in &captures {
            eprintln!("--- {reference} ({} bytes) ---\n{decoded:#?}", raw.len());
        }
        let (reference, raw, _) = captures
            .first()
            .expect("no board-only footprint captured to write");
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../konnect-core/tests/fixtures/board_only_shared_reference.ipc.bin");
        std::fs::write(&out, raw).expect("failed to write capture");
        eprintln!(
            "wrote {} bytes from {reference} to {}",
            raw.len(),
            out.display()
        );
    }

    assert_eq!(
        seen.len(),
        count,
        "IPC returned {} footprints called {shared}, but the file carries {count}",
        seen.len()
    );
    assert!(
        seen.iter().all(|(_, empty_path)| *empty_path),
        "every footprint sharing {shared} must arrive with a present-but-empty SheetPath, \
         or it is not the board-only case #452 is about: {seen:?}"
    );
}

#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn moving_and_rotating_footprint_preserves_child_geometry() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let reference = std::env::var("KONNECT_LIVE_KICAD_REFERENCE").unwrap_or_else(|_| "MH1".into());
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match client.get_open_documents() {
            Ok(documents) if !documents.is_empty() => break,
            Ok(_) if std::time::Instant::now() < deadline => {}
            Ok(_) => panic!("KiCad has no PCB document open"),
            Err(error)
                if error.to_string().contains("AS_NOT_READY")
                    && std::time::Instant::now() < deadline => {}
            Err(error) => panic!("KiCad IPC connection failed: {error:#}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    client.save_board().expect("initial board save failed");
    let before_tree = load_board(Path::new(&board));
    let before = footprint(&before_tree, &reference);
    let original_at = footprint_at(before);
    let original_pads = pad_offsets(before);
    let original_geometry = child_geometry(before);
    assert!(!original_pads.is_empty(), "test footprint has no pads");

    let target = (original_at.0 + 10.0, original_at.1 + 7.0);
    client
        .move_footprint(&reference, target.0, target.1)
        .expect("footprint move failed");
    client.save_board().expect("moved board save failed");

    let after_tree = load_board(Path::new(&board));
    let after = footprint(&after_tree, &reference);
    let moved_at = at(after);
    assert!((moved_at.0 - target.0).abs() < 1e-6);
    assert!((moved_at.1 - target.1).abs() < 1e-6);
    assert_eq!(
        pad_offsets(after),
        original_pads,
        "moving a footprint must not rewrite its child-relative pad positions"
    );
    assert_eq!(
        child_geometry(after),
        original_geometry,
        "moving a footprint must preserve all child-relative geometry"
    );

    let target_rotation = (original_at.2 + 90.0) % 360.0;
    client
        .rotate_footprint(&reference, target_rotation)
        .expect("footprint rotation failed");
    client.save_board().expect("rotated board save failed");

    let rotated_tree = load_board(Path::new(&board));
    let rotated = footprint(&rotated_tree, &reference);
    assert!((footprint_at(rotated).2 - target_rotation).abs() < 1e-6);
    assert_eq!(
        child_geometry(rotated),
        original_geometry,
        "rotating a footprint must preserve all child-relative geometry"
    );
}

/// #117 regression: v0.2.1 shipped an `add_via` that KiCad rejected outright
/// with `AS_BAD_REQUEST "could not unpack PCB_VIA"`, because the padstack
/// carried two copper entries under PST_NORMAL.
///
/// Nothing offline can catch that class: the message is schema-valid, so it
/// encodes and decodes cleanly — only KiCad's own `Deserialize` refuses it.
/// This test is the gate; run it (and the rest of this file) before tagging a
/// release, not just weekly.
#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn adding_a_via_actually_creates_it_on_the_board() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);

    let net = client
        .get_nets()
        .expect("net list query failed")
        .into_iter()
        .find(|net| !net.name.is_empty())
        .expect(
            "board has no named net to attach a via to — on the bundled fixture \
             this means its GND/SIG1 track segments have been lost",
        );

    client.save_board().expect("initial board save failed");
    let vias_before = load_board(Path::new(&board)).find_all("via").len();

    // Inside the EuroCard outline and clear of its mounting holes and tracks.
    let (x, y) = VIA_SPOT;
    client
        .add_via(&net.name, x, y, 0.4, 0.8)
        .expect("add_via reported an error");
    client
        .save_board()
        .expect("board save after add_via failed");

    let after = load_board(Path::new(&board));
    let vias: Vec<_> = after.find_all("via");
    assert_eq!(
        vias.len(),
        vias_before + 1,
        "add_via returned Ok but the saved board has no new via — this is \
         exactly the v0.2.1 failure mode (silent success, nothing created)"
    );
    let placed = vias
        .iter()
        .find(|via| {
            via.find("at")
                .map(|node| {
                    (node.get_f64(1).unwrap_or_default() - x).abs() < 1e-6
                        && (node.get_f64(2).unwrap_or_default() - y).abs() < 1e-6
                })
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no via at ({x}, {y}) in the saved board"));
    assert!(
        placed.find("size").is_some() && placed.find("drill").is_some(),
        "via is missing its size/drill: {placed:?}"
    );
}

/// #412 live gate: `delete_trace` must identify a real trace on the requested
/// board before sending the generic KiCad `DeleteItems` command, then prove
/// that exact trace is absent from live readback. The test creates its own
/// disposable segment so it does not consume fixture routing.
#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled and a disposable open board"]
fn verified_trace_delete_round_trips_through_live_kicad() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);
    let net = client
        .get_nets()
        .expect("net list query failed")
        .into_iter()
        .find(|net| !net.name.is_empty())
        .expect("the live fixture must contain a named net");
    let before: std::collections::HashSet<String> = client
        .get_tracks(None, None)
        .expect("initial trace query failed")
        .into_iter()
        .map(|track| track.uuid)
        .collect();

    client
        .add_track(&net.name, "F.Cu", 0.25, 120.0, 125.0, 130.0, 125.0)
        .expect("temporary trace creation failed");
    let created = client
        .get_tracks(None, None)
        .expect("created trace readback failed")
        .into_iter()
        .find(|track| !before.contains(&track.uuid))
        .expect("KiCad did not return the newly created trace");

    let deleted = client
        .delete_trace_segment_verified(Path::new(&board), &created.uuid)
        .expect("verified trace deletion failed")
        .expect("the created UUID was not observed as a trace");
    assert_eq!(deleted.uuid, created.uuid);
    assert!(
        client
            .get_tracks(None, None)
            .expect("final trace readback failed")
            .iter()
            .all(|track| track.uuid != created.uuid),
        "deleted trace remains in live KiCad"
    );
    client.save_board().expect("final board save failed");
}

/// `add_zone` over IPC, end to end: create, read the zone back out of KiCad,
/// and delete it again.
///
/// This is the gate for the same class of defect `add_via` hit in v0.2.1 and
/// for the one `add_zone` itself shipped: a zone written only into the
/// `.kicad_pcb` file is invisible to an open pcbnew and is discarded by its
/// next save, so "the tool returned Ok" proves nothing. Only a live KiCad can
/// say whether the `Zone` message it was handed deserialises at all — the
/// mocks accept any schema-valid protobuf, which is exactly why a malformed
/// padstack got through offline testing before.
///
/// Reads the zone back over IPC rather than from the saved file so the
/// assertion is against KiCad's own model, including the fill it computed.
#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn adding_a_zone_creates_it_on_the_live_board() {
    use konnect_ipc::gen::kiapi::board::types::{BoardLayer, Zone, ZoneConnectionStyle, ZoneType};
    use prost::Message;

    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);

    // GND by preference, not merely the first named net: the fill assertion
    // below needs the pour to reach copper already on its own net, and on the
    // bundled fixture that copper is the GND segment at y = 105.
    let nets = client.get_nets().expect("net list query failed");
    let net = nets
        .iter()
        .find(|net| net.name == "GND")
        .or_else(|| nets.iter().find(|net| !net.name.is_empty()))
        .expect(
            "board has no named net to attach a zone to — on the bundled fixture \
             this means its GND/SIG1 track segments have been lost",
        )
        .clone();

    // Distinctive so the read-back cannot pick up a zone the board already had.
    let name = "konnect live add_zone";

    // Leftovers from an earlier run of this very test survive in the live
    // session (nothing here saves the board), so a rerun against the same
    // pcbnew found 4 zones where it asserted 1. Start clean.
    let stale: Vec<String> = client
        .get_items(konnect_ipc::gen::kiapi::common::types::KiCadObjectType::KotPcbZone)
        .expect("pre-test zone query failed")
        .iter()
        .filter_map(|item| Zone::decode(item.value.as_slice()).ok())
        .filter(|zone| zone.name == name)
        .filter_map(|zone| zone.id.map(|id| id.value))
        .collect();
    client
        .delete_items(stale)
        .expect("pre-test zone cleanup failed");
    // Inside the EuroCard outline (55, 45)-(215, 145), clear of its mounting
    // holes, and straddling the fixture's GND track so the fill has something
    // on its own net to connect to.
    let points = ZONE_OUTLINE;

    let zone_id = client
        .add_zone(&konnect_ipc::builders::ZoneSpec {
            layer: "B.Cu",
            net_name: &net.name,
            points: &points,
            clearance_mm: 0.3,
            min_thickness_mm: 0.25,
            name,
            priority: 2,
            connection: ZoneConnectionStyle::ZcsFull,
        })
        .expect("add_zone reported an error");

    // KiCad refills the zone right after creation and answers AS_BUSY to any
    // request that lands mid-fill — deterministically on a board this size,
    // not as a flake. Busy-then-fine is normal here; give it a bounded retry.
    let read_back = || -> Vec<Zone> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match client
                .get_items(konnect_ipc::gen::kiapi::common::types::KiCadObjectType::KotPcbZone)
            {
                Ok(items) => {
                    return items
                        .iter()
                        .filter_map(|item| Zone::decode(item.value.as_slice()).ok())
                        .filter(|zone| zone.name == name)
                        .collect()
                }
                Err(error)
                    if error.to_string().contains("AS_BUSY")
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(error) => panic!("zone query failed: {error:#}"),
            }
        }
    };

    let found = read_back();
    assert_eq!(
        found.len(),
        1,
        "add_zone returned Ok but KiCad holds {} zones named {name:?} — a silent \
         success with nothing created is the v0.2.1 failure mode",
        found.len()
    );
    let zone = &found[0];

    assert_eq!(zone.r#type, ZoneType::ZtCopper as i32);
    assert_eq!(zone.layers, vec![BoardLayer::BlBCu as i32]);
    assert_eq!(zone.priority, 2);

    let outline = zone.outline.as_ref().expect("zone has no outline");
    assert_eq!(outline.polygons.len(), 1);
    let nodes = outline.polygons[0]
        .outline
        .as_ref()
        .expect("outline polyline")
        .nodes
        .len();
    assert_eq!(nodes, points.len(), "KiCad kept a different vertex count");

    let settings = match zone.settings.as_ref().expect("zone settings") {
        konnect_ipc::gen::kiapi::board::types::zone::Settings::CopperSettings(s) => s,
        other => panic!("expected copper zone settings, got {other:?}"),
    };
    assert_eq!(
        settings.net.as_ref().expect("zone net").name,
        net.name,
        "the zone landed on the wrong net"
    );
    assert_eq!(
        settings
            .connection
            .as_ref()
            .expect("connection settings")
            .zone_connection,
        ZoneConnectionStyle::ZcsFull as i32
    );
    assert!(zone.filled, "add_zone refills before returning");
    assert!(
        !zone.filled_polygons.is_empty(),
        "the zone has an outline but no computed copper, which is exactly what \
         the user would be left looking at. Most likely cause on a board other \
         than the bundled fixture: add_zone sets island removal to IRM_ALWAYS \
         (KiCad's default), so a pour that reaches nothing else on its own net \
         has its whole fill discarded as islands — check that {} has copper \
         inside the test region",
        net.name
    );

    // Leave the disposable board as we found it.
    let id = zone_id
        .or_else(|| zone.id.as_ref().map(|id| id.value.clone()))
        .expect("no KIID to delete the zone by");
    client.delete_items(vec![id]).expect("zone cleanup failed");
    assert!(
        read_back().is_empty(),
        "the test zone survived its own cleanup"
    );
}

/// The bundled fixture's own preconditions, checked without KiCad — and
/// deliberately **not** `#[ignore]`d, so CI runs it.
///
/// Every net-attached test in this file used to die on its `expect` line
/// before reaching the code under test: the EuroCard template ships with no
/// nets at all, so `get_nets` came back empty. The via test could therefore
/// never have passed against this fixture in any environment, which is its own
/// evidence that the live suite has never actually run. Since no CI job runs
/// it even now, this guard is the only automatic check that the fixture still
/// satisfies what the live tests assume.
#[test]
fn the_live_fixture_satisfies_what_the_live_tests_assume() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/live_ipc.kicad_pcb");
    let tree = load_board(&fixture);

    let nets = konnect_sexp::net::collect_net_keys(&tree);
    assert!(
        nets.contains("GND"),
        "the fixture lost its GND net; the zone test's fill assertion needs \
         copper on GND inside its outline. Nets found: {nets:?}"
    );
    assert!(
        nets.iter().any(|net| net != "GND" && !net.is_empty()),
        "the fixture needs a second named net, so that a test picking 'the \
         first named net' is picking from more than one. Nets found: {nets:?}"
    );
    // This board carries a top-level net table and copper references nets by
    // id. A fixture re-saved by KiCad 10 would come back in the name-only
    // shape instead — which still works, but means the tracked file has been
    // overwritten by a live run rather than copied first.
    assert_eq!(
        konnect_sexp::net::net_ref_for_write(&tree, "GND"),
        Some(konnect_sexp::net::NetRef::ById {
            id: "1".into(),
            name: "GND".into()
        }),
        "the fixture is no longer in the legacy net-table shape — it looks like \
         a live run saved over the tracked file instead of a copy"
    );

    // `kicad_reports_an_empty_sheet_path_for_a_board_only_footprint` needs at
    // least two footprints with no `(path ...)` — that is the #452 collision,
    // and one would let the test pass on a case the defect never reached. The
    // template's four mounting holes supply them; a fixture that gained
    // schematic-backed footprints, or lost the holes, would leave that test
    // asserting nothing.
    let pathless = tree
        .find_all("footprint")
        .into_iter()
        .filter(|node| node.find("path").is_none())
        .count();
    assert!(
        pathless >= 2,
        "the fixture needs at least two footprints with no (path ...) for the \
         board-only identity test; found {pathless}"
    );

    // The template is not at the origin, and coordinates in the live tests are
    // absolute. A zone outside the outline is clipped to nothing, and a via
    // outside it is not the regression the via test means to catch.
    let outline = tree
        .find_all("gr_rect")
        .into_iter()
        .find(|rect| {
            rect.find("layer")
                .and_then(|layer| layer.get(1))
                .and_then(SexpNode::as_str)
                == Some("Edge.Cuts")
        })
        .expect("fixture has no Edge.Cuts rectangle");
    let start = outline.find("start").expect("outline start");
    let end = outline.find("end").expect("outline end");
    let (x1, y1) = (start.get_f64(1).unwrap(), start.get_f64(2).unwrap());
    let (x2, y2) = (end.get_f64(1).unwrap(), end.get_f64(2).unwrap());
    let (left, right) = (x1.min(x2), x1.max(x2));
    let (top, bottom) = (y1.min(y2), y1.max(y2));

    // A margin, so a point is not merely on the edge where fill is clipped.
    let inside =
        |(x, y): (f64, f64)| x > left + 2.0 && x < right - 2.0 && y > top + 2.0 && y < bottom - 2.0;
    assert!(
        inside(VIA_SPOT),
        "VIA_SPOT {VIA_SPOT:?} is not inside the fixture outline \
         ({left}, {top})-({right}, {bottom})"
    );
    for point in ZONE_OUTLINE {
        assert!(
            inside(point),
            "ZONE_OUTLINE vertex {point:?} is not inside the fixture outline \
             ({left}, {top})-({right}, {bottom})"
        );
    }

    // The zone test asserts KiCad computed fill polygons. add_zone sets island
    // removal to IRM_ALWAYS, so a pour reaching nothing else on its own net
    // has its entire fill discarded — the outline must cross GND copper.
    let zone_left = ZONE_OUTLINE.iter().map(|p| p.0).fold(f64::MAX, f64::min);
    let zone_right = ZONE_OUTLINE.iter().map(|p| p.0).fold(f64::MIN, f64::max);
    let zone_top = ZONE_OUTLINE.iter().map(|p| p.1).fold(f64::MAX, f64::min);
    let zone_bottom = ZONE_OUTLINE.iter().map(|p| p.1).fold(f64::MIN, f64::max);
    // How copper spells "GND" depends on the board format: an id on this
    // legacy fixture, the name itself once KiCad 10 has rewritten it.
    let gnd = match konnect_sexp::net::net_ref_for_write(&tree, "GND") {
        Some(konnect_sexp::net::NetRef::ById { id, .. }) => id,
        Some(konnect_sexp::net::NetRef::ByName(name)) => name,
        None => unreachable!("GND was asserted present above"),
    };
    let crosses_zone = tree.find_all("segment").into_iter().any(|segment| {
        let on_gnd = segment
            .find("net")
            .and_then(|net| net.get(1))
            .and_then(SexpNode::as_str)
            == Some(gnd.as_str());
        let touches = |node: Option<&SexpNode>| {
            node.and_then(|n| Some((n.get_f64(1)?, n.get_f64(2)?)))
                .is_some_and(|(x, y)| {
                    y > zone_top && y < zone_bottom && x > zone_left - 20.0 && x < zone_right + 20.0
                })
        };
        on_gnd && (touches(segment.find("start")) || touches(segment.find("end")))
    });
    assert!(
        crosses_zone,
        "no GND segment runs through the zone test's region \
         ({zone_left}, {zone_top})-({zone_right}, {zone_bottom}); its fill \
         would be removed as unconnected islands"
    );
}

/// The shared-reference fixture's own preconditions, checked without KiCad —
/// and, like the guard above, deliberately **not** `#[ignore]`d.
///
/// `board_only_shared_reference.kicad_pcb` exists to witness one fact: KiCad
/// leaves an unannotated footprint the library's `REF**`, so a board can carry
/// two board-only footprints with the same reference and nothing is wrong with
/// it. The unit tests in `konnect-core`'s `pcb_sync` build their duplicate pair
/// by varying only the KIID on the capture taken from this board, which is
/// honest only while the board really does carry the pair. Nothing in CI opens
/// KiCad, so this is the only automatic check that it still does.
#[test]
fn the_shared_reference_fixture_still_carries_a_duplicate_board_only_reference() {
    use std::collections::BTreeMap;

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/board_only_shared_reference.kicad_pcb");
    let tree = load_board(&fixture);

    let mut pathless_by_reference: BTreeMap<String, usize> = BTreeMap::new();
    let mut with_path = 0usize;
    for node in tree.find_all("footprint") {
        if node.find("path").is_some() {
            with_path += 1;
            continue;
        }
        let reference = node.find_all("property").into_iter().find_map(|property| {
            (property.get(1).and_then(SexpNode::as_str) == Some("Reference"))
                .then(|| property.get(2).and_then(SexpNode::as_str))
                .flatten()
                .map(str::to_string)
        });
        if let Some(reference) = reference {
            *pathless_by_reference.entry(reference).or_default() += 1;
        }
    }

    assert_eq!(
        with_path, 0,
        "every footprint on this fixture must be board-only; a schematic-backed one \
         would give the duplicate reference an adoption target and change the case"
    );
    assert_eq!(
        pathless_by_reference.get("REF**").copied(),
        Some(2),
        "the fixture must carry exactly two board-only footprints called REF**; \
         found {pathless_by_reference:?}"
    );
}
