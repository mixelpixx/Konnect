//! `verification` toolset — DRC, design rules, KiCAD UI management, routing utilities.
//!
//! DRC delegates to `kicad-cli`. Design rules are read/written as S-expressions.
//! KiCAD UI management uses process inspection + subprocess spawning.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::pcb_board::block_tag;
use crate::tools::pcb_components::escape_sexp_string;
use crate::tools::{
    get_path, invalid_arg, opt_str, require_f64, require_str, with_board_ipc_classified,
    ToolContext, ToolDef,
};
use konnect_sexp::board::exact_coordinate_pair;
use konnect_sexp::geometry::round6;
use konnect_sexp::net::{collect_net_keys, names_nets_in_place, net_name};
use konnect_sexp::writer::{
    apply_edits, find_direct_child_blocks, new_uuid, read_consistent, write_atomic,
    write_atomic_if_unchanged, SexpEdit,
};
use konnect_sexp::{parse_sexp, SexpError, SexpNode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::task;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "run_drc",
            "Run the Design Rule Check on the PCB and return structured violation results, \
             with separate error and warning counts in the summary. Prefer this over \
             `get_drc_violations` (pcb_export toolset) — they run the same underlying \
             kicad-cli check, but `run_drc` returns a cleaner breakdown. KiCad runs the \
             complete configured DRC ruleset; kicad-cli has no per-test selector.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Optional path to write DRC report JSON" },
                    "sync_live_board": { "type": "boolean", "default": false, "description": "Bind the requested open board, optionally refill, save and verify its persisted snapshot before CLI DRC. Finish other mutations first." },
                    "refill_zones": { "type": "boolean", "default": false, "description": "Refill before checking: persisted IPC fill with sync_live_board, analysis-only CLI fill otherwise." },
                    "severity": {
                        "type": "string",
                        "description": "Minimum violation severity to include: 'error', 'warning' (default), 'info'",
                        "default": "warning"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of violations to return",
                        "default": 50
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_run_drc(args, ctx).await }
        ),
        tool!(
            "set_design_rules",
            "Set board-level design rules (clearance, trace width, via size) in the sibling KiCAD \
             project file. Refuses while KiCad holds the board open, because KiCad's next save \
             rewrites the project file from its own copy.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "min_clearance": { "type": "number", "description": "Minimum clearance in mm" },
                    "min_trace_width": { "type": "number", "description": "Minimum trace width in mm" },
                    "min_via_drill": { "type": "number", "description": "Minimum via drill diameter in mm" },
                    "min_via_size": { "type": "number", "description": "Minimum via pad diameter in mm" },
                    "min_hole_to_hole": { "type": "number", "description": "Minimum hole-to-hole clearance in mm" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_set_design_rules(args, ctx).await }
        ),
        tool!(
            "get_design_rules",
            "Return the current design rule constraints defined in the sibling KiCAD project file.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_design_rules(args, ctx).await }
        ),
        tool!(
            "set_predefined_sizes",
            "Write the PCB editor Pre-defined Sizes list (track widths and via pad/drill \
             pairs) into the sibling .kicad_pro. These populate the Track/Via dropdowns \
             and W/Shift+W while routing; they are not DRC limits and do not change \
             netclasses. A leading 0 mm track and 0/0 via is always kept as the \
             'use netclass' sentinel. Pass only the lists you want to replace. KiCad \
             reads the change on next project open. The board file is not modified.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_pro is edited" },
                    "track_widths": {
                        "type": "array",
                        "description": "Track widths in mm for the router dropdown, excluding the 0 mm netclass sentinel (that row is always prepended). An empty array leaves only the sentinel.",
                        "items": { "type": "number" }
                    },
                    "via_dimensions": {
                        "type": "array",
                        "description": "Via pad/drill pairs in mm for the router dropdown, excluding the 0/0 netclass sentinel (that row is always prepended). An empty array leaves only the sentinel.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "diameter": { "type": "number", "description": "Via pad diameter in mm" },
                                "drill": { "type": "number", "description": "Via drill diameter in mm" }
                            },
                            "required": ["diameter", "drill"]
                        }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_set_predefined_sizes(args, ctx).await }
        ),
        tool!(
            "get_predefined_sizes",
            "Return the PCB editor Pre-defined Sizes list from the sibling .kicad_pro: \
             track_widths (mm) and via_dimensions (diameter/drill mm), including the \
             0 / 0,0 netclass sentinel KiCad keeps at the front of each list.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_pro is read" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_predefined_sizes(args, ctx).await }
        ),
        tool!(
            "check_kicad_ui",
            "Check whether the KiCad GUI application is running and whether IPC responds within a bounded timeout. When IPC does not answer, ipc_failure.kind says why: not_configured, no_listener, access_denied (the operating system refused this account; likely a different account or a restrictive ACL, as with a sandboxed client), handshake_failed (the listener did not complete NNG's handshake, so it is probably not KiCad; this takes NNG's 10 s limit, so pass timeout_seconds above 10 to see it), transport_error, or request_failed (the request did not complete and may have reached the endpoint; an explicit KiCad status such as AS_NOT_READY proves receipt, a timeout or malformed reply does not). ipc_failure is null when no kind was established: the Ping succeeded with AS_OK, or this check's own timeout expired first (timed_out: true).",
            json!({
                "type": "object",
                "properties": {
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Timeout for the health check in seconds",
                        "minimum": 1,
                        "maximum": 300,
                        "default": 5
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_check_kicad_ui(args, ctx).await }
        ),
        tool!(
            "launch_kicad_ui",
            "Launch the KiCAD GUI application and optionally open a project file.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to .kicad_pro file to open (optional)" },
                    "wait_ready": {
                        "type": "boolean",
                        "description": "Wait until KiCAD IPC is responsive before returning",
                        "default": true
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Maximum wait time in seconds",
                        "default": 30
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_launch_kicad_ui(args, ctx).await }
        ),
        tool!(
            "copy_routing_pattern",
            "Copy the segments, arcs and vias inside a source region of a board KiCad is not \
             holding, shifted by (dest_x - src_x1, dest_y - src_y1). An item is copied when all \
             of its points (segment start/end, arc start/mid/end, via position) are inside the \
             region, edges included; one that crosses the edge is listed in `excluded_crossing` \
             and not copied. Copies keep every attribute and get fresh UUIDs.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "src_x1": { "type": "number", "description": "Source region bounding box min X" },
                    "src_y1": { "type": "number", "description": "Source region bounding box min Y" },
                    "src_x2": { "type": "number", "description": "Source region bounding box max X" },
                    "src_y2": { "type": "number", "description": "Source region bounding box max Y" },
                    "dest_x": { "type": "number", "description": "Destination anchor X (maps to src_x1)" },
                    "dest_y": { "type": "number", "description": "Destination anchor Y (maps to src_y1)" },
                    "net_map": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Optional mapping from source net names to destination net names"
                    }
                },
                "required": ["board", "src_x1", "src_y1", "src_x2", "src_y2", "dest_x", "dest_y"]
            }),
            |args, ctx| async move { handle_copy_routing_pattern(args, ctx).await }
        ),
        tool!(
            "set_layer_constraints",
            "Set per-layer design constraints (e.g. min trace width, clearance) in the sibling .kicad_dru custom rules file.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "layer": { "type": "string", "description": "Layer name (e.g. 'F.Cu', 'B.Cu')" },
                    "min_clearance": { "type": "number", "description": "Minimum clearance for this layer in mm" },
                    "min_trace_width": { "type": "number", "description": "Minimum trace width for this layer in mm" }
                },
                "required": ["board", "layer"]
            }),
            |args, ctx| async move { handle_set_layer_constraints(args, ctx).await }
        ),
        tool!(
            "check_clearance",
            "Measure spacing between two footprints on the requested board. `mode: anchor` \
             (the compatibility default) returns straight-line placement-anchor distance. \
             `mode: courtyard` returns edge-to-edge distance between transformed, authored \
             courtyard bounding boxes for same-side footprints. It refuses missing or \
             malformed courtyard geometry and does not substitute pads or anchors. Neither \
             mode measures pad, trace or copper clearance; use `run_drc` for electrical \
             clearance and `get_component_pads` for pad geometry. Reads the live KiCad board \
             when open, otherwise the saved file, and reports the source.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "ref1":  { "type": "string", "description": "First component reference (e.g. 'U1')" },
                    "ref2":  { "type": "string", "description": "Second component reference (e.g. 'C1')" },
                    "mode": {
                        "type": "string",
                        "enum": ["anchor", "courtyard"],
                        "default": "anchor",
                        "description": "anchor: placement-origin distance; courtyard: authored courtyard bbox edge distance"
                    }
                },
                "required": ["board", "ref1", "ref2"]
            }),
            |args, ctx| async move { handle_check_clearance(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

fn severity_rank(s: &str) -> u8 {
    match s {
        "error" => 2,
        "warning" => 1,
        _ => 0,
    }
}

async fn handle_run_drc(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let severity_filter = args["severity"].as_str().unwrap_or("warning");
    let min_rank = severity_rank(severity_filter);
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    let (report, provenance) = match super::drc::run(ctx, &board, args).await? {
        Ok(result) => result,
        Err(error) => return Ok(error),
    };

    // Optionally write report
    if let Some(out_path) = args["output"].as_str() {
        let json = serde_json::to_string_pretty(&report)?;
        if let Err(error) = write_report(out_path, &json).await {
            return super::drc::report_write_failure(&provenance, &board, out_path, error);
        }
    }

    // Every category, not just `violations`. An unrouted net is reported under
    // `unconnected_items`, which Konnect used to discard — so a board that
    // KiCad called unrouted came back from here clean (#245).
    let filtered: Vec<_> = report
        .all()
        .filter(|v| severity_rank(&v.severity) >= min_rank)
        .collect();

    let errors = filtered.iter().filter(|v| v.severity == "error").count();
    let warnings = filtered.iter().filter(|v| v.severity == "warning").count();
    let shown = filtered.len().min(limit);
    let truncated = filtered.len() > limit;
    let missing = report.missing_categories();

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "total_violations": report.all().count(),
            "source": provenance.source,
            "live_board_synced": provenance.live_board_synced,
            "zones_refilled": provenance.zones_refilled,
            "zone_refill_source": provenance.zone_refill_source,
            "design_rule_violations": report.violations.len(),
            // Null, not zero, when this kicad-cli did not report the category:
            // "none found" and "never asked" are different answers.
            "unconnected_items": report.unconnected_items.as_ref().map(Vec::len),
            "schematic_parity": report.schematic_parity.as_ref().map(Vec::len),
            "categories_not_reported": missing,
            // Present only when parity is null because no schematic sits
            // beside the board — kicad-cli writes an empty array then, which
            // is not a checked zero (#516).
            "schematic_parity_diagnostic": report.schematic_parity_diagnostic,
            "filtered_count": filtered.len(),
            "errors": errors,
            "warnings": warnings,
            "severity_filter": severity_filter,
            "shown": shown,
            "truncated": truncated,
            "violations": filtered.iter().take(limit).map(|v| json!({
                "severity": v.severity,
                "rule": v.rule,
                "description": v.description,
                "pos": v.pos.as_ref().map(|p| json!({ "x": p.x, "y": p.y })),
                "items": v.items
            })).collect::<Vec<_>>()
        }))
        .unwrap(),
    ))
}

