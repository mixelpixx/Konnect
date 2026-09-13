//! `sch_export` toolset — export, netlist, ERC, connectivity fix, board sync.
//!
//! All export operations delegate to `kicad-cli` via the `cli` module.
//! `export_netlist_summary` and `fix_connectivity` operate directly on
//! S-expression file content so they work without a running KiCAD instance.

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{get_path, placed_pins, placed_pins_by_reference, ToolContext, ToolDef};
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident},
    parser::{parse_sexp, SexpNode},
    schematic::{
        extract_all_net_labels, extract_labels, extract_symbol_instances, extract_wires,
        pin_endpoint, read_schematic,
    },
    writer::{apply_edits, find_direct_child_blocks, write_atomic_if_unchanged, SexpEdit},
    SexpError,
};
use serde_json::json;
use std::path::{Path, PathBuf};

use super::cli;
use super::sch_connectivity::net_graph_for;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "export_schematic_svg",
            "Export a schematic sheet to an SVG file using kicad-cli. The result doubles as a              machine-readable geometry source: kicad-cli writes every string twice — visibly as              stroke paths, and again as an invisible <text opacity=\"0\"> element carrying x, y,              textLength, font-size and text-anchor — so text content, position and width are              checkable without rendering a pixel.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Output SVG file path (directory used as output dir)" },
                    "black_and_white": { "type": "boolean", "description": "Render in black and white", "default": false },
                    "theme": { "type": "string", "description": "KiCad colour theme name (optional)" }
                },
                "required": ["schematic", "output"]
            }),
            |args, ctx| async move { handle_export_svg(args, ctx).await }
        ),
        tool!(
            "render_schematic_png",
            "Render a schematic sheet to PNG: kicad-cli SVG export rasterized in-process \
             (deterministic stroke-font rendering, no system fonts consulted). Returns the \
             PNG path and its actual pixel dimensions; with 'inline' true the response also \
             carries the image as base64 so the caller can inspect its own output. Width is \
             capped at 4096 px.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Output PNG path (default: alongside the schematic)" },
                    "width_px":  { "type": "integer", "description": "Output width in pixels", "default": 1600, "maximum": 4096 },
                    "inline":    { "type": "boolean", "description": "Also return the PNG as base64 content", "default": false },
                    "monochrome": { "type": "boolean", "description": "Render in black and white", "default": false }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_render_png(args, ctx).await }
        ),
        tool!(
            "set_visual_baseline",
            "Capture the current render of a schematic sheet as its visual baseline, \
             stored under the project's .konnect/baselines/ keyed by sheet name, with \
             the source file's hash and the renderer identity recorded so a later \
             compare can tell design drift from a stale renderer.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "width_px":  { "type": "integer", "description": "Baseline render width in pixels", "default": 1600, "maximum": 4096 }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_baseline_set(args, ctx).await }
        ),
        tool!(
            "compare_visual_baseline",
            "Re-render a sheet at its stored baseline's width and report pixel drift: \
             changed-pixel percentage against a 2% threshold, the changed region's pixel \
             bounding box, and whether the source file changed since the baseline. \
             'No baseline stored' is an explicit result, not an error; a baseline made \
             by a different renderer version is flagged, never silently trusted.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "inline_diff": { "type": "boolean", "description": "Also return the current render as base64 for inspection", "default": false }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_baseline_compare(args, ctx).await }
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
                    "all_sheets": { "type": "boolean", "description": "Include all hierarchical sheets; false exports page 1 only", "default": true }
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
             components, their nets, pin counts. Nets come from labels and from \
             power symbols. Does not require kicad-cli.",
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
                    "output":    { "type": "string", "description": "Optional path to write this tool's filtered violation list as JSON (not KiCad's own ERC report)" },
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
        tool!(
            "update_pcb_from_schematic",
            "Plan or atomically apply saved schematic hierarchy changes to the live KiCad PCB. \
             Defaults to a non-mutating dry run; apply requires its exact plan revision. \
             Preserves placement, routing, board-only footprints, and footprint artwork. \
             A symbol with no footprint assigned is reported under `unassigned_footprints` \
             and the sync proceeds for every other component.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Saved root .kicad_sch path" },
                    "board": { "type": "string", "description": "Matching .kicad_pcb path currently open in KiCad" },
                    "dry_run": { "type": "boolean", "description": "Plan without changing the board", "default": true },
                    "expected_plan_revision": { "type": "string", "description": "Required for apply; exact revision returned by the latest dry run" }
                },
                "required": ["schematic", "board"]
            }),
            |args, ctx| async move {
                super::pcb_sync::handle_update_pcb_from_schematic(args, ctx).await
            }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_export_svg(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let output_path = get_path(args, "output")?;
    let options = cli::SchematicSvgOptions {
        black_and_white: args["black_and_white"].as_bool().unwrap_or(false),
        theme: args["theme"].as_str(),
    };

    // kicad-cli writes to an output directory and names the file <stem>.svg
    let output_dir = output_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    std::fs::create_dir_all(&output_dir)?;

    let svg_path =
        cli::export_schematic_svg(&ctx.config.kicad_cli, &sch_path, &output_dir, &options).await?;

    Ok(CallToolResult::json(&json!({
        "exported": svg_path.display().to_string(),
        "black_and_white": options.black_and_white,
        "theme": options.theme
    })))
}

async fn handle_render_png(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let width_px = match args.get("width_px") {
        None => 1600,
        Some(v) => match v.as_u64() {
            Some(w) if (1..=4096).contains(&w) => w as u32,
            _ => {
                return Ok(CallToolResult::error_kind(
                    crate::mcp::error::ToolErrorKind::InvalidArgument {
                        field: "width_px".into(),
                        reason: "must be an integer in 1..=4096".into(),
                    },
                    "Argument 'width_px' must be an integer between 1 and 4096",
                ))
            }
        },
    };
    let inline = args["inline"].as_bool().unwrap_or(false);
    let monochrome = args["monochrome"].as_bool().unwrap_or(false);

    let output_path = match args.get("output").and_then(|v| v.as_str()) {
        Some(p) => std::path::PathBuf::from(p),
        None => sch_path.with_extension("png"),
    };

    // Render the SVG into a temp dir so the intermediate never collides with
    // caller files, then rasterize in-process.
    let svg_dir = tempfile::tempdir()?;
    let options = cli::SchematicSvgOptions {
        black_and_white: monochrome,
        theme: None,
    };
    let svg_path =
        cli::export_schematic_svg(&ctx.config.kicad_cli, &sch_path, svg_dir.path(), &options)
            .await?;
    let svg_bytes = std::fs::read(&svg_path)?;

    let rendered = match konnect_render::svg_to_png(&svg_bytes, width_px) {
        Ok(rendered) => rendered,
        Err(error) => {
            return Ok(CallToolResult::error(format!(
                "kicad-cli exported the sheet but rasterization refused it: {error}"
            )))
        }
    };

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&output_path, &rendered.png)?;

    // Dimensions come from the produced image, never echoed from the request.
    let mut response = json!({
        "rendered": output_path.display().to_string(),
        "width_px": rendered.width_px,
        "height_px": rendered.height_px,
        "png_bytes": rendered.png.len(),
        "monochrome": monochrome
    });
    if inline {
        use base64::Engine as _;
        response["png_base64"] =
            json!(base64::engine::general_purpose::STANDARD.encode(&rendered.png));
    }
    Ok(CallToolResult::json(&response))
}

