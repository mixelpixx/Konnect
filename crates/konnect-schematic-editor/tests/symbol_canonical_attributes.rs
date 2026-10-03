//! A symbol instance must be written back in eeschema's own attribute form (#692).
//!
//! Two defects made every symbol in a sheet differ after any whole-file write:
//!
//! 1. `(fields_autoplaced yes)` was emitted as the bare token
//!    `(fields_autoplaced)`. eeschema writes the value, and omits the token
//!    entirely when fields are not autoplaced — it never writes `no`.
//! 2. `body_style` and `in_pos_files` were not modelled, so they fell into
//!    `raw_sub_nodes` and were replayed *after* `uuid` and every `property`
//!    instead of in their canonical positions.
//!
//! The fixture is shaped like real eeschema 10 output. Comparisons are scoped to
//! the symbol block: root-level element ordering is a separate defect.

use konnect_schematic_editor::Schematic;

const SHEET: &str = r#"(kicad_sch
	(version 20250114)
	(generator "eeschema")
	(generator_version "10.0")
	(uuid "e8b100c6-72dd-423c-a210-6592c5e270e2")
	(paper "A4")
	(lib_symbols)
	(symbol
		(lib_id "Device:R")
		(at 100 100 0)
		(unit 1)
		(body_style 1)
		(exclude_from_sim no)
		(in_bom yes)
		(on_board yes)
		(in_pos_files yes)
		(dnp no)
		(fields_autoplaced yes)
		(uuid "5368c36f-d21b-4cc5-ab34-4ea768732114")
		(property "Reference" "R1"
			(at 100 96 0)
			(effects
				(font
					(size 1.27 1.27)
				)
			)
		)
		(instances
			(project "board"
				(path "/e8b100c6-72dd-423c-a210-6592c5e270e2"
					(reference "R1")
					(unit 1)
				)
			)
		)
	)
	(sheet_instances
		(path "/"
			(page "1")
		)
	)
	(embedded_fonts no)
)
"#;

/// The `(symbol …)` block, from its opening line to the closing paren at its
/// own indent.
fn symbol_block(sheet: &str) -> String {
    let start = sheet.find("\t(symbol\n").expect("a symbol instance");
    let end = sheet[start..].find("\n\t)").expect("its closing paren") + start + 3;
    sheet[start..end].to_owned()
}

fn round_trip(src: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.kicad_sch");
    let output = dir.path().join("out.kicad_sch");
    std::fs::write(&input, src).expect("write fixture");
    Schematic::load(&input)
        .expect("load")
        .save(&output)
        .expect("save");
    std::fs::read_to_string(&output).expect("read back")
}

#[test]
fn symbol_block_round_trips_byte_identically() {
    let out = round_trip(SHEET);
    assert_eq!(symbol_block(&out), symbol_block(SHEET));
}

#[test]
fn fields_autoplaced_keeps_its_value() {
    let out = round_trip(SHEET);
    assert!(
        out.contains("(fields_autoplaced yes)"),
        "expected the KiCAD 8+ form:\n{out}"
    );
}

/// Konnect itself wrote the bare token until this fix, so files it has already
/// touched contain one. Reading such a file must recover the flag, not lose it.
#[test]
fn bare_fields_autoplaced_is_read_as_set() {
    let src = SHEET.replace("(fields_autoplaced yes)", "(fields_autoplaced)");
    let out = round_trip(&src);
    assert!(
        out.contains("(fields_autoplaced yes)"),
        "a bare token should round-trip as an explicit yes:\n{out}"
    );
}

#[test]
fn attributes_keep_their_canonical_positions() {
    let out = round_trip(SHEET);
    let block = symbol_block(&out);
    let index = |needle: &str| {
        block
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing"))
    };

    // eeschema's order: unit, body_style, exclude_from_sim, in_bom, on_board,
    // in_pos_files, dnp, fields_autoplaced, uuid, then the properties.
    let order = [
        "(unit 1)",
        "(body_style 1)",
        "(exclude_from_sim no)",
        "(in_bom yes)",
        "(on_board yes)",
        "(in_pos_files yes)",
        "(dnp no)",
        "(fields_autoplaced yes)",
        "(uuid ",
        "(property ",
    ];
    for pair in order.windows(2) {
        assert!(
            index(pair[0]) < index(pair[1]),
            "{} must come before {} in:\n{block}",
            pair[0],
            pair[1]
        );
    }
}

/// Attributes the file does not carry must not be invented on write.
#[test]
fn absent_attributes_are_not_invented() {
    let src = SHEET
        .replace("\t\t(body_style 1)\n", "")
        .replace("\t\t(in_pos_files yes)\n", "")
        .replace("\t\t(fields_autoplaced yes)\n", "");
    let out = round_trip(&src);
    for absent in ["body_style", "in_pos_files", "fields_autoplaced"] {
        assert!(!out.contains(absent), "{absent} was invented:\n{out}");
    }
    assert_eq!(symbol_block(&out), symbol_block(&src));
}