/// Write a report, creating the directory the caller named.
///
/// A missing parent used to surface as a bare OS "path not found" with nothing
/// naming what was missing — the export tools next door already call
/// `create_dir_all` first.
async fn write_report(out_path: &str, contents: &str) -> anyhow::Result<()> {
    super::drc::write_report(out_path, contents).await
}

// ─── Design rules helpers ────────────────────────────────────────────────────

fn sibling_project_path(board: &Path) -> PathBuf {
    board.with_extension("kicad_pro")
}

fn sibling_custom_rules_path(board: &Path) -> PathBuf {
    board.with_extension("kicad_dru")
}

fn project_rules_mut(
    project: &mut serde_json::Value,
) -> anyhow::Result<&mut serde_json::Map<String, serde_json::Value>> {
    let project = project
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("KiCAD project root must be a JSON object"))?;
    let board = project
        .entry("board")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("KiCAD project 'board' must be a JSON object"))?;
    let design_settings = board
        .entry("design_settings")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            anyhow::anyhow!("KiCAD project 'board.design_settings' must be a JSON object")
        })?;
    design_settings
        .entry("rules")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            anyhow::anyhow!("KiCAD project 'board.design_settings.rules' must be a JSON object")
        })
}

fn project_rule_value(project: &serde_json::Value, key: &str) -> Option<f64> {
    project["board"]["design_settings"]["rules"][key].as_f64()
}

fn named_rule_range(content: &str, name: &str) -> Option<(usize, usize)> {
    let needle = format!("(rule \"{name}\"");
    let start = content.find(&needle)?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut in_comment = false;

    for (offset, character) in content[start..].char_indices() {
        if in_comment {
            if character == '\n' {
                in_comment = false;
            }
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '#' => in_comment = true,
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((start, start + offset + character.len_utf8()));
                }
            }
            _ => {}
        }
    }
    None
}

fn upsert_named_rule(content: &str, name: &str, rule: &str) -> String {
    if let Some((start, end)) = named_rule_range(content, name) {
        return format!("{}{}{}", &content[..start], rule, &content[end..]);
    }

    let mut result = content.trim_end().to_string();
    if !result.is_empty() {
        result.push_str("\n\n");
    }
    result.push_str(rule);
    result.push('\n');
    result
}

fn layer_rule(name: &str, constraint: &str, value: f64, layer: &str) -> String {
    format!("(rule \"{name}\"\n  (constraint {constraint} (min {value}mm))\n  (layer \"{layer}\"))")
}

async fn handle_set_design_rules(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    // KiCad rewrites the project file from memory when it saves the board, so
    // a rule written here while it holds the board is reverted (#791). Asked
    // before the read too: the file can be older than what KiCad holds.
    if let Some(refusal) =
        crate::tools::pcb_board::refuse_if_board_open_in_kicad(ctx, &board, "design rule").await?
    {
        return Ok(refusal);
    }
    let project_path = sibling_project_path(&board);
    let project_content = tokio::fs::read_to_string(&project_path).await?;
    let mut project: serde_json::Value = serde_json::from_str(&project_content)?;

    let mut changed = Vec::new();

    let rules: &[(&str, &str)] = &[
        ("min_clearance", "min_clearance"),
        ("min_track_width", "min_trace_width"),
        ("min_through_hole_diameter", "min_via_drill"),
        ("min_via_size", "min_via_size"),
        ("min_hole_to_hole", "min_hole_to_hole"),
    ];

    let project_rules = project_rules_mut(&mut project)?;
    for (project_key, arg_key) in rules {
        if let Some(val) = args[arg_key].as_f64() {
            let storage_key = if *project_key == "min_via_size" {
                "min_via_diameter"
            } else {
                project_key
            };
            project_rules.insert(storage_key.to_string(), json!(val));
            changed.push(format!("{} = {}", storage_key, val));
        }
    }

    if !changed.is_empty() {
        let mut content = serde_json::to_string_pretty(&project)?;
        content.push('\n');
        write_atomic(&project_path, &content)?;
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "project": project_path,
            "changed": changed
        }))
        .unwrap(),
    ))
}

async fn handle_get_design_rules(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let project_path = sibling_project_path(&board);
    let content = tokio::fs::read_to_string(&project_path).await?;
    let project: serde_json::Value = serde_json::from_str(&content)?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "board": board.to_str().unwrap_or(""),
            "project": project_path.to_str().unwrap_or(""),
            "rules": {
                "min_clearance": project_rule_value(&project, "min_clearance"),
                "min_trace_width": project_rule_value(&project, "min_track_width"),
                "min_via_drill": project_rule_value(&project, "min_through_hole_diameter"),
                "min_via_size": project_rule_value(&project, "min_via_diameter"),
                "min_hole_to_hole": project_rule_value(&project, "min_hole_to_hole")
            }
        }))
        .unwrap(),
    ))
}

// ─── Pre-defined sizes (Board Setup → Design Rules → Pre-defined Sizes) ───────
//
// KiCad stores the router palette in board.design_settings.track_widths /
// via_dimensions. A leading 0 (track) or {diameter:0, drill:0} (via) is the
// "use netclass values" sentinel the dropdown always shows first. These lists
// are not DRC floors and are not netclass widths.

fn project_design_settings_mut(
    project: &mut Value,
) -> anyhow::Result<&mut serde_json::Map<String, Value>> {
    let project = project
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("KiCAD project root must be a JSON object"))?;
    let board = project
        .entry("board")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("KiCAD project 'board' must be a JSON object"))?;
    board
        .entry("design_settings")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            anyhow::anyhow!("KiCAD project 'board.design_settings' must be a JSON object")
        })
}

fn load_sibling_project(board: &Path) -> anyhow::Result<Result<(PathBuf, Value), CallToolResult>> {
    let project_path = sibling_project_path(board);
    if !project_path.exists() {
        return Ok(Err(CallToolResult::error(format!(
            "No project file at {} — Pre-defined Sizes live in the .kicad_pro, \
             and a list written anywhere else is never read. Create the project \
             (KiCad: File > Save a Copy, or place the board inside a project) and retry.",
            project_path.display()
        ))));
    }
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(&project_path)?)
        .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", project_path.display()))?;
    Ok(Ok((project_path, settings)))
}

fn finite_positive(value: f64, field: &str) -> Result<f64, CallToolResult> {
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(CallToolResult::error(format!(
            "Argument '{field}' is invalid: must be a finite number greater than 0 mm, got {value}"
        )))
    }
}

