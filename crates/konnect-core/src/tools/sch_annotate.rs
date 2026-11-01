//! `annotate_schematic` (#454): Konnect's own annotator, because kicad-cli has
//! no `sch annotate`.
//!
//! The semantics are eeschema's Tools → Annotate defaults, pinned against
//! eeschema 10.0.5 on `tests/fixtures/annotate_duplicates*.kicad_sch`:
//!
//! - a `?` designator is numbered with the first free number for its prefix
//!   on its sheet instance, in ascending X (then Y);
//! - a designator that is already numbered is kept, so two symbols that share
//!   one are **reported**, never renumbered — unless `resolve_duplicates`
//!   asks for it, in which case the first in ascending X keeps the number and
//!   the rest get the first free one, which is what eeschema's "Reset existing
//!   annotations" produced on the fixture;
//! - a designator lives in two places, `(property "Reference" …)` and the
//!   `(instances …)` block, and both are written, as eeschema writes both;
//! - a `#`-prefixed designator is spelled the way eeschema spells it,
//!   `#PWR01`, `#PWR010`, `#PWR0100`: the prefix, a `0`, then the number.
//!
//! Every count in the response is derived from the committed file's readback,
//! never from the plan.

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::outcome::{self, OutcomeStatus};
use konnect_sexp::{
    parse_sexp,
    schematic::parse_subsymbol_unit,
    writer::{
        apply_edits, find_enclosing_block, read_consistent, write_atomic_if_unchanged, SexpEdit,
    },
    SexpError, SexpNode,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tracing::info;

const OPERATION: &str = "annotate_schematic";

/// An optional boolean argument: absent or `null` is `false`; anything else
/// that is not a boolean is refused by name rather than read as a default.
pub(crate) fn opt_bool(args: &Value, key: &str) -> Result<bool, CallToolResult> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: key.to_string(),
                reason: "must be a boolean".to_string(),
            },
            format!("Argument '{key}' must be a boolean"),
        )),
    }
}

// ─── Model ────────────────────────────────────────────────────────────────────

/// One `(instances (project … (path … (reference …))))` entry of a placed symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathReference {
    pub project: String,
    pub path: String,
    pub reference: String,
}

/// A placed symbol as the annotator sees it: KiCad's ordering key, the property
/// designator, and the per-sheet-instance designators in its instances block.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlacedSymbol {
    pub uuid: String,
    pub lib_symbol: String,
    pub unit: u32,
    /// Units the embedded library definition declares (`Name_U_S` sub-symbols);
    /// `None` when the definition is missing from `lib_symbols`, in which case
    /// package identity cannot be proven.
    pub unit_count: Option<u32>,
    pub value: String,
    pub x: f64,
    pub y: f64,
    pub property_reference: String,
    pub paths: Vec<PathReference>,
}

/// Every placed symbol of a schematic, in file order.
///
/// Only the root's direct `symbol` children are placements; the definitions
/// under `lib_symbols` are nested one level down and carry no `lib_id`.
/// Unit count per embedded library definition, read from the `Name_U_S`
/// sub-symbol names KiCad writes under `lib_symbols`; a derived definition
/// (`extends`) takes its parent's count. A definition with no unit
/// sub-symbols is a single-unit part.
fn lib_symbol_units(tree: &SexpNode) -> BTreeMap<String, u32> {
    let mut declared: BTreeMap<String, Option<u32>> = BTreeMap::new();
    let mut extends: BTreeMap<String, String> = BTreeMap::new();
    if let Some(lib) = tree.find("lib_symbols") {
        for definition in lib.find_all("symbol") {
            let Some(name) = definition.get(1).and_then(SexpNode::as_str) else {
                continue;
            };
            let max_unit = definition
                .find_all("symbol")
                .iter()
                .filter_map(|sub| sub.get(1).and_then(SexpNode::as_str))
                .filter_map(parse_subsymbol_unit)
                .filter(|&unit| unit >= 1)
                .max();
            if let Some(parent) = definition.find_str("extends") {
                extends.insert(name.to_string(), parent.to_string());
            }
            declared.insert(name.to_string(), max_unit);
        }
    }
    let mut out = BTreeMap::new();
    for name in declared.keys() {
        let mut current = name.clone();
        let mut seen = BTreeSet::new();
        let count = loop {
            if !seen.insert(current.clone()) {
                break 1;
            }
            match declared.get(&current) {
                Some(Some(units)) => break *units,
                Some(None) => match extends.get(&current) {
                    Some(parent) => current = parent.clone(),
                    None => break 1,
                },
                None => break 1,
            }
        };
        out.insert(name.clone(), count);
    }
    out
}

pub(crate) fn placed_symbols(tree: &SexpNode) -> Result<Vec<PlacedSymbol>, String> {
    let lib_units = lib_symbol_units(tree);
    let mut out = Vec::new();
    for node in tree.find_all("symbol") {
        let Some(lib_id) = node.find_str("lib_id") else {
            continue;
        };
        let lib_symbol = node.find_str("lib_name").unwrap_or(lib_id).to_string();
        let at = node
            .find("at")
            .ok_or_else(|| format!("a placed '{lib_id}' has no (at …)"))?;
        let (x, y) = match (at.get_f64(1), at.get_f64(2)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Err(format!("a placed '{lib_id}' has a malformed (at …)")),
        };
        let unit = node.find_f64("unit").map(|u| u as u32).unwrap_or(1);
        let uuid = node
            .find_str("uuid")
            .ok_or_else(|| format!("the '{lib_id}' at ({x}, {y}) has no uuid"))?
            .to_string();
        let property = |name: &str| {
            node.find_all("property")
                .into_iter()
                .find(|property| property.get(1).and_then(SexpNode::as_str) == Some(name))
                .and_then(|property| property.get(2))
                .and_then(SexpNode::as_str)
                .map(str::to_string)
        };
        let property_reference = property("Reference")
            .ok_or_else(|| format!("symbol {uuid} has no Reference property"))?;
        let value = property("Value").unwrap_or_default();
        let mut paths = Vec::new();
        if let Some(instances) = node.find("instances") {
            for project in instances.find_all("project") {
                let project_name = project
                    .get(1)
                    .and_then(SexpNode::as_str)
                    .unwrap_or_default()
                    .to_string();
                for path in project.find_all("path") {
                    let Some(path_name) = path.get(1).and_then(SexpNode::as_str) else {
                        continue;
                    };
                    let reference = path
                        .find_str("reference")
                        .ok_or_else(|| {
                            format!("symbol {uuid} has an instance path without a reference")
                        })?
                        .to_string();
                    paths.push(PathReference {
                        project: project_name.clone(),
                        path: path_name.to_string(),
                        reference,
                    });
                }
            }
        }
        out.push(PlacedSymbol {
            uuid,
            unit_count: lib_units.get(&lib_symbol).copied(),
            lib_symbol,
            unit,
            value,
            x,
            y,
            property_reference,
            paths,
        });
    }
    Ok(out)
}

// ─── Designators ──────────────────────────────────────────────────────────────

/// A designator split the way eeschema splits it: the letters (and any `#`)
/// in front, the number behind. `None` is unannotated: a trailing `?`, or no
/// number at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Designator {
    pub prefix: String,
    pub number: Option<u32>,
}

pub(crate) fn parse_designator(reference: &str) -> Designator {
    let unannotated = reference.ends_with('?');
    let trimmed = reference.trim_end_matches('?');
    let prefix_len = trimmed.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let number = if unannotated {
        None
    } else {
        trimmed[prefix_len..].parse::<u32>().ok()
    };
    Designator {
        prefix: trimmed[..prefix_len].to_string(),
        number,
    }
}

/// Spell a designator the way eeschema does. Power-style prefixes get a `0`
/// between prefix and number: eeschema writes `#PWR01`, `#PWR010`, `#PWR0100`
/// (every KiCad demo agrees), never `#PWR1` or `#PWR001`.
pub(crate) fn format_designator(prefix: &str, number: u32) -> String {
    if prefix.starts_with('#') {
        format!("{prefix}0{number}")
    } else {
        format!("{prefix}{number}")
    }
}

