//! Reading the `(layers …)` stackup table of a `.kicad_pcb`.
//!
//! Layer entries are the one table in the board file that is *not* keyed by a
//! tag. Everything else is `(tag …)` and can be found with [`SexpNode::find_all`];
//! a layer is `(0 "F.Cu" signal)`, whose head is the ordinal itself. There is no
//! tag to match on, so the entries have to be read by shape: every list child of
//! the `(layers …)` node is a layer.
//!
//! ```
//! use konnect_sexp::{parse_sexp, layers};
//! let board = parse_sexp(r#"(kicad_pcb (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (9 "F.Adhes" user)))"#).unwrap();
//! let stack = layers::layers(&board);
//! assert_eq!(stack.len(), 3);
//! assert_eq!(stack[0].name, "F.Cu");
//! assert_eq!(layers::copper(&stack).len(), 2);
//! ```

use crate::parser::SexpNode;
use std::collections::BTreeSet;

/// One entry of the board stackup.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    /// Ordinal as written in the file. Not an index: KiCad leaves gaps
    /// (`0 F.Cu`, `2 B.Cu`, `9 F.Adhes` …) and inner copper occupies 1..=30.
    pub id: i32,
    /// Canonical name — `F.Cu`, `In1.Cu`, `Edge.Cuts`.
    pub name: String,
    /// `signal`, `power`, `mixed`, `jumper` or `user`.
    pub kind: String,
    /// Optional user-facing rename, e.g. `(0 "F.Cu" signal "Top Layer")`.
    pub user_name: Option<String>,
}

impl Layer {
    /// Copper is decided by the canonical name, not by `kind`. See
    /// [`is_copper_name`].
    pub fn is_copper(&self) -> bool {
        is_copper_name(&self.name)
    }
}

/// Is this canonical layer name a copper layer?
///
/// KiCad marks copper with four different kinds (`signal`, `power`, `mixed`,
/// `jumper`) and a board that uses `power` for a plane would be undercounted
/// by a kind allow-list. The `.Cu` suffix is the invariant — which is why this
/// takes a name: a layer read over KiCad's IPC API has no kind at all.
pub fn is_copper_name(name: &str) -> bool {
    name.ends_with(".Cu")
}

/// Read the stackup from a parsed board. Empty if there is no `(layers …)`.
pub fn layers(board: &SexpNode) -> Vec<Layer> {
    board.find("layers").map(table_layers).unwrap_or_default()
}

/// The entries of a parsed `(layers …)` table itself.
pub fn table_layers(node: &SexpNode) -> Vec<Layer> {
    node.children()
        .unwrap_or(&[])
        .iter()
        // Skips the head atom (`layers`), which is a child like any other, and
        // any stray atom: a layer is always a list.
        .filter(|child| child.children().is_some())
        .filter_map(layer_from)
        .collect()
}

/// Copper layers only, in file order — the "how many layers is this board"
/// answer, and what a fab house quotes on.
pub fn copper(stack: &[Layer]) -> Vec<&Layer> {
    stack.iter().filter(|l| l.is_copper()).collect()
}

/// How many copper layers a parsed board declares — the number a fab house
/// quotes on.
///
/// This is the one place that answer is computed, so `get_board_info`,
/// `validate_for_manufacturing` and `estimate_cost` cannot disagree (#461:
/// the manufacturing tools counted the substring `signal)` in the file text,
/// which misses every `power`, `mixed` and `jumper` copper layer and quoted a
/// six-layer board as two-layer). A board with no `(layers …)` table has zero
/// copper layers; callers decide what that means, never this function.
pub fn copper_layer_count(board: &SexpNode) -> usize {
    copper(&layers(board)).len()
}

