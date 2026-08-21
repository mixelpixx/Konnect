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
        extract_all_net_labels, extract_labels, extract_lib_pins, extract_symbol_instances,
        extract_wires, find_lib_symbol, pin_endpoint, read_schematic,
    },
    writer::{
        apply_edits, find_block_with_leading_whitespace, write_atomic_if_unchanged, SexpEdit,
    },
};
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
                    "theme": { "type": "string", "description": "KiCad colour theme name (optional)" }
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
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut g = build_net_graph(
        &wires,
        &labels,
        &konnect_sexp::schematic::extract_junctions(&tree),
    );

    // Collect distinct net names
    let mut net_names: Vec<String> = labels.iter().map(|l| l.net.clone()).collect();
    net_names.sort();
    net_names.dedup();

    // Build per-component net map
    let components: Vec<serde_json::Value> = instances
        .iter()
        .map(|inst| {
            let lib_sym = find_lib_symbol(&lib_syms, inst);

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

    if let Some(root) = owning_project_root(&sch_path) {
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
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Collect all valid snap targets: pin endpoints + label positions + wire endpoints
    let mut snap_targets: Vec<(f64, f64)> = Vec::new();

    for inst in &instances {
        let lib_sym = find_lib_symbol(&lib_syms, inst);
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

#[cfg(test)]
mod netlist_summary_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::io::Write;
    use std::sync::Arc;

    /// R1 with pin 2 wired to a `power:GND` symbol, and pin 1 on a plain label.
    /// The rail is the case that used to disappear.
    const SCH: &str = include_str!("../../tests/fixtures/power_rail.kicad_sch");

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
fn owning_project_root(file: &Path) -> Option<PathBuf> {
    if file.with_extension("kicad_pro").is_file() {
        return None;
    }
    let root = project_root_schematic(&crate::tools::library::project_root_for(file)?)?;
    if same_file(&root, file) {
        return None;
    }
    let mut visited = HashSet::new();
    sheet_tree_contains(&root, file, 0, &mut visited).then_some(root)
}

/// The `<stem>.kicad_sch` beside the single `.kicad_pro` in `dir`. A directory
/// holding more than one project says nothing definite about which root a loose
/// sheet belongs to, so it yields nothing rather than a guess.
fn project_root_schematic(dir: &Path) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "kicad_pro") {
            if found.is_some() {
                return None;
            }
            found = Some(path);
        }
    }
    let sch = found?.with_extension("kicad_sch");
    sch.is_file().then_some(sch)
}

/// Whether `target` is reachable as a sheet from `root`. Depth and visited set
/// guard the same way [`crate::tools::sch_hierarchy::build_hierarchy_node`]
/// does: a sheet may reference a file that references it back.
fn sheet_tree_contains(
    root: &Path,
    target: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
) -> bool {
    if depth > crate::tools::sch_hierarchy::MAX_HIERARCHY_DEPTH {
        return false;
    }
    let canon = canonical(root);
    if !visited.insert(canon) {
        return false;
    }
    let Ok(sch) = konnect_schematic_editor::Schematic::load(root) else {
        return false;
    };
    let dir = root.parent().unwrap_or_else(|| Path::new("."));
    sch.sheets.iter().any(|sheet| {
        let child = dir.join(sheet.file());
        same_file(&child, target) || sheet_tree_contains(&child, target, depth + 1, visited)
    })
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

        assert_eq!(owning_project_root(&child), Some(root));
    }

    #[test]
    fn a_root_of_its_own_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "proj.kicad_pro", "{}");
        let root = root_with_child(tmp.path(), "proj.kicad_sch", "child.kicad_sch");
        blank(tmp.path(), "child.kicad_sch");

        assert_eq!(owning_project_root(&root), None);
    }

    #[test]
    fn a_sheet_belonging_to_no_project_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        let loose = blank(tmp.path(), "loose.kicad_sch");

        assert_eq!(owning_project_root(&loose), None);
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

        assert_eq!(owning_project_root(&stranger), None);
    }

    /// A sheet cycle must not hang the walk.
    #[test]
    fn a_reference_cycle_terminates() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "proj.kicad_pro", "{}");
        root_with_child(tmp.path(), "proj.kicad_sch", "a.kicad_sch");
        root_with_child(tmp.path(), "a.kicad_sch", "proj.kicad_sch");
        let stranger = blank(tmp.path(), "stranger.kicad_sch");

        assert_eq!(owning_project_root(&stranger), None);
    }
}
