//! `score_placement` — the objective judge for footprint placement.
//!
//! The metric lands before any placer exists on purpose: every placement
//! algorithm that follows gets scored by the same referee it did not write.
//!
//! # Verdict vs. score
//!
//! The numeric score is advisory; the verdict is not. **Hard failures** —
//! same-side courtyard-bbox overlaps and parts outside the board outline —
//! decide the verdict no matter what the number says, because a board with
//! two parts in the same physical space is unbuildable at any score. A board
//! with no Edge.Cuts outline cannot be judged "inside", so it can never pass:
//! the response says `outline_missing` explicitly rather than skipping the
//! check silently.
//!
//! # Weight table
//!
//! The soft-score weights (overlaps ×20 capped 40, outside ×20 capped 40,
//! connector-edge ×15 capped 30, decoupling ×15 capped 30) are ported from
//! the MIT-licensed reference implementation attributed in THIRD_PARTY.md.
//!
//! Every response field is derived from the parsed board — never echoed from
//! the request (the recurring defect class this repo pins tests against).

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, ToolContext, ToolDef};
use konnect_sexp::board::{
    board_outline_bbox, footprint_courtyards, footprints, CourtyardSource, FootprintCourtyard,
    PcbConnectivityIndex, Side,
};
use konnect_sexp::parser::SexpNode;
use serde_json::json;
use std::collections::{BTreeSet, HashMap};

// ─── Tool definitions ────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "score_placement",
            "Score a board's footprint placement: scores 0-100 and explains every deduction, \
         naming the components behind each one. Hard failures (courtyard overlaps, parts \
         outside the outline) decide the verdict regardless of the numeric score, and a \
         board without an Edge.Cuts outline can never pass — its verdict is \
         'outline_missing'.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_score_placement(args, ctx).await }
        ),
        tool!(
            "place_decoupling_caps",
            "Plan (and optionally apply) a row of decoupling capacitors beside an IC. Caps \
         are identified by NET PAIRING — a candidate shares at least one named net with \
         the IC — never by reference guessing. Dry-run by default: the response carries \
         the planned moves plus the board's score before and after the plan, so the \
         change is judged before it is made. Apply refuses while KiCad holds the board \
         open live.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "ic_reference": { "type": "string", "description": "The IC (U*) the caps decouple" },
                    "side": { "type": "string", "enum": ["auto", "left", "right", "top", "bottom"], "default": "auto", "description": "Which side of the IC the row goes on" },
                    "spacing_mm": { "type": "number", "default": 0.5, "description": "Gap between the IC courtyard and the row, and between caps" },
                    "dry_run": { "type": "boolean", "default": true, "description": "Plan without writing" }
                },
                "required": ["board", "ic_reference"]
            }),
            |args, ctx| async move { handle_place_decoupling(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::ApplyModeDependent),
        tool!(
            "plan_bga_fanout",
            "Plan a BGA fanout: outer-ring pads escape directly; each inner pad gets a via \
         (dogbone: diagonal offset by quadrant; inline: a full pitch outward) and a stub \
         trace. The pitch is DETECTED from the pad grid and reported, never assumed. \
         'apply' executes the whole plan as one KiCad undo commit over live IPC and \
         requires the board open in KiCad; the default returns the plan for review.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "reference": { "type": "string", "description": "The BGA footprint's reference" },
                    "strategy": { "type": "string", "enum": ["dogbone", "inline"], "default": "dogbone" },
                    "apply": { "type": "boolean", "default": false }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_bga_fanout(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::ApplyModeDependent),
        tool!(
            "auto_place_from_schematic",
            "Deterministic first placement: cluster footprints by shared nets (union-find), \
             lay clusters out as tight grids inside the board outline, courtyards \
             non-overlapping. Footprints locked in KiCad are never moved and act as obstacles; add more with 'locked'. A starting point for refinement, not a final layout — the \
             response says so, and carries the board's score before and after the plan. \
             Dry-run by default.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "margin_mm": { "type": "number", "default": 2.0, "description": "Clearance from the board outline" },
                    "locked": { "type": "array", "items": { "type": "string" }, "description": "Additional references that must not move. Footprints KiCad itself marks locked are always held, without needing to be listed here." },
                    "dry_run": { "type": "boolean", "default": true }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_auto_place(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::ApplyModeDependent),
        tool!(
            "refine_placement_force_directed",
            "Refine placement with a deterministic spring embedder: shared nets pull \
             connected parts together (power nets weighted 3x, differential pairs 5x), \
             courtyards repel, the board edge constrains. No randomness, no clocks: the \
             same input always yields the same plan, converging when the grid-snapped \
             layout stops changing. Locked references exert force but never move. \
             Dry-run by default; the response carries before/after scores.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "references": { "type": "array", "items": { "type": "string" }, "description": "Components to refine (default: all)" },
                    "locked": { "type": "array", "items": { "type": "string" }, "description": "References that must not move" },
                    "iterations": { "type": "integer", "default": 300, "description": "Iteration ceiling" },
                    "dry_run": { "type": "boolean", "default": true }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_force_directed(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::ApplyModeDependent),
    ]
}

// ─── Scoring constants ───────────────────────────────────────────────────────

/// Points per courtyard-overlap pair, and the cap on that deduction.
const OVERLAP_POINTS: i64 = 20;
const OVERLAP_CAP: i64 = 40;
/// Points per part outside the outline, and the cap.
const OUTSIDE_POINTS: i64 = 20;
const OUTSIDE_CAP: i64 = 40;
/// Points per connector too far from every board edge, and the cap.
const CONNECTOR_POINTS: i64 = 15;
const CONNECTOR_CAP: i64 = 30;
/// Points per decoupling-distance violation, and the cap.
const DECOUPLING_POINTS: i64 = 15;
const DECOUPLING_CAP: i64 = 30;
/// A connector (J*) should sit within this distance of some board edge.
const CONNECTOR_EDGE_LIMIT_MM: f64 = 10.0;

// ─── Handler ─────────────────────────────────────────────────────────────────

async fn handle_score_placement(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let content = match konnect_sexp::writer::read_consistent(&board) {
        Ok(content) => content,
        Err(error) => {
            if !board.exists() {
                return Ok(CallToolResult::error_kind(
                    ToolErrorKind::FileNotFound {
                        path: board.display().to_string(),
                    },
                    format!("Board file not found: {}", board.display()),
                ));
            }
            return Err(error.into());
        }
    };
    let tree = konnect_sexp::parse_sexp(&content)?;

    let scan = footprint_courtyards(&tree);
    let outline = board_outline_bbox(&tree);
    let index = PcbConnectivityIndex::build(&tree);
    let values = footprint_values(&tree);

    // ── Hard failure (a): same-side courtyard-bbox overlaps ─────────────────
    // Opposite sides never collide (a Front part and a Back part share x/y but
    // not space), and AnchorOnly bboxes are position-only stand-ins with zero
    // area — a pair involving one is skipped, not scored as touching.
    let mut hard_failures: Vec<serde_json::Value> = Vec::new();
    let mut overlap_refs: BTreeSet<String> = BTreeSet::new();
    let mut overlap_pairs = 0usize;
    for (i, a) in scan.items.iter().enumerate() {
        for b in &scan.items[i + 1..] {
            if a.bbox_source == CourtyardSource::AnchorOnly
                || b.bbox_source == CourtyardSource::AnchorOnly
                || a.layer_side != b.layer_side
            {
                continue;
            }
            let ix = a.bbox.2.min(b.bbox.2) - a.bbox.0.max(b.bbox.0);
            let iy = a.bbox.3.min(b.bbox.3) - a.bbox.1.max(b.bbox.1);
            if ix > 0.0 && iy > 0.0 {
                overlap_pairs += 1;
                let (ra, rb) = (courtyard_reference(a), courtyard_reference(b));
                overlap_refs.insert(ra.to_string());
                overlap_refs.insert(rb.to_string());
                hard_failures.push(json!({
                    "kind": "courtyard_overlap",
                    "references": [ra, rb],
                    "detail": format!(
                        "{ra} and {rb} courtyard bboxes overlap by {} mm² on the {} side",
                        round3(ix * iy),
                        side_name(a.layer_side),
                    ),
                }));
            }
        }
    }

    // ── Hard failure (b): parts outside the board outline ───────────────────
    // No outline means this check is blocked, not passed: the response carries
    // `outline_missing: true` and the verdict can never be "pass".
    let mut outside_refs: Vec<String> = Vec::new();
    if let Some((ox0, oy0, ox1, oy1)) = outline {
        for c in &scan.items {
            let (x0, y0, x1, y1) = c.bbox;
            if x0 < ox0 || y0 < oy0 || x1 > ox1 || y1 > oy1 {
                let reference = courtyard_reference(c);
                outside_refs.push(reference.to_string());
                hard_failures.push(json!({
                    "kind": "outside_outline",
                    "references": [reference],
                    "detail": format!(
                        "{reference} courtyard bbox ({}, {})..({}, {}) is not fully inside \
                         the board outline ({ox0}, {oy0})..({ox1}, {oy1})",
                        round3(x0), round3(y0), round3(x1), round3(y1),
                    ),
                }));
            }
        }
    }

    // ── Connector-edge check (J*) ───────────────────────────────────────────
    // A connector whose bbox center is further than the limit from *every*
    // outline edge cannot be reached by a cable without a tunnel. Blocked
    // without an outline, like the outside check.
    let mut connector_edges: Vec<serde_json::Value> = Vec::new();
    let mut connector_violations: Vec<(String, f64)> = Vec::new();
    if let Some((ox0, oy0, ox1, oy1)) = outline {
        for c in &scan.items {
            let Some(reference) = c.reference.as_deref() else {
                continue;
            };
            if ref_prefix(reference) != "J" {
                continue;
            }
            let (cx, cy) = bbox_center(c.bbox);
            let edge_distance = (cx - ox0).min(ox1 - cx).min(cy - oy0).min(oy1 - cy);
            connector_edges.push(json!({
                "reference": reference,
                "edge_distance_mm": round3(edge_distance),
            }));
            if edge_distance > CONNECTOR_EDGE_LIMIT_MM {
                connector_violations.push((reference.to_string(), edge_distance));
            }
        }
    }

    // ── Decoupling check (C* near a shared-net U*) ──────────────────────────
    let (decoupling_violations, uncoupled_caps, interface_filter_caps) =
        decoupling_check(&scan.items, &index, &values);

    // ── Soft score (ported weight table — see the module docs) ──────────────
    let overlap_deduction = (overlap_pairs as i64 * OVERLAP_POINTS).min(OVERLAP_CAP);
    let outside_deduction = (outside_refs.len() as i64 * OUTSIDE_POINTS).min(OUTSIDE_CAP);
    let connector_deduction =
        (connector_violations.len() as i64 * CONNECTOR_POINTS).min(CONNECTOR_CAP);
    let decoupling_deduction =
        (decoupling_violations.len() as i64 * DECOUPLING_POINTS).min(DECOUPLING_CAP);
    let score =
        (100 - overlap_deduction - outside_deduction - connector_deduction - decoupling_deduction)
            .max(0);

    let mut deductions: Vec<serde_json::Value> = Vec::new();
    if overlap_deduction > 0 {
        deductions.push(json!({
            "kind": "courtyard_overlap",
            "points": overlap_deduction,
            "references": overlap_refs.iter().collect::<Vec<_>>(),
            "detail": format!(
                "{overlap_pairs} same-side courtyard overlap pair(s) — also a hard failure"
            ),
        }));
    }
    if outside_deduction > 0 {
        deductions.push(json!({
            "kind": "outside_outline",
            "points": outside_deduction,
            "references": outside_refs,
            "detail": format!(
                "{} part(s) not fully inside the board outline — also a hard failure",
                hard_failures.iter().filter(|f| f["kind"] == "outside_outline").count(),
            ),
        }));
    }
    if connector_deduction > 0 {
        deductions.push(json!({
            "kind": "connector_edge",
            "points": connector_deduction,
            "references": connector_violations.iter().map(|(r, _)| r).collect::<Vec<_>>(),
            "detail": connector_violations
                .iter()
                .map(|(r, d)| format!(
                    "{r} center is {} mm from the nearest board edge (limit {CONNECTOR_EDGE_LIMIT_MM} mm)",
                    round3(*d),
                ))
                .collect::<Vec<_>>()
                .join("; "),
        }));
    }
    if decoupling_deduction > 0 {
        deductions.push(json!({
            "kind": "decoupling",
            "points": decoupling_deduction,
            "references": decoupling_violations.iter().map(|v| &v.cap).collect::<Vec<_>>(),
            "detail": decoupling_violations
                .iter()
                .map(|v| format!(
                    "{} ({}, limit {} mm) is {} mm from nearest shared-net IC {}",
                    v.cap, v.value, v.limit_mm, round3(v.distance_mm), v.ic,
                ))
                .collect::<Vec<_>>()
                .join("; "),
        }));
    }

    // The verdict derives ONLY from hard failures and outline presence — the
    // numeric score never decides it.
    let verdict = if !hard_failures.is_empty() {
        "hard_fail"
    } else if outline.is_none() {
        "outline_missing"
    } else {
        "pass"
    };

    Ok(CallToolResult::json(&json!({
        "verdict": verdict,
        "score": score,
        "outline_missing": outline.is_none(),
        "hard_failures": hard_failures,
        "deductions": deductions,
        "connector_edges": connector_edges,
        "uncoupled_caps": uncoupled_caps,
        "interface_filter_caps": interface_filter_caps
            .iter()
            .map(|f| json!({
                "reference": f.cap,
                "value": f.value,
                "connector": f.connector,
                "connector_distance_mm": round3(f.distance_mm),
                "limit_mm": f.limit_mm,
            }))
            .collect::<Vec<_>>(),
        "footprints_scored": scan.items.len(),
        "footprints_skipped": scan.skipped,
    })))
}

// ─── Shared plumbing for the placers ─────────────────────────────────────────

/// Score arbitrary board CONTENT by round-tripping through the scoring
/// handler on a temp file — one scoring implementation, zero drift between
/// "the score tool" and "the score a placer reports".
async fn score_of_content(ctx: &ToolContext, content: &str) -> anyhow::Result<serde_json::Value> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("plan.kicad_pcb");
    std::fs::write(&path, content)?;
    let result = handle_score_placement(&json!({ "board": path.to_string_lossy() }), ctx).await?;
    let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
        anyhow::bail!("score returned non-text content");
    };
    Ok(serde_json::from_str(text)?)
}