/// Baseline storage for a sheet: `<project>/.konnect/baselines/<stem>.png`
/// plus a sibling json carrying the facts a compare needs. The project
/// directory is the schematic's parent — baselines belong to the design they
/// describe.
fn baseline_paths(
    sch_path: &std::path::Path,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let parent = sch_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("schematic path has no parent directory"))?;
    let stem = sch_path
        .file_stem()
        .ok_or_else(|| anyhow::anyhow!("schematic path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let dir = parent.join(".konnect").join("baselines");
    Ok((
        dir.join(format!("{stem}.png")),
        dir.join(format!("{stem}.json")),
    ))
}

async fn render_sheet_png(
    ctx: &ToolContext,
    sch_path: &std::path::Path,
    width_px: u32,
) -> anyhow::Result<konnect_render::Rendered> {
    let svg_dir = tempfile::tempdir()?;
    let svg_path = cli::export_schematic_svg(
        &ctx.config.kicad_cli,
        sch_path,
        svg_dir.path(),
        &cli::SchematicSvgOptions::default(),
    )
    .await?;
    let svg_bytes = std::fs::read(&svg_path)?;
    konnect_render::svg_to_png(&svg_bytes, width_px)
}

async fn handle_baseline_set(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let width_px = match args.get("width_px") {
        None => 1600u32,
        Some(v) => match v.as_u64() {
            Some(w) if (1..=4096).contains(&w) => w as u32,
            _ => {
                return Ok(CallToolResult::error_kind(
                    crate::mcp::error::ToolErrorKind::InvalidArgument {
                        field: "width_px".into(),
                        reason: "must be an integer in 1..=4096".into(),
                    },
                    "Argument 'width_px' must be an integer between 1 and 4096",
                ))
            }
        },
    };

    let source_bytes = std::fs::read(&sch_path)?;
    let rendered = render_sheet_png(ctx, &sch_path, width_px).await?;

    let (png_path, json_path) = baseline_paths(&sch_path)?;
    std::fs::create_dir_all(png_path.parent().expect("baseline dir has parent"))?;
    std::fs::write(&png_path, &rendered.png)?;

    use sha2::Digest as _;
    let source_sha256 = format!("{:x}", sha2::Sha256::digest(&source_bytes));
    let meta = json!({
        "sheet": sch_path.display().to_string(),
        "source_sha256": source_sha256,
        "width_px": rendered.width_px,
        "height_px": rendered.height_px,
        "renderer": konnect_render::RENDERER_ID,
    });
    std::fs::write(
        &json_path,
        format!("{}\n", serde_json::to_string_pretty(&meta)?),
    )?;

    // Report what was stored by re-reading it — the write is not its own witness.
    let stored = std::fs::metadata(&png_path)?.len();
    Ok(CallToolResult::json(&json!({
        "baseline_png": png_path.display().to_string(),
        "baseline_meta": json_path.display().to_string(),
        "png_bytes": stored,
        "width_px": rendered.width_px,
        "height_px": rendered.height_px,
        "source_sha256": source_sha256,
        "renderer": konnect_render::RENDERER_ID,
    })))
}

/// Drift above this percentage of changed pixels is reported as DRIFT.
const DRIFT_THRESHOLD_PCT: f64 = 2.0;

async fn handle_baseline_compare(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let inline_diff = args["inline_diff"].as_bool().unwrap_or(false);

    let (png_path, json_path) = baseline_paths(&sch_path)?;
    if !png_path.exists() || !json_path.exists() {
        // An explicit result, not an error: "you have no baseline yet" is a
        // normal state the caller acts on by setting one.
        return Ok(CallToolResult::json(&json!({
            "status": "no_baseline",
            "detail": "No stored baseline for this sheet; call set_visual_baseline first.",
        })));
    }

    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json_path)?)?;
    let baseline_width = meta["width_px"]
        .as_u64()
        .and_then(|w| u32::try_from(w).ok());
    let Some(baseline_width) = baseline_width else {
        return Ok(CallToolResult::error(format!(
            "baseline metadata at {} is missing width_px — re-set the baseline",
            json_path.display()
        )));
    };

    let mut warnings = Vec::new();
    if meta["renderer"] != json!(konnect_render::RENDERER_ID) {
        warnings.push(format!(
            "baseline_stale_renderer: baseline was rendered by {} but this binary renders \
             with {} — pixel drift may be renderer drift; re-set the baseline to clear",
            meta["renderer"],
            konnect_render::RENDERER_ID
        ));
    }

    let source_bytes = std::fs::read(&sch_path)?;
    use sha2::Digest as _;
    let current_sha = format!("{:x}", sha2::Sha256::digest(&source_bytes));
    let source_changed = meta["source_sha256"] != json!(current_sha);

    // Compare at the BASELINE's width so pixel counts stay commensurable.
    let rendered = render_sheet_png(ctx, &sch_path, baseline_width).await?;
    let baseline_png = std::fs::read(&png_path)?;
    let diff = match konnect_render::diff_pngs(&baseline_png, &rendered.png) {
        Ok(diff) => diff,
        Err(error) => return Ok(CallToolResult::error(format!("diff failed: {error}"))),
    };

    // The threshold judges change against the DRAWING, not the mostly-blank
    // page: a page-relative percentage under-reports by an order of
    // magnitude on a normal sheet.
    let status = if diff.changed_pct_of_content > DRIFT_THRESHOLD_PCT {
        "DRIFT"
    } else {
        "PASS"
    };
    let mut response = json!({
        "status": status,
        "changed_pct_of_content": diff.changed_pct_of_content,
        "content_pixels": diff.content_pixels,
        "changed_pct_of_page": diff.changed_pct,
        "changed_pixels": diff.changed_pixels,
        "total_pixels": diff.total_pixels,
        "threshold_pct": DRIFT_THRESHOLD_PCT,
        "changed_bbox_px": diff.changed_bbox.map(|(x0, y0, x1, y1)| json!({
            "x_min": x0, "y_min": y0, "x_max": x1, "y_max": y1
        })),
        "source_changed_since_baseline": source_changed,
        "baseline_renderer": meta["renderer"],
        "warnings": warnings,
    });
    if inline_diff {
        use base64::Engine as _;
        response["current_png_base64"] =
            json!(base64::engine::general_purpose::STANDARD.encode(&rendered.png));
    }
    Ok(CallToolResult::json(&response))
}

