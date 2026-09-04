//! `pcb_export` toolset — Gerber, PDF, SVG, 3D, BOM, netlist, position file, DRC,
//! zone refill, and DXF/GenCAD/IPC-2581/ODB++ interchange formats.
//!
//! Most operations delegate to `kicad-cli` via the `cli` module. `refill_zones`
//! and revision-bound Specctra export use the KiCad IPC API.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_array, with_ipc, ToolContext, ToolDef};
use anyhow::Context;
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::task;

use super::cli;

// ─── Severity filter helpers ──────────────────────────────────────────────────

fn severity_rank(s: &str) -> u8 {
    match s {
        "error" => 2,
        "warning" => 1,
        _ => 0,
    }
}

fn invalid_export_argument(field: &str, reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.clone(),
        },
        format!("Argument '{field}' is invalid: {reason}"),
    )
}

pub(crate) fn optional_string_array(
    args: &serde_json::Value,
    field: &str,
) -> Result<Vec<String>, CallToolResult> {
    let Some(value) = args.get(field) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(invalid_export_argument(field, "must be an array"));
    };
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value.as_str().map(String::from).ok_or_else(|| {
                invalid_export_argument(&format!("{field}[{index}]"), "must be a layer name string")
            })
        })
        .collect()
}

/// Default Gerber selection for a fabrication package: every enabled copper
/// layer plus solder mask, silkscreen, and the board outline. It deliberately
/// excludes drawings, comments, adhesive, courtyard, fab, and margin layers.
pub(crate) fn standard_gerber_layers(board_source: &str) -> anyhow::Result<Vec<String>> {
    let board = konnect_sexp::parser::parse_sexp(board_source)?;
    let layers = konnect_sexp::layers::layers(&board)
        .into_iter()
        .filter(|layer| {
            layer.is_copper()
                || matches!(
                    layer.name.as_str(),
                    "F.Mask" | "B.Mask" | "F.SilkS" | "B.SilkS" | "Edge.Cuts"
                )
        })
        .map(|layer| layer.name)
        .collect::<Vec<_>>();
    if layers.is_empty() {
        anyhow::bail!("board declares no standard fabrication layers");
    }
    Ok(layers)
}

