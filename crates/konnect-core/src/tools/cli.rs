//! kicad-cli subprocess wrapper for KiCAD 10.
//!
//! All exports, ERC, DRC, and annotation operations shell out to kicad-cli.
//! This module provides a typed interface to those commands.
//!
//! VERIFIED against: kicad-cli from KiCAD 10.0 (C:\Program Files\KiCad\10.0\bin\kicad-cli.exe)
//! Commands validated: sch erc, sch export (bom/netlist/pdf/svg), pcb drc,
//!   pcb export (gerbers/drill/pdf/svg/step/vrml/pos/ipcd356/dxf/gencad/ipc2581/odb),
//!   pcb render

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// Extended timeout for long operations (export, ERC, DRC).
const LONG_TIMEOUT: Duration = Duration::from_secs(600);

// ─── Result Types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErcViolation {
    pub severity: String,
    pub description: String,
    pub sheet: Option<String>,
    pub pos: Option<ErcPos>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErcPos {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrcViolation {
    pub severity: String,
    pub description: String,
    /// KiCad's rule key (`"silk_edge_clearance"`, `"clearance"`, …). This is
    /// what a caller needs to fix or waive the rule; the prose description
    /// alone is not addressable.
    pub rule: String,
    /// Where to look. KiCad reports one position per *involved item*, not one
    /// per violation, so this is the first item's — which is what the report
    /// used to try to read from a top-level `pos` field that does not exist,
    /// making every position `null`.
    pub pos: Option<ErcPos>,
}

/// Everything `kicad-cli pcb drc` reports, not just the part Konnect used to
/// read.
///
/// The JSON carries three sibling arrays. Konnect took `violations` and
/// dropped the other two, so a board with an unrouted net — which is what
/// `unconnected_items` is for — came back clean from every tool that gates on
/// DRC (#245).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrcReport {
    pub violations: Vec<DrcViolation>,
    /// `None` means this kicad-cli did not report the category at all, which
    /// is not the same as "there are none" and must not be rendered as zero.
    pub unconnected_items: Option<Vec<DrcViolation>>,
    pub schematic_parity: Option<Vec<DrcViolation>>,
}

impl DrcReport {
    /// Findings across every category, for a caller that just wants to know
    /// whether the board is clean.
    pub fn all(&self) -> impl Iterator<Item = &DrcViolation> {
        self.violations
            .iter()
            .chain(self.unconnected_items.iter().flatten())
            .chain(self.schematic_parity.iter().flatten())
    }

    pub fn error_count(&self) -> usize {
        self.all().filter(|v| v.severity == "error").count()
    }

    /// Categories this kicad-cli did not report, by name. A gate that wants to
    /// fail closed needs to know its evidence was incomplete.
    pub fn missing_categories(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.unconnected_items.is_none() {
            missing.push("unconnected_items");
        }
        if self.schematic_parity.is_none() {
            missing.push("schematic_parity");
        }
        missing
    }
}

// ─── KiCAD CLI Runner ─────────────────────────────────────────────────────────

/// Run a kicad-cli command with arguments and capture stdout.
async fn run_cli(cli: &str, args: &[&str], timeout_dur: Duration) -> Result<String> {
    info!("[BETA] kicad-cli {} {}", cli, args.join(" "));

    let mut cmd = Command::new(cli);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn kicad-cli: {}", cli))?;

    let output = timeout(timeout_dur, child.wait_with_output())
        .await
        .with_context(|| format!("kicad-cli timed out after {:?}", timeout_dur))?
        .with_context(|| "kicad-cli process failed")?;

    if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            if line.contains("Error") || line.contains("error") {
                warn!("[BETA] kicad-cli: {}", line);
            } else {
                debug!("[BETA] kicad-cli stderr: {}", line);
            }
        }
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "kicad-cli exited with {}: {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ─── ERC ─────────────────────────────────────────────────────────────────────