async fn handle_export_pdf(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let output_path = get_path(args, "output")?;
    let options = cli::SchematicPdfOptions {
        black_and_white: args["black_and_white"].as_bool().unwrap_or(false),
        all_sheets: args["all_sheets"].as_bool().unwrap_or(true),
    };

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    cli::export_schematic_pdf(&ctx.config.kicad_cli, &sch_path, &output_path, &options).await?;

    Ok(CallToolResult::json(&json!({
        "exported": output_path.display().to_string(),
        "black_and_white": options.black_and_white,
        "all_sheets": options.all_sheets
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
    let labels = extract_all_net_labels(&tree);
    let placed = placed_pins_by_reference(&tree);

    let mut g = net_graph_for(&tree, &wires, &labels);

    // Collect distinct net names
    let mut net_names: Vec<String> = labels.iter().map(|l| l.net.clone()).collect();
    net_names.sort();
    net_names.dedup();

    // Build per-component net map
    let components: Vec<serde_json::Value> = instances
        .iter()
        .map(|inst| {
            let pins: Vec<serde_json::Value> = placed
                .iter()
                .find(
                    |(placed_instance, _)| match (&placed_instance.uuid, &inst.uuid) {
                        (Some(placed_uuid), Some(uuid)) => placed_uuid == uuid,
                        _ => {
                            placed_instance.reference == inst.reference
                                && placed_instance.unit == inst.unit
                                && placed_instance.x == inst.x
                                && placed_instance.y == inst.y
                                && placed_instance.lib_symbol_name() == inst.lib_symbol_name()
                        }
                    },
                )
                .map(|(_, pins)| {
                    pins.iter()
                        .map(|(pin, transform)| {
                            let (px, py) = pin_endpoint(pin, *transform);
                            let net = g.net_at(px, py).unwrap_or_else(|| "~".to_string());
                            json!({
                                "number": pin.number,
                                "name": pin.name,
                                "net": net,
                                "x": px, "y": py
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

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

/// ERC positions ride on the entry itself as `x`/`y`, not as a nested object.
fn flatten_pos(entry: &mut serde_json::Value, pos: Option<&cli::ReportPos>) {
    if let Some(pos) = pos {
        entry["x"] = json!(pos.x);
        entry["y"] = json!(pos.y);
    }
}

async fn handle_run_erc(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let min_severity = args["severity"].as_str().unwrap_or("warning");

    let owning_root = match owning_project_root(&sch_path) {
        Ok(root) => root,
        Err(error) => return Ok(error.into_tool_result()),
    };
    if let Some(root) = owning_root {
        // Structured, not free text: a caller can react to `invalid_argument`
        // on `schematic` by retrying against the named root, which is exactly
        // what the message says to do.
        let reason = format!(
            "{} is a sheet inside the project rooted at {}, not a project root of its own. \
             kicad-cli treats the file it is handed as the root and looks for a .kicad_pro \
             beside it, so the project's sym-lib-table is never read and every symbol from a \
             project library is reported as an unknown library — violations that describe the \
             invocation, not the design. ERC covers the whole hierarchy in any case: run it on \
             {}.",
            sch_path.display(),
            root.display(),
            root.display()
        );
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "schematic".to_string(),
                reason: reason.clone(),
            },
            reason,
        ));
    }

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
            let items: Vec<serde_json::Value> = v
                .items
                .iter()
                .map(|item| {
                    let mut entry = json!({ "description": item.description });
                    flatten_pos(&mut entry, item.pos.as_ref());
                    if let Some(uuid) = &item.uuid {
                        entry["uuid"] = json!(uuid);
                    }
                    entry
                })
                .collect();
            let mut entry = json!({
                "severity": v.severity,
                "description": v.description,
                // KiCad's stable key for the rule; `description` is prose.
                "rule": v.rule,
                // A pin conflict names two pins, and `description` above
                // carries only the first.
                "items": items,
            });
            if let Some(sheet) = &v.sheet {
                entry["sheet"] = json!(sheet);
            }
            // The violation's own x/y predate `items` and stay the first
            // item's.
            flatten_pos(&mut entry, v.items.first().and_then(|i| i.pos.as_ref()));
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

    // Collect all valid snap targets: pin endpoints + label positions + wire endpoints
    let mut snap_targets: Vec<(f64, f64)> = Vec::new();

    for (pin, transform) in placed_pins(&tree) {
        snap_targets.push(pin_endpoint(&pin, transform));
    }
    for l in &labels {
        snap_targets.push((l.x, l.y));
    }
    for w in &wires {
        snap_targets.push((w.x1, w.y1));
        snap_targets.push((w.x2, w.y2));
    }

    let mut fixes = Vec::new();

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

            // A geometric near miss is still ambiguous when two distinct
            // destinations are in range. Refuse instead of depending on file
            // order to choose one.
            let mut raw_near = Vec::new();
            for &(tx, ty) in &snap_targets {
                let dist = ((px - tx).powi(2) + (py - ty).powi(2)).sqrt();
                let own_other_endpoint = if *is_start {
                    (w.x2, w.y2)
                } else {
                    (w.x1, w.y1)
                };
                if dist > exact_tol
                    && dist <= snap_tol
                    && !points_coincident(
                        tx,
                        ty,
                        own_other_endpoint.0,
                        own_other_endpoint.1,
                        exact_tol,
                    )
                {
                    raw_near.push((tx, ty));
                }
            }
            let near = distinct_geometric_destinations(&raw_near, exact_tol);
            if near.len() > 1 {
                let target = format!(
                    "wire {} {} endpoint at ({px}, {py})",
                    w.uuid.as_deref().unwrap_or("without UUID"),
                    if *is_start { "start" } else { "end" }
                );
                let candidates = near
                    .iter()
                    .map(|(x, y)| format!("({x}, {y})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Ok(stale_connectivity_target(
                    target,
                    format!("more than one snap destination was observed: {candidates}"),
                ));
            }

            if let Some(&(tx, ty)) = near.first() {
                let Some(uuid) = &w.uuid else {
                    let target = format!("wire endpoint at ({px}, {py})");
                    return Ok(stale_connectivity_target(
                        target,
                        "the wire has no UUID and cannot be identified after a concurrent edit",
                    ));
                };
                fixes.push(PlannedWireFix {
                    uuid: uuid.clone(),
                    endpoint: if *is_start {
                        WireEndpoint::Start
                    } else {
                        WireEndpoint::End
                    },
                    from: (px, py),
                    to: (tx, ty),
                });
            }
        }
    }

    // When two loose wire ends see only each other, the independently planned
    // fixes point in opposite directions. Applying both would merely swap the
    // coordinates and leave the wires disconnected. Keep the target with the
    // lexicographically smaller stable item key stationary and move only its
    // peer onto it.
    let mut keep = vec![true; fixes.len()];
    for left in 0..fixes.len() {
        for right in (left + 1)..fixes.len() {
            if points_coincident(
                fixes[left].from.0,
                fixes[left].from.1,
                fixes[right].to.0,
                fixes[right].to.1,
                exact_tol,
            ) && points_coincident(
                fixes[left].to.0,
                fixes[left].to.1,
                fixes[right].from.0,
                fixes[right].from.1,
                exact_tol,
            ) {
                if fixes[left].stable_key() <= fixes[right].stable_key() {
                    keep[left] = false;
                } else {
                    keep[right] = false;
                }
            }
        }
    }
    fixes = fixes
        .into_iter()
        .zip(keep)
        .filter_map(|(fix, keep)| keep.then_some(fix))
        .collect();

    // Resolve every target structurally before constructing a replacement. A
    // failure refuses the whole operation and leaves the document unchanged.
    let mut file_edits = Vec::with_capacity(fixes.len());
    for fix in &fixes {
        let (start, end) = match wire_endpoint_block(&content, &fix.uuid, fix.endpoint) {
            Ok(range) => range,
            Err(error) => return Ok(error.into_result()),
        };
        let tag = parse_sexp(&content[start..end])
            .ok()
            .and_then(|node| node.head().map(ToOwned::to_owned))
            .unwrap_or_else(|| "xy".to_owned());
        file_edits.push(SexpEdit::replace(
            start,
            end,
            format!("({tag} {} {})", fix.to.0, fix.to.1),
        ));
    }

    if dry_run || fixes.is_empty() {
        return Ok(CallToolResult::json(&json!({
            "fixes_found": fixes.len(),
            "applied": false,
            "dry_run": dry_run,
            "fixes": fixes.iter().map(PlannedWireFix::as_json).collect::<Vec<_>>()
        })));
    }

    let new_content = apply_edits(content.clone(), file_edits);
    if let Err(error) = write_atomic_if_unchanged(&sch_path, &content, &new_content) {
        if let Some(refusal) = connectivity_write_refusal(&sch_path, &error) {
            return Ok(refusal);
        }
        return Err(error.into());
    }

    // Never report the requested coordinates as though they were committed.
    // Reparse the saved document and derive every returned endpoint from it.
    let (committed, _) = read_schematic(&sch_path)?;
    let mut observed = Vec::with_capacity(fixes.len());
    for fix in &fixes {
        let actual = match observed_wire_endpoint(&committed, &fix.uuid, fix.endpoint) {
            Ok(point) => point,
            Err(error) => return Ok(error.into_result()),
        };
        if !points_coincident(actual.0, actual.1, fix.to.0, fix.to.1, 1e-9) {
            return Ok(stale_connectivity_target(
                format!("wire {} {} endpoint", fix.uuid, fix.endpoint.name()),
                format!(
                    "committed readback was ({}, {}) instead of ({}, {})",
                    actual.0, actual.1, fix.to.0, fix.to.1
                ),
            ));
        }
        observed.push(json!({
            "wire_uuid": fix.uuid,
            "endpoint": fix.endpoint.name(),
            "from": { "x": fix.from.0, "y": fix.from.1 },
            "to": { "x": actual.0, "y": actual.1 }
        }));
    }

    Ok(CallToolResult::json(&json!({
        "fixes_found": observed.len(),
        "applied": !observed.is_empty(),
        "dry_run": false,
        "fixes": observed
    })))
}

/// Collapse different schematic objects that identify the same geometric
/// destination. Ambiguity is about coordinates, not how many pins, labels, or
/// wire endpoints happen to coexist there.
fn distinct_geometric_destinations(candidates: &[(f64, f64)], tolerance: f64) -> Vec<(f64, f64)> {
    let mut destinations = Vec::new();
    for &(x, y) in candidates {
        if !destinations
            .iter()
            .any(|&(dx, dy)| points_coincident(x, y, dx, dy, tolerance))
        {
            destinations.push((x, y));
        }
    }
    destinations
}

#[derive(Clone, Copy, Debug)]
enum WireEndpoint {
    Start,
    End,
}

impl WireEndpoint {
    fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::End => "end",
        }
    }
}

#[derive(Debug)]
struct PlannedWireFix {
    uuid: String,
    endpoint: WireEndpoint,
    from: (f64, f64),
    to: (f64, f64),
}

impl PlannedWireFix {
    fn stable_key(&self) -> (&str, &'static str) {
        (&self.uuid, self.endpoint.name())
    }

    fn as_json(&self) -> serde_json::Value {
        json!({
            "wire_uuid": self.uuid,
            "endpoint": self.endpoint.name(),
            "from": { "x": self.from.0, "y": self.from.1 },
            "to": { "x": self.to.0, "y": self.to.1 }
        })
    }
}

#[derive(Debug)]
enum ConnectivityTargetError {
    Ambiguous {
        target: String,
        candidates: Vec<String>,
    },
    Stale {
        target: String,
        reason: String,
    },
}

impl ConnectivityTargetError {
    fn into_result(self) -> CallToolResult {
        match self {
            Self::Ambiguous { target, candidates } => stale_connectivity_target(
                target,
                format!(
                    "more than one schematic item identifies the target: {}",
                    candidates.join(", ")
                ),
            ),
            Self::Stale { target, reason } => stale_connectivity_target(target, reason),
        }
    }
}

fn stale_connectivity_target(
    target: impl Into<String>,
    reason: impl Into<String>,
) -> CallToolResult {
    let target = target.into();
    let reason = reason.into();
    CallToolResult::error_kind(
        ToolErrorKind::StaleTarget {
            target: target.clone(),
            reason: reason.clone(),
        },
        format!("cannot safely edit {target}: {reason}"),
    )
}

fn connectivity_write_refusal(path: &Path, error: &SexpError) -> Option<CallToolResult> {
    let reason = match error {
        SexpError::Conflict { .. } => {
            "the schematic changed after connectivity fixes were planned; reload and retry"
        }
        SexpError::KiCadEditorLocked { .. } => {
            "KiCad owns the schematic; use a live editor mutation or close the document"
        }
        _ => return None,
    };
    Some(stale_connectivity_target(
        path.display().to_string(),
        reason,
    ))
}

fn node_uuid(node: &SexpNode) -> Option<&str> {
    node.find("uuid")
        .and_then(|uuid| uuid.get(1))
        .and_then(SexpNode::as_str)
}

fn wire_block(content: &str, uuid: &str) -> Result<(usize, usize), ConnectivityTargetError> {
    let target = format!("wire UUID {uuid}");
    let mut matches = Vec::new();
    for (start, end) in find_direct_child_blocks(content, "kicad_sch") {
        let Ok(node) = parse_sexp(&content[start..end]) else {
            continue;
        };
        if node_uuid(&node) == Some(uuid) {
            matches.push((start, end, node.head().unwrap_or("unknown").to_owned()));
        }
    }

    match matches.as_slice() {
        [] => Err(ConnectivityTargetError::Stale {
            target,
            reason: "no top-level schematic item has that UUID".to_owned(),
        }),
        [(start, end, kind)] if kind == "wire" => Ok((*start, *end)),
        [(_, _, kind)] => Err(ConnectivityTargetError::Stale {
            target,
            reason: format!("the UUID identifies a {kind}, not a wire"),
        }),
        _ => Err(ConnectivityTargetError::Ambiguous {
            target,
            candidates: matches
                .iter()
                .map(|(start, _, kind)| format!("{kind} at byte {start}"))
                .collect(),
        }),
    }
}

fn direct_children_with_tag(source: &str, parent: &str, tag: &str) -> Vec<(usize, usize)> {
    find_direct_child_blocks(source, parent)
        .into_iter()
        .filter(|(start, end)| {
            parse_sexp(&source[*start..*end])
                .ok()
                .is_some_and(|node| node.head() == Some(tag))
        })
        .collect()
}

fn wire_endpoint_block(
    content: &str,
    uuid: &str,
    endpoint: WireEndpoint,
) -> Result<(usize, usize), ConnectivityTargetError> {
    let (wire_start, wire_end) = wire_block(content, uuid)?;
    let wire = &content[wire_start..wire_end];
    let pts = direct_children_with_tag(wire, "wire", "pts");
    let legacy_start = direct_children_with_tag(wire, "wire", "start");
    let legacy_end = direct_children_with_tag(wire, "wire", "end");
    let target = format!("wire UUID {uuid} {} endpoint", endpoint.name());

    if pts.len() == 1 && legacy_start.is_empty() && legacy_end.is_empty() {
        let (pts_start, pts_end) = pts[0];
        let pts_source = &wire[pts_start..pts_end];
        let xy = direct_children_with_tag(pts_source, "pts", "xy");
        if xy.len() != 2 {
            return Err(ConnectivityTargetError::Stale {
                target,
                reason: format!("expected exactly two xy points, observed {}", xy.len()),
            });
        }
        let (start, end) = xy[match endpoint {
            WireEndpoint::Start => 0,
            WireEndpoint::End => 1,
        }];
        return Ok((wire_start + pts_start + start, wire_start + pts_start + end));
    }

    if pts.is_empty() && legacy_start.len() == 1 && legacy_end.len() == 1 {
        let (start, end) = match endpoint {
            WireEndpoint::Start => legacy_start[0],
            WireEndpoint::End => legacy_end[0],
        };
        return Ok((wire_start + start, wire_start + end));
    }

    Err(ConnectivityTargetError::Stale {
        target,
        reason: "wire endpoint representation is missing or structurally ambiguous".to_owned(),
    })
}

fn observed_wire_endpoint(
    content: &str,
    uuid: &str,
    endpoint: WireEndpoint,
) -> Result<(f64, f64), ConnectivityTargetError> {
    let (start, end) = wire_endpoint_block(content, uuid, endpoint)?;
    let node =
        parse_sexp(&content[start..end]).map_err(|error| ConnectivityTargetError::Stale {
            target: format!("wire UUID {uuid} {} endpoint", endpoint.name()),
            reason: error.to_string(),
        })?;
    let x = node
        .get_f64(1)
        .ok_or_else(|| ConnectivityTargetError::Stale {
            target: format!("wire UUID {uuid} {} endpoint", endpoint.name()),
            reason: "x coordinate is missing".to_owned(),
        })?;
    let y = node
        .get_f64(2)
        .ok_or_else(|| ConnectivityTargetError::Stale {
            target: format!("wire UUID {uuid} {} endpoint", endpoint.name()),
            reason: "y coordinate is missing".to_owned(),
        })?;
    Ok((x, y))
}

#[cfg(test)]
mod netlist_summary_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::io::Write;
    use std::sync::Arc;

    /// R1 with pin 2 wired to a `power:GND` symbol, and pin 1 on a plain label.
    /// The rail is the case that used to disappear.
    const SCH: &str = include_str!("../../tests/fixtures/power_rail.kicad_sch");
    const ECC83: &str = include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch");

    async fn summary(content: &str) -> serde_json::Value {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();

        let ctx = ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        );
        let result = handle_export_netlist_summary(
            &json!({ "schematic": f.path().to_str().unwrap() }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    fn net_of(summary: &serde_json::Value, reference: &str, pin: &str) -> String {
        summary["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["reference"] == reference)
            .expect("component present")["pins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["number"] == pin)
            .expect("pin present")["net"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The regression: nets came from labels alone, so a pin reached only
    /// through a power symbol reported `"~"` and the rail was absent from
    /// `nets` — the summary showed every power connection as unconnected.
    #[tokio::test]
    async fn a_rail_reached_through_a_power_symbol_is_named() {
        let s = summary(SCH).await;
        assert_eq!(net_of(&s, "R1", "2"), "GND");
        assert_eq!(net_of(&s, "R1", "1"), "SIG");

        let nets: Vec<&str> = s["nets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap())
            .collect();
        assert_eq!(nets, vec!["GND", "SIG"]);
        assert_eq!(s["net_count"], 2);
    }

    /// The power symbol's own pin resolves too, so `#PWR01` is not reported as
    /// a floating part.
    #[tokio::test]
    async fn the_power_symbol_pin_resolves_to_its_own_rail() {
        let s = summary(SCH).await;
        assert_eq!(net_of(&s, "#PWR01", "1"), "GND");
    }

    /// A rail with no wire on it is still a net: the symbol declares it.
    #[tokio::test]
    async fn an_unwired_power_symbol_still_declares_its_net() {
        let sch = SCH.replace(
            "(wire (pts (xy 100 103.81) (xy 100 110)) (uuid \"w1\"))",
            "",
        );
        let s = summary(&sch).await;
        assert_eq!(net_of(&s, "R1", "2"), "~");
        assert!(s["nets"].as_array().unwrap().iter().any(|n| n == "GND"));
    }

    #[tokio::test]
    async fn multi_unit_summary_reports_only_the_pins_placed_by_each_unit() {
        let result = summary(ECC83).await;
        let mut pin_sets: Vec<Vec<String>> = result["components"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|component| component["reference"] == "U1")
            .map(|component| {
                let mut pins: Vec<String> = component["pins"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|pin| pin["number"].as_str().unwrap().to_string())
                    .collect();
                pins.sort();
                pins
            })
            .collect();
        pin_sets.sort();

        assert_eq!(
            pin_sets,
            vec![
                vec!["1".to_string(), "2".to_string(), "3".to_string()],
                vec!["4".to_string(), "5".to_string(), "9".to_string()],
                vec!["6".to_string(), "7".to_string(), "8".to_string()],
            ]
        );
    }
}

#[cfg(test)]
mod multi_unit_connectivity_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::io::Write;
    use std::sync::Arc;

    const ECC83: &str = include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch");

    #[tokio::test]
    async fn connectivity_fix_does_not_snap_to_a_pin_from_an_unplaced_unit() {
        // Unit 1 is placed at (160.02, 64.77). Applying that transform to the
        // heater unit's pin 5 invents a phantom target at (162.56, 76.20).
        // Keep a loose wire end 0.02 mm from that point: the old all-unit
        // extraction proposed a destructive snap, while the placed-unit view
        // correctly leaves it alone.
        let probe_wire = r#"
	(wire
		(pts
			(xy 162.56 76.22) (xy 170 80)
		)
		(stroke (width 0) (type default))
		(uuid "00000000-0000-4000-8000-000000000182")
	)
"#;
        let content = ECC83.replacen(
            "\t(sheet_instances",
            &format!("{probe_wire}\t(sheet_instances"),
            1,
        );
        assert_ne!(content, ECC83, "fixture insertion point changed");

        let mut schematic = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        schematic.write_all(content.as_bytes()).unwrap();
        schematic.flush().unwrap();

        let definition = tools()
            .into_iter()
            .find(|tool_def| tool_def.name == "fix_connectivity")
            .unwrap();
        let context = ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        );
        let result = (definition.handler)(
            &json!({
                "schematic": schematic.path().to_str().unwrap(),
                "snap_tolerance": 0.05,
                "dry_run": true
            }),
            Arc::new(context),
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        let response: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(response["fixes_found"], 0, "phantom pin snap: {response}");
    }
}

#[cfg(test)]
mod connectivity_edit_tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use crate::tools::ServerConfig;
    use std::io::Write;
    use std::sync::Arc;

    const KICAD_STRUCTURAL_FIXTURE: &str =
        include_str!("../../tests/fixtures/structural_scans_kicad10.kicad_sch");

    fn fixture(uuid: Option<&str>, decoy: &str, labels: &str) -> String {
        let uuid = uuid
            .map(|uuid| format!("\r\n\t\t(uuid \"{uuid}\")"))
            .unwrap_or_default();
        format!(
            "(kicad_sch\r\n\t(version 20231120)\r\n{decoy}\t(wire\r\n\t\t(pts\r\n\t\t\t(xy 0 0)\r\n\t\t\t(xy 10.04 0)\r\n\t\t)\r\n\t\t(stroke (width 0) (type default)){uuid}\r\n\t)\r\n{labels})\r\n"
        )
    }

    fn label(name: &str, x: f64, uuid: &str) -> String {
        format!("\t(label \"{name}\"\r\n\t\t(at {x} 0 0)\r\n\t\t(uuid \"{uuid}\")\r\n\t)\r\n")
    }

    async fn run_fix(content: &str) -> (CallToolResult, tempfile::NamedTempFile) {
        let mut file = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        let context = ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        );
        let result = handle_fix_connectivity(
            &json!({
                "schematic": file.path().to_str().unwrap(),
                "snap_tolerance": 0.05,
                "dry_run": false
            }),
            &context,
        )
        .await
        .unwrap();
        (result, file)
    }

    fn response(result: &CallToolResult) -> serde_json::Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text response");
        };
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn tab_crlf_kicad10_wire_is_edited_structurally_and_read_back() {
        let decoy = "\t(symbol\r\n\t\t(property \"decoy\" \"nested\"\r\n\t\t\t(uuid \"wire-1\")\r\n\t\t)\r\n\t\t(uuid \"symbol-1\")\r\n\t)\r\n";
        let original = fixture(Some("wire-1"), decoy, &label("N", 10.0, "label-1"));
        let (result, file) = run_fix(&original).await;

        assert!(!result.is_error, "{result:?}");
        let body = response(&result);
        assert_eq!(body["fixes_found"], 1);
        assert_eq!(body["applied"], true);
        assert_eq!(body["fixes"][0]["wire_uuid"], "wire-1");
        assert_eq!(body["fixes"][0]["endpoint"], "end");
        assert_eq!(body["fixes"][0]["to"], json!({ "x": 10.0, "y": 0.0 }));

        let committed = std::fs::read_to_string(file.path()).unwrap();
        assert!(committed.contains("\r\n\t\t\t(xy 0 0)\r\n"));
        assert!(committed.contains("\r\n\t\t\t(xy 10 0)\r\n"));
        assert!(committed.contains("(property \"decoy\" \"nested\""));
        assert_eq!(
            observed_wire_endpoint(&committed, "wire-1", WireEndpoint::End).unwrap(),
            (10.0, 0.0)
        );
    }

    #[tokio::test]
    async fn coincident_kicad_pin_and_label_are_one_snap_destination() {
        let tree = parse_sexp(KICAD_STRUCTURAL_FIXTURE).unwrap();
        let destination = (190.5, 193.04);
        assert!(extract_labels(&tree).iter().any(|label| points_coincident(
            label.x,
            label.y,
            destination.0,
            destination.1,
            1e-9
        )));
        assert!(placed_pins(&tree).into_iter().any(|(pin, transform)| {
            let endpoint = pin_endpoint(&pin, transform);
            points_coincident(endpoint.0, endpoint.1, destination.0, destination.1, 1e-9)
        }));

        let (result, file) = run_fix(KICAD_STRUCTURAL_FIXTURE).await;

        assert!(!result.is_error, "{result:?}");
        let body = response(&result);
        assert_eq!(body["fixes_found"], 1);
        assert_eq!(
            body["fixes"][0]["wire_uuid"],
            "11111111-2222-4333-8444-555555555555"
        );
        assert_eq!(body["fixes"][0]["to"], json!({ "x": 190.5, "y": 193.04 }));
        let committed = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(
            observed_wire_endpoint(
                &committed,
                "11111111-2222-4333-8444-555555555555",
                WireEndpoint::Start,
            )
            .unwrap(),
            destination
        );
    }

    #[tokio::test]
    async fn wire_without_uuid_is_a_stale_refusal_and_does_not_write() {
        let original = fixture(None, "", &label("N", 10.0, "label-1"));
        let (result, file) = run_fix(&original).await;

        assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), original);
    }

    #[tokio::test]
    async fn two_distinct_snap_destinations_are_refused_without_writing() {
        let labels = format!(
            "{}{}",
            label("LEFT", 10.0, "label-1"),
            label("RIGHT", 10.08, "label-2")
        );
        let original = fixture(Some("wire-1"), "", &labels);
        let (result, file) = run_fix(&original).await;

        assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), original);
    }

    #[tokio::test]
    async fn reciprocal_wire_endpoints_coalesce_to_one_observed_connection() {
        let original = "(kicad_sch\r\n\t(version 20231120)\r\n\t(wire\r\n\t\t(pts (xy 0 0) (xy 10 0))\r\n\t\t(uuid \"wire-a\")\r\n\t)\r\n\t(wire\r\n\t\t(pts (xy 10.04 0) (xy 20 0))\r\n\t\t(uuid \"wire-b\")\r\n\t)\r\n)\r\n";
        let (result, file) = run_fix(original).await;

        assert!(!result.is_error, "{result:?}");
        let body = response(&result);
        assert_eq!(body["fixes_found"], 1);
        assert_eq!(body["fixes"][0]["wire_uuid"], "wire-b");
        assert_eq!(body["fixes"][0]["to"], json!({ "x": 10.0, "y": 0.0 }));

        let committed = std::fs::read_to_string(file.path()).unwrap();
        let left = observed_wire_endpoint(&committed, "wire-a", WireEndpoint::End).unwrap();
        let right = observed_wire_endpoint(&committed, "wire-b", WireEndpoint::Start).unwrap();
        assert_eq!(left, right);
    }

    #[test]
    fn duplicate_top_level_uuid_is_a_stale_refusal() {
        let content = "(kicad_sch\n  (wire (start 0 0) (end 1 0) (uuid \"dup\"))\n  (wire (start 2 0) (end 3 0) (uuid \"dup\"))\n)";
        let error = wire_endpoint_block(content, "dup", WireEndpoint::Start).unwrap_err();
        assert!(matches!(&error, ConnectivityTargetError::Ambiguous { .. }));
        assert_eq!(
            extract_error_kind(&error.into_result()).as_deref(),
            Some("stale_target")
        );
    }

    #[test]
    fn uuid_on_wrong_item_kind_is_stale() {
        let content =
            "(kicad_sch\n  (junction (at 1 2) (diameter 0) (color 0 0 0 0) (uuid \"not-wire\"))\n)";
        let error = wire_endpoint_block(content, "not-wire", WireEndpoint::Start).unwrap_err();
        assert!(matches!(error, ConnectivityTargetError::Stale { .. }));
    }

    #[test]
    fn legacy_endpoint_keeps_its_structural_tag() {
        let content = "(kicad_sch\n  (wire (start 1 2) (end 3 4) (uuid \"legacy\"))\n)";
        let (start, end) = wire_endpoint_block(content, "legacy", WireEndpoint::End).unwrap();
        assert_eq!(&content[start..end], "(end 3 4)");
        assert_eq!(
            observed_wire_endpoint(content, "legacy", WireEndpoint::End).unwrap(),
            (3.0, 4.0)
        );
    }

    #[test]
    fn stale_revision_maps_to_structured_refusal() {
        let path = Path::new("design.kicad_sch");
        let error = SexpError::Conflict {
            path: path.to_path_buf(),
        };
        let result = connectivity_write_refusal(path, &error).unwrap();
        assert_eq!(extract_error_kind(&result).as_deref(), Some("stale_target"));
    }
}

// ─── Project-root detection for ERC ───────────────────────────────────────────

/// The root schematic of the project that owns `file` as a sub-sheet, if any.
///
/// `kicad-cli` treats whatever file it is handed as the root of the hierarchy
/// and looks for a `.kicad_pro` named after it. A sub-sheet has no such file,
/// so the project's `sym-lib-table` is never read and every symbol from a
/// project library comes back as an unknown library. Those violations describe
/// the invocation rather than the design, and the obvious remedy — registering
/// the library again — is the wrong one, so the case is worth naming.
///
/// Returns `None` for a proven root or a loose schematic with no project
/// candidate. Unproven or ambiguous ownership is a structured conflict.
fn owning_project_root(file: &Path) -> Result<Option<PathBuf>, crate::tools::SchematicTargetError> {
    Ok(
        crate::tools::resolve_schematic_ownership(file)?.and_then(|ownership| {
            (!same_file(&ownership.root_schematic, file)).then_some(ownership.root_schematic)
        }),
    )
}

/// Path equality that survives `.\foo` versus `foo` and case-insensitive
/// filesystems. Falls back to a literal comparison for paths that do not exist.
fn same_file(a: &Path, b: &Path) -> bool {
    canonical(a) == canonical(b)
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn render_ctx() -> ToolContext {
        ToolContext::new(
            crate::tools::ServerConfig::default(),
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    #[tokio::test]
    async fn render_png_refuses_an_out_of_range_width() {
        let result = handle_render_png(
            &json!({ "schematic": "unused.kicad_sch", "width_px": 9000 }),
            &render_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text");
        };
        assert!(text.contains("width_px"), "{text}");
    }

    #[test]
    fn render_and_baseline_tools_are_registered() {
        let names: Vec<&str> = tools().iter().map(|t| t.name).collect();
        assert!(names.contains(&"render_schematic_png"), "{names:?}");
        assert!(names.contains(&"set_visual_baseline"), "{names:?}");
        assert!(names.contains(&"compare_visual_baseline"), "{names:?}");
        assert_eq!(names.len(), 10, "sch_export tool count");
    }

    #[tokio::test]
    async fn compare_without_a_baseline_is_an_explicit_result_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let sch = dir.path().join("fresh.kicad_sch");
        std::fs::write(&sch, "(kicad_sch)").unwrap();
        let result = handle_baseline_compare(
            &json!({ "schematic": sch.to_string_lossy() }),
            &render_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "no-baseline is a state, not a failure");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text");
        };
        let response: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(response["status"], "no_baseline");
    }

    #[test]
    fn baseline_paths_live_under_the_projects_konnect_dir() {
        let (png, meta) = baseline_paths(std::path::Path::new("C:/proj/amp.kicad_sch")).unwrap();
        assert!(png
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with(".konnect/baselines/amp.png"));
        assert!(meta.to_string_lossy().ends_with("amp.json"));
    }

    /// A root sheet holding one sub-sheet reference. The `(sheet …)` block is
    /// shaped the way KiCad 10 writes one — trimmed to the fields the loader
    /// reads.
    fn root_with_child(dir: &Path, root: &str, child_file: &str) -> PathBuf {
        let path = dir.join(root);
        std::fs::write(
            &path,
            format!(
                r#"(kicad_sch
	(version 20250610)
	(generator "konnect")
	(generator_version "10.0")
	(uuid "00000000-0000-4000-8000-000000000001")
	(paper "A4")
	(sheet
		(at 40 50)
		(size 80 25)
		(uuid "00000000-0000-4000-8000-000000000002")
		(property "Sheetname" "Child"
			(at 40 49.365 0)
		)
		(property "Sheetfile" "{child_file}"
			(at 40 75.635 0)
		)
	)
	(sheet_instances
		(path "/" (page "1"))
	)
)
"#
            ),
        )
        .unwrap();
        path
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn blank(dir: &Path, name: &str) -> PathBuf {
        write(dir, name, &crate::tools::blank_schematic_template())
    }

    #[test]
    fn child_sheet_resolves_to_its_project_root() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "proj.kicad_pro", "{}");
        let root = root_with_child(tmp.path(), "proj.kicad_sch", "child.kicad_sch");
        let child = blank(tmp.path(), "child.kicad_sch");

        assert_eq!(owning_project_root(&child).unwrap(), Some(root));
    }

    #[test]
    fn a_root_of_its_own_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "proj.kicad_pro", "{}");
        let root = root_with_child(tmp.path(), "proj.kicad_sch", "child.kicad_sch");
        blank(tmp.path(), "child.kicad_sch");

        assert_eq!(owning_project_root(&root).unwrap(), None);
    }

    #[test]
    fn a_sheet_belonging_to_no_project_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        let loose = blank(tmp.path(), "loose.kicad_sch");

        assert_eq!(owning_project_root(&loose).unwrap(), None);
    }

    /// The refusal is a structured `invalid_argument` naming `schematic`, so a
    /// caller can react by retrying against the root the message names — the
    /// convention `mcp/error.rs` asks for on anything an LLM might branch on.
    #[tokio::test]
    async fn the_sub_sheet_refusal_is_structured_and_names_the_field() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "proj.kicad_pro", "{}");
        root_with_child(tmp.path(), "proj.kicad_sch", "child.kicad_sch");
        let child = blank(tmp.path(), "child.kicad_sch");

        let ctx = crate::tools::ToolContext::new(
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
        );
        let result = handle_run_erc(
            &serde_json::json!({ "schematic": child.display().to_string() }),
            &ctx,
        )
        .await
        .expect("a refusal is a tool error, not a transport error");

        assert!(result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        let output: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(output["error"]["kind"], "invalid_argument");
        assert_eq!(output["error"]["field"], "schematic");
        assert!(output["error"]["reason"]
            .as_str()
            .unwrap()
            .contains("proj.kicad_sch"));
    }

    /// Sitting beside a project is not the same as belonging to it — the file
    /// has to appear in the sheet tree.
    #[test]
    fn unrelated_neighbour_returns_ownership_conflict() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "proj.kicad_pro", "{}");
        root_with_child(tmp.path(), "proj.kicad_sch", "child.kicad_sch");
        blank(tmp.path(), "child.kicad_sch");
        let stranger = blank(tmp.path(), "stranger.kicad_sch");

        let result = owning_project_root(&stranger)
            .unwrap_err()
            .into_tool_result();
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("conflict")
        );
    }

    #[test]
    fn sibling_project_does_not_bypass_erc_ownership_conflict() {
        let directory = TempDir::new().unwrap();
        let (outer, _) = crate::tools::schematic_target_tests::native_project(directory.path());
        let (inner, _) =
            crate::tools::schematic_target_tests::native_project(&directory.path().join("nested"));
        let content = std::fs::read_to_string(&outer).unwrap();
        std::fs::write(
            &outer,
            content.replace(
                "\"ampli_ht.kicad_sch\"",
                "\"nested/complex_hierarchy.kicad_sch\"",
            ),
        )
        .unwrap();
        let result = owning_project_root(&inner).unwrap_err().into_tool_result();
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("conflict")
        );
    }

    /// A sheet cycle must not hang the walk.
    #[test]
    fn a_reference_cycle_terminates() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "proj.kicad_pro", "{}");
        root_with_child(tmp.path(), "proj.kicad_sch", "a.kicad_sch");
        root_with_child(tmp.path(), "a.kicad_sch", "proj.kicad_sch");
        let stranger = blank(tmp.path(), "stranger.kicad_sch");

        let result = owning_project_root(&stranger)
            .unwrap_err()
            .into_tool_result();
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("conflict")
        );
    }
}
