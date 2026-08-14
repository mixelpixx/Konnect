//! Library symbol resolution — loads symbol definitions from KiCAD's installed libraries.
//!
//! KiCAD 10 stores symbols in `.kicad_symdir` directories:
//! ```text
//! C:\KiCad\10.0\share\kicad\symbols\Device.kicad_symdir\R.kicad_sym
//! C:\KiCad\10.0\share\kicad\symbols\power.kicad_symdir\VCC.kicad_sym
//! ```
//!
//! This module resolves a `lib_id` like `"Device:R"` to the full symbol S-expression
//! definition, and can inject it into a Schematic's `lib_symbols` section.

use crate::sexp::{parser, SexpNode};
use crate::Schematic;
use std::path::{Path, PathBuf};

/// Prefix a symbol block's outer name with its library, as eeschema expects
/// in an embedded `lib_symbols` entry, and prefix any `(extends "PARENT")`
/// to match.
///
/// Unit sub-symbols ("Name_0_1", "Name_1_1") deliberately stay UNPREFIXED:
/// eeschema names only the outer symbol with the library prefix and refuses
/// to load a schematic whose units carry it ("Failed to load schematic" —
/// verified against kicad-cli 10.0 and the KiCAD demo corpus).
fn prefix_symbol_block(block: &str, library_name: &str, symbol_name: &str) -> String {
    let mut renamed = block.replacen(
        &format!("(symbol \"{}\"", symbol_name),
        &format!("(symbol \"{}:{}\"", library_name, symbol_name),
        1,
    );
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
pub fn resolve_lib_symbol(lib_id: &str) -> Option<String> {
    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return None;
    }
    let (library_name, symbol_name) = (parts[0], parts[1]);

    // sym-lib-table first: it is what KiCad itself uses, so it is the only
    // source that knows about user libraries and about installs sitting
    // outside the hardcoded paths in `find_symbol_dirs`. It also carries the
    // nickname → file mapping, which a directory scan has to guess.
    if let Some(path) = symbol_lib_path(library_name) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Some(block) = extract_symbol_block(&content, symbol_name) {
                return Some(prefix_symbol_block(&block, library_name, symbol_name));
            }
        }
    }

    for base_dir in find_symbol_dirs() {
        // KiCAD 10: Library.kicad_symdir/SymbolName.kicad_sym
        let symdir = base_dir.join(format!("{}.kicad_symdir", library_name));
        let sym_file = symdir.join(format!("{}.kicad_sym", symbol_name));

        if sym_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&sym_file) {
                if let Some(block) = extract_symbol_block(&content, symbol_name) {
                    // Rename symbol to include library prefix
                    let mut renamed = block.replacen(
                        &format!("(symbol \"{}\"", symbol_name),
                        &format!("(symbol \"{}:{}\"", library_name, symbol_name),
                        1,
                    );
                    // Also fix (extends "ParentName") to use prefixed name
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
                    // Unit sub-symbols ("Name_0_1", "Name_1_1") must stay
                    // UNPREFIXED: eeschema names only the outer symbol with
                    // the library prefix and refuses to load a schematic
                    // whose units carry it ("Failed to load schematic" —
                    // verified against kicad-cli 10.0 and the KiCAD demo
                    // corpus, which embeds units without the prefix).
                    return Some(renamed);
                }
            }
        }

        // Fallback: KiCAD 8/9 format — single Library.kicad_sym file
        let legacy = base_dir.join(format!("{}.kicad_sym", library_name));
        if legacy.exists() {
            if let Ok(content) = std::fs::read_to_string(&legacy) {
                if let Some(block) = extract_symbol_block(&content, symbol_name) {
                    let mut renamed = block.replacen(
                        &format!("(symbol \"{}\"", symbol_name),
                        &format!("(symbol \"{}:{}\"", library_name, symbol_name),
                        1,
                    );
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
                    // Unit sub-symbols stay UNPREFIXED here too — same rule
                    // as the symdir branch above (eeschema refuses prefixed
                    // unit names; hit in CI where KiCAD ships single-file
                    // libraries and this legacy branch handles the embed).
                    return Some(renamed);
                }
            }
        }
    }
    None
}

