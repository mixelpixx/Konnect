//! `manufacturing` toolset — Design-to-fab pipeline: export packages, cost estimation, validation.
//!
//! Orchestrates gerber export, BOM generation, and pick-and-place file creation
//! into a single manufacturing-ready package for a specific fab house.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, ToolContext, ToolDef};
use serde_json::json;
use std::path::PathBuf;
use tracing::{debug, error, info, warn};

use super::{cli, pcb_export};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "export_manufacturing_package",
            "Generate ALL files needed for PCB fabrication and assembly in one call: \
             Gerbers, drill files, BOM (fab-house format), and pick-and-place positions. \
             Targets a specific fab house (JLCPCB, PCBWay, etc.).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file (for BOM generation)" },
                    "output_dir": { "type": "string", "description": "Directory to write all output files" },
                    "fab_house": {
                        "type": "string",
                        "description": "Target manufacturer: 'jlcpcb' (default), 'pcbway', 'oshpark', 'generic'",
                        "default": "jlcpcb"
                    },
                    "include_assembly": {
                        "type": "boolean",
                        "description": "Include BOM + pick-and-place files for SMT assembly",
                        "default": true
                    },
                    "bom_fields": {
                        "type": "string",
                        "description": "Ordered, comma-separated BOM columns, e.g. 'Reference,Value,Footprint,MPN,${QUANTITY}'. Any schematic field name works — this is how MPN/LCSC columns reach the fab. Omit for KiCAD's default Reference,Value,Footprint,QUANTITY,DNP."
                    },
                    "bom_labels": {
                        "type": "string",
                        "description": "Ordered, comma-separated BOM column headings matching 'bom_fields'. Omit to label each column with its field name."
                    },
                    "bom_group_by": {
                        "type": "string",
                        "description": "Comma-separated fields whose matching references collapse into one BOM row, e.g. 'Value,Footprint'."
                    },
                    "gerber_layers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Exact Gerber layers. Omit or pass [] to auto-select enabled copper, F/B.Mask, F/B.SilkS, and Edge.Cuts while excluding documentation layers."
                    },
                    "position_side": {
                        "type": "string",
                        "enum": ["front", "back", "both"],
                        "description": "Board side(s) in the assembly position file.",
                        "default": "both"
                    },
                    "position_units": {
                        "type": "string",
                        "enum": ["mm", "in"],
                        "description": "Coordinate units in the assembly position file.",
                        "default": "mm"
                    }
                },
                "required": ["board", "output_dir"]
            }),
            |args, ctx| async move { handle_export_manufacturing_package(args, ctx).await }
        ),
        tool!(
            "validate_for_manufacturing",
            "Pre-flight check before ordering: verifies the design is ready for the target \
             fab house. Runs KiCad's DRC and checks board outline, design rules, footprints, \
             and routing evidence. Returns NOT READY — never READY — if \
             DRC reports errors or could not be run, so a READY verdict always rests on \
             evidence rather than on an absence of findings.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "fab_house": {
                        "type": "string",
                        "description": "Target manufacturer: 'jlcpcb', 'pcbway', 'oshpark'",
                        "default": "jlcpcb"
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_validate_for_manufacturing(args, ctx).await }
        ),
        tool!(
            "estimate_cost",
            "Estimate the total manufacturing cost for PCB fabrication and assembly at a given fab house. \
             Counts components from board footprints and returns an itemized rough estimate: \
             PCB, components, assembly, and total.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "fab_house": {
                        "type": "string",
                        "description": "'jlcpcb' (default), 'pcbway'",
                        "default": "jlcpcb"
                    },
                    "quantity": {
                        "type": "integer",
                        "description": "Number of boards to manufacture",
                        "default": 5
                    },
                    "layers": {
                        "type": "integer",
                        "description": "Layer count (2, 4, 6). Auto-detected from board if omitted."
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_estimate_cost(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_export_manufacturing_package(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output_dir = get_path(args, "output_dir")?;
    let fab_house = args["fab_house"].as_str().unwrap_or("jlcpcb");
    let include_assembly = args["include_assembly"].as_bool().unwrap_or(true);
    let schematic = args["schematic"].as_str().map(PathBuf::from);
    let requested_gerber_layers = match pcb_export::optional_string_array(args, "gerber_layers") {
        Ok(layers) => layers,
        Err(error) => return Ok(error),
    };
    let gerber_layers = if requested_gerber_layers.is_empty() {
        let board_source = tokio::fs::read_to_string(&board).await?;
        pcb_export::standard_gerber_layers(&board_source)?
    } else {
        requested_gerber_layers
    };
    let position_side = args["position_side"].as_str().unwrap_or("both");
    let position_units = args["position_units"].as_str().unwrap_or("mm");
    if let Err((field, reason)) =
        pcb_export::validate_position_values("csv", position_side, position_units)
    {
        let public_field = match field {
            "side" => "position_side",
            "units" => "position_units",
            other => other,
        };
        return Ok(invalid_manufacturing_argument(public_field, reason));
    }

    info!(
        board = %board.display(),
        output_dir = %output_dir.display(),
        fab_house = %fab_house,
        include_assembly = include_assembly,
        "[BETA] Generating manufacturing package"
    );

    tokio::fs::create_dir_all(&output_dir).await?;

    let cli_path = &ctx.config.kicad_cli;
    let mut files_generated = Vec::new();
    let mut warnings = Vec::new();

    // 1. Export Gerbers
    let gerber_dir = output_dir.join("gerbers");
    tokio::fs::create_dir_all(&gerber_dir).await?;
    let gerber_layer_refs = gerber_layers.iter().map(String::as_str).collect::<Vec<_>>();
    match cli::export_gerber(cli_path, &board, &gerber_dir, &gerber_layer_refs).await {
        Ok(()) => {
            info!("[BETA] Gerber export succeeded");
            files_generated.push(json!({
                "type": "gerber",
                "path": gerber_dir.to_str().unwrap_or(""),
                "layers": gerber_layers.clone()
            }));
        }
        Err(e) => {
            error!(error = %e, "[BETA] Gerber export failed");
            warnings.push(format!("Gerber export failed: {}", e));
        }
    }

    // 2. Export drill files, into the gerber directory so a fab receives the
    //    plated and non-plated Excellon files alongside the layers they belong
    //    to. `--output` is a directory; the old `output_dir.join("drill.drl")`
    //    made KiCad create a directory named drill.drl, so the package
    //    advertised a "drill" file that was really an empty-looking folder and
    //    the real Excellon output never appeared in the file list at all.
    match cli::export_drill(cli_path, &board, &gerber_dir).await {
        Ok(drill_files) => {
            if drill_files.is_empty() {
                warn!("[BETA] Drill export produced no .drl files");
                warnings.push("Drill export produced no .drl files.".to_string());
            } else {
                info!(files = drill_files.len(), "[BETA] Drill export succeeded");
                for file in &drill_files {
                    files_generated.push(json!({
                        "type": "drill",
                        "path": file.to_str().unwrap_or("")
                    }));
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "[BETA] Drill export failed (may be included in gerbers)");
            // Not critical — some gerber exports include drill
        }
    }

    // 3. Assembly files (BOM + pick-and-place)
    if include_assembly {
        // Pick-and-place (position file)
        let pos_format = match fab_house {
            "jlcpcb" => "csv",
            _ => "csv",
        };
        let pos_path = output_dir.join(format!("positions.{}", pos_format));
        match cli::export_position_file(
            cli_path,
            &board,
            &pos_path,
            pos_format,
            position_units,
            position_side,
        )
        .await
        {
            Ok(()) => {
                info!("[BETA] Position file export succeeded");
                files_generated.push(json!({
                    "type": "pick_and_place",
                    "path": pos_path.to_str().unwrap_or(""),
                    "format": pos_format,
                    "units": position_units,
                    "side": position_side
                }));
            }
            Err(e) => {
                error!(error = %e, "[BETA] Position file export failed");
                warnings.push(format!("Position file export failed: {}", e));
            }
        }

        // BOM
        if let Some(ref sch) = schematic {
            let bom_path = output_dir.join("bom.csv");
            // Without bom_fields the package gets kicad-cli's fixed
            // Reference,Value,Footprint,QUANTITY,DNP set — no MPN, no supplier
            // part number, nothing a fab can source a part from.
            let bom_options = cli::BomOptions {
                fields: args["bom_fields"].as_str(),
                labels: args["bom_labels"].as_str(),
                group_by: args["bom_group_by"].as_str(),
                ..Default::default()
            };
            match cli::export_bom(cli_path, sch, &bom_path, &bom_options).await {
                Ok(()) => {
                    info!("[BETA] BOM export succeeded");
                    files_generated.push(json!({
                        "type": "bom",
                        "path": bom_path.to_str().unwrap_or(""),
                        "format": "csv",
                        "fields": bom_options.fields
                    }));
                }
                Err(e) => {
                    error!(error = %e, "[BETA] BOM export failed");
                    warnings.push(format!("BOM export failed: {}", e));
                }
            }
        } else {
            warnings.push("No schematic provided — BOM not generated. Pass 'schematic' for full assembly package.".to_string());
        }
    }

    // List all files in output dir
    let mut all_files = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&output_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            all_files.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    // Also list gerber subdir
    if let Ok(mut rd) = tokio::fs::read_dir(&gerber_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            all_files.push(format!("gerbers/{}", entry.file_name().to_string_lossy()));
        }
    }
    all_files.sort();

    let summary = format!(
        "Generated for {}. {} files total. {}",
        fab_house.to_uppercase(),
        all_files.len(),
        if warnings.is_empty() {
            "No warnings.".to_string()
        } else {
            format!("{} warnings.", warnings.len())
        }
    );

    info!(
        files = all_files.len(),
        warnings = warnings.len(),
        "[BETA] Manufacturing package complete"
    );

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "fab_house": fab_house,
            "output_dir": output_dir.to_str().unwrap_or(""),
            "files": all_files,
            "files_generated": files_generated,
            "gerber_layers": gerber_layers,
            "position_units": if include_assembly { Some(position_units) } else { None },
            "position_side": if include_assembly { Some(position_side) } else { None },
            "warnings": warnings,
            "summary": summary,
            "next_steps": format!(
                "Upload the contents of {} to {}'s order page. Gerbers go in the PCB order, BOM + positions go in the assembly order.",
                output_dir.display(),
                fab_house.to_uppercase()
            )
        }))
        .unwrap(),
    ))
}

