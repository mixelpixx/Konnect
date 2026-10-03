//! Library symbol resolution — loads symbol definitions from KiCad libraries.
//!
//! Resolves a `lib_id` like `"Device:R"` to its full symbol S-expression and
//! can inject it into a Schematic's `lib_symbols` section. *Where* a library
//! lives is supplied by a [`SymbolLibrarySource`]; this module reads whatever
//! paths it is given, in either the KiCad 10 symdir layout
//! (`Device.kicad_symdir/R.kicad_sym`) or a single-file `Device.kicad_sym`.

use crate::sexp::{parser, SexpNode};
use crate::Schematic;
use std::path::{Path, PathBuf};

/// Locates the on-disk libraries that may define a library nickname.
///
/// Keeps KiCad-config knowledge (install paths, `sym-lib-table`) in
/// konnect-core, which already owns it for footprints.
pub trait SymbolLibrarySource {
    /// Candidate locations for `nickname`, highest priority first: either a
    /// `.kicad_symdir` directory or a `.kicad_sym` file. Full paths, since a
    /// table may map a nickname to any filename. Missing entries are skipped
    /// by callers.
    fn candidates(&self, nickname: &str) -> Vec<PathBuf>;
}

impl<F> SymbolLibrarySource for F
where
    F: Fn(&str) -> Vec<PathBuf>,
{
    fn candidates(&self, nickname: &str) -> Vec<PathBuf> {
        self(nickname)
    }
}

/// The `.kicad_sym` file that would define `symbol_name` at `candidate`.
/// A symdir holds one file per symbol; a single-file library holds them all.
pub fn symbol_file_in(candidate: &Path, symbol_name: &str) -> Option<PathBuf> {
    if candidate.is_dir() {
        let file = candidate.join(format!("{}.kicad_sym", symbol_name));
        return file.is_file().then_some(file);
    }
    candidate.is_file().then(|| candidate.to_path_buf())
}

/// Prefix a symbol block's name with its library, as an embedded `lib_symbols`
/// entry must be. Unit sub-symbols (`Name_0_1`) stay unprefixed — eeschema
/// refuses to load a schematic whose units carry the prefix.
fn prefix_symbol_block(block: &str, library_name: &str, symbol_name: &str) -> String {
    let mut renamed = block.replacen(
        &format!("(symbol \"{}\"", symbol_name),
        &format!("(symbol \"{}:{}\"", library_name, symbol_name),
        1,
    );
    // `(extends "Parent")` names a sibling; qualify it into a lib_id.
    if let Some(ext_pos) = renamed.find("(extends \"") {
        let after = &renamed[ext_pos + 10..];
        if let Some(end) = after.find('"') {
            let parent = after[..end].to_string();
            renamed = renamed.replace(
                &format!("(extends \"{}\")", parent),
                &format!("(extends \"{}:{}\")", library_name, parent),
            );
        }
    }
    renamed
}

/// Resolve a lib_id (e.g. "Device:R") to the full symbol S-expression string.
/// The returned string is the raw content of the `(symbol "R" ...)` block,
/// with the name prefixed as `"Device:R"`.
pub fn resolve_lib_symbol(lib_id: &str, src: &dyn SymbolLibrarySource) -> Option<String> {
    let (library_name, symbol_name) = lib_id.split_once(':')?;

    for candidate in src.candidates(library_name) {
        let Some(file) = symbol_file_in(&candidate, symbol_name) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        if let Some(block) = extract_symbol_block(&content, symbol_name) {
            return Some(prefix_symbol_block(&block, library_name, symbol_name));
        }
    }
    None
}

/// Resolve a lib_id to a parsed SexpNode tree.
pub fn resolve_lib_symbol_node(lib_id: &str, src: &dyn SymbolLibrarySource) -> Option<SexpNode> {
    let raw = resolve_lib_symbol(lib_id, src)?;
    parser::parse(&raw).ok()
}

/// Resolve a lib_id to a parsed tree with any `(extends "Parent")` chain
/// FLATTENED, the way eeschema itself saves derived symbols (#35):
///
/// - the parent chain's unit sub-symbols are deep-copied into the child,
///   renamed `Parent_N_M` → `Derived_N_M`;
/// - parent properties and attribute nodes (pin_numbers, pin_names, in_bom,
///   …) are inherited unless the child overrides them;
/// - the `(extends …)` marker is dropped.
///
/// An extends STUB embed (child + separately embedded parent) is a shape
/// kicad-cli cannot resolve — the netlist gets a pinless libpart — and one
/// eeschema never writes. A missing/broken parent stops the walk gracefully,
/// returning the partially flattened child.
pub fn resolve_lib_symbol_flattened_node(
    lib_id: &str,
    src: &dyn SymbolLibrarySource,
) -> Option<SexpNode> {
    let mut node = resolve_lib_symbol_node(lib_id, src)?;
    let child_base = lib_id.split_once(':')?.1.to_string();

    let mut parent_id = node.get_value("extends").map(str::to_string);
    if parent_id.is_none() {
        return Some(node); // not derived: nothing to flatten
    }
    if let SexpNode::List(children) = &mut node {
        children.retain(|c| c.tag() != Some("extends"));
    }

    let mut visited: std::collections::HashSet<String> =
        std::collections::HashSet::from([lib_id.to_string()]);
    while let Some(pid) = parent_id {
        if !visited.insert(pid.clone()) {
            break; // cyclic extends: stop, keep what we have
        }
        let Some(parent) = resolve_lib_symbol_node(&pid, src) else {
            break; // broken library (dangling parent): keep what we have
        };
        let parent_base = pid
            .split_once(':')
            .map_or(pid.as_str(), |x| x.1)
            .to_string();
        merge_parent_into_child(&mut node, &parent, &parent_base, &child_base);
        parent_id = parent.get_value("extends").map(str::to_string);
    }
    Some(node)
}

