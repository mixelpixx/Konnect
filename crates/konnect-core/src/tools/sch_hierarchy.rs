//! `sch_hierarchy` toolset — sheet object lifecycle (PR-A) plus sheet pin
//! lifecycle (PR-B): add, edit, move, delete, duplicate a sheet; recursive
//! hierarchy/page-numbering queries; import/add/edit/delete sheet pins and a
//! read-only pin/label sync check.
//!
//! Every handler here is file-editing only — KiCAD's own IPC API has no
//! schematic-editing commands upstream (`schematic_commands.proto` is empty),
//! so there's no dual IPC/file path to maintain, unlike the PCB toolsets.

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{
    get_path, invalid_arg, opt_f64, opt_str, project_name_for, require_f64, require_str,
    ToolContext, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::schematic::{format_hierarchical_sheet, HierarchicalSheetSpec};
use konnect_sexp::{
    commit_command, commit_file_transaction, parse_sexp, prepare_command, read_consistent,
    FileTransition, ItemAnchor, ItemId, SchematicCommand,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_hierarchical_sheet",
            "Insert a hierarchical sheet into a parent schematic, linking it to a child \
             .kicad_sch file. Creates the child file (blank) if it doesn't exist yet, or \
             links to it as-is if it does — reusing an existing file places the *same* \
             sub-circuit at a second location (KiCAD's multi-instance sheet pattern) rather \
             than duplicating it. If the linked file already has symbols in it, their \
             hierarchical instance paths are patched immediately so ERC resolves them.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to the parent .kicad_sch file" },
                    "sheet_file": { "type": "string", "description": "Filename of the child .kicad_sch, resolved relative to the parent's directory" },
                    "sheet_name": { "type": "string", "description": "Display name (Sheetname property). Default: 'Sheet'" },
                    "x": { "type": "number", "description": "Top-left X in mm. Default: 50" },
                    "y": { "type": "number", "description": "Top-left Y in mm. Default: 50" },
                    "width": { "type": "number", "description": "Sheet box width in mm. Default: 80" },
                    "height": { "type": "number", "description": "Sheet box height in mm. Default: 50" },
                    "project_name": { "type": "string", "description": "Project name key for the page-number instance entry. Default: the schematic file's stem (matching eeschema)" }
                },
                "required": ["schematic", "sheet_file"]
            }),
            |args, ctx| async move { handle_add_hierarchical_sheet(args, ctx).await }
        ),
        tool!(
            "edit_sheet",
            "Rename, resize, reposition, or repoint (Sheetfile) an existing sheet. Provide \
             at least one of: new_name, new_file, or both x+y, or both width+height. Does \
             NOT rename the child file on disk when new_file is given — it only repoints \
             the reference; the file itself must already exist at that path.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string", "description": "Current Sheetname to look up" },
                    "new_name": { "type": "string" },
                    "new_file": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "width": { "type": "number" }, "height": { "type": "number" },
                    "project_name": { "type": "string", "description": PROJECT_NAME_DESC }
                },
                "required": ["schematic", "sheet_name"]
            }),
            |args, ctx| async move { handle_edit_sheet(args, ctx).await }
        ),
        tool!(
            "move_sheet",
            "Reposition a sheet on the parent canvas without touching any other field.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "sheet_name", "x", "y"]
            }),
            |args, ctx| async move { handle_move_sheet(args, ctx).await }
        ),
        tool!(
            "delete_sheet",
            "Remove a sheet reference from the parent schematic. Does NOT delete the child \
             .kicad_sch file on disk. Remaining sheets' page numbers may now have a gap — \
             call renumber_sheet_pages afterward if that matters.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" }
                },
                "required": ["schematic", "sheet_name"]
            }),
            |args, ctx| async move { handle_delete_sheet(args, ctx).await }
        ),
        tool!(
            "duplicate_sheet",
            "Copy an existing sheet and its child .kicad_sch file under a new name/file, \
             offset slightly so the new sheet box doesn't overlap the source. The copy gets \
             its own internal schematic UUID and its symbols' hierarchical instance paths \
             are patched for the new sheet — it is a fully independent sub-circuit, not a \
             live-linked reuse (for that, use add_hierarchical_sheet pointed at the existing file).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "source_sheet_name": { "type": "string" },
                    "new_sheet_name": { "type": "string" },
                    "new_file": { "type": "string", "description": "Filename for the copy, resolved relative to the parent's directory. Must not already exist." },
                    "project_name": { "type": "string", "description": PROJECT_NAME_DESC }
                },
                "required": ["schematic", "source_sheet_name", "new_sheet_name", "new_file"]
            }),
            |args, ctx| async move { handle_duplicate_sheet(args, ctx).await }
        ),
        tool!(
            "get_sheet_hierarchy",
            "Recursively walk the sheet tree starting from a schematic file, returning \
             nested JSON: each sheet's name/file/uuid/position/size/page/pins plus its own \
             children. Handles missing child files and reference cycles gracefully instead \
             of failing.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Root schematic to start from" },
                    "project_name": { "type": "string", "description": PROJECT_NAME_DESC }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_sheet_hierarchy(args, ctx).await }
        ),
        tool!(
            "renumber_sheet_pages",
            "Walk the whole sheet tree from a root schematic and reassign sequential page \
             numbers (2, 3, 4, ... — page 1 is always the root and is left untouched) in \
             depth-first order. Fixes gaps left by delete_sheet/duplicate_sheet. Only \
             touches files whose page numbers actually changed.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Root schematic to start from" },
                    "project_name": { "type": "string", "description": PROJECT_NAME_DESC }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_renumber_sheet_pages(args, ctx).await }
        ),
        tool!(
            "import_sheet_pins",
            "Scan the child sheet's hierarchical_labels and auto-generate matching pins on \
             the parent sheet block, skipping names that already have a pin. This is the \
             primary, expected way sheet pins get created — mirrors KiCAD's own 'Import Sheet \
             Pins' command rather than pairing every pin to a label by hand. New pins are \
             placed along one edge of the sheet box, stacked below any existing pins.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to the parent .kicad_sch file" },
                    "sheet_name": { "type": "string" },
                    "side": { "type": "string", "enum": ["right", "left", "top", "bottom"], "description": "Which edge to place new pins on; new pins stack down a left/right edge and along a top/bottom one, below the pins already on that edge. Sets each pin's rotation, which is what KiCad reads the edge from. An import that would not fit on the named edge is refused entire and writes nothing, because KiCad clamps a pin placed past a corner back onto the box. Default: 'right'" }
                },
                "required": ["schematic", "sheet_name"]
            }),
            |args, ctx| async move { handle_import_sheet_pins(args, ctx).await }
        ),
        tool!(
            "add_sheet_pin",
            "Manually add a single pin to an existing sheet block. Prefer import_sheet_pins \
             for the common case; use this when a hierarchical_label hasn't been written yet \
             or a pin needs to exist ahead of the label. Pass 'side' to put the pin on the \
             top or bottom edge; without it the pin is written on the right edge as before.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" },
                    "pin_name": { "type": "string" },
                    "pin_type": { "type": "string", "enum": ALLOWED_PIN_TYPES },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "side": { "type": "string", "enum": ["right", "left", "top", "bottom"], "description": SHEET_PIN_SIDE_DESC }
                },
                "required": ["schematic", "sheet_name", "pin_name", "pin_type", "x", "y"]
            }),
            |args, ctx| async move { handle_add_sheet_pin(args, ctx).await }
        ),
        tool!(
            "edit_sheet_pin",
            "Rename a sheet pin, change its electrical type, move it, or move it to a \
             different edge of the sheet box. Provide at least one of: new_name, pin_type, \
             side, or both x+y.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" },
                    "pin_name": { "type": "string", "description": "Current pin name to look up" },
                    "new_name": { "type": "string" },
                    "pin_type": { "type": "string", "enum": ALLOWED_PIN_TYPES },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "side": { "type": "string", "enum": ["right", "left", "top", "bottom"], "description": SHEET_PIN_SIDE_DESC }
                },
                "required": ["schematic", "sheet_name", "pin_name"]
            }),
            |args, ctx| async move { handle_edit_sheet_pin(args, ctx).await }
        ),
        tool!(
            "delete_sheet_pin",
            "Remove a single pin from a sheet without touching the rest of the sheet.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" },
                    "pin_name": { "type": "string" }
                },
                "required": ["schematic", "sheet_name", "pin_name"]
            }),
            |args, ctx| async move { handle_delete_sheet_pin(args, ctx).await }
        ),
        tool!(
            "validate_sheet_pins",
            "Read-only. Walk the whole sheet tree from a root schematic and report \
             hierarchical_labels with no matching parent sheet pin, and sheet pins with no \
             matching child hierarchical_label. Does not modify anything — use as a pre-ERC \
             sanity check or to catch drift after manual edits.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Root schematic to start from" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_sheet_pins(args, ctx).await }
        ),
    ]
}

// ─── Shared helpers ─────────────────────────────────────────────────────────

pub(crate) const MAX_HIERARCHY_DEPTH: usize = 20;
const ALLOWED_PIN_TYPES: &[&str] = &["input", "output", "bidirectional", "tri_state", "passive"];
const SHEET_PIN_SPACING_MM: f64 = 2.54;
const SHEET_PIN_SIDE_DESC: &str =
    "Which edge of the sheet box the pin sits on. This is what KiCad reads the pin's \
     orientation from (right 0°, top 90°, left 180°, bottom 270°); it is not \
     inferred from the position. When given, the position must already be on that edge or \
     the call is refused, because KiCad would otherwise move the pin to the named edge on \
     load. Default: 'right', which is what these tools have always written.";

const PROJECT_NAME_DESC: &str =
    "Project name key for instance entries. Default: the schematic file's stem (matching eeschema)";

fn validate_pin_type(pin_type: &str) -> Result<(), CallToolResult> {
    if ALLOWED_PIN_TYPES.contains(&pin_type) {
        Ok(())
    } else {
        Err(CallToolResult::error(format!(
            "Invalid pin_type '{}' — must be one of: {}",
            pin_type,
            ALLOWED_PIN_TYPES.join(", ")
        )))
    }
}

/// The four sheet-box edges a pin may sit on, in the order the schema lists them.
const SHEET_PIN_SIDES: &[&str] = &["right", "left", "top", "bottom"];

/// One nanometre in millimetres — KiCad's own schematic resolution, and so the
/// most a pin position may differ from an edge before it is a different point.
const SHEET_PIN_EDGE_TOLERANCE_MM: f64 = 1e-6;

/// The rotation KiCad reads as "this pin is on that edge".
///
/// KiCad does not derive a sheet pin's edge from its position: the rotation in
/// the pin's `at` selects the edge, and a pin whose position sits on a
/// different one is *relocated* on load. So the rotation is the side, and
/// writing the wrong one produces a file that stops describing what the editor
/// shows.
fn rotation_for_sheet_pin_side(side: &str) -> Option<f64> {
    match side {
        "right" => Some(0.0),
        "top" => Some(90.0),
        "left" => Some(180.0),
        "bottom" => Some(270.0),
        _ => None,
    }
}

/// Inverse of [`rotation_for_sheet_pin_side`], so a response can name the side
/// the written pin actually carries instead of echoing the requested one.
/// `None` for a rotation KiCad does not map to an edge.
fn sheet_pin_side_for_rotation(rotation: Option<f64>) -> Option<&'static str> {
    let rotation = rotation?;
    SHEET_PIN_SIDES
        .iter()
        .copied()
        .find(|side| rotation_for_sheet_pin_side(side) == Some(rotation))
}

/// Accept a `side` argument, returning the rotation it selects.
fn sheet_pin_rotation_for(side: &str) -> Result<f64, CallToolResult> {
    rotation_for_sheet_pin_side(side).ok_or_else(|| {
        invalid_arg(
            "side",
            &format!(
                "'{}' is not a sheet edge — must be one of: {}",
                side,
                SHEET_PIN_SIDES.join(", ")
            ),
        )
    })
}

/// A sheet block's box: top-left corner plus size, in schematic millimetres.
#[derive(Clone, Copy)]
struct SheetBox {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl SheetBox {
    fn of(sheet: &cse::Sheet) -> Self {
        let (x, y) = sheet.position();
        SheetBox {
            x,
            y,
            width: sheet.width,
            height: sheet.height,
        }
    }

