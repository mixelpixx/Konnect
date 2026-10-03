//! A saved board measured the way KiCad measures it (#688).
//!
//! The goldens under `fixtures/board_bounds/` are KiCad 10.0.5's own
//! `GetBoundingBox` (`BBM_ITEM_ONLY`) answers for every item of four checked-in
//! KiCad boards, captured over its IPC API with the board open in pcbnew. Each
//! item measured from the file must land where KiCad put it.

use konnect_sexp::bounds::{board_item_bounds, pad_bbox, Bbox, ItemClass};
use konnect_sexp::parse_sexp;
use std::collections::{BTreeMap, HashMap};

/// KiCad measures in integer nanometres and adds 1 nm to a track's width and
/// height; arc extrema then differ from the circumcentre computed here by a few
/// nanometres. Ten nanometres is far below anything a board can show.
const TOLERANCE_MM: f64 = 1e-5;

struct Golden {
    class: String,
    uuid: String,
    bbox: Bbox,
}

fn golden(name: &str) -> Vec<Golden> {
    let path = format!(
        "{}/tests/fixtures/board_bounds/{name}.tsv",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let nm = |i: usize| f[i].parse::<i64>().unwrap() as f64 / 1e6;
            Golden {
                class: f[0].to_owned(),
                uuid: f[1].to_owned(),
                bbox: (nm(2), nm(3), nm(2) + nm(4), nm(3) + nm(5)),
            }
        })
        .collect()
}

fn board(rel: &str) -> konnect_sexp::SexpNode {
    let path = format!("{}/tests/fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    parse_sexp(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn deviation(a: Bbox, b: Bbox) -> f64 {
    (a.0 - b.0)
        .abs()
        .max((a.1 - b.1).abs())
        .max((a.2 - b.2).abs())
        .max((a.3 - b.3).abs())
}

/// The golden class a measured item is reported under.
fn golden_class(class: ItemClass) -> &'static str {
    match class {
        ItemClass::Footprint => "footprint",
        ItemClass::Shape => "shape",
        ItemClass::Track => "track",
        ItemClass::Arc => "arc",
        ItemClass::Via => "via",
        ItemClass::Zone => "zone",
        ItemClass::TextBox => "textbox",
    }
}

/// Every item KiCad measured is measured here to within [`TOLERANCE_MM`], or
/// is one of the classes the file read names as not measured, in KiCad's count.
fn agrees_with_kicad(rel: &str, name: &str) {
    let tree = board(rel);
    let measured = board_item_bounds(&tree);
    assert_eq!(measured.skipped, 0, "{name}: unreadable items");
    let ours: HashMap<&str, (ItemClass, Bbox)> = measured
        .items
        .iter()
        .map(|item| (item.uuid.as_deref().unwrap(), (item.class, item.bbox)))
        .collect();

    let kicad = golden(name);
    let mut unmeasured: BTreeMap<&str, usize> = BTreeMap::new();
    let mut kicad_union: Option<Bbox> = None;
    let mut compared = 0;
    for item in kicad.iter().filter(|g| g.class != "pad") {
        let b = item.bbox;
        kicad_union = Some(match kicad_union {
            None => b,
            Some(u) => (u.0.min(b.0), u.1.min(b.1), u.2.max(b.2), u.3.max(b.3)),
        });
        match ours.get(item.uuid.as_str()) {
            Some((class, bbox)) => {
                assert_eq!(golden_class(*class), item.class, "{name} {}", item.uuid);
                let dev = deviation(*bbox, item.bbox);
                assert!(
                    dev <= TOLERANCE_MM,
                    "{name} {} {}: off by {dev} mm\n  kicad {:?}\n  file  {:?}",
                    item.class,
                    item.uuid,
                    item.bbox,
                    bbox
                );
                compared += 1;
            }
            None => *unmeasured.entry(item.class.as_str()).or_insert(0) += 1,
        }
    }
    assert_eq!(
        compared,
        measured.items.len(),
        "{name}: items KiCad did not list"
    );

    // What the file read did not measure is exactly what KiCad measured beyond it.
    let named: BTreeMap<&str, usize> = measured
        .not_measured
        .iter()
        .map(|(k, v)| (*k, *v))
        .collect();
    let expected: BTreeMap<&str, usize> = unmeasured
        .into_iter()
        .map(|(class, n)| match class {
            "text" => ("text", n),
            "dimension" => ("dimensions", n),
            other => panic!(
                "{name}: KiCad measured {n} {other} the file read neither measured nor named"
            ),
        })
        .collect();
    assert_eq!(named, expected, "{name}: not_measured");

    // Where nothing went unmeasured, the board's extent is KiCad's.
    if expected.is_empty() {
        let dev = deviation(measured.union().unwrap(), kicad_union.unwrap());
        assert!(dev <= TOLERANCE_MM, "{name}: union off by {dev} mm");
    }
}