/// The fixed layer names, i.e. every `BoardLayer` variant that is neither
/// `In<n>.Cu` nor `User.<n>` nor a sentinel.
const FIXED_NAMES: &[&str] = &[
    "F.Cu",
    "B.Cu",
    "B.Adhes",
    "F.Adhes",
    "B.Paste",
    "F.Paste",
    "B.SilkS",
    "F.SilkS",
    "B.Mask",
    "F.Mask",
    "Dwgs.User",
    "Cmts.User",
    "Eco1.User",
    "Eco2.User",
    "Edge.Cuts",
    "Margin",
    "B.CrtYd",
    "F.CrtYd",
    "B.Fab",
    "F.Fab",
    "Rescue",
];

/// Highest `In<n>.Cu` / `User.<n>` KiCad defines (`BL_In30_Cu`, `BL_User_45`).
const MAX_INNER_COPPER: u32 = 30;
const MAX_USER: u32 = 45;

/// Is this a layer name KiCad will accept in a board file?
///
/// KiCad does not take arbitrary layer names: the set is closed, and it is the
/// `BoardLayer` enum of the official API protos
/// (`konnect-ipc/proto/board/board_types.proto`), whose variant names map onto
/// file names by dropping the `BL_` prefix and turning the remaining `_` into a
/// `.` — `BL_F_Cu` → `F.Cu`, `BL_User_1` → `User.1`.
///
/// A board carrying a name outside that set is **rejected outright** by KiCad —
/// it does not degrade, the file simply will not open. `layers_canonical_names`
/// in konnect-core keeps this in step with the enum.
pub fn is_canonical_name(name: &str) -> bool {
    if FIXED_NAMES.contains(&name) {
        return true;
    }
    if inner_copper_index(name).is_some() {
        return true;
    }
    if let Some(n) = name.strip_prefix("User.") {
        return matches!(n.parse::<u32>(), Ok(n) if (1..=MAX_USER).contains(&n))
            && !n.starts_with('0');
    }
    false
}

/// The copper stack a footprint flips through: `F.Cu`, `In1.Cu`..`In<N-2>.Cu`
/// and `B.Cu`, which is the only shape KiCad gives a board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopperStack {
    count: u32,
}

impl CopperStack {
    /// The stack of a board with `count` copper layers: even, 2 to 32, as
    /// KiCad allows.
    pub fn new(count: u32) -> Result<Self, FlipLayerError> {
        if !count.is_multiple_of(2) || !(2..=MAX_INNER_COPPER + 2).contains(&count) {
            return Err(FlipLayerError::IrregularStack(format!(
                "{count} copper layers"
            )));
        }
        Ok(Self { count })
    }

    /// Build the stack from a board's layer names; non-copper names are
    /// ignored. Any other copper set is refused, since mirroring through it
    /// would be a guess.
    pub fn from_layer_names<'a>(
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, FlipLayerError> {
        let copper = names
            .into_iter()
            .filter(|name| is_copper_name(name))
            .collect::<BTreeSet<_>>();
        let count = copper.len() as u32;
        let regular = copper.iter().all(|name| {
            matches!(*name, "F.Cu" | "B.Cu")
                || inner_copper_index(name).is_some_and(|k| k + 1 < count)
        });
        if !regular || !copper.contains("F.Cu") || !copper.contains("B.Cu") {
            let names = copper.into_iter().collect::<Vec<_>>().join(", ");
            return Err(FlipLayerError::IrregularStack(format!(
                "copper layers [{names}]"
            )));
        }
        Self::new(count)
    }

    /// How many copper layers the board has.
    pub fn count(self) -> u32 {
        self.count
    }
}

/// Why a layer has no place on the other side of the board.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlipLayerError {
    #[error("unsupported side-specific layer '{0}'")]
    UnsupportedSideLayer(String),
    #[error("layer '{layer}' is not on this {copper_layers}-copper-layer board")]
    InnerLayerNotOnBoard { layer: String, copper_layers: u32 },
    #[error("{0} are not F.Cu, In1.Cu..In<N-2>.Cu and B.Cu")]
    IrregularStack(String),
}