/// Serialized form of [`resolve_lib_symbol_flattened_node`], for callers that
/// splice raw text into a schematic's `lib_symbols` section.
pub fn resolve_lib_symbol_flattened(lib_id: &str, src: &dyn SymbolLibrarySource) -> Option<String> {
    resolve_lib_symbol_flattened_node(lib_id, src).map(|n| crate::sexp::writer::write(&n))
}

/// Copy one parent level into a derived symbol: unit sub-symbols renamed to
/// the child's base name, plus properties / attribute nodes the child does
/// not define itself (most-derived wins, matching eeschema's inheritance).
fn merge_parent_into_child(
    child: &mut SexpNode,
    parent: &SexpNode,
    parent_base: &str,
    child_base: &str,
) {
    let child_subs: std::collections::HashSet<String> = child
        .find_all("symbol")
        .iter()
        .filter_map(|s| s.value())
        .map(String::from)
        .collect();
    let child_props: std::collections::HashSet<String> = child
        .find_all("property")
        .iter()
        .filter_map(|p| p.value())
        .map(String::from)
        .collect();

    let mut inherited: Vec<SexpNode> = Vec::new();
    for item in parent.args() {
        match item.tag() {
            Some("symbol") => {
                let Some(name) = item.value() else { continue };
                let Some(suffix) = unit_suffix_of(name, parent_base) else {
                    continue;
                };
                let new_name = format!("{child_base}{suffix}");
                if child_subs.contains(&new_name) {
                    continue; // child overrides this unit's drawing
                }
                let mut cloned = item.clone();
                if let SexpNode::List(c) = &mut cloned {
                    if c.len() >= 2 {
                        c[1] = SexpNode::Str(new_name);
                    }
                }
                inherited.push(cloned);
            }
            Some("property") => {
                let Some(key) = item.value() else { continue };
                if !child_props.contains(key) {
                    inherited.push(item.clone());
                }
            }
            // extends handled by the caller's chain walk.
            Some("extends") | None => {}
            // Attribute-style nodes (pin_numbers, pin_names, in_bom,
            // on_board, exclude_from_sim, …): inherit unless overridden.
            Some(tag) => {
                if child.find(tag).is_none() {
                    inherited.push(item.clone());
                }
            }
        }
    }
    if let SexpNode::List(c) = child {
        c.extend(inherited);
    }
}

/// The `_N_M` unit suffix of `name` given its base (e.g. `LM2904_1_1` with
/// base `LM2904` → `_1_1`). `None` unless the remainder is exactly two
/// `_`-separated integers.
fn unit_suffix_of<'a>(name: &'a str, base: &str) -> Option<&'a str> {
    let rest = name.strip_prefix(base)?;
    let mut it = rest.rsplitn(3, '_');
    let style = it.next()?;
    let unit = it.next()?;
    let lead = it.next()?;
    (lead.is_empty()
        && !style.is_empty()
        && !unit.is_empty()
        && style.bytes().all(|b| b.is_ascii_digit())
        && unit.bytes().all(|b| b.is_ascii_digit()))
    .then_some(rest)
}

/// Ensure a library symbol definition is present in the schematic's lib_symbols section.
/// If the symbol is already present (by name), does nothing.
/// If the lib_symbols node doesn't exist in raw_other, creates one.
///
/// Derived symbols (`(extends "Parent")`) are embedded FLATTENED — parent
/// units deep-copied and renamed, no extends stub — the way eeschema saves
/// them. The stub-plus-parent shape this used to write is unresolvable by
/// kicad-cli: its netlist showed a pinless libpart for every derived symbol
/// (#35).
///
/// Returns `false` when `lib_id` cannot be resolved from the installed
/// libraries — callers MUST surface that as an error: a symbol instance
/// without an embedded definition is invisible to KiCAD's netlister and
/// yields empty pin lists downstream (#34).
#[must_use]
pub fn ensure_lib_symbol(
    schematic: &mut Schematic,
    lib_id: &str,
    src: &dyn SymbolLibrarySource,
) -> bool {
    // Check if already present. This is a structural match on the direct
    // children of lib_symbols: it used to substring-search a `{:?}` debug
    // rendering of the whole node, which matched on any nested occurrence of
    // the name — a property value, a unit sub-symbol, or an unrelated entry
    // that merely contains it (#143).
    let already_present = schematic.raw_other.iter().any(|node| {
        node.tag() == Some("lib_symbols")
            && node
                .find_all("symbol")
                .iter()
                .any(|s| s.value() == Some(lib_id))
    });
    if already_present {
        return true;
    }

    // Resolve and embed the symbol, flattening any extends chain.
    let sym_node = match resolve_lib_symbol_flattened_node(lib_id, src) {
        Some(n) => n,
        None => return false,
    };

    // Find or create the lib_symbols node
    let lib_syms_idx = schematic
        .raw_other
        .iter()
        .position(|n| n.tag() == Some("lib_symbols"));

    match lib_syms_idx {
        Some(idx) => {
            // Append the symbol to the existing lib_symbols list
            if let SexpNode::List(ref mut children) = schematic.raw_other[idx] {
                children.push(sym_node);
            }
        }
        None => {
            // Create a new lib_symbols node with this symbol
            let lib_syms =
                SexpNode::List(vec![SexpNode::Atom("lib_symbols".to_string()), sym_node]);
            // Insert at the beginning of raw_other (lib_symbols should come early)
            schematic.raw_other.insert(0, lib_syms);
        }
    }
    true
}

