//! `project` toolset — create, open, save, and snapshot KiCAD projects.
//!
//! Tools: create_project, open_project, save_project, get_project_info, snapshot_project
//!
//! KiCAD interface:
//!   - create_project   → file system (template)
//!   - open_project     → IPC open-document query
//!   - save_project     → IPC board.save()
//!   - get_project_info → file system read
//!   - snapshot_project → kicad-cli export PDF

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{get_path, opt_str, require_str, ToolContext, ToolDef};
use konnect_sexp::{commit_file_transaction, FileTransition, SexpError};
use serde_json::json;
use std::path::{Path, PathBuf};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "create_project",
            "Create a new KiCAD project at the given path. Creates the directory, \
             a blank .kicad_pro file, an empty .kicad_sch schematic, and a blank \
             .kicad_pcb board file. Refuses to replace any existing project file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path where the project will be created"
                    },
                    "name": {
                        "type": "string",
                        "description": "Project name (used as filename stem)"
                    }
                },
                "required": ["path", "name"]
            }),
            |args, ctx| async move { handle_create_project(args, ctx).await }
        ),
        tool!(
            "open_project",
            "List PCB documents open in the running KiCad UI and optionally check whether \
             one specific KiCad project or board is open over IPC.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Optional path to a .kicad_pro project or .kicad_pcb board to check"
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_open_project(args, ctx).await }
        ),
        tool!(
            "save_project",
            "Save the currently open PCB board file via KiCAD IPC. \
             Requires KiCAD to be running with IPC enabled.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            |args, ctx| async move { handle_save_project(args, ctx).await }
        ),
        tool!(
            "rename_project",
            "Rename a KiCAD project: renames the .kicad_pro/.kicad_sch/.kicad_pcb/.kicad_prl \
             files and rewrites the internal references that carry the old name. Renaming the \
             files alone is NOT enough — every symbol instance stores (project \"name\"), and \
             a mismatch there makes KiCAD treat the design as unannotated, losing every \
             reference designator. Equivalent to eeschema's File > Save As. Use dry_run first.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to the existing .kicad_pro file" },
                    "new_name": { "type": "string", "description": "New project name, without extension" },
                    "rename_directory": { "type": "boolean", "description": "Also rename the containing folder when it matches the old project name. Default false.", "default": false },
                    "dry_run": { "type": "boolean", "description": "Report the planned changes without touching anything. Default false.", "default": false }
                },
                "required": ["project", "new_name"]
            }),
            |args, ctx| async move { handle_rename_project(args, ctx).await }
        ),
        tool!(
            "get_project_info",
            "Read project metadata from a .kicad_pro file. Returns the project name, \
             schematic and PCB paths, last modified time, project-file format version, \
             and the generator versions recorded by the sibling design files. The \
             compatibility kicad_version is null when those siblings disagree.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the .kicad_pro project file"
                    }
                },
                "required": ["path"]
            }),
            |args, ctx| async move { handle_get_project_info(args, ctx).await }
        ),
        tool!(
            "snapshot_project",
            "Export the schematic and PCB to PDF as a timestamped snapshot/checkpoint. \
             Useful for saving progress before major edits.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": {
                        "type": "string",
                        "description": "Path to .kicad_sch file"
                    },
                    "pcb": {
                        "type": "string",
                        "description": "Optional: path to .kicad_pcb file"
                    },
                    "output_dir": {
                        "type": "string",
                        "description": "Directory to write snapshot PDFs"
                    },
                    "label": {
                        "type": "string",
                        "description": "Optional label to include in the filename"
                    }
                },
                "required": ["schematic", "output_dir"]
            }),
            |args, ctx| async move { handle_snapshot_project(args, ctx).await }
        ),
        tool!(
            "open_schematic_viewer",
            "Launch the live schematic viewer. The viewer shows the schematic as SVG and \
             auto-refreshes when the file changes. Use this after placing components so the \
             user can see the schematic in real-time as you edit it. For hierarchical designs, \
             pass the root schematic — the viewer discovers every sheet reachable from it and \
             shows a sheet selector.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": {
                        "type": "string",
                        "description": "Path to the root .kicad_sch file to view"
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_open_viewer(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

fn existing_project_paths(paths: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut existing = Vec::new();
    for path in paths {
        match std::fs::symlink_metadata(path) {
            Ok(_) => existing.push(path.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(existing)
}

fn project_conflict(paths: Vec<PathBuf>) -> CallToolResult {
    let paths: Vec<String> = paths
        .into_iter()
        .map(|path| path.display().to_string())
        .collect();
    CallToolResult::error_kind(
        ToolErrorKind::Conflict {
            paths: paths.clone(),
        },
        format!(
            "Project creation would replace existing path(s): {}",
            paths.join(", ")
        ),
    )
}

fn create_project_files(
    project_dir: &Path,
    name: &str,
    pro_path: &Path,
    sch_path: &Path,
    pcb_path: &Path,
) -> Result<(), SexpError> {
    commit_file_transaction(
        project_dir,
        vec![
            FileTransition::create(pro_path, blank_kicad_pro(name)),
            FileTransition::create(sch_path, blank_kicad_sch()),
            FileTransition::create(pcb_path, blank_kicad_pcb()),
        ],
    )?;
    Ok(())
}

async fn handle_create_project(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "path")?;
    let name = match require_str(args, "name") {
        Ok(n) => n.to_string(),
        Err(e) => return Ok(e),
    };

    let pro_path = path.join(format!("{}.kicad_pro", name));
    let sch_path = path.join(format!("{}.kicad_sch", name));
    let pcb_path = path.join(format!("{}.kicad_pcb", name));
    let project_paths = vec![pro_path.clone(), sch_path.clone(), pcb_path.clone()];

    let existing = tokio::task::spawn_blocking({
        let project_paths = project_paths.clone();
        move || existing_project_paths(&project_paths)
    })
    .await??;
    if !existing.is_empty() {
        return Ok(project_conflict(existing));
    }

    tokio::fs::create_dir_all(&path).await?;

    let transaction = tokio::task::spawn_blocking({
        let project_dir = path.clone();
        let name = name.clone();
        let pro_path = pro_path.clone();
        let sch_path = sch_path.clone();
        let pcb_path = pcb_path.clone();
        move || create_project_files(&project_dir, &name, &pro_path, &sch_path, &pcb_path)
    })
    .await?;
    match transaction {
        Ok(()) => {}
        Err(SexpError::TransactionConflict { path, .. }) => {
            return Ok(project_conflict(vec![path]));
        }
        Err(error) => return Err(error.into()),
    }

    Ok(CallToolResult::json(&json!({
        "created": true,
        "project_file": pro_path.display().to_string(),
        "schematic": sch_path.display().to_string(),
        "pcb": pcb_path.display().to_string()
    })))
}

fn requested_board_path(
    args: &serde_json::Value,
) -> Result<Option<(String, PathBuf)>, CallToolResult> {
    let Some(value) = args.get("path").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(raw) = value.as_str().filter(|path| !path.trim().is_empty()) else {
        return Err(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "path".to_string(),
                reason: "must be a non-empty .kicad_pro or .kicad_pcb path".to_string(),
            },
            "Argument 'path' must be a non-empty .kicad_pro or .kicad_pcb path",
        ));
    };
    let requested = PathBuf::from(raw);
    let extension = requested
        .extension()
        .and_then(|extension| extension.to_str());
    let board = match extension {
        Some(extension) if extension.eq_ignore_ascii_case("kicad_pro") => {
            requested.with_extension("kicad_pcb")
        }
        Some(extension) if extension.eq_ignore_ascii_case("kicad_pcb") => requested.clone(),
        _ => {
            return Err(CallToolResult::error_kind(
                ToolErrorKind::InvalidArgument {
                    field: "path".to_string(),
                    reason: "must end in .kicad_pro or .kicad_pcb".to_string(),
                },
                "Argument 'path' must end in .kicad_pro or .kicad_pcb",
            ));
        }
    };
    Ok(Some((raw.to_string(), board)))
}

async fn handle_open_project(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let requested = match requested_board_path(args) {
        Ok(requested) => requested,
        Err(error) => return Ok(error),
    };
    let ipc = konnect_ipc::KiCadIpcClient::new(&ctx.config.ipc_address);
    let connected = ipc.ping().unwrap_or(false);
    let (open_boards, open_boards_error) = if connected {
        match ipc.get_open_board_paths() {
            Ok(paths) => (
                paths
                    .into_iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
                None,
            ),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        }
    } else {
        (Vec::new(), None)
    };
    let (requested_path, requested_board, requested_open, requested_check_error) = match requested {
        Some((project_or_board, board)) if connected => match ipc.find_open_board(&board) {
            Ok(_) => {
                ctx.board_session.observe_live(&board);
                (
                    Some(project_or_board),
                    Some(board.display().to_string()),
                    Some(true),
                    None,
                )
            }
            Err(error) => (
                Some(project_or_board),
                Some(board.display().to_string()),
                Some(false),
                Some(format!("{error:#}")),
            ),
        },
        Some((project_or_board, board)) => (
            Some(project_or_board),
            Some(board.display().to_string()),
            None,
            None,
        ),
        None => (None, None, None, None),
    };

    let message = if !connected {
        "KiCad IPC is not reachable. Start KiCad and enable the IPC API, or work in file-only mode."
    } else if requested_open == Some(true) {
        "The requested board is open in KiCad."
    } else if requested_open == Some(false) {
        "KiCad IPC is available, but the requested board is not open."
    } else {
        "KiCad IPC is available; open PCB documents are listed in open_boards."
    };

    Ok(CallToolResult::json(&json!({
        "kicad_ui_running": connected,
        "ipc_available": connected,
        "ipc_address": ctx.config.ipc_address,
        "open_board_count": open_boards.len(),
        "open_boards": open_boards,
        "open_boards_error": open_boards_error,
        "requested_path": requested_path,
        "requested_board": requested_board,
        "requested_open": requested_open,
        "requested_check_error": requested_check_error,
        "message": message
    })))
}

async fn handle_save_project(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let ipc = konnect_ipc::KiCadIpcClient::new(&ctx.config.ipc_address);
    ipc.save_board()?;
    Ok(CallToolResult::text("Board saved successfully."))
}

async fn handle_get_project_info(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "path")?;

    // Demonstration of the structured-error pattern: returning a FileNotFound
    // kind lets clients branch (e.g. show a file picker) without string-parsing
    // the message.
    if !path.exists() {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::FileNotFound {
                path: path.display().to_string(),
            },
            format!("Project file not found: {}", path.display()),
        ));
    }

    let content = tokio::fs::read_to_string(&path).await?;
    let pro: serde_json::Value = serde_json::from_str(&content)?;

    let dir = path.parent().unwrap_or(&path);
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let sch = dir.join(format!("{}.kicad_sch", stem));
    let pcb = dir.join(format!("{}.kicad_pcb", stem));

    let meta = tokio::fs::metadata(&path).await.ok();
    let modified = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    let schematic_generator_version = read_generator_version(&sch).await;
    let pcb_generator_version = read_generator_version(&pcb).await;
    let kicad_version = match (
        schematic_generator_version.as_deref(),
        pcb_generator_version.as_deref(),
    ) {
        (Some(schematic), Some(pcb)) if schematic == pcb => Some(schematic.to_string()),
        (Some(schematic), None) => Some(schematic.to_string()),
        (None, Some(pcb)) => Some(pcb.to_string()),
        _ => None,
    };

    Ok(CallToolResult::json(&json!({
        "name": stem,
        "path": path.display().to_string(),
        "schematic": sch.display().to_string(),
        "schematic_exists": sch.exists(),
        "pcb": pcb.display().to_string(),
        "pcb_exists": pcb.exists(),
        "last_modified_unix": modified,
        "project_file_version": pro.get("meta").and_then(|m| m.get("version")),
        "schematic_generator_version": schematic_generator_version,
        "pcb_generator_version": pcb_generator_version,
        "kicad_version": kicad_version
    })))
}

