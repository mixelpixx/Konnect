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
use konnect_sexp::board::{ItemIdentity, ItemOwner};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// Extended timeout for long operations (export, ERC, DRC).
const LONG_TIMEOUT: Duration = Duration::from_secs(600);

fn cli_failure_diagnostics(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let stdout = stdout.trim();
    let stderr = stderr.trim();

    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("stdout:\n{stdout}\nstderr:\n{stderr}"),
        (false, true) => format!("stdout:\n{stdout}"),
        (true, false) => format!("stderr:\n{stderr}"),
        (true, true) => "no diagnostic output".to_string(),
    }
}

// ─── Result Types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErcViolation {
    pub severity: String,
    pub description: String,
    /// KiCad's rule key (`"pin_to_pin"`, `"pin_not_connected"`, …). Stable,
    /// unlike the prose description beside it.
    pub rule: String,
    pub sheet: Option<String>,
    /// Every item the rule caught, in report order. A `pin_to_pin` violation
    /// always names two pins and the second is regularly the actionable one,
    /// so keeping only the first hid what explains the violation.
    pub items: Vec<ReportItem>,
}

/// One item involved in an ERC or DRC violation. Both reports use the same
/// item shape, so both parsers decode it the same way.
///
/// The four ownership fields are filled in by [`enrich_drc_items`] on the DRC
/// path only — ERC has no board to index — and every one of them serialises
/// away when it was never set, so the ERC response and any caller that ignores
/// them see exactly the shape they saw before (#413).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportItem {
    pub description: String,
    pub pos: Option<ReportPos>,
    /// Absent rather than null when KiCad names no item id, which is the
    /// shape both the ERC and DRC responses have always had.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Whether ownership was resolved, and if not, why. Absent when ownership
    /// enrichment did not run at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership_status: Option<OwnershipStatus>,
    /// The board node's head — `fp_circle`, `gr_line`, `pad`, `segment`, … —
    /// as the `.kicad_pcb` spells it. Only set when the item resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_kind: Option<String>,
    /// The resolved item's single `(layer …)`, when it names one. Only set
    /// when the item resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// Three states, which is why this is nested:
    ///
    /// - outer `None` — enrichment never ran (ERC, or the board could not be
    ///   re-read); the key is absent from the JSON entirely.
    /// - `Some(None)` — enrichment ran and could not resolve the item; the key
    ///   serialises as `"owner": null`, paired with an `ownership_status` that
    ///   says why.
    /// - `Some(Some(owner))` — resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Option<ItemOwner>>,
}

/// Why a DRC report item does or does not name an owner.
///
/// Only ever set from an exact UUID lookup against the saved board. There is
/// deliberately no "guessed" state: inferring ownership from a description
/// like `"Circle of J1"` is what this field exists to replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipStatus {
    /// The item's UUID was found in the board; `owner` names what owns it.
    Resolved,
    /// KiCad reported this item without a UUID, so there is nothing to look
    /// up. `owner` is null.
    UuidMissing,
    /// The item's UUID is not in the board Konnect indexed — a stale report,
    /// or a board saved after the run. `owner` is null.
    NotFound,
}