#[test]
fn ecc83_agrees_with_kicad_item_by_item() {
    agrees_with_kicad("ecc83-pp.kicad_pcb", "ecc83-pp");
}

#[test]
fn pic_programmer_agrees_with_kicad_item_by_item() {
    agrees_with_kicad("pic_programmer.kicad_pcb", "pic_programmer");
}

#[test]
fn nfc_antenna_agrees_with_kicad_item_by_item() {
    agrees_with_kicad(
        "RoyalBlue54L-NFC-Antenna.kicad_pcb",
        "RoyalBlue54L-NFC-Antenna",
    );
}

#[test]
fn placement_fixture_agrees_with_kicad_item_by_item() {
    agrees_with_kicad("placement/placement_fixture.kicad_pcb", "placement_fixture");
}

/// Two Jetson footprints draw on `Dwgs.User`/`Eco1.User` beyond their copper and
/// silk; KiCad leaves those layers out of a footprint's box, by up to 2.95 mm here.
#[test]
fn annotation_layer_graphics_stay_out_of_a_footprint_box() {
    agrees_with_kicad(
        "board_bounds/bounds_annotation_layers.kicad_pcb",
        "bounds_annotation_layers",
    );
}

/// Two zones whose outlines are drawn with arc segments, from the Feather demo.
#[test]
fn zone_outline_arcs_reach_their_extremes() {
    agrees_with_kicad(
        "board_bounds/bounds_zone_arcs.kicad_pcb",
        "bounds_zone_arcs",
    );
}

/// Board text boxes and footprint text boxes, saved by KiCad 10.0.5 (#688):
/// borders of 0.2 and 1.5 mm, a hidden 1 mm border, no border, a box turned 30°
/// that KiCad writes as four corners, and text overflowing a small box. In
/// footprints, a text box on `F.Fab` turned 30°, one on `Dwgs.User`, and the
/// stock `Raytac_MDBT42Q` with its two on `Dwgs.User` and two keep-out zones.
#[test]
fn text_boxes_agree_with_kicad_item_by_item() {
    agrees_with_kicad(
        "board_bounds/bounds_text_boxes.kicad_pcb",
        "bounds_text_boxes",
    );
}