async fn read_generator_version(path: &Path) -> Option<String> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let root = konnect_sexp::parse_sexp(&content).ok()?;
    root.find_str("generator_version").map(str::to_owned)
}

async fn handle_snapshot_project(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let schematic = get_path(args, "schematic")?;
    let output_dir = get_path(args, "output_dir")?;
    let label = opt_str(args, "label").unwrap_or("snapshot");

    tokio::fs::create_dir_all(&output_dir).await?;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let stem = schematic
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let pdf_name = format!("{}_{}_{}.pdf", stem, label, ts);
    let pdf_path = output_dir.join(&pdf_name);

    crate::tools::cli::export_schematic_pdf(
        &ctx.config.kicad_cli,
        &schematic,
        &pdf_path,
        &crate::tools::cli::SchematicPdfOptions::default(),
    )
    .await?;

    let mut result = json!({
        "snapshot": pdf_path.display().to_string(),
        "label": label,
        "timestamp": ts
    });

    // Optionally snapshot PCB too
    if let Some(pcb_str) = opt_str(args, "pcb") {
        let pcb = PathBuf::from(pcb_str);
        let pcb_pdf_name = format!("{}_pcb_{}_{}.pdf", stem, label, ts);
        let pcb_pdf_path = output_dir.join(&pcb_pdf_name);
        let layers = &["F.Cu", "B.Cu", "F.Silkscreen", "B.Silkscreen", "Edge.Cuts"];
        let _ = crate::tools::cli::export_pdf(
            &ctx.config.kicad_cli,
            &pcb,
            &pcb_pdf_path,
            layers,
            false,
        )
        .await;
        result["pcb_snapshot"] = json!(pcb_pdf_path.display().to_string());
    }

    Ok(CallToolResult::json(&result))
}