// ─── Plan ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Assignment {
    pub uuid: String,
    pub unit: u32,
    pub project: String,
    pub path: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DuplicateGroup {
    pub reference: String,
    pub project: String,
    pub paths: Vec<String>,
    pub uuids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Unresolved {
    pub reference: String,
    pub project: String,
    pub paths: Vec<String>,
    pub uuids: Vec<String>,
    pub reason: String,
}

/// A symbol whose instance records name other projects but not the selected
/// one: outside this annotation, and left exactly as found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OutsideProject {
    pub uuid: String,
    pub reference: String,
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct AnnotationPlan {
    pub project: String,
    pub paths: Vec<String>,
    pub unannotated_before: usize,
    pub duplicates_before: Vec<DuplicateGroup>,
    pub assignments: Vec<Assignment>,
    pub unresolved: Vec<Unresolved>,
    pub outside_project: Vec<OutsideProject>,
}

const REASON_DUPLICATE: &str = "duplicate designator in this project; set resolve_duplicates \
                                to keep the first in ascending X and renumber the rest";
const REASON_AMBIGUOUS: &str = "shared by different library symbols, at least one of them \
                                multi-unit; whether these are separate parts or units of one \
                                package is not proven, so nothing is renumbered";
const REASON_UNPROVEN_PACKAGE: &str =
    "units of one multi-unit part share this designator but do not form one package (a unit \
     repeats, is out of range, or the values differ); which units belong together is not \
     proven, so nothing is renumbered";
const REASON_UNPROVEN_NEW_PACKAGE: &str =
    "unannotated units of one multi-unit part on this sheet instance form more than one \
     package; which units belong together is not proven, so they stay unannotated";
const REASON_NO_DEFINITION: &str =
    "the library definition is missing from lib_symbols, so the unit count and package \
     identity cannot be proven; nothing is renumbered";
const REASON_NO_INSTANCE: &str =
    "no (instances …) entry ties this symbol to a sheet instance, so it \
                                  is left alone";

/// What a set of placements sharing one designator provably is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Identity {
    /// The units of one multi-unit package: not a duplicate.
    OnePackage,
    /// Distinct single-unit parts: duplicates that can be renumbered
    /// placement by placement without dismantling anything.
    SeparateParts,
    /// Neither is proven; renumbering could split a package.
    Unproven(&'static str),
}

fn package_identity(members: &[&PlacedSymbol]) -> Identity {
    let Some(first) = members.first() else {
        return Identity::OnePackage;
    };
    let libraries: BTreeSet<&str> = members.iter().map(|m| m.lib_symbol.as_str()).collect();
    let units: BTreeSet<u32> = members.iter().map(|m| m.unit).collect();
    let all_single = members
        .iter()
        .all(|m| m.unit_count == Some(1) && m.unit == 1);
    if libraries.len() > 1 {
        return if all_single {
            Identity::SeparateParts
        } else {
            Identity::Unproven(REASON_AMBIGUOUS)
        };
    }
    match first.unit_count {
        None => Identity::Unproven(REASON_NO_DEFINITION),
        Some(1) => {
            if all_single {
                Identity::SeparateParts
            } else {
                Identity::Unproven(REASON_UNPROVEN_PACKAGE)
            }
        }
        Some(count) => {
            let same_value = members.iter().all(|m| m.value == first.value);
            let in_range = units.iter().all(|unit| (1..=count).contains(unit));
            if same_value && in_range && units.len() == members.len() {
                Identity::OnePackage
            } else {
                Identity::Unproven(REASON_UNPROVEN_PACKAGE)
            }
        }
    }
}

/// eeschema's annotation order: ascending X, then Y; the uuid only breaks an
/// exact tie so the result is deterministic.
fn kicad_order(a: &PlacedSymbol, b: &PlacedSymbol) -> std::cmp::Ordering {
    a.x.total_cmp(&b.x)
        .then(a.y.total_cmp(&b.y))
        .then_with(|| a.uuid.cmp(&b.uuid))
}

fn distinct_paths(symbols: &[PlacedSymbol], entries: &[(usize, usize)]) -> Vec<String> {
    entries
        .iter()
        .map(|&(si, pi)| symbols[si].paths[pi].path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// (symbol index, path index) into the placed symbols.
type Entry = (usize, usize);
/// One number to hand out: the sheet instance path, the leftmost member's
/// symbol index for ordering, and every entry that receives the number.
type Allocation = (String, usize, Vec<Entry>);
/// (path, prefix, library symbol, value): the unannotated placements that
/// could be the units of one package.
type NewGroupKey = (String, String, String, String);

/// Plan the annotation of one project's instance records.
///
/// References are unique across the whole project (KiCad's flat list), so
/// numbers are reserved per project across every sheet instance in the file,
/// and a designator shared across instances is a duplicate unless it is the
/// units of one package. Instance records of other projects are never planned
/// or edited. Numbers used on the project's other sheets arrive through
/// `reserved` (see [`reserved_on_other_sheets`]); duplicates that already
/// exist across files are not detected here — annotating a whole hierarchy
/// from its root is #463.
#[cfg(test)]
pub(crate) fn plan_annotation(
    symbols: &[PlacedSymbol],
    project: &str,
    resolve_duplicates: bool,
) -> AnnotationPlan {
    plan_annotation_reserving(symbols, project, &Reserved::default(), resolve_duplicates)
}

/// Numbers in use per prefix on the project's other sheets, so the file being
/// annotated cannot hand out a number the root or a sibling sheet already
/// owns.
pub(crate) type Reserved = BTreeMap<String, BTreeSet<u32>>;

pub(crate) fn plan_annotation_reserving(
    symbols: &[PlacedSymbol],
    project: &str,
    reserved: &Reserved,
    resolve_duplicates: bool,
) -> AnnotationPlan {
    let mut plan = AnnotationPlan {
        project: project.to_string(),
        ..AnnotationPlan::default()
    };

    // Entries of the selected project: (symbol index, path index), in path
    // order then file order.
    let mut entries: Vec<(usize, usize)> = Vec::new();
    for (si, symbol) in symbols.iter().enumerate() {
        if symbol.paths.is_empty() {
            plan.unresolved.push(Unresolved {
                reference: symbol.property_reference.clone(),
                project: project.to_string(),
                paths: Vec::new(),
                uuids: vec![symbol.uuid.clone()],
                reason: REASON_NO_INSTANCE.to_string(),
            });
            continue;
        }
        let mut own = false;
        for (pi, entry) in symbol.paths.iter().enumerate() {
            if entry.project == project {
                entries.push((si, pi));
                own = true;
            }
        }
        if !own {
            plan.outside_project.push(OutsideProject {
                uuid: symbol.uuid.clone(),
                reference: symbol.property_reference.clone(),
                projects: symbol
                    .paths
                    .iter()
                    .map(|p| p.project.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            });
        }
    }
    entries.sort_by(|a, b| {
        symbols[a.0].paths[a.1]
            .path
            .cmp(&symbols[b.0].paths[b.1].path)
            .then(a.cmp(b))
    });
    plan.paths = distinct_paths(symbols, &entries);

    // Numbers in use anywhere in the project, per prefix: the other sheets'
    // first, then every instance in this file.
    let mut used: BTreeMap<String, BTreeSet<u32>> = reserved.clone();
    let mut groups: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
    let mut unannotated: Vec<(usize, usize)> = Vec::new();
    for &(si, pi) in &entries {
        let reference = symbols[si].paths[pi].reference.as_str();
        match parse_designator(reference).number {
            Some(number) => {
                used.entry(parse_designator(reference).prefix)
                    .or_default()
                    .insert(number);
                groups.entry(reference).or_default().push((si, pi));
            }
            None => {
                plan.unannotated_before += 1;
                unannotated.push((si, pi));
            }
        }
    }

    // An allocation takes one number: a single placement, or every unit of a
    // proven package. Ordered by path, then eeschema's X order of the
    // leftmost member.
    let mut allocations: Vec<Allocation> = Vec::new();

    for (reference, members) in &groups {
        if members.len() < 2 {
            continue;
        }
        let placed: Vec<&PlacedSymbol> = members.iter().map(|&(si, _)| &symbols[si]).collect();
        let identity = package_identity(&placed);
        if identity == Identity::OnePackage {
            continue;
        }
        let mut ordered = members.clone();
        ordered.sort_by(|a, b| {
            symbols[a.0].paths[a.1]
                .path
                .cmp(&symbols[b.0].paths[b.1].path)
                .then_with(|| kicad_order(&symbols[a.0], &symbols[b.0]))
        });
        let uuids: Vec<String> = ordered
            .iter()
            .map(|&(si, _)| symbols[si].uuid.clone())
            .collect();
        let paths = distinct_paths(symbols, &ordered);
        plan.duplicates_before.push(DuplicateGroup {
            reference: reference.to_string(),
            project: project.to_string(),
            paths: paths.clone(),
            uuids: uuids.clone(),
        });
        match identity {
            Identity::Unproven(reason) => plan.unresolved.push(Unresolved {
                reference: reference.to_string(),
                project: project.to_string(),
                paths,
                uuids,
                reason: reason.to_string(),
            }),
            Identity::SeparateParts if resolve_duplicates => {
                for (si, pi) in ordered.into_iter().skip(1) {
                    allocations.push((symbols[si].paths[pi].path.clone(), si, vec![(si, pi)]));
                }
            }
            Identity::SeparateParts => plan.unresolved.push(Unresolved {
                reference: reference.to_string(),
                project: project.to_string(),
                paths,
                uuids,
                reason: REASON_DUPLICATE.to_string(),
            }),
            Identity::OnePackage => unreachable!("handled above"),
        }
    }

    // Unannotated placements: the units of one multi-unit part on one sheet
    // instance (same definition and value) are one package when their units
    // are distinct; otherwise which units belong together is a guess, and
    // eeschema's guess is not reproduced here.
    let mut new_groups: BTreeMap<NewGroupKey, Vec<Entry>> = BTreeMap::new();
    for (si, pi) in unannotated {
        let symbol = &symbols[si];
        let key = (
            symbol.paths[pi].path.clone(),
            parse_designator(&symbol.paths[pi].reference).prefix,
            symbol.lib_symbol.clone(),
            symbol.value.clone(),
        );
        new_groups.entry(key).or_default().push((si, pi));
    }
    for ((path, _, _, _), members) in new_groups {
        let count = symbols[members[0].0].unit_count;
        let units: BTreeSet<u32> = members.iter().map(|&(si, _)| symbols[si].unit).collect();
        let one_package = match count {
            Some(count) if count >= 2 => {
                units.len() == members.len() && units.iter().all(|u| (1..=count).contains(u))
            }
            _ => false,
        };
        if one_package {
            let leftmost = members
                .iter()
                .map(|&(si, _)| si)
                .min_by(|&a, &b| kicad_order(&symbols[a], &symbols[b]))
                .unwrap_or(members[0].0);
            allocations.push((path, leftmost, members));
        } else if count.is_some_and(|count| count >= 2) {
            let mut ordered = members.clone();
            ordered.sort_by(|a, b| kicad_order(&symbols[a.0], &symbols[b.0]));
            plan.unresolved.push(Unresolved {
                reference: symbols[ordered[0].0].paths[ordered[0].1].reference.clone(),
                project: project.to_string(),
                paths: vec![path.clone()],
                uuids: ordered
                    .iter()
                    .map(|&(si, _)| symbols[si].uuid.clone())
                    .collect(),
                reason: REASON_UNPROVEN_NEW_PACKAGE.to_string(),
            });
        } else {
            for (si, pi) in members {
                allocations.push((path.clone(), si, vec![(si, pi)]));
            }
        }
    }

    allocations.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| kicad_order(&symbols[a.1], &symbols[b.1]))
    });
    for (path, _, members) in allocations {
        let prefix = parse_designator(&symbols[members[0].0].paths[members[0].1].reference).prefix;
        let taken = used.entry(prefix.clone()).or_default();
        let number = (1u32..).find(|n| !taken.contains(n)).unwrap_or(1);
        taken.insert(number);
        let to = format_designator(&prefix, number);
        for (si, pi) in members {
            let symbol = &symbols[si];
            plan.assignments.push(Assignment {
                uuid: symbol.uuid.clone(),
                unit: symbol.unit,
                project: project.to_string(),
                path: path.clone(),
                from: symbol.paths[pi].reference.clone(),
                to: to.clone(),
            });
        }
    }
    plan
}

// ─── Edits ────────────────────────────────────────────────────────────────────

/// Byte range of the placed `(symbol …)` block carrying `uuid`.
pub(crate) fn symbol_block(content: &str, uuid: &str) -> Result<(usize, usize), String> {
    let needle = format!("(uuid \"{uuid}\")");
    let pos = content
        .find(&needle)
        .ok_or_else(|| format!("symbol {uuid} is not in the file"))?;
    let (start, end) = find_enclosing_block(content, "symbol", pos)
        .ok_or_else(|| format!("symbol {uuid} has no enclosing (symbol …) block"))?;
    if !content[start..end].contains("(lib_id ") {
        return Err(format!(
            "symbol {uuid} resolved to a library definition, not a placement"
        ));
    }
    Ok((start, end))
}

/// The property a symbol shows after its assignments.
///
/// In a reused sheet the property carries the designator of whichever sheet
/// instance eeschema last displayed, which need not be the first instance
/// path in the file (the `multichannel` demo has `#PWR016` in the property
/// and `#PWR024` on its first path). So the property follows the instance
/// whose designator it currently shows, and is otherwise left alone.
fn property_after(symbol: &PlacedSymbol, assignments: &[&Assignment]) -> String {
    assignments
        .iter()
        .find(|a| a.from == symbol.property_reference)
        .map(|a| a.to.clone())
        .unwrap_or_else(|| symbol.property_reference.clone())
}

/// Replace the quoted value that follows `needle` inside `block` (an absolute
/// byte range of `content`), demanding exactly one occurrence.
fn replace_quoted_after(
    content: &str,
    block: (usize, usize),
    needle: &str,
    replacement: &str,
    what: &str,
) -> Result<SexpEdit, String> {
    let text = &content[block.0..block.1];
    let mut hits = text.match_indices(needle);
    let (rel, _) = hits
        .next()
        .ok_or_else(|| format!("{what}: '{needle}' not found in the symbol block"))?;
    if hits.next().is_some() {
        return Err(format!(
            "{what}: '{needle}' occurs more than once in the symbol block"
        ));
    }
    let value_start = block.0 + rel + needle.len();
    let value_len = content[value_start..]
        .find('"')
        .ok_or_else(|| format!("{what}: unterminated value after '{needle}'"))?;
    Ok(SexpEdit::replace(
        value_start,
        value_start + value_len,
        replacement.to_string(),
    ))
}

/// The byte edits that realise `plan` on `content`: for every assigned symbol,
/// the `(reference …)` of each assigned sheet instance and, when it changes,
/// the `(property "Reference" …)`.
pub(crate) fn plan_edits(
    content: &str,
    symbols: &[PlacedSymbol],
    plan: &AnnotationPlan,
) -> Result<Vec<SexpEdit>, String> {
    let mut by_uuid: BTreeMap<&str, Vec<&Assignment>> = BTreeMap::new();
    for assignment in &plan.assignments {
        by_uuid
            .entry(assignment.uuid.as_str())
            .or_default()
            .push(assignment);
    }
    let mut edits = Vec::new();
    for (uuid, assignments) in by_uuid {
        let symbol = symbols
            .iter()
            .find(|s| s.uuid == uuid)
            .ok_or_else(|| format!("assignment for unknown symbol {uuid}"))?;
        let block = symbol_block(content, uuid)?;
        for assignment in &assignments {
            // The instance entry is addressed by project first, then path: two
            // projects sharing a sheet share its path strings.
            let project_needle = format!("(project \"{}\"", assignment.project);
            let rel = content[block.0..block.1]
                .find(&project_needle)
                .ok_or_else(|| {
                    format!(
                        "symbol {uuid} has no instance records for project '{}'",
                        assignment.project
                    )
                })?;
            let project_block = find_enclosing_block(content, "project", block.0 + rel + 1)
                .ok_or_else(|| format!("symbol {uuid}: instance project block not found"))?;
            let path_needle = format!("(path \"{}\"", assignment.path);
            let rel = content[project_block.0..project_block.1]
                .find(&path_needle)
                .ok_or_else(|| {
                    format!(
                        "symbol {uuid} has no instance path '{}' in project '{}'",
                        assignment.path, assignment.project
                    )
                })?;
            let path_block = find_enclosing_block(content, "path", project_block.0 + rel + 1)
                .ok_or_else(|| format!("symbol {uuid}: instance path block not found"))?;
            edits.push(replace_quoted_after(
                content,
                path_block,
                "(reference \"",
                &assignment.to,
                &format!("symbol {uuid} instance '{}'", assignment.path),
            )?);
        }
        let property = property_after(symbol, &assignments);
        if property != symbol.property_reference {
            edits.push(replace_quoted_after(
                content,
                block,
                "(property \"Reference\" \"",
                &property,
                &format!("symbol {uuid} Reference property"),
            )?);
        }
    }
    Ok(edits)
}

/// Prove that `after` is `before` with exactly the plan's assignments applied:
/// every assigned instance shows its new designator, every other instance and
/// every property is as expected, and no symbol appeared or vanished.
pub(crate) fn verify_applied(
    before: &[PlacedSymbol],
    after: &[PlacedSymbol],
    plan: &AnnotationPlan,
) -> Result<(), String> {
    if before.len() != after.len() {
        return Err(format!(
            "symbol count changed from {} to {}",
            before.len(),
            after.len()
        ));
    }
    let assigned: BTreeMap<(&str, &str, &str), &Assignment> = plan
        .assignments
        .iter()
        .map(|a| ((a.uuid.as_str(), a.project.as_str(), a.path.as_str()), a))
        .collect();
    for symbol in before {
        let observed = after
            .iter()
            .find(|s| s.uuid == symbol.uuid)
            .ok_or_else(|| format!("symbol {} vanished", symbol.uuid))?;
        if observed.paths.len() != symbol.paths.len() {
            return Err(format!(
                "symbol {} lost or gained an instance path",
                symbol.uuid
            ));
        }
        for entry in &symbol.paths {
            let expected = assigned
                .get(&(
                    symbol.uuid.as_str(),
                    entry.project.as_str(),
                    entry.path.as_str(),
                ))
                .map(|a| a.to.as_str())
                .unwrap_or(entry.reference.as_str());
            let got = observed
                .paths
                .iter()
                .find(|p| p.project == entry.project && p.path == entry.path)
                .map(|p| p.reference.as_str())
                .ok_or_else(|| {
                    format!(
                        "symbol {} lost instance path '{}' of project '{}'",
                        symbol.uuid, entry.path, entry.project
                    )
                })?;
            if got != expected {
                return Err(format!(
                    "symbol {} on '{}' ({}) reads '{got}', expected '{expected}'",
                    symbol.uuid, entry.path, entry.project
                ));
            }
        }
        let own: Vec<&Assignment> = plan
            .assignments
            .iter()
            .filter(|a| a.uuid == symbol.uuid)
            .collect();
        let expected_property = property_after(symbol, &own);
        if observed.property_reference != expected_property {
            return Err(format!(
                "symbol {} Reference property reads '{}', expected '{expected_property}'",
                symbol.uuid, observed.property_reference
            ));
        }
    }
    Ok(())
}

// ─── Run ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub(crate) struct AnnotateOptions {
    pub resolve_duplicates: bool,
    pub dry_run: bool,
    /// Project whose instance records are annotated; `None` resolves it from
    /// the schematic's owner (its `.kicad_pro`) or the only project named.
    pub project: Option<String>,
}

