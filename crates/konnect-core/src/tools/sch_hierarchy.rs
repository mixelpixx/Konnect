//! `sch_hierarchy` toolset — PR-A: hierarchical sheet lifecycle.
//!
//! Sheet pin lifecycle (import/add/edit/delete pins) lands in a follow-up PR;
//! this toolset covers the sheet object itself: add, edit, move, delete,
//! duplicate, and the recursive hierarchy/page-numbering queries needed to
//! reason about a multi-sheet design.
//!
//! Every handler here is file-editing only — KiCAD's own IPC API has no
//! schematic-editing commands upstream (`schematic_commands.proto` is empty),
//! so there's no dual IPC/file path to maintain, unlike the PCB toolsets.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, opt_f64, opt_str, require_f64, require_str, ToolContext, ToolDef};
use konnect_schematic_editor as cse;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_hierarchical_sheet",
            "Insert a hierarchical sheet into a parent schematic, linking it to a child \
             .kicad_sch file. Creates the child file (blank) if it doesn't exist yet, or \
             links to it as-is if it does — reusing an existing file places the *same* \
             sub-circuit at a second location (KiCAD's multi-instance sheet pattern) rather \
             than duplicating it. If the linked file already has symbols in it, their \
             hierarchical instance paths are patched immediately so ERC resolves them.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to the parent .kicad_sch file" },
                    "sheet_file": { "type": "string", "description": "Filename of the child .kicad_sch, resolved relative to the parent's directory" },
                    "sheet_name": { "type": "string", "description": "Display name (Sheetname property). Default: 'Sheet'" },
                    "x": { "type": "number", "description": "Top-left X in mm. Default: 50" },
                    "y": { "type": "number", "description": "Top-left Y in mm. Default: 50" },
                    "width": { "type": "number", "description": "Sheet box width in mm. Default: 80" },
                    "height": { "type": "number", "description": "Sheet box height in mm. Default: 50" },
                    "project_name": { "type": "string", "description": "Project name key for the page-number instance entry. Default: '' (matches this codebase's existing convention for symbol instances)" }
                },
                "required": ["schematic", "sheet_file"]
            }),
            |args, ctx| async move { handle_add_hierarchical_sheet(args, ctx).await }
        ),
        tool!(
            "edit_sheet",
            "Rename, resize, reposition, or repoint (Sheetfile) an existing sheet. Provide \
             at least one of: new_name, new_file, or both x+y, or both width+height. Does \
             NOT rename the child file on disk when new_file is given — it only repoints \
             the reference; the file itself must already exist at that path.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string", "description": "Current Sheetname to look up" },
                    "new_name": { "type": "string" },
                    "new_file": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "width": { "type": "number" }, "height": { "type": "number" },
                    "project_name": { "type": "string", "description": "Default: ''" }
                },
                "required": ["schematic", "sheet_name"]
            }),
            |args, ctx| async move { handle_edit_sheet(args, ctx).await }
        ),
        tool!(
            "move_sheet",
            "Reposition a sheet on the parent canvas without touching any other field.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" }
                },
                "required": ["schematic", "sheet_name", "x", "y"]
            }),
            |args, ctx| async move { handle_move_sheet(args, ctx).await }
        ),
        tool!(
            "delete_sheet",
            "Remove a sheet reference from the parent schematic. Does NOT delete the child \
             .kicad_sch file on disk. Remaining sheets' page numbers may now have a gap — \
             call renumber_sheet_pages afterward if that matters.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "sheet_name": { "type": "string" }
                },
                "required": ["schematic", "sheet_name"]
            }),
            |args, ctx| async move { handle_delete_sheet(args, ctx).await }
        ),
        tool!(
            "duplicate_sheet",
            "Copy an existing sheet and its child .kicad_sch file under a new name/file, \
             offset slightly so the new sheet box doesn't overlap the source. The copy gets \
             its own internal schematic UUID and its symbols' hierarchical instance paths \
             are patched for the new sheet — it is a fully independent sub-circuit, not a \
             live-linked reuse (for that, use add_hierarchical_sheet pointed at the existing file).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "source_sheet_name": { "type": "string" },
                    "new_sheet_name": { "type": "string" },
                    "new_file": { "type": "string", "description": "Filename for the copy, resolved relative to the parent's directory. Must not already exist." },
                    "project_name": { "type": "string", "description": "Default: ''" }
                },
                "required": ["schematic", "source_sheet_name", "new_sheet_name", "new_file"]
            }),
            |args, ctx| async move { handle_duplicate_sheet(args, ctx).await }
        ),
        tool!(
            "get_sheet_hierarchy",
            "Recursively walk the sheet tree starting from a schematic file, returning \
             nested JSON: each sheet's name/file/uuid/position/size/page/pins plus its own \
             children. Handles missing child files and reference cycles gracefully instead \
             of failing.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Root schematic to start from" },
                    "project_name": { "type": "string", "description": "Default: ''" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_sheet_hierarchy(args, ctx).await }
        ),
        tool!(
            "renumber_sheet_pages",
            "Walk the whole sheet tree from a root schematic and reassign sequential page \
             numbers (2, 3, 4, ... — page 1 is always the root and is left untouched) in \
             depth-first order. Fixes gaps left by delete_sheet/duplicate_sheet. Only \
             touches files whose page numbers actually changed.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Root schematic to start from" },
                    "project_name": { "type": "string", "description": "Default: ''" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_renumber_sheet_pages(args, ctx).await }
        ),
    ]
}