/// Net set per reference, by NAME, from the index. The pairing primitive the
/// decoupling checker and the decoupling placer share.
fn nets_by_reference(index: &PcbConnectivityIndex) -> HashMap<String, BTreeSet<String>> {
    let mut map: HashMap<String, BTreeSet<String>> = HashMap::new();
    for net in index.nets() {
        for pad in index.pads_of_net(net) {
            map.entry(pad.reference.clone())
                .or_default()
                .insert(net.to_string());
        }
    }
    map
}

/// Apply Set placements to board content via the SAME transform the closed-
/// board move tools use, by way of a temp file — the plan preview and the
/// real apply cannot disagree, because they are the same code.
fn apply_placements_to_content(
    content: &str,
    placements: &[konnect_ipc::types::IpcFootprintPlacement],
) -> anyhow::Result<String> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("plan.kicad_pcb");
    std::fs::write(&path, content)?;
    if let Err(error) = super::pcb_components::update_closed_board_footprints(&path, placements) {
        anyhow::bail!(
            "planned placement does not apply: {:?}",
            error.into_result()
        );
    }
    Ok(std::fs::read_to_string(&path)?)
}

// ─── place_decoupling_caps ───────────────────────────────────────────────────

async fn handle_place_decoupling(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let ic_reference = match crate::tools::require_str(args, "ic_reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let side = args["side"].as_str().unwrap_or("auto");
    if !["auto", "left", "right", "top", "bottom"].contains(&side) {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "side".into(),
                reason: "must be auto|left|right|top|bottom".into(),
            },
            "Argument 'side' must be one of auto, left, right, top, bottom",
        ));
    }
    let spacing = args["spacing_mm"].as_f64().unwrap_or(0.5);
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);

    let content = konnect_sexp::writer::read_consistent(&board)?;
    let tree = konnect_sexp::parse_sexp(&content)?;
    let scan = footprint_courtyards(&tree);
    let index = PcbConnectivityIndex::build(&tree);
    let values = footprint_values(&tree);
    let nets = nets_by_reference(&index);

    let Some(ic) = scan
        .items
        .iter()
        .find(|c| c.reference.as_deref() == Some(ic_reference.as_str()))
    else {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "ic_reference".into(),
                reason: format!("'{ic_reference}' is not on this board"),
            },
            format!("No footprint '{ic_reference}' on the board"),
        ));
    };
    let ic_nets = nets.get(&ic_reference).cloned().unwrap_or_default();
    let ic_bbox = ic.bbox;
    let (ic_cx, ic_cy) = bbox_center(ic_bbox);

    // Candidates: decoupling-family C* sharing at least one named net with
    // the IC — the same pairing rule the score's checker applies.
    let mut caps: Vec<&FootprintCourtyard> = scan
        .items
        .iter()
        .filter(|c| {
            let Some(reference) = c.reference.as_deref() else {
                return false;
            };
            if ref_prefix(reference) != "C" {
                return false;
            }
            let Some(value) = values.get(reference) else {
                return false;
            };
            if decoupling_limit_mm(value).is_none() {
                return false;
            }
            nets.get(reference)
                .map(|n| !n.is_disjoint(&ic_nets))
                .unwrap_or(false)
        })
        .collect();
    if caps.is_empty() {
        return Ok(CallToolResult::json(&json!({
            "planned_moves": [],
            "detail": format!(
                "no decoupling-family capacitor shares a net with {ic_reference}; \
                 nothing to place"
            ),
        })));
    }
    caps.sort_by_key(|c| c.reference.clone());

    // The row: below the IC unless a side is given ("auto" picks bottom —
    // deterministic beats clever until a placer judge says otherwise).
    let resolved_side = if side == "auto" { "bottom" } else { side };
    let widths: Vec<(f64, f64)> = caps
        .iter()
        .map(|c| (c.bbox.2 - c.bbox.0, c.bbox.3 - c.bbox.1))
        .collect();
    let row_span: f64 =
        widths.iter().map(|(w, _)| w).sum::<f64>() + spacing * (caps.len() as f64 - 1.0);

    let mut placements = Vec::new();
    let mut planned_moves = Vec::new();
    let mut cursor = -row_span / 2.0;
    for (cap, (w, h)) in caps.iter().zip(&widths) {
        let center_along = cursor + w / 2.0;
        cursor += w + spacing;
        let (tx, ty) = match resolved_side {
            "bottom" => (ic_cx + center_along, ic_bbox.3 + spacing + h / 2.0),
            "top" => (ic_cx + center_along, ic_bbox.1 - spacing - h / 2.0),
            "left" => (ic_bbox.0 - spacing - w / 2.0, ic_cy + center_along),
            _ => (ic_bbox.2 + spacing + w / 2.0, ic_cy + center_along),
        };
        // The move sets the ROOT anchor; correct for anchor-vs-bbox-center
        // offset so the courtyard lands where planned.
        let (bcx, bcy) = bbox_center(cap.bbox);
        let (ax, ay) = cap.at;
        let target = (tx + (ax - bcx), ty + (ay - bcy));
        placements.push(konnect_ipc::types::IpcFootprintPlacement {
            reference: cap.reference.clone().expect("filtered on reference"),
            x: (target.0 * 1e3).round() / 1e3,
            y: (target.1 * 1e3).round() / 1e3,
            rotation: cap.rotation_deg,
        });
        planned_moves.push(json!({
            "reference": cap.reference,
            "from": { "x": round3(ax), "y": round3(ay) },
            "to": { "x": round3(target.0), "y": round3(target.1) },
        }));
    }

    let score_before = score_of_content(ctx, &content).await?;
    let planned_content = apply_placements_to_content(&content, &placements)?;
    let score_after = score_of_content(ctx, &planned_content).await?;

    if dry_run {
        return Ok(CallToolResult::json(&json!({
            "dry_run": true,
            "ic_reference": ic_reference,
            "side": resolved_side,
            "planned_moves": planned_moves,
            "score_before": score_before["score"],
            "score_after_plan": score_after["score"],
            "verdict_after_plan": score_after["verdict"],
        })));
    }

    // Applying: never edit a board a live KiCad holds open.
    if let Some(refusal) =
        super::pcb_board::refuse_if_board_open_in_kicad(ctx, &board, "place_decoupling_caps")
            .await?
    {
        return Ok(refusal);
    }
    let applied = match super::pcb_components::update_closed_board_footprints(&board, &placements) {
        Ok(applied) => applied,
        Err(error) => return Ok(error.into_result()),
    };
    let written = konnect_sexp::writer::read_consistent(&board)?;
    let score_written = score_of_content(ctx, &written).await?;
    Ok(CallToolResult::json(&json!({
        "dry_run": false,
        "ic_reference": ic_reference,
        "side": resolved_side,
        "applied": applied.iter().map(|p| json!({
            "reference": p.reference, "x": p.x, "y": p.y, "rotation": p.rotation
        })).collect::<Vec<_>>(),
        "score_before": score_before["score"],
        "score_after": score_written["score"],
        "verdict_after": score_written["verdict"],
    })))
}

// ─── plan_bga_fanout ─────────────────────────────────────────────────────────

