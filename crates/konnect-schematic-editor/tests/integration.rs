use konnect_schematic_editor::{
    sexp::{parser, writer},
    Error, Schematic,
};

// ---- S-expression parser round-trip ----------------------------------------

#[test]
fn parse_simple_list() {
    let node = parser::parse("(kicad_sch (version 20231120))").unwrap();
    assert_eq!(node.tag(), Some("kicad_sch"));
    let ver = node.find("version").unwrap();
    assert_eq!(ver.value(), Some("20231120"));
}

#[test]
fn parse_quoted_string() {
    let node = parser::parse(r#"(property "Reference" "C1")"#).unwrap();
    assert_eq!(node.tag(), Some("property"));
    let args = node.scalar_args();
    assert_eq!(args, vec!["Reference", "C1"]);
}

#[test]
fn parse_escaped_string() {
    let node = parser::parse(r#"(text "hello \"world\"")"#).unwrap();
    assert_eq!(node.value(), Some("hello \"world\""));
}

#[test]
fn writer_round_trip() {
    let input = r#"(kicad_sch (version 20231120) (generator "kicad"))"#;
    let node = parser::parse(input).unwrap();
    let out = writer::write(&node);
    // Re-parse and check structure is preserved
    let node2 = parser::parse(out.trim()).unwrap();
    assert_eq!(node2.tag(), Some("kicad_sch"));
    assert_eq!(node2.find("version").unwrap().value(), Some("20231120"));
    assert_eq!(node2.find("generator").unwrap().value(), Some("kicad"));
}

#[test]
fn parse_nested() {
    let src = r#"(symbol (lib_id "Device:R") (at 100.33 88.9 90) (unit 1))"#;
    let node = parser::parse(src).unwrap();
    assert_eq!(node.tag(), Some("symbol"));
    assert_eq!(node.get_value("lib_id"), Some("Device:R"));
    let at = node.find("at").unwrap();
    let scalars = at.scalar_args();
    assert_eq!(scalars, vec!["100.33", "88.9", "90"]);
}

// ---- Schematic from string --------------------------------------------------

fn minimal_sch() -> &'static str {
    r#"(kicad_sch
  (version 20231120)
  (generator "test")
  (uuid "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
  (paper "A4")

  (symbol
    (lib_id "Device:R")
    (at 100 100 0)
    (unit 1)
    (in_bom yes)
    (on_board yes)
    (dnp no)
    (uuid "11111111-0000-0000-0000-000000000001")
    (property "Reference" "R1"
      (at 100 95 0)
    )
    (property "Value" "10k"
      (at 100 105 0)
    )
    (property "Footprint" "Resistor_SMD:R_0402"
      (at 100 110 0)
    )
    (property "Datasheet" ""
      (at 100 115 0)
    )
  )

  (symbol
    (lib_id "Device:C")
    (at 150 100 0)
    (unit 1)
    (in_bom yes)
    (on_board yes)
    (dnp no)
    (uuid "22222222-0000-0000-0000-000000000002")
    (property "Reference" "C1"
      (at 150 95 0)
    )
    (property "Value" "100nF"
      (at 150 105 0)
    )
    (property "Footprint" "Capacitor_SMD:C_0402"
      (at 150 110 0)
    )
    (property "Datasheet" ""
      (at 150 115 0)
    )
  )

  (wire
    (pts
      (xy 90 100)
      (xy 100 100)
    )
    (stroke (width 0) (type default))
    (uuid "33333333-0000-0000-0000-000000000003")
  )

  (label "VCC"
    (at 90 100 180)
    (uuid "44444444-0000-0000-0000-000000000004")
  )

  (junction
    (at 100 100)
    (diameter 0)
    (uuid "55555555-0000-0000-0000-000000000005")
  )
)"#
}

fn load_minimal() -> Schematic {
    // Each call gets its own tempfile. The file is deleted when this function
    // returns — `Schematic::load` has already pulled the content into memory.
    // Previously this used a fixed shared path, which raced under parallel
    // test execution and caused spurious "Unexpected end of input" parse
    // errors.
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .expect("create tempfile");
    std::fs::write(tmp.path(), minimal_sch()).unwrap();
    Schematic::load(tmp.path()).unwrap()
}

/// Create a persistent tempfile seeded with the minimal schematic.
/// Callers mutate + overwrite + re-load, so they must keep the returned
/// `NamedTempFile` alive for the duration of the test.
fn fresh_minimal_file() -> tempfile::NamedTempFile {
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .expect("create tempfile");
    std::fs::write(tmp.path(), minimal_sch()).unwrap();
    tmp
}

#[test]
fn load_symbol_count() {
    let sch = load_minimal();
    assert_eq!(sch.symbols.len(), 2);
}

#[test]
fn load_wire_count() {
    let sch = load_minimal();
    assert_eq!(sch.wires.len(), 1);
}

#[test]
fn load_label_count() {
    let sch = load_minimal();
    assert_eq!(sch.labels.len(), 1);
}

#[test]
fn load_junction_count() {
    let sch = load_minimal();
    assert_eq!(sch.junctions.len(), 1);
}

#[test]
fn symbol_by_reference() {
    let sch = load_minimal();
    let r1 = sch.symbols.by_reference("R1").expect("R1 not found");
    assert_eq!(r1.lib_id, "Device:R");
    assert_eq!(r1.value_str(), Some("10k"));
    assert_eq!(r1.footprint(), Some("Resistor_SMD:R_0402"));
}

#[test]
fn symbol_properties() {
    let sch = load_minimal();
    let c1 = sch.symbols.by_reference("C1").expect("C1 not found");
    assert_eq!(c1.property("Value"), Some("100nF"));
    assert_eq!(c1.property("Footprint"), Some("Capacitor_SMD:C_0402"));
}

#[test]
fn symbol_position() {
    let sch = load_minimal();
    let r1 = sch.symbols.by_reference("R1").expect("R1 not found");
    assert_eq!(r1.position(), (100.0, 100.0));
}

#[test]
fn symbol_booleans() {
    let sch = load_minimal();
    let r1 = sch.symbols.by_reference("R1").unwrap();
    assert_eq!(r1.in_bom, true);
    assert_eq!(r1.on_board, true);
    assert_eq!(r1.dnp, false);
}

#[test]
fn mutate_property_round_trips() {
    let tmp = fresh_minimal_file();

    {
        let mut sch = Schematic::load(tmp.path()).unwrap();
        let r1 = sch.symbols.by_reference_mut("R1").unwrap();
        r1.set_value_str("4.7k");
        r1.dnp = true;
        sch.overwrite().unwrap();
    }

    let sch2 = Schematic::load(tmp.path()).unwrap();
    let r1 = sch2.symbols.by_reference("R1").unwrap();
    assert_eq!(r1.value_str(), Some("4.7k"));
    assert_eq!(r1.dnp, true);
}

#[test]
fn set_all_dnp() {
    let tmp = fresh_minimal_file();

    let mut sch = Schematic::load(tmp.path()).unwrap();
    sch.symbols.set_all_dnp(true);
    for sym in &sch.symbols {
        assert_eq!(sym.dnp, true);
    }
}

#[test]
fn reference_startswith_filter() {
    let sch = load_minimal();
    let caps = sch.symbols.reference_startswith("C");
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].reference(), Some("C1"));

    let resistors = sch.symbols.reference_startswith("R");
    assert_eq!(resistors.len(), 1);
}

