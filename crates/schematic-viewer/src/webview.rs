//! Konnect — Live Schematic Viewer
//!
//! Watches a .kicad_sch file (and, for hierarchical designs, every sheet
//! reachable from it), renders each to SVG via kicad-cli, and displays them
//! in a native window with pan/zoom, a bottom filmstrip, and auto-refresh.
//!
//! Usage: `schematic-viewer [--kicad-cli <path>] [path/to/file.kicad_sch]`

use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

// ─── Sheet hierarchy ────────────────────────────────────────────────────────

/// One entry in the flat, depth-indented sheet list shown in the frontend's
/// filmstrip. `file` is the absolute path, used as the cache/lookup key.
#[derive(Debug, Clone, serde::Serialize)]
struct SheetEntry {
    name: String,
    file: String,
    depth: usize,
}

/// Guards against a pathological reference cycle in a hand-edited file —
/// matches the same depth cap `sch_hierarchy.rs`'s `get_sheet_hierarchy` uses.
const MAX_SHEET_DEPTH: usize = 20;

/// Walk the sheet tree starting at `root` in depth-first order, using
/// `konnect-schematic-editor`'s typed `Sheet` model (the same one
/// `sch_hierarchy`'s MCP tools are built on) rather than re-implementing
/// `.kicad_sch` parsing here. Missing child files are skipped silently —
/// this is a display surface, not a validator; `sch_hierarchy`'s own
/// `validate_sheet_pins`/`get_sheet_hierarchy` tools are where that's
/// surfaced loudly.
fn walk_sheet_tree(root: &Path) -> Vec<SheetEntry> {
    let root_name = root
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    walk_sheet_tree_inner(root, &root_name, 0, &mut visited, &mut out);
    out
}

fn walk_sheet_tree_inner(
    path: &Path,
    display_name: &str,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<SheetEntry>,
) {
    if depth > MAX_SHEET_DEPTH {
        return;
    }
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canon.clone()) {
        return; // cycle guard — already an ancestor on *this* branch
    }

    out.push(SheetEntry {
        name: display_name.to_string(),
        file: path.to_string_lossy().to_string(),
        depth,
    });

    if let Ok(sch) = konnect_schematic_editor::Schematic::load(path) {
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        for sheet in sch.sheets.iter() {
            let child_path = dir.join(sheet.file());
            if child_path.exists() {
                walk_sheet_tree_inner(&child_path, sheet.name(), depth + 1, visited, out);
            }
        }
    }

    // Backtrack: remove from the ancestor set so a sibling branch reusing the
    // same file (a legitimate multi-instance sheet, not a cycle) isn't
    // mistakenly skipped — only an actual ancestor-of-itself loop should stop.
    visited.remove(&canon);
}

/// Unique parent directories across every sheet's file — what the watcher
/// needs to cover so an edit to any sheet (not just the root) is caught.
fn compute_watch_dirs(entries: &[SheetEntry]) -> HashSet<PathBuf> {
    entries
        .iter()
        .filter_map(|e| Path::new(&e.file).parent().map(|p| p.to_path_buf()))
        .collect()
}

/// Whether a changed path is a genuine schematic edit worth re-walking the
/// tree for. KiCAD periodically writes its own `_autosave-*.kicad_sch` and
/// `~*.kicad_sch` (lock) files into the same directory as the real sheets —
/// these fire on a timer independent of any real edit, and reacting to them
/// wastes a full parallel re-render *and* can eat the debounce window right
/// before a genuine save, silently dropping the real change.
fn is_relevant_sch_change(path: &Path) -> bool {
    let is_sch = path.extension().map(|e| e == "kicad_sch").unwrap_or(false);
    if !is_sch {
        return false;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    !(name.starts_with("_autosave-") || name.starts_with('~'))
}

// ─── State ──────────────────────────────────────────────────────────────────

struct ViewerState {
    root_path: Mutex<Option<PathBuf>>,
    active_file: Mutex<Option<PathBuf>>,
    sheets: Mutex<Vec<SheetEntry>>,
    /// Rendered SVG per sheet, keyed by `SheetEntry::file`. All sheets are
    /// rendered eagerly on open/refresh so switching sheets is instant.
    svg_cache: Mutex<HashMap<String, String>>,
    /// Directories the live watcher currently covers — diffed against a
    /// fresh `compute_watch_dirs` on every change so sheets added in a new
    /// folder get picked up without reopening the viewer.
    watched_dirs: Mutex<HashSet<PathBuf>>,
    kicad_cli: Mutex<String>,
    /// File passed on the command line, handed to the frontend when it asks.
    startup_file: Mutex<Option<String>>,
    /// The active file watcher. Replacing it drops (and stops) the previous
    /// one, so only the currently-open design is ever watched.
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

// ─── Binary discovery ───────────────────────────────────────────────────────

/// Resolve kicad-cli: explicit override → platform candidates → PATH.
/// Candidate lists mirror `plugin/settings_dialog.py::detect_kicad_cli`.
fn resolve_kicad_cli(override_path: Option<String>) -> String {
    if let Some(p) = override_path.or_else(|| std::env::var("KICAD_CLI").ok()) {
        if !p.is_empty() {
            return p;
        }
    }
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[
            r"C:\KiCad\10.0\bin\kicad-cli.exe",
            r"C:\Program Files\KiCad\10.0\bin\kicad-cli.exe",
            r"C:\Program Files\KiCad\9.0\bin\kicad-cli.exe",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli",
            "/usr/local/bin/kicad-cli",
        ]
    } else {
        &[
            "/usr/bin/kicad-cli",
            "/usr/local/bin/kicad-cli",
            "/snap/bin/kicad-cli",
            "/snap/kicad/current/usr/bin/kicad-cli",
        ]
    };
    for c in candidates {
        if Path::new(c).exists() {
            return c.to_string();
        }
    }
    "kicad-cli".to_string() // hope it's on PATH
}

/// Resolve the KiCAD GUI binary for "Open in KiCAD".
fn resolve_kicad_binary() -> String {
    if let Ok(path) = std::env::var("KICAD_BINARY") {
        if !path.is_empty() {
            return path;
        }
    }
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[
            r"C:\KiCad\10.0\bin\kicad.exe",
            r"C:\Program Files\KiCad\10.0\bin\kicad.exe",
            r"C:\Program Files\KiCad\9.0\bin\kicad.exe",
        ]
    } else if cfg!(target_os = "macos") {
        &["/Applications/KiCad/KiCad.app/Contents/MacOS/kicad"]
    } else {
        &["/usr/bin/kicad", "/usr/local/bin/kicad"]
    };
    for c in candidates {
        if Path::new(c).exists() {
            return c.to_string();
        }
    }
    "kicad".to_string()
}