async fn handle_open_viewer(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;

    if !sch_path.exists() {
        return Ok(CallToolResult::error(format!(
            "File not found: {}",
            sch_path.display()
        )));
    }

    // Find the viewer binary — it should be next to the konnect binary
    let viewer_binary = find_viewer_binary();

    match viewer_binary {
        Some(viewer_path) => {
            tracing::info!(
                "[BETA] Launching schematic viewer: {} {}",
                viewer_path.display(),
                sch_path.display()
            );

            // Spawn as detached process, forwarding the configured kicad-cli
            // path so the viewer renders with the same binary we use.
            let mut cmd = std::process::Command::new(&viewer_path);
            if !ctx.config.kicad_cli.is_empty() {
                cmd.arg("--kicad-cli").arg(&ctx.config.kicad_cli);
            }
            let child = cmd.arg(&sch_path).spawn();

            match child {
                Ok(_) => Ok(CallToolResult::text(
                    serde_json::to_string(&json!({
                        "launched": true,
                        "viewer": viewer_path.to_str().unwrap_or(""),
                        "schematic": sch_path.to_str().unwrap_or(""),
                        "note": "Schematic viewer opened. It will auto-refresh as you make changes to the schematic file."
                    }))
                    .unwrap(),
                )),
                Err(e) => Ok(CallToolResult::error(format!("Failed to launch viewer: {}", e))),
            }
        }
        None => Ok(CallToolResult::error(
            "Schematic viewer binary (schematic-viewer.exe) not found. \
             It should be in the same directory as konnect.exe.",
        )),
    }
}

fn find_viewer_binary() -> Option<std::path::PathBuf> {
    // Check next to the current executable
    if let Ok(exe_path) = std::env::current_exe() {
        let dir = exe_path.parent()?;
        let viewer = dir.join(if cfg!(target_os = "windows") {
            "schematic-viewer.exe"
        } else {
            "schematic-viewer"
        });
        if viewer.exists() {
            return Some(viewer);
        }
    }

    // Check common locations
    let candidates = ["schematic-viewer.exe", "schematic-viewer"];
    for c in &candidates {
        let p = std::path::PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

// ─── Blank file templates ──────────────────────────────────────────────────────

fn blank_kicad_pro(name: &str) -> String {
    format!(
        r#"{{
  "meta": {{
    "filename": "{name}.kicad_pro",
    "version": 1
  }},
  "board": {{
    "design_settings": {{}}
  }},
  "schematic": {{
    "legacy_lib_dir": "",
    "legacy_lib_list": []
  }}
}}
"#
    )
}