// ─── Shared helpers ─────────────────────────────────────────────────────────

const MAX_HIERARCHY_DEPTH: usize = 20;

fn parent_dir(sch_path: &Path) -> PathBuf {
    sch_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn create_blank_schematic(path: &Path) -> anyhow::Result<()> {
    let template = "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n";
    konnect_sexp::writer::write_atomic(path, template)?;
    // Round-trip through cse so the file is normalised to its writer's format,
    // matching the existing `create_schematic` tool's behavior.
    let sch = cse::Schematic::load(path)?;
    sch.overwrite()?;
    Ok(())
}

fn next_free_page(parent: &cse::Schematic, project_name: &str) -> u32 {
    let mut max_page: u32 = 1; // page 1 is always the root sheet
    for sheet in parent.sheets.iter() {
        if let Some(p) = sheet.page(project_name) {
            if let Ok(n) = p.parse::<u32>() {
                max_page = max_page.max(n);
            }
        }
    }
    max_page + 1
}

fn sheet_json(sheet: &cse::Sheet, project_name: &str) -> Value {
    let (x, y) = sheet.position();
    json!({
        "name": sheet.name(),
        "file": sheet.file(),
        "uuid": sheet.uuid,
        "x": x,
        "y": y,
        "width": sheet.width,
        "height": sheet.height,
        "page": sheet.page(project_name),
        "pins": sheet.pins.iter().map(|p| {
            let (px, py) = p.position();
            json!({ "name": p.name, "pin_type": p.pin_type, "x": px, "y": py })
        }).collect::<Vec<_>>()
    })
}

/// Insert `sheet` into `parent`. If `patch_existing_symbols` is set, load the
/// child file and ensure every symbol in it carries an instance path for this
/// sheet's UUID — needed whenever the child file already has components in it
/// (a reused file, or a freshly duplicated one).
fn link_sheet(
    parent: &mut cse::Schematic,
    sheet: cse::Sheet,
    child_path: &Path,
    project_name: &str,
    patch_existing_symbols: bool,
) -> anyhow::Result<usize> {
    let sheet_uuid = sheet.uuid.clone();
    parent.add_sheet(sheet);

    let mut patched = 0usize;
    if patch_existing_symbols {
        let mut child = cse::Schematic::load(child_path)?;
        let hier_path = format!("/{}/", sheet_uuid);
        for sym in child.symbols.iter_mut() {
            if !sym.has_instance_path(project_name, &hier_path) {
                let reference = sym.reference().unwrap_or("").to_string();
                sym.set_instance_path(project_name, &hier_path, &reference, sym.unit);
                patched += 1;
            }
        }
        if patched > 0 {
            child.overwrite()?;
        }
    }
    Ok(patched)
}

// ─── Handlers ───────────────────────────────────────────────────────────────

async fn handle_add_hierarchical_sheet(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let parent_path = get_path(args, "schematic")?;
    let sheet_file = match require_str(args, "sheet_file") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let sheet_name = opt_str(args, "sheet_name").unwrap_or("Sheet").to_string();
    let x = opt_f64(args, "x").unwrap_or(50.0);
    let y = opt_f64(args, "y").unwrap_or(50.0);
    let width = opt_f64(args, "width").unwrap_or(80.0);
    let height = opt_f64(args, "height").unwrap_or(50.0);
    let project_name = opt_str(args, "project_name").unwrap_or("").to_string();

    let dir = parent_dir(&parent_path);
    let child_path = dir.join(&sheet_file);

    let mut parent = cse::Schematic::load(&parent_path)?;

    if parent.sheets.by_name(&sheet_name).is_some() {
        return Ok(CallToolResult::error(format!(
            "Sheet named '{}' already exists in this schematic — use edit_sheet to modify it \
             or pick a different name",
            sheet_name
        )));
    }

    let child_existed = child_path.exists();
    if !child_existed {
        create_blank_schematic(&child_path)?;
    }

    let page = next_free_page(&parent, &project_name).to_string();
    let mut sheet = cse::Sheet::new(
        sheet_name.as_str(),
        sheet_file.as_str(),
        x,
        y,
        width,
        height,
    );
    sheet.set_page(&project_name, "/", &page);

    let patched = link_sheet(
        &mut parent,
        sheet,
        &child_path,
        &project_name,
        child_existed,
    )?;
    parent.overwrite()?;

    let sheet_ref = parent.sheets.by_name(&sheet_name).expect("just added");
    Ok(CallToolResult::json(&json!({
        "added": sheet_name,
        "sheet": sheet_json(sheet_ref, &project_name),
        "child_file": child_path.display().to_string(),
        "reused_existing_file": child_existed,
        "patched_symbol_instances": patched
    })))
}

async fn handle_edit_sheet(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let project_name = opt_str(args, "project_name").unwrap_or("").to_string();

    let mut sch = cse::Schematic::load(&sch_path)?;
    let sheet = match sch.sheets.by_name_mut(&sheet_name) {
        Some(s) => s,
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                sheet_name
            )))
        }
    };

    let mut changed = Vec::new();
    if let Some(new_name) = opt_str(args, "new_name") {
        sheet.set_name(new_name);
        changed.push("name");
    }
    if let Some(new_file) = opt_str(args, "new_file") {
        sheet.set_file(new_file);
        changed.push("file");
    }
    if let (Some(x), Some(y)) = (opt_f64(args, "x"), opt_f64(args, "y")) {
        sheet.move_to(x, y);
        changed.push("position");
    }
    if let (Some(w), Some(h)) = (opt_f64(args, "width"), opt_f64(args, "height")) {
        sheet.set_size(w, h);
        changed.push("size");
    }

    if changed.is_empty() {
        return Ok(CallToolResult::error(
            "No fields to change — provide at least one of: new_name, new_file, x+y, width+height",
        ));
    }

    let summary = sheet_json(sheet, &project_name);
    sch.overwrite()?;
    Ok(CallToolResult::json(&json!({
        "edited": sheet_name,
        "changed_fields": changed,
        "sheet": summary
    })))
}

