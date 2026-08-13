//! `pcb_export` toolset — Gerber, PDF, SVG, 3D, BOM, netlist, position file, DRC,
//! zone refill, and DXF/GenCAD/IPC-2581/ODB++ interchange formats.
//!
//! All operations delegate to `kicad-cli` via the `cli` module, except `refill_zones`
//! which uses the KiCAD IPC API.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, ToolContext, ToolDef};
use serde_json::json;
use tokio::task;

use super::cli;

// ─── IPC helpers (mirrors pcb_board / pcb_components) ───────────────────────

async fn with_ipc<T, F>(addr: String, f: F) -> anyhow::Result<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&konnect_ipc::client::KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    let result = task::spawn_blocking(move || {
        let client = konnect_ipc::client::KiCadIpcClient::new(&addr);
        f(&client).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking panicked: {e}"))?;
    Ok(result)
}

// ─── Severity filter helpers ──────────────────────────────────────────────────

fn severity_rank(s: &str) -> u8 {
    match s {
        "error" => 2,
        "warning" => 1,
        _ => 0,
    }
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "export_gerber",
            "Export Gerber production files for all copper and mask layers using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output_dir": { "type": "string", "description": "Directory to write Gerber files into" },
                    "layers": {
                        "type": "array",
                        "description": "Layer names to export (empty = all fabrication layers)",
                        "items": { "type": "string" }
                    },
                    "drill_file": { "type": "boolean", "description": "Also generate Excellon drill file", "default": true }
                },
                "required": ["board", "output_dir"]
            }),
            |args, ctx| async move { handle_export_gerber(args, ctx).await }
        ),
        tool!(
            "export_pdf",
            "Export the PCB layout to a PDF file using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Output PDF file path" },
                    "layers": {
                        "type": "array",
                        "description": "Layer names to include (empty = all visible layers)",
                        "items": { "type": "string" }
                    },
                    "black_and_white": { "type": "boolean", "description": "Render in black and white", "default": false }
                },
                "required": ["board", "output"]
            }),
            |args, ctx| async move { handle_export_pdf(args, ctx).await }
        ),
        tool!(
            "export_svg",
            "Export the PCB layout to an SVG file using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Output SVG file path" },
                    "layers": {
                        "type": "array",
                        "description": "Layer names to include (empty = all visible layers)",
                        "items": { "type": "string" }
                    },
                    "black_and_white": { "type": "boolean", "description": "Render in black and white", "default": false }
                },
                "required": ["board", "output"]
            }),
            |args, ctx| async move { handle_export_svg(args, ctx).await }
        ),
        tool!(
            "export_3d",
            "Export the PCB as a 3D model (STEP or VRML) using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Output file path (.step or .wrl)" },
                    "format": {
                        "type": "string",
                        "description": "Export format: 'step' (default) or 'vrml'",
                        "default": "step"
                    },
                    "include_unspecified": {
                        "type": "boolean",
                        "description": "Include footprints with unspecified 3D models",
                        "default": false
                    }
                },
                "required": ["board", "output"]
            }),
            |args, ctx| async move { handle_export_3d(args, ctx).await }
        ),
        tool!(
            "export_bom",
            "Generate a Bill of Materials (BOM) CSV from the schematic's component data.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file (BOM uses schematic data)" },
                    "output": { "type": "string", "description": "Output CSV file path" },
                    "format": {
                        "type": "string",
                        "description": "BOM format passed to kicad-cli: 'csv' (default)",
                        "default": "csv"
                    },
                    "exclude_dnp": {
                        "type": "boolean",
                        "description": "Exclude 'Do Not Place' components",
                        "default": true
                    }
                },
                "required": ["schematic", "output"]
            }),
            |args, ctx| async move { handle_export_bom(args, ctx).await }
        ),
        tool!(
            "export_netlist",
            "Export the PCB netlist to a file in KiCAD or IPC-D-356 format.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file (or .kicad_sch for schematic netlist)" },
                    "output": { "type": "string", "description": "Output netlist file path" },
                    "format": {
                        "type": "string",
                        "description": "Netlist format: 'kicad' or 'ipc' (IPC-D-356)",
                        "default": "kicad"
                    }
                },
                "required": ["board", "output"]
            }),
            |args, ctx| async move { handle_export_netlist(args, ctx).await }
        ),
        tool!(
            "export_position_file",
            "Generate a component placement (pick-and-place) position file for SMT assembly.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Output position file path" },
                    "format": {
                        "type": "string",
                        "description": "File format: 'csv' (default) or 'gerber'",
                        "default": "csv"
                    },
                    "side": {
                        "type": "string",
                        "description": "Board side: 'front', 'back', or 'both'",
                        "default": "both"
                    },
                    "units": {
                        "type": "string",
                        "description": "Coordinate units: 'mm' (default) or 'in'",
                        "default": "mm"
                    }
                },
                "required": ["board", "output"]
            }),
            |args, ctx| async move { handle_export_position_file(args, ctx).await }
        ),
        tool!(
            "export_dxf",
            "Export the PCB to DXF using kicad-cli, one file per requested layer. \
             Useful for mechanical CAD interchange (enclosures, panelization, laser cutting).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output_dir": { "type": "string", "description": "Directory to write DXF files into (one per layer)" },
                    "layers": {
                        "type": "array",
                        "description": "Layer names to export, e.g. ['Edge.Cuts', 'F.Cu']",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board", "output_dir", "layers"]
            }),
            |args, ctx| async move { handle_export_dxf(args, ctx).await }
        ),
        tool!(
            "export_gencad",
            "Export the PCB in GenCAD format using kicad-cli. GenCAD is accepted by some \
             CAM and test-fixture tooling as an alternative to a raw Gerber bundle.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Output .cad file path" }
                },
                "required": ["board", "output"]
            }),
            |args, ctx| async move { handle_export_gencad(args, ctx).await }
        ),
        tool!(
            "export_ipc2581",
            "Export the PCB in IPC-2581 format using kicad-cli. IPC-2581 is a unified \
             fabrication/assembly/test data format accepted by many contract manufacturers \
             as an alternative to a Gerber + drill + BOM + pick-and-place bundle.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Output file path (.xml)" },
                    "units": { "type": "string", "description": "Output units: 'mm' (default) or 'in'", "default": "mm" },
                    "compress": { "type": "boolean", "description": "Compress the output into a zip archive", "default": false }
                },
                "required": ["board", "output"]
            }),
            |args, ctx| async move { handle_export_ipc2581(args, ctx).await }
        ),
        tool!(
            "export_odb",
            "Export the PCB in ODB++ format using kicad-cli. ODB++ is a unified fabrication \
             data format accepted by many fab houses as an alternative to a Gerber + drill bundle.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Output file path" },
                    "units": { "type": "string", "description": "Output units: 'mm' (default) or 'in'", "default": "mm" },
                    "compression": { "type": "string", "description": "Compression mode: 'zip' (default), 'none', or 'tgz'", "default": "zip" }
                },
                "required": ["board", "output"]
            }),
            |args, ctx| async move { handle_export_odb(args, ctx).await }
        ),
        tool!(
            "refill_zones",
            "Refill all copper pour zones on the board. Requires a running KiCAD instance with IPC enabled; returns an error with instructions if KiCAD is not open.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "zones": {
                        "type": "array",
                        "description": "Net names of specific zones to refill (empty = all zones, currently not filtered)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_refill_zones(args, ctx).await }
        ),
        tool!(
            "get_drc_violations",
            "Run the Design Rule Check (DRC) on the PCB and return a list of violations. \
             Provided in `pcb_export` because the output is handy to bundle alongside \
             Gerbers when preparing a build package. For interactive / iterative DRC \
             work, prefer `run_drc` (verification toolset) — same kicad-cli check, \
             cleaner summary with error/warning counts.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Optional path to write DRC report JSON" },
                    "severity": {
                        "type": "string",
                        "description": "Minimum severity to include: 'error', 'warning' (default), 'info'",
                        "default": "warning"
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_drc_violations(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_export_gerber(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output_dir = get_path(args, "output_dir")?;
    let drill = args["drill_file"].as_bool().unwrap_or(true);

    // Ensure output dir exists
    tokio::fs::create_dir_all(&output_dir).await?;

    let cli = &ctx.config.kicad_cli;
    cli::export_gerber(cli, &board, &output_dir).await?;

    if drill {
        // Dedicated drill export. --output is the directory; KiCAD names the
        // .drl file(s) after the board and writes them inside it.
        let _ = cli::export_drill(cli, &board, &output_dir).await; // best-effort
    }

    // List produced files
    let mut files = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&output_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            files.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    files.sort();

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output_dir": output_dir.to_str().unwrap_or(""),
            "files": files
        }))
        .unwrap(),
    ))
}