/// Every pad, including pic_programmer's two custom-shape solder jumpers and the
/// placement fixture's BGA, lands where KiCad put it.
#[test]
fn every_pad_agrees_with_kicad() {
    for (rel, name) in [
        ("ecc83-pp.kicad_pcb", "ecc83-pp"),
        ("pic_programmer.kicad_pcb", "pic_programmer"),
        (
            "RoyalBlue54L-NFC-Antenna.kicad_pcb",
            "RoyalBlue54L-NFC-Antenna",
        ),
        ("placement/placement_fixture.kicad_pcb", "placement_fixture"),
        (
            "board_bounds/bounds_annotation_layers.kicad_pcb",
            "bounds_annotation_layers",
        ),
        (
            "board_bounds/bounds_text_boxes.kicad_pcb",
            "bounds_text_boxes",
        ),
    ] {
        let kicad: HashMap<String, Bbox> = golden(name)
            .into_iter()
            .filter(|g| g.class == "pad")
            .map(|g| (g.uuid, g.bbox))
            .collect();
        let tree = board(rel);
        let mut compared = 0;
        for fp in tree.find_all("footprint") {
            let at = fp.find("at").unwrap();
            let (fx, fy) = (at.get_f64(1).unwrap(), at.get_f64(2).unwrap());
            let rot = at.get_f64(3).unwrap_or(0.0);
            for pad in fp.find_all("pad") {
                let uuid = pad.find_str("uuid").unwrap();
                let ours = pad_bbox(pad, fx, fy, rot).expect("pad measured");
                let theirs = kicad[uuid];
                let dev = deviation(ours, theirs);
                assert!(
                    dev <= TOLERANCE_MM,
                    "{name} pad {uuid}: off by {dev} mm\n  kicad {theirs:?}\n  file  {ours:?}"
                );
                compared += 1;
            }
        }
        assert_eq!(compared, kicad.len(), "{name}: pads KiCad did not list");
    }
}

#[test]
fn malformed_optional_geometry_is_skipped_instead_of_defaulted() {
    for malformed in [
        // A present footprint angle is not the same thing as KiCad omitting a
        // zero angle.
        r#"(footprint "F" (at 10 20 nope) (uuid "fp"))"#,
        // The same distinction applies to a pad angle.
        r#"(footprint "F" (at 10 20) (uuid "fp")
             (pad "1" smd rect (at 0 0 nope) (size 1 1) (layers "F.Cu")))"#,
        // A malformed present ratio must not become the default 0.25 ratio.
        r#"(footprint "F" (at 10 20) (uuid "fp")
             (pad "1" smd roundrect (at 0 0) (size 1 1) (layers "F.Cu")
               (roundrect_rratio nope)))"#,
        // A malformed present point size must not become the default 1 mm.
        r#"(footprint "F" (at 10 20) (uuid "fp")
             (point (at 12 22) (size nope) (layer "F.Cu")))"#,
        // A malformed per-layer via size must not leave the base diameter.
        r#"(via (at 5 5) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu")
             (padstack (mode front_inner_back) (layer "Inner" (size nope)))
             (uuid "v"))"#,
        // A text box without its second corner has no box.
        r#"(gr_text_box "t" (start 1 1) (layer "F.SilkS") (uuid "tb")
             (stroke (width 0.1) (type solid)))"#,
    ] {
        let tree = parse_sexp(&format!("(kicad_pcb {malformed})")).unwrap();
        let bounds = board_item_bounds(&tree);
        assert!(bounds.items.is_empty(), "malformed item was measured");
        assert_eq!(bounds.skipped, 1, "malformed item was not disclosed");
    }

    // Actually omitted optional values still receive their declared defaults.
    let tree = parse_sexp(
        r#"(kicad_pcb
             (footprint "F" (at 10 20) (uuid "fp")
               (pad "1" smd roundrect (at 0 0) (size 1 1) (layers "F.Cu"))))"#,
    )
    .unwrap();
    let bounds = board_item_bounds(&tree);
    assert_eq!(bounds.items.len(), 1);
    assert_eq!(bounds.skipped, 0);

    // An omitted point size is KiCad's 1 mm, and a via layer without a size
    // keeps the via's own diameter.
    let tree = parse_sexp(
        r#"(kicad_pcb
             (footprint "F" (at 10 20) (uuid "fp") (point (at 40 50) (layer "F.Cu")))
             (via (at 5 5) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu")
               (padstack (mode front_inner_back) (layer "Inner")) (uuid "v")))"#,
    )
    .unwrap();
    let bounds = board_item_bounds(&tree);
    assert_eq!(bounds.skipped, 0);
    let boxes: Vec<Bbox> = bounds.items.iter().map(|item| item.bbox).collect();
    assert_eq!(boxes, [(9.75, 19.75, 40.5, 50.5), (4.7, 4.7, 5.3, 5.3)]);
}