/// Palette track widths, with the 0 mm netclass sentinel always first.
/// Caller zeros are dropped rather than duplicated.
fn parse_track_widths(raw: &[Value]) -> Result<Vec<Value>, CallToolResult> {
    let mut out = vec![json!(0.0)];
    for (i, item) in raw.iter().enumerate() {
        let width = item.as_f64().ok_or_else(|| {
            CallToolResult::error(format!(
                "Argument 'track_widths[{i}]' is invalid: missing or not a number"
            ))
        })?;
        if width == 0.0 {
            continue;
        }
        let width = finite_positive(width, &format!("track_widths[{i}]"))?;
        let encoded = json!(width);
        if !out.contains(&encoded) {
            out.push(encoded);
        }
    }
    Ok(out)
}

/// Palette vias, with the 0/0 netclass sentinel always first.
fn parse_via_dimensions(raw: &[Value]) -> Result<Vec<Value>, CallToolResult> {
    let mut out = vec![json!({ "diameter": 0.0, "drill": 0.0 })];
    for (i, item) in raw.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| {
            CallToolResult::error(format!(
                "Argument 'via_dimensions[{i}]' is invalid: missing or not an object"
            ))
        })?;
        let diameter = obj.get("diameter").and_then(Value::as_f64).ok_or_else(|| {
            CallToolResult::error(format!(
                "Argument 'via_dimensions[{i}].diameter' is invalid: missing or not a number"
            ))
        })?;
        let drill = obj.get("drill").and_then(Value::as_f64).ok_or_else(|| {
            CallToolResult::error(format!(
                "Argument 'via_dimensions[{i}].drill' is invalid: missing or not a number"
            ))
        })?;
        if diameter == 0.0 && drill == 0.0 {
            continue;
        }
        let diameter = finite_positive(diameter, &format!("via_dimensions[{i}].diameter"))?;
        let drill = finite_positive(drill, &format!("via_dimensions[{i}].drill"))?;
        if diameter <= drill {
            return Err(CallToolResult::error(format!(
                "Argument 'via_dimensions[{i}]' is invalid: diameter ({diameter} mm) \
                 must be greater than drill ({drill} mm)"
            )));
        }
        let encoded = json!({ "diameter": diameter, "drill": drill });
        if !out.contains(&encoded) {
            out.push(encoded);
        }
    }
    Ok(out)
}

fn current_predefined_sizes(project: &Value) -> (Value, Value) {
    let settings = &project["board"]["design_settings"];
    let track_widths = settings["track_widths"].clone();
    let via_dimensions = settings["via_dimensions"].clone();
    (
        if track_widths.is_array() {
            track_widths
        } else {
            json!([0.0])
        },
        if via_dimensions.is_array() {
            via_dimensions
        } else {
            json!([{ "diameter": 0.0, "drill": 0.0 }])
        },
    )
}

async fn handle_set_predefined_sizes(
    args: &Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let tracks_arg = match args.get("track_widths") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => match parse_track_widths(items) {
            Ok(v) => Some(v),
            Err(e) => return Ok(e),
        },
        Some(_) => {
            return Ok(CallToolResult::error(
                "Argument 'track_widths' is invalid: missing or not an array".to_string(),
            ))
        }
    };
    let vias_arg = match args.get("via_dimensions") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => match parse_via_dimensions(items) {
            Ok(v) => Some(v),
            Err(e) => return Ok(e),
        },
        Some(_) => {
            return Ok(CallToolResult::error(
                "Argument 'via_dimensions' is invalid: missing or not an array".to_string(),
            ))
        }
    };
    if tracks_arg.is_none() && vias_arg.is_none() {
        return Ok(CallToolResult::error(
            "Argument 'track_widths' is invalid: name at least one of track_widths, via_dimensions"
                .to_string(),
        ));
    }

    if let Some(refusal) = crate::tools::pcb_board::refuse_if_board_open_in_kicad(
        ctx,
        &board,
        "Pre-defined Sizes list",
    )
    .await?
    {
        return Ok(refusal);
    }

    let (project_path, mut project) = match load_sibling_project(&board)? {
        Ok(v) => v,
        Err(refusal) => return Ok(refusal),
    };

    let (mut track_widths, mut via_dimensions) = current_predefined_sizes(&project);
    let mut changed_fields = Vec::new();
    if let Some(next) = tracks_arg {
        let next = Value::Array(next);
        if next != track_widths {
            changed_fields.push("track_widths");
            track_widths = next;
        }
    }
    if let Some(next) = vias_arg {
        let next = Value::Array(next);
        if next != via_dimensions {
            changed_fields.push("via_dimensions");
            via_dimensions = next;
        }
    }

    if !changed_fields.is_empty() {
        let settings = project_design_settings_mut(&mut project)?;
        settings.insert("track_widths".into(), track_widths.clone());
        settings.insert("via_dimensions".into(), via_dimensions.clone());
        let mut content = serde_json::to_string_pretty(&project)?;
        content.push('\n');
        write_atomic(&project_path, &content)?;
    }

    Ok(CallToolResult::json(&json!({
        "success": true,
        "project": project_path,
        "changed": changed_fields,
        "track_widths": track_widths,
        "via_dimensions": via_dimensions,
        "note": "Pre-defined Sizes live in the project file and fill the Track/Via dropdowns. \
                 They are not DRC limits. KiCad reads the change on next project open."
    })))
}

async fn handle_get_predefined_sizes(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let (project_path, project) = match load_sibling_project(&board)? {
        Ok(v) => v,
        Err(refusal) => return Ok(refusal),
    };
    let (track_widths, via_dimensions) = current_predefined_sizes(&project);
    Ok(CallToolResult::json(&json!({
        "project": project_path,
        "track_widths": track_widths,
        "via_dimensions": via_dimensions
    })))
}

// ─── KiCAD UI management ──────────────────────────────────────────────────────

const KICAD_GUI_PROCESS_NAMES: &[&str] = &["kicad", "pcbnew", "eeschema"];

fn is_kicad_process_name(name: &str) -> bool {
    let file_name = std::path::Path::new(name.trim_matches('"'))
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name)
        .to_ascii_lowercase();
    let stem = file_name.strip_suffix(".exe").unwrap_or(&file_name);
    KICAD_GUI_PROCESS_NAMES.contains(&stem)
}

fn process_list_has_kicad(output: &str) -> bool {
    output.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(is_kicad_process_name)
    })
}

/// Check if the KiCad project manager or either standalone editor is running.
fn is_kicad_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("tasklist")
            .output()
            .ok()
            .map(|output| process_list_has_kicad(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("ps")
            .args(["-A", "-o", "comm="])
            .output()
            .ok()
            .map(|output| process_list_has_kicad(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or(false)
    }
}

fn ui_running(process_detected: bool, ipc_responsive: bool) -> bool {
    process_detected || ipc_responsive
}

/// Resolve the KiCAD binary path from config or well-known locations.
fn find_kicad_binary(config_binary: &str, config_cli: &str) -> String {
    crate::kicad_install::find_gui(config_binary, config_cli)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                "kicad.exe".to_string()
            } else {
                "kicad".to_string()
            }
        })
}

fn health_timeout_seconds(args: &serde_json::Value) -> Result<u64, CallToolResult> {
    let timeout = match args.get("timeout_seconds") {
        None | Some(serde_json::Value::Null) => 5,
        Some(value) => value.as_u64().ok_or_else(|| {
            CallToolResult::error_kind(
                crate::mcp::error::ToolErrorKind::InvalidArgument {
                    field: "timeout_seconds".to_string(),
                    reason: "must be an integer from 1 to 300".to_string(),
                },
                "Argument 'timeout_seconds' must be an integer from 1 to 300",
            )
        })?,
    };
    if !(1..=300).contains(&timeout) {
        return Err(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "timeout_seconds".to_string(),
                reason: "must be between 1 and 300 seconds".to_string(),
            },
            "Argument 'timeout_seconds' must be between 1 and 300 seconds",
        ));
    }
    Ok(timeout)
}

async fn bounded_health_check<F, T>(
    timeout: std::time::Duration,
    future: F,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(timeout, future).await
}