pub(crate) fn opt_string(args: &Value, key: &str) -> Result<Option<String>, CallToolResult> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: key.to_string(),
                reason: "must be a string".to_string(),
            },
            format!("Argument '{key}' must be a string"),
        )),
    }
}

/// The project whose instance records this call annotates: the explicit
/// argument, else the schematic's owning project (#189's sheet-tree rule),
/// else the only project the file names. Anything ambiguous is refused with
/// the candidates, so another project's records are never edited by accident.
/// The selected project and, when ownership is proven through a `.kicad_pro`
/// and its sheet tree, the root schematic that tree hangs from.
fn select_project(
    path: &Path,
    symbols: &[PlacedSymbol],
    requested: Option<&str>,
) -> Result<(String, Option<std::path::PathBuf>), CallToolResult> {
    let named: Vec<String> = symbols
        .iter()
        .flat_map(|s| s.paths.iter().map(|p| p.project.clone()))
        .filter(|p| !p.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let refuse = |reason: String| {
        CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "project".to_string(),
                reason: reason.clone(),
            },
            format!(
                "Cannot decide which project's instance records to annotate: {reason}. \
                 Pass project to choose one of {named:?}."
            ),
        )
    };
    if named.is_empty() && requested.is_none() {
        // Nothing carries an instance record; the plan reports every symbol
        // as untied to a sheet instance.
        return Ok((String::new(), None));
    }
    // Ownership is consulted even for an explicit project: it is what makes
    // the other sheets' numbers visible.
    let owner = match crate::tools::resolve_schematic_ownership(path) {
        Ok(owner) => owner,
        Err(error) if requested.is_some() => {
            info!("[annotate] project ownership not proven ({error}); only this file's numbers are reserved");
            None
        }
        Err(error) => {
            return Err(refuse(format!("project ownership is not proven: {error}")));
        }
    };
    let owner_stem = owner.as_ref().map(|owner| {
        owner
            .project_file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let root_for = |project: &str| {
        owner
            .as_ref()
            .filter(|_| owner_stem.as_deref() == Some(project))
            .map(|owner| owner.root_schematic.clone())
    };
    if let Some(requested) = requested {
        return if named.iter().any(|n| n == requested) {
            Ok((requested.to_string(), root_for(requested)))
        } else {
            Err(refuse(format!(
                "'{requested}' is not among the projects named by {}'s instance records",
                path.display()
            )))
        };
    }
    match (owner_stem.as_deref(), named.len()) {
        (Some(stem), _) if named.iter().any(|n| n == stem) => {
            let root = root_for(stem);
            Ok((stem.to_string(), root))
        }
        (Some(stem), _) => Err(refuse(format!(
            "the schematic belongs to project '{stem}' but its instance records name {named:?}"
        ))),
        (None, 1) => Ok((named[0].clone(), None)),
        (None, _) => Err(refuse(format!(
            "no .kicad_pro owns this schematic and its instance records name several \
             projects: {named:?}"
        ))),
    }
}

/// Numbered instance references of `project` on every sheet of the tree
/// under `root` except `target` itself, plus the files consulted. A sheet
/// that cannot be read is skipped and named in the second list, so a caller
/// knows which numbers stayed invisible.
pub(crate) fn reserved_on_other_sheets(
    root: &Path,
    target: &Path,
    project: &str,
) -> (Reserved, Vec<String>, Vec<String>) {
    let mut reserved = Reserved::new();
    let mut consulted = Vec::new();
    let mut unreadable = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut seen: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target_canonical = canonical(target);
    while let Some((file, depth)) = stack.pop() {
        if depth > crate::tools::sch_hierarchy::MAX_HIERARCHY_DEPTH
            || !seen.insert(canonical(&file))
        {
            continue;
        }
        let content = match read_consistent(&file) {
            Ok(content) => content,
            Err(_) => {
                unreadable.push(file.display().to_string());
                continue;
            }
        };
        let tree = match parse_sexp(&content) {
            Ok(tree) => tree,
            Err(_) => {
                unreadable.push(file.display().to_string());
                continue;
            }
        };
        let directory = file.parent().unwrap_or_else(|| Path::new("."));
        for child in konnect_sexp::schematic::sheet_files(&tree) {
            stack.push((directory.join(child), depth + 1));
        }
        if canonical(&file) == target_canonical {
            continue;
        }
        let Ok(symbols) = placed_symbols(&tree) else {
            unreadable.push(file.display().to_string());
            continue;
        };
        consulted.push(file.display().to_string());
        for symbol in &symbols {
            for entry in symbol.paths.iter().filter(|p| p.project == project) {
                let designator = parse_designator(&entry.reference);
                if let Some(number) = designator.number {
                    reserved
                        .entry(designator.prefix)
                        .or_default()
                        .insert(number);
                }
            }
        }
    }
    (reserved, consulted, unreadable)
}

fn parse_file(path: &Path, content: &str) -> Result<Vec<PlacedSymbol>, String> {
    let tree = parse_sexp(content).map_err(|e| format!("{}: {e}", path.display()))?;
    placed_symbols(&tree)
}

fn retry_for(plan: &AnnotationPlan, resolve_duplicates: bool) -> Option<Value> {
    if plan.unresolved.is_empty() {
        return None;
    }
    let only_resolvable = plan.unresolved.iter().all(|u| u.reason == REASON_DUPLICATE);
    if only_resolvable && !resolve_duplicates {
        Some(json!({
            "safe": true,
            "scope": "unresolved_duplicates",
            "instruction": "call again with resolve_duplicates: true to renumber the listed duplicate groups; nothing already assigned needs repeating"
        }))
    } else {
        Some(json!({
            "safe": false,
            "scope": "unresolved_designators",
            "instruction": "the listed groups need a decision the tool will not guess at; edit the designators (edit_schematic_component) and call again"
        }))
    }
}

fn body(
    path: &Path,
    options: &AnnotateOptions,
    plan: &AnnotationPlan,
    sheets: (&[String], &[String]),
    written: bool,
) -> Value {
    json!({
        "schematic": path.display().to_string(),
        "dry_run": options.dry_run,
        "resolve_duplicates": options.resolve_duplicates,
        "written": written,
        "project": plan.project,
        "paths": plan.paths,
        "outside_project": plan.outside_project,
        "other_sheets_consulted": sheets.0,
        "other_sheets_unreadable": sheets.1,
        "unannotated_before": plan.unannotated_before,
        "duplicates_before": plan.duplicates_before,
        "assigned": plan.assignments,
        "unresolved": plan.unresolved,
    })
}

/// Annotate the schematic at `path` and report exactly what happened.
pub(crate) fn annotate_file(path: &Path, options: AnnotateOptions) -> CallToolResult {
    annotate_file_with_persistence(path, options, write_atomic_if_unchanged)
}

/// Keep persistence injectable so tests can exercise failures after an actual
/// replacement without racing an external editor. Production uses the shared
/// conditional atomic writer above.
fn annotate_file_with_persistence(
    path: &Path,
    options: AnnotateOptions,
    persist: impl FnOnce(&Path, &str, &str) -> Result<(), SexpError>,
) -> CallToolResult {
    let content = match read_consistent(path) {
        Ok(content) => content,
        Err(SexpError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return CallToolResult::error_kind(
                ToolErrorKind::FileNotFound {
                    path: path.display().to_string(),
                },
                format!("Schematic not found: {}", path.display()),
            );
        }
        Err(error) => {
            return CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: error.to_string(),
                },
                format!("Could not read {}: {error}", path.display()),
            );
        }
    };
    let before = match parse_file(path, &content) {
        Ok(symbols) => symbols,
        Err(reason) => {
            return CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: reason.clone(),
                },
                format!("Could not read the schematic's symbols: {reason}"),
            );
        }
    };
    let (project, root) = match select_project(path, &before, options.project.as_deref()) {
        Ok(selected) => selected,
        Err(refusal) => return refusal,
    };
    let (reserved, other_sheets, unreadable_sheets) = match &root {
        Some(root) => reserved_on_other_sheets(root, path, &project),
        None => (Reserved::new(), Vec::new(), Vec::new()),
    };
    let plan = plan_annotation_reserving(&before, &project, &reserved, options.resolve_duplicates);
    let target = path.display().to_string();
    let requested = plan.assignments.len() + plan.unresolved.len();

    if options.dry_run || plan.assignments.is_empty() {
        let status = if plan.unresolved.is_empty() {
            OutcomeStatus::Complete
        } else {
            OutcomeStatus::Partial
        };
        let source = if options.dry_run {
            "dry_run_plan"
        } else {
            "saved_file"
        };
        let mut response = body(
            path,
            &options,
            &plan,
            (&other_sheets, &unreadable_sheets),
            false,
        );
        response["message"] = json!(if options.dry_run {
            "Dry run: nothing was written; assigned lists what a real run would write."
        } else if plan.unresolved.is_empty() {
            "Nothing to annotate: every designator is numbered and unique on every sheet instance."
        } else {
            "Nothing was written: no designator needed a number, but the listed groups are unresolved."
        });
        return outcome::attach(
            CallToolResult::json(&response),
            outcome::summary(
                status,
                target,
                source,
                requested,
                plan.assignments.len(),
                plan.unresolved.len(),
                retry_for(&plan, options.resolve_duplicates),
            ),
        );
    }

    // Preflight: the exact prospective text must show every assignment and
    // nothing else changed, or nothing is written.
    let edits = match plan_edits(&content, &before, &plan) {
        Ok(edits) => edits,
        Err(reason) => return refused(path, &plan, &options, reason),
    };
    let prospective = apply_edits(content.clone(), edits);
    match parse_file(path, &prospective).and_then(|after| verify_applied(&before, &after, &plan)) {
        Ok(()) => {}
        Err(reason) => return refused(path, &plan, &options, reason),
    }

    match persist(path, &content, &prospective) {
        Ok(()) => {}
        Err(error) => {
            // The shared writer can return Conflict before replacement OR
            // after replacement when its readback differs. Without timing
            // evidence, no persistence error proves that nothing was applied.
            let result = crate::tools::mutation_outcome_uncertain(
                path,
                OPERATION,
                format!("schematic persistence failed: {error}"),
            );
            return outcome::attach(
                result,
                outcome::summary(
                    OutcomeStatus::Uncertain,
                    target,
                    "saved_file_readback",
                    requested,
                    0,
                    requested,
                    Some(outcome::inspect_before_retry()),
                ),
            );
        }
    }

    // Readback: every count below comes from the committed file.
    let committed = match read_consistent(path)
        .map_err(|e| e.to_string())
        .and_then(|content| parse_file(path, &content))
    {
        Ok(after) => after,
        Err(reason) => {
            let result = crate::tools::mutation_outcome_uncertain(
                path,
                OPERATION,
                format!("saved schematic could not be reloaded: {reason}"),
            );
            return outcome::attach(
                result,
                outcome::summary(
                    OutcomeStatus::Uncertain,
                    target,
                    "saved_file_readback",
                    requested,
                    0,
                    requested,
                    Some(outcome::inspect_before_retry()),
                ),
            );
        }
    };
    if let Err(reason) = verify_applied(&before, &committed, &plan) {
        let result = crate::tools::mutation_outcome_uncertain(
            path,
            OPERATION,
            format!("the saved schematic does not show the annotation as planned: {reason}"),
        );
        return outcome::attach(
            result,
            outcome::summary(
                OutcomeStatus::Uncertain,
                target,
                "saved_file_readback",
                requested,
                0,
                requested,
                Some(outcome::inspect_before_retry()),
            ),
        );
    }
    // What still needs a decision after the write, read from the file.
    let remaining =
        plan_annotation_reserving(&committed, &project, &reserved, options.resolve_duplicates);
    let status = if remaining.assignments.is_empty() && remaining.unresolved.is_empty() {
        OutcomeStatus::Complete
    } else {
        OutcomeStatus::Partial
    };
    let mut response = body(
        path,
        &options,
        &plan,
        (&other_sheets, &unreadable_sheets),
        true,
    );
    response["remaining_unannotated"] = json!(remaining.unannotated_before);
    response["message"] = json!(match status {
        OutcomeStatus::Complete =>
            "Annotated; every designator is numbered and unique on every sheet instance.",
        _ => "Annotated what could be decided; the listed groups are unresolved.",
    });
    outcome::attach(
        CallToolResult::json(&response),
        outcome::summary(
            status,
            target,
            "saved_file_readback",
            requested,
            plan.assignments.len(),
            plan.unresolved.len(),
            retry_for(&remaining, options.resolve_duplicates),
        ),
    )
}