/// The layer an item moves to when its footprint changes sides, as KiCad's
/// `FlipLayer` computes it: front and back pairs swap, and on N copper layers
/// `In<k>.Cu` mirrors to `In<N-1-k>.Cu`. Wildcards and layers with no side
/// stay where they are.
///
/// Where KiCad clamps an inner layer the board lacks to `In1.Cu`, or leaves it
/// in place on two layers, this refuses.
pub fn flip_layer(layer: &str, stack: CopperStack) -> Result<String, FlipLayerError> {
    let flipped = match layer {
        "F.Cu" => "B.Cu",
        "B.Cu" => "F.Cu",
        "F.Adhes" => "B.Adhes",
        "B.Adhes" => "F.Adhes",
        "F.Paste" => "B.Paste",
        "B.Paste" => "F.Paste",
        "F.SilkS" | "F.Silkscreen" => "B.SilkS",
        "B.SilkS" | "B.Silkscreen" => "F.SilkS",
        "F.Mask" => "B.Mask",
        "B.Mask" => "F.Mask",
        "F.CrtYd" | "F.Courtyard" => "B.CrtYd",
        "B.CrtYd" | "B.Courtyard" => "F.CrtYd",
        "F.Fab" => "B.Fab",
        "B.Fab" => "F.Fab",
        other if other.starts_with("F.") || other.starts_with("B.") => {
            return Err(FlipLayerError::UnsupportedSideLayer(other.to_string()))
        }
        other if other.starts_with("In") && other.ends_with(".Cu") => {
            return match inner_copper_index(other) {
                Some(k) if k + 1 < stack.count => Ok(format!("In{}.Cu", stack.count - 1 - k)),
                _ => Err(FlipLayerError::InnerLayerNotOnBoard {
                    layer: other.to_string(),
                    copper_layers: stack.count,
                }),
            };
        }
        other => other,
    };
    Ok(flipped.to_string())
}

/// `k` for a canonical `In<k>.Cu`.
fn inner_copper_index(name: &str) -> Option<u32> {
    let n = name.strip_prefix("In")?.strip_suffix(".Cu")?;
    let k = n.parse::<u32>().ok()?;
    ((1..=MAX_INNER_COPPER).contains(&k) && !n.starts_with('0')).then_some(k)
}

