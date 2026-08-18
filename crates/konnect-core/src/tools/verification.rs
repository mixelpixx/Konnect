//! `verification` toolset — DRC, design rules, KiCAD UI management, routing utilities.
//!
//! DRC delegates to `kicad-cli`. Design rules are read/written as S-expressions.
//! KiCAD UI management uses process inspection + subprocess spawning.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use anyhow::Context;
use konnect_sexp::writer::write_atomic;
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::task;

use super::cli;

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
            "Set board-level design rules (clearance, trace width, via size) in the sibling KiCAD project file.",
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
            "check_kicad_ui",
            "Check whether the KiCad GUI application is running and whether IPC responds within a bounded timeout.",
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
            "Copy a routing pattern (traces and vias) from one region of the board to another.",
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
            "Check the physical clearance (distance) between two components on the PCB.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "ref1":  { "type": "string", "description": "First component reference (e.g. 'U1')" },
                    "ref2":  { "type": "string", "description": "Second component reference (e.g. 'C1')" }
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

    let refill = args["refill_zones"].as_bool().unwrap_or(false);
    let report = cli::run_drc(&ctx.config.kicad_cli, &board, refill).await?;

    // Optionally write report
    if let Some(out_path) = args["output"].as_str() {
        let json = serde_json::to_string_pretty(&report)?;
        write_report(out_path, &json).await?;
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
            "design_rule_violations": report.violations.len(),
            // Null, not zero, when this kicad-cli did not report the category:
            // "none found" and "never asked" are different answers.
            "unconnected_items": report.unconnected_items.as_ref().map(Vec::len),
            "schematic_parity": report.schematic_parity.as_ref().map(Vec::len),
            "categories_not_reported": missing,
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
                "pos": v.pos.as_ref().map(|p| json!({ "x": p.x, "y": p.y }))
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
    let path = Path::new(out_path);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("could not create report directory {}", parent.display()))?;
    }
    tokio::fs::write(path, contents)
        .await
        .with_context(|| format!("could not write report to {}", path.display()))?;
    Ok(())
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
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
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

// ─── KiCAD UI management ──────────────────────────────────────────────────────

/// Check if the KiCAD GUI is running by scanning the process list.
fn is_kicad_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        // On Windows, use `tasklist` to check
        std::process::Command::new("tasklist")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("kicad.exe"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("pgrep")
            .arg("-x")
            .arg("kicad")
            .output()
            .ok()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Resolve the KiCAD binary path from config or well-known locations.
fn find_kicad_binary(config_binary: &str) -> String {
    if !config_binary.is_empty() && std::path::Path::new(config_binary).exists() {
        return config_binary.to_string();
    }
    #[cfg(target_os = "windows")]
    {
        // Scan common install roots and KiCAD version directories
        let roots = [
            r"C:\Program Files\KiCad",
            r"C:\KiCad",
            r"D:\KiCad",
            r"D:\Program Files\KiCad",
        ];
        let versions = ["10.0", "9.0", "8.0"];
        for root in &roots {
            for ver in &versions {
                let path = format!(r"{}\{}\bin\kicad.exe", root, ver);
                if std::path::Path::new(&path).exists() {
                    return path;
                }
            }
            // Also check without version subdir
            let path = format!(r"{}\bin\kicad.exe", root);
            if std::path::Path::new(&path).exists() {
                return path;
            }
        }
        "kicad".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad".to_string()
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        "kicad".to_string()
    }
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
        let running = task::spawn_blocking(is_kicad_running).await?;
        if !running {
            return Ok::<_, tokio::task::JoinError>((false, false));
        }
        let ipc_responsive = task::spawn_blocking(move || {
            konnect_ipc::client::KiCadIpcClient::new(&addr)
                .ping()
                .unwrap_or(false)
        })
        .await?;
        Ok((true, ipc_responsive))
    };

    match bounded_health_check(std::time::Duration::from_secs(timeout_seconds), check).await {
        Ok(result) => {
            let (running, ipc_responsive) = result?;
            Ok(CallToolResult::json(&json!({
                "running": running,
                "ipc_responsive": ipc_responsive,
                "timed_out": false,
                "timeout_seconds": timeout_seconds,
                "elapsed_ms": started.elapsed().as_millis() as u64
            })))
        }
        Err(_) => Ok(CallToolResult::json(&json!({
            "running": null,
            "ipc_responsive": false,
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
    let binary = find_kicad_binary(&ctx.config.kicad_binary);

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
    _ctx: &ToolContext,
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

    let dx = dest_x - src_x1;
    let dy = dest_y - src_y1;

    let net_map: std::collections::HashMap<String, String> =
        if let Some(obj) = args["net_map"].as_object() {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        } else {
            std::collections::HashMap::new()
        };

    let content = tokio::fs::read_to_string(&board).await?;
    let mut new_tracks = Vec::new();

    // Find all (segment ...) and (via ...) blocks within the bounding box
    // and collect translated copies.
    for (block_start, block_end, _block_type) in find_routing_blocks(&content) {
        let block = &content[block_start..block_end];
        if let Some((bx, by)) = extract_start_xy(block) {
            if bx >= src_x1 && bx <= src_x2 && by >= src_y1 && by <= src_y2 {
                let translated = translate_block(block, dx, dy, &net_map);
                new_tracks.push(translated);
            }
        }
    }

    if new_tracks.is_empty() {
        return Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "copied": 0,
                "note": "No routing elements found in the specified source region"
            }))
            .unwrap(),
        ));
    }

    // Insert all new blocks before the final `)` of the file
    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let insertion = new_tracks.join("\n");
    let new_content = format!(
        "{}\n{}\n{}",
        &content[..insert_pos],
        insertion,
        &content[insert_pos..]
    );

    // Assign new UUIDs to inserted blocks (replace uuid "ORIGINAL" with new ones)
    let new_content = reassign_uuids(&new_content, insert_pos);

    write_atomic(&board, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "copied": new_tracks.len(),
            "dx": dx,
            "dy": dy
        }))
        .unwrap(),
    ))
}