    /// The edge `side` names, or `None` for a name that is not one.
    fn edge(&self, side: &str) -> Option<SheetEdge> {
        let (axis, coordinate) = match side {
            "right" => ("x", self.x + self.width),
            "left" => ("x", self.x),
            "top" => ("y", self.y),
            "bottom" => ("y", self.y + self.height),
            _ => return None,
        };
        let (span_axis, span_start, span_end) = if axis == "x" {
            ("y", self.y, self.y + self.height)
        } else {
            ("x", self.x, self.x + self.width)
        };
        Some(SheetEdge {
            axis,
            coordinate,
            span_axis,
            span_start,
            span_end,
        })
    }
}

/// One edge of a sheet box: the axis a pin on it is pinned to and the
/// coordinate it must hold there, plus how far the edge runs along the other
/// axis. KiCad constrains a pin on both — a position past a corner is clamped
/// to that corner, corners themselves included.
struct SheetEdge {
    axis: &'static str,
    coordinate: f64,
    span_axis: &'static str,
    span_start: f64,
    span_end: f64,
}

/// Refuse a position that is not on the edge the caller named.
///
/// KiCad would accept the write and then move the pin to the edge the rotation
/// selects, leaving the tool reporting a position the editor does not use. A
/// refusal at Konnect's boundary is the only point at which the caller still
/// learns that the request cannot be carried out.
fn ensure_pin_is_on_sheet_edge(
    sheet_box: SheetBox,
    sheet_name: &str,
    side: &str,
    x: f64,
    y: f64,
) -> Result<(), CallToolResult> {
    let Some(edge) = sheet_box.edge(side) else {
        return Ok(());
    };
    let coordinate_on = |axis: &str| if axis == "x" { x } else { y };

    let axis = edge.axis;
    let actual = coordinate_on(axis);
    let expected = edge.coordinate;
    if (actual - expected).abs() > SHEET_PIN_EDGE_TOLERANCE_MM {
        return Err(invalid_arg(
            axis,
            &format!(
                "{axis} = {actual} is not on the '{side}' edge of sheet '{sheet_name}', which is \
                 {axis} = {expected}. KiCad relocates a sheet pin whose position and side \
                 disagree, so the pin would not stay where this call puts it"
            ),
        ));
    }

    // On the right line but past the end of it: KiCad pulls the pin back to the
    // corner, which is the same failure one axis over.
    let span_axis = edge.span_axis;
    let along = coordinate_on(span_axis);
    let (start, end) = (edge.span_start, edge.span_end);
    if along < start - SHEET_PIN_EDGE_TOLERANCE_MM || along > end + SHEET_PIN_EDGE_TOLERANCE_MM {
        return Err(invalid_arg(
            span_axis,
            &format!(
                "{span_axis} = {along} is past the end of the '{side}' edge of sheet \
                 '{sheet_name}', which runs from {span_axis} = {start} to {end}. KiCad clamps a \
                 sheet pin to the nearest corner rather than leaving it off the box, so the pin \
                 would not stay where this call puts it"
            ),
        ));
    }
    Ok(())
}

/// Refuse a whole import that would run an edge past its corner.
///
/// `import_sheet_pins` derives every position itself, so the caller cannot see
/// the overflow in its own arguments the way `add_sheet_pin`'s caller can. The
/// message therefore has to say how many pins the edge holds and how many are
/// already on it, and that nothing was written — the import is refused entire.
fn sheet_pin_edge_is_full(
    sheet_box: SheetBox,
    sheet_name: &str,
    side: &str,
    occupied: usize,
    first_that_did_not_fit: &str,
) -> CallToolResult {
    let capacity = sheet_box
        .edge(side)
        .map(|edge| ((edge.span_end - edge.span_start) / SHEET_PIN_SPACING_MM).floor() as usize)
        .unwrap_or(0);
    CallToolResult::error(format!(
        "The '{side}' edge of sheet '{sheet_name}' holds {capacity} sheet pins at \
         {SHEET_PIN_SPACING_MM} mm apart and already carries {occupied}, so '{first_that_did_not_fit}' \
         and everything after it would be placed past a corner. KiCad clamps such a pin back onto \
         the box, piling the overflow on the corner instead of leaving it where it was written, \
         so the import is refused entire and nothing was written. Enlarge the sheet with \
         edit_sheet, or import onto more than one side."
    ))
}

fn parent_dir(sch_path: &Path) -> PathBuf {
    sch_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
fn create_blank_schematic(path: &Path) -> anyhow::Result<()> {
    let template = crate::tools::blank_schematic_template();
    konnect_sexp::writer::write_new_atomic(path, &template)?;
    // Round-trip through cse so the file is normalised to its writer's format,
    // matching the existing `create_schematic` tool's behavior.
    let sch = cse::Schematic::load(path)?;
    sch.overwrite()?;
    Ok(())
}

fn next_free_page(parent: &cse::Schematic, project_name: &str) -> u32 {
    let mut max_page: u32 = 1; // page 1 is always the root sheet
    for sheet in parent.sheets.iter() {
        if let Some(p) = sheet.page(project_name) {
            if let Ok(n) = p.parse::<u32>() {
                max_page = max_page.max(n);
            }
        }
    }
    max_page + 1
}

fn sheet_json(sheet: &cse::Sheet, project_name: &str) -> Value {
    let (x, y) = sheet.position();
    json!({
        "name": sheet.name(),
        "file": sheet.file(),
        "uuid": sheet.uuid,
        "x": x,
        "y": y,
        "width": sheet.width,
        "height": sheet.height,
        "page": sheet.page(project_name),
        "pins": sheet.pins.iter().map(|p| {
            let (px, py) = p.position();
            json!({ "name": p.name, "pin_type": p.pin_type, "x": px, "y": py })
        }).collect::<Vec<_>>()
    })
}

fn ensure_source_root_uuid(source: &str) -> anyhow::Result<(String, String)> {
    let tree = parse_sexp(source)?;
    if let Some(uuid) = tree.find_str("uuid") {
        return Ok((source.to_owned(), uuid.to_owned()));
    }
    let uuid = konnect_sexp::writer::new_uuid();
    let children = konnect_sexp::writer::find_direct_child_blocks(source, "kicad_sch");
    let anchor = children
        .iter()
        .find_map(|(start, end)| {
            let node = parse_sexp(&source[*start..*end]).ok()?;
            (!matches!(
                node.head(),
                Some("version" | "generator" | "generator_version")
            ))
            .then_some(*start)
        })
        .ok_or_else(|| anyhow::anyhow!("parent schematic has no UUID insertion anchor"))?;
    let line_start = source[..anchor]
        .rfind('\n')
        .map_or(anchor, |newline| newline + 1);
    let indent = &source[line_start..anchor];
    if !indent.chars().all(char::is_whitespace) {
        anyhow::bail!("parent schematic metadata is not line-oriented");
    }
    let replacement = format!("{indent}(uuid \"{uuid}\")\n");
    let updated = konnect_sexp::writer::apply_edits(
        source.to_owned(),
        vec![konnect_sexp::writer::SexpEdit::insert(
            line_start,
            replacement,
        )],
    );
    Ok((updated, uuid))
}

/// Give every item in a duplicated document its own UUID.
///
/// `duplicate_sheet` rewrote only the root `(uuid ...)`. Every nested item —
/// text, symbols, wires, labels, sheet pins — arrived in the copy still
/// carrying the source's UUID, so two sheets claimed the same identities and
/// anything resolving by UUID picks one of them arbitrarily.
///
/// Replacements are applied per quoted string rather than by substring, and a
/// string is remapped segment by segment, so an instance `(path "/a/b")` that
/// names a renamed item follows it instead of dangling. Matching whole segments
/// also keeps short non-UUID identifiers, which fixtures and project names use,
/// from being rewritten where they merely occur inside another word.
fn regenerate_item_uuids(source: &str) -> String {
    const DECLARATION: &str = "(uuid \"";

    let mut mapping: HashMap<&str, String> = HashMap::new();
    let mut rest = source;
    while let Some(at) = rest.find(DECLARATION) {
        let body = &rest[at + DECLARATION.len()..];
        let Some(end) = body.find('"') else { break };
        let declared = &body[..end];
        if !declared.is_empty() {
            mapping
                .entry(declared)
                .or_insert_with(|| uuid::Uuid::new_v4().to_string());
        }
        rest = &body[end..];
    }
    if mapping.is_empty() {
        return source.to_owned();
    }

    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(at) = rest.find('"') {
        out.push_str(&rest[..=at]);
        let body = &rest[at + 1..];
        let mut end = None;
        let mut escaped = false;
        for (index, ch) in body.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => {
                    end = Some(index);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            rest = body;
            break;
        };
        out.push_str(&remap_uuid_string(&body[..end], &mapping));
        out.push('"');
        rest = &body[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Remap a quoted string: either the whole value, or each `/`-separated segment
/// of an instance path.
fn remap_uuid_string(value: &str, mapping: &HashMap<&str, String>) -> String {
    if let Some(replacement) = mapping.get(value) {
        return replacement.clone();
    }
    if !value.contains('/') {
        return value.to_owned();
    }
    value
        .split('/')
        .map(|segment| {
            mapping
                .get(segment)
                .map_or(segment, |replacement| replacement.as_str())
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn replace_source_root_uuid(source: &str, uuid: &str) -> anyhow::Result<String> {
    let children = konnect_sexp::writer::find_direct_child_blocks(source, "kicad_sch");
    let range = children.iter().find_map(|(start, end)| {
        parse_sexp(&source[*start..*end])
            .ok()
            .is_some_and(|node| node.head() == Some("uuid"))
            .then_some((*start, *end))
    });
    if let Some((start, end)) = range {
        return Ok(konnect_sexp::writer::apply_edits(
            source.to_owned(),
            vec![konnect_sexp::writer::SexpEdit::replace(
                start,
                end,
                format!("(uuid \"{uuid}\")"),
            )],
        ));
    }
    let (with_uuid, generated) = ensure_source_root_uuid(source)?;
    replace_source_root_uuid(&with_uuid, uuid)
        .or_else(|_| anyhow::bail!("could not replace newly inserted schematic UUID {generated}"))
}

#[derive(Debug)]
enum SheetMutationIntent {
    Present {
        uuid: String,
        expected: cse::sexp::SexpNode,
    },
    Absent {
        uuid: String,
    },
}

fn verify_sheet_mutation(
    schematic: &cse::Schematic,
    intent: &SheetMutationIntent,
    phase: &str,
) -> Result<(), String> {
    let uuid = match intent {
        SheetMutationIntent::Present { uuid, .. } | SheetMutationIntent::Absent { uuid } => uuid,
    };
    let matching: Vec<_> = schematic
        .sheets
        .iter()
        .filter(|sheet| sheet.uuid == *uuid)
        .collect();
    match intent {
        SheetMutationIntent::Present { expected, .. } => {
            if matching.len() != 1 {
                return Err(format!(
                    "{phase} result contains {} sheet items with UUID {uuid}; expected exactly one",
                    matching.len()
                ));
            }
            if &matching[0].to_sexp() != expected {
                return Err(format!(
                    "{phase} sheet UUID {uuid} differs from the edited intent"
                ));
            }
        }
        SheetMutationIntent::Absent { .. } if !matching.is_empty() => {
            return Err(format!(
                "{phase} result still contains {} sheet items with deleted UUID {uuid}",
                matching.len()
            ));
        }
        SheetMutationIntent::Absent { .. } => {}
    }
    Ok(())
}

fn sheet_precommit_refusal(path: &Path, reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        ToolErrorKind::StaleTarget {
            target: path.display().to_string(),
            reason: reason.clone(),
        },
        format!("Prospective hierarchy validation failed; nothing was written. {reason}"),
    )
}

#[cfg(test)]
enum HierarchyProspectiveFault {
    Replace { from: String, to: String },
    RestoreDeletedSheet(String),
}

#[cfg(test)]
thread_local! {
    /// One-shot corruption of a prospective hierarchy result. Tests use it to
    /// reproduce the wrong-bytes writer failure through a real handler.
    static HIERARCHY_PROSPECTIVE_FAULT: std::cell::RefCell<Option<HierarchyProspectiveFault>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn apply_hierarchy_prospective_fault(source: String) -> String {
    HIERARCHY_PROSPECTIVE_FAULT.with(|fault| {
        let Some(fault) = fault.borrow_mut().take() else {
            return source;
        };
        match fault {
            HierarchyProspectiveFault::Replace { from, to } => {
                assert!(
                    source.contains(&from),
                    "prospective hierarchy fault anchor was not present: {from}"
                );
                source.replacen(&from, &to, 1)
            }
            HierarchyProspectiveFault::RestoreDeletedSheet(sheet) => {
                let closing = source
                    .rfind("\n)")
                    .expect("prospective schematic has a root closing line");
                let mut corrupted = source;
                corrupted.insert_str(closing, &format!("\n{sheet}"));
                corrupted
            }
        }
    })
}

fn commit_verified_sheet_mutation(
    path: &Path,
    before: &str,
    command: &SchematicCommand,
    intent: &SheetMutationIntent,
    operation: &str,
) -> anyhow::Result<Option<CallToolResult>> {
    let (prospective_source, _) = prepare_command(path, before, command)?;
    #[cfg(test)]
    let prospective_source = apply_hierarchy_prospective_fault(prospective_source);
    let prospective = match cse::Schematic::from_source(path, prospective_source) {
        Ok(schematic) => schematic,
        Err(error) => {
            return Ok(Some(sheet_precommit_refusal(
                path,
                format!("prospective schematic is not readable: {error}"),
            )))
        }
    };
    if let Err(reason) = verify_sheet_mutation(&prospective, intent, "prospective") {
        return Ok(Some(sheet_precommit_refusal(path, reason)));
    }

    commit_command(path, command)?;
    let committed = match cse::Schematic::load(path) {
        Ok(schematic) => schematic,
        Err(error) => {
            return Ok(Some(super::mutation_outcome_uncertain(
                path,
                operation,
                format!("The committed schematic could not be loaded: {error}"),
            )))
        }
    };
    if let Err(reason) = verify_sheet_mutation(&committed, intent, "post-commit") {
        return Ok(Some(super::mutation_outcome_uncertain(
            path, operation, reason,
        )));
    }
    Ok(None)
}

/// Commit one edited sheet item after validating the exact prospective text,
/// then independently verify the saved document.
fn commit_edited_sheet_item(
    path: &Path,
    before: &str,
    edited: &cse::Schematic,
    uuid: &str,
    label: &str,
) -> anyhow::Result<Option<CallToolResult>> {
    let command = SchematicCommand::replace_item_from_document(
        before,
        &edited.to_source(),
        ItemId::new(uuid)?,
        label,
    )?;
    let intended = edited
        .sheets
        .by_uuid(uuid)
        .ok_or_else(|| anyhow::anyhow!("edited {label} result has no sheet UUID {uuid}"))?;
    let intent = SheetMutationIntent::Present {
        uuid: uuid.to_owned(),
        expected: intended.to_sexp(),
    };
    commit_verified_sheet_mutation(path, before, &command, &intent, label)
}

/// Read one sheet pin back off the file a write has just committed, so a
/// response can describe the pin that was *saved* rather than the request that
/// produced it.
///
/// The two are not interchangeable. The schematic writer holds six decimal
/// places, so a finer position reaches the file rounded and a response that
/// restated its arguments would name a position no reader will ever see. More
/// to the point, `side` exists precisely because a sheet pin does not always
/// end up where the caller put it — a response that echoes the request cannot
/// report that, which would leave the argument unable to prevent the failure it
/// was added for.
fn read_back_sheet_pin(
    path: &Path,
    sheet_name: &str,
    pin_name: &str,
) -> anyhow::Result<cse::SheetPin> {
    let saved = cse::Schematic::load(path)?;
    let sheet = saved.sheets.by_name(sheet_name).ok_or_else(|| {
        anyhow::anyhow!("sheet '{sheet_name}' is not in the schematic that was just written")
    })?;
    sheet.pin_by_name(pin_name).cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "pin '{pin_name}' is not on sheet '{sheet_name}' in the schematic that was just \
             written"
        )
    })
}

// ─── Handlers ───────────────────────────────────────────────────────────────

async fn handle_add_hierarchical_sheet(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let parent_path = get_path(args, "schematic")?;
    let sheet_file = match require_str(args, "sheet_file") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let sheet_name = opt_str(args, "sheet_name").unwrap_or("Sheet").to_string();
    let x = opt_f64(args, "x").unwrap_or(50.0);
    let y = opt_f64(args, "y").unwrap_or(50.0);
    let width = opt_f64(args, "width").unwrap_or(80.0);
    let height = opt_f64(args, "height").unwrap_or(50.0);
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&parent_path));

    let dir = parent_dir(&parent_path);
    let child_path = dir.join(&sheet_file);

    let relative = Path::new(&sheet_file);
    let valid_relative = !relative.is_absolute()
        && relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && relative
            .extension()
            .is_some_and(|extension| extension == "kicad_sch");
    if !valid_relative {
        return Ok(CallToolResult::error(
            "sheet_file must be a relative .kicad_sch path without parent traversal",
        ));
    }
    if !child_path.parent().is_some_and(Path::is_dir) {
        return Ok(CallToolResult::error(
            "The child sheet directory does not exist",
        ));
    }
    if child_path == parent_path {
        return Ok(CallToolResult::error(
            "A hierarchical sheet cannot reference its parent file",
        ));
    }

    let parent_before = read_consistent(&parent_path)?;
    let parent = cse::Schematic::load(&parent_path)?;

    if parent.sheets.by_name(&sheet_name).is_some() {
        return Ok(CallToolResult::error(format!(
            "Sheet named '{}' already exists in this schematic — use edit_sheet to modify it \
             or pick a different name",
            sheet_name
        )));
    }

    let child_existed = child_path.is_file();
    if child_path.exists() && !child_existed {
        return Ok(CallToolResult::error(
            "The child schematic path exists but is not a regular file",
        ));
    }
    let page = next_free_page(&parent, &project_name).to_string();
    let (parent_base, root_uuid) = ensure_source_root_uuid(&parent_before)?;
    let root_path = format!("/{root_uuid}");
    let block = format_hierarchical_sheet(HierarchicalSheetSpec {
        name: &sheet_name,
        file: &sheet_file,
        x,
        y,
        width,
        height,
        project_name: &project_name,
        parent_instance_path: &root_path,
        page: &page,
    });
    let parent_command = SchematicCommand::insert_item(
        &parent_base,
        block,
        ItemAnchor::BeforeFooter,
        "Add hierarchical sheet",
    )?
    .requiring_unchanged_document();
    let sheet_uuid = parent_command
        .changes
        .first()
        .map(|change| change.id.to_string())
        .ok_or_else(|| anyhow::anyhow!("sheet insertion produced no item change"))?;
    let (parent_after, _) = prepare_command(&parent_path, &parent_base, &parent_command)?;

    let child_before = child_path
        .is_file()
        .then(|| read_consistent(&child_path))
        .transpose()?;
    let mut transitions = vec![FileTransition::replace(
        &parent_path,
        parent_before,
        parent_after,
    )];
    let mut patched = 0usize;
    if let Some(child_before) = child_before {
        let hierarchy_path = format!("{root_path}/{sheet_uuid}");
        if let Some(child_command) = SchematicCommand::ensure_symbol_instance_path(
            &child_before,
            &project_name,
            &hierarchy_path,
            "Link hierarchical child symbols",
        )? {
            patched = child_command.changes.len();
            let (child_after, _) = prepare_command(&child_path, &child_before, &child_command)?;
            transitions.push(FileTransition::replace(
                &child_path,
                child_before,
                child_after,
            ));
        }
    } else {
        transitions.push(FileTransition::create(
            &child_path,
            konnect_sexp::schematic::format_blank_schematic(),
        ));
    }
    commit_file_transaction(&dir, transitions)?;

    let committed = cse::Schematic::load(&parent_path)?;
    let sheet_ref = committed
        .sheets
        .by_name(&sheet_name)
        .ok_or_else(|| anyhow::anyhow!("committed sheet was not readable"))?;
    Ok(CallToolResult::json(&json!({
        "added": sheet_name,
        "sheet": sheet_json(sheet_ref, &project_name),
        "child_file": child_path.display().to_string(),
        "reused_existing_file": child_existed,
        "patched_symbol_instances": patched
    })))
}

async fn handle_edit_sheet(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&sch_path));

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    let sheet = match sch.sheets.by_name_mut(&sheet_name) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };
    let sheet_uuid = sheet.uuid.clone();