#[test]
fn wire_properties() {
    let sch = load_minimal();
    let w = sch.wires.get(0).unwrap();
    assert_eq!(w.start, (90.0, 100.0));
    assert_eq!(w.end, (100.0, 100.0));
    assert!(w.is_horizontal());
    assert!(!w.is_vertical());
    assert!((w.length() - 10.0).abs() < 1e-9);
}

#[test]
fn wire_touches() {
    let sch = load_minimal();
    let w = sch.wires.get(0).unwrap();
    assert!(w.touches(90.0, 100.0));
    assert!(w.touches(100.0, 100.0));
    assert!(!w.touches(95.0, 100.0));
}

#[test]
fn spatial_within_circle() {
    let sch = load_minimal();
    // R1 is at (100,100), C1 at (150,100) — radius 10 from R1 should find R1 only
    let found = sch.within_circle(100.0, 100.0, 10.0);
    let sym_count = found
        .iter()
        .filter(|e| matches!(e, konnect_schematic_editor::LocatedElement::Symbol(_)))
        .count();
    assert_eq!(sym_count, 1);
}

#[test]
fn spatial_within_rectangle() {
    let sch = load_minimal();
    // Box that covers both symbols
    let found = sch.within_rectangle(80.0, 80.0, 200.0, 120.0);
    let sym_count = found
        .iter()
        .filter(|e| matches!(e, konnect_schematic_editor::LocatedElement::Symbol(_)))
        .count();
    assert_eq!(sym_count, 2);
}

#[test]
fn add_wire_and_save() {
    let tmp = fresh_minimal_file();

    let wire_count_before;
    {
        let sch = Schematic::load(tmp.path()).unwrap();
        wire_count_before = sch.wires.len();
    }

    {
        let mut sch = Schematic::load(tmp.path()).unwrap();
        sch.add_wire(100.0, 100.0, 150.0, 100.0);
        sch.overwrite().unwrap();
    }

    let sch2 = Schematic::load(tmp.path()).unwrap();
    assert_eq!(sch2.wires.len(), wire_count_before + 1);
}

#[test]
fn add_label_and_save() {
    let tmp = fresh_minimal_file();

    {
        let mut sch = Schematic::load(tmp.path()).unwrap();
        sch.add_label("GND", 100.0, 110.0);
        sch.overwrite().unwrap();
    }

    let sch2 = Schematic::load(tmp.path()).unwrap();
    assert!(sch2.labels.value_contains("GND").len() > 0);
}

#[test]
fn overwrite_rejects_a_stale_loaded_revision() {
    let tmp = fresh_minimal_file();
    let mut schematic = Schematic::load(tmp.path()).unwrap();
    let external_revision = minimal_sch().replace("10k", "22k");
    std::fs::write(tmp.path(), &external_revision).unwrap();
    schematic
        .symbols
        .by_reference_mut("R1")
        .unwrap()
        .set_value_str("4.7k");

    let error = schematic.overwrite().unwrap_err();

    assert!(matches!(error, Error::Conflict(_)));
    assert_eq!(
        std::fs::read_to_string(tmp.path()).unwrap(),
        external_revision
    );
}

#[test]
fn diff_detects_value_change() {
    let tmp = fresh_minimal_file();

    let mut sch = Schematic::load(tmp.path()).unwrap();
    sch.symbols
        .by_reference_mut("R1")
        .unwrap()
        .set_value_str("1k");

    let cs = sch.diff_against_disk().unwrap();
    assert!(!cs.is_empty());
    let summary = cs.summary();
    assert!(summary.contains("R1"));
    assert!(summary.contains("Value"));
}

#[test]
fn changeset_display() {
    use konnect_schematic_editor::ChangeSet;
    let mut cs = ChangeSet::new();
    cs.record("R1.Value: \"10k\" → \"4.7k\"");
    cs.record("R1: dnp false → true");
    assert_eq!(cs.len(), 2);
    assert!(cs.summary().contains("R1.Value"));
}
