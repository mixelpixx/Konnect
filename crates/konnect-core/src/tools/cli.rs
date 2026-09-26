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
use konnect_sexp::board::{ItemOwner, UuidIndexEntry};
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

/// Everything `run_erc` answers with: the violations, and what was done to
/// their coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErcReport {
    pub violations: Vec<ErcViolation>,
    pub coordinates: ErcCoordinates,
}

/// What happened to the coordinates in an ERC report, and why.
///
/// KiCad's ERC *JSON* writer divided every `pos` by 100, from the release that
/// introduced it (8.0) through 10.0.6: it formatted schematic internal units
/// through `pcbIUScale` (1e6 IU/mm) where the text report used `schIUScale`
/// (1e4 IU/mm). Upstream fixed exactly that line in `6d8e1fe` for 10.0.7
/// ([kicad#25582]). `pcb drc` has its own writer and never shared the fault,
/// so the correction is scoped to this one reader (#541).
///
/// [kicad#25582]: https://gitlab.com/kicad/code/kicad/-/issues/25582
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErcCoordinates {
    pub status: ErcCoordinateStatus,
    /// The version that wrote this report, as the report itself states it.
    /// That is the binary which produced these numbers, which is what the
    /// classification needs — not whatever `kicad-cli --version` answers now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kicad_version: Option<String>,
    /// What every coordinate was multiplied by. Only set for `corrected`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_applied: Option<f64>,
    pub reason: String,
}

/// Whether an ERC report's coordinates can be believed, after checking the
/// version that wrote them against the scaling defect described on
/// [`ErcCoordinates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErcCoordinateStatus {
    /// A version proven affected wrote the report; every coordinate was
    /// multiplied back by 100.
    Corrected,
    /// A version with the upstream fix wrote it; the coordinates are KiCad's
    /// own, untouched.
    Verbatim,
    /// The writing version could not be classified, so no coordinate is
    /// reported as a location. KiCad's own numbers are kept in
    /// [`ReportItem::kicad_reported_pos`], unscaled, for diagnosis.
    Withheld,
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
    /// The position exactly as KiCad wrote it, kept only when the ERC report's
    /// version could not be classified against the scaling defect and `pos`
    /// was therefore withheld. Never set on the DRC path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kicad_reported_pos: Option<ReportPos>,
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
    /// The same UUID appears on more than one board item, so file order cannot
    /// be used to choose an owner.
    Ambiguous,
    /// The board could not be reread or parsed, so no ownership lookup was
    /// possible.
    Unavailable,
}

impl ReportItem {
    /// Resolve this item against a board's UUID index, in place.
    ///
    /// Exact UUID match or nothing: an unmatched item is left explicitly
    /// unresolved rather than defaulted to `board`, because "we could not tell"
    /// and "the board owns it" lead to opposite repairs.
    fn resolve_ownership(&mut self, index: &HashMap<String, UuidIndexEntry>) {
        let Some(uuid) = self.uuid.as_deref() else {
            self.ownership_status = Some(OwnershipStatus::UuidMissing);
            self.owner = Some(None);
            return;
        };
        match index.get(uuid) {
            Some(UuidIndexEntry::Unique(identity)) => {
                self.ownership_status = Some(OwnershipStatus::Resolved);
                self.item_kind = Some(identity.item_kind.clone());
                self.layer = identity.layer.clone();
                self.owner = Some(Some(identity.owner.clone()));
            }
            Some(UuidIndexEntry::Ambiguous) => {
                self.ownership_status = Some(OwnershipStatus::Ambiguous);
                self.owner = Some(None);
            }
            None => {
                self.ownership_status = Some(OwnershipStatus::NotFound);
                self.owner = Some(None);
            }
        }
    }