    // `requested` is what the caller asked to set, `changed` is what actually
    // differs. They diverge when a caller re-asserts the state that is already
    // there, which is a request the sheet can honor.
    let mut requested = Vec::new();
    let mut changed = Vec::new();
    if let Some(new_name) = opt_str(args, "new_name") {
        requested.push("name");
        if sheet.name() != new_name {
            sheet.set_name(new_name);
            changed.push("name");
        }
    }
    if let Some(new_file) = opt_str(args, "new_file") {
        requested.push("file");
        if sheet.file() != new_file {
            sheet.set_file(new_file);
            changed.push("file");
        }
    }
    if let (Some(x), Some(y)) = (opt_f64(args, "x"), opt_f64(args, "y")) {
        requested.push("position");
        if sheet.at.x != x || sheet.at.y != y {
            sheet.move_to(x, y);
            changed.push("position");
        }
    }
    if let (Some(w), Some(h)) = (opt_f64(args, "width"), opt_f64(args, "height")) {
        requested.push("size");
        if sheet.width != w || sheet.height != h {
            sheet.set_size(w, h);
            changed.push("size");
        }
    }

    if requested.is_empty() {
        return Ok(CallToolResult::error(
            "No fields to change — provide at least one of: new_name, new_file, x+y, width+height",
        ));
    }

    let summary = sheet_json(sheet, &project_name);
    // Skip the commit outright when nothing differs. Writing would reserialise
    // the whole sheet (#210) and produce a diff for a request that asked for
    // the state already on disk.
    if !changed.is_empty() {
        if let Some(error) =
            commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "edit_sheet")?
        {
            return Ok(error);
        }
    }
    Ok(CallToolResult::json(&json!({
        "edited": sheet_name,
        "changed": !changed.is_empty(),
        "changed_fields": changed,
        "requested_fields": requested,
        "sheet": summary
    })))
}

async fn handle_move_sheet(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    match sch.sheets.by_name_mut(&sheet_name) {
        Some(sheet) => {
            let sheet_uuid = sheet.uuid.clone();
            let changed = sheet.at.x != x || sheet.at.y != y;
            if changed {
                sheet.move_to(x, y);
                if let Some(error) =
                    commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "move_sheet")?
                {
                    return Ok(error);
                }
            }
            Ok(CallToolResult::json(
                &json!({ "moved": sheet_name, "x": x, "y": y, "changed": changed }),
            ))
        }
        None => Ok(CallToolResult::error(format!(
            "Sheet '{}' not found",
            sheet_name
        ))),
    }
}

async fn handle_delete_sheet(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let before = read_consistent(&sch_path)?;
    let sch = cse::Schematic::load(&sch_path)?;
    match sch.sheets.by_name(&sheet_name) {
        Some(removed) => {
            let child_file = removed.file().to_owned();
            let uuid = removed.uuid.clone();
            let command =
                SchematicCommand::delete_item(&before, ItemId::new(uuid.clone())?, "Delete sheet")?;
            let intent = SheetMutationIntent::Absent { uuid };
            if let Some(error) = commit_verified_sheet_mutation(
                &sch_path,
                &before,
                &command,
                &intent,
                "delete_sheet",
            )? {
                return Ok(error);
            }
            Ok(CallToolResult::json(&json!({
                "deleted": sheet_name,
                "child_file_preserved": child_file,
                "note": "The child schematic file was not deleted. Remaining sheets' page \
                         numbers may now have a gap — call renumber_sheet_pages if needed."
            })))
        }
        None => Ok(CallToolResult::error(format!(
            "Sheet '{}' not found",
            sheet_name
        ))),
    }
}

async fn handle_duplicate_sheet(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let source_name = match require_str(args, "source_sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_name = match require_str(args, "new_sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_file = match require_str(args, "new_file") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&sch_path));

    let parent_before = read_consistent(&sch_path)?;
    let parent = cse::Schematic::load(&sch_path)?;

    if parent.sheets.by_name(&new_name).is_some() {
        return Ok(CallToolResult::error(format!(
            "Sheet named '{}' already exists",
            new_name
        )));
    }

    let (src_x, src_y, src_w, src_h, src_file) = match parent.sheets.by_name(&source_name) {
        Some(s) => {
            let (x, y) = s.position();
            (x, y, s.width, s.height, s.file().to_string())
        }
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                source_name
            )))
        }
    };

    let dir = parent_dir(&sch_path);
    let source_child = dir.join(&src_file);
    let new_child = dir.join(&new_file);

    let relative = Path::new(&new_file);
    let valid_relative = !relative.is_absolute()
        && relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && relative
            .extension()
            .is_some_and(|extension| extension == "kicad_sch");
    if !valid_relative || !new_child.parent().is_some_and(Path::is_dir) {
        return Ok(CallToolResult::error(
            "new_file must be a relative .kicad_sch path in an existing project directory",
        ));
    }

    if new_child.exists() {
        return Ok(CallToolResult::error(format!(
            "'{}' already exists — pick a different file name, or use add_hierarchical_sheet \
             to link the existing file instead of duplicating",
            new_file
        )));
    }
    if !source_child.exists() {
        return Ok(CallToolResult::error(format!(
            "Source sheet's file '{}' was not found on disk — cannot duplicate",
            src_file
        )));
    }

    const DUPLICATE_OFFSET_MM: f64 = 20.0;
    let page = next_free_page(&parent, &project_name).to_string();
    let (parent_base, root_uuid) = ensure_source_root_uuid(&parent_before)?;
    let root_path = format!("/{root_uuid}");
    let block = format_hierarchical_sheet(HierarchicalSheetSpec {
        name: &new_name,
        file: &new_file,
        x: src_x + DUPLICATE_OFFSET_MM,
        y: src_y + DUPLICATE_OFFSET_MM,
        width: src_w,
        height: src_h,
        project_name: &project_name,
        parent_instance_path: &root_path,
        page: &page,
    });
    let parent_command = SchematicCommand::insert_item(
        &parent_base,
        block,
        ItemAnchor::BeforeFooter,
        "Duplicate hierarchical sheet",
    )?
    .requiring_unchanged_document();
    let sheet_uuid = parent_command
        .changes
        .first()
        .map(|change| change.id.to_string())
        .ok_or_else(|| anyhow::anyhow!("sheet duplication produced no item change"))?;
    let (parent_after, _) = prepare_command(&sch_path, &parent_base, &parent_command)?;

    let source_child_content = read_consistent(&source_child)?;
    // Fresh identities for the copy's own items before the root is renamed;
    // otherwise the duplicate shares every nested UUID with its source.
    let refreshed_child = regenerate_item_uuids(&source_child_content);
    let duplicated_uuid = uuid::Uuid::new_v4().to_string();
    let duplicated_base = replace_source_root_uuid(&refreshed_child, &duplicated_uuid)?;
    let hierarchy_path = format!("{root_path}/{sheet_uuid}");
    let (duplicated_after, patched) = if let Some(command) =
        SchematicCommand::ensure_symbol_instance_path(
            &duplicated_base,
            &project_name,
            &hierarchy_path,
            "Link duplicated child symbols",
        )? {
        let count = command.changes.len();
        let (after, _) = prepare_command(&new_child, &duplicated_base, &command)?;
        (after, count)
    } else {
        (duplicated_base, 0)
    };
    commit_file_transaction(
        &dir,
        vec![
            FileTransition::replace(&sch_path, parent_before, parent_after),
            FileTransition::create(&new_child, duplicated_after),
        ],
    )?;

    let committed = cse::Schematic::load(&sch_path)?;
    let sheet_ref = committed
        .sheets
        .by_name(&new_name)
        .ok_or_else(|| anyhow::anyhow!("duplicated sheet was not readable"))?;
    Ok(CallToolResult::json(&json!({
        "duplicated_from": source_name,
        "sheet": sheet_json(sheet_ref, &project_name),
        "child_file": new_child.display().to_string(),
        "patched_symbol_instances": patched
    })))
}

