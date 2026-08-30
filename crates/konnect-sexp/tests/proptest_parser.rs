//! Property-based tests for the S-expression parser and editor.
//!
//! Motivation: the predecessor project (and Konnect's own PR #9) both found
//! panics on malformed input in parsing code. These properties pin the two
//! guarantees the rest of the codebase relies on:
//!
//! 1. `parse_sexp` NEVER panics — malformed schematic files must surface as
//!    `Err`, not a crash, because tool handlers feed it user files verbatim.
//! 2. Parsing is total and deterministic over well-formed input.

use konnect_sexp::{
    board::ZoneOutlineElement, parse_sexp, writer::apply_edits, SexpEdit, SexpNode,
};
use proptest::prelude::*;

// ─── Strategies ──────────────────────────────────────────────────────────────

/// Atoms KiCAD actually emits: identifiers, numbers, uuids, yes/no.
fn atom_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z_][a-z0-9_]{0,15}",
        r"-?[0-9]{1,4}(\.[0-9]{1,4})?",
        Just("yes".to_string()),
        Just("no".to_string()),
    ]
}

/// Quoted-string content (no embedded quotes/backslashes — matching what
/// KiCAD writes for names/values).
fn string_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 ._:/+-]{0,24}"
}

/// Recursively generate well-formed s-expression text.
fn sexp_text_strategy() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        atom_strategy(),
        string_strategy().prop_map(|s| format!("\"{}\"", s)),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        (atom_strategy(), prop::collection::vec(inner, 0..6)).prop_map(|(head, children)| {
            let mut out = format!("({}", head);
            for c in children {
                out.push(' ');
                out.push_str(&c);
            }
            out.push(')');
            out
        })
    })
}

/// Scalar atoms as a hostile file might carry them: ordinary numbers, but
/// also the strings `f64::parse` happily accepts and geometry must reject.
fn scalar_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => r"-?[0-9]{1,4}(\.[0-9]{1,4})?",
        1 => Just("NaN".to_string()),
        1 => Just("inf".to_string()),
        1 => Just("-inf".to_string()),
        1 => Just("0".to_string()),
        1 => Just("junk".to_string()),
    ]
}

/// A `(segment …)` with every field individually present, absent, or hostile —
/// the shapes a truncated or hand-mangled board file produces.
fn segment_strategy() -> impl Strategy<Value = String> {
    (
        prop::option::of((scalar_strategy(), scalar_strategy())),
        prop::option::of((scalar_strategy(), scalar_strategy())),
        prop::option::of(scalar_strategy()),
        prop::option::of(Just("F.Cu".to_string())),
    )
        .prop_map(|(start, end, width, layer)| {
            let mut s = String::from("(segment");
            if let Some((x, y)) = start {
                s.push_str(&format!(" (start {x} {y})"));
            }
            if let Some((x, y)) = end {
                s.push_str(&format!(" (end {x} {y})"));
            }
            if let Some(w) = width {
                s.push_str(&format!(" (width {w})"));
            }
            if let Some(l) = layer {
                s.push_str(&format!(" (layer \"{l}\")"));
            }
            s.push(')');
            s
        })
}

// ─── Properties ──────────────────────────────────────────────────────────────