fn blank_kicad_sch() -> String {
    crate::tools::blank_schematic_template()
}

fn blank_kicad_pcb() -> &'static str {
    "(kicad_pcb\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(general\n\t\t(thickness 1.6)\n\t)\n\t(paper \"A4\")\n\t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(31 \"B.Cu\" signal)\n\t\t(32 \"B.Adhes\" user \"B.Adhesive\")\n\t\t(33 \"F.Adhes\" user \"F.Adhesive\")\n\t\t(34 \"B.Paste\" user)\n\t\t(35 \"F.Paste\" user)\n\t\t(36 \"B.SilkS\" user \"B.Silkscreen\")\n\t\t(37 \"F.SilkS\" user \"F.Silkscreen\")\n\t\t(38 \"B.Mask\" user)\n\t\t(39 \"F.Mask\" user)\n\t\t(40 \"Dwgs.User\" user \"User.Drawings\")\n\t\t(41 \"Cmts.User\" user \"User.Comments\")\n\t\t(44 \"Edge.Cuts\" user)\n\t\t(45 \"Margin\" user)\n\t\t(46 \"B.CrtYd\" user \"B.Courtyard\")\n\t\t(47 \"F.CrtYd\" user \"F.Courtyard\")\n\t\t(48 \"B.Fab\" user)\n\t\t(49 \"F.Fab\" user)\n\t)\n\t(setup\n\t\t(pad_to_mask_clearance 0.05)\n\t)\n\t(net 0 \"\")\n)\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
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

    // ─── Blank file templates ─────────────────────────────────────────────

    #[test]
    fn blank_kicad_pro_is_valid_json_with_name() {
        let content = blank_kicad_pro("my_board");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["meta"]["filename"], "my_board.kicad_pro");
    }

    #[test]
    fn blank_kicad_sch_has_expected_header() {
        let content = blank_kicad_sch();
        assert!(content.starts_with("(kicad_sch"));
        assert!(content.contains("(lib_symbols"));
    }

    #[test]
    fn blank_kicad_sch_has_root_uuid() {
        // Without a root (uuid ...) KiCAD's netlister silently drops every
        // wire-only net (symbol instance paths can't resolve).
        let content = blank_kicad_sch();
        assert!(content.contains("(uuid \""));
        let sch = {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("t.kicad_sch");
            std::fs::write(&path, &content).unwrap();
            konnect_schematic_editor::Schematic::load(&path).unwrap()
        };
        let uuid = sch.uuid.expect("blank schematic must carry a root uuid");
        assert_eq!(uuid.len(), 36, "expected a v4 uuid, got '{uuid}'");
    }

    #[test]
    fn blank_kicad_pcb_declares_core_layers() {
        let content = blank_kicad_pcb();
        assert!(content.contains("\"F.Cu\""));
        assert!(content.contains("\"B.Cu\""));
        assert!(content.contains("\"Edge.Cuts\""));
    }

    #[test]
    fn open_project_maps_a_project_file_to_its_board() {
        let requested = requested_board_path(&json!({ "path": "/work/voice.kicad_pro" }))
            .unwrap()
            .unwrap();
        assert_eq!(requested.0, "/work/voice.kicad_pro");
        assert_eq!(requested.1, PathBuf::from("/work/voice.kicad_pcb"));

        let board = requested_board_path(&json!({ "path": "/work/voice.kicad_pcb" }))
            .unwrap()
            .unwrap();
        assert_eq!(board.1, PathBuf::from("/work/voice.kicad_pcb"));
    }

    #[test]
    fn open_project_rejects_a_path_it_cannot_check_over_pcb_ipc() {
        let error = requested_board_path(&json!({ "path": "/work/voice.kicad_sch" }))
            .expect_err("schematic documents are not exposed by this IPC query");
        assert_eq!(
            extract_error_kind(&error).as_deref(),
            Some("invalid_argument")
        );
    }

    // ─── handle_create_project ─────────────────────────────────────────────

    #[tokio::test]
    async fn create_project_writes_all_three_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({
            "path": dir.path().to_str().unwrap(),
            "name": "widget"
        });

        let result = handle_create_project(&args, &ctx)
            .await
            .expect("handler should succeed");
        assert!(!result.is_error);

        assert!(dir.path().join("widget.kicad_pro").exists());
        assert!(dir.path().join("widget.kicad_sch").exists());
        assert!(dir.path().join("widget.kicad_pcb").exists());

        let pro_content = tokio::fs::read_to_string(dir.path().join("widget.kicad_pro"))
            .await
            .unwrap();
        let pro: serde_json::Value = serde_json::from_str(&pro_content).unwrap();
        assert_eq!(pro["meta"]["filename"], "widget.kicad_pro");
    }

    #[tokio::test]
    async fn create_project_rejects_a_partially_populated_directory_without_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let existing_path = dir.path().join("widget.kicad_sch");
        tokio::fs::write(&existing_path, b"existing schematic")
            .await
            .unwrap();
        let ctx = test_ctx();
        let args = json!({
            "path": dir.path().to_str().unwrap(),
            "name": "widget"
        });

        let result = handle_create_project(&args, &ctx)
            .await
            .expect("handler should return a structured conflict");

        assert!(result.is_error);
        assert_eq!(extract_error_kind(&result).as_deref(), Some("conflict"));
        let body = response_json(&result);
        assert_eq!(
            body["error"]["paths"],
            json!([existing_path.display().to_string()])
        );
        assert_eq!(
            tokio::fs::read(&existing_path).await.unwrap(),
            b"existing schematic"
        );
        assert!(!dir.path().join("widget.kicad_pro").exists());
        assert!(!dir.path().join("widget.kicad_pcb").exists());
    }

    #[tokio::test]
    async fn create_project_rejects_a_completed_project_without_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({
            "path": dir.path().to_str().unwrap(),
            "name": "widget"
        });
        handle_create_project(&args, &ctx)
            .await
            .expect("initial creation should succeed");
        let paths = [
            dir.path().join("widget.kicad_pro"),
            dir.path().join("widget.kicad_sch"),
            dir.path().join("widget.kicad_pcb"),
        ];
        let before = [
            tokio::fs::read(&paths[0]).await.unwrap(),
            tokio::fs::read(&paths[1]).await.unwrap(),
            tokio::fs::read(&paths[2]).await.unwrap(),
        ];

        let result = handle_create_project(&args, &ctx)
            .await
            .expect("repeat creation should return a conflict");

        assert!(result.is_error);
        assert_eq!(extract_error_kind(&result).as_deref(), Some("conflict"));
        let body = response_json(&result);
        assert_eq!(body["error"]["paths"].as_array().unwrap().len(), 3);
        for (path, original) in paths.iter().zip(before) {
            assert_eq!(tokio::fs::read(path).await.unwrap(), original);
        }
    }

    #[tokio::test]
    async fn create_project_missing_name_returns_structured_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({ "path": dir.path().to_str().unwrap() });

        let result = handle_create_project(&args, &ctx)
            .await
            .expect("handler should return Ok even on validation failure");
        assert!(result.is_error);
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
    }

    // ─── handle_get_project_info ───────────────────────────────────────────

    #[tokio::test]
    async fn get_project_info_reports_existing_sibling_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let create_args = json!({
            "path": dir.path().to_str().unwrap(),
            "name": "widget"
        });
        handle_create_project(&create_args, &ctx)
            .await
            .expect("setup: create_project should succeed");

        let pro_path = dir.path().join("widget.kicad_pro");
        let info_args = json!({ "path": pro_path.to_str().unwrap() });
        let result = handle_get_project_info(&info_args, &ctx)
            .await
            .expect("handler should succeed");
        assert!(!result.is_error);

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["name"], "widget");
        assert_eq!(parsed["schematic_exists"], true);
        assert_eq!(parsed["pcb_exists"], true);
        assert_eq!(parsed["project_file_version"], 1);
        assert_eq!(parsed["schematic_generator_version"], "10.0");
        assert_eq!(parsed["pcb_generator_version"], "10.0");
        assert_eq!(parsed["kicad_version"], "10.0");
    }

    #[tokio::test]
    async fn get_project_info_does_not_claim_a_version_when_siblings_disagree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let create_args = json!({
            "path": dir.path().to_str().unwrap(),
            "name": "widget"
        });
        handle_create_project(&create_args, &ctx)
            .await
            .expect("setup: create_project should succeed");

        let pcb_path = dir.path().join("widget.kicad_pcb");
        let pcb = std::fs::read_to_string(&pcb_path).expect("read generated board");
        std::fs::write(
            &pcb_path,
            pcb.replace("generator_version \"10.0\"", "generator_version \"10.1\""),
        )
        .expect("write board with a distinct generator version");

        let pro_path = dir.path().join("widget.kicad_pro");
        let info_args = json!({ "path": pro_path.to_str().unwrap() });
        let result = handle_get_project_info(&info_args, &ctx)
            .await
            .expect("handler should succeed");
        let parsed = response_json(&result);

        assert_eq!(parsed["schematic_generator_version"], "10.0");
        assert_eq!(parsed["pcb_generator_version"], "10.1");
        assert!(parsed["kicad_version"].is_null());
    }

    #[tokio::test]
    async fn get_project_info_missing_file_returns_file_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let missing = dir.path().join("does_not_exist.kicad_pro");
        let args = json!({ "path": missing.to_str().unwrap() });

        let result = handle_get_project_info(&args, &ctx)
            .await
            .expect("handler should return Ok with a structured error body");
        assert!(result.is_error);
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("file_not_found")
        );
    }

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text content"),
        }
    }
}