/// Library-space (Y-up) anchors of a symbol's Reference and Value text, each
/// `(x, y, text_rotation)` as written in the library's `(property … (at …))`,
/// plus the `(justify …)` that says which way the text runs from there.
///
/// eeschema places an instance's fields at these anchors pushed through the
/// instance transform, so reading them is what keeps a placement looking like
/// one eeschema made. A fixed ±3.81 offset only matches down-pointing symbols
/// such as `power:GND`; `power:VCC` and every other up-pointing flavor anchor
/// Value *above* the graphic, and `Device:R` anchors both fields beside the
/// body at 90° (#101).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FieldAnchors {
    /// `(x, y, rotation)` of the Reference text, library-local mm/degrees.
    pub reference_at: Option<(f64, f64, f64)>,
    /// `(x, y, rotation)` of the Value text, library-local mm/degrees.
    pub value_at: Option<(f64, f64, f64)>,
    /// `(justify …)` of the Reference text.
    pub reference_justify: FieldJustify,
    /// `(justify …)` of the Value text.
    pub value_justify: FieldJustify,
}

/// The `(justify …)` of a field's `(effects …)`: which side of the anchor the
/// text runs to. Default is KiCad's centred, written as no `(justify …)` at
/// all.
///
/// An anchor without its justification is half the placement. Libraries that
/// left-justify keep Reference and Value apart at the same `y` —
/// `Regulator_Linear:AP2112K-3.3` anchors Reference at `(-5.08, 5.715)` and
/// Value at `(0, 5.715)`, both `justify left`, so centring them prints `U2`
/// on top of `AP2112K-3.3`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldJustify {
    /// `left` / `right`; `None` is centred.
    pub horizontal: Option<HorizontalJustify>,
    /// `top` / `bottom`; `None` is centred.
    pub vertical: Option<VerticalJustify>,
    /// `mirror`: the text reads backwards.
    pub mirror: bool,
}

/// Horizontal half of a [`FieldJustify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalJustify {
    Left,
    Right,
}

/// Vertical half of a [`FieldJustify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalJustify {
    Top,
    Bottom,
}

impl FieldJustify {
    /// Read the `(justify …)` of a `(property …)` node's `(effects …)`.
    ///
    /// Tokens are order-independent and any KiCad does not define is ignored,
    /// so an unknown one degrades to centred rather than failing a placement.
    pub fn of_property(property: &SexpNode) -> Self {
        let mut justify = Self::default();
        let Some(node) = property.find("effects").and_then(|e| e.find("justify")) else {
            return justify;
        };
        for token in node.scalar_args() {
            match token {
                "left" => justify.horizontal = Some(HorizontalJustify::Left),
                "right" => justify.horizontal = Some(HorizontalJustify::Right),
                "top" => justify.vertical = Some(VerticalJustify::Top),
                "bottom" => justify.vertical = Some(VerticalJustify::Bottom),
                "mirror" => justify.mirror = true,
                _ => {}
            }
        }
        justify
    }

    /// The tokens of a `(justify …)`, in the order KiCad writes them. Empty
    /// when the field is centred, which is spelled by omitting the node.
    pub fn tokens(&self) -> Vec<&'static str> {
        let mut tokens = Vec::new();
        if let Some(h) = self.horizontal {
            tokens.push(match h {
                HorizontalJustify::Left => "left",
                HorizontalJustify::Right => "right",
            });
        }
        if let Some(v) = self.vertical {
            tokens.push(match v {
                VerticalJustify::Top => "top",
                VerticalJustify::Bottom => "bottom",
            });
        }
        if self.mirror {
            tokens.push("mirror");
        }
        tokens
    }
}

/// Read the Reference/Value anchors of `lib_id` from the copy embedded in
/// `schematic`'s `lib_symbols`.
///
/// The embedded copy — not the library on disk — is what KiCad draws, so it is
/// also what a placement should anchor to; it is already flattened by
/// [`ensure_lib_symbol`], so a derived symbol inherits its parent's anchors.
/// A `lib_id` the schematic has no definition for yields `None` anchors and
/// leaves the caller on its fallback.
pub fn field_anchors(schematic: &Schematic, lib_id: &str) -> FieldAnchors {
    embedded_lib_symbol(schematic, lib_id)
        .map(field_anchors_of)
        .unwrap_or_default()
}