/// A refusal before any write: the prospective text did not prove the plan.
fn refused(
    path: &Path,
    plan: &AnnotationPlan,
    options: &AnnotateOptions,
    reason: String,
) -> CallToolResult {
    let requested = plan.assignments.len() + plan.unresolved.len();
    let result = CallToolResult::error_kind(
        ToolErrorKind::StaleTarget {
            target: path.display().to_string(),
            reason: reason.clone(),
        },
        format!(
            "Annotation refused before writing; {} is unchanged: {reason}",
            path.display()
        ),
    );
    let _ = options;
    outcome::attach(
        result,
        outcome::summary(
            OutcomeStatus::Failed,
            path.display().to_string(),
            "saved_file",
            requested,
            0,
            requested,
            Some(outcome::inspect_before_retry()),
        ),
    )
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use crate::mcp::protocol::ToolContent;

    #[test]
    fn persistence_conflicts_require_inspection_before_retry() {
        for after_replacement in [true, false] {
            let (_dir, path) = write_fixture(BEFORE);
            let result = annotate_file_with_persistence(
                &path,
                AnnotateOptions::default(),
                |target, expected, prospective| {
                    if after_replacement {
                        // Execute the real conditional atomic replacement, then
                        // simulate the writer's post-write readback conflict.
                        write_atomic_if_unchanged(target, expected, prospective).unwrap();
                    }
                    Err(SexpError::Conflict {
                        path: target.to_path_buf(),
                    })
                },
            );
            let saved = std::fs::read_to_string(&path).unwrap();
            if after_replacement {
                assert_eq!(designators(&saved), designators(KICAD_KEEP));
            } else {
                assert_eq!(saved, BEFORE);
            }
            let body = response_json(&result);
            assert_eq!(body["outcome"]["status"], "uncertain", "{body}");
            assert_eq!(
                body["outcome"]["retry"],
                outcome::inspect_before_retry(),
                "{body}"
            );
            assert_eq!(
                extract_error_kind(&result).as_deref(),
                Some("mutation_outcome_uncertain")
            );
            assert!(!text_of(&result).contains("nothing was written"));
        }
    }

    /// Built through Konnect's tools, re-serialised by `kicad-cli sch upgrade`
    /// (eeschema, version 20260306): three `R1` (1k at x=101.6, 2k at
    /// x=50.8, 3k at x=127.0) and two `C?` (100n at x=139.7, 1u at x=63.5).
    const BEFORE: &str = include_str!("../../tests/fixtures/annotate_duplicates.kicad_sch");
    /// The same file after eeschema 10.0.5 Tools → Annotate with its defaults
    /// (keep existing annotations, sort by X): `C1`/`C2` assigned, `R1`s
    /// untouched, two "Duplicate items R1" errors reported.
    const KICAD_KEEP: &str =
        include_str!("../../tests/fixtures/annotate_duplicates.kicad_keep_existing.kicad_sch");
    /// The same file after eeschema's "Reset existing annotations": the
    /// leftmost `R1` (2k) kept its number, 1k became `R2`, 3k `R3`.
    const KICAD_RESET: &str =
        include_str!("../../tests/fixtures/annotate_duplicates.kicad_reset.kicad_sch");
    /// eeschema-authored: U1 is three placed units of one dual triode.
    const MULTI_UNIT: &str = include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch");
    /// KiCad's `multichannel` demo child sheet, instantiated four times by
    /// its parent: every symbol carries four instance paths.
    const REUSED_SHEET: &str =
        include_str!("../../tests/fixtures/multichannel_channel_strip.kicad_sch");

    const UUID_1K: &str = "3e7f8c3e-69e6-41ab-a78f-765980213126";
    const UUID_2K: &str = "837d10e6-224a-4c0b-975a-139efaa126f3";
    const UUID_3K: &str = "e27496de-5f59-4cbf-a398-22e562bed8a2";
    const UUID_100N: &str = "4795413c-fead-4496-9988-5992855383e1";
    const UUID_1U: &str = "ac80bc97-8707-4117-b008-ef58f4ccc658";

    fn symbols_of(content: &str) -> Vec<PlacedSymbol> {
        placed_symbols(&parse_sexp(content).unwrap()).unwrap()
    }

    /// (uuid, property, [(path, reference)]) for every symbol — the shape a
    /// designator-only comparison needs, independent of serialisation.
    fn designators(content: &str) -> BTreeMap<String, (String, Vec<(String, String)>)> {
        symbols_of(content)
            .into_iter()
            .map(|s| {
                (
                    s.uuid,
                    (
                        s.property_reference,
                        s.paths.into_iter().map(|p| (p.path, p.reference)).collect(),
                    ),
                )
            })
            .collect()
    }

    fn write_fixture(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("annotate.kicad_sch");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    fn response_json(result: &CallToolResult) -> Value {
        match result.content.first() {
            Some(ToolContent::Text { text }) => serde_json::from_str(text).unwrap(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn designators_split_and_spell_the_way_eeschema_does() {
        assert_eq!(
            parse_designator("R?"),
            Designator {
                prefix: "R".into(),
                number: None
            }
        );
        assert_eq!(
            parse_designator("R12"),
            Designator {
                prefix: "R".into(),
                number: Some(12)
            }
        );
        assert_eq!(
            parse_designator("R"),
            Designator {
                prefix: "R".into(),
                number: None
            }
        );
        assert_eq!(
            parse_designator("#PWR?"),
            Designator {
                prefix: "#PWR".into(),
                number: None
            }
        );
        assert_eq!(
            parse_designator("#PWR01"),
            Designator {
                prefix: "#PWR".into(),
                number: Some(1)
            }
        );
        assert_eq!(
            parse_designator("#PWR0100"),
            Designator {
                prefix: "#PWR".into(),
                number: Some(100)
            }
        );
        assert_eq!(
            parse_designator("#PWR001"),
            Designator {
                prefix: "#PWR".into(),
                number: Some(1)
            }
        );
        assert_eq!(format_designator("R", 7), "R7");
        assert_eq!(format_designator("#PWR", 1), "#PWR01");
        assert_eq!(format_designator("#PWR", 10), "#PWR010");
        assert_eq!(format_designator("#PWR", 100), "#PWR0100");
        assert_eq!(format_designator("#FLG", 2), "#FLG02");
    }

    #[test]
    fn default_run_matches_eeschema_keep_existing_and_reports_the_duplicates() {
        let (_dir, path) = write_fixture(BEFORE);
        let result = annotate_file(&path, AnnotateOptions::default());
        let body = response_json(&result);
        assert!(!result.is_error, "{body}");

        // The two places eeschema wrote, and nothing else.
        assert_eq!(
            designators(&std::fs::read_to_string(&path).unwrap()),
            designators(KICAD_KEEP)
        );

        assert_eq!(body["written"], true);
        assert_eq!(body["unannotated_before"], 2);
        let assigned = body["assigned"].as_array().unwrap();
        assert_eq!(assigned.len(), 2, "{body}");
        assert_eq!(
            assigned[0]["uuid"], UUID_1U,
            "1u at x=63.5 is annotated first"
        );
        assert_eq!(assigned[0]["to"], "C1");
        assert_eq!(assigned[1]["uuid"], UUID_100N);
        assert_eq!(assigned[1]["to"], "C2");

        let unresolved = body["unresolved"].as_array().unwrap();
        assert_eq!(unresolved.len(), 1, "{body}");
        assert_eq!(unresolved[0]["reference"], "R1");
        assert_eq!(
            unresolved[0]["uuids"],
            json!([UUID_2K, UUID_1K, UUID_3K]),
            "ascending X: 2k, 1k, 3k"
        );
        assert_eq!(body["outcome"]["status"], "partial", "{body}");
        assert_eq!(body["outcome"]["completed"], 2);
        assert_eq!(body["outcome"]["failed"], 1);
        assert_eq!(body["outcome"]["retry"]["scope"], "unresolved_duplicates");
    }

    #[test]
    fn resolve_duplicates_matches_eeschema_reset_on_the_duplicate_groups() {
        let (_dir, path) = write_fixture(BEFORE);
        let result = annotate_file(
            &path,
            AnnotateOptions {
                resolve_duplicates: true,
                dry_run: false,
                project: None,
            },
        );
        let body = response_json(&result);
        assert!(!result.is_error, "{body}");
        assert_eq!(
            designators(&std::fs::read_to_string(&path).unwrap()),
            designators(KICAD_RESET)
        );

        let assigned = body["assigned"].as_array().unwrap();
        let by_uuid: BTreeMap<&str, &str> = assigned
            .iter()
            .map(|a| (a["uuid"].as_str().unwrap(), a["to"].as_str().unwrap()))
            .collect();
        assert_eq!(by_uuid.get(UUID_1K), Some(&"R2"), "{body}");
        assert_eq!(by_uuid.get(UUID_3K), Some(&"R3"), "{body}");
        assert!(
            !by_uuid.contains_key(UUID_2K),
            "the leftmost R1 keeps its number"
        );
        assert_eq!(body["outcome"]["status"], "complete", "{body}");
        assert!(body["unresolved"].as_array().unwrap().is_empty());
        assert!(body["outcome"]["retry"].is_null());
    }

    #[test]
    fn the_property_is_written_alongside_the_instances_block() {
        // Defect 2 of #454: the old annotator rewrote only (instances …).
        let (_dir, path) = write_fixture(BEFORE);
        annotate_file(&path, AnnotateOptions::default());
        let after = designators(&std::fs::read_to_string(&path).unwrap());
        for uuid in [UUID_1U, UUID_100N] {
            let (property, paths) = &after[uuid];
            assert_eq!(
                property, &paths[0].1,
                "{uuid}: property and instances disagree"
            );
            assert!(property.starts_with('C') && !property.ends_with('?'));
        }
    }

    #[test]
    fn a_multi_unit_part_is_not_a_duplicate() {
        let symbols = symbols_of(MULTI_UNIT);
        let plan = plan_annotation(&symbols, &sole_project(&symbols), true);
        let u1_units = symbols
            .iter()
            .filter(|s| s.property_reference == "U1")
            .count();
        assert!(u1_units >= 2, "the fixture places several units of U1");
        assert!(plan.duplicates_before.is_empty(), "{plan:?}");
        assert!(plan.assignments.is_empty(), "{plan:?}");

        let (_dir, path) = write_fixture(MULTI_UNIT);
        let before = std::fs::read(&path).unwrap();
        let body = response_json(&annotate_file(&path, AnnotateOptions::default()));
        assert_eq!(body["written"], false, "{body}");
        assert_eq!(body["outcome"]["status"], "complete", "{body}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "an annotated file is left alone"
        );
    }

    #[test]
    fn a_reused_sheet_is_numbered_per_instance_path() {
        let symbols = symbols_of(REUSED_SHEET);
        assert!(
            symbols.iter().all(|s| s.paths.len() == 4),
            "four instances per symbol"
        );
        let plan = plan_annotation(&symbols, &sole_project(&symbols), false);
        assert_eq!(plan.paths.len(), 4);
        assert!(
            plan.duplicates_before.is_empty() && plan.assignments.is_empty(),
            "{plan:?}"
        );

        // Derived input, stated rather than hidden: on ONE instance path only,
        // the second symbol takes the first symbol's designator.
        let first = &symbols[0];
        let second = &symbols[1];
        let path0 = first.paths[0].path.clone();
        let clash = first.paths[0].reference.clone();
        let (start, end) = symbol_block(REUSED_SHEET, &second.uuid).unwrap();
        let block = &REUSED_SHEET[start..end];
        let path_needle = format!("(path \"{path0}\"");
        let rel = block.find(&path_needle).unwrap();
        let path_block = find_enclosing_block(REUSED_SHEET, "path", start + rel + 1).unwrap();
        let edit = replace_quoted_after(REUSED_SHEET, path_block, "(reference \"", &clash, "test")
            .unwrap();
        let derived = apply_edits(REUSED_SHEET.to_string(), vec![edit]);

        let plan = plan_annotation(
            &symbols_of(&derived),
            &sole_project(&symbols_of(&derived)),
            true,
        );
        assert_eq!(plan.duplicates_before.len(), 1, "{plan:?}");
        assert_eq!(plan.duplicates_before[0].paths, vec![path0.clone()]);
        assert_eq!(
            plan.assignments.len(),
            1,
            "only the clashing instance is renumbered"
        );
        assert_eq!(plan.assignments[0].path, path0);

        let (_dir, path) = write_fixture(&derived);
        let body = response_json(&annotate_file(
            &path,
            AnnotateOptions {
                resolve_duplicates: true,
                dry_run: false,
                project: None,
            },
        ));
        assert_eq!(body["outcome"]["status"], "complete", "{body}");
        let after = designators(&std::fs::read_to_string(&path).unwrap());
        let before = designators(REUSED_SHEET);
        for (uuid, (_, paths_before)) in &before {
            for (p, reference) in paths_before {
                if !(uuid == &second.uuid && p == &path0) {
                    let (_, paths_after) = &after[uuid];
                    let got = &paths_after.iter().find(|(q, _)| q == p).unwrap().1;
                    assert_eq!(got, reference, "{uuid} on {p} must be untouched");
                }
            }
        }
    }

    #[test]
    fn the_first_free_number_fills_a_gap_as_eeschema_documents() {
        // Derived from BEFORE: 1k becomes R3 in both places, 3k becomes R?.
        let mut derived = BEFORE.to_string();
        for (uuid, from, to) in [(UUID_1K, "R1", "R3"), (UUID_3K, "R1", "R?")] {
            let block = symbol_block(&derived, uuid).unwrap();
            let edits = vec![
                replace_quoted_after(&derived, block, "(property \"Reference\" \"", to, "t")
                    .unwrap(),
                replace_quoted_after(&derived, block, "(reference \"", to, "t").unwrap(),
            ];
            assert_eq!(&derived[edits[0].start..edits[0].end], from);
            derived = apply_edits(derived, edits);
        }
        let plan = plan_annotation(
            &symbols_of(&derived),
            &sole_project(&symbols_of(&derived)),
            false,
        );
        let r = plan
            .assignments
            .iter()
            .find(|a| a.uuid == UUID_3K)
            .expect("3k is assigned");
        assert_eq!(
            r.to, "R2",
            "R1 and R3 are taken, so the first free number is 2: {plan:?}"
        );
    }

    #[test]
    fn a_designator_shared_by_different_symbols_with_distinct_units_is_never_guessed() {
        // Derived from BEFORE: 2k becomes a Device:C unit 2, 3k a unit 3. The
        // R1 group now looks like a multi-unit part across two libraries.
        let mut derived = BEFORE.to_string();
        let (s, e) = symbol_block(&derived, UUID_2K).unwrap();
        let block = derived[s..e]
            .replacen("(lib_id \"Device:R\")", "(lib_id \"Device:C\")", 1)
            .replacen("(unit 1)", "(unit 2)", 1);
        derived.replace_range(s..e, &block);
        let (s, e) = symbol_block(&derived, UUID_3K).unwrap();
        let block = derived[s..e].replacen("(unit 1)", "(unit 3)", 1);
        derived.replace_range(s..e, &block);

        let plan = plan_annotation(
            &symbols_of(&derived),
            &sole_project(&symbols_of(&derived)),
            true,
        );
        assert_eq!(plan.unresolved.len(), 1, "{plan:?}");
        assert_eq!(plan.unresolved[0].reason, REASON_AMBIGUOUS);
        assert!(
            plan.assignments.iter().all(|a| a.from == "C?"),
            "only the C? symbols are assigned: {plan:?}"
        );
    }

    #[test]
    fn a_dry_run_writes_nothing_and_reports_the_same_plan() {
        let (_dir, path) = write_fixture(BEFORE);
        let before = std::fs::read(&path).unwrap();
        let body = response_json(&annotate_file(
            &path,
            AnnotateOptions {
                resolve_duplicates: true,
                dry_run: true,
                project: None,
            },
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(body["dry_run"], true);
        assert_eq!(body["written"], false);
        assert_eq!(body["assigned"].as_array().unwrap().len(), 4, "{body}");
        assert_eq!(body["outcome"]["source"], "dry_run_plan");
        assert_eq!(body["outcome"]["status"], "complete");
    }

    #[test]
    fn a_missing_schematic_is_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = annotate_file(
            &dir.path().join("absent.kicad_sch"),
            AnnotateOptions::default(),
        );
        assert!(result.is_error);
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("file_not_found")
        );
    }

    #[test]
    fn a_plan_that_does_not_survive_preflight_writes_nothing() {
        // A plan naming a symbol the file does not have cannot be realised;
        // the refusal must leave the file byte-identical.
        let (_dir, path) = write_fixture(BEFORE);
        let before = std::fs::read(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let symbols = symbols_of(&content);
        let mut plan = plan_annotation(&symbols, &sole_project(&symbols), false);
        plan.assignments.push(Assignment {
            uuid: "00000000-0000-0000-0000-00000000dead".into(),
            unit: 1,
            project: "adj".into(),
            path: "/nowhere".into(),
            from: "X?".into(),
            to: "X1".into(),
        });
        let error = plan_edits(&content, &symbols, &plan).unwrap_err();
        assert!(error.contains("unknown symbol"), "{error}");
        let result = refused(&path, &plan, &AnnotateOptions::default(), error);
        assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let body = response_json(&result);
        assert_eq!(body["outcome"]["status"], "failed");
        assert_eq!(body["outcome"]["completed"], 0);
    }

    fn sole_project(symbols: &[PlacedSymbol]) -> String {
        let named: BTreeSet<&str> = symbols
            .iter()
            .flat_map(|s| s.paths.iter().map(|p| p.project.as_str()))
            .collect();
        assert_eq!(named.len(), 1, "fixture names one project: {named:?}");
        named.into_iter().next().unwrap().to_string()
    }

    /// The Reference property and every `(reference "…")` inside the symbol
    /// block carrying `uuid`, set to `reference`.
    fn set_every_reference(content: &str, uuid: &str, reference: &str) -> String {
        let (start, end) = symbol_block(content, uuid).unwrap();
        let mut block = String::new();
        let mut rest = &content[start..end];
        while let Some(i) = rest.find("(reference \"") {
            let value_start = i + "(reference \"".len();
            let value_len = rest[value_start..].find('"').unwrap();
            block.push_str(&rest[..value_start]);
            block.push_str(reference);
            rest = &rest[value_start + value_len..];
        }
        block.push_str(rest);
        let needle = "(property \"Reference\" \"";
        let i = block.find(needle).unwrap() + needle.len();
        let len = block[i..].find('"').unwrap();
        block.replace_range(i..i + len, reference);
        let mut derived = content.to_string();
        derived.replace_range(start..end, &block);
        derived
    }

    fn text_of(result: &CallToolResult) -> String {
        match result.content.first() {
            Some(ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn unannotated_units_of_one_package_share_one_reference() {
        let symbols = symbols_of(MULTI_UNIT);
        let units: Vec<&PlacedSymbol> = symbols
            .iter()
            .filter(|s| s.property_reference == "U1")
            .collect();
        assert_eq!(units.len(), 3, "the ecc83 places three units of U1");
        assert_eq!(
            units[0].unit_count,
            Some(3),
            "the embedded ECC83 definition declares three units"
        );
        let mut derived = MULTI_UNIT.to_string();
        for unit in &units {
            derived = set_every_reference(&derived, &unit.uuid, "U?");
        }
        let derived_symbols = symbols_of(&derived);
        let plan = plan_annotation(&derived_symbols, &sole_project(&derived_symbols), false);
        assert_eq!(plan.unannotated_before, 3, "{plan:?}");
        let u: Vec<&Assignment> = plan.assignments.iter().filter(|a| a.from == "U?").collect();
        assert_eq!(u.len(), 3, "{plan:?}");
        assert!(
            u.iter().all(|a| a.to == "U1"),
            "one package takes one number: {plan:?}"
        );
        assert!(plan.unresolved.is_empty(), "{plan:?}");

        let (_dir, path) = write_fixture(&derived);
        let body = response_json(&annotate_file(&path, AnnotateOptions::default()));
        assert_eq!(body["outcome"]["status"], "complete", "{body}");
        assert_eq!(body["written"], true, "{body}");
        let after = symbols_of(&std::fs::read_to_string(&path).unwrap());
        let mut placed_units: Vec<u32> = Vec::new();
        for unit in &units {
            let observed = after.iter().find(|s| s.uuid == unit.uuid).unwrap();
            assert_eq!(observed.property_reference, "U1", "{}", unit.uuid);
            assert_eq!(observed.paths.len(), 1);
            assert_eq!(observed.paths[0].reference, "U1");
            placed_units.push(observed.unit);
        }
        placed_units.sort_unstable();
        assert_eq!(placed_units, vec![1, 2, 3], "the package keeps its units");
    }

    #[test]
    fn unannotated_units_forming_two_packages_are_not_guessed() {
        let symbols = symbols_of(MULTI_UNIT);
        let units: Vec<String> = symbols
            .iter()
            .filter(|s| s.property_reference == "U1")
            .map(|s| s.uuid.clone())
            .collect();
        let mut derived = MULTI_UNIT.to_string();
        for uuid in &units {
            derived = set_every_reference(&derived, uuid, "U?");
        }
        // The third unit becomes a second unit 1: two packages' worth of units.
        let third = symbols
            .iter()
            .find(|s| s.property_reference == "U1" && s.unit == 3)
            .unwrap();
        let (s, e) = symbol_block(&derived, &third.uuid).unwrap();
        let block = derived[s..e].replacen("(unit 3)", "(unit 1)", 1);
        derived.replace_range(s..e, &block);

        let derived_symbols = symbols_of(&derived);
        let plan = plan_annotation(&derived_symbols, &sole_project(&derived_symbols), false);
        assert!(
            plan.assignments.iter().all(|a| a.from != "U?"),
            "no unit is numbered: {plan:?}"
        );
        let unresolved = plan
            .unresolved
            .iter()
            .find(|u| u.reference == "U?")
            .expect("the units are reported");
        assert_eq!(unresolved.reason, REASON_UNPROVEN_NEW_PACKAGE);
        assert_eq!(unresolved.uuids.len(), 3);

        let (_dir, path) = write_fixture(&derived);
        let before = std::fs::read(&path).unwrap();
        let body = response_json(&annotate_file(&path, AnnotateOptions::default()));
        assert_eq!(body["outcome"]["status"], "partial", "{body}");
        assert_eq!(body["written"], false, "{body}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing is guessed");
    }

    #[test]
    fn a_reused_sheet_reserves_numbers_project_wide() {
        let symbols = symbols_of(REUSED_SHEET);
        let resistor = symbols
            .iter()
            .find(|s| s.property_reference.starts_with('R'))
            .unwrap();
        assert_eq!(resistor.paths.len(), 4, "four instances of the sheet");
        let derived = set_every_reference(REUSED_SHEET, &resistor.uuid, "R?");
        let derived_symbols = symbols_of(&derived);
        let project = sole_project(&derived_symbols);
        let taken: BTreeSet<String> = derived_symbols
            .iter()
            .filter(|s| s.uuid != resistor.uuid)
            .flat_map(|s| s.paths.iter().map(|p| p.reference.clone()))
            .collect();

        let plan = plan_annotation(&derived_symbols, &project, false);
        let own: Vec<&Assignment> = plan
            .assignments
            .iter()
            .filter(|a| a.uuid == resistor.uuid)
            .collect();
        assert_eq!(own.len(), 4, "one assignment per instance path: {plan:?}");
        let numbers: BTreeSet<&str> = own.iter().map(|a| a.to.as_str()).collect();
        assert_eq!(numbers.len(), 4, "four distinct numbers: {plan:?}");
        assert!(
            numbers.iter().all(|n| !taken.contains(*n)),
            "a number used on any instance is never reused: {plan:?}"
        );
        assert_eq!(plan.assignments.len(), 4, "{plan:?}");
        assert!(plan.unresolved.is_empty(), "{plan:?}");

        let (_dir, path) = write_fixture(&derived);
        let body = response_json(&annotate_file(&path, AnnotateOptions::default()));
        assert_eq!(body["outcome"]["status"], "complete", "{body}");
        assert_eq!(body["project"], project, "{body}");
        let after = symbols_of(&std::fs::read_to_string(&path).unwrap());
        let all_r: Vec<String> = after
            .iter()
            .filter(|s| s.property_reference.starts_with('R'))
            .flat_map(|s| s.paths.iter().map(|p| p.reference.clone()))
            .collect();
        let distinct: BTreeSet<&String> = all_r.iter().collect();
        let resistors = after
            .iter()
            .filter(|s| s.property_reference.starts_with('R'))
            .count();
        assert_eq!(
            all_r.len(),
            4 * resistors,
            "every resistor on four instances"
        );
        assert_eq!(
            distinct.len(),
            all_r.len(),
            "every resistor instance keeps a unique reference: {all_r:?}"
        );
    }

    #[test]
    fn separate_projects_sharing_a_path_are_not_duplicates() {
        // 2k's instance record moves to another project on the same sheet
        // path: it is no longer one of adj's R1s, and adj's run leaves it
        // byte-identical.
        let mut derived = BEFORE.to_string();
        let (s, e) = symbol_block(&derived, UUID_2K).unwrap();
        let block = derived[s..e].replacen("(project \"adj\"", "(project \"other\"", 1);
        derived.replace_range(s..e, &block);
        let (s, e) = symbol_block(&derived, UUID_2K).unwrap();
        let other_block = derived[s..e].to_string();

        let symbols = symbols_of(&derived);
        let plan = plan_annotation(&symbols, "adj", false);
        assert_eq!(plan.duplicates_before.len(), 1, "{plan:?}");
        assert_eq!(
            plan.duplicates_before[0].uuids.len(),
            2,
            "1k and 3k only: {plan:?}"
        );
        assert!(!plan.duplicates_before[0]
            .uuids
            .contains(&UUID_2K.to_string()));
        assert_eq!(plan.outside_project.len(), 1, "{plan:?}");
        assert_eq!(plan.outside_project[0].uuid, UUID_2K);
        assert_eq!(plan.outside_project[0].projects, vec!["other".to_string()]);
        let other = plan_annotation(&symbols, "other", true);
        assert!(
            other.duplicates_before.is_empty() && other.assignments.is_empty(),
            "{other:?}"
        );

        let (_dir, path) = write_fixture(&derived);
        let body = response_json(&annotate_file(
            &path,
            AnnotateOptions {
                resolve_duplicates: true,
                dry_run: false,
                project: Some("adj".into()),
            },
        ));
        assert_eq!(body["outcome"]["status"], "complete", "{body}");
        assert_eq!(body["project"], "adj", "{body}");
        let after = std::fs::read_to_string(&path).unwrap();
        let (s, e) = symbol_block(&after, UUID_2K).unwrap();
        assert_eq!(
            &after[s..e],
            other_block,
            "the other project's record is byte-identical"
        );
    }

    #[test]
    fn edits_address_the_selected_projects_instance_entry() {
        // 1k carries records for two projects on the same sheet path, the
        // other project's first. adj's entry is unannotated; only it is written.
        let mut derived = BEFORE.to_string();
        let (s, e) = symbol_block(&derived, UUID_1K).unwrap();
        let block = &derived[s..e];
        let project_start = block.find("(project \"adj\"").unwrap();
        let project_block = find_enclosing_block(block, "project", project_start + 1).unwrap();
        let adj_record = block[project_block.0..project_block.1].to_string();
        let other_record = adj_record
            .replacen("(project \"adj\"", "(project \"other\"", 1)
            .replacen("(reference \"R1\")", "(reference \"R9\")", 1);
        let adj_unannotated = adj_record.replacen("(reference \"R1\")", "(reference \"R?\")", 1);
        let mut new_block = block.to_string();
        new_block.replace_range(
            project_block.0..project_block.1,
            &format!("{other_record}{adj_unannotated}"),
        );
        let needle = "(property \"Reference\" \"R1\"";
        let at = new_block.find(needle).unwrap() + "(property \"Reference\" \"".len();
        new_block.replace_range(at..at + 2, "R?");
        derived.replace_range(s..e, &new_block);

        let symbols = symbols_of(&derived);
        let one_k = symbols.iter().find(|s| s.uuid == UUID_1K).unwrap();
        assert_eq!(one_k.paths.len(), 2);
        assert_eq!(one_k.paths[0].project, "other");
        let plan = plan_annotation(&symbols, "adj", false);
        let a = plan
            .assignments
            .iter()
            .find(|a| a.uuid == UUID_1K)
            .expect("adj's entry is assigned");
        assert_eq!(
            (a.project.as_str(), a.from.as_str(), a.to.as_str()),
            ("adj", "R?", "R2")
        );

        let (_dir, path) = write_fixture(&derived);
        let body = response_json(&annotate_file(
            &path,
            AnnotateOptions {
                resolve_duplicates: false,
                dry_run: false,
                project: Some("adj".into()),
            },
        ));
        assert_eq!(body["written"], true, "{body}");
        let after = symbols_of(&std::fs::read_to_string(&path).unwrap());
        let one_k = after.iter().find(|s| s.uuid == UUID_1K).unwrap();
        let by_project: BTreeMap<&str, &str> = one_k
            .paths
            .iter()
            .map(|p| (p.project.as_str(), p.reference.as_str()))
            .collect();
        assert_eq!(by_project.get("adj"), Some(&"R2"));
        assert_eq!(
            by_project.get("other"),
            Some(&"R9"),
            "the other project's entry on the same path is untouched"
        );
        assert_eq!(one_k.property_reference, "R2");
    }

    #[test]
    fn resolve_duplicates_never_splits_a_multi_unit_package() {
        let symbols = symbols_of(MULTI_UNIT);
        let third = symbols
            .iter()
            .find(|s| s.property_reference == "U1" && s.unit == 3)
            .unwrap();
        let mut derived = MULTI_UNIT.to_string();
        let (s, e) = symbol_block(&derived, &third.uuid).unwrap();
        let block = derived[s..e].replacen("(unit 3)", "(unit 1)", 1);
        derived.replace_range(s..e, &block);

        let derived_symbols = symbols_of(&derived);
        let plan = plan_annotation(&derived_symbols, &sole_project(&derived_symbols), true);
        let group = plan
            .unresolved
            .iter()
            .find(|u| u.reference == "U1")
            .expect("U1 is reported");
        assert_eq!(group.reason, REASON_UNPROVEN_PACKAGE);
        assert_eq!(group.uuids.len(), 3);
        assert!(
            plan.assignments.iter().all(|a| a.from != "U1"),
            "nothing of U1 is renumbered: {plan:?}"
        );

        let (_dir, path) = write_fixture(&derived);
        let before = std::fs::read(&path).unwrap();
        let body = response_json(&annotate_file(
            &path,
            AnnotateOptions {
                resolve_duplicates: true,
                dry_run: false,
                project: None,
            },
        ));
        assert_eq!(body["outcome"]["status"], "partial", "{body}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn project_selection_is_refused_when_ambiguous_and_honoured_when_named() {
        let mut derived = BEFORE.to_string();
        let (s, e) = symbol_block(&derived, UUID_2K).unwrap();
        let block = derived[s..e].replacen("(project \"adj\"", "(project \"other\"", 1);
        derived.replace_range(s..e, &block);
        let (_dir, path) = write_fixture(&derived);
        let before = std::fs::read(&path).unwrap();

        let result = annotate_file(&path, AnnotateOptions::default());
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("invalid_argument"),
            "{result:?}"
        );
        let text = text_of(&result);
        assert!(
            text.contains("adj") && text.contains("other"),
            "the candidates are named: {text}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let wrong = annotate_file(
            &path,
            AnnotateOptions {
                resolve_duplicates: false,
                dry_run: true,
                project: Some("nope".into()),
            },
        );
        assert_eq!(
            extract_error_kind(&wrong).as_deref(),
            Some("invalid_argument")
        );

        let named = response_json(&annotate_file(
            &path,
            AnnotateOptions {
                resolve_duplicates: false,
                dry_run: true,
                project: Some("other".into()),
            },
        ));
        assert_eq!(named["project"], "other", "{named}");
        assert_eq!(
            named["outside_project"].as_array().map(Vec::len),
            Some(4),
            "1k, 3k and the two C? belong to adj only: {named}"
        );
    }

    #[test]
    fn numbers_used_on_the_projects_other_sheets_are_reserved() {
        // The multichannel demo's root sheet owns R1..R3; the child sheet,
        // annotated on its own, must not hand those numbers out.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("multichannel_mixer.kicad_pro"),
            include_str!("../../tests/fixtures/multichannel_mixer.kicad_pro"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("multichannel_mixer.kicad_sch"),
            include_str!("../../tests/fixtures/multichannel_mixer.kicad_sch"),
        )
        .unwrap();
        let symbols = symbols_of(REUSED_SHEET);
        let resistor = symbols
            .iter()
            .find(|s| s.property_reference.starts_with('R'))
            .unwrap();
        let derived = set_every_reference(REUSED_SHEET, &resistor.uuid, "R?");
        let child = dir.path().join("channel_strip.kicad_sch");
        std::fs::write(&child, &derived).unwrap();

        let root_numbers: BTreeSet<String> = symbols_of(include_str!(
            "../../tests/fixtures/multichannel_mixer.kicad_sch"
        ))
        .iter()
        .flat_map(|s| s.paths.iter().map(|p| p.reference.clone()))
        .filter(|r| r.starts_with('R'))
        .collect();
        assert!(
            root_numbers.contains("R1"),
            "the root sheet owns R1: {root_numbers:?}"
        );
        let (reserved, consulted, unreadable) = reserved_on_other_sheets(
            &dir.path().join("multichannel_mixer.kicad_sch"),
            &child,
            "multichannel_mixer",
        );
        assert_eq!(consulted.len(), 1, "the root only: {consulted:?}");
        assert!(unreadable.is_empty(), "{unreadable:?}");
        assert!(reserved["R"].contains(&1) && reserved["R"].contains(&3));

        let body = response_json(&annotate_file(&child, AnnotateOptions::default()));
        assert_eq!(body["outcome"]["status"], "complete", "{body}");
        assert_eq!(body["project"], "multichannel_mixer", "{body}");
        assert_eq!(
            body["other_sheets_consulted"].as_array().map(Vec::len),
            Some(1),
            "{body}"
        );
        let assigned: Vec<String> = body["assigned"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["to"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(assigned.len(), 4, "{body}");
        assert!(
            assigned.iter().all(|r| !root_numbers.contains(r)),
            "no number the root owns is handed out: {assigned:?}"
        );
    }
}