pub(crate) fn validate_position_values(
    format: &str,
    side: &str,
    units: &str,
) -> Result<(), (&'static str, String)> {
    if !matches!(format, "csv" | "gerber") {
        return Err((
            "format",
            format!("must be 'csv' or 'gerber', got '{format}'"),
        ));
    }
    if !matches!(side, "front" | "back" | "both") {
        return Err((
            "side",
            format!("must be 'front', 'back', or 'both', got '{side}'"),
        ));
    }
    if !matches!(units, "mm" | "in") {
        return Err(("units", format!("must be 'mm' or 'in', got '{units}'")));
    }
    if format == "gerber" && side == "both" {
        return Err((
            "side",
            "Gerber position output supports only 'front' or 'back'".to_string(),
        ));
    }
    Ok(())
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "export_gerber",
            "Export Gerber production files using kicad-cli. By default Konnect selects all \
             enabled copper layers, masks, silkscreens, and Edge.Cuts while excluding \
             documentation-only layers.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output_dir": { "type": "string", "description": "Directory to write Gerber files into" },
                    "layers": {
                        "type": "array",
                        "description": "Exact layer names to export. Omit or pass an empty array to auto-select enabled copper, F/B.Mask, F/B.SilkS, and Edge.Cuts.",
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
                        "enum": ["csv"],
                        "description": "BOM format. KiCad 10's schematic BOM export is CSV.",
                        "default": "csv"
                    },
                    "fields": {
                        "type": "string",
                        "description": "Ordered, comma-separated columns to export, e.g. 'Reference,Value,Footprint,MPN,${QUANTITY}'. Any schematic field name works — this is how MPN/LCSC columns reach the fab. Omit for KiCAD's default Reference,Value,Footprint,QUANTITY,DNP."
                    },
                    "labels": {
                        "type": "string",
                        "description": "Ordered, comma-separated column headings matching 'fields'. Omit to label each column with its field name."
                    },
                    "group_by": {
                        "type": "string",
                        "description": "Comma-separated fields whose matching references collapse into one row, e.g. 'Value,Footprint'. Omit for one row per symbol."
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
            "export_specctra_dsn",
            "Export a deterministic Specctra DSN routing job from the exact live KiCad PCB \
             editor revision. Requires the named board to be open with IPC enabled. On KiCad 10, \
             an enabled authenticated ActionPlugin bridge supplies KiCad's native DSN while the \
             IPC snapshot and reverse manifest remain authoritative; otherwise the Rust exporter \
             is used. The first \
             supported profile is deliberately narrow: two copper layers, front-side SMD or \
             through-hole footprints, circle/rectangle pads, one straight-line closed outline, \
             and no existing tracks, vias, or zones. Konnect refuses unsupported geometry or \
             custom DRC rules, or incomplete effective routing rules instead of approximating \
             them, and writes a \
             revision-bound reverse manifest beside the DSN for later SES import.",
            json!({
                "type": "object",
                "properties": {
                    "board": {
                        "type": "string",
                        "description": "Path to the .kicad_pcb file currently open in KiCad PCB Editor"
                    },
                    "output": {
                        "type": "string",
                        "description": "New .dsn file path. Existing files are never replaced."
                    },
                    "manifest_output_path": {
                        "type": "string",
                        "description": "Optional new reverse-manifest JSON path. Defaults to <output>.konnect.json. Existing files are never replaced."
                    },
                    "native_bridge_mode": {
                        "type": "string",
                        "enum": ["prefer", "require", "disable"],
                        "default": "disable",
                        "description": "KiCad 10 native-export policy. Rust-only 'disable' is the default; 'prefer' explicitly opts into the enabled authenticated ActionPlugin bridge with the Rust exporter as fallback; 'require' refuses fallback."
                    }
                },
                "required": ["board", "output"]
            }),
            |args, ctx| async move { handle_export_specctra_dsn(args, ctx).await }
        ),
        tool!(
            "export_position_file",
            "Generate a component placement (pick-and-place) position file for SMT assembly. \
             KiCad automatically omits footprints marked exclude_from_pos_files.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Output position file path" },
                    "format": {
                        "type": "string",
                        "description": "File format: 'csv' (default) or 'gerber'",
                        "enum": ["csv", "gerber"],
                        "default": "csv"
                    },
                    "side": {
                        "type": "string",
                        "description": "Board side: 'front', 'back', or 'both'",
                        "enum": ["front", "back", "both"],
                        "default": "both"
                    },
                    "units": {
                        "type": "string",
                        "description": "Coordinate units for CSV: 'mm' (default) or 'in'. Gerber position output has format-defined units.",
                        "enum": ["mm", "in"],
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
            "Refill all copper pour zones on the board. KiCad IPC refills the complete board; per-zone selection is not available. Requires a running KiCad instance with IPC enabled.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
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

    let requested_layers = match optional_string_array(args, "layers") {
        Ok(layers) => layers,
        Err(error) => return Ok(error),
    };
    let layers = if requested_layers.is_empty() {
        let board_source = tokio::fs::read_to_string(&board).await?;
        standard_gerber_layers(&board_source)?
    } else {
        requested_layers
    };
    let layer_refs = layers.iter().map(String::as_str).collect::<Vec<_>>();

    // Ensure output dir exists
    tokio::fs::create_dir_all(&output_dir).await?;

    let cli = &ctx.config.kicad_cli;
    let mut verified_files = cli::export_gerber(cli, &board, &output_dir, &layer_refs).await?;

    if drill {
        // kicad-cli also has a dedicated drill export. Its --output is a
        // directory: the gerber directory itself, so the PTH and NPTH files
        // land beside the gerbers the way a fab expects them. The previous
        // `output_dir.join("drill.drl")` made KiCad create a *directory*
        // called drill.drl and bury the drill files inside it, where the
        // listing below never saw them.
        verified_files.extend(cli::export_drill(cli, &board, &output_dir).await?);
    }

    // Report only files that this export path verified as regular and
    // non-empty; never echo a directory listing containing stale artifacts.
    let mut files = verified_files
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    files.sort();

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output_dir": output_dir.to_str().unwrap_or(""),
            "layers": layers,
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
    let black_and_white = args["black_and_white"].as_bool().unwrap_or(false);

    let cli = &ctx.config.kicad_cli;
    cli::export_pdf(cli, &board, &output, &layer_refs, black_and_white).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output": output.to_str().unwrap_or(""),
            "black_and_white": black_and_white
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
    let black_and_white = args["black_and_white"].as_bool().unwrap_or(false);

    let cli = &ctx.config.kicad_cli;
    cli::export_svg_pcb(cli, &board, &output, &layer_refs, black_and_white).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output": output.to_str().unwrap_or(""),
            "black_and_white": black_and_white
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
    let include_unspecified = args["include_unspecified"].as_bool().unwrap_or(false);

    let cli = &ctx.config.kicad_cli;
    cli::export_3d(cli, &board, &output, format, include_unspecified).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "format": format,
            "include_unspecified": include_unspecified,
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
    if format != "csv" {
        let reason = format!("only 'csv' is supported, got '{format}'");
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "format".to_string(),
                reason: reason.clone(),
            },
            format!("Argument 'format' is invalid: {reason}"),
        ));
    }
    let options = cli::BomOptions {
        fields: args["fields"].as_str(),
        labels: args["labels"].as_str(),
        group_by: args["group_by"].as_str(),
        // The schema has advertised this default since the tool shipped; the
        // handler never read it, so DNP parts landed in every BOM.
        exclude_dnp: args["exclude_dnp"].as_bool().unwrap_or(true),
    };

    let cli = &ctx.config.kicad_cli;
    cli::export_bom(cli, &schematic, &output, &options).await?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "success": true,
            "output": output.to_str().unwrap_or(""),
            "format": format,
            "fields": options.fields,
            "exclude_dnp": options.exclude_dnp
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