/// Find all `(segment ...)` and `(via ...)` blocks in the PCB content.
/// Returns (start, end, type) tuples.
fn find_routing_blocks(content: &str) -> Vec<(usize, usize, &'static str)> {
    let mut results = Vec::new();
    for (prefix, kind) in &[("\n  (segment ", "segment"), ("\n  (via ", "via")] {
        let mut pos = 0;
        while let Some(found) = content[pos..].find(prefix) {
            let start = pos + found + 3; // skip \n
            let mut depth = 0i32;
            let mut end = start;
            for (i, ch) in content[start..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = start + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            results.push((start, end, *kind));
            pos = start + 1;
        }
    }
    results
}

/// Extract the `(start X Y)` coordinates from a routing block.
fn extract_start_xy(block: &str) -> Option<(f64, f64)> {
    let pat = "(start ";
    let pos = block.find(pat)?;
    let after = &block[pos + pat.len()..];
    let end = after.find(')')?;
    let parts: Vec<&str> = after[..end].split_whitespace().collect();
    let x = parts.first()?.parse::<f64>().ok()?;
    let y = parts.get(1)?.parse::<f64>().ok()?;
    Some((x, y))
}

/// Translate all coordinate pairs in a routing block by (dx, dy).
fn translate_block(
    block: &str,
    dx: f64,
    dy: f64,
    net_map: &std::collections::HashMap<String, String>,
) -> String {
    let mut result = block.to_string();

    // Translate (start X Y), (end X Y), (at X Y) coordinate pairs
    for coord_key in &["start", "end", "at"] {
        let pat = format!("({} ", coord_key);
        let mut new_result = String::new();
        let mut remaining = result.as_str();
        while let Some(pos) = remaining.find(&pat) {
            new_result.push_str(&remaining[..pos]);
            new_result.push_str(&pat);
            let after = &remaining[pos + pat.len()..];
            if let Some(close) = after.find(')') {
                let coords_str = &after[..close];
                let parts: Vec<&str> = coords_str.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let (Ok(x), Ok(y)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                        new_result.push_str(&format!("{} {}", x + dx, y + dy));
                        if parts.len() > 2 {
                            new_result.push(' ');
                            new_result.push_str(&parts[2..].join(" "));
                        }
                        new_result.push(')');
                        remaining = &remaining[pos + pat.len() + close + 1..];
                        continue;
                    }
                }
                // Fall through if parsing failed
                new_result.push_str(coords_str);
                new_result.push(')');
                remaining = &remaining[pos + pat.len() + close + 1..];
            } else {
                break;
            }
        }
        new_result.push_str(remaining);
        result = new_result;
    }

    // Remap net names
    for (old_net, new_net) in net_map {
        let old_pat = format!("(net \"{}\")", old_net);
        let new_pat = format!("(net \"{}\")", new_net);
        result = result.replace(&old_pat, &new_pat);
        // Also handle numeric net references if needed (not replaced here)
    }

    result
}

/// Reassign UUIDs in all newly inserted blocks (those after `insert_boundary`).
fn reassign_uuids(content: &str, insert_boundary: usize) -> String {
    let mut result = String::with_capacity(content.len() + 64);
    result.push_str(&content[..insert_boundary]);
    let tail = &content[insert_boundary..];
    let mut remaining = tail;
    while let Some(pos) = remaining.find("(uuid \"") {
        result.push_str(&remaining[..pos]);
        result.push_str("(uuid \"");
        // Find end of UUID string
        let after = &remaining[pos + 7..];
        if let Some(end) = after.find('"') {
            let new_uuid = uuid::Uuid::new_v4().to_string();
            result.push_str(&new_uuid);
            result.push('"');
            remaining = &remaining[pos + 7 + end + 1..];
        } else {
            break;
        }
    }
    result.push_str(remaining);
    result
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
    _ctx: &ToolContext,
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

    let content = std::fs::read_to_string(&board)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    let pos1 = find_footprint_position(&tree, &ref1)?;
    let pos2 = find_footprint_position(&tree, &ref2)?;

    let dx = pos2.0 - pos1.0;
    let dy = pos2.1 - pos1.1;
    let distance = (dx * dx + dy * dy).sqrt();

    Ok(CallToolResult::json(&json!({
        "ref1": ref1,
        "ref2": ref2,
        "pos1": { "x": pos1.0, "y": pos1.1 },
        "pos2": { "x": pos2.0, "y": pos2.1 },
        "distance_mm": (distance * 1000.0).round() / 1000.0
    })))
}

/// Look up the board-space (x, y) position of a footprint by its reference designator.
fn find_footprint_position(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
) -> anyhow::Result<(f64, f64)> {
    let fp_node = tree
        .find_all("footprint")
        .into_iter()
        .find(|fp| {
            fp.find_all("property").iter().any(|p| {
                p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                    && p.get(2).and_then(|n| n.as_str()) == Some(reference)
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found on board", reference))?;

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);

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