// ─── rename_project ──────────────────────────────────────────────────────────

/// Project files that carry the project name, in the order they are renamed.
const PROJECT_EXTS: [&str; 4] = ["kicad_pro", "kicad_sch", "kicad_pcb", "kicad_prl"];

/// Every other `.kicad_sch` in the project directory — the child sheets.
///
/// A hierarchical design keeps each sheet in its own file, and **every one of
/// them** stores `(project "NAME"` on its symbol instances: KiCad's own
/// `complex_hierarchy` demo has 46 of them in `ampli_ht.kicad_sch` alone,
/// which is not named after the project and so is never renamed. Rewriting
/// only the root sheet leaves those pointing at the old name, and KiCad reads
/// their symbols as unannotated — the exact failure this tool exists to
/// prevent, moved from the root sheet to the children.
///
/// Child sheets are rewritten in place; they are not renamed, since their
/// names are referenced by `(sheet … (property "Sheetfile" …))` entries.
fn sibling_sheets(
    dir: &std::path::Path,
    already: &[std::path::PathBuf],
) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sheets: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("kicad_sch"))
        .filter(|p| !already.contains(p))
        .collect();
    // Directory order is filesystem-defined; a stable report beats a shuffled one.
    sheets.sort();
    sheets
}

/// Rewrite the project name only where it is structurally meaningful.
///
/// A blind `text.replace(old, new)` corrupts unrelated content, and the blast
/// radius scales with how ordinary the name is: a project called `led` would
/// rewrite the footprint `LED_THT:LED_D3.0mm`, a net named `LED_LIGHT` and
/// every value containing the substring, silently, across the whole board and
/// schematic.
///
/// Every real occurrence is quoted, and the two file families need different
/// care:
///
/// - `.kicad_sch` / `.kicad_pcb` hold user content, so only `(project "NAME"`
///   is touched — the key symbol instances hang their annotation off. Getting
///   this wrong is what makes KiCad treat the design as unannotated.
/// - `.kicad_pro` / `.kicad_prl` are settings/metadata with no netlist in
///   them, so quoted whole-string matches and `NAME.kicad_*` filenames are
///   safe there.
fn rewrite_project_references(text: &str, old: &str, new: &str, sexp: bool) -> (String, usize) {
    let mut out = text.to_string();
    let mut hits = 0usize;

    let swap = |out: &mut String, hits: &mut usize, from: &str, to: &str| {
        if from != to {
            let n = out.matches(from).count();
            if n > 0 {
                *hits += n;
                *out = out.replace(from, to);
            }
        }
    };

    if sexp {
        // The annotation key, and nothing else.
        swap(
            &mut out,
            &mut hits,
            &format!("(project \"{old}\""),
            &format!("(project \"{new}\""),
        );
    } else {
        for ext in PROJECT_EXTS {
            swap(
                &mut out,
                &mut hits,
                &format!("\"{old}.{ext}\""),
                &format!("\"{new}.{ext}\""),
            );
        }
        // Bare quoted name: the `name` field and the root sheet entry.
        swap(
            &mut out,
            &mut hits,
            &format!("\"{old}\""),
            &format!("\"{new}\""),
        );
    }

    (out, hits)
}