async fn handle_export_pdf(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output = get_path(args, "output")?;

    // Collect optional layer list
    let layers: Vec<String> = args["layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let layer_refs: Vec<&str> = layers.iter().map(|s| s.as_str()).collect();

    let cli = &ctx.config.kicad_cli;
    cli::export_pdf(cli, &board, &output, &layer_refs).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output": output.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_export_svg(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output = get_path(args, "output")?;

    let layers: Vec<String> = args["layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let layer_refs: Vec<&str> = layers.iter().map(|s| s.as_str()).collect();

    let cli = &ctx.config.kicad_cli;
    cli::export_svg_pcb(cli, &board, &output, &layer_refs).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output": output.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_export_3d(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output = get_path(args, "output")?;
    let format = args["format"].as_str().unwrap_or("step");

    let cli = &ctx.config.kicad_cli;
    cli::export_3d(cli, &board, &output, format).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "format": format,
            "output": output.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_export_bom(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let schematic = get_path(args, "schematic")?;
    let output = get_path(args, "output")?;
    let format = args["format"].as_str().unwrap_or("csv");

    let cli = &ctx.config.kicad_cli;
    cli::export_bom(cli, &schematic, &output, format).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output": output.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_export_netlist(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output = get_path(args, "output")?;
    let format = args["format"].as_str().unwrap_or("kicad");

    let cli = &ctx.config.kicad_cli;
    // kicad-cli `sch export netlist` works on both .kicad_sch and .kicad_pcb paths.
    // For PCB-specific netlist formats (IPC-D-356), delegate same way.
    cli::export_netlist(cli, &board, &output, format).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "format": format,
            "output": output.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_export_position_file(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output = get_path(args, "output")?;
    let format = args["format"].as_str().unwrap_or("csv");
    let side = args["side"].as_str().unwrap_or("both");
    let units = args["units"].as_str().unwrap_or("mm");

    let cli = &ctx.config.kicad_cli;
    cli::export_position_file(cli, &board, &output, format).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "format": format,
            "side": side,
            "units": units,
            "output": output.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_export_dxf(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output_dir = get_path(args, "output_dir")?;
    let layers: Vec<String> = args["layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let layer_refs: Vec<&str> = layers.iter().map(|s| s.as_str()).collect();

    tokio::fs::create_dir_all(&output_dir).await?;

    let cli = &ctx.config.kicad_cli;
    cli::export_dxf(cli, &board, &output_dir, &layer_refs).await?;

    let mut files = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&output_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            files.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    files.sort();

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output_dir": output_dir.to_str().unwrap_or(""),
            "files": files
        }))
        .unwrap(),
    ))
}

async fn handle_export_gencad(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output = get_path(args, "output")?;

    let cli = &ctx.config.kicad_cli;
    cli::export_gencad(cli, &board, &output).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output": output.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_export_ipc2581(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output = get_path(args, "output")?;
    let units = args["units"].as_str().unwrap_or("mm");
    let compress = args["compress"].as_bool().unwrap_or(false);

    let cli = &ctx.config.kicad_cli;
    cli::export_ipc2581(cli, &board, &output, units, compress).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "units": units,
            "compressed": compress,
            "output": output.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_export_odb(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output = get_path(args, "output")?;
    let units = args["units"].as_str().unwrap_or("mm");
    let compression = args["compression"].as_str().unwrap_or("zip");

    let cli = &ctx.config.kicad_cli;
    cli::export_odb(cli, &board, &output, units, compression).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "units": units,
            "compression": compression,
            "output": output.to_str().unwrap_or("")
        }))
        .unwrap(),
    ))
}