fn extension_is(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn default_specctra_manifest_path(output_path: &Path) -> PathBuf {
    let mut name = output_path.as_os_str().to_os_string();
    name.push(".konnect.json");
    PathBuf::from(name)
}

fn existing_export_targets(paths: &[&Path]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| path.exists())
        .map(|path| path.display().to_string())
        .collect()
}

fn write_specctra_export_pair(
    output_path: &Path,
    dsn: &str,
    manifest_output_path: &Path,
    manifest: &str,
) -> anyhow::Result<()> {
    if let Some(parent) = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create DSN output directory {}", parent.display()))?;
    }
    if let Some(parent) = manifest_output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create manifest output directory {}", parent.display()))?;
    }

    // Both writes are create-new and individually atomic. If the second write
    // loses a race, remove only the exact DSN this call just created so callers
    // never receive half of a routing job.
    konnect_sexp::write_new_atomic(output_path, dsn)
        .with_context(|| format!("create DSN {}", output_path.display()))?;
    if let Err(error) = konnect_sexp::write_new_atomic(manifest_output_path, manifest) {
        let cleanup = std::fs::remove_file(output_path);
        if let Err(cleanup_error) = cleanup {
            return Err(anyhow::anyhow!(error)).context(format!(
                "create manifest {} failed and cleanup of newly-created DSN {} also failed: {cleanup_error}",
                manifest_output_path.display(),
                output_path.display()
            ));
        }
        return Err(anyhow::anyhow!(error))
            .with_context(|| format!("create manifest {}", manifest_output_path.display()));
    }
    Ok(())
}

fn resolve_native_bridge_mode(args: &serde_json::Value) -> &str {
    args["native_bridge_mode"].as_str().unwrap_or("disable")
}