/// Resolve a lib_id to a parsed SexpNode tree.
pub fn resolve_lib_symbol_node(lib_id: &str) -> Option<SexpNode> {
    let raw = resolve_lib_symbol(lib_id)?;
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
pub fn resolve_lib_symbol_flattened_node(lib_id: &str) -> Option<SexpNode> {
    let mut node = resolve_lib_symbol_node(lib_id)?;
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
        let Some(parent) = resolve_lib_symbol_node(&pid) else {
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
pub fn resolve_lib_symbol_flattened(lib_id: &str) -> Option<String> {
    resolve_lib_symbol_flattened_node(lib_id).map(|n| crate::sexp::writer::write(&n))
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
pub fn ensure_lib_symbol(schematic: &mut Schematic, lib_id: &str) -> bool {
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
    let sym_node = match resolve_lib_symbol_flattened_node(lib_id) {
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
pub fn symbol_unit_count(lib_id: &str) -> Option<u32> {
    let mut current = lib_id.to_string();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    while visited.insert(current.clone()) {
        let node = resolve_lib_symbol_node(&current)?;
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

/// Whether `library_name` (e.g. "Device") exists in any installed symbol dir,
/// in either the KiCAD 10 symdir layout or the legacy single-file one.
pub fn library_exists(library_name: &str) -> bool {
    find_symbol_dirs().iter().any(|base| {
        base.join(format!("{}.kicad_symdir", library_name)).is_dir()
            || base.join(format!("{}.kicad_sym", library_name)).is_file()
    })
}

/// Symbol names similar to the one in `lib_id`, for did-you-mean hints when a
/// lib_id doesn't resolve (#34: LLM callers habitually reach for KiCAD ≤9
/// names like `Device:CP` that KiCAD 10 renamed). Returns full `Library:Name`
/// ids, closest first, at most `limit`.
pub fn suggest_symbols(lib_id: &str, limit: usize) -> Vec<String> {
    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Vec::new();
    }
    let (library_name, symbol_name) = (parts[0], parts[1]);
    let wanted = symbol_name.to_lowercase();

    let mut candidates: Vec<String> = Vec::new();
    for base in find_symbol_dirs() {
        let symdir = base.join(format!("{}.kicad_symdir", library_name));
        if let Ok(entries) = std::fs::read_dir(&symdir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("kicad_sym") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        candidates.push(stem.to_string());
                    }
                }
            }
        }
        // Legacy single-file library: scan top-level (symbol "NAME" entries.
        let legacy = base.join(format!("{}.kicad_sym", library_name));
        if let Ok(content) = std::fs::read_to_string(&legacy) {
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

// ─── KiCad library-table resolution ──────────────────────────────────────────
//
// KiCad finds a symbol library by nickname through `sym-lib-table`, not by
// scanning directories for a file named after the nickname. Resolving by
// directory scan alone misses two whole classes of library:
//
//   * every user library, wherever it lives on disk;
//   * KiCad's own, whenever it is installed outside the hardcoded paths
//     below (a macOS bundle dragged anywhere but /Applications, say).
//
// It also assumes nickname == filename, which KiCad never requires: a table
// may register `.../TM16xx.kicad_sym` under the nickname `JY-TM16xx`.
//
// The table is therefore consulted first, and the directory scan kept as a
// fallback for setups with no table at all.

/// KiCad's per-user configuration directory, where `sym-lib-table` and
/// `kicad_common.json` live. Newest supported version wins.
fn kicad_config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = PathBuf::from(std::env::var("APPDATA").ok()?).join("kicad");
    #[cfg(target_os = "macos")]
    let base = PathBuf::from(std::env::var("HOME").ok()?).join("Library/Preferences/kicad");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = PathBuf::from(std::env::var("HOME").ok()?).join(".config/kicad");

    for ver in ["10.0", "9.0", "8.0"] {
        let d = base.join(ver);
        if d.is_dir() {
            return Some(d);
        }
    }
    base.is_dir().then_some(base)
}

/// Path variables the user defined in KiCad's Preferences → Configure Paths.
///
/// These live in `kicad_common.json`, not the process environment, so a plain
/// `std::env::var` lookup never sees them — yet they are the normal way to
/// write a portable library table (`${MY_LIB}/parts.kicad_sym`).
fn kicad_user_path_vars() -> Vec<(String, String)> {
    let Some(cfg) = kicad_config_dir() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(cfg.join("kicad_common.json")) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    json.get("environment")
        .and_then(|e| e.get("vars"))
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Expand a lib-table URI to a filesystem path.
///
/// `table_dir` is the directory of the table the URI came from: it is the
/// expansion base for `${KIPRJMOD}` (KiCad's per-project variable), and the
/// anchor for the `${KICAD*_SYMBOL_DIR}` fallback — KiCad's bundled table sits
/// in `<share>/template` with the symbols beside it in `<share>/symbols`, so
/// the install root is recoverable from the table's own location even when the
/// variable is unset.
fn expand_lib_uri(uri: &str, table_dir: Option<&Path>) -> Option<PathBuf> {
    let Some(rest) = uri.strip_prefix("${") else {
        return (!uri.is_empty()).then(|| PathBuf::from(uri));
    };
    let close = rest.find('}')?;
    let var = &rest[..close];
    let tail = rest[close + 1..].trim_start_matches(['/', '\\']);

    if var == "KIPRJMOD" {
        return table_dir.map(|d| d.join(tail));
    }

    if let Some(base) = std::env::var_os(var) {
        return Some(PathBuf::from(base).join(tail));
    }

    for (k, v) in kicad_user_path_vars() {
        if k == var {
            return Some(PathBuf::from(v).join(tail));
        }
    }

    // KiCad's own ${KICAD<n>_SYMBOL_DIR}, unset in our environment.
    if var.ends_with("_SYMBOL_DIR") {
        if let Some(d) = table_dir.and_then(|d| d.parent()) {
            let p = d.join("symbols").join(tail);
            if p.exists() {
                return Some(p);
            }
        }
        return find_symbol_dirs()
            .into_iter()
            .map(|d| d.join(tail))
            .find(|p| p.exists());
    }

    None
}

/// Every `(lib …)` entry of a sym-lib-table as `(nickname, path)`, following
/// `(type "Table")` indirection. `depth` bounds a table that references itself.
fn read_sym_lib_table(path: &Path, depth: usize) -> Vec<(String, PathBuf)> {
    if depth > 4 {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let dir = path.parent();
    let mut out = Vec::new();

    for line in content.lines() {
        let Some(name) = field(line, "name") else {
            continue;
        };
        let Some(uri) = field(line, "uri") else {
            continue;
        };
        let Some(resolved) = expand_lib_uri(&uri, dir) else {
            continue;
        };
        if field(line, "type").as_deref() == Some("Table") {
            out.extend(read_sym_lib_table(&resolved, depth + 1));
        } else {
            out.push((name, resolved));
        }
    }
    out
}

/// Value of `(<key> "…")` on a lib-table line, if present.
fn field(line: &str, key: &str) -> Option<String> {
    let pat = format!("({key} \"");
    let start = line.find(&pat)? + pat.len();
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_string())
}

/// On-disk `.kicad_sym` for a library nickname, via the global sym-lib-table.
fn symbol_lib_path(nickname: &str) -> Option<PathBuf> {
    let table = kicad_config_dir()?.join("sym-lib-table");
    read_sym_lib_table(&table, 0)
        .into_iter()
        .find(|(nick, _)| nick == nickname)
        .map(|(_, p)| p)
}

/// Find directories where KiCAD symbol libraries are stored.
///
/// Fallback only: `sym-lib-table` is authoritative and is consulted first.
pub fn find_symbol_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(dir) = std::env::var("KICAD10_SYMBOL_DIR") {
        let p = PathBuf::from(&dir);
        if p.is_dir() {
            dirs.push(p);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let candidates = [
            r"C:\KiCad\10.0\share\kicad\symbols",
            r"C:\Program Files\KiCad\10.0\share\kicad\symbols",
            r"C:\KiCad\9.0\share\kicad\symbols",
            r"C:\Program Files\KiCad\9.0\share\kicad\symbols",
        ];
        for c in &candidates {
            let p = PathBuf::from(c);
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // KiCad on macOS ships its libraries inside the app bundle.
        let mut candidates = vec![
            PathBuf::from("/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols"),
            PathBuf::from("/usr/local/share/kicad/symbols"),
        ];
        if let Ok(home) = std::env::var("HOME") {
            // Per-user install (KiCad.app dragged into ~/Applications)
            candidates.push(
                PathBuf::from(home)
                    .join("Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols"),
            );
        }
        for p in candidates {
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let candidates = ["/usr/share/kicad/symbols", "/usr/local/share/kicad/symbols"];
        for c in &candidates {
            let p = PathBuf::from(c);
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    dirs
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
        std::env::set_var("KICAD10_SYMBOL_DIR", libdir.path());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flat.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n\t)\n)\n",
        )
        .unwrap();
        let mut sch = Schematic::load(&path).unwrap();
        assert!(ensure_lib_symbol(&mut sch, "Amp:NE5532"));
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
        assert!(!ensure_lib_symbol(
            &mut sch,
            "Definitely_Not_A_Library_xyzzy:Nope"
        ));
    }
}

#[cfg(test)]
mod lib_table_tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    /// A plain URI is taken as written.
    #[test]
    fn expand_lib_uri_passes_through_a_plain_path() {
        assert_eq!(
            expand_lib_uri("/libs/parts.kicad_sym", None),
            Some(PathBuf::from("/libs/parts.kicad_sym"))
        );
        assert_eq!(expand_lib_uri("", None), None);
    }

    /// ${KIPRJMOD} resolves against the table's own directory, because KiCad
    /// sets it per open project — an exported value could name another project.
    #[test]
    fn expand_lib_uri_resolves_kiprjmod_from_the_table_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            expand_lib_uri("${KIPRJMOD}/local.kicad_sym", Some(dir.path())),
            Some(dir.path().join("local.kicad_sym"))
        );
        // No table dir to anchor against: unresolvable rather than guessed.
        assert_eq!(expand_lib_uri("${KIPRJMOD}/local.kicad_sym", None), None);
    }

    /// An exported environment variable is honoured.
    #[test]
    fn expand_lib_uri_expands_an_exported_env_var() {
        std::env::set_var("KONNECT_TEST_LIBDIR", "/opt/parts");
        assert_eq!(
            expand_lib_uri("${KONNECT_TEST_LIBDIR}/x.kicad_sym", None),
            Some(PathBuf::from("/opt/parts/x.kicad_sym"))
        );
        std::env::remove_var("KONNECT_TEST_LIBDIR");
    }

    /// An unknown variable fails rather than silently dropping the prefix and
    /// producing a wrong relative path.
    #[test]
    fn expand_lib_uri_rejects_an_unknown_variable() {
        assert_eq!(
            expand_lib_uri("${NO_SUCH_VAR_HERE}/x.kicad_sym", None),
            None
        );
    }

    /// The regression this exists for: a nickname that does not match the
    /// filename. KiCad allows it; resolving by directory scan cannot.
    #[test]
    fn table_maps_a_nickname_that_differs_from_the_filename() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "TM16xx.kicad_sym", "(kicad_symbol_lib)");
        let table = write(
            dir.path(),
            "sym-lib-table",
            &format!(
                "(sym_lib_table\n\t(version 7)\n\t(lib (name \"JY-TM16xx\") (type \"KiCad\") (uri \"{}/TM16xx.kicad_sym\") (options \"\") (descr \"\"))\n)\n",
                dir.path().display()
            ),
        );
        let entries = read_sym_lib_table(&table, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "JY-TM16xx");
        assert!(entries[0].1.ends_with("TM16xx.kicad_sym"));
    }

    /// `(type "Table")` indirection is followed, which is how KiCad references
    /// its own bundled libraries and how a shared master table is wired in.
    #[test]
    fn nested_table_indirection_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let inner_dir = dir.path().join("inner");
        write(&inner_dir, "Parts.kicad_sym", "(kicad_symbol_lib)");
        write(
            &inner_dir,
            "sym-lib-table",
            &format!(
                "(sym_lib_table\n\t(lib (name \"Inner\") (type \"KiCad\") (uri \"{}/Parts.kicad_sym\") (options \"\") (descr \"\"))\n)\n",
                inner_dir.display()
            ),
        );
        let outer = write(
            dir.path(),
            "sym-lib-table",
            &format!(
                "(sym_lib_table\n\t(lib (name \"Group\") (type \"Table\") (uri \"{}/sym-lib-table\") (options \"\") (descr \"\"))\n)\n",
                inner_dir.display()
            ),
        );
        let entries = read_sym_lib_table(&outer, 0);
        assert_eq!(entries.len(), 1, "the nested table's entry should surface");
        assert_eq!(entries[0].0, "Inner");
    }

    /// A table that references itself must terminate rather than recurse away.
    #[test]
    fn self_referencing_table_terminates() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sym-lib-table");
        write(
            dir.path(),
            "sym-lib-table",
            &format!(
                "(sym_lib_table\n\t(lib (name \"Loop\") (type \"Table\") (uri \"{}\") (options \"\") (descr \"\"))\n)\n",
                p.display()
            ),
        );
        let _ = read_sym_lib_table(&p, 0); // must return, not hang or overflow
    }

    #[test]
    fn field_extracts_quoted_values() {
        let line =
            r#"(lib (name "A B") (type "KiCad") (uri "/x/y.kicad_sym") (options "") (descr ""))"#;
        assert_eq!(field(line, "name").as_deref(), Some("A B"));
        assert_eq!(field(line, "type").as_deref(), Some("KiCad"));
        assert_eq!(field(line, "uri").as_deref(), Some("/x/y.kicad_sym"));
        assert_eq!(field(line, "missing"), None);
    }
}