async fn handle_check_kicad_ui(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let timeout_seconds = match health_timeout_seconds(args) {
        Ok(timeout) => timeout,
        Err(error) => return Ok(error),
    };
    let addr = ctx.config.ipc_address.clone();
    let started = std::time::Instant::now();
    let check = async move {
        let process_detected = task::spawn_blocking(is_kicad_running).await?;
        let ipc = task::spawn_blocking(move || {
            konnect_ipc::client::KiCadIpcClient::new(&addr).ping_outcome()
        })
        .await?;
        Ok::<_, tokio::task::JoinError>((process_detected, ipc))
    };

    match bounded_health_check(std::time::Duration::from_secs(timeout_seconds), check).await {
        Ok(result) => {
            let (process_detected, ipc) = result?;
            let ipc_responsive = ipc.is_responsive();
            Ok(CallToolResult::json(&json!({
                "running": ui_running(process_detected, ipc_responsive),
                "process_detected": process_detected,
                "ipc_responsive": ipc_responsive,
                "ipc_failure": crate::tools::ipc_failure_evidence(&ipc),
                "timed_out": false,
                "timeout_seconds": timeout_seconds,
                "elapsed_ms": started.elapsed().as_millis() as u64
            })))
        }
        Err(_) => Ok(CallToolResult::json(&json!({
            "running": null,
            "process_detected": null,
            "ipc_responsive": false,
            "ipc_failure": null,
            "timed_out": true,
            "timeout_seconds": timeout_seconds,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "note": "KiCad health check exceeded the requested timeout"
        }))),
    }
}

async fn handle_launch_kicad_ui(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let wait_ready = args["wait_ready"].as_bool().unwrap_or(true);
    let timeout_secs = args["timeout_seconds"].as_u64().unwrap_or(30);
    let binary = find_kicad_binary(&ctx.config.kicad_binary, &ctx.config.kicad_cli);

    let mut cmd = tokio::process::Command::new(&binary);
    if let Some(project) = args["project"].as_str() {
        cmd.arg(project);
    }

    // Spawn detached — we don't wait for the process to exit
    match cmd.spawn() {
        Ok(_child) => {
            if wait_ready {
                // Poll IPC until responsive or timeout
                let addr = ctx.config.ipc_address.clone();
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let addr2 = addr.clone();
                    let ok = task::spawn_blocking(move || {
                        konnect_ipc::client::KiCadIpcClient::new(&addr2)
                            .ping()
                            .unwrap_or(false)
                    })
                    .await
                    .unwrap_or(false);

                    if ok {
                        return Ok(CallToolResult::text(
                            serde_json::to_string(&json!({
                                "launched": true,
                                "ipc_ready": true
                            }))
                            .unwrap(),
                        ));
                    }
                    if std::time::Instant::now() >= deadline {
                        return Ok(CallToolResult::text(
                            serde_json::to_string(&json!({
                                "launched": true,
                                "ipc_ready": false,
                                "note": "KiCAD launched but IPC not yet responsive within timeout"
                            }))
                            .unwrap(),
                        ));
                    }
                }
            }

            Ok(CallToolResult::text(
                serde_json::to_string(&json!({
                    "launched": true,
                    "ipc_ready": null
                }))
                .unwrap(),
            ))
        }
        Err(e) => Ok(CallToolResult::error(format!(
            "Failed to launch KiCAD ({}): {}",
            binary, e
        ))),
    }
}

// ─── Copy routing pattern ─────────────────────────────────────────────────────

async fn handle_copy_routing_pattern(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    // All six are schema-required and each defaulted to 0.0. Omitting them all
    // was harmless — the source box collapsed to a point and matched nothing —
    // but a *partial* omission was not: drop only `dest_x`/`dest_y` and the
    // whole source region is duplicated onto the board origin and written to
    // the .kicad_pcb, reported as `{"copied": N}` (#218).
    let mut coords = [0.0f64; 6];
    for (slot, key) in coords
        .iter_mut()
        .zip(["src_x1", "src_y1", "src_x2", "src_y2", "dest_x", "dest_y"])
    {
        match require_f64(args, key) {
            Ok(v) => *slot = v,
            Err(e) => return Ok(e),
        }
    }
    let [src_x1, src_y1, src_x2, src_y2, dest_x, dest_y] = coords;
    // An inverted box used to select nothing and report success.
    for (field, low, high) in [("src_x2", src_x1, src_x2), ("src_y2", src_y1, src_y2)] {
        if high < low {
            return Ok(invalid_arg(
                field,
                &format!("{high} is less than the region's minimum {low}"),
            ));
        }
    }

    let mut net_map = HashMap::new();
    if let Some(obj) = args["net_map"].as_object() {
        for (from, to) in obj {
            let Some(to) = to.as_str() else {
                return Ok(invalid_arg(
                    &format!("net_map.{from}"),
                    "must be a destination net name",
                ));
            };
            net_map.insert(from.clone(), to.to_string());
        }
    }

    let copy = RoutingCopy {
        region: [src_x1, src_y1, src_x2, src_y2],
        dx: round6(dest_x - src_x1),
        dy: round6(dest_y - src_y1),
        net_map,
    };
    if copy.dx == 0.0 && copy.dy == 0.0 {
        return Ok(invalid_arg(
            "dest_x",
            "the destination is the source anchor (src_x1, src_y1), so every copy would land \
             on its original",
        ));
    }
    // A copy written to the file while KiCad holds the board would be
    // discarded by its next save. Asked before the read: the file can be
    // older than what KiCad holds.
    if let Some(refusal) =
        crate::tools::pcb_board::refuse_if_board_open_in_kicad(ctx, &board, "routing copy").await?
    {
        return Ok(refusal);
    }
    task::spawn_blocking(move || {
        let content = read_consistent(&board)?;
        match plan_routing_copy(&content, &copy) {
            Ok(planned) => commit_routing_copy(&board, &content, &copy, &planned),
            Err(refusal) => Ok(refusal),
        }
    })
    .await?
}

/// What one `copy_routing_pattern` call asks for.
pub(crate) struct RoutingCopy {
    /// The source region `[x1, y1, x2, y2]`, inclusive, with `x1 ≤ x2` and
    /// `y1 ≤ y2`.
    pub(crate) region: [f64; 4],
    pub(crate) dx: f64,
    pub(crate) dy: f64,
    pub(crate) net_map: HashMap<String, String>,
}

/// The copies to insert, and what was left out.
pub(crate) struct PlannedCopy {
    /// The board with the copies inserted; `None` when nothing is copied.
    content: Option<String>,
    uuids: Vec<String>,
    /// Copies per entry of [`ROUTING_KINDS`].
    counts: [usize; 3],
    excluded_crossing: Vec<Value>,
}

const ROUTING_KINDS: [&str; 3] = ["segment", "arc", "via"];

/// The points that place a routing item, as KiCad writes it.
fn defining_points(kind: &str) -> &'static [&'static str] {
    match kind {
        "segment" => &["start", "end"],
        "arc" => &["start", "mid", "end"],
        _ => &["at"],
    }
}

/// An item's identity child: `uuid` since KiCad 8, `tstamp` before it.
fn identity_of(node: &SexpNode) -> Option<(&'static str, &str)> {
    ["uuid", "tstamp"]
        .into_iter()
        .find_map(|tag| Some((tag, node.find_str(tag)?)))
}

fn unreadable(reason: String) -> CallToolResult {
    CallToolResult::error(format!(
        "Refusing to copy routing: {reason}. The board file was not modified."
    ))
}