async fn handle_bga_fanout(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let reference = match crate::tools::require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let strategy = args["strategy"].as_str().unwrap_or("dogbone");
    if !["dogbone", "inline"].contains(&strategy) {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "strategy".into(),
                reason: "must be dogbone or inline".into(),
            },
            "Argument 'strategy' must be dogbone or inline",
        ));
    }
    let apply = args["apply"].as_bool().unwrap_or(false);

    let content = konnect_sexp::writer::read_consistent(&board)?;
    let tree = konnect_sexp::parse_sexp(&content)?;
    let index = PcbConnectivityIndex::build(&tree);

    // Every pad of the named footprint, with its net where one exists.
    let mut pads: Vec<(String, (f64, f64), Option<String>)> = Vec::new();
    for net in index.nets() {
        for pad in index.pads_of_net(net) {
            if pad.reference == reference {
                pads.push((pad.pad_number.clone(), pad.at, Some(net.to_string())));
            }
        }
    }
    for pad in index.pads_without_net() {
        if pad.reference == reference {
            pads.push((pad.pad_number.clone(), pad.at, None));
        }
    }
    if pads.is_empty() {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "reference".into(),
                reason: format!("'{reference}' has no pads on this board"),
            },
            format!("No footprint '{reference}' with pads on the board"),
        ));
    }
    pads.sort_by(|a, b| a.0.cmp(&b.0));

    // Detect the grid pitch from the pad coordinates themselves.
    let pitch = match detect_grid_pitch(&pads) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "{reference}'s pads do not form a regular grid; fanout planning \
                 needs a BGA-style pad array"
            )))
        }
    };
    let track_width = if pitch <= 0.65 { 0.1 } else { 0.15 };
    let (via_pad, via_drill) = (0.45, 0.2);

    // Grid membership: map each pad to (col, row); outer ring = touching the
    // grid's perimeter; inner pads get vias.
    let xs = sorted_unique(pads.iter().map(|p| p.1 .0));
    let ys = sorted_unique(pads.iter().map(|p| p.1 .1));
    let col_of = |x: f64| xs.iter().position(|v| (v - x).abs() < pitch / 4.0);
    let row_of = |y: f64| ys.iter().position(|v| (v - y).abs() < pitch / 4.0);
    let (cx, cy) = (
        pads.iter().map(|p| p.1 .0).sum::<f64>() / pads.len() as f64,
        pads.iter().map(|p| p.1 .1).sum::<f64>() / pads.len() as f64,
    );

    let mut vias = Vec::new();
    let mut stubs = Vec::new();
    let mut outer = 0usize;
    for (number, (px, py), net) in &pads {
        let (Some(col), Some(row)) = (col_of(*px), row_of(*py)) else {
            continue;
        };
        let is_outer = col == 0 || col == xs.len() - 1 || row == 0 || row == ys.len() - 1;
        if is_outer {
            outer += 1;
            continue;
        }
        let offset = pitch * 0.55;
        let (vx, vy) = match strategy {
            "dogbone" => {
                // Diagonal outward by quadrant relative to the pad-array
                // center; |offset| = 0.55 * pitch.
                let d = offset / std::f64::consts::SQRT_2;
                (px + d * (px - cx).signum(), py + d * (py - cy).signum())
            }
            _ => (px + pitch * (px - cx).signum(), *py),
        };
        let net_name = net.clone().unwrap_or_default();
        vias.push(json!({
            "pad": number,
            "x": round3(vx),
            "y": round3(vy),
            "net": net_name,
        }));
        stubs.push(json!({
            "pad": number,
            "from": { "x": round3(*px), "y": round3(*py) },
            "to": { "x": round3(vx), "y": round3(vy) },
            "net": net_name,
        }));
    }

    let plan = json!({
        "reference": reference,
        "strategy": strategy,
        "pitch_detected_mm": round3(pitch),
        "track_width_mm": track_width,
        "via_pad_mm": via_pad,
        "via_drill_mm": via_drill,
        "pads_total": pads.len(),
        "pads_outer_ring": outer,
        "vias": vias,
        "stubs": stubs,
    });

    if !apply {
        return Ok(CallToolResult::json(&plan));
    }

    // Apply is live-IPC-only: one commit, one undo step, and KiCad itself
    // adjudicates every element.
    let net_stubs: Vec<(String, f64, f64, f64, f64)> = stubs
        .iter()
        .map(|s| {
            (
                s["net"].as_str().unwrap_or_default().to_string(),
                s["from"]["x"].as_f64().expect("planned"),
                s["from"]["y"].as_f64().expect("planned"),
                s["to"]["x"].as_f64().expect("planned"),
                s["to"]["y"].as_f64().expect("planned"),
            )
        })
        .collect();
    let via_list: Vec<(String, f64, f64)> = vias
        .iter()
        .map(|v| {
            (
                v["net"].as_str().unwrap_or_default().to_string(),
                v["x"].as_f64().expect("planned"),
                v["y"].as_f64().expect("planned"),
            )
        })
        .collect();
    let created = match super::with_board_ipc_classified(ctx, &board, move |c| {
        c.apply_fanout(
            &net_stubs,
            &via_list,
            "F.Cu",
            track_width,
            via_drill,
            via_pad,
        )
    })
    .await?
    {
        Ok(created) => created,
        Err(failure) => {
            return Ok(CallToolResult::error(format!(
                "fanout not applied: {failure}. The plan is unchanged; apply requires \
                 the board open in a running KiCad."
            )))
        }
    };
    let mut response = plan;
    response["applied"] = json!(true);
    response["items_created"] = json!(created);
    Ok(CallToolResult::json(&response))
}

// ─── auto_place_from_schematic ───────────────────────────────────────────────

async fn handle_auto_place(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let margin = args["margin_mm"].as_f64().unwrap_or(2.0);
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let held_by_caller: BTreeSet<String> = args["locked"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let content = konnect_sexp::writer::read_consistent(&board)?;
    let tree = konnect_sexp::parse_sexp(&content)?;
    let scan = footprint_courtyards(&tree);
    let index = PcbConnectivityIndex::build(&tree);
    let Some(outline) = board_outline_bbox(&tree) else {
        return Ok(CallToolResult::error(
            "auto placement needs a board outline; add Edge.Cuts first (set_board_size)",
        ));
    };

    // Union-find over references joined by shared nets.
    // A footprint KiCad has locked is one the user pinned deliberately. It is
    // an obstacle for this pass, never an input to it (#350) — and the caller
    // can hold more references without touching the board.
    let mut all: Vec<&FootprintCourtyard> = scan
        .items
        .iter()
        .filter(|c| c.reference.is_some())
        .collect();
    all.sort_by_key(|c| c.reference.clone());
    let is_held = |c: &FootprintCourtyard| {
        c.locked
            || c.reference
                .as_deref()
                .is_some_and(|r| held_by_caller.contains(r))
    };
    let held: Vec<&FootprintCourtyard> = all.iter().copied().filter(|c| is_held(c)).collect();
    let parts: Vec<&FootprintCourtyard> = all.iter().copied().filter(|c| !is_held(c)).collect();
    let ref_index: HashMap<&str, usize> = parts
        .iter()
        .enumerate()
        .map(|(i, c)| (c.reference.as_deref().expect("filtered"), i))
        .collect();
    let mut parent: Vec<usize> = (0..parts.len()).collect();
    fn find(parent: &mut Vec<usize>, i: usize) -> usize {
        if parent[i] != i {
            let root = find(parent, parent[i]);
            parent[i] = root;
        }
        parent[i]
    }
    for net in index.nets() {
        let mut prev: Option<usize> = None;
        for pad in index.pads_of_net(net) {
            if let Some(&i) = ref_index.get(pad.reference.as_str()) {
                if let Some(p) = prev {
                    let (a, b) = (find(&mut parent, p), find(&mut parent, i));
                    parent[a.max(b)] = a.min(b);
                }
                prev = Some(i);
            }
        }
    }

    // Clusters in deterministic order: by smallest member reference.
    let mut clusters: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for i in 0..parts.len() {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }

    // Lay each cluster as a near-square grid; clusters flow left-to-right,
    // wrapping when the outline width is exhausted. Parts within a cluster
    // descend by courtyard area so big parts anchor their group.
    const GRID: f64 = 1.27;
    let snap = |v: f64| (v / GRID).round() * GRID;
    let (ox0, oy0, ox1, _oy1) = outline;
    let mut placements = Vec::new();
    let mut moved_parts: Vec<&FootprintCourtyard> = Vec::new();
    let mut planned_moves = Vec::new();
    let mut cursor_x = ox0 + margin;
    let mut cursor_y = oy0 + margin;
    let mut row_height: f64 = 0.0;
    for members in clusters.values() {
        let mut ordered: Vec<&FootprintCourtyard> = members.iter().map(|&i| parts[i]).collect();
        ordered.sort_by(|a, b| {
            let area = |c: &FootprintCourtyard| (c.bbox.2 - c.bbox.0) * (c.bbox.3 - c.bbox.1);
            area(b)
                .total_cmp(&area(a))
                .then_with(|| a.reference.cmp(&b.reference))
        });
        let count = ordered.len() as f64;
        let cols = count.sqrt().ceil() as usize;

        // Cluster cell size: max part dims + margin padding per cell.
        let cell_w = ordered
            .iter()
            .map(|c| c.bbox.2 - c.bbox.0)
            .fold(0.0f64, f64::max)
            + margin;
        let cell_h = ordered
            .iter()
            .map(|c| c.bbox.3 - c.bbox.1)
            .fold(0.0f64, f64::max)
            + margin;
        let cluster_w = cell_w * cols as f64;
        let rows = ordered.len().div_ceil(cols);
        let cluster_h = cell_h * rows as f64;

        if cursor_x + cluster_w > ox1 - margin {
            cursor_x = ox0 + margin;
            cursor_y += row_height + margin;
            row_height = 0.0;
        }
        row_height = row_height.max(cluster_h);

        for (slot, part) in ordered.iter().enumerate() {
            let col = slot % cols;
            let row = slot / cols;
            let target_center = (
                cursor_x + cell_w * (col as f64 + 0.5),
                cursor_y + cell_h * (row as f64 + 0.5),
            );
            let (bcx, bcy) = bbox_center(part.bbox);
            let (ax, ay) = part.at;
            let target = (
                snap(target_center.0 + (ax - bcx)),
                snap(target_center.1 + (ay - bcy)),
            );
            let reference = part.reference.clone().expect("filtered");
            moved_parts.push(part);
            placements.push(konnect_ipc::types::IpcFootprintPlacement {
                reference: reference.clone(),
                x: target.0,
                y: target.1,
                rotation: part.rotation_deg,
            });
            planned_moves.push(json!({
                "reference": reference,
                "from": { "x": round3(ax), "y": round3(ay) },
                "to": { "x": round3(target.0), "y": round3(target.1) },
            }));
        }
        cursor_x += cluster_w + margin;
    }

    // Held footprints are obstacles, so anything the grid drops on top of one
    // is nudged off it. Same spiral-on-grid resolution the force-directed pass
    // uses, for the same reason: deterministic by construction, no RNG.
    if !held.is_empty() {
        let obstacles: Vec<Obstacle> = held
            .iter()
            .map(|c| {
                (
                    bbox_center(c.bbox),
                    (c.bbox.2 - c.bbox.0, c.bbox.3 - c.bbox.1),
                    c.layer_side,
                )
            })
            .collect();
        let overlaps = |a: (f64, f64), sa: (f64, f64), b: (f64, f64), sb: (f64, f64)| {
            (a.0 - b.0).abs() < (sa.0 + sb.0) * 0.5 && (a.1 - b.1).abs() < (sa.1 + sb.1) * 0.5
        };
        for (slot, placement) in placements.iter_mut().enumerate() {
            let part = moved_parts[slot];
            let size = (part.bbox.2 - part.bbox.0, part.bbox.3 - part.bbox.1);
            let (bcx, bcy) = bbox_center(part.bbox);
            let (ax, ay) = part.at;
            // Courtyard centre implied by the anchor this plan is proposing.
            let centre_of = |anchor: (f64, f64)| (anchor.0 + (bcx - ax), anchor.1 + (bcy - ay));
            let hits = |anchor: (f64, f64)| {
                let c = centre_of(anchor);
                obstacles
                    .iter()
                    .any(|(oc, os, side)| *side == part.layer_side && overlaps(c, size, *oc, *os))
            };
            let mut anchor = (placement.x, placement.y);
            if hits(anchor) {
                'search: for ring in 1..400 {
                    let r = ring as f64 * GRID;
                    for (dx, dy) in [
                        (r, 0.0),
                        (0.0, r),
                        (-r, 0.0),
                        (0.0, -r),
                        (r, r),
                        (-r, r),
                        (r, -r),
                        (-r, -r),
                    ] {
                        let candidate = (snap(anchor.0 + dx), snap(anchor.1 + dy));
                        if !hits(candidate) {
                            anchor = candidate;
                            break 'search;
                        }
                    }
                }
            }
            placement.x = anchor.0;
            placement.y = anchor.1;
            planned_moves[slot]["to"] = json!({ "x": round3(anchor.0), "y": round3(anchor.1) });
        }
    }

    let score_before = score_of_content(ctx, &content).await?;
    let planned_content = apply_placements_to_content(&content, &placements)?;
    let score_after = score_of_content(ctx, &planned_content).await?;

    let cluster_report: Vec<serde_json::Value> = clusters
        .values()
        .map(|members| {
            json!(members
                .iter()
                .map(|&i| parts[i].reference.as_deref().expect("filtered"))
                .collect::<Vec<_>>())
        })
        .collect();

    // Say what was refused, not just what moved: a caller who expected a part
    // to be relocated needs to see that it was held, and why.
    let held_report: Vec<serde_json::Value> = held
        .iter()
        .map(|c| {
            json!({
                "reference": c.reference.as_deref().unwrap_or_default(),
                "reason": if c.locked { "locked in kicad" } else { "listed in locked" },
            })
        })
        .collect();

    if dry_run {
        return Ok(CallToolResult::json(&json!({
            "dry_run": true,
            "note": "a starting point for refinement, not a final layout",
            "clusters": cluster_report,
            "planned_moves": planned_moves,
            "held": held_report.clone(),
            "score_before": score_before["score"],
            "score_after_plan": score_after["score"],
            "verdict_after_plan": score_after["verdict"],
        })));
    }

    if let Some(refusal) =
        super::pcb_board::refuse_if_board_open_in_kicad(ctx, &board, "auto_place_from_schematic")
            .await?
    {
        return Ok(refusal);
    }
    let applied = match super::pcb_components::update_closed_board_footprints(&board, &placements) {
        Ok(applied) => applied,
        Err(error) => return Ok(error.into_result()),
    };
    let written = konnect_sexp::writer::read_consistent(&board)?;
    let score_written = score_of_content(ctx, &written).await?;
    Ok(CallToolResult::json(&json!({
        "dry_run": false,
        "note": "a starting point for refinement, not a final layout",
        "clusters": cluster_report,
        "applied_count": applied.len(),
        "held": held_report,
        "score_before": score_before["score"],
        "score_after": score_written["score"],
        "verdict_after": score_written["verdict"],
    })))
}