    /// Put an ERC coordinate back where it belongs, or withhold it, according
    /// to what the report's version is worth. The DRC path never calls this.
    ///
    /// The correction is rounded to nanometre-scale precision because
    /// `0.6985 × 100.0` is `69.85000000000001` in binary floating point, and a
    /// coordinate that reads as 12 decimals of a millimetre invites a caller
    /// to believe a precision KiCad never had: the schematic grid is 1e4 IU/mm.
    fn apply_erc_coordinates(&mut self, coordinates: &ErcCoordinates) {
        fn corrected(value: f64) -> f64 {
            const ROUND_TO: f64 = 1e6;
            (value * ERC_JSON_SCALE_CORRECTION * ROUND_TO).round() / ROUND_TO
        }

        match coordinates.status {
            ErcCoordinateStatus::Verbatim => {}
            ErcCoordinateStatus::Corrected => {
                if let Some(pos) = self.pos.as_mut() {
                    pos.x = corrected(pos.x);
                    pos.y = corrected(pos.y);
                }
            }
            ErcCoordinateStatus::Withheld => self.kicad_reported_pos = self.pos.take(),
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
    /// `None` also when the parity test could not compare anything: kicad-cli
    /// writes an *empty* array when no schematic exists beside the board,
    /// which is not a checked zero (#516). `schematic_parity_diagnostic` says
    /// which of the two it was.
    pub schematic_parity: Option<Vec<DrcViolation>>,
    /// Why ownership enrichment was unavailable for this report. Absent when
    /// enrichment completed normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership_diagnostic: Option<String>,
    /// Why `schematic_parity` is `None` although the parity test was requested:
    /// there was no schematic beside the board for kicad-cli to compare against.
    /// Absent when parity was checked, or when this kicad-cli never reported the
    /// category at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schematic_parity_diagnostic: Option<String>,
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

    fn mark_ownership_unavailable(&mut self, reason: String) {
        self.ownership_diagnostic = Some(reason);
        for item in self
            .all_mut()
            .flat_map(|violation| violation.items.iter_mut())
        {
            item.ownership_status = Some(OwnershipStatus::Unavailable);
            item.item_kind = None;
            item.layer = None;
            item.owner = Some(None);
        }
    }
}

// ─── KiCAD CLI Runner ─────────────────────────────────────────────────────────

/// Turn the configured `kicad_cli` value into something `Command::new` can
/// actually start.
///
/// Turn the configured `kicad_cli` value into something `Command::new` can
/// actually start.
///
/// The default configuration is the bare filename, and KiCad 10's installer
/// does not put `kicad-cli` on PATH, so on a stock Windows install every
/// kicad-cli-backed tool failed until the user hand-edited the config (#460).
/// #344 taught `kicad_install` where KiCad lives, but only the library
/// resolver asked it; this spawn path kept passing the raw string through,
/// which is why the failure reproduced on a build that already had #344.
///
/// Three cases, and they are deliberately not one:
///
/// - **Empty** means "no kicad-cli". It stays empty so the spawn fails with
///   the familiar error. Sixty-odd test fixtures rely on that sentinel, and a
///   user who blanks the value has said something, not nothing.
/// - **A bare name** (the default) is discovered: PATH first, then the known
///   install locations, via `kicad_install::find_cli`. This is the #460 fix.
/// - **An explicit path** is the user's decision and passes through untouched.
///   If it does not exist the spawn fails on *that* path, so a typo is
///   reported rather than quietly replaced by a different KiCad — running a
///   binary the user did not name and reporting its results as theirs is the
///   request-versus-result defect this project keeps finding elsewhere.
pub(crate) fn resolve_cli_executable(configured: &str) -> std::path::PathBuf {
    let configured = configured.trim();
    let path = std::path::PathBuf::from(configured);
    if configured.is_empty() || path.components().count() > 1 {
        return path;
    }
    match crate::kicad_install::find_cli(configured) {
        Some(found) => {
            if found.as_os_str() != configured {
                info!("kicad-cli resolved to {}", found.display());
            }
            found
        }
        None => path,
    }
}

/// What a kicad-cli run wrote. `stderr` is where KiCad states what it could
/// not do while still exiting 0 — the parity test's "Failed to fetch schematic
/// netlist" is one such statement — so a caller that must not mistake a silent
/// skip for a clean result reads it.
struct CliOutput {
    stdout: String,
    stderr: String,
}

/// Run a kicad-cli command with arguments and capture stdout.
async fn run_cli(cli: &str, args: &[&str], timeout_dur: Duration) -> Result<String> {
    Ok(run_cli_captured(cli, args, timeout_dur).await?.stdout)
}

/// [`run_cli`], keeping stderr as well.
async fn run_cli_captured(cli: &str, args: &[&str], timeout_dur: Duration) -> Result<CliOutput> {
    info!("[BETA] kicad-cli {} {}", cli, args.join(" "));

    let exe = resolve_cli_executable(cli);
    let mut cmd = Command::new(&exe);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

    #[cfg(test)]
    let child = test_support::spawn_cli(&mut cmd)
        .with_context(|| format!("Failed to spawn kicad-cli: {}", cli))?;
    #[cfg(not(test))]
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

    Ok(CliOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
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

/// Publish generated bytes through the same verified, replacement-safe boundary
/// used by kicad-cli exports. Vendor adapters use this after structurally
/// translating a native KiCad artifact in memory.
pub(crate) async fn publish_verified_bytes(
    output: &Path,
    bytes: &[u8],
    artifact: &str,
) -> Result<()> {
    let staging = export_staging_dir(output)?;
    let staged = staging
        .path()
        .join(output.file_name().context("output has no file name")?);
    tokio::fs::write(&staged, bytes).await?;
    publish_verified_file(&staged, output, artifact).await?;
    Ok(())
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

    // Linux returns ETXTBSY (errno 26) when a parallel test forks while a
    // freshly written fake CLI still has a writer inherited across the fork.
    // Production never executes a file it just created, so keep this bounded
    // mitigation inside test support rather than hiding real kicad-cli errors.
    const ETXTBSY_RAW_OS_ERROR: i32 = 26;
    const TEST_CLI_SPAWN_ATTEMPTS: usize = 5;
    const TEST_CLI_SPAWN_RETRY_DELAY: Duration = Duration::from_millis(10);

    fn spawn_with_executable_busy_retry<T>(
        mut spawn: impl FnMut() -> std::io::Result<T>,
        mut pause: impl FnMut(Duration),
    ) -> std::io::Result<T> {
        for attempt in 1..=TEST_CLI_SPAWN_ATTEMPTS {
            match spawn() {
                Err(error)
                    if error.raw_os_error() == Some(ETXTBSY_RAW_OS_ERROR)
                        && attempt < TEST_CLI_SPAWN_ATTEMPTS =>
                {
                    pause(TEST_CLI_SPAWN_RETRY_DELAY);
                }
                result => return result,
            }
        }
        unreachable!("the final spawn attempt always returns")
    }

    pub(crate) fn spawn_cli(cmd: &mut Command) -> std::io::Result<tokio::process::Child> {
        spawn_with_executable_busy_retry(|| cmd.spawn(), std::thread::sleep)
    }

    pub(crate) fn write_script(
        dir: &Path,
        stem: &str,
        unix_body: &str,
        windows_body: &str,
    ) -> PathBuf {
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

    #[test]
    fn fake_cli_spawn_retries_only_executable_busy_errors() {
        let mut attempts = 0;
        let mut pauses = Vec::new();
        let result = spawn_with_executable_busy_retry(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(std::io::Error::from_raw_os_error(26))
                } else {
                    Ok("spawned")
                }
            },
            |delay| pauses.push(delay),
        );

        assert_eq!(result.unwrap(), "spawned");
        assert_eq!(attempts, 3);
        assert_eq!(pauses.len(), 2);

        let mut other_attempts = 0;
        let other_error = spawn_with_executable_busy_retry(
            || -> std::io::Result<()> {
                other_attempts += 1;
                Err(std::io::Error::from_raw_os_error(2))
            },
            |_| panic!("non-ETXTBSY errors must not be retried"),
        )
        .unwrap_err();
        assert_eq!(other_error.raw_os_error(), Some(2));
        assert_eq!(other_attempts, 1);

        let mut busy_attempts = 0;
        let mut busy_pauses = 0;
        let busy_error = spawn_with_executable_busy_retry(
            || -> std::io::Result<()> {
                busy_attempts += 1;
                Err(std::io::Error::from_raw_os_error(ETXTBSY_RAW_OS_ERROR))
            },
            |_| busy_pauses += 1,
        )
        .unwrap_err();
        assert_eq!(busy_error.raw_os_error(), Some(ETXTBSY_RAW_OS_ERROR));
        assert_eq!(busy_attempts, TEST_CLI_SPAWN_ATTEMPTS);
        assert_eq!(busy_pauses, TEST_CLI_SPAWN_ATTEMPTS - 1);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn fake_cli_spawn_survives_a_real_executable_busy_window() {
        let dir = tempfile::tempdir().unwrap();
        let cli = write_script(
            dir.path(),
            "temporarily-busy-kicad-cli",
            "#!/bin/sh\nprintf 'ready'\n",
            "",
        );
        let writer = std::fs::OpenOptions::new().write(true).open(&cli).unwrap();
        let release_writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(15));
            drop(writer);
        });

        let output = run_cli(cli.to_str().unwrap(), &[], Duration::from_secs(1))
            .await
            .expect("the fake CLI should start after the inherited writer closes");
        release_writer.join().unwrap();

        assert_eq!(output, "ready");
    }
}

// ─── ERC ─────────────────────────────────────────────────────────────────────

/// Ratio between KiCad's PCB and schematic internal-unit scales — 1e6 against
/// 1e4 IU/mm — which is exactly what the defective ERC JSON writer divided
/// every coordinate by.
const ERC_JSON_SCALE_CORRECTION: f64 = 100.0;

/// First KiCad release whose ERC JSON writer reports true coordinates.
const ERC_JSON_SCALE_FIXED_IN: (u64, u64, u64) = (10, 0, 7);

/// KiCad's development branches carry minor version 99 — 10.99.0 is the work
/// toward 11.0. Only the branch whose major matches the fixed release is
/// undatable: the fix reached master and the 10.0 branch on the same day, so a
/// 10.99 nightly may sit on either side of it. Every other development branch
/// is wholly before or wholly after, and its version orders like any other.
const KICAD_DEVELOPMENT_MINOR: u64 = 99;

/// Run ERC on a schematic and return parsed violations.
/// KiCAD 10: `sch erc --output <path> --format json <input>`
pub async fn run_erc(cli: &str, schematic: &Path) -> Result<ErcReport> {
    run_erc_with_temp_root(cli, schematic, None).await
}

async fn run_erc_with_temp_root(
    cli: &str,
    schematic: &Path,
    temp_root: Option<&Path>,
) -> Result<ErcReport> {
    let report_dir = match temp_root {
        Some(root) => tempfile::Builder::new()
            .prefix("konnect-erc-")
            .tempdir_in(root),
        None => tempfile::Builder::new().prefix("konnect-erc-").tempdir(),
    }
    .context("failed to create temporary ERC report directory")?;
    let out_path = report_dir.path().join("report.json");

    let operation = async {
        let args = [
            "sch",
            "erc",
            "--output",
            out_path
                .to_str()
                .context("temporary ERC path is not UTF-8")?,
            "--format",
            "json",
            // kicad-cli defaults to millimetres, but the response states the
            // unit, so it is pinned here rather than inherited.
            "--units",
            "mm",
            schematic.to_str().context("schematic path is not UTF-8")?,
        ];
        run_cli(cli, &args, LONG_TIMEOUT).await?;

        let json_str = tokio::fs::read_to_string(&out_path)
            .await
            .context("ERC output file not found")?;
        let raw: serde_json::Value = serde_json::from_str(&json_str)?;
        Ok(parse_erc_json(&raw))
    }
    .await;

    let cleanup = report_dir
        .close()
        .context("failed to clean up temporary ERC report directory");
    match (operation, cleanup) {
        (Ok(violations), Ok(())) => Ok(violations),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(operation_error), Err(cleanup_error)) => Err(operation_error.context(format!(
            "also failed to clean up the temporary ERC report: {cleanup_error:#}"
        ))),
    }
}