/// The `lib_symbols` entry `schematic` carries for `lib_id`.
pub fn embedded_lib_symbol<'a>(schematic: &'a Schematic, lib_id: &str) -> Option<&'a SexpNode> {
    schematic
        .raw_other
        .iter()
        .find(|n| n.tag() == Some("lib_symbols"))?
        .find_all("symbol")
        .into_iter()
        .find(|s| s.value() == Some(lib_id))
}

/// Library-owned fields that a placement must copy onto the symbol instance.
///
/// Read these from the embedded definition rather than resolving the library
/// again: the embedded copy is already flattened for derived symbols and is
/// the exact definition the schematic instance refers to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolMetadata {
    /// The reference prefix, such as `R` or `#FLG`.
    pub reference: String,
    pub value: String,
    pub footprint: String,
    pub datasheet: String,
    pub description: String,
}

/// Read the metadata fields of `lib_id` from the schematic's embedded symbol.
/// Missing fields and an unknown symbol both produce empty values, matching
/// KiCad's mandatory placed-field representation.
pub fn symbol_metadata(schematic: &Schematic, lib_id: &str) -> SymbolMetadata {
    let Some(symbol) = embedded_lib_symbol(schematic, lib_id) else {
        return SymbolMetadata::default();
    };
    let value = |name: &str| {
        symbol
            .find_all("property")
            .into_iter()
            .find(|property| property.value() == Some(name))
            .and_then(|property| property.args().get(1))
            .and_then(SexpNode::text)
            .unwrap_or_default()
            .to_string()
    };
    SymbolMetadata {
        reference: value("Reference"),
        value: value("Value"),
        footprint: value("Footprint"),
        datasheet: value("Datasheet"),
        description: value("Description"),
    }
}

/// [`FieldAnchors`] of a resolved symbol node.
///
/// Only direct `(property …)` children count: a unit sub-symbol carries its
/// own graphics, not the instance's fields.
pub fn field_anchors_of(symbol: &SexpNode) -> FieldAnchors {
    let mut anchors = FieldAnchors::default();
    for prop in symbol.find_all("property") {
        let Some(name) = prop.value() else { continue };
        let Some(at) = prop.find("at") else { continue };
        let s = at.scalar_args();
        let num = |i: usize| s.get(i).and_then(|v| v.parse::<f64>().ok());
        let (Some(x), Some(y)) = (num(0), num(1)) else {
            continue;
        };
        let rot = num(2).unwrap_or(0.0);
        match name {
            "Reference" => {
                anchors.reference_at = Some((x, y, rot));
                anchors.reference_justify = FieldJustify::of_property(prop);
            }
            "Value" => {
                anchors.value_at = Some((x, y, rot));
                anchors.value_justify = FieldJustify::of_property(prop);
            }
            _ => {}
        }
    }
    anchors
}

/// Number of units of the symbol `lib_id` resolves to, following the
/// `(extends "Parent")` chain when the symbol has no unit sub-symbols of its
/// own (#35). The count is the maximum `N >= 1` over `Name_N_M` sub-symbol
/// names; symbols with only a `_0_1` body (or none) count as 1. Returns
/// `None` when `lib_id` cannot be resolved at all.
pub fn symbol_unit_count(lib_id: &str, src: &dyn SymbolLibrarySource) -> Option<u32> {
    let mut current = lib_id.to_string();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    while visited.insert(current.clone()) {
        let node = resolve_lib_symbol_node(&current, src)?;
        let max_unit = node
            .find_all("symbol")
            .iter()
            .filter_map(|s| s.value())
            .filter_map(konnect_sexp::schematic::parse_subsymbol_unit)
            .filter(|&n| n >= 1)
            .max();
        if let Some(n) = max_unit {
            return Some(n);
        }
        // No unit sub-symbols: a derived symbol inherits the parent's units.
        match node.get_value("extends") {
            Some(parent) if parent.contains(':') => current = parent.to_string(),
            _ => return Some(1),
        }
    }
    Some(1) // cyclic extends: treat as single-unit rather than erroring
}

/// Whether any library `src` offers for `library_name` is actually on disk,
/// in either the KiCad 10 symdir layout or the single-file one.
pub fn library_exists(library_name: &str, src: &dyn SymbolLibrarySource) -> bool {
    src.candidates(library_name)
        .iter()
        .any(|p| p.is_dir() || p.is_file())
}

