//! `sch_components` toolset — add, edit, move, rotate, delete schematic symbols.
//!
//! Simple CRUD operations use `konnect_schematic_editor` (cse) for structured
//! round-trip parsing.  Pin coordinate math still delegates to
//! `konnect_sexp::geometry::transform_pin`.

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{
    find_all_symbol_instance_blocks, get_path, opt_f64, opt_str, reembed_lib_symbols,
    require_array, require_f64, require_str, ReembedOutcome, ToolContext, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    commit_command,
    geometry::snap_point,
    parse_sexp, prepare_command,
    schematic::{
        extract_lib_pins_for_unit, extract_symbol_instances, find_lib_symbol, pin_endpoint,
        pin_outward_direction, read_schematic,
    },
    writer::{
        apply_edits, find_direct_child_blocks, read_consistent, write_atomic_if_unchanged,
        write_new_atomic, SexpEdit,
    },
    ItemAnchor, ItemId, SchematicCommand, SexpError,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "create_schematic",
            "Create a new blank .kicad_sch schematic file, on A4 unless another paper \
             size is given. Use set_schematic_page to change it later.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full path for the new .kicad_sch file" },
                    "size": {
                        "type": "string",
                        "description": "Paper size, e.g. 'A4', 'A3', 'USLetter' (default 'A4')",
                        "enum": ["A0", "A1", "A2", "A3", "A4", "A5",
                                 "A", "B", "C", "D", "E",
                                 "USLetter", "USLegal", "USLedger"],
                        "default": "A4"
                    },
                    "portrait": {
                        "type": "boolean",
                        "description": "Portrait instead of the default landscape",
                        "default": false
                    }
                },
                "required": ["path"]
            }),
            |args, ctx| async move { handle_create_schematic(args, ctx).await }
        ),
        tool!(
            "set_schematic_page",
            "Set the sheet's paper size (A0-A5, A-E, USLetter, USLegal, USLedger) and \
             orientation. Content outside the frame still exports and still nets up, so a \
             too-small page is a silent defect — check the layout extents against the size.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "size": {
                        "type": "string",
                        "description": "Paper size, e.g. 'A4', 'A3', 'A2', 'USLetter'",
                        "enum": ["A0", "A1", "A2", "A3", "A4", "A5",
                                 "A", "B", "C", "D", "E",
                                 "USLetter", "USLegal", "USLedger"]
                    },
                    "portrait": {
                        "type": "boolean",
                        "description": "Portrait instead of the default landscape",
                        "default": false
                    }
                },
                "required": ["schematic", "size"]
            }),
            |args, ctx| async move { handle_set_page(args, ctx).await }
        ),
        tool!(
            "add_schematic_component",
            "Add a symbol from a KiCAD library to the schematic. The symbol is snapped \
             to the 1.27mm schematic grid. Preserves every saved hierarchy instance, reports \
             committed-file readback, and refuses stale instance metadata before writing. \
             Specify position in schematic mm coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "lib_id": { "type": "string", "description": "Library:Symbol (e.g. 'Device:R')" },
                    "x": { "type": "number", "description": "X position in mm" },
                    "y": { "type": "number", "description": "Y position in mm" },
                    "rotation": { "type": "number", "description": "Rotation in degrees (0/90/180/270)", "default": 0 },
                    "reference": { "type": "string", "description": "Optional override for reference designator" },
                    "value": { "type": "string", "description": "Optional override for value field" },
                    "unit": { "type": "integer", "description": "Unit number for multi-unit symbols (gate/part selection). Default 1.", "default": 1 }
                },
                "required": ["schematic", "lib_id", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_component(args, ctx).await }
        ),
        tool!(
            "delete_schematic_component",
            "Remove a component by reference designator, including every placed unit of a multi-unit symbol.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator (e.g. 'R1')" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_delete_schematic_component(args, ctx).await }
        ),
        tool!(
            "edit_schematic_component",
            "Update fields (Reference, Value, Footprint, custom properties) consistently across every placed unit of a component.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Current reference designator" },
                    "new_reference": { "type": "string", "description": "New reference designator (optional)" },
                    "value": { "type": "string", "description": "New value (optional)" },
                    "footprint": { "type": "string", "description": "New footprint (optional)" },
                    "datasheet": { "type": "string", "description": "New datasheet URL (optional)" },
                    "fields": {
                        "type": "object",
                        "description": "Additional property fields to set as key:value pairs"
                    }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_edit_schematic_component(args, ctx).await }
        ),
        tool!(
            "get_schematic_component",
            "Get a component's shared properties and every placed unit's position. Use get_schematic_pin_locations for pins.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_component(args, ctx).await }
        ),
        tool!(
            "list_schematic_components",
            "List all symbol instances in a schematic with their positions, values, \
             footprints, and pin locations.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_list_schematic_components(args, ctx).await }
        ),
        tool!(
            "move_schematic_component",
            "Move a component's lowest-numbered unit to a new position and translate every \
             other placed unit by the same delta. Does NOT adjust connected wires. \
             Junction dots are re-judged where the pins moved: a dot the pins leave \
             unjustified is removed and a pin landing mid-span on a wire gains one, \
             reported as junctions_pruned_count and junctions_added_count.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "x": { "type": "number", "description": "New X position in mm" },
                    "y": { "type": "number", "description": "New Y position in mm" }
                },
                "required": ["schematic", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_schematic_component(args, ctx).await }
        ),
        tool!(
            "rotate_schematic_component",
            "Set the lowest-numbered unit's absolute rotation and rotate every other placed unit by the same delta.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "rotation": { "type": "number", "description": "Absolute rotation in degrees" }
                },
                "required": ["schematic", "reference", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_schematic_component(args, ctx).await }
        ),
        tool!(
            "move_connected",
            "REFUSED until implemented: moving a symbol while stretching its connected              wires is not built yet. Calling this returns an error naming              move_schematic_component as the working alternative — it moves the symbol              only, leaving wires where they are.",
            // No parameters: the handler refuses unconditionally, and the
            // schema-parameter guard (rightly) refuses a schema that advertises
            // arguments nothing reads. The old parameters are documented in
            // docs/API_MIGRATIONS.md alongside #285's removals.
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            |args, ctx| async move { handle_move_connected(args, ctx).await }
        ),
        tool!(
            "move_region",
            "Move all symbols within a bounding box by a given offset.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x1": { "type": "number", "description": "Region bounding box min X" },
                    "y1": { "type": "number", "description": "Region bounding box min Y" },
                    "x2": { "type": "number", "description": "Region bounding box max X" },
                    "y2": { "type": "number", "description": "Region bounding box max Y" },
                    "dx": { "type": "number", "description": "X offset to move by" },
                    "dy": { "type": "number", "description": "Y offset to move by" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2", "dx", "dy"]
            }),
            |args, ctx| async move { handle_move_region(args, ctx).await }
        ),
        tool!(
            "annotate_schematic",
            "Run kicad-cli to auto-assign reference designators (R? → R1, U? → U1, etc.).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_annotate_schematic(args, ctx).await }
        ),
        tool!(
            "get_schematic_pin_locations",
            "Get the exact schematic-space (X,Y) coordinates of every pin on every placed unit of a component, \
             accounting for rotation and mirroring. Uses the canonical pin transform. \
             Each pin also reports 'orientation_degrees', the direction leading away \
             from the symbol body (0 = east) — a net label at the pin must read that \
             way or its text runs back over the symbol's pin names — and 'length_mm'.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_pin_locations(args, ctx).await }
        ),
        tool!(
            "batch_get_schematic_pin_locations",
            "Get pin locations for multiple components in a single file read. Reports the \
             same per-pin fields as get_schematic_pin_locations, including \
             'orientation_degrees' and 'length_mm'.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators"
                    }
                },
                "required": ["schematic", "references"]
            }),
            |args, ctx| async move { handle_batch_get_pin_locations(args, ctx).await }
        ),
        tool!(
            "add_component_annotation",
            "Add or update a custom property consistently across every placed unit of a component.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'R1')" },
                    "key": { "type": "string", "description": "Property name" },
                    "value": { "type": "string", "description": "Property value" }
                },
                "required": ["schematic", "reference", "key", "value"]
            }),
            |args, ctx| async move { handle_add_component_annotation(args, ctx).await }
        ),
        tool!(
            "group_components",
            "Add or update a group property on every placed unit of multiple components.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators to group"
                    },
                    "group_name": { "type": "string", "description": "Group name to assign" }
                },
                "required": ["schematic", "references", "group_name"]
            }),
            |args, ctx| async move { handle_group_components(args, ctx).await }
        ),
        tool!(
            "replace_component",
            "Replace every placed unit of a component with a new library symbol while preserving unit numbers. A unit override is accepted only for a single placement.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'U1')" },
                    "new_lib_id": { "type": "string", "description": "New Library:Symbol identifier (e.g. 'Device:C')" },
                    "unit": { "type": "integer", "description": "Optional unit number for a single placed unit; rejected as ambiguous when the reference has multiple placements. When omitted, every existing unit number is preserved and validated against the new symbol." }
                },
                "required": ["schematic", "reference", "new_lib_id"]
            }),
            |args, ctx| async move { handle_replace_component(args, ctx).await }
        ),
        tool!(
            "update_symbols_from_library",
            "Re-embed placed symbols' definitions from their libraries, like KiCad's \
             'Update Symbols from Library'. A schematic carries its own copy of every \
             symbol, so editing one in its library leaves the sheet drawing the old \
             shape — this refreshes it. A symbol whose pins moved or disappeared in \
             the library is refused (reported in pins_moved) unless allow_pin_moves \
             is set, because wires and labels attach at pin coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Component references to update (e.g. ['U1']). Omit to update every symbol in the schematic.",
                        "items": { "type": "string" }
                    },
                    "dry_run": { "type": "boolean", "default": false,
                        "description": "Report what would change without writing." },
                    "allow_pin_moves": { "type": "boolean", "default": false,
                        "description": "Update symbols even when the library moved or removed pins. Wires and labels attached at the old pin positions are NOT moved with them — reconnect them afterwards." }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_update_symbols_from_library(args, ctx).await }
        ),
        tool!(
            "reset_schematic_field_positions",
            "Move each placed symbol's Reference and Value text back to the position its \
             library definition anchors them at, carried through the symbol's own rotation \
             — KiCad's 'Reset field text positions'. Use it on a sheet whose fields sit at \
             a uniform offset instead of where the library puts them (labels inside a \
             connector body, a rail's name below an up-pointing arrow). Fields a symbol's \
             definition gives no anchor for are left alone.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Component references to reset (e.g. ['U1']). Omit to reset every symbol in the schematic.",
                        "items": { "type": "string" }
                    },
                    "dry_run": { "type": "boolean", "default": false,
                        "description": "Report what would move without writing." }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_reset_schematic_field_positions(args, ctx).await }
        ),
        tool!(
            "get_schematic_view",
            "Render a schematic sheet with kicad-cli and return the path to the SVG it wrote.              There is no PNG: KiCad ships no schematic rasteriser — `sch export` offers no bitmap              format and there is no `sch render` — so this is a vector file, not an image that can              be shown inline. It lands in a temporary directory and is overwritten by the next view              of the same sheet; use export_schematic_svg (toolset sch_export) to choose where it              goes. The SVG doubles as a geometry source: kicad-cli writes every string a second              time as an invisible <text opacity=\"0\"> element carrying x, y, textLength, font-size              and text-anchor, so text content, position and width can be checked without rendering              a pixel.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_schematic_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_create_schematic(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "path")?;
    let size = opt_str(args, "size").unwrap_or("A4").to_string();
    let portrait = args["portrait"].as_bool().unwrap_or(false);
    let (w, h) = match paper_dimensions(&size) {
        Ok(dims) => dims,
        Err(e) => return Ok(e),
    };
    let (width_mm, height_mm) = if portrait { (h, w) } else { (w, h) };

    // Build a minimal valid schematic and save via cse's atomic writer.
    let template = crate::tools::blank_schematic_template_with_paper(&size, portrait);
    // Write the template then immediately load/save through cse so the file
    // is normalised to cse's writer output format.
    write_new_atomic(&path, &template)?;
    let sch = cse::Schematic::load(&path)?;
    sch.overwrite()?;
    Ok(CallToolResult::json(&json!({
        "created": path.display().to_string(),
        "size": size,
        "portrait": portrait,
        "width_mm": width_mm,
        "height_mm": height_mm
    })))
}

/// Paper sizes KiCad accepts in a `(paper …)` node, with their landscape
/// dimensions in mm — reported back so the caller can sanity-check the layout
/// against the frame instead of discovering the overflow at print time.
const PAPER_SIZES: &[(&str, f64, f64)] = &[
    ("A0", 1189.0, 841.0),
    ("A1", 841.0, 594.0),
    ("A2", 594.0, 420.0),
    ("A3", 420.0, 297.0),
    ("A4", 297.0, 210.0),
    ("A5", 210.0, 148.0),
    ("A", 279.4, 215.9),
    ("B", 431.8, 279.4),
    ("C", 558.8, 431.8),
    ("D", 863.6, 558.8),
    ("E", 1117.6, 863.6),
    ("USLetter", 279.4, 215.9),
    ("USLegal", 355.6, 215.9),
    ("USLedger", 431.8, 279.4),
];

/// Landscape width and height of a named paper size, or the `invalid_argument`
/// refusal naming every size that would have worked.
fn paper_dimensions(size: &str) -> Result<(f64, f64), CallToolResult> {
    match PAPER_SIZES.iter().find(|(n, _, _)| *n == size) {
        Some(&(_, w, h)) => Ok((w, h)),
        None => {
            let valid = PAPER_SIZES
                .iter()
                .map(|(n, _, _)| *n)
                .collect::<Vec<_>>()
                .join(", ");
            Err(CallToolResult::error_kind(
                crate::mcp::error::ToolErrorKind::InvalidArgument {
                    field: "size".into(),
                    reason: format!("unknown paper size '{size}'; valid: {valid}"),
                },
                format!("Argument 'size' is invalid: unknown paper size '{size}'; valid: {valid}"),
            ))
        }
    }
}

async fn handle_set_page(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let size = match require_str(args, "size") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let portrait = args["portrait"].as_bool().unwrap_or(false);

    let dims = match paper_dimensions(&size) {
        Ok(dims) => dims,
        Err(e) => return Ok(e),
    };
    let (w, h) = if portrait { (dims.1, dims.0) } else { dims };

    let node = if portrait {
        format!("(paper \"{size}\" portrait)")
    } else {
        format!("(paper \"{size}\")")
    };

    let mut content = read_consistent(&sch_path)?;
    let expected = content.clone();
    match content.find("(paper ") {
        Some(start) => {
            let end = start
                + content[start..]
                    .find(')')
                    .map(|p| p + 1)
                    .unwrap_or(content.len() - start);
            content.replace_range(start..end, &node);
        }
        None => {
            // A freshly created blank sheet has no paper node; it belongs in
            // the header, right after the uuid.
            let anchor = content
                .find("(uuid ")
                .and_then(|p| content[p..].find(')').map(|q| p + q + 1))
                .unwrap_or_else(|| content.find('\n').map(|p| p + 1).unwrap_or(0));
            content.insert_str(anchor, &format!("\n  {node}"));
        }
    }
    write_atomic_if_unchanged(&sch_path, &expected, &content)?;

    Ok(CallToolResult::json(&json!({
        "size": size,
        "portrait": portrait,
        "width_mm": w,
        "height_mm": h
    })))
}

async fn handle_add_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let lib_id = match require_str(args, "lib_id") {
        Ok(s) => s.to_string(),
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
    let rotation = opt_f64(args, "rotation").unwrap_or(0.0);
    let reference = opt_str(args, "reference");
    let value = opt_str(args, "value");
    let unit = opt_f64(args, "unit").unwrap_or(1.0) as u32;
    let ref_str = reference.unwrap_or("?");

    // Load via konnect-schematic-editor
    let mut sch = cse::Schematic::load(&sch_path)?;

    // KiCAD's netlister resolves instances against the ROOT sheet's uuid and
    // the project's name, and silently forms no wire-only nets for symbols
    // whose path doesn't resolve. On a child sheet both differ from this
    // file's own stem and uuid, which is what left hierarchical designs
    // unannotated (#204).
    let context = match crate::tools::sheet_instance_context(&sch_path, &mut sch) {
        Ok(context) => context,
        Err(error) => return Ok(error.into_tool_result()),
    };
    if let Err(error) = crate::tools::validate_sheet_instance_state(&sch_path, &sch, &context) {
        return Ok(error.into_tool_result());
    }
    let source = match crate::tools::library::KiCadSymbolSource::for_file(&sch_path) {
        Ok(source) => source,
        Err(error) => return Ok(error.into_tool_result()),
    };

    let uuid = match place_one_component(
        &mut sch,
        &context.instance_paths,
        &context.project_name,
        &lib_id,
        x,
        y,
        rotation,
        ref_str,
        value,
        unit,
        &source,
    ) {
        Ok(uuid) => uuid,
        Err(e) => return Ok(e),
    };

    let (expected_x, expected_y) = snap_point(x, y, 1.27);
    let placement = ComponentTargetUnit::placement(
        &uuid, &context, &lib_id, expected_x, expected_y, rotation, ref_str, value, unit,
    );
    sch.overwrite()?;

    // A pin landing mid-segment on an existing wire needs a junction dot, or
    // KiCad's netlister treats it as unconnected. Runs after the write because
    // it re-reads the saved file; `place_one_component` stays pure so the batch
    // path can do one junction pass for the whole batch instead of one per part.
    let junctions = crate::tools::add_pin_midwire_junctions(&sch_path, ref_str)?;
    let committed = cse::Schematic::load(&sch_path)?;
    let mut result = match placed_component_readback(&sch_path, &committed, &placement, &context) {
        Ok(result) => result,
        Err(error) => return Ok(error),
    };
    result["junctions_added"] = json!(junctions
        .iter()
        .map(|(x, y)| json!({ "x": x, "y": y }))
        .collect::<Vec<_>>());

    Ok(CallToolResult::json(&result))
}

/// Place one symbol into `sch`: embeds the lib_symbols definition, validates
/// the unit, and adds the positioned instance. Does not write the file --
/// callers own the read/write cycle (single-add and batch-add alike).
#[allow(clippy::too_many_arguments)]
pub(crate) fn place_one_component(
    sch: &mut cse::Schematic,
    instance_paths: &[String],
    project_name: &str,
    lib_id: &str,
    x: f64,
    y: f64,
    rotation: f64,
    reference: &str,
    value: Option<&str>,
    unit: u32,
    src: &dyn cse::library::SymbolLibrarySource,
) -> Result<String, CallToolResult> {
    // Snap to 1.27mm grid
    let (x, y) = snap_point(x, y, 1.27);
    let val_str = value.unwrap_or(lib_id.split(':').next_back().unwrap_or("?"));

    // Embed the library symbol definition
    if !cse::library::ensure_lib_symbol(sch, lib_id, src) {
        return Err(crate::tools::lib_symbol_not_found_error(lib_id, src));
    }
    let metadata = cse::library::symbol_metadata(sch, lib_id);

    // Validate the unit against the resolved symbol BEFORE writing anything:
    // eeschema silently renders an out-of-range unit as unit 1 and the
    // netlister mis-assigns its pins (#35).
    let unit_count = cse::library::symbol_unit_count(lib_id, src).unwrap_or(1);
    if unit < 1 || unit > unit_count {
        return Err(CallToolResult::error(format!(
            "Invalid unit {} for '{}': the symbol has {} unit(s) (valid: 1..={}).",
            unit, lib_id, unit_count, unit_count
        )));
    }

    // Build the Symbol struct
    let mut sym = cse::Symbol::new(lib_id, x, y);
    sym.at.rotation = Some(rotation);
    sym.unit = unit;

    // Reference and Value go where the library anchors them, carried through
    // the placement transform so they follow a rotated body (#101);
    // Footprint/Datasheet/Description stay hidden at the origin. KiCad copies
    // Datasheet and Description from the resolved library symbol onto every
    // placed instance; without those copies its BOM exporter leaves both
    // columns blank even though lib_symbols still carries the values (#226).
    // Power symbols get their Reference hidden too, matching eeschema: a
    // #PWR designator is never shown on the sheet.
    let hide_reference = lib_id.starts_with("power:") || reference.starts_with("#PWR");
    let anchors = cse::library::field_anchors(sch, lib_id);
    let t = konnect_sexp::geometry::PinTransform {
        comp_x: x,
        comp_y: y,
        rotation_deg: rotation,
        mirror_x: false,
        mirror_y: false,
    };
    let (ref_x, ref_y, ref_rot) =
        crate::tools::field_at(anchors.reference_at, crate::tools::FALLBACK_REFERENCE_AT, t);
    let (val_x, val_y, val_rot) =
        crate::tools::field_at(anchors.value_at, crate::tools::FALLBACK_VALUE_AT, t);
    let positioned = crate::tools::positioned_property;
    let centred = cse::library::FieldJustify::default();
    sym.properties.push(positioned(
        "Reference",
        reference,
        ref_x,
        ref_y,
        ref_rot,
        hide_reference,
        anchors.reference_justify,
    ));
    sym.properties.push(positioned(
        "Value",
        val_str,
        val_x,
        val_y,
        val_rot,
        false,
        anchors.value_justify,
    ));
    sym.properties
        .push(positioned("Footprint", "", x, y, 0.0, true, centred));
    sym.properties.push(positioned(
        "Datasheet",
        &metadata.datasheet,
        x,
        y,
        0.0,
        true,
        centred,
    ));
    sym.properties.push(positioned(
        "Description",
        &metadata.description,
        x,
        y,
        0.0,
        true,
        centred,
    ));

    // Instance entry, keyed to the root sheet UUID like eeschema writes it:
    // (instances (project "<name>" (path "/<root-uuid>" (reference ...) (unit 1))))
    for instance_path in instance_paths {
        sym.set_instance_path(project_name, instance_path, reference, unit);
    }

    let uuid = sym.uuid.clone();
    sch.add_symbol(sym);

    Ok(uuid)
}

#[derive(Debug, Clone)]
pub(crate) struct ComponentTargetUnit {
    uuid: String,
    unit: u32,
    fields: BTreeMap<String, String>,
    lib_id: String,
    x: f64,
    y: f64,
    rotation: f64,
    instances: Vec<(String, String)>,
}

impl ComponentTargetUnit {
    /// Bind placement intent independently of the model produced by the writer.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn placement(
        uuid: &str,
        context: &crate::tools::SheetInstanceContext,
        lib_id: &str,
        x: f64,
        y: f64,
        rotation: f64,
        reference: &str,
        value: Option<&str>,
        unit: u32,
    ) -> Self {
        let mut instances = context
            .instance_paths
            .iter()
            .map(|path| (context.project_name.clone(), path.clone()))
            .collect::<Vec<_>>();
        instances.sort();
        Self {
            uuid: uuid.to_owned(),
            unit,
            lib_id: lib_id.to_owned(),
            x,
            y,
            rotation,
            fields: BTreeMap::from([
                ("Reference".to_owned(), reference.to_owned()),
                (
                    "Value".to_owned(),
                    value
                        .unwrap_or_else(|| lib_id.rsplit(':').next().unwrap_or(lib_id))
                        .to_owned(),
                ),
            ]),
            instances,
        }
    }
}