/// Decide what the coordinates of a report written by `kicad_version` are
/// worth. A version that cannot be placed on either side of the upstream fix
/// yields `Withheld`: a coordinate that may be off by 100× cannot be told from
/// a true one by the caller, so none is offered.
fn classify_erc_coordinates(kicad_version: Option<&str>) -> ErcCoordinates {
    let (fixed_major, fixed_minor, fixed_patch) = ERC_JSON_SCALE_FIXED_IN;
    let fixed_in = format!("{fixed_major}.{fixed_minor}.{fixed_patch}");
    // Every withheld reason ends the same way, because the caller's next move
    // is the same whichever way the version defeated the check.
    let withheld = |cause: String| ErcCoordinates {
        status: ErcCoordinateStatus::Withheld,
        kicad_version: kicad_version.map(String::from),
        scale_applied: None,
        reason: format!(
            "{cause} Coordinates are in kicad_reported_x/kicad_reported_y, exactly as KiCad \
             wrote them, and may be {ERC_JSON_SCALE_CORRECTION}× too small."
        ),
    };

    let Some(version) = kicad_version else {
        return withheld(format!(
            "The ERC report names no kicad_version, so it cannot be placed against the \
             coordinate scaling defect KiCad fixed in {fixed_in} (kicad#25582)."
        ));
    };
    let Some((major, minor, patch)) = parse_kicad_version(version) else {
        return withheld(format!(
            "The ERC report's kicad_version '{version}' is not a major.minor.patch version, so \
             it cannot be placed against the coordinate scaling defect KiCad fixed in \
             {fixed_in} (kicad#25582)."
        ));
    };
    if (major, minor) == (fixed_major, KICAD_DEVELOPMENT_MINOR) {
        return withheld(format!(
            "KiCad {version} is a development build of the branch the fix for the ERC coordinate \
             scaling defect (kicad#25582) landed on mid-cycle, so the version cannot say whether \
             this build predates it."
        ));
    }

    let kicad_version = Some(version.to_string());
    if (major, minor, patch) < ERC_JSON_SCALE_FIXED_IN {
        ErcCoordinates {
            status: ErcCoordinateStatus::Corrected,
            kicad_version,
            scale_applied: Some(ERC_JSON_SCALE_CORRECTION),
            reason: format!(
                "KiCad {version} writes every ERC JSON coordinate at 1/{ERC_JSON_SCALE_CORRECTION} \
                 of its true value (kicad#25582, fixed in {fixed_in}); Konnect multiplied it \
                 back. A length quoted inside KiCad's own violation text is scaled the same way \
                 and is left as KiCad wrote it."
            ),
        }
    } else {
        ErcCoordinates {
            status: ErcCoordinateStatus::Verbatim,
            kicad_version,
            scale_applied: None,
            reason: format!(
                "KiCad {version} carries the fix for the ERC JSON coordinate scaling defect \
                 (kicad#25582, fixed in {fixed_in}), so its coordinates are passed through \
                 unchanged."
            ),
        }
    }
}

/// Read KiCad's `major.minor.patch` version, which is how both the ERC and DRC
/// reports spell the version that wrote them. Anything else is no version:
/// guessing at a shape KiCad does not write would be guessing at the fix
/// boundary too.
fn parse_kicad_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let mut next = || {
        parts
            .next()
            .filter(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|part| part.parse::<u64>().ok())
    };
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

pub(crate) fn parse_erc_json(raw: &serde_json::Value) -> ErcReport {
    // KiCAD's ERC report (https://schemas.kicad.org/erc.v1.json) nests
    // violations per sheet — { "sheets": [ { "path": …, "violations": […] } ] }
    // — with positions on the affected items. There is no top-level
    // "violations" key (that's the DRC report's shape), so reading one here
    // silently returned zero violations for every schematic.
    let coordinates = classify_erc_coordinates(raw.get("kicad_version").and_then(|v| v.as_str()));
    let Some(sheets) = raw.get("sheets").and_then(|s| s.as_array()) else {
        return ErcReport {
            violations: Vec::new(),
            coordinates,
        };
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
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            let mut item = parse_report_item(item);
                            item.apply_erc_coordinates(&coordinates);
                            item
                        })
                        .collect()
                })
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
    ErcReport {
        violations: out,
        coordinates,
    }
}

/// Decode one item of an ERC or DRC violation — the two reports spell it the
/// same way.
fn parse_report_item(item: &serde_json::Value) -> ReportItem {
    ReportItem {
        description: item["description"].as_str().unwrap_or("").to_string(),
        pos: parse_item_pos(item),
        uuid: item["uuid"].as_str().map(String::from),
        // Set on the ERC path only, by `apply_erc_coordinates`, and only when
        // the report's version left `pos` unusable.
        kicad_reported_pos: None,
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
/// KiCAD 10: `pcb drc --output <path> --format json --schematic-parity
/// [--refill-zones] <input>`
pub async fn run_drc(cli: &str, pcb: &Path, refill_zones: bool) -> Result<DrcReport> {
    let out_path = pcb.with_extension("drc.json");
    let args = drc_args(
        out_path.to_str().unwrap(),
        refill_zones,
        pcb.to_str().unwrap(),
    );
    let output = run_cli_captured(cli, &args, LONG_TIMEOUT).await?;

    let json_str = tokio::fs::read_to_string(&out_path)
        .await
        .context("DRC output file not found")?;
    let raw: serde_json::Value = serde_json::from_str(&json_str)?;
    let _ = tokio::fs::remove_file(&out_path).await;

    let mut report = parse_drc_report(&raw)?;
    apply_parity_evidence(&mut report, &output.stderr, pcb);

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
            Err(error) => {
                let reason = format!("could not re-read {}: {error}", pcb.display());
                warn!("[BETA] DRC ownership enrichment unavailable: {reason}");
                report.mark_ownership_unavailable(reason);
            }
        }
    }

    Ok(report)
}

/// The `kicad-cli pcb drc` argument list. Split out so a test can pin what is
/// sent without running kicad-cli.
///
/// `--schematic-parity` is always requested: without it KiCad 10 still writes
/// the `schematic_parity` key, as an empty array, so the parser read "never
/// asked" as "checked, none found" and a board whose footprints disagreed with
/// its schematic in 129 places reported parity `0` (#516).
fn drc_args<'a>(out_path: &'a str, refill_zones: bool, pcb: &'a str) -> Vec<&'a str> {
    let mut args = vec![
        "pcb",
        "drc",
        "--output",
        out_path,
        "--format",
        "json",
        "--schematic-parity",
    ];
    if refill_zones {
        args.push("--refill-zones");
    }
    args.push(pcb);
    args
}

/// KiCad's own statement that the parity test compared nothing. Written to
/// stderr, with exit 0 and an empty `schematic_parity` array, whenever the
/// board's project has no root schematic to read.
const PARITY_NOT_RUN: &str = "Failed to fetch schematic netlist for parity tests";

/// The schematic kicad-cli's parity test reads: the root of the board's own
/// project, which shares the board's file stem — the same project↔root
/// relation `resolve_schematic_ownership` uses (`<project>.kicad_pro` ↔
/// `<project>.kicad_sch`). Measured on KiCad 10: the root beside a board is
/// read with or without its `.kicad_pro`, and a project of a different name
/// in the same directory is *not* consulted (KiCad reports
/// [`PARITY_NOT_RUN`] and writes an empty array). So this names one path and
/// never scans the directory: a root found under another project's name
/// would be a schematic KiCad did not compare against.
fn parity_root_schematic(pcb: &Path) -> std::path::PathBuf {
    pcb.with_extension("kicad_pro").with_extension("kicad_sch")
}

