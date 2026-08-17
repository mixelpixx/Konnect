use crate::mcp::protocol::{CallToolResult, ToolContent};
use crate::tool;
use crate::tools::{
    pcb_board::{attempt_ipc_write, BoardWrite},
    ToolContext, ToolDef,
};
use anyhow::{bail, Context, Result};
use konnect_ipc::gen::kiapi;
use prost::Message;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone)]
struct LibraryFootprint {
    library_id: String,
    definition: kiapi::board::types::Footprint,
    attributes: kiapi::board::types::FootprintAttributes,
    datasheet: Option<String>,
    description_field: Option<String>,
    pads: Vec<konnect_ipc::IpcPadDefinition>,
    graphics: Vec<konnect_ipc::IpcGraphicDefinition>,
    models: Vec<kiapi::board::types::Footprint3DModel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChangedDomain {
    Pads,
    Graphics,
    Attributes,
    Metadata,
    Models,
}

#[derive(Debug)]
struct PreparedUpdate {
    item: prost_types::Any,
    changed_domains: BTreeSet<ChangedDomain>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct UpdateFilters {
    references: Option<BTreeSet<String>>,
    library_ids: Option<BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArgumentError {
    field: String,
    reason: String,
}

impl std::fmt::Display for ArgumentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for ArgumentError {}

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
struct UpdateCoverage {
    selected: CountPair,
    changed: CountPair,
    unchanged: CountPair,
    skipped_unlinked: CountPair,
    conflicts: CountPair,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PreservedState {
    position: bool,
    rotation: bool,
    layer: bool,
    locked: bool,
    kiid: bool,
    symbol_path: bool,
    pad_nets: bool,
    instance_overrides: bool,
}

impl Default for PreservedState {
    fn default() -> Self {
        Self {
            position: true,
            rotation: true,
            layer: true,
            locked: true,
            kiid: true,
            symbol_path: true,
            pad_nets: true,
            instance_overrides: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PlannedUpdate {
    reference: String,
    library_id: String,
    changed_domains: BTreeSet<ChangedDomain>,
    preserved: PreservedState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct UpdateDiagnostic {
    code: String,
    message: String,
    reference: Option<String>,
}

#[derive(Debug)]
struct UpdatePlan {
    status: PlanStatus,
    plan_revision: String,
    coverage: UpdateCoverage,
    changes: Vec<PlannedUpdate>,
    diagnostics: Vec<UpdateDiagnostic>,
    prepared_items: Vec<prost_types::Any>,
}

pub(crate) fn tool() -> ToolDef {
    tool!(
        "update_footprints_from_library",
        "Plan or atomically apply KiCad's Update Footprints from Library operation to placed \
         footprints on the live board. Defaults to a non-mutating dry run; apply requires its \
         exact plan revision. Preserves placed-instance state and pad nets while refreshing \
         supported library-owned content.",
        json!({
            "type": "object",
            "properties": {
                "board": {
                    "type": "string",
                    "description": "Open .kicad_pcb path whose placed footprints will be refreshed"
                },
                "references": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional exact reference allowlist; omitted means all eligible footprints"
                },
                "library_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional exact Library:Footprint allowlist"
                },
                "dry_run": {
                    "type": "boolean",
                    "default": true,
                    "description": "Plan and report changes without mutating the board"
                },
                "expected_plan_revision": {
                    "type": "string",
                    "description": "Required for apply; exact revision returned by the reviewed dry run"
                }
            },
            "required": ["board"],
            "additionalProperties": false
        }),
        |args, ctx| async move { handle_update_footprints_from_library(args, ctx).await }
    )
}

pub(crate) async fn handle_update_footprints_from_library(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> Result<CallToolResult> {
    use kiapi::common::types::KiCadObjectType as ObjectType;

    let board_path = crate::tools::get_path(args, "board")?;
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
    if !board_path.is_file() {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::FileNotFound {
                path: board_path.display().to_string(),
            },
            format!("Board file not found: {}", board_path.display()),
        ));
    }
    let filters = match parse_filters(args) {
        Ok(filters) => filters,
        Err(error) => {
            return Ok(CallToolResult::error_kind(
                crate::mcp::error::ToolErrorKind::InvalidArgument {
                    field: error.field.clone(),
                    reason: error.reason.clone(),
                },
                format!("Argument '{}' is invalid: {}", error.field, error.reason),
            ))
        }
    };

    let ipc_board_path = board_path.clone();
    let planning_board_path = board_path.clone();
    let operation = if dry_run {
        "footprint library update dry run"
    } else {
        "footprint library update apply"
    };
    let result = attempt_ipc_write(
        ctx.config.ipc_address.clone(),
        &board_path,
        operation,
        move |client| {
            let document = client.find_open_board(&ipc_board_path)?;
            let footprint_items =
                client.get_items_in(document.clone(), ObjectType::KotPcbFootprint)?;
            let nets = client.get_nets_in(document.clone())?;
            let net_codes = nets
                .into_iter()
                .map(|net| (net.name, net.netcode))
                .collect::<BTreeMap<_, _>>();
            let routed_nets = snapshot_routed_nets(client, document.clone())?;
            let mut plan = plan_updates(
                &planning_board_path,
                &footprint_items,
                &net_codes,
                &routed_nets,
                &filters,
            );

            if dry_run || plan.status == PlanStatus::Conflict {
                return Ok(plan_response(&plan, false));
            }
            if expected_revision.as_deref() != Some(plan.plan_revision.as_str()) {
                plan.status = PlanStatus::Conflict;
                plan.coverage.conflicts.planned += 1;
                plan.diagnostics.push(UpdateDiagnostic {
                    code: "stale_plan_revision".to_string(),
                    message: "The live board, selected libraries, or filters changed; rerun dry run and apply its new plan revision."
                        .to_string(),
                    reference: None,
                });
                plan.changes.clear();
                plan.prepared_items.clear();
                return Ok(plan_response(&plan, false));
            }
            if plan.status == PlanStatus::Noop {
                return Ok(plan_response(&plan, false));
            }

            let update_count = plan.prepared_items.len();
            client.run_commit("Update footprints from libraries", |client| {
                client.update_items_in(document, std::mem::take(&mut plan.prepared_items))
            })?;
            plan.coverage.selected.applied = plan.coverage.selected.planned;
            plan.coverage.changed.applied = update_count;
            plan.coverage.unchanged.applied = plan.coverage.unchanged.planned;
            plan.coverage.skipped_unlinked.applied = plan.coverage.skipped_unlinked.planned;
            Ok(plan_response(&plan, true))
        },
    )
    .await?;

    Ok(match result {
        BoardWrite::Ipc(response) => response,
        BoardWrite::File => preflight_conflict(
            "KiCad IPC is unreachable. update_footprints_from_library is live-IPC-only and \
             never edits the board file directly. Open the requested board in KiCad and retry.",
        ),
        BoardWrite::Refused(result) => {
            let message = result
                .content
                .into_iter()
                .find_map(|content| match content {
                    ToolContent::Text { text } => Some(text),
                    _ => None,
                })
                .unwrap_or_else(|| "KiCad refused the footprint library update".to_string());
            preflight_conflict(message)
        }
    })
}

fn snapshot_routed_nets(
    client: &konnect_ipc::KiCadIpcClient,
    document: kiapi::common::types::DocumentSpecifier,
) -> Result<BTreeSet<String>> {
    use kiapi::common::types::KiCadObjectType as ObjectType;

    let mut routed = BTreeSet::new();
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbTrace)? {
        if let Ok(track) = kiapi::board::types::Track::decode(item.value.as_slice()) {
            if let Some(net) = track.net.filter(|net| !net.name.is_empty()) {
                routed.insert(net.name);
            }
        }
    }
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbArc)? {
        if let Ok(arc) = kiapi::board::types::Arc::decode(item.value.as_slice()) {
            if let Some(net) = arc.net.filter(|net| !net.name.is_empty()) {
                routed.insert(net.name);
            }
        }
    }
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbVia)? {
        if let Ok(via) = kiapi::board::types::Via::decode(item.value.as_slice()) {
            if let Some(net) = via.net.filter(|net| !net.name.is_empty()) {
                routed.insert(net.name);
            }
        }
    }
    if !client
        .get_items_in(document.clone(), ObjectType::KotPcbZone)?
        .is_empty()
    {
        routed.extend(
            client
                .get_nets_in(document)?
                .into_iter()
                .filter(|net| !net.name.is_empty())
                .map(|net| net.name),
        );
    }
    Ok(routed)
}

fn plan_response(plan: &UpdatePlan, applied: bool) -> CallToolResult {
    CallToolResult::json(&json!({
        "status": if applied {
            "applied"
        } else {
            match plan.status {
                PlanStatus::Ready => "ready",
                PlanStatus::Noop => "noop",
                PlanStatus::Conflict => "conflict",
            }
        },
        "plan_revision": plan.plan_revision,
        "coverage": {
            "transport": "live_kicad_ipc",
            "atomicity": "single_kicad_undo_commit",
            "selected": plan.coverage.selected,
            "changed": plan.coverage.changed,
            "unchanged": plan.coverage.unchanged,
            "skipped_unlinked": plan.coverage.skipped_unlinked,
            "conflicts": plan.coverage.conflicts
        },
        "changes": plan.changes,
        "diagnostics": plan.diagnostics,
        "undo": if applied {
            Some("Ctrl-Z reverses the whole footprint library update.")
        } else {
            None
        }
    }))
}

fn preflight_conflict(message: impl Into<String>) -> CallToolResult {
    CallToolResult::json(&json!({
        "status": "conflict",
        "coverage": {
            "transport": "live_kicad_ipc",
            "atomicity": "single_kicad_undo_commit",
            "selected": CountPair::default(),
            "changed": CountPair::default(),
            "unchanged": CountPair::default(),
            "skipped_unlinked": CountPair::default(),
            "conflicts": CountPair { planned: 1, applied: 0 }
        },
        "changes": [],
        "diagnostics": [{
            "code": "preflight_conflict",
            "message": message.into()
        }],
        "undo": null
    }))
}

fn parse_filters(args: &serde_json::Value) -> std::result::Result<UpdateFilters, ArgumentError> {
    fn parse(
        args: &serde_json::Value,
        field: &str,
    ) -> std::result::Result<Option<BTreeSet<String>>, ArgumentError> {
        let Some(value) = args.get(field) else {
            return Ok(None);
        };
        let values = value.as_array().ok_or_else(|| ArgumentError {
            field: field.to_string(),
            reason: "must be an array of strings when supplied".to_string(),
        })?;
        let parsed = values
            .iter()
            .map(|value| {
                let value = value.as_str().ok_or_else(|| ArgumentError {
                    field: field.to_string(),
                    reason: "must contain only strings".to_string(),
                })?;
                if value.is_empty() {
                    return Err(ArgumentError {
                        field: field.to_string(),
                        reason: "must not contain empty strings".to_string(),
                    });
                }
                Ok(value.to_string())
            })
            .collect::<std::result::Result<BTreeSet<_>, _>>()
            .map(Some)?;
        if field == "library_ids" {
            if let Some(invalid) = parsed.as_ref().and_then(|values| {
                values.iter().find(|value| {
                    value.split_once(':').is_none_or(|(nickname, entry)| {
                        nickname.is_empty()
                            || entry.is_empty()
                            || value.contains('/')
                            || value.contains('\\')
                    })
                })
            }) {
                return Err(ArgumentError {
                    field: field.to_string(),
                    reason: format!(
                        "'{invalid}' must use a non-empty Library:Footprint identifier"
                    ),
                });
            }
        }
        Ok(parsed)
    }

    Ok(UpdateFilters {
        references: parse(args, "references")?,
        library_ids: parse(args, "library_ids")?,
    })
}

fn plan_updates(
    board_path: &Path,
    footprint_items: &[prost_types::Any],
    net_codes: &BTreeMap<String, i32>,
    routed_nets: &BTreeSet<String>,
    filters: &UpdateFilters,
) -> UpdatePlan {
    #[derive(Debug)]
    struct Candidate {
        reference: String,
        library_id: String,
        item: prost_types::Any,
        instance: kiapi::board::types::FootprintInstance,
    }

    let mut coverage = UpdateCoverage::default();
    let mut diagnostics = Vec::new();
    let mut candidates = Vec::new();
    let mut reference_counts = BTreeMap::<String, usize>::new();

    for item in footprint_items {
        let instance = match kiapi::board::types::FootprintInstance::decode(item.value.as_slice()) {
            Ok(instance) => instance,
            Err(error) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "invalid_board_footprint".to_string(),
                    message: format!("KiCad returned an invalid footprint: {error}"),
                    reference: None,
                });
                continue;
            }
        };
        let reference = field_text(&instance.reference_field);
        *reference_counts.entry(reference.clone()).or_insert(0) += 1;
        let library_id = instance
            .definition
            .as_ref()
            .and_then(|definition| definition.id.as_ref())
            .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
            .unwrap_or_default();
        candidates.push(Candidate {
            reference,
            library_id,
            item: item.clone(),
            instance,
        });
    }

    if let Some(references) = filters.references.as_ref() {
        for reference in references {
            match reference_counts.get(reference).copied().unwrap_or(0) {
                0 => diagnostics.push(UpdateDiagnostic {
                    code: "reference_not_found".to_string(),
                    message: format!("footprint reference '{reference}' was not found"),
                    reference: Some(reference.clone()),
                }),
                count if count > 1 => diagnostics.push(UpdateDiagnostic {
                    code: "duplicate_reference".to_string(),
                    message: format!("footprint reference '{reference}' appears {count} times"),
                    reference: Some(reference.clone()),
                }),
                _ => {}
            }
        }
    } else {
        for (reference, count) in &reference_counts {
            if *count > 1 {
                diagnostics.push(UpdateDiagnostic {
                    code: "duplicate_reference".to_string(),
                    message: format!("footprint reference '{reference}' appears {count} times"),
                    reference: Some(reference.clone()),
                });
            }
        }
    }

    candidates.sort_by(|left, right| {
        left.reference
            .cmp(&right.reference)
            .then_with(|| left.library_id.cmp(&right.library_id))
    });
    let selected = candidates
        .into_iter()
        .filter(|candidate| {
            filters
                .references
                .as_ref()
                .is_none_or(|references| references.contains(&candidate.reference))
        })
        .filter(|candidate| {
            filters
                .library_ids
                .as_ref()
                .is_none_or(|ids| ids.contains(&candidate.library_id))
        })
        .collect::<Vec<_>>();

    coverage.skipped_unlinked.planned = selected
        .iter()
        .filter(|candidate| candidate.library_id.is_empty())
        .count();
    coverage.selected.planned = selected
        .iter()
        .filter(|candidate| !candidate.library_id.is_empty())
        .count();
    if filters.references.is_some() {
        for candidate in selected
            .iter()
            .filter(|candidate| candidate.library_id.is_empty())
        {
            diagnostics.push(UpdateDiagnostic {
                code: "unlinked_footprint".to_string(),
                message: format!(
                    "footprint '{}' has no resolvable Library:Footprint identifier",
                    candidate.reference
                ),
                reference: Some(candidate.reference.clone()),
            });
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(filters).expect("filters serialize"));
    let mut changes = Vec::new();
    let mut prepared_items = Vec::new();
    for candidate in selected {
        if candidate.library_id.is_empty() {
            continue;
        }
        let Some((nickname, entry)) = candidate.library_id.split_once(':') else {
            diagnostics.push(UpdateDiagnostic {
                code: "invalid_library_id".to_string(),
                message: format!(
                    "footprint '{}' has malformed library id '{}'",
                    candidate.reference, candidate.library_id
                ),
                reference: Some(candidate.reference),
            });
            continue;
        };
        if nickname.is_empty() || entry.is_empty() {
            diagnostics.push(UpdateDiagnostic {
                code: "invalid_library_id".to_string(),
                message: format!(
                    "footprint '{}' has malformed library id '{}'",
                    candidate.reference, candidate.library_id
                ),
                reference: Some(candidate.reference),
            });
            continue;
        }

        let library_path = match super::library::resolve_footprint_path(
            &candidate.library_id,
            board_path.parent(),
        ) {
            Ok(path) => path,
            Err(message) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "footprint_library_resolution_failed".to_string(),
                    message,
                    reference: Some(candidate.reference),
                });
                continue;
            }
        };
        let source = match std::fs::read_to_string(&library_path) {
            Ok(source) => source,
            Err(error) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "footprint_library_read_failed".to_string(),
                    message: format!("failed to read {}: {error}", library_path.display()),
                    reference: Some(candidate.reference),
                });
                continue;
            }
        };
        let library = match parse_library_footprint(&candidate.library_id, &source) {
            Ok(library) => library,
            Err(error) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "unsupported_library_footprint".to_string(),
                    message: format!("{error:#}"),
                    reference: Some(candidate.reference),
                });
                continue;
            }
        };
        let prepared =
            match build_updated_instance(&candidate.instance, &library, net_codes, routed_nets) {
                Ok(prepared) => prepared,
                Err(error) => {
                    diagnostics.push(UpdateDiagnostic {
                        code: "footprint_update_conflict".to_string(),
                        message: format!("{error:#}"),
                        reference: Some(candidate.reference),
                    });
                    continue;
                }
            };

        hasher.update(candidate.item.type_url.as_bytes());
        hasher.update(&candidate.item.value);
        hasher.update(candidate.library_id.as_bytes());
        hasher.update(source.as_bytes());
        if prepared.changed_domains.is_empty() {
            coverage.unchanged.planned += 1;
        } else {
            coverage.changed.planned += 1;
            changes.push(PlannedUpdate {
                reference: candidate.reference,
                library_id: candidate.library_id,
                changed_domains: prepared.changed_domains,
                preserved: PreservedState::default(),
            });
            prepared_items.push(prepared.item);
        }
    }

    if !diagnostics.is_empty() {
        coverage.conflicts.planned = diagnostics.len();
        changes.clear();
        prepared_items.clear();
        coverage.changed.planned = 0;
        return UpdatePlan {
            status: PlanStatus::Conflict,
            plan_revision: format!("{:x}", hasher.finalize()),
            coverage,
            changes,
            diagnostics,
            prepared_items,
        };
    }

    let status = if changes.is_empty() {
        PlanStatus::Noop
    } else {
        PlanStatus::Ready
    };
    UpdatePlan {
        status,
        plan_revision: format!("{:x}", hasher.finalize()),
        coverage,
        changes,
        diagnostics,
        prepared_items,
    }
}

