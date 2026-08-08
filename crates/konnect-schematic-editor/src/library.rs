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
    // Check if already present
    let check_name = format!("\"{}\"", lib_id);
    let already_present = schematic.raw_other.iter().any(|node| {
        if node.tag() == Some("lib_symbols") {
            let content = format!("{:?}", node);
            content.contains(&check_name)
        } else {
            false
        }
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
            "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"LM2904\"\n\t\t(pin_names (offset 0.127))\n\t\t(in_bom yes)\n\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t(property \"Value\" \"LM2904\" (at 0 0 0))\n\t\t(property \"Datasheet\" \"lm2904.pdf\" (at 0 0 0))\n\t\t(symbol \"LM2904_1_1\"\n\t\t\t(pin output line (at 7.62 0 180) (length 2.54)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t)\n\t\t(symbol \"LM2904_2_1\"\n\t\t\t(pin output line (at 7.62 0 180) (length 2.54)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"7\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t)\n\t)\n)\n",
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
