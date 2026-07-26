//! `sch_export` toolset — export, netlist, ERC, connectivity fix, board sync.
//!
//! All export operations delegate to `kicad-cli` via the `cli` module.
//! `export_netlist_summary` and `fix_connectivity` operate directly on
//! S-expression file content so they work without a running KiCAD instance.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, ToolContext, ToolDef};
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident},
    schematic::{
        extract_labels, extract_lib_pins, extract_symbol_instances, extract_wires, pin_endpoint,
        read_schematic,
    },
    writer::{
        apply_edits, find_block_with_leading_whitespace, write_atomic_if_unchanged, SexpEdit,
    },
};
use serde_json::json;

use super::cli;
use super::sch_analysis::build_net_graph;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "export_schematic_svg",
            "Export a schematic sheet to an SVG file using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Output SVG file path (directory used as output dir)" },
                    "black_and_white": { "type": "boolean", "description": "Render in black and white", "default": false },
                    "theme": { "type": "string", "description": "KiCAD colour theme name (optional)" }
                },
                "required": ["schematic", "output"]
            }),
            |args, ctx| async move { handle_export_svg(args, ctx).await }
        ),
        tool!(
            "export_schematic_pdf",
            "Export a schematic sheet to a PDF file using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Output PDF file path" },
                    "black_and_white": { "type": "boolean", "description": "Render in black and white", "default": false },
                    "all_sheets": { "type": "boolean", "description": "Include all hierarchical sheets", "default": true }
                },
                "required": ["schematic", "output"]
            }),
            |args, ctx| async move { handle_export_pdf(args, ctx).await }
        ),
        tool!(
            "generate_netlist",
            "Generate a KiCAD netlist file from the schematic using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Output .net file path" },
                    "format": {
                        "type": "string",
                        "description": "Netlist format: 'kicad', 'orcadpcb2', 'cadstar', 'spice'",
                        "default": "kicad"
                    }
                },
                "required": ["schematic", "output"]
            }),
            |args, ctx| async move { handle_generate_netlist(args, ctx).await }
        ),
        tool!(
            "export_netlist_summary",
            "Return a human-readable JSON summary of the schematic netlist: all \
             components, their nets, pin counts. Does not require kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_export_netlist_summary(args, ctx).await }
        ),
        tool!(
            "run_erc",
            "Run the Electrical Rules Check (ERC) on the schematic via kicad-cli \
             and return a list of violations filtered by severity.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Optional path to write ERC report JSON" },
                    "severity":  {
                        "type": "string",
                        "description": "Minimum severity to report: 'error', 'warning', 'info'",
                        "default": "warning"
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_run_erc(args, ctx).await }
        ),
        tool!(
            "fix_connectivity",
            "Scan the schematic for near-miss wire endpoints (within snap_tolerance of a \
             pin or label but not exactly on it) and snap them into place. Use dry_run \
             to preview fixes without writing.",
            json!({
                "type": "object",
                "properties": {
                    "schematic":       { "type": "string", "description": "Path to .kicad_sch file" },
                    "snap_tolerance":  { "type": "number", "description": "Snap distance in mm", "default": 0.05 },
                    "dry_run":         { "type": "boolean", "description": "Report fixes without applying them", "default": false }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_fix_connectivity(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_export_svg(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let output_path = get_path(args, "output")?;

    // kicad-cli writes to an output directory and names the file <stem>.svg
    let output_dir = output_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    std::fs::create_dir_all(&output_dir)?;

    let svg_path = cli::export_schematic_svg(&ctx.config.kicad_cli, &sch_path, &output_dir).await?;

    Ok(CallToolResult::json(&json!({
        "exported": svg_path.display().to_string()
    })))
}

async fn handle_export_pdf(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let output_path = get_path(args, "output")?;

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    cli::export_schematic_pdf(&ctx.config.kicad_cli, &sch_path, &output_path).await?;

    Ok(CallToolResult::json(&json!({
        "exported": output_path.display().to_string()
    })))
}

async fn handle_generate_netlist(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let output_path = get_path(args, "output")?;
    let format = args["format"].as_str().unwrap_or("kicad");

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    cli::export_netlist(&ctx.config.kicad_cli, &sch_path, &output_path, format).await?;

    Ok(CallToolResult::json(&json!({
        "exported": output_path.display().to_string(),
        "format": format
    })))
}

async fn handle_export_netlist_summary(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let (_, tree) = read_schematic(&sch_path)?;

    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut g = build_net_graph(&wires, &labels);

    // Collect distinct net names
    let mut net_names: Vec<String> = labels.iter().map(|l| l.net.clone()).collect();
    net_names.sort();
    net_names.dedup();

    // Build per-component net map
    let components: Vec<serde_json::Value> = instances
        .iter()
        .map(|inst| {
            let lib_sym = lib_syms
                .iter()
                .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));

            let pins: Vec<serde_json::Value> = if let Some(sym) = lib_sym {
                let t = inst.pin_transform();
                extract_lib_pins(sym)
                    .iter()
                    .map(|p| {
                        let (px, py) = pin_endpoint(p, t);
                        let net = g.net_at(px, py).unwrap_or_else(|| "~".to_string());
                        json!({
                            "number": p.number,
                            "name": p.name,
                            "net": net,
                            "x": px, "y": py
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };

            json!({
                "reference": inst.reference,
                "value": inst.value,
                "footprint": inst.footprint,
                "lib_id": inst.lib_id,
                "pin_count": pins.len(),
                "pins": pins
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "component_count": components.len(),
        "net_count": net_names.len(),
        "nets": net_names,
        "components": components
    })))
}

async fn handle_run_erc(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let min_severity = args["severity"].as_str().unwrap_or("warning");

    let violations = cli::run_erc(&ctx.config.kicad_cli, &sch_path).await?;

    let severity_rank = |s: &str| match s {
        "error" => 2,
        "warning" => 1,
        _ => 0,
    };
    let min_rank = severity_rank(min_severity);

    let filtered: Vec<serde_json::Value> = violations
        .iter()
        .filter(|v| severity_rank(&v.severity) >= min_rank)
        .map(|v| {
            let mut entry = json!({
                "severity": v.severity,
                "description": v.description,
            });
            if let Some(sheet) = &v.sheet {
                entry["sheet"] = json!(sheet);
            }
            if let Some(pos) = &v.pos {
                entry["x"] = json!(pos.x);
                entry["y"] = json!(pos.y);
            }
            entry
        })
        .collect();

    // Optionally write the report to a file
    if let Some(out_path) = args["output"].as_str() {
        let report = serde_json::to_string_pretty(&filtered)?;
        std::fs::write(out_path, report)?;
    }

    let error_count = filtered.iter().filter(|v| v["severity"] == "error").count();
    let warning_count = filtered
        .iter()
        .filter(|v| v["severity"] == "warning")
        .count();

    Ok(CallToolResult::json(&json!({
        "total": filtered.len(),
        "errors": error_count,
        "warnings": warning_count,
        "violations": filtered
    })))
}

async fn handle_fix_connectivity(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let snap_tol = args["snap_tolerance"].as_f64().unwrap_or(0.05);
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let exact_tol = 0.01_f64;

    let (content, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Collect all valid snap targets: pin endpoints + label positions + wire endpoints
    let mut snap_targets: Vec<(f64, f64)> = Vec::new();

    for inst in &instances {
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            for pin in extract_lib_pins(sym) {
                snap_targets.push(pin_endpoint(&pin, t));
            }
        }
    }
    for l in &labels {
        snap_targets.push((l.x, l.y));
    }
    for w in &wires {
        snap_targets.push((w.x1, w.y1));
        snap_targets.push((w.x2, w.y2));
    }

    let mut fixes: Vec<serde_json::Value> = Vec::new();
    let mut file_edits: Vec<SexpEdit> = Vec::new();

    for w in &wires {
        for (is_start, (px, py)) in &[(true, (w.x1, w.y1)), (false, (w.x2, w.y2))] {
            let px = *px;
            let py = *py;
            // Count how many targets are exactly at this point
            // (count >= 2 → there is at least one other connected thing)
            let exact_count = snap_targets
                .iter()
                .filter(|(tx, ty)| points_coincident(px, py, *tx, *ty, exact_tol))
                .count();

            if exact_count >= 2 {
                continue; // already connected
            }
            // Also consider T-junctions (endpoint in middle of another wire)
            if wires.iter().any(|w2| {
                point_on_segment(px, py, w2.x1, w2.y1, w2.x2, w2.y2, exact_tol)
                    && !points_coincident(px, py, w2.x1, w2.y1, exact_tol)
                    && !points_coincident(px, py, w2.x2, w2.y2, exact_tol)
            }) {
                continue; // T-junction — already connected
            }

            // Look for a near-miss snap target within snap_tol
            let near = snap_targets.iter().find(|(tx, ty)| {
                let dist = ((px - tx).powi(2) + (py - ty).powi(2)).sqrt();
                dist > exact_tol && dist <= snap_tol
            });

            if let Some(&(tx, ty)) = near {
                fixes.push(json!({
                    "wire_uuid": w.uuid,
                    "endpoint": if *is_start { "start" } else { "end" },
                    "from": { "x": px, "y": py },
                    "to":   { "x": tx, "y": ty }
                }));

                if !dry_run {
                    // Find the wire block by UUID and replace the coordinate
                    if let Some(uuid_str) = &w.uuid {
                        let uuid_pat = format!(r#"(uuid "{uuid_str}")"#);
                        if let Some(uuid_pos) = content.find(&uuid_pat) {
                            let before = &content[..uuid_pos];
                            if let Some(ws) = before.rfind("\n  (wire").map(|p| p + 1) {
                                if let Some((wbs, wbe)) =
                                    find_block_with_leading_whitespace(&content, ws)
                                {
                                    let wire_block = &content[wbs..wbe];
                                    let coord_prefix = if *is_start { "(start " } else { "(end " };
                                    if let Some(coord_rel) = wire_block.find(coord_prefix) {
                                        let vals_abs = wbs + coord_rel + coord_prefix.len();
                                        let close_rel =
                                            wire_block[coord_rel..].find(')').unwrap_or(0);
                                        let vals_end = wbs + coord_rel + close_rel;
                                        file_edits.push(SexpEdit::replace(
                                            vals_abs,
                                            vals_end,
                                            format!("{tx} {ty}"),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let applied_count = if dry_run { 0 } else { file_edits.len() };
    if applied_count > 0 {
        let expected = content.clone();
        let new_content = apply_edits(content, file_edits);
        write_atomic_if_unchanged(&sch_path, &expected, &new_content)?;
    }

    Ok(CallToolResult::json(&json!({
        "fixes_found": fixes.len(),
        "applied": !dry_run && !fixes.is_empty(),
        "dry_run": dry_run,
        "fixes": fixes
    })))
}