async fn handle_export_specctra_dsn(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let output_path = get_path(args, "output")?;
    let manifest_output_path = args["manifest_output_path"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_specctra_manifest_path(&output_path));
    let native_bridge_mode = resolve_native_bridge_mode(args);

    if !extension_is(&board_path, "kicad_pcb") {
        return Ok(invalid_export_argument(
            "board",
            "must have the .kicad_pcb extension",
        ));
    }
    if !extension_is(&output_path, "dsn") {
        return Ok(invalid_export_argument(
            "output",
            "must have the .dsn extension",
        ));
    }
    if output_path == manifest_output_path {
        return Ok(invalid_export_argument(
            "manifest_output_path",
            "must not name the DSN output path",
        ));
    }
    if !matches!(native_bridge_mode, "prefer" | "require" | "disable") {
        return Ok(invalid_export_argument(
            "native_bridge_mode",
            "must be 'prefer', 'require', or 'disable'",
        ));
    }
    let conflicts = existing_export_targets(&[&output_path, &manifest_output_path]);
    if !conflicts.is_empty() {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::Conflict {
                paths: conflicts.clone(),
            },
            format!(
                "Specctra export is non-destructive; remove or choose new output paths: {}",
                conflicts.join(", ")
            ),
        ));
    }
    let board_path = board_path
        .canonicalize()
        .with_context(|| format!("resolve board {}", board_path.display()))?;
    let custom_rules_path = board_path.with_extension("kicad_dru");
    if custom_rules_path.is_file() {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::HandlerError {
                reason: format!(
                    "custom DRC rules are outside the first supported routing profile: {}",
                    custom_rules_path.display()
                ),
            },
            format!(
                "Specctra export refused: custom DRC rules are outside the first supported routing profile ({})",
                custom_rules_path.display()
            ),
        ));
    }
    let board_for_ipc = board_path.clone();
    let addr = ctx.config.ipc_address.clone();
    let export = with_ipc(addr, move |client| {
        let document = client.find_open_board(&board_for_ipc)?;
        let before = client.save_document_to_string_in(document.clone())?;
        let rules = client.get_effective_routing_rules_in(document.clone())?;
        let after = client.save_document_to_string_in(document)?;
        if before != after {
            anyhow::bail!(
                "KiCad board changed while routing rules were captured; retry from a stable editor revision"
            );
        }
        let export = crate::specctra::export_dsn(&board_for_ipc, &before, &rules)?;
        Ok((export, before))
    })
    .await?;
    let (baseline, source_snapshot) = match export {
        Ok(export) => export,
        Err(reason) => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: reason.clone(),
                },
                format!("Specctra export refused: {reason}"),
            ));
        }
    };
    // Preserve the method string shipped with the Rust exporter so existing
    // callers do not break when the optional native path is introduced.
    let mut method = "kicad_ipc_snapshot";
    let mut bridge_pid = None;
    let mut bridge_protocol_version = None;
    let mut bridge_diagnostics = Vec::new();
    let export = if native_bridge_mode == "disable" {
        baseline
    } else {
        let attempt = crate::native_specctra_bridge::try_export(&board_path).await;
        bridge_diagnostics = attempt.diagnostics;
        if let Some(native) = attempt.export {
            let board_for_stability = board_path.clone();
            let addr = ctx.config.ipc_address.clone();
            let stable = with_ipc(addr, move |client| {
                let document = client.find_open_board(&board_for_stability)?;
                client.save_document_to_string_in(document)
            })
            .await?;
            let stable = match stable {
                Ok(stable) => stable,
                Err(reason) => {
                    return Ok(CallToolResult::error_kind(
                        ToolErrorKind::HandlerError {
                            reason: reason.clone(),
                        },
                        format!("Specctra export refused after native bridge call: {reason}"),
                    ));
                }
            };
            if stable != source_snapshot {
                return Ok(CallToolResult::error_kind(
                    ToolErrorKind::Conflict {
                        paths: vec![board_path.display().to_string()],
                    },
                    "KiCad board changed during native Specctra export; retry from a stable editor revision",
                ));
            }
            match crate::specctra::adopt_native_dsn(baseline.clone(), native.dsn) {
                Ok(adopted) => {
                    method = "kicad10_native_actionplugin";
                    bridge_pid = Some(native.plugin_pid);
                    bridge_protocol_version = Some(native.protocol_version);
                    adopted
                }
                Err(error) if native_bridge_mode == "prefer" => {
                    bridge_diagnostics.push(format!(
                        "native DSN semantic validation failed; used Rust fallback: {error:#}"
                    ));
                    baseline
                }
                Err(error) => {
                    return Ok(CallToolResult::error(format!(
                        "KiCad native Specctra export was required but its semantic validation failed: {error:#}"
                    )));
                }
            }
        } else if native_bridge_mode == "require" {
            return Ok(CallToolResult::error(format!(
                "KiCad 10 native Specctra bridge was required but unavailable: {}",
                bridge_diagnostics.join("; ")
            )));
        } else {
            baseline
        }
    };

    let output_for_write = output_path.clone();
    let manifest_for_write = manifest_output_path.clone();
    let dsn_for_write = export.dsn.clone();
    let manifest_text_for_write = export.manifest.clone();
    task::spawn_blocking(move || {
        write_specctra_export_pair(
            &output_for_write,
            &dsn_for_write,
            &manifest_for_write,
            &manifest_text_for_write,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("Specctra output writer panicked: {error}"))??;

    Ok(CallToolResult::text(
        json!({
            "success": true,
            "method": method,
            "native_bridge_mode": native_bridge_mode,
            "native_bridge_pid": bridge_pid,
            "native_bridge_protocol_version": bridge_protocol_version,
            "native_bridge_diagnostics": bridge_diagnostics,
            "board": board_path,
            "output": output_path,
            "manifest_output_path": manifest_output_path,
            "source_sha256": export.source_sha256,
            "component_count": export.component_count,
            "pad_count": export.pad_count,
            "net_count": export.net_count,
            "routing_class_count": export.class_count,
            "capabilities": {
                "dsn_export_available": true,
                "ses_import_available": false,
                "freerouting_bridge_available": false,
                "source_revision_bound": true,
                "supported_profile": "two_layer_front_side_circle_rect_no_existing_routing_or_zones"
            }
        })
        .to_string(),
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

    if let Err((field, reason)) = validate_position_values(format, side, units) {
        return Ok(invalid_export_argument(field, reason));
    }

    let cli = &ctx.config.kicad_cli;
    cli::export_position_file(cli, &board, &output, format, units, side).await?;

    let mut result = json!({
        "success": true,
        "format": format,
        "side": side,
        "output": output.to_str().unwrap_or("")
    });
    if format != "gerber" {
        result["units"] = json!(units);
    }
    Ok(CallToolResult::text(
        serde_json::to_string(&result).unwrap(),
    ))
}

async fn handle_export_dxf(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output_dir = get_path(args, "output_dir")?;
    // Required here, unlike `export_pdf`/`export_svg` where it is genuinely
    // optional. An omitted `layers` became an empty vec, and `cli.rs` only
    // passes `--layers` when the list is non-empty — so the flag vanished from
    // the kicad-cli command line and kicad-cli applied its own default layer
    // set. Files appeared in `output_dir` and the tool reported success for an
    // export nobody specified (#218).
    let layers: Vec<String> = match require_array(args, "layers") {
        Ok(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Err(e) => return Ok(e),
    };
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
    let report = cli::run_drc(cli, &board, refill).await?;

    // Optionally write report
    if let Some(out_path) = args["output"].as_str() {
        let json = serde_json::to_string_pretty(&report)?;
        let path = std::path::Path::new(out_path);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("could not create report directory {}", parent.display())
            })?;
        }
        tokio::fs::write(path, json)
            .await
            .with_context(|| format!("could not write report to {}", path.display()))?;
    }

    let filtered: Vec<_> = report
        .all()
        .filter(|v| severity_rank(&v.severity) >= min_rank)
        .collect();

    let summary = json!({
        "total": report.all().count(),
        "design_rule_violations": report.violations.len(),
        "unconnected_items": report.unconnected_items.as_ref().map(Vec::len),
        "schematic_parity": report.schematic_parity.as_ref().map(Vec::len),
        "categories_not_reported": report.missing_categories(),
        "filtered_count": filtered.len(),
        "severity_filter": severity_filter,
        "violations": filtered.iter().map(|v| json!({
            "severity": v.severity,
            "rule": v.rule,
            "description": v.description,
            "pos": v.pos.as_ref().map(|p| json!({ "x": p.x, "y": p.y })),
            "items": v.items
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
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    #[test]
    fn specctra_manifest_defaults_beside_dsn() {
        assert_eq!(
            default_specctra_manifest_path(Path::new("build/clock.dsn")),
            PathBuf::from("build/clock.dsn.konnect.json")
        );
    }

    #[test]
    fn rust_specctra_export_is_the_default_path() {
        assert_eq!(resolve_native_bridge_mode(&json!({})), "disable");
        assert_eq!(
            resolve_native_bridge_mode(&json!({ "native_bridge_mode": "prefer" })),
            "prefer"
        );
    }

    #[test]
    fn specctra_pair_writer_removes_partial_dsn_if_manifest_creation_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dsn = dir.path().join("board.dsn");
        let manifest = dir.path().join("board.dsn.konnect.json");
        std::fs::write(&manifest, "owned by another process").expect("seed conflict");

        let error = write_specctra_export_pair(&dsn, "(pcb board)", &manifest, "{}")
            .expect_err("existing manifest must be preserved");

        assert!(error.to_string().contains("create manifest"));
        assert!(!dsn.exists(), "a failed pair must not leave a DSN behind");
        assert_eq!(
            std::fs::read_to_string(manifest).unwrap(),
            "owned by another process"
        );
    }

    #[tokio::test]
    async fn specctra_export_reports_existing_output_as_structured_conflict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board = dir.path().join("board.kicad_pcb");
        let output = dir.path().join("board.dsn");
        std::fs::write(&board, "(kicad_pcb)").expect("board fixture");
        std::fs::write(&output, "do not replace").expect("seed conflict");

        let result =
            handle_export_specctra_dsn(&json!({ "board": board, "output": output }), &test_ctx())
                .await
                .expect("conflict is an MCP result");

        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("conflict")
        );
        assert_eq!(std::fs::read_to_string(output).unwrap(), "do not replace");
    }

    #[tokio::test]
    async fn specctra_export_refuses_custom_rule_file_before_ipc() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board = dir.path().join("board.kicad_pcb");
        let rules = dir.path().join("board.kicad_dru");
        let output = dir.path().join("board.dsn");
        std::fs::write(&board, "(kicad_pcb)").expect("board fixture");
        std::fs::write(&rules, "(rule custom)").expect("custom rules fixture");

        let result =
            handle_export_specctra_dsn(&json!({ "board": board, "output": output }), &test_ctx())
                .await
                .expect("unsupported profile is an MCP result");

        assert!(result.is_error);
        assert!(!output.exists());
        let text = match result.content.first().unwrap() {
            crate::mcp::protocol::ToolContent::Text { text } => text,
            _ => panic!("expected text result"),
        };
        assert!(text.contains("custom DRC rules"), "{text}");
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

    #[tokio::test]
    async fn export_bom_rejects_a_format_kicad_cannot_produce() {
        let ctx = test_ctx();
        let result = handle_export_bom(
            &json!({
                "schematic": "/tmp/design.kicad_sch",
                "output": "/tmp/bom.xlsx",
                "format": "xlsx"
            }),
            &ctx,
        )
        .await
        .expect("validation does not spawn kicad-cli");

        assert!(result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text error, got {other:?}"),
        };
        let error: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(error["error"]["field"], "format");
    }
}

/// `export_dxf` declares `layers` required and defaulted it to an empty list.
/// `cli.rs` only passes `--layers` when the list is non-empty, so the flag
/// vanished from the kicad-cli command line entirely and kicad-cli applied its
/// own default layer set — a different export than the one asked for, reported
/// as success (#218).
///
/// `export_pdf` and `export_svg` share the same reading code but declare
/// `layers` optional, and must keep accepting its absence.
#[cfg(test)]
mod required_layers_tests {
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

    fn schema_requires(tool_name: &str, key: &str) -> bool {
        tools()
            .into_iter()
            .find(|t| t.name == tool_name)
            .unwrap_or_else(|| panic!("{tool_name} is registered"))
            .input_schema["required"]
            .as_array()
            .map(|r| r.iter().any(|v| v.as_str() == Some(key)))
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn export_dxf_refuses_a_missing_layers_argument() {
        assert!(schema_requires("export_dxf", "layers"));
        let dir = tempfile::tempdir().unwrap();
        let def = tools()
            .into_iter()
            .find(|t| t.name == "export_dxf")
            .expect("registered");
        let result = (def.handler)(
            &json!({
                "board": dir.path().join("b.kicad_pcb").display().to_string(),
                "output_dir": dir.path().join("out").display().to_string(),
            }),
            ctx(),
        )
        .await
        .expect("no anyhow");

        assert!(result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(parsed["error"]["kind"], "invalid_argument");
        assert_eq!(parsed["error"]["field"], "layers");
        assert!(
            !dir.path().join("out").exists(),
            "a refused export must not create its output directory"
        );
    }

    /// The neighbouring exporters read `layers` the same way but do not require
    /// it. Requiring it there would break every caller that omits it today.
    #[test]
    fn export_pdf_and_svg_keep_layers_optional() {
        for tool_name in ["export_pdf", "export_svg", "export_gerber"] {
            assert!(
                !schema_requires(tool_name, "layers"),
                "{tool_name} must keep `layers` optional"
            );
        }
    }
}

#[cfg(test)]
mod fabrication_option_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use serde_json::json;
    use std::sync::Arc;

    fn ctx() -> ToolContext {
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
    fn default_gerbers_include_only_manufacturing_layers() {
        // KiCad 9 output from the IPC test fixture, including the real layer
        // table order and every documentation-only layer this filter excludes.
        let board = include_str!("../../../konnect-ipc/tests/fixtures/live_ipc.kicad_pcb");
        assert_eq!(
            standard_gerber_layers(board).unwrap(),
            [
                "F.Cu",
                "B.Cu",
                "F.SilkS",
                "B.SilkS",
                "F.Mask",
                "B.Mask",
                "Edge.Cuts"
            ]
        );
    }

    #[test]
    fn public_schemas_describe_the_options_that_reach_kicad() {
        let gerber = tools()
            .into_iter()
            .find(|tool| tool.name == "export_gerber")
            .unwrap();
        assert!(gerber.input_schema["properties"]["layers"]["description"]
            .as_str()
            .unwrap()
            .contains("auto-select"));

        let position = tools()
            .into_iter()
            .find(|tool| tool.name == "export_position_file")
            .unwrap();
        assert_eq!(
            position.input_schema["properties"]["units"]["enum"],
            json!(["mm", "in"])
        );
        assert_eq!(
            position.input_schema["properties"]["side"]["enum"],
            json!(["front", "back", "both"])
        );
        assert!(position.description.contains("exclude_from_pos_files"));
    }

    #[tokio::test]
    async fn impossible_gerber_side_is_rejected_before_running_kicad() {
        let result = handle_export_position_file(
            &json!({
                "board": "/tmp/board.kicad_pcb",
                "output": "/tmp/positions.gbr",
                "format": "gerber",
                "side": "both",
                "units": "mm"
            }),
            &ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text error, got {other:?}"),
        };
        let error: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(error["error"]["field"], "side");
    }

    #[test]
    fn malformed_layer_items_name_their_index() {
        let error = optional_string_array(&json!({ "layers": ["F.Cu", 7] }), "layers")
            .expect_err("numeric layer must be rejected");
        let text = match error.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text error, got {other:?}"),
        };
        let error: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(error["error"]["field"], "layers[1]");
    }
}