impl ReportItem {
    /// Resolve this item against a board's UUID index, in place.
    ///
    /// Exact UUID match or nothing: an unmatched item is left explicitly
    /// unresolved rather than defaulted to `board`, because "we could not tell"
    /// and "the board owns it" lead to opposite repairs.
    fn resolve_ownership(&mut self, index: &HashMap<String, ItemIdentity>) {
        let Some(uuid) = self.uuid.as_deref() else {
            self.ownership_status = Some(OwnershipStatus::UuidMissing);
            self.owner = Some(None);
            return;
        };
        match index.get(uuid) {
            Some(identity) => {
                self.ownership_status = Some(OwnershipStatus::Resolved);
                self.item_kind = Some(identity.item_kind.clone());
                self.layer = identity.layer.clone();
                self.owner = Some(Some(identity.owner.clone()));
            }
            None => {
                self.ownership_status = Some(OwnershipStatus::NotFound);
                self.owner = Some(None);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ReportPos {
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
    pub pos: Option<ReportPos>,
    /// Every item the rule caught, in report order. The prose description of
    /// an `unconnected_items` violation is a constant, so the pads and the net
    /// its items name are the only record of what is unrouted — and two
    /// violations sharing a rule, a description and a first position differ
    /// nowhere else.
    pub items: Vec<ReportItem>,
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

    /// Every finding across every category, mutably — the enrichment pass
    /// walks this so no category can be left behind (#413).
    fn all_mut(&mut self) -> impl Iterator<Item = &mut DrcViolation> {
        self.violations
            .iter_mut()
            .chain(self.unconnected_items.iter_mut().flatten())
            .chain(self.schematic_parity.iter_mut().flatten())
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
        anyhow::bail!(
            "kicad-cli exited with {}:\n{}",
            output.status.code().unwrap_or(-1),
            cli_failure_diagnostics(&output.stdout, &output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// An export command returning success is necessary but not sufficient: KiCad
/// can exit successfully without creating the path a caller asked for. Every
/// path Konnect reports as an artifact passes this check first (#252).
async fn verify_nonempty_file(path: &Path, artifact: &str) -> Result<u64> {
    let metadata = tokio::fs::metadata(path).await.with_context(|| {
        format!(
            "{artifact} export reported success but did not create {}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        anyhow::bail!(
            "{artifact} export reported success but {} is not a file",
            path.display()
        );
    }
    if metadata.len() == 0 {
        anyhow::bail!(
            "{artifact} export reported success but created an empty file at {}",
            path.display()
        );
    }
    Ok(metadata.len())
}

fn export_staging_dir(destination: &Path) -> Result<tempfile::TempDir> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    tempfile::Builder::new()
        .prefix(".konnect-export-")
        .tempdir_in(parent)
        .context("failed to create an export staging directory")
}

/// Publish a verified artifact without letting a stale destination stand in
/// for output from the current command. The staging directory is a sibling of
/// the destination, so every rename remains on the same filesystem.
async fn publish_verified_file(staged: &Path, destination: &Path, artifact: &str) -> Result<u64> {
    let size = verify_nonempty_file(staged, artifact).await?;
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    if !destination.exists() {
        tokio::fs::rename(staged, destination).await?;
        return Ok(size);
    }

    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    let backup = staged.with_file_name(format!(".konnect-previous-{file_name}"));
    tokio::fs::rename(destination, &backup)
        .await
        .with_context(|| {
            format!(
                "cannot preserve the previous artifact at {} before publishing",
                destination.display()
            )
        })?;
    if let Err(install_error) = tokio::fs::rename(staged, destination).await {
        if let Err(restore_error) = tokio::fs::rename(&backup, destination).await {
            anyhow::bail!(
                "failed to publish {} ({install_error}) and restore its previous contents ({restore_error})",
                destination.display()
            );
        }
        return Err(install_error).with_context(|| {
            format!(
                "failed to publish verified artifact {}",
                destination.display()
            )
        });
    }
    tokio::fs::remove_file(backup).await?;
    Ok(size)
}

async fn publish_verified_files(
    staged_files: &[PathBuf],
    output_dir: &Path,
    artifact: &str,
) -> Result<Vec<PathBuf>> {
    for staged in staged_files {
        verify_nonempty_file(staged, artifact).await?;
    }

    let mut published = Vec::with_capacity(staged_files.len());
    for staged in staged_files {
        let file_name = staged
            .file_name()
            .context("export produced a path without a file name")?;
        let destination = output_dir.join(file_name);
        publish_verified_file(staged, &destination, artifact).await?;
        published.push(destination);
    }
    Ok(published)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    fn write_script(dir: &Path, stem: &str, unix_body: &str, windows_body: &str) -> PathBuf {
        #[cfg(windows)]
        let path = dir.join(format!("{stem}.cmd"));
        #[cfg(not(windows))]
        let path = dir.join(stem);

        #[cfg(windows)]
        {
            let _ = unix_body;
            std::fs::write(&path, windows_body).unwrap();
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = windows_body;
            std::fs::write(&path, unix_body).unwrap();
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        path
    }

    pub(crate) fn noop_cli(dir: &Path) -> PathBuf {
        write_script(
            dir,
            "fake-kicad-cli",
            "#!/bin/sh\nexit 0\n",
            "@exit /b 0\r\n",
        )
    }

    pub(crate) fn schematic_only_cli(dir: &Path) -> PathBuf {
        write_script(
            dir,
            "fake-kicad-cli",
            "#!/bin/sh\nif [ \"$1\" = \"sch\" ]; then\n  while [ \"$#\" -gt 0 ]; do\n    if [ \"$1\" = \"--output\" ]; then\n      shift\n      printf '%s' 'PDF-test' > \"$1\"\n      break\n    fi\n    shift\n  done\nfi\nexit 0\n",
            "@echo off\r\nif not \"%1\"==\"sch\" exit /b 0\r\n:loop\r\nif \"%1\"==\"\" goto done\r\nif not \"%1\"==\"--output\" goto next\r\nshift\r\necho PDF-test>\"%1\"\r\ngoto done\r\n:next\r\nshift\r\ngoto loop\r\n:done\r\nexit /b 0\r\n",
        )
    }
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
            let items: Vec<ReportItem> = v
                .get("items")
                .and_then(|i| i.as_array())
                .map(|items| items.iter().map(parse_report_item).collect())
                .unwrap_or_default();
            let mut description = v["description"].as_str().unwrap_or("").to_string();
            // The per-item description names the offender ("Symbol R1 Pin 1…")
            // — without it "Pin not connected" is unactionable.
            if let Some(detail) = items
                .first()
                .map(|item| item.description.as_str())
                .filter(|detail| !detail.is_empty())
            {
                description = format!("{}: {}", description, detail);
            }
            out.push(ErcViolation {
                severity: v["severity"].as_str().unwrap_or("error").to_string(),
                description,
                rule: v["type"].as_str().unwrap_or("").to_string(),
                sheet: sheet_path.clone(),
                items,
            });
        }
    }
    out
}

/// Decode one item of an ERC or DRC violation — the two reports spell it the
/// same way.
fn parse_report_item(item: &serde_json::Value) -> ReportItem {
    ReportItem {
        description: item["description"].as_str().unwrap_or("").to_string(),
        pos: parse_item_pos(item),
        uuid: item["uuid"].as_str().map(String::from),
        // Filled in on the DRC path by `enrich_drc_items`; ERC leaves them
        // unset and they serialise away.
        ownership_status: None,
        item_kind: None,
        layer: None,
        owner: None,
    }
}

fn parse_item_pos(item: &serde_json::Value) -> Option<ReportPos> {
    let pos = item.get("pos")?;
    Some(ReportPos {
        x: pos["x"].as_f64()?,
        y: pos["y"].as_f64()?,
    })
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

    let mut report = parse_drc_report(&raw)?;

    // Ownership comes from the exact board this DRC ran on, and it is attached
    // here — the one path `run_drc` and `get_drc_violations` share — so the two
    // tools cannot report different ownership for the same violation (#413).
    //
    // A report with nothing to annotate does not pay for the board re-read and
    // parse: a clean board is the common case, and `run_drc` is called once per
    // candidate inside the routing loop.
    if report.all().any(|violation| !violation.items.is_empty()) {
        match tokio::fs::read_to_string(pcb).await {
            Ok(source) => enrich_drc_items(&mut report, &source),
            Err(error) => warn!(
                "[BETA] DRC ownership enrichment skipped: could not re-read {}: {error}",
                pcb.display()
            ),
        }
    }

    Ok(report)
}

/// Name what owns every item of every violation, by exact UUID.
///
/// A `copper_edge_clearance` item reads identically whether the offending
/// `Edge.Cuts` geometry is the board outline or a cutout that a footprint
/// carries in its own artwork, and the repairs are opposite: move the part, or
/// edit the part's footprint. Footprint ownership does not make the finding
/// false — a footprint-owned cutout is still fabrication geometry, still cut
/// out of the board — it selects the remedy (#413).
///
/// Best effort by design: a board that will not parse leaves every item exactly
/// as KiCad reported it, because withholding DRC results over a failed lookup
/// would be a worse answer than an unannotated one. Split out of [`run_drc`] so
/// it is testable against a board/report pair with no `kicad-cli` present.
fn enrich_drc_items(report: &mut DrcReport, board_source: &str) {
    let tree = match konnect_sexp::parse_sexp(board_source) {
        Ok(tree) => tree,
        Err(error) => {
            warn!("[BETA] DRC ownership enrichment skipped: board did not parse: {error}");
            return;
        }
    };
    let index = konnect_sexp::board::uuid_index(&tree);
    for violation in report.all_mut() {
        for item in &mut violation.items {
            item.resolve_ownership(&index);
        }
    }
}

/// Split out so it can be tested against a real `kicad-cli` report without
/// running kicad-cli.
fn parse_drc_report(raw: &serde_json::Value) -> Result<DrcReport> {
    fn category(raw: &serde_json::Value, key: &str) -> Option<Vec<DrcViolation>> {
        Some(
            raw.get(key)?
                .as_array()?
                .iter()
                .map(|v| {
                    let items: Vec<ReportItem> = v["items"]
                        .as_array()
                        .map(|items| items.iter().map(parse_report_item).collect())
                        .unwrap_or_default();
                    DrcViolation {
                        severity: v["severity"].as_str().unwrap_or("error").to_string(),
                        description: v["description"].as_str().unwrap_or("").to_string(),
                        rule: v["type"].as_str().unwrap_or("").to_string(),
                        // The position lives on each involved item; a violation
                        // has no `pos` of its own.
                        pos: items.iter().find_map(|item| item.pos),
                        items,
                    }
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

#[derive(Debug, Default, Clone)]
pub struct SchematicSvgOptions<'a> {
    pub black_and_white: bool,
    pub theme: Option<&'a str>,
}

fn schematic_svg_args<'a>(
    output_dir: &'a str,
    schematic: &'a str,
    options: &'a SchematicSvgOptions<'a>,
) -> Vec<&'a str> {
    let mut args = vec!["sch", "export", "svg", "--output", output_dir];
    if options.black_and_white {
        args.push("--black-and-white");
    }
    if let Some(theme) = options.theme {
        args.push("--theme");
        args.push(theme);
    }
    args.push(schematic);
    args
}

/// KiCAD 10: `sch export svg --output <dir> [--black-and-white]
/// [--theme <name>] <input>`
pub async fn export_schematic_svg(
    cli: &str,
    schematic: &Path,
    output_dir: &Path,
    options: &SchematicSvgOptions<'_>,
) -> Result<PathBuf> {
    let staging = export_staging_dir(output_dir)?;
    let args = schematic_svg_args(
        staging.path().to_str().unwrap(),
        schematic.to_str().unwrap(),
        options,
    );
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    let stem = schematic.file_stem().unwrap_or_default().to_string_lossy();
    let staged_root = staging.path().join(format!("{}.svg", stem));

    let mut staged_files = Vec::new();
    let mut entries = tokio::fs::read_dir(staging.path()).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("svg") {
            staged_files.push(path);
        }
    }
    staged_files.sort();
    verify_nonempty_file(&staged_root, "schematic SVG").await?;
    publish_verified_files(&staged_files, output_dir, "schematic SVG").await?;
    Ok(output_dir.join(format!("{}.svg", stem)))
}

#[derive(Debug, Clone)]
pub struct SchematicPdfOptions {
    pub black_and_white: bool,
    pub all_sheets: bool,
}

impl Default for SchematicPdfOptions {
    fn default() -> Self {
        Self {
            black_and_white: false,
            all_sheets: true,
        }
    }
}

fn schematic_pdf_args<'a>(
    output: &'a str,
    schematic: &'a str,
    options: &SchematicPdfOptions,
) -> Vec<&'a str> {
    let mut args = vec!["sch", "export", "pdf", "--output", output];
    if options.black_and_white {
        args.push("--black-and-white");
    }
    if !options.all_sheets {
        args.extend(["--pages", "1"]);
    }
    args.push(schematic);
    args
}

/// KiCAD 10: `sch export pdf --output <path> [--black-and-white]
/// [--pages 1] <input>`
pub async fn export_schematic_pdf(
    cli: &str,
    schematic: &Path,
    output: &Path,
    options: &SchematicPdfOptions,
) -> Result<()> {
    let staging = export_staging_dir(output)?;
    let staged = staging.path().join(
        output
            .file_name()
            .context("schematic PDF output has no file name")?,
    );
    let args = schematic_pdf_args(
        staged.to_str().unwrap(),
        schematic.to_str().unwrap(),
        options,
    );
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    publish_verified_file(&staged, output, "schematic PDF").await?;
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
    let staging = export_staging_dir(output)?;
    let staged = staging
        .path()
        .join(output.file_name().context("BOM output has no file name")?);
    let args = bom_args(
        staged.to_str().unwrap_or(""),
        schematic.to_str().unwrap_or(""),
        options,
    );
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    publish_verified_file(&staged, output, "BOM").await?;
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
) -> Result<Vec<PathBuf>> {
    let staging = export_staging_dir(output_dir)?;
    let layers_csv = layers.join(",");
    let args = gerber_args(
        staging.path().to_str().unwrap_or(""),
        pcb.to_str().unwrap_or(""),
        &layers_csv,
    );
    run_cli(cli, &args, LONG_TIMEOUT).await?;

    let board_stem = pcb.file_stem().unwrap_or_default().to_string_lossy();
    let mut files = Vec::new();
    let mut entries = tokio::fs::read_dir(staging.path()).await.with_context(|| {
        format!(
            "Gerber export reported success but output directory {} is missing",
            staging.path().display()
        )
    })?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_gerber = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.to_ascii_lowercase().starts_with('g'));
        if name.starts_with(board_stem.as_ref()) && is_gerber {
            verify_nonempty_file(&path, "Gerber").await?;
            files.push(path);
        }
    }
    files.sort();
    let plot_count = files
        .iter()
        .filter(|path| path.extension().and_then(|value| value.to_str()) != Some("gbrjob"))
        .count();
    if plot_count < layers.len().max(1) {
        anyhow::bail!(
            "Gerber export reported success but produced {plot_count} non-empty plot file(s) for {} requested layer(s) in {}",
            layers.len(),
            staging.path().display()
        );
    }
    publish_verified_files(&files, output_dir, "Gerber").await
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
    let staging = export_staging_dir(output_dir)?;
    let dir_arg = drill_output_dir_arg(staging.path().to_str().unwrap_or(""));
    let args = drill_args(&dir_arg, pcb.to_str().unwrap_or(""));
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    let files = drill_files_in(staging.path()).await;
    if files.is_empty() {
        anyhow::bail!(
            "drill export reported success but produced no .drl files in {}",
            staging.path().display()
        );
    }
    for file in &files {
        verify_nonempty_file(file, "drill").await?;
    }
    publish_verified_files(&files, output_dir, "drill").await
}

fn single_file_pcb_export_args(
    format: &str,
    output: &str,
    layers: &[&str],
    black_and_white: bool,
    pcb: &str,
) -> Vec<String> {
    let mut args = vec![
        "pcb".to_string(),
        "export".to_string(),
        format.to_string(),
        "--output".to_string(),
        output.to_string(),
        "--mode-single".to_string(),
    ];
    if !layers.is_empty() {
        args.push("--layers".to_string());
        args.push(layers.join(","));
    }
    if black_and_white {
        args.push("--black-and-white".to_string());
    }
    args.push(pcb.to_string());
    args
}

/// KiCAD 10: `pcb export pdf --output <path> --mode-single [--layers <a,b>]
/// [--black-and-white] <input>`
pub async fn export_pdf(
    cli: &str,
    pcb: &Path,
    output: &Path,
    layers: &[&str],
    black_and_white: bool,
) -> Result<()> {
    let staging = export_staging_dir(output)?;
    let staged = staging.path().join(
        output
            .file_name()
            .context("PCB PDF output has no file name")?,
    );
    let args = single_file_pcb_export_args(
        "pdf",
        staged.to_str().unwrap(),
        layers,
        black_and_white,
        pcb.to_str().unwrap(),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    publish_verified_file(&staged, output, "PCB PDF").await?;
    Ok(())
}

/// KiCAD 10: `pcb export svg --output <path> --mode-single [--layers <a,b>]
/// [--black-and-white] <input>`
pub async fn export_svg_pcb(
    cli: &str,
    pcb: &Path,
    output: &Path,
    layers: &[&str],
    black_and_white: bool,
) -> Result<()> {
    let args = single_file_pcb_export_args(
        "svg",
        output.to_str().unwrap(),
        layers,
        black_and_white,
        pcb.to_str().unwrap(),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    Ok(())
}

/// KiCAD 10: `pcb export <format> --output <path> [--no-unspecified] <input>`
/// Supported 3D formats: step, vrml, glb, brep, stl, ply, stpz, u3d, xao, 3dpdf
fn export_3d_args<'a>(
    pcb: &'a str,
    output: &'a str,
    format: &str,
    include_unspecified: bool,
) -> Result<Vec<&'a str>> {
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
    let mut args = vec!["pcb", "export", subcommand, "--output", output];
    if !include_unspecified {
        args.push("--no-unspecified");
    }
    args.push(pcb);
    Ok(args)
}

pub async fn export_3d(
    cli: &str,
    pcb: &Path,
    output: &Path,
    format: &str,
    include_unspecified: bool,
) -> Result<()> {
    let args = export_3d_args(
        pcb.to_str().unwrap(),
        output.to_str().unwrap(),
        format,
        include_unspecified,
    )?;
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
    let staging = export_staging_dir(output)?;
    let staged = staging.path().join(
        output
            .file_name()
            .context("position output has no file name")?,
    );
    let args = position_args(
        staged.to_str().unwrap_or(""),
        pcb.to_str().unwrap_or(""),
        format,
        units,
        side,
    );
    run_cli(cli, &args, LONG_TIMEOUT).await?;
    publish_verified_file(&staged, output, "position file").await?;
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
/// `--layers` takes a single comma-separated value, the same as every PCB
/// exporter (the pdf/svg wrappers used to repeat the flag per layer, which
/// KiCAD 10 rejects — #250). DXF differs in output shape only: one file per
/// requested layer is written into `output_dir` (verified against KiCAD 10.0).
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
    export_schematic_svg(cli, schematic, output_dir, &SchematicSvgOptions::default()).await
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
mod schematic_export_option_tests {
    use super::*;

    #[test]
    fn svg_theme_and_monochrome_flags_reach_kicad() {
        let options = SchematicSvgOptions {
            black_and_white: true,
            theme: Some("Solarized Dark"),
        };
        let args = schematic_svg_args("/out", "/tmp/design.kicad_sch", &options);

        assert!(args.contains(&"--black-and-white"));
        let theme = args
            .iter()
            .position(|argument| *argument == "--theme")
            .map(|index| args[index + 1]);
        assert_eq!(theme, Some("Solarized Dark"));
        assert_eq!(args.last().copied(), Some("/tmp/design.kicad_sch"));
    }

    #[test]
    fn pdf_can_limit_the_export_to_the_root_sheet() {
        let options = SchematicPdfOptions {
            black_and_white: true,
            all_sheets: false,
        };
        let args = schematic_pdf_args("/out/design.pdf", "/tmp/design.kicad_sch", &options);

        assert!(args.contains(&"--black-and-white"));
        let pages = args
            .iter()
            .position(|argument| *argument == "--pages")
            .map(|index| args[index + 1]);
        assert_eq!(pages, Some("1"));
    }

    #[test]
    fn schematic_defaults_leave_kicad_theme_and_page_selection_alone() {
        let svg_options = SchematicSvgOptions::default();
        let svg = schematic_svg_args("/out", "/tmp/design.kicad_sch", &svg_options);
        assert!(!svg.contains(&"--black-and-white"));
        assert!(!svg.contains(&"--theme"));

        let pdf_options = SchematicPdfOptions::default();
        let pdf = schematic_pdf_args("/out/design.pdf", "/tmp/design.kicad_sch", &pdf_options);
        assert!(!pdf.contains(&"--black-and-white"));
        assert!(!pdf.contains(&"--pages"));
    }
}

#[cfg(test)]
mod three_d_export_option_tests {
    use super::*;

    #[test]
    fn unspecified_models_are_excluded_by_default() {
        let args =
            export_3d_args("/tmp/board.kicad_pcb", "/out/board.step", "step", false).unwrap();
        assert!(args.contains(&"--no-unspecified"));
    }

    #[test]
    fn including_unspecified_models_omits_the_exclusion_flag() {
        let args = export_3d_args("/tmp/board.kicad_pcb", "/out/board.wrl", "vrml", true).unwrap();
        assert!(!args.contains(&"--no-unspecified"));
    }
}

#[cfg(test)]
mod pcb_plot_export_tests {
    use super::*;

    #[test]
    fn layers_are_one_comma_separated_argument_for_kicad_10() {
        let args = single_file_pcb_export_args(
            "svg",
            "/out/board.svg",
            &["F.Cu", "F.Paste", "F.SilkS", "Edge.Cuts"],
            false,
            "/tmp/board.kicad_pcb",
        );

        assert_eq!(args.iter().filter(|arg| *arg == "--layers").count(), 1);
        let layers = args
            .iter()
            .position(|arg| arg == "--layers")
            .map(|index| args[index + 1].as_str());
        assert_eq!(layers, Some("F.Cu,F.Paste,F.SilkS,Edge.Cuts"));
    }

    #[test]
    fn file_output_uses_single_mode_and_empty_layers_are_omitted() {
        let args = single_file_pcb_export_args(
            "pdf",
            "/out/board.pdf",
            &[],
            false,
            "/tmp/board.kicad_pcb",
        );

        assert!(args.iter().any(|arg| arg == "--mode-single"));
        assert!(!args.iter().any(|arg| arg == "--layers"));
        assert_eq!(
            args.last().map(String::as_str),
            Some("/tmp/board.kicad_pcb")
        );
    }

    #[test]
    fn black_and_white_reaches_both_single_file_plotters() {
        for format in ["pdf", "svg"] {
            let args = single_file_pcb_export_args(
                format,
                "/out/board.plot",
                &["F.Cu"],
                true,
                "/tmp/board.kicad_pcb",
            );
            assert!(args.iter().any(|argument| argument == "--black-and-white"));
        }
    }

    #[test]
    fn cli_failures_include_stdout_and_stderr_diagnostics() {
        assert_eq!(
            cli_failure_diagnostics(b"Duplicate argument --layers\n", b""),
            "stdout:\nDuplicate argument --layers"
        );
        assert_eq!(
            cli_failure_diagnostics(b"usage text", b"fatal detail"),
            "stdout:\nusage text\nstderr:\nfatal detail"
        );
        assert_eq!(cli_failure_diagnostics(b"", b""), "no diagnostic output");
    }
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

    /// `unconnected_items` says "Missing connection between items" and nothing
    /// else; the pads and the net live in the items, so dropping them left the
    /// caller with "something, somewhere, is unrouted".
    #[test]
    fn a_violation_keeps_every_item_it_names() {
        let report = parse_drc_report(&real_report()).unwrap();
        let unconnected = &report.unconnected_items.as_ref().unwrap()[0];

        assert_eq!(unconnected.description, "Missing connection between items");
        assert_eq!(unconnected.items.len(), 2);
        assert_eq!(
            unconnected.items[0].description,
            "PTH pad 1 [Net-(P3-P1)] of C1"
        );
        assert_eq!(
            unconnected.items[1].description,
            "PTH pad 1 [Net-(P3-P1)] of P3"
        );
        assert!(unconnected.items.iter().all(|item| item.uuid.is_some()));
        assert!(unconnected.items.iter().all(|item| item.pos.is_some()));

        // The violation's own position stays the first item's.
        let pos = unconnected.pos.as_ref().unwrap();
        let first = unconnected.items[0].pos.as_ref().unwrap();
        assert_eq!((pos.x, pos.y), (first.x, first.y));
    }

    /// The two `silk_edge_clearance` violations share a severity, a rule, a
    /// description and a first-item position. Without the items they serialise
    /// identically, and a caller cannot tell there are two problems.
    #[test]
    fn two_violations_alike_but_for_their_items_stay_distinguishable() {
        let report = parse_drc_report(&real_report()).unwrap();
        let (first, second) = (&report.violations[0], &report.violations[1]);

        assert_eq!(first.rule, second.rule);
        assert_eq!(first.description, second.description);
        assert_eq!(
            serde_json::to_value(&first.items[0]).unwrap(),
            serde_json::to_value(&second.items[0]).unwrap()
        );
        assert_ne!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap(),
            "two different problems must not serialise byte-identically"
        );
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
                        },
                        {
                            "description": "Pins of type Power output and Power output are connected",
                            "items": [
                                {
                                    "description": "Symbol #PWR031 Pin 1 [Power output, Line]",
                                    "pos": { "x": 1.4351, "y": 0.889 },
                                    "uuid": "0f7ec4d9-8a03-4a2f-8f9c-3d8f3f4e1c22"
                                },
                                {
                                    "description": "Symbol U2 Pin 5 [VOUT, Power output, Line]",
                                    "pos": { "x": 1.6002, "y": 1.016 },
                                    "uuid": "5b6a1f42-2c17-4f0b-9a6e-8c3f7d21e0a4"
                                }
                            ],
                            "severity": "error",
                            "type": "pin_to_pin"
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
            3,
            "must flatten sheets[].violations — a top-level 'violations' key does not exist in ERC reports"
        );
        assert_eq!(violations[0].severity, "error");
        assert!(violations[0].description.contains("Pin not connected"));
        assert!(
            violations[0].description.contains("R1"),
            "description should name the offending item"
        );
        assert_eq!(violations[0].sheet.as_deref(), Some("/"));
        let pos = violations[0].items[0].pos.as_ref().expect("item position");
        assert!((pos.x - 1.0033).abs() < 1e-9);
        assert_eq!(violations[1].severity, "warning");
    }

    /// A `pin_to_pin` violation names both conflicting pins, and the second is
    /// regularly the one that explains the first — here the regulator output
    /// that makes the `PWR_FLAG` redundant. Keeping only `items[0]` sent the
    /// caller back to `kicad-cli` by hand.
    #[test]
    fn every_item_of_a_violation_survives() {
        let conflict = &parse_erc_json(&real_report())[2];
        assert_eq!(conflict.items.len(), 2);
        assert!(conflict.items[0].description.contains("#PWR031"));
        let explains = &conflict.items[1];
        assert!(explains.description.contains("U2 Pin 5"));
        assert!((explains.pos.as_ref().expect("item position").y - 1.016).abs() < 1e-9);
        assert_eq!(
            explains.uuid.as_deref(),
            Some("5b6a1f42-2c17-4f0b-9a6e-8c3f7d21e0a4")
        );
    }

    /// `type` is the addressable key; `description` beside it is prose.
    #[test]
    fn violations_carry_kicads_rule_key() {
        let violations = parse_erc_json(&real_report());
        assert_eq!(violations[0].rule, "pin_not_connected");
        assert_eq!(violations[2].rule, "pin_to_pin");
    }

    /// The violation description predates `items` and callers read it, so a
    /// second item must not change what it says.
    #[test]
    fn the_description_still_names_the_first_item_only() {
        let conflict = &parse_erc_json(&real_report())[2];
        assert!(conflict.description.contains("#PWR031"));
        assert!(!conflict.description.contains("U2"));
    }

    /// KiCad omits `pos` and `uuid` on some item kinds; that must not drop the
    /// item, whose description is still the only thing naming the offender.
    #[test]
    fn an_item_without_a_position_is_still_reported() {
        let violations = parse_erc_json(&serde_json::json!({
            "sheets": [{
                "path": "/",
                "violations": [{
                    "description": "Label not connected",
                    "items": [{ "description": "Label VIN" }],
                    "severity": "warning",
                    "type": "label_dangling"
                }]
            }]
        }));
        assert_eq!(violations[0].items.len(), 1);
        assert!(violations[0].items[0].pos.is_none());
        assert!(violations[0].items[0].uuid.is_none());
        assert!(violations[0].description.contains("Label VIN"));
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
mod artifact_verification_tests {
    use super::*;

    #[tokio::test]
    async fn missing_and_empty_artifacts_are_not_successes() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pdf");
        let error = verify_nonempty_file(&missing, "test PDF")
            .await
            .expect_err("missing file must fail");
        assert!(error.to_string().contains("did not create"));

        let empty = dir.path().join("empty.pdf");
        std::fs::write(&empty, []).unwrap();
        let error = verify_nonempty_file(&empty, "test PDF")
            .await
            .expect_err("empty file must fail");
        assert!(error.to_string().contains("empty file"));

        let real = dir.path().join("real.pdf");
        std::fs::write(&real, b"%PDF-test").unwrap();
        assert_eq!(verify_nonempty_file(&real, "test PDF").await.unwrap(), 9);
    }

    #[tokio::test]
    async fn stale_destination_cannot_satisfy_a_new_export() {
        let dir = tempfile::tempdir().unwrap();
        let cli = test_support::noop_cli(dir.path());
        let schematic = dir.path().join("clock.kicad_sch");
        let destination = dir.path().join("clock.pdf");
        std::fs::write(&schematic, "placeholder").unwrap();
        std::fs::write(&destination, "stale-but-nonempty").unwrap();

        let error = export_schematic_pdf(
            cli.to_str().unwrap(),
            &schematic,
            &destination,
            &SchematicPdfOptions::default(),
        )
        .await
        .expect_err("the current invocation produced no artifact");

        assert!(error.to_string().contains("did not create"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(destination).unwrap(),
            "stale-but-nonempty",
            "a failed export must preserve the previous artifact"
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
            std::fs::write(dir.path().join(name), "non-empty").unwrap();
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

#[cfg(test)]
mod drc_ownership_tests {
    //! Issue #413: a `copper_edge_clearance` item reads the same whether the
    //! offending `Edge.Cuts` geometry is the board outline or a cutout a
    //! footprint carries in its own artwork, and the two need opposite repairs.
    //!
    //! Footprint ownership is not a false positive. J1's circles below are real
    //! cutouts in real copper; naming their owner says *edit the footprint*
    //! rather than *move the part*, nothing more.
    //!
    //! The fixture pair's provenance — which bytes KiCad wrote and which were
    //! added by hand — is in `tests/fixtures/drc_ownership_j1.README.md`.

    use super::*;

    const BOARD: &str = include_str!("../../tests/fixtures/drc_ownership_j1.kicad_pcb");
    const REPORT: &str = include_str!("../../tests/fixtures/drc_ownership_j1.drc.json");

    /// UUIDs KiCad itself wrote, quoted from the fixture.
    const J1_FOOTPRINT: &str = "b432574a-bdcd-4387-8d5e-65f34938c3a0";
    const J1_PEG_CIRCLE: &str = "7b970478-1e4a-48b6-b01a-35348027ca5e";
    const J1_PAD_1: &str = "5bc25fc3-1886-4e08-a602-7e08cc66255e";
    /// UUIDs of the hand-added board-level items (see the fixture README).
    const BOARD_OUTLINE: &str = "e0000000-0000-4000-8000-000000000004";
    const BOARD_SILK: &str = "50000000-0000-4000-8000-000000000001";
    const UNKNOWN: &str = "ffffffff-ffff-4fff-8fff-ffffffffffff";

    fn enriched() -> DrcReport {
        let raw: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        let mut report = parse_drc_report(&raw).unwrap();
        enrich_drc_items(&mut report, BOARD);
        report
    }

    fn by_uuid<'a>(report: &'a DrcReport, uuid: &str) -> &'a ReportItem {
        report
            .all()
            .flat_map(|violation| violation.items.iter())
            .find(|item| item.uuid.as_deref() == Some(uuid))
            .unwrap_or_else(|| panic!("no report item carries uuid {uuid}"))
    }

    fn footprint_j1() -> ItemOwner {
        ItemOwner::Footprint {
            reference: Some("J1".to_string()),
            uuid: Some(J1_FOOTPRINT.to_string()),
        }
    }

    /// The reported case. `"Circle of J1 on Edge.Cuts"` is prose; the structured
    /// answer has to come from the board, and it has to say J1.
    #[test]
    fn a_footprint_owned_edge_cuts_circle_names_its_footprint() {
        let report = enriched();
        let circle = by_uuid(&report, J1_PEG_CIRCLE);

        assert_eq!(circle.description, "Circle of J1 on Edge.Cuts");
        assert_eq!(circle.ownership_status, Some(OwnershipStatus::Resolved));
        assert_eq!(circle.item_kind.as_deref(), Some("fp_circle"));
        assert_eq!(circle.layer.as_deref(), Some("Edge.Cuts"));
        assert_eq!(circle.owner, Some(Some(footprint_j1())));
    }

    /// The other half of the violation. Pad and cutout belonging to the same
    /// footprint is exactly what makes "move J1" the wrong advice: they move
    /// together, so their mutual clearance cannot change.
    #[test]
    fn a_pad_names_the_footprint_that_carries_it() {
        let report = enriched();
        let pad = by_uuid(&report, J1_PAD_1);

        assert_eq!(pad.description, "Pad 1 [GND] of J1 on F.Cu");
        assert_eq!(pad.ownership_status, Some(OwnershipStatus::Resolved));
        assert_eq!(pad.item_kind.as_deref(), Some("pad"));
        assert_eq!(pad.owner, Some(Some(footprint_j1())));

        let circle = by_uuid(&report, J1_PEG_CIRCLE);
        assert_eq!(
            pad.owner, circle.owner,
            "same footprint owns both items of this violation"
        );
    }

    /// The board's real outline sits beside J1's cutouts on the same layer,
    /// with the same node shape. Only ownership separates them.
    #[test]
    fn the_boards_own_outline_is_board_owned() {
        let report = enriched();
        let outline = by_uuid(&report, BOARD_OUTLINE);

        assert_eq!(outline.ownership_status, Some(OwnershipStatus::Resolved));
        assert_eq!(outline.item_kind.as_deref(), Some("gr_line"));
        assert_eq!(outline.layer.as_deref(), Some("Edge.Cuts"));
        assert_eq!(outline.owner, Some(Some(ItemOwner::Board)));

        let circle = by_uuid(&report, J1_PEG_CIRCLE);
        assert_eq!(outline.layer, circle.layer, "same layer, opposite remedy");
        assert_ne!(outline.owner, circle.owner);
    }

    /// An unrelated board graphic, on a different layer, is board-owned too —
    /// ownership follows the tree, not the layer.
    #[test]
    fn an_unrelated_board_graphic_is_board_owned() {
        let report = enriched();
        let silk = by_uuid(&report, BOARD_SILK);

        assert_eq!(silk.ownership_status, Some(OwnershipStatus::Resolved));
        assert_eq!(silk.layer.as_deref(), Some("F.SilkS"));
        assert_eq!(silk.owner, Some(Some(ItemOwner::Board)));
    }

    /// KiCad reported an item with no `uuid`. There is nothing to look up, so
    /// the answer is "unresolved", said out loud — never a board default.
    #[test]
    fn an_item_without_a_uuid_stays_explicitly_unresolved() {
        let report = enriched();
        let orphan = report
            .all()
            .flat_map(|violation| violation.items.iter())
            .find(|item| item.uuid.is_none())
            .expect("the fixture carries one item with no uuid");

        assert_eq!(orphan.ownership_status, Some(OwnershipStatus::UuidMissing));
        assert_eq!(orphan.owner, Some(None), "explicitly null, not board");
        assert_eq!(orphan.item_kind, None);
        assert_eq!(orphan.layer, None);
        assert_eq!(
            serde_json::to_value(orphan).unwrap()["owner"],
            serde_json::Value::Null
        );
    }

    /// A UUID the board does not carry — a stale report, or a board saved after
    /// the run. Also unresolved, and distinguishable from the case above.
    #[test]
    fn an_unknown_uuid_stays_explicitly_unresolved() {
        let report = enriched();
        let stale = by_uuid(&report, UNKNOWN);

        assert_eq!(stale.ownership_status, Some(OwnershipStatus::NotFound));
        assert_eq!(stale.owner, Some(None), "explicitly null, not board");
        assert_eq!(stale.item_kind, None);
    }

    /// No ownership answer is ever derived from `"Circle of J1"`. Rename every
    /// reference designator in the board and the same items stop resolving to a
    /// footprint reference, because only the UUID index is consulted.
    #[test]
    fn ownership_never_comes_from_the_description() {
        let renamed = BOARD.replace("\"Reference\" \"J1\"", "\"Reference\" \"J9\"");
        assert_ne!(renamed, BOARD, "the fixture must contain J1's reference");

        let raw: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        let mut report = parse_drc_report(&raw).unwrap();
        enrich_drc_items(&mut report, &renamed);

        let circle = by_uuid(&report, J1_PEG_CIRCLE);
        assert_eq!(
            circle.description, "Circle of J1 on Edge.Cuts",
            "KiCad's prose is passed through untouched"
        );
        assert_eq!(
            circle.owner,
            Some(Some(ItemOwner::Footprint {
                reference: Some("J9".to_string()),
                uuid: Some(J1_FOOTPRINT.to_string()),
            })),
            "the reference comes from the board, never from the description"
        );
    }

    /// The raw KiCad text is API. Enrichment is additive or it is a breaking
    /// change wearing a feature's clothes.
    #[test]
    fn the_raw_kicad_fields_are_untouched() {
        let raw: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        let plain = parse_drc_report(&raw).unwrap();
        let report = enriched();

        for (before, after) in plain.all().zip(report.all()) {
            assert_eq!(before.description, after.description);
            assert_eq!(before.rule, after.rule);
            assert_eq!(before.severity, after.severity);
            assert_eq!(before.items.len(), after.items.len());
            for (before, after) in before.items.iter().zip(after.items.iter()) {
                assert_eq!(before.description, after.description);
                assert_eq!(before.uuid, after.uuid);
                assert_eq!(
                    before.pos.map(|p| (p.x, p.y)),
                    after.pos.map(|p| (p.x, p.y))
                );
            }
        }
    }

    /// The exact JSON a caller sees. Named fields, `owner: null` where
    /// unresolved, and nothing removed from what shipped before.
    #[test]
    fn the_response_shape_is_additive() {
        let report = enriched();

        assert_eq!(
            serde_json::to_value(by_uuid(&report, J1_PEG_CIRCLE)).unwrap(),
            serde_json::json!({
                "description": "Circle of J1 on Edge.Cuts",
                "pos": { "x": 136.19, "y": 93.375 },
                "uuid": J1_PEG_CIRCLE,
                "ownership_status": "resolved",
                "item_kind": "fp_circle",
                "layer": "Edge.Cuts",
                "owner": { "kind": "footprint", "reference": "J1", "uuid": J1_FOOTPRINT },
            })
        );
        assert_eq!(
            serde_json::to_value(by_uuid(&report, BOARD_OUTLINE)).unwrap(),
            serde_json::json!({
                "description": "Segment on Edge.Cuts",
                "pos": { "x": 120.0, "y": 100.0 },
                "uuid": BOARD_OUTLINE,
                "ownership_status": "resolved",
                "item_kind": "gr_line",
                "layer": "Edge.Cuts",
                "owner": { "kind": "board" },
            })
        );
        assert_eq!(
            serde_json::to_value(by_uuid(&report, UNKNOWN)).unwrap(),
            serde_json::json!({
                "description": "Track [SDA] on F.Cu",
                "pos": { "x": 159.9, "y": 57.0 },
                "uuid": UNKNOWN,
                "ownership_status": "not_found",
                "owner": serde_json::Value::Null,
            })
        );
    }

    /// ERC shares `ReportItem` and has no board to index. Its items must
    /// serialise exactly as they did before this change.
    #[test]
    fn erc_items_carry_no_ownership_fields() {
        let item = parse_report_item(&serde_json::json!({
            "description": "Symbol R1 Pin 1",
            "pos": { "x": 1.0, "y": 2.0 },
            "uuid": "erc-item"
        }));

        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            serde_json::json!({
                "description": "Symbol R1 Pin 1",
                "pos": { "x": 1.0, "y": 2.0 },
                "uuid": "erc-item",
            }),
            "an unenriched item must not grow keys"
        );

        let no_uuid = parse_report_item(&serde_json::json!({ "description": "Pin" }));
        assert_eq!(
            serde_json::to_value(&no_uuid).unwrap(),
            serde_json::json!({ "description": "Pin", "pos": serde_json::Value::Null })
        );
    }

    /// `unconnected_items` and `schematic_parity` are DRC findings too. Missing
    /// them would reintroduce the split this change exists to close.
    #[test]
    fn every_drc_category_is_enriched() {
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/drc_report_kicad10.json"))
                .unwrap();
        let mut report = parse_drc_report(&raw).unwrap();
        assert!(!report.unconnected_items.as_ref().unwrap().is_empty());
        enrich_drc_items(&mut report, BOARD);

        let statuses: Vec<_> = report
            .all()
            .flat_map(|violation| violation.items.iter())
            .map(|item| item.ownership_status)
            .collect();
        assert!(!statuses.is_empty());
        assert!(
            statuses.iter().all(|status| status.is_some()),
            "every item of every category is answered: {statuses:?}"
        );
        // That report came from a different board, so nothing resolves — which
        // is the honest answer, not a board default.
        assert!(statuses
            .iter()
            .all(|status| *status == Some(OwnershipStatus::NotFound)));
    }

    /// A board Konnect cannot parse must not cost the caller their DRC results.
    /// The items come back exactly as KiCad reported them, with no ownership
    /// keys at all — which is also how a caller tells "unanswered" from
    /// "answered: unresolved".
    #[test]
    fn an_unparseable_board_leaves_the_report_untouched() {
        let raw: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        let mut report = parse_drc_report(&raw).unwrap();
        enrich_drc_items(&mut report, "");

        let item = by_uuid(&report, J1_PEG_CIRCLE);
        assert_eq!(item.ownership_status, None);
        assert_eq!(item.owner, None);
        assert_eq!(
            serde_json::to_value(item).unwrap(),
            serde_json::json!({
                "description": "Circle of J1 on Edge.Cuts",
                "pos": { "x": 136.19, "y": 93.375 },
                "uuid": J1_PEG_CIRCLE,
            })
        );
    }

    /// The same assertions against a report `kicad-cli` produces right now,
    /// rather than one committed months ago. Needs a real KiCad, so it is
    /// ignored like the other live tests:
    ///
    ///     cargo test -p konnect-core --lib drc_ownership -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs a real kicad-cli on PATH"]
    async fn run_drc_enriches_items_from_the_board_it_ran_on() {
        let tmp = tempfile::tempdir().unwrap();
        let board = tmp.path().join("drc_ownership_j1.kicad_pcb");
        std::fs::write(&board, BOARD).unwrap();

        let cli = std::env::var("KICAD_CLI").unwrap_or_else(|_| "kicad-cli".to_string());
        let report = run_drc(&cli, &board, false)
            .await
            .expect("kicad-cli pcb drc on the fixture board");

        let items: Vec<&ReportItem> = report
            .all()
            .flat_map(|violation| violation.items.iter())
            .collect();
        assert!(!items.is_empty(), "the fixture board is not DRC-clean");
        assert!(
            items.iter().all(|item| item.ownership_status.is_some()),
            "run_drc must answer ownership for every item it returns"
        );

        let j1 = ItemOwner::Footprint {
            reference: Some("J1".to_string()),
            uuid: Some(J1_FOOTPRINT.to_string()),
        };
        assert!(
            items
                .iter()
                .any(|item| item.owner == Some(Some(j1.clone()))),
            "J1's own Edge.Cuts cutouts and pads are footprint-owned"
        );
        for item in &items {
            if item.uuid.as_deref() == Some(J1_FOOTPRINT) {
                continue;
            }
            if let Some(Some(ItemOwner::Board)) = &item.owner {
                assert!(
                    item.item_kind.as_deref() != Some("fp_circle"),
                    "a footprint's circle can never be board-owned: {item:?}"
                );
            }
        }
    }
}