/// Turn kicad-cli's "nothing to compare" back into "not checked".
///
/// With `--schematic-parity` and no root schematic for the board's project,
/// kicad-cli exits 0, prints [`PARITY_NOT_RUN`] to stderr, and writes
/// `"schematic_parity": []` — the same silent zero #245 removed from Konnect's
/// side, now produced by KiCad. KiCad's own statement is the signal, not a
/// filesystem guess; the expected root path only names what was missing.
///
/// A **non-empty** array is KiCad's evidence and is kept whatever stderr or
/// the filesystem say: findings cannot be un-found by a lookup failing.
fn apply_parity_evidence(report: &mut DrcReport, stderr: &str, pcb: &Path) {
    let Some(parity) = report.schematic_parity.as_ref() else {
        return;
    };
    if !parity.is_empty() || !stderr.contains(PARITY_NOT_RUN) {
        return;
    }
    let expected = parity_root_schematic(pcb);
    let root_state = if expected.is_file() {
        "which exists but could not be read for parity"
    } else {
        "which does not exist"
    };
    report.schematic_parity = None;
    report.schematic_parity_diagnostic = Some(format!(
        "kicad-cli reported \"{PARITY_NOT_RUN}\" for {}: the parity test reads the project's root \
         schematic {} ({root_state}), so the empty parity result it wrote is not a checked zero",
        pcb.display(),
        expected.display()
    ));
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
            let reason = format!("board did not parse: {error}");
            warn!("[BETA] DRC ownership enrichment unavailable: {reason}");
            report.mark_ownership_unavailable(reason);
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
        ownership_diagnostic: None,
        schematic_parity_diagnostic: None,
    })
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
#[cfg(test)]
fn bom_args<'a>(output: &'a str, schematic: &'a str, options: &BomOptions<'a>) -> Vec<&'a str> {
    bom_args_with_ref_range_delimiter(output, schematic, options, None)
}

fn bom_args_with_ref_range_delimiter<'a>(
    output: &'a str,
    schematic: &'a str,
    options: &BomOptions<'a>,
    ref_range_delimiter: Option<&'a str>,
) -> Vec<&'a str> {
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
    if let Some(delimiter) = ref_range_delimiter {
        args.push("--ref-range-delimiter");
        args.push(delimiter);
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
    export_bom_with_options(cli, schematic, output, options, None).await
}

/// Export a BOM while overriding KiCad's reference-range delimiter. This is
/// intentionally crate-private so vendor policy does not expand the public
/// Rust API used by generic callers.
pub(crate) async fn export_bom_with_ref_range_delimiter(
    cli: &str,
    schematic: &Path,
    output: &Path,
    options: &BomOptions<'_>,
    delimiter: &str,
) -> Result<()> {
    export_bom_with_options(cli, schematic, output, options, Some(delimiter)).await
}

async fn export_bom_with_options(
    cli: &str,
    schematic: &Path,
    output: &Path,
    options: &BomOptions<'_>,
    ref_range_delimiter: Option<&str>,
) -> Result<()> {
    let staging = export_staging_dir(output)?;
    let staged = staging
        .path()
        .join(output.file_name().context("BOM output has no file name")?);
    let args = bom_args_with_ref_range_delimiter(
        staged.to_str().unwrap_or(""),
        schematic.to_str().unwrap_or(""),
        options,
        ref_range_delimiter,
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
    exclude_dnp: bool,
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
    if exclude_dnp {
        args.push("--exclude-dnp");
    }
    args.push(pcb);
    args
}

/// KiCad 10: `pcb export pos --output <path> --format <fmt> --side <side>
/// [--units <units>] [--exclude-dnp] <input>`
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
    export_position_file_with_options(cli, pcb, output, format, units, side, false).await
}

/// Export a position file while asking KiCad to omit DNP footprints. Kept
/// crate-private because this policy is currently specific to matched vendor
/// assembly packages.
pub(crate) async fn export_position_file_excluding_dnp(
    cli: &str,
    pcb: &Path,
    output: &Path,
    format: &str,
    units: &str,
    side: &str,
) -> Result<()> {
    export_position_file_with_options(cli, pcb, output, format, units, side, true).await
}

