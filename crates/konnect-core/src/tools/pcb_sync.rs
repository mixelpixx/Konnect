//! Pure schematic-to-board synchronization planning.
//!
//! The public tool handler and KiCad IPC adapter live outside this module.
//! This module owns the deep planning interface: turn a KiCad-exported
//! flattened netlist plus a board snapshot into a complete, immutable plan.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::mcp::protocol::{CallToolResult, ToolContent};
use crate::tools::{
    pcb_board::{attempt_ipc_write, BoardWrite},
    ToolContext,
};
use anyhow::{bail, Context, Result};
use konnect_sexp::SexpNode;
use prost::Message;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExportedDesign {
    components: Vec<DesignComponent>,
    skipped: Vec<SkippedComponent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedComponent {
    reference: String,
    symbol_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesignComponent {
    reference: String,
    value: String,
    footprint_id: String,
    symbol_path: String,
    dnp: bool,
    pad_nets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct Bounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct BoardFootprint {
    kiid: String,
    reference: String,
    value: String,
    footprint_id: String,
    symbol_path: Option<String>,
    pad_nets: BTreeMap<String, String>,
    position: Point,
    rotation: f64,
    layer: String,
    locked: bool,
    dnp: bool,
    not_in_schematic: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct BoardState {
    footprints: Vec<BoardFootprint>,
    /// Net name to the number of routed copper objects (tracks, arcs, vias,
    /// and zones) carrying the net.
    routed_nets: BTreeMap<String, usize>,
    bounds: Bounds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PlanStatus {
    Ready,
    Noop,
    Conflict,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
struct CountPair {
    planned: usize,
    applied: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct SyncCounts {
    added: CountPair,
    updated: CountPair,
    pads_reassigned: CountPair,
    board_only_preserved: CountPair,
    skipped_by_flag: CountPair,
    conflicts: CountPair,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct PreservedBoardState {
    position: Point,
    rotation: f64,
    layer: String,
    locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PlannedChange {
    Add {
        reference: String,
        value: String,
        footprint_id: String,
        symbol_path: String,
        dnp: bool,
        pad_nets: BTreeMap<String, String>,
        position: Point,
    },
    Update {
        kiid: String,
        reference: String,
        value: String,
        symbol_path: String,
        dnp: bool,
        pad_nets: BTreeMap<String, String>,
        preserve: PreservedBoardState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SyncDiagnostic {
    code: String,
    message: String,
    reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct SyncPlan {
    status: PlanStatus,
    plan_revision: String,
    counts: SyncCounts,
    changes: Vec<PlannedChange>,
    diagnostics: Vec<SyncDiagnostic>,
}

#[derive(Debug)]
struct LiveSnapshot {
    state: BoardState,
    items: BTreeMap<String, prost_types::Any>,
    net_codes: BTreeMap<String, i32>,
    document: konnect_ipc::gen::kiapi::common::types::DocumentSpecifier,
}

#[derive(Debug)]
struct PreparedFootprint {
    pads: Vec<konnect_ipc::IpcPadDefinition>,
    graphics: Vec<konnect_ipc::IpcGraphicDefinition>,
    fields: konnect_ipc::IpcFieldPlacement,
    width: f64,
    height: f64,
}

pub(crate) async fn handle_update_pcb_from_schematic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> Result<CallToolResult> {
    let schematic = crate::tools::get_path(args, "schematic")?;
    let board = crate::tools::get_path(args, "board")?;
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let expected_revision = args["expected_plan_revision"].as_str().map(str::to_string);
    if !dry_run && expected_revision.is_none() {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "expected_plan_revision".to_string(),
                reason: "required when dry_run is false".to_string(),
            },
            "Apply requires the plan revision returned by a current dry run.",
        ));
    }
    if !schematic.exists() || !board.exists() {
        let missing = if !schematic.exists() {
            &schematic
        } else {
            &board
        };
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::FileNotFound {
                path: missing.display().to_string(),
            },
            format!("{} does not exist", missing.display()),
        ));
    }

    let hierarchy = match saved_hierarchy_files(&schematic) {
        Ok(files) => files,
        Err(error) => {
            return Ok(conflict_result(format!(
                "saved schematic preflight failed: {error:#}"
            )))
        }
    };
    let temp = tempfile::Builder::new().suffix(".net").tempfile()?;
    if let Err(error) =
        super::cli::export_netlist(&ctx.config.kicad_cli, &schematic, temp.path(), "kicadsexpr")
            .await
    {
        return Ok(conflict_result(format!(
            "KiCad netlist export failed: {error:#}"
        )));
    }
    let netlist_source = match std::fs::read_to_string(temp.path()) {
        Ok(source) => source,
        Err(error) => {
            return Ok(conflict_result(format!(
                "KiCad netlist export could not be read: {error}"
            )))
        }
    };
    let mut design = match parse_exported_netlist(&netlist_source) {
        Ok(design) => design,
        Err(error) => {
            return Ok(conflict_result(format!(
                "netlist preflight failed: {error:#}"
            )))
        }
    };
    if let Err(error) = apply_saved_symbol_flags(&hierarchy, &mut design) {
        return Ok(conflict_result(format!(
            "schematic flag preflight failed: {error:#}"
        )));
    }

    let what = if dry_run {
        "PCB sync dry run"
    } else {
        "PCB sync apply"
    };
    let ipc_board = board.clone();
    let library_board = board.clone();
    let result = attempt_ipc_write(
        ctx,
        &board,
        what,
        move |client| {
            let snapshot = snapshot_board(client, &ipc_board)?;
            let mut plan = plan_sync(&netlist_source, &design, &snapshot.state);
            let prepared = match prepare_additions(&library_board, &plan) {
                Ok(prepared) => prepared,
                Err(error) => {
                    plan.status = PlanStatus::Conflict;
                    plan.counts.added.planned = 0;
                    plan.counts.updated.planned = 0;
                    plan.counts.pads_reassigned.planned = 0;
                    plan.counts.conflicts.planned += 1;
                    plan.diagnostics.push(conflict(
                        "footprint_library_resolution_failed",
                        format!("{error:#}"),
                        None,
                    ));
                    plan.changes.clear();
                    return Ok(sync_response(&plan, "conflict", hierarchy.len(), false));
                }
            };
            restage_additions(&mut plan, &prepared, snapshot.state.bounds);
            refresh_revision_with_staging(&mut plan);

            if dry_run || plan.status == PlanStatus::Conflict {
                let status = match plan.status {
                    PlanStatus::Ready => "ready",
                    PlanStatus::Noop => "noop",
                    PlanStatus::Conflict => "conflict",
                };
                return Ok(sync_response(&plan, status, hierarchy.len(), false));
            }
            if expected_revision.as_deref() != Some(plan.plan_revision.as_str()) {
                plan.status = PlanStatus::Conflict;
                plan.counts.conflicts.planned += 1;
                plan.diagnostics.push(conflict(
                    "stale_plan_revision",
                    "The live board or saved schematic changed; rerun dry run and apply its new plan revision."
                        .to_string(),
                    None,
                ));
                plan.changes.clear();
                return Ok(sync_response(&plan, "conflict", hierarchy.len(), false));
            }
            if plan.status == PlanStatus::Noop {
                return Ok(sync_response(&plan, "noop", hierarchy.len(), false));
            }

            let (creates, updates) = build_mutation_items(&plan, &prepared, &snapshot)?;
            // What we are about to send, so the board can be held to it.
            let expected = footprint_shapes(creates.iter().chain(updates.iter()));
            client.run_commit("Update PCB from saved schematic", |client| {
                client.create_items_in(snapshot.document.clone(), creates)?;
                client.update_items_in(snapshot.document.clone(), updates)?;
                Ok(())
            })?;
            for detail in verify_board_matches_what_was_sent(client, &snapshot.document, &expected)?
            {
                plan.diagnostics.push(conflict(
                    "board_readback_differs",
                    format!(
                        "the board KiCad wrote differs from what was sent — {detail}. \
                         No pad was invented, so this is reported rather than \
                         refused; check the footprint before relying on it."
                    ),
                    None,
                ));
            }
            plan.counts.added.applied = plan.counts.added.planned;
            plan.counts.updated.applied = plan.counts.updated.planned;
            plan.counts.pads_reassigned.applied = plan.counts.pads_reassigned.planned;
            plan.counts.board_only_preserved.applied =
                plan.counts.board_only_preserved.planned;
            plan.counts.skipped_by_flag.applied = plan.counts.skipped_by_flag.planned;
            Ok(sync_response(&plan, "applied", hierarchy.len(), true))
        },
    )
    .await?;

    Ok(match result {
        BoardWrite::Ipc(result) => result,
        BoardWrite::File(reason) => conflict_result(format!(
            "{} update_pcb_from_schematic is live-IPC-only and never edits the board file \
             directly. Open the requested board in KiCad and retry.",
            reason.premise()
        )),
        BoardWrite::Refused(result) => {
            let message = result
                .content
                .into_iter()
                .find_map(|content| match content {
                    ToolContent::Text { text } => Some(text),
                    _ => None,
                })
                .unwrap_or_else(|| "KiCad refused the sync request".to_string());
            conflict_result(message)
        }
    })
}

fn sync_response(
    plan: &SyncPlan,
    status: &str,
    hierarchy_files: usize,
    applied: bool,
) -> CallToolResult {
    let value = serde_json::json!({
        "status": status,
        "plan_revision": plan.plan_revision,
        "coverage": {
            "source": "saved_schematic_hierarchy",
            "hierarchy_files": hierarchy_files,
            "transport": "live_kicad_ipc",
            "atomicity": "single_kicad_undo_commit",
            "footprints_added": plan.counts.added,
            "footprints_updated": plan.counts.updated,
            "pads_reassigned": plan.counts.pads_reassigned,
            "board_only_preserved": plan.counts.board_only_preserved,
            "skipped_by_flag": plan.counts.skipped_by_flag,
            "conflicts": plan.counts.conflicts
        },
        "changes": plan.changes,
        "diagnostics": plan.diagnostics,
        "undo": if applied { Some("Ctrl-Z reverses the whole schematic-to-PCB update.") } else { None }
    });
    CallToolResult::json(&value)
}

fn conflict_result(message: String) -> CallToolResult {
    let value = serde_json::json!({
        "status": "conflict",
        "coverage": {
            "transport": "live_kicad_ipc",
            "footprints_added": CountPair::default(),
            "footprints_updated": CountPair::default(),
            "pads_reassigned": CountPair::default(),
            "board_only_preserved": CountPair::default(),
            "skipped_by_flag": CountPair::default(),
            "conflicts": CountPair { planned: 1, applied: 0 }
        },
        "diagnostics": [{ "code": "preflight_conflict", "message": message }]
    });
    CallToolResult {
        content: vec![ToolContent::Text {
            text: value.to_string(),
        }],
        is_error: true,
    }
}

fn plan_sync(netlist_source: &str, design: &ExportedDesign, board: &BoardState) -> SyncPlan {
    let mut diagnostics = Vec::new();
    let mut counts = SyncCounts::default();
    let mut changes = Vec::new();
    let mut board_by_path = HashMap::new();
    let mut board_by_reference = HashMap::new();

    for (index, footprint) in board.footprints.iter().enumerate() {
        if board_by_reference
            .insert(footprint.reference.as_str(), index)
            .is_some()
        {
            diagnostics.push(conflict(
                "duplicate_board_reference",
                format!("board contains duplicate reference {}", footprint.reference),
                Some(&footprint.reference),
            ));
        }
        if let Some(path) = footprint.symbol_path.as_deref() {
            if board_by_path.insert(path, index).is_some() {
                diagnostics.push(conflict(
                    "duplicate_board_identity",
                    format!("board contains duplicate schematic identity {path}"),
                    Some(&footprint.reference),
                ));
            }
        }
    }

    let mut matched = std::collections::HashSet::new();
    let mut design_references = std::collections::HashSet::new();
    let mut design_paths = std::collections::HashSet::new();
    let staging_x = board.bounds.max_x + 10.0;
    let mut add_index = 0usize;

    let mut skipped_references = HashSet::new();
    let mut skipped_paths = HashSet::new();
    for skipped in &design.skipped {
        if !skipped_references.insert(skipped.reference.as_str())
            || !skipped_paths.insert(skipped.symbol_path.as_str())
        {
            diagnostics.push(conflict(
                "duplicate_skipped_identity",
                format!(
                    "on_board=no instance {} has a duplicate reference or identity",
                    skipped.reference
                ),
                Some(&skipped.reference),
            ));
            continue;
        }
        counts.skipped_by_flag.planned += 1;
        let existing = board_by_path
            .get(skipped.symbol_path.as_str())
            .copied()
            .or_else(|| board_by_reference.get(skipped.reference.as_str()).copied());
        if let Some(index) = existing {
            matched.insert(index);
            diagnostics.push(conflict(
                "on_board_exclusion_conflict",
                format!(
                    "{} is marked on_board=no but already exists on the board",
                    skipped.reference
                ),
                Some(&skipped.reference),
            ));
        }
    }

    for component in &design.components {
        if !design_references.insert(component.reference.as_str()) {
            diagnostics.push(conflict(
                "duplicate_schematic_reference",
                format!(
                    "schematic export contains duplicate reference {}",
                    component.reference
                ),
                Some(&component.reference),
            ));
            continue;
        }
        if !design_paths.insert(component.symbol_path.as_str()) {
            diagnostics.push(conflict(
                "duplicate_schematic_identity",
                format!(
                    "schematic export contains duplicate identity {}",
                    component.symbol_path
                ),
                Some(&component.reference),
            ));
            continue;
        }

        let matched_index = board_by_path
            .get(component.symbol_path.as_str())
            .copied()
            .or_else(|| {
                board_by_reference
                    .get(component.reference.as_str())
                    .copied()
                    .filter(|index| board.footprints[*index].symbol_path.is_none())
            });

        let Some(index) = matched_index else {
            if let Some(index) = board_by_reference
                .get(component.reference.as_str())
                .copied()
            {
                diagnostics.push(conflict(
                    "reference_identity_conflict",
                    format!(
                        "reference {} belongs to a different schematic identity on the board",
                        component.reference
                    ),
                    Some(&board.footprints[index].reference),
                ));
                continue;
            }
            let possible_renames = board
                .footprints
                .iter()
                .enumerate()
                .filter(|(index, footprint)| {
                    !matched.contains(index)
                        && footprint.symbol_path.is_none()
                        && !footprint.not_in_schematic
                        && footprint.footprint_id == component.footprint_id
                        && footprint.value == component.value
                })
                .collect::<Vec<_>>();
            if !possible_renames.is_empty() {
                diagnostics.push(conflict(
                    "reference_only_rename_ambiguous",
                    format!(
                        "{} has no stable board identity and could be a rename of {}; link or resolve the identity in KiCad",
                        component.reference,
                        possible_renames
                            .iter()
                            .map(|(_, footprint)| footprint.reference.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    Some(&component.reference),
                ));
                continue;
            }
            let position = Point {
                x: staging_x,
                y: board.bounds.min_y + add_index as f64 * 10.0,
            };
            add_index += 1;
            changes.push(PlannedChange::Add {
                reference: component.reference.clone(),
                value: component.value.clone(),
                footprint_id: component.footprint_id.clone(),
                symbol_path: component.symbol_path.clone(),
                dnp: component.dnp,
                pad_nets: component.pad_nets.clone(),
                position,
            });
            counts.added.planned += 1;
            continue;
        };

        matched.insert(index);
        let footprint = &board.footprints[index];
        if footprint.reference != component.reference {
            if let Some(other_index) = board_by_reference
                .get(component.reference.as_str())
                .copied()
                .filter(|other_index| *other_index != index)
            {
                diagnostics.push(conflict(
                    "reference_rename_collision",
                    format!(
                        "cannot rename {} to {} because that reference belongs to board footprint {}",
                        footprint.reference,
                        component.reference,
                        board.footprints[other_index].kiid
                    ),
                    Some(&component.reference),
                ));
                continue;
            }
        }
        if footprint.footprint_id != component.footprint_id {
            diagnostics.push(conflict(
                "footprint_id_changed",
                format!(
                    "{} uses {} on the board but {} in the schematic",
                    component.reference, footprint.footprint_id, component.footprint_id
                ),
                Some(&component.reference),
            ));
            continue;
        }

        let mut changed_pads = 0usize;
        let pad_numbers = component
            .pad_nets
            .keys()
            .chain(footprint.pad_nets.keys())
            .collect::<std::collections::BTreeSet<_>>();
        for number in pad_numbers {
            let new_net = component
                .pad_nets
                .get(number)
                .map(String::as_str)
                .unwrap_or("");
            let old_net = footprint
                .pad_nets
                .get(number)
                .map(String::as_str)
                .unwrap_or("");
            if old_net == new_net {
                continue;
            }
            if board.routed_nets.contains_key(old_net) || board.routed_nets.contains_key(new_net) {
                diagnostics.push(conflict(
                    "routed_pad_net_change",
                    format!(
                        "{} pad {} would change from '{}' to '{}' while routed copper uses that net",
                        component.reference, number, old_net, new_net
                    ),
                    Some(&component.reference),
                ));
            } else {
                changed_pads += 1;
            }
        }

        let needs_update = footprint.reference != component.reference
            || footprint.value != component.value
            || footprint.symbol_path.as_deref() != Some(component.symbol_path.as_str())
            || footprint.dnp != component.dnp
            || changed_pads > 0;
        if needs_update {
            changes.push(PlannedChange::Update {
                kiid: footprint.kiid.clone(),
                reference: component.reference.clone(),
                value: component.value.clone(),
                symbol_path: component.symbol_path.clone(),
                dnp: component.dnp,
                pad_nets: component.pad_nets.clone(),
                preserve: PreservedBoardState {
                    position: footprint.position,
                    rotation: footprint.rotation,
                    layer: footprint.layer.clone(),
                    locked: footprint.locked,
                },
            });
            counts.updated.planned += 1;
            counts.pads_reassigned.planned += changed_pads;
        }
    }

    counts.board_only_preserved.planned = board.footprints.len() - matched.len();
    counts.conflicts.planned = diagnostics.len();
    if !diagnostics.is_empty() {
        changes.clear();
        counts.added.planned = 0;
        counts.updated.planned = 0;
        counts.pads_reassigned.planned = 0;
    }
    let status = if !diagnostics.is_empty() {
        PlanStatus::Conflict
    } else if changes.is_empty() {
        PlanStatus::Noop
    } else {
        PlanStatus::Ready
    };
    let plan_revision = plan_revision(netlist_source, board);
    SyncPlan {
        status,
        plan_revision,
        counts,
        changes,
        diagnostics,
    }
}

fn conflict(code: &str, message: String, reference: Option<&str>) -> SyncDiagnostic {
    SyncDiagnostic {
        code: code.to_string(),
        message,
        reference: reference.map(str::to_string),
    }
}

/// A stable identity for the design-bearing netlist sections.
///
/// `kicad-cli sch export netlist` stamps `(date "…T14:48:16")` and the
/// exporting tool's version into every export, so hashing the raw source
/// yields a different revision **every second** for a design nobody touched —
/// and since apply requires the revision a dry run returned, apply could only
/// ever succeed if both calls landed inside the same wall-clock second. That
/// is a race, not a guarantee: it passes on a fast machine and fails on a
/// human reviewing the plan first, which is the whole point of the plan.
///
/// The revision must cover what the plan *read*: the complete top-level
/// `components` and `nets` trees. Hashing those trees structurally ignores the
/// volatile header without confusing nested nodes or quoted text for header
/// metadata.
fn netlist_identity(netlist_source: &str) -> Vec<u8> {
    let Ok(root) = konnect_sexp::parse_sexp(netlist_source) else {
        // Production reaches this function only after successful netlist
        // parsing. Keeping invalid synthetic planner inputs distinct makes the
        // pure planner tests useful without creating a second error path here.
        return netlist_source.as_bytes().to_vec();
    };

    let mut identity = Vec::new();
    for tag in ["components", "nets"] {
        match root.find(tag) {
            Some(node) => {
                identity.push(1);
                append_sexp_identity(node, &mut identity);
            }
            None => identity.push(0),
        }
    }
    identity
}

fn append_sexp_identity(node: &SexpNode, identity: &mut Vec<u8>) {
    match node {
        SexpNode::Atom(value) => {
            identity.push(0);
            append_identity_bytes(value.as_bytes(), identity);
        }
        SexpNode::Str(value) => {
            identity.push(1);
            append_identity_bytes(value.as_bytes(), identity);
        }
        SexpNode::List(children) => {
            identity.push(2);
            identity.extend_from_slice(&(children.len() as u64).to_le_bytes());
            for child in children {
                append_sexp_identity(child, identity);
            }
        }
    }
}

fn append_identity_bytes(value: &[u8], identity: &mut Vec<u8>) {
    identity.extend_from_slice(&(value.len() as u64).to_le_bytes());
    identity.extend_from_slice(value);
}

fn plan_revision(netlist_source: &str, board: &BoardState) -> String {
    let mut footprints = board.footprints.iter().collect::<Vec<_>>();
    footprints.sort_by(|a, b| a.kiid.cmp(&b.kiid));
    let mut hasher = Sha256::new();
    hasher.update(netlist_identity(netlist_source));
    hasher.update(serde_json::to_vec(&board.bounds).expect("bounds serialize"));
    for footprint in footprints {
        hasher.update(footprint.kiid.as_bytes());
        hasher.update(footprint.reference.as_bytes());
        hasher.update(footprint.footprint_id.as_bytes());
        hasher.update(footprint.symbol_path.as_deref().unwrap_or("").as_bytes());
        for (pad, net) in &footprint.pad_nets {
            hasher.update(pad.as_bytes());
            hasher.update(net.as_bytes());
        }
    }
    for (net, count) in &board.routed_nets {
        hasher.update(net.as_bytes());
        hasher.update(count.to_le_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn refresh_revision_with_staging(plan: &mut SyncPlan) {
    let mut hasher = Sha256::new();
    hasher.update(plan.plan_revision.as_bytes());
    hasher.update(serde_json::to_vec(&plan.changes).expect("planned changes serialize"));
    plan.plan_revision = format!("{:x}", hasher.finalize());
}

fn parse_exported_netlist(source: &str) -> Result<ExportedDesign> {
    let root = konnect_sexp::parse_sexp(source).context("invalid KiCad netlist S-expression")?;
    let components_node = root
        .find("components")
        .context("KiCad netlist has no components section")?;

    let mut components = Vec::new();
    let mut by_reference = HashMap::new();
    for component_node in components_node.find_all("comp") {
        let reference = required_value(component_node, "ref")?;
        if by_reference.contains_key(&reference) {
            bail!("KiCad netlist contains duplicate component reference {reference}");
        }

        let sheet_stamp = component_node
            .find("sheetpath")
            .and_then(|sheet| sheet.find_str("tstamps"))
            .context("KiCad netlist component has no sheet timestamp")?;
        let symbol_stamp = component_node
            .find_str("tstamps")
            .context("KiCad netlist component has no symbol timestamp")?;
        let symbol_path = format!(
            "/{}/{}",
            sheet_stamp.trim_matches('/'),
            symbol_stamp.trim_matches('/')
        )
        .replace("//", "/");
        let dnp = component_node.find_all("property").iter().any(|property| {
            property.find_str("name") == Some("dnp")
                || property.get(1).and_then(SexpNode::as_str) == Some("dnp")
        });

        let index = components.len();
        by_reference.insert(reference.clone(), index);
        components.push(DesignComponent {
            reference,
            value: required_value(component_node, "value")?,
            footprint_id: required_value(component_node, "footprint")?,
            symbol_path,
            dnp,
            pad_nets: BTreeMap::new(),
        });
    }

    if components.is_empty() {
        bail!("KiCad netlist contains zero components");
    }

    if let Some(nets_node) = root.find("nets") {
        for net_node in nets_node.find_all("net") {
            let net_name = required_value(net_node, "name")?;
            for node in net_node.find_all("node") {
                let reference = required_value(node, "ref")?;
                let pin = required_value(node, "pin")?;
                let Some(&index) = by_reference.get(&reference) else {
                    bail!("net {net_name} refers to unknown component {reference}");
                };
                if components[index]
                    .pad_nets
                    .insert(pin.clone(), net_name.clone())
                    .is_some()
                {
                    bail!("component {reference} pad {pin} appears in more than one net");
                }
            }
        }
    }

    Ok(ExportedDesign {
        components,
        skipped: Vec::new(),
    })
}

fn required_value(node: &SexpNode, tag: &str) -> Result<String> {
    node.find_str(tag)
        .map(str::to_owned)
        .with_context(|| format!("KiCad netlist node is missing {tag}"))
}

fn update_footprint_item(
    item: &prost_types::Any,
    change: &PlannedChange,
    net_codes: &BTreeMap<String, i32>,
) -> Result<prost_types::Any> {
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    let PlannedChange::Update {
        kiid,
        reference,
        value,
        symbol_path,
        dnp,
        pad_nets,
        ..
    } = change
    else {
        bail!("an add change cannot update an existing footprint");
    };
    let mut footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
        .context("KiCad returned an invalid footprint item")?;
    if footprint.id.as_ref().map(|id| id.value.as_str()) != Some(kiid.as_str()) {
        bail!("planned footprint {kiid} no longer matches the live board item");
    }

    apply_footprint_fields(
        &mut footprint,
        reference,
        value,
        symbol_path,
        *dnp,
        pad_nets,
        net_codes,
    )?;

    Ok(konnect_ipc::builders::pack_any(
        &footprint,
        "kiapi.board.types.FootprintInstance",
    ))
}

#[allow(clippy::too_many_arguments)]
fn apply_footprint_fields(
    footprint: &mut konnect_ipc::gen::kiapi::board::types::FootprintInstance,
    reference: &str,
    value: &str,
    symbol_path: &str,
    dnp: bool,
    pad_nets: &BTreeMap<String, String>,
    net_codes: &BTreeMap<String, i32>,
) -> Result<()> {
    use konnect_ipc::gen::kiapi;

    set_field_text(&mut footprint.reference_field, "Reference", reference);
    set_field_text(&mut footprint.value_field, "Value", value);
    let definition = footprint
        .definition
        .as_mut()
        .context("board footprint has no library definition")?;
    set_field_text(&mut definition.reference_field, "Reference", reference);
    set_field_text(&mut definition.value_field, "Value", value);

    footprint.symbol_path = Some(kiapi::common::types::SheetPath {
        path: symbol_path
            .split('/')
            .filter(|part| !part.is_empty())
            .map(|part| kiapi::common::types::Kiid {
                value: part.to_string(),
            })
            .collect(),
        path_human_readable: String::new(),
    });
    footprint
        .attributes
        .get_or_insert_with(Default::default)
        .do_not_populate = dnp;
    definition
        .attributes
        .get_or_insert_with(Default::default)
        .do_not_populate = dnp;

    let mut seen_pads = std::collections::HashSet::new();
    for child in &mut definition.items {
        // `definition.items` mixes pads, graphics and text in one repeated
        // field, so the type URL is the only sound discriminator. Filtering by
        // "did `Pad::decode` succeed" instead accepted every graphic — proto3
        // skips unrecognised field numbers rather than failing — and the write
        // back below then re-typed each one as a pad, so every footprint this
        // tool touched lost its artwork and gained a nameless pad at (0,0)
        // for each shape it used to have (#244).
        if !konnect_ipc::builders::any_is(child, "kiapi.board.types.Pad") {
            continue;
        }
        // A child that *declares* itself a pad and will not decode is a real
        // failure, not something to skip past silently.
        let mut pad =
            kiapi::board::types::Pad::decode(child.value.as_slice()).with_context(|| {
                format!("footprint {reference} has a pad KiCad sent in a form Konnect cannot read")
            })?;
        seen_pads.insert(pad.number.clone());
        pad.net = pad_nets
            .get(&pad.number)
            .map(|name| kiapi::board::types::Net {
                // Net codes are KiCad-internal. Preserve a resolved live code
                // when one exists; for a schematic-only net, the name is the
                // public identity and lets KiCad create the new board net.
                code: net_codes
                    .get(name)
                    .copied()
                    .map(|value| kiapi::board::types::NetCode { value }),
                name: name.clone(),
            });
        *child = konnect_ipc::builders::pack_any(&pad, "kiapi.board.types.Pad");
    }
    for number in pad_nets.keys() {
        if !seen_pads.contains(number) {
            bail!("footprint {reference} has no pad {number}");
        }
    }

    Ok(())
}

/// How many pads and how many drawn items a footprint carries.
///
/// The two numbers #244 got wrong in opposite directions: every graphic became
/// a pad, so pads went up by exactly the number of drawings, and drawings went
/// to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct FootprintShape {
    pads: usize,
    drawings: usize,
}

/// Tally the pads and drawings of each footprint in a set of packed items,
/// keyed by reference.
fn footprint_shapes<'a>(
    items: impl Iterator<Item = &'a prost_types::Any>,
) -> BTreeMap<String, FootprintShape> {
    use konnect_ipc::gen::kiapi;
    use prost::Message;

    let mut out = BTreeMap::new();
    for item in items {
        let Ok(footprint) = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
        else {
            continue;
        };
        let Some(definition) = footprint.definition.as_ref() else {
            continue;
        };
        let reference = field_text(&footprint.reference_field);
        if reference.is_empty() {
            continue;
        }
        let mut shape = FootprintShape::default();
        for child in &definition.items {
            match konnect_ipc::builders::any_type_name(child) {
                "kiapi.board.types.Pad" => shape.pads += 1,
                "kiapi.board.types.BoardGraphicShape" | "kiapi.board.types.BoardText" => {
                    shape.drawings += 1
                }
                _ => {}
            }
        }
        out.insert(reference, shape);
    }
    out
}

/// Read the board back and hold it to what was just sent.
///
/// `create_items`/`update_items` only confirm that KiCad *accepted* each item,
/// and the counts this tool reports are copied from the plan — so when #244
/// turned every footprint graphic into a nameless pad, KiCad returned ISC_OK
/// for each one and the response said the sync succeeded. Nothing anywhere
/// looked at what actually landed.
///
/// This is a backstop for that class, not for that bug: with the type-URL fix
/// in place it should never fire. `delete_footprint` already re-queries after
/// mutating; this follows it.
///
/// **It fails the call only on a gained pad.** KiCad has no business inventing
/// one, so that is unambiguous and is #244's exact signature. A *drop* in
/// drawings is reported instead of refused, because it has a benign
/// explanation this check cannot yet rule out — KiCad re-creates a footprint's
/// children from the message on deserialize, and if it promotes a `BoardText`
/// child into a `Field` (which this tally deliberately ignores) the count
/// would fall without anything being wrong. Turning a working sync into an
/// error over that is worse than the warning. Tighten it once it has been
/// watched against a live KiCad; see the note on #244.
fn verify_board_matches_what_was_sent(
    client: &konnect_ipc::KiCadIpcClient,
    document: &konnect_ipc::gen::kiapi::common::types::DocumentSpecifier,
    expected: &BTreeMap<String, FootprintShape>,
) -> Result<Vec<String>> {
    use konnect_ipc::gen::kiapi;

    if expected.is_empty() {
        return Ok(Vec::new());
    }
    let items = client.get_items_in(
        document.clone(),
        kiapi::common::types::KiCadObjectType::KotPcbFootprint,
    )?;
    let actual = footprint_shapes(items.iter());

    let mut corrupted = Vec::new();
    let mut suspicious = Vec::new();
    for (reference, want) in expected {
        // A reference the read-back cannot see is its own problem, but not this
        // check's: KiCad may name it differently after a rename, and failing
        // here would turn a successful sync into an error over bookkeeping.
        let Some(got) = actual.get(reference) else {
            continue;
        };
        let detail = format!(
            "{reference}: sent {} pads and {} drawings, board now has {} and {}",
            want.pads, want.drawings, got.pads, got.drawings
        );
        if got.pads > want.pads {
            corrupted.push(detail);
        } else if got != want {
            suspicious.push(detail);
        }
    }
    if !corrupted.is_empty() {
        bail!(
            "KiCad's board gained pads this sync never sent, so the footprints on \
             it are not the ones that were planned — inspect the board and do not \
             save it: {}",
            corrupted.join("; ")
        );
    }
    Ok(suspicious)
}

fn set_field_text(
    field: &mut Option<konnect_ipc::gen::kiapi::board::types::Field>,
    name: &str,
    value: &str,
) {
    let field = field.get_or_insert_with(Default::default);
    field.name = name.to_string();
    let board_text = field.text.get_or_insert_with(Default::default);
    board_text.text.get_or_insert_with(Default::default).text = value.to_string();
}

fn saved_hierarchy_files(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(
        path: &Path,
        seen: &mut HashSet<PathBuf>,
        active: &mut HashSet<PathBuf>,
        files: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("cannot resolve schematic {}", path.display()))?;
        if active.contains(&canonical) {
            bail!(
                "schematic hierarchy contains a cycle at {}",
                canonical.display()
            );
        }
        if !seen.insert(canonical.clone()) {
            return Ok(());
        }
        active.insert(canonical.clone());
        let name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .context("schematic path has no file name")?;
        let lock = canonical.with_file_name(format!("~{name}.lck"));
        if lock.exists() {
            bail!(
                "{} is open in the schematic editor; save and close the hierarchy before syncing",
                canonical.display()
            );
        }
        let schematic = konnect_schematic_editor::Schematic::load(&canonical)
            .with_context(|| format!("cannot load schematic {}", canonical.display()))?;
        files.push(canonical.clone());
        let parent = canonical.parent().unwrap_or_else(|| Path::new("."));
        for sheet in schematic.sheets.iter() {
            let child = parent.join(sheet.file());
            if !child.exists() {
                bail!(
                    "hierarchical sheet {} referenced by {} does not exist",
                    child.display(),
                    canonical.display()
                );
            }
            visit(&child, seen, active, files)?;
        }
        active.remove(&canonical);
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, &mut HashSet::new(), &mut HashSet::new(), &mut files)?;
    Ok(files)
}

fn apply_saved_symbol_flags(files: &[PathBuf], design: &mut ExportedDesign) -> Result<()> {
    #[derive(Debug)]
    struct Flags {
        reference: String,
        symbol_path: String,
        in_bom: bool,
        on_board: bool,
        dnp: bool,
    }

    let mut flags = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(file)?;
        let tree = konnect_sexp::parse_sexp(&source)?;
        let root_uuid = tree.find_str("uuid").unwrap_or("");
        for symbol in tree.find_all("symbol") {
            let Some(uuid) = symbol.find_str("uuid") else {
                continue;
            };
            let in_bom = symbol.find_str("in_bom") != Some("no");
            let on_board = symbol.find_str("on_board") != Some("no");
            let dnp = symbol.find_str("dnp") == Some("yes");
            let projects = symbol
                .find("instances")
                .map(|instances| instances.find_all("project"))
                .unwrap_or_default();
            for project in projects {
                for path in project.find_all("path") {
                    let Some(reference) = path.find_str("reference") else {
                        continue;
                    };
                    let instance = path.get(1).and_then(SexpNode::as_str).unwrap_or("/");
                    let base = if instance == "/" && !root_uuid.is_empty() {
                        format!("/{root_uuid}")
                    } else {
                        instance.trim_end_matches('/').to_string()
                    };
                    flags.push(Flags {
                        reference: reference.to_string(),
                        symbol_path: format!("{base}/{uuid}").replace("//", "/"),
                        in_bom,
                        on_board,
                        dnp,
                    });
                }
            }
        }
    }

    for reference in flags
        .iter()
        .map(|entry| entry.reference.as_str())
        .collect::<HashSet<_>>()
    {
        let entries = flags
            .iter()
            .filter(|entry| entry.reference == reference)
            .collect::<Vec<_>>();
        if entries.iter().any(|entry| {
            entry.in_bom != entries[0].in_bom
                || entry.on_board != entries[0].on_board
                || entry.dnp != entries[0].dnp
        }) {
            bail!("multi-unit reference {reference} has inconsistent board/BOM/DNP flags");
        }
    }

    design.components.retain_mut(|component| {
        let path_match = flags
            .iter()
            .find(|entry| entry.symbol_path == component.symbol_path);
        let reference_matches = flags
            .iter()
            .filter(|entry| entry.reference == component.reference)
            .collect::<Vec<_>>();
        let entry = path_match.or_else(|| reference_matches.first().copied());
        let Some(entry) = entry else {
            return true;
        };
        if !entry.in_bom {
            return false;
        }
        if !entry.on_board {
            design.skipped.push(SkippedComponent {
                reference: entry.reference.clone(),
                symbol_path: entry.symbol_path.clone(),
            });
            return false;
        }
        component.dnp = entry.dnp;
        true
    });
    let mut skipped_references = HashSet::new();
    for entry in flags.iter().filter(|entry| entry.in_bom && !entry.on_board) {
        if !skipped_references.insert(entry.reference.as_str()) {
            continue;
        }
        if !design
            .skipped
            .iter()
            .any(|skipped| skipped.symbol_path == entry.symbol_path)
        {
            design.skipped.push(SkippedComponent {
                reference: entry.reference.clone(),
                symbol_path: entry.symbol_path.clone(),
            });
        }
    }
    Ok(())
}

fn snapshot_board(client: &konnect_ipc::KiCadIpcClient, board: &Path) -> Result<LiveSnapshot> {
    use kiapi::common::types::KiCadObjectType as ObjectType;
    use konnect_ipc::gen::kiapi;

    let document = client.find_open_board(board)?;
    let footprint_items = client.get_items_in(document.clone(), ObjectType::KotPcbFootprint)?;
    let mut footprints = Vec::new();
    let mut items = BTreeMap::new();
    for item in footprint_items {
        let footprint = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            .context("KiCad returned an invalid footprint item")?;
        let kiid = footprint
            .id
            .as_ref()
            .map(|id| id.value.clone())
            .filter(|id| !id.is_empty())
            .context("KiCad returned a footprint without a KIID")?;
        let definition = footprint
            .definition
            .as_ref()
            .context("KiCad returned a footprint without a definition")?;
        let mut pad_nets = BTreeMap::new();
        for child in &definition.items {
            // Same discriminator as `apply_footprint_fields`, for the same
            // reason: a graphic decodes happily as an empty pad.
            //
            // No test covers this one, and deliberately so — it has no
            // observable effect today. A graphic decoded as a pad has
            // `net: None`, so the filter below drops it anyway, and this
            // function never writes. It is here because the next person to add
            // a field to this loop should not have to rediscover why reading
            // `definition.items` untyped is unsafe. Neutering it changes
            // nothing, which is the honest result.
            if !konnect_ipc::builders::any_is(child, "kiapi.board.types.Pad") {
                continue;
            }
            let Ok(pad) = kiapi::board::types::Pad::decode(child.value.as_slice()) else {
                continue;
            };
            if let Some(net) = pad.net.filter(|net| !net.name.is_empty()) {
                pad_nets.insert(pad.number, net.name);
            }
        }
        let position = footprint.position.as_ref();
        footprints.push(BoardFootprint {
            kiid: kiid.clone(),
            reference: field_text(&footprint.reference_field),
            value: field_text(&footprint.value_field),
            footprint_id: definition
                .id
                .as_ref()
                .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
                .unwrap_or_default(),
            symbol_path: footprint.symbol_path.as_ref().map(sheet_path_string),
            pad_nets,
            position: Point {
                x: position
                    .map(|point| konnect_ipc::builders::nm_to_mm(point.x_nm))
                    .unwrap_or(0.0),
                y: position
                    .map(|point| konnect_ipc::builders::nm_to_mm(point.y_nm))
                    .unwrap_or(0.0),
            },
            rotation: footprint
                .orientation
                .as_ref()
                .map(|angle| angle.value_degrees)
                .unwrap_or(0.0),
            layer: board_layer_name(footprint.layer),
            locked: footprint.locked == kiapi::common::types::LockedState::LsLocked as i32,
            dnp: footprint
                .attributes
                .as_ref()
                .map(|attributes| attributes.do_not_populate)
                .unwrap_or(false),
            not_in_schematic: footprint
                .attributes
                .as_ref()
                .map(|attributes| attributes.not_in_schematic)
                .unwrap_or(false),
        });
        items.insert(kiid, item);
    }

    let nets = client.get_nets_in(document.clone())?;
    let net_codes = nets
        .iter()
        .map(|net| (net.name.clone(), net.netcode))
        .collect::<BTreeMap<_, _>>();
    let mut routed_nets = BTreeMap::new();
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbTrace)? {
        if let Ok(track) = kiapi::board::types::Track::decode(item.value.as_slice()) {
            record_routed_net(&mut routed_nets, track.net.as_ref());
        }
    }
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbArc)? {
        if let Ok(arc) = kiapi::board::types::Arc::decode(item.value.as_slice()) {
            record_routed_net(&mut routed_nets, arc.net.as_ref());
        }
    }
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbVia)? {
        if let Ok(via) = kiapi::board::types::Via::decode(item.value.as_slice()) {
            record_routed_net(&mut routed_nets, via.net.as_ref());
        }
    }
    if !client
        .get_items_in(document.clone(), ObjectType::KotPcbZone)?
        .is_empty()
    {
        // KiCad 10's Zone protobuf does not expose the zone net. A pad-net
        // reassignment on a zoned board therefore fails closed.
        for net in net_codes.keys() {
            *routed_nets.entry(net.clone()).or_insert(0) += 1;
        }
    }
    let extents = client
        .get_optional_board_extents_in(document.clone())?
        .unwrap_or(konnect_ipc::IpcBoardExtents {
            min: konnect_ipc::IpcVector2 { x: 0.0, y: 0.0 },
            max: konnect_ipc::IpcVector2 { x: 0.0, y: 0.0 },
        });
    Ok(LiveSnapshot {
        state: BoardState {
            footprints,
            routed_nets,
            bounds: Bounds {
                min_x: extents.min.x,
                min_y: extents.min.y,
                max_x: extents.max.x,
                max_y: extents.max.y,
            },
        },
        items,
        net_codes,
        document,
    })
}

fn field_text(field: &Option<konnect_ipc::gen::kiapi::board::types::Field>) -> String {
    field
        .as_ref()
        .and_then(|field| field.text.as_ref())
        .and_then(|text| text.text.as_ref())
        .map(|text| text.text.clone())
        .unwrap_or_default()
}

fn sheet_path_string(path: &konnect_ipc::gen::kiapi::common::types::SheetPath) -> String {
    format!(
        "/{}",
        path.path
            .iter()
            .map(|part| part.value.as_str())
            .collect::<Vec<_>>()
            .join("/")
    )
}

fn board_layer_name(layer: i32) -> String {
    use konnect_ipc::gen::kiapi::board::types::BoardLayer;
    match BoardLayer::try_from(layer).ok() {
        Some(BoardLayer::BlFCu) => "F.Cu".to_string(),
        Some(BoardLayer::BlBCu) => "B.Cu".to_string(),
        Some(layer) => layer.as_str_name().to_string(),
        None => format!("layer_{layer}"),
    }
}

fn record_routed_net(
    routed: &mut BTreeMap<String, usize>,
    net: Option<&konnect_ipc::gen::kiapi::board::types::Net>,
) {
    if let Some(net) = net.filter(|net| !net.name.is_empty()) {
        *routed.entry(net.name.clone()).or_insert(0) += 1;
    }
}

fn prepare_additions(board: &Path, plan: &SyncPlan) -> Result<BTreeMap<String, PreparedFootprint>> {
    let mut prepared = BTreeMap::new();
    for change in &plan.changes {
        let PlannedChange::Add { footprint_id, .. } = change else {
            continue;
        };
        if prepared.contains_key(footprint_id) {
            continue;
        }
        let source = super::pcb_components::resolve_footprint_source(footprint_id, board)?;
        let pads = super::pcb_components::extract_pad_definitions(&source)?;
        let graphics = super::pcb_components::extract_graphic_definitions(&source)?;
        let fields = super::pcb_components::extract_field_placement(&source);
        let (width, height) = footprint_dimensions(&pads, &graphics);
        prepared.insert(
            footprint_id.clone(),
            PreparedFootprint {
                pads,
                graphics,
                fields,
                width,
                height,
            },
        );
    }
    Ok(prepared)
}

fn footprint_dimensions(
    pads: &[konnect_ipc::IpcPadDefinition],
    graphics: &[konnect_ipc::IpcGraphicDefinition],
) -> (f64, f64) {
    use konnect_ipc::IpcGraphicDefinition as Graphic;

    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut include = |x: f64, y: f64| {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    };
    for pad in pads {
        include(pad.x - pad.size_x / 2.0, pad.y - pad.size_y / 2.0);
        include(pad.x + pad.size_x / 2.0, pad.y + pad.size_y / 2.0);
    }
    for graphic in graphics {
        match graphic {
            Graphic::Line { start, end, .. } | Graphic::Rect { start, end, .. } => {
                include(start.0, start.1);
                include(end.0, end.1);
            }
            Graphic::Circle { center, end, .. } => {
                let radius = ((end.0 - center.0).powi(2) + (end.1 - center.1).powi(2)).sqrt();
                include(center.0 - radius, center.1 - radius);
                include(center.0 + radius, center.1 + radius);
            }
            Graphic::Arc {
                start, mid, end, ..
            } => {
                include(start.0, start.1);
                include(mid.0, mid.1);
                include(end.0, end.1);
            }
            Graphic::Poly { points, .. } => {
                for point in points {
                    include(point.0, point.1);
                }
            }
            Graphic::Text { position, size, .. } => {
                include(position.0 - size / 2.0, position.1 - size / 2.0);
                include(position.0 + size / 2.0, position.1 + size / 2.0);
            }
        }
    }
    if !min_x.is_finite() {
        return (10.0, 10.0);
    }
    ((max_x - min_x).max(1.0), (max_y - min_y).max(1.0))
}

fn restage_additions(
    plan: &mut SyncPlan,
    prepared: &BTreeMap<String, PreparedFootprint>,
    bounds: Bounds,
) {
    let mut next_y = bounds.min_y;
    for change in &mut plan.changes {
        let PlannedChange::Add {
            footprint_id,
            position,
            ..
        } = change
        else {
            continue;
        };
        let dimensions = prepared.get(footprint_id);
        let width = dimensions.map(|part| part.width).unwrap_or(10.0);
        let height = dimensions.map(|part| part.height).unwrap_or(10.0);
        *position = Point {
            x: bounds.max_x + 5.0 + width / 2.0,
            y: next_y + height / 2.0,
        };
        next_y += height + 5.0;
    }
}

fn build_mutation_items(
    plan: &SyncPlan,
    prepared: &BTreeMap<String, PreparedFootprint>,
    snapshot: &LiveSnapshot,
) -> Result<(Vec<prost_types::Any>, Vec<prost_types::Any>)> {
    use konnect_ipc::gen::kiapi;

    let mut creates = Vec::new();
    let mut updates = Vec::new();
    for change in &plan.changes {
        match change {
            PlannedChange::Add {
                reference,
                value,
                footprint_id,
                symbol_path,
                dnp,
                pad_nets,
                position,
            } => {
                let part = prepared
                    .get(footprint_id)
                    .with_context(|| format!("no prepared footprint for {footprint_id}"))?;
                let item = konnect_ipc::KiCadIpcClient::build_footprint_item(
                    footprint_id,
                    reference,
                    value,
                    &part.pads,
                    &part.graphics,
                    &part.fields,
                    position.x,
                    position.y,
                    0.0,
                    "F.Cu",
                )?;
                let mut footprint =
                    kiapi::board::types::FootprintInstance::decode(item.value.as_slice())?;
                apply_footprint_fields(
                    &mut footprint,
                    reference,
                    value,
                    symbol_path,
                    *dnp,
                    pad_nets,
                    &snapshot.net_codes,
                )?;
                creates.push(konnect_ipc::builders::pack_any(
                    &footprint,
                    "kiapi.board.types.FootprintInstance",
                ));
            }
            PlannedChange::Update { kiid, .. } => {
                let item = snapshot
                    .items
                    .get(kiid)
                    .with_context(|| format!("planned footprint {kiid} disappeared"))?;
                updates.push(update_footprint_item(item, change, &snapshot.net_codes)?);
            }
        }
    }
    Ok((creates, updates))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_RESISTOR: &str = r#"
(export
  (components
    (comp
      (ref "R1")
      (value "10k")
      (footprint "Resistor_SMD:R_0603_1608Metric")
      (sheetpath (names "/Power/") (tstamps "/sheet-uuid/"))
      (tstamps "symbol-uuid")
      (units (unit (name "A") (pins (pin (num "1")) (pin (num "2")))))))
  (nets
    (net (code "1") (name "/Power/VCC") (class "Default")
      (node (ref "R1") (pin "1") (pintype "passive")))
    (net (code "2") (name "GND") (class "Default")
      (node (ref "R1") (pin "2") (pintype "passive")))))
"#;

    #[test]
    fn exported_netlist_is_one_flattened_source_of_component_and_pad_truth() {
        let design = parse_exported_netlist(ONE_RESISTOR).expect("valid KiCad netlist");

        assert_eq!(design.components.len(), 1);
        let component = &design.components[0];
        assert_eq!(component.reference, "R1");
        assert_eq!(component.value, "10k");
        assert_eq!(component.footprint_id, "Resistor_SMD:R_0603_1608Metric");
        assert_eq!(component.symbol_path, "/sheet-uuid/symbol-uuid");
        assert_eq!(
            component.pad_nets.get("1").map(String::as_str),
            Some("/Power/VCC")
        );
        assert_eq!(component.pad_nets.get("2").map(String::as_str), Some("GND"));
        assert!(!component.dnp);
    }

    fn resistor(reference: &str, symbol_path: &str) -> DesignComponent {
        DesignComponent {
            reference: reference.to_string(),
            value: "10k".to_string(),
            footprint_id: "Resistor_SMD:R_0603_1608Metric".to_string(),
            symbol_path: symbol_path.to_string(),
            dnp: false,
            pad_nets: BTreeMap::from([
                ("1".to_string(), "VCC".to_string()),
                ("2".to_string(), "GND".to_string()),
            ]),
        }
    }

    fn board_resistor(reference: &str, symbol_path: Option<&str>) -> BoardFootprint {
        BoardFootprint {
            kiid: format!("{reference}-kiid"),
            reference: reference.to_string(),
            value: "10k".to_string(),
            footprint_id: "Resistor_SMD:R_0603_1608Metric".to_string(),
            symbol_path: symbol_path.map(str::to_string),
            pad_nets: BTreeMap::from([
                ("1".to_string(), "VCC".to_string()),
                ("2".to_string(), "GND".to_string()),
            ]),
            position: Point { x: 1.0, y: 2.0 },
            rotation: 0.0,
            layer: "F.Cu".to_string(),
            locked: false,
            dnp: false,
            not_in_schematic: false,
        }
    }

    fn board_with(footprints: Vec<BoardFootprint>) -> BoardState {
        BoardState {
            footprints,
            routed_nets: BTreeMap::new(),
            bounds: Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        }
    }

    #[test]
    fn planner_matches_identity_preserves_pose_and_stages_new_parts_deterministically() {
        let design = ExportedDesign {
            components: vec![
                resistor("R2", "/sheet/existing"),
                resistor("R3", "/sheet/new"),
            ],
            skipped: Vec::new(),
        };
        let board = BoardState {
            footprints: vec![
                BoardFootprint {
                    kiid: "existing-kiid".to_string(),
                    reference: "R1".to_string(),
                    value: "1k".to_string(),
                    footprint_id: "Resistor_SMD:R_0603_1608Metric".to_string(),
                    symbol_path: Some("/sheet/existing".to_string()),
                    pad_nets: BTreeMap::from([
                        ("1".to_string(), "VCC".to_string()),
                        ("2".to_string(), "GND".to_string()),
                    ]),
                    position: Point { x: 25.0, y: 30.0 },
                    rotation: 90.0,
                    layer: "B.Cu".to_string(),
                    locked: true,
                    dnp: false,
                    not_in_schematic: false,
                },
                BoardFootprint {
                    kiid: "board-only".to_string(),
                    reference: "MH1".to_string(),
                    value: "MountingHole".to_string(),
                    footprint_id: "MountingHole:MountingHole_3.2mm_M3".to_string(),
                    symbol_path: None,
                    pad_nets: BTreeMap::new(),
                    position: Point { x: 2.0, y: 2.0 },
                    rotation: 0.0,
                    layer: "F.Cu".to_string(),
                    locked: true,
                    dnp: false,
                    not_in_schematic: true,
                },
            ],
            routed_nets: BTreeMap::new(),
            bounds: Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 50.0,
                max_y: 40.0,
            },
        };

        let first = plan_sync("netlist bytes", &design, &board);
        let second = plan_sync("netlist bytes", &design, &board);

        assert_eq!(first.status, PlanStatus::Ready);
        assert_eq!(first.plan_revision, second.plan_revision);
        assert_eq!(first.counts.added.planned, 1);
        assert_eq!(first.counts.updated.planned, 1);
        assert_eq!(first.counts.board_only_preserved.planned, 1);
        assert_eq!(first.changes, second.changes);
        assert!(first.changes.iter().any(|change| matches!(
            change,
            PlannedChange::Update { kiid, reference, preserve, .. }
                if kiid == "existing-kiid"
                    && reference == "R2"
                    && preserve.position == Point { x: 25.0, y: 30.0 }
                    && preserve.rotation == 90.0
                    && preserve.layer == "B.Cu"
                    && preserve.locked
        )));
        assert!(first.changes.iter().any(|change| matches!(
            change,
            PlannedChange::Add { reference, position, .. }
                if reference == "R3" && position.x > board.bounds.max_x
        )));
    }

    /// Build a footprint carrying one pad and one child of every graphic kind,
    /// the way a real library footprint arrives from KiCad. The existing sync
    /// test passes `&[]` for graphics, which is precisely why #244 survived it.
    fn footprint_with_artwork(reference: &str) -> prost_types::Any {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let silk = || "F.SilkS".to_string();
        let item = konnect_ipc::KiCadIpcClient::build_footprint_item(
            "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm",
            reference,
            "NE555",
            &[konnect_ipc::IpcPadDefinition {
                number: "1".to_string(),
                pad_type: "smd".to_string(),
                shape: "rect".to_string(),
                x: 0.0,
                y: 0.0,
                rotation: 0.0,
                size_x: 1.0,
                size_y: 1.0,
                drill_x: None,
                drill_y: None,
                drill_oval: false,
                layers: vec!["F.Cu".to_string()],
                roundrect_ratio: 0.0,
            }],
            &[
                konnect_ipc::IpcGraphicDefinition::Line {
                    start: (-2.0, -2.5),
                    end: (2.0, -2.5),
                    layer: silk(),
                    width: 0.12,
                },
                konnect_ipc::IpcGraphicDefinition::Rect {
                    start: (-2.6, -3.0),
                    end: (2.6, 3.0),
                    layer: "F.CrtYd".to_string(),
                    width: 0.05,
                    filled: false,
                },
                konnect_ipc::IpcGraphicDefinition::Circle {
                    center: (-1.8, -1.8),
                    end: (-1.6, -1.8),
                    layer: silk(),
                    width: 0.12,
                    filled: true,
                },
                konnect_ipc::IpcGraphicDefinition::Arc {
                    start: (-1.0, -2.5),
                    mid: (0.0, -2.0),
                    end: (1.0, -2.5),
                    layer: "F.Fab".to_string(),
                    width: 0.1,
                },
                konnect_ipc::IpcGraphicDefinition::Poly {
                    points: vec![(-1.0, 2.0), (1.0, 2.0), (0.0, 2.8)],
                    layer: "F.Fab".to_string(),
                    width: 0.1,
                    filled: true,
                },
                konnect_ipc::IpcGraphicDefinition::Text {
                    text: "U1".to_string(),
                    position: (0.0, -3.5),
                    rotation: 0.0,
                    layer: silk(),
                    size: 1.0,
                    stroke_width_mm: 0.15,
                },
            ],
            &konnect_ipc::IpcFieldPlacement::default(),
            25.0,
            30.0,
            0.0,
            "F.Cu",
        )
        .unwrap();
        // Give it the KIID the update path matches against.
        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        footprint.id = Some(kiapi::common::types::Kiid {
            value: format!("{}-kiid", reference.to_lowercase()),
        });
        konnect_ipc::builders::pack_any(&footprint, "kiapi.board.types.FootprintInstance")
    }

    /// Tally a footprint definition's children by the protobuf type they
    /// declare — the property #244 destroyed.
    fn child_types(item: &prost_types::Any) -> BTreeMap<String, usize> {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        let mut counts = BTreeMap::new();
        for child in &footprint.definition.as_ref().unwrap().items {
            *counts
                .entry(konnect_ipc::builders::any_type_name(child).to_string())
                .or_insert(0) += 1;
        }
        counts
    }

    /// #244. A footprint's pads, graphics and text all live in one repeated
    /// `Any` field, and proto3 skips field numbers it does not recognise rather
    /// than failing — so a `BoardGraphicShape` decodes cleanly as a near-empty
    /// `Pad`. Filtering that list with `Pad::decode(..).ok()` therefore matched
    /// every graphic, and packing the decoded value back re-typed it. In
    /// neusse's benchmark an 8-pad SOIC-8 came out of a sync with 28 pads —
    /// the 20 extras nameless, at (0,0), one per lost graphic — and no artwork.
    #[test]
    fn syncing_a_footprint_leaves_its_graphics_as_graphics() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let item = footprint_with_artwork("U1");
        let before = child_types(&item);

        // Sanity: the fixture must actually carry the mixture, or this test
        // proves nothing — which is the trap the pre-existing sync test fell
        // into by passing `&[]` graphics.
        assert_eq!(before.get("kiapi.board.types.Pad"), Some(&1));
        assert_eq!(before.get("kiapi.board.types.BoardGraphicShape"), Some(&5));
        assert_eq!(before.get("kiapi.board.types.BoardText"), Some(&1));

        let change = PlannedChange::Update {
            kiid: "u1-kiid".to_string(),
            reference: "U1".to_string(),
            value: "NE555".to_string(),
            symbol_path: "/root/u1".to_string(),
            dnp: false,
            pad_nets: BTreeMap::from([("1".to_string(), "GND".to_string())]),
            preserve: PreservedBoardState {
                position: Point { x: 25.0, y: 30.0 },
                rotation: 0.0,
                layer: "F.Cu".to_string(),
                locked: false,
            },
        };
        let updated =
            update_footprint_item(&item, &change, &BTreeMap::from([("GND".to_string(), 1)]))
                .unwrap();

        assert_eq!(
            child_types(&updated),
            before,
            "sync re-typed footprint children; graphics must survive as graphics"
        );

        // And the pad still got the net it was there to get.
        let footprint =
            kiapi::board::types::FootprintInstance::decode(updated.value.as_slice()).unwrap();
        let pad = footprint
            .definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|child| konnect_ipc::builders::any_is(child, "kiapi.board.types.Pad"))
            .map(|child| kiapi::board::types::Pad::decode(child.value.as_slice()).unwrap())
            .next()
            .expect("the pad survived");
        assert_eq!(pad.net.as_ref().unwrap().name, "GND");
    }

    /// The add path calls `apply_footprint_fields` too (`build_mutation_items`),
    /// so a brand-new footprint was corrupted before it ever reached KiCad.
    #[test]
    fn a_newly_added_footprint_keeps_its_graphics_too() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let item = footprint_with_artwork("U2");
        let before = child_types(&item);
        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();

        apply_footprint_fields(
            &mut footprint,
            "U2",
            "NE555",
            "/root/u2",
            false,
            &BTreeMap::from([("1".to_string(), "VCC".to_string())]),
            &BTreeMap::from([("VCC".to_string(), 3)]),
        )
        .unwrap();

        let repacked =
            konnect_ipc::builders::pack_any(&footprint, "kiapi.board.types.FootprintInstance");
        assert_eq!(child_types(&repacked), before);
    }

    /// The invariant that would have caught #244 on its own.
    ///
    /// `create_items`/`update_items` only confirm KiCad *accepted* each item,
    /// and the reported counts are copied from the plan — so the corruption
    /// travelled all the way to a success message. Here the exact damage is
    /// reproduced (every drawing re-typed as a pad, which is what the old
    /// `Pad::decode` filter did) and the shape comparison is shown to see it.
    #[test]
    fn the_post_apply_check_sees_drawings_turned_into_pads() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;

        let sent = footprint_with_artwork("U4");
        let expected = footprint_shapes(std::iter::once(&sent));
        assert_eq!(
            expected["U4"],
            FootprintShape {
                pads: 1,
                drawings: 6
            }
        );

        // Exactly #244: decode every child as a Pad and pack it back as one.
        let mut corrupted =
            kiapi::board::types::FootprintInstance::decode(sent.value.as_slice()).unwrap();
        for child in &mut corrupted.definition.as_mut().unwrap().items {
            if let Ok(pad) = kiapi::board::types::Pad::decode(child.value.as_slice()) {
                *child = konnect_ipc::builders::pack_any(&pad, "kiapi.board.types.Pad");
            }
        }
        let corrupted =
            konnect_ipc::builders::pack_any(&corrupted, "kiapi.board.types.FootprintInstance");
        let actual = footprint_shapes(std::iter::once(&corrupted));

        // The reported symptom, reproduced: the five graphic shapes each become
        // a pad. The text survives — `BoardText`'s bytes genuinely fail to
        // decode as a `Pad`, while `BoardGraphicShape`'s do not — which is why
        // #239 reported footprints losing their *graphics* while their
        // reference and value text stayed put.
        assert_eq!(
            actual["U4"],
            FootprintShape {
                pads: 6,
                drawings: 1
            }
        );
        assert_ne!(actual["U4"], expected["U4"]);
    }

    /// A child that declares itself a pad and will not decode is a real
    /// failure, and has to be reported as *that*.
    ///
    /// Skipping it silently does still end in an error — the "footprint has no
    /// pad N" check downstream fires, because the pad never made it into
    /// `seen_pads` — but that error sends the reader looking for a missing pad
    /// that is in fact present and unreadable. So this asserts the specific
    /// message, not merely that something failed: a neuter that restored the
    /// silent skip passed an assertion that only checked for the reference.
    #[test]
    fn an_undecodable_pad_is_reported_not_skipped() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;
        let item = footprint_with_artwork("U3");
        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        for child in &mut footprint.definition.as_mut().unwrap().items {
            if konnect_ipc::builders::any_is(child, "kiapi.board.types.Pad") {
                // Wire type 7 does not exist; nothing can decode this.
                child.value = vec![0xff, 0xff, 0xff];
            }
        }

        let error = apply_footprint_fields(
            &mut footprint,
            "U3",
            "NE555",
            "/root/u3",
            false,
            &BTreeMap::from([("1".to_string(), "VCC".to_string())]),
            &BTreeMap::new(),
        )
        .expect_err("an unreadable pad must not pass silently");
        let text = format!("{error:#}");
        assert!(
            text.contains("U3") && text.contains("cannot read"),
            "must say the pad is unreadable, not that it is missing: {text}"
        );
    }

    #[test]
    fn update_item_changes_only_schematic_owned_fields() {
        use konnect_ipc::gen::kiapi;
        use prost::Message;

        let item = konnect_ipc::KiCadIpcClient::build_footprint_item(
            "Resistor_SMD:R_0603_1608Metric",
            "R1",
            "1k",
            &[konnect_ipc::IpcPadDefinition {
                number: "1".to_string(),
                pad_type: "smd".to_string(),
                shape: "rect".to_string(),
                x: 0.0,
                y: 0.0,
                rotation: 0.0,
                size_x: 1.0,
                size_y: 1.0,
                drill_x: None,
                drill_y: None,
                drill_oval: false,
                layers: vec!["F.Cu".to_string()],
                roundrect_ratio: 0.0,
            }],
            &[],
            &konnect_ipc::IpcFieldPlacement::default(),
            25.0,
            30.0,
            90.0,
            "F.Cu",
        )
        .unwrap();
        let mut footprint =
            kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        footprint.id = Some(kiapi::common::types::Kiid {
            value: "keep-kiid".to_string(),
        });
        footprint.locked = kiapi::common::types::LockedState::LsLocked as i32;
        let item =
            konnect_ipc::builders::pack_any(&footprint, "kiapi.board.types.FootprintInstance");
        let change = PlannedChange::Update {
            kiid: "keep-kiid".to_string(),
            reference: "R2".to_string(),
            value: "10k".to_string(),
            symbol_path: "/root/symbol".to_string(),
            dnp: true,
            pad_nets: BTreeMap::from([("1".to_string(), "VCC".to_string())]),
            preserve: PreservedBoardState {
                position: Point { x: 25.0, y: 30.0 },
                rotation: 90.0,
                layer: "F.Cu".to_string(),
                locked: true,
            },
        };

        let updated =
            update_footprint_item(&item, &change, &BTreeMap::from([("VCC".to_string(), 7)]))
                .unwrap();
        let updated =
            kiapi::board::types::FootprintInstance::decode(updated.value.as_slice()).unwrap();

        assert_eq!(updated.id.as_ref().unwrap().value, "keep-kiid");
        assert_eq!(updated.position, footprint.position);
        assert_eq!(updated.orientation, footprint.orientation);
        assert_eq!(updated.layer, footprint.layer);
        assert_eq!(updated.locked, footprint.locked);
        assert!(updated.attributes.as_ref().unwrap().do_not_populate);
        let pad = updated
            .definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find_map(|item| kiapi::board::types::Pad::decode(item.value.as_slice()).ok())
            .unwrap();
        assert_eq!(pad.net.as_ref().unwrap().name, "VCC");
        assert_eq!(pad.net.as_ref().unwrap().code.as_ref().unwrap().value, 7);
    }

    #[test]
    fn removing_a_pad_from_a_routed_net_conflicts_the_whole_plan() {
        let design = ExportedDesign {
            components: vec![DesignComponent {
                pad_nets: BTreeMap::new(),
                ..resistor("R1", "/sheet/existing")
            }],
            skipped: Vec::new(),
        };
        let board = BoardState {
            footprints: vec![BoardFootprint {
                kiid: "existing-kiid".to_string(),
                reference: "R1".to_string(),
                value: "10k".to_string(),
                footprint_id: "Resistor_SMD:R_0603_1608Metric".to_string(),
                symbol_path: Some("/sheet/existing".to_string()),
                pad_nets: BTreeMap::from([("1".to_string(), "VCC".to_string())]),
                position: Point { x: 1.0, y: 2.0 },
                rotation: 0.0,
                layer: "F.Cu".to_string(),
                locked: false,
                dnp: false,
                not_in_schematic: false,
            }],
            routed_nets: BTreeMap::from([("VCC".to_string(), 1)]),
            bounds: Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 10.0,
                max_y: 10.0,
            },
        };

        let plan = plan_sync("netlist", &design, &board);

        assert_eq!(plan.status, PlanStatus::Conflict);
        assert!(plan.changes.is_empty());
        assert!(plan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "routed_pad_net_change"));
    }

    #[test]
    fn already_synchronized_design_is_noop() {
        let design = ExportedDesign {
            components: vec![resistor("R1", "/sheet/existing")],
            skipped: Vec::new(),
        };
        let plan = plan_sync(
            "netlist",
            &design,
            &board_with(vec![board_resistor("R1", Some("/sheet/existing"))]),
        );

        assert_eq!(plan.status, PlanStatus::Noop);
        assert!(plan.changes.is_empty());
        assert_eq!(plan.counts.conflicts.planned, 0);
    }

    #[test]
    fn footprint_swap_conflicts_but_an_unrouted_net_change_is_planned() {
        let design = ExportedDesign {
            components: vec![resistor("R1", "/sheet/existing")],
            skipped: Vec::new(),
        };
        let mut footprint = board_resistor("R1", Some("/sheet/existing"));
        footprint.footprint_id = "Resistor_SMD:R_0805_2012Metric".to_string();
        let swap = plan_sync("netlist", &design, &board_with(vec![footprint]));
        assert_eq!(swap.status, PlanStatus::Conflict);
        assert!(swap
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "footprint_id_changed"));

        let mut footprint = board_resistor("R1", Some("/sheet/existing"));
        footprint
            .pad_nets
            .insert("1".to_string(), "OLD_VCC".to_string());
        let net_change = plan_sync("netlist", &design, &board_with(vec![footprint]));
        assert_eq!(net_change.status, PlanStatus::Ready);
        assert_eq!(net_change.counts.pads_reassigned.planned, 1);
    }

    #[test]
    fn on_board_no_skips_absent_but_conflicts_when_present() {
        let design = ExportedDesign {
            components: Vec::new(),
            skipped: vec![SkippedComponent {
                reference: "R1".to_string(),
                symbol_path: "/sheet/existing".to_string(),
            }],
        };
        let absent = plan_sync("netlist", &design, &board_with(Vec::new()));
        assert_eq!(absent.status, PlanStatus::Noop);
        assert_eq!(absent.counts.skipped_by_flag.planned, 1);

        let present = plan_sync(
            "netlist",
            &design,
            &board_with(vec![board_resistor("R1", Some("/sheet/existing"))]),
        );
        assert_eq!(present.status, PlanStatus::Conflict);
        assert!(present
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "on_board_exclusion_conflict"));
    }

    #[test]
    fn reference_only_possible_rename_is_a_conflict() {
        let design = ExportedDesign {
            components: vec![resistor("R2", "/sheet/existing")],
            skipped: Vec::new(),
        };
        let plan = plan_sync(
            "netlist",
            &design,
            &board_with(vec![board_resistor("R1", None)]),
        );

        assert_eq!(plan.status, PlanStatus::Conflict);
        assert!(plan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "reference_only_rename_ambiguous"));
    }

    #[test]
    fn empty_and_duplicate_component_exports_are_rejected() {
        let empty = parse_exported_netlist("(export (components) (nets))")
            .unwrap_err()
            .to_string();
        assert!(empty.contains("zero components"), "{empty}");

        let duplicate = r#"
(export
  (components
    (comp (ref "R1") (value "1k") (footprint "Resistor_SMD:R_0603_1608Metric")
      (sheetpath (tstamps "/one/")) (tstamps "one"))
    (comp (ref "R1") (value "2k") (footprint "Resistor_SMD:R_0603_1608Metric")
      (sheetpath (tstamps "/two/")) (tstamps "two")))
  (nets))
"#;
        let duplicate = parse_exported_netlist(duplicate).unwrap_err().to_string();
        assert!(
            duplicate.contains("duplicate component reference R1"),
            "{duplicate}"
        );
    }

    #[test]
    fn plan_revision_changes_when_reviewed_board_bounds_change() {
        let design = ExportedDesign {
            components: vec![resistor("R1", "/sheet/new")],
            skipped: Vec::new(),
        };
        let first = plan_sync("netlist", &design, &board_with(Vec::new()));
        let mut changed_board = board_with(Vec::new());
        changed_board.bounds.max_x = 11.0;
        let second = plan_sync("netlist", &design, &changed_board);

        assert_ne!(first.plan_revision, second.plan_revision);
    }

    /// A plan revision must survive the clock. `kicad-cli` stamps the export
    /// time and its own version into every netlist, so hashing the raw source
    /// changed the revision every second — and apply, which requires the
    /// revision a dry run returned, could then only succeed if both calls
    /// landed inside the same wall-clock second.
    #[test]
    fn plan_revision_ignores_the_export_timestamp_and_tool_version() {
        let netlist = |date: &str, tool: &str| {
            format!(
                "(export (version \"E\")
  (design
    (source \"/tmp/x.kicad_sch\")
    (date \"{date}\")
    (tool \"{tool}\")
  )
  (components
    (comp (ref \"R1\")
      (value \"10k\")
      (footprint \"Resistor_SMD:R_0805\")
      (tstamps \"/aaa\")))
  (nets
    (net (code \"1\") (name \"GND\")
      (node (ref \"R1\") (pin \"1\")))))
"
            )
        };
        let board = BoardState {
            footprints: Vec::new(),
            routed_nets: BTreeMap::new(),
            bounds: Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 0.0,
                max_y: 0.0,
            },
        };
        let a = plan_revision(
            &netlist("2026-08-15T14:48:16", "kicad-cli (10.0.5)"),
            &board,
        );
        let b = plan_revision(
            &netlist("2026-08-15T14:48:18", "kicad-cli (10.0.5)"),
            &board,
        );
        assert_eq!(a, b, "two seconds apart is not a design change");

        let c = plan_revision(
            &netlist("2026-08-15T14:48:16", "kicad-cli (10.1.0)"),
            &board,
        );
        assert_eq!(a, c, "a KiCad upgrade is not a design change");

        // A real change still moves it, or the guard is worthless.
        let changed = netlist("2026-08-15T14:48:16", "kicad-cli (10.0.5)")
            .replace("Resistor_SMD:R_0805", "Resistor_SMD:R_0603");
        assert_ne!(
            a,
            plan_revision(&changed, &board),
            "a footprint swap must move the revision"
        );
    }

    #[test]
    fn plan_revision_keeps_nested_and_quoted_design_content() {
        let netlist = |nested_date: &str, value: &str| {
            format!(
                r#"(export
  (design (date "2026-08-15T14:48:16") (tool "kicad-cli (10.0.5)"))
  (components
    (comp (ref "R1")
      (value "{value}")
      (footprint "Resistor_SMD:R_0805")
      (date "{nested_date}")
      (tstamps "/aaa")))
  (nets
    (net (code "1") (name "GND")
      (node (ref "R1") (pin "1")))))"#
            )
        };
        let board = board_with(Vec::new());
        let baseline = plan_revision(&netlist("2025-01-01", "literal (tool alpha)"), &board);

        assert_ne!(
            baseline,
            plan_revision(&netlist("2025-01-02", "literal (tool alpha)"), &board),
            "a nested date node is component content, not export metadata"
        );
        assert_ne!(
            baseline,
            plan_revision(&netlist("2025-01-01", "literal (tool beta)"), &board),
            "tool-like text inside a quoted value is design content"
        );
    }
}