fn parse_library_footprint(library_id: &str, source: &str) -> Result<LibraryFootprint> {
    let (library_nickname, entry_name) = library_id
        .split_once(':')
        .filter(|(nickname, entry)| !nickname.is_empty() && !entry.is_empty())
        .context("footprint identifier must use non-empty Library:Footprint syntax")?;
    let root = konnect_sexp::parse_sexp(source).context("invalid footprint S-expression")?;
    if root.head() != Some("footprint") {
        bail!("library source root must be a footprint");
    }

    validate_supported_children(&root)?;
    let pads = super::pcb_components::extract_pad_definitions(source)?;
    let graphics = super::pcb_components::extract_graphic_definitions(source)?;
    let models = parse_models(&root)?;
    let attributes = parse_attributes(&root)?;
    let datasheet = property_value(&root, "Datasheet");
    let description_field = property_value(&root, "Description");
    let definition = kiapi::board::types::Footprint {
        id: Some(kiapi::common::types::LibraryIdentifier {
            library_nickname: library_nickname.to_string(),
            entry_name: entry_name.to_string(),
        }),
        attributes: Some(kiapi::board::types::FootprintAttributes {
            description: root.find_str("descr").unwrap_or_default().to_string(),
            keywords: root.find_str("tags").unwrap_or_default().to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };

    Ok(LibraryFootprint {
        library_id: library_id.to_string(),
        definition,
        attributes,
        datasheet,
        description_field,
        pads,
        graphics,
        models,
    })
}

fn property_value(root: &konnect_sexp::SexpNode, name: &str) -> Option<String> {
    root.find_all("property")
        .into_iter()
        .find(|property| property.get(1).and_then(konnect_sexp::SexpNode::as_str) == Some(name))
        .and_then(|property| property.get(2))
        .and_then(konnect_sexp::SexpNode::as_str)
        .map(str::to_string)
}

fn validate_supported_children(root: &konnect_sexp::SexpNode) -> Result<()> {
    for child in root.children().unwrap_or_default().iter().skip(2) {
        let Some(tag) = child.head() else {
            continue;
        };
        match tag {
            "version" | "generator" | "generator_version" | "layer" | "descr" | "tags" | "attr"
            | "property" | "fp_text" | "fp_line" | "fp_rect" | "fp_circle" | "fp_arc"
            | "fp_poly" | "pad" | "model" => {}
            unsupported => {
                bail!("footprint child '{unsupported}' is not supported by typed library refresh")
            }
        }
        match tag {
            "pad" => validate_pad(child)?,
            "fp_line" | "fp_rect" | "fp_circle" | "fp_arc" | "fp_poly" => {
                validate_graphic(child, tag)?
            }
            "fp_text" => {
                let kind = child
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("fp_text is missing its kind")?;
                if !matches!(kind, "reference" | "value") {
                    bail!(
                        "fp_text kind '{kind}' is not supported losslessly by typed library refresh"
                    );
                }
            }
            "property" => {
                let name = child
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("property is missing its name")?;
                if !matches!(name, "Reference" | "Value" | "Datasheet" | "Description") {
                    bail!("property '{name}' is not supported losslessly by typed library refresh");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_graphic(graphic: &konnect_sexp::SexpNode, kind: &str) -> Result<()> {
    let allowed: &[&str] = match kind {
        "fp_line" => &["start", "end", "stroke", "layer", "uuid", "tstamp"],
        "fp_rect" => &["start", "end", "stroke", "fill", "layer", "uuid", "tstamp"],
        "fp_circle" => &["center", "end", "stroke", "fill", "layer", "uuid", "tstamp"],
        "fp_arc" => &["start", "mid", "end", "stroke", "layer", "uuid", "tstamp"],
        "fp_poly" => &["pts", "stroke", "fill", "layer", "uuid", "tstamp"],
        _ => bail!("unsupported footprint graphic '{kind}'"),
    };
    for child in graphic.children().unwrap_or_default().iter().skip(1) {
        let Some(tag) = child.head() else {
            bail!("{kind} contains an unsupported atom");
        };
        if !allowed.contains(&tag) {
            bail!("{kind} clause '{tag}' is not supported by typed library refresh");
        }
        match tag {
            "stroke" => {
                for stroke_child in child.children().unwrap_or_default().iter().skip(1) {
                    let stroke_tag = stroke_child
                        .head()
                        .context("graphic stroke contains an unsupported atom")?;
                    match stroke_tag {
                        "width" => {}
                        "type" => {
                            let stroke_type = stroke_child
                                .get(1)
                                .and_then(konnect_sexp::SexpNode::as_str)
                                .context("graphic stroke type is missing")?;
                            if !matches!(stroke_type, "solid" | "default") {
                                bail!(
                                    "graphic stroke type '{stroke_type}' is not supported losslessly"
                                );
                            }
                        }
                        unsupported => bail!(
                            "graphic stroke clause '{unsupported}' is not supported losslessly"
                        ),
                    }
                }
            }
            "fill" => {
                let fill = child
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("graphic fill is missing")?;
                if !matches!(fill, "none" | "no" | "solid" | "yes") {
                    bail!("graphic fill '{fill}' is not supported losslessly");
                }
            }
            "pts"
                if child
                    .children()
                    .unwrap_or_default()
                    .iter()
                    .skip(1)
                    .any(|point| point.head() != Some("xy")) =>
            {
                bail!("fp_poly contains a non-xy point");
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_pad(pad: &konnect_sexp::SexpNode) -> Result<()> {
    let shape = pad
        .get(3)
        .and_then(konnect_sexp::SexpNode::as_str)
        .context("footprint pad is missing its shape")?;
    if !matches!(shape, "circle" | "rect" | "oval" | "roundrect") {
        bail!("pad shape '{shape}' is not supported by typed library refresh");
    }
    for child in pad.children().unwrap_or_default().iter().skip(4) {
        let Some(tag) = child.head() else {
            continue;
        };
        if !matches!(
            tag,
            "at" | "size" | "layers" | "drill" | "roundrect_rratio" | "uuid" | "tstamp"
        ) {
            bail!("pad clause '{tag}' is not supported by typed library refresh");
        }
        if tag == "drill" {
            for nested in child.children().unwrap_or_default().iter().skip(1) {
                if nested.head().is_some() {
                    bail!("nested drill clauses are not supported by typed library refresh");
                }
            }
        }
    }
    Ok(())
}

fn parse_attributes(
    root: &konnect_sexp::SexpNode,
) -> Result<kiapi::board::types::FootprintAttributes> {
    use kiapi::board::types::FootprintMountingStyle;

    let mut attributes = kiapi::board::types::FootprintAttributes::default();
    let Some(attr) = root.find("attr") else {
        return Ok(attributes);
    };
    for value in attr.children().unwrap_or_default().iter().skip(1) {
        match value
            .as_str()
            .context("footprint attr contains a non-atom")?
        {
            "smd" => attributes.mounting_style = FootprintMountingStyle::FmsSmd as i32,
            "through_hole" => {
                attributes.mounting_style = FootprintMountingStyle::FmsThroughHole as i32
            }
            "board_only" => attributes.not_in_schematic = true,
            "exclude_from_pos_files" => attributes.exclude_from_position_files = true,
            "exclude_from_bom" => attributes.exclude_from_bill_of_materials = true,
            "allow_missing_courtyard" => attributes.exempt_from_courtyard_requirement = true,
            "dnp" => attributes.do_not_populate = true,
            "allow_soldermask_bridges" => attributes.allow_soldermask_bridges = true,
            unsupported => bail!(
                "footprint attribute '{unsupported}' is not supported by typed library refresh"
            ),
        }
    }
    Ok(attributes)
}

fn parse_models(
    root: &konnect_sexp::SexpNode,
) -> Result<Vec<kiapi::board::types::Footprint3DModel>> {
    root.find_all("model")
        .into_iter()
        .map(|model| {
            for child in model.children().unwrap_or_default().iter().skip(2) {
                let Some(tag) = child.head() else {
                    if child.as_str() == Some("hide") {
                        continue;
                    }
                    bail!("3D model contains an unsupported atom");
                };
                if !matches!(tag, "offset" | "scale" | "rotate" | "opacity") {
                    bail!("3D model clause '{tag}' is not supported");
                }
            }
            let vector = |tag: &str, default: [f64; 3]| -> Result<kiapi::common::types::Vector3D> {
                let Some(wrapper) = model.find(tag) else {
                    return Ok(kiapi::common::types::Vector3D {
                        x_nm: default[0],
                        y_nm: default[1],
                        z_nm: default[2],
                    });
                };
                let xyz = wrapper
                    .find("xyz")
                    .with_context(|| format!("3D model {tag} is missing xyz"))?;
                Ok(kiapi::common::types::Vector3D {
                    x_nm: xyz
                        .get_f64(1)
                        .with_context(|| format!("3D model {tag}.x is invalid"))?,
                    y_nm: xyz
                        .get_f64(2)
                        .with_context(|| format!("3D model {tag}.y is invalid"))?,
                    z_nm: xyz
                        .get_f64(3)
                        .with_context(|| format!("3D model {tag}.z is invalid"))?,
                })
            };
            Ok(kiapi::board::types::Footprint3DModel {
                filename: model
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("3D model is missing its filename")?
                    .to_string(),
                scale: Some(vector("scale", [1.0, 1.0, 1.0])?),
                rotation: Some(vector("rotate", [0.0, 0.0, 0.0])?),
                offset: Some(vector("offset", [0.0, 0.0, 0.0])?),
                visible: !model
                    .children()
                    .unwrap_or_default()
                    .iter()
                    .any(|child| child.as_str() == Some("hide")),
                opacity: model.find_f64("opacity").unwrap_or(1.0),
            })
        })
        .collect()
}

fn build_updated_instance(
    current: &kiapi::board::types::FootprintInstance,
    library: &LibraryFootprint,
    net_codes: &BTreeMap<String, i32>,
    routed_nets: &BTreeSet<String>,
) -> Result<PreparedUpdate> {
    let current_definition = current
        .definition
        .as_ref()
        .context("board footprint has no embedded definition")?;
    let current_id = current_definition
        .id
        .as_ref()
        .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
        .unwrap_or_default();
    if current_id != library.library_id {
        bail!(
            "board footprint library id '{current_id}' does not match '{}'",
            library.library_id
        );
    }

    let mut old_nets = BTreeMap::<String, kiapi::board::types::Net>::new();
    for item in &current_definition.items {
        if !item.type_url.ends_with("kiapi.board.types.Pad") {
            continue;
        }
        let pad = kiapi::board::types::Pad::decode(item.value.as_slice())
            .context("board footprint contains an invalid pad")?;
        let Some(net) = pad.net.filter(|net| !net.name.is_empty()) else {
            continue;
        };
        if let Some(existing) = old_nets.insert(pad.number.clone(), net.clone()) {
            if existing.name != net.name {
                bail!(
                    "logical pad {} carries multiple nets ('{}' and '{}')",
                    pad.number,
                    existing.name,
                    net.name
                );
            }
        }
    }
    let new_numbers = library
        .pads
        .iter()
        .map(|pad| pad.number.as_str())
        .collect::<BTreeSet<_>>();
    for (number, net) in &old_nets {
        if !new_numbers.contains(number.as_str()) {
            let routed = routed_nets.contains(&net.name);
            bail!(
                "library update removes connected pad {number} on net '{}'{}",
                net.name,
                if routed { " with routed copper" } else { "" }
            );
        }
    }

    let position = current.position.as_ref().cloned().unwrap_or_default();
    let rotation = current
        .orientation
        .as_ref()
        .map(|angle| angle.value_degrees)
        .unwrap_or(0.0);
    let is_back = current.layer == kiapi::board::types::BoardLayer::BlBCu as i32;
    let layer = if is_back { "B.Cu" } else { "F.Cu" };
    let (pads, graphics) = if is_back {
        (
            library
                .pads
                .iter()
                .map(mirror_pad)
                .collect::<Result<Vec<_>>>()?,
            library
                .graphics
                .iter()
                .map(mirror_graphic)
                .collect::<Result<Vec<_>>>()?,
        )
    } else {
        (library.pads.clone(), library.graphics.clone())
    };
    let client = konnect_ipc::KiCadIpcClient::new("tcp://never-dialed");
    let packed = client.build_footprint_item(
        &library.library_id,
        &field_text(&current.reference_field),
        &field_text(&current.value_field),
        &pads,
        &graphics,
        &Default::default(),
        konnect_ipc::builders::nm_to_mm(position.x_nm),
        konnect_ipc::builders::nm_to_mm(position.y_nm),
        rotation,
        layer,
    )?;
    let built = kiapi::board::types::FootprintInstance::decode(packed.value.as_slice())
        .context("typed footprint builder returned an invalid item")?;

    let mut updated = current.clone();
    let mut definition = built
        .definition
        .context("typed footprint builder returned no definition")?;
    definition.attributes = library.definition.attributes.clone();
    definition.reference_field = current_definition.reference_field.clone();
    definition.value_field = current_definition.value_field.clone();
    definition.datasheet_field = current_definition.datasheet_field.clone();
    definition.description_field = current_definition.description_field.clone();
    apply_field_value(
        &mut definition.datasheet_field,
        library.datasheet.as_deref(),
    );
    apply_field_value(
        &mut definition.description_field,
        library.description_field.as_deref(),
    );
    definition.items.extend(
        library.models.iter().map(|model| {
            konnect_ipc::builders::pack_any(model, "kiapi.board.types.Footprint3DModel")
        }),
    );
    for item in &mut definition.items {
        if item.type_url.ends_with("kiapi.board.types.Pad") {
            let mut pad = kiapi::board::types::Pad::decode(item.value.as_slice())?;
            pad.net = old_nets
                .get(&pad.number)
                .map(|old| kiapi::board::types::Net {
                    code: net_codes
                        .get(&old.name)
                        .copied()
                        .or_else(|| old.code.as_ref().map(|code| code.value))
                        .map(|value| kiapi::board::types::NetCode { value }),
                    name: old.name.clone(),
                });
            *item = konnect_ipc::builders::pack_any(&pad, "kiapi.board.types.Pad");
        } else if is_back && item.type_url.ends_with("kiapi.board.types.BoardText") {
            let mut text = kiapi::board::types::BoardText::decode(item.value.as_slice())?;
            if is_side_specific_layer(text.layer) {
                if let Some(attributes) =
                    text.text.as_mut().and_then(|text| text.attributes.as_mut())
                {
                    attributes.mirrored = true;
                }
            }
            *item = konnect_ipc::builders::pack_any(&text, "kiapi.board.types.BoardText");
        }
    }
    updated.definition = Some(definition);
    apply_field_value(&mut updated.datasheet_field, library.datasheet.as_deref());
    apply_field_value(
        &mut updated.description_field,
        library.description_field.as_deref(),
    );
    let mut attributes = library.attributes.clone();
    if let Some(current_attributes) = current.attributes.as_ref() {
        attributes.not_in_schematic = current_attributes.not_in_schematic;
        attributes.do_not_populate = current_attributes.do_not_populate;
    }
    updated.attributes = Some(attributes);

    let changed_domains = changed_domains(current, &updated)?;
    Ok(PreparedUpdate {
        item: konnect_ipc::builders::pack_any(&updated, "kiapi.board.types.FootprintInstance"),
        changed_domains,
    })
}

fn apply_field_value(field: &mut Option<kiapi::board::types::Field>, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    field
        .get_or_insert_with(Default::default)
        .text
        .get_or_insert_with(Default::default)
        .text
        .get_or_insert_with(Default::default)
        .text = value.to_string();
}

fn mirror_pad(pad: &konnect_ipc::IpcPadDefinition) -> Result<konnect_ipc::IpcPadDefinition> {
    let mut mirrored = pad.clone();
    mirrored.y = -mirrored.y;
    mirrored.rotation = -mirrored.rotation;
    mirrored.layers = mirrored
        .layers
        .iter()
        .map(|layer| flip_layer_name(layer))
        .collect::<Result<_>>()?;
    Ok(mirrored)
}

fn mirror_graphic(
    graphic: &konnect_ipc::IpcGraphicDefinition,
) -> Result<konnect_ipc::IpcGraphicDefinition> {
    use konnect_ipc::IpcGraphicDefinition as Graphic;

    let point = |(x, y): (f64, f64)| (x, -y);
    Ok(match graphic {
        Graphic::Line {
            start,
            end,
            layer,
            width,
        } => Graphic::Line {
            start: point(*start),
            end: point(*end),
            layer: flip_layer_name(layer)?,
            width: *width,
        },
        Graphic::Rect {
            start,
            end,
            layer,
            width,
            filled,
        } => Graphic::Rect {
            start: point(*start),
            end: point(*end),
            layer: flip_layer_name(layer)?,
            width: *width,
            filled: *filled,
        },
        Graphic::Circle {
            center,
            end,
            layer,
            width,
            filled,
        } => Graphic::Circle {
            center: point(*center),
            end: point(*end),
            layer: flip_layer_name(layer)?,
            width: *width,
            filled: *filled,
        },
        Graphic::Arc {
            start,
            mid,
            end,
            layer,
            width,
        } => Graphic::Arc {
            start: point(*end),
            mid: point(*mid),
            end: point(*start),
            layer: flip_layer_name(layer)?,
            width: *width,
        },
        Graphic::Poly {
            points,
            layer,
            width,
            filled,
        } => Graphic::Poly {
            points: points.iter().copied().map(point).collect(),
            layer: flip_layer_name(layer)?,
            width: *width,
            filled: *filled,
        },
        Graphic::Text {
            text,
            position,
            rotation,
            layer,
            size,
        } => Graphic::Text {
            text: text.clone(),
            position: point(*position),
            rotation: 180.0 - rotation,
            layer: flip_layer_name(layer)?,
            size: *size,
        },
    })
}

fn flip_layer_name(layer: &str) -> Result<String> {
    let flipped = match layer {
        "F.Cu" => "B.Cu",
        "B.Cu" => "F.Cu",
        "F.Adhes" => "B.Adhes",
        "B.Adhes" => "F.Adhes",
        "F.Paste" => "B.Paste",
        "B.Paste" => "F.Paste",
        "F.SilkS" | "F.Silkscreen" => "B.SilkS",
        "B.SilkS" | "B.Silkscreen" => "F.SilkS",
        "F.Mask" => "B.Mask",
        "B.Mask" => "F.Mask",
        "F.CrtYd" | "F.Courtyard" => "B.CrtYd",
        "B.CrtYd" | "B.Courtyard" => "F.CrtYd",
        "F.Fab" => "B.Fab",
        "B.Fab" => "F.Fab",
        "*.Cu" | "*.Mask" | "*.Paste" => layer,
        other if other.starts_with("F.") || other.starts_with("B.") => {
            bail!("unsupported side-specific footprint layer '{other}'")
        }
        other => other,
    };
    Ok(flipped.to_string())
}

fn is_side_specific_layer(layer: i32) -> bool {
    use kiapi::board::types::BoardLayer;

    matches!(
        BoardLayer::try_from(layer).ok(),
        Some(
            BoardLayer::BlFCu
                | BoardLayer::BlBCu
                | BoardLayer::BlFAdhes
                | BoardLayer::BlBAdhes
                | BoardLayer::BlFPaste
                | BoardLayer::BlBPaste
                | BoardLayer::BlFSilkS
                | BoardLayer::BlBSilkS
                | BoardLayer::BlFMask
                | BoardLayer::BlBMask
                | BoardLayer::BlFCrtYd
                | BoardLayer::BlBCrtYd
                | BoardLayer::BlFFab
                | BoardLayer::BlBFab
        )
    )
}

fn changed_domains(
    current: &kiapi::board::types::FootprintInstance,
    updated: &kiapi::board::types::FootprintInstance,
) -> Result<BTreeSet<ChangedDomain>> {
    let mut changed = BTreeSet::new();
    let current_definition = current
        .definition
        .as_ref()
        .context("missing current definition")?;
    let updated_definition = updated
        .definition
        .as_ref()
        .context("missing updated definition")?;

    let current_pads = normalized_items(current_definition, "Pad")?;
    let updated_pads = normalized_items(updated_definition, "Pad")?;
    if current_pads != updated_pads {
        changed.insert(ChangedDomain::Pads);
    }
    let current_graphics = normalized_graphics(current_definition)?;
    let updated_graphics = normalized_graphics(updated_definition)?;
    if current_graphics != updated_graphics {
        changed.insert(ChangedDomain::Graphics);
    }
    if normalized_items(current_definition, "Footprint3DModel")?
        != normalized_items(updated_definition, "Footprint3DModel")?
    {
        changed.insert(ChangedDomain::Models);
    }
    if current_definition.attributes != updated_definition.attributes
        || field_text(&current_definition.datasheet_field)
            != field_text(&updated_definition.datasheet_field)
        || field_text(&current_definition.description_field)
            != field_text(&updated_definition.description_field)
    {
        changed.insert(ChangedDomain::Metadata);
    }
    if library_owned_attributes(current.attributes.as_ref())
        != library_owned_attributes(updated.attributes.as_ref())
    {
        changed.insert(ChangedDomain::Attributes);
    }
    Ok(changed)
}

fn normalized_items(
    definition: &kiapi::board::types::Footprint,
    suffix: &str,
) -> Result<Vec<Vec<u8>>> {
    let mut items = definition
        .items
        .iter()
        .filter(|item| item.type_url.ends_with(suffix))
        .map(|item| {
            if suffix == "Pad" {
                let mut pad = kiapi::board::types::Pad::decode(item.value.as_slice())?;
                pad.id = None;
                pad.net = None;
                pad.pad_to_die_length = None;
                pad.symbol_pin = None;
                pad.pad_to_die_delay = None;
                if let Some(stack) = pad.pad_stack.as_mut() {
                    stack.layers.sort_unstable();
                    if stack.drill.as_ref().is_some_and(|drill| {
                        drill.start_layer == kiapi::board::types::BoardLayer::BlUndefined as i32
                            && drill.end_layer
                                == kiapi::board::types::BoardLayer::BlUndefined as i32
                            && drill
                                .diameter
                                .as_ref()
                                .is_none_or(|diameter| diameter.x_nm == 0 && diameter.y_nm == 0)
                    }) {
                        stack.drill = None;
                    }
                    if stack.zone_settings.as_ref().is_some_and(|settings| {
                        settings.zone_connection
                            == kiapi::board::types::ZoneConnectionStyle::ZcsInherited as i32
                    }) {
                        stack.zone_settings = None;
                    }
                }
                Ok(pad.encode_to_vec())
            } else {
                Ok(item.value.clone())
            }
        })
        .collect::<Result<Vec<_>>>()?;
    items.sort();
    Ok(items)
}

fn normalized_graphics(definition: &kiapi::board::types::Footprint) -> Result<Vec<Vec<u8>>> {
    let mut graphics = definition
        .items
        .iter()
        .filter_map(|item| {
            if item.type_url.ends_with("BoardGraphicShape") {
                Some(
                    kiapi::board::types::BoardGraphicShape::decode(item.value.as_slice()).map(
                        |mut shape| {
                            shape.id = None;
                            shape.net = None;
                            shape.locked = kiapi::common::types::LockedState::LsUnlocked as i32;
                            if let Some(graphic) = shape.shape.as_mut() {
                                if let Some(stroke) = graphic
                                    .attributes
                                    .as_mut()
                                    .and_then(|attributes| attributes.stroke.as_mut())
                                {
                                    if matches!(
                                        stroke.style(),
                                        kiapi::common::types::StrokeLineStyle::SlsUnknown
                                            | kiapi::common::types::StrokeLineStyle::SlsDefault
                                            | kiapi::common::types::StrokeLineStyle::SlsSolid
                                    ) {
                                        stroke.style =
                                            kiapi::common::types::StrokeLineStyle::SlsSolid as i32;
                                    }
                                    stroke.color = None;
                                }
                                if let Some(
                                    kiapi::common::types::graphic_shape::Geometry::Polygon(polyset),
                                ) = graphic.geometry.as_mut()
                                {
                                    normalize_polyset(polyset);
                                }
                                if let Some(
                                    kiapi::common::types::graphic_shape::Geometry::Rectangle(
                                        rectangle,
                                    ),
                                ) = graphic.geometry.as_mut()
                                {
                                    if rectangle
                                        .corner_radius
                                        .as_ref()
                                        .is_some_and(|radius| radius.value_nm == 0)
                                    {
                                        rectangle.corner_radius = None;
                                    }
                                }
                            }
                            shape.encode_to_vec()
                        },
                    ),
                )
            } else if item.type_url.ends_with("BoardText") {
                Some(
                    kiapi::board::types::BoardText::decode(item.value.as_slice()).map(
                        |mut text| {
                            text.id = None;
                            text.encode_to_vec()
                        },
                    ),
                )
            } else {
                None
            }
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    graphics.sort();
    Ok(graphics)
}

fn normalize_polyset(polyset: &mut kiapi::common::types::PolySet) {
    for polygon in &mut polyset.polygons {
        if let Some(outline) = polygon.outline.as_mut() {
            normalize_polyline(outline);
        }
        for hole in &mut polygon.holes {
            normalize_polyline(hole);
        }
        polygon.holes.sort_by_key(prost::Message::encode_to_vec);
    }
    polyset.polygons.sort_by_key(prost::Message::encode_to_vec);
}

fn normalize_polyline(polyline: &mut kiapi::common::types::PolyLine) {
    if !polyline.closed || polyline.nodes.len() < 2 {
        return;
    }
    let candidates = [
        polyline.nodes.clone(),
        polyline.nodes.iter().cloned().rev().collect(),
    ];
    let mut best: Option<(Vec<u8>, Vec<kiapi::common::types::PolyLineNode>)> = None;
    for nodes in candidates {
        for offset in 0..nodes.len() {
            let rotated = nodes[offset..]
                .iter()
                .chain(nodes[..offset].iter())
                .cloned()
                .collect::<Vec<_>>();
            let encoded = rotated
                .iter()
                .flat_map(prost::Message::encode_to_vec)
                .collect::<Vec<_>>();
            if best
                .as_ref()
                .is_none_or(|(best_encoded, _)| encoded < *best_encoded)
            {
                best = Some((encoded, rotated));
            }
        }
    }
    if let Some((_, nodes)) = best {
        polyline.nodes = nodes;
    }
}

fn library_owned_attributes(
    attributes: Option<&kiapi::board::types::FootprintAttributes>,
) -> Option<(bool, bool, bool, bool, i32, bool)> {
    attributes.map(|attributes| {
        let mounting_style = match attributes.mounting_style() {
            kiapi::board::types::FootprintMountingStyle::FmsUnknown
            | kiapi::board::types::FootprintMountingStyle::FmsUnspecified => {
                kiapi::board::types::FootprintMountingStyle::FmsUnspecified as i32
            }
            style => style as i32,
        };
        (
            attributes.exclude_from_position_files,
            attributes.exclude_from_bill_of_materials,
            attributes.exempt_from_courtyard_requirement,
            attributes.allow_soldermask_bridges,
            mounting_style,
            attributes.not_in_schematic,
        )
    })
}

fn field_text(field: &Option<kiapi::board::types::Field>) -> String {
    field
        .as_ref()
        .and_then(|field| field.text.as_ref())
        .and_then(|text| text.text.as_ref())
        .map(|text| text.text.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_ipc::builders;
    use konnect_ipc::gen::kiapi;
    use prost::Message;
    use std::collections::{BTreeMap, BTreeSet};

    const LIBRARY_FOOTPRINT: &str = r#"
(footprint "Socket"
  (version 20240108)
  (generator "konnect")
  (layer "F.Cu")
  (descr "updated description")
  (tags "keyboard socket")
  (attr smd exclude_from_pos_files)
  (fp_line (start -1 -2) (end 3 4)
    (stroke (width 0.12) (type solid))
    (layer "F.SilkS"))
  (fp_poly (pts (xy -2 -1) (xy 2 -1) (xy 2 1) (xy -2 1))
    (stroke (width 0.05) (type solid))
    (fill none)
    (layer "B.CrtYd"))
  (pad "1" smd roundrect (at -2 0 15) (size 2 1)
    (layers "B.Cu" "B.Paste" "B.Mask")
    (roundrect_rratio 0.2))
  (pad "1" smd rect (at -1 0) (size 1 1) (layers "B.Cu"))
  (pad "2" thru_hole circle (at 2 0) (size 3 3)
    (layers "*.Cu" "*.Mask") (drill 1))
  (pad "3" smd rect (at 0 3) (size 1 1) (layers "F.Cu"))
  (fp_text reference "REF**" (at 0 -4 0) (layer "F.SilkS")
    (effects (font (size 1 1) (thickness 0.15))))
  (fp_text value "Socket" (at 0 4 0) (layer "F.Fab")
    (effects (font (size 1 1) (thickness 0.15))))
  (property "Datasheet" "new-datasheet.pdf" (at 0 0) (layer "F.Fab") (hide yes))
  (property "Description" "new field description" (at 0 0) (layer "F.Fab") (hide yes))
  (model "../models/Socket.step"
    (offset (xyz 1 2 3))
    (scale (xyz -1 1 1))
    (rotate (xyz 90 0 45)))
)
"#;

    fn field(name: &str, value: &str, x: f64, y: f64, visible: bool) -> kiapi::board::types::Field {
        kiapi::board::types::Field {
            name: name.to_string(),
            visible,
            text: Some(kiapi::board::types::BoardText {
                text: Some(kiapi::common::types::Text {
                    position: Some(builders::vec2(x, y)),
                    attributes: Some(kiapi::common::types::TextAttributes {
                        size: Some(builders::vec2(1.25, 1.25)),
                        angle: Some(kiapi::common::types::Angle {
                            value_degrees: 17.0,
                        }),
                        mirrored: true,
                        ..Default::default()
                    }),
                    text: value.to_string(),
                    ..Default::default()
                }),
                layer: kiapi::board::types::BoardLayer::BlBSilkS as i32,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn current_pad(number: &str, net_name: &str, net_code: i32) -> prost_types::Any {
        builders::pack_any(
            &kiapi::board::types::Pad {
                id: Some(kiapi::common::types::Kiid {
                    value: format!("old-pad-{number}-{net_code}"),
                }),
                number: number.to_string(),
                net: Some(kiapi::board::types::Net {
                    code: Some(kiapi::board::types::NetCode { value: net_code }),
                    name: net_name.to_string(),
                }),
                position: Some(builders::vec2(100.0, 50.0)),
                ..Default::default()
            },
            "kiapi.board.types.Pad",
        )
    }

    fn current_instance(
        layer: kiapi::board::types::BoardLayer,
    ) -> kiapi::board::types::FootprintInstance {
        let reference = field("Reference", "SW1", 101.0, 48.0, true);
        let value = field("Value", "Socket Value", 99.0, 52.0, false);
        kiapi::board::types::FootprintInstance {
            id: Some(kiapi::common::types::Kiid {
                value: "instance-kiid".to_string(),
            }),
            position: Some(builders::vec2(100.0, 50.0)),
            orientation: Some(kiapi::common::types::Angle {
                value_degrees: 37.0,
            }),
            layer: layer as i32,
            locked: kiapi::common::types::LockedState::LsLocked as i32,
            definition: Some(kiapi::board::types::Footprint {
                id: Some(kiapi::common::types::LibraryIdentifier {
                    library_nickname: "Test".to_string(),
                    entry_name: "Socket".to_string(),
                }),
                reference_field: Some(reference.clone()),
                value_field: Some(value.clone()),
                items: vec![current_pad("1", "ROW1", 11), current_pad("2", "COL1", 12)],
                ..Default::default()
            }),
            reference_field: Some(reference),
            value_field: Some(value),
            datasheet_field: Some(field("Datasheet", "placed-datasheet", 0.0, 0.0, false)),
            description_field: Some(field("Description", "placed-description", 0.0, 0.0, false)),
            attributes: Some(kiapi::board::types::FootprintAttributes {
                not_in_schematic: false,
                exclude_from_bill_of_materials: true,
                do_not_populate: true,
                ..Default::default()
            }),
            overrides: Some(kiapi::board::types::FootprintDesignRuleOverrides {
                copper_clearance: Some(builders::distance(0.3)),
                ..Default::default()
            }),
            symbol_path: Some(kiapi::common::types::SheetPath {
                path: vec![kiapi::common::types::Kiid {
                    value: "sheet-kiid".to_string(),
                }],
                path_human_readable: "/Keyboard".to_string(),
            }),
            symbol_sheet_name: "Keyboard".to_string(),
            symbol_sheet_filename: "keyboard.kicad_sch".to_string(),
            symbol_footprint_filters: "Test:*".to_string(),
        }
    }

    fn decoded_pads(
        instance: &kiapi::board::types::FootprintInstance,
    ) -> Vec<kiapi::board::types::Pad> {
        instance
            .definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|item| item.type_url.ends_with("kiapi.board.types.Pad"))
            .map(|item| kiapi::board::types::Pad::decode(item.value.as_slice()).unwrap())
            .collect()
    }

    #[test]
    fn parses_supported_library_definition_without_dropping_domains() {
        let library = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();

        assert_eq!(library.library_id, "Test:Socket");
        assert_eq!(
            library.definition.attributes.as_ref().unwrap().description,
            "updated description"
        );
        assert_eq!(
            library.definition.attributes.as_ref().unwrap().keywords,
            "keyboard socket"
        );
        assert_eq!(library.pads.len(), 4);
        assert_eq!(library.graphics.len(), 2);
        assert_eq!(library.models.len(), 1);
        let model = &library.models[0];
        assert_eq!(model.filename, "../models/Socket.step");
        assert_eq!(model.offset.as_ref().unwrap().x_nm, 1.0);
        assert_eq!(model.scale.as_ref().unwrap().x_nm, -1.0);
        assert_eq!(model.rotation.as_ref().unwrap().z_nm, 45.0);
    }

    #[test]
    fn merge_preserves_instance_state_and_nets_by_logical_pad_number() {
        let current = current_instance(kiapi::board::types::BoardLayer::BlBCu);
        let library = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();
        let prepared = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::from(["ROW1".to_string(), "COL1".to_string()]),
        )
        .unwrap();
        let updated =
            kiapi::board::types::FootprintInstance::decode(prepared.item.value.as_slice()).unwrap();

        assert_eq!(updated.id, current.id);
        assert_eq!(updated.position, current.position);
        assert_eq!(updated.orientation, current.orientation);
        assert_eq!(updated.layer, current.layer);
        assert_eq!(updated.locked, current.locked);
        assert_eq!(updated.reference_field, current.reference_field);
        assert_eq!(updated.value_field, current.value_field);
        assert_eq!(field_text(&updated.datasheet_field), "new-datasheet.pdf");
        assert_eq!(
            updated
                .datasheet_field
                .as_ref()
                .and_then(|field| field.text.as_ref())
                .and_then(|text| text.text.as_ref())
                .and_then(|text| text.attributes.as_ref()),
            current
                .datasheet_field
                .as_ref()
                .and_then(|field| field.text.as_ref())
                .and_then(|text| text.text.as_ref())
                .and_then(|text| text.attributes.as_ref())
        );
        assert_eq!(
            field_text(&updated.description_field),
            "new field description"
        );
        let attributes = updated.attributes.as_ref().unwrap();
        let current_attributes = current.attributes.as_ref().unwrap();
        assert_eq!(
            attributes.not_in_schematic,
            current_attributes.not_in_schematic
        );
        assert_eq!(
            attributes.do_not_populate,
            current_attributes.do_not_populate
        );
        assert!(attributes.exclude_from_position_files);
        assert!(!attributes.exclude_from_bill_of_materials);
        assert_eq!(
            attributes.mounting_style,
            kiapi::board::types::FootprintMountingStyle::FmsSmd as i32
        );
        assert_eq!(updated.overrides, current.overrides);
        assert_eq!(updated.symbol_path, current.symbol_path);
        assert_eq!(updated.symbol_sheet_name, current.symbol_sheet_name);
        assert_eq!(updated.symbol_sheet_filename, current.symbol_sheet_filename);
        assert_eq!(
            updated.symbol_footprint_filters,
            current.symbol_footprint_filters
        );

        let pads = decoded_pads(&updated);
        assert_eq!(pads.iter().filter(|pad| pad.number == "1").count(), 2);
        assert!(pads.iter().filter(|pad| pad.number == "1").all(|pad| pad
            .net
            .as_ref()
            .map(|net| net.name.as_str())
            == Some("ROW1")));
        assert_eq!(
            pads.iter()
                .find(|pad| pad.number == "2")
                .and_then(|pad| pad.net.as_ref())
                .map(|net| net.name.as_str()),
            Some("COL1")
        );
        assert!(pads
            .iter()
            .find(|pad| pad.number == "3")
            .unwrap()
            .net
            .is_none());
        let flipped_pad = pads
            .iter()
            .find(|pad| pad.number == "3")
            .expect("new pad 3");
        let flipped_stack = flipped_pad.pad_stack.as_ref().expect("pad stack");
        assert_eq!(
            flipped_stack.layers,
            vec![kiapi::board::types::BoardLayer::BlBCu as i32],
            "a front-copper library pad moves to back copper with a B.Cu instance"
        );
        let expected = konnect_sexp::geometry::transform_pad(0.0, -3.0, 100.0, 50.0, 37.0);
        let actual = flipped_pad.position.as_ref().expect("pad position");
        let actual = (
            builders::nm_to_mm(actual.x_nm),
            builders::nm_to_mm(actual.y_nm),
        );
        assert!((actual.0 - expected.0).abs() <= 0.000_001);
        assert!((actual.1 - expected.1).abs() <= 0.000_001);
        assert!(prepared.changed_domains.contains(&ChangedDomain::Pads));
        assert!(prepared.changed_domains.contains(&ChangedDomain::Graphics));
        assert!(prepared.changed_domains.contains(&ChangedDomain::Metadata));
        assert!(prepared.changed_domains.contains(&ChangedDomain::Models));
    }

    #[test]
    fn merge_conflicts_when_library_removes_a_connected_logical_pad() {
        let current = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        let without_pad_two = LIBRARY_FOOTPRINT.replace(
            "  (pad \"2\" thru_hole circle (at 2 0) (size 3 3)\n    (layers \"*.Cu\" \"*.Mask\") (drill 1))\n",
            "",
        );
        let library = parse_library_footprint("Test:Socket", &without_pad_two).unwrap();

        let error = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::from(["COL1".to_string()]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("connected pad 2"), "{error:#}");
    }

    #[test]
    fn parser_rejects_unsupported_children_before_any_update_can_be_built() {
        let unsupported = LIBRARY_FOOTPRINT.replace(
            "  (model \"../models/Socket.step\"",
            "  (zone (net 0) (layers \"F.Cu\"))\n  (model \"../models/Socket.step\"",
        );

        let error = parse_library_footprint("Test:Socket", &unsupported).unwrap_err();

        assert!(error.to_string().contains("zone"), "{error:#}");
    }

    #[test]
    fn refresh_refuses_an_unrepresentable_layer_before_building_the_update() {
        let unsupported = LIBRARY_FOOTPRINT.replace("(layer \"F.SilkS\"))", "(layer \"In99.Cu\"))");
        let library = parse_library_footprint("Test:Socket", &unsupported).unwrap();
        let current = current_instance(kiapi::board::types::BoardLayer::BlFCu);

        let error = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("In99.Cu"), "{message}");
        assert!(message.contains("request was not sent"), "{message}");
    }

    #[test]
    fn parser_rejects_lossy_graphic_and_pad_clauses() {
        let dashed = LIBRARY_FOOTPRINT.replace(
            "(stroke (width 0.12) (type solid))",
            "(stroke (width 0.12) (type dash))",
        );
        let error = parse_library_footprint("Test:Socket", &dashed).unwrap_err();
        assert!(error.to_string().contains("stroke type"), "{error:#}");

        let solder_margin = LIBRARY_FOOTPRINT.replace(
            "(roundrect_rratio 0.2))",
            "(roundrect_rratio 0.2) (solder_mask_margin 0.05))",
        );
        let error = parse_library_footprint("Test:Socket", &solder_margin).unwrap_err();
        assert!(
            error.to_string().contains("solder_mask_margin"),
            "{error:#}"
        );
    }

    #[test]
    fn rebuilding_an_applied_instance_is_a_noop_at_every_rotation_and_side() {
        let library = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let routed = BTreeSet::from(["ROW1".to_string(), "COL1".to_string()]);

        for layer in [
            kiapi::board::types::BoardLayer::BlFCu,
            kiapi::board::types::BoardLayer::BlBCu,
        ] {
            for rotation in [0.0, 90.0, 180.0, 270.0, 37.0] {
                let mut current = current_instance(layer);
                current.orientation = Some(kiapi::common::types::Angle {
                    value_degrees: rotation,
                });
                let first =
                    build_updated_instance(&current, &library, &net_codes, &routed).unwrap();
                let applied =
                    kiapi::board::types::FootprintInstance::decode(first.item.value.as_slice())
                        .unwrap();

                let second =
                    build_updated_instance(&applied, &library, &net_codes, &routed).unwrap();

                assert!(
                    second.changed_domains.is_empty(),
                    "{layer:?} at {rotation} degrees changed again: {:?}",
                    second.changed_domains
                );
            }
        }
    }

    #[test]
    fn kicad_roundtrip_defaults_do_not_create_false_pad_or_graphic_changes() {
        let library = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();
        let current = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        let prepared = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
        )
        .unwrap();
        let mut prepared =
            kiapi::board::types::FootprintInstance::decode(prepared.item.value.as_slice()).unwrap();
        let mut roundtripped = prepared.clone();
        roundtripped.definition.as_mut().unwrap().items.reverse();
        roundtripped.attributes.as_mut().unwrap().mounting_style =
            kiapi::board::types::FootprintMountingStyle::FmsUnspecified as i32;
        prepared.attributes.as_mut().unwrap().mounting_style =
            kiapi::board::types::FootprintMountingStyle::FmsUnknown as i32;

        for item in &mut roundtripped.definition.as_mut().unwrap().items {
            if item.type_url.ends_with("kiapi.board.types.Pad") {
                let mut pad = kiapi::board::types::Pad::decode(item.value.as_slice()).unwrap();
                pad.id = Some(kiapi::common::types::Kiid {
                    value: format!("kicad-pad-{}", pad.number),
                });
                pad.net = Some(kiapi::board::types::Net::default());
                pad.pad_to_die_length = Some(kiapi::common::types::Distance { value_nm: 0 });
                pad.symbol_pin = Some(kiapi::board::types::SymbolPinInfo::default());
                pad.pad_to_die_delay = Some(kiapi::common::types::Time { value_as: 0 });
                let stack = pad.pad_stack.as_mut().unwrap();
                stack.layers.reverse();
                if stack.drill.is_none() {
                    stack.drill = Some(kiapi::board::types::DrillProperties {
                        start_layer: kiapi::board::types::BoardLayer::BlUndefined as i32,
                        end_layer: kiapi::board::types::BoardLayer::BlUndefined as i32,
                        diameter: Some(builders::vec2(0.0, 0.0)),
                        shape: kiapi::board::types::DrillShape::DsUndefined as i32,
                        ..Default::default()
                    });
                }
                stack.zone_settings = Some(kiapi::board::types::ZoneConnectionSettings {
                    zone_connection: kiapi::board::types::ZoneConnectionStyle::ZcsInherited as i32,
                    thermal_spokes: Some(kiapi::board::types::ThermalSpokeSettings {
                        angle: Some(kiapi::common::types::Angle {
                            value_degrees: 90.0,
                        }),
                        ..Default::default()
                    }),
                });
                *item = builders::pack_any(&pad, "kiapi.board.types.Pad");
            } else if item
                .type_url
                .ends_with("kiapi.board.types.BoardGraphicShape")
            {
                let mut graphic =
                    kiapi::board::types::BoardGraphicShape::decode(item.value.as_slice()).unwrap();
                graphic.id = Some(kiapi::common::types::Kiid {
                    value: "kicad-graphic".to_string(),
                });
                graphic.net = Some(kiapi::board::types::Net::default());
                graphic
                    .shape
                    .as_mut()
                    .unwrap()
                    .attributes
                    .as_mut()
                    .unwrap()
                    .stroke
                    .as_mut()
                    .unwrap()
                    .style = kiapi::common::types::StrokeLineStyle::SlsSolid as i32;
                if let Some(kiapi::common::types::graphic_shape::Geometry::Polygon(polyset)) =
                    graphic
                        .shape
                        .as_mut()
                        .and_then(|shape| shape.geometry.as_mut())
                {
                    if let Some(outline) = polyset
                        .polygons
                        .first_mut()
                        .and_then(|polygon| polygon.outline.as_mut())
                    {
                        outline.nodes.rotate_left(1);
                        outline.nodes.reverse();
                    }
                }
                if let Some(kiapi::common::types::graphic_shape::Geometry::Rectangle(rectangle)) =
                    graphic
                        .shape
                        .as_mut()
                        .and_then(|shape| shape.geometry.as_mut())
                {
                    rectangle.corner_radius = Some(kiapi::common::types::Distance { value_nm: 0 });
                }
                *item = builders::pack_any(&graphic, "kiapi.board.types.BoardGraphicShape");
            }
        }

        let roundtripped_pads = normalized_items(roundtripped.definition.as_ref().unwrap(), "Pad")
            .unwrap()
            .into_iter()
            .map(|bytes| kiapi::board::types::Pad::decode(bytes.as_slice()).unwrap())
            .collect::<Vec<_>>();
        let prepared_pads = normalized_items(prepared.definition.as_ref().unwrap(), "Pad")
            .unwrap()
            .into_iter()
            .map(|bytes| kiapi::board::types::Pad::decode(bytes.as_slice()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(roundtripped_pads, prepared_pads);
        assert!(changed_domains(&roundtripped, &prepared)
            .unwrap()
            .is_empty());
    }

    fn plan_item(reference: &str, library_id: Option<&str>, kiid: &str) -> prost_types::Any {
        let mut instance = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        instance.id = Some(kiapi::common::types::Kiid {
            value: kiid.to_string(),
        });
        instance.reference_field = Some(field("Reference", reference, 101.0, 48.0, true));
        if let Some(definition) = instance.definition.as_mut() {
            definition.reference_field = instance.reference_field.clone();
            definition.id = library_id.map(|library_id| {
                let (nickname, entry) = library_id.split_once(':').unwrap();
                kiapi::common::types::LibraryIdentifier {
                    library_nickname: nickname.to_string(),
                    entry_name: entry.to_string(),
                }
            });
        }
        builders::pack_any(&instance, "kiapi.board.types.FootprintInstance")
    }

    fn plan_fixture() -> (tempfile::TempDir, std::path::PathBuf, Vec<prost_types::Any>) {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20240108))").unwrap();
        let library_dir = temp.path().join("Test.pretty");
        std::fs::create_dir(&library_dir).unwrap();
        std::fs::write(library_dir.join("Socket.kicad_mod"), LIBRARY_FOOTPRINT).unwrap();
        std::fs::write(
            temp.path().join("fp-lib-table"),
            "(fp_lib_table (lib (name \"Test\") (type \"KiCad\") (uri \"${KIPRJMOD}/Test.pretty\") (options \"\") (descr \"\")))",
        )
        .unwrap();
        let items = vec![
            plan_item("SW2", Some("Test:Socket"), "kiid-2"),
            plan_item("TP1", None, "kiid-3"),
            plan_item("SW1", Some("Test:Socket"), "kiid-1"),
        ];
        (temp, board, items)
    }

    #[test]
    fn filters_are_normalized_and_supplied_empty_arrays_select_nothing() {
        let filters = parse_filters(&serde_json::json!({
            "references": ["SW2", "SW1", "SW2"],
            "library_ids": ["Test:Socket", "Test:Socket"]
        }))
        .unwrap();
        assert_eq!(
            filters.references,
            Some(BTreeSet::from(["SW1".to_string(), "SW2".to_string()]))
        );
        assert_eq!(
            filters.library_ids,
            Some(BTreeSet::from(["Test:Socket".to_string()]))
        );

        let (_, board, items) = plan_fixture();
        let empty = plan_updates(
            &board,
            &items,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": [] })).unwrap(),
        );
        assert_eq!(empty.status, PlanStatus::Noop);
        assert_eq!(empty.coverage.selected.planned, 0);
        assert!(empty.changes.is_empty());

        let malformed =
            parse_filters(&serde_json::json!({ "library_ids": ["not-a-library-id"] })).unwrap_err();
        assert_eq!(malformed.field, "library_ids");
        assert!(malformed.reason.contains("Library:Footprint"));
    }

    #[test]
    fn planner_selects_deterministically_and_intersects_filters() {
        let (_temp, board, items) = plan_fixture();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let all = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &UpdateFilters::default(),
        );
        assert_eq!(all.status, PlanStatus::Ready);
        assert_eq!(all.coverage.selected.planned, 2);
        assert_eq!(all.coverage.changed.planned, 2);
        assert_eq!(all.coverage.skipped_unlinked.planned, 1);
        assert_eq!(
            all.changes
                .iter()
                .map(|change| change.reference.as_str())
                .collect::<Vec<_>>(),
            vec!["SW1", "SW2"]
        );

        let intersection = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({
                "references": ["SW2"],
                "library_ids": ["Other:Socket"]
            }))
            .unwrap(),
        );
        assert_eq!(intersection.status, PlanStatus::Noop);
        assert_eq!(intersection.coverage.selected.planned, 0);
    }

    #[test]
    fn planner_conflicts_on_missing_or_duplicate_selected_references() {
        let (_temp, board, mut items) = plan_fixture();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let missing = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": ["SW404"] })).unwrap(),
        );
        assert_eq!(missing.status, PlanStatus::Conflict);
        assert!(missing
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "reference_not_found"));

        items.push(plan_item("SW1", Some("Test:Socket"), "kiid-duplicate"));
        let duplicate = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": ["SW1"] })).unwrap(),
        );
        assert_eq!(duplicate.status, PlanStatus::Conflict);
        assert!(duplicate
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "duplicate_reference"));
        assert!(duplicate.prepared_items.is_empty());
    }

    #[test]
    fn explicitly_selected_unlinked_footprint_is_a_conflict() {
        let (_temp, board, items) = plan_fixture();
        let plan = plan_updates(
            &board,
            &items,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": ["TP1"] })).unwrap(),
        );

        assert_eq!(plan.status, PlanStatus::Conflict);
        assert!(plan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unlinked_footprint"));
        assert!(plan.prepared_items.is_empty());
    }

    #[test]
    fn planner_returns_noop_after_the_prepared_update_is_applied() {
        let (_temp, board, items) = plan_fixture();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let first = plan_updates(
            &board,
            &[items[2].clone()],
            &net_codes,
            &BTreeSet::new(),
            &UpdateFilters::default(),
        );
        assert_eq!(first.status, PlanStatus::Ready);

        let second = plan_updates(
            &board,
            &first.prepared_items,
            &net_codes,
            &BTreeSet::new(),
            &UpdateFilters::default(),
        );

        assert_eq!(second.status, PlanStatus::Noop);
        assert_eq!(second.coverage.selected.planned, 1);
        assert_eq!(second.coverage.changed.planned, 0);
        assert_eq!(second.coverage.unchanged.planned, 1);
        assert!(second.prepared_items.is_empty());
    }

    #[test]
    fn datasheet_only_library_change_is_reported_as_metadata() {
        let base = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();
        let current = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let routed = BTreeSet::new();
        let applied = build_updated_instance(&current, &base, &net_codes, &routed).unwrap();
        let applied =
            kiapi::board::types::FootprintInstance::decode(applied.item.value.as_slice()).unwrap();
        let changed_source =
            LIBRARY_FOOTPRINT.replace("new-datasheet.pdf", "replacement-datasheet.pdf");
        let changed = parse_library_footprint("Test:Socket", &changed_source).unwrap();

        let prepared = build_updated_instance(&applied, &changed, &net_codes, &routed).unwrap();

        assert_eq!(
            prepared.changed_domains,
            BTreeSet::from([ChangedDomain::Metadata])
        );
    }

    #[test]
    fn plan_revision_tracks_selected_board_library_and_filter_inputs_only() {
        let (_temp, board, mut items) = plan_fixture();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let filters = parse_filters(&serde_json::json!({ "references": ["SW1"] })).unwrap();
        let first = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        let identical = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        assert_eq!(first.plan_revision, identical.plan_revision);

        items[0] = plan_item("SW2", Some("Test:Socket"), "unselected-changed");
        let unrelated = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        assert_eq!(first.plan_revision, unrelated.plan_revision);

        items[2] = plan_item("SW1", Some("Test:Socket"), "selected-changed");
        let board_changed = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        assert_ne!(first.plan_revision, board_changed.plan_revision);

        std::fs::write(
            board.parent().unwrap().join("Test.pretty/Socket.kicad_mod"),
            LIBRARY_FOOTPRINT.replace("updated description", "library changed"),
        )
        .unwrap();
        let library_changed = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        assert_ne!(board_changed.plan_revision, library_changed.plan_revision);

        let different_filter = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": ["SW2"] })).unwrap(),
        );
        assert_ne!(
            library_changed.plan_revision,
            different_filter.plan_revision
        );
    }

    fn test_context(ipc_address: &str) -> crate::tools::ToolContext {
        crate::tools::ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: ipc_address.to_string(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn result_json(result: &crate::mcp::protocol::CallToolResult) -> serde_json::Value {
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        serde_json::from_str(text).expect("tool result must be JSON")
    }

    #[test]
    fn tool_schema_defaults_to_dry_run_and_requires_only_the_board() {
        let definition = tool();

        assert_eq!(definition.name, "update_footprints_from_library");
        assert_eq!(
            definition.input_schema["required"],
            serde_json::json!(["board"])
        );
        assert_eq!(
            definition.input_schema["properties"]["dry_run"]["default"],
            true
        );
        assert_eq!(
            definition.input_schema["properties"]["references"]["items"]["type"],
            "string"
        );
        assert_eq!(
            definition.input_schema["properties"]["library_ids"]["items"]["type"],
            "string"
        );
    }

    #[tokio::test]
    async fn apply_without_a_reviewed_revision_is_rejected_before_ipc() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20240108))").unwrap();

        let result = handle_update_footprints_from_library(
            &serde_json::json!({ "board": board, "dry_run": false }),
            &test_context(""),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
        let text = match result.content.first().unwrap() {
            crate::mcp::protocol::ToolContent::Text { text } => text,
            _ => unreachable!(),
        };
        assert!(text.contains("expected_plan_revision"));
    }

    #[tokio::test]
    async fn unreachable_ipc_returns_a_non_mutating_conflict_response() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        let before = "(kicad_pcb (version 20240108))";
        std::fs::write(&board, before).unwrap();

        let result = handle_update_footprints_from_library(
            &serde_json::json!({ "board": board }),
            &test_context(""),
        )
        .await
        .unwrap();

        assert!(!result.is_error);
        let response = result_json(&result);
        assert_eq!(response["status"], "conflict");
        assert_eq!(response["coverage"]["transport"], "live_kicad_ipc");
        assert!(response["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("live-IPC-only"));
        assert_eq!(std::fs::read_to_string(board).unwrap(), before);
    }

    #[derive(Default)]
    struct MutationCapture {
        begin_count: usize,
        update_batches: Vec<usize>,
        commit_actions: Vec<kiapi::common::commands::CommitAction>,
    }

    struct PlannerMock {
        url: String,
        capture: std::sync::Arc<std::sync::Mutex<MutationCapture>>,
        _thread: std::thread::JoinHandle<()>,
    }

    fn api_ok() -> kiapi::common::ApiResponse {
        kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsOk as i32,
                error_message: String::new(),
            }),
            header: None,
            message: None,
        }
    }

    fn api_reply(message: prost_types::Any) -> kiapi::common::ApiResponse {
        kiapi::common::ApiResponse {
            message: Some(message),
            ..api_ok()
        }
    }

    fn spawn_planner_mock(
        board: &Path,
        footprint: prost_types::Any,
        fail_update: bool,
    ) -> PlannerMock {
        use nng::options::Options;

        static NEXT_MOCK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://footprint-update-{}",
            NEXT_MOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).unwrap();
        socket
            .set_opt::<nng::options::RecvTimeout>(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        socket.listen(&url).unwrap();

        let board_name = board.to_string_lossy().to_string();
        let capture = std::sync::Arc::new(std::sync::Mutex::new(MutationCapture::default()));
        let captured = capture.clone();
        let thread = std::thread::spawn(move || {
            while let Ok(message) = socket.recv() {
                let request = kiapi::common::ApiRequest::decode(message.as_slice()).unwrap();
                let message = request.message.expect("request message");
                let response = if message.type_url.ends_with("GetOpenDocuments") {
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![kiapi::common::types::DocumentSpecifier {
                                r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                                identifier: Some(
                                    kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                        board_name.clone(),
                                    ),
                                ),
                                project: None,
                            }],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    ))
                } else if message.type_url.ends_with("GetItems") {
                    let command =
                        kiapi::common::commands::GetItems::decode(message.value.as_slice())
                            .unwrap();
                    let items = if command.types
                        == vec![kiapi::common::types::KiCadObjectType::KotPcbFootprint as i32]
                    {
                        vec![footprint.clone()]
                    } else {
                        Vec::new()
                    };
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::GetItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            items,
                        },
                        "kiapi.common.commands.GetItemsResponse",
                    ))
                } else if message.type_url.ends_with("GetNets") {
                    api_reply(builders::pack_any(
                        &kiapi::board::commands::NetsResponse {
                            nets: vec![builders::net("ROW1", 11), builders::net("COL1", 12)],
                        },
                        "kiapi.board.commands.NetsResponse",
                    ))
                } else if message.type_url.ends_with("BeginCommit") {
                    captured.lock().unwrap().begin_count += 1;
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::BeginCommitResponse {
                            id: Some(kiapi::common::types::Kiid {
                                value: "commit-1".to_string(),
                            }),
                        },
                        "kiapi.common.commands.BeginCommitResponse",
                    ))
                } else if message.type_url.ends_with("UpdateItems") {
                    let update =
                        kiapi::common::commands::UpdateItems::decode(message.value.as_slice())
                            .unwrap();
                    captured
                        .lock()
                        .unwrap()
                        .update_batches
                        .push(update.items.len());
                    let updated_items = update
                        .items
                        .into_iter()
                        .map(|item| kiapi::common::commands::ItemUpdateResult {
                            status: Some(kiapi::common::commands::ItemStatus {
                                code: if fail_update {
                                    kiapi::common::commands::ItemStatusCode::IscInvalidData as i32
                                } else {
                                    kiapi::common::commands::ItemStatusCode::IscOk as i32
                                },
                                error_message: if fail_update {
                                    "mock update failure".to_string()
                                } else {
                                    String::new()
                                },
                            }),
                            item: Some(item),
                        })
                        .collect();
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::UpdateItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            updated_items,
                        },
                        "kiapi.common.commands.UpdateItemsResponse",
                    ))
                } else if message.type_url.ends_with("EndCommit") {
                    let end = kiapi::common::commands::EndCommit::decode(message.value.as_slice())
                        .unwrap();
                    captured.lock().unwrap().commit_actions.push(end.action());
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::EndCommitResponse {},
                        "kiapi.common.commands.EndCommitResponse",
                    ))
                } else {
                    panic!("unexpected mock request {}", message.type_url);
                };
                let response = nng::Message::from(response.encode_to_vec().as_slice());
                if socket.send(response).is_err() {
                    break;
                }
            }
        });
        PlannerMock {
            url,
            capture,
            _thread: thread,
        }
    }

    #[tokio::test]
    async fn dry_run_and_stale_revision_never_open_a_commit_or_send_updates() {
        let (_temp, board, items) = plan_fixture();
        let mock = spawn_planner_mock(&board, items[2].clone(), false);
        let context = test_context(&mock.url);

        let dry_run =
            handle_update_footprints_from_library(&serde_json::json!({ "board": board }), &context)
                .await
                .unwrap();
        assert_eq!(result_json(&dry_run)["status"], "ready");

        let stale = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board,
                "dry_run": false,
                "expected_plan_revision": "stale"
            }),
            &context,
        )
        .await
        .unwrap();
        let stale = result_json(&stale);
        assert_eq!(stale["status"], "conflict");
        assert_eq!(stale["diagnostics"][0]["code"], "stale_plan_revision");

        let capture = mock.capture.lock().unwrap();
        assert_eq!(capture.begin_count, 0);
        assert!(capture.update_batches.is_empty());
        assert!(capture.commit_actions.is_empty());
    }

    #[tokio::test]
    async fn apply_sends_one_batch_in_one_commit() {
        let (_temp, board, items) = plan_fixture();
        let mock = spawn_planner_mock(&board, items[2].clone(), false);
        let context = test_context(&mock.url);
        let dry_run =
            handle_update_footprints_from_library(&serde_json::json!({ "board": board }), &context)
                .await
                .unwrap();
        let revision = result_json(&dry_run)["plan_revision"]
            .as_str()
            .unwrap()
            .to_string();

        let applied = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board,
                "dry_run": false,
                "expected_plan_revision": revision
            }),
            &context,
        )
        .await
        .unwrap();

        let applied = result_json(&applied);
        assert_eq!(applied["status"], "applied");
        assert_eq!(applied["coverage"]["changed"]["applied"], 1);
        let capture = mock.capture.lock().unwrap();
        assert_eq!(capture.begin_count, 1);
        assert_eq!(capture.update_batches, vec![1]);
        assert_eq!(
            capture.commit_actions,
            vec![kiapi::common::commands::CommitAction::CmaCommit]
        );
    }

    #[tokio::test]
    async fn failed_update_drops_the_single_commit() {
        let (_temp, board, items) = plan_fixture();
        let mock = spawn_planner_mock(&board, items[2].clone(), true);
        let context = test_context(&mock.url);
        let dry_run =
            handle_update_footprints_from_library(&serde_json::json!({ "board": board }), &context)
                .await
                .unwrap();
        let revision = result_json(&dry_run)["plan_revision"]
            .as_str()
            .unwrap()
            .to_string();

        let failed = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board,
                "dry_run": false,
                "expected_plan_revision": revision
            }),
            &context,
        )
        .await
        .unwrap();

        assert_eq!(result_json(&failed)["status"], "conflict");
        let capture = mock.capture.lock().unwrap();
        assert_eq!(capture.begin_count, 1);
        assert_eq!(capture.update_batches, vec![1]);
        assert_eq!(
            capture.commit_actions,
            vec![kiapi::common::commands::CommitAction::CmaDrop]
        );
    }
}