async fn handle_refill_zones(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let _cli = &ctx.config.kicad_cli;

    // kicad-cli pcb export gerber triggers zone fills as a side-effect,
    // but the proper command is kicad-cli pcb --refill-zones (not in all versions).
    // Use IPC refill_zones when available, otherwise fall back to file-level
    // zone fill marker update.
    let addr = ctx.config.ipc_address.clone();
    let result = with_ipc(addr, move |client| {
        client.refill_zones()?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "success": true,
                "method": "ipc",
                "board": board.to_str().unwrap_or("")
            }))
            .unwrap(),
        )),
        _ => {
            // Fallback: run kicad-cli with zone-fill option if supported
            // kicad-cli pcb export gerber fills zones as a side effect
            // For now report the limitation
            Ok(CallToolResult::text(
                serde_json::to_string(&json!({
                    "success": false,
                    "note": "Zone refill requires a running KiCAD instance with IPC enabled, or manual zone fill in KiCAD GUI",
                    "board": board.to_str().unwrap_or("")
                }))
                .unwrap(),
            ))
        }
    }
}

async fn handle_get_drc_violations(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let severity_filter = args["severity"].as_str().unwrap_or("warning");
    let min_rank = severity_rank(severity_filter);

    let cli = &ctx.config.kicad_cli;
    let refill = args["refill_zones"].as_bool().unwrap_or(false);
    let violations = cli::run_drc(cli, &board, refill).await?;

    // Optionally write report
    if let Some(out_path) = args["output"].as_str() {
        let report = serde_json::to_string_pretty(&violations)?;
        tokio::fs::write(out_path, report).await?;
    }

    let filtered: Vec<_> = violations
        .iter()
        .filter(|v| severity_rank(&v.severity) >= min_rank)
        .collect();

    let summary = json!({
        "total": violations.len(),
        "filtered_count": filtered.len(),
        "severity_filter": severity_filter,
        "violations": filtered.iter().map(|v| json!({
            "severity": v.severity,
            "description": v.description,
            "pos": v.pos.as_ref().map(|p| json!({ "x": p.x, "y": p.y }))
        })).collect::<Vec<_>>()
    });

    Ok(CallToolResult::text(
        serde_json::to_string(&summary).unwrap(),
    ))
}