fn invalid_manufacturing_argument(field: &str, reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.clone(),
        },
        format!("Argument '{field}' is invalid: {reason}"),
    )
}

async fn handle_validate_for_manufacturing(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let fab_house = args["fab_house"].as_str().unwrap_or("jlcpcb");

    info!(
        board = %board.display(),
        fab_house = %fab_house,
        "[BETA] Running manufacturing validation"
    );

    let content = tokio::fs::read_to_string(&board).await?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    let mut issues = Vec::new();

    // Check board outline
    let has_outline = content.contains("Edge.Cuts");
    if !has_outline {
        issues.push(json!({
            "severity": "error",
            "issue": "No board outline found on Edge.Cuts layer",
            "fix": "Add a board outline using add_board_outline before ordering"
        }));
    }

    // Check that footprints exist
    let fp_count = tree.find_all("footprint").len();
    if fp_count == 0 {
        issues.push(json!({
            "severity": "error",
            "issue": "No footprints found on the board",
            "fix": "Open the PCB in KiCAD and run Tools > Update PCB from Schematic (kicad-cli has no 'pcb sync' command)"
        }));
    }

    // Check layer count
    let _layers = tree
        .find("layers")
        .map(|l| l.find_all("*"))
        .unwrap_or_default();
    let copper_layers = content.matches("signal)").count() + content.matches("signal \"").count();
    debug!(
        copper_layers = copper_layers,
        "[BETA] Detected copper layers"
    );

    // Fab-specific checks
    let (min_trace, _min_drill, _max_layers) = match fab_house {
        "jlcpcb" => (0.127, 0.3, 32),
        "oshpark" => (0.152, 0.254, 4),
        "pcbway" => (0.1, 0.2, 32),
        _ => (0.15, 0.3, 32),
    };

    // Check design rules
    if let Some(min_tw) = find_setup_value(&content, "min_trace_width") {
        if min_tw < min_trace {
            issues.push(json!({
                "severity": "error",
                "issue": format!("Trace width {:.3}mm is below {}'s minimum ({:.3}mm)", min_tw, fab_house, min_trace),
                "fix": format!("Increase minimum trace width to {:.3}mm in design rules", min_trace)
            }));
        }
    }

    // Check for unrouted nets (ratsnest)
    let (net_count, track_count) = count_nets_and_tracks(&tree);
    if net_count > 3 && track_count == 0 {
        issues.push(json!({
            "severity": "error",
            "issue": format!("{} nets defined but no traces routed", net_count),
            "fix": "Route traces using route_trace before manufacturing"
        }));
    }

    // That heuristic only fires on a board with *zero* tracks, so a board
    // routed except for one net sailed past it — and nothing here had ever
    // consulted DRC, which is the only thing that actually knows. This tool
    // returned READY on a board with 25 DRC errors and an unrouted item
    // (#247). A readiness verdict now requires the evidence.
    let drc = cli::run_drc(&ctx.config.kicad_cli, &board, false).await;
    let drc_summary = match &drc {
        Ok(report) => {
            for violation in report.all().filter(|v| v.severity == "error") {
                issues.push(json!({
                    "severity": "error",
                    "issue": format!("DRC [{}]: {}", violation.rule, violation.description),
                    "fix": "Fix in the PCB editor, or waive the rule deliberately; \
                            run_drc lists every violation with its location"
                }));
            }
            for missing in report.missing_categories() {
                issues.push(json!({
                    "severity": "error",
                    "issue": format!("kicad-cli did not report DRC '{missing}'"),
                    "fix": "Readiness cannot be established without it; check the \
                            kicad-cli version"
                }));
            }
            json!({
                "errors": report.error_count(),
                "design_rule_violations": report.violations.len(),
                "unconnected_items": report.unconnected_items.as_ref().map(Vec::len),
                "schematic_parity": report.schematic_parity.as_ref().map(Vec::len),
            })
        }
        Err(error) => {
            issues.push(json!({
                "severity": "error",
                "issue": format!("DRC could not run: {error:#}"),
                "fix": "Without DRC this tool cannot tell a clean board from a \
                        broken one; fix the kicad-cli path and re-run"
            }));
            serde_json::Value::Null
        }
    };

    let verdict = if issues.iter().any(|i| i["severity"] == "error") {
        "NOT READY"
    } else if !issues.is_empty() {
        "NEEDS REVIEW"
    } else {
        "READY"
    };

    info!(
        verdict = verdict,
        issues = issues.len(),
        "[BETA] Manufacturing validation complete"
    );

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "verdict": verdict,
            "fab_house": fab_house,
            "board_info": {
                "footprint_count": fp_count,
                "copper_layers": copper_layers,
                "net_count": net_count,
                "track_count": track_count
            },
            // Null means DRC did not run, and an issue above says so. Never a
            // zeroed-out object: this tool must not be able to imply a clean
            // board it never checked.
            "drc": drc_summary,
            "issues": issues,
            "summary": format!(
                "{}: {} issues found. {} footprints, {} copper layers.",
                verdict, issues.len(), fp_count, copper_layers
            )
        }))
        .unwrap(),
    ))
}