/// Inspect every record before sorting: duplicate identities and conflicting
/// project/unit records must not be collapsed into a plausible first answer.
fn checked_instance_paths(
    path: &std::path::Path,
    symbol: &cse::Symbol,
) -> Result<Vec<(String, String)>, ComponentDeleteTargetError> {
    let mut identities = BTreeSet::new();
    let mut projects = BTreeSet::new();
    for instance in symbol.instances() {
        let (Some(project), Some(instance_path), Some(reference), Some(unit)) = (
            instance.project,
            instance.path,
            instance.reference,
            instance.unit,
        ) else {
            return Err(ComponentDeleteTargetError::stale(
                path,
                format!(
                    "component UUID {} has incomplete instance metadata",
                    symbol.uuid
                ),
            ));
        };
        if symbol.reference() != Some(reference.as_str()) {
            return Err(ComponentDeleteTargetError::stale(
                path,
                format!(
                    "component UUID {} has a conflicting instance reference",
                    symbol.uuid
                ),
            ));
        }
        projects.insert(project.clone());
        if unit != symbol.unit || !identities.insert((project, instance_path)) || projects.len() > 1
        {
            return Err(ComponentDeleteTargetError::Ambiguous {
                target: format!("component UUID {} instance records", symbol.uuid),
                candidates: symbol
                    .instances()
                    .iter()
                    .map(|entry| format!("{entry:?}"))
                    .collect(),
            });
        }
    }
    Ok(identities.into_iter().collect())
}

fn verify_component_expectations(
    path: &std::path::Path,
    observed: &serde_json::Value,
    expected: &[ComponentTargetUnit],
) -> Result<(), CallToolResult> {
    for target in expected {
        let unit = observed["units"]
            .as_array()
            .and_then(|units| {
                units
                    .iter()
                    .find(|unit| unit["uuid"].as_str() == Some(&target.uuid))
            })
            .ok_or_else(|| {
                ComponentDeleteTargetError::stale(path, "bound unit missing from readback")
                    .into_result()
            })?;
        // Separate comparisons keep each identity/intent obligation explicit.
        for (field, matches) in [
            (
                "unit",
                unit["unit"].as_u64() == Some(u64::from(target.unit)),
            ),
            ("lib_id", unit["lib_id"].as_str() == Some(&target.lib_id)),
            (
                "x",
                unit["x"]
                    .as_f64()
                    .is_some_and(|v| (v - target.x).abs() < 1e-6),
            ),
            (
                "y",
                unit["y"]
                    .as_f64()
                    .is_some_and(|v| (v - target.y).abs() < 1e-6),
            ),
            (
                "rotation",
                unit["rotation"]
                    .as_f64()
                    .is_some_and(|v| (v - target.rotation).abs() < 1e-6),
            ),
            (
                "instance_paths",
                unit["instance_paths"] == json!(target.instances),
            ),
        ] {
            if !matches {
                return Err(ComponentDeleteTargetError::stale(
                    path,
                    format!(
                        "post-write {field} differs from bound intent for UUID {}",
                        target.uuid
                    ),
                )
                .into_result());
            }
        }
        for (field, value) in &target.fields {
            if unit["fields"][field].as_str() != Some(value) {
                return Err(ComponentDeleteTargetError::stale(
                    path,
                    format!(
                        "post-write property {field} differs from bound intent for UUID {}",
                        target.uuid
                    ),
                )
                .into_result());
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ComponentTarget {
    reference: String,
    units: Vec<ComponentTargetUnit>,
}

impl ComponentTarget {
    fn with_fields(&self, fields: &BTreeMap<String, String>) -> Self {
        let mut expected = self.clone();
        if let Some(reference) = fields.get("Reference") {
            expected.reference = reference.clone();
        }
        for unit in &mut expected.units {
            unit.fields.extend(fields.clone());
        }
        expected
    }

    fn uuids(&self) -> Vec<String> {
        self.units.iter().map(|unit| unit.uuid.clone()).collect()
    }

    fn item_ids(&self) -> Result<Vec<ItemId>, ComponentDeleteTargetError> {
        self.units
            .iter()
            .map(|unit| {
                ItemId::new(unit.uuid.clone()).map_err(|error| ComponentDeleteTargetError::Stale {
                    target: format!("component {}", self.reference),
                    reason: error.to_string(),
                })
            })
            .collect()
    }
}

fn component_target_from_source(
    path: &std::path::Path,
    content: &str,
    reference: &str,
) -> Result<ComponentTarget, ComponentDeleteTargetError> {
    let tree =
        parse_sexp(content).map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
    let indexed = indexed_uuid_items(path, content)?;
    let instances = extract_symbol_instances(&tree)
        .into_iter()
        .filter(|instance| instance.reference == reference)
        .collect::<Vec<_>>();
    if instances.is_empty() {
        return Err(ComponentDeleteTargetError::stale(
            path,
            format!("component {reference} is not present"),
        ));
    }

    let mut by_unit: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut units = Vec::new();
    for instance in instances {
        let uuid = instance.uuid.ok_or_else(|| {
            ComponentDeleteTargetError::stale(
                path,
                format!("component {reference} unit {} has no UUID", instance.unit),
            )
        })?;
        let Some(item) = indexed.get(&uuid) else {
            return Err(ComponentDeleteTargetError::stale(
                path,
                format!("component {reference} UUID {uuid} is not a top-level item"),
            ));
        };
        if item.kind != "symbol" {
            return Err(ComponentDeleteTargetError::stale(
                path,
                format!("component {reference} UUID {uuid} identifies {}", item.kind),
            ));
        }
        let node = parse_sexp(&item.source)
            .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
        let mut fields = BTreeMap::new();
        for property in node.find_all("property") {
            let Some(name) = property.get(1).and_then(|value| value.as_str()) else {
                continue;
            };
            let value = property
                .get(2)
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if fields.insert(name.to_owned(), value.to_owned()).is_some() {
                return Err(ComponentDeleteTargetError::Ambiguous {
                    target: format!("component {reference} unit {} field {name}", instance.unit),
                    candidates: vec![uuid.clone(), uuid],
                });
            }
        }
        for instances in node.find_all("instances") {
            for project in instances.find_all("project") {
                for path_node in project.find_all("path") {
                    let Some(instance_reference) = path_node.find_str("reference") else {
                        return Err(ComponentDeleteTargetError::stale(
                            path,
                            format!(
                                "component {reference} UUID {uuid} has an instance path without a reference"
                            ),
                        ));
                    };
                    if instance_reference != reference {
                        return Err(ComponentDeleteTargetError::stale(
                            path,
                            format!(
                                "component {reference} UUID {uuid} has an instance path naming {instance_reference}"
                            ),
                        ));
                    }
                }
            }
        }
        by_unit.entry(instance.unit).or_default().push(uuid.clone());
        let symbol = cse::sexp::parser::parse(&item.source)
            .and_then(|node| cse::Symbol::from_sexp(&node))
            .map_err(|error| ComponentDeleteTargetError::stale(path, error.to_string()))?;
        let instance_paths = checked_instance_paths(path, &symbol)?;
        units.push(ComponentTargetUnit {
            uuid,
            unit: instance.unit,
            fields,
            lib_id: instance.lib_id,
            x: instance.x,
            y: instance.y,
            rotation: instance.rotation,
            instances: instance_paths,
        });
    }
    if let Some((unit, uuids)) = by_unit.iter().find(|(_, uuids)| uuids.len() > 1) {
        return Err(ComponentDeleteTargetError::Ambiguous {
            target: format!("component {reference} unit {unit}"),
            candidates: uuids.clone(),
        });
    }
    if units
        .iter()
        .any(|unit| unit.instances != units[0].instances)
    {
        return Err(ComponentDeleteTargetError::Ambiguous {
            target: format!("component {reference} hierarchy instances"),
            candidates: units
                .iter()
                .map(|unit| format!("{}: {:?}", unit.uuid, unit.instances))
                .collect(),
        });
    }
    units.sort_by(|left, right| {
        left.unit
            .cmp(&right.unit)
            .then_with(|| left.uuid.cmp(&right.uuid))
    });
    Ok(ComponentTarget {
        reference: reference.to_owned(),
        units,
    })
}

fn component_mutation_readback_from_schematic(
    path: &std::path::Path,
    committed: &cse::Schematic,
    expected_uuids: &[String],
    expected_reference: Option<&str>,
) -> Result<serde_json::Value, CallToolResult> {
    if !super::same_schematic_document(path, committed.filepath()) {
        return Err(ComponentDeleteTargetError::stale(
            path,
            "component mutation readback came from a different schematic",
        )
        .into_result());
    }
    if expected_uuids.is_empty() {
        return Err(
            ComponentDeleteTargetError::stale(path, "no component UUIDs were bound").into_result(),
        );
    }
    let expected = expected_uuids.iter().cloned().collect::<BTreeSet<_>>();
    if expected.len() != expected_uuids.len() {
        return Err(ComponentDeleteTargetError::Ambiguous {
            target: path.display().to_string(),
            candidates: expected_uuids.to_vec(),
        }
        .into_result());
    }
    let committed_source = committed.to_source();
    let indexed =
        indexed_uuid_items(path, &committed_source).map_err(|error| error.into_result())?;
    for uuid in &expected {
        if indexed.get(uuid).is_none_or(|item| item.kind != "symbol") {
            return Err(ComponentDeleteTargetError::stale(
                path,
                format!("bound UUID {uuid} no longer identifies one top-level symbol"),
            )
            .into_result());
        }
    }

    let mut observed = Vec::new();
    for uuid in &expected {
        let matches = committed
            .symbols
            .iter()
            .filter(|symbol| symbol.uuid == *uuid)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Err(ComponentDeleteTargetError::stale(
                path,
                format!("component UUID {uuid} is absent from post-write readback"),
            )
            .into_result());
        }
        if matches.len() > 1 {
            return Err(ComponentDeleteTargetError::Ambiguous {
                target: format!("schematic symbol UUID {uuid}"),
                candidates: matches.iter().map(|symbol| symbol.uuid.clone()).collect(),
            }
            .into_result());
        }
        observed.push(matches[0]);
    }

    let references = observed
        .iter()
        .filter_map(|symbol| symbol.reference().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    if observed.iter().any(|symbol| symbol.reference().is_none()) {
        return Err(ComponentDeleteTargetError::stale(
            path,
            "a bound component UUID has no observed Reference field",
        )
        .into_result());
    }
    if references.len() != 1 {
        return Err(ComponentDeleteTargetError::Ambiguous {
            target: path.display().to_string(),
            candidates: references.iter().cloned().collect(),
        }
        .into_result());
    }
    let reference = references.iter().next().expect("one reference").clone();
    if expected_reference.is_some_and(|expected| expected != reference) {
        return Err(ComponentDeleteTargetError::stale(
            path,
            format!(
                "bound component UUIDs resolve to {reference}, not {}",
                expected_reference.unwrap_or_default()
            ),
        )
        .into_result());
    }

    observed.sort_by(|left, right| {
        left.unit
            .cmp(&right.unit)
            .then_with(|| left.uuid.cmp(&right.uuid))
    });
    let mut units = Vec::new();
    let mut component_instances = None;
    for symbol in &observed {
        let mut fields = BTreeMap::new();
        for property in &symbol.properties {
            if fields
                .insert(property.name.clone(), property.value.clone())
                .is_some()
            {
                return Err(ComponentDeleteTargetError::Ambiguous {
                    target: format!(
                        "component {reference} unit {} field {}",
                        symbol.unit, property.name
                    ),
                    candidates: vec![symbol.uuid.clone(), symbol.uuid.clone()],
                }
                .into_result());
            }
        }
        let instance_paths =
            checked_instance_paths(path, symbol).map_err(|error| error.into_result())?;
        if component_instances
            .as_ref()
            .is_some_and(|expected| expected != &instance_paths)
        {
            return Err(ComponentDeleteTargetError::Ambiguous {
                target: format!("component {reference} hierarchy instances"),
                candidates: observed
                    .iter()
                    .map(|symbol| format!("{}: {:?}", symbol.uuid, symbol.instance_paths()))
                    .collect(),
            }
            .into_result());
        }
        component_instances = Some(instance_paths.clone());
        let mut instance_references = Vec::new();
        for instances in symbol
            .raw_sub_nodes
            .iter()
            .filter(|node| node.tag() == Some("instances"))
        {
            for project in instances.find_all("project") {
                for path_node in project.find_all("path") {
                    let Some(instance_reference) = path_node.get_value("reference") else {
                        return Err(ComponentDeleteTargetError::stale(
                            path,
                            format!(
                                "component UUID {} has an instance path without a reference",
                                symbol.uuid
                            ),
                        )
                        .into_result());
                    };
                    instance_references.push(instance_reference.to_owned());
                }
            }
        }
        instance_references.sort();
        instance_references.dedup();
        if let Some(stale_reference) = instance_references
            .iter()
            .find(|instance_reference| **instance_reference != reference)
        {
            return Err(ComponentDeleteTargetError::stale(
                path,
                format!(
                    "component UUID {} renders as {reference} but an instance path still names {stale_reference}",
                    symbol.uuid
                ),
            )
            .into_result());
        }
        units.push(json!({
            "uuid": symbol.uuid,
            "unit": symbol.unit,
            "x": symbol.at.x,
            "y": symbol.at.y,
            "rotation": symbol.at.rotation.unwrap_or(0.0),
            "lib_id": symbol.lib_id,
            "fields": fields,
            "instance_paths": instance_paths,
            "instance_references": instance_references
        }));
    }
    let anchor = observed[0];
    let fields = units[0]["fields"].clone();
    Ok(json!({
        "schematic": committed.filepath().display().to_string(),
        "reference": reference,
        "value": anchor.value_str().unwrap_or(""),
        "footprint": anchor.footprint().unwrap_or(""),
        "datasheet": anchor.datasheet().unwrap_or(""),
        "lib_id": anchor.lib_id,
        "uuid": anchor.uuid,
        "x": anchor.at.x,
        "y": anchor.at.y,
        "rotation": anchor.at.rotation.unwrap_or(0.0),
        "unit_count": units.len(),
        "units": units,
        "fields": fields
    }))
}

fn load_component_mutation_readback(
    path: &std::path::Path,
    expected: &ComponentTarget,
) -> anyhow::Result<Result<serde_json::Value, CallToolResult>> {
    let committed = cse::Schematic::load(path)?;
    Ok(verified_component_readback(path, &committed, expected))
}

fn verified_component_readback(
    path: &std::path::Path,
    committed: &cse::Schematic,
    expected: &ComponentTarget,
) -> Result<serde_json::Value, CallToolResult> {
    let observed = component_mutation_readback_from_schematic(
        path,
        committed,
        &expected.uuids(),
        Some(&expected.reference),
    )?;
    verify_component_expectations(path, &observed, &expected.units)?;
    Ok(observed)
}

fn copy_component_observation(result: &mut serde_json::Value, observed: &serde_json::Value) {
    for key in [
        "schematic",
        "reference",
        "value",
        "footprint",
        "datasheet",
        "lib_id",
        "uuid",
        "x",
        "y",
        "rotation",
        "unit_count",
        "units",
        "fields",
    ] {
        result[key] = observed[key].clone();
    }
}

fn verify_observed_field(
    path: &std::path::Path,
    observed: &serde_json::Value,
    field: &str,
    expected: &str,
) -> Result<(), CallToolResult> {
    let mismatches = observed["units"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|unit| unit["fields"][field].as_str() != Some(expected))
        .map(|unit| unit["uuid"].as_str().unwrap_or("unknown").to_owned())
        .collect::<Vec<_>>();
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(ComponentDeleteTargetError::stale(
            path,
            format!(
                "post-write readback did not observe {field}={expected:?} on UUIDs {}",
                mismatches.join(", ")
            ),
        )
        .into_result())
    }
}

/// Build a placement response only from the committed schematic that was read
/// back after the write. The UUID is the mutation's stable identity; requested
/// coordinates, fields, and hierarchy paths are never echoed as proof.
pub(crate) fn placed_component_readback(
    sch_path: &std::path::Path,
    committed: &cse::Schematic,
    expected: &ComponentTargetUnit,
    context: &crate::tools::SheetInstanceContext,
) -> Result<serde_json::Value, CallToolResult> {
    if !super::same_schematic_document(sch_path, committed.filepath()) {
        return Err(crate::tools::SchematicTargetError::StaleTarget {
            target: sch_path.to_path_buf(),
            reason: "placement readback came from a different schematic".to_string(),
        }
        .into_tool_result());
    }
    let uuid = &expected.uuid;
    let mut result = component_mutation_readback_from_schematic(
        sch_path,
        committed,
        &[uuid.to_owned()],
        expected.fields.get("Reference").map(String::as_str),
    )?;
    verify_component_expectations(sch_path, &result, std::slice::from_ref(expected))?;
    if let Err(error) = crate::tools::validate_sheet_instance_state(sch_path, committed, context) {
        return Err(error.into_tool_result());
    }
    let symbol = committed
        .symbols
        .iter()
        .find(|symbol| symbol.uuid == *uuid)
        .expect("shared readback proved this UUID");
    let mut observed_instances = symbol.instance_paths();
    observed_instances.sort();
    let Some((project, _)) = observed_instances.first() else {
        return Err(ComponentDeleteTargetError::stale(
            committed.filepath(),
            format!("placed symbol UUID '{uuid}' has no project in post-write readback"),
        )
        .into_result());
    };
    let project = project.clone();
    let mut instance_paths = observed_instances
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    instance_paths.sort();

    result["added"] = result["lib_id"].clone();
    result["unit"] = json!(symbol.unit);
    result["project"] = json!(project);
    result["instance_paths"] = json!(instance_paths);
    Ok(result)
}

async fn handle_delete_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let plan = match plan_component_deletion(&sch_path, &content, &reference) {
        Ok(plan) => plan,
        Err(error) => return Ok(error.into_result()),
    };
    let outcome = match commit_component_deletion(&sch_path, plan)? {
        Ok(outcome) => outcome,
        Err(refusal) => return Ok(refusal),
    };
    let unit_uuids = outcome
        .units_by_reference
        .get(&reference)
        .cloned()
        .unwrap_or_default();

    Ok(CallToolResult::json(&json!({
        "deleted": reference,
        "deleted_units": unit_uuids.len(),
        "deleted_unit_uuids": unit_uuids,
        "removed_no_connects_count": outcome.marker_uuids.len(),
        "removed_no_connect_uuids": outcome.marker_uuids,
        "junctions_added_count": outcome.added_junctions.len(),
        "junctions_added_uuids": outcome.added_junctions,
        "junctions_pruned_count": outcome.pruned_junctions.len(),
        "junctions_pruned_uuids": outcome.pruned_junctions
    })))
}

#[derive(Debug, Clone)]
pub(crate) struct IndexedSchematicItem {
    pub(crate) kind: String,
    source: String,
}

pub(crate) struct ComponentDeletePlan {
    command: SchematicCommand,
    before_items: BTreeMap<String, IndexedSchematicItem>,
    unit_uuids: Vec<String>,
    units_by_reference: BTreeMap<String, Vec<String>>,
    marker_uuids: Vec<String>,
    item_uuids: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct ComponentDeleteOutcome {
    pub(crate) units_by_reference: BTreeMap<String, Vec<String>>,
    pub(crate) marker_uuids: Vec<String>,
    pub(crate) item_uuids: Vec<String>,
    pub(crate) added_junctions: Vec<String>,
    pub(crate) pruned_junctions: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum ComponentDeleteTargetError {
    Ambiguous {
        target: String,
        candidates: Vec<String>,
    },
    Stale {
        target: String,
        reason: String,
    },
}

impl ComponentDeleteTargetError {
    fn stale(path: &std::path::Path, reason: impl Into<String>) -> Self {
        Self::Stale {
            target: path.display().to_string(),
            reason: reason.into(),
        }
    }

    fn from_sexp(path: &std::path::Path, error: SexpError) -> Self {
        Self::stale(path, error.to_string())
    }

    pub(crate) fn into_result(self) -> CallToolResult {
        match self {
            Self::Ambiguous { target, candidates } => {
                let reason = format!(
                    "more than one schematic item identifies the target: {}",
                    candidates.join(", ")
                );
                CallToolResult::error_kind(
                    ToolErrorKind::AmbiguousTarget {
                        target: target.clone(),
                        candidates,
                    },
                    format!("cannot safely resolve {target}: {reason}"),
                )
            }
            Self::Stale { target, reason } => CallToolResult::error_kind(
                ToolErrorKind::StaleTarget {
                    target: target.clone(),
                    reason: reason.clone(),
                },
                format!("cannot safely resolve {target}: {reason}"),
            ),
        }
    }
}

pub(crate) fn indexed_uuid_items(
    path: &std::path::Path,
    content: &str,
) -> Result<BTreeMap<String, IndexedSchematicItem>, ComponentDeleteTargetError> {
    let ranges = find_direct_child_blocks(content, "kicad_sch");
    if ranges.is_empty() {
        return Err(ComponentDeleteTargetError::stale(
            path,
            "the kicad_sch root is missing or malformed",
        ));
    }
    let mut items = BTreeMap::new();
    for (start, end) in ranges {
        let node = parse_sexp(&content[start..end])
            .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
        let Some(uuid) = node.find_str("uuid") else {
            continue;
        };
        let item = IndexedSchematicItem {
            kind: node.head().unwrap_or("unknown").to_owned(),
            source: content[start..end].to_owned(),
        };
        if let Some(previous) = items.insert(uuid.to_owned(), item) {
            return Err(ComponentDeleteTargetError::Ambiguous {
                target: format!("schematic UUID {uuid}"),
                candidates: vec![previous.kind, node.head().unwrap_or("unknown").to_owned()],
            });
        }
    }
    Ok(items)
}

fn dedup_points(points: impl IntoIterator<Item = (f64, f64)>) -> Vec<(f64, f64)> {
    let mut unique = Vec::new();
    for point in points {
        if !unique
            .iter()
            .any(|&(x, y)| konnect_sexp::geometry::points_coincident(x, y, point.0, point.1, 0.01))
        {
            unique.push(point);
        }
    }
    unique
}

fn plan_component_deletion(
    path: &std::path::Path,
    content: &str,
    reference: &str,
) -> Result<ComponentDeletePlan, ComponentDeleteTargetError> {
    plan_component_deletions(path, content, &[reference.to_owned()])
}

pub(crate) fn plan_component_deletions(
    path: &std::path::Path,
    content: &str,
    references: &[String],
) -> Result<ComponentDeletePlan, ComponentDeleteTargetError> {
    plan_component_and_item_deletions(path, content, references, &[])
}

pub(crate) fn plan_component_and_item_deletions(
    path: &std::path::Path,
    content: &str,
    references: &[String],
    item_uuids: &[String],
) -> Result<ComponentDeletePlan, ComponentDeleteTargetError> {
    let references = references.iter().cloned().collect::<BTreeSet<_>>();
    let mut item_uuids = item_uuids.to_vec();
    item_uuids.sort();
    item_uuids.dedup();
    if references.is_empty() && item_uuids.is_empty() {
        return Err(ComponentDeleteTargetError::stale(
            path,
            "no schematic items were selected",
        ));
    }
    let reference_label = if references.is_empty() {
        "selected items".to_owned()
    } else {
        references.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    let tree =
        parse_sexp(content).map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
    let before_items = indexed_uuid_items(path, content)?;
    for uuid in &item_uuids {
        if !before_items.contains_key(uuid) {
            return Err(ComponentDeleteTargetError::stale(
                path,
                format!("schematic item UUID {uuid} is not present"),
            ));
        }
    }
    let instances = extract_symbol_instances(&tree);
    let selected = instances
        .iter()
        .filter(|instance| references.contains(&instance.reference))
        .collect::<Vec<_>>();
    let found = selected
        .iter()
        .map(|instance| instance.reference.clone())
        .collect::<BTreeSet<_>>();
    if let Some(missing) = references.difference(&found).next() {
        return Err(ComponentDeleteTargetError::stale(
            path,
            format!("component {missing} is not present"),
        ));
    }

    let mut by_unit: BTreeMap<(String, u32), Vec<String>> = BTreeMap::new();
    let mut units_by_reference: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unit_uuids = Vec::new();
    for instance in &selected {
        let uuid = instance.uuid.clone().ok_or_else(|| {
            ComponentDeleteTargetError::stale(
                path,
                format!(
                    "component {} unit {} has no UUID",
                    instance.reference, instance.unit
                ),
            )
        })?;
        if before_items
            .get(&uuid)
            .is_none_or(|item| item.kind != "symbol")
        {
            return Err(ComponentDeleteTargetError::stale(
                path,
                format!("UUID {uuid} no longer identifies a top-level symbol"),
            ));
        }
        by_unit
            .entry((instance.reference.clone(), instance.unit))
            .or_default()
            .push(uuid.clone());
        units_by_reference
            .entry(instance.reference.clone())
            .or_default()
            .push(uuid.clone());
        unit_uuids.push(uuid);
    }
    if let Some(((reference, unit), uuids)) = by_unit.iter().find(|(_, uuids)| uuids.len() > 1) {
        return Err(ComponentDeleteTargetError::Ambiguous {
            target: format!("component {reference} unit {unit}"),
            candidates: uuids.clone(),
        });
    }
    for uuids in units_by_reference.values_mut() {
        uuids.sort();
        uuids.dedup();
    }
    unit_uuids.sort();
    unit_uuids.dedup();
    if unit_uuids.len() != selected.len() {
        return Err(ComponentDeleteTargetError::Ambiguous {
            target: format!("components {reference_label}"),
            candidates: unit_uuids,
        });
    }

    // Require every placed symbol's library definition to resolve before
    // deciding marker ownership or junction validity. Unknown pins are stale
    // state, not evidence that no pin remains at a coordinate.
    let grouped = if selected.is_empty() {
        Vec::new()
    } else {
        let grouped = crate::tools::placed_pins_by_reference(&tree);
        if grouped.len() != instances.len() {
            return Err(ComponentDeleteTargetError::stale(
                path,
                "one or more placed symbols have unresolved library pin geometry",
            ));
        }
        grouped
    };
    let selected_ids = unit_uuids.iter().cloned().collect::<BTreeSet<_>>();
    let mut affected = Vec::new();
    let mut remaining = Vec::new();
    for (instance, pins) in grouped {
        let points = pins
            .into_iter()
            .map(|(pin, transform)| pin_endpoint(&pin, transform));
        if instance
            .uuid
            .as_ref()
            .is_some_and(|uuid| selected_ids.contains(uuid))
        {
            affected.extend(points);
        } else {
            remaining.extend(points);
        }
    }
    let affected = dedup_points(affected);
    let remaining = dedup_points(remaining);

    let mut marker_uuids = Vec::new();
    for (start, end) in find_direct_child_blocks(content, "kicad_sch") {
        let node = parse_sexp(&content[start..end])
            .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
        if node.head() != Some("no_connect") {
            continue;
        }
        let Some((x, y, _)) = konnect_sexp::schematic::parse_at(&node) else {
            continue;
        };
        let attached = affected
            .iter()
            .any(|&(px, py)| konnect_sexp::geometry::points_coincident(x, y, px, py, 0.01));
        let still_owned = remaining
            .iter()
            .any(|&(px, py)| konnect_sexp::geometry::points_coincident(x, y, px, py, 0.01));
        if attached && !still_owned {
            let uuid = node.find_str("uuid").ok_or_else(|| {
                ComponentDeleteTargetError::stale(
                    path,
                    format!("attached no-connect at ({x}, {y}) has no UUID"),
                )
            })?;
            marker_uuids.push(uuid.to_owned());
        }
    }
    marker_uuids.sort();
    marker_uuids.dedup();

    let initial_uuid_set = unit_uuids
        .iter()
        .chain(&marker_uuids)
        .chain(&item_uuids)
        .cloned()
        .collect::<BTreeSet<_>>();
    let initial_ids = initial_uuid_set
        .into_iter()
        .map(ItemId::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
    let initial = SchematicCommand::delete_items(content, initial_ids, "prepare component delete")
        .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
    let candidate = prepare_command(path, content, &initial)
        .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?
        .0;
    let (reconciled, _, _) = crate::tools::sch_wiring::reconcile_junctions_at(candidate, &affected);

    let after_items = indexed_uuid_items(path, &reconciled)?;
    let before_ids = before_items.keys().cloned().collect::<BTreeSet<_>>();
    let after_ids = after_items.keys().cloned().collect::<BTreeSet<_>>();
    let removed = before_ids
        .difference(&after_ids)
        .cloned()
        .collect::<Vec<_>>();
    let added = after_ids
        .difference(&before_ids)
        .cloned()
        .collect::<Vec<_>>();
    let modified = before_ids
        .intersection(&after_ids)
        .filter(|uuid| before_items[*uuid].source != after_items[*uuid].source)
        .cloned()
        .collect::<Vec<_>>();

    let mut changes = Vec::new();
    if !removed.is_empty() {
        let command = SchematicCommand::delete_items(
            content,
            removed
                .iter()
                .map(|uuid| ItemId::new(uuid.clone()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?,
            format!("delete {reference_label} and dependent markers"),
        )
        .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
        changes.extend(command.changes);
    }
    for uuid in added {
        let command = SchematicCommand::insert_item(
            content,
            after_items[&uuid].source.clone(),
            ItemAnchor::EndOfDocument,
            format!("restore junction after deleting {reference_label}"),
        )
        .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
        changes.extend(command.changes);
    }
    for uuid in modified {
        let command = SchematicCommand::replace_item(
            content,
            ItemId::new(uuid.clone())
                .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?,
            after_items[&uuid].source.clone(),
            format!("reconcile {uuid} after deleting {reference_label}"),
        )
        .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?;
        changes.extend(command.changes);
    }
    let command = SchematicCommand::from_changes(
        content,
        format!("delete components {reference_label}"),
        changes,
    )
    .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?
    .requiring_unchanged_document();
    let prepared = prepare_command(path, content, &command)
        .map_err(|error| ComponentDeleteTargetError::from_sexp(path, error))?
        .0;
    if parse_sexp(&prepared).ok() != parse_sexp(&reconciled).ok() {
        return Err(ComponentDeleteTargetError::stale(
            path,
            "the structural command cannot represent every dependent connectivity edit",
        ));
    }

    Ok(ComponentDeletePlan {
        command,
        before_items,
        unit_uuids,
        units_by_reference,
        marker_uuids,
        item_uuids,
    })
}

pub(crate) fn commit_component_deletion(
    path: &std::path::Path,
    plan: ComponentDeletePlan,
) -> anyhow::Result<Result<ComponentDeleteOutcome, CallToolResult>> {
    if let Err(error) = commit_command(path, &plan.command) {
        if let Some(refusal) = component_delete_commit_refusal(path, &error) {
            return Ok(Err(refusal));
        }
        return Err(error.into());
    }

    let committed = read_consistent(path)?;
    let after_items = match indexed_uuid_items(path, &committed) {
        Ok(items) => items,
        Err(error) => return Ok(Err(error.into_result())),
    };
    let after_tree = match parse_sexp(&committed) {
        Ok(tree) => tree,
        Err(error) => {
            return Ok(Err(
                ComponentDeleteTargetError::from_sexp(path, error).into_result()
            ));
        }
    };
    for reference in plan.units_by_reference.keys() {
        let remaining = extract_symbol_instances(&after_tree)
            .into_iter()
            .filter(|instance| instance.reference == *reference)
            .count();
        if remaining != 0 {
            return Ok(Err(ComponentDeleteTargetError::stale(
                path,
                format!("post-write readback still contains {remaining} unit(s) of {reference}"),
            )
            .into_result()));
        }
    }
    for uuid in plan
        .unit_uuids
        .iter()
        .chain(&plan.marker_uuids)
        .chain(&plan.item_uuids)
    {
        if after_items.contains_key(uuid) {
            return Ok(Err(ComponentDeleteTargetError::stale(
                path,
                format!("post-write readback still contains deleted item UUID {uuid}"),
            )
            .into_result()));
        }
    }

    let before_junctions = plan
        .before_items
        .iter()
        .filter_map(|(uuid, item)| (item.kind == "junction").then_some(uuid.clone()))
        .collect::<BTreeSet<_>>();
    let after_junctions = after_items
        .iter()
        .filter_map(|(uuid, item)| (item.kind == "junction").then_some(uuid.clone()))
        .collect::<BTreeSet<_>>();
    let pruned_junctions = before_junctions
        .difference(&after_junctions)
        .cloned()
        .collect::<Vec<_>>();
    let added_junctions = after_junctions
        .difference(&before_junctions)
        .cloned()
        .collect::<Vec<_>>();

    Ok(Ok(ComponentDeleteOutcome {
        units_by_reference: plan.units_by_reference,
        marker_uuids: plan.marker_uuids,
        item_uuids: plan.item_uuids,
        added_junctions,
        pruned_junctions,
    }))
}

fn component_delete_commit_refusal(
    path: &std::path::Path,
    error: &SexpError,
) -> Option<CallToolResult> {
    let reason = match error {
        SexpError::Conflict { .. } => "the schematic changed after deletion was planned",
        SexpError::ItemConflict { reason, .. } => reason,
        SexpError::KiCadEditorLocked { .. } => {
            "KiCad owns the schematic; use a live editor mutation or close the document"
        }
        _ => return None,
    };
    Some(ComponentDeleteTargetError::stale(path, reason).into_result())
}

/// Properties this tool exposes as first-class parameters. Routing one of them
/// through `fields` too would let a single call set the same property twice
/// with different values, and for Reference it would skip the instances-path
/// rewrite entirely — a rename that the netlist ignores (#157).
fn is_reserved_property(name: &str) -> bool {
    matches!(name, "Reference" | "Value" | "Footprint" | "Datasheet")
}

#[derive(Debug, Clone, Copy)]
struct PropertyWriteCounts {
    updated: usize,
    added: usize,
}

fn escape_property_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn closing_quote(content: &str, value_start: usize) -> Option<usize> {
    let mut escaped = false;
    for (offset, ch) in content[value_start..].char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(value_start + offset);
        }
    }
    None
}

/// Build the insertion for a custom property in one placed unit's block.
///
/// The property is anchored at that unit's own placement and inherits its
/// indentation, so applying this to every block neither piles fields at the
/// origin nor rewrites an eeschema-formatted file wholesale.
fn property_insert_edit(
    content: &str,
    reference: &str,
    start: usize,
    end: usize,
    name: &str,
    value: &str,
) -> Result<SexpEdit, String> {
    let block = &content[start..end];

    // The symbol's placement, to anchor the new property on.
    let (x, y) = block
        .find("(at ")
        .and_then(|at| {
            let rest = &block[at + 4..];
            let close = rest.find(')')?;
            let mut parts = rest[..close].split_whitespace();
            Some((
                parts.next()?.parse::<f64>().ok()?,
                parts.next()?.parse::<f64>().ok()?,
            ))
        })
        .ok_or_else(|| format!("'{reference}' has no readable (at …) placement"))?;

    // Match the block's own indentation rather than assuming: eeschema saves
    // with tabs, this crate's writer uses two spaces.
    let indent = block
        .find("(property ")
        .map(|p| {
            let line_start = block[..p].rfind('\n').map_or(0, |n| n + 1);
            block[line_start..p].to_string()
        })
        .unwrap_or_else(|| "\t\t".to_string());

    let escaped_name = escape_property_text(name);
    let escaped_value = escape_property_text(value);
    let prop = format!(
        "\n{indent}(property \"{escaped_name}\" \"{escaped_value}\"\n{indent}\t(at {x} {y} 0)\n\
         {indent}\t(hide yes)\n{indent}\t(effects\n{indent}\t\t(font\n{indent}\t\t\t\
         (size 1.27 1.27)\n{indent}\t\t)\n{indent}\t)\n{indent})"
    );

    // Insert before the block's closing paren so the property stays inside it.
    let close = block
        .rfind(')')
        .map(|offset| start + offset)
        .ok_or_else(|| format!("symbol block for '{reference}' is malformed"))?;
    Ok(SexpEdit::insert(close, prop))
}

/// Set one shared component property in every placed unit.
///
/// Built-in properties must already exist in every unit (`add_missing=false`).
/// Custom fields may be present on only some units in a legacy/broken sheet;
/// `add_missing=true` updates those copies and fills the missing ones in the
/// same atomic document command.
fn set_property_value(
    content: &str,
    reference: &str,
    field: &str,
    new_value: &str,
    add_missing: bool,
) -> Result<(String, PropertyWriteCounts), String> {
    let blocks = find_all_symbol_instance_blocks(content, reference);
    if blocks.is_empty() {
        return Err(format!("symbol '{reference}' not found in this schematic"));
    }

    let escaped_field = escape_property_text(field);
    let field_search = format!(r#"(property "{escaped_field}" ""#);
    let escaped_value = escape_property_text(new_value);
    let mut edits = Vec::new();
    let mut updated = 0;
    let mut added = 0;

    for (start, end) in blocks {
        let block = &content[start..end];
        if let Some(relative) = block.find(&field_search) {
            let value_start = start + relative + field_search.len();
            let value_end = closing_quote(content, value_start)
                .ok_or_else(|| format!("'{field}' property on '{reference}' is malformed"))?;
            edits.push(SexpEdit::replace(
                value_start,
                value_end,
                escaped_value.clone(),
            ));
            updated += 1;
        } else if add_missing {
            edits.push(property_insert_edit(
                content, reference, start, end, field, new_value,
            )?);
            added += 1;
        } else {
            return Err(format!(
                "'{reference}' is missing the shared '{field}' property on one of its placed units"
            ));
        }
    }

    Ok((
        apply_edits(content.to_string(), edits),
        PropertyWriteCounts { updated, added },
    ))
}

/// Rewrite the `(reference "…")` inside every unit's `(instances …)` block.
///
/// Returns the updated content and how many were rewritten. A multi-unit part
/// is placed once per unit and each placement carries its own instances block,
/// so a rename has to reach all of them or the units disagree about their own
/// designator.
fn rewrite_instance_references(
    content: &str,
    old_ref: &str,
    new_ref: &str,
) -> Result<(String, usize), String> {
    let blocks = find_all_symbol_instance_blocks(content, new_ref);
    if blocks.is_empty() {
        return Err(format!("symbol '{old_ref}' not found after the rename"));
    }

    let search = format!(r#"(reference "{old_ref}")"#);
    let replacement = format!(r#"(reference "{new_ref}")"#);
    let mut edits = Vec::new();
    for (start, end) in &blocks {
        let block = &content[*start..*end];
        let mut from = 0usize;
        while let Some(rel) = block[from..].find(&search) {
            let at = *start + from + rel;
            edits.push(SexpEdit::replace(
                at,
                at + search.len(),
                replacement.clone(),
            ));
            from += rel + search.len();
        }
    }
    if edits.is_empty() {
        return Err(format!(
            "'{new_ref}' has no (reference \"{old_ref}\") in its instances path — \
             the property was renamed but the netlist still reads the old designator"
        ));
    }
    let count = edits.len();
    Ok((apply_edits(content.to_string(), edits), count))
}

async fn handle_edit_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let mut content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let target = match component_target_from_source(&sch_path, &expected, &reference) {
        Ok(target) => target,
        Err(error) => return Ok(error.into_result()),
    };
    let mut changed = Vec::new();
    let mut expected_fields = BTreeMap::new();
    let mut edit_reference = reference.as_str();

    let mut errors: Vec<String> = Vec::new();
    // A macro rather than a closure: the body also needs `changed`/`errors`
    // between calls (the instances rewrite below, and the custom-field loop),
    // and a closure capturing them mutably would lock both for its lifetime.
    macro_rules! apply {
        ($field:expr, $new_val:expr) => {
            match set_property_value(&content, edit_reference, $field, $new_val, false) {
                Ok((updated, counts)) => {
                    content = updated;
                    expected_fields.insert($field.to_owned(), $new_val.to_owned());
                    changed.push(format!(
                        "{} → {} ({} unit(s))",
                        $field, $new_val, counts.updated
                    ));
                }
                Err(why) => errors.push(format!("{}: {}", $field, why)),
            }
        };
    }

    if let Some(new_ref) = opt_str(args, "new_reference") {
        let before_rename = content.clone();
        let before_changes = changed.len();
        let before_expected_fields = expected_fields.clone();
        apply!("Reference", new_ref);
        // A designator lives in TWO places. The (property "Reference" …) is
        // what renders; the (reference …) inside (instances …) is what KiCad
        // reads when it builds the netlist. Rewriting only the property leaves
        // the netlist on the old designator, so the rename appears to work in
        // eeschema and is ignored everywhere it matters (#157).
        match rewrite_instance_references(&content, &reference, new_ref) {
            Ok((updated, count)) => {
                content = updated;
                edit_reference = new_ref;
                changed.push(format!("instances reference → {new_ref} ({count})"));
            }
            Err(why) => {
                content = before_rename;
                changed.truncate(before_changes);
                expected_fields = before_expected_fields;
                errors.push(format!("instances reference: {why}"));
            }
        }
    }
    if let Some(val) = opt_str(args, "value") {
        apply!("Value", val);
    }
    if let Some(fp) = opt_str(args, "footprint") {
        apply!("Footprint", fp);
    }
    if let Some(ds) = opt_str(args, "datasheet") {
        apply!("Datasheet", ds);
    }

    // `fields` has been in this tool's schema since it shipped and the handler
    // never read it, so custom properties were dropped and the call still
    // reported success (#158). An existing property is updated in place; a new
    // one is appended to the symbol block.
    let custom_fields = args["fields"].as_object();
    if let Some(fields) = custom_fields {
        for (name, value) in fields {
            let Some(value) = value.as_str() else {
                errors.push(format!("{name}: field values must be strings"));
                continue;
            };
            if is_reserved_property(name) {
                errors.push(format!(
                    "{name}: set this through the '{}' parameter, not 'fields'",
                    name.to_ascii_lowercase()
                ));
                continue;
            }
            match set_property_value(&content, edit_reference, name, value, true) {
                Ok((updated, counts)) => {
                    content = updated;
                    expected_fields.insert(name.clone(), value.to_owned());
                    changed.push(format!(
                        "{name} → {value} ({} updated, {} added)",
                        counts.updated, counts.added
                    ));
                }
                Err(why) => errors.push(format!("{name}: {why}")),
            }
        }
    }

    // A request that changed nothing is a failure, not a success — silently
    // reporting `"changes": []` is what let the tab-indentation bug hide, and
    // what made a fields-only call report success while dropping every field
    // (#158): with `fields` unread, both `changed` and `errors` came back
    // empty and this guard never fired.
    if changed.is_empty() && custom_fields.is_some_and(|f| !f.is_empty()) && errors.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No fields were updated on '{reference}'"
        )));
    }
    if changed.is_empty() && !errors.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No fields were updated on '{}': {}",
            reference,
            errors.join("; ")
        )));
    }

    if !changed.is_empty() {
        let item_ids = match target.item_ids() {
            Ok(item_ids) => item_ids,
            Err(error) => return Ok(error.into_result()),
        };
        let command = SchematicCommand::replace_items_from_document(
            &expected,
            &content,
            item_ids,
            format!("Edit {reference}"),
        )?;
        commit_command(&sch_path, &command)?;
    }

    let observed =
        match load_component_mutation_readback(&sch_path, &target.with_fields(&expected_fields))? {
            Ok(observed) => observed,
            Err(error) => return Ok(error),
        };
    let mut result = json!({
        "reference": observed["reference"],
        "changes": changed
    });
    if observed["reference"] != reference {
        result["requested_reference"] = json!(reference);
    }
    copy_component_observation(&mut result, &observed);
    if !errors.is_empty() {
        result["errors"] = json!(errors);
    }
    Ok(CallToolResult::json(&result))
}