#[cfg(test)]
mod new_export_format_tests {
    //! Tests for `export_dxf`/`export_gencad`/`export_ipc2581`/`export_odb`.
    //!
    //! These handlers shell out to `kicad-cli`, which isn't available in CI
    //! (see ROADMAP.md's "mocked IPC endpoint" item — no kicad-cli mock exists
    //! yet either), so we can only test what's reachable without it:
    //! argument validation (missing required args fail before ever touching
    //! `kicad-cli`) and that a missing/unconfigured `kicad-cli` binary produces
    //! a clean error instead of a panic.

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
            },
            Arc::new(ToolRouter::new()),
        )
    }

    #[tokio::test]
    async fn export_dxf_missing_board_returns_error() {
        let ctx = test_ctx();
        let args = json!({ "output_dir": "out", "layers": ["Edge.Cuts"] });
        assert!(handle_export_dxf(&args, &ctx).await.is_err());
    }

    #[tokio::test]
    async fn export_dxf_fails_gracefully_without_kicad_cli() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({
            "board": dir.path().join("board.kicad_pcb").to_str().unwrap(),
            "output_dir": dir.path().join("out").to_str().unwrap(),
            "layers": ["Edge.Cuts", "F.Cu"]
        });
        // kicad_cli is "" in test_ctx, so spawning must fail — but as a
        // returned error, not a panic.
        assert!(handle_export_dxf(&args, &ctx).await.is_err());
    }

    #[tokio::test]
    async fn export_gencad_missing_output_returns_error() {
        let ctx = test_ctx();
        let args = json!({ "board": "board.kicad_pcb" });
        assert!(handle_export_gencad(&args, &ctx).await.is_err());
    }

    #[tokio::test]
    async fn export_gencad_fails_gracefully_without_kicad_cli() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({
            "board": dir.path().join("board.kicad_pcb").to_str().unwrap(),
            "output": dir.path().join("board.cad").to_str().unwrap()
        });
        assert!(handle_export_gencad(&args, &ctx).await.is_err());
    }

    #[tokio::test]
    async fn export_ipc2581_missing_board_returns_error() {
        let ctx = test_ctx();
        let args = json!({ "output": "board.xml" });
        assert!(handle_export_ipc2581(&args, &ctx).await.is_err());
    }

    #[tokio::test]
    async fn export_ipc2581_fails_gracefully_without_kicad_cli() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({
            "board": dir.path().join("board.kicad_pcb").to_str().unwrap(),
            "output": dir.path().join("board.xml").to_str().unwrap(),
            "units": "mm",
            "compress": true
        });
        assert!(handle_export_ipc2581(&args, &ctx).await.is_err());
    }

    #[tokio::test]
    async fn export_odb_missing_output_returns_error() {
        let ctx = test_ctx();
        let args = json!({ "board": "board.kicad_pcb" });
        assert!(handle_export_odb(&args, &ctx).await.is_err());
    }

    #[tokio::test]
    async fn export_odb_fails_gracefully_without_kicad_cli() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({
            "board": dir.path().join("board.kicad_pcb").to_str().unwrap(),
            "output": dir.path().join("board_odb.zip").to_str().unwrap(),
            "units": "mm",
            "compression": "zip"
        });
        assert!(handle_export_odb(&args, &ctx).await.is_err());
    }
}