async fn handle_rename_project(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let pro_path = get_path(args, "project")?;
    let new_name = match require_str(args, "new_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let dry_run = args
        .get("dry_run")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let rename_dir = args
        .get("rename_directory")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    if !pro_path.exists() {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::FileNotFound {
                path: pro_path.display().to_string(),
            },
            format!("Project file not found: {}", pro_path.display()),
        ));
    }
    let Some(dir) = pro_path.parent().map(std::path::Path::to_path_buf) else {
        return Ok(CallToolResult::error(
            "project path has no parent directory",
        ));
    };
    let Some(old_name) = pro_path.file_stem().and_then(|s| s.to_str()) else {
        return Ok(CallToolResult::error("project path has no file stem"));
    };
    let old_name = old_name.to_string();

    if new_name == old_name {
        return Ok(CallToolResult::json(&json!({
            "renamed": false, "note": "new_name matches the current name"
        })));
    }
    // A name with a path separator would move the project, not rename it.
    if new_name.contains('/') || new_name.contains('\\') {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "new_name".to_string(),
                reason: "must be a bare name, not a path".to_string(),
            },
            "new_name must not contain a path separator.",
        ));
    }

    let mut planned_files = Vec::new();
    let mut collisions = Vec::new();
    for ext in PROJECT_EXTS {
        let from = dir.join(format!("{old_name}.{ext}"));
        if !from.exists() {
            continue;
        }
        let to = dir.join(format!("{new_name}.{ext}"));
        if to.exists() {
            collisions.push(to.display().to_string());
        }
        planned_files.push((from, to));
    }
    if !collisions.is_empty() {
        return Ok(CallToolResult::error(format!(
            "Refusing to rename: these target files already exist: {}",
            collisions.join(", ")
        )));
    }

    // Rewriting content is what keeps annotations attached: each symbol
    // instance in the schematic stores `(project "NAME"`, and the .kicad_pro
    // and .kicad_prl embed their own filenames.
    let mut rewritten = Vec::new();
    if !dry_run {
        // Rename the set, undoing what landed if one fails: a half-renamed
        // project is one KiCad cannot open at all.
        let mut done: Vec<(&std::path::PathBuf, &std::path::PathBuf)> = Vec::new();
        for (from, to) in &planned_files {
            if let Err(e) = std::fs::rename(from, to) {
                for (undo_from, undo_to) in done.iter().rev() {
                    let _ = std::fs::rename(undo_to, undo_from);
                }
                return Ok(CallToolResult::error(format!(
                    "Rename failed on {} ({e}); rolled back, nothing was changed.",
                    to.display()
                )));
            }
            done.push((from, to));
        }
        // The renamed set plus every child sheet: a hierarchical design keeps
        // `(project "NAME"` in each sheet file, and the children are never
        // renamed, so rewriting only the root would de-annotate them.
        let renamed: Vec<std::path::PathBuf> =
            planned_files.iter().map(|(_, to)| to.clone()).collect();
        let mut to_rewrite = renamed.clone();
        to_rewrite.extend(sibling_sheets(&dir, &renamed));

        for target in &to_rewrite {
            let sexp = matches!(
                target.extension().and_then(|s| s.to_str()),
                Some("kicad_sch" | "kicad_pcb")
            );
            let text = std::fs::read_to_string(target)?;
            let (updated, hits) = rewrite_project_references(&text, &old_name, &new_name, sexp);
            if hits > 0 {
                konnect_sexp::writer::write_atomic(target, &updated)?;
                rewritten.push(json!({
                    "file": target.file_name().and_then(|s| s.to_str()).unwrap_or_default(),
                    "references_updated": hits,
                }));
            }
        }
    } else {
        let sources: Vec<std::path::PathBuf> =
            planned_files.iter().map(|(from, _)| from.clone()).collect();
        let mut previewed: Vec<(std::path::PathBuf, String)> = planned_files
            .iter()
            .map(|(from, to)| {
                (
                    from.clone(),
                    to.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect();
        // Child sheets keep their names, so the reported name is their own.
        for sheet in sibling_sheets(&dir, &sources) {
            let name = sheet
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            previewed.push((sheet, name));
        }

        for (source, reported_name) in &previewed {
            let sexp = matches!(
                source.extension().and_then(|s| s.to_str()),
                Some("kicad_sch" | "kicad_pcb")
            );
            let text = std::fs::read_to_string(source).unwrap_or_default();
            let (_, hits) = rewrite_project_references(&text, &old_name, &new_name, sexp);
            rewritten.push(json!({
                "file": reported_name,
                "references_updated": hits,
            }));
        }
    }

    // The auto-backup folder is named after the project too.
    let backups_from = dir.join(format!("{old_name}-backups"));
    let mut backups = serde_json::Value::Null;
    if backups_from.is_dir() {
        let backups_to = dir.join(format!("{new_name}-backups"));
        if !dry_run && !backups_to.exists() {
            std::fs::rename(&backups_from, &backups_to)?;
        }
        backups = json!(backups_to.file_name().and_then(|s| s.to_str()));
    }

    let mut directory = serde_json::Value::Null;
    if rename_dir && dir.file_name().and_then(|s| s.to_str()) == Some(old_name.as_str()) {
        if let Some(parent) = dir.parent() {
            let new_dir = parent.join(&new_name);
            if !new_dir.exists() {
                if !dry_run {
                    std::fs::rename(&dir, &new_dir)?;
                }
                directory = json!(new_dir.display().to_string());
            }
        }
    }

    Ok(CallToolResult::json(&json!({
        "dry_run": dry_run,
        "old_name": old_name,
        "new_name": new_name,
        "files": planned_files.iter()
            .map(|(f, t)| json!({
                "from": f.file_name().and_then(|s| s.to_str()),
                "to": t.file_name().and_then(|s| s.to_str())
            }))
            .collect::<Vec<_>>(),
        "content_rewrites": rewritten,
        "backups_folder": backups,
        "directory": directory,
    })))
}

#[cfg(test)]
mod rename_rewrite_tests {
    use super::rewrite_project_references;

    /// The corruption this guards against. A project named `led` used to have
    /// every occurrence of that substring rewritten across the schematic —
    /// footprints, net names, values — because the rewrite was a bare
    /// `text.replace(old, new)`.
    #[test]
    fn an_ordinary_name_does_not_eat_unrelated_content() {
        let sch = r#"(kicad_sch
	(symbol (lib_id "Device:LED")
		(property "Footprint" "LED_THT:LED_D3.0mm")
		(property "Value" "led")
	)
	(label "LED_LIGHT" (at 10 20 0))
	(instances
		(project "led"
			(path "/abc" (reference "D1") (unit 1))
		)
	)
)
"#;
        let (out, hits) = rewrite_project_references(sch, "led", "dro", true);
        assert_eq!(hits, 1, "only the (project …) key should match");
        assert!(out.contains(r#"(project "dro""#));
        // Everything else survives untouched.
        assert!(out.contains(r#""LED_THT:LED_D3.0mm""#));
        assert!(out.contains(r#"(property "Value" "led")"#));
        assert!(out.contains(r#"(label "LED_LIGHT""#));
        assert!(out.contains(r#"(lib_id "Device:LED")"#));
    }

    #[test]
    fn sexp_files_rewrite_the_annotation_key() {
        let sch = "(instances\n\t(project \"old name\"\n\t\t(path \"/x\")\n\t)\n)\n";
        let (out, hits) = rewrite_project_references(sch, "old name", "new name", true);
        assert_eq!(hits, 1);
        assert!(out.contains("(project \"new name\""));
        assert!(!out.contains("old name"));
    }

    /// Settings files carry the name as a bare quoted string and inside
    /// `NAME.kicad_*` filenames; both must move or KiCad reopens the old paths.
    #[test]
    fn settings_files_rewrite_names_and_filenames() {
        let pro = r#"{
  "meta": { "filename": "old.kicad_pro" },
  "sheets": [ [ "uuid-1", "old" ] ],
  "schematic": { "filename": "old.kicad_sch" },
  "board": { "filename": "old.kicad_pcb" },
  "name": "old"
}
"#;
        let (out, hits) = rewrite_project_references(pro, "old", "new", false);
        assert!(hits >= 5, "expected every quoted form to move, got {hits}");
        for want in [
            "\"new.kicad_pro\"",
            "\"new.kicad_sch\"",
            "\"new.kicad_pcb\"",
            "\"uuid-1\", \"new\"",
            "\"name\": \"new\"",
        ] {
            assert!(out.contains(want), "missing {want} in:\n{out}");
        }
        assert!(!out.contains("\"old"), "an old reference survived:\n{out}");
    }

    /// A settings file must not have unrelated *unquoted* text touched, and a
    /// schematic must not have its quoted values touched at all.
    #[test]
    fn no_partial_word_matches() {
        let sch = "(property \"Value\" \"prototype\")\n(project \"proto\"\n";
        let (out, hits) = rewrite_project_references(sch, "proto", "final", true);
        assert_eq!(hits, 1);
        assert!(out.contains("\"prototype\""), "substring was eaten: {out}");
        assert!(out.contains("(project \"final\""));
    }

    #[test]
    fn a_no_op_rename_changes_nothing() {
        let sch = "(project \"same\"\n";
        let (out, hits) = rewrite_project_references(sch, "same", "same", true);
        assert_eq!(hits, 0);
        assert_eq!(out, sch);
    }
}

/// A hierarchical project keeps each sheet in its own file, and every one of
/// them stores `(project "NAME"` on its symbol instances. Only the root sheet
/// is named after the project, so a rename that rewrites just the renamed set
/// leaves every child sheet pointing at the old name — KiCad then reads those
/// symbols as unannotated, which is the failure this tool exists to prevent.
#[cfg(test)]
mod rename_hierarchy_tests {
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

    /// Root sheet + one child, shaped like KiCad's `complex_hierarchy` demo:
    /// the child is not named after the project and carries its own
    /// `(project …)` instances.
    fn hierarchy(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let pro = dir.join(format!("{name}.kicad_pro"));
        std::fs::write(
            &pro,
            format!("{{\n  \"meta\": {{\n    \"filename\": \"{name}.kicad_pro\"\n  }},\n  \"sheets\": []\n}}\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{name}.kicad_sch")),
            format!("(kicad_sch\n\t(uuid \"root\")\n\t(sheet\n\t\t(property \"Sheetfile\" \"ampli.kicad_sch\")\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(instances\n\t\t\t(project \"{name}\"\n\t\t\t\t(path \"/root\" (reference \"R1\") (unit 1))\n\t\t\t)\n\t\t)\n\t)\n)\n"),
        )
        .unwrap();
        // The child sheet: never renamed, and full of project references.
        std::fs::write(
            dir.join("ampli.kicad_sch"),
            format!("(kicad_sch\n\t(uuid \"child\")\n\t(symbol\n\t\t(lib_id \"Device:C\")\n\t\t(instances\n\t\t\t(project \"{name}\"\n\t\t\t\t(path \"/root/child\" (reference \"C1\") (unit 1))\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(instances\n\t\t\t(project \"{name}\"\n\t\t\t\t(path \"/root/child\" (reference \"R9\") (unit 1))\n\t\t\t)\n\t\t)\n\t)\n)\n"),
        )
        .unwrap();
        pro
    }

    fn body(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn renaming_rewrites_child_sheets_too() {
        let dir = tempfile::tempdir().unwrap();
        let pro = hierarchy(dir.path(), "oldproj");

        let result = handle_rename_project(
            &json!({ "project": pro.to_str().unwrap(), "new_name": "newproj" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        // The child keeps its filename and gains the new project name.
        let child = std::fs::read_to_string(dir.path().join("ampli.kicad_sch")).unwrap();
        assert_eq!(
            child.matches("(project \"newproj\"").count(),
            2,
            "every child instance must follow the rename:\n{child}"
        );
        assert!(
            !child.contains("oldproj"),
            "no stale project reference may survive:\n{child}"
        );
        let root = std::fs::read_to_string(dir.path().join("newproj.kicad_sch")).unwrap();
        assert!(root.contains("(project \"newproj\""), "{root}");
        // The child sheet is referenced by name, so it must NOT be renamed.
        assert!(dir.path().join("ampli.kicad_sch").exists());
        assert!(root.contains("\"ampli.kicad_sch\""), "{root}");

        let reported: Vec<String> = body(&result)["content_rewrites"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["file"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            reported.iter().any(|f| f == "ampli.kicad_sch"),
            "the child sheet must be reported: {reported:?}"
        );
    }

    /// dry_run must preview the child sheets too, and write nothing.
    #[tokio::test]
    async fn dry_run_previews_child_sheets_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let pro = hierarchy(dir.path(), "oldproj");
        let before = std::fs::read_to_string(dir.path().join("ampli.kicad_sch")).unwrap();

        let result = handle_rename_project(
            &json!({ "project": pro.to_str().unwrap(), "new_name": "newproj",
                     "dry_run": true }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let entry = body(&result)["content_rewrites"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["file"] == "ampli.kicad_sch")
            .cloned()
            .unwrap_or_else(|| panic!("child sheet missing from the preview: {:?}", body(&result)));
        assert_eq!(entry["references_updated"], json!(2), "{entry}");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("ampli.kicad_sch")).unwrap(),
            before,
            "dry_run must not write"
        );
        assert!(pro.exists(), "dry_run must not rename");
    }
}