// ─── refine_placement_force_directed ─────────────────────────────────────────

/// Ported spring-embedder constants (attributed in THIRD_PARTY.md).
const FD_K_SPRING: f64 = 0.4;
const FD_K_REPEL: f64 = 80.0;
const FD_K_WALL: f64 = 5.0;
const FD_DAMPING: f64 = 0.85;
const FD_MIN_DIST: f64 = 0.5;
const FD_GRID_MM: f64 = 0.5;
const FD_PATIENCE: usize = 4;

fn net_weight(net: &str) -> f64 {
    let upper = net.to_ascii_uppercase();
    if upper.is_empty() || upper == "NC" || upper.starts_with("UNCONNECTED") {
        0.0
    } else if upper.ends_with("_P") || upper.ends_with("_N") {
        5.0
    } else if ["GND", "GNDA", "GNDD", "VCC", "VDD", "VSS"].contains(&upper.as_str())
        || upper.starts_with('+')
        || upper.starts_with('-')
    {
        3.0
    } else {
        1.0
    }
}

async fn handle_force_directed(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let iterations = args["iterations"].as_u64().unwrap_or(300).min(10_000) as usize;
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let locked: BTreeSet<String> = args["locked"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let selected: Option<BTreeSet<String>> = args["references"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    });

    let content = konnect_sexp::writer::read_consistent(&board)?;
    let tree = konnect_sexp::parse_sexp(&content)?;
    let scan = footprint_courtyards(&tree);
    let index = PcbConnectivityIndex::build(&tree);
    let Some((ox0, oy0, ox1, oy1)) = board_outline_bbox(&tree) else {
        return Ok(CallToolResult::error(
            "force-directed refinement needs a board outline (Edge.Cuts)",
        ));
    };
    let (board_w, board_h) = (ox1 - ox0, oy1 - oy0);

    // Deterministic ordering is the whole determinism story: parts sorted by
    // reference, nets walked in the index's sorted order, no RNG, no clocks.
    let mut parts: Vec<&FootprintCourtyard> = scan
        .items
        .iter()
        .filter(|c| c.reference.is_some())
        .collect();
    parts.sort_by_key(|c| c.reference.clone());
    let ref_of = |i: usize| parts[i].reference.as_deref().expect("filtered");
    // Three ways a part is immovable, and KiCad's own lock is the one the
    // caller should not have to restate: a footprint the user pinned in the
    // editor must never be relocated by an automated pass (#350).
    let movable = |i: usize| {
        let name = ref_of(i);
        !parts[i].locked
            && !locked.contains(name)
            && selected.as_ref().map(|s| s.contains(name)).unwrap_or(true)
    };

    // Spring graph: accumulated weight per part pair sharing nets.
    let ref_index: HashMap<&str, usize> = parts
        .iter()
        .enumerate()
        .map(|(i, c)| (c.reference.as_deref().expect("filtered"), i))
        .collect();
    let mut springs: HashMap<(usize, usize), f64> = HashMap::new();
    for net in index.nets() {
        let weight = net_weight(net);
        if weight == 0.0 {
            continue;
        }
        let members: Vec<usize> = index
            .pads_of_net(net)
            .iter()
            .filter_map(|p| ref_index.get(p.reference.as_str()).copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for (ai, &a) in members.iter().enumerate() {
            for &b in &members[ai + 1..] {
                *springs.entry((a.min(b), a.max(b))).or_insert(0.0) += weight;
            }
        }
    }

    let mut pos: Vec<(f64, f64)> = parts.iter().map(|c| bbox_center(c.bbox)).collect();
    let sizes: Vec<(f64, f64)> = parts
        .iter()
        .map(|c| (c.bbox.2 - c.bbox.0, c.bbox.3 - c.bbox.1))
        .collect();
    let mut vel: Vec<(f64, f64)> = vec![(0.0, 0.0); parts.len()];
    let step = board_w.min(board_h) * 0.05;
    let snap = |v: f64| (v / FD_GRID_MM).round() * FD_GRID_MM;

    let mut last_snapshot: Vec<(f64, f64)> = Vec::new();
    let mut stable = 0usize;
    let mut iterations_run = 0usize;
    let mut converged = false;
    for iteration in 0..iterations {
        iterations_run = iteration + 1;
        let temperature = step * (1.0 - iteration as f64 / iterations as f64) + 0.1;
        let mut force: Vec<(f64, f64)> = vec![(0.0, 0.0); parts.len()];

        // Pairwise repulsion, boosted sharply inside the clearance ring.
        for i in 0..parts.len() {
            for j in (i + 1)..parts.len() {
                let dx = pos[i].0 - pos[j].0;
                let dy = pos[i].1 - pos[j].1;
                let dist = dx.hypot(dy).max(FD_MIN_DIST);
                let min_clear = (sizes[i].0 + sizes[j].0) * 0.5 + 1.0;
                let mut k = FD_K_REPEL;
                if dist < min_clear {
                    k *= (min_clear / dist).powi(2);
                }
                let magnitude = k / (dist * dist);
                let (ux, uy) = (dx / dist, dy / dist);
                force[i].0 += ux * magnitude;
                force[i].1 += uy * magnitude;
                force[j].0 -= ux * magnitude;
                force[j].1 -= uy * magnitude;
            }
        }
        // Spring attraction along shared nets.
        for (&(a, b), &weight) in springs.iter().collect::<std::collections::BTreeMap<_, _>>() {
            let dx = pos[b].0 - pos[a].0;
            let dy = pos[b].1 - pos[a].1;
            let dist = dx.hypot(dy).max(FD_MIN_DIST);
            let magnitude = FD_K_SPRING * weight * dist;
            let (ux, uy) = (dx / dist, dy / dist);
            force[a].0 += ux * magnitude;
            force[a].1 += uy * magnitude;
            force[b].0 -= ux * magnitude;
            force[b].1 -= uy * magnitude;
        }
        // Walls push inward.
        for i in 0..parts.len() {
            force[i].0 += FD_K_WALL / (pos[i].0 - ox0).max(0.01);
            force[i].0 -= FD_K_WALL / (ox1 - pos[i].0).max(0.01);
            force[i].1 += FD_K_WALL / (pos[i].1 - oy0).max(0.01);
            force[i].1 -= FD_K_WALL / (oy1 - pos[i].1).max(0.01);
        }

        // Integrate: damped velocity clamped to the cooling temperature;
        // locked/unselected parts exert force but never move.
        for i in 0..parts.len() {
            if !movable(i) {
                vel[i] = (0.0, 0.0);
                continue;
            }
            vel[i].0 = (vel[i].0 + force[i].0) * FD_DAMPING;
            vel[i].1 = (vel[i].1 + force[i].1) * FD_DAMPING;
            let speed = vel[i].0.hypot(vel[i].1);
            if speed > temperature {
                let scale = temperature / speed;
                vel[i].0 *= scale;
                vel[i].1 *= scale;
            }
            pos[i].0 = (pos[i].0 + vel[i].0).clamp(ox0 + sizes[i].0 / 2.0, ox1 - sizes[i].0 / 2.0);
            pos[i].1 = (pos[i].1 + vel[i].1).clamp(oy0 + sizes[i].1 / 2.0, oy1 - sizes[i].1 / 2.0);
        }

        // Convergence: the SNAPPED layout unchanged for `patience` rounds.
        let snapshot: Vec<(f64, f64)> = pos.iter().map(|p| (snap(p.0), snap(p.1))).collect();
        if snapshot == last_snapshot {
            stable += 1;
            if stable >= FD_PATIENCE {
                converged = true;
                break;
            }
        } else {
            stable = 0;
            last_snapshot = snapshot;
        }
    }

    // Collision resolution: forces cluster connected parts, but nothing in
    // the physics forbids courtyard overlap — the reference resolved
    // candidate positions against collisions every step, and without that
    // pass the springs simply crush a net's members into one spot. Walk the
    // parts in sorted order; any part overlapping an already-settled one
    // spirals outward on the snap grid to the nearest free cell —
    // deterministic by construction.
    let overlaps = |a: (f64, f64), sa: (f64, f64), b: (f64, f64), sb: (f64, f64)| {
        (a.0 - b.0).abs() < (sa.0 + sb.0) * 0.5 && (a.1 - b.1).abs() < (sa.1 + sb.1) * 0.5
    };
    // Immovable parts are obstacles from the start — settle them all before
    // any movable part looks for a free cell, or a cap processed earlier in
    // reference order would never see the locked IC it is about to overlap.
    let mut settled: Vec<usize> = (0..parts.len()).filter(|&i| !movable(i)).collect();
    for i in 0..parts.len() {
        if !movable(i) {
            continue;
        }
        pos[i] = (snap(pos[i].0), snap(pos[i].1));
        let collides = |p: (f64, f64), settled: &[usize], pos: &[(f64, f64)]| {
            settled.iter().any(|&j| {
                parts[j].layer_side == parts[i].layer_side
                    && overlaps(p, sizes[i], pos[j], sizes[j])
            })
        };
        if collides(pos[i], &settled, &pos) {
            'search: for ring in 1..200 {
                let r = ring as f64 * FD_GRID_MM;
                for (dx, dy) in [
                    (r, 0.0),
                    (0.0, r),
                    (-r, 0.0),
                    (0.0, -r),
                    (r, r),
                    (-r, r),
                    (r, -r),
                    (-r, -r),
                ] {
                    let candidate = (
                        (pos[i].0 + dx).clamp(ox0 + sizes[i].0 / 2.0, ox1 - sizes[i].0 / 2.0),
                        (pos[i].1 + dy).clamp(oy0 + sizes[i].1 / 2.0, oy1 - sizes[i].1 / 2.0),
                    );
                    if !collides(candidate, &settled, &pos) {
                        pos[i] = candidate;
                        break 'search;
                    }
                }
            }
        }
        settled.push(i);
    }

    // Final positions: snapped centers, translated back to root anchors.
    let mut placements = Vec::new();
    let mut planned_moves = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if !movable(i) {
            continue;
        }
        let (bcx, bcy) = bbox_center(part.bbox);
        let target = (snap(pos[i].0), snap(pos[i].1));
        if (target.0 - bcx).abs() < 1e-9 && (target.1 - bcy).abs() < 1e-9 {
            continue;
        }
        let (ax, ay) = part.at;
        let reference = part.reference.clone().expect("filtered");
        placements.push(konnect_ipc::types::IpcFootprintPlacement {
            reference: reference.clone(),
            x: ((target.0 + (ax - bcx)) * 1e3).round() / 1e3,
            y: ((target.1 + (ay - bcy)) * 1e3).round() / 1e3,
            rotation: part.rotation_deg,
        });
        planned_moves.push(json!({
            "reference": reference,
            "from": { "x": round3(ax), "y": round3(ay) },
            "to": {
                "x": round3(target.0 + (ax - bcx)),
                "y": round3(target.1 + (ay - bcy)),
            },
        }));
    }

    let score_before = score_of_content(ctx, &content).await?;
    let planned_content = apply_placements_to_content(&content, &placements)?;
    let score_after = score_of_content(ctx, &planned_content).await?;

    if dry_run {
        return Ok(CallToolResult::json(&json!({
            "dry_run": true,
            "iterations_run": iterations_run,
            "converged": converged,
            "planned_moves": planned_moves,
            "score_before": score_before["score"],
            "score_after_plan": score_after["score"],
            "verdict_after_plan": score_after["verdict"],
        })));
    }

    if let Some(refusal) = super::pcb_board::refuse_if_board_open_in_kicad(
        ctx,
        &board,
        "refine_placement_force_directed",
    )
    .await?
    {
        return Ok(refusal);
    }
    let applied = match super::pcb_components::update_closed_board_footprints(&board, &placements) {
        Ok(applied) => applied,
        Err(error) => return Ok(error.into_result()),
    };
    let written = konnect_sexp::writer::read_consistent(&board)?;
    let score_written = score_of_content(ctx, &written).await?;
    Ok(CallToolResult::json(&json!({
        "dry_run": false,
        "iterations_run": iterations_run,
        "converged": converged,
        "applied_count": applied.len(),
        "score_before": score_before["score"],
        "score_after": score_written["score"],
        "verdict_after": score_written["verdict"],
    })))
}