async fn handle_estimate_cost(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let fab_house = args["fab_house"].as_str().unwrap_or("jlcpcb");
    let quantity = args["quantity"].as_u64().unwrap_or(5) as usize;

    info!(
        board = %board.display(),
        fab_house = %fab_house,
        quantity = quantity,
        "[BETA] Estimating manufacturing cost"
    );

    let content = tokio::fs::read_to_string(&board).await?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    // Count components
    let fps = tree.find_all("footprint");
    let component_count = fps.len();

    // Detect layers
    let copper_layers = args["layers"].as_u64().unwrap_or_else(|| {
        let count = content.matches("signal)").count() + content.matches("signal \"").count();
        (count as u64).max(2)
    }) as usize;

    // Estimate board dimensions from Edge.Cuts
    let (width_mm, height_mm) = estimate_board_dimensions(&content);

    // Rough cost estimation based on fab house pricing models
    let (pcb_cost, assembly_cost, component_est) = match fab_house {
        "jlcpcb" => {
            let pcb = match copper_layers {
                2 => 2.0 + (quantity as f64 - 5.0).max(0.0) * 0.40,
                4 => 7.0 + (quantity as f64 - 5.0).max(0.0) * 1.40,
                6 => 15.0 + (quantity as f64 - 5.0).max(0.0) * 3.00,
                _ => 30.0 + (quantity as f64 - 5.0).max(0.0) * 5.00,
            };
            let smt_setup = if component_count > 0 { 8.0 } else { 0.0 };
            let smt_per_board = component_count as f64 * 0.003 * quantity as f64;
            let comp_est = component_count as f64 * 0.05; // rough avg per component
            (pcb, smt_setup + smt_per_board, comp_est * quantity as f64)
        }
        "pcbway" => {
            let pcb = match copper_layers {
                2 => 5.0 + (quantity as f64 - 5.0).max(0.0) * 0.50,
                4 => 12.0 + (quantity as f64 - 5.0).max(0.0) * 2.00,
                _ => 25.0 + (quantity as f64 - 5.0).max(0.0) * 4.00,
            };
            let smt = component_count as f64 * 0.005 * quantity as f64;
            let comp_est = component_count as f64 * 0.08 * quantity as f64;
            (pcb, smt, comp_est)
        }
        _ => {
            let pcb = 10.0 + quantity as f64 * 2.0;
            (pcb, 0.0, 0.0)
        }
    };

    let total = pcb_cost + assembly_cost + component_est;

    debug!(
        pcb_cost = pcb_cost,
        assembly_cost = assembly_cost,
        component_est = component_est,
        total = total,
        "[BETA] Cost estimate calculated"
    );

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "fab_house": fab_house,
            "quantity": quantity,
            "board": {
                "width_mm": width_mm,
                "height_mm": height_mm,
                "copper_layers": copper_layers,
                "component_count": component_count
            },
            "cost_estimate": {
                "pcb_fabrication": format!("${:.2}", pcb_cost),
                "smt_assembly": format!("${:.2}", assembly_cost),
                "components_estimate": format!("${:.2}", component_est),
                "total_estimate": format!("${:.2}", total),
                "per_board": format!("${:.2}", total / quantity as f64)
            },
            "notes": [
                "Estimates are approximate — actual cost depends on board size, finish, and specific components",
                "Component costs are rough averages — use generate_bom with supply chain data for accurate pricing",
                format!("Based on {} quantity from {}", quantity, fab_house.to_uppercase())
            ],
            "disclaimer": "BETA: Cost estimates are indicative only. Always confirm with the fab house's online quoting tool."
        }))
        .unwrap(),
    ))
}