/// Plan the copy of every top-level `segment`, `arc` and `via` whose defining
/// points all lie in the region. An item with only some of them inside is
/// left out and named in `excluded_crossing`, so a copy is never a silent
/// subset of a track. An item whose points cannot be read refuses the whole
/// copy, wherever it is: without them, whether it is in the region is unknown.
pub(crate) fn plan_routing_copy(
    content: &str,
    copy: &RoutingCopy,
) -> Result<PlannedCopy, CallToolResult> {
    crate::tools::pcb_components::check_single_board_form(content).map_err(unreadable)?;
    if !copy.net_map.is_empty() {
        // A destination net with no pads is a typo, not a net: KiCad would
        // create it and DRC would report the copy unconnected.
        let tree = parse_sexp(content)
            .map_err(|e| unreadable(format!("the board does not parse: {e}")))?;
        if !names_nets_in_place(&tree) {
            return Err(invalid_arg(
                "net_map",
                "this board references nets by number, as boards before KiCad 10 do, and \
                 net_map renames by name; resave the board in KiCad 10 or omit net_map",
            ));
        }
        let nets = collect_net_keys(&tree);
        let mut map: Vec<_> = copy.net_map.iter().collect();
        map.sort();
        if let Some((from, to)) = map.into_iter().find(|(_, to)| !nets.contains(*to)) {
            return Err(invalid_arg(
                &format!("net_map.{from}"),
                &format!("net '{to}' is not on this board"),
            ));
        }
    }
    let [x1, y1, x2, y2] = copy.region;
    let inside = |&(x, y): &(f64, f64)| x >= x1 && x <= x2 && y >= y1 && y <= y2;
    let eol = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };

    let mut insertion = String::new();
    let mut insert_at = content.len();
    let mut uuids = Vec::new();
    let mut counts = [0usize; 3];
    let mut excluded_crossing = Vec::new();
    for (start, end) in find_direct_child_blocks(content, "kicad_pcb") {
        let block = &content[start..end];
        let Some(kind_index) =
            block_tag(block).and_then(|tag| ROUTING_KINDS.iter().position(|k| *k == tag))
        else {
            continue;
        };
        let kind = ROUTING_KINDS[kind_index];
        // Copies go after the last routing item, where KiCad keeps them.
        insert_at = end;
        let node =
            parse_sexp(block).map_err(|e| unreadable(format!("a {kind} does not parse: {e}")))?;
        let (identity_tag, id) =
            identity_of(&node).ok_or_else(|| unreadable(format!("a {kind} has no uuid")))?;
        let points = defining_points(kind);
        let xys: Vec<(f64, f64)> = points
            .iter()
            .map(|p| match node.find_all(p).as_slice() {
                [one] => exact_coordinate_pair(one),
                _ => None,
            })
            .collect::<Option<_>>()
            .ok_or_else(|| {
                unreadable(format!(
                    "{kind} {id} does not have exactly one readable ({}) point each",
                    points.join(", ")
                ))
            })?;
        if !xys.iter().any(inside) {
            continue;
        }
        if !xys.iter().all(inside) {
            excluded_crossing.push(json!({ "kind": kind, "uuid": id }));
            continue;
        }

        let new_uuid = new_uuid();
        let mut edits = Vec::new();
        for (child_start, child_end) in find_direct_child_blocks(block, kind) {
            let child = &block[child_start..child_end];
            let replacement = match block_tag(child) {
                Some(tag) if points.contains(&tag) => {
                    let (x, y) = xys[points.iter().position(|p| *p == tag).expect("guarded")];
                    format!("({tag} {} {})", round6(x + copy.dx), round6(y + copy.dy))
                }
                Some(tag) if tag == identity_tag => format!("({tag} \"{new_uuid}\")"),
                Some("net") => match node
                    .find("net")
                    .and_then(net_name)
                    .and_then(|name| copy.net_map.get(name))
                {
                    Some(to) => format!("(net \"{}\")", escape_sexp_string(to)),
                    None => continue,
                },
                _ => continue,
            };
            edits.push(SexpEdit::replace(child_start, child_end, replacement));
        }
        let line_start = content[..start].rfind('\n').map_or(0, |i| i + 1);
        let indent: String = content[line_start..start]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        insertion.push_str(eol);
        insertion.push_str(&indent);
        insertion.push_str(&apply_edits(block.to_string(), edits));
        uuids.push(new_uuid);
        counts[kind_index] += 1;
    }

    let content = (!uuids.is_empty()).then(|| {
        apply_edits(
            content.to_string(),
            vec![SexpEdit::insert(insert_at, insertion)],
        )
    });
    Ok(PlannedCopy {
        content,
        uuids,
        counts,
        excluded_crossing,
    })
}

/// Write the planned copies if the board still holds `expected`.
pub(crate) fn commit_routing_copy(
    board: &Path,
    expected: &str,
    copy: &RoutingCopy,
    planned: &PlannedCopy,
) -> anyhow::Result<CallToolResult> {
    let reply = |counts: [usize; 3]| {
        let by_kind: serde_json::Map<String, Value> = ROUTING_KINDS
            .iter()
            .zip(counts)
            .map(|(kind, n)| (kind.to_string(), json!(n)))
            .collect();
        json!({
            "copied": counts.iter().sum::<usize>(),
            "copied_by_kind": by_kind,
            "dx": copy.dx,
            "dy": copy.dy,
            "uuids": planned.uuids,
            "excluded_crossing": planned.excluded_crossing,
        })
    };
    let Some(next) = &planned.content else {
        let mut reply = reply([0; 3]);
        reply["note"] = json!(
            "No routing item lies wholly inside the source region; the board was not modified"
        );
        return Ok(CallToolResult::json(&reply));
    };

    // Under the lock, this rereads the file after the rename and compares it
    // with `next` byte for byte, so the planned counts are what the board now
    // holds.
    match write_atomic_if_unchanged(board, expected, next) {
        Ok(()) => {}
        Err(SexpError::Conflict { .. }) => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::Conflict {
                    paths: vec![board.display().to_string()],
                },
                "No routing was copied because the board changed after it was read. Read it \
                 again and retry.",
            ))
        }
        Err(error) => return Err(error.into()),
    }

    let mut reply = reply(planned.counts);
    reply["warning"] = json!(crate::tools::pcb_board::FILE_ONLY_EDIT_WARNING);
    Ok(CallToolResult::json(&reply))
}
// ─── Symbol info ──────────────────────────────────────────────────────────────

// ─── Layer constraints ───────────────────────────────────────────────────────

async fn handle_set_layer_constraints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    anyhow::ensure!(
        !layer.is_empty()
            && layer
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || character == '.'
                    || character == '_'),
        "Layer name contains unsupported characters"
    );
    let rules_path = sibling_custom_rules_path(&board);
    let mut content = match tokio::fs::read_to_string(&rules_path).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "(version 1)\n".to_string(),
        Err(error) => return Err(error.into()),
    };
    let mut changed = Vec::new();

    if let Some(clearance) = args["min_clearance"].as_f64() {
        let rule_name = format!("konnect:{layer}:clearance");
        content = upsert_named_rule(
            &content,
            &rule_name,
            &layer_rule(&rule_name, "clearance", clearance, &layer),
        );
        changed.push(format!("clearance = {} on {}", clearance, layer));
    }

    if let Some(trace_width) = args["min_trace_width"].as_f64() {
        let rule_name = format!("konnect:{layer}:track_width");
        content = upsert_named_rule(
            &content,
            &rule_name,
            &layer_rule(&rule_name, "track_width", trace_width, &layer),
        );
        changed.push(format!("min_trace_width = {} on {}", trace_width, layer));
    }

    if !changed.is_empty() {
        write_atomic(&rules_path, &content)?;
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "layer": layer,
            "rules_file": rules_path,
            "changed": changed
        }))
        .unwrap(),
    ))
}

// ─── Check clearance ─────────────────────────────────────────────────────────

async fn handle_check_clearance(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let mode = opt_str(args, "mode").unwrap_or("anchor");
    if !matches!(mode, "anchor" | "courtyard") {
        return Ok(CallToolResult::error(format!(
            "Invalid mode '{mode}'; expected 'anchor' or 'courtyard'"
        )));
    }

    // Prefer the exact unsaved board KiCad owns. Falling back only when the
    // requested board is not open gives both modes the same live/file
    // semantics and prevents a plausible answer from stale placement data.
    let live = with_board_ipc_classified(ctx, &board, move |client, document| {
        client.save_document_to_string_in(document)
    })
    .await?;
    let (content, source) = match live {
        Ok(content) => (content, "ipc"),
        Err(konnect_ipc::IpcFailure::Unreachable(_)) => {
            (std::fs::read_to_string(&board)?, "saved_file")
        }
        Err(konnect_ipc::IpcFailure::Target { error, .. }) if error.proves_not_open() => {
            (std::fs::read_to_string(&board)?, "saved_file")
        }
        Err(konnect_ipc::IpcFailure::Target { error, .. }) => {
            return Ok(crate::tools::ipc_target_error_result(&error));
        }
        Err(konnect_ipc::IpcFailure::Rejected(message)) => {
            return Ok(CallToolResult::error(message));
        }
        Err(konnect_ipc::IpcFailure::Recovered(message)) => {
            return Ok(crate::tools::ipc_recovered_result(&message));
        }
        Err(konnect_ipc::IpcFailure::Uncertain(message)) => {
            return Ok(crate::tools::ipc_uncertain_result(&message));
        }
    };
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    if mode == "courtyard" {
        return courtyard_clearance_result(&tree, &ref1, &ref2, source);
    }

    let pos1 = find_footprint_position(&tree, &ref1)?;
    let pos2 = find_footprint_position(&tree, &ref2)?;

    let dx = pos2.0 - pos1.0;
    let dy = pos2.1 - pos1.1;
    let distance = (dx * dx + dy * dy).sqrt();
    let anchor_distance_mm = (distance * 1000.0).round() / 1000.0;

    // The number has always been the distance between the two placement
    // anchors. Under a description that said "physical clearance" it was read
    // as copper-to-copper spacing and a 21 mm answer stood in for a 3 mm
    // courtyard gap (#410). Stage 1 names what the number is; `distance_mm`
    // stays as a deprecated alias so no consumer breaks before stage 2 adds
    // real geometry.
    Ok(CallToolResult::json(&json!({
        "ref1": ref1,
        "ref2": ref2,
        "mode": "anchor",
        "source": source,
        "pos1": { "x": pos1.0, "y": pos1.1 },
        "pos2": { "x": pos2.0, "y": pos2.1 },
        "measurement": MEASUREMENT_ANCHOR_TO_ANCHOR,
        "anchor_distance_mm": anchor_distance_mm,
        "distance_mm": anchor_distance_mm,
        // A collection takes a self-describing plural noun
        // (docs/NAMING_CONVENTIONS.md): what it holds is field names.
        "deprecated_fields": ["distance_mm"],
        "note": "anchor-to-anchor distance only; not pad, trace or courtyard clearance — \
                 footprint size and shape are not considered. Use run_drc for clearance."
    })))
}

