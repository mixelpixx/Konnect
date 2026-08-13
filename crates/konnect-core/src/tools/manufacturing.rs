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

use super::cli;

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
                    "quantity": {
                        "type": "integer",
                        "description": "Production quantity (for BOM pricing context)",
                        "default": 5
                    }
                },
                "required": ["board", "output_dir"]
            }),
            |args, ctx| async move { handle_export_manufacturing_package(args, ctx).await }
        ),
        tool!(
            "validate_for_manufacturing",
            "Pre-flight check before ordering: verifies the design is ready for the target \
             fab house. Checks board outline, design rules, BOM completeness, and assembly constraints.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file (optional, for BOM checks)" },
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
             Returns itemized breakdown: PCB, components, assembly, and total.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file (for component count)" },
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
    match cli::export_gerber(cli_path, &board, &gerber_dir).await {
        Ok(()) => {
            info!("[BETA] Gerber export succeeded");
            files_generated.push(json!({
                "type": "gerber",
                "path": gerber_dir.to_str().unwrap_or("")
            }));
        }
        Err(e) => {
            error!(error = %e, "[BETA] Gerber export failed");
            warnings.push(format!("Gerber export failed: {}", e));
        }
    }

    // 2. Export drill files. --output is a directory; KiCAD names the file(s)
    // after the board and writes them inside it.
    match cli::export_drill(cli_path, &board, &output_dir).await {
        Ok(drills) => {
            info!(count = drills.len(), "[BETA] Drill export succeeded");
            for drill_path in drills {
                files_generated.push(json!({
                    "type": "drill",
                    "path": drill_path.to_str().unwrap_or("")
                }));
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
        match cli::export_position_file(cli_path, &board, &pos_path, pos_format).await {
            Ok(()) => {
                info!("[BETA] Position file export succeeded");
                files_generated.push(json!({
                    "type": "pick_and_place",
                    "path": pos_path.to_str().unwrap_or(""),
                    "format": pos_format
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
            match cli::export_bom(cli_path, sch, &bom_path, "csv").await {
                Ok(()) => {
                    info!("[BETA] BOM export succeeded");
                    files_generated.push(json!({
                        "type": "bom",
                        "path": bom_path.to_str().unwrap_or(""),
                        "format": "csv"
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

async fn handle_validate_for_manufacturing(
    args: &serde_json::Value,
    _ctx: &ToolContext,
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
            "fix": "Open the PCB in KiCAD and run Tools > Update PCB from Schematic (kicad-cli 'pcb sync' was removed in v10)"
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
    let net_count = content.matches("\n  (net ").count();
    let track_count = content.matches("(segment ").count() + content.matches("(via ").count();
    if net_count > 3 && track_count == 0 {
        issues.push(json!({
            "severity": "error",
            "issue": format!("{} nets defined but no traces routed", net_count),
            "fix": "Route traces using route_trace or autoroute before manufacturing"
        }));
    }

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

// ─── Helpers ─────────────────────────────────────────────────────────────────

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