/// Symbol names similar to the one in `lib_id`, for did-you-mean hints when a
/// lib_id doesn't resolve (#34: LLM callers habitually reach for KiCAD ≤9
/// names like `Device:CP` that KiCAD 10 renamed). Returns full `Library:Name`
/// ids, closest first, at most `limit`.
pub fn suggest_symbols(lib_id: &str, limit: usize, src: &dyn SymbolLibrarySource) -> Vec<String> {
    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Vec::new();
    }
    let (library_name, symbol_name) = (parts[0], parts[1]);
    let wanted = symbol_name.to_lowercase();

    let mut candidates: Vec<String> = Vec::new();
    for base in src.candidates(library_name) {
        // symdir layout: one .kicad_sym per symbol, so the file stems are the
        // symbol names.
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("kicad_sym") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        candidates.push(stem.to_string());
                    }
                }
            }
        }
        // Single-file library: scan top-level (symbol "NAME" entries.
        if let Ok(content) = std::fs::read_to_string(&base) {
            let mut from = 0usize;
            while let Some(rel) = content[from..].find("(symbol \"") {
                let start = from + rel + 9;
                if let Some(end) = content[start..].find('"') {
                    let name = &content[start..start + end];
                    // Skip unit sub-symbols ("R_0_1") and prefixed names.
                    if !name.contains(':') && extract_symbol_block(&content, name).is_some() {
                        candidates.push(name.to_string());
                    }
                    from = start + end;
                } else {
                    break;
                }
            }
        }
    }
    candidates.sort();
    candidates.dedup();

    rank_candidates(&wanted, candidates, limit)
        .into_iter()
        .map(|name| format!("{}:{}", library_name, name))
        .collect()
}

/// Rank `candidates` by similarity to `wanted` (already lowercased), keeping
/// at most `limit`, closest first. Pure so it's unit-testable without an
/// installed KiCAD.
fn rank_candidates(wanted: &str, candidates: Vec<String>, limit: usize) -> Vec<String> {
    let mut scored: Vec<(usize, String)> = candidates
        .into_iter()
        .filter_map(|name| {
            let lower = name.to_lowercase();
            // Stylized matches cover the classic KiCAD ≤9 shorthands the
            // renames expanded (CP → C_Polarized, R_POT_TRIM →
            // R_Potentiometer_Trim); substring containment covers truncations;
            // otherwise edit distance, capped so unrelated names don't surface.
            let dist = if stylized_match(wanted, &lower)
                || lower.contains(wanted)
                || wanted.contains(&lower)
            {
                1
            } else {
                edit_distance(wanted, &lower)
            };
            let cutoff = (wanted.len().max(lower.len()) * 2).div_ceil(3);
            (dist <= cutoff).then_some((dist, name))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, n)| n).collect()
}

/// Shorthand relationships between a wanted name and a candidate (both
/// lowercase): the wanted name is the candidate's initials ("cp" vs
/// "c_polarized"), or both split into the same number of `_` tokens with each
/// wanted token a prefix of the candidate's ("r_pot_trim" vs
/// "r_potentiometer_trim").
fn stylized_match(wanted: &str, cand: &str) -> bool {
    let toks = |s: &str| -> Vec<String> {
        s.split(['_', '-', '.'])
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (w, c) = (toks(wanted), toks(cand));
    if w.len() == 1 && c.len() >= 2 {
        let initials: String = c.iter().filter_map(|t| t.chars().next()).collect();
        if initials == w[0] {
            return true;
        }
    }
    !w.is_empty() && w.len() == c.len() && w.iter().zip(&c).all(|(a, b)| b.starts_with(a.as_str()))
}

/// Plain Levenshtein distance, O(len(a)·len(b)) with a single-row table.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev_diag = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            let val = (prev_diag + cost).min(row[j] + 1).min(row[j + 1] + 1);
            prev_diag = row[j + 1];
            row[j + 1] = val;
        }
    }
    row[b.len()]
}