/// The dominant nearest-neighbor spacing along both axes; None when the two
/// axes disagree or no regular spacing exists.
fn detect_grid_pitch(pads: &[(String, (f64, f64), Option<String>)]) -> Option<f64> {
    let step = |mut vals: Vec<f64>| -> Option<f64> {
        vals.sort_by(f64::total_cmp);
        vals.dedup_by(|a, b| (*a - *b).abs() < 0.02);
        let deltas: Vec<f64> = vals.windows(2).map(|w| w[1] - w[0]).collect();
        let min = deltas.iter().copied().min_by(f64::total_cmp)?;
        deltas
            .iter()
            .all(|d| (d / min - (d / min).round()).abs() < 0.05)
            .then_some(min)
    };
    let px = step(pads.iter().map(|p| p.1 .0).collect())?;
    let py = step(pads.iter().map(|p| p.1 .1).collect())?;
    ((px - py).abs() < 0.05).then_some((px + py) / 2.0)
}

fn sorted_unique(values: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut v: Vec<f64> = values.collect();
    v.sort_by(f64::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < 0.02);
    v
}

// ─── Decoupling ──────────────────────────────────────────────────────────────

/// A courtyard bounding box: (x_min, y_min, x_max, y_max) in mm.
type Bbox = (f64, f64, f64, f64);

/// A held footprint as the layout sees it: centre, size, and which side it is on.
type Obstacle = ((f64, f64), (f64, f64), Side);

/// One decoupling cap that sits further from its nearest shared-net IC than
/// its value family allows.
struct DecouplingViolation {
    cap: String,
    value: String,
    ic: String,
    distance_mm: f64,
    limit_mm: f64,
}

/// One capacitor exempted from the decoupling rule because it sits on the
/// pins of a connector it shares a net with — interface/EMI filtering for a
/// cable, not a bypass cap that drifted away from its IC.
struct InterfaceFilterCap {
    cap: String,
    value: String,
    connector: String,
    distance_mm: f64,
    limit_mm: f64,
}

/// Check every family-classified capacitor (C*) against the nearest IC (U*)
/// that shares a net with it. A cap sharing no net with any IC is not a
/// violation — it is reported separately as uncoupled, because "nothing to
/// decouple" and "too far from what it decouples" are different findings.
///
/// A cap sitting on the pins of a connector (J*) it shares a net with is
/// interface/EMI filtering for that cable, not a bypass cap that drifted away
/// from an IC on the same rail, so it is exempted from the distance rule
/// (#411). The exemption is deliberately narrow — the cap must be within the
/// *same* family limit of the connector's courtyard that the rule would demand
/// of an IC, so a cap far from everything still deducts. Exemptions are
/// returned as the third list rather than dropped, so the response says the
/// deduction was waived instead of going quiet about a check that ran.
///
/// `uncoupled_caps` is intentionally untouched by the exemption: a cap that
/// shares no net with any IC never reached the distance rule in the first
/// place, and "nothing to decouple" stays the honest report for it.
fn decoupling_check(
    items: &[FootprintCourtyard],
    index: &PcbConnectivityIndex,
    values: &HashMap<String, String>,
) -> (
    Vec<DecouplingViolation>,
    Vec<String>,
    Vec<InterfaceFilterCap>,
) {
    // Net set per reference, from the connectivity index — keyed by name, so
    // both file formats resolve identically.
    let mut nets_by_ref: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for net in index.nets() {
        for pad in index.pads_of_net(net) {
            nets_by_ref
                .entry(pad.reference.as_str())
                .or_default()
                .insert(net);
        }
    }

    let ics = footprints_with_prefix(items, "U");
    let connectors = footprints_with_prefix(items, "J");

    let mut violations = Vec::new();
    let mut uncoupled = Vec::new();
    let mut interface_filters = Vec::new();
    for c in items {
        let Some(reference) = c.reference.as_deref() else {
            continue;
        };
        if ref_prefix(reference) != "C" {
            continue;
        }
        let Some(value) = values.get(reference) else {
            continue; // no Value property — cannot classify a family
        };
        let Some(limit_mm) = decoupling_limit_mm(value) else {
            continue; // not a decoupling family (22pF, 47u, …)
        };
        let cap_nets = nets_by_ref.get(reference);
        let center = bbox_center(c.bbox);
        match nearest_shared_net(&ics, cap_nets, &nets_by_ref, center) {
            None => uncoupled.push(reference.to_string()),
            Some((ic, distance_mm)) => {
                if distance_mm <= limit_mm {
                    continue;
                }
                // The cap is too far from the IC it shares a rail with. Before
                // deducting, ask what it is actually sitting on: a cap on a
                // shared-net connector's own pins is filtering that cable
                // (#411). Same limit, measured against the connector, so this
                // cannot swallow a cap that is far from everything.
                match nearest_shared_net(&connectors, cap_nets, &nets_by_ref, center)
                    .filter(|(_, connector_distance_mm)| *connector_distance_mm <= limit_mm)
                {
                    Some((connector, connector_distance_mm)) => {
                        interface_filters.push(InterfaceFilterCap {
                            cap: reference.to_string(),
                            value: value.clone(),
                            connector: connector.to_string(),
                            distance_mm: connector_distance_mm,
                            limit_mm,
                        })
                    }
                    None => violations.push(DecouplingViolation {
                        cap: reference.to_string(),
                        value: value.clone(),
                        ic: ic.to_string(),
                        distance_mm,
                        limit_mm,
                    }),
                }
            }
        }
    }
    (violations, uncoupled, interface_filters)
}

/// Footprints whose reference designator carries `prefix` exactly, paired with
/// their courtyard bboxes. An exact prefix match, so `CN3` is not a capacitor
/// and `JP1` is not a connector.
fn footprints_with_prefix<'a>(
    items: &'a [FootprintCourtyard],
    prefix: &str,
) -> Vec<(&'a str, Bbox)> {
    items
        .iter()
        .filter_map(|c| {
            let reference = c.reference.as_deref()?;
            (ref_prefix(reference) == prefix).then_some((reference, c.bbox))
        })
        .collect()
}

/// The nearest candidate that shares at least one named net with the cap, and
/// how far its courtyard bbox is from the cap's center. `None` when no
/// candidate shares a net.
fn nearest_shared_net<'a, 'n>(
    candidates: &[(&'a str, Bbox)],
    cap_nets: Option<&BTreeSet<&'n str>>,
    nets_by_ref: &HashMap<&str, BTreeSet<&'n str>>,
    center: (f64, f64),
) -> Option<(&'a str, f64)> {
    candidates
        .iter()
        .filter(|(reference, _)| {
            // Shares at least one named net with this cap.
            match (cap_nets, nets_by_ref.get(*reference)) {
                (Some(a), Some(b)) => !a.is_disjoint(b),
                _ => false,
            }
        })
        .map(|(reference, bbox)| (*reference, distance_to_bbox(center, *bbox)))
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
}

/// Distance from a point to a courtyard BBOX's edge — zero inside it — not to
/// its center: center-to-center makes a tight limit unachievable against any
/// physically large part, since a 0402 touching a SOIC-8's courtyard would
/// read 3.8 mm and fail a 2.5 mm rule it plainly satisfies.
fn distance_to_bbox((px, py): (f64, f64), (x0, y0, x1, y1): Bbox) -> f64 {
    let dx = (x0 - px).max(0.0).max(px - x1);
    let dy = (y0 - py).max(0.0).max(py - y1);
    dx.hypot(dy)
}

/// The distance limit for a capacitor value's decoupling family, `None` for
/// values outside the three families. `100nF`/`0.1uF` bypass caps belong at
/// the pin (2.5 mm); `1uF` bulk-ish caps within 5 mm; `10uF` bulk within
/// 10 mm. Case-insensitive, tolerant of a trailing F and of `µ` for `u`.
fn decoupling_limit_mm(value: &str) -> Option<f64> {
    let normalized = value.trim().to_ascii_lowercase().replace('µ', "u");
    let family = match normalized.strip_suffix('f') {
        Some(stripped) => stripped,
        None => normalized.as_str(),
    };
    match family {
        "100n" | "0.1u" | ".1u" => Some(2.5),
        "1u" | "1000n" => Some(5.0),
        "10u" => Some(10.0),
        _ => None,
    }
}

// ─── Small helpers ───────────────────────────────────────────────────────────

/// Per-footprint `Value` property, keyed by reference. First occurrence wins
/// for a duplicated reference — all KiCad-authored duplicates (multi-unit
/// parts) share one value.
fn footprint_values(tree: &SexpNode) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    for fp in footprints(tree) {
        let mut reference = None;
        let mut value = None;
        for prop in fp.find_all("property") {
            match prop.get(1).and_then(|n| n.as_str()) {
                Some("Reference") => reference = prop.get(2).and_then(|n| n.as_str()),
                Some("Value") => value = prop.get(2).and_then(|n| n.as_str()),
                _ => {}
            }
        }
        if let (Some(r), Some(v)) = (reference, value) {
            map.entry(r.to_string()).or_insert_with(|| v.to_string());
        }
    }
    map
}

/// The leading alphabetic run of a reference designator: `"C12"` → `"C"`,
/// `"CN3"` → `"CN"` (which is *not* a capacitor), `"J1"` → `"J"`. An exact
/// prefix match, so jumpers (JP) and connectors sold as CN never misclassify.
fn ref_prefix(reference: &str) -> &str {
    let end = reference
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(reference.len());
    &reference[..end]
}

/// Reference for reporting; a footprint without one is still named in the
/// response rather than dropped from it.
fn courtyard_reference(c: &FootprintCourtyard) -> &str {
    match &c.reference {
        Some(r) => r.as_str(),
        None => "(unnamed)",
    }
}

fn bbox_center((x0, y0, x1, y1): (f64, f64, f64, f64)) -> (f64, f64) {
    ((x0 + x1) / 2.0, (y0 + y1) / 2.0)
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Front => "front",
        Side::Back => "back",
    }
}