fn layer_from(node: &SexpNode) -> Option<Layer> {
    // `(0 "F.Cu" signal "Top Layer")` — the ordinal is child 0, being the head
    // of the list, so every field sits one place earlier than in a tagged node.
    let id = node.get_f64(0)? as i32;
    let name = node.get(1)?.as_str()?.to_string();
    let kind = node
        .get(2)
        .and_then(|n| n.as_str())
        .unwrap_or("user")
        .to_string();
    let user_name = node.get(3).and_then(|n| n.as_str()).map(str::to_string);
    Some(Layer {
        id,
        name,
        kind,
        user_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_sexp;

    fn board(inner: &str) -> SexpNode {
        parse_sexp(&format!("(kicad_pcb (layers {inner}))")).unwrap()
    }

    #[test]
    fn reads_id_name_and_kind() {
        let stack = layers(&board(r#"(0 "F.Cu" signal)"#));
        assert_eq!(
            stack,
            vec![Layer {
                id: 0,
                name: "F.Cu".into(),
                kind: "signal".into(),
                user_name: None
            }]
        );
    }

    #[test]
    fn reads_the_optional_user_name() {
        let stack = layers(&board(r#"(0 "F.Cu" signal "Top Layer")"#));
        assert_eq!(stack[0].user_name.as_deref(), Some("Top Layer"));
    }

    #[test]
    fn the_head_atom_is_not_a_layer() {
        // `(layers …)` yields `layers` itself as a child; counting children
        // blindly overcounts by exactly one.
        let stack = layers(&board(r#"(0 "F.Cu" signal) (2 "B.Cu" signal)"#));
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn ids_are_read_as_written_not_as_positions() {
        // KiCad leaves gaps; a positional read would report 0,1,2.
        let stack = layers(&board(
            r#"(0 "F.Cu" signal) (2 "B.Cu" signal) (9 "F.Adhes" user)"#,
        ));
        assert_eq!(
            stack.iter().map(|l| l.id).collect::<Vec<_>>(),
            vec![0, 2, 9]
        );
    }

    #[test]
    fn copper_is_by_name_not_by_kind() {
        // `power` and `mixed` are copper too; `user` never is, even on a layer
        // whose name merely contains "Cu".
        let stack = layers(&board(
            r#"(0 "F.Cu" signal) (1 "In1.Cu" power) (2 "In2.Cu" mixed) (3 "B.Cu" jumper) (9 "F.Adhes" user) (60 "Cu.Marks" user)"#,
        ));
        assert_eq!(
            copper(&stack).iter().map(|l| &l.name).collect::<Vec<_>>(),
            vec!["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"]
        );
    }

    #[test]
    fn a_board_without_a_layers_block_is_empty_not_a_panic() {
        assert!(layers(&parse_sexp("(kicad_pcb)").unwrap()).is_empty());
    }

    /// #461: the count a fab quotes on. Every copper kind counts, the user
    /// layers never do, and a board with no table is zero, not two.
    #[test]
    fn copper_layer_count_counts_every_copper_kind() {
        assert_eq!(
            copper_layer_count(&board(
                r#"(0 "F.Cu" signal) (4 "In1.Cu" power) (6 "In2.Cu" signal) (8 "In3.Cu" mixed) (10 "In4.Cu" jumper) (2 "B.Cu" signal) (9 "F.Adhes" user) (25 "Edge.Cuts" user)"#,
            )),
            6
        );
        assert_eq!(
            copper_layer_count(&board(
                r#"(0 "F.Cu" signal "Top Layer") (2 "B.Cu" signal "Bottom")"#
            )),
            2
        );
        assert_eq!(copper_layer_count(&parse_sexp("(kicad_pcb)").unwrap()), 0);
    }

    #[test]
    fn a_malformed_entry_is_skipped_and_the_rest_survive() {
        let stack = layers(&board(r#"(0 "F.Cu" signal) (nonsense) (2 "B.Cu" signal)"#));
        assert_eq!(
            stack.iter().map(|l| &l.name).collect::<Vec<_>>(),
            vec!["F.Cu", "B.Cu"]
        );
    }

    #[test]
    fn canonical_names_are_accepted() {
        for name in [
            "F.Cu",
            "B.Cu",
            "In1.Cu",
            "In30.Cu",
            "Edge.Cuts",
            "Margin",
            "Rescue",
            "User.1",
            "User.45",
            "Dwgs.User",
            "F.CrtYd",
        ] {
            assert!(is_canonical_name(name), "{name} should be canonical");
        }
    }

    #[test]
    fn invented_names_are_rejected() {
        // The one that matters: a caller-supplied name KiCad has never heard of
        // produces a board that will not open at all.
        for name in [
            "TestLayer",
            "MyLayer",
            "",
            "f.cu",
            "F_Cu",
            "In0.Cu",
            "User.0",
        ] {
            assert!(!is_canonical_name(name), "{name} should not be canonical");
        }
    }

    #[test]
    fn out_of_range_and_padded_ordinals_are_rejected() {
        // KiCad stops at In30.Cu / User.45, and does not zero-pad.
        for name in ["In31.Cu", "User.46", "In01.Cu", "User.01", "In.Cu", "User."] {
            assert!(!is_canonical_name(name), "{name} should not be canonical");
        }
    }

    fn stack(count: u32) -> CopperStack {
        CopperStack::new(count).unwrap()
    }

    /// #831: the in-range rows of KiCad 10.0.6's `pcbnew.FlipLayer(layer, N)`.
    #[test]
    fn inner_copper_mirrors_as_kicads_flip_layer() {
        for (count, rows) in [
            (2, &[("F.Cu", "B.Cu"), ("B.Cu", "F.Cu")][..]),
            (4, &[("In1.Cu", "In2.Cu"), ("In2.Cu", "In1.Cu")][..]),
            (
                6,
                &[
                    ("In1.Cu", "In4.Cu"),
                    ("In2.Cu", "In3.Cu"),
                    ("In4.Cu", "In1.Cu"),
                ][..],
            ),
            (
                8,
                &[
                    ("In1.Cu", "In6.Cu"),
                    ("In2.Cu", "In5.Cu"),
                    ("In3.Cu", "In4.Cu"),
                    ("In5.Cu", "In2.Cu"),
                    ("In6.Cu", "In1.Cu"),
                ][..],
            ),
            (
                32,
                &[
                    ("In1.Cu", "In30.Cu"),
                    ("In8.Cu", "In23.Cu"),
                    ("In30.Cu", "In1.Cu"),
                ][..],
            ),
        ] {
            for (from, to) in rows {
                assert_eq!(
                    flip_layer(from, stack(count)).unwrap(),
                    *to,
                    "{from} on {count}"
                );
            }
        }
    }

    #[test]
    fn sideless_layers_and_wildcards_stay_put() {
        for layer in [
            "*.Cu",
            "*.Mask",
            "*.Paste",
            "F&B.Cu",
            "Edge.Cuts",
            "User.3",
            "Dwgs.User",
        ] {
            assert_eq!(flip_layer(layer, stack(8)).unwrap(), layer);
        }
        assert_eq!(flip_layer("F.SilkS", stack(8)).unwrap(), "B.SilkS");
        assert_eq!(flip_layer("B.Courtyard", stack(8)).unwrap(), "F.CrtYd");
    }

    /// Where KiCad clamps to In1.Cu (or keeps the layer on two layers), the
    /// flip refuses.
    #[test]
    fn an_inner_layer_the_board_lacks_is_refused() {
        for (layer, count) in [("In5.Cu", 4), ("In1.Cu", 2), ("In7.Cu", 8), ("In0.Cu", 8)] {
            assert_eq!(
                flip_layer(layer, stack(count)),
                Err(FlipLayerError::InnerLayerNotOnBoard {
                    layer: layer.to_string(),
                    copper_layers: count,
                })
            );
        }
        assert!(matches!(
            flip_layer("F.Bogus", stack(2)),
            Err(FlipLayerError::UnsupportedSideLayer(_))
        ));
    }

    #[test]
    fn the_stack_is_read_from_the_board_layer_names() {
        let names = ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu", "Edge.Cuts", "User.1"];
        assert_eq!(CopperStack::from_layer_names(names).unwrap().count(), 4);
        for names in [
            &[][..],
            &["F.Cu"][..],
            &["F.Cu", "In1.Cu", "B.Cu"][..],
            &["F.Cu", "In2.Cu", "In3.Cu", "B.Cu"][..],
            &["F.Cu", "In1.Cu", "In2.Cu", "In4.Cu", "B.Cu", "In01.Cu"][..],
        ] {
            assert!(matches!(
                CopperStack::from_layer_names(names.iter().copied()),
                Err(FlipLayerError::IrregularStack(_))
            ));
        }
        for count in [0, 3, 34] {
            assert!(CopperStack::new(count).is_err(), "{count}");
        }
    }

    #[test]
    fn tab_indentation_is_irrelevant_to_a_tree_read() {
        // The shape is what matters; KiCad 10 writes tabs, 9 writes spaces.
        let spaces = parse_sexp("(kicad_pcb\n  (layers\n    (0 \"F.Cu\" signal)\n  )\n)").unwrap();
        let tabs = parse_sexp("(kicad_pcb\n\t(layers\n\t\t(0 \"F.Cu\" signal)\n\t)\n)").unwrap();
        assert_eq!(layers(&spaces), layers(&tabs));
    }
}