async fn export_position_file_with_options(
    cli: &str,
    pcb: &Path,
    output: &Path,
    format: &str,
    units: &str,
    side: &str,
    exclude_dnp: bool,
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
        exclude_dnp,
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

    /// The scoping control for #541: `pcb drc` has its own report writer and
    /// never carried the ÷100 defect, so its coordinates must reach the caller
    /// exactly as KiCad wrote them. This fixture is a real KiCad 10.0.0
    /// report, a version the ERC correction *does* apply to, so a correction
    /// that leaked out of the ERC reader would move these numbers.
    #[test]
    fn drc_coordinates_are_never_rescaled() {
        let raw = real_report();
        let report = parse_drc_report(&raw).unwrap();

        let expected = |index: usize| {
            let pos = &raw["violations"][index]["items"][0]["pos"];
            (pos["x"].as_f64().unwrap(), pos["y"].as_f64().unwrap())
        };
        // Restated from the fixture, which is KiCad's own output: the board's
        // first violation sits at these millimetres.
        assert_eq!(expected(0), (121.285, 136.525));

        for (index, violation) in report.violations.iter().enumerate() {
            let pos = violation.pos.as_ref().expect("items[0].pos");
            assert_eq!((pos.x, pos.y), expected(index));
        }
        for violation in report.all() {
            for item in &violation.items {
                let pos = item.pos.as_ref().expect("every item here has one");
                assert!(
                    pos.x > 1.0 && pos.y > 1.0,
                    "a scaled coordinate would be two orders of magnitude smaller: {pos:?}"
                );
                assert!(
                    item.kicad_reported_pos.is_none(),
                    "withholding is an ERC-only state"
                );
            }
        }
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

    /// #516: the parity test is opt-in on the kicad-cli side. Without the flag
    /// KiCad writes the key as an empty array, which #245's parser correctly
    /// read as a checked zero — so the flag must be on every invocation, and
    /// this pins it at the argv level, where it disappeared.
    #[test]
    fn drc_always_asks_kicad_for_schematic_parity() {
        for refill in [false, true] {
            let args = drc_args("/out/board.drc.json", refill, "/in/board.kicad_pcb");
            let flag = args
                .iter()
                .position(|argument| *argument == "--schematic-parity")
                .expect("--schematic-parity is sent");
            // Flags before the positional input file, as kicad-cli expects.
            assert_eq!(args.last(), Some(&"/in/board.kicad_pcb"));
            assert!(flag < args.len() - 1);
            assert_eq!(args.contains(&"--refill-zones"), refill);
            assert_eq!(&args[..2], ["pcb", "drc"]);
        }
    }

    const NOT_RUN_STDERR: &str = "Failed to fetch schematic netlist for parity tests.\n\
                                   Schematic parity tests require a fully annotated schematic.\n";

    /// Measured on KiCad 10: no root schematic for the board's project → exit
    /// 0, this stderr, `"schematic_parity": []`. It must come back as "not
    /// checked", with KiCad's statement and the root it would have read.
    #[test]
    fn kicads_parity_not_run_statement_makes_an_empty_array_unchecked() {
        let mut report = parse_drc_report(&real_report()).unwrap();
        assert_eq!(report.schematic_parity.as_ref().map(Vec::len), Some(0));
        let pcb = Path::new("/nowhere/lone.kicad_pcb");

        apply_parity_evidence(&mut report, NOT_RUN_STDERR, pcb);

        assert!(report.schematic_parity.is_none());
        assert!(report.missing_categories().contains(&"schematic_parity"));
        let reason = report.schematic_parity_diagnostic.as_deref().unwrap();
        assert!(reason.contains(PARITY_NOT_RUN), "{reason}");
        assert!(reason.contains("lone.kicad_sch"), "{reason}");
        assert!(reason.contains("does not exist"), "{reason}");
        // The other categories are untouched.
        assert_eq!(report.violations.len(), 2);
        assert_eq!(report.unconnected_items.as_ref().map(Vec::len), Some(2));
    }

    /// A non-empty array is KiCad's evidence and survives whatever the lookup
    /// says — even a stderr that claims the test did not run.
    #[test]
    fn a_non_empty_parity_array_is_kept_whatever_the_lookup_says() {
        let mut raw = real_report();
        raw["schematic_parity"] = raw["violations"].clone();
        let mut report = parse_drc_report(&raw).unwrap();
        assert_eq!(report.schematic_parity.as_ref().map(Vec::len), Some(2));

        apply_parity_evidence(
            &mut report,
            NOT_RUN_STDERR,
            Path::new("/nowhere/lone.kicad_pcb"),
        );

        assert_eq!(report.schematic_parity.as_ref().map(Vec::len), Some(2));
        assert!(report.schematic_parity_diagnostic.is_none());
        assert!(!report.missing_categories().contains(&"schematic_parity"));
    }

    /// No statement from KiCad and an empty array is a real checked zero.
    #[test]
    fn a_silent_empty_parity_array_is_a_checked_zero() {
        let mut report = parse_drc_report(&real_report()).unwrap();

        apply_parity_evidence(&mut report, "", Path::new("/somewhere/board.kicad_pcb"));

        assert_eq!(report.schematic_parity.as_ref().map(Vec::len), Some(0));
        assert!(report.schematic_parity_diagnostic.is_none());
    }

    /// A kicad-cli that never reported the category stays "not reported" and
    /// gains no diagnostic claiming a schematic was missing.
    #[test]
    fn a_missing_parity_category_is_left_alone() {
        let mut report = parse_drc_report(&serde_json::json!({ "violations": [] })).unwrap();

        apply_parity_evidence(&mut report, NOT_RUN_STDERR, Path::new("/x/board.kicad_pcb"));

        assert!(report.schematic_parity.is_none());
        assert!(report.schematic_parity_diagnostic.is_none());
    }

    /// The root the parity test reads is the board's own project's, through
    /// the project↔root stem relation; a project of another name beside the
    /// board is not it (measured: KiCad does not consult it).
    #[test]
    fn the_parity_root_is_the_boards_own_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let board = tmp.path().join("ecc83-pp.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb)").unwrap();
        std::fs::write(tmp.path().join("ecc83-pp.kicad_pro"), "{}").unwrap();
        std::fs::write(tmp.path().join("ecc83-pp.kicad_sch"), "(kicad_sch)").unwrap();
        std::fs::write(tmp.path().join("other.kicad_pro"), "{}").unwrap();
        std::fs::write(tmp.path().join("other.kicad_sch"), "(kicad_sch)").unwrap();

        let root = parity_root_schematic(&board);
        assert_eq!(root, tmp.path().join("ecc83-pp.kicad_sch"));
        assert!(root.is_file());

        let renamed = tmp.path().join("renamed.kicad_pcb");
        let root = parity_root_schematic(&renamed);
        assert_eq!(root, tmp.path().join("renamed.kicad_sch"));
        assert!(
            !root.is_file(),
            "another project's root is not this board's"
        );
    }

    /// Live, against KiCad's own ecc83 demo: with its schematic beside the
    /// board the parity test reports real `footprint_symbol_mismatch` items
    /// (six on KiCad 10.0), and the same board copied alone reports the
    /// category as unchecked, not zero.
    ///
    ///     KICAD_CLI=C:/KiCad/10.0/bin/kicad-cli.exe \
    ///     KICAD_DEMOS=C:/KiCad/10.0/share/kicad/demos \
    ///     cargo test -p konnect-core --lib drc_parse_tests::live -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs a real kicad-cli and the bundled KiCad demos"]
    async fn live_parity_is_checked_when_the_schematic_is_beside_the_board() {
        let cli = std::env::var("KICAD_CLI").unwrap_or_else(|_| "kicad-cli".to_string());
        let demos = std::env::var("KICAD_DEMOS")
            .unwrap_or_else(|_| "C:/KiCad/10.0/share/kicad/demos".to_string());
        let demo = Path::new(&demos).join("ecc83");
        let tmp = tempfile::tempdir().unwrap();

        let with_sch = tmp.path().join("with_sch");
        std::fs::create_dir(&with_sch).unwrap();
        for name in [
            "ecc83-pp.kicad_pcb",
            "ecc83-pp.kicad_sch",
            "ecc83-pp.kicad_pro",
            "ecc83-pp.kicad_sym",
            "fp-lib-table",
        ] {
            std::fs::copy(demo.join(name), with_sch.join(name)).unwrap();
        }
        let report = run_drc(&cli, &with_sch.join("ecc83-pp.kicad_pcb"), false)
            .await
            .expect("kicad-cli pcb drc on the demo with its schematic");
        let parity = report.schematic_parity.as_ref().expect("parity checked");
        assert!(!parity.is_empty(), "KiCad's own demo has parity findings");
        assert!(parity.iter().any(|v| v.rule == "footprint_symbol_mismatch"));
        assert!(report.schematic_parity_diagnostic.is_none());

        let lone = tmp.path().join("lone");
        std::fs::create_dir(&lone).unwrap();
        std::fs::copy(
            demo.join("ecc83-pp.kicad_pcb"),
            lone.join("ecc83-pp.kicad_pcb"),
        )
        .unwrap();
        let report = run_drc(&cli, &lone.join("ecc83-pp.kicad_pcb"), false)
            .await
            .expect("kicad-cli pcb drc on the lone board");
        assert!(report.schematic_parity.is_none());
        assert!(report.missing_categories().contains(&"schematic_parity"));
        assert!(report
            .schematic_parity_diagnostic
            .as_deref()
            .is_some_and(|reason| reason.contains(PARITY_NOT_RUN)));

        // A project of another name beside the board is not consulted.
        let renamed = tmp.path().join("renamed");
        std::fs::create_dir(&renamed).unwrap();
        for name in [
            "ecc83-pp.kicad_pro",
            "ecc83-pp.kicad_sch",
            "ecc83-pp.kicad_sym",
            "fp-lib-table",
        ] {
            std::fs::copy(demo.join(name), renamed.join(name)).unwrap();
        }
        std::fs::copy(
            demo.join("ecc83-pp.kicad_pcb"),
            renamed.join("other.kicad_pcb"),
        )
        .unwrap();
        let report = run_drc(&cli, &renamed.join("other.kicad_pcb"), false)
            .await
            .expect("kicad-cli pcb drc on the renamed board");
        assert!(report.schematic_parity.is_none());
        assert!(report
            .schematic_parity_diagnostic
            .as_deref()
            .is_some_and(|reason| reason.contains("other.kicad_sch")));
    }
}

#[cfg(test)]
mod erc_parse_tests {
    use super::*;

    fn erc_cli(
        dir: &Path,
        stem: &str,
        observed_path: &Path,
        report: Option<&str>,
        exit_code: i32,
    ) -> PathBuf {
        let observed = observed_path.display();
        let unix_report = report
            .map(|contents| format!("printf '%s' '{contents}' > \"$4\"\n"))
            .unwrap_or_default();
        let windows_report = report
            .map(|contents| format!("> \"%~4\" echo {contents}\r\n"))
            .unwrap_or_default();
        test_support::write_script(
            dir,
            stem,
            &format!(
                "#!/bin/sh\nprintf '%s' \"$4\" > \"{observed}\"\n{unix_report}exit {exit_code}\n"
            ),
            &format!(
                "@echo off\r\n> \"{observed}\" echo %~4\r\n{windows_report}exit /b {exit_code}\r\n"
            ),
        )
    }

    fn assert_empty(directory: &Path) {
        assert_eq!(
            std::fs::read_dir(directory).unwrap().count(),
            0,
            "temporary ERC directory should contain no artifacts"
        );
    }

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