async fn handle_move_sheet(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    match sch.sheets.by_name_mut(&sheet_name) {
        Some(sheet) => {
            sheet.move_to(x, y);
            sch.overwrite()?;
            Ok(CallToolResult::json(
                &json!({ "moved": sheet_name, "x": x, "y": y }),
            ))
        }
        None => Ok(CallToolResult::error(format!(
            "Sheet '{}' not found",
            sheet_name
        ))),
    }
}

async fn handle_delete_sheet(args: &Value, _ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sheet_name = match require_str(args, "sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;
    match sch.sheets.remove_by_name(&sheet_name) {
        Some(removed) => {
            sch.overwrite()?;
            Ok(CallToolResult::json(&json!({
                "deleted": sheet_name,
                "child_file_preserved": removed.file(),
                "note": "The child schematic file was not deleted. Remaining sheets' page \
                         numbers may now have a gap — call renumber_sheet_pages if needed."
            })))
        }
        None => Ok(CallToolResult::error(format!(
            "Sheet '{}' not found",
            sheet_name
        ))),
    }
}

async fn handle_duplicate_sheet(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let source_name = match require_str(args, "source_sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_name = match require_str(args, "new_sheet_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_file = match require_str(args, "new_file") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let project_name = opt_str(args, "project_name").unwrap_or("").to_string();

    let mut parent = cse::Schematic::load(&sch_path)?;

    if parent.sheets.by_name(&new_name).is_some() {
        return Ok(CallToolResult::error(format!(
            "Sheet named '{}' already exists",
            new_name
        )));
    }

    let (src_x, src_y, src_w, src_h, src_file) = match parent.sheets.by_name(&source_name) {
        Some(s) => {
            let (x, y) = s.position();
            (x, y, s.width, s.height, s.file().to_string())
        }
        None => {
            return Ok(CallToolResult::error(format!(
                "Sheet '{}' not found",
                source_name
            )))
        }
    };

    let dir = parent_dir(&sch_path);
    let source_child = dir.join(&src_file);
    let new_child = dir.join(&new_file);

    if new_child.exists() {
        return Ok(CallToolResult::error(format!(
            "'{}' already exists — pick a different file name, or use add_hierarchical_sheet \
             to link the existing file instead of duplicating",
            new_file
        )));
    }
    if !source_child.exists() {
        return Ok(CallToolResult::error(format!(
            "Source sheet's file '{}' was not found on disk — cannot duplicate",
            src_file
        )));
    }

    std::fs::copy(&source_child, &new_child)?;

    // Give the copy its own schematic-level identity so KiCAD doesn't see two
    // files sharing the same internal UUID.
    {
        let mut copied = cse::Schematic::load(&new_child)?;
        copied.uuid = Some(uuid::Uuid::new_v4().to_string());
        copied.overwrite()?;
    }

    const DUPLICATE_OFFSET_MM: f64 = 20.0;
    let page = next_free_page(&parent, &project_name).to_string();
    let mut new_sheet = cse::Sheet::new(
        new_name.as_str(),
        new_file.as_str(),
        src_x + DUPLICATE_OFFSET_MM,
        src_y + DUPLICATE_OFFSET_MM,
        src_w,
        src_h,
    );
    new_sheet.set_page(&project_name, "/", &page);

    let patched = link_sheet(&mut parent, new_sheet, &new_child, &project_name, true)?;
    parent.overwrite()?;

    let sheet_ref = parent.sheets.by_name(&new_name).expect("just added");
    Ok(CallToolResult::json(&json!({
        "duplicated_from": source_name,
        "sheet": sheet_json(sheet_ref, &project_name),
        "child_file": new_child.display().to_string(),
        "patched_symbol_instances": patched
    })))
}

async fn handle_get_sheet_hierarchy(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root_path = get_path(args, "schematic")?;
    let project_name = opt_str(args, "project_name").unwrap_or("").to_string();

    if !root_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root_path.display()
        )));
    }

    let mut visited = HashSet::new();
    let tree = build_hierarchy_node(&root_path, &project_name, 0, &mut visited)?;
    Ok(CallToolResult::json(&tree))
}