/// Compatibility measurement retained by the default `anchor` mode. The
/// courtyard mode has its own explicitly named result; this value never
/// changes meaning.
const MEASUREMENT_ANCHOR_TO_ANCHOR: &str = "anchor_to_anchor";
const MEASUREMENT_COURTYARD_BBOX_EDGE: &str = "courtyard_bbox_edge_to_edge";

fn footprint_reference(fp: &konnect_sexp::parser::SexpNode) -> Option<&str> {
    fp.find_all("property")
        .iter()
        .find_map(|property| {
            (property.get(1).and_then(|n| n.as_str()) == Some("Reference"))
                .then(|| property.get(2).and_then(|n| n.as_str()))
                .flatten()
        })
        .or_else(|| {
            fp.find_all("fp_text").iter().find_map(|text| {
                (text.get(1).and_then(|n| n.as_str()) == Some("reference"))
                    .then(|| text.get(2).and_then(|n| n.as_str()))
                    .flatten()
            })
        })
}

fn authored_courtyard(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
) -> anyhow::Result<Result<konnect_sexp::board::FootprintCourtyard, &'static str>> {
    let footprint_count = tree
        .find_all("footprint")
        .into_iter()
        .filter(|fp| footprint_reference(fp) == Some(reference))
        .count();
    match footprint_count {
        0 => anyhow::bail!("Footprint '{}' not found on board", reference),
        1 => {}
        count => anyhow::bail!(
            "Footprint reference '{}' is ambiguous: found {} instances",
            reference,
            count
        ),
    }

    let scan = konnect_sexp::board::footprint_courtyards(tree);
    let Some(courtyard) = scan
        .items
        .iter()
        .find(|item| item.reference.as_deref() == Some(reference))
    else {
        return Ok(Err("courtyard_geometry_unreadable"));
    };
    if courtyard.bbox_source != konnect_sexp::board::CourtyardSource::Courtyard {
        return Ok(Err("authored_courtyard_missing"));
    }
    Ok(Ok(courtyard.clone()))
}

fn side_name(side: konnect_sexp::board::Side) -> &'static str {
    match side {
        konnect_sexp::board::Side::Front => "front",
        konnect_sexp::board::Side::Back => "back",
    }
}

fn bbox_json(bbox: (f64, f64, f64, f64)) -> serde_json::Value {
    json!({
        "min_x": bbox.0,
        "min_y": bbox.1,
        "max_x": bbox.2,
        "max_y": bbox.3
    })
}