async fn handle_get_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let sch = cse::Schematic::load(&sch_path)?;

    let placed: Vec<_> = sch
        .symbols
        .iter()
        .filter(|symbol| symbol.reference() == Some(reference.as_str()))
        .collect();
    let Some(anchor) = placed.iter().copied().min_by_key(|symbol| symbol.unit) else {
        return Ok(CallToolResult::error(format!(
            "Component '{}' not found",
            reference
        )));
    };
    let (x, y) = anchor.position();
    let rotation = anchor.at.rotation.unwrap_or(0.0);
    let mirror = anchor.mirror.as_deref().unwrap_or("");
    let units: Vec<_> = placed
        .iter()
        .map(|symbol| {
            let (unit_x, unit_y) = symbol.position();
            let unit_mirror = symbol.mirror.as_deref().unwrap_or("");
            json!({
                "unit": symbol.unit,
                "x": unit_x,
                "y": unit_y,
                "rotation": symbol.at.rotation.unwrap_or(0.0),
                "mirror_x": unit_mirror.contains('x'),
                "mirror_y": unit_mirror.contains('y'),
                "uuid": symbol.uuid
            })
        })
        .collect();
    Ok(CallToolResult::json(&json!({
        "reference": anchor.reference().unwrap_or("?"),
        "value": anchor.value_str().unwrap_or(""),
        "footprint": anchor.footprint().unwrap_or(""),
        "lib_id": anchor.lib_id,
        "x": x,
        "y": y,
        "rotation": rotation,
        "mirror_x": mirror.contains('x'),
        "mirror_y": mirror.contains('y'),
        "uuid": anchor.uuid,
        "unit_count": units.len(),
        "units": units
    })))
}

async fn handle_list_schematic_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;

    let items: Vec<serde_json::Value> = sch
        .symbols
        .iter()
        .map(|sym| {
            let (x, y) = sym.position();
            let rotation = sym.at.rotation.unwrap_or(0.0);
            let mirror = sym.mirror.as_deref().unwrap_or("");
            json!({
                "reference": sym.reference().unwrap_or("?"),
                "value": sym.value_str().unwrap_or(""),
                "footprint": sym.footprint().unwrap_or(""),
                "lib_id": sym.lib_id,
                "x": x,
                "y": y,
                "rotation": rotation,
                "mirror_x": mirror.contains('x'),
                "mirror_y": mirror.contains('y')
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "components": items
    })))
}

async fn handle_move_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let new_y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let (new_x, new_y) = snap_point(new_x, new_y, 1.27);

    // Pin positions before the move, so the dots the pins vacate can be judged
    // afterwards (#120). Wires do not change here — pins do. A sheet with no
    // wires has nothing to reconcile, and skipping spares it the symbol walk.
    let before_pins = if read_consistent(&sch_path)
        .map(|c| c.contains("(wire"))
        .unwrap_or(false)
    {
        pin_endpoints_of(&sch_path)
    } else {
        Vec::new()
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    let mut target = match component_target_from_source(&sch_path, &sch.to_source(), &reference) {
        Ok(target) => target,
        Err(error) => return Ok(error.into_result()),
    };
    let target_uuids = target.uuids();
    let selected = target_uuids.iter().cloned().collect::<BTreeSet<_>>();
    let anchor_uuid = &target.units[0].uuid;
    let anchor = sch
        .symbols
        .iter()
        .find(|symbol| symbol.uuid == *anchor_uuid)
        .expect("structural target UUID is present in the loaded schematic");
    let (old_x, old_y) = anchor.position();
    let (dx, dy) = (new_x - old_x, new_y - old_y);
    for unit in &mut target.units {
        unit.x += dx;
        unit.y += dy;
    }
    for symbol in sch
        .symbols
        .iter_mut()
        .filter(|symbol| selected.contains(&symbol.uuid))
    {
        symbol.translate(dx, dy);
    }
    sch.overwrite()?;
    let (added, pruned) = reconcile_junctions_after_move(&sch_path, &before_pins)?;
    let observed = match load_component_mutation_readback(&sch_path, &target)? {
        Ok(observed) => observed,
        Err(error) => return Ok(error),
    };
    let mut result = json!({
        "moved": observed["reference"],
        "x": observed["x"],
        "y": observed["y"],
        "moved_units": observed["unit_count"],
        "placements": observed["units"],
        "junctions_added_count": added,
        "junctions_pruned_count": pruned
    });
    copy_component_observation(&mut result, &observed);
    Ok(CallToolResult::json(&result))
}