/// KiCAD's own lock-file convention for a file it has open: `~<name>.lck`
/// in the same directory.
fn kicad_lock_path(target: &Path) -> PathBuf {
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    dir.join(format!("~{}.lck", name))
}

// ─── SVG Rendering ──────────────────────────────────────────────────────────

/// Per-process temp dir so concurrent viewer instances don't clobber each
/// other's rendered SVGs.
fn render_temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("konnect-viewer-{}", std::process::id()))
}

/// A fresh, unique working directory for one render batch, so overlapping
/// batches (a watcher-triggered render racing a manual refresh or a reopen)
/// never share snapshot or output paths.
fn render_batch_dir() -> PathBuf {
    static RENDER_BATCH: AtomicU64 = AtomicU64::new(0);
    render_temp_dir().join(format!(
        "batch-{}",
        RENDER_BATCH.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Copy every unique sheet file (plus the root's `.kicad_pro`, if any) into
/// `snap_dir`, mirroring each file's path relative to the root's parent so
/// parent-to-child sheet references still resolve inside the snapshot.
/// Returns a map from each live file (the `SheetEntry::file` string) to the
/// path kicad-cli should actually be pointed at.
///
/// Why render from copies at all: kicad-cli holds every `.kicad_sch` in the
/// exported tree open *without write sharing* for the entire export
/// (verified directly on Windows: concurrent write-opens fail with a
/// sharing violation, which KiCAD's GUI surfaces as the misleading "You do
/// not have write permissions to: ..." dialog when a user's save lands
/// mid-export). Rendering from a temp snapshot means no kicad-cli process
/// ever holds the user's real files open, so a KiCAD save can never collide
/// with a render, no matter how many renders run in parallel. It also keeps
/// kicad-cli's transient `~<project>.lck` file out of the real project
/// folder.
fn snapshot_tree(root: &Path, entries: &[SheetEntry], snap_dir: &Path) -> HashMap<String, PathBuf> {
    let base = root.parent().unwrap_or(Path::new("."));
    let mut map = HashMap::new();
    let mut seen = HashSet::new();
    for e in entries {
        if !seen.insert(e.file.as_str()) {
            continue;
        }
        let live = Path::new(&e.file);
        let copied = live
            .strip_prefix(base)
            .ok()
            .map(|rel| snap_dir.join(rel))
            .and_then(|dest| {
                std::fs::create_dir_all(dest.parent()?).ok()?;
                // Explicit read + write rather than fs::copy: std's
                // File::open shares read/write/delete access, so the copy
                // itself can never block a concurrent KiCAD save either.
                let bytes = std::fs::read(live).ok()?;
                std::fs::write(&dest, bytes).ok()?;
                Some(dest)
            });
        // A sheet outside the root's folder (or a failed copy) falls back to
        // rendering from the live path: no worse than the pre-snapshot
        // behavior, and the common everything-under-one-project-dir case is
        // fully isolated.
        map.insert(e.file.clone(), copied.unwrap_or_else(|| live.to_path_buf()));
    }
    // Text variables and title-block settings live in the project file;
    // copy it alongside the root sheet so rendered frames stay faithful.
    let pro = root.with_extension("kicad_pro");
    if pro.exists() {
        if let Some(name) = pro.file_name() {
            let _ = std::fs::create_dir_all(snap_dir);
            if let Ok(bytes) = std::fs::read(&pro) {
                let _ = std::fs::write(snap_dir.join(name), bytes);
            }
        }
    }
    map
}

/// Render `schematic` to SVG via kicad-cli, writing into `output_dir` (which
/// is created if needed). Takes an explicit output directory — rather than
/// always using the shared per-process temp dir — so concurrent calls (see
/// `render_all`) never race on the same `<stem>.svg` output filename, which
/// two *different* sheets could coincidentally share.
fn render_to_svg_in(cli: &str, schematic: &Path, output_dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(output_dir).map_err(|e| e.to_string())?;

    let mut cmd = Command::new(cli);
    cmd.args(["sch", "export", "svg", "--output"])
        .arg(output_dir)
        .arg(schematic);
    // kicad-cli is a console-subsystem binary — without this, Windows
    // flashes a console window for every invocation. Since every sheet is
    // rendered eagerly, a multi-sheet design would otherwise flash one
    // window per sheet on every open/refresh.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("Failed to run kicad-cli ({}): {}", cli, e))?;

    let stem = schematic.file_stem().unwrap_or_default().to_string_lossy();
    let svg_path = output_dir.join(format!("{}.svg", stem));

    if !output.status.success() {
        // kicad-cli's exit code can lie: with a pre-existing foreign or
        // unparseable project lock file present it completes the entire
        // export and still exits -1 (verified against KiCAD 10 on Windows).
        // Trust the artifact over the exit code; only a missing or empty
        // SVG is a real failure.
        let produced = std::fs::metadata(&svg_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        if !produced {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("kicad-cli failed: {}", stderr));
        }
    }

    std::fs::read_to_string(&svg_path).map_err(|e| format!("Failed to read SVG: {}", e))
}

/// Render every *unique* file among `entries`, in parallel — one kicad-cli
/// subprocess per file rather than one per tree position, since a
/// multi-instance sheet can appear at several positions but only needs
/// rendering once. A failure on one sheet doesn't stop the rest — the
/// caller gets whatever succeeded plus a list of what didn't.
///
/// Deliberately NOT using kicad-cli's own single-call whole-tree export
/// (`sch export svg` against the root emits one SVG per sheet in one
/// invocation): it names outputs by hyphen-joining ancestor sheet names,
/// which is ambiguous to parse back when a sheet name itself contains a
/// hyphen (real-world example: a sheet literally named "H-Bridge" produces
/// the same kind of filename shape as a genuine two-level "PMS-Filter"
/// path). Calling kicad-cli once per exactly-known file path sidesteps that
/// ambiguity entirely, at the cost of more subprocess spawns — mitigated
/// by running them concurrently instead of serially.
fn render_all(
    cli: &str,
    root: &Path,
    entries: &[SheetEntry],
) -> (HashMap<String, String>, Vec<String>) {
    render_some(cli, root, entries, None)
}

/// Like `render_all`, but when `subset` is Some, only the unique files it
/// names are actually rendered — the watcher uses this to re-render just
/// what a save touched instead of the whole design (a full ~20-sheet
/// re-render costs several seconds of save-to-screen lag; one edited sheet
/// costs one render). The snapshot still covers the *whole* tree
/// regardless, because kicad-cli loads a sheet's children even when
/// exporting only that sheet.
fn render_some(
    cli: &str,
    root: &Path,
    entries: &[SheetEntry],
    subset: Option<&HashSet<String>>,
) -> (HashMap<String, String>, Vec<String>) {
    let mut unique_files: Vec<(&str, &str)> = Vec::new(); // (file, first-seen name)
    let mut seen = HashSet::new();
    for e in entries {
        if seen.insert(e.file.as_str()) {
            let wanted = match subset {
                Some(s) => s.contains(e.file.as_str()),
                None => true,
            };
            if wanted {
                unique_files.push((e.file.as_str(), e.name.as_str()));
            }
        }
    }
    if unique_files.is_empty() {
        return (HashMap::new(), Vec::new());
    }

    // Renders run against a temp snapshot of the sheet files, never the
    // live ones (see snapshot_tree for why), inside a batch dir that's
    // cleaned up at the end.
    let batch_dir = render_batch_dir();
    let snapshot = snapshot_tree(root, entries, &batch_dir.join("src"));

    let results: Vec<(String, Result<String, String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = unique_files
            .iter()
            .enumerate()
            .map(|(i, (file, name))| {
                let out_dir = batch_dir.join(i.to_string());
                let src = snapshot
                    .get(*file)
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from(*file));
                scope.spawn(move || {
                    let r = render_to_svg_in(cli, &src, &out_dir);
                    (*file, *name, r)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .map(|(file, name, r)| (file.to_string(), r.map_err(|e| format!("{}: {}", name, e))))
            .collect()
    });

    let mut cache = HashMap::new();
    let mut errors = Vec::new();
    for (file, r) in results {
        match r {
            Ok(svg) => {
                let _ = crate::svg_order_cache::store(Path::new(&file), &svg);
                cache.insert(file, svg);
            }
            Err(e) => errors.push(e),
        }
    }
    let _ = std::fs::remove_dir_all(&batch_dir);
    (cache, errors)
}

// ─── Tauri Commands ─────────────────────────────────────────────────────────

/// The file passed on the command line, if any. The frontend calls this once
/// its scripts are ready — no timing games with `window.eval`.
#[tauri::command]
fn get_startup_file(state: tauri::State<'_, ViewerState>) -> Option<String> {
    state.startup_file.lock().unwrap().take()
}

#[derive(serde::Serialize)]
struct OpenResult {
    svg: String,
    sheets: Vec<SheetEntry>,
    previews: Vec<SheetPreview>,
    active_file: String,
    render_errors: Vec<String>,
}

/// One cached SVG sent to the filmstrip. Sheet metadata stays in
/// `SheetEntry`, so incremental watcher events only need to transfer the
/// files whose rendered content changed.
#[derive(Debug, Clone, serde::Serialize)]
struct SheetPreview {
    file: String,
    svg: String,
}

/// Collect previews in hierarchy order, optionally restricted to a set of
/// files updated by the latest watcher cycle. Sheets whose render failed are
/// omitted; the frontend still keeps their metadata and can show an error
/// placeholder instead of receiving an invalid SVG.
fn collect_sheet_previews(
    entries: &[SheetEntry],
    cache: &HashMap<String, String>,
    only: Option<&HashSet<String>>,
) -> Vec<SheetPreview> {
    entries
        .iter()
        .filter(|entry| only.is_none_or(|files| files.contains(&entry.file)))
        .filter_map(|entry| {
            cache.get(&entry.file).map(|svg| SheetPreview {
                file: entry.file.clone(),
                svg: svg.clone(),
            })
        })
        .collect()
}

/// Payload for the `sheets-updated` event, emitted by the file watcher when
/// a render cycle completes. `updated_previews` contains only newly rendered
/// SVGs; the frontend merges them into its filmstrip cache and uses `sheets`
/// to add, remove, and reorder thumbnails.
#[derive(serde::Serialize)]
struct SheetsUpdated {
    sheets: Vec<SheetEntry>,
    active_file: String,
    updated_previews: Vec<SheetPreview>,
}

#[tauri::command]
fn open_schematic(
    app: AppHandle,
    state: tauri::State<'_, ViewerState>,
    path: String,
) -> Result<OpenResult, String> {
    let root = PathBuf::from(&path);
    if !root.exists() {
        return Err(format!("File not found: {}", path));
    }

    let cli = state.kicad_cli.lock().unwrap().clone();
    let sheets = walk_sheet_tree(&root);
    let (cache, errors) = render_all(&cli, &root, &sheets);

    let active_key = root.to_string_lossy().to_string();
    let svg = cache
        .get(&active_key)
        .cloned()
        .ok_or_else(|| format!("Failed to render root schematic: {}", errors.join("; ")))?;
    let previews = collect_sheet_previews(&sheets, &cache, None);

    if let Some(window) = app.get_webview_window("main") {
        let name = root.file_name().unwrap_or_default().to_string_lossy();
        let _ = window.set_title(&format!("{} — Schematic Viewer", name));
    }

    let watch_dirs = compute_watch_dirs(&sheets);
    let watcher = build_watcher(app.clone(), cli, root.clone())?;

    *state.root_path.lock().unwrap() = Some(root.clone());
    *state.active_file.lock().unwrap() = Some(root);
    *state.sheets.lock().unwrap() = sheets.clone();
    *state.svg_cache.lock().unwrap() = cache;
    *state.watched_dirs.lock().unwrap() = watch_dirs;
    *state.watcher.lock().unwrap() = Some(watcher);

    Ok(OpenResult {
        svg,
        sheets,
        previews,
        active_file: active_key,
        render_errors: errors,
    })
}

/// Switch which sheet is displayed, without re-rendering — all sheets were
/// already rendered eagerly by `open_schematic`/the watcher, so this is
/// just a cache lookup.
#[tauri::command]
fn select_sheet(state: tauri::State<'_, ViewerState>, file: String) -> Result<String, String> {
    let known = state.sheets.lock().unwrap().iter().any(|e| e.file == file);
    if !known {
        return Err(format!(
            "'{}' is not a known sheet in the current design",
            file
        ));
    }
    let svg = state
        .svg_cache
        .lock()
        .unwrap()
        .get(&file)
        .cloned()
        .ok_or_else(|| "This sheet failed to render — see the earlier error".to_string())?;
    *state.active_file.lock().unwrap() = Some(PathBuf::from(&file));
    Ok(svg)
}

#[tauri::command]
fn refresh(state: tauri::State<'_, ViewerState>) -> Result<SheetPreview, String> {
    let active = state
        .active_file
        .lock()
        .unwrap()
        .clone()
        .ok_or("No schematic loaded")?;
    let cli = state.kicad_cli.lock().unwrap().clone();
    // Same snapshot isolation as render_all, scoped to the active sheet's
    // subtree (children must be present for kicad-cli to load the sheet).
    let entries = walk_sheet_tree(&active);
    let batch_dir = render_batch_dir();
    let snapshot = snapshot_tree(&active, &entries, &batch_dir.join("src"));
    let key = active.to_string_lossy().to_string();
    let src = snapshot
        .get(&key)
        .cloned()
        .unwrap_or_else(|| active.clone());
    let result = render_to_svg_in(&cli, &src, &batch_dir.join("out"));
    let _ = std::fs::remove_dir_all(&batch_dir);
    let svg = result?;
    state
        .svg_cache
        .lock()
        .unwrap()
        .insert(key.clone(), svg.clone());
    Ok(SheetPreview { file: key, svg })
}

/// Opens the currently *active* sheet in KiCAD — not necessarily the root —
/// so "Open in KiCAD" matches whatever the user is actually looking at.
#[tauri::command]
fn open_in_kicad(state: tauri::State<'_, ViewerState>) -> Result<(), String> {
    let root = state
        .root_path
        .lock()
        .unwrap()
        .clone()
        .ok_or("No schematic loaded")?;
    let active = state
        .active_file
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| root.clone());

    // kicad.exe (the full GUI app, as opposed to kicad-cli) expects a
    // .kicad_pro project file — passing a bare .kicad_sch produces "does
    // not appear to be a KiCad project file". Prefer the project file
    // (standard KiCad convention: same file stem as the root schematic,
    // same directory) when it exists; otherwise fall back to opening the
    // sheet directly, which KiCad can still do for a standalone schematic
    // with no project file. Note: this opens the *project*, not
    // necessarily jumping straight to whichever sub-sheet is active in the
    // viewer — there's no reliable way to make kicad.exe do that from the
    // command line.
    let project = root.with_extension("kicad_pro");
    let target = if project.exists() { project } else { active };

    // KiCAD marks an open project/file with a `~<name>.lck` file in the
    // same directory (confirmed against a real KiCAD 10 session). Spawning
    // kicad.exe against an already-locked project produces a warning dialog
    // that, if declined, still leaves a new blank KiCAD window running —
    // checking first avoids that stray window entirely. Known tradeoff: a
    // stale lock left behind by a crashed KiCAD would incorrectly block a
    // legitimate open; not attempting to validate lock freshness here.
    if kicad_lock_path(&target).exists() {
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        return Err(format!(
            "'{}' is already open in another KiCAD window — switch to that window instead of opening a new one.",
            name
        ));
    }

    Command::new(resolve_kicad_binary())
        .arg(&target)
        .spawn()
        .map_err(|e| format!("Failed to launch KiCAD: {}", e))?;
    Ok(())
}

// ─── File Watcher ───────────────────────────────────────────────────────────

/// Build a watcher covering every sheet's parent directory. On any relevant
/// change, the worker re-walks the whole tree because an edit to the root can
/// add or remove sheets. It then re-renders only changed or uncached sheets;
/// untouched pages retain their cached SVGs and do not cross the frontend IPC
/// boundary again.
fn build_watcher(
    app: AppHandle,
    cli: String,
    root: PathBuf,
) -> Result<notify::RecommendedWatcher, String> {
    let initial_dirs = compute_watch_dirs(&walk_sheet_tree(&root));

    // All slow work happens on a dedicated worker thread, never in the
    // notify callback below — see spawn_render_worker for why that split is
    // load-bearing, not just tidy.
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_render_worker(app, cli, root, rx);

    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let Ok(event) = res else { return };

            match event.kind {
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {}
                _ => return,
            }

            let changed: Vec<PathBuf> = event
                .paths
                .iter()
                .filter(|p| is_relevant_sch_change(p))
                .cloned()
                .collect();
            if changed.is_empty() {
                return;
            }

            let _ = tx.send(changed);
        })
        .map_err(|e| format!("Failed to create file watcher: {}", e))?;

    for dir in &initial_dirs {
        watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(|e| format!("Failed to watch {}: {}", dir.display(), e))?;
    }

    Ok(watcher)
}

/// How long file events must stay quiet before a re-render starts. A KiCAD
/// save rewrites every modified sheet file in a burst spread over a second
/// or two (observed: ~24 files across 2 seconds on a real 20-sheet board),
/// so rendering must begin *after* the burst ends, not on its first event —
/// a leading-edge debounce reads files KiCAD is still writing. A quarter
/// second still coalesces the observed event spacing while making ordinary
/// one-sheet saves feel immediate.
const QUIET_WINDOW: Duration = Duration::from_millis(250);

/// Upper bound on waiting for quiet, so a pathological never-ending event
/// stream still renders eventually instead of starving forever.
const MAX_BURST_WAIT: Duration = Duration::from_secs(5);

/// Record a batch of changed paths in both raw and canonical form. Notify's
/// event paths and the tree walker's joined paths can disagree in shape
/// (canonicalize on Windows yields \\?\-prefixed paths; a deleted file
/// can't be canonicalized at all), so membership tests check both forms.
fn note_changed(changed: &mut HashSet<PathBuf>, paths: Vec<PathBuf>) {
    for p in paths {
        if let Ok(c) = p.canonicalize() {
            changed.insert(c);
        }
        changed.insert(p);
    }
}

/// Absorb a burst of file events, accumulating every reported path into
/// `changed`: keep draining until `quiet` elapses with no new event, or
/// give up waiting once `cap` has passed since the burst began. Returns
/// false when the channel disconnected — the watcher owning the sender was
/// dropped, so this design is no longer current and the worker should exit.
fn drain_until_quiet(
    rx: &Receiver<Vec<PathBuf>>,
    quiet: Duration,
    cap: Duration,
    changed: &mut HashSet<PathBuf>,
) -> bool {
    let burst_start = Instant::now();
    loop {
        if burst_start.elapsed() >= cap {
            return true;
        }
        match rx.recv_timeout(quiet) {
            Ok(paths) => note_changed(changed, paths),
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Which sheet files must be re-rendered for this burst: any whose path
/// (raw or canonical) is in `changed`, plus any without a cached SVG — a
/// newly added sheet, or one whose previous render failed. Everything else
/// keeps its cached image untouched.
fn files_needing_render(
    entries: &[SheetEntry],
    changed: &HashSet<PathBuf>,
    cached: &HashMap<String, String>,
) -> HashSet<String> {
    let mut need = HashSet::new();
    for e in entries {
        if cached.contains_key(&e.file) {
            let raw = PathBuf::from(&e.file);
            let canon = raw.canonicalize().unwrap_or_else(|_| raw.clone());
            if !changed.contains(&raw) && !changed.contains(&canon) {
                continue;
            }
        }
        need.insert(e.file.clone());
    }
    need
}

/// The render worker owns everything slow: it collapses each burst of file
/// events into a single re-walk + re-render once the burst goes quiet.
///
/// Keeping this off the notify callback is load-bearing on Windows: while
/// the callback runs, directory changes queue in a small fixed OS buffer
/// (ReadDirectoryChangesW). A multi-second render inside the callback
/// overflows that buffer and silently drops every event that arrives
/// meanwhile — the watcher then looks permanently "deaf" even though
/// nothing errored. The callback must return in microseconds; it forwards
/// each event's relevant paths through `rx` and nothing more.
///
/// The worker exits when `rx` disconnects, which happens exactly when the
/// watcher holding the sender is dropped (a new design was opened, or the
/// viewer is shutting down).
fn spawn_render_worker(app: AppHandle, cli: String, root: PathBuf, rx: Receiver<Vec<PathBuf>>) {
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            // Surface "the watcher noticed" immediately, before the quiet
            // wait and the render — both take a while on a big design.
            // Without this there is no way for the user to tell "the edit
            // was never seen" apart from "seen, but the render failed".
            let _ = app.emit("change-detected", ());

            let mut changed = HashSet::new();
            note_changed(&mut changed, first);
            if !drain_until_quiet(&rx, QUIET_WINDOW, MAX_BURST_WAIT, &mut changed) {
                return; // watcher dropped mid-burst — this design was closed
            }

            let state = app.state::<ViewerState>();

            // Stale guard: if the user opened a different design while this
            // burst was in flight, drop it.
            {
                let current_root = state.root_path.lock().unwrap().clone();
                if current_root.as_deref() != Some(root.as_path()) {
                    return;
                }
            }

            let new_sheets = walk_sheet_tree(&root);

            // Re-render only what this burst touched (plus anything not yet
            // cached); untouched sheets keep their existing SVGs. On a big
            // design this turns save-to-screen latency from "render the
            // whole board" into "render the sheets you edited".
            let old_cache = state.svg_cache.lock().unwrap().clone();
            let need = files_needing_render(&new_sheets, &changed, &old_cache);
            let (fresh, errors) = render_some(&cli, &root, &new_sheets, Some(&need));

            // Merge: freshly rendered wins; sheets still in the tree keep
            // their old SVG; sheets that left the tree drop out. A sheet
            // that needed a render but failed stays absent, matching
            // render_all's contract.
            let mut cache: HashMap<String, String> = HashMap::new();
            for e in &new_sheets {
                if let Some(svg) = fresh.get(&e.file) {
                    cache.insert(e.file.clone(), svg.clone());
                } else if !need.contains(&e.file) {
                    if let Some(svg) = old_cache.get(&e.file) {
                        cache.insert(e.file.clone(), svg.clone());
                    }
                }
            }

            // If the previously active sheet vanished from the tree (its
            // sheet was deleted), fall back to the root.
            let (active_key, active_switched) = {
                let mut active = state.active_file.lock().unwrap();
                let prev = active.as_ref().map(|a| a.to_string_lossy().to_string());
                let still_valid = active
                    .as_ref()
                    .map(|a| new_sheets.iter().any(|e| e.file == a.to_string_lossy()))
                    .unwrap_or(false);
                if !still_valid {
                    *active = Some(root.clone());
                }
                let key = active.as_ref().unwrap().to_string_lossy().to_string();
                let switched = prev.as_deref() != Some(key.as_str());
                (key, switched)
            };

            // Re-derive the watch set and diff it against what's currently
            // watched, so a sheet added in a brand-new folder gets covered
            // without requiring the user to reopen the viewer.
            let new_dirs = compute_watch_dirs(&new_sheets);
            if let Some(w) = state.watcher.lock().unwrap().as_mut() {
                let mut watched = state.watched_dirs.lock().unwrap();
                for dir in new_dirs.difference(&watched) {
                    let _ = w.watch(dir, RecursiveMode::NonRecursive);
                }
                for dir in watched.difference(&new_dirs) {
                    let _ = w.unwatch(dir);
                }
                *watched = new_dirs;
            }

            *state.sheets.lock().unwrap() = new_sheets.clone();
            *state.svg_cache.lock().unwrap() = cache.clone();

            let updated_previews = collect_sheet_previews(&new_sheets, &cache, Some(&need));
            let _ = app.emit(
                "sheets-updated",
                &SheetsUpdated {
                    sheets: new_sheets,
                    active_file: active_key.clone(),
                    updated_previews,
                },
            );
            // Push the active sheet only when its content actually changed
            // (it was re-rendered) or the view must switch (fallback to
            // root after a delete); re-pushing an identical SVG would
            // pointlessly redraw and flash "updated" for edits that only
            // touched other sheets.
            if active_switched || need.contains(&active_key) {
                if let Some(svg) = cache.get(&active_key) {
                    let _ = app.emit("schematic-updated", svg);
                }
            }
            for e in &errors {
                let _ = app.emit("viewer-error", format!("Render failed: {}", e));
            }
        }
    });
}

// ─── Main ───────────────────────────────────────────────────────────────────

/// Select WebKitGTK's reliable renderer before GTK chooses Wayland or X11.
///
/// WebKitGTK's DMA-BUF path can fail with a Wayland protocol error or create a
/// blank surface under either display backend on otherwise valid Linux GPU
/// configurations. The non-DMA-BUF path works on both backends and is ample
/// for this viewer's static SVG workload. Respect an explicit environment
/// value so users and distro maintainers can opt back into DMA-BUF rendering.
#[cfg(target_os = "linux")]
fn configure_linux_webview_renderer() {
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_linux_webview_renderer() {}

pub(crate) fn run() {
    // Must run before Tauri initializes GTK/WebKitGTK or starts worker threads.
    // GDK_BACKEND is deliberately untouched: GTK should use native Wayland
    // when available and X11 when that is the active/explicit backend.
    configure_linux_webview_renderer();

    // Minimal arg parsing: [--kicad-cli <path>] [schematic-file]
    let mut kicad_cli_override: Option<String> = None;
    let mut file_arg: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--kicad-cli" {
            kicad_cli_override = args.next();
        } else if !a.starts_with('-') {
            file_arg = Some(a);
        }
    }

    let state = ViewerState {
        root_path: Mutex::new(None),
        active_file: Mutex::new(None),
        sheets: Mutex::new(Vec::new()),
        svg_cache: Mutex::new(HashMap::new()),
        watched_dirs: Mutex::new(HashSet::new()),
        kicad_cli: Mutex::new(resolve_kicad_cli(kicad_cli_override)),
        startup_file: Mutex::new(file_arg),
        watcher: Mutex::new(None),
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_startup_file,
            open_schematic,
            select_sheet,
            refresh,
            open_in_kicad
        ])
        .build(tauri::generate_context!())
        .expect("error while building schematic viewer")
        .run(|_app, event| {
            if let tauri::RunEvent::Exit = event {
                // Best-effort cleanup of this instance's rendered SVGs
                let _ = std::fs::remove_dir_all(render_temp_dir());
            }
        });
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_schematic_editor::{Schematic, Sheet};
    use tempfile::TempDir;

    fn blank_schematic(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let template = "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n";
        std::fs::write(&path, template).unwrap();
        let sch = Schematic::load(&path).unwrap();
        sch.overwrite().unwrap();
        path
    }

    fn add_sheet(parent: &Path, name: &str, file: &str) {
        let mut sch = Schematic::load(parent).unwrap();
        sch.add_sheet(Sheet::new(name, file, 50.0, 50.0, 80.0, 50.0));
        sch.overwrite().unwrap();
    }

    #[test]
    fn kicad_lock_path_matches_real_kicad_convention() {
        // Verified against a real KiCAD 10 session: opening
        // "MultiSheet.kicad_pro" produces "~MultiSheet.kicad_pro.lck" in
        // the same directory.
        let path = Path::new("/proj/MultiSheet.kicad_pro");
        assert_eq!(
            kicad_lock_path(path),
            PathBuf::from("/proj/~MultiSheet.kicad_pro.lck")
        );
    }

    #[test]
    fn is_relevant_sch_change_ignores_autosave_and_lock_files() {
        assert!(!is_relevant_sch_change(Path::new(
            "/proj/_autosave-Root.kicad_sch"
        )));
        assert!(!is_relevant_sch_change(Path::new("/proj/~Root.kicad_sch")));
        assert!(!is_relevant_sch_change(Path::new("/proj/Root.kicad_pro"))); // wrong extension
        assert!(is_relevant_sch_change(Path::new("/proj/Root.kicad_sch")));
        assert!(is_relevant_sch_change(Path::new("/proj/PMS.kicad_sch")));
    }

    #[test]
    fn walk_sheet_tree_finds_root_only_when_no_sheets() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");

        let entries = walk_sheet_tree(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].depth, 0);
        assert_eq!(entries[0].name, "root");
    }

    #[test]
    fn walk_sheet_tree_finds_nested_sheets_in_depth_first_order() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        blank_schematic(tmp.path(), "mid.kicad_sch");
        blank_schematic(tmp.path(), "leaf.kicad_sch");
        add_sheet(&root, "Mid", "mid.kicad_sch");
        add_sheet(&tmp.path().join("mid.kicad_sch"), "Leaf", "leaf.kicad_sch");

        let entries = walk_sheet_tree(&root);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "root");
        assert_eq!(entries[0].depth, 0);
        assert_eq!(entries[1].name, "Mid");
        assert_eq!(entries[1].depth, 1);
        assert_eq!(entries[2].name, "Leaf");
        assert_eq!(entries[2].depth, 2);
    }

    #[test]
    fn walk_sheet_tree_skips_missing_child_file_without_crashing() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        add_sheet(&root, "Gone", "does_not_exist.kicad_sch");

        let entries = walk_sheet_tree(&root);
        assert_eq!(entries.len(), 1); // just the root — missing child skipped
    }

    #[test]
    fn walk_sheet_tree_handles_multiple_siblings() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        blank_schematic(tmp.path(), "a.kicad_sch");
        blank_schematic(tmp.path(), "b.kicad_sch");
        add_sheet(&root, "A", "a.kicad_sch");
        add_sheet(&root, "B", "b.kicad_sch");

        let entries = walk_sheet_tree(&root);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].name, "A");
        assert_eq!(entries[1].depth, 1);
        assert_eq!(entries[2].name, "B");
        assert_eq!(entries[2].depth, 1);
    }

    #[test]
    fn walk_sheet_tree_reuse_of_same_file_is_not_treated_as_a_cycle() {
        // Multi-instance sheets: the same child file placed twice from two
        // different parents is legitimate (see PR #15's design note), not a
        // reference cycle — only an actual ancestor-of-itself loop should stop.
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        blank_schematic(tmp.path(), "shared.kicad_sch");
        add_sheet(&root, "First", "shared.kicad_sch");
        add_sheet(&root, "Second", "shared.kicad_sch");

        let entries = walk_sheet_tree(&root);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].name, "First");
        assert_eq!(entries[2].name, "Second");
    }

    #[test]
    fn walk_sheet_tree_stops_on_genuine_cycle() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        blank_schematic(tmp.path(), "child.kicad_sch");
        add_sheet(&root, "Child", "child.kicad_sch");
        // Child references root — a genuine cycle.
        add_sheet(
            &tmp.path().join("child.kicad_sch"),
            "Root",
            "root.kicad_sch",
        );

        let entries = walk_sheet_tree(&root);
        // root, child — then root-again is skipped (already an ancestor)
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn compute_watch_dirs_dedupes_siblings_in_same_folder() {
        let entries = vec![
            SheetEntry {
                name: "root".into(),
                file: "/proj/root.kicad_sch".into(),
                depth: 0,
            },
            SheetEntry {
                name: "A".into(),
                file: "/proj/a.kicad_sch".into(),
                depth: 1,
            },
            SheetEntry {
                name: "B".into(),
                file: "/proj/sub/b.kicad_sch".into(),
                depth: 1,
            },
        ];
        let dirs = compute_watch_dirs(&entries);
        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains(Path::new("/proj")));
        assert!(dirs.contains(Path::new("/proj/sub")));
    }

    #[test]
    fn compute_watch_dirs_empty_for_no_entries() {
        assert!(compute_watch_dirs(&[]).is_empty());
    }

    #[test]
    fn collect_sheet_previews_preserves_order_and_filters_incremental_updates() {
        let entries = vec![
            SheetEntry {
                name: "root".into(),
                file: "/proj/root.kicad_sch".into(),
                depth: 0,
            },
            SheetEntry {
                name: "Power".into(),
                file: "/proj/power.kicad_sch".into(),
                depth: 1,
            },
            SheetEntry {
                name: "Missing".into(),
                file: "/proj/missing.kicad_sch".into(),
                depth: 1,
            },
        ];
        let cache = HashMap::from([
            (entries[0].file.clone(), "<svg>root</svg>".to_string()),
            (entries[1].file.clone(), "<svg>power</svg>".to_string()),
        ]);

        let all = collect_sheet_previews(&entries, &cache, None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].file, entries[0].file);
        assert_eq!(all[1].file, entries[1].file);

        let changed = HashSet::from([entries[1].file.clone(), entries[2].file.clone()]);
        let incremental = collect_sheet_previews(&entries, &cache, Some(&changed));
        assert_eq!(incremental.len(), 1);
        assert_eq!(incremental[0].file, entries[1].file);
        assert_eq!(incremental[0].svg, "<svg>power</svg>");
    }

    #[test]
    fn render_all_reports_per_sheet_errors_without_stopping_the_batch() {
        // No real kicad-cli in the test environment — every entry should
        // fail gracefully and land in `errors`, not panic.
        let entries = vec![
            SheetEntry {
                name: "root".into(),
                file: "/nonexistent/root.kicad_sch".into(),
                depth: 0,
            },
            SheetEntry {
                name: "leaf".into(),
                file: "/nonexistent/leaf.kicad_sch".into(),
                depth: 1,
            },
        ];
        let (cache, errors) = render_all(
            "kicad-cli-that-does-not-exist",
            Path::new("/nonexistent/root.kicad_sch"),
            &entries,
        );
        assert!(cache.is_empty());
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn snapshot_tree_mirrors_relative_layout_and_copies_content() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        blank_schematic(&tmp.path().join("sub"), "child.kicad_sch");
        add_sheet(&root, "Child", "sub/child.kicad_sch");

        let entries = walk_sheet_tree(&root);
        assert_eq!(entries.len(), 2);

        let snap = TempDir::new().unwrap();
        let map = snapshot_tree(&root, &entries, snap.path());

        let snap_root = map.get(&entries[0].file).unwrap();
        let snap_child = map.get(&entries[1].file).unwrap();
        assert!(snap_root.starts_with(snap.path()));
        assert!(snap_child.starts_with(snap.path()));
        // Relative layout preserved, so parent-to-child references resolve
        assert!(snap_child.ends_with(Path::new("sub").join("child.kicad_sch")));
        assert_eq!(
            std::fs::read(snap_root).unwrap(),
            std::fs::read(&entries[0].file).unwrap()
        );
        // The snapshot itself walks identically to the live tree
        assert_eq!(walk_sheet_tree(snap_root).len(), 2);
    }

    #[test]
    fn snapshot_tree_copies_root_project_file_when_present() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        std::fs::write(tmp.path().join("root.kicad_pro"), "{}").unwrap();

        let entries = walk_sheet_tree(&root);
        let snap = TempDir::new().unwrap();
        snapshot_tree(&root, &entries, snap.path());

        assert!(snap.path().join("root.kicad_pro").exists());
    }

    #[test]
    fn snapshot_tree_falls_back_to_live_path_for_files_outside_root_dir() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let other = TempDir::new().unwrap();
        let outside = blank_schematic(other.path(), "outside.kicad_sch");

        let entries = vec![
            SheetEntry {
                name: "root".into(),
                file: root.to_string_lossy().to_string(),
                depth: 0,
            },
            SheetEntry {
                name: "Out".into(),
                file: outside.to_string_lossy().to_string(),
                depth: 1,
            },
        ];
        let snap = TempDir::new().unwrap();
        let map = snapshot_tree(&root, &entries, snap.path());

        // In-tree file is copied; out-of-tree file is left at its live path
        assert!(map.get(&entries[0].file).unwrap().starts_with(snap.path()));
        assert_eq!(map.get(&entries[1].file).unwrap(), &outside);
    }

    #[test]
    fn drain_until_quiet_completes_and_accumulates_changed_paths() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        tx.send(vec![PathBuf::from("/proj/a.kicad_sch")]).unwrap();
        tx.send(vec![PathBuf::from("/proj/b.kicad_sch")]).unwrap();
        let mut changed = HashSet::new();
        // Queued events get absorbed, then silence: proceed with the render
        assert!(drain_until_quiet(
            &rx,
            Duration::from_millis(50),
            Duration::from_secs(5),
            &mut changed
        ));
        // Raw paths recorded even when canonicalize fails (files don't exist)
        assert!(changed.contains(Path::new("/proj/a.kicad_sch")));
        assert!(changed.contains(Path::new("/proj/b.kicad_sch")));
    }

    #[test]
    fn drain_until_quiet_reports_disconnect() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        drop(tx);
        // Sender gone means the watcher was dropped: worker must exit
        assert!(!drain_until_quiet(
            &rx,
            Duration::from_millis(50),
            Duration::from_secs(5),
            &mut HashSet::new()
        ));
    }

    #[test]
    fn drain_until_quiet_gives_up_after_cap_during_constant_events() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        let sender = std::thread::spawn(move || {
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(400) {
                let _ = tx.send(vec![PathBuf::from("/proj/a.kicad_sch")]);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        // Events never go quiet, but the cap forces a render anyway
        assert!(drain_until_quiet(
            &rx,
            Duration::from_millis(100),
            Duration::from_millis(150),
            &mut HashSet::new()
        ));
        assert!(start.elapsed() < Duration::from_millis(390));
        sender.join().unwrap();
    }

    #[test]
    fn files_needing_render_selects_changed_and_uncached_only() {
        let tmp = TempDir::new().unwrap();
        let a = blank_schematic(tmp.path(), "a.kicad_sch");
        let b = blank_schematic(tmp.path(), "b.kicad_sch");
        let c = blank_schematic(tmp.path(), "c.kicad_sch");
        let entry = |p: &Path, n: &str| SheetEntry {
            name: n.into(),
            file: p.to_string_lossy().to_string(),
            depth: 0,
        };
        let entries = vec![entry(&a, "A"), entry(&b, "B"), entry(&c, "C")];

        let mut cached = HashMap::new();
        cached.insert(entries[0].file.clone(), "svg".to_string());
        cached.insert(entries[1].file.clone(), "svg".to_string());
        // c is not cached (new sheet, or its earlier render failed)

        // b changed, reported in canonical form like a real notify event
        let mut changed = HashSet::new();
        changed.insert(b.canonicalize().unwrap());

        let need = files_needing_render(&entries, &changed, &cached);
        assert!(!need.contains(&entries[0].file)); // cached + untouched
        assert!(need.contains(&entries[1].file)); // changed on disk
        assert!(need.contains(&entries[2].file)); // not cached
    }

    #[test]
    fn files_needing_render_matches_raw_paths_when_canonicalize_unavailable() {
        // A deleted/unreachable file can't be canonicalized on either side;
        // raw string-path comparison must still match.
        let entries = vec![SheetEntry {
            name: "x".into(),
            file: "/nope/x.kicad_sch".into(),
            depth: 0,
        }];
        let cached: HashMap<String, String> =
            [("/nope/x.kicad_sch".to_string(), "svg".to_string())]
                .into_iter()
                .collect();
        let mut changed = HashSet::new();
        changed.insert(PathBuf::from("/nope/x.kicad_sch"));

        let need = files_needing_render(&entries, &changed, &cached);
        assert!(need.contains("/nope/x.kicad_sch"));
    }

    #[test]
    fn files_needing_render_empty_when_nothing_relevant_changed() {
        // A burst that only touched files outside the tree (e.g. an orphan
        // .kicad_sch in the same folder) must not trigger any renders.
        let entries = vec![SheetEntry {
            name: "root".into(),
            file: "/proj/root.kicad_sch".into(),
            depth: 0,
        }];
        let cached: HashMap<String, String> =
            [("/proj/root.kicad_sch".to_string(), "svg".to_string())]
                .into_iter()
                .collect();
        let mut changed = HashSet::new();
        changed.insert(PathBuf::from("/proj/orphan.kicad_sch"));

        assert!(files_needing_render(&entries, &changed, &cached).is_empty());
    }
}