/// Run ERC on a schematic and return parsed violations.
/// KiCAD 10: `sch erc --output <path> --format json <input>`
pub async fn run_erc(cli: &str, schematic: &Path) -> Result<Vec<ErcViolation>> {
    let out_path = schematic.with_extension("erc.json");
    let args = [
        "sch",
        "erc",
        "--output",
        out_path.to_str().unwrap(),
        "--format",
        "json",
        schematic.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;

    let json_str = tokio::fs::read_to_string(&out_path)
        .await
        .context("ERC output file not found")?;
    let raw: serde_json::Value = serde_json::from_str(&json_str)?;

    let violations = parse_erc_json(&raw);
    let _ = tokio::fs::remove_file(&out_path).await;
    Ok(violations)
}

fn parse_erc_json(raw: &serde_json::Value) -> Vec<ErcViolation> {
    // KiCAD's ERC report (https://schemas.kicad.org/erc.v1.json) nests
    // violations per sheet — { "sheets": [ { "path": …, "violations": […] } ] }
    // — with positions on the affected items. There is no top-level
    // "violations" key (that's the DRC report's shape), so reading one here
    // silently returned zero violations for every schematic.
    let Some(sheets) = raw.get("sheets").and_then(|s| s.as_array()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for sheet in sheets {
        let sheet_path = sheet.get("path").and_then(|p| p.as_str()).map(String::from);
        let Some(violations) = sheet.get("violations").and_then(|v| v.as_array()) else {
            continue;
        };
        for v in violations {
            let first_item = v
                .get("items")
                .and_then(|i| i.as_array())
                .and_then(|i| i.first());
            let mut description = v["description"].as_str().unwrap_or("").to_string();
            // The per-item description names the offender ("Symbol R1 Pin 1…")
            // — without it "Pin not connected" is unactionable.
            if let Some(detail) = first_item
                .and_then(|item| item.get("description"))
                .and_then(|d| d.as_str())
            {
                if !detail.is_empty() {
                    description = format!("{}: {}", description, detail);
                }
            }
            out.push(ErcViolation {
                severity: v["severity"].as_str().unwrap_or("error").to_string(),
                description,
                sheet: sheet_path.clone(),
                pos: first_item.and_then(|item| item.get("pos")).and_then(|p| {
                    Some(ErcPos {
                        x: p["x"].as_f64()?,
                        y: p["y"].as_f64()?,
                    })
                }),
            });
        }
    }
    out
}

// ─── DRC ─────────────────────────────────────────────────────────────────────

/// Run DRC on a PCB and return parsed violations.
/// KiCAD 10: `pcb drc --output <path> --format json [--refill-zones] <input>`
pub async fn run_drc(cli: &str, pcb: &Path, refill_zones: bool) -> Result<DrcReport> {
    let out_path = pcb.with_extension("drc.json");
    let mut args = vec![
        "pcb",
        "drc",
        "--output",
        out_path.to_str().unwrap(),
        "--format",
        "json",
    ];
    if refill_zones {
        args.push("--refill-zones");
    }
    args.push(pcb.to_str().unwrap());
    run_cli(cli, &args, LONG_TIMEOUT).await?;

    let json_str = tokio::fs::read_to_string(&out_path)
        .await
        .context("DRC output file not found")?;
    let raw: serde_json::Value = serde_json::from_str(&json_str)?;
    let _ = tokio::fs::remove_file(&out_path).await;

    parse_drc_report(&raw)
}

/// Split out so it can be tested against a real `kicad-cli` report without
/// running kicad-cli.
fn parse_drc_report(raw: &serde_json::Value) -> Result<DrcReport> {
    fn category(raw: &serde_json::Value, key: &str) -> Option<Vec<DrcViolation>> {
        Some(
            raw.get(key)?
                .as_array()?
                .iter()
                .map(|v| DrcViolation {
                    severity: v["severity"].as_str().unwrap_or("error").to_string(),
                    description: v["description"].as_str().unwrap_or("").to_string(),
                    rule: v["type"].as_str().unwrap_or("").to_string(),
                    // The position lives on each involved item; a violation has
                    // no `pos` of its own.
                    pos: v["items"].as_array().and_then(|items| {
                        items.iter().find_map(|item| {
                            let p = item.get("pos")?;
                            Some(ErcPos {
                                x: p["x"].as_f64()?,
                                y: p["y"].as_f64()?,
                            })
                        })
                    }),
                })
                .collect(),
        )
    }

    Ok(DrcReport {
        // A report without this key is not a DRC report. Defaulting it to an
        // empty list would render as a clean board, which is the failure mode
        // this whole change exists to remove.
        violations: category(raw, "violations")
            .context("DRC report has no 'violations' array; kicad-cli did not produce a report")?,
        unconnected_items: category(raw, "unconnected_items"),
        schematic_parity: category(raw, "schematic_parity"),
    })
}

// ─── Annotation ───────────────────────────────────────────────────────────────

/// KiCAD 10: `sch annotate` is NOT in the CLI.
/// We implement annotation ourselves by parsing the schematic and assigning
/// sequential reference designators to unannotated symbols (those with "?" suffix).
pub async fn annotate_schematic(_cli: &str, schematic: &Path) -> Result<()> {
    use std::collections::HashMap;

    let read_path = schematic.to_path_buf();
    let content =
        tokio::task::spawn_blocking(move || konnect_sexp::read_consistent(&read_path)).await??;
    let mut new_content = content.clone();
    let mut counters: HashMap<String, usize> = HashMap::new();

    // First pass: find all existing numbered references to avoid conflicts
    let mut pos = 0;
    while let Some(ref_pos) = new_content[pos..].find("(reference \"") {
        let abs = pos + ref_pos + 12;
        if let Some(end) = new_content[abs..].find('"') {
            let reference = &new_content[abs..abs + end];
            // Extract prefix and number: "R1" → ("R", 1)
            let prefix: String = reference
                .chars()
                .take_while(|c| c.is_alphabetic() || *c == '#')
                .collect();
            let num_str: String = reference.chars().skip(prefix.len()).collect();
            if let Ok(num) = num_str.parse::<usize>() {
                let counter = counters.entry(prefix).or_insert(0);
                if num >= *counter {
                    *counter = num + 1;
                }
            }
        }
        pos = abs + 1;
    }

    // Second pass: replace "?" references with sequential numbers
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    pos = 0;
    while let Some(ref_pos) = new_content[pos..].find("(reference \"") {
        let abs = pos + ref_pos + 12;
        if let Some(end) = new_content[abs..].find('"') {
            let reference = &new_content[abs..abs + end];
            if reference.ends_with('?') {
                let prefix = reference.trim_end_matches('?').to_string();
                let counter = counters.entry(prefix.clone()).or_insert(1);
                let new_ref = format!("{}{}", prefix, counter);
                *counter += 1;
                replacements.push((abs, abs + end, new_ref));
            }
        }
        pos = abs + 1;
    }

    // Apply replacements in reverse order to preserve offsets
    for (start, end, new_ref) in replacements.into_iter().rev() {
        new_content.replace_range(start..end, &new_ref);
    }

    if new_content != content {
        let write_path = schematic.to_path_buf();
        tokio::task::spawn_blocking(move || {
            konnect_sexp::write_atomic_if_unchanged(&write_path, &content, &new_content)
        })
        .await??;
    }

    Ok(())
}

// ─── Schematic Export ────────────────────────────────────────────────────────

/// KiCAD 10: `sch export svg --output <dir> <input>`
pub async fn export_schematic_svg(
    cli: &str,
    schematic: &Path,
    output_dir: &Path,
) -> Result<PathBuf> {
    let args = [
        "sch",
        "export",
        "svg",
        "--output",
        output_dir.to_str().unwrap(),
        schematic.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    let stem = schematic.file_stem().unwrap_or_default().to_string_lossy();
    Ok(output_dir.join(format!("{}.svg", stem)))
}

/// KiCAD 10: `sch export pdf --output <path> <input>`
pub async fn export_schematic_pdf(cli: &str, schematic: &Path, output: &Path) -> Result<()> {
    let args = [
        "sch",
        "export",
        "pdf",
        "--output",
        output.to_str().unwrap(),
        schematic.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// Column and filtering options for `sch export bom`.
///
/// All-`None`/`false` reproduces kicad-cli's own defaults: the fixed
/// `Reference,Value,Footprint,QUANTITY,DNP` column set, ungrouped, DNP rows
/// included.
#[derive(Debug, Default, Clone)]
pub struct BomOptions<'a> {
    /// Ordered field list, e.g. `Reference,Value,Footprint,MPN,${QUANTITY}`.
    /// Any schematic field name works, which is how MPN/LCSC columns reach the
    /// fab; generated fields (`QUANTITY`, `DNP`, `ITEM_NUMBER`, …) may be
    /// written with or without `${}`.
    pub fields: Option<&'a str>,
    /// Ordered column headings. When omitted KiCad labels each column with its
    /// field name.
    pub labels: Option<&'a str>,
    /// Fields whose matching references collapse into one row, e.g.
    /// `Value,Footprint`.
    pub group_by: Option<&'a str>,
    /// Drop Do-Not-Populate symbols.
    pub exclude_dnp: bool,
}

/// Argument vector for the BOM export, factored out so the flags can be
/// asserted without a kicad-cli on the machine.
fn bom_args<'a>(output: &'a str, schematic: &'a str, options: &BomOptions<'a>) -> Vec<&'a str> {
    let mut args = vec!["sch", "export", "bom", "--output", output];
    if let Some(fields) = options.fields {
        args.push("--fields");
        args.push(fields);
    }
    if let Some(labels) = options.labels {
        args.push("--labels");
        args.push(labels);
    }
    if let Some(group_by) = options.group_by {
        args.push("--group-by");
        args.push(group_by);
    }
    if options.exclude_dnp {
        args.push("--exclude-dnp");
    }
    args.push(schematic);
    args
}

/// KiCAD 10: `sch export bom --output <path> [--fields …] [--labels …]
/// [--group-by …] [--exclude-dnp] <input>`
///
/// Note: v10 BOM does NOT use `--format`. Without `--fields` kicad-cli emits
/// its fixed `Reference,Value,Footprint,QUANTITY,DNP` set, so every custom
/// schematic field (MPN, LCSC, supplier part numbers) is dropped.
pub async fn export_bom(
    cli: &str,
    schematic: &Path,
    output: &Path,
    options: &BomOptions<'_>,
) -> Result<()> {
    let args = bom_args(
        output.to_str().unwrap_or(""),
        schematic.to_str().unwrap_or(""),
        options,
    );
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `sch export netlist --output <path> --format <fmt> <input>`
/// Valid formats: kicadsexpr, kicadxml, cadstar, orcadpcb2, spice, spicemodel, pads, allegro
pub async fn export_netlist(
    cli: &str,
    schematic: &Path,
    output: &Path,
    format: &str,
) -> Result<()> {
    // Map friendly names to v10 format values
    let lower = format.to_lowercase();
    let v10_format = match lower.as_str() {
        "kicad" | "kicadsexpr" | "sexp" => "kicadsexpr",
        "xml" | "kicadxml" => "kicadxml",
        "spice" => "spice",
        "cadstar" => "cadstar",
        "orcad" | "orcadpcb2" => "orcadpcb2",
        "pads" => "pads",
        "allegro" => "allegro",
        _ => &lower,
    };
    let args = [
        "sch",
        "export",
        "netlist",
        "--output",
        output.to_str().unwrap(),
        "--format",
        v10_format,
        schematic.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

// ─── PCB Export ──────────────────────────────────────────────────────────────

/// Argument vector for Gerber export. KiCad's plural `gerbers` subcommand
/// accepts the complete selection as one comma-separated `--layers` value.
fn gerber_args<'a>(output_dir: &'a str, pcb: &'a str, layers_csv: &'a str) -> Vec<&'a str> {
    let mut args = vec!["pcb", "export", "gerbers", "--output", output_dir];
    if !layers_csv.is_empty() {
        args.push("--layers");
        args.push(layers_csv);
    }
    args.push(pcb);
    args
}

/// KiCad 10: `pcb export gerbers --output <dir> [--layers <csv>] <input>`
/// (PLURAL!)
pub async fn export_gerber(
    cli: &str,
    pcb: &Path,
    output_dir: &Path,
    layers: &[&str],
) -> Result<()> {
    let layers_csv = layers.join(",");
    let args = gerber_args(
        output_dir.to_str().unwrap_or(""),
        pcb.to_str().unwrap_or(""),
        &layers_csv,
    );
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// `--output` for a drill export names a *directory*, and some kicad-cli
/// versions decide directory-vs-file by the trailing separator alone. An empty
/// string is left alone so we never hand kicad-cli a bare separator, which
/// would mean the filesystem root. Credit to @anyn99 (#161) for catching this.
fn drill_output_dir_arg(output_dir: &str) -> String {
    let mut arg = output_dir.to_string();
    if !arg.is_empty() && !arg.ends_with(['/', '\\']) {
        arg.push(std::path::MAIN_SEPARATOR);
    }
    arg
}

/// Argument vector for the drill export, factored out so the flags can be
/// asserted without a kicad-cli on the machine.
fn drill_args<'a>(output_dir: &'a str, pcb: &'a str) -> Vec<&'a str> {
    vec![
        "pcb",
        "export",
        "drill",
        // Plated and non-plated holes as separate files. Without this flag
        // KiCad emits ONE `MixedPlating` file in which the NPTH tools are
        // distinguished only by an `#@! TA.AperFunction ... NonPlated`
        // comment — a comment most Excellon readers drop, so the fab plates
        // holes that must stay unplated (connector flanges, mounting holes).
        "--excellon-separate-th",
        "--output",
        output_dir,
        pcb,
    ]
}

/// The `.drl` files in `dir`, sorted.
async fn drill_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("drl") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// KiCAD 10: `pcb export drill --output <dir> <input>`
///
/// `--output` is a **directory**, not a file: kicad-cli names the outputs after
/// the board (`<board>-PTH.drl` and `<board>-NPTH.drl`). Handing it a filename
/// makes KiCad create a *directory* of that name and hide the real drill files
/// one level down.
///
/// Returns the `.drl` files produced, sorted.
pub async fn export_drill(cli: &str, pcb: &Path, output_dir: &Path) -> Result<Vec<PathBuf>> {
    tokio::fs::create_dir_all(output_dir).await?;
    let dir_arg = drill_output_dir_arg(output_dir.to_str().unwrap_or(""));
    let args = drill_args(&dir_arg, pcb.to_str().unwrap_or(""));
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(drill_files_in(output_dir).await)
}

/// KiCAD 10: `pcb export pdf --output <path> [--layers <layer>]... <input>`
pub async fn export_pdf(cli: &str, pcb: &Path, output: &Path, layers: &[&str]) -> Result<()> {
    let mut args = vec!["pcb", "export", "pdf", "--output", output.to_str().unwrap()];
    for layer in layers {
        args.push("--layers");
        args.push(layer);
    }
    args.push(pcb.to_str().unwrap());
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export svg --output <path> [--layers <layer>]... <input>`
pub async fn export_svg_pcb(cli: &str, pcb: &Path, output: &Path, layers: &[&str]) -> Result<()> {
    let mut args = vec!["pcb", "export", "svg", "--output", output.to_str().unwrap()];
    for layer in layers {
        args.push("--layers");
        args.push(layer);
    }
    args.push(pcb.to_str().unwrap());
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export <format> --output <path> <input>`
/// Supported 3D formats: step, vrml, glb, brep, stl, ply, stpz, u3d, xao, 3dpdf
pub async fn export_3d(cli: &str, pcb: &Path, output: &Path, format: &str) -> Result<()> {
    let subcommand = match format.to_lowercase().as_str() {
        "step" | "stp" => "step",
        "vrml" | "wrl" => "vrml",
        "glb" | "gltf" => "glb",
        "brep" => "brep",
        "stl" => "stl",
        "ply" => "ply",
        "stpz" => "stpz",
        "u3d" => "u3d",
        "xao" => "xao",
        "3dpdf" | "pdf3d" => "3dpdf",
        other => anyhow::bail!(
            "Unsupported 3D format: '{}'. Supported: step, vrml, glb, brep, stl, ply, stpz, u3d, xao, 3dpdf",
            other
        ),
    };
    let args = vec![
        "pcb",
        "export",
        subcommand,
        "--output",
        output.to_str().unwrap(),
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// Argument vector for position export, factored out so the public options can
/// be regression-tested without a kicad-cli installation.
fn position_args<'a>(
    output: &'a str,
    pcb: &'a str,
    format: &'a str,
    units: &'a str,
    side: &'a str,
) -> Vec<&'a str> {
    let mut args = vec![
        "pcb", "export", "pos", "--output", output, "--format", format, "--side", side,
    ];
    // Gerber coordinates have format-defined units; KiCad only accepts this
    // option for its ASCII and CSV position formats.
    if format != "gerber" {
        args.push("--units");
        args.push(units);
    }
    args.push(pcb);
    args
}

/// KiCad 10: `pcb export pos --output <path> --format <fmt> --side <side>
/// [--units <units>] <input>`
///
/// KiCad itself omits footprints carrying `exclude_from_pos_files`; Konnect
/// deliberately leaves that source-of-truth filtering to the exporter rather
/// than trying to post-process CSV and Gerber output differently.
pub async fn export_position_file(
    cli: &str,
    pcb: &Path,
    output: &Path,
    format: &str,
    units: &str,
    side: &str,
) -> Result<()> {
    let args = position_args(
        output.to_str().unwrap_or(""),
        pcb.to_str().unwrap_or(""),
        format,
        units,
        side,
    );
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export ipcd356 --output <path> <input>`
pub async fn export_ipcd356(cli: &str, pcb: &Path, output: &Path) -> Result<()> {
    let args = [
        "pcb",
        "export",
        "ipcd356",
        "--output",
        output.to_str().unwrap(),
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export dxf --output <dir> [--layers <csv>] --mode-multi <input>`
///
/// Unlike `pdf`/`svg`, DXF's `--layers` takes a single comma-separated value
/// rather than a repeatable flag, and one file per requested layer is written
/// into `output_dir` (verified against KiCAD 10.0).
pub async fn export_dxf(cli: &str, pcb: &Path, output_dir: &Path, layers: &[&str]) -> Result<()> {
    let output_str = output_dir.to_str().unwrap();
    let pcb_str = pcb.to_str().unwrap();
    let layers_csv = layers.join(",");

    let mut args: Vec<&str> = vec!["pcb", "export", "dxf", "--output", output_str];
    if !layers_csv.is_empty() {
        args.push("--layers");
        args.push(&layers_csv);
    }
    args.push("--mode-multi");
    args.push(pcb_str);

    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export gencad --output <path> <input>`
pub async fn export_gencad(cli: &str, pcb: &Path, output: &Path) -> Result<()> {
    let args = [
        "pcb",
        "export",
        "gencad",
        "--output",
        output.to_str().unwrap(),
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export ipc2581 --output <path> --units <mm|in> [--compress] <input>`
pub async fn export_ipc2581(
    cli: &str,
    pcb: &Path,
    output: &Path,
    units: &str,
    compress: bool,
) -> Result<()> {
    let output_str = output.to_str().unwrap();
    let pcb_str = pcb.to_str().unwrap();

    let mut args: Vec<&str> = vec![
        "pcb", "export", "ipc2581", "--output", output_str, "--units", units,
    ];
    if compress {
        args.push("--compress");
    }
    args.push(pcb_str);

    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export odb --output <path> --units <mm|in> --compression <mode> <input>`
/// Compression modes (verified against KiCAD 10.0): `zip`, `none`, `tgz`.
pub async fn export_odb(
    cli: &str,
    pcb: &Path,
    output: &Path,
    units: &str,
    compression: &str,
) -> Result<()> {
    let args = [
        "pcb",
        "export",
        "odb",
        "--output",
        output.to_str().unwrap(),
        "--units",
        units,
        "--compression",
        compression,
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

// ─── Render to image ─────────────────────────────────────────────────────────

/// Render schematic to SVG (no bitmap export in KiCAD 10 CLI).
/// KiCAD 10: `sch export svg --output <dir> <input>`
pub async fn render_schematic_svg(cli: &str, schematic: &Path, output: &Path) -> Result<PathBuf> {
    let output_dir = output.parent().unwrap_or(Path::new("."));
    export_schematic_svg(cli, schematic, output_dir).await
}

/// KiCAD 10: `pcb render --output <path> --width <w> --height <h> <input>`
///
/// `pcb render` is the 3-D renderer and takes **no** `--layers`: passing it
/// makes kicad-cli exit non-zero with `Unknown argument: --layers`, which is
/// how this was broken from 1ec5b81 (2026-07-08) through v0.2.0 and v0.2.1 —
/// every call failed, and nothing tested it. Layer-aware 2-D output is
/// `pcb export svg`, tracked separately.
pub async fn render_pcb_png(
    cli: &str,
    pcb: &Path,
    output: &Path,
    width: u32,
    height: u32,
) -> Result<()> {
    let width_str = width.to_string();
    let height_str = height.to_string();
    let args = vec![
        "pcb",
        "render",
        "--output",
        output.to_str().unwrap(),
        "--width",
        &width_str,
        "--height",
        &height_str,
        pcb.to_str().unwrap(),
    ];
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

#[cfg(test)]
mod drc_parse_tests {
    use super::*;

    /// Real `kicad-cli pcb drc --format json` output (KiCAD 10.0.0, schema
    /// https://schemas.kicad.org/drc.v1.json), captured from the bundled
    /// `ecc83-pp` demo with its track segments removed so KiCad would actually
    /// report unconnected items. Trimmed to two entries per category; nothing
    /// is reshaped.
    fn real_report() -> serde_json::Value {
        serde_json::from_str(include_str!("../../tests/fixtures/drc_report_kicad10.json")).unwrap()
    }

    /// The whole point of #245: `unconnected_items` is where an unrouted net
    /// is reported, it carries severity `error`, and Konnect read only
    /// `violations` — so this board came back with zero errors.
    #[test]
    fn unconnected_items_are_part_of_the_result() {
        let report = parse_drc_report(&real_report()).unwrap();

        assert_eq!(report.violations.len(), 2);
        assert_eq!(report.unconnected_items.as_ref().unwrap().len(), 2);
        assert_eq!(report.schematic_parity.as_ref().unwrap().len(), 0);
        assert_eq!(report.all().count(), 4);

        // Reading `violations` alone would have said zero.
        assert_eq!(report.error_count(), 2);
        assert!(report
            .unconnected_items
            .as_ref()
            .unwrap()
            .iter()
            .all(|v| v.severity == "error"));
    }

    /// KiCad reports a position per *involved item*, not one per violation.
    /// Reading a top-level `pos` — which the schema has never had — made every
    /// position `null`, and the rule key was dropped entirely, leaving the
    /// caller with prose they cannot act on.
    #[test]
    fn a_violation_carries_its_rule_key_and_a_real_position() {
        let report = parse_drc_report(&real_report()).unwrap();
        let first = &report.violations[0];

        assert!(
            !first.rule.is_empty(),
            "the rule key is what you fix or waive"
        );
        let pos = first
            .pos
            .as_ref()
            .expect("position comes from items[0].pos");
        assert!(pos.x != 0.0 || pos.y != 0.0);

        let unconnected = &report.unconnected_items.as_ref().unwrap()[0];
        assert_eq!(unconnected.rule, "unconnected_items");
        assert!(unconnected.pos.is_some());
    }

    /// A report missing `violations` is not a DRC report. Defaulting it to an
    /// empty list renders as a clean board, which is the failure this change
    /// exists to remove.
    #[test]
    fn a_report_without_violations_is_an_error_not_a_clean_board() {
        let error = parse_drc_report(&serde_json::json!({ "source": "x.kicad_pcb" }))
            .expect_err("a report with no violations array is not a result");
        assert!(format!("{error:#}").contains("violations"));
    }

    /// A kicad-cli that does not report a category must read as `None`, never
    /// as zero: "none found" and "never asked" are different answers, and only
    /// one of them justifies calling a board clean.
    #[test]
    fn an_unreported_category_is_absent_not_zero() {
        let report = parse_drc_report(&serde_json::json!({ "violations": [] })).unwrap();
        assert!(report.unconnected_items.is_none());
        assert!(report.schematic_parity.is_none());
        assert_eq!(
            report.missing_categories(),
            vec!["unconnected_items", "schematic_parity"]
        );
    }
}

#[cfg(test)]
mod erc_parse_tests {
    use super::*;

    /// Shape produced by `kicad-cli sch erc --format json` (KiCAD 10.0.3,
    /// schema https://schemas.kicad.org/erc.v1.json), trimmed to the fields
    /// the parser touches. Captured from a real run on a 2-resistor divider.
    fn real_report() -> serde_json::Value {
        serde_json::json!({
            "$schema": "https://schemas.kicad.org/erc.v1.json",
            "coordinate_units": "mm",
            "kicad_version": "10.0.3",
            "sheets": [
                {
                    "path": "/",
                    "uuid_path": "/14ad3364-2bf7-4e0f-ab6e-27bd0021e859",
                    "violations": [
                        {
                            "description": "Pin not connected",
                            "items": [
                                {
                                    "description": "Symbol R1 Pin 1 [Passive, Line]",
                                    "pos": { "x": 1.0033, "y": 0.762 },
                                    "uuid": "bf26e4e8-972e-4f6c-8144-fe6b3fdd68ad"
                                }
                            ],
                            "severity": "error",
                            "type": "pin_not_connected"
                        },
                        {
                            "description": "Pin not connected",
                            "items": [
                                {
                                    "description": "Symbol R2 Pin 2 [Passive, Line]",
                                    "pos": { "x": 1.0033, "y": 1.143 },
                                    "uuid": "da98d3c5-aa74-4df3-8151-0d6e1e166975"
                                }
                            ],
                            "severity": "warning",
                            "type": "pin_not_connected"
                        }
                    ]
                }
            ]
        })
    }

    #[test]
    fn parses_violations_nested_under_sheets() {
        let violations = parse_erc_json(&real_report());
        assert_eq!(
            violations.len(),
            2,
            "must flatten sheets[].violations — a top-level 'violations' key does not exist in ERC reports"
        );
        assert_eq!(violations[0].severity, "error");
        assert!(violations[0].description.contains("Pin not connected"));
        assert!(
            violations[0].description.contains("R1"),
            "description should name the offending item"
        );
        assert_eq!(violations[0].sheet.as_deref(), Some("/"));
        let pos = violations[0].pos.as_ref().expect("position from items[0]");
        assert!((pos.x - 1.0033).abs() < 1e-9);
        assert_eq!(violations[1].severity, "warning");
    }

    #[test]
    fn empty_or_alien_reports_yield_no_violations() {
        assert!(parse_erc_json(&serde_json::json!({})).is_empty());
        assert!(parse_erc_json(&serde_json::json!({ "sheets": [] })).is_empty());
        // DRC-shaped input (top-level violations) is not an ERC report.
        assert!(
            parse_erc_json(&serde_json::json!({ "violations": [{ "severity": "error" }] }))
                .is_empty()
        );
    }
}

#[cfg(test)]
mod gerber_export_tests {
    use super::*;

    #[test]
    fn requested_layers_reach_kicad_as_one_csv_argument() {
        let args = gerber_args(
            "/out/gerbers",
            "/tmp/board.kicad_pcb",
            "F.Cu,In1.Cu,B.Cu,F.Mask,B.Mask,Edge.Cuts",
        );
        let layers = args
            .iter()
            .position(|argument| *argument == "--layers")
            .map(|index| args[index + 1]);
        assert_eq!(layers, Some("F.Cu,In1.Cu,B.Cu,F.Mask,B.Mask,Edge.Cuts"));
        assert_eq!(args.last().copied(), Some("/tmp/board.kicad_pcb"));
    }

    #[test]
    fn empty_layer_selection_keeps_the_flag_absent() {
        let args = gerber_args("/out", "/tmp/board.kicad_pcb", "");
        assert!(!args.contains(&"--layers"));
    }
}

#[cfg(test)]
mod position_export_tests {
    use super::*;

    fn flag<'a>(args: &'a [&str], name: &str) -> Option<&'a str> {
        args.iter()
            .position(|argument| *argument == name)
            .map(|index| args[index + 1])
    }

    #[test]
    fn csv_units_and_side_reach_kicad_cli() {
        let args = position_args(
            "/out/positions.csv",
            "/tmp/board.kicad_pcb",
            "csv",
            "mm",
            "back",
        );
        assert_eq!(flag(&args, "--format"), Some("csv"));
        assert_eq!(flag(&args, "--units"), Some("mm"));
        assert_eq!(flag(&args, "--side"), Some("back"));
        assert_eq!(args.last().copied(), Some("/tmp/board.kicad_pcb"));
    }

    #[test]
    fn gerber_position_export_does_not_claim_a_units_flag() {
        let args = position_args(
            "/out/positions.gbr",
            "/tmp/board.kicad_pcb",
            "gerber",
            "mm",
            "front",
        );
        assert_eq!(flag(&args, "--format"), Some("gerber"));
        assert_eq!(flag(&args, "--side"), Some("front"));
        assert_eq!(flag(&args, "--units"), None);
    }
}

#[cfg(test)]
mod drill_export_tests {
    use super::*;

    /// Non-plated holes must come out as their own Excellon file. The merged
    /// default marks them with nothing but an `#@! TA.AperFunction` comment,
    /// which most fab-side Excellon readers discard — so a connector flange or
    /// mounting hole arrives plated.
    #[test]
    fn drill_export_separates_plated_from_non_plated_holes() {
        let args = drill_args("/out/gerbers", "/tmp/board.kicad_pcb");
        assert!(
            args.contains(&"--excellon-separate-th"),
            "NPTH holes need their own file: {args:?}"
        );
    }

    /// `--output` is a directory. Passing a filename makes kicad-cli create a
    /// directory with that name and write the real drill files inside it.
    #[test]
    fn drill_export_output_is_the_directory_it_was_given() {
        let args = drill_args("/out/gerbers", "/tmp/board.kicad_pcb");
        let output = args
            .iter()
            .position(|a| *a == "--output")
            .map(|i| args[i + 1])
            .expect("--output");
        assert_eq!(output, "/out/gerbers");
        assert_eq!(args.last().copied(), Some("/tmp/board.kicad_pcb"));
    }

    #[tokio::test]
    async fn drill_files_are_collected_sorted_and_filtered_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["board-PTH.drl", "board-NPTH.drl", "board-drl_map.pdf"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let files = drill_files_in(dir.path()).await;
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["board-NPTH.drl", "board-PTH.drl"]);
    }

    /// Some kicad-cli versions read directory-vs-file from the trailing
    /// separator alone, so the directory argument carries one.
    #[test]
    fn drill_output_directory_argument_ends_in_a_separator() {
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            drill_output_dir_arg("/out/gerbers"),
            format!("/out/gerbers{sep}")
        );
    }

    /// Already separator-terminated input must not grow a second one, and an
    /// empty path must not become a bare separator — that would be the root.
    #[test]
    fn drill_output_directory_argument_is_idempotent_and_skips_empty() {
        assert_eq!(drill_output_dir_arg("/out/gerbers/"), "/out/gerbers/");
        assert_eq!(drill_output_dir_arg(r"C:\out\gerbers\"), r"C:\out\gerbers\");
        assert_eq!(drill_output_dir_arg(""), "");
    }
}

#[cfg(test)]
mod bom_export_tests {
    use super::*;

    /// Custom schematic fields are the whole point of a fab BOM: without
    /// `--fields` kicad-cli emits only Reference,Value,Footprint,QUANTITY,DNP
    /// and an MPN column never reaches the manufacturer.
    #[test]
    fn requested_fields_and_labels_reach_kicad_cli() {
        let options = BomOptions {
            fields: Some("Reference,Value,Footprint,MPN,${QUANTITY}"),
            labels: Some("Refs,Value,Footprint,MPN,Qty"),
            group_by: Some("Value,Footprint"),
            exclude_dnp: false,
        };
        let args = bom_args("/out/bom.csv", "/tmp/board.kicad_sch", &options);
        let flag = |name: &str| {
            args.iter()
                .position(|a| *a == name)
                .map(|i| args[i + 1])
                .unwrap_or_else(|| panic!("{name} missing from {args:?}"))
        };
        assert_eq!(
            flag("--fields"),
            "Reference,Value,Footprint,MPN,${QUANTITY}"
        );
        assert_eq!(flag("--labels"), "Refs,Value,Footprint,MPN,Qty");
        assert_eq!(flag("--group-by"), "Value,Footprint");
        assert_eq!(flag("--output"), "/out/bom.csv");
        assert_eq!(args.last().copied(), Some("/tmp/board.kicad_sch"));
    }

    /// `exclude_dnp` has been in the export_bom schema (default true) since the
    /// tool shipped, but the handler never read it and the flag was never sent.
    #[test]
    fn exclude_dnp_is_passed_only_when_asked_for() {
        let on = BomOptions {
            exclude_dnp: true,
            ..Default::default()
        };
        assert!(bom_args("/out/bom.csv", "/s.kicad_sch", &on).contains(&"--exclude-dnp"));

        let off = BomOptions::default();
        assert!(!bom_args("/out/bom.csv", "/s.kicad_sch", &off).contains(&"--exclude-dnp"));
    }

    /// Defaults must reproduce the previous argv exactly, so a caller that
    /// wants KiCAD's own BOM keeps getting it.
    #[test]
    fn default_options_are_the_bare_kicad_cli_invocation() {
        let args = bom_args("/out/bom.csv", "/s.kicad_sch", &BomOptions::default());
        assert_eq!(
            args,
            [
                "sch",
                "export",
                "bom",
                "--output",
                "/out/bom.csv",
                "/s.kicad_sch"
            ]
        );
    }
}