#[cfg(test)]
mod package_export_option_tests {
    use super::*;

    #[test]
    fn package_schema_exposes_applied_gerber_and_position_options() {
        let package = tools()
            .into_iter()
            .find(|tool| tool.name == "export_manufacturing_package")
            .unwrap();
        let properties = &package.input_schema["properties"];
        assert_eq!(properties["gerber_layers"]["items"]["type"], "string");
        assert_eq!(properties["position_units"]["enum"], json!(["mm", "in"]));
        assert_eq!(
            properties["position_side"]["enum"],
            json!(["front", "back", "both"])
        );
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Distinct nets and routed items on the board, read from the parsed tree.
///
/// Both net shapes have to be handled. KiCad ≤ 9 declares each net once at top
/// level — `(net 1 "GND")` — and items refer to it by number, `(net 1)`. KiCad
/// 10 dropped the top-level table entirely and writes the name on every item,
/// `(net "GND")`. Counting names when any are present covers both without
/// double-counting a KiCad 9 net through its declaration *and* its references.
///
/// Tracks are always direct children of `(kicad_pcb …)`, so they are counted
/// there rather than by walking: `(arc …)` also appears inside `(pts …)` of a
/// zone outline, which is not routed copper.
fn count_nets_and_tracks(tree: &konnect_sexp::SexpNode) -> (usize, usize) {
    use std::collections::HashSet;

    fn walk(node: &konnect_sexp::SexpNode, names: &mut HashSet<String>, ids: &mut HashSet<String>) {
        let Some(children) = node.children() else {
            return;
        };
        if node.head() == Some("net") {
            match children.last() {
                // (net 1 "GND") and (net "GND") both end in the quoted name.
                Some(konnect_sexp::SexpNode::Str(name)) if !name.is_empty() => {
                    names.insert(name.clone());
                }
                // (net 1) — a bare reference in a file whose table we have not
                // seen. Net 0 is the unconnected pseudo-net.
                Some(konnect_sexp::SexpNode::Atom(id)) if id != "0" => {
                    ids.insert(id.clone());
                }
                _ => {}
            }
        }
        for child in children {
            walk(child, names, ids);
        }
    }

    let mut names = HashSet::new();
    let mut ids = HashSet::new();
    walk(tree, &mut names, &mut ids);
    let net_count = if names.is_empty() {
        ids.len()
    } else {
        names.len()
    };

    let track_count =
        tree.find_all("segment").len() + tree.find_all("via").len() + tree.find_all("arc").len();

    (net_count, track_count)
}

fn find_setup_value(content: &str, key: &str) -> Option<f64> {
    let pat = format!("({} ", key);
    let pos = content.find(&pat)?;
    let after = &content[pos + pat.len()..];
    let end = after.find(')')?;
    after[..end].trim().parse().ok()
}

fn estimate_board_dimensions(content: &str) -> (f64, f64) {
    let mut min_x = f64::MAX;
    let mut max_x = f64::MIN;
    let mut min_y = f64::MAX;
    let mut max_y = f64::MIN;
    let mut found = false;

    // Scan gr_line on Edge.Cuts for board outline coordinates
    let mut pos = 0;
    while let Some(line_pos) = content[pos..].find("(gr_line") {
        let abs = pos + line_pos;
        let block_end = content[abs..].find(")\n").unwrap_or(300) + abs;
        let block = &content[abs..block_end.min(content.len())];

        if block.contains("Edge.Cuts") {
            // Extract start and end coordinates
            if let (Some(sx), Some(sy)) = (
                extract_coord(block, "start", 0),
                extract_coord(block, "start", 1),
            ) {
                if sx < min_x {
                    min_x = sx;
                }
                if sx > max_x {
                    max_x = sx;
                }
                if sy < min_y {
                    min_y = sy;
                }
                if sy > max_y {
                    max_y = sy;
                }
                found = true;
            }
            if let (Some(ex), Some(ey)) = (
                extract_coord(block, "end", 0),
                extract_coord(block, "end", 1),
            ) {
                if ex < min_x {
                    min_x = ex;
                }
                if ex > max_x {
                    max_x = ex;
                }
                if ey < min_y {
                    min_y = ey;
                }
                if ey > max_y {
                    max_y = ey;
                }
            }
        }
        pos = abs + 1;
    }

    if found {
        ((max_x - min_x).abs(), (max_y - min_y).abs())
    } else {
        (0.0, 0.0) // Unknown
    }
}

fn extract_coord(block: &str, keyword: &str, index: usize) -> Option<f64> {
    let pat = format!("({} ", keyword);
    let pos = block.find(&pat)? + pat.len();
    let rest = &block[pos..];
    let parts: Vec<&str> = rest.split([' ', ')']).collect();
    parts.get(index)?.parse().ok()
}

#[cfg(test)]
mod net_track_count_tests {
    use super::*;
    use konnect_sexp::parser::parse_sexp;

    /// KiCad 10 (file format 20260206) writes tab indentation, puts each
    /// `(segment …)` / `(via …)` on its own multi-line form, and has **no**
    /// top-level net table — every item names its net instead. The old
    /// substring probes (`"\n  (net "`, `"(segment "`, `"(via "`) match none of
    /// that, so a fully routed board reported net_count 0 / track_count 0 and
    /// still came back READY.
    const KICAD_10_BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(segment\n\t\t(start 110 110)\n\t\t(end 120 110)\n\t\t(width 0.2)\n\t\t(layer \"F.Cu\")\n\t\t(net \"GND\")\n\t)\n\
        \t(segment\n\t\t(start 120 110)\n\t\t(end 130 120)\n\t\t(width 0.2)\n\t\t(layer \"F.Cu\")\n\t\t(net \"GND\")\n\t)\n\
        \t(via\n\t\t(at 130 120)\n\t\t(size 0.6)\n\t\t(drill 0.3)\n\t\t(net \"GND\")\n\t)\n\
        \t(segment\n\t\t(start 110 130)\n\t\t(end 120 130)\n\t\t(width 0.2)\n\t\t(layer \"B.Cu\")\n\t\t(net \"VCC\")\n\t)\n\
        )\n";

    /// KiCad ≤ 9: a top-level net table plus numeric references on the items.
    /// The same net must not be counted once for its declaration and again for
    /// every segment that mentions it.
    const KICAD_9_BOARD: &str = "(kicad_pcb\n\
        \t(version 20241229)\n\
        \t(net 0 \"\")\n\
        \t(net 1 \"GND\")\n\
        \t(net 2 \"VCC\")\n\
        \t(segment (start 110 110) (end 120 110) (width 0.2) (layer \"F.Cu\") (net 1))\n\
        \t(segment (start 120 110) (end 130 120) (width 0.2) (layer \"F.Cu\") (net 1))\n\
        \t(via (at 130 120) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net 1))\n\
        \t(segment (start 110 130) (end 120 130) (width 0.2) (layer \"B.Cu\") (net 2))\n\
        )\n";

    #[test]
    fn counts_a_kicad_10_board_with_no_top_level_net_table() {
        let tree = parse_sexp(KICAD_10_BOARD).unwrap();
        assert_eq!(count_nets_and_tracks(&tree), (2, 4));
    }

    #[test]
    fn counts_a_kicad_9_board_without_double_counting_declarations() {
        let tree = parse_sexp(KICAD_9_BOARD).unwrap();
        assert_eq!(count_nets_and_tracks(&tree), (2, 4));
    }

    #[test]
    fn the_unconnected_pseudo_net_does_not_count() {
        let tree = parse_sexp("(kicad_pcb\n\t(net 0 \"\")\n)\n").unwrap();
        assert_eq!(count_nets_and_tracks(&tree), (0, 0));
    }

    /// A zone outline may carry `(arc …)` inside its `(pts …)`; that is a
    /// polygon corner, not routed copper.
    #[test]
    fn zone_outline_arcs_are_not_routed_copper() {
        let board = "(kicad_pcb\n\
            \t(zone\n\t\t(net \"GND\")\n\t\t(polygon\n\t\t\t(pts\n\t\t\t\t(xy 0 0)\n\t\t\t\t(arc (start 1 0) (mid 2 1) (end 1 2))\n\t\t\t)\n\t\t)\n\t)\n\
            )\n";
        let tree = parse_sexp(board).unwrap();
        assert_eq!(count_nets_and_tracks(&tree), (1, 0));
    }

    // ─── End to end through the tool ─────────────────────────────────────────

    fn test_ctx() -> ToolContext {
        ToolContext::new(
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
        )
    }

    async fn validate(board_text: &str) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, board_text).unwrap();
        let result = handle_validate_for_manufacturing(
            &json!({ "board": board.to_str().unwrap() }),
            &test_ctx(),
        )
        .await
        .unwrap();
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => {
                serde_json::from_str(text).unwrap()
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_routed_kicad_10_board_reports_its_nets_and_tracks() {
        let report = validate(KICAD_10_BOARD).await;
        assert_eq!(report["board_info"]["net_count"], json!(2));
        assert_eq!(report["board_info"]["track_count"], json!(4));
    }

    /// The symptom that surfaced this: an unrouted board came back READY on the
    /// routing check because both counts read zero, so the `net_count > 3 &&
    /// track_count == 0` guard could never fire.
    #[tokio::test]
    async fn an_unrouted_kicad_10_board_is_flagged_not_ready() {
        let board = "(kicad_pcb\n\
            \t(version 20260206)\n\
            \t(gr_line (start 0 0) (end 10 0) (layer \"Edge.Cuts\"))\n\
            \t(footprint \"R:R_0402\"\n\
            \t\t(pad \"1\" smd rect (at 0 0) (size 1 1) (net \"GND\"))\n\
            \t\t(pad \"2\" smd rect (at 1 0) (size 1 1) (net \"VCC\"))\n\
            \t)\n\
            \t(footprint \"R:R_0402\"\n\
            \t\t(pad \"1\" smd rect (at 5 0) (size 1 1) (net \"SDA\"))\n\
            \t\t(pad \"2\" smd rect (at 6 0) (size 1 1) (net \"SCL\"))\n\
            \t)\n\
            )\n";
        let report = validate(board).await;
        assert_eq!(report["board_info"]["net_count"], json!(4));
        assert_eq!(report["board_info"]["track_count"], json!(0));
        assert_eq!(report["verdict"], json!("NOT READY"));
        let issues = report["issues"].as_array().unwrap();
        assert!(
            issues.iter().any(|i| i["issue"]
                .as_str()
                .unwrap_or("")
                .contains("no traces routed")),
            "{issues:?}"
        );
    }
}

#[cfg(test)]
mod readiness_evidence_tests {
    use super::*;
    use serde_json::json;

    /// No kicad-cli, so DRC cannot run — the point of these tests.
    fn ctx_without_kicad_cli() -> ToolContext {
        ToolContext::new(
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
        )
    }

    async fn validate(board_text: &str) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, board_text).unwrap();
        let result = handle_validate_for_manufacturing(
            &json!({ "board": board.to_str().unwrap() }),
            &ctx_without_kicad_cli(),
        )
        .await
        .unwrap();
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => {
                serde_json::from_str(text).unwrap()
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// A board that passes every check this tool performs itself.
    const CLEAN_LOOKING_BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(31 \"B.Cu\" signal)\n\t)\n\
        \t(gr_line (start 0 0) (end 50 0) (layer \"Edge.Cuts\") (width 0.1))\n\
        \t(footprint \"R:R_0402\"\n\
        \t\t(pad \"1\" smd rect (at 5 0) (size 1 1) (net \"SDA\"))\n\
        \t)\n\
        \t(segment (start 0 0) (end 5 0) (width 0.25) (layer \"F.Cu\") (net \"SDA\"))\n\
        )\n";

    /// #247. This tool returned `READY` with zero issues on a board carrying
    /// 25 DRC errors and an unrouted item, because it never asked DRC —
    /// its only routing check fires when a board has *no* tracks at all.
    ///
    /// The test context has no kicad-cli, so DRC cannot run. Missing evidence
    /// must block the verdict: "I found nothing wrong" is not the same claim
    /// as "nothing is wrong", and only one of them justifies ordering boards.
    #[tokio::test]
    async fn readiness_needs_drc_evidence_not_just_an_absence_of_findings() {
        let report = validate(CLEAN_LOOKING_BOARD).await;

        assert_ne!(
            report["verdict"], "READY",
            "a board whose DRC was never run cannot be declared ready: {report}"
        );
        assert!(report["drc"].is_null(), "no DRC ran, so no DRC summary");
        let issues = report["issues"].as_array().unwrap();
        assert!(
            issues.iter().any(|i| i["issue"]
                .as_str()
                .unwrap_or("")
                .contains("DRC could not run")),
            "the missing evidence must be named: {issues:?}"
        );
    }
}