/// Extract a `(symbol "NAME" ...)` block from file content by balanced-paren matching.
fn extract_symbol_block(content: &str, symbol_name: &str) -> Option<String> {
    let pattern = format!("(symbol \"{}\"", symbol_name);
    let start = content.find(&pattern)?;
    let mut depth = 0i32;
    let mut end = start;
    for (i, ch) in content[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end > start {
        Some(content[start..end].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod suggestion_tests {
    use super::*;

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "abd"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn stylized_match_covers_the_kicad10_renames() {
        // The two shorthands from #34's repro.
        assert!(stylized_match("cp", "c_polarized"));
        assert!(stylized_match("r_pot_trim", "r_potentiometer_trim"));
        // Not everything matches.
        assert!(!stylized_match("cp", "resistor"));
        assert!(!stylized_match("irf830", "irf840"));
    }

    #[test]
    fn rank_candidates_surfaces_the_renamed_symbol() {
        let candidates = vec![
            "C".to_string(),
            "C_Polarized".to_string(),
            "C_Polarized_Small".to_string(),
            "R".to_string(),
            "L".to_string(),
        ];
        let ranked = rank_candidates("cp", candidates, 3);
        assert!(
            ranked.contains(&"C_Polarized".to_string()),
            "CP must suggest C_Polarized, got {ranked:?}"
        );
        assert!(!ranked.contains(&"R".to_string()));
    }

    #[test]
    fn rank_candidates_close_typo_and_cap() {
        let candidates = vec![
            "R_Potentiometer".to_string(),
            "R_Potentiometer_Trim".to_string(),
            "Fuse".to_string(),
        ];
        let ranked = rank_candidates("r_pot_trim", candidates, 2);
        assert_eq!(ranked.len().min(2), ranked.len(), "limit respected");
        assert_eq!(ranked[0], "R_Potentiometer_Trim");
        assert!(!ranked.contains(&"Fuse".to_string()));
    }

    #[test]
    fn ensure_lib_symbol_flattens_extends_chain() {
        // NE5532-style derived symbol: (extends "LM2904"), no drawing of its
        // own. The embed must copy the parent's unit sub-symbols renamed to
        // the derived name and drop the extends marker — the old stub+parent
        // shape produced a pinless libpart in kicad-cli's netlist (#35).
        let libdir = tempfile::tempdir().unwrap();
        let symdir = libdir.path().join("Amp.kicad_symdir");
        std::fs::create_dir_all(&symdir).unwrap();
        std::fs::write(
            symdir.join("LM2904.kicad_sym"),
            "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"LM2904\"\n\t\t(pin_names (offset 0.127))\n\t\t(in_bom yes)\n\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t(property \"Value\" \"LM2904\" (at 0 0 0))\n\t\t(property \"Footprint\" \"Package_DIP:DIP-8_W7.62mm\" (at 0 0 0))\n\t\t(property \"Datasheet\" \"lm2904.pdf\" (at 0 0 0))\n\t\t(symbol \"LM2904_1_1\"\n\t\t\t(pin output line (at 7.62 0 180) (length 2.54)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t)\n\t\t(symbol \"LM2904_2_1\"\n\t\t\t(pin output line (at 7.62 0 180) (length 2.54)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"7\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
        std::fs::write(
            symdir.join("NE5532.kicad_sym"),
            "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"NE5532\"\n\t\t(extends \"LM2904\")\n\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t(property \"Value\" \"NE5532\" (at 0 0 0))\n\t)\n)\n",
        )
        .unwrap();
        // Explicit source, not KICAD10_SYMBOL_DIR: env vars are process-wide
        // and force the test to be serialised.
        let root = libdir.path().to_path_buf();
        let src = move |nick: &str| vec![root.join(format!("{}.kicad_symdir", nick))];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flat.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n\t)\n)\n",
        )
        .unwrap();
        let mut sch = Schematic::load(&path).unwrap();
        assert!(ensure_lib_symbol(&mut sch, "Amp:NE5532", &src));
        sch.overwrite().unwrap();
        let out = std::fs::read_to_string(&path).unwrap();

        assert!(
            out.contains("(symbol \"Amp:NE5532\""),
            "derived symbol embedded:\n{out}"
        );
        assert!(
            out.contains("(symbol \"NE5532_1_1\"") && out.contains("(symbol \"NE5532_2_1\""),
            "parent units must be copied in, renamed to the derived base:\n{out}"
        );
        assert!(
            !out.contains("(extends"),
            "no extends stub may remain:\n{out}"
        );
        assert!(
            !out.contains("(symbol \"Amp:LM2904\""),
            "the parent must not be embedded separately:\n{out}"
        );
        assert!(
            out.contains("\"NE5532\""),
            "the child's own Value wins:\n{out}"
        );
        assert!(
            out.contains("lm2904.pdf"),
            "properties the child lacks are inherited:\n{out}"
        );
        let metadata = symbol_metadata(&sch, "Amp:NE5532");
        assert_eq!(metadata.value, "NE5532", "the child's own Value wins");
        assert_eq!(
            metadata.footprint, "Package_DIP:DIP-8_W7.62mm",
            "a field absent from the child is inherited from the parent"
        );
        assert_eq!(
            metadata.datasheet, "lm2904.pdf",
            "placed-instance metadata must follow the flattened definition"
        );
        // Pins from both units present exactly once.
        assert_eq!(out.matches("(number \"1\"").count(), 1);
        assert_eq!(out.matches("(number \"7\"").count(), 1);
    }

    #[test]
    fn ensure_lib_symbol_reports_failure_for_bogus_lib_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n\t)\n)\n",
        )
        .unwrap();
        let mut sch = Schematic::load(&path).unwrap();
        // No library named like this exists anywhere.
        let empty = |_: &str| Vec::<PathBuf>::new();
        assert!(!ensure_lib_symbol(
            &mut sch,
            "Definitely_Not_A_Library_xyzzy:Nope",
            &empty
        ));
    }

    /// Load a schematic from literal source in a tempdir. The `TempDir` is
    /// returned so the caller keeps it alive for the schematic's lifetime.
    fn sch_from(src: &str) -> (tempfile::TempDir, std::path::PathBuf, Schematic) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.kicad_sch");
        std::fs::write(&path, src).unwrap();
        let sch = Schematic::load(&path).unwrap();
        (dir, path, sch)
    }

    #[test]
    fn ensure_lib_symbol_presence_check_is_exact_and_structural() {
        // Resolves nothing: the negative cases below must return false because
        // the presence check declined, not because a real library happened to
        // be missing on this machine.
        let src = |_: &str| Vec::<std::path::PathBuf>::new();
        // The presence check used to substring-search a `{:?}` rendering of the
        // whole lib_symbols node, so any nested occurrence of the name — a
        // property value, a unit sub-symbol — read as "already embedded" and
        // the real definition was never written. It is now an exact match on
        // the direct children (#143). The negative cases below use lib_ids
        // that resolve nowhere, so a `false` return proves the check declined
        // to claim presence rather than merely failing to embed.

        // 1. Exact direct child: present, and embedding again must not
        //    duplicate the entry.
        let (_d, path, mut sch) = sch_from(
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t)\n\t)\n)\n",
        );
        assert!(ensure_lib_symbol(&mut sch, "Device:R", &src));
        sch.overwrite().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .matches("(symbol \"Device:R\"")
                .count(),
            1,
            "an already-embedded symbol must not be embedded twice"
        );

        // 2. The name appears only nested, as an unrelated entry's property
        //    value. That is not an embedded definition.
        let (_d, _p, mut sch) = sch_from(
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n\t\t(symbol \"Other:Thing\"\n\t\t\t(property \"Value\" \"Zzz_NoSuchLib:Part\" (at 0 0 0))\n\t\t)\n\t)\n)\n",
        );
        assert!(
            !ensure_lib_symbol(&mut sch, "Zzz_NoSuchLib:Part", &src),
            "a nested property value must not count as an embedded definition"
        );

        // 3. Near-miss sibling: a unit sub-symbol name that merely starts with
        //    the wanted name.
        let (_d, _p, mut sch) = sch_from(
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n\t\t(symbol \"Zzz_NoSuchLib:R_1_1\"\n\t\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t)\n\t)\n)\n",
        );
        assert!(
            !ensure_lib_symbol(&mut sch, "Zzz_NoSuchLib:R_1", &src),
            "a longer sibling name must not satisfy the lookup"
        );

        // 4. More than one lib_symbols block: the match may be in any of them.
        //    Both land in `raw_other`, which the check iterates.
        let (_d, _p, mut sch) = sch_from(
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n\t\t(symbol \"Other:Thing\"\n\t\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t)\n\t)\n\t(lib_symbols\n\t\t(symbol \"Zzz_NoSuchLib:Part\"\n\t\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t)\n\t)\n)\n",
        );
        assert!(
            ensure_lib_symbol(&mut sch, "Zzz_NoSuchLib:Part", &src),
            "a match in a second lib_symbols block must be found"
        );
    }

    /// A project library is one `.kicad_sym` file under a nickname that need
    /// not match the filename. The old directory scan could not express that.
    #[test]
    fn resolves_a_single_file_library_whose_name_differs_from_its_nickname() {
        let libdir = tempfile::tempdir().unwrap();
        let file = libdir.path().join("vendor-parts.kicad_sym");
        std::fs::write(
            &file,
            "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"CH347T\"\n\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t(property \"Value\" \"CH347T\" (at 0 0 0))\n\t\t(symbol \"CH347T_1_1\"\n\t\t\t(pin bidirectional line (at 7.62 0 180) (length 2.54)\n\t\t\t\t(name \"TMS\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"5\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();

        // What a project sym-lib-table entry resolves to.
        let target = file.clone();
        let src = move |nick: &str| {
            if nick == "MyParts" {
                vec![target.clone()]
            } else {
                Vec::new()
            }
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proj.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n\t)\n)\n",
        )
        .unwrap();
        let mut sch = Schematic::load(&path).unwrap();

        assert!(ensure_lib_symbol(&mut sch, "MyParts:CH347T", &src));
        assert_eq!(symbol_unit_count("MyParts:CH347T", &src), Some(1));
        assert!(library_exists("MyParts", &src));

        sch.overwrite().unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains("(symbol \"MyParts:CH347T\""),
            "symbol embedded under its lib_id:\n{out}"
        );
        assert!(
            out.contains("(number \"5\""),
            "pins must come with it — a definition-less instance netlists empty (#34):\n{out}"
        );
    }
}

