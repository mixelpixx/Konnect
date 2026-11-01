//! `(text …)` elements must survive a load/save round-trip unchanged (#691).
//!
//! `Text` modelled only the string, `at`, `effects` and `uuid`, and — unlike
//! `Symbol` and `Sheet` — collected no unmodelled children, so every other child
//! eeschema wrote was parsed away and never written back. Today that is
//! `(exclude_from_sim no)`, which eeschema puts on every text element it saves.
//!
//! The fixture below is shaped like real eeschema 10 output: tab indent, the
//! attribute ahead of `at`, closing parens on their own line. It contains only
//! text elements so that this test is about text fidelity alone.

use konnect_schematic_editor::Schematic;

const SHEET: &str = r#"(kicad_sch
	(version 20250114)
	(generator "eeschema")
	(generator_version "10.0")
	(uuid "e8b100c6-72dd-423c-a210-6592c5e270e2")
	(paper "A4")
	(lib_symbols)
	(text "Power LED"
		(exclude_from_sim no)
		(at 25.4 530.86 0)
		(effects
			(font
				(size 2.54 2.54)
			)
		)
		(uuid "1368c36f-d21b-4cc5-ab34-4ea768732114")
	)
	(sheet_instances
		(path "/"
			(page "1")
		)
	)
	(embedded_fonts no)
)
"#;

/// The `(text …)` block, from its opening line to the closing paren at its own
/// indent. Scoping the comparison this way keeps the test about text fidelity:
/// root-level element ordering is a separate defect and is not asserted here.
fn text_block(sheet: &str) -> String {
    let start = sheet.find("\t(text ").expect("a text element");
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
fn text_element_round_trips_byte_identically() {
    let out = round_trip(SHEET);
    assert_eq!(text_block(&out), text_block(SHEET));
}

#[test]
fn exclude_from_sim_survives_and_keeps_its_position() {
    let out = round_trip(SHEET);
    assert_eq!(
        out.matches("(exclude_from_sim no)").count(),
        1,
        "attribute was dropped: {out}"
    );
    let attr = out
        .find("(exclude_from_sim no)")
        .expect("attribute present");
    let at = out.find("(at 25.4 530.86 0)").expect("at present");
    assert!(
        attr < at,
        "eeschema writes exclude_from_sim ahead of `at`, got:\n{out}"
    );
}

/// The defect was structural, not specific to one attribute: anything the model
/// does not know about was discarded. A future KiCAD attribute must survive too.
#[test]
fn unmodelled_children_are_preserved() {
    let src = SHEET.replace(
        "\t\t(exclude_from_sim no)\n",
        "\t\t(exclude_from_sim no)\n\t\t(some_future_attribute yes)\n",
    );
    let out = round_trip(&src);
    assert!(
        out.contains("(some_future_attribute yes)"),
        "unmodelled child was dropped:\n{out}"
    );
}

/// A text element that never had the attribute must not acquire one.
#[test]
fn absent_attribute_is_not_invented() {
    let src = SHEET.replace("\t\t(exclude_from_sim no)\n", "");
    let out = round_trip(&src);
    assert!(
        !out.contains("exclude_from_sim"),
        "attribute was invented:\n{out}"
    );
    assert_eq!(text_block(&out), text_block(&src));
}