fn build_hierarchy_node(
    path: &Path,
    project_name: &str,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
) -> anyhow::Result<Value> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    if depth > MAX_HIERARCHY_DEPTH {
        return Ok(json!({
            "file": path.display().to_string(),
            "error": "max hierarchy depth exceeded — possible reference cycle",
            "children": []
        }));
    }
    if !visited.insert(canon.clone()) {
        return Ok(json!({
            "file": path.display().to_string(),
            "error": "cycle detected — this file is already an ancestor in this tree",
            "children": []
        }));
    }

    let sch = match cse::Schematic::load(path) {
        Ok(s) => s,
        Err(e) => {
            visited.remove(&canon);
            return Ok(json!({
                "file": path.display().to_string(),
                "error": format!("failed to load: {}", e),
                "children": []
            }));
        }
    };

    let dir = parent_dir(path);
    let mut children = Vec::new();
    for sheet in sch.sheets.iter() {
        let child_path = dir.join(sheet.file());
        let mut node = sheet_json(sheet, project_name);
        if child_path.exists() {
            let sub = build_hierarchy_node(&child_path, project_name, depth + 1, visited)?;
            node["children"] = sub["children"].clone();
            if let Some(err) = sub.get("error") {
                node["error"] = err.clone();
            }
        } else {
            node["children"] = json!([]);
            node["error"] = json!("child file not found on disk");
        }
        children.push(node);
    }
    visited.remove(&canon);

    Ok(json!({
        "file": path.display().to_string(),
        "children": children
    }))
}