/// Pin endpoints on the sheet as it currently stands on disk, or empty if it
/// cannot be read — the caller only ever diffs two of these.
fn pin_endpoints_of(path: &std::path::Path) -> Vec<(f64, f64)> {
    read_consistent(path)
        .ok()
        .and_then(|c| konnect_sexp::parse_sexp(&c).ok())
        .map(|t| crate::tools::all_pin_endpoints(&t))
        .unwrap_or_default()
}

/// Re-judge junction dots wherever a pin appeared or disappeared.
///
/// The points that matter are exactly the symmetric difference of the pin sets:
/// a dot at a vacated position may now be stranded, and a pin that has landed
/// mid-span on a wire needs one. Everything else on the sheet is untouched, so
/// unrelated dots cannot be disturbed.
fn reconcile_junctions_after_move(
    path: &std::path::Path,
    before_pins: &[(f64, f64)],
) -> anyhow::Result<(usize, usize)> {
    const TOL: f64 = 0.01;
    let after_pins = pin_endpoints_of(path);
    let differs = |a: &[(f64, f64)], b: &[(f64, f64)]| -> Vec<(f64, f64)> {
        a.iter()
            .copied()
            .filter(|&(x, y)| {
                !b.iter()
                    .any(|&(ox, oy)| konnect_sexp::geometry::points_coincident(x, y, ox, oy, TOL))
            })
            .collect()
    };
    let mut points = differs(before_pins, &after_pins);
    points.extend(differs(&after_pins, before_pins));
    if points.is_empty() {
        return Ok((0, 0));
    }
    let content = read_consistent(path)?;
    let expected = content.clone();
    let (new_content, added, pruned) =
        crate::tools::sch_wiring::reconcile_junctions_at(content, &points);
    if added > 0 || pruned > 0 {
        write_atomic_if_unchanged(path, &expected, &new_content)?;
    }
    Ok((added, pruned))
}

async fn handle_rotate_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    let mut target = match component_target_from_source(&sch_path, &sch.to_source(), &reference) {
        Ok(target) => target,
        Err(error) => return Ok(error.into_result()),
    };
    let target_uuids = target.uuids();
    let selected = target_uuids.iter().cloned().collect::<BTreeSet<_>>();
    let anchor_uuid = &target.units[0].uuid;
    let anchor = sch
        .symbols
        .iter()
        .find(|symbol| symbol.uuid == *anchor_uuid)
        .expect("structural target UUID is present in the loaded schematic");
    let rotation_delta = rotation - anchor.at.rotation.unwrap_or(0.0);
    for unit in &mut target.units {
        unit.rotation = (unit.rotation + rotation_delta).rem_euclid(360.0);
    }
    for symbol in sch
        .symbols
        .iter_mut()
        .filter(|symbol| selected.contains(&symbol.uuid))
    {
        // The delta lands each unit at its own angle, so a unit already at
        // 270° asked to follow a +90° turn computes 360° — normalize into
        // [0, 360) before writing; eeschema only ever stores 0/90/180/270
        // and re-saves anything else, so an unnormalized angle survives only
        // until KiCad touches the file and then silently diverges from what
        // this response reported.
        let new_rotation = (symbol.at.rotation.unwrap_or(0.0) + rotation_delta).rem_euclid(360.0);
        symbol.set_rotation(new_rotation);
    }
    sch.overwrite()?;
    let observed = match load_component_mutation_readback(&sch_path, &target)? {
        Ok(observed) => observed,
        Err(error) => return Ok(error),
    };
    let mut result = json!({
        "rotated": observed["reference"],
        "rotation": observed["rotation"],
        "rotated_units": observed["unit_count"],
        "placements": observed["units"]
    });
    copy_component_observation(&mut result, &observed);
    Ok(CallToolResult::json(&result))
}

async fn handle_move_connected(
    _args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    // Since the first release this silently delegated to the plain move and
    // reported success — the symbol moved, every wire stayed put, and the
    // caller was told the connections were preserved (#315). A tool must not
    // claim work it does not do: refuse until the wire-carrying move exists
    // (it needs #120's connectivity model to know which wires to stretch).
    Ok(CallToolResult::error(
        "move_connected is not implemented: it used to move the symbol and leave          every wire behind while reporting the connections preserved. Use          move_schematic_component (moves the symbol only), then re-route or use          connect_pins for the affected nets. Wire-carrying moves are tracked in          issue #315 and depend on the connectivity work in #120.",
    ))
}

async fn handle_move_region(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    // Select placements by UUID, not reference. A multi-unit reference may
    // have one unit inside the rectangle and another outside; resolving the
    // selected reference back through `by_reference_mut` moved unit 1 every
    // time, and could move it twice when both units were selected (#182).
    let uuids_to_move: std::collections::HashSet<String> = sch
        .symbols
        .within_rectangle(x1, y1, x2, y2)
        .iter()
        .map(|symbol| symbol.uuid.clone())
        .collect();

    let mut moved_references = Vec::new();
    let mut placements = Vec::new();
    for symbol in sch.symbols.iter_mut() {
        if uuids_to_move.contains(&symbol.uuid) {
            let (old_x, old_y) = symbol.position();
            let (new_x, new_y) = snap_point(old_x + dx, old_y + dy, 1.27);
            symbol.move_to(new_x, new_y);
            let reference = symbol.reference().unwrap_or("?").to_string();
            if !moved_references.contains(&reference) {
                moved_references.push(reference.clone());
            }
            placements.push(json!({
                "reference": reference,
                "unit": symbol.unit,
                "x": new_x,
                "y": new_y
            }));
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "moved_count": moved_references.len(),
        "moved": moved_references,
        "moved_unit_count": placements.len(),
        "placements": placements
    })))
}

async fn handle_annotate_schematic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    crate::tools::cli::annotate_schematic(&ctx.config.kicad_cli, &sch_path).await?;
    Ok(CallToolResult::text("Annotation complete."))
}

async fn handle_get_schematic_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let (_, tree) = read_schematic(&sch_path)?;
    match pin_locations_for_reference(&tree, &reference) {
        Ok(result) => Ok(CallToolResult::json(&result)),
        Err(error) => Ok(CallToolResult::error(error)),
    }
}

fn pin_locations_for_reference(
    tree: &konnect_sexp::SexpNode,
    reference: &str,
) -> Result<serde_json::Value, String> {
    let instances = extract_symbol_instances(tree);
    let placed: Vec<_> = instances
        .iter()
        .filter(|instance| instance.reference == reference)
        .collect();
    let Some(anchor) = placed.iter().copied().min_by_key(|instance| instance.unit) else {
        return Err(format!("Component '{reference}' not found"));
    };
    let lib_syms = tree
        .find("lib_symbols")
        .map(|node| node.find_all("symbol"))
        .unwrap_or_default();

    let mut all_pins = Vec::new();
    let mut units = Vec::new();
    for instance in placed.iter().copied() {
        // A missing embedded definition is an error, not an empty pin list —
        // silently returning [] hid bad lib_ids until netlisting (#34).
        let Some(symbol) = find_lib_symbol(&lib_syms, instance) else {
            return Err(format!(
                "Component '{}' unit {} has no embedded definition for '{}' in this \
                 schematic's lib_symbols — re-add it with a valid lib_id",
                reference,
                instance.unit,
                instance.lib_symbol_name()
            ));
        };
        let lib_pins = extract_lib_pins_for_unit(symbol, instance.unit);
        if lib_pins.is_empty() {
            if let Some(parent) = symbol.find_str("extends") {
                return Err(format!(
                    "Component '{}' unit {}: the embedded definition for '{}' is an \
                     (extends \"{}\") stub with no pins — re-add the component so the \
                     definition is embedded in full",
                    reference,
                    instance.unit,
                    instance.lib_symbol_name(),
                    parent
                ));
            }
        }

        let transform = instance.pin_transform();
        let pins: Vec<serde_json::Value> = lib_pins
            .iter()
            .map(|pin| {
                let (x, y) = pin_endpoint(pin, transform);
                json!({
                    "number": pin.number,
                    "name": pin.name,
                    "unit": instance.unit,
                    "x": x,
                    "y": y,
                    "orientation_degrees": pin_outward_direction(pin, transform),
                    "length_mm": pin.length
                })
            })
            .collect();
        all_pins.extend(pins.iter().cloned());
        units.push(json!({
            "unit": instance.unit,
            "x": instance.x,
            "y": instance.y,
            "rotation": instance.rotation,
            "pins": pins
        }));
    }

    Ok(json!({
        "reference": reference,
        // Preserve the original single-placement fields as the logical
        // component anchor while exposing every real placement below.
        "component_x": anchor.x,
        "component_y": anchor.y,
        "x": anchor.x,
        "y": anchor.y,
        "rotation": anchor.rotation,
        "unit_count": units.len(),
        "units": units,
        "pins": all_pins
    }))
}

async fn handle_batch_get_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    // Required by the schema. Defaulting it returned `{"components": []}` —
    // indistinguishable from "none of your references exist" (#218).
    let refs = match require_array(args, "references") {
        Ok(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect::<Vec<_>>(),
        Err(e) => return Ok(e),
    };

    let (_, tree) = read_schematic(&sch_path)?; // single read
    let results: Vec<serde_json::Value> = refs
        .iter()
        .map(
            |reference| match pin_locations_for_reference(&tree, reference) {
                Ok(component) => component,
                Err(error) => json!({ "reference": reference, "error": error }),
            },
        )
        .collect();

    Ok(CallToolResult::json(&json!({ "components": results })))
}

/// A stable per-schematic directory under the system temp dir.
///
/// The old handler made a fresh `konnect_<uuid>` directory for every call and
/// deleted it again, so nothing survived to be returned. Keeping a uuid per call
/// would instead leak a directory per call, so the slot is derived from the
/// schematic's path: repeated views of the same sheet overwrite one file, and
/// two sheets that merely share a stem do not collide.
fn schematic_view_dir(schematic: &std::path::Path) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    schematic.hash(&mut hasher);
    std::env::temp_dir()
        .join("konnect-schematic-views")
        .join(format!("{:016x}", hasher.finish()))
}

async fn handle_get_schematic_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let out_dir = schematic_view_dir(&sch_path);
    tokio::fs::create_dir_all(&out_dir).await?;

    // KiCad has no schematic rasteriser: `sch export` offers no bitmap format
    // and there is no `sch render` at all. SVG is what there is.
    let svg_path =
        crate::tools::cli::render_schematic_svg(&ctx.config.kicad_cli, &sch_path, &out_dir).await?;

    // Deliberately not deleted. The previous handler rendered the file, read its
    // length, removed it, and then reported "The SVG file has been generated" —
    // the caller got neither the image nor a path to it.
    let bytes = tokio::fs::metadata(&svg_path).await?.len();

    Ok(CallToolResult::json(&json!({
        "schematic": sch_path.display().to_string(),
        "svg": svg_path.display().to_string(),
        "bytes": bytes,
        "format": "svg"
    })))
}

async fn handle_add_component_annotation(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let key = match require_str(args, "key") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let value = match require_str(args, "value") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    // Reference/Value/Footprint/Datasheet have dedicated parameters on
    // edit_schematic_component with their own side effects — a Reference
    // rename must also rewrite the instances path (#157) — so annotating
    // them here would bypass those.
    if is_reserved_property(&key) {
        return Ok(CallToolResult::error(format!(
            "'{key}' is a built-in field — set it through edit_schematic_component's \
             dedicated parameter, not as an annotation."
        )));
    }

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let target = match component_target_from_source(&sch_path, &expected, &reference) {
        Ok(target) => target,
        Err(error) => return Ok(error.into_result()),
    };

    // An existing key is updated in place; appending a second `(property
    // "KEY" …)` gives eeschema two fields with one name — it shows both,
    // edits the wrong one, and the duplicate survives save/reload (#203).
    // A new key is anchored separately at every unit's own position and uses
    // that block's indentation. A partially populated legacy component is
    // repaired by updating existing copies and adding only the missing ones.
    let (new_content, _) = match set_property_value(&content, &reference, &key, &value, true) {
        Ok(updated) => updated,
        Err(why) => return Ok(CallToolResult::error(format!("{key}: {why}"))),
    };
    let item_ids = match target.item_ids() {
        Ok(item_ids) => item_ids,
        Err(error) => return Ok(error.into_result()),
    };
    let command = SchematicCommand::replace_items_from_document(
        &expected,
        &new_content,
        item_ids,
        format!("Add {key} property to {reference}"),
    )?;
    commit_command(&sch_path, &command)?;

    let observed = match load_component_mutation_readback(
        &sch_path,
        &target.with_fields(&BTreeMap::from([(key.clone(), value.clone())])),
    )? {
        Ok(observed) => observed,
        Err(error) => return Ok(error),
    };
    if let Err(error) = verify_observed_field(&sch_path, &observed, &key, &value) {
        return Ok(error);
    }
    let updated_units = target
        .units
        .iter()
        .filter(|unit| unit.fields.contains_key(&key))
        .count();
    let added_units = target.units.len() - updated_units;
    let mut result = json!({
        "reference": observed["reference"],
        "added_property": key,
        "value": observed["fields"][key.as_str()],
        "updated_existing": updated_units > 0,
        "updated_units": updated_units,
        "added_units": added_units
    });
    copy_component_observation(&mut result, &observed);
    result["component_value"] = observed["value"].clone();
    result["value"] = observed["fields"][key.as_str()].clone();
    Ok(CallToolResult::json(&result))
}

async fn handle_group_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let group_name = match require_str(args, "group_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let refs = match require_array(args, "references") {
        Ok(values) => values,
        Err(error) => return Ok(error),
    };

    if refs.is_empty() {
        return Ok(CallToolResult::error("No references provided"));
    }

    let mut content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut item_ids = Vec::new();
    let mut targets = Vec::new();
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();

    for value in refs {
        let Some(reference) = value.as_str() else {
            errors.push("Component reference must be a string".to_owned());
            continue;
        };
        if !seen.insert(reference.to_owned()) {
            continue;
        }
        let target = match component_target_from_source(&sch_path, &expected, reference) {
            Ok(target) => target,
            Err(ComponentDeleteTargetError::Stale { reason, .. })
                if reason.contains("is not present") =>
            {
                errors.push(format!("Component '{reference}' not found"));
                continue;
            }
            Err(error) => return Ok(error.into_result()),
        };
        match set_property_value(&content, reference, "Group", &group_name, true) {
            Ok((updated, _)) => {
                content = updated;
                item_ids.extend(match target.item_ids() {
                    Ok(item_ids) => item_ids,
                    Err(error) => return Ok(error.into_result()),
                });
                targets.push(target);
            }
            Err(error) => errors.push(format!("{reference}: {error}")),
        }
    }

    if item_ids.is_empty() {
        return Ok(ComponentDeleteTargetError::stale(
            &sch_path,
            if errors.is_empty() {
                "no components were selected".to_owned()
            } else {
                errors.join("; ")
            },
        )
        .into_result());
    }
    let command = SchematicCommand::replace_items_from_document(
        &expected,
        &content,
        item_ids,
        format!("Group components as {group_name}"),
    )?;
    commit_command(&sch_path, &command)?;

    let committed = cse::Schematic::load(&sch_path)?;
    let mut grouped = Vec::new();
    let mut components = Vec::new();
    for target in &targets {
        let observed = match verified_component_readback(
            &sch_path,
            &committed,
            &target.with_fields(&BTreeMap::from([("Group".to_owned(), group_name.clone())])),
        ) {
            Ok(observed) => observed,
            Err(error) => return Ok(error),
        };
        if let Err(error) = verify_observed_field(&sch_path, &observed, "Group", &group_name) {
            return Ok(error);
        }
        grouped.push(
            observed["reference"]
                .as_str()
                .expect("readback reference is a string")
                .to_owned(),
        );
        components.push(observed);
    }
    let observed_group_name = components[0]["fields"]["Group"].clone();
    let observed_schematic = components[0]["schematic"].clone();

    Ok(CallToolResult::json(&json!({
        "group_name": observed_group_name,
        "grouped_count": grouped.len(),
        "grouped": grouped,
        "components": components,
        "schematic": observed_schematic,
        "errors": errors
    })))
}

async fn handle_update_symbols_from_library(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let only: Option<Vec<String>> = args["references"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let allow_pin_moves = args["allow_pin_moves"].as_bool().unwrap_or(false);

    let (mut content, tree) = read_schematic(&sch_path)?;
    let expected = content.clone();

    let instances = extract_symbol_instances(&tree);
    if let Some(refs) = &only {
        if let Some(missing) = refs
            .iter()
            .find(|r| !instances.iter().any(|i| &i.reference == *r))
        {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found in {}",
                missing,
                sch_path.display()
            )));
        }
    }

    // One definition serves every instance of a lib_id, so refresh each once.
    let mut lib_ids: Vec<String> = Vec::new();
    for inst in instances {
        if only.as_ref().is_some_and(|r| !r.contains(&inst.reference)) {
            continue;
        }
        if !lib_ids.contains(&inst.lib_id) {
            lib_ids.push(inst.lib_id);
        }
    }

    let mut updated = Vec::new();
    let mut unchanged = Vec::new();
    let mut pins_moved = Vec::new();
    let mut errors = Vec::new();
    let src = match crate::tools::library::KiCadSymbolSource::for_file(&sch_path) {
        Ok(source) => source,
        Err(error) => return Ok(error.into_tool_result()),
    };
    let outcomes = reembed_lib_symbols(&mut content, &lib_ids, allow_pin_moves, &src);
    for (lib_id, outcome) in lib_ids.iter().zip(outcomes) {
        match outcome {
            ReembedOutcome::Updated => updated.push(lib_id.clone()),
            ReembedOutcome::Unchanged => unchanged.push(lib_id.clone()),
            ReembedOutcome::PinsMoved(pins) => pins_moved.push(json!({
                "lib_id": lib_id,
                "pins": pins,
            })),
            ReembedOutcome::Unresolved => errors.push(format!(
                "'{}' no longer resolves in any registered library — the \
                 embedded copy is left as it is",
                lib_id
            )),
            ReembedOutcome::NotEmbedded => {
                errors.push(format!("'{}' has no embedded definition to update", lib_id))
            }
        }
    }

    if !updated.is_empty() && !dry_run {
        write_atomic_if_unchanged(&sch_path, &expected, &content)?;
    }

    let mut body = json!({
        "updated": updated,
        "updated_count": updated.len(),
        "unchanged": unchanged,
        "pins_moved": pins_moved,
        "errors": errors,
        "dry_run": dry_run
    });
    if !pins_moved.is_empty() {
        body["hint"] = json!(
            "Symbols listed in pins_moved were left untouched: the library moved or \
             removed pins, and wires and labels attach at pin coordinates. Pass \
             allow_pin_moves: true to update them anyway, then reconnect."
        );
    }
    Ok(CallToolResult::json(&body))
}

/// Put every instance field back on its library anchor.
///
/// `add_schematic_component` places new symbols there already; this repairs
/// sheets written before it did, where every field sat at a fixed ±3.81mm
/// offset regardless of what the symbol's definition asked for (#101).
async fn handle_reset_schematic_field_positions(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let only: Option<std::collections::HashSet<String>> = args["references"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);

    let mut sch = cse::Schematic::load(&sch_path)?;

    // Anchors first: reading them borrows the schematic, mutating the symbols
    // borrows it again, so the lookup cannot be inlined into the loop.
    let lib_ids: Vec<String> = {
        let mut ids: Vec<String> = Vec::new();
        for sym in sch.symbols.iter() {
            if !ids.contains(&sym.lib_id) {
                ids.push(sym.lib_id.clone());
            }
        }
        ids
    };
    let anchors: std::collections::HashMap<String, cse::library::FieldAnchors> = lib_ids
        .into_iter()
        .map(|id| {
            let a = cse::library::field_anchors(&sch, &id);
            (id, a)
        })
        .collect();

    let mut moved = Vec::new();
    let mut unchanged = Vec::new();
    let mut no_anchor = Vec::new();
    let mut no_property = Vec::new();
    let mut missing: Vec<String> = only
        .clone()
        .map(|r| r.into_iter().collect())
        .unwrap_or_default();

    for sym in sch.symbols.iter_mut() {
        let Some(reference) = sym.reference().map(String::from) else {
            continue;
        };
        if only.as_ref().is_some_and(|r| !r.contains(&reference)) {
            continue;
        }
        missing.retain(|r| r != &reference);

        let anchor = anchors.get(&sym.lib_id).copied().unwrap_or_default();
        let mirror = sym.mirror.as_deref().unwrap_or("");
        let t = konnect_sexp::geometry::PinTransform {
            comp_x: sym.at.x,
            comp_y: sym.at.y,
            rotation_deg: sym.at.rotation.unwrap_or(0.0),
            mirror_x: mirror.contains('x'),
            mirror_y: mirror.contains('y'),
        };

        for (name, anchor) in [
            ("Reference", anchor.reference_at),
            ("Value", anchor.value_at),
        ] {
            let Some(anchor) = anchor else {
                no_anchor.push(format!("{}.{}", reference, name));
                continue;
            };
            let (x, y, rot) = crate::tools::field_at(Some(anchor), (0.0, 0.0, 0.0), t);
            // The library anchors this field but the placed symbol carries no
            // such property. Report it rather than dropping it in silence —
            // an unreported skip reads as "reset" to the caller.
            let Some(prop) = sym.properties.iter_mut().find(|p| p.name == name) else {
                no_property.push(format!("{}.{}", reference, name));
                continue;
            };
            if set_property_at(prop, x, y, rot) {
                moved.push(format!("{}.{}", reference, name));
            } else {
                unchanged.push(format!("{}.{}", reference, name));
            }
        }
    }

    if !moved.is_empty() && !dry_run {
        sch.overwrite()?;
    }
    // `missing` starts life as a HashSet, whose iteration order varies run to
    // run; a caller asking about several unknown references would get them
    // back in a different order each time.
    missing.sort_unstable();

    Ok(CallToolResult::json(&json!({
        "moved": moved,
        "moved_count": moved.len(),
        "unchanged": unchanged,
        "no_library_anchor": no_anchor,
        "no_property": no_property,
        "not_found": missing,
        "dry_run": dry_run
    })))
}

/// Rewrite a property's `(at …)` in place. Returns whether anything changed,
/// so an already-correct field is not reported as moved.
fn set_property_at(prop: &mut cse::types::Property, x: f64, y: f64, rotation: f64) -> bool {
    use cse::sexp::{atom, SexpNode};
    use cse::types::fmt_f64;

    let at = SexpNode::List(vec![
        atom("at"),
        atom(fmt_f64(x)),
        atom(fmt_f64(y)),
        atom(fmt_f64(rotation)),
    ]);
    match prop.sub_nodes.iter_mut().find(|n| n.tag() == Some("at")) {
        Some(existing) => {
            if *existing == at {
                return false;
            }
            *existing = at;
        }
        // A field with no (at) is drawn at the sheet origin — always a move.
        None => prop.sub_nodes.insert(0, at),
    }
    true
}