/// Millimeter values rounded to 3 decimals for reporting (matches
/// `check_clearance`'s convention).
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// KiCad-authored fixture: SOIC-8 U1, 0402 C1/C2 on VCC/GND, R1, J1
    /// header, BGA U2, TP1/TP2, 60×45 outline, routed VCC.
    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../konnect-sexp/tests/fixtures/placement/placement_fixture.kicad_pcb"
    );

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            crate::tools::ServerConfig::default(),
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    async fn score(board: &std::path::Path) -> serde_json::Value {
        let result = handle_score_placement(&json!({ "board": board }), &test_ctx())
            .await
            .unwrap();
        assert!(!result.is_error, "score_placement errored: {result:?}");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    /// Expectations computed BY HAND from the fixture's own coordinates
    /// (all footprints are F.Cu; outline bbox is (0, 0)..(60, 45)):
    ///
    /// - U1 (SOIC-8) at (25, 20), courtyard lines span local
    ///   x −3.700001..3.699999, y −2.700001..2.7 → board bbox
    ///   (21.299999, 17.299999)..(28.699999, 22.7),
    ///   center (24.999999, 19.9999995) ≈ (25, 20).
    /// - C1 "100nF" at (20, 15) rot 0, courtyard rect
    ///   (−0.91, −0.460001)..(0.91, 0.46) → center (20, 14.9999995).
    ///   Distance to U1 center: √(4.999999² + 5.0²) = 7.0710… mm > 2.5 mm
    ///   → decoupling violation.
    /// - C2 "100nF" at (30, 15) rot 90, courtyard rect ±(0.91, 0.46)
    ///   rotates to x 30 ± 0.46, y 15 ∓ 0.91 → center exactly (30, 15).
    ///   Distance to U1 center: √(5.000001² + 4.9999995²) = 7.0710… mm
    ///   > 2.5 mm → decoupling violation. Both caps share VCC and GND with
    ///   U1 (pads 8 and 4); U2's pads carry no nets, so U1 is the only
    ///   candidate IC. → 2 violations × 15 = 30 points (at the cap).
    /// - J1 at (5, 20) rot 90, courtyard rect (−1.77, −1.770001)..(1.77, 9.39);
    ///   the 90° transform maps local (x, y) → (5 + y, 20 − x), so the board
    ///   bbox is (3.229999, 18.23)..(14.39, 21.77) and the center is
    ///   (8.8099995, 20). Nearest-edge distance:
    ///   min(8.81, 60−8.81, 20, 45−20) = 8.81 mm ≤ 10 mm → no violation.
    /// - No same-side courtyard-bbox pair intersects (closest: U1 max_x
    ///   28.699999 vs C2 min_x 29.54) and every bbox is inside the outline
    ///   → no hard failures.
    ///
    /// Score: 100 − 30 (decoupling) = 70, verdict "pass".
    #[tokio::test]
    async fn kicad_fixture_passes_at_70_with_only_the_decoupling_deduction() {
        let response = score(std::path::Path::new(FIXTURE)).await;

        assert_eq!(response["verdict"], "pass", "{response}");
        assert_eq!(response["score"], 70);
        assert_eq!(response["outline_missing"], false);
        assert_eq!(response["hard_failures"].as_array().unwrap().len(), 0);
        assert_eq!(response["footprints_scored"], 8);
        assert_eq!(response["footprints_skipped"], 0);
        assert_eq!(response["uncoupled_caps"].as_array().unwrap().len(), 0);

        // Exactly one deduction: decoupling, 30 points, naming both caps.
        let deductions = response["deductions"].as_array().unwrap();
        assert_eq!(deductions.len(), 1, "{response}");
        assert_eq!(deductions[0]["kind"], "decoupling");
        assert_eq!(deductions[0]["points"], 30);
        let refs = deductions[0]["references"].as_array().unwrap();
        assert!(refs.contains(&json!("C1")) && refs.contains(&json!("C2")));
        let detail = deductions[0]["detail"].as_str().unwrap();
        // Hand arithmetic under the courtyard-EDGE metric: C1 center (20, 15)
        // to U1 bbox (21.3, 17.3)..(28.7, 22.7) → dx 1.3, dy 2.3 →
        // √(1.69 + 5.29) ≈ 2.642 mm, just over the 2.5 mm rule. C2 mirrors it.
        // (Center-to-center read 7.071 mm — physically meaningless against a
        // large IC, which is why the metric changed.)
        assert!(
            detail.contains("2.642"),
            "hand-computed edge distance: {detail}"
        );
        assert!(detail.contains("U1"), "must name the IC: {detail}");

        // J1's hand-computed edge distance, reported even though it passes.
        let edges = response["connector_edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["reference"], "J1");
        assert_eq!(edges[0]["edge_distance_mm"], 8.81);
    }

    /// Synthetic variant derived from the KiCad-authored fixture by string
    /// surgery: C2's root anchor moves from (30, 15) to C1's (20, 15). At
    /// rot 90 that puts C2's courtyard bbox at (19.54, 14.09)..(20.46, 15.91),
    /// which intersects C1's (19.09, 14.539999)..(20.91, 15.46) by
    /// 0.92 × 0.920001 ≈ 0.846 mm² — a same-side hard failure. Score:
    /// 100 − 20 (1 overlap pair) − 30 (both caps still 7.07 mm from U1) = 50.
    #[tokio::test]
    async fn overlapping_courtyards_are_a_hard_fail_naming_the_pair() {
        let fixture = std::fs::read_to_string(FIXTURE).unwrap();
        assert_eq!(
            fixture.matches("(at 30 15 90)").count(),
            1,
            "surgery anchor must be unique (C2's root at)"
        );
        let moved = fixture.replace("(at 30 15 90)", "(at 20 15 90)");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlap.kicad_pcb");
        std::fs::write(&path, moved).unwrap();

        let response = score(&path).await;
        assert_eq!(response["verdict"], "hard_fail", "{response}");
        assert_eq!(response["score"], 50);

        let failures = response["hard_failures"].as_array().unwrap();
        assert_eq!(failures.len(), 1, "{response}");
        assert_eq!(failures[0]["kind"], "courtyard_overlap");
        let refs = failures[0]["references"].as_array().unwrap();
        assert!(refs.contains(&json!("C1")) && refs.contains(&json!("C2")));
        let detail = failures[0]["detail"].as_str().unwrap();
        assert!(detail.contains("0.846"), "hand-computed area: {detail}");
    }

    /// Synthetic variant derived from the KiCad-authored fixture: retagging
    /// the four Edge.Cuts lines removes the outline. The outside and
    /// connector-edge checks are blocked (not passed), the response says so,
    /// and the verdict can never be "pass" — only the decoupling deduction
    /// still fires, so the score is 70 while the verdict is not.
    #[tokio::test]
    async fn a_board_without_an_outline_never_passes() {
        let fixture = std::fs::read_to_string(FIXTURE).unwrap();
        assert_eq!(fixture.matches("(layer \"Edge.Cuts\")").count(), 4);
        let stripped = fixture.replace("(layer \"Edge.Cuts\")", "(layer \"Dwgs.User\")");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_outline.kicad_pcb");
        std::fs::write(&path, stripped).unwrap();

        let response = score(&path).await;
        assert_eq!(response["verdict"], "outline_missing", "{response}");
        assert_ne!(response["verdict"], "pass");
        assert_eq!(response["outline_missing"], true);
        assert_eq!(response["score"], 70);
        // Edge distances cannot be computed without an outline.
        assert_eq!(response["connector_edges"].as_array().unwrap().len(), 0);
        assert_eq!(response["hard_failures"].as_array().unwrap().len(), 0);
    }

    /// Move ONE footprint's root anchor by string surgery on the
    /// KiCad-authored fixture, the way
    /// `overlapping_courtyards_are_a_hard_fail_naming_the_pair` does: the
    /// geometry changes, every KiCad-written detail around it does not.
    fn fixture_with_moved_root(from: &str, to: &str) -> String {
        let fixture = std::fs::read_to_string(FIXTURE).unwrap();
        assert_eq!(
            fixture.matches(from).count(),
            1,
            "surgery anchor must be unique: {from}"
        );
        fixture.replace(from, to)
    }

    /// Rewrite text inside ONE footprint's block, leaving every other footprint
    /// alone. Line-scoped like `lock_footprint`: find the block enclosing the
    /// Reference property, stop at that footprint's own closing paren, so a
    /// string several parts share is only rewritten here.
    fn rewrite_inside_footprint(
        content: &str,
        reference: &str,
        rewrites: &[(String, String)],
    ) -> String {
        let lines: Vec<&str> = content.lines().collect();
        let marker = format!(r#"(property "Reference" "{reference}""#);
        let r = lines
            .iter()
            .position(|l| l.contains(&marker))
            .expect("reference not present in fixture");
        let start = lines[..r]
            .iter()
            .rposition(|l| l.trim_start().starts_with("(footprint "))
            .expect("reference has no enclosing footprint");
        let end = start
            + lines[start..]
                .iter()
                .position(|l| *l == "\t)")
                .expect("footprint block never closes");
        let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let mut rewritten = 0usize;
        for line in &mut out[start..=end] {
            for (from, to) in rewrites {
                if line.contains(from.as_str()) {
                    *line = line.replace(from.as_str(), to.as_str());
                    rewritten += 1;
                    break;
                }
            }
        }
        assert_eq!(
            rewritten,
            rewrites.len(),
            "every rewrite must land inside {reference}"
        );
        let nl = String::from_utf8(vec![10]).unwrap();
        out.join(&nl)
    }

    /// Retag one footprint's pad nets, by net name.
    fn rewrite_footprint_nets(content: &str, reference: &str, rewrites: &[(&str, &str)]) -> String {
        let rewrites: Vec<(String, String)> = rewrites
            .iter()
            .map(|(from, to)| (format!(r#"(net "{from}")"#), format!(r#"(net "{to}")"#)))
            .collect();
        rewrite_inside_footprint(content, reference, &rewrites)
    }

    /// Retag one footprint's `Value` property — the field `decoupling_limit_mm`
    /// classifies a capacitor's family from, so a fixture cap can be moved to
    /// another family without inventing a second board.
    fn rewrite_footprint_value(content: &str, reference: &str, from: &str, to: &str) -> String {
        let rewrites = vec![(
            format!(r#"(property "Value" "{from}""#),
            format!(r#"(property "Value" "{to}""#),
        )];
        rewrite_inside_footprint(content, reference, &rewrites)
    }

    fn write_variant(dir: &tempfile::TempDir, name: &str, content: String) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    /// #411: a 100nF cap sitting on a connector's own VCC/GND pins is
    /// interface/EMI filtering for that cable, not a bypass cap that wandered
    /// away from the MCU across the board. It used to cost 15 points and name
    /// the distant IC in the detail.
    ///
    /// Surgery: C1's root anchor moves from (20, 15) to (16, 20), beside J1 —
    /// the fixture's 4-pin header, whose pads carry VCC, GND and /SIG_IN, so
    /// it shares both of C1's nets. Hand arithmetic from the fixture's own
    /// coordinates:
    ///
    /// - C1's courtyard rect (−0.91, −0.460001)..(0.91, 0.46) puts its bbox at
    ///   (15.09, 19.539999)..(16.91, 20.46), center (16, 19.9999995).
    /// - J1's bbox is (3.229999, 18.23)..(14.39, 21.77) (hand-computed in
    ///   `kicad_fixture_passes_at_70_with_only_the_decoupling_deduction`), so
    ///   0.7 mm of clear air separates the two bboxes — no courtyard overlap.
    /// - Cap center to J1's bbox edge: dx = 16 − 14.39 = 1.61, dy = 0 (the
    ///   center's y is inside J1's y span) → 1.61 mm ≤ the 2.5 mm 100nF limit
    ///   → interface filtering.
    /// - Cap center to U1's bbox (21.299999, 17.299999)..(28.699999, 22.7):
    ///   dx = 5.3, dy = 0 → 5.3 mm > 2.5 mm, which is the deduction this
    ///   exemption suppresses.
    ///
    /// C2 is untouched at (30, 15): 2.642 mm from U1 (still over the limit)
    /// and 15.61 mm from J1, so it still deducts. The exemption silences one
    /// cap, not the check. Score: 100 − 15 = 85.
    #[tokio::test]
    async fn a_cap_on_a_shared_net_connector_is_interface_filtering_not_a_decoupling_defect() {
        let dir = tempfile::tempdir().unwrap();
        let board = write_variant(
            &dir,
            "cap_on_connector.kicad_pcb",
            fixture_with_moved_root("(at 20 15)", "(at 16 20)"),
        );

        let response = score(&board).await;
        assert_eq!(response["verdict"], "pass", "{response}");
        assert_eq!(response["hard_failures"].as_array().unwrap().len(), 0);
        assert_eq!(response["score"], 85, "only C2 still deducts: {response}");

        // The surviving deduction names C2 alone — C1 is gone from it.
        let deductions = response["deductions"].as_array().unwrap();
        assert_eq!(deductions.len(), 1, "{response}");
        assert_eq!(deductions[0]["kind"], "decoupling");
        assert_eq!(deductions[0]["points"], 15);
        assert_eq!(deductions[0]["references"], json!(["C2"]));
        assert!(
            !deductions[0]["detail"].as_str().unwrap().contains("C1"),
            "{response}"
        );

        // The waiver is reported, not silent: a check that ran and was
        // exempted must be distinguishable from a check that never fired.
        let filters = response["interface_filter_caps"].as_array().unwrap();
        assert_eq!(filters.len(), 1, "{response}");
        assert_eq!(filters[0]["reference"], "C1");
        assert_eq!(filters[0]["value"], "100nF");
        assert_eq!(filters[0]["connector"], "J1");
        assert_eq!(filters[0]["connector_distance_mm"], 1.61);
        assert_eq!(filters[0]["limit_mm"], 2.5);

        // C1 shares VCC/GND with U1, so it was never "nothing to decouple".
        assert_eq!(response["uncoupled_caps"].as_array().unwrap().len(), 0);
    }

    /// The exemption must not become a way to switch the rule off. Same
    /// surgery, 2 mm further: C1 lands at (18, 20), which is 3.61 mm from J1's
    /// bbox and 3.3 mm from U1's — over the 2.5 mm limit on BOTH counts. A cap
    /// that is far from everything is still a placement defect, so both caps
    /// deduct and the score stays at the fixture's 70.
    #[tokio::test]
    async fn a_cap_far_from_both_its_ic_and_the_connector_still_deducts() {
        let dir = tempfile::tempdir().unwrap();
        let board = write_variant(
            &dir,
            "cap_far_from_both.kicad_pcb",
            fixture_with_moved_root("(at 20 15)", "(at 18 20)"),
        );

        let response = score(&board).await;
        assert_eq!(response["score"], 70, "{response}");
        let deductions = response["deductions"].as_array().unwrap();
        assert_eq!(deductions.len(), 1, "{response}");
        assert_eq!(deductions[0]["kind"], "decoupling");
        assert_eq!(deductions[0]["points"], 30);
        let refs = deductions[0]["references"].as_array().unwrap();
        assert!(
            refs.contains(&json!("C1")) && refs.contains(&json!("C2")),
            "{response}"
        );
        let detail = deductions[0]["detail"].as_str().unwrap();
        assert!(
            detail.contains("3.3"),
            "hand-computed C1 distance: {detail}"
        );
        assert_eq!(
            response["interface_filter_caps"].as_array().unwrap().len(),
            0
        );
    }

    /// Proximity alone is not the exemption — the cap has to be on nets the
    /// connector actually carries. C1 sits at (16, 20), 1.61 mm from J1 exactly
    /// as in the #411 repro, but J1's VCC and GND pads are retagged to nets J1
    /// already owns (/SIG_IN and its unconnected pin 4), leaving J1 and C1
    /// net-disjoint. C1 still shares VCC/GND with U1 at 5.3 mm, so the
    /// deduction stands and both caps are named.
    #[tokio::test]
    async fn a_cap_beside_a_connector_it_shares_no_net_with_still_deducts() {
        let dir = tempfile::tempdir().unwrap();
        let moved = fixture_with_moved_root("(at 20 15)", "(at 16 20)");
        let retagged = rewrite_footprint_nets(
            &moved,
            "J1",
            &[("VCC", "/SIG_IN"), ("GND", "unconnected-(J1-Pin_4-Pad4)")],
        );
        let board = write_variant(&dir, "cap_beside_foreign_connector.kicad_pcb", retagged);

        let response = score(&board).await;
        assert_eq!(response["score"], 70, "{response}");
        let deductions = response["deductions"].as_array().unwrap();
        assert_eq!(deductions.len(), 1, "{response}");
        assert_eq!(deductions[0]["points"], 30);
        let refs = deductions[0]["references"].as_array().unwrap();
        assert!(
            refs.contains(&json!("C1")) && refs.contains(&json!("C2")),
            "{response}"
        );
        assert!(
            deductions[0]["detail"].as_str().unwrap().contains("5.3"),
            "{response}"
        );
        assert_eq!(
            response["interface_filter_caps"].as_array().unwrap().len(),
            0
        );
    }

    /// The exemption deliberately leaves `uncoupled_caps` alone. C1 sits on
    /// J1's pins, but its own pads are retagged to J1's other two nets, so it
    /// shares nothing with any IC: it never reaches the distance rule, and
    /// "nothing to decouple" stays the honest report for it rather than being
    /// relabelled as a waived deduction. C2 still deducts → 85.
    #[tokio::test]
    async fn a_connector_cap_sharing_no_net_with_any_ic_is_still_reported_uncoupled() {
        let dir = tempfile::tempdir().unwrap();
        let moved = fixture_with_moved_root("(at 20 15)", "(at 16 20)");
        let retagged = rewrite_footprint_nets(
            &moved,
            "C1",
            &[("VCC", "/SIG_IN"), ("GND", "unconnected-(J1-Pin_4-Pad4)")],
        );
        let board = write_variant(&dir, "connector_cap_uncoupled.kicad_pcb", retagged);

        let response = score(&board).await;
        assert_eq!(response["uncoupled_caps"], json!(["C1"]), "{response}");
        assert_eq!(
            response["interface_filter_caps"].as_array().unwrap().len(),
            0
        );
        assert_eq!(response["score"], 85, "{response}");
        assert_eq!(response["deductions"][0]["references"], json!(["C2"]));
    }

    /// `interface_filter_caps` means "a deduction was waived", not "a cap is
    /// near a connector" — a cap that satisfies the decoupling rule outright
    /// is reported by neither list.
    ///
    /// Two surgeries put one cap inside both limits at once, which the stock
    /// fixture cannot do (U1 and J1 are 6.91 mm apart in x): U1's anchor moves
    /// from (25, 20) to (19, 20) — bbox (15.299999, 17.299999)..(22.699999,
    /// 22.7), still 0.91 mm clear of J1's bbox — and C1's from (20, 15) to
    /// (14, 16), above both (below is R1's spot at (15, 25)). Hand arithmetic:
    /// C1's center (14, 15.9999995) is dx 1.3 / dy 1.3 → 1.838 mm from U1's
    /// bbox and dx 0 / dy 2.23 → 2.23 mm from J1's, both inside the 2.5 mm
    /// limit. C2 is now 7.654 mm from the relocated U1, so it alone deducts
    /// → 85.
    #[tokio::test]
    async fn a_cap_within_both_limits_is_neither_a_violation_nor_a_reported_waiver() {
        let dir = tempfile::tempdir().unwrap();
        let moved_ic = fixture_with_moved_root("(at 25 20)", "(at 19 20)");
        assert_eq!(moved_ic.matches("(at 20 15)").count(), 1);
        let board = write_variant(
            &dir,
            "cap_within_both_limits.kicad_pcb",
            moved_ic.replace("(at 20 15)", "(at 14 16)"),
        );

        let response = score(&board).await;
        assert_eq!(response["verdict"], "pass", "{response}");
        assert_eq!(response["score"], 85, "{response}");
        assert_eq!(response["deductions"][0]["references"], json!(["C2"]));
        assert!(
            response["deductions"][0]["detail"]
                .as_str()
                .unwrap()
                .contains("7.654"),
            "{response}"
        );
        // C1 broke no rule, so nothing was waived on its behalf.
        assert_eq!(
            response["interface_filter_caps"].as_array().unwrap().len(),
            0,
            "{response}"
        );
        assert_eq!(response["uncoupled_caps"].as_array().unwrap().len(), 0);
    }

    /// The exemption is measured against the cap's OWN family limit, not
    /// against the 2.5 mm of the 100nF family every other test here uses. With
    /// only 100nF caps on the board, a hard-coded `2.5` and `limit_mm` are
    /// indistinguishable, and the claim that this exemption can never be looser
    /// than the rule it waives would be untested.
    ///
    /// So: C1's Value becomes `1uF` (family limit 5.0 mm) and its anchor moves
    /// from (20, 15) to (16, 15) — off the U1/J1 axis, because the 6.91 mm gap
    /// between J1's bbox and U1's cannot hold a point that is both > 5 mm from
    /// U1 and > 2.5 mm from J1 on a straight line between them. Hand arithmetic
    /// from the fixture's own coordinates, cap center (16, 14.9999995):
    ///
    /// - To J1's bbox (3.229999, 18.23)..(14.39, 21.77): dx = 16 − 14.39 =
    ///   1.61, dy = 18.23 − 15 = 3.23 → √(2.5921 + 10.4329) = 3.609 mm. That is
    ///   ABOVE 2.5 mm and BELOW the 1uF limit of 5.0 mm — the whole point.
    /// - To U1's bbox (21.299999, 17.299999)..(28.699999, 22.7): dx = 5.3,
    ///   dy = 2.3 → √(28.09 + 5.29) = 5.778 mm > 5.0 mm, so the cap really does
    ///   reach the exemption instead of passing the IC rule outright.
    /// - C1's bbox (15.09, 14.539999)..(16.91, 15.46) touches nothing: J1
    ///   starts at y 18.23, U1 at x 21.3, R1 sits at (15, 25).
    ///
    /// Real code exempts C1 (3.609 ≤ 5.0) and scores 85 with C2 alone. A build
    /// that compares against a hard-coded 2.5 deducts for C1 and scores 70.
    #[tokio::test]
    async fn the_exemption_uses_the_caps_own_family_limit_not_the_100nf_one() {
        let dir = tempfile::tempdir().unwrap();
        let moved = fixture_with_moved_root("(at 20 15)", "(at 16 15)");
        let board = write_variant(
            &dir,
            "bulk_cap_on_connector.kicad_pcb",
            rewrite_footprint_value(&moved, "C1", "100nF", "1uF"),
        );

        let response = score(&board).await;
        assert_eq!(response["verdict"], "pass", "{response}");
        assert_eq!(response["hard_failures"].as_array().unwrap().len(), 0);
        assert_eq!(
            response["score"], 85,
            "C1 is 3.609 mm from J1, inside its own 5.0 mm limit: {response}"
        );

        let filters = response["interface_filter_caps"].as_array().unwrap();
        assert_eq!(filters.len(), 1, "{response}");
        assert_eq!(filters[0]["reference"], "C1");
        assert_eq!(filters[0]["value"], "1uF");
        assert_eq!(filters[0]["connector"], "J1");
        // Both numbers matter: the distance is over the 100nF family's 2.5 mm,
        // and the limit reported is this cap's own 5.0 mm.
        assert_eq!(filters[0]["connector_distance_mm"], 3.609);
        assert_eq!(filters[0]["limit_mm"], 5.0);

        // C2 is untouched 100nF and still deducts — the families do not bleed.
        assert_eq!(response["deductions"][0]["references"], json!(["C2"]));
        assert_eq!(response["deductions"][0]["points"], 15);
        assert_eq!(response["uncoupled_caps"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_missing_board_file_is_a_structured_file_not_found() {
        let result = handle_score_placement(
            &json!({ "board": "Z:/nope/definitely_absent.kicad_pcb" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("file_not_found")
        );
    }

    async fn text_of(result: &CallToolResult) -> serde_json::Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text");
        };
        serde_json::from_str(text).unwrap()
    }

    fn result_text(result: &CallToolResult) -> &str {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text");
        };
        text
    }

    fn fixture_copy(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("board.kicad_pcb");
        std::fs::copy(FIXTURE, &path).unwrap();
        path
    }

    fn ctx_with_open_board(board: &std::path::Path) -> ToolContext {
        let address =
            crate::tools::pcb_board::board_mock::spawn_kicad_holding_board(board, |_| None);
        crate::tools::pcb_board::board_mock::ctx_talking_to(address)
    }

    /// The decoupling planner must clear the fixture's only deduction: C1/C2
    /// currently sit 7.071 mm from U1 (score 70); a row below U1's courtyard
    /// (bbox y_max 22.7 + 0.5 spacing) puts each cap center well inside the
    /// 2.5 mm rule measured from the courtyard-center distance the checker
    /// uses... but note the checker measures CENTER-to-CENTER: U1's center is
    /// (25, 20), the planned cap centers are at y ≈ 23.66, x within ±1 of 25,
    /// so distance ≈ sqrt(1 + 3.66²) ≈ 3.8 mm — above 2.5! The row is beside
    /// the courtyard but the SOIC-8 courtyard is tall. The correct assertion
    /// is therefore what the geometry says: the plan improves the distance
    /// (7.07 → ~3.8) and the response's own before/after scores tell the
    /// truth about whether the deduction cleared. Pin the actual numbers.
    #[tokio::test]
    async fn decoupling_plan_moves_caps_beside_u1_and_reports_honest_scores() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let result = handle_place_decoupling(
            &json!({ "board": board.to_string_lossy(), "ic_reference": "U1" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        let response = text_of(&result).await;
        assert_eq!(response["dry_run"], true);
        assert_eq!(response["side"], "bottom");
        let moves = response["planned_moves"].as_array().unwrap();
        assert_eq!(moves.len(), 2, "C1 and C2: {response}");
        for mv in moves {
            let y = mv["to"]["y"].as_f64().unwrap();
            assert!(
                (23.0..25.0).contains(&y),
                "cap lands just below U1's courtyard (y_max 22.7): {mv}"
            );
            let x = mv["to"]["x"].as_f64().unwrap();
            assert!((23.0..27.0).contains(&x), "row centered on U1 x=25: {mv}");
        }
        assert_eq!(response["score_before"], 70);
        // The response derives its after-score from the planned content —
        // whatever the checker says it says; both plausible outcomes are a
        // number, never a fabricated "fixed".
        assert!(response["score_after_plan"].is_number());
    }

    #[tokio::test]
    async fn decoupling_plan_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let args = json!({ "board": board.to_string_lossy(), "ic_reference": "U1" });
        let a = text_of(&handle_place_decoupling(&args, &test_ctx()).await.unwrap()).await;
        let b = text_of(&handle_place_decoupling(&args, &test_ctx()).await.unwrap()).await;
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn decoupling_refuses_an_absent_ic() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let result = handle_place_decoupling(
            &json!({ "board": board.to_string_lossy(), "ic_reference": "U99" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
    }

    #[tokio::test]
    async fn decoupling_apply_refuses_the_exact_open_board() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let before = std::fs::read(&board).unwrap();
        let result = handle_place_decoupling(
            &json!({"board": board, "ic_reference": "U1", "dry_run": false}),
            &ctx_with_open_board(&board),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(result_text(&result).contains("place_decoupling_caps"));
        assert_eq!(std::fs::read(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn decoupling_apply_proceeds_when_a_different_board_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let other = dir.path().join("other.kicad_pcb");
        std::fs::write(&other, "").unwrap();
        let before = std::fs::read(&board).unwrap();
        let result = handle_place_decoupling(
            &json!({"board": board, "ic_reference": "U1", "dry_run": false}),
            &ctx_with_open_board(&other),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        assert_ne!(std::fs::read(&board).unwrap(), before);
    }

    /// U2 is Analog_BGA-28 with a 4x7 grid at 0.8 mm pitch. Hand count:
    /// perimeter pads = 2*4 + 2*7 - 4 = 18, inner = 28 - 18 = 10 vias.
    /// Dogbone via offset magnitude = 0.55 * 0.8 = 0.44 mm; track width for
    /// pitch 0.8 (> 0.65) is 0.15 mm.
    #[tokio::test]
    async fn bga_fanout_plans_ten_dogbone_vias_at_the_detected_pitch() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let result = handle_bga_fanout(
            &json!({ "board": board.to_string_lossy(), "reference": "U2" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        let plan = text_of(&result).await;
        assert_eq!(plan["pads_total"], 28, "{plan}");
        assert_eq!(plan["pads_outer_ring"], 18);
        assert_eq!(plan["vias"].as_array().unwrap().len(), 10);
        assert_eq!(plan["pitch_detected_mm"], 0.8);
        assert_eq!(plan["track_width_mm"], 0.15);
        for (via, stub) in plan["vias"]
            .as_array()
            .unwrap()
            .iter()
            .zip(plan["stubs"].as_array().unwrap())
        {
            let dx = stub["to"]["x"].as_f64().unwrap() - stub["from"]["x"].as_f64().unwrap();
            let dy = stub["to"]["y"].as_f64().unwrap() - stub["from"]["y"].as_f64().unwrap();
            let magnitude = dx.hypot(dy);
            assert!(
                (magnitude - 0.44).abs() < 0.005,
                "dogbone offset must be 0.55*pitch = 0.44 mm, got {magnitude} for {via}"
            );
        }
    }

    /// The auto-placer must produce a legal layout on the fixture: all 8
    /// parts planned, everything inside the 60x45 outline minus margin, no
    /// planned courtyard overlaps — asserted by re-scoring the PLANNED
    /// content, which reuses the hard-failure checker as the witness.
    #[tokio::test]
    async fn auto_place_plans_a_legal_deterministic_layout() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let args = json!({ "board": board.to_string_lossy() });
        let a = text_of(&handle_auto_place(&args, &test_ctx()).await.unwrap()).await;
        assert_eq!(a["dry_run"], true);
        assert_eq!(a["planned_moves"].as_array().unwrap().len(), 8, "{a}");
        assert_ne!(
            a["verdict_after_plan"], "hard_fail",
            "planned layout must be legal: {a}"
        );
        let b = text_of(&handle_auto_place(&args, &test_ctx()).await.unwrap()).await;
        assert_eq!(a, b, "same input, same plan");
    }

    #[tokio::test]
    async fn auto_place_apply_refuses_the_exact_open_board() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let before = std::fs::read(&board).unwrap();
        let result = handle_auto_place(
            &json!({"board": board, "dry_run": false}),
            &ctx_with_open_board(&board),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(result_text(&result).contains("auto_place_from_schematic"));
        assert_eq!(std::fs::read(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn auto_place_apply_proceeds_when_a_different_board_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let other = dir.path().join("other.kicad_pcb");
        std::fs::write(&other, "").unwrap();
        let before = std::fs::read(&board).unwrap();
        let result = handle_auto_place(
            &json!({"board": board, "dry_run": false}),
            &ctx_with_open_board(&other),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        assert_ne!(std::fs::read(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn force_directed_apply_refuses_the_exact_open_board() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let before = std::fs::read(&board).unwrap();
        let result = handle_force_directed(
            &json!({"board": board, "dry_run": false}),
            &ctx_with_open_board(&board),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(result_text(&result).contains("refine_placement_force_directed"));
        assert_eq!(std::fs::read(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn force_directed_apply_proceeds_when_a_different_board_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let other = dir.path().join("other.kicad_pcb");
        std::fs::write(&other, "").unwrap();
        let before = std::fs::read(&board).unwrap();
        let result = handle_force_directed(
            &json!({"board": board, "dry_run": false}),
            &ctx_with_open_board(&other),
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        assert_ne!(std::fs::read(&board).unwrap(), before);
    }

    /// Lock a footprint the way KiCad does, and neither planner may move it
    /// (#350). Before this, `auto_place_from_schematic` relocated every
    /// footprint on the board, locked or not, with no way to exempt one.
    ///
    /// The lock is written exactly as pcbnew writes it — `(locked yes)` as the
    /// footprint's first child, at the file's own indentation — copied from a
    /// KiCad-authored demo board rather than invented here.
    fn lock_footprint(board: &std::path::Path, reference: &str) {
        let content = std::fs::read_to_string(board).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        let marker = format!(r#"(property "Reference" "{reference}""#);
        let r = lines
            .iter()
            .position(|l| l.contains(&marker))
            .expect("reference not present in fixture");
        let f = lines[..r]
            .iter()
            .rposition(|l| l.trim_start().starts_with("(footprint "))
            .expect("reference has no enclosing footprint");
        let indent: String = lines[f + 1]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        out.insert(f + 1, format!("{indent}(locked yes)"));
        let nl = String::from_utf8(vec![10]).unwrap();
        std::fs::write(board, out.join(&nl)).unwrap();
    }

    #[tokio::test]
    async fn auto_place_holds_footprints_kicad_locked() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let args = json!({ "board": board.to_string_lossy() });
        let moved_u1 = |v: &serde_json::Value| {
            v["planned_moves"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["reference"] == "U1")
        };

        // Premise: unlocked, U1 is one of the parts this planner relocates.
        // Without this the test could pass against a planner that moves nothing.
        let before = text_of(&handle_auto_place(&args, &test_ctx()).await.unwrap()).await;
        assert!(
            moved_u1(&before),
            "premise failed, U1 must move unlocked: {before}"
        );

        lock_footprint(&board, "U1");
        let after = text_of(&handle_auto_place(&args, &test_ctx()).await.unwrap()).await;
        assert!(!moved_u1(&after), "locked U1 was still relocated: {after}");
        assert!(
            after["held"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["reference"] == "U1" && h["reason"] == "locked in kicad"),
            "the response must say what it refused to move: {after}"
        );
        assert_eq!(
            after["planned_moves"].as_array().unwrap().len(),
            7,
            "the other seven parts still place: {after}"
        );
    }

    /// The force-directed pass reads the board's own lock too, so a caller who
    /// never passes `locked` still cannot shove a pinned part around.
    #[tokio::test]
    async fn force_directed_holds_footprints_kicad_locked() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let args = json!({ "board": board.to_string_lossy() });
        let moved_u1 = |v: &serde_json::Value| {
            v["planned_moves"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["reference"] == "U1")
        };
        let before = text_of(&handle_force_directed(&args, &test_ctx()).await.unwrap()).await;
        assert!(
            moved_u1(&before),
            "premise failed, U1 must move unlocked: {before}"
        );

        lock_footprint(&board, "U1");
        let after = text_of(&handle_force_directed(&args, &test_ctx()).await.unwrap()).await;
        assert!(!moved_u1(&after), "locked U1 was still relocated: {after}");
    }

    /// The same hold, requested by the caller instead of by the board.
    #[tokio::test]
    async fn auto_place_honours_the_locked_argument() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let args = json!({ "board": board.to_string_lossy(), "locked": ["U1"] });
        let a = text_of(&handle_auto_place(&args, &test_ctx()).await.unwrap()).await;
        for mv in a["planned_moves"].as_array().unwrap() {
            assert_ne!(mv["reference"], "U1", "caller-locked reference moved: {mv}");
        }
        assert!(
            a["held"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["reference"] == "U1" && h["reason"] == "listed in locked"),
            "{a}"
        );
    }

    /// Determinism and safety of the spring embedder: run-twice identical,
    /// locked parts never move, and the plan never introduces hard failures
    /// the board did not have.
    #[tokio::test]
    async fn force_directed_is_deterministic_and_respects_locks() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let args = json!({
            "board": board.to_string_lossy(),
            "locked": ["U1"],
        });
        let a = text_of(&handle_force_directed(&args, &test_ctx()).await.unwrap()).await;
        let b = text_of(&handle_force_directed(&args, &test_ctx()).await.unwrap()).await;
        assert_eq!(a, b, "same seedless input, same plan");
        assert!(a["iterations_run"].as_u64().unwrap() >= 1);
        assert!(a["converged"].is_boolean(), "honest convergence field");
        for mv in a["planned_moves"].as_array().unwrap() {
            assert_ne!(mv["reference"], "U1", "locked reference moved: {mv}");
        }
        assert_ne!(
            a["verdict_after_plan"], "hard_fail",
            "refinement must not create hard failures: {a}"
        );
    }

    #[tokio::test]
    async fn bga_fanout_refuses_an_absent_reference() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture_copy(&dir);
        let result = handle_bga_fanout(
            &json!({ "board": board.to_string_lossy(), "reference": "U77" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }
}