async fn handle_get_sheet_hierarchy(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root_path = get_path(args, "schematic")?;
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&root_path));

    if !root_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root_path.display()
        )));
    }

    let mut visited = HashSet::new();
    let tree = build_hierarchy_node(&root_path, &project_name, 0, &mut visited)?;
    Ok(CallToolResult::json(&tree))
}

pub(crate) fn build_hierarchy_node(
    path: &Path,
    project_name: &str,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
) -> anyhow::Result<Value> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    if depth > MAX_HIERARCHY_DEPTH {
        return Ok(json!({
            "file": path.display().to_string(),
            "error": "max hierarchy depth exceeded — possible reference cycle",
            "children": []
        }));
    }
    if !visited.insert(canon.clone()) {
        return Ok(json!({
            "file": path.display().to_string(),
            "error": "cycle detected — this file is already an ancestor in this tree",
            "children": []
        }));
    }

    let sch = match cse::Schematic::load(path) {
        Ok(s) => s,
        Err(e) => {
            visited.remove(&canon);
            return Ok(json!({
                "file": path.display().to_string(),
                "error": format!("failed to load: {}", e),
                "children": []
            }));
        }
    };

    let dir = parent_dir(path);
    let mut children = Vec::new();
    for sheet in sch.sheets.iter() {
        let child_path = dir.join(sheet.file());
        let mut node = sheet_json(sheet, project_name);
        if child_path.exists() {
            let sub = build_hierarchy_node(&child_path, project_name, depth + 1, visited)?;
            node["children"] = sub["children"].clone();
            if let Some(err) = sub.get("error") {
                node["error"] = err.clone();
            }
        } else {
            node["children"] = json!([]);
            node["error"] = json!("child file not found on disk");
        }
        children.push(node);
    }
    visited.remove(&canon);

    Ok(json!({
        "file": path.display().to_string(),
        "children": children
    }))
}

async fn handle_renumber_sheet_pages(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root_path = get_path(args, "schematic")?;
    let project_name = opt_str(args, "project_name")
        .map(str::to_string)
        .unwrap_or_else(|| project_name_for(&root_path));

    if !root_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root_path.display()
        )));
    }

    // Page paths are hierarchical instance paths rooted at the root sheet's
    // UUID ("/<root-uuid>", then "/<root-uuid>/<sheet-uuid>" one level down),
    // matching what eeschema writes.
    let root_before = read_consistent(&root_path)?;
    let (root_base, root_uuid) = ensure_source_root_uuid(&root_before)?;
    let root_prefix = format!("/{root_uuid}");

    let mut next_page = 2u32; // page 1 is always the root, left untouched
    let mut renumbered = Vec::new();
    let mut visited = HashSet::new();
    let mut transitions = Vec::new();
    collect_renumber_transitions(
        &root_path,
        &root_prefix,
        &project_name,
        &mut next_page,
        &mut renumbered,
        &mut visited,
        Some((&root_before, &root_base, &root_uuid)),
        &mut transitions,
    )?;
    if !transitions.is_empty() {
        commit_file_transaction(parent_dir(&root_path), transitions)?;
    }

    Ok(CallToolResult::json(&json!({
        "renumbered_count": renumbered.len(),
        "pages": renumbered
    })))
}

#[allow(clippy::too_many_arguments)]
fn collect_renumber_transitions(
    path: &Path,
    hier_prefix: &str,
    project_name: &str,
    next_page: &mut u32,
    renumbered: &mut Vec<Value>,
    visited: &mut HashSet<PathBuf>,
    source_override: Option<(&str, &str, &str)>,
    transitions: &mut Vec<FileTransition>,
) -> anyhow::Result<()> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canon.clone()) {
        return Ok(()); // cycle guard — already on this DFS path, skip
    }

    let loaded_before = source_override
        .map(|(before, _, _)| before.to_owned())
        .unwrap_or(read_consistent(path)?);
    let command_source = source_override
        .map(|(_, base, _)| base)
        .unwrap_or(loaded_before.as_str());
    let mut sch = cse::Schematic::load(path)?;
    if let Some((_, _, root_uuid)) = source_override {
        sch.uuid = Some(root_uuid.to_owned());
    }
    let dir = parent_dir(path);
    let mut changed_ids = Vec::new();

    // Snapshot the sheet order first: recursing below needs `sch` unborrowed.
    let sheet_order: Vec<(String, String, String)> = sch
        .sheets
        .iter()
        .map(|s| (s.name().to_string(), s.file().to_string(), s.uuid.clone()))
        .collect();

    for (name, file, sheet_uuid) in &sheet_order {
        let page = next_page.to_string();
        *next_page += 1;
        if let Some(sheet) = sch.sheets.by_name_mut(name) {
            if sheet.page(project_name) != Some(page.as_str()) {
                sheet.set_page(project_name, hier_prefix, &page);
                changed_ids.push(ItemId::new(sheet.uuid.clone())?);
            }
        }
        renumbered.push(json!({ "sheet_name": name, "file": file, "page": page }));

        let child_path = dir.join(file);
        if child_path.exists() {
            let child_prefix = format!("{}/{}", hier_prefix, sheet_uuid);
            collect_renumber_transitions(
                &child_path,
                &child_prefix,
                project_name,
                next_page,
                renumbered,
                visited,
                None,
                transitions,
            )?;
        }
    }

    let replacement = if changed_ids.is_empty() {
        command_source.to_owned()
    } else {
        let command = SchematicCommand::replace_items_from_document(
            command_source,
            &sch.to_source(),
            changed_ids,
            "Renumber hierarchical sheets",
        )?;
        prepare_command(path, command_source, &command)?.0
    };
    if replacement != loaded_before {
        transitions.push(FileTransition::replace(path, loaded_before, replacement));
    }
    visited.remove(&canon);
    Ok(())
}

async fn handle_import_sheet_pins(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let side = opt_str(args, "side").unwrap_or("right").to_string();
    let rotation = match sheet_pin_rotation_for(&side) {
        Ok(r) => r,
        Err(e) => return Ok(e),
    };

    let before = read_consistent(&sch_path)?;
    let mut parent = cse::Schematic::load(&sch_path)?;
    let dir = parent_dir(&sch_path);

    let (child_path, sheet_x, sheet_y, sheet_w, sheet_h) = match parent.sheets.by_name(&sheet_name)
    {
        Some(s) => {
            let (x, y) = s.position();
            (dir.join(s.file()), x, y, s.width, s.height)
        }
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };

    if !child_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Child file '{}' not found on disk — cannot read its hierarchical labels",
            child_path.display()
        )));
    }
    let child = cse::Schematic::load(&child_path)?;
    let label_names: Vec<(String, String)> = child
        .hierarchical_labels
        .iter()
        .map(|l| {
            (
                l.text.clone(),
                l.shape.clone().unwrap_or_else(|| "passive".to_string()),
            )
        })
        .collect();

    let sheet = parent
        .sheets
        .by_name_mut(&sheet_name)
        .expect("looked up above");
    let sheet_uuid = sheet.uuid.clone();
    let sheet_box = SheetBox::of(sheet);

    // A left/right edge runs down the box, so pins stack in y along it; a
    // top/bottom edge runs across, so they stack in x. Either way the pinned
    // coordinate is the edge's own, and the rotation says which edge that is.
    let stacks_in_y = side == "right" || side == "left";
    let edge_x = if side == "right" {
        sheet_x + sheet_w
    } else {
        sheet_x
    };
    let edge_y = if side == "bottom" {
        sheet_y + sheet_h
    } else {
        sheet_y
    };

    // The stack continues below the pins already on *this* edge. Counting every
    // pin on the sheet instead — which is what this did — starts the stack past
    // the other three edges' pins and walks it straight off the end of this one.
    // The rotation is what says which edge a pin is on, so it is what is counted.
    //
    // A pin whose rotation names no edge — absent, or an angle KiCad does not
    // map — is counted here too. The blunt total had that safety by accident,
    // and making the count precise would otherwise have taken it away: if we
    // cannot tell which edge a pin is on, it could be this one, and stacking
    // further out is always safe where stacking over it is not. A well-formed
    // file has no such pin, so this changes nothing for one.
    let occupied = sheet
        .pins
        .iter()
        .filter(|pin| match sheet_pin_side_for_rotation(pin.at.rotation) {
            Some(pin_side) => pin_side == side.as_str(),
            None => true,
        })
        .count();

    // Plan the whole import before any of it is written. These positions are
    // generated here rather than supplied, so an overflow is not something the
    // caller can see coming — and a loop that wrote as it went would already
    // have committed the pins before the one that did not fit. Every generated
    // point goes through the same edge guard `add_sheet_pin` uses.
    let mut planned: Vec<(String, String, f64, f64)> = Vec::new();
    let mut planned_names: HashSet<String> = HashSet::new();
    let mut skipped_existing = Vec::new();
    let mut slot = occupied;
    for (name, shape) in label_names {
        if sheet.pin_by_name(&name).is_some() || planned_names.contains(&name) {
            skipped_existing.push(name);
            continue;
        }
        let pin_type = if ALLOWED_PIN_TYPES.contains(&shape.as_str()) {
            shape
        } else {
            "passive".to_string()
        };
        slot += 1;
        let offset = SHEET_PIN_SPACING_MM * slot as f64;
        let (pin_x, pin_y) = if stacks_in_y {
            (edge_x, sheet_y + offset)
        } else {
            (sheet_x + offset, edge_y)
        };
        if ensure_pin_is_on_sheet_edge(sheet_box, &sheet_name, &side, pin_x, pin_y).is_err() {
            return Ok(sheet_pin_edge_is_full(
                sheet_box,
                &sheet_name,
                &side,
                occupied,
                &name,
            ));
        }
        planned_names.insert(name.clone());
        planned.push((name, pin_type, pin_x, pin_y));
    }

    let mut imported = Vec::new();
    for (name, pin_type, pin_x, pin_y) in planned {
        let mut pin = cse::SheetPin::new(name.as_str(), pin_type.as_str(), pin_x, pin_y);
        pin.at.rotation = Some(rotation);
        imported.push(pin.name.clone());
        sheet.add_pin(pin);
    }

    if !imported.is_empty() {
        if let Some(error) = commit_edited_sheet_item(
            &sch_path,
            &before,
            &parent,
            &sheet_uuid,
            "import_sheet_pins",
        )? {
            return Ok(error);
        }
    }

    // Read the side back off a pin that was actually saved, rather than echoing
    // the argument. An import that wrote nothing has no pin to read and reports
    // no side rather than asserting one.
    let written_side = match imported.first() {
        Some(name) => {
            let saved = read_back_sheet_pin(&sch_path, &sheet_name, name)?;
            sheet_pin_side_for_rotation(saved.at.rotation)
        }
        None => None,
    };

    Ok(CallToolResult::json(&json!({
        "sheet": sheet_name,
        "side": written_side,
        "imported_pins": imported,
        "skipped_existing": skipped_existing
    })))
}