async fn handle_renumber_sheet_pages(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let root_path = get_path(args, "schematic")?;
    let project_name = opt_str(args, "project_name").unwrap_or("").to_string();

    if !root_path.exists() {
        return Ok(CallToolResult::error(format!(
            "Schematic '{}' not found",
            root_path.display()
        )));
    }

    let mut next_page = 2u32; // page 1 is always the root, left untouched
    let mut renumbered = Vec::new();
    let mut visited = HashSet::new();
    renumber_walk(
        &root_path,
        &project_name,
        &mut next_page,
        &mut renumbered,
        &mut visited,
    )?;

    Ok(CallToolResult::json(&json!({
        "renumbered_count": renumbered.len(),
        "pages": renumbered
    })))
}

fn renumber_walk(
    path: &Path,
    project_name: &str,
    next_page: &mut u32,
    renumbered: &mut Vec<Value>,
    visited: &mut HashSet<PathBuf>,
) -> anyhow::Result<()> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canon.clone()) {
        return Ok(()); // cycle guard — already on this DFS path, skip
    }

    let mut sch = cse::Schematic::load(path)?;
    let dir = parent_dir(path);
    let mut changed = false;

    // Snapshot the sheet order first: recursing below needs `sch` unborrowed.
    let sheet_order: Vec<(String, String)> = sch
        .sheets
        .iter()
        .map(|s| (s.name().to_string(), s.file().to_string()))
        .collect();

    for (name, file) in &sheet_order {
        let page = next_page.to_string();
        *next_page += 1;
        if let Some(sheet) = sch.sheets.by_name_mut(name) {
            if sheet.page(project_name) != Some(page.as_str()) {
                sheet.set_page(project_name, "/", &page);
                changed = true;
            }
        }
        renumbered.push(json!({ "sheet_name": name, "file": file, "page": page }));

        let child_path = dir.join(file);
        if child_path.exists() {
            renumber_walk(&child_path, project_name, next_page, renumbered, visited)?;
        }
    }

    if changed {
        sch.overwrite()?;
    }
    visited.remove(&canon);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_ctx() -> ToolContext {
        let config = ServerConfig {
            kicad_cli: "kicad-cli".into(),
            kicad_binary: "kicad".into(),
            ipc_address: String::new(),
            project_dir: None,
            jlcpcb_db_path: None,
        };
        ToolContext::new(config, Arc::new(crate::router::ToolRouter::new()))
    }

    fn blank_schematic(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        create_blank_schematic(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn add_hierarchical_sheet_creates_child_file_and_links_it() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();

        let args = json!({
            "schematic": root.display().to_string(),
            "sheet_file": "power.kicad_sch",
            "sheet_name": "Power Supply",
            "x": 20.0, "y": 20.0
        });
        let result = handle_add_hierarchical_sheet(&args, &ctx).await.unwrap();
        assert!(!result.is_error);

        assert!(tmp.path().join("power.kicad_sch").exists());
        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.len(), 1);
        assert_eq!(
            parent.sheets.by_name("Power Supply").unwrap().file(),
            "power.kicad_sch"
        );
        assert_eq!(
            parent.sheets.by_name("Power Supply").unwrap().page(""),
            Some("2")
        );
    }

    #[tokio::test]
    async fn add_hierarchical_sheet_rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();

        let args = json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" });
        handle_add_hierarchical_sheet(&args, &ctx).await.unwrap();

        let args2 = json!({ "schematic": root.display().to_string(), "sheet_file": "b.kicad_sch", "sheet_name": "A" });
        let result = handle_add_hierarchical_sheet(&args2, &ctx).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn second_sheet_gets_next_free_page() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();

        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "b.kicad_sch", "sheet_name": "B" }),
            &ctx,
        )
        .await
        .unwrap();

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.by_name("A").unwrap().page(""), Some("2"));
        assert_eq!(parent.sheets.by_name("B").unwrap().page(""), Some("3"));
    }

    #[tokio::test]
    async fn edit_sheet_renames_and_resizes() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "new_name": "Renamed", "width": 100.0, "height": 60.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert!(parent.sheets.by_name("A").is_none());
        let renamed = parent.sheets.by_name("Renamed").unwrap();
        assert_eq!(renamed.width, 100.0);
        assert_eq!(renamed.height, 60.0);
    }

    #[tokio::test]
    async fn edit_sheet_with_no_fields_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn move_sheet_updates_position_only() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A", "x": 10.0, "y": 10.0 }),
            &ctx,
        )
        .await
        .unwrap();

        handle_move_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A", "x": 99.0, "y": 88.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let parent = cse::Schematic::load(&root).unwrap();
        let sheet = parent.sheets.by_name("A").unwrap();
        assert_eq!(sheet.position(), (99.0, 88.0));
    }

    #[tokio::test]
    async fn delete_sheet_removes_reference_but_keeps_child_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_delete_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert!(parent.sheets.is_empty());
        assert!(tmp.path().join("a.kicad_sch").exists());
    }

    #[tokio::test]
    async fn delete_sheet_not_found_errors() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        let result = handle_delete_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "Nope" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn duplicate_sheet_copies_file_independently() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "amp.kicad_sch", "sheet_name": "Amp1", "x": 10.0, "y": 10.0 }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_duplicate_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "source_sheet_name": "Amp1",
                "new_sheet_name": "Amp2",
                "new_file": "amp2.kicad_sch"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        assert!(tmp.path().join("amp2.kicad_sch").exists());

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.len(), 2);
        let amp2 = parent.sheets.by_name("Amp2").unwrap();
        assert_eq!(amp2.file(), "amp2.kicad_sch");
        assert_eq!(amp2.position(), (30.0, 30.0)); // offset from source (10,10)

        // Independent files: the two schematics have different internal UUIDs.
        let sch1 = cse::Schematic::load(tmp.path().join("amp.kicad_sch")).unwrap();
        let sch2 = cse::Schematic::load(tmp.path().join("amp2.kicad_sch")).unwrap();
        assert_ne!(sch1.uuid, sch2.uuid);
    }

    #[tokio::test]
    async fn duplicate_sheet_refuses_to_overwrite_existing_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "a.kicad_sch", "sheet_name": "A" }),
            &ctx,
        )
        .await
        .unwrap();
        // A second, unrelated sheet already occupies "b.kicad_sch".
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "b.kicad_sch", "sheet_name": "B" }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_duplicate_sheet(
            &json!({
                "schematic": root.display().to_string(),
                "source_sheet_name": "A",
                "new_sheet_name": "A-copy",
                "new_file": "b.kicad_sch"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn get_sheet_hierarchy_returns_nested_tree() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "mid.kicad_sch", "sheet_name": "Mid" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": tmp.path().join("mid.kicad_sch").display().to_string(), "sheet_file": "leaf.kicad_sch", "sheet_name": "Leaf" }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_get_sheet_hierarchy(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        assert!(!result.is_error);

        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let tree: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(tree["children"][0]["name"], "Mid");
        assert_eq!(tree["children"][0]["children"][0]["name"], "Leaf");
    }

    #[tokio::test]
    async fn get_sheet_hierarchy_reports_missing_child_file() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "gone.kicad_sch", "sheet_name": "Gone" }),
            &ctx,
        )
        .await
        .unwrap();
        std::fs::remove_file(tmp.path().join("gone.kicad_sch")).unwrap();

        let result =
            handle_get_sheet_hierarchy(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let tree: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(tree["children"][0]["error"], "child file not found on disk");
    }

    #[tokio::test]
    async fn renumber_sheet_pages_closes_gap_after_delete() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let ctx = test_ctx();
        for (file, name) in [
            ("a.kicad_sch", "A"),
            ("b.kicad_sch", "B"),
            ("c.kicad_sch", "C"),
        ] {
            handle_add_hierarchical_sheet(
                &json!({ "schematic": root.display().to_string(), "sheet_file": file, "sheet_name": name }),
                &ctx,
            )
            .await
            .unwrap();
        }
        // A=2, B=3, C=4. Delete B, leaving a gap at page 3.
        handle_delete_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_name": "B" }),
            &ctx,
        )
        .await
        .unwrap();

        let result =
            handle_renumber_sheet_pages(&json!({ "schematic": root.display().to_string() }), &ctx)
                .await
                .unwrap();
        assert!(!result.is_error);

        let parent = cse::Schematic::load(&root).unwrap();
        assert_eq!(parent.sheets.by_name("A").unwrap().page(""), Some("2"));
        assert_eq!(parent.sheets.by_name("C").unwrap().page(""), Some("3"));
    }

    #[tokio::test]
    async fn linking_existing_file_with_symbols_patches_instance_paths() {
        let tmp = TempDir::new().unwrap();
        let root = blank_schematic(tmp.path(), "root.kicad_sch");
        let child_path = tmp.path().join("reused.kicad_sch");
        create_blank_schematic(&child_path).unwrap();

        // Put a symbol in the child file before it's ever linked.
        {
            let mut child = cse::Schematic::load(&child_path).unwrap();
            let mut sym = cse::Symbol::new("Device:R", 10.0, 10.0);
            sym.set_reference("R1");
            child.add_symbol(sym);
            child.overwrite().unwrap();
        }

        let ctx = test_ctx();
        let result = handle_add_hierarchical_sheet(
            &json!({ "schematic": root.display().to_string(), "sheet_file": "reused.kicad_sch", "sheet_name": "Reused" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let child = cse::Schematic::load(&child_path).unwrap();
        let sym = child.symbols.by_reference("R1").unwrap();
        assert!(sym.has_instance_path(
            "",
            &format!("/{}/", {
                let parent = cse::Schematic::load(&root).unwrap();
                parent.sheets.by_name("Reused").unwrap().uuid.clone()
            })
        ));
    }
}