async fn handle_replace_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_lib_id = match require_str(args, "new_lib_id") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_unit = opt_f64(args, "unit").map(|u| u as u32);

    let mut content = read_consistent(&sch_path)?;
    let expected = content.clone();

    let blocks = find_all_symbol_instance_blocks(&content, &reference);
    if blocks.is_empty() {
        return Ok(CallToolResult::error(format!(
            "Component '{}' not found",
            reference
        )));
    }
    if blocks.len() > 1 && new_unit.is_some() {
        return Ok(CallToolResult::error(format!(
            "Component '{}' has {} placed units; the 'unit' override is only \
             unambiguous for a single placement. Omit it to preserve each unit.",
            reference,
            blocks.len()
        )));
    }

    let parsed = parse_sexp(&content)?;
    let current_units: Vec<u32> = extract_symbol_instances(&parsed)
        .into_iter()
        .filter(|instance| instance.reference == reference)
        .map(|instance| instance.unit)
        .collect();

    let src = match crate::tools::library::KiCadSymbolSource::for_file(&sch_path) {
        Ok(source) => source,
        Err(error) => return Ok(error.into_tool_result()),
    };
    let embedded_unit_count = parsed
        .find("lib_symbols")
        .and_then(|libraries| {
            libraries.find_all("symbol").into_iter().find(|symbol| {
                symbol.get(1).and_then(|value| value.as_str()) == Some(new_lib_id.as_str())
            })
        })
        .map(|symbol| {
            symbol
                .find_all("symbol")
                .into_iter()
                .filter_map(|unit| {
                    unit.get(1)
                        .and_then(|value| value.as_str())
                        .and_then(konnect_sexp::schematic::parse_subsymbol_unit)
                })
                .max()
                .unwrap_or(1)
                .max(1)
        });
    let unit_count = embedded_unit_count
        .or_else(|| cse::library::symbol_unit_count(&new_lib_id, &src))
        .unwrap_or(1);
    if let Some(unit) = new_unit {
        if unit < 1 || unit > unit_count {
            return Ok(CallToolResult::error(format!(
                "Invalid unit {} for '{}': the symbol has {} unit(s) (valid: 1..={}).",
                unit, new_lib_id, unit_count, unit_count
            )));
        }
    } else if let Some(invalid) = current_units
        .iter()
        .find(|unit| **unit < 1 || **unit > unit_count)
    {
        return Ok(CallToolResult::error(format!(
            "Cannot replace '{}' with '{}': placed unit {} does not exist in the \
             new {}-unit symbol. Delete and re-place the component deliberately.",
            reference, new_lib_id, invalid, unit_count
        )));
    }

    // Replace the library id in every unit block. Shared component identity
    // must not leave one unit pointing at the old symbol (#182).
    let lib_id_pat = "(lib_id \"";
    let escaped_lib_id = escape_property_text(&new_lib_id);
    let mut edits = Vec::new();
    let mut old_lib_ids = Vec::new();
    for (start, end) in &blocks {
        let block = &content[*start..*end];
        let Some(relative) = block.find(lib_id_pat) else {
            return Ok(CallToolResult::error(format!(
                "A unit of '{}' has no lib_id",
                reference
            )));
        };
        let value_start = *start + relative + lib_id_pat.len();
        let Some(value_end) = closing_quote(&content, value_start) else {
            return Ok(CallToolResult::error("Malformed lib_id"));
        };
        old_lib_ids.push(content[value_start..value_end].to_string());
        edits.push(SexpEdit::replace(
            value_start,
            value_end,
            escaped_lib_id.clone(),
        ));
    }

    // Add the optional unit edits without a second source read. The multi-unit
    // guard above means this scan has at most one block.
    if let Some(unit) = new_unit {
        let (start, end) = blocks[0];
        let block = &content[start..end];
        let mut from = 0usize;
        while let Some(relative) = block[from..].find("(unit ") {
            let number_start = from + relative + "(unit ".len();
            let Some(close) = block[number_start..].find(')') else {
                break;
            };
            edits.push(SexpEdit::replace(
                start + number_start,
                start + number_start + close,
                unit.to_string(),
            ));
            from = number_start + close;
        }
    }

    old_lib_ids.sort();
    old_lib_ids.dedup();
    if old_lib_ids.len() != 1 {
        return Ok(CallToolResult::error(format!(
            "Component '{}' already has inconsistent library ids across its units: {}",
            reference,
            old_lib_ids.join(", ")
        )));
    }
    let old_lib_id = old_lib_ids.remove(0);
    content = apply_edits(content, edits);

    // Ensure the new library symbol definition is present. Bail BEFORE writing:
    // a replace that can't embed its definition would leave the component
    // netlist-invisible (#34).
    if !super::ensure_lib_symbol_in_schematic(&mut content, &new_lib_id, &src) {
        return Ok(crate::tools::lib_symbol_not_found_error(&new_lib_id, &src));
    }
    write_atomic_if_unchanged(&sch_path, &expected, &content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "old_lib_id": old_lib_id,
        "new_lib_id": new_lib_id,
        "unit": new_unit,
        "units_replaced": blocks.len()
    })))
}

// Library symbol resolution moved to tools/mod.rs (shared with sch_wiring.rs)