#[cfg(test)]
mod field_anchor_tests {
    use super::*;

    fn schematic_with(lib_symbols: &str) -> (Schematic, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anchors.kicad_sch");
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n{}\n\t)\n)\n",
                lib_symbols
            ),
        )
        .unwrap();
        // The TempDir rides along: dropping it would delete the file the
        // Schematic still refers to.
        (Schematic::load(&path).unwrap(), dir)
    }

    #[test]
    fn reads_both_anchors_with_their_text_rotation() {
        let (sch, _dir) = schematic_with(
            "(symbol \"Device:R\" (property \"Reference\" \"R\" (at 2.032 0 90)) (property \"Value\" \"R\" (at 0 0 90)))",
        );
        let anchors = field_anchors(&sch, "Device:R");
        assert_eq!(anchors.reference_at, Some((2.032, 0.0, 90.0)));
        assert_eq!(anchors.value_at, Some((0.0, 0.0, 90.0)));
    }

    #[test]
    fn reads_instance_metadata_from_direct_library_properties_only() {
        let (sch, _dir) = schematic_with(
            "(symbol \"Device:R\" (property \"Reference\" \"R\" (at 0 0 0)) (property \"Value\" \"R\" (at 0 0 0)) (property \"Footprint\" \"Resistor_THT:R_Axial\" (at 0 0 0)) (property \"Datasheet\" \"~\" (at 0 0 0)) (property \"Description\" \"Resistor\" (at 0 0 0)) (symbol \"R_0_1\" (property \"Description\" \"nested\" (at 0 0 0))))",
        );
        assert_eq!(
            symbol_metadata(&sch, "Device:R"),
            SymbolMetadata {
                reference: "R".to_string(),
                value: "R".to_string(),
                footprint: "Resistor_THT:R_Axial".to_string(),
                datasheet: "~".to_string(),
                description: "Resistor".to_string(),
            }
        );
        assert_eq!(symbol_metadata(&sch, "Device:C"), SymbolMetadata::default());
    }

    /// The anchor alone is not enough: `Regulator_Linear:AP2112K-3.3` puts
    /// Reference and Value on the same `y` and keeps them apart with
    /// `justify left`.
    #[test]
    fn reads_the_justify_that_keeps_fields_on_a_shared_row_apart() {
        let (sch, _dir) = schematic_with(
            "(symbol \"Regulator_Linear:AP2112K-3.3\" (property \"Reference\" \"U\" (at -5.08 5.715 0) (effects (font (size 1.27 1.27)) (justify left))) (property \"Value\" \"AP2112K-3.3\" (at 0 5.715 0) (effects (font (size 1.27 1.27)) (justify left))))",
        );
        let anchors = field_anchors(&sch, "Regulator_Linear:AP2112K-3.3");
        let left = FieldJustify {
            horizontal: Some(HorizontalJustify::Left),
            ..Default::default()
        };
        assert_eq!(anchors.reference_justify, left);
        assert_eq!(anchors.value_justify, left);
        assert_eq!(anchors.reference_justify.tokens(), vec!["left"]);
    }

    #[test]
    fn justify_reads_every_token_and_re_emits_them_in_kicads_order() {
        let (sch, _dir) = schematic_with(
            "(symbol \"Device:R\" (property \"Value\" \"R\" (at 0 0 0) (effects (font (size 1.27 1.27)) (justify mirror bottom right nonsense))))",
        );
        let justify = field_anchors(&sch, "Device:R").value_justify;
        assert_eq!(justify.horizontal, Some(HorizontalJustify::Right));
        assert_eq!(justify.vertical, Some(VerticalJustify::Bottom));
        assert!(justify.mirror);
        assert_eq!(justify.tokens(), vec!["right", "bottom", "mirror"]);
    }

    /// No `(justify …)` is how KiCad spells centred, and centred must stay
    /// spelled that way rather than becoming an explicit token.
    #[test]
    fn a_field_without_justify_is_centred() {
        let (sch, _dir) = schematic_with(
            "(symbol \"Device:R\" (property \"Value\" \"R\" (at 0 0 90) (effects (font (size 1.27 1.27)))))",
        );
        let justify = field_anchors(&sch, "Device:R").value_justify;
        assert_eq!(justify, FieldJustify::default());
        assert!(justify.tokens().is_empty());
    }

    /// The whole point of #101: one fixed offset cannot serve both, because
    /// GND anchors Value below the origin and VCC above it.
    #[test]
    fn opposite_pointing_power_symbols_anchor_value_opposite_ways() {
        let (sch, _dir) = schematic_with(
            "(symbol \"power:GND\" (property \"Value\" \"GND\" (at 0 -3.81 0)))\n(symbol \"power:VCC\" (property \"Value\" \"VCC\" (at 0 3.556 0)))",
        );
        assert_eq!(
            field_anchors(&sch, "power:GND").value_at,
            Some((0.0, -3.81, 0.0))
        );
        assert_eq!(
            field_anchors(&sch, "power:VCC").value_at,
            Some((0.0, 3.556, 0.0))
        );
    }

    #[test]
    fn a_symbol_the_schematic_does_not_embed_has_no_anchors() {
        let (sch, _dir) =
            schematic_with("(symbol \"Device:R\" (property \"Value\" \"R\" (at 0 0 0)))");
        assert_eq!(field_anchors(&sch, "Device:C"), FieldAnchors::default());
    }

    /// Unit sub-symbols carry graphics, not instance fields — a property
    /// nested in one must not be mistaken for the symbol's own anchor.
    #[test]
    fn unit_sub_symbol_properties_are_not_anchors() {
        let (sch, _dir) = schematic_with(
            "(symbol \"Device:R\" (property \"Reference\" \"R\" (at 2.032 0 90)) (symbol \"R_0_1\" (property \"Value\" \"nested\" (at 99 99 0))))",
        );
        let anchors = field_anchors(&sch, "Device:R");
        assert_eq!(anchors.reference_at, Some((2.032, 0.0, 90.0)));
        assert_eq!(anchors.value_at, None, "nested property must not count");
    }

    #[test]
    fn a_property_without_an_at_is_skipped() {
        let (sch, _dir) = schematic_with("(symbol \"Device:R\" (property \"Value\" \"R\"))");
        assert_eq!(field_anchors(&sch, "Device:R").value_at, None);
    }
}