async fn handle_add_sheet_pin(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_name = match require_str(args, "pin_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_type = match require_str(args, "pin_type") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    if let Err(e) = validate_pin_type(&pin_type) {
        return Ok(e);
    }
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    // Absent `side` keeps the behaviour every existing caller has: rotation 0,
    // and no opinion about where the position sits. The edge check below rides
    // on the argument, so nothing that used to be accepted starts being refused.
    let requested_side = opt_str(args, "side");
    let side = requested_side.unwrap_or("right").to_string();
    let rotation = match sheet_pin_rotation_for(&side) {
        Ok(r) => r,
        Err(e) => return Ok(e),
    };

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    let sheet = match sch.sheets.by_name_mut(&sheet_name) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };
    let sheet_uuid = sheet.uuid.clone();
    let sheet_box = SheetBox::of(sheet);

    if sheet.pin_by_name(&pin_name).is_some() {
        return Ok(CallToolResult::error(format!(
            "Sheet '{}' already has a pin named '{}'",
            sheet_name, pin_name
        )));
    }

    if requested_side.is_some() {
        if let Err(e) = ensure_pin_is_on_sheet_edge(sheet_box, &sheet_name, &side, x, y) {
            return Ok(e);
        }
    }

    let mut pin = cse::SheetPin::new(pin_name.as_str(), pin_type.as_str(), x, y);
    pin.at.rotation = Some(rotation);
    sheet.add_pin(pin);
    if let Some(error) =
        commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "add_sheet_pin")?
    {
        return Ok(error);
    }

    // Position and side come off the saved pin, not out of the arguments.
    let saved = read_back_sheet_pin(&sch_path, &sheet_name, &pin_name)?;
    Ok(CallToolResult::json(&json!({
        "added_pin": pin_name,
        "sheet": sheet_name,
        "pin_type": pin_type,
        "x": saved.at.x,
        "y": saved.at.y,
        "side": sheet_pin_side_for_rotation(saved.at.rotation)
    })))
}

async fn handle_edit_sheet_pin(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_name = match require_str(args, "pin_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    if let Some(pt) = opt_str(args, "pin_type") {
        if let Err(e) = validate_pin_type(pt) {
            return Ok(e);
        }
    }
    let requested_side = opt_str(args, "side").map(str::to_string);
    let requested_rotation = match requested_side.as_deref().map(sheet_pin_rotation_for) {
        Some(Ok(r)) => Some(r),
        Some(Err(e)) => return Ok(e),
        None => None,
    };

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    let sheet = match sch.sheets.by_name_mut(&sheet_name) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };
    let sheet_uuid = sheet.uuid.clone();
    let sheet_box = SheetBox::of(sheet);
    let pin = match sheet.pin_by_name_mut(&pin_name) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Pin '{}' not found on sheet '{}'",
                pin_name, sheet_name
            )))
        }
    };

    // `requested` is what the caller asked to set, `changed` what actually
    // differs — the same split `edit_sheet` makes, and for the same reason: a
    // caller that re-asserts the state already there has made a legal request
    // that changed nothing, and the response has to be able to say so.
    let mut requested = Vec::new();
    let mut changed = Vec::new();
    if let Some(new_name) = opt_str(args, "new_name") {
        requested.push("name");
        pin.name = new_name.to_string();
        changed.push("name");
    }
    if let Some(pt) = opt_str(args, "pin_type") {
        requested.push("pin_type");
        pin.pin_type = pt.to_string();
        changed.push("pin_type");
    }
    if let (Some(x), Some(y)) = (opt_f64(args, "x"), opt_f64(args, "y")) {
        requested.push("position");
        pin.at.x = x;
        pin.at.y = y;
        changed.push("position");
    }
    // Checked against where the pin ends up, so `side` and `x`+`y` in one call
    // are judged together rather than against the position being replaced. The
    // edge still has to be a legal one for the pin's position even when the pin
    // is already on it, so the guard runs before the delta is taken.
    if let (Some(side), Some(rotation)) = (requested_side.as_deref(), requested_rotation) {
        requested.push("side");
        if let Err(e) =
            ensure_pin_is_on_sheet_edge(sheet_box, &sheet_name, side, pin.at.x, pin.at.y)
        {
            return Ok(e);
        }
        if pin.at.rotation != Some(rotation) {
            pin.at.rotation = Some(rotation);
            changed.push("side");
        }
    }

    // `new_name` may have just renamed it, so the read-back below has to look
    // for the name the pin carries now.
    let saved_name = pin.name.clone();

    if requested.is_empty() {
        return Ok(CallToolResult::error(
            "No fields to change — provide at least one of: new_name, pin_type, side, x+y",
        ));
    }

    // Nothing differs, so nothing is written: a commit here would reserialise
    // the sheet (#210) for a request that asked for the state already on disk.
    if !changed.is_empty() {
        if let Some(error) =
            commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "edit_sheet_pin")?
        {
            return Ok(error);
        }
    }

    // Reported off the saved pin rather than the in-memory one, for the same
    // reason as `add_sheet_pin`: the response has to describe the file.
    let saved = read_back_sheet_pin(&sch_path, &sheet_name, &saved_name)?;
    let summary = json!({
        "name": saved.name,
        "pin_type": saved.pin_type,
        "x": saved.at.x,
        "y": saved.at.y,
        "side": sheet_pin_side_for_rotation(saved.at.rotation)
    });

    Ok(CallToolResult::json(&json!({
        "edited_pin": pin_name,
        "sheet": sheet_name,
        "changed": !changed.is_empty(),
        "changed_fields": changed,
        "requested_fields": requested,
        "pin": summary
    })))
}

async fn handle_delete_sheet_pin(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_name = match require_str(args, "pin_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let before = read_consistent(&sch_path)?;
    let mut sch = cse::Schematic::load(&sch_path)?;
    let sheet = match sch.sheets.by_name_mut(&sheet_name) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };
    let sheet_uuid = sheet.uuid.clone();

    if !sheet.remove_pin(&pin_name) {
        return Ok(CallToolResult::error(format!(
            "Pin '{}' not found on sheet '{}'",
            pin_name, sheet_name
        )));
    }
    if let Some(error) =
        commit_edited_sheet_item(&sch_path, &before, &sch, &sheet_uuid, "delete_sheet_pin")?
    {
        return Ok(error);
    }

    Ok(CallToolResult::json(&json!({
        "deleted_pin": pin_name,
        "sheet": sheet_name
    })))
}

async fn handle_validate_sheet_pins(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root_path = get_path(args, "schematic")?;

    if !root_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root_path.display()
        )));
    }

    let mut issues = Vec::new();
    let mut visited = HashSet::new();
    collect_pin_mismatches(&root_path, 0, &mut visited, &mut issues)?;

    Ok(CallToolResult::json(&json!({
        "issue_count": issues.len(),
        "issues": issues
    })))
}