// `stub_symbol_dir` returns a MutexGuard that the async tests then hold across
// their `.await`s, which is what `await_holding_lock` warns about. It is
// deliberate and safe here: the lock serialises process-wide `KICAD*_DIR`
// environment variables, which the awaited calls read, so releasing it early
// would defeat its only purpose. cargo runs each test on its own OS thread with
// its own current-thread runtime, and each runtime drives exactly one task, so
// there is no second task that could contend for the guard and deadlock.
#[allow(clippy::await_holding_lock)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// Serializes tests that set KICAD10_SYMBOL_DIR (process-wide env), shared
    /// with every other module that does so.
    use crate::tools::KICAD_ENV_LOCK as SYMBOL_DIR_ENV;

    /// Only the stub carries this, so asserting on it proves a placement
    /// resolved the fixture and not a KiCad library installed on the machine.
    const STUB_MARKER: &str = "stub://device";

    /// A stub symbol library so component adds resolve without an installed
    /// KiCad (CI has none): Device:R and Device:C_Polarized in the KiCad 10
    /// symdir layout, plus a `sym-lib-table` registering them.
    ///
    /// The returned tempdir doubles as the project directory — put the test's
    /// schematic in it, so the project table is the one consulted.
    fn stub_symbol_dir() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = SYMBOL_DIR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let symdir = dir.path().join("Device.kicad_symdir");
        std::fs::create_dir_all(&symdir).unwrap();
        let symbol = |name: &str| {
            format!(
                "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"{name}\"\n\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t(property \"Value\" \"{name}\" (at 0 0 0))\n\t\t(property \"Datasheet\" \"{STUB_MARKER}\" (at 0 0 0))\n\t\t(symbol \"{name}_0_1\"\n\t\t\t(pin passive line (at 0 3.81 270) (length 1.27)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t\t(pin passive line (at 0 -3.81 90) (length 1.27)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"2\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t)\n\t)\n)\n"
            )
        };
        std::fs::write(symdir.join("R.kicad_sym"), symbol("R")).unwrap();
        std::fs::write(symdir.join("C_Polarized.kicad_sym"), symbol("C_Polarized")).unwrap();
        // LM2904-style multi-unit part: unit 1 = pins 1-3, unit 2 = pins 5-7,
        // unit 3 = power pins 4/8 (#35 repro shape).
        let pin = |num: &str, x: f64, y: f64, angle: u32| {
            format!(
                "\t\t\t(pin passive line (at {x} {y} {angle}) (length 2.54)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"{num}\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n"
            )
        };
        let opamp = format!(
            "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"OPAMP_DUAL\"\n\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t(property \"Value\" \"OPAMP_DUAL\" (at 0 0 0))\n\t\t(symbol \"OPAMP_DUAL_1_1\"\n{}{}{}\t\t)\n\t\t(symbol \"OPAMP_DUAL_2_1\"\n{}{}{}\t\t)\n\t\t(symbol \"OPAMP_DUAL_3_1\"\n{}{}\t\t)\n\t)\n)\n",
            pin("1", -7.62, 2.54, 0),
            pin("2", -7.62, -2.54, 0),
            pin("3", 7.62, 0.0, 180),
            pin("5", -7.62, 2.54, 0),
            pin("6", -7.62, -2.54, 0),
            pin("7", 7.62, 0.0, 180),
            pin("4", 0.0, -7.62, 90),
            pin("8", 0.0, 7.62, 270),
        );
        std::fs::write(symdir.join("OPAMP_DUAL.kicad_sym"), opamp).unwrap();
        // Derived symbol: an extends stub with no drawing of its own, like
        // Amplifier_Operational:NE5532 → LM2904.
        std::fs::write(
            symdir.join("OPAMP_DERIVED.kicad_sym"),
            "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"OPAMP_DERIVED\"\n\t\t(extends \"OPAMP_DUAL\")\n\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t(property \"Value\" \"OPAMP_DERIVED\" (at 0 0 0))\n\t)\n)\n",
        )
        .unwrap();
        // A project sym-lib-table, checked before the global one, is what
        // makes this hermetic: KICAD10_SYMBOL_DIR alone is not enough, because
        // the global table's own `Device` entry resolves to whatever KiCad the
        // developer has installed and would shadow the stub.
        std::fs::write(
            dir.path().join("sym-lib-table"),
            format!(
                "(sym_lib_table\n  (version 7)\n  (lib (name \"Device\") (type \"KiCad\") (uri \"{}\") (options \"\") (descr \"\"))\n)\n",
                symdir.display()
            ),
        )
        .unwrap();
        std::env::set_var("KICAD10_SYMBOL_DIR", dir.path());
        (dir, guard)
    }

    #[tokio::test]
    async fn create_schematic_writes_root_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.kicad_sch");
        let ctx = test_ctx();

        let result = handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(
            sch.uuid.is_some(),
            "root (uuid ...) is required for KiCAD's netlister to resolve instance paths"
        );
    }

    #[tokio::test]
    async fn create_schematic_defaults_to_a4() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.kicad_sch");
        handle_create_schematic(&json!({ "path": path.display().to_string() }), &test_ctx())
            .await
            .unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("(paper \"A4\")"), "got {out}");
    }

    #[tokio::test]
    async fn create_schematic_honours_size_and_orientation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.kicad_sch");
        let result = handle_create_schematic(
            &json!({ "path": path.display().to_string(), "size": "A3", "portrait": true }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text")
        };
        // The dimensions are reported swapped for portrait, matching
        // set_schematic_page.
        assert!(text.contains("\"width_mm\":297"), "got {text}");
        assert!(text.contains("\"height_mm\":420"), "got {text}");

        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("(paper \"A3\" portrait)"), "got {out}");
        // The orientation token has to survive cse's normalising rewrite:
        // KiCad rejects a `(paper …)` it cannot parse.
        assert_eq!(
            cse::Schematic::load(&path).unwrap().paper.as_deref(),
            Some("A3")
        );
    }

    #[tokio::test]
    async fn create_schematic_refuses_an_unknown_size_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.kicad_sch");
        let result = handle_create_schematic(
            &json!({ "path": path.display().to_string(), "size": "A9" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert!(!path.exists(), "a rejected size must leave no file behind");
    }

    /// #204: on a child sheet both halves of the instance key came from the
    /// child file — its own stem as the project name, its own uuid as the
    /// whole path. KiCad matches that against nothing, so every symbol placed
    /// on a sub-sheet read as unannotated.
    #[tokio::test]
    async fn a_child_sheet_keys_instances_to_the_root_not_itself() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx();
        std::fs::write(dir.path().join("board.kicad_pro"), "{}").unwrap();
        let root = dir.path().join("board.kicad_sch");
        let child = dir.path().join("amp.kicad_sch");
        std::fs::write(
            &root,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"ROOTUUID\")\n\t(paper \"A4\")\n\t(lib_symbols)\n\t(sheet\n\t\t(at 50 50)\n\t\t(size 20 20)\n\t\t(uuid \"SHEETUUID\")\n\t\t(property \"Sheetname\" \"amp\")\n\t\t(property \"Sheetfile\" \"amp.kicad_sch\")\n\t)\n)\n",
        )
        .unwrap();
        std::fs::write(
            &child,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"CHILDUUID\")\n\t(paper \"A4\")\n\t(lib_symbols)\n)\n",
        )
        .unwrap();

        let placed = handle_add_schematic_component(
            &json!({ "schematic": child.display().to_string(), "lib_id": "Device:R",
                     "reference": "R1", "x": 100.0, "y": 100.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!placed.is_error, "{placed:?}");

        let written = std::fs::read_to_string(&child).unwrap();
        assert!(
            written.contains("(project \"board\""),
            "the project name is the .kicad_pro stem, not the child file stem:\n{written}"
        );
        assert!(
            written.contains("/ROOTUUID/SHEETUUID"),
            "the path must run root -> sheet:\n{written}"
        );
        assert!(
            !written.contains("(path \"/CHILDUUID\""),
            "the child's own uuid must not be the whole path:\n{written}"
        );
    }

    #[tokio::test]
    async fn placement_refuses_ambiguous_project_ownership_without_writing() {
        let outer = tempfile::tempdir().unwrap();
        let nested = outer.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(outer.path().join("outer.kicad_pro"), "{}").unwrap();
        std::fs::write(nested.join("inner.kicad_pro"), "{}").unwrap();
        let root = |root_uuid: &str, sheet_uuid: &str, child: &str| {
            format!(
                r#"(kicad_sch
	(version 20250610)
	(generator "eeschema")
	(uuid "{root_uuid}")
	(paper "A4")
	(lib_symbols)
	(sheet
		(at 20 20)
		(size 40 20)
		(uuid "{sheet_uuid}")
		(property "Sheetname" "Child" (at 20 19.365 0))
		(property "Sheetfile" "{child}" (at 20 40.635 0))
	)
	(sheet_instances (path "/" (page "1")))
)
"#,
            )
        };
        std::fs::write(
            outer.path().join("outer.kicad_sch"),
            root("outer-root", "outer-path", "nested/child.kicad_sch"),
        )
        .unwrap();
        std::fs::write(
            nested.join("inner.kicad_sch"),
            root("inner-root", "inner-path", "child.kicad_sch"),
        )
        .unwrap();
        let child = nested.join("child.kicad_sch");
        std::fs::write(&child, crate::tools::blank_schematic_template()).unwrap();
        let before = std::fs::read(&child).unwrap();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": child.display().to_string(),
                "lib_id": "Device:R",
                "reference": "R1",
                "x": 100.0,
                "y": 100.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("conflict")
        );
        assert_eq!(std::fs::read(&child).unwrap(), before);
    }

    /// A standalone sheet — no project file, no parent — keeps the old
    /// behaviour: it is its own root.
    #[tokio::test]
    async fn a_standalone_sheet_still_keys_instances_to_itself() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx();
        let path = dir.path().join("loose.kicad_sch");
        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        handle_add_schematic_component(
            &json!({ "schematic": path.display().to_string(), "lib_id": "Device:R",
                     "reference": "R1", "x": 100.0, "y": 100.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let sch = cse::Schematic::load(&path).unwrap();
        let own = sch.uuid.clone().unwrap();
        assert!(
            written.contains(&format!("(path \"/{own}\"")),
            "a loose sheet is its own root:\n{written}"
        );
        assert!(written.contains("(project \"loose\""), "{written}");
    }

    #[tokio::test]
    async fn add_component_writes_eeschema_style_instance_path() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("amp.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 100.0, "y": 80.0,
                "reference": "R1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        // Guards the fixture itself: the project sym-lib-table must win over
        // any real Device library the developer has installed, or these tests
        // silently stop exercising the stub they set up.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(STUB_MARKER),
            "Device:R must resolve from the stub, not an installed KiCad library"
        );

        let sch = cse::Schematic::load(&path).unwrap();
        let root_uuid = sch.uuid.clone().expect("root uuid present");
        let sym = sch.symbols.by_reference("R1").unwrap();
        // KiCAD only forms wire-only nets when the instance path is exactly
        // "/<root-uuid>"; the project key mirrors eeschema (file stem).
        assert!(
            sym.has_instance_path("amp", &format!("/{}", root_uuid)),
            "instance path must be /<root-uuid> under the file-stem project name"
        );
        assert!(
            !raw.lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "component placement must not leave trailing whitespace: {raw:?}"
        );
    }

    #[tokio::test]
    async fn add_component_writes_requested_unit() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("multi.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:OPAMP_DUAL",
                "x": 100.0, "y": 80.0,
                "reference": "U1",
                "unit": 3
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "unit 3 of a 3-unit part must be accepted");
        let response: serde_json::Value =
            serde_json::from_str(&content_text(&result)).expect("placement JSON");
        assert_eq!(response["schematic"], path.display().to_string());
        assert_eq!(response["unit"], 3);
        assert_eq!(response["unit_count"], 1);
        assert_eq!(response["units"][0]["unit"], 3);
        assert_eq!(response["units"][0]["fields"]["Reference"], "U1");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch.symbols.by_reference("U1").unwrap();
        assert_eq!(sym.unit, 3, "symbol (unit N) must match the requested unit");
        let root_uuid = sch.uuid.clone().unwrap();
        // Instance entry must carry the same unit, not a hardcoded 1.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(&format!("/{}", root_uuid)));
        assert!(raw.contains("(unit 3)"), "instance unit must be 3");
    }

    fn content_text(res: &CallToolResult) -> String {
        match res.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_component_rejects_out_of_range_unit() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("units.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        for bad_unit in [0, 99] {
            let result = handle_add_schematic_component(
                &json!({
                    "schematic": path.display().to_string(),
                    "lib_id": "Device:OPAMP_DUAL",
                    "x": 100.0, "y": 80.0,
                    "reference": "U1",
                    "unit": bad_unit
                }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(result.is_error, "unit {bad_unit} must be rejected");
            let text = content_text(&result);
            assert!(
                text.contains("3 unit"),
                "error must state the unit count: {text}"
            );
        }
        // A single-unit symbol only accepts unit 1.
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 100.0, "y": 80.0,
                "reference": "R1",
                "unit": 2
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            result.is_error,
            "unit 2 of a 1-unit symbol must be rejected"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "rejected placements must not modify the schematic"
        );
    }

    #[tokio::test]
    async fn pin_locations_are_unit_aware() {
        // The #35 repro: an LM2904-style dual op-amp placed as unit 1 and as
        // unit 2 must report DISJOINT pin sets, not all units superimposed.
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("dual.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        for (reference, unit, x) in [("U1", 1, 100.0), ("U2", 2, 150.0)] {
            let res = handle_add_schematic_component(
                &json!({
                    "schematic": path.display().to_string(),
                    "lib_id": "Device:OPAMP_DUAL",
                    "x": x, "y": 80.0,
                    "reference": reference,
                    "unit": unit
                }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(!res.is_error, "placing {reference}: {:?}", res.content);
        }

        let pin_numbers = |res: &CallToolResult| -> Vec<String> {
            let out: serde_json::Value = serde_json::from_str(&content_text(res)).unwrap();
            let mut nums: Vec<String> = out["pins"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["number"].as_str().unwrap().to_string())
                .collect();
            nums.sort();
            nums
        };

        let u1 = handle_get_schematic_pin_locations(
            &json!({ "schematic": path.display().to_string(), "reference": "U1" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!u1.is_error);
        assert_eq!(pin_numbers(&u1), vec!["1", "2", "3"], "unit 1 pins only");

        let u2 = handle_get_schematic_pin_locations(
            &json!({ "schematic": path.display().to_string(), "reference": "U2" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!u2.is_error);
        assert_eq!(pin_numbers(&u2), vec!["5", "6", "7"], "unit 2 pins only");

        // Batch variant agrees.
        let batch = handle_batch_get_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "references": ["U1", "U2"]
            }),
            &ctx,
        )
        .await
        .unwrap();
        let out: serde_json::Value = serde_json::from_str(&content_text(&batch)).unwrap();
        let comps = out["components"].as_array().unwrap();
        let nums = |i: usize| -> Vec<String> {
            let mut v: Vec<String> = comps[i]["pins"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["number"].as_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(nums(0), vec!["1", "2", "3"]);
        assert_eq!(nums(1), vec!["5", "6", "7"]);
    }

    #[tokio::test]
    async fn pin_locations_error_on_extends_stub_with_zero_pins() {
        // A pre-flattening schematic: the embedded definition for the derived
        // symbol is an (extends "Parent") stub with no pins. The #34 guard
        // only catches MISSING definitions; a resolving-but-pinless stub must
        // be a structured error too, not pins:[] (#35).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stub.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t\t(symbol \"Device:OPAMP_DERIVED\"\n\t\t\t(extends \"Device:OPAMP_DUAL\")\n\t\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:OPAMP_DERIVED\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let res = handle_get_schematic_pin_locations(
            &json!({ "schematic": path.display().to_string(), "reference": "U1" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(res.is_error, "extends stub with zero pins must be an error");
        let text = content_text(&res);
        assert!(
            text.contains("Device:OPAMP_DERIVED"),
            "error must name the lib_id: {text}"
        );
        assert!(
            text.contains("Device:OPAMP_DUAL"),
            "error must name the extends target: {text}"
        );

        // Batch variant reports it per-entry.
        let batch = handle_batch_get_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "references": ["U1"]
            }),
            &ctx,
        )
        .await
        .unwrap();
        let out: serde_json::Value = serde_json::from_str(&content_text(&batch)).unwrap();
        let err = out["components"][0]["error"].as_str().unwrap_or("");
        assert!(
            err.contains("Device:OPAMP_DUAL"),
            "batch entry must carry the stub error: {out}"
        );
    }

    #[tokio::test]
    async fn pin_locations_resolve_through_lib_name_not_lib_id() {
        // eeschema stores a locally edited library symbol under a derived name
        // and points the instance at it with (lib_name …). Resolving on lib_id
        // alone picks the *base* definition, whose pins sit elsewhere — the
        // wrong answer is returned silently, and every wire placed from it
        // lands off-pin (#143).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250114)\n\t(generator \"eeschema\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(symbol \"R_1_1\"\n\t\t\t\t(pin passive line (at 0 3.81 270) (length 1.27) (name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t\t(symbol \"R_1\"\n\t\t\t(symbol \"R_1_1_1\"\n\t\t\t\t(pin passive line (at 0 6.35 270) (length 1.27) (name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t\t(symbol \"C_1\"\n\t\t\t(symbol \"C_1_1_1\"\n\t\t\t\t(pin passive line (at 0 3.81 270) (length 3.048) (name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_name \"R_1\")\n\t\t(lib_id \"Device:R\")\n\t\t(at 88.9 63.5 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-000000000001\")\n\t\t(property \"Reference\" \"R2\" (at 91.44 62.23 0))\n\t)\n\t(symbol\n\t\t(lib_name \"C_1\")\n\t\t(lib_id \"Device:C\")\n\t\t(at 139.7 63.5 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-000000000002\")\n\t\t(property \"Reference\" \"C1\" (at 142.24 62.23 0))\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let res = handle_get_schematic_pin_locations(
            &json!({ "schematic": path.display().to_string(), "reference": "R2" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{}", content_text(&res));
        let out: serde_json::Value = serde_json::from_str(&content_text(&res)).unwrap();
        // R_1's pin sits at local +6.35 => 63.5 - 6.35; Device:R's would be
        // 63.5 - 3.81 = 59.69.
        assert_eq!(out["pins"][0]["y"].as_f64().unwrap(), 57.15);

        // Device:C is not embedded at all — only the derived C_1 is. Matching
        // on lib_id reported "no embedded definition ... nonexistent lib_id",
        // which is both wrong and dangerous advice.
        let batch = handle_batch_get_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "references": ["C1"]
            }),
            &ctx,
        )
        .await
        .unwrap();
        let out: serde_json::Value = serde_json::from_str(&content_text(&batch)).unwrap();
        assert!(
            out["components"][0]["error"].is_null(),
            "C1 must resolve through C_1: {out}"
        );
        assert_eq!(
            out["components"][0]["pins"][0]["y"].as_f64().unwrap(),
            59.69
        );
    }

    #[tokio::test]
    async fn replace_component_sets_validated_unit() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("swap.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:OPAMP_DUAL",
                "x": 100.0, "y": 80.0,
                "reference": "U1",
                "unit": 1
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Out-of-range unit on the new symbol is rejected before any write.
        let before = std::fs::read_to_string(&path).unwrap();
        let bad = handle_replace_component(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "U1",
                "new_lib_id": "Device:OPAMP_DUAL",
                "unit": 99
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(bad.is_error, "unit 99 must be rejected");
        assert!(content_text(&bad).contains("3 unit"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // Valid unit is written to the symbol and its instances entry.
        let ok = handle_replace_component(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "U1",
                "new_lib_id": "Device:OPAMP_DUAL",
                "unit": 2
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!ok.is_error, "{:?}", ok.content);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("(unit 2)"),
            "unit must be updated to 2:\n{raw}"
        );
        assert!(
            !raw.contains("(unit 1)"),
            "no stale (unit 1) may remain in the instance:\n{raw}"
        );
        let sch = cse::Schematic::load(&path).unwrap();
        assert_eq!(sch.symbols.by_reference("U1").unwrap().unit, 2);
    }

    #[tokio::test]
    async fn add_component_repairs_legacy_file_without_root_uuid() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("legacy.kicad_sch");
        // File shape produced by Konnect before root UUIDs were written.
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 50.0, "y": 50.0,
                "reference": "R1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        let root_uuid = sch.uuid.clone().expect("legacy file gains a root uuid");
        let sym = sch.symbols.by_reference("R1").unwrap();
        assert!(sym.has_instance_path("legacy", &format!("/{}", root_uuid)));
    }

    #[tokio::test]
    async fn add_component_with_nonexistent_lib_id_errors_with_suggestion() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("ghost.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Device:CP is the KiCAD ≤9 name; 10 renamed it to C_Polarized (#34).
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:CP",
                "x": 100.0, "y": 80.0,
                "reference": "C1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error, "nonexistent lib_id must be an error");
        let msg = format!("{:?}", result.content);
        assert!(msg.contains("Device:CP"), "names the bad lib_id: {msg}");
        assert!(
            msg.contains("C_Polarized"),
            "did-you-mean should surface the rename: {msg}"
        );

        // And nothing was written: no ghost instance in the file.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn add_component_with_unknown_library_says_so() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("nolib.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Transistor_FET_xyzzy:IRF830",
                "x": 100.0, "y": 80.0
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let msg = format!("{:?}", result.content);
        assert!(
            msg.contains("Library 'Transistor_FET_xyzzy' not found"),
            "distinguishes missing library from missing symbol: {msg}"
        );
    }

    #[tokio::test]
    async fn pin_locations_error_when_definition_not_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noembed.kicad_sch");
        // A symbol instance whose lib_id has NO lib_symbols entry — the file
        // shape a ghost lib_id used to leave behind (#34).
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t)\n\t(symbol\n\t\t(lib_id \"Device:CP\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n\t\t(property \"Reference\" \"C1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let result = handle_get_schematic_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "C1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            result.is_error,
            "missing embedded definition must be an error, not pins: []"
        );
        let msg = format!("{:?}", result.content);
        assert!(msg.contains("Device:CP"));
        assert!(msg.contains("no embedded definition"));
    }

    /// Fields follow the library anchor through the instance rotation (#101).
    /// `Device:R` anchors Reference beside the body at (2.032, 0) rotated 90°,
    /// so an upright resistor labels its right-hand side vertically and a
    /// 90°-rotated one labels above, horizontally — a fixed ±3.81 offset at 0°
    /// put both beside the wrong edge.
    async fn place_rotated_resistor(rotation: f64) -> (String, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rot.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 2.032 0 90))\n      (property \"Value\" \"R\" (at 0 0 90))\n    )\n  )\n)\n",
        )
        .unwrap();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                // Already on the 1.27mm grid the placement snaps to, so the
                // expected field coordinates are the anchors plus the origin.
                "x": 101.6,
                "y": 50.8,
                "rotation": rotation,
                "reference": "R1",
                "value": "10k"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch
            .symbols
            .iter()
            .find(|s| s.reference() == Some("R1"))
            .expect("placed resistor");
        let field = |name: &str| {
            cse::sexp::writer::write(
                &sym.properties
                    .iter()
                    .find(|p| p.name == name)
                    .unwrap()
                    .to_sexp(),
            )
        };
        (field("Reference"), field("Value"))
    }

    /// An anchor without its justification collides: this symbol anchors
    /// Reference and Value on the same row and relies on `justify left` to
    /// keep `U2` off `AP2112K-3.3`. Device:R, which the tests above place,
    /// justifies nothing — centred stays spelled as no `(justify …)`.
    #[tokio::test]
    async fn placement_carries_the_librarys_field_justification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("justify.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"Regulator_Linear:AP2112K-3.3\"\n      (property \"Reference\" \"U\" (at -5.08 5.715 0) (effects (font (size 1.27 1.27)) (justify left)))\n      (property \"Value\" \"AP2112K-3.3\" (at 0 5.715 0) (effects (font (size 1.27 1.27)) (justify left)))\n    )\n  )\n)\n",
        )
        .unwrap();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Regulator_Linear:AP2112K-3.3",
                "x": 101.6,
                "y": 50.8,
                "reference": "U2",
                "value": "AP2112K-3.3"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch
            .symbols
            .iter()
            .find(|s| s.reference() == Some("U2"))
            .expect("placed regulator");
        let field = |name: &str| {
            cse::sexp::writer::write(
                &sym.properties
                    .iter()
                    .find(|p| p.name == name)
                    .unwrap()
                    .to_sexp(),
            )
        };
        for name in ["Reference", "Value"] {
            let written = field(name);
            assert!(
                written.contains("(justify left)"),
                "{name} must keep the library's justification: {written}"
            );
        }
        // Hidden fields have no library anchor here, so they stay centred.
        assert!(!field("Footprint").contains("justify"));

        let (reference, _) = place_rotated_resistor(0.0).await;
        assert!(
            !reference.contains("justify"),
            "a centred library field must not gain a justify: {reference}"
        );
    }

    #[tokio::test]
    async fn placement_copies_library_datasheet_and_description() {
        // Pre-seed two real KiCad field shapes so this remains independent of
        // an installed symbol library: one URL and one no-datasheet sentinel.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 2.032 0 90))\n      (property \"Value\" \"R\" (at 0 0 90))\n      (property \"Datasheet\" \"https://example.com/resistor.pdf\" (at 0 0 0))\n      (property \"Description\" \"Resistor\" (at 0 0 0))\n    )\n    (symbol \"Device:C\"\n      (property \"Reference\" \"C\" (at 2.032 0 90))\n      (property \"Value\" \"C\" (at 0 0 90))\n      (property \"Datasheet\" \"~\" (at 0 0 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        for (lib_id, reference, x) in [("Device:R", "R1", 100.0), ("Device:C", "C1", 120.0)] {
            let result = handle_add_schematic_component(
                &json!({
                    "schematic": path.display().to_string(),
                    "lib_id": lib_id,
                    "reference": reference,
                    "x": x,
                    "y": 50.0
                }),
                &test_ctx(),
            )
            .await
            .unwrap();
            assert!(!result.is_error, "{result:?}");
        }

        let sch = cse::Schematic::load(&path).unwrap();
        let field = |reference: &str, name: &str| {
            sch.symbols
                .iter()
                .find(|symbol| symbol.reference() == Some(reference))
                .and_then(|symbol| symbol.properties.iter().find(|p| p.name == name))
                .map(|property| property.value.as_str())
        };
        assert_eq!(
            field("R1", "Datasheet"),
            Some("https://example.com/resistor.pdf")
        );
        assert_eq!(field("R1", "Description"), Some("Resistor"));
        assert_eq!(field("C1", "Datasheet"), Some("~"));
        assert_eq!(
            field("C1", "Description"),
            Some(""),
            "KiCad writes the mandatory Description field even when empty"
        );
    }

    #[tokio::test]
    async fn unrotated_symbol_takes_the_librarys_field_anchors() {
        let (reference, value) = place_rotated_resistor(0.0).await;
        // Same numbers eeschema writes for this library symbol at (100, 50).
        assert!(
            reference.contains("(at 103.632 50.8 90)"),
            "Reference belongs beside the body, rotated: {reference}"
        );
        assert!(
            value.contains("(at 101.6 50.8 90)"),
            "Value belongs on the body's axis, rotated: {value}"
        );
    }

    #[tokio::test]
    async fn rotated_symbol_carries_its_fields_around_with_it() {
        let (reference, value) = place_rotated_resistor(90.0).await;
        // The anchor rotates with the body: 2.032mm to the right of the
        // origin becomes 2.032mm above it. The stored angle stays at the
        // library's 90° — KiCad adds the symbol's rotation when it draws, so
        // this renders horizontally above the now-horizontal body.
        assert!(
            reference.contains("(at 101.6 48.768 90)"),
            "Reference must follow the rotated body: {reference}"
        );
        assert!(
            value.contains("(at 101.6 50.8 90)"),
            "Value must follow the rotated body: {value}"
        );
    }

    /// The repair path for sheets written before fields followed the library
    /// (#101): an instance whose fields sit at the old fixed offset is put
    /// back on its anchors, and a second run reports nothing left to move.
    #[tokio::test]
    async fn reset_field_positions_puts_stale_fields_back_on_their_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale-fields.kicad_sch");
        // A sheet as the old code wrote it: Reference at y-3.81 and Value at
        // y+3.81, while the library anchors them beside the body at 90.
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 2.032 0 90))\n      (property \"Value\" \"R\" (at 0 0 90))\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 101.6 50.8 0)\n    (unit 1)\n    (uuid \"bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee\")\n    (property \"Reference\" \"R1\" (at 101.6 46.99 0))\n    (property \"Value\" \"10k\" (at 101.6 54.61 0))\n  )\n)\n",
        )
        .unwrap();

        let args = json!({ "schematic": path.display().to_string() });
        let dry = handle_reset_schematic_field_positions(
            &json!({
                "schematic": path.display().to_string(), "dry_run": true
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &dry.content[0] else {
            panic!("expected text")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["moved"], json!(["R1.Reference", "R1.Value"]));
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("46.99"),
            "dry_run must not write"
        );

        let done = handle_reset_schematic_field_positions(&args, &test_ctx())
            .await
            .unwrap();
        assert!(!done.is_error, "{done:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch.symbols.by_reference("R1").expect("R1");
        let field = |name: &str| {
            cse::sexp::writer::write(
                &sym.properties
                    .iter()
                    .find(|p| p.name == name)
                    .unwrap()
                    .to_sexp(),
            )
        };
        assert!(
            field("Reference").contains("(at 103.632 50.8 90)"),
            "{}",
            field("Reference")
        );
        assert!(
            field("Value").contains("(at 101.6 50.8 90)"),
            "{}",
            field("Value")
        );

        // Idempotent: nothing left to move on a second pass.
        let again = handle_reset_schematic_field_positions(&args, &test_ctx())
            .await
            .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &again.content[0] else {
            panic!("expected text")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["moved"], json!([]));
        assert_eq!(body["unchanged"], json!(["R1.Reference", "R1.Value"]));
    }

    /// A reference that is not in the sheet is reported rather than silently
    /// doing nothing.
    #[tokio::test]
    async fn reset_field_positions_reports_an_unknown_reference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 2.032 0 90))\n      (property \"Value\" \"R\" (at 0 0 90))\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 101.6 50.8 0)\n    (unit 1)\n    (uuid \"bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee\")\n    (property \"Reference\" \"R1\" (at 101.6 46.99 0))\n    (property \"Value\" \"10k\" (at 101.6 54.61 0))\n  )\n)\n",
        )
        .unwrap();

        let result = handle_reset_schematic_field_positions(
            &json!({
                "schematic": path.display().to_string(), "references": ["R9"]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["not_found"], json!(["R9"]));
        assert_eq!(body["moved"], json!([]));
    }

    /// `not_found` is built from a HashSet, whose iteration order varies run
    /// to run — several unknown references would come back in a different
    /// order each call unless it is sorted.
    #[tokio::test]
    async fn reset_field_positions_reports_unknown_references_in_a_stable_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stable.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 2.032 0 90))\n      (property \"Value\" \"R\" (at 0 0 90))\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 101.6 50.8 0)\n    (unit 1)\n    (uuid \"bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee\")\n    (property \"Reference\" \"R1\" (at 101.6 46.99 0))\n    (property \"Value\" \"10k\" (at 101.6 54.61 0))\n  )\n)\n",
        )
        .unwrap();

        // Repeated because a HashSet of this size reorders between runs; an
        // unsorted list passes once and then does not.
        for _ in 0..8 {
            let result = handle_reset_schematic_field_positions(
                &json!({
                    "schematic": path.display().to_string(),
                    "references": ["R9", "R2", "U7", "C3"]
                }),
                &test_ctx(),
            )
            .await
            .unwrap();
            let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
                panic!("expected text")
            };
            let body: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(body["not_found"], json!(["C3", "R2", "R9", "U7"]));
        }
    }

    /// A field the library anchors but the placed symbol does not carry is
    /// reported, not skipped in silence — an unreported skip reads as "reset".
    #[tokio::test]
    async fn reset_field_positions_reports_a_field_the_symbol_does_not_have() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-value.kicad_sch");
        // The library anchors Reference and Value; the instance has only
        // Reference.
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\" (at 2.032 0 90))\n      (property \"Value\" \"R\" (at 0 0 90))\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 101.6 50.8 0)\n    (unit 1)\n    (uuid \"bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee\")\n    (property \"Reference\" \"R1\" (at 101.6 46.99 0))\n  )\n)\n",
        )
        .unwrap();

        let result = handle_reset_schematic_field_positions(
            &json!({ "schematic": path.display().to_string() }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["moved"], json!(["R1.Reference"]));
        assert_eq!(
            body["no_property"],
            json!(["R1.Value"]),
            "the skipped field must be accounted for: {body}"
        );
    }

    #[tokio::test]
    async fn add_schematic_component_hides_power_reference() {
        // Pre-seed lib_symbols so ensure_lib_symbol succeeds without KiCad.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("power-via-add.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:GND\"\n      (property \"Reference\" \"#PWR\" (at 0 0 0) (hide yes))\n      (property \"Value\" \"GND\" (at 0 0 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "power:GND",
                "x": 50.0,
                "y": 60.0,
                "reference": "#PWR010",
                "value": "GND"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch
            .symbols
            .iter()
            .find(|s| s.reference() == Some("#PWR010"))
            .expect("power instance");
        let ref_sexp = cse::sexp::writer::write(
            &sym.properties
                .iter()
                .find(|p| p.name == "Reference")
                .unwrap()
                .to_sexp(),
        );
        let hide_at = ref_sexp.find("(hide yes)").expect("property-level hide");
        let effects_at = ref_sexp.find("(effects").expect("effects");
        assert!(
            hide_at < effects_at,
            "power: via add_schematic_component must hide Reference like add_power_symbol: {ref_sexp}"
        );
        let val_sexp = cse::sexp::writer::write(
            &sym.properties
                .iter()
                .find(|p| p.name == "Value")
                .unwrap()
                .to_sexp(),
        );
        assert!(
            !val_sexp.contains("hide"),
            "Value stays visible: {val_sexp}"
        );
    }

    /// A schematic keeps its own copy of every symbol, so editing the library
    /// leaves the sheet drawing the old shape — what KiCad reports as
    /// "doesn't match copy in library".
    #[tokio::test]
    async fn update_symbols_from_library_refreshes_a_stale_embedded_copy() {
        // In the stub project dir, so its sym-lib-table shadows the global
        // `Device` entry — off a developer's KiCad install, that entry resolves
        // and the edit below then lands on a library nothing reads.
        let (symdir, _env) = stub_symbol_dir();
        let path = symdir.path().join("stale.kicad_sch");
        let ctx = test_ctx();
        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let placed = handle_add_schematic_component(
            &json!({ "schematic": path.display().to_string(), "lib_id": "Device:R",
                     "reference": "R1", "x": 100.0, "y": 100.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!placed.is_error, "{placed:?}");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("WIDENED"));

        // Edit the library out from under the schematic.
        let lib = symdir
            .path()
            .join("Device.kicad_symdir")
            .join("R.kicad_sym");
        let edited = std::fs::read_to_string(&lib).unwrap().replace(
            "(property \"Value\" \"R\"",
            "(property \"Value\" \"WIDENED\"",
        );
        std::fs::write(&lib, edited).unwrap();

        // A dry run reports the stale copy without touching the file.
        let dry = handle_update_symbols_from_library(
            &json!({ "schematic": path.display().to_string(), "dry_run": true }),
            &ctx,
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &dry.content[0] else {
            panic!("expected text")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["updated"], json!(["Device:R"]));
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("WIDENED"),
            "dry_run must not write"
        );

        let done = handle_update_symbols_from_library(
            &json!({ "schematic": path.display().to_string() }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!done.is_error, "{done:?}");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("WIDENED"), "{after}");
        assert!(konnect_sexp::parse_sexp(&after).is_ok(), "{after}");

        // Idempotent: a second run finds nothing to do.
        let again = handle_update_symbols_from_library(
            &json!({ "schematic": path.display().to_string() }),
            &ctx,
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &again.content[0] else {
            panic!("expected text")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["updated_count"], json!(0));
        assert_eq!(body["unchanged"], json!(["Device:R"]));
    }

    #[tokio::test]
    async fn update_symbols_from_library_rejects_an_unknown_reference() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.kicad_sch");
        let ctx = test_ctx();
        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_update_symbols_from_library(
            &json!({ "schematic": path.display().to_string(), "references": ["U9"] }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error, "{result:?}");
    }

    /// Wires and labels attach at pin coordinates, so a library edit that
    /// moved a pin would silently orphan them. The update is refused and
    /// reported instead, unless the caller opts in with allow_pin_moves
    /// (grafted from #177 by @JYPochez).
    #[tokio::test]
    async fn update_symbols_from_library_refuses_a_moved_pin_unless_allowed() {
        // In the stub project dir — see the stale-copy test above.
        let (symdir, _env) = stub_symbol_dir();
        let path = symdir.path().join("guarded.kicad_sch");
        let ctx = test_ctx();
        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let placed = handle_add_schematic_component(
            &json!({ "schematic": path.display().to_string(), "lib_id": "Device:R",
                     "reference": "R1", "x": 100.0, "y": 100.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!placed.is_error, "{placed:?}");

        // Move pin 2 in the library: (at 0 -3.81 90) → (at 0 -5.08 90).
        let lib = symdir
            .path()
            .join("Device.kicad_symdir")
            .join("R.kicad_sym");
        let edited = std::fs::read_to_string(&lib)
            .unwrap()
            .replace("(at 0 -3.81 90)", "(at 0 -5.08 90)");
        std::fs::write(&lib, edited).unwrap();

        let refused = handle_update_symbols_from_library(
            &json!({ "schematic": path.display().to_string() }),
            &ctx,
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &refused.content[0] else {
            panic!("expected text")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["updated_count"], json!(0), "{body}");
        assert_eq!(body["pins_moved"][0]["lib_id"], json!("Device:R"), "{body}");
        let detail = body["pins_moved"][0]["pins"][0].as_str().unwrap();
        assert!(detail.contains("pin 2"), "{detail}");
        assert!(
            detail.contains("-3.81") && detail.contains("-5.08"),
            "{detail}"
        );
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("-5.08"),
            "a refused update must not touch the schematic"
        );

        // The explicit opt-in updates it.
        let forced = handle_update_symbols_from_library(
            &json!({ "schematic": path.display().to_string(), "allow_pin_moves": true }),
            &ctx,
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &forced.content[0] else {
            panic!("expected text")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["updated"], json!(["Device:R"]), "{body}");
        assert_eq!(body["pins_moved"], json!([]), "{body}");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("(at 0 -5.08 90)"), "{after}");
        assert!(konnect_sexp::parse_sexp(&after).is_ok(), "{after}");
    }

    /// #203: annotating the same key twice must update the one property in
    /// place, not append a sibling — eeschema shows both and edits the wrong
    /// one, and a malformed duplicate survives save/reload.
    #[tokio::test]
    async fn add_component_annotation_updates_an_existing_key_in_place() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("annot.kicad_sch");
        let ctx = test_ctx();
        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        handle_add_schematic_component(
            &json!({ "schematic": path.display().to_string(), "lib_id": "Device:R",
                     "reference": "R1", "x": 100.0, "y": 100.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let first = handle_add_component_annotation(
            &json!({ "schematic": path.display().to_string(), "reference": "R1",
                     "key": "MPN", "value": "RC0402" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!first.is_error, "{first:?}");
        let second = handle_add_component_annotation(
            &json!({ "schematic": path.display().to_string(), "reference": "R1",
                     "key": "MPN", "value": "RC0603" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!second.is_error, "{second:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.matches("(property \"MPN\"").count(),
            1,
            "one MPN property, updated in place:
{after}"
        );
        assert!(after.contains("RC0603"), "{after}");
        assert!(
            !after.contains("RC0402"),
            "old value must be gone:
{after}"
        );
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    /// The old path hardcoded (at 0 0 0) — the annotation rendered at the
    /// sheet origin, far from its symbol. The shared property writer anchors
    /// each property on its own placed unit.
    #[tokio::test]
    async fn add_component_annotation_anchors_at_the_symbol_not_the_origin() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anchor.kicad_sch");
        let ctx = test_ctx();
        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        handle_add_schematic_component(
            &json!({ "schematic": path.display().to_string(), "lib_id": "Device:R",
                     "reference": "R1", "x": 100.0, "y": 100.0 }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_component_annotation(
            &json!({ "schematic": path.display().to_string(), "reference": "R1",
                     "key": "MPN", "value": "RC0402" }),
            &ctx,
        )
        .await
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        let prop_at = after.find("(property \"MPN\"").unwrap();
        let prop_block = &after[prop_at..prop_at + 120];
        assert!(
            !prop_block.contains("(at 0 0 0)"),
            "annotation must anchor near its symbol, not the origin:
{prop_block}"
        );
        assert!(prop_block.contains("(at 100"), "{prop_block}");
    }

    /// Reference/Value/Footprint/Datasheet have dedicated parameters with
    /// their own side effects (#157's instances rewrite); annotating them
    /// would bypass those.
    #[tokio::test]
    async fn add_component_annotation_refuses_reserved_keys() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reserved.kicad_sch");
        let ctx = test_ctx();
        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        handle_add_schematic_component(
            &json!({ "schematic": path.display().to_string(), "lib_id": "Device:R",
                     "reference": "R1", "x": 100.0, "y": 100.0 }),
            &ctx,
        )
        .await
        .unwrap();
        let result = handle_add_component_annotation(
            &json!({ "schematic": path.display().to_string(), "reference": "R1",
                     "key": "Reference", "value": "R9" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error, "{result:?}");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text")
        };
        assert!(text.contains("edit_schematic_component"), "{text}");
    }

    /// A removed pin is as dangerous as a moved one — whatever attached to it
    /// dangles. Same guard, different message.
    #[tokio::test]
    async fn update_symbols_from_library_refuses_a_removed_pin() {
        // In the stub project dir — see the stale-copy test above.
        let (symdir, _env) = stub_symbol_dir();
        let path = symdir.path().join("shrunk.kicad_sch");
        let ctx = test_ctx();
        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        handle_add_schematic_component(
            &json!({ "schematic": path.display().to_string(), "lib_id": "Device:R",
                     "reference": "R1", "x": 100.0, "y": 100.0 }),
            &ctx,
        )
        .await
        .unwrap();

        // Delete pin 2 from the library definition entirely.
        let lib = symdir
            .path()
            .join("Device.kicad_symdir")
            .join("R.kicad_sym");
        let content = std::fs::read_to_string(&lib).unwrap();
        let start = content.find("(pin passive line (at 0 -3.81 90)").unwrap();
        // Cut up to the unit subsymbol's closer, "\n\t\t)" — the pin's own
        // closer is "\n\t\t\t)", which this pattern cannot match early.
        let end = start + content[start..].find("\n\t\t)").unwrap();
        let mut edited = content;
        edited.replace_range(start..end, "");
        std::fs::write(&lib, edited).unwrap();

        let refused = handle_update_symbols_from_library(
            &json!({ "schematic": path.display().to_string() }),
            &ctx,
        )
        .await
        .unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &refused.content[0] else {
            panic!("expected text")
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["updated_count"], json!(0), "{body}");
        let detail = body["pins_moved"][0]["pins"][0].as_str().unwrap();
        assert!(
            detail.contains("pin 2") && detail.contains("removed"),
            "{detail}"
        );
    }
}

/// `edit_schematic_component` had two independent defects, both of which
/// reported success: `fields` was declared in the schema and never read
/// (#158), and `new_reference` rewrote only the rendered property, leaving the
/// instances path — which is where KiCad reads the designator for the netlist
/// — on the old value (#157).
#[cfg(test)]
mod edit_component_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    /// One R1, with an instances path, as eeschema writes it.
    const SCH: &str = "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 50 60 0)\n\t\t(unit 1)\n\t\t(uuid \"sym-1\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 52 58 0)\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 52 62 0)\n\t\t)\n\t\t(instances\n\t\t\t(project \"proj\"\n\t\t\t\t(path \"/root\"\n\t\t\t\t\t(reference \"R1\") (unit 1)\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n";

    async fn edit(args: serde_json::Value) -> (String, String) {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let mut args = args;
        args["schematic"] = json!(f.path().to_str().unwrap());

        let def = tools()
            .into_iter()
            .find(|t| t.name == "edit_schematic_component")
            .unwrap();
        let ctx = Arc::new(ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        ));
        let res = (def.handler)(&args, ctx).await.unwrap();
        let reply = match res.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        (std::fs::read_to_string(f.path()).unwrap(), reply)
    }

    /// #157: the rename must reach the instances path, not just the property.
    #[tokio::test]
    async fn renaming_a_reference_rewrites_the_instances_path() {
        let (out, _) = edit(json!({ "reference": "R1", "new_reference": "R7" })).await;
        assert!(
            out.contains("(property \"Reference\" \"R7\""),
            "property renamed:\n{out}"
        );
        assert!(
            out.contains("(reference \"R7\")"),
            "instances path must carry the new designator, or the netlist \
             ignores the rename:\n{out}"
        );
        assert!(
            !out.contains("(reference \"R1\")"),
            "no instances entry may keep the old designator:\n{out}"
        );
    }

    /// #158: a custom field that does not exist yet must be created.
    #[tokio::test]
    async fn a_new_custom_field_is_written_into_the_symbol() {
        let (out, reply) = edit(json!({
            "reference": "R1",
            "fields": { "MPN": "RC0402FR-0710KL" }
        }))
        .await;
        assert!(
            out.contains("(property \"MPN\" \"RC0402FR-0710KL\""),
            "custom field must land in the file:\n{out}"
        );
        assert!(
            out.contains("(hide yes)"),
            "a custom field is data, not sheet artwork:\n{out}"
        );
        assert!(reply.contains("MPN"), "the reply must report it: {reply}");
        // Anchored on the symbol, not defaulted to the sheet origin (#95).
        assert!(
            !out.contains("(property \"MPN\" \"RC0402FR-0710KL\"\n\t\t\t(at 0 0 0)"),
            "must not land at the sheet origin:\n{out}"
        );
    }

    /// #158: an existing custom field is updated rather than duplicated.
    #[tokio::test]
    async fn an_existing_custom_field_is_updated_not_duplicated() {
        let (out, _) = edit(json!({ "reference": "R1", "fields": { "MPN": "first" } })).await;
        assert_eq!(out.matches("(property \"MPN\"").count(), 1);

        // Value is a first-class parameter, so it must be updated in place.
        let (out2, _) = edit(json!({ "reference": "R1", "value": "22k" })).await;
        assert_eq!(out2.matches("(property \"Value\"").count(), 1, "{out2}");
        assert!(out2.contains("(property \"Value\" \"22k\""), "{out2}");
    }

    /// The defect that made #158 invisible: with `fields` unread, both
    /// `changed` and `errors` came back empty, so the no-op guard never fired
    /// and the call reported success having done nothing.
    #[tokio::test]
    async fn a_fields_only_call_no_longer_reports_an_empty_success() {
        let (_, reply) = edit(json!({
            "reference": "R1",
            "fields": { "MPN": "RC0402FR-0710KL" }
        }))
        .await;
        assert!(
            !reply.contains("\"changes\":[]"),
            "a fields-only call must not report an empty change set: {reply}"
        );
    }

    /// Reserved names belong to their own parameters — routing Reference
    /// through `fields` would skip the instances rewrite and silently
    /// reintroduce #157.
    #[tokio::test]
    async fn reserved_names_are_refused_inside_fields() {
        let (out, reply) = edit(json!({
            "reference": "R1",
            "fields": { "Reference": "R9" }
        }))
        .await;
        assert!(
            out.contains("(property \"Reference\" \"R1\""),
            "the designator must be untouched:\n{out}"
        );
        assert!(
            reply.contains("Reference"),
            "the refusal is reported: {reply}"
        );
    }
}

#[cfg(test)]
mod page_tests {
    use super::{tools, PAPER_SIZES};
    use crate::tools::ToolContext;
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    async fn set_page(body: &str, size: &str, portrait: bool) -> String {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        let def = tools()
            .into_iter()
            .find(|t| t.name == "set_schematic_page")
            .unwrap();
        let cfg = crate::tools::ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        };
        let ctx = Arc::new(ToolContext::new(
            cfg,
            Arc::new(crate::router::ToolRouter::new()),
        ));
        let args = json!({
            "schematic": f.path().to_str().unwrap(),
            "size": size, "portrait": portrait
        });
        (def.handler)(&args, ctx).await.unwrap();
        std::fs::read_to_string(f.path()).unwrap()
    }

    const WITH_PAPER: &str =
        "(kicad_sch\n  (version 20260306)\n  (uuid \"root\")\n  (paper \"A4\")\n  (symbol)\n)\n";
    const NO_PAPER: &str = "(kicad_sch\n  (version 20260306)\n  (uuid \"root\")\n  (symbol)\n)\n";

    #[tokio::test]
    async fn replaces_an_existing_paper_node() {
        let out = set_page(WITH_PAPER, "A2", false).await;
        assert!(out.contains("(paper \"A2\")"), "got {out}");
        assert!(!out.contains("A4"), "old size must be gone: {out}");
        assert_eq!(out.matches("(paper").count(), 1);
    }

    /// A sheet written without a paper node — KiCad treats it as A4 — takes the
    /// new one in the header, before any element.
    #[tokio::test]
    async fn inserts_when_absent_and_stays_in_the_header() {
        let out = set_page(NO_PAPER, "A3", false).await;
        assert!(out.contains("(paper \"A3\")"), "got {out}");
        assert!(out.find("(paper").unwrap() < out.find("(symbol").unwrap());
    }

    #[tokio::test]
    async fn portrait_is_marked_on_the_node() {
        let out = set_page(WITH_PAPER, "A3", true).await;
        assert!(out.contains("(paper \"A3\" portrait)"), "got {out}");
    }

    #[tokio::test]
    async fn unknown_size_leaves_the_file_alone() {
        let out = set_page(WITH_PAPER, "A9", false).await;
        assert!(
            out.contains("(paper \"A4\")"),
            "must not have written: {out}"
        );
    }

    #[test]
    fn paper_table_is_landscape_and_unique() {
        let mut names: Vec<_> = PAPER_SIZES.iter().map(|(n, _, _)| *n).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate paper size name");
        for (n, w, h) in PAPER_SIZES {
            assert!(w > h, "{n} is listed portrait; the table is landscape");
        }
    }
}

#[cfg(test)]
mod schematic_view_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn the_view_slot_is_stable_for_one_schematic() {
        let sheet = Path::new("/projects/alpha/power.kicad_sch");
        assert_eq!(schematic_view_dir(sheet), schematic_view_dir(sheet));
    }

    /// The old handler used a fresh uuid per call, which is why nothing could
    /// be returned without leaking a directory per call. Deriving the slot from
    /// the path bounds it to one per schematic — but it must be the whole path,
    /// or two projects with a `power.kicad_sch` would overwrite each other.
    #[test]
    fn two_sheets_sharing_a_stem_get_different_slots() {
        let alpha = schematic_view_dir(Path::new("/projects/alpha/power.kicad_sch"));
        let beta = schematic_view_dir(Path::new("/projects/beta/power.kicad_sch"));
        assert_ne!(alpha, beta);
    }

    #[test]
    fn the_view_slot_lives_under_the_system_temp_dir() {
        let dir = schematic_view_dir(Path::new("/projects/alpha/power.kicad_sch"));
        assert!(
            dir.starts_with(std::env::temp_dir()),
            "views must not be written next to the caller's project: {}",
            dir.display()
        );
    }

    /// The reported defect, end to end: the tool used to render the SVG, read
    /// its length, delete it, and report "The SVG file has been generated".
    /// Needs a real kicad-cli, so it is ignored like the other live tests.
    #[tokio::test]
    #[ignore = "needs a real kicad-cli on PATH"]
    async fn the_rendered_svg_survives_the_call() {
        let tmp = tempfile::tempdir().unwrap();
        let sheet = tmp.path().join("view.kicad_sch");
        std::fs::write(
            &sheet,
            "(kicad_sch\n\t(version 20260101)\n\t(generator \"eeschema\")\n\t(uuid \"view-0001\")\n\t(paper \"A4\")\n\t(lib_symbols)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n",
        )
        .unwrap();

        let cfg = crate::tools::ServerConfig {
            kicad_cli: std::env::var("KICAD_CLI").unwrap_or_else(|_| "kicad-cli".to_string()),
            kicad_binary: String::new(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        };
        let ctx = ToolContext::new(cfg, std::sync::Arc::new(crate::router::ToolRouter::new()));
        let args = json!({ "schematic": sheet.display().to_string() });

        let first = handle_get_schematic_view(&args, &ctx).await.unwrap();
        assert!(!first.is_error, "{:?}", first.content);
        let body: serde_json::Value = match &first.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => {
                serde_json::from_str(text).expect("the result is JSON, not prose")
            }
            _ => panic!("expected text content"),
        };

        let svg = PathBuf::from(body["svg"].as_str().expect("a path to the SVG"));
        assert!(
            svg.exists(),
            "the file the tool names must still be there: {}",
            svg.display()
        );
        assert_eq!(
            std::fs::metadata(&svg).unwrap().len(),
            body["bytes"].as_u64().unwrap(),
            "the reported size is the file's size"
        );
        assert_eq!(body["format"], "svg");

        // The invisible text layer this SVG is also useful for.
        let content = std::fs::read_to_string(&svg).unwrap();
        assert!(
            content.contains("opacity=\"0\""),
            "kicad-cli writes a machine-readable text layer"
        );

        // A second view reuses the slot instead of leaving a directory behind.
        let second = handle_get_schematic_view(&args, &ctx).await.unwrap();
        let body2: serde_json::Value = match &second.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text content"),
        };
        assert_eq!(body2["svg"], body["svg"]);
    }
}

#[cfg(test)]
mod move_connected_tests {
    use super::*;

    /// #315: this tool silently delegated to the plain move since the first
    /// release — symbol moved, wires stayed, success reported. Until the
    /// wire-carrying move exists it must refuse, naming the alternative.
    #[tokio::test]
    async fn move_connected_refuses_instead_of_faking_success() {
        let result = handle_move_connected(
            &serde_json::json!({
                "schematic": "unused.kicad_sch",
                "reference": "R1", "x": 10.0, "y": 10.0
            }),
            &crate::tools::ToolContext::new(
                crate::tools::ServerConfig {
                    kicad_cli: String::new(),
                    kicad_binary: String::new(),
                    ipc_address: String::new(),
                    project_dir: None,
                    jlcpcb_db_path: None,
                    auto_load_toolsets: false,
                    eager_toolsets: false,
                },
                std::sync::Arc::new(crate::router::ToolRouter::new()),
            ),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text");
        };
        assert!(
            text.contains("move_schematic_component"),
            "must name the working alternative: {text}"
        );
    }
}

#[cfg(test)]
mod component_delete_connectivity_tests {
    use super::*;
    use crate::mcp::{error::extract_error_kind, protocol::ToolContent};
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    const CONNECTIVITY: &str = include_str!("../../tests/fixtures/junction_reconcile.kicad_sch");

    fn context() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn fixture(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("delete.kicad_sch");
        std::fs::write(&path, content).unwrap();
        (directory, path)
    }

    fn body(result: &CallToolResult) -> serde_json::Value {
        assert!(!result.is_error, "{result:?}");
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        serde_json::from_str(text).unwrap()
    }

    fn has_junction(content: &str, x: f64, y: f64) -> bool {
        let tree = parse_sexp(content).unwrap();
        konnect_sexp::schematic::extract_junctions(&tree)
            .iter()
            .any(|&(jx, jy)| konnect_sexp::geometry::points_coincident(x, y, jx, jy, 0.01))
    }

    #[tokio::test]
    async fn deleting_a_pin_only_dot_prunes_it_but_preserves_an_unrelated_wire_t() {
        let (_directory, path) = fixture(CONNECTIVITY);
        let result = handle_delete_schematic_component(
            &json!({ "schematic": path, "reference": "R1" }),
            &context(),
        )
        .await
        .unwrap();
        let response = body(&result);

        assert_eq!(response["deleted_units"], 1);
        assert_eq!(response["junctions_pruned_count"], 1);
        let committed = std::fs::read_to_string(&path).unwrap();
        assert!(!has_junction(&committed, 120.65, 139.7));
        assert!(
            has_junction(&committed, 120.65, 170.18),
            "the two-wire T remains justified independently of R1"
        );
    }

    #[tokio::test]
    async fn attached_no_connect_is_removed_and_unrelated_marker_survives() {
        let unrelated = "\t(no_connect\n\t\t(at 250 250)\n\t\t(uuid \"unrelated-marker\")\n\t)\n";
        let closing = CONNECTIVITY.rfind("\n)").unwrap();
        let original = format!(
            "{}{unrelated}{}",
            &CONNECTIVITY[..closing + 1],
            &CONNECTIVITY[closing + 1..]
        );
        assert!(original.contains("unrelated-marker"));
        let (_directory, path) = fixture(&original);
        let result = handle_delete_schematic_component(
            &json!({ "schematic": path, "reference": "R3" }),
            &context(),
        )
        .await
        .unwrap();
        let response = body(&result);

        assert_eq!(response["removed_no_connects_count"], 1);
        assert_eq!(
            response["removed_no_connect_uuids"][0],
            "3f9dbc19-858e-4bf8-b937-b169159de4c8"
        );
        let committed = std::fs::read_to_string(&path).unwrap();
        assert!(!committed.contains("3f9dbc19-858e-4bf8-b937-b169159de4c8"));
        assert!(committed.contains("unrelated-marker"));
    }

    #[tokio::test]
    async fn attached_no_connect_survives_when_a_remaining_pin_shares_the_point() {
        let original =
            CONNECTIVITY.replace("\t\t(at 120.65 135.89 0)\n", "\t\t(at 190.5 196.85 0)\n");
        assert_ne!(original, CONNECTIVITY);
        let (_directory, path) = fixture(&original);
        let result = handle_delete_schematic_component(
            &json!({ "schematic": path, "reference": "R3" }),
            &context(),
        )
        .await
        .unwrap();
        let response = body(&result);

        assert_eq!(response["removed_no_connects_count"], 0);
        let committed = std::fs::read_to_string(&path).unwrap();
        assert!(committed.contains("3f9dbc19-858e-4bf8-b937-b169159de4c8"));
    }

    #[tokio::test]
    async fn missing_reference_is_structured_stale_and_does_not_write() {
        let (_directory, path) = fixture(CONNECTIVITY);
        let result = handle_delete_schematic_component(
            &json!({ "schematic": path, "reference": "R404" }),
            &context(),
        )
        .await
        .unwrap();

        assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), CONNECTIVITY);
    }

    #[tokio::test]
    async fn unresolved_pin_geometry_is_stale_and_does_not_write() {
        let original = "(kicad_sch\n  (version 20260306)\n  (uuid \"root\")\n  (lib_symbols)\n  (symbol\n    (lib_id \"Missing:Part\")\n    (at 10 10 0)\n    (unit 1)\n    (uuid \"missing-lib\")\n    (property \"Reference\" \"U1\")\n  )\n)\n";
        let (_directory, path) = fixture(original);
        let result = handle_delete_schematic_component(
            &json!({ "schematic": path, "reference": "U1" }),
            &context(),
        )
        .await
        .unwrap();

        assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn attached_marker_without_uuid_is_stale_and_does_not_write() {
        let original =
            CONNECTIVITY.replace("\t\t(uuid \"3f9dbc19-858e-4bf8-b937-b169159de4c8\")\n", "");
        assert_ne!(original, CONNECTIVITY);
        let (_directory, path) = fixture(&original);
        let result = handle_delete_schematic_component(
            &json!({ "schematic": path, "reference": "R3" }),
            &context(),
        )
        .await
        .unwrap();

        assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn duplicate_reference_and_unit_is_ambiguous_and_does_not_write() {
        let original = CONNECTIVITY.replacen(
            "(property \"Reference\" \"R3\"",
            "(property \"Reference\" \"R1\"",
            1,
        );
        let (_directory, path) = fixture(&original);
        let result = handle_delete_schematic_component(
            &json!({ "schematic": path, "reference": "R1" }),
            &context(),
        )
        .await
        .unwrap();

        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("ambiguous_target")
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn stale_revision_refuses_the_prepared_delete_without_overwriting() {
        let (_directory, path) = fixture(CONNECTIVITY);
        let plan = plan_component_deletion(&path, CONNECTIVITY, "R1").unwrap();
        let newer = CONNECTIVITY.replace("(paper \"A4\")", "(paper \"A3\")");
        assert_ne!(newer, CONNECTIVITY);
        std::fs::write(&path, &newer).unwrap();

        let error = commit_command(&path, &plan.command).unwrap_err();
        let refusal = component_delete_commit_refusal(&path, &error).unwrap();
        assert_eq!(
            extract_error_kind(&refusal).as_deref(),
            Some("stale_target")
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), newer);
    }

    #[tokio::test]
    async fn kicad_lock_is_structured_stale_and_preserves_the_file() {
        let (_directory, path) = fixture(CONNECTIVITY);
        let lock = path.with_file_name(format!(
            "~{}.lck",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&lock, "locked").unwrap();
        let result = handle_delete_schematic_component(
            &json!({ "schematic": path, "reference": "R1" }),
            &context(),
        )
        .await
        .unwrap();

        assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), CONNECTIVITY);
    }
}

#[cfg(test)]
mod multi_unit_component_tests {
    use super::*;
    use crate::mcp::{error::extract_error_kind, protocol::ToolContent};
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    const SCHEMATIC: &str = r#"(kicad_sch
  (version 20260306)
  (uuid "11111111-1111-4111-8111-111111111111")
  (lib_symbols
    (symbol "Test:DUAL"
      (symbol "DUAL_1_1"
        (pin input line (at 0 0 0) (length 0) (name "A") (number "1"))
      )
      (symbol "DUAL_2_1"
        (pin output line (at 0 0 0) (length 0) (name "Y") (number "2"))
      )
    )
    (symbol "Test:DUAL_NEW"
      (symbol "DUAL_NEW_1_1"
        (pin input line (at 0 0 0) (length 0) (name "A") (number "1"))
      )
      (symbol "DUAL_NEW_2_1"
        (pin output line (at 0 0 0) (length 0) (name "Y") (number "2"))
      )
    )
  )
  (symbol
    (lib_id "Test:DUAL")
    (at 100 100 0)
    (unit 1)
    (uuid "22222222-2222-4222-8222-222222222222")
    (property "Reference" "U1" (at 100 98 0))
    (property "Value" "OLD" (at 100 102 0))
    (property "Footprint" "" (at 100 100 0))
    (property "Datasheet" "" (at 100 100 0))
    (property "Note" "OLD" (at 100 100 0) (hide yes))
    (instances
      (project "multi"
        (path "/11111111-1111-4111-8111-111111111111"
          (reference "U1")
          (unit 1)
        )
      )
    )
  )
  (symbol
    (lib_id "Test:DUAL")
    (at 100 120 180)
    (unit 2)
    (uuid "33333333-3333-4333-8333-333333333333")
    (property "Reference" "U1" (at 100 118 0))
    (property "Value" "OLD" (at 100 122 0))
    (property "Footprint" "" (at 100 120 0))
    (property "Datasheet" "" (at 100 120 0))
    (instances
      (project "multi"
        (path "/11111111-1111-4111-8111-111111111111"
          (reference "U1")
          (unit 2)
        )
      )
    )
  )
  (sheet_instances (path "/" (page "1")))
)
"#;

    fn context() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("multi.kicad_sch");
        std::fs::write(&path, SCHEMATIC).unwrap();
        (directory, path)
    }

    /// A real eeschema save (KiCad's ecc83 demo): tabs, CRLF, and U1 placed
    /// as units 2 and 3 of the embedded `ecc83-pp:ECC83` dual triode. The
    /// hand-written `SCHEMATIC` above shares this module's own serialization
    /// habits, so only this file exercises the indentation- and
    /// dialect-matching branches against what KiCad actually writes.
    fn eeschema_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ecc83.kicad_sch");
        std::fs::write(
            &path,
            include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch"),
        )
        .unwrap();
        (directory, path)
    }

    fn native_readback_intent_mismatch(field: &str) {
        let (_directory, path) = eeschema_fixture();
        let mut committed = cse::Schematic::load(&path).unwrap();
        let target = component_target_from_source(&path, &committed.to_source(), "U1").unwrap();
        assert!(verified_component_readback(&path, &committed, &target).is_ok());
        // Keep UUID and Reference intact. Change every unit's hierarchy together
        // so the expected-path comparison, not a cross-unit conflict, must fire.
        for symbol in committed
            .symbols
            .iter_mut()
            .filter(|symbol| symbol.reference() == Some("U1"))
        {
            match field {
                "unit" => {
                    symbol.unit += 10;
                    for (project, instance_path) in symbol.instance_paths() {
                        symbol.set_instance_path(&project, &instance_path, "U1", symbol.unit);
                    }
                }
                "lib_id" => symbol.lib_id = "Device:WRONG".to_owned(),
                "x" => symbol.at.x += 1.27,
                "y" => symbol.at.y += 1.27,
                "rotation" => symbol.at.rotation = Some(symbol.at.rotation.unwrap_or(0.0) + 90.0),
                "project" | "path" => {
                    let previous = symbol.instance_paths();
                    symbol
                        .raw_sub_nodes
                        .retain(|node| node.tag() != Some("instances"));
                    for (project, instance_path) in previous {
                        symbol.set_instance_path(
                            if field == "project" {
                                "wrong-project"
                            } else {
                                &project
                            },
                            if field == "path" {
                                "/wrong/path"
                            } else {
                                &instance_path
                            },
                            "U1",
                            symbol.unit,
                        );
                    }
                }
                _ => unreachable!(),
            }
        }
        committed.overwrite().unwrap();
        let reloaded = cse::Schematic::load(&path).unwrap();
        let error = verified_component_readback(&path, &reloaded, &target).unwrap_err();
        assert_eq!(
            extract_error_kind(&error).as_deref(),
            Some("stale_target"),
            "{field}: {error:?}"
        );
    }

    #[test]
    fn native_readback_intent_unit() {
        native_readback_intent_mismatch("unit");
    }
    #[test]
    fn native_readback_intent_library() {
        native_readback_intent_mismatch("lib_id");
    }
    #[test]
    fn native_readback_intent_x() {
        native_readback_intent_mismatch("x");
    }
    #[test]
    fn native_readback_intent_y() {
        native_readback_intent_mismatch("y");
    }
    #[test]
    fn native_readback_intent_rotation() {
        native_readback_intent_mismatch("rotation");
    }
    #[test]
    fn native_readback_intent_project() {
        native_readback_intent_mismatch("project");
    }
    #[test]
    fn native_readback_intent_path() {
        native_readback_intent_mismatch("path");
    }

    #[test]
    fn native_readback_intent_requested_properties() {
        for field in ["Value", "Footprint", "Datasheet", "MPN", "Group"] {
            let (_directory, path) = eeschema_fixture();
            let mut committed = cse::Schematic::load(&path).unwrap();
            let target = component_target_from_source(&path, &committed.to_source(), "U1")
                .unwrap()
                .with_fields(&BTreeMap::from([(
                    field.to_owned(),
                    "requested-value".to_owned(),
                )]));
            for symbol in committed
                .symbols
                .iter_mut()
                .filter(|symbol| symbol.reference() == Some("U1"))
            {
                symbol.set_property(field, "requested-value");
            }
            committed.overwrite().unwrap();
            assert!(verified_component_readback(
                &path,
                &cse::Schematic::load(&path).unwrap(),
                &target
            )
            .is_ok());
            committed
                .symbols
                .iter_mut()
                .find(|symbol| symbol.reference() == Some("U1"))
                .unwrap()
                .set_property(field, "wrong-value");
            committed.overwrite().unwrap();
            let error =
                verified_component_readback(&path, &cse::Schematic::load(&path).unwrap(), &target)
                    .unwrap_err();
            assert_eq!(
                extract_error_kind(&error).as_deref(),
                Some("stale_target"),
                "{field}"
            );
        }
    }

    #[test]
    fn native_readback_instance_conflicts() {
        for corruption in ["project", "duplicate", "unit", "cross-unit"] {
            let (_directory, path) = eeschema_fixture();
            let mut committed = cse::Schematic::load(&path).unwrap();
            let target = component_target_from_source(&path, &committed.to_source(), "U1").unwrap();
            let symbol = committed
                .symbols
                .iter_mut()
                .find(|symbol| symbol.reference() == Some("U1"))
                .unwrap();
            let (project, instance_path) = symbol.instance_paths()[0].clone();
            match corruption {
                "project" => {
                    symbol.set_instance_path("other-project", &instance_path, "U1", symbol.unit)
                }
                "unit" => symbol.set_instance_path(&project, &instance_path, "U1", symbol.unit + 1),
                "cross-unit" => {
                    symbol
                        .raw_sub_nodes
                        .retain(|node| node.tag() != Some("instances"));
                    symbol.set_instance_path(&project, "/other/path", "U1", symbol.unit);
                }
                "duplicate" => {
                    let instances = symbol
                        .raw_sub_nodes
                        .iter()
                        .find(|node| node.tag() == Some("instances"))
                        .unwrap()
                        .clone();
                    symbol.raw_sub_nodes.push(instances);
                }
                _ => unreachable!(),
            }
            if corruption != "cross-unit" {
                let error = checked_instance_paths(&path, symbol)
                    .unwrap_err()
                    .into_result();
                assert_eq!(
                    extract_error_kind(&error).as_deref(),
                    Some("ambiguous_target"),
                    "{corruption}"
                );
            }
            committed.overwrite().unwrap();
            let reloaded = cse::Schematic::load(&path).unwrap();
            let error = verified_component_readback(&path, &reloaded, &target).unwrap_err();
            assert_eq!(
                extract_error_kind(&error).as_deref(),
                Some("ambiguous_target"),
                "{corruption}"
            );
            let preflight = component_target_from_source(&path, &reloaded.to_source(), "U1")
                .unwrap_err()
                .into_result();
            assert_eq!(
                extract_error_kind(&preflight).as_deref(),
                Some("ambiguous_target"),
                "{corruption}"
            );
        }
    }

    #[tokio::test]
    async fn native_mutations_verify_each_units_intended_state() {
        let (_directory, path) = eeschema_fixture();
        body(
            handle_move_schematic_component(
                &json!({"schematic": path, "reference": "U1", "x": 110.1, "y": 120.2}),
                &context(),
            )
            .await
            .unwrap(),
        );
        body(
            handle_rotate_schematic_component(
                &json!({"schematic": path, "reference": "U1", "rotation": 450.0}),
                &context(),
            )
            .await
            .unwrap(),
        );
        let edited = body(handle_edit_schematic_component(&json!({"schematic": path, "reference": "U1", "new_reference": "U99", "value": "updated", "footprint": "Package:New", "datasheet": "https://example.invalid/datasheet", "fields": {"MPN": "requested"}}), &context()).await.unwrap());
        assert_eq!(edited["reference"], "U99");
        for unit in edited["units"].as_array().unwrap() {
            assert_eq!(unit["fields"]["MPN"], "requested");
            assert_eq!(unit["fields"]["Value"], "updated");
            assert_eq!(unit["fields"]["Footprint"], "Package:New");
            assert_eq!(
                unit["fields"]["Datasheet"],
                "https://example.invalid/datasheet"
            );
        }
        let grouped = body(
            handle_group_components(
                &json!({"schematic": path, "references": ["U99"], "group_name": "native-group"}),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(grouped["grouped_count"], 1);
    }

    #[tokio::test]
    async fn a_real_eeschema_multi_unit_component_is_seen_whole() {
        let (_directory, path) = eeschema_fixture();
        let result = body(
            handle_get_schematic_component(
                &json!({ "schematic": path, "reference": "U1" }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            result["unit_count"], 3,
            "U1 is placed as both triodes plus the heater unit"
        );
        let mut units: Vec<i64> = result["units"]
            .as_array()
            .unwrap()
            .iter()
            .map(|unit| unit["unit"].as_i64().unwrap())
            .collect();
        units.sort_unstable();
        assert_eq!(units, [1, 2, 3]);
    }

    #[tokio::test]
    async fn annotating_a_real_eeschema_file_reaches_every_unit_in_its_own_dialect() {
        let (_directory, path) = eeschema_fixture();
        let result = body(
            handle_add_component_annotation(
                &json!({
                    "schematic": path,
                    "reference": "U1",
                    "key": "MPN",
                    "value": "ECC83-JJ"
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["added_units"], 3, "{result}");
        let source = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            source.matches("(property \"MPN\" \"ECC83-JJ\"").count(),
            3,
            "the property lands in every unit block"
        );
        // The inserted lines must follow the file's own indentation (tabs) —
        // a 2-space insert in a tab-indented eeschema file is exactly the
        // drift the KiCad-authored fixture exists to catch.
        for line in source.lines().filter(|line| line.contains("\"MPN\"")) {
            assert!(
                line.starts_with('\t'),
                "inserted property must be tab-indented like its file: {line:?}"
            );
        }
    }

    fn body(result: CallToolResult) -> serde_json::Value {
        assert!(!result.is_error, "mutation unexpectedly failed: {result:?}");
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        serde_json::from_str(text).unwrap()
    }

    fn instances(path: &std::path::Path) -> Vec<konnect_sexp::schematic::SymbolInstance> {
        let (_, tree) = read_schematic(path).unwrap();
        extract_symbol_instances(&tree)
            .into_iter()
            .filter(|instance| instance.reference == "U1" || instance.reference == "U9")
            .collect()
    }

    #[tokio::test]
    async fn delete_removes_every_placed_unit() {
        let (_directory, path) = fixture();
        let result = body(
            handle_delete_schematic_component(
                &json!({ "schematic": path, "reference": "U1" }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["deleted_units"], 2);
        assert!(instances(&path).is_empty());
    }

    #[tokio::test]
    async fn move_translates_every_unit_by_one_shared_delta() {
        let (_directory, path) = fixture();
        let result = body(
            handle_move_schematic_component(
                &json!({
                    "schematic": path,
                    "reference": "U1",
                    "x": 110.0,
                    "y": 110.0
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["moved_units"], 2);
        assert_eq!(result["schematic"], path.display().to_string());
        assert_eq!(result["units"].as_array().unwrap().len(), 2);
        assert_eq!(result["placements"], result["units"]);
        let mut placed = instances(&path);
        placed.sort_by_key(|instance| instance.unit);
        assert!((placed[0].x - placed[1].x).abs() < 0.001);
        assert!(((placed[1].y - placed[0].y) - 20.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn rotate_preserves_the_units_relative_orientation() {
        let (_directory, path) = fixture();
        let result = body(
            handle_rotate_schematic_component(
                &json!({
                    "schematic": path,
                    "reference": "U1",
                    "rotation": 90.0
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["rotated_units"], 2);
        assert_eq!(result["rotation"], 90.0);
        assert_eq!(result["placements"], result["units"]);
        assert!(result["units"].as_array().unwrap().iter().any(|unit| {
            unit["uuid"] == "33333333-3333-4333-8333-333333333333" && unit["rotation"] == 270.0
        }));
        let mut placed = instances(&path);
        placed.sort_by_key(|instance| instance.unit);
        assert_eq!(placed[0].rotation, 90.0);
        assert_eq!(placed[1].rotation, 270.0);
    }

    /// The delta arithmetic can push a trailing unit past 360° — a unit at
    /// 270° following a +90° turn computes 360°, which eeschema never writes
    /// (it stores 0/90/180/270 and re-saves anything else). The stored and
    /// reported angle must be the normalized one, or the response diverges
    /// from the file the moment KiCad touches it.
    #[tokio::test]
    async fn rotation_past_a_full_turn_normalizes_instead_of_writing_360() {
        let (_directory, path) = fixture();
        for target in [90.0, 180.0] {
            body(
                handle_rotate_schematic_component(
                    &json!({
                        "schematic": path,
                        "reference": "U1",
                        "rotation": target
                    }),
                    &context(),
                )
                .await
                .unwrap(),
            );
        }
        let mut placed = instances(&path);
        placed.sort_by_key(|instance| instance.unit);
        assert_eq!(placed[0].rotation, 180.0);
        assert_eq!(
            placed[1].rotation, 0.0,
            "270° + 90° must store 0°, not 360°"
        );
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(
            !source.contains("(at 40 20 360)") && !source.contains(" 360)"),
            "no unnormalized angle may reach the file"
        );
    }

    #[tokio::test]
    async fn edit_updates_shared_fields_on_every_unit() {
        let (_directory, path) = fixture();
        let result = body(
            handle_edit_schematic_component(
                &json!({
                    "schematic": path,
                    "reference": "U1",
                    "value": "NEW",
                    "fields": { "MPN": "A\\\"B" }
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["value"], "NEW");
        assert_eq!(result["fields"]["MPN"], "A\\\"B");
        assert!(result["units"]
            .as_array()
            .unwrap()
            .iter()
            .all(|unit| unit["fields"]["Value"] == "NEW" && unit["fields"]["MPN"] == "A\\\"B"));
        let source = std::fs::read_to_string(&path).unwrap();
        assert_eq!(source.matches("(property \"Value\" \"NEW\"").count(), 2);
        assert_eq!(
            source.matches("(property \"MPN\" \"A\\\\\\\"B\"").count(),
            2
        );
    }

    #[tokio::test]
    async fn rename_updates_rendered_and_netlist_references_on_every_unit() {
        let (_directory, path) = fixture();
        let result = body(
            handle_edit_schematic_component(
                &json!({
                    "schematic": path,
                    "reference": "U1",
                    "new_reference": "U9"
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["reference"], "U9");
        assert_eq!(result["requested_reference"], "U1");
        assert!(result["units"]
            .as_array()
            .unwrap()
            .iter()
            .all(|unit| unit["fields"]["Reference"] == "U9"
                && unit["instance_references"] == json!(["U9"])));
        let source = std::fs::read_to_string(&path).unwrap();
        assert_eq!(source.matches("(property \"Reference\" \"U9\"").count(), 2);
        assert_eq!(source.matches("(reference \"U9\")").count(), 2);
        assert_eq!(instances(&path).len(), 2);
    }

    #[tokio::test]
    async fn annotation_repairs_a_field_missing_from_one_unit() {
        let (_directory, path) = fixture();
        let result = body(
            handle_add_component_annotation(
                &json!({
                    "schematic": path,
                    "reference": "U1",
                    "key": "Note",
                    "value": "NEW"
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["updated_units"], 1);
        assert_eq!(result["added_units"], 1);
        assert!(result["units"]
            .as_array()
            .unwrap()
            .iter()
            .all(|unit| unit["fields"]["Note"] == "NEW"));
        let source = std::fs::read_to_string(&path).unwrap();
        assert_eq!(source.matches("(property \"Note\" \"NEW\"").count(), 2);
    }

    #[tokio::test]
    async fn grouping_adds_one_property_to_every_unit() {
        let (_directory, path) = fixture();
        let result = body(
            handle_group_components(
                &json!({
                    "schematic": path,
                    "group_name": "Logic",
                    "references": ["U1"]
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["grouped"], json!(["U1"]));
        assert_eq!(result["schematic"], result["components"][0]["schematic"]);
        assert_eq!(
            result["components"][0]["schematic"],
            path.display().to_string()
        );
        assert!(result["components"][0]["units"]
            .as_array()
            .unwrap()
            .iter()
            .all(|unit| unit["fields"]["Group"] == "Logic"));
        let source = std::fs::read_to_string(&path).unwrap();
        assert_eq!(source.matches("(property \"Group\" \"Logic\"").count(), 2);
    }

    #[tokio::test]
    async fn pin_locations_include_every_units_real_placement() {
        let (_directory, path) = fixture();
        let result = body(
            handle_get_schematic_pin_locations(
                &json!({ "schematic": path, "reference": "U1" }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["unit_count"], 2);
        assert_eq!(result["pins"].as_array().unwrap().len(), 2);
        assert!(result["pins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pin| pin["number"] == "1" && pin["unit"] == 1 && pin["y"] == 100.0));
        assert!(result["pins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pin| pin["number"] == "2" && pin["unit"] == 2 && pin["y"] == 120.0));
    }

    #[tokio::test]
    async fn component_summary_lists_every_placement() {
        let (_directory, path) = fixture();
        let result = body(
            handle_get_schematic_component(
                &json!({ "schematic": path, "reference": "U1" }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["unit_count"], 2);
        assert_eq!(
            result["unit_count"].as_u64().unwrap() as usize,
            result["units"].as_array().unwrap().len()
        );
        assert!(result["units"]
            .as_array()
            .unwrap()
            .iter()
            .any(|unit| unit["unit"] == 2 && unit["y"] == 120.0));
    }

    #[tokio::test]
    async fn replace_changes_every_unit_and_preserves_unit_numbers() {
        let (_directory, path) = fixture();
        let result = body(
            handle_replace_component(
                &json!({
                    "schematic": path,
                    "reference": "U1",
                    "new_lib_id": "Test:DUAL_NEW"
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["units_replaced"], 2);
        let mut placed = instances(&path);
        placed.sort_by_key(|instance| instance.unit);
        assert_eq!(
            placed
                .iter()
                .map(|instance| instance.unit)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(placed
            .iter()
            .all(|instance| instance.lib_id == "Test:DUAL_NEW"));
    }

    #[tokio::test]
    async fn replace_rejects_an_ambiguous_unit_override_without_writing() {
        let (_directory, path) = fixture();
        let before = std::fs::read(&path).unwrap();
        let result = handle_replace_component(
            &json!({
                "schematic": path,
                "reference": "U1",
                "new_lib_id": "Test:DUAL_NEW",
                "unit": 1
            }),
            &context(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn move_region_moves_the_selected_unit_not_unit_one() {
        let (_directory, path) = fixture();
        let result = body(
            handle_move_region(
                &json!({
                    "schematic": path,
                    "x1": 95.0,
                    "y1": 115.0,
                    "x2": 105.0,
                    "y2": 125.0,
                    "dx": 10.0,
                    "dy": 0.0
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["moved_unit_count"], 1);
        assert_eq!(result["placements"][0]["unit"], 2);
        let mut placed = instances(&path);
        placed.sort_by_key(|instance| instance.unit);
        assert_eq!(placed[0].x, 100.0, "unit 1 must stay put");
        assert_ne!(placed[1].x, 100.0, "selected unit 2 must move");
    }

    #[test]
    fn shared_readback_refuses_missing_or_renamed_bound_identity() {
        let (_directory, path) = fixture();
        let committed = cse::Schematic::load(&path).unwrap();
        let target = component_target_from_source(&path, SCHEMATIC, "U1").unwrap();

        let wrong_reference = component_mutation_readback_from_schematic(
            &path,
            &committed,
            &target.uuids(),
            Some("U404"),
        )
        .unwrap_err();
        assert_eq!(
            extract_error_kind(&wrong_reference).as_deref(),
            Some("stale_target")
        );

        let missing_uuid = component_mutation_readback_from_schematic(
            &path,
            &committed,
            &["missing-uuid".to_owned()],
            None,
        )
        .unwrap_err();
        assert_eq!(
            extract_error_kind(&missing_uuid).as_deref(),
            Some("stale_target")
        );
    }

    #[test]
    fn shared_readback_refuses_wrong_document_and_mismatched_fields() {
        let (directory, path) = fixture();
        let other = directory.path().join("other.kicad_sch");
        std::fs::write(&other, SCHEMATIC).unwrap();
        let committed = cse::Schematic::load(&path).unwrap();
        let committed_other = cse::Schematic::load(&other).unwrap();
        let target = component_target_from_source(&path, SCHEMATIC, "U1").unwrap();

        let wrong_document = component_mutation_readback_from_schematic(
            &path,
            &committed_other,
            &target.uuids(),
            Some("U1"),
        )
        .unwrap_err();
        assert_eq!(
            extract_error_kind(&wrong_document).as_deref(),
            Some("stale_target")
        );

        let observed = component_mutation_readback_from_schematic(
            &path,
            &committed,
            &target.uuids(),
            Some("U1"),
        )
        .unwrap();
        let mismatched_field =
            verify_observed_field(&path, &observed, "Value", "NOT-COMMITTED").unwrap_err();
        assert_eq!(
            extract_error_kind(&mismatched_field).as_deref(),
            Some("stale_target")
        );
    }

    #[test]
    fn shared_readback_reports_the_committed_models_document_path() {
        let (_directory, path) = fixture();
        let committed = cse::Schematic::load(&path).unwrap();
        let target = component_target_from_source(&path, SCHEMATIC, "U1").unwrap();
        let differently_spelled = path
            .parent()
            .unwrap()
            .join(".")
            .join(path.file_name().unwrap());
        assert_ne!(
            differently_spelled.display().to_string(),
            committed.filepath().display().to_string()
        );

        let observed = component_mutation_readback_from_schematic(
            &differently_spelled,
            &committed,
            &target.uuids(),
            Some("U1"),
        )
        .unwrap();

        assert_eq!(
            observed["schematic"],
            committed.filepath().display().to_string()
        );
        assert_ne!(
            observed["schematic"],
            differently_spelled.display().to_string()
        );
    }

    #[test]
    fn duplicate_bound_uuid_and_property_identities_are_ambiguous() {
        let (_directory, path) = fixture();
        let committed = cse::Schematic::load(&path).unwrap();
        let uuid = "22222222-2222-4222-8222-222222222222".to_owned();
        let duplicate_bound_uuid = component_mutation_readback_from_schematic(
            &path,
            &committed,
            &[uuid.clone(), uuid.clone()],
            Some("U1"),
        )
        .unwrap_err();
        assert_eq!(
            extract_error_kind(&duplicate_bound_uuid).as_deref(),
            Some("ambiguous_target")
        );

        let property = "    (property \"Note\" \"OLD\" (at 100 100 0) (hide yes))\n";
        let duplicate_source = SCHEMATIC.replacen(property, &format!("{property}{property}"), 1);
        assert_ne!(duplicate_source, SCHEMATIC);
        std::fs::write(&path, duplicate_source).unwrap();
        let committed = cse::Schematic::load(&path).unwrap();
        let duplicate_property =
            component_mutation_readback_from_schematic(&path, &committed, &[uuid], Some("U1"))
                .unwrap_err();
        assert_eq!(
            extract_error_kind(&duplicate_property).as_deref(),
            Some("ambiguous_target")
        );
    }

    #[tokio::test]
    async fn duplicate_reference_unit_is_ambiguous_before_a_move() {
        let original = SCHEMATIC.replacen("    (unit 2)\n", "    (unit 1)\n", 1);
        assert_ne!(original, SCHEMATIC);
        let (directory, path) = fixture();
        std::fs::write(&path, &original).unwrap();
        let result = handle_move_schematic_component(
            &json!({ "schematic": path, "reference": "U1", "x": 110.0, "y": 110.0 }),
            &context(),
        )
        .await
        .unwrap();

        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("ambiguous_target")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        drop(directory);
    }

    #[tokio::test]
    async fn missing_reference_is_stale_and_does_not_mutate_either_document() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.kicad_sch");
        let other = directory.path().join("other.kicad_sch");
        std::fs::write(&target, SCHEMATIC).unwrap();
        std::fs::write(&other, SCHEMATIC).unwrap();
        let result = handle_rotate_schematic_component(
            &json!({ "schematic": target, "reference": "MISSING", "rotation": 90.0 }),
            &context(),
        )
        .await
        .unwrap();

        assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), SCHEMATIC);
        assert_eq!(std::fs::read_to_string(&other).unwrap(), SCHEMATIC);
    }

    #[tokio::test]
    async fn successful_move_names_only_the_requested_document_from_readback() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.kicad_sch");
        let other = directory.path().join("other.kicad_sch");
        std::fs::write(&target, SCHEMATIC).unwrap();
        std::fs::write(&other, SCHEMATIC).unwrap();
        let result = body(
            handle_move_schematic_component(
                &json!({ "schematic": target, "reference": "U1", "x": 110.0, "y": 110.0 }),
                &context(),
            )
            .await
            .unwrap(),
        );

        assert_eq!(result["schematic"], target.display().to_string());
        assert_ne!(std::fs::read_to_string(&target).unwrap(), SCHEMATIC);
        assert_eq!(std::fs::read_to_string(&other).unwrap(), SCHEMATIC);
    }

    #[test]
    fn stale_target_revision_refuses_a_prepared_component_edit() {
        let (_directory, path) = fixture();
        let target = component_target_from_source(&path, SCHEMATIC, "U1").unwrap();
        let candidate = set_property_value(SCHEMATIC, "U1", "Value", "PLANNED", false)
            .unwrap()
            .0;
        let command = SchematicCommand::replace_items_from_document(
            SCHEMATIC,
            &candidate,
            target.item_ids().unwrap(),
            "planned edit",
        )
        .unwrap();
        let newer = SCHEMATIC.replacen(
            "(property \"Value\" \"OLD\"",
            "(property \"Value\" \"NEWER\"",
            1,
        );
        std::fs::write(&path, &newer).unwrap();

        assert!(matches!(
            commit_command(&path, &command).unwrap_err(),
            SexpError::Conflict { .. } | SexpError::ItemConflict { .. }
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), newer);
    }
}
