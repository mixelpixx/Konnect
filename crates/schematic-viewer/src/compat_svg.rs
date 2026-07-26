//! Isolated KiCad SVG fallback for native sheets with unsupported primitives.
//!
//! Export always runs against a disposable hierarchy snapshot. It never opens
//! the live schematic with `kicad-cli`, so it cannot create a lock beside the
//! user's files or race an atomic editor commit.

use crate::native_scene::HierarchyEntry;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static SNAPSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn load_or_export(
    root: &Path,
    hierarchy: &[HierarchyEntry],
    schematic: &Path,
) -> Result<String, String> {
    if let Some(svg) = crate::svg_order_cache::load_fresh(schematic) {
        return Ok(svg);
    }

    let snapshot = snapshot_dir();
    let result = export_snapshot(&snapshot, root, hierarchy, schematic);
    let _ = fs::remove_dir_all(&snapshot);
    let svg = result?;
    crate::svg_order_cache::store(schematic, &svg)
        .map_err(|error| format!("could not cache KiCad fallback: {error}"))?;
    Ok(svg)
}

fn export_snapshot(
    snapshot: &Path,
    root: &Path,
    hierarchy: &[HierarchyEntry],
    schematic: &Path,
) -> Result<String, String> {
    let project_dir = root
        .parent()
        .ok_or_else(|| "schematic has no project directory".to_owned())?;
    fs::create_dir_all(snapshot).map_err(|error| format!("snapshot directory: {error}"))?;

    let mut snapshot_target = None;
    for entry in hierarchy {
        let relative = entry
            .file
            .strip_prefix(project_dir)
            .unwrap_or_else(|_| Path::new(entry.file.file_name().unwrap_or_default()));
        let target = snapshot.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("snapshot hierarchy directory: {error}"))?;
        }
        fs::copy(&entry.file, &target)
            .map_err(|error| format!("snapshot {}: {error}", entry.file.display()))?;
        if same_path(&entry.file, schematic) {
            snapshot_target = Some(target);
        }
    }

    copy_project_sidecars(project_dir, snapshot)?;
    let snapshot_target = snapshot_target.ok_or_else(|| {
        format!(
            "sheet {} is absent from the discovered hierarchy",
            schematic.display()
        )
    })?;
    let output_dir = snapshot.join("rendered");
    fs::create_dir_all(&output_dir).map_err(|error| format!("render directory: {error}"))?;
    let cli = resolve_kicad_cli();
    let mut command = Command::new(&cli);
    command.args(["sch", "export", "svg", "--output"]);
    command.arg(&output_dir).arg(&snapshot_target);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command
        .output()
        .map_err(|error| format!("failed to run {cli}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!("kicad-cli SVG export failed: {stderr}"));
    }
    let stem = snapshot_target
        .file_stem()
        .ok_or_else(|| "snapshot sheet has no file stem".to_owned())?;
    let svg_path = output_dir.join(stem).with_extension("svg");
    fs::read_to_string(&svg_path)
        .map_err(|error| format!("read fallback {}: {error}", svg_path.display()))
}

fn copy_project_sidecars(project_dir: &Path, snapshot: &Path) -> Result<(), String> {
    let entries = fs::read_dir(project_dir)
        .map_err(|error| format!("read project directory {}: {error}", project_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let copy = path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name == "sym-lib-table"
                || name == "fp-lib-table"
                || matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("kicad_pro" | "kicad_sym")
                )
        });
        if copy {
            fs::copy(&path, snapshot.join(path.file_name().unwrap_or_default()))
                .map_err(|error| format!("snapshot sidecar {}: {error}", path.display()))?;
        }
    }
    Ok(())
}

fn resolve_kicad_cli() -> String {
    if let Some(cli) = std::env::var_os("KICAD_CLI") {
        return cli.to_string_lossy().into_owned();
    }
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\KiCad\10.0\bin\kicad-cli.exe",
        r"C:\Program Files\KiCad\9.0\bin\kicad-cli.exe",
    ];
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &[
        "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli",
        "/usr/local/bin/kicad-cli",
    ];
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    const CANDIDATES: &[&str] = &[
        "/usr/bin/kicad-cli",
        "/usr/local/bin/kicad-cli",
        "/snap/bin/kicad-cli",
    ];
    CANDIDATES
        .iter()
        .find(|candidate| Path::new(candidate).is_file())
        .copied()
        .unwrap_or("kicad-cli")
        .to_owned()
}

fn snapshot_dir() -> PathBuf {
    let sequence = SNAPSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "konnect-schematic-fallback-{}-{sequence}",
        std::process::id()
    ))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.canonicalize().unwrap_or_else(|_| a.to_path_buf())
        == b.canonicalize().unwrap_or_else(|_| b.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_names_are_process_unique() {
        assert_ne!(snapshot_dir(), snapshot_dir());
    }
}