fn courtyard_bbox_spacing(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> (f64, bool) {
    let dx = (a.0 - b.2).max(b.0 - a.2).max(0.0);
    let dy = (a.1 - b.3).max(b.1 - a.3).max(0.0);
    let clearance = ((dx * dx + dy * dy).sqrt() * 1000.0).round() / 1000.0;
    let overlaps = a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3;
    (clearance, overlaps)
}

fn courtyard_clearance_result(
    tree: &konnect_sexp::parser::SexpNode,
    ref1: &str,
    ref2: &str,
    source: &str,
) -> anyhow::Result<CallToolResult> {
    let first = authored_courtyard(tree, ref1)?;
    let second = authored_courtyard(tree, ref2)?;
    if let Err(reason_code) = first {
        return Ok(courtyard_unavailable(ref1, ref2, source, ref1, reason_code));
    }
    if let Err(reason_code) = second {
        return Ok(courtyard_unavailable(ref1, ref2, source, ref2, reason_code));
    }
    let first = first.expect("checked above");
    let second = second.expect("checked above");
    let side1 = side_name(first.layer_side);
    let side2 = side_name(second.layer_side);
    if first.layer_side != second.layer_side {
        return Ok(CallToolResult::json(&json!({
            "ref1": ref1,
            "ref2": ref2,
            "mode": "courtyard",
            "source": source,
            "measurement": MEASUREMENT_COURTYARD_BBOX_EDGE,
            "available": false,
            "applicable": false,
            "courtyard_clearance_mm": serde_json::Value::Null,
            "overlaps": serde_json::Value::Null,
            "side1": side1,
            "side2": side2,
            "reason_code": "opposite_board_sides",
            "note": "Opposite-side courtyard spacing is not a same-side placement collision. Use run_drc for through-board and copper clearance."
        })));
    }

    let a = first.bbox;
    let b = second.bbox;
    let (clearance, overlaps) = courtyard_bbox_spacing(a, b);
    Ok(CallToolResult::json(&json!({
        "ref1": ref1,
        "ref2": ref2,
        "mode": "courtyard",
        "source": source,
        "measurement": MEASUREMENT_COURTYARD_BBOX_EDGE,
        "available": true,
        "applicable": true,
        "courtyard_clearance_mm": clearance,
        "overlaps": overlaps,
        "side1": side1,
        "side2": side2,
        "geometry1": {
            "source": "authored_courtyard_bbox",
            "bbox": bbox_json(a),
            "rotation_deg": first.rotation_deg
        },
        "geometry2": {
            "source": "authored_courtyard_bbox",
            "bbox": bbox_json(b),
            "rotation_deg": second.rotation_deg
        },
        "note": "Axis-aligned board-space hull distance between authored courtyards; zero with overlaps=true means the hulls overlap. This is not copper clearance; use run_drc for electrical clearance."
    })))
}

fn courtyard_unavailable(
    ref1: &str,
    ref2: &str,
    source: &str,
    unavailable_reference: &str,
    reason_code: &str,
) -> CallToolResult {
    CallToolResult::json(&json!({
        "ref1": ref1,
        "ref2": ref2,
        "mode": "courtyard",
        "source": source,
        "measurement": MEASUREMENT_COURTYARD_BBOX_EDGE,
        "available": false,
        "applicable": serde_json::Value::Null,
        "courtyard_clearance_mm": serde_json::Value::Null,
        "overlaps": serde_json::Value::Null,
        "unavailable_reference": unavailable_reference,
        "reason_code": reason_code,
        "note": "An authored, fully readable courtyard is required for each footprint; pads and anchors are not substituted."
    }))
}

/// Look up the board-space (x, y) position of a footprint by its reference designator.
fn find_footprint_position(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
) -> anyhow::Result<(f64, f64)> {
    let matches: Vec<_> = tree
        .find_all("footprint")
        .into_iter()
        .filter(|fp| footprint_reference(fp) == Some(reference))
        .collect();
    let fp_node = match matches.as_slice() {
        [] => anyhow::bail!("Footprint '{}' not found on board", reference),
        [fp] => *fp,
        many => anyhow::bail!(
            "Footprint reference '{}' is ambiguous: found {} instances",
            reference,
            many.len()
        ),
    };

    let fp_at = fp_node
        .find("at")
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' has no placement anchor", reference))?;
    let fp_x = fp_at
        .get_f64(1)
        .filter(|value| value.is_finite())
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' has an invalid X anchor", reference))?;
    let fp_y = fp_at
        .get_f64(2)
        .filter(|value| value.is_finite())
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' has an invalid Y anchor", reference))?;

    Ok((fp_x, fp_y))
}

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

    #[test]
    fn health_timeout_is_bounded_and_typed() {
        assert_eq!(health_timeout_seconds(&json!({})).unwrap(), 5);
        assert_eq!(
            health_timeout_seconds(&json!({ "timeout_seconds": 17 })).unwrap(),
            17
        );
        for invalid in [json!(0), json!(301), json!(1.5), json!("5")] {
            let error = health_timeout_seconds(&json!({ "timeout_seconds": invalid }))
                .expect_err("out-of-range or non-integer timeout must be refused");
            assert!(error.is_error);
        }
    }

    #[test]
    fn standalone_editors_count_as_kicad_ui_processes() {
        for name in ["kicad", "pcbnew", "eeschema", "PCBNEW.EXE"] {
            assert!(is_kicad_process_name(name), "did not recognize {name}");
        }
        assert!(!is_kicad_process_name("kicad-cli"));
        assert!(!is_kicad_process_name("freerouting"));
        assert!(process_list_has_kicad(
            "/usr/bin/Finder\n/Applications/KiCad/pcbnew\n"
        ));
    }

    #[test]
    fn responsive_ipc_is_sufficient_running_evidence() {
        assert!(ui_running(false, true));
        assert!(ui_running(true, false));
        assert!(!ui_running(false, false));
    }

    #[tokio::test]
    async fn check_kicad_ui_names_why_ipc_did_not_answer() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx();
        ctx.config.ipc_address =
            format!("ipc://{}", dir.path().join("no-kicad-here.sock").display());

        let result = handle_check_kicad_ui(&json!({ "timeout_seconds": 60 }), &ctx)
            .await
            .unwrap();
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let response: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(response["timed_out"], false, "{response}");
        assert_eq!(response["ipc_responsive"], false, "{response}");
        assert_eq!(response["ipc_failure"]["kind"], "no_listener", "{response}");
        assert!(
            response["ipc_failure"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("Nothing is listening there")),
            "{response}"
        );
    }

    #[tokio::test]
    async fn health_deadline_returns_without_waiting_for_the_inner_future() {
        let timed_out = bounded_health_check(std::time::Duration::from_millis(1), async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            1
        })
        .await;
        assert!(timed_out.is_err());

        let completed = bounded_health_check(std::time::Duration::from_secs(1), async { 2 })
            .await
            .unwrap();
        assert_eq!(completed, 2);
    }

    fn blank_board() -> &'static str {
        "(kicad_pcb\n  (version 20250610)\n  (generator \"test\")\n  (general (thickness 1.6))\n  (paper \"A4\")\n  (layers\n    (0 \"F.Cu\" signal)\n    (31 \"B.Cu\" signal)\n    (44 \"Edge.Cuts\" user)\n  )\n  (setup (pad_to_mask_clearance 0))\n  (net 0 \"\")\n)\n"
    }

    fn blank_project() -> &'static str {
        "{\n  \"meta\": {\"filename\": \"board.kicad_pro\", \"version\": 1},\n  \"board\": {\"design_settings\": {}},\n  \"schematic\": {}\n}\n"
    }

    #[tokio::test]
    async fn set_design_rules_updates_project_json_without_touching_board() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        let project = dir.path().join("board.kicad_pro");
        tokio::fs::write(&board, blank_board()).await.unwrap();
        tokio::fs::write(&project, blank_project()).await.unwrap();
        let original_board = tokio::fs::read(&board).await.unwrap();

        let args = json!({
            "board": board,
            "min_clearance": 0.25,
            "min_trace_width": 0.25,
            "min_via_drill": 0.30,
            "min_via_size": 0.70,
            "min_hole_to_hole": 0.45
        });
        let result = handle_set_design_rules(&args, &test_ctx()).await.unwrap();
        assert!(!result.is_error);

        assert_eq!(tokio::fs::read(&board).await.unwrap(), original_board);
        let project_json: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(&project).await.unwrap()).unwrap();
        let rules = &project_json["board"]["design_settings"]["rules"];
        assert_eq!(rules["min_clearance"], 0.25);
        assert_eq!(rules["min_track_width"], 0.25);
        assert_eq!(rules["min_through_hole_diameter"], 0.30);
        assert_eq!(rules["min_via_diameter"], 0.70);
        assert_eq!(rules["min_hole_to_hole"], 0.45);
    }

    fn text_of(result: &CallToolResult) -> String {
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn project_json(project: &std::path::Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(project).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn set_predefined_sizes_writes_palette_and_leaves_the_board_alone() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        let project = dir.path().join("board.kicad_pro");
        tokio::fs::write(&board, blank_board()).await.unwrap();
        tokio::fs::write(&project, blank_project()).await.unwrap();
        let original_board = tokio::fs::read(&board).await.unwrap();

        let result = handle_set_predefined_sizes(
            &json!({
                "board": board,
                "track_widths": [0.2, 0.5, 0.8],
                "via_dimensions": [
                    { "diameter": 0.6, "drill": 0.3 },
                    { "diameter": 0.8, "drill": 0.4 }
                ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        assert_eq!(tokio::fs::read(&board).await.unwrap(), original_board);

        let stored = project_json(&project);
        assert_eq!(
            stored["board"]["design_settings"]["track_widths"],
            json!([0.0, 0.2, 0.5, 0.8])
        );
        assert_eq!(
            stored["board"]["design_settings"]["via_dimensions"],
            json!([
                { "diameter": 0.0, "drill": 0.0 },
                { "diameter": 0.6, "drill": 0.3 },
                { "diameter": 0.8, "drill": 0.4 }
            ])
        );
        assert_eq!(stored["meta"]["filename"], json!("board.kicad_pro"));
    }

    #[tokio::test]
    async fn get_predefined_sizes_reads_what_set_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        let project = dir.path().join("board.kicad_pro");
        tokio::fs::write(&board, blank_board()).await.unwrap();
        tokio::fs::write(&project, blank_project()).await.unwrap();
        handle_set_predefined_sizes(
            &json!({ "board": board, "track_widths": [0.2] }),
            &test_ctx(),
        )
        .await
        .unwrap();

        let result = handle_get_predefined_sizes(&json!({ "board": board }), &test_ctx())
            .await
            .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        let body: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(body["track_widths"], json!([0.0, 0.2]));
        assert_eq!(
            body["via_dimensions"],
            json!([{ "diameter": 0.0, "drill": 0.0 }])
        );
    }

    #[tokio::test]
    async fn set_predefined_sizes_without_a_project_file_refuses_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        tokio::fs::write(&board, blank_board()).await.unwrap();
        let original_board = tokio::fs::read(&board).await.unwrap();

        let result = handle_set_predefined_sizes(
            &json!({ "board": board, "track_widths": [0.2] }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{}", text_of(&result));
        assert!(
            text_of(&result).contains("kicad_pro"),
            "{}",
            text_of(&result)
        );
        assert_eq!(tokio::fs::read(&board).await.unwrap(), original_board);
        assert!(!dir.path().join("board.kicad_pro").exists());
    }

    #[tokio::test]
    async fn set_predefined_sizes_refuses_a_via_with_no_annular_ring() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        let project = dir.path().join("board.kicad_pro");
        tokio::fs::write(&board, blank_board()).await.unwrap();
        tokio::fs::write(&project, blank_project()).await.unwrap();
        let original_project = tokio::fs::read(&project).await.unwrap();

        let result = handle_set_predefined_sizes(
            &json!({
                "board": board,
                "via_dimensions": [{ "diameter": 0.3, "drill": 0.3 }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{}", text_of(&result));
        assert!(
            text_of(&result).contains("greater than drill"),
            "{}",
            text_of(&result)
        );
        assert_eq!(tokio::fs::read(&project).await.unwrap(), original_project);
    }

    #[tokio::test]
    async fn set_predefined_sizes_omitting_vias_leaves_existing_vias_alone() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        let project = dir.path().join("board.kicad_pro");
        tokio::fs::write(&board, blank_board()).await.unwrap();
        tokio::fs::write(&project, blank_project()).await.unwrap();
        handle_set_predefined_sizes(
            &json!({
                "board": board,
                "track_widths": [0.2],
                "via_dimensions": [{ "diameter": 0.6, "drill": 0.3 }]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();

        let result = handle_set_predefined_sizes(
            &json!({ "board": board, "track_widths": [0.2, 0.5] }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        let stored = project_json(&project);
        assert_eq!(
            stored["board"]["design_settings"]["track_widths"],
            json!([0.0, 0.2, 0.5])
        );
        assert_eq!(
            stored["board"]["design_settings"]["via_dimensions"],
            json!([
                { "diameter": 0.0, "drill": 0.0 },
                { "diameter": 0.6, "drill": 0.3 }
            ])
        );
    }

    #[tokio::test]
    async fn set_layer_constraints_writes_idempotent_custom_rules_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        tokio::fs::write(&board, blank_board()).await.unwrap();
        let original_board = tokio::fs::read(&board).await.unwrap();
        let args = json!({
            "board": board,
            "layer": "F.Cu",
            "min_clearance": 0.25,
            "min_trace_width": 0.25
        });

        for _ in 0..2 {
            let result = handle_set_layer_constraints(&args, &test_ctx())
                .await
                .unwrap();
            assert!(!result.is_error);
        }

        assert_eq!(tokio::fs::read(&board).await.unwrap(), original_board);
        let rules = tokio::fs::read_to_string(dir.path().join("board.kicad_dru"))
            .await
            .unwrap();
        assert!(rules.starts_with("(version 1)"));
        assert_eq!(rules.matches("(rule \"konnect:F.Cu:clearance\"").count(), 1);
        assert_eq!(
            rules.matches("(rule \"konnect:F.Cu:track_width\"").count(),
            1
        );
        assert!(rules.contains("(constraint clearance (min 0.25mm))"));
        assert!(rules.contains("(constraint track_width (min 0.25mm))"));
        assert!(rules.contains("(layer \"F.Cu\")"));
    }

    /// #410 stage 1: the number is the distance between two placement
    /// anchors, and the response now says so. Real KiCad-saved board
    /// (`specctra_two_resistors.kicad_pcb`, see its README).
    #[tokio::test]
    async fn check_clearance_names_its_measurement_and_keeps_the_alias() {
        let board = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/specctra_two_resistors.kicad_pcb");
        let result = handle_check_clearance(
            &json!({ "board": board.to_str().unwrap(), "ref1": "R1", "ref2": "R2" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        let value: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();

        assert_eq!(value["measurement"], MEASUREMENT_ANCHOR_TO_ANCHOR);
        assert_eq!(value["mode"], "anchor");
        assert_eq!(value["source"], "saved_file");
        // The alias is the same number, not a second measurement.
        assert_eq!(value["anchor_distance_mm"], value["distance_mm"]);
        assert_eq!(value["deprecated_fields"], json!(["distance_mm"]));
        assert!(value.get("deprecated").is_none());
        // And the number is exactly the anchor-to-anchor distance of the two
        // positions the response itself reports.
        let dx = value["pos2"]["x"].as_f64().unwrap() - value["pos1"]["x"].as_f64().unwrap();
        let dy = value["pos2"]["y"].as_f64().unwrap() - value["pos1"]["y"].as_f64().unwrap();
        let expected = ((dx * dx + dy * dy).sqrt() * 1000.0).round() / 1000.0;
        assert_eq!(value["anchor_distance_mm"].as_f64().unwrap(), expected);
        assert!(value["note"]
            .as_str()
            .unwrap()
            .contains("not pad, trace or courtyard clearance"));
    }

    fn sexp_fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../konnect-sexp/tests/fixtures")
            .join(name)
    }

    /// #410 stage 2: values are pinned independently in the konnect-sexp
    /// fixture tests from KiCad-authored geometry. C1 and R1 are both rotated;
    /// their transformed courtyard hulls have a 4.455 mm vertical gap.
    /// Anchor distance is deliberately much larger, which is the negative
    /// control against restoring the original implementation.
    #[tokio::test]
    async fn check_clearance_measures_rotated_authored_courtyards_not_anchors() {
        let board = sexp_fixture("ecc83-pp.kicad_pcb");
        let courtyard = handle_check_clearance(
            &json!({
                "board": board,
                "ref1": "C1",
                "ref2": "R1",
                "mode": "courtyard"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!courtyard.is_error, "{}", text_of(&courtyard));
        let courtyard: serde_json::Value = serde_json::from_str(&text_of(&courtyard)).unwrap();
        assert_eq!(courtyard["measurement"], MEASUREMENT_COURTYARD_BBOX_EDGE);
        assert_eq!(courtyard["available"], true);
        assert_eq!(courtyard["applicable"], true);
        assert_eq!(courtyard["courtyard_clearance_mm"], 4.455);
        assert_eq!(courtyard["overlaps"], false);
        assert_eq!(courtyard["geometry1"]["rotation_deg"], 90.0);
        assert_eq!(courtyard["geometry2"]["rotation_deg"], -90.0);

        let anchor = handle_check_clearance(
            &json!({ "board": board, "ref1": "C1", "ref2": "R1" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let anchor: serde_json::Value = serde_json::from_str(&text_of(&anchor)).unwrap();
        assert!(anchor["anchor_distance_mm"].as_f64().unwrap() > 8.0);
        assert_ne!(
            anchor["anchor_distance_mm"],
            courtyard["courtyard_clearance_mm"]
        );
    }

    #[tokio::test]
    async fn check_clearance_refuses_pad_or_anchor_fallback_as_courtyard() {
        let result = handle_check_clearance(
            &json!({
                "board": sexp_fixture("RoyalBlue54L-NFC-Antenna.kicad_pcb"),
                "ref1": "J1",
                "ref2": "REF**",
                "mode": "courtyard"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        let value: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(value["available"], false);
        assert_eq!(value["courtyard_clearance_mm"], serde_json::Value::Null);
        assert_eq!(value["unavailable_reference"], "J1");
        assert_eq!(value["reason_code"], "authored_courtyard_missing");
    }

    #[tokio::test]
    async fn check_clearance_reports_malformed_courtyard_as_unavailable() {
        let original = std::fs::read_to_string(sexp_fixture("ecc83-pp.kicad_pcb")).unwrap();
        let malformed = original.replacen("(end 7.75 0)", "(end unreadable 0)", 1);
        assert_ne!(malformed, original, "fixture premise changed");
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("malformed.kicad_pcb");
        std::fs::write(&board, malformed).unwrap();

        let result = handle_check_clearance(
            &json!({
                "board": board,
                "ref1": "C1",
                "ref2": "R1",
                "mode": "courtyard"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        let value: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(value["available"], false);
        assert_eq!(value["unavailable_reference"], "C1");
        assert_eq!(value["reason_code"], "courtyard_geometry_unreadable");
    }

    #[tokio::test]
    async fn check_clearance_reports_same_side_courtyard_overlap_explicitly() {
        let original = std::fs::read_to_string(sexp_fixture("ecc83-pp.kicad_pcb")).unwrap();
        let overlapping =
            original.replacen("(at 136.271 107.95 -90)", "(at 141.605 99.695 -90)", 1);
        assert_ne!(overlapping, original, "fixture premise changed");
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("overlap.kicad_pcb");
        std::fs::write(&board, overlapping).unwrap();

        let result = handle_check_clearance(
            &json!({
                "board": board,
                "ref1": "C1",
                "ref2": "R1",
                "mode": "courtyard"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        let value: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(value["available"], true);
        assert_eq!(value["courtyard_clearance_mm"], 0.0);
        assert_eq!(value["overlaps"], true);
    }

    #[tokio::test]
    async fn check_clearance_marks_opposite_board_sides_not_applicable() {
        let result = handle_check_clearance(
            &json!({
                "board": sexp_fixture("pic_programmer.kicad_pcb"),
                "ref1": "C1",
                "ref2": "JP1",
                "mode": "courtyard"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        let value: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(value["available"], false);
        assert_eq!(value["applicable"], false);
        assert_eq!(value["reason_code"], "opposite_board_sides");
        assert_eq!(value["side1"], "front");
        assert_eq!(value["side2"], "back");
    }

    #[test]
    fn courtyard_bbox_spacing_distinguishes_overlap_from_touching() {
        assert_eq!(
            courtyard_bbox_spacing((0.0, 0.0, 2.0, 2.0), (1.0, 1.0, 3.0, 3.0)),
            (0.0, true)
        );
        assert_eq!(
            courtyard_bbox_spacing((0.0, 0.0, 2.0, 2.0), (2.0, 0.0, 4.0, 2.0)),
            (0.0, false)
        );
    }

    /// The description is the contract an LLM reads before calling. It must
    /// not claim clearance again; this pins the wording, not just the code.
    #[test]
    fn check_clearance_schema_and_description_name_both_measurements() {
        let tool = tools()
            .into_iter()
            .find(|tool| tool.name == "check_clearance")
            .expect("check_clearance is registered");
        let description = tool.description.to_lowercase();
        assert!(description.contains("anchor"), "{description}");
        assert!(description.contains("courtyard"), "{description}");
        assert!(
            description.contains("neither mode measures pad, trace or copper clearance"),
            "{description}"
        );
        let modes = &tool.input_schema["properties"]["mode"]["enum"];
        assert_eq!(modes, &json!(["anchor", "courtyard"]));
        assert_eq!(tool.input_schema["properties"]["mode"]["default"], "anchor");
    }
}

/// `copy_routing_pattern` declares all six coordinates required and defaulted
/// each to 0.0. Omitting all six was harmless — the source box collapsed to a
/// point and matched nothing — but omitting only the destination silently
/// duplicated the whole source region onto the board origin and wrote it,
/// reporting `{"copied": N}` (#218).
#[cfg(test)]
mod required_coordinate_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use serde_json::json;
    use std::sync::Arc;

    fn ctx() -> Arc<ToolContext> {
        Arc::new(ToolContext::new(
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
        ))
    }

    #[tokio::test]
    async fn every_missing_coordinate_is_refused_by_name_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("b.kicad_pcb");
        let original = "(kicad_pcb\n  (version 20250610)\n  (generator \"konnect\")\n  \
                        (paper \"A4\")\n  (net 0 \"\")\n)\n";
        std::fs::write(&board, original).unwrap();

        let all = [
            ("src_x1", 1.0),
            ("src_y1", 2.0),
            ("src_x2", 3.0),
            ("src_y2", 4.0),
            ("dest_x", 5.0),
            ("dest_y", 6.0),
        ];
        let def = tools()
            .into_iter()
            .find(|t| t.name == "copy_routing_pattern")
            .expect("registered");

        // Leave out exactly one each time: the partial omission is the case
        // that used to write.
        for (omitted, _) in all {
            let mut args = json!({ "board": board.display().to_string() });
            for (key, value) in all {
                if key != omitted {
                    args[key] = json!(value);
                }
            }
            let result = (def.handler)(&args, ctx()).await.expect("no anyhow");
            assert!(result.is_error, "omitting {omitted} must be refused");

            let text = match result.content.first() {
                Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
            assert_eq!(parsed["error"]["kind"], "invalid_argument", "{omitted}");
            assert_eq!(
                parsed["error"]["field"], omitted,
                "the refusal must name the coordinate that is missing"
            );
            assert_eq!(
                std::fs::read_to_string(&board).unwrap(),
                original,
                "a refused copy must not touch the board"
            );
        }
    }
}