proptest! {
    /// Arbitrary bytes must never panic the parser. (Result value is
    /// irrelevant — only the absence of a crash matters.)
    #[test]
    fn parse_never_panics_on_arbitrary_input(input in ".{0,256}") {
        let _ = parse_sexp(&input);
    }

    /// Arbitrary *bracket soup* — the adversarial case for a recursive
    /// descent parser (unbalanced parens, quotes cut mid-string).
    #[test]
    fn parse_never_panics_on_bracket_soup(input in r#"[()" a-z0-9\\]{0,256}"#) {
        let _ = parse_sexp(&input);
    }

    /// Every generated well-formed s-expression parses successfully.
    #[test]
    fn generated_sexp_always_parses(text in sexp_text_strategy()) {
        let parsed = parse_sexp(&text);
        prop_assert!(parsed.is_ok(), "failed to parse generated sexp: {}", text);
    }

    /// Parsing is deterministic: same input, same tree.
    #[test]
    fn parse_is_deterministic(text in sexp_text_strategy()) {
        let a = parse_sexp(&text).unwrap();
        let b = parse_sexp(&text).unwrap();
        prop_assert_eq!(a, b);
    }

    /// A generated list's head round-trips through the accessor API without
    /// panicking, whatever the shape.
    #[test]
    fn accessors_never_panic(text in sexp_text_strategy()) {
        if let Ok(node) = parse_sexp(&text) {
            let _ = node.head();
            let _ = node.children();
            let _ = node.as_str();
            let _ = node.find("symbol");
            let _ = node.find_all("wire");
            let _ = node.get(0);
            let _ = node.get_f64(1);
        }
    }

    /// Board scans are total over arbitrary well-formed trees: no panic, and
    /// no returned item ever violates the invariants `board::Track` promises —
    /// finite coordinates and strictly positive finite width — however wrong
    /// the input's shape is.
    #[test]
    fn board_scans_are_total_over_arbitrary_trees(text in sexp_text_strategy()) {
        if let Ok(tree) = parse_sexp(&text) {
            for t in &konnect_sexp::board::tracks(&tree).items {
                prop_assert!(t.width.is_finite() && t.width > 0.0);
                for c in [t.x1, t.y1, t.x2, t.y2] {
                    prop_assert!(c.is_finite());
                }
            }
            for v in &konnect_sexp::board::vias(&tree).items {
                prop_assert!(v.size.is_finite() && v.size > 0.0);
                prop_assert!(v.drill.is_finite() && v.drill > 0.0);
                prop_assert!(!v.layers.is_empty());
            }
            for z in &konnect_sexp::board::zones(&tree).items {
                let defining_coordinate_count: usize = z
                    .elements
                    .iter()
                    .map(|element| match element {
                        ZoneOutlineElement::Point(_) => 1,
                        ZoneOutlineElement::Arc { .. } => 3,
                    })
                    .sum();
                prop_assert!(defining_coordinate_count >= 3);
                prop_assert!(!z.layers.is_empty());
            }
            if let Some((x0, y0, x1, y1)) = konnect_sexp::board::board_outline_bbox(&tree) {
                prop_assert!(x0 <= x1 && y0 <= y1);
            }
        }
    }

    /// Nothing vanishes silently: over boards made of arbitrarily mangled
    /// segments, every segment node is either parsed or counted as skipped —
    /// and the parsed ones all hold the width/coordinate invariants.
    #[test]
    fn every_segment_is_parsed_or_counted(
        segments in prop::collection::vec(segment_strategy(), 0..12)
    ) {
        let board = format!("(kicad_pcb {})", segments.join(" "));
        let tree = parse_sexp(&board).unwrap();
        let scan = konnect_sexp::board::tracks(&tree);
        prop_assert_eq!(scan.items.len() + scan.skipped, segments.len());
        for t in &scan.items {
            prop_assert!(t.width.is_finite() && t.width > 0.0);
            for c in [t.x1, t.y1, t.x2, t.y2] {
                prop_assert!(c.is_finite());
            }
        }
    }

    /// apply_edits length math: replacing [start, end) with `rep` yields
    /// len - (end - start) + rep.len(), for any in-bounds ASCII range.
    /// (ASCII content keeps every offset a valid char boundary.)
    #[test]
    fn apply_edits_length_invariant(
        content in "[a-z0-9 ()]{1,64}",
        a in 0usize..64,
        b in 0usize..64,
        rep in "[a-z ]{0,16}",
    ) {
        let len = content.len();
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        prop_assume!(end <= len);
        let expected = len - (end - start) + rep.len();
        let out = apply_edits(
            content,
            vec![SexpEdit { start, end, replacement: rep }],
        );
        prop_assert_eq!(out.len(), expected);
    }

    /// Two non-overlapping edits produce the same result regardless of the
    /// order they're supplied in (the writer sorts internally).
    #[test]
    fn apply_edits_order_independent(
        content in "[a-z0-9 ()]{20,64}",
        rep1 in "[a-z]{0,8}",
        rep2 in "[a-z]{0,8}",
    ) {
        let e1 = SexpEdit { start: 2, end: 6, replacement: rep1 };
        let e2 = SexpEdit { start: 10, end: 14, replacement: rep2 };
        let forward = apply_edits(content.clone(), vec![e1.clone(), e2.clone()]);
        let reverse = apply_edits(content, vec![e2, e1]);
        prop_assert_eq!(forward, reverse);
    }
}

/// Deterministic sanity check that the generated-corpus shape resembles a
/// real KiCAD file header (guards the strategies themselves).
#[test]
fn strategy_shape_smoke() {
    let sample = "(kicad_sch (version 20250610) (generator \"konnect\") (uuid \"abc\"))";
    let node = parse_sexp(sample).unwrap();
    assert_eq!(node.head(), Some("kicad_sch"));
    assert!(matches!(node, SexpNode::List(_)));
}
