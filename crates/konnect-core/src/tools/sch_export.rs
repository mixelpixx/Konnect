//! `sch_export` toolset — export, netlist, ERC, connectivity fix, board sync.
//!
//! All export operations delegate to `kicad-cli` via the `cli` module.
//! `export_netlist_summary` and `fix_connectivity` operate directly on
//! S-expression file content so they work without a running KiCAD instance.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, placed_pins, placed_pins_by_reference, ToolContext, ToolDef};
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident},
    schematic::{
        extract_all_net_labels, extract_labels, extract_symbol_instances, extract_wires,
        pin_endpoint, read_schematic,
    },
    writer::{
        apply_edits, find_block_with_leading_whitespace, write_atomic_if_unchanged, SexpEdit,
    },
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
             Preserves placement, routing, board-only footprints, and footprint artwork.",
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
/// Returns `None` for a schematic that is a root in its own right, one that
/// belongs to no project, and one that sits beside a project without appearing
/// in its sheet tree.
fn owning_project_root(file: &Path) -> Result<Option<PathBuf>, crate::tools::SchematicTargetError> {
    if file.with_extension("kicad_pro").is_file() {
        return Ok(None);
    }
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
    fn unrelated_neighbour_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "proj.kicad_pro", "{}");
        root_with_child(tmp.path(), "proj.kicad_sch", "child.kicad_sch");
        blank(tmp.path(), "child.kicad_sch");
        let stranger = blank(tmp.path(), "stranger.kicad_sch");

        assert_eq!(owning_project_root(&stranger).unwrap(), None);
    }

    /// A sheet cycle must not hang the walk.
    #[test]
    fn a_reference_cycle_terminates() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "proj.kicad_pro", "{}");
        root_with_child(tmp.path(), "proj.kicad_sch", "a.kicad_sch");
        root_with_child(tmp.path(), "a.kicad_sch", "proj.kicad_sch");
        let stranger = blank(tmp.path(), "stranger.kicad_sch");

        assert_eq!(owning_project_root(&stranger).unwrap(), None);
    }
}