fn collect_pin_mismatches(
    path: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    issues: &mut Vec<Value>,
) -> anyhow::Result<()> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if depth > MAX_HIERARCHY_DEPTH || !visited.insert(canon.clone()) {
        return Ok(());
    }

    let sch = match cse::Schematic::load(path) {
        Ok(s) => s,
        Err(_) => {
            visited.remove(&canon);
            return Ok(());
        }
    };
    let dir = parent_dir(path);

    for sheet in sch.sheets.iter() {
        let child_path = dir.join(sheet.file());
        if !child_path.exists() {
            issues.push(json!({
                "sheet": sheet.name(),
                "file": sheet.file(),
                "error": "child file not found on disk"
            }));
            continue;
        }
        let child = cse::Schematic::load(&child_path)?;
        let label_names: HashSet<String> = child
            .hierarchical_labels
            .iter()
            .map(|l| l.text.clone())
            .collect();
        let pin_names: HashSet<String> = sheet.pins.iter().map(|p| p.name.clone()).collect();

        let labels_without_pins: Vec<&String> = label_names.difference(&pin_names).collect();
        let pins_without_labels: Vec<&String> = pin_names.difference(&label_names).collect();

        if !labels_without_pins.is_empty() || !pins_without_labels.is_empty() {
            issues.push(json!({
                "sheet": sheet.name(),
                "file": sheet.file(),
                "labels_without_pins": labels_without_pins,
                "pins_without_labels": pins_without_labels
            }));
        }

        collect_pin_mismatches(&child_path, depth + 1, visited, issues)?;
    }
    visited.remove(&canon);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_ctx() -> ToolContext {
        let config = ServerConfig {
            kicad_cli: "kicad-cli".into(),
            kicad_binary: "kicad".into(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        };
        ToolContext::new(config, Arc::new(crate::router::ToolRouter::new()))
    }

    fn blank_schematic(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        create_blank_schematic(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn add_hierarchical_sheet_creates_child_file_and_links_it() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();

        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_file": "power.kicad_sch",
            "sheet_name": "Power Supply",
            "x": 20.0, "y": 20.0
        });
        let result = handle_add_hierarchical_sheet(&args, &ctx).await.unwrap();
        assert!(!result.is_error);

        assert!(tmp.path().join("power.kicad_sch").exists());
        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.len(), 1);
        assert_eq!(
            parent.sheets.by_name("Power Supply").unwrap().file(),
            "power.kicad_sch"
        );
        // Pages are stored under the default project name (the file stem) at
        // the parent's "/<root-uuid>" instance path.
        assert_eq!(
            parent.sheets.by_name("Power Supply").unwrap().page("root"),
            Some("2")
        );
    }

    fn result_json(result: &CallToolResult) -> Value {
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        serde_json::from_str(&text).unwrap()
    }

    async fn sheet_at(tmp: &TempDir, ctx: &ToolContext, x: f64, y: f64) -> PathBuf {
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_file": "power.kicad_sch",
            "sheet_name": "Power",
            "x": x, "y": y
        });
        handle_add_hierarchical_sheet(&args, ctx).await.unwrap();
        root
    }

    #[tokio::test]
    async fn edit_sheet_accepts_the_position_the_sheet_already_has() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;
        let before = std::fs::read_to_string(&root).unwrap();

        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "x": 20.0, "y": 20.0
        });
        let result = handle_edit_sheet(&args, &ctx).await.unwrap();

        assert!(!result.is_error, "an idempotent edit is not an error");
        let body = result_json(&result);
        assert_eq!(body["changed"], json!(false));
        assert_eq!(body["changed_fields"], json!([]));
        assert_eq!(body["requested_fields"], json!(["position"]));
        assert_eq!(
            std::fs::read_to_string(&root).unwrap(),
            before,
            "a no-op edit leaves the file alone"
        );
    }

    #[tokio::test]
    async fn edit_sheet_reports_only_the_fields_that_differ() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;

        // Position is restated, the name is genuinely new.
        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "new_name": "Power Supply",
            "x": 20.0, "y": 20.0
        });
        let result = handle_edit_sheet(&args, &ctx).await.unwrap();

        assert!(!result.is_error);
        let body = result_json(&result);
        assert_eq!(body["changed"], json!(true));
        assert_eq!(body["changed_fields"], json!(["name"]));
        assert_eq!(body["requested_fields"], json!(["name", "position"]));
    }

    #[tokio::test]
    async fn move_sheet_accepts_the_position_the_sheet_already_has() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;

        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "x": 20.0, "y": 20.0
        });
        let result = handle_move_sheet(&args, &ctx).await.unwrap();

        assert!(!result.is_error, "an idempotent move is not an error");
        assert_eq!(result_json(&result)["changed"], json!(false));
    }

    #[tokio::test]
    async fn edit_sheet_is_idempotent_on_a_sheet_konnect_already_wrote() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;
        let move_it = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "x": 30.0, "y": 30.0
        });

        // The first edit rewrites the sheet in Konnect's own serialisation, so
        // the block round-trips byte-for-byte from here on. That is the state
        // in which the reported error appeared.
        let first = handle_edit_sheet(&move_it, &ctx).await.unwrap();
        assert!(!first.is_error);
        assert_eq!(result_json(&first)["changed"], json!(true));
        let settled = std::fs::read_to_string(&root).unwrap();

        let second = handle_edit_sheet(&move_it, &ctx).await.unwrap();

        assert!(
            !second.is_error,
            "re-asserting the current position must not error"
        );
        assert_eq!(result_json(&second)["changed"], json!(false));
        assert_eq!(std::fs::read_to_string(&root).unwrap(), settled);
    }

    #[tokio::test]
    async fn edit_sheet_pin_accepts_the_position_the_pin_already_has() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;
        let add = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "pin_name": "VCC",
            "pin_type": "input",
            "x": 20.0, "y": 25.0
        });
        handle_add_sheet_pin(&add, &ctx).await.unwrap();

        let restate = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power",
            "pin_name": "VCC",
            "x": 20.0, "y": 25.0
        });
        handle_edit_sheet_pin(&restate, &ctx).await.unwrap();
        let result = handle_edit_sheet_pin(&restate, &ctx).await.unwrap();

        // This handler does no field pre-comparison; it reaches the command
        // layer with an identical block and relies on the no-op being legal.
        assert!(
            !result.is_error,
            "the relaxed command layer covers callers that do not pre-compare"
        );
    }

    #[tokio::test]
    async fn edit_sheet_still_rejects_a_call_with_no_fields() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_at(&tmp, &ctx, 20.0, 20.0).await;

        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_name": "Power"
        });
        let result = handle_edit_sheet(&args, &ctx).await.unwrap();

        assert!(result.is_error, "asking for nothing is still an error");
    }

    #[tokio::test]
    async fn add_hierarchical_sheet_rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();

        let args = json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" });
        handle_add_hierarchical_sheet(&args, &ctx).await.unwrap();

        let args2 = json!({ "schematic": root.display().to_string(), "sheet_file": "b.kicad_sch", "sheet_name": "A" });
        let result = handle_add_hierarchical_sheet(&args2, &ctx).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn second_sheet_gets_next_free_page() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();

        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "b.kicad_sch", "sheet_name": "B" }),
            &ctx,
        )
        .await
        .unwrap();

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.by_name("A").unwrap().page("root"), Some("2"));
        assert_eq!(parent.sheets.by_name("B").unwrap().page("root"), Some("3"));
    }

    #[tokio::test]
    async fn edit_sheet_renames_and_resizes() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "new_name": "Renamed", "width": 100.0, "height": 60.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert!(parent.sheets.by_name("A").is_none());
        let renamed = parent.sheets.by_name("Renamed").unwrap();
        assert_eq!(renamed.width, 100.0);
        assert_eq!(renamed.height, 60.0);
    }

    #[tokio::test]
    async fn edit_sheet_with_no_fields_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn move_sheet_updates_position_only() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A", "x": 10.0, "y": 10.0 }),
            &ctx,
        )
        .await
        .unwrap();

        handle_move_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "x": 99.0, "y": 88.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("A").unwrap();
        assert_eq!(sheet.position(), (99.0, 88.0));
    }

    #[tokio::test]
    async fn delete_sheet_removes_reference_but_keeps_child_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_delete_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert!(parent.sheets.is_empty());
        assert!(tmp.path().join("a.kicad_sch").exists());
    }

    #[tokio::test]
    async fn delete_sheet_not_found_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        let result = handle_delete_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Nope" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn duplicate_sheet_copies_file_independently() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "amp.kicad_sch", "sheet_name": "Amp1", "x": 10.0, "y": 10.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_duplicate_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "source_sheet_name": "Amp1",
                "new_sheet_name": "Amp2",
                "new_file": "amp2.kicad_sch"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        assert!(tmp.path().join("amp2.kicad_sch").exists());

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.len(), 2);
        let amp2 = parent.sheets.by_name("Amp2").unwrap();
        assert_eq!(amp2.file(), "amp2.kicad_sch");
        assert_eq!(amp2.position(), (30.0, 30.0)); // offset from source (10,10)

        // Independent files: the two schematics have different internal UUIDs.
        let sch1 = cse::Schematic::load(tmp.path().join("amp.kicad_sch")).unwrap();
        let sch2 = cse::Schematic::load(tmp.path().join("amp2.kicad_sch")).unwrap();
        assert_ne!(sch1.uuid, sch2.uuid);
    }

    fn declared_uuids(source: &str) -> HashSet<String> {
        const DECLARATION: &str = "(uuid \"";
        let mut found = HashSet::new();
        let mut rest = source;
        while let Some(at) = rest.find(DECLARATION) {
            let body = &rest[at + DECLARATION.len()..];
            let Some(end) = body.find('"') else { break };
            found.insert(body[..end].to_owned());
            rest = &body[end + 1..];
        }
        found
    }

    #[test]
    fn regenerating_uuids_replaces_declarations_and_the_paths_that_name_them() {
        let source = r#"(kicad_sch
  (symbol (lib_id "Device:R") (uuid "sym-a")
    (instances (project "demo" (path "/root-a/sym-a" (reference "R1"))))
  )
  (text "see sym-a in the notes" (uuid "text-a"))
  (sheet_instances (path "/root-a" (page "2")))
)
"#;

        let out = regenerate_item_uuids(source);

        let before = declared_uuids(source);
        let after = declared_uuids(&out);
        assert_eq!(before.len(), 2, "fixture declares two UUIDs");
        assert_eq!(after.len(), 2, "the copy declares two UUIDs");
        assert!(
            before.is_disjoint(&after),
            "every declaration must change: {before:?} vs {after:?}"
        );

        // The instance path naming the renamed symbol follows it.
        let new_symbol = out
            .split_once("(uuid \"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(id, _)| id.to_owned())
            .expect("symbol uuid present");
        assert!(
            out.contains(&format!("(path \"/root-a/{new_symbol}\"")),
            "the path must follow the renamed item:\n{out}"
        );

        // Strings that are not declared UUIDs are left alone — including a
        // sentence that merely contains one, and "root-a", which is a path
        // segment but was never declared here.
        assert!(out.contains("(project \"demo\""), "{out}");
        assert!(out.contains("(reference \"R1\")"), "{out}");
        assert!(out.contains("(lib_id \"Device:R\")"), "{out}");
        assert!(
            out.contains("\"see sym-a in the notes\""),
            "text content must survive verbatim:\n{out}"
        );
        assert!(out.contains("(path \"/root-a\" (page \"2\"))"), "{out}");
    }

    #[test]
    fn regenerating_uuids_leaves_a_document_without_any_alone() {
        let source = "(kicad_sch\n  (lib_symbols)\n)\n";
        assert_eq!(regenerate_item_uuids(source), source);
    }

    /// The scan walks every quoted string in the file, so an escaped quote
    /// inside text content must not shift it out of step — a bug there
    /// corrupts the whole document, not just the annotation.
    #[test]
    fn regenerating_uuids_survives_escaped_quotes_in_text() {
        let source = r#"(kicad_sch
  (text "a \"b\" c" (uuid "text-a"))
  (generator "konnect")
)
"#;

        let out = regenerate_item_uuids(source);

        assert!(
            out.contains("(generator \"konnect\")"),
            "a later string was corrupted by the escape:\n{out}"
        );
        assert!(out.contains(r#""a \"b\" c""#), "{out}");
        assert!(!out.contains("text-a"), "{out}");
    }

    /// The report: `add_schematic_text` then `duplicate_sheet` leaves both
    /// sheets carrying the same text UUID.
    #[tokio::test]
    async fn duplicate_sheet_gives_the_copy_its_own_item_uuids() {
        const SOURCE_TEXT_UUID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "sheet_file": "amp.kicad_sch",
                "sheet_name": "Amp1",
                "x": 10.0, "y": 10.0
            }),
            &ctx,
        )
        .await
        .unwrap();

        // An annotation in the child, shaped as add_schematic_text writes one.
        let child = tmp.path().join("amp.kicad_sch");
        let content = std::fs::read_to_string(&child).unwrap();
        let cut = content.rfind(')').unwrap();
        let block = format!(
            "\n  (text \"NOTE\"\n    (at 10 10 0)\n    \
             (effects (font (size 1.27 1.27)) (justify left bottom))\n    \
             (uuid \"{SOURCE_TEXT_UUID}\")\n  )\n"
        );
        std::fs::write(
            &child,
            format!("{}{}{}", &content[..cut], block, &content[cut..]),
        )
        .unwrap();

        let result = handle_duplicate_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "source_sheet_name": "Amp1",
                "new_sheet_name": "Amp2",
                "new_file": "amp2.kicad_sch"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let source_ids = declared_uuids(&std::fs::read_to_string(&child).unwrap());
        let copy_ids =
            declared_uuids(&std::fs::read_to_string(tmp.path().join("amp2.kicad_sch")).unwrap());

        assert!(
            source_ids.contains(SOURCE_TEXT_UUID),
            "the source keeps its own annotation"
        );
        assert!(
            !copy_ids.contains(SOURCE_TEXT_UUID),
            "the copy kept the source's text UUID"
        );
        assert!(
            source_ids.is_disjoint(&copy_ids),
            "no UUID may be shared between a sheet and its copy:\n{source_ids:?}\n{copy_ids:?}"
        );
        assert_eq!(
            source_ids.len(),
            copy_ids.len(),
            "same items, new identities"
        );
    }

    #[tokio::test]
    async fn duplicate_sheet_refuses_to_overwrite_existing_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        // A second, unrelated sheet already occupies "b.kicad_sch".
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "b.kicad_sch", "sheet_name": "B" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_duplicate_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "source_sheet_name": "A",
                "new_sheet_name": "A-copy",
                "new_file": "b.kicad_sch"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn get_sheet_hierarchy_returns_nested_tree() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "mid.kicad_sch", "sheet_name": "Mid" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": tmp.path().join("mid.kicad_sch").display().to_string(), "sheet_file": "leaf.kicad_sch", "sheet_name": "Leaf" }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_get_sheet_hierarchy(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        assert!(!result.is_error);

        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let tree: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(tree["children"][0]["name"], "Mid");
        assert_eq!(tree["children"][0]["children"][0]["name"], "Leaf");
    }

    #[tokio::test]
    async fn get_sheet_hierarchy_reports_missing_child_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "gone.kicad_sch", "sheet_name": "Gone" }),
            &ctx,
        )
        .await
        .unwrap();
        std::fs::remove_file(tmp.path().join("gone.kicad_sch")).unwrap();

        let result =
            handle_get_sheet_hierarchy(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let tree: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(tree["children"][0]["error"], "child file not found on disk");
    }

    #[tokio::test]
    async fn renumber_sheet_pages_closes_gap_after_delete() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        for (file, name) in [
            ("a.kicad_sch", "A"),
            ("b.kicad_sch", "B"),
            ("c.kicad_sch", "C"),
        ] {
            handle_add_hierarchical_sheet(
                &json!({ "schematic": root.display().to_string(), "sheet_file": file, "sheet_name": name }),
                &ctx,
            )
            .await
            .unwrap();
        }
        // A=2, B=3, C=4. Delete B, leaving a gap at page 3.
        handle_delete_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "B" }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_renumber_sheet_pages(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.by_name("A").unwrap().page("root"), Some("2"));
        assert_eq!(parent.sheets.by_name("C").unwrap().page("root"), Some("3"));
    }

    #[tokio::test]
    async fn linking_existing_file_with_symbols_patches_instance_paths() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let child_path = tmp.path().join("reused.kicad_sch");
        create_blank_schematic(&child_path).unwrap();

        // Put a symbol in the child file before it's ever linked.
        {
            let mut child = cse::Schematic::load(&child_path).unwrap();
            let mut sym = cse::Symbol::new("Device:R", 10.0, 10.0);
            sym.set_reference("R1");
            child.add_symbol(sym);
            child.overwrite().unwrap();
        }

        let ctx = test_ctx();
        let result = handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "reused.kicad_sch", "sheet_name": "Reused" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let child = cse::Schematic::load(&child_path).unwrap();
        let sym = child.symbols.by_reference("R1").unwrap();
        // eeschema path format: "/<root-uuid>/<sheet-symbol-uuid>", keyed
        // under the default project name (the parent file's stem).
        let parent = cse::Schematic::load(&root).unwrap();
        let hier_path = format!(
            "/{}/{}",
            parent.uuid.as_deref().expect("root uuid must exist"),
            parent.sheets.by_name("Reused").unwrap().uuid
        );
        assert!(sym.has_instance_path("root", &hier_path));
    }

    // ─── PR-B: sheet pin lifecycle ─────────────────────────────────────────

    fn add_label(sch_path: &Path, text: &str, shape: &str, x: f64, y: f64) {
        let mut sch = cse::Schematic::load(sch_path).unwrap();
        sch.add_hierarchical_label(text, shape, x, y);
        sch.overwrite().unwrap();
    }

    #[tokio::test]
    async fn import_sheet_pins_creates_matching_pins_from_labels() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "power.kicad_sch", "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let child_path = tmp.path().join("power.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);
        add_label(&child_path, "GND", "passive", 5.0, 10.0);

        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("Power").unwrap();
        assert_eq!(sheet.pins.len(), 2);
        assert_eq!(sheet.pin_by_name("VIN").unwrap().pin_type, "input");
        assert_eq!(sheet.pin_by_name("GND").unwrap().pin_type, "passive");
    }

    #[tokio::test]
    async fn import_sheet_pins_skips_already_imported_names() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "power.kicad_sch", "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let child_path = tmp.path().join("power.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);

        handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.by_name("Power").unwrap().pins.len(), 1); // not duplicated
    }

    #[tokio::test]
    async fn add_sheet_pin_writes_a_rotation_kicad_can_load() {
        // Regression for #303: the pin used to be written as `(at x y)` with no
        // rotation, and KiCAD then refused to load the whole schematic.
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "TESTNET", "pin_type": "input", "x": 100.0, "y": 105.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let written = std::fs::read_to_string(&root).unwrap();
        assert!(
            written.contains("(at 100 105 0)"),
            "sheet pin must be written with a rotation, got: {}",
            written
                .lines()
                .skip_while(|l| !l.contains("(pin \"TESTNET\""))
                .take(3)
                .collect::<Vec<_>>()
                .join("\n")
        );

        // And it must survive a reload through the same parser.
        let parent = cse::Schematic::load(&root).unwrap();
        let pin_rotation = parent.sheets.by_name("A").unwrap().pins[0].at.rotation;
        assert_eq!(pin_rotation, Some(0.0));
    }

    #[tokio::test]
    async fn add_sheet_pin_rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let args = json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "input", "x": 90.0, "y": 55.0 });
        let result = handle_add_sheet_pin(&args, &ctx).await.unwrap();
        assert!(!result.is_error);

        let result2 = handle_add_sheet_pin(&args, &ctx).await.unwrap();
        assert!(result2.is_error);
    }

    // ─── sheet pin sides ───────────────────────────────────────────────────
    //
    // The default sheet `handle_add_hierarchical_sheet` writes is 80 × 50 at
    // (50, 50), so its edges are x = 50 (left), x = 130 (right), y = 50 (top)
    // and y = 100 (bottom).

    /// A sheet with the default box, plus the paths to work on it.
    async fn sheet_for_pins(tmp: &TempDir, ctx: &ToolContext) -> PathBuf {
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            ctx,
        )
        .await
        .unwrap();
        root
    }

    fn pin_rotation(root: &Path, pin_name: &str) -> Option<f64> {
        cse::Schematic::load(root)
            .unwrap()
            .sheets
            .by_name("A")
            .unwrap()
            .pin_by_name(pin_name)
            .unwrap()
            .at
            .rotation
    }

    #[tokio::test]
    async fn add_sheet_pin_writes_the_rotation_each_side_selects() {
        // KiCad reads a sheet pin's edge from its rotation, so `side` has to
        // reach the file as an angle. 90 and 270 were unreachable from any tool.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;

        // name, side, position on that edge, the angle KiCad reads it back as
        let cases = [
            ("R_PIN", "right", 130.0, 55.0, 0.0),
            ("T_PIN", "top", 60.0, 50.0, 90.0),
            ("L_PIN", "left", 50.0, 60.0, 180.0),
            ("B_PIN", "bottom", 70.0, 100.0, 270.0),
        ];
        for (name, side, x, y, expected_rotation) in cases {
            let result = handle_add_sheet_pin(
                &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                         "pin_name": name, "pin_type": "input",
                         "x": x, "y": y, "side": side }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(!result.is_error, "{side} pin refused: {:?}", result.content);
            assert_eq!(
                result_json(&result)["side"],
                json!(side),
                "the reported side must be read back off the written pin"
            );
            assert_eq!(
                pin_rotation(&root, name),
                Some(expected_rotation),
                "'{side}' must reach the file as {expected_rotation}"
            );
        }
    }

    #[tokio::test]
    async fn add_sheet_pin_refuses_a_position_off_the_named_edge() {
        // The whole point of the parameter: KiCad would accept this write and
        // then move the pin to the top edge, so the file would stop describing
        // what the editor shows.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;

        let result = handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "STRAY", "pin_type": "input",
                     "x": 60.0, "y": 75.0, "side": "top" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error, "a position off the named edge must refuse");
        let message = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            message.contains("'top' edge") && message.contains("y = 50"),
            "the refusal must name the edge and where it is, got: {message}"
        );
        let sheet_pins = cse::Schematic::load(&root)
            .unwrap()
            .sheets
            .by_name("A")
            .unwrap()
            .pins
            .len();
        assert_eq!(sheet_pins, 0, "a refused call must write nothing");
    }

    #[tokio::test]
    async fn add_sheet_pin_without_a_side_still_accepts_any_position() {
        // Scope test, not coverage: it survives neutering every new guard. It
        // is here because the change has to be purely additive — the position
        // check rides on `side`, so a caller that never passed one keeps the
        // behaviour it had, off-edge position and all.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;

        let result = handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "LEGACY", "pin_type": "input", "x": 100.0, "y": 105.0 }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(!result.is_error, "an old-style call must keep working");
        assert_eq!(pin_rotation(&root, "LEGACY"), Some(0.0));
    }

    #[tokio::test]
    async fn add_sheet_pin_refuses_a_position_past_the_end_of_the_named_edge() {
        // Measured against KiCad 10.0.6: a pin on the right edge's x but below
        // the box was rewritten from `(at 130 140 0)` to `(at 130 100 0)` — the
        // edge constrains the pin along its length as well as across it.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;

        let result = handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "PAST_END", "pin_type": "input",
                     "x": 130.0, "y": 140.0, "side": "right" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error, "a position past the corner must refuse");
        let message = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            message.contains("past the end of the 'right' edge") && message.contains("to 100"),
            "the refusal must say the edge ran out and where, got: {message}"
        );
    }

    #[tokio::test]
    async fn add_sheet_pin_accepts_a_pin_on_a_corner() {
        // Both corners of an edge are on it: KiCad left `(at 130 50 0)` and
        // `(at 130 100 0)` untouched, so the span check has to be inclusive.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;

        for (name, y) in [("TOP_CORNER", 50.0), ("BOTTOM_CORNER", 100.0)] {
            let result = handle_add_sheet_pin(
                &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                         "pin_name": name, "pin_type": "input",
                         "x": 130.0, "y": y, "side": "right" }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(!result.is_error, "{name} refused: {:?}", result.content);
        }
    }

    #[tokio::test]
    async fn add_sheet_pin_refuses_a_side_that_is_not_an_edge() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;

        let result = handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "VCC", "pin_type": "input",
                     "x": 130.0, "y": 55.0, "side": "north" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error, "'north' is not a sheet edge");
    }

    /// The pin the file holds, read back the long way round rather than through
    /// the handler that is under test.
    fn saved_pin(root: &Path, pin_name: &str) -> cse::SheetPin {
        cse::Schematic::load(root)
            .unwrap()
            .sheets
            .by_name("A")
            .unwrap()
            .pin_by_name(pin_name)
            .unwrap()
            .clone()
    }

    #[tokio::test]
    async fn add_sheet_pin_takes_the_rotation_from_the_side_not_the_position() {
        // Negative control for the rotation write. Every other side case sits on
        // exactly one edge, so an implementation that inferred the side from the
        // position — never mind the argument — would pass all of them. A corner
        // belongs to two edges at once, so the same point has to produce a
        // different rotation depending on which edge the caller named, and only
        // an implementation that reads the argument can do that.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;

        // (130, 50) is the top-right corner of the default box: the right
        // edge's x and the top edge's y, both exactly.
        for (name, side, expected_rotation) in
            [("CORNER_R", "right", 0.0), ("CORNER_T", "top", 90.0)]
        {
            let result = handle_add_sheet_pin(
                &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                         "pin_name": name, "pin_type": "input",
                         "x": 130.0, "y": 50.0, "side": side }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(!result.is_error, "{name} refused: {:?}", result.content);
            assert_eq!(
                pin_rotation(&root, name),
                Some(expected_rotation),
                "'{side}' at the shared corner must reach the file as {expected_rotation}"
            );
            assert_eq!(
                result_json(&result)["side"],
                json!(side),
                "the reported side must follow the written rotation"
            );
        }
    }

    #[tokio::test]
    async fn add_sheet_pin_reports_the_saved_position_not_the_requested_one() {
        // The schematic writer holds six decimal places, so a finer position
        // cannot reach the file intact. That is the one place where the saved
        // pin and the request differ observably, and it is what tells a response
        // derived from the file apart from one that restates its arguments.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;

        // Inside SHEET_PIN_EDGE_TOLERANCE_MM, so the edge guard accepts it —
        // the subject here is the response, not a refusal.
        let requested_x = 130.000_000_4_f64;
        let requested_y = 55.000_000_4_f64;
        let result = handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "FINE", "pin_type": "input",
                     "x": requested_x, "y": requested_y, "side": "right" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let saved = saved_pin(&root, "FINE");
        assert_ne!(
            saved.at.x, requested_x,
            "the fixture proves nothing unless the file really does round the request"
        );
        assert_ne!(saved.at.y, requested_y);

        let body = result_json(&result);
        assert_eq!(
            body["x"],
            json!(saved.at.x),
            "x must be read back off the saved pin, got {body}"
        );
        assert_eq!(
            body["y"],
            json!(saved.at.y),
            "y must be read back off the saved pin, got {body}"
        );
        assert_ne!(body["x"], json!(requested_x), "x must not echo the request");
        assert_ne!(body["y"], json!(requested_y), "y must not echo the request");
    }

    #[tokio::test]
    async fn edit_sheet_pin_reports_the_saved_position_not_the_requested_one() {
        // Same control, one tool over: `edit_sheet_pin` built its summary from
        // the in-memory pin before the commit, which is the request by another
        // name.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "pin_type": "input",
                     "x": 130.0, "y": 55.0, "side": "right" }),
            &ctx,
        )
        .await
        .unwrap();

        let requested_x = 130.000_000_4_f64;
        let requested_y = 60.000_000_4_f64;
        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "x": requested_x, "y": requested_y,
                     "side": "right" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let saved = saved_pin(&root, "CLK");
        assert_ne!(
            saved.at.x, requested_x,
            "the fixture proves nothing unless the file really does round the request"
        );
        assert_ne!(saved.at.y, requested_y);

        let pin = &result_json(&result)["pin"];
        assert_eq!(pin["x"], json!(saved.at.x), "x must come off the saved pin");
        assert_eq!(pin["y"], json!(saved.at.y), "y must come off the saved pin");
        assert_ne!(pin["x"], json!(requested_x), "x must not echo the request");
        assert_ne!(pin["y"], json!(requested_y), "y must not echo the request");
        assert_eq!(pin["side"], json!("right"));
    }

    #[tokio::test]
    async fn edit_sheet_pin_reports_the_name_it_saved_under() {
        // A rename moves the pin the response describes, so the read-back has to
        // follow it. Reporting the old name would either fail to find the pin or
        // find a different one.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "OLD", "pin_type": "input",
                     "x": 130.0, "y": 55.0, "side": "right" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "OLD", "new_name": "NEW", "pin_type": "output" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let pin = &result_json(&result)["pin"];
        assert_eq!(pin["name"], json!("NEW"));
        assert_eq!(pin["pin_type"], json!("output"));
        assert_eq!(saved_pin(&root, "NEW").pin_type, "output");
    }

    #[tokio::test]
    async fn edit_sheet_pin_moves_a_pin_to_another_edge() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "pin_type": "input", "x": 130.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "x": 65.0, "y": 100.0, "side": "bottom" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let body = result_json(&result);
        assert_eq!(body["changed_fields"], json!(["position", "side"]));
        assert_eq!(body["pin"]["side"], json!("bottom"));
        assert_eq!(pin_rotation(&root, "CLK"), Some(270.0));
    }

    #[tokio::test]
    async fn edit_sheet_pin_judges_the_side_against_the_position_it_is_given() {
        // `side` and `x`+`y` in one call describe the pin's end state, so the
        // check has to run after the move, not against the position replaced.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "pin_type": "input", "x": 130.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        // The pin is on the right edge; the new position is on the top edge.
        // Judged against the old position this would refuse, and it must not.
        let ok = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "x": 65.0, "y": 50.0, "side": "top" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!ok.is_error, "{:?}", ok.content);
        assert_eq!(pin_rotation(&root, "CLK"), Some(90.0));

        // And the reverse: a move that lands off the named edge is refused.
        let refused = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "x": 65.0, "y": 60.0, "side": "top" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(refused.is_error, "a move off the named edge must refuse");
        assert_eq!(
            pin_rotation(&root, "CLK"),
            Some(90.0),
            "a refused edit leaves the pin as it was"
        );
    }

    #[tokio::test]
    async fn edit_sheet_pin_refuses_a_side_the_pin_is_not_already_on() {
        // `side` with no position: the pin does not move, so it has to already
        // be on that edge.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "pin_type": "input", "x": 130.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "side": "bottom" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert_eq!(pin_rotation(&root, "CLK"), Some(0.0), "nothing was written");
    }

    #[tokio::test]
    async fn edit_sheet_pin_counts_side_on_its_own_as_a_change() {
        // `side` on its own is a legal one-field edit, not the "no fields to
        // change" refusal — and here it is a real one, moving the pin from the
        // right edge it defaulted to onto the left.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "pin_type": "input", "x": 50.0, "y": 60.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "side": "left" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let body = result_json(&result);
        assert_eq!(body["changed_fields"], json!(["side"]));
        assert_eq!(body["requested_fields"], json!(["side"]));
        assert_eq!(body["changed"], json!(true));
        assert_eq!(pin_rotation(&root, "CLK"), Some(180.0));
    }

    #[tokio::test]
    async fn edit_sheet_pin_does_not_count_a_restated_side_as_a_change() {
        // `changed_fields` describes the delta that was saved, not the
        // arguments that were sent. A caller that sets `side` unconditionally
        // restates the edge the pin is already on, and reporting that as a
        // change would make the response say the file moved when it did not.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "pin_type": "input",
                     "x": 60.0, "y": 100.0, "side": "bottom" }),
            &ctx,
        )
        .await
        .unwrap();
        let before = std::fs::read(&root).unwrap();

        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "CLK", "side": "bottom" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let body = result_json(&result);
        assert_eq!(
            body["changed_fields"],
            json!([]),
            "nothing differed, so nothing may be reported as changed"
        );
        assert_eq!(body["changed"], json!(false));
        assert_eq!(
            body["requested_fields"],
            json!(["side"]),
            "the request is still reported — it was made, it just changed nothing"
        );
        assert_eq!(body["pin"]["side"], json!("bottom"));
        assert!(
            std::fs::read(&root).unwrap() == before,
            "a no-op edit must not rewrite the file"
        );
    }

    #[tokio::test]
    async fn import_sheet_pins_stacks_along_a_top_or_bottom_edge() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        let child_path = tmp.path().join("a.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);
        add_label(&child_path, "GND", "passive", 5.0, 10.0);

        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "top" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result_json(&result)["side"], json!("top"));

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("A").unwrap();
        for name in ["VIN", "GND"] {
            let pin = sheet.pin_by_name(name).unwrap();
            assert_eq!(pin.at.rotation, Some(90.0), "{name} must face the top edge");
            assert_eq!(pin.at.y, 50.0, "{name} must sit on the top edge itself");
        }
        // Stacked along the edge rather than piled on one point.
        assert_ne!(
            sheet.pin_by_name("VIN").unwrap().at.x,
            sheet.pin_by_name("GND").unwrap().at.x
        );
        assert!(
            sheet.pins.iter().all(|p| p.at.x >= 50.0),
            "a top-edge stack runs across the box from its left corner"
        );
    }

    #[tokio::test]
    async fn import_sheet_pins_keeps_placing_a_left_or_right_stack_down_the_edge() {
        // Scope test: it survives neutering the new guards, and exists because
        // the two old sides must land exactly where they always did.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        let child_path = tmp.path().join("a.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);
        add_label(&child_path, "GND", "passive", 5.0, 10.0);

        handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "left" }),
            &ctx,
        )
        .await
        .unwrap();

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("A").unwrap();
        assert_eq!(sheet.pin_by_name("VIN").unwrap().at.x, 50.0);
        assert_eq!(sheet.pin_by_name("VIN").unwrap().at.y, 52.54);
        assert_eq!(sheet.pin_by_name("GND").unwrap().at.y, 55.08);
        assert_eq!(sheet.pin_by_name("GND").unwrap().at.rotation, Some(180.0));
    }

    #[tokio::test]
    async fn import_sheet_pins_reports_no_side_when_it_saved_no_pin() {
        // The reported side is read off a pin the import actually wrote. An
        // import that wrote nothing has none to read, and has to say so rather
        // than assert the side it was asked for — the only case in which a
        // derived answer and an echoed one differ.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        let child_path = tmp.path().join("a.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);

        handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "top" }),
            &ctx,
        )
        .await
        .unwrap();

        // Second pass: the only label already has a pin, so nothing is saved —
        // and a different side is asked for, so an echo would be visibly wrong.
        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "bottom" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        let body = result_json(&result);
        assert_eq!(body["imported_pins"], json!([]));
        assert_eq!(body["skipped_existing"], json!(["VIN"]));
        assert_eq!(
            body["side"],
            serde_json::Value::Null,
            "an import that saved no pin has no side to report"
        );
    }

    /// Give a pin a rotation that names no edge — the shape a file written by
    /// something other than these tools can carry.
    fn set_pin_rotation(root: &Path, pin_name: &str, rotation: Option<f64>) {
        let mut sch = cse::Schematic::load(root).unwrap();
        sch.sheets
            .by_name_mut("A")
            .unwrap()
            .pin_by_name_mut(pin_name)
            .unwrap()
            .at
            .rotation = rotation;
        sch.overwrite().unwrap();
    }

    #[tokio::test]
    async fn import_sheet_pins_counts_a_pin_whose_rotation_names_no_edge() {
        // Counting only the pins on the selected edge is more precise than
        // counting every pin on the sheet, and precision cost a safety margin
        // the blunt count had by accident: a pin whose rotation names no edge
        // belongs to no edge under that filter, so the stack walks over it.
        // If we cannot tell which edge a pin is on, we have to assume it could
        // be this one — stacking further out is safe, stacking over it is not.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                     "pin_name": "ODD", "pin_type": "passive",
                     "x": 50.0, "y": 52.54, "side": "left" }),
            &ctx,
        )
        .await
        .unwrap();
        // Exactly where the first imported left-edge pin would otherwise land.
        set_pin_rotation(&root, "ODD", None);

        let child_path = tmp.path().join("a.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);
        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "left" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("A").unwrap();
        let vin = sheet.pin_by_name("VIN").unwrap();
        let odd = sheet.pin_by_name("ODD").unwrap();
        assert_ne!(
            (vin.at.x, vin.at.y),
            (odd.at.x, odd.at.y),
            "an import must not stack over a pin whose edge it cannot determine"
        );
        assert_eq!(
            vin.at.y, 55.08,
            "the unattributable pin has to occupy a slot, so the import starts at the next one"
        );
    }

    #[tokio::test]
    async fn import_sheet_pins_refuses_a_left_edge_import_that_runs_past_the_corner() {
        // The sheet box is 80 x 50 mm, so its left edge holds 19 pins at
        // 2.54 mm. The nineteenth is the last that fits; the twentieth lands at
        // y = 100.8, past the corner at y = 100, and KiCad would pull it back
        // onto the corner. The import has to refuse before writing any of it.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        let child_path = tmp.path().join("a.kicad_sch");
        for n in 0..19 {
            add_label(
                &child_path,
                &format!("NET{n}"),
                "passive",
                5.0,
                5.0 + n as f64,
            );
        }

        // A full edge is not an overflowing one: all nineteen must land.
        let filled = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "left" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!filled.is_error, "{:?}", filled.content);
        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.by_name("A").unwrap().pins.len(), 19);
        assert_eq!(
            parent
                .sheets
                .by_name("A")
                .unwrap()
                .pin_by_name("NET18")
                .unwrap()
                .at
                .y,
            98.26,
            "the last pin that fits sits 2.54 mm short of the corner"
        );

        // One more label, and the edge is out of room.
        add_label(&child_path, "ONE_TOO_MANY", "passive", 5.0, 30.0);
        let before = std::fs::read(&root).unwrap();

        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "left" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error, "an import past the corner must refuse");
        let message = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            message.contains("'left' edge")
                && message.contains("19 sheet pins")
                && message.contains("ONE_TOO_MANY"),
            "the refusal must name the edge, its capacity and the pin that did not fit, got: \
             {message}"
        );
        assert!(
            std::fs::read(&root).unwrap() == before,
            "a refused import must leave the file byte-identical"
        );
        let after = cse::Schematic::load(&root).unwrap();
        let sheet = after.sheets.by_name("A").unwrap();
        assert_eq!(sheet.pins.len(), 19, "no pin was written");
        assert!(
            sheet.pin_by_name("ONE_TOO_MANY").is_none(),
            "the overflowing pin must not be on the sheet"
        );
    }

    #[tokio::test]
    async fn import_sheet_pins_refuses_a_top_edge_import_that_runs_past_the_corner() {
        // Same failure one axis over: the 80 mm top edge holds 31 pins, so the
        // thirty-second lands at x = 131.28 against a corner at x = 130. This
        // one overflows from an empty sheet, so a loop that wrote as it went
        // would leave thirty-one pins behind — the assertion below is that the
        // file is untouched, not merely that an error came back.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        let child_path = tmp.path().join("a.kicad_sch");
        for n in 0..32 {
            add_label(
                &child_path,
                &format!("BUS{n}"),
                "passive",
                5.0,
                5.0 + n as f64,
            );
        }
        let before = std::fs::read(&root).unwrap();

        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "top" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error, "an import past the corner must refuse");
        let message = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            message.contains("'top' edge") && message.contains("31 sheet pins"),
            "the refusal must name the edge and its capacity, got: {message}"
        );
        assert!(
            std::fs::read(&root).unwrap() == before,
            "a refused import must leave the file byte-identical"
        );
        assert_eq!(
            cse::Schematic::load(&root)
                .unwrap()
                .sheets
                .by_name("A")
                .unwrap()
                .pins
                .len(),
            0,
            "not one of the thirty-two pins was written"
        );
    }

    #[tokio::test]
    async fn import_sheet_pins_stacks_below_the_pins_already_on_that_edge() {
        // The stack used to start below *every* pin on the sheet, so pins on
        // one edge pushed the next import down another edge and eventually off
        // the end of it. Only the pins carrying this edge's rotation count.
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;
        for (name, y) in [("R0", 60.0), ("R1", 70.0), ("R2", 80.0)] {
            handle_add_sheet_pin(
                &json!({ "schematic": root.display().to_string(), "sheet_name": "A",
                         "pin_name": name, "pin_type": "input",
                         "x": 130.0, "y": y, "side": "right" }),
                &ctx,
            )
            .await
            .unwrap();
        }
        let child_path = tmp.path().join("a.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);

        handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "left" }),
            &ctx,
        )
        .await
        .unwrap();

        let parent = cse::Schematic::load(&root).unwrap();
        let vin = parent
            .sheets
            .by_name("A")
            .unwrap()
            .pin_by_name("VIN")
            .unwrap()
            .clone();
        assert_eq!(vin.at.x, 50.0);
        assert_eq!(
            vin.at.y, 52.54,
            "the left edge is empty, so the first imported pin takes its first slot"
        );
    }

    #[tokio::test]
    async fn import_sheet_pins_refuses_a_side_that_is_not_an_edge() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = sheet_for_pins(&tmp, &ctx).await;

        let result = handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "side": "middle" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error);
    }

    #[tokio::test]
    async fn add_sheet_pin_rejects_invalid_pin_type() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "not_a_type", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn edit_sheet_pin_renames_and_retypes() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "input", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "new_name": "VDD", "pin_type": "output" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("A").unwrap();
        assert!(sheet.pin_by_name("VCC").is_none());
        let renamed = sheet.pin_by_name("VDD").unwrap();
        assert_eq!(renamed.pin_type, "output");
    }

    #[tokio::test]
    async fn edit_sheet_pin_with_no_fields_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "input", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn delete_sheet_pin_removes_it() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC", "pin_type": "input", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_delete_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "VCC" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert!(parent
            .sheets
            .by_name("A")
            .unwrap()
            .pin_by_name("VCC")
            .is_none());
    }

    #[tokio::test]
    async fn delete_sheet_pin_not_found_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_delete_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "pin_name": "Nope" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn validate_sheet_pins_reports_mismatches() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "power.kicad_sch", "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let child_path = tmp.path().join("power.kicad_sch");
        // Label with no pin, and (below) a pin with no label — deliberate mismatch.
        add_label(&child_path, "VIN", "input", 5.0, 5.0);
        handle_add_sheet_pin(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power", "pin_name": "GND", "pin_type": "passive", "x": 90.0, "y": 55.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_validate_sheet_pins(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        assert!(!result.is_error);

        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let report: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["issue_count"], 1);
        let issue = &report["issues"][0];
        assert_eq!(issue["sheet"], "Power");
        assert!(issue["labels_without_pins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "VIN"));
        assert!(issue["pins_without_labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "GND"));
    }

    #[tokio::test]
    async fn validate_sheet_pins_reports_no_issues_when_synced() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "power.kicad_sch", "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();
        let child_path = tmp.path().join("power.kicad_sch");
        add_label(&child_path, "VIN", "input", 5.0, 5.0);
        handle_import_sheet_pins(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Power" }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_validate_sheet_pins(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let report: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["issue_count"], 0);
    }

    fn real_hierarchy_fixture(dir: &Path) -> PathBuf {
        let path = dir.join("probe.kicad_sch");
        std::fs::write(
            &path,
            include_str!("../../tests/fixtures/junction_sheet_pin.kicad_sch"),
        )
        .unwrap();
        path
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hierarchy_handlers_refuse_bad_prospective_results_without_writing() {
        let tmp = TempDir::new().unwrap();
        let root = real_hierarchy_fixture(tmp.path());
        let ctx = test_ctx();
        let before = std::fs::read_to_string(&root).unwrap();
        HIERARCHY_PROSPECTIVE_FAULT.with(|fault| {
            *fault.borrow_mut() = Some(HierarchyProspectiveFault::Replace {
                from: "(at 75 80)".to_owned(),
                to: "(at 76 80)".to_owned(),
            });
        });
        let refusal = handle_move_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "sheet_name": "test",
                "x": 75.0,
                "y": 80.0,
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            extract_error_kind(&refusal).as_deref(),
            Some("stale_target")
        );
        assert_eq!(
            std::fs::read_to_string(&root).unwrap(),
            before,
            "move_sheet must leave the real KiCad file byte-identical"
        );

        let deleted_sheet = cse::Schematic::load(&root)
            .unwrap()
            .sheets
            .by_name("test")
            .unwrap()
            .to_sexp();
        HIERARCHY_PROSPECTIVE_FAULT.with(|fault| {
            *fault.borrow_mut() = Some(HierarchyProspectiveFault::RestoreDeletedSheet(
                cse::sexp::writer::write(&deleted_sheet),
            ));
        });
        let refusal = handle_delete_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "sheet_name": "test",
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            extract_error_kind(&refusal).as_deref(),
            Some("stale_target")
        );
        assert_eq!(
            std::fs::read_to_string(&root).unwrap(),
            before,
            "delete_sheet must leave the real KiCad file byte-identical"
        );
    }
}