    #[tokio::test]
    async fn erc_report_never_touches_the_project_directory() {
        let project = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        let schematic = project.path().join("clock.kicad_sch");
        let legacy_report = schematic.with_extension("erc.json");
        std::fs::write(&schematic, "(kicad_sch)").unwrap();
        std::fs::write(&legacy_report, b"user-owned report").unwrap();

        let observed = control.path().join("observed.txt");
        let cli = erc_cli(
            control.path(),
            "erc-success",
            &observed,
            Some(r#"{"sheets":[]}"#),
            0,
        );

        #[cfg(unix)]
        let original_permissions = {
            use std::os::unix::fs::PermissionsExt;
            let original = std::fs::metadata(project.path()).unwrap().permissions();
            let mut read_only = original.clone();
            read_only.set_mode(0o555);
            std::fs::set_permissions(project.path(), read_only).unwrap();
            original
        };

        let result =
            run_erc_with_temp_root(cli.to_str().unwrap(), &schematic, Some(scratch.path())).await;

        #[cfg(unix)]
        std::fs::set_permissions(project.path(), original_permissions).unwrap();

        assert!(result.unwrap().violations.is_empty());
        assert_eq!(std::fs::read(&legacy_report).unwrap(), b"user-owned report");
        let report_path = PathBuf::from(std::fs::read_to_string(observed).unwrap().trim());
        assert!(report_path.starts_with(scratch.path()));
        assert_ne!(report_path.parent(), Some(project.path()));
        assert!(!report_path.exists(), "temporary report should be removed");
        assert_empty(scratch.path());
    }

    #[tokio::test]
    async fn concurrent_erc_runs_use_distinct_report_paths() {
        let project = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        let schematic = project.path().join("clock.kicad_sch");
        std::fs::write(&schematic, "(kicad_sch)").unwrap();

        let observed_a = control.path().join("observed-a.txt");
        let observed_b = control.path().join("observed-b.txt");
        let cli_a = erc_cli(
            control.path(),
            "erc-a",
            &observed_a,
            Some(r#"{"sheets":[]}"#),
            0,
        );
        let cli_b = erc_cli(
            control.path(),
            "erc-b",
            &observed_b,
            Some(r#"{"sheets":[]}"#),
            0,
        );

        let (result_a, result_b) = tokio::join!(
            run_erc_with_temp_root(cli_a.to_str().unwrap(), &schematic, Some(scratch.path())),
            run_erc_with_temp_root(cli_b.to_str().unwrap(), &schematic, Some(scratch.path()))
        );
        result_a.unwrap();
        result_b.unwrap();

        let path_a = PathBuf::from(std::fs::read_to_string(observed_a).unwrap().trim());
        let path_b = PathBuf::from(std::fs::read_to_string(observed_b).unwrap().trim());
        assert_ne!(path_a, path_b);
        assert!(path_a.starts_with(scratch.path()));
        assert!(path_b.starts_with(scratch.path()));
        assert_empty(scratch.path());
    }

    #[tokio::test]
    async fn temporary_report_is_cleaned_after_cli_and_parse_failures() {
        let project = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        let schematic = project.path().join("clock.kicad_sch");
        std::fs::write(&schematic, "(kicad_sch)").unwrap();

        let failed_cli = erc_cli(
            control.path(),
            "erc-fails",
            &control.path().join("failed-path.txt"),
            None,
            7,
        );
        run_erc_with_temp_root(
            failed_cli.to_str().unwrap(),
            &schematic,
            Some(scratch.path()),
        )
        .await
        .expect_err("CLI failure must be returned");
        assert_empty(scratch.path());

        let malformed_cli = erc_cli(
            control.path(),
            "erc-malformed",
            &control.path().join("malformed-path.txt"),
            Some("not-json"),
            0,
        );
        run_erc_with_temp_root(
            malformed_cli.to_str().unwrap(),
            &schematic,
            Some(scratch.path()),
        )
        .await
        .expect_err("malformed report must be returned");
        assert_empty(scratch.path());
    }

    #[test]
    fn parses_violations_nested_under_sheets() {
        let violations = parse_erc_json(&real_report()).violations;
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
        // 1.0033 in the report; the fixture was written by KiCad 10.0.3, which
        // is inside the scaled range, so the sheet position is 100.33 mm.
        assert!((pos.x - 100.33).abs() < 1e-9);
        assert_eq!(violations[1].severity, "warning");
    }

    /// A `pin_to_pin` violation names both conflicting pins, and the second is
    /// regularly the one that explains the first — here the regulator output
    /// that makes the `PWR_FLAG` redundant. Keeping only `items[0]` sent the
    /// caller back to `kicad-cli` by hand.
    #[test]
    fn every_item_of_a_violation_survives() {
        let conflict = &parse_erc_json(&real_report()).violations[2];
        assert_eq!(conflict.items.len(), 2);
        assert!(conflict.items[0].description.contains("#PWR031"));
        let explains = &conflict.items[1];
        assert!(explains.description.contains("U2 Pin 5"));
        assert!((explains.pos.as_ref().expect("item position").y - 101.6).abs() < 1e-9);
        assert_eq!(
            explains.uuid.as_deref(),
            Some("5b6a1f42-2c17-4f0b-9a6e-8c3f7d21e0a4")
        );
    }

    /// `type` is the addressable key; `description` beside it is prose.
    #[test]
    fn violations_carry_kicads_rule_key() {
        let violations = parse_erc_json(&real_report()).violations;
        assert_eq!(violations[0].rule, "pin_not_connected");
        assert_eq!(violations[2].rule, "pin_to_pin");
    }

    /// The violation description predates `items` and callers read it, so a
    /// second item must not change what it says.
    #[test]
    fn the_description_still_names_the_first_item_only() {
        let conflict = &parse_erc_json(&real_report()).violations[2];
        assert!(conflict.description.contains("#PWR031"));
        assert!(!conflict.description.contains("U2"));
    }

    /// KiCad omits `pos` and `uuid` on some item kinds; that must not drop the
    /// item, whose description is still the only thing naming the offender.
    #[test]
    fn an_item_without_a_position_is_still_reported() {
        let violations = parse_erc_json(&serde_json::json!({
            "kicad_version": "10.0.6",
            "sheets": [{
                "path": "/",
                "violations": [{
                    "description": "Label not connected",
                    "items": [{ "description": "Label VIN" }],
                    "severity": "warning",
                    "type": "label_dangling"
                }]
            }]
        }))
        .violations;
        assert_eq!(violations[0].items.len(), 1);
        assert!(violations[0].items[0].pos.is_none());
        assert!(violations[0].items[0].uuid.is_none());
        assert!(violations[0].description.contains("Label VIN"));
    }

    /// KiCad 10.0.6's own ERC JSON for the committed `single_pin_nets`
    /// hierarchy, and — the oracle — its own *text* report of the same run,
    /// whose coordinates were never affected. Provenance and the item table
    /// are in `erc_coordinate_scale.README.md`.
    const AFFECTED_REPORT: &str =
        include_str!("../../tests/fixtures/erc_coordinate_scale_kicad10_0_6.json");
    const AFFECTED_TEXT_REPORT: &str =
        include_str!("../../tests/fixtures/erc_coordinate_scale_kicad10_0_6.rpt");
    /// A KiCad carrying the upstream fix writing the same hierarchy: its JSON
    /// report, and that same run's text report. Captured from a build of the
    /// 10.0 branch, which is where the fix lives until 10.0.7 ships — see the
    /// README for the build and for why its version had to be stamped.
    const FIXED_REPORT: &str =
        include_str!("../../tests/fixtures/erc_coordinate_scale_kicad10_0_7.json");
    const FIXED_TEXT_REPORT: &str =
        include_str!("../../tests/fixtures/erc_coordinate_scale_kicad10_0_7.rpt");
    /// The same branch build with upstream's version left as it is: it
    /// carries the fix but stamps its report `10.0.6`.
    const UNRELEASED_BRANCH_REPORT: &str =
        include_str!("../../tests/fixtures/erc_coordinate_scale_kicad10_branch.json");

    /// The release upstream fixed kicad#25582 in. Restated here as the promise
    /// the tests hold Konnect to, deliberately not read from the constant the
    /// implementation gates on.
    const FIRST_FIXED_KICAD: &str = "10.0.7";

    /// Every `@(x mm, y mm): description` line of a KiCad ERC text report, in
    /// file order — which is the order the JSON report lists the same items
    /// in. Deliberately parsed here rather than transcribed, so the
    /// expectations stay KiCad's numbers.
    fn text_report_items(report: &str) -> Vec<(f64, f64, String)> {
        report
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("@(")?;
                let (x, rest) = rest.split_once(" mm, ")?;
                let (y, description) = rest.split_once(" mm): ")?;
                Some((
                    x.parse().ok()?,
                    y.parse().ok()?,
                    description.trim().to_string(),
                ))
            })
            .collect()
    }

    fn reported_items(violations: &[ErcViolation]) -> Vec<&ReportItem> {
        violations.iter().flat_map(|v| v.items.iter()).collect()
    }

    /// The whole point of the correction: what `run_erc` reports has to be
    /// findable on the sheet, which is what KiCad's own text report says it is.
    #[test]
    fn an_affected_reports_coordinates_are_put_back_where_kicad_says_they_are() {
        let report = parse_erc_json(&serde_json::from_str(AFFECTED_REPORT).unwrap());
        let oracle = text_report_items(AFFECTED_TEXT_REPORT);
        assert_eq!(oracle.len(), 12, "the text report names 12 items");

        assert_eq!(report.coordinates.status, ErcCoordinateStatus::Corrected);
        assert_eq!(report.coordinates.kicad_version.as_deref(), Some("10.0.6"));
        assert_eq!(report.coordinates.scale_applied, Some(100.0));
        assert!(report.coordinates.reason.contains(FIRST_FIXED_KICAD));

        let items = reported_items(&report.violations);
        assert_eq!(items.len(), oracle.len());
        for (item, (x, y, description)) in items.iter().zip(&oracle) {
            let pos = item
                .pos
                .as_ref()
                .expect("every item in this report has one");
            assert!(
                (pos.x - x).abs() < 1e-9 && (pos.y - y).abs() < 1e-9,
                "{} reported at ({}, {}), text report says ({x}, {y})",
                item.description,
                pos.x,
                pos.y
            );
            assert!(item.kicad_reported_pos.is_none(), "nothing was withheld");
            // KiCad scales the measurement inside its own violation text the
            // same way, and Konnect does not rewrite KiCad's prose — so the
            // three wire items keep the description KiCad wrote.
            if !description.contains("length") {
                assert_eq!(&item.description, description);
            }
        }
        assert_eq!(
            reported_items(&report.violations)
                .iter()
                .filter(|item| item.description.contains("length 0.1270 mm"))
                .count(),
            3,
            "the scaled-down lengths in KiCad's prose are left alone"
        );
    }

    /// The fixture is only evidence while it still shows the defect: if a
    /// later capture is dropped in unscaled, the test above would pass by
    /// doing nothing.
    #[test]
    fn the_affected_fixture_still_carries_the_defect() {
        let raw: serde_json::Value = serde_json::from_str(AFFECTED_REPORT).unwrap();
        let first = &raw["sheets"][0]["violations"][0]["items"][0]["pos"];
        assert_eq!(first["x"].as_f64(), Some(0.6985));
        assert_eq!(first["y"].as_f64(), Some(1.8034));
        let (x, y, _) = text_report_items(AFFECTED_TEXT_REPORT)[0].clone();
        assert_eq!((x, y), (69.85, 180.34));
    }

    /// A fixed KiCad's numbers are already right; touching them would be the
    /// same bug with the sign flipped.
    #[test]
    fn a_fixed_kicads_coordinates_are_passed_through_untouched() {
        let raw: serde_json::Value = serde_json::from_str(FIXED_REPORT).unwrap();
        let report = parse_erc_json(&raw);

        assert_eq!(report.coordinates.status, ErcCoordinateStatus::Verbatim);
        assert_eq!(
            report.coordinates.kicad_version.as_deref(),
            Some(FIRST_FIXED_KICAD)
        );
        assert_eq!(report.coordinates.scale_applied, None);

        // Two oracles, neither of them this crate. The fixed JSON report has
        // to agree with the text report of its own run — same binary, but a
        // writer the fix did not touch — and with the 10.0.6 text report,
        // written months earlier by a different binary. Nothing but the true
        // geometry satisfies both.
        let own_oracle = text_report_items(FIXED_TEXT_REPORT);
        let affected_oracle = text_report_items(AFFECTED_TEXT_REPORT);
        assert_eq!(
            own_oracle, affected_oracle,
            "the fix did not touch the text writer, so both releases report the same geometry"
        );

        let items = reported_items(&report.violations);
        assert_eq!(items.len(), own_oracle.len());
        for (item, (x, y, _)) in items.iter().zip(&own_oracle) {
            let pos = item
                .pos
                .as_ref()
                .expect("every item in this report has one");
            assert!((pos.x - x).abs() < 1e-9 && (pos.y - y).abs() < 1e-9);
            assert!(item.kicad_reported_pos.is_none());
        }
    }

    /// Known limitation, outside the supported contract by maintainer decision
    /// on #541: only released builds are covered. An unreleased 10.0-branch
    /// build writes true coordinates but stamps them `10.0.6`, so they are
    /// scaled like an affected release. Pinned so the limit cannot change
    /// unnoticed in either direction.
    #[test]
    fn an_unreleased_branch_build_stamped_as_the_affected_release_is_scaled() {
        let raw: serde_json::Value = serde_json::from_str(UNRELEASED_BRANCH_REPORT).unwrap();
        assert_eq!(raw["kicad_version"], "10.0.6");
        let report = parse_erc_json(&raw);
        assert_eq!(report.coordinates.status, ErcCoordinateStatus::Corrected);
        assert_eq!(report.coordinates.scale_applied, Some(100.0));

        let oracle = text_report_items(AFFECTED_TEXT_REPORT);
        let items = reported_items(&report.violations);
        assert_eq!(items.len(), oracle.len());
        for (item, (x, y, _)) in items.iter().zip(&oracle) {
            let pos = item
                .pos
                .as_ref()
                .expect("every item in this report has one");
            assert!(
                (pos.x - x * 100.0).abs() < 1e-6 && (pos.y - y * 100.0).abs() < 1e-6,
                "{} reported at ({}, {}), expected 100x the true ({x}, {y})",
                item.description,
                pos.x,
                pos.y
            );
        }
    }

    /// Three ways a version cannot be placed against the fix. None of them may
    /// produce a location — a coordinate that might be 100× out cannot be told
    /// from a true one by the caller — and all of them keep KiCad's own number.
    #[test]
    fn an_unclassifiable_version_withholds_the_location_and_keeps_kicads_number() {
        let mut raw: serde_json::Value = serde_json::from_str(AFFECTED_REPORT).unwrap();
        for version in [
            serde_json::Value::Null,
            // A development build of the branch the fix landed on mid-cycle:
            // it reached master and 10.0 on the same day, so 10.99 alone
            // cannot date the build.
            serde_json::json!("10.99.0"),
            serde_json::json!("10.0"),
            serde_json::json!("10.0.6-rc1"),
        ] {
            match &version {
                serde_json::Value::Null => {
                    raw.as_object_mut().unwrap().remove("kicad_version");
                }
                version => raw["kicad_version"] = version.clone(),
            }

            let report = parse_erc_json(&raw);
            assert_eq!(
                report.coordinates.status,
                ErcCoordinateStatus::Withheld,
                "{version:?}"
            );
            assert_eq!(report.coordinates.scale_applied, None, "{version:?}");
            assert!(
                report.coordinates.reason.contains("kicad_reported_x"),
                "the reason has to say where the number went: {}",
                report.coordinates.reason
            );

            for item in reported_items(&report.violations) {
                assert!(item.pos.is_none(), "{version:?}");
            }
            let first = reported_items(&report.violations)[0]
                .kicad_reported_pos
                .expect("KiCad's own number is kept for diagnosis");
            assert_eq!((first.x, first.y), (0.6985, 1.8034), "{version:?}");
        }
    }

    /// The boundary itself, stated as the release notes state it rather than
    /// as the code spells it.
    #[test]
    fn the_correction_covers_every_release_before_the_upstream_fix() {
        // 9.99 is the development branch that became 10.0, so it is wholly
        // before the fix; 11.99 became 12.0 and is wholly after. Only 10.99,
        // the branch the fix landed on, is undatable — see the test above.
        for affected in ["8.0.0", "9.0.0", "9.99.0", "10.0.0", "10.0.5", "10.0.6"] {
            assert_eq!(
                classify_erc_coordinates(Some(affected)).status,
                ErcCoordinateStatus::Corrected,
                "{affected}"
            );
        }
        for fixed in [FIRST_FIXED_KICAD, "10.0.8", "10.1.0", "11.0.0", "11.99.0"] {
            assert_eq!(
                classify_erc_coordinates(Some(fixed)).status,
                ErcCoordinateStatus::Verbatim,
                "{fixed}"
            );
        }
    }

    #[test]
    fn empty_or_alien_reports_yield_no_violations() {
        assert!(parse_erc_json(&serde_json::json!({})).violations.is_empty());
        assert!(parse_erc_json(&serde_json::json!({ "sheets": [] }))
            .violations
            .is_empty());
        // DRC-shaped input (top-level violations) is not an ERC report.
        assert!(
            parse_erc_json(&serde_json::json!({ "violations": [{ "severity": "error" }] }))
                .violations
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
            false,
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
            false,
        );
        assert_eq!(flag(&args, "--format"), Some("gerber"));
        assert_eq!(flag(&args, "--side"), Some("front"));
        assert_eq!(flag(&args, "--units"), None);
    }

    #[test]
    fn dnp_exclusion_reaches_kicad_cli_only_when_requested() {
        let excluded = position_args(
            "/out/positions.csv",
            "/board.kicad_pcb",
            "csv",
            "mm",
            "both",
            true,
        );
        let included = position_args(
            "/out/positions.csv",
            "/board.kicad_pcb",
            "csv",
            "mm",
            "both",
            false,
        );
        assert!(excluded.contains(&"--exclude-dnp"));
        assert!(!included.contains(&"--exclude-dnp"));
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

    /// JLCPCB compares individual BOM references with individual CPL rows; a
    /// KiCad range such as C3-C18 is one opaque, unmatched designator there.
    #[test]
    fn an_empty_reference_range_delimiter_reaches_kicad_cli() {
        let options = BomOptions::default();
        let args =
            bom_args_with_ref_range_delimiter("/out/bom.csv", "/s.kicad_sch", &options, Some(""));
        let index = args
            .iter()
            .position(|arg| *arg == "--ref-range-delimiter")
            .expect("range delimiter flag");
        assert_eq!(args[index + 1], "");

        let without_guard = bom_args("/out/bom.csv", "/s.kicad_sch", &BomOptions::default());
        assert!(!without_guard.contains(&"--ref-range-delimiter"));
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

    #[test]
    fn a_duplicate_uuid_is_ambiguous_not_file_order_truth() {
        let duplicated = BOARD.replace(BOARD_OUTLINE, J1_PEG_CIRCLE);
        let raw: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        let mut report = parse_drc_report(&raw).unwrap();
        enrich_drc_items(&mut report, &duplicated);

        let item = by_uuid(&report, J1_PEG_CIRCLE);
        assert_eq!(item.ownership_status, Some(OwnershipStatus::Ambiguous));
        assert_eq!(item.owner, Some(None));
        assert_eq!(item.item_kind, None);
        assert_eq!(item.layer, None);
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
        report
            .schematic_parity
            .as_mut()
            .expect("fixture must expose the category")
            .push(DrcViolation {
                severity: "error".to_string(),
                description: "constructed parity finding".to_string(),
                rule: "schematic_parity".to_string(),
                pos: None,
                items: vec![parse_report_item(&serde_json::json!({
                    "description": "constructed parity item",
                    "uuid": UNKNOWN
                }))],
            });
        assert!(!report.schematic_parity.as_ref().unwrap().is_empty());
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

    /// A board Konnect cannot parse must not cost the caller their DRC results,
    /// but the failed enrichment must remain visible in the response.
    #[test]
    fn an_unparseable_board_marks_ownership_unavailable() {
        let raw: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        let mut report = parse_drc_report(&raw).unwrap();
        enrich_drc_items(&mut report, "");

        let item = by_uuid(&report, J1_PEG_CIRCLE);
        assert_eq!(item.ownership_status, Some(OwnershipStatus::Unavailable));
        assert_eq!(item.owner, Some(None));
        assert!(report
            .ownership_diagnostic
            .as_deref()
            .is_some_and(|reason| reason.contains("board did not parse")));
        assert_eq!(
            serde_json::to_value(item).unwrap(),
            serde_json::json!({
                "description": "Circle of J1 on Edge.Cuts",
                "pos": { "x": 136.19, "y": 93.375 },
                "uuid": J1_PEG_CIRCLE,
                "ownership_status": "unavailable",
                "owner": serde_json::Value::Null,
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

#[cfg(test)]
mod cli_discovery_tests {
    use super::resolve_cli_executable;
    use std::path::PathBuf;

    /// Empty is the "no kicad-cli" sentinel and must survive resolution:
    /// discovering a real binary here would turn every fixture that says
    /// "unavailable" into one that runs DRC for real on a developer machine.
    #[test]
    fn empty_stays_empty() {
        assert_eq!(resolve_cli_executable(""), PathBuf::from(""));
        assert_eq!(resolve_cli_executable("   "), PathBuf::from(""));
    }

    /// An explicit path to an existing file is used as-is.
    #[test]
    fn an_explicit_existing_path_is_returned_unchanged() {
        let me = std::env::current_exe().expect("test binary path");
        let configured = me.to_string_lossy().to_string();
        assert_eq!(resolve_cli_executable(&configured), me);
    }

    /// An explicit path that does not exist is the user's mistake to see, not
    /// ours to paper over with whatever KiCad happens to be installed.
    #[test]
    fn an_explicit_missing_path_is_not_replaced_by_a_discovered_install() {
        let missing = if cfg!(windows) {
            "C:/definitely/not/here/kicad-cli.exe"
        } else {
            "/definitely/not/here/kicad-cli"
        };
        assert_eq!(resolve_cli_executable(missing), PathBuf::from(missing));
    }

    /// A bare name that resolves to nothing falls through to whatever KiCad is
    /// discoverable, and only with no KiCad anywhere comes back untouched. The
    /// result is never a phantom: it is the input or a file that exists.
    #[test]
    fn a_bare_unresolvable_name_is_the_input_or_a_real_discovered_file() {
        let bogus = "definitely-not-a-real-kicad-cli-binary-4f2a.exe";
        let resolved = resolve_cli_executable(bogus);
        assert!(
            resolved.as_os_str() == bogus || resolved.is_file(),
            "resolved to a path that neither is the input nor exists: {}",
            resolved.display()
        );
    }

    /// The case #460 is about: the bare default name on a machine where KiCad
    /// is installed but not on PATH. Skips silently where KiCad is absent, so
    /// CI on a runner without KiCad does not fail for the wrong reason.
    #[test]
    fn the_bare_default_name_resolves_to_an_installed_kicad_cli() {
        let bare = if cfg!(windows) {
            "kicad-cli.exe"
        } else {
            "kicad-cli"
        };
        let resolved = resolve_cli_executable(bare);
        if resolved.as_os_str() == bare {
            eprintln!("SKIP: no KiCad installation discoverable on this machine");
            return;
        }
        assert!(
            resolved.is_absolute(),
            "resolved to a relative path: {}",
            resolved.display()
        );
        assert!(
            resolved.is_file(),
            "resolved path is not a file: {}",
            resolved.display()
        );
    }
}
