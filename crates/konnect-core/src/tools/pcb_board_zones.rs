//! Revision-bound inventory and exact deletion for PCB zones.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::board_source::{self, BoardRead, BoardSource};
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use konnect_ipc::client::KiCadIpcClient;
use konnect_ipc::gen::kiapi;
use konnect_sexp::{parse_sexp, read_consistent, transact_atomic, SexpNode};
use prost::Message;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
enum ZoneKind {
    Copper,
    RuleArea,
    Unsupported(&'static str),
}

impl ZoneKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Copper => "copper",
            Self::RuleArea => "rule_area",
            Self::Unsupported(kind) => kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ZoneRecord {
    uuid: String,
    name: String,
    kind: ZoneKind,
    layers: Vec<String>,
    net: Option<String>,
    span: Option<(usize, usize)>,
    filled: Option<bool>,
}

#[derive(Debug, Clone)]
struct ZoneInventory {
    revision: String,
    zones: Vec<ZoneRecord>,
}

pub(crate) fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "list_zones",
            "List copper fill zones and rule areas on the exact live or saved board. Returns stable UUIDs, names, kind, layers, net, source, and a revision token for a later exact deletion. A name filter limits returned rows only; the revision always covers the complete board.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "name": { "type": "string", "description": "Optional exact zone name filter" },
                    "board_source": board_source::board_source_schema()
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_list_zones(args, ctx).await }
        ).with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
        tool!(
            "delete_zone",
            "Delete exactly one previously observed copper zone or rule area by UUID. Pass the source and revision returned by list_zones; stale observations and unsupported zone kinds are refused. The live deletion uses one undoable KiCad commit; refill is requested afterward and verified separately. The saved-file path checks the revision under the atomic document lock and verifies the replacement.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "uuid": { "type": "string", "description": "Exact observed zone UUID" },
                    "revision": { "type": "string", "description": "Revision returned by list_zones" },
                    "source": { "type": "string", "enum": ["ipc", "saved_board"], "description": "Exact source returned by list_zones" }
                },
                "required": ["board", "uuid", "revision", "source"]
            }),
            |args, ctx| async move { handle_delete_zone(args, ctx).await }
        ).with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
    ]
}

async fn handle_list_zones(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let board_path = match canonical_board(args) {
        Ok(path) => path,
        Err(result) => return Ok(result),
    };
    let name = match optional_name(args) {
        Ok(name) => name,
        Err(result) => return Ok(result),
    };
    let source = match BoardSource::from_args(args) {
        Ok(source) => source,
        Err(result) => return Ok(result),
    };
    let observed = board_source::read_board(ctx, &board_path, source, "zone inventory", {
        let path = board_path.clone();
        move |client, document| live_inventory(client, document, &path)
    })
    .await?;
    let (inventory, source_name, source_evidence) = match observed {
        BoardRead::Live(inventory) => (inventory, "ipc", board_source::live_evidence()),
        BoardRead::Saved(evidence) => {
            let inventory = saved_inventory(&board_path)?;
            (inventory, "saved_board", evidence.evidence())
        }
        BoardRead::Refused(result) => return Ok(result),
    };
    let zones = inventory
        .zones
        .iter()
        .filter(|zone| matches!(zone.kind, ZoneKind::Copper | ZoneKind::RuleArea))
        .filter(|zone| name.as_ref().is_none_or(|wanted| zone.name == *wanted))
        .map(zone_json)
        .collect::<Vec<_>>();
    Ok(CallToolResult::json(&json!({
        "count": zones.len(),
        "zones": zones,
        "excluded_unsupported_count": inventory.zones.iter().filter(|zone| matches!(zone.kind, ZoneKind::Unsupported(_))).count(),
        "revision": inventory.revision,
        "source": source_name,
        "source_evidence": source_evidence,
    })))
}

async fn handle_delete_zone(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let result = delete_zone(args, ctx).await?;
    if crate::outcome::status(&result).is_some() {
        return Ok(result);
    }
    let uncertain = matches!(
        crate::mcp::error::extract_error_kind(&result).as_deref(),
        Some("mutation_outcome_uncertain" | "ipc_outcome_unknown")
    );
    let status = if uncertain {
        crate::outcome::OutcomeStatus::Uncertain
    } else {
        crate::outcome::OutcomeStatus::Failed
    };
    Ok(crate::outcome::attach(
        result,
        crate::outcome::summary(
            status,
            format!(
                "{} (zone {})",
                args["board"].as_str().unwrap_or("unknown board"),
                args["uuid"].as_str().unwrap_or("unknown UUID")
            ),
            args["source"].as_str().unwrap_or("unresolved"),
            1,
            0,
            1,
            Some(if uncertain {
                crate::outcome::inspect_before_retry()
            } else {
                crate::outcome::retry_whole_request()
            }),
        ),
    ))
}

async fn delete_zone(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let board_path = match canonical_board(args) {
        Ok(path) => path,
        Err(result) => return Ok(result),
    };
    let uuid = match require_str(args, "uuid") {
        Ok(value) => value.to_string(),
        Err(result) => return Ok(result),
    };
    let revision = match require_str(args, "revision") {
        Ok(value) => value.to_string(),
        Err(result) => return Ok(result),
    };
    let source = match require_str(args, "source") {
        Ok(value) if value == "ipc" || value == "saved_board" => value.to_string(),
        Ok(value) => return Ok(invalid_source(value)),
        Err(result) => return Ok(result),
    };

    let expected_source = source.clone();
    let ipc_board = board_path.clone();
    let target_uuid = uuid.clone();
    let expected_revision = revision.clone();
    let attempt = crate::tools::pcb_board::attempt_ipc_write(
        ctx,
        &board_path,
        "zone deletion",
        move |client| {
            if expected_source == "saved_board" {
                // A saved observation may never silently switch to unsaved live state.
                return Ok(None);
            }
            let document = client.find_open_board(&ipc_board)?;
            let initial = live_inventory(client, document.clone(), &ipc_board)?;
            if let Err(error) = ensure_revision(&initial.revision, &expected_revision, &target_uuid)
            {
                return Ok(Some(LiveAttempt::Refused(stale_result(
                    &target_uuid,
                    &format!("{error:#}"),
                ))));
            }
            let target = match exact_zone(&initial.zones, &target_uuid) {
                Ok(target) => target,
                Err(error) => {
                    return Ok(Some(LiveAttempt::Refused(stale_result(
                        &target_uuid,
                        &format!("{error:#}"),
                    ))))
                }
            };

            let delete_uuid = target_uuid.clone();
            let transaction_revision = expected_revision.clone();
            let path = ipc_board.clone();
            let mut prewrite_refusal = None;
            let transaction =
                client.run_commit_recovering_in(document.clone(), "Delete PCB zone", |client| {
                    // Re-check while the KiCad commit is open. The earlier inventory
                    // exists for user-facing details; this is the actual prewrite gate.
                    let current = live_inventory(client, document.clone(), &path)?;
                    if let Err(error) =
                        ensure_revision(&current.revision, &transaction_revision, &delete_uuid)
                    {
                        prewrite_refusal = Some(stale_result(&delete_uuid, &format!("{error:#}")));
                        anyhow::bail!(
                            "zone observation became stale before the KiCad delete commit"
                        );
                    }
                    if let Err(error) = exact_zone(&current.zones, &delete_uuid) {
                        prewrite_refusal = Some(stale_result(&delete_uuid, &format!("{error:#}")));
                        anyhow::bail!("requested zone is no longer a supported exact target");
                    }
                    client.delete_items_in(document, vec![delete_uuid.clone()])
                });
            if let Some(result) = prewrite_refusal {
                return match transaction {
                    Err(error)
                        if error
                            .downcast_ref::<konnect_ipc::client::IpcOutcomeUnknown>()
                            .is_some() =>
                    {
                        Err(error)
                    }
                    Err(_) => Ok(Some(LiveAttempt::Refused(result))),
                    Ok(()) => Err(anyhow::anyhow!(
                        "stale zone gate unexpectedly committed without deleting"
                    )),
                };
            }
            transaction?;

            let refill = client.refill_zones();
            // KiCad schedules the actual fill after acknowledging RefillZones.
            // Allow the same settling interval used by the official API client,
            // then require readiness and two matching complete snapshots below.
            if refill.is_ok() {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            let readback = read_settled_inventory(std::time::Duration::from_secs(30), || {
                let document = client.find_open_board(&ipc_board)?;
                live_inventory(client, document, &ipc_board)
            });
            let readiness = readback
                .as_ref()
                .map(|_| ())
                .map_err(|error| anyhow::anyhow!("{error:#}"));
            Ok(Some(LiveAttempt::Deleted(live_delete_evidence(
                &initial, &target, refill, readiness, readback,
            ))))
        },
    )
    .await?;

    match attempt {
        crate::tools::pcb_board::BoardWrite::Ipc(Some(LiveAttempt::Deleted(evidence))) => Ok(evidence.into_result(&board_path)),
        crate::tools::pcb_board::BoardWrite::Ipc(Some(LiveAttempt::Refused(result))) => Ok(result),
        crate::tools::pcb_board::BoardWrite::Ipc(None) => Ok(stale_result(
            &uuid,
            "the observation source was saved_board, but the exact board is now live in KiCad",
        )),
        crate::tools::pcb_board::BoardWrite::File(reason) if source == "saved_board" => {
            delete_saved_zone(&board_path, &uuid, &revision, reason.evidence())
        }
        crate::tools::pcb_board::BoardWrite::File(reason) => Ok(CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: uuid.clone(),
                reason: format!(
                    "the observed source was IPC, but KiCad no longer holds this board; {}",
                    reason.premise()
                ),
            },
            "delete_zone requires the exact live board named by the list_zones observation. Run list_zones again and retry with its current source and revision.".to_string(),
        )),
        crate::tools::pcb_board::BoardWrite::Refused(result) => Ok(result),
    }
}

enum LiveAttempt {
    Deleted(LiveDeleteEvidence),
    Refused(CallToolResult),
}

#[derive(Debug)]
struct LiveDeleteEvidence {
    record: ZoneRecord,
    deleted: Option<bool>,
    refill_ok: bool,
    refill_error: Option<String>,
    readiness_ok: bool,
    readiness_error: Option<String>,
    readback_ok: bool,
    readback_error: Option<String>,
    unrelated_preserved: Option<bool>,
    remaining_copper_filled: Option<bool>,
}

impl LiveDeleteEvidence {
    fn into_result(self, board_path: &Path) -> CallToolResult {
        let complete = self.deleted == Some(true)
            && self.refill_ok
            && self.readiness_ok
            && self.readback_ok
            && self.unrelated_preserved == Some(true)
            && self.remaining_copper_filled == Some(true);
        let status = if complete {
            crate::outcome::OutcomeStatus::Complete
        } else if self.deleted == Some(true) && self.readback_ok {
            crate::outcome::OutcomeStatus::Partial
        } else {
            crate::outcome::OutcomeStatus::Uncertain
        };
        let refill_completed = self.refill_ok
            && self.readiness_ok
            && self.readback_ok
            && self.unrelated_preserved == Some(true)
            && self.remaining_copper_filled == Some(true);
        let result = CallToolResult::json(&json!({
            "success": complete,
            "deleted": self.deleted,
            "refill_request_accepted": self.refill_ok,
            "refill_completed": refill_completed,
            "remaining_copper_filled": self.remaining_copper_filled,
            "board_ready_after_refill": self.readiness_ok,
            "readback_verified": self.readback_ok,
            "unrelated_zones_preserved": self.unrelated_preserved,
            "zone": zone_json(&self.record),
            "source": "ipc",
            "board": board_path,
            "diagnostic": self.refill_error.or(self.readiness_error).or(self.readback_error).or_else(|| (self.unrelated_preserved == Some(false)).then(|| "unrelated zones changed during deletion or refill".to_string()))
                .or_else(|| (self.remaining_copper_filled == Some(false)).then(|| "KiCad did not report every remaining copper zone as filled".to_string())),
        }));
        crate::outcome::attach(
            result,
            crate::outcome::summary(
                status,
                board_path.display().to_string(),
                "ipc",
                1,
                usize::from(complete),
                usize::from(!complete),
                (!complete).then(crate::outcome::inspect_before_retry),
            ),
        )
    }
}

fn live_delete_evidence(
    before: &ZoneInventory,
    target: &ZoneRecord,
    refill: anyhow::Result<()>,
    readiness: anyhow::Result<()>,
    readback: anyhow::Result<ZoneInventory>,
) -> LiveDeleteEvidence {
    match readback {
        Ok(after) => {
            let deleted = !after.zones.iter().any(|zone| zone.uuid == target.uuid);
            let expected: BTreeSet<_> = before
                .zones
                .iter()
                .filter(|zone| zone.uuid != target.uuid)
                .map(zone_invariant)
                .collect();
            let actual: BTreeSet<_> = after.zones.iter().map(zone_invariant).collect();
            let remaining_copper_filled = after
                .zones
                .iter()
                .filter(|zone| zone.kind == ZoneKind::Copper)
                .all(|zone| zone.filled == Some(true));
            LiveDeleteEvidence {
                record: target.clone(),
                deleted: Some(deleted),
                refill_ok: refill.is_ok(),
                refill_error: refill
                    .err()
                    .map(|error| format!("zone refill failed: {error:#}")),
                readiness_ok: readiness.is_ok(),
                readiness_error: readiness
                    .err()
                    .map(|error| format!("board did not become ready after refill: {error:#}")),
                readback_ok: deleted,
                readback_error: (!deleted)
                    .then(|| "the requested UUID is still present after deletion".to_string()),
                unrelated_preserved: Some(expected == actual),
                remaining_copper_filled: Some(remaining_copper_filled),
            }
        }
        Err(error) => LiveDeleteEvidence {
            record: target.clone(),
            deleted: None,
            refill_ok: refill.is_ok(),
            refill_error: refill
                .err()
                .map(|refill| format!("zone refill failed: {refill:#}")),
            readiness_ok: readiness.is_ok(),
            readiness_error: readiness
                .err()
                .map(|error| format!("board did not become ready after refill: {error:#}")),
            readback_ok: false,
            readback_error: Some(format!(
                "post-delete inventory could not be read: {error:#}"
            )),
            unrelated_preserved: None,
            remaining_copper_filled: None,
        },
    }
}

/// RefillZones is asynchronous: GetOpenDocuments can answer while item reads
/// still return AS_BUSY. Retry only read operations with that explicit response,
/// and require two stable complete inventories before reporting readiness.
fn read_settled_inventory(
    timeout: std::time::Duration,
    mut read: impl FnMut() -> anyhow::Result<ZoneInventory>,
) -> anyhow::Result<ZoneInventory> {
    let deadline = std::time::Instant::now() + timeout;
    let mut previous_revision = None;
    loop {
        match read() {
            Ok(inventory) => {
                if previous_revision.as_ref() == Some(&inventory.revision) {
                    return Ok(inventory);
                }
                previous_revision = Some(inventory.revision);
            }
            Err(error) => {
                let busy =
                    konnect_ipc::client::ApiStatusError::from_error(&error).is_some_and(|status| {
                        status.code == kiapi::common::ApiStatusCode::AsBusy as i32
                    });
                if !busy {
                    return Err(error);
                }
                previous_revision = None;
            }
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "post-refill zone inventory did not become readable and stable before the deadline"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn live_inventory(
    client: &KiCadIpcClient,
    document: kiapi::common::types::DocumentSpecifier,
    board_path: &Path,
) -> anyhow::Result<ZoneInventory> {
    let before = client.save_document_to_string_in(document.clone())?;
    let items = client.get_items_in(
        document.clone(),
        kiapi::common::types::KiCadObjectType::KotPcbZone,
    )?;
    let zones = decode_ipc_zones(&items)?;
    let snapshot_zones = parse_file_zones(&before)?;
    anyhow::ensure!(
        same_inventory(&zones, &snapshot_zones),
        "KiCad's zone item response does not match its complete serialized board snapshot"
    );
    let after = client.save_document_to_string_in(document)?;
    anyhow::ensure!(
        before == after,
        "KiCad board changed while the complete zone inventory was read"
    );
    Ok(ZoneInventory {
        revision: revision(board_path, "ipc", &before)?,
        zones,
    })
}

fn same_inventory(left: &[ZoneRecord], right: &[ZoneRecord]) -> bool {
    fn key(zone: &ZoneRecord) -> (&str, &str, &str, &Option<String>, &Vec<String>) {
        (
            &zone.uuid,
            zone.kind.as_str(),
            &zone.name,
            &zone.net,
            &zone.layers,
        )
    }
    let mut left = left
        .iter()
        .filter(|zone| !matches!(zone.kind, ZoneKind::Unsupported(_)))
        .map(key)
        .collect::<Vec<_>>();
    let mut right = right
        .iter()
        .filter(|zone| !matches!(zone.kind, ZoneKind::Unsupported(_)))
        .map(key)
        .collect::<Vec<_>>();
    left.sort();
    right.sort();
    left == right
}

fn zone_invariant(zone: &ZoneRecord) -> (&str, &str, &str, &Option<String>, &Vec<String>) {
    (
        zone.uuid.as_str(),
        zone.name.as_str(),
        zone.kind.as_str(),
        &zone.net,
        &zone.layers,
    )
}

fn block_tag(block: &str) -> Option<&str> {
    let open = block.find('(')?;
    let tail = &block[open + 1..];
    let end = tail.find(|ch: char| ch.is_whitespace() || ch == ')')?;
    Some(&tail[..end])
}

fn decode_ipc_zones(items: &[prost_types::Any]) -> anyhow::Result<Vec<ZoneRecord>> {
    use kiapi::board::types::{BoardLayer, Zone, ZoneType};
    let mut zones = Vec::with_capacity(items.len());
    let mut ids = BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        anyhow::ensure!(
            konnect_ipc::builders::any_is(item, "kiapi.board.types.Zone"),
            "KiCad returned an unexpected type URL for zone inventory item {index}: {}",
            item.type_url
        );
        let zone = Zone::decode(item.value.as_slice()).map_err(|error| {
            anyhow::anyhow!("KiCad returned an unreadable zone at index {index}: {error}")
        })?;
        let kind = match ZoneType::try_from(zone.r#type) {
            Ok(ZoneType::ZtCopper) => ZoneKind::Copper,
            Ok(ZoneType::ZtRuleArea) => ZoneKind::RuleArea,
            Ok(ZoneType::ZtGraphical) => ZoneKind::Unsupported("graphical"),
            Ok(ZoneType::ZtTeardrop) => ZoneKind::Unsupported("teardrop"),
            Ok(other) => anyhow::bail!(
                "zone at index {index} has unsupported KiCad type {}",
                other.as_str_name()
            ),
            Err(_) => anyhow::bail!(
                "zone at index {index} has unknown KiCad type {}",
                zone.r#type
            ),
        };
        let uuid = zone
            .id
            .as_ref()
            .map(|id| id.value.clone())
            .unwrap_or_default();
        if !matches!(&kind, ZoneKind::Unsupported("teardrop")) {
            anyhow::ensure!(
                !uuid.is_empty(),
                "KiCad returned a zone without a UUID at index {index}"
            );
            anyhow::ensure!(
                ids.insert(uuid.clone()),
                "KiCad returned duplicate zone UUID '{uuid}'"
            );
        }
        let layers = zone
            .layers
            .iter()
            .map(|value| {
                let layer = BoardLayer::try_from(*value).map_err(|_| {
                    anyhow::anyhow!("zone '{uuid}' has unknown board layer value {value}")
                })?;
                anyhow::ensure!(
                    layer != BoardLayer::BlUndefined,
                    "zone '{uuid}' has an undefined board layer"
                );
                let name = konnect_ipc::builders::layer_name(layer).ok_or_else(|| {
                    anyhow::anyhow!("zone '{uuid}' has no canonical name for layer {layer:?}")
                })?;
                Ok(name.to_string())
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let net = match &kind {
            ZoneKind::Copper => match zone.settings.as_ref() {
                Some(kiapi::board::types::zone::Settings::CopperSettings(settings)) => settings
                    .net
                    .as_ref()
                    .map(|net| net.name.clone())
                    .filter(|name| !name.is_empty()),
                _ => anyhow::bail!("copper zone '{uuid}' has no copper settings proof"),
            },
            ZoneKind::RuleArea => {
                anyhow::ensure!(
                    matches!(
                        zone.settings.as_ref(),
                        Some(kiapi::board::types::zone::Settings::RuleAreaSettings(_))
                    ),
                    "rule area '{uuid}' has no rule-area settings proof"
                );
                None
            }
            ZoneKind::Unsupported(_) => None,
        };
        let uuid = if uuid.is_empty() {
            format!("unsupported:teardrop:{index}")
        } else {
            uuid
        };
        zones.push(ZoneRecord {
            uuid,
            name: zone.name,
            kind,
            layers,
            net,
            span: None,
            filled: Some(zone.filled),
        });
    }
    Ok(zones)
}

fn saved_inventory(board_path: &Path) -> anyhow::Result<ZoneInventory> {
    let content = read_consistent(board_path)?;
    let zones = parse_file_zones(&content)?;
    Ok(ZoneInventory {
        revision: revision(board_path, "saved_board", &content)?,
        zones,
    })
}

fn parse_file_zones(content: &str) -> anyhow::Result<Vec<ZoneRecord>> {
    let root = parse_sexp(content)
        .map_err(|error| anyhow::anyhow!("board S-expression is malformed: {error}"))?;
    anyhow::ensure!(
        root.head() == Some("kicad_pcb"),
        "saved file is not a kicad_pcb document"
    );
    let mut zones = Vec::new();
    let mut ids = BTreeSet::new();
    for (start, end) in konnect_sexp::writer::find_direct_child_blocks(content, "kicad_pcb") {
        if block_tag(&content[start..end]) != Some("zone") {
            continue;
        }
        let node = parse_sexp(&content[start..end])
            .map_err(|error| anyhow::anyhow!("board zone block is malformed: {error}"))?;
        let attr = node
            .find("attr")
            .and_then(|value| value.get(1))
            .and_then(SexpNode::as_str);
        let teardrop = node
            .find("attr")
            .and_then(|value| value.find("teardrop"))
            .is_some();
        let mut layers = Vec::new();
        if let Some(layer) = node.find_str("layer") {
            layers.push(layer.to_string());
        }
        if let Some(layer_list) = node.find("layers") {
            for child in layer_list.children().unwrap_or(&[]).iter().skip(1) {
                let value = child
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("zone has malformed layer entry"))?;
                if !layers.iter().any(|existing| existing == value) {
                    layers.push(value.to_string());
                }
            }
        }
        let non_copper_layers =
            !layers.is_empty() && layers.iter().any(|layer| !layer.ends_with(".Cu"));
        let kind = if teardrop {
            ZoneKind::Unsupported("teardrop")
        } else if node.find("keepout").is_some() || node.find("placement").is_some() {
            ZoneKind::RuleArea
        } else if non_copper_layers {
            ZoneKind::Unsupported("graphical")
        } else {
            match attr {
                None | Some("copper") => ZoneKind::Copper,
                Some("graphical") => ZoneKind::Unsupported("graphical"),
                Some("teardrop") => ZoneKind::Unsupported("teardrop"),
                Some(other) => anyhow::bail!(
                    "zone '{}' has unsupported saved-file kind '{other}'",
                    node.find_str("uuid").unwrap_or("<missing>")
                ),
            }
        };
        let mut uuid = node.find_str("uuid").unwrap_or_default().to_string();
        if !matches!(&kind, ZoneKind::Unsupported("teardrop")) {
            anyhow::ensure!(
                !uuid.is_empty(),
                "saved board contains a zone without a UUID"
            );
            anyhow::ensure!(
                ids.insert(uuid.clone()),
                "saved board contains duplicate zone UUID '{uuid}'"
            );
        } else if uuid.is_empty() {
            uuid = format!("unsupported:teardrop:{}", zones.len());
        }
        anyhow::ensure!(
            !layers.is_empty() || matches!(&kind, ZoneKind::Unsupported("teardrop")),
            "zone '{uuid}' has no layer evidence"
        );
        let net = if kind == ZoneKind::Copper {
            match node.find("net") {
                None => None,
                Some(net_block) => {
                    let name = node
                        .find_str("net_name")
                        .or_else(|| net_block.get(1).and_then(SexpNode::as_str))
                        .ok_or_else(|| {
                            anyhow::anyhow!("copper zone '{uuid}' has malformed net evidence")
                        })?;
                    (!name.is_empty()).then(|| name.to_string())
                }
            }
        } else {
            None
        };
        zones.push(ZoneRecord {
            uuid,
            name: node.find_str("name").unwrap_or_default().to_string(),
            kind,
            layers,
            net,
            span: Some(
                konnect_sexp::writer::find_block_with_leading_whitespace(content, start)
                    .unwrap_or((start, end)),
            ),
            filled: None,
        });
    }
    Ok(zones)
}

fn delete_saved_zone(
    board_path: &Path,
    uuid: &str,
    expected_revision: &str,
    source_evidence: Value,
) -> anyhow::Result<CallToolResult> {
    let observed = match saved_inventory(board_path) {
        Ok(inventory) => inventory,
        Err(error) => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: format!("complete saved-board zone inventory failed: {error:#}"),
                },
                format!("Saved-board zone deletion refused before writing: {error:#}"),
            ))
        }
    };
    if let Err(error) = ensure_revision(&observed.revision, expected_revision, uuid) {
        return Ok(stale_result(uuid, &format!("{error:#}")));
    }
    let observed_target = match exact_zone(&observed.zones, uuid) {
        Ok(target) => target,
        Err(error) => return Ok(stale_result(uuid, &format!("{error:#}"))),
    };
    let expected_remaining: BTreeSet<_> = observed
        .zones
        .iter()
        .filter(|zone| zone.uuid != uuid)
        .map(zone_invariant)
        .collect();
    let mut deleted = None;
    let transaction = transact_atomic(board_path, |current| {
        let inventory = inventory_from_content(board_path, current).map_err(|error| {
            konnect_sexp::SexpError::Parse {
                offset: 0,
                message: format!("{error:#}"),
            }
        })?;
        ensure_revision(&inventory.revision, expected_revision, uuid).map_err(|error| {
            konnect_sexp::SexpError::Parse {
                offset: 0,
                message: format!("{error:#}"),
            }
        })?;
        let target =
            exact_zone(&inventory.zones, uuid).map_err(|error| konnect_sexp::SexpError::Parse {
                offset: 0,
                message: format!("{error:#}"),
            })?;
        let span = target.span.ok_or_else(|| konnect_sexp::SexpError::Parse {
            offset: 0,
            message: "saved zone lacks a source span".to_string(),
        })?;
        let next = konnect_sexp::apply_edits(
            current.to_string(),
            vec![konnect_sexp::SexpEdit::delete(span.0, span.1)],
        );
        deleted = Some(target);
        Ok((next, ()))
    });
    if let Err(error) = transaction {
        let message = error.to_string();
        if message.contains("stale_zone_observation:") {
            return Ok(stale_result(uuid, &message));
        }
        return Ok(uncertain_result(
            board_path,
            uuid,
            &format!("atomic saved-board transaction failed: {message}"),
        ));
    }
    let after = match saved_inventory(board_path) {
        Ok(inventory) => inventory,
        Err(error) => {
            return Ok(uncertain_result(
                board_path,
                uuid,
                &format!("postwrite saved-board inventory failed: {error:#}"),
            ))
        }
    };
    if after.zones.iter().any(|zone| zone.uuid == uuid) {
        return Ok(uncertain_result(
            board_path,
            uuid,
            "postwrite inventory still contains the requested UUID",
        ));
    }
    let actual: BTreeSet<_> = after.zones.iter().map(zone_invariant).collect();
    if actual != expected_remaining {
        return Ok(uncertain_result(
            board_path,
            uuid,
            "postwrite readback differs from the expected unrelated-zone inventory",
        ));
    }
    let record = deleted.unwrap_or(observed_target);
    let result = CallToolResult::json(&json!({
        "success": true,
        "deleted": true,
        "readback_verified": true,
        "unrelated_zones_preserved": actual == expected_remaining,
        "zone": zone_json(&record),
        "source": "saved_board",
        "source_evidence": source_evidence,
        "revision": after.revision,
    }));
    Ok(crate::outcome::attach(
        result,
        crate::outcome::summary(
            crate::outcome::OutcomeStatus::Complete,
            board_path.display().to_string(),
            "saved_board",
            1,
            1,
            0,
            None,
        ),
    ))
}

fn inventory_from_content(path: &Path, content: &str) -> anyhow::Result<ZoneInventory> {
    Ok(ZoneInventory {
        revision: revision(path, "saved_board", content)?,
        zones: parse_file_zones(content)?,
    })
}

fn exact_zone(zones: &[ZoneRecord], uuid: &str) -> anyhow::Result<ZoneRecord> {
    let mut matches = zones.iter().filter(|zone| zone.uuid == uuid);
    let zone = matches.next().ok_or_else(|| {
        stale(
            uuid,
            "UUID is no longer present in the complete zone inventory",
        )
    })?;
    anyhow::ensure!(
        matches.next().is_none(),
        "zone UUID '{uuid}' is ambiguous in the observed inventory"
    );
    anyhow::ensure!(
        !matches!(zone.kind, ZoneKind::Unsupported(_)),
        "zone '{uuid}' has unsupported kind '{}' and cannot be deleted",
        zone.kind.as_str()
    );
    Ok(zone.clone())
}

fn ensure_revision(actual: &str, expected: &str, uuid: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        actual == expected,
        "{}",
        stale(uuid, "board revision changed since list_zones")
    );
    Ok(())
}

fn stale(uuid: &str, reason: &str) -> anyhow::Error {
    anyhow::anyhow!("stale_zone_observation:{uuid}:{reason}")
}

fn stale_result(uuid: &str, reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::StaleTarget { target: uuid.to_string(), reason: reason.to_string() },
        format!("delete_zone refused for UUID '{uuid}': {reason}. Run list_zones again and use its current source and revision."),
    )
}

fn uncertain_result(board_path: &Path, uuid: &str, reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::MutationOutcomeUncertain {
            operation: "delete_zone".to_string(),
            path: board_path.display().to_string(),
            reason: format!("UUID '{uuid}': {reason}"),
        },
        format!("The saved-board zone deletion may have changed UUID '{uuid}', but readback could not prove the complete result ({reason}). Re-read the board before retrying."),
    )
}

fn revision(board_path: &Path, source: &str, snapshot: &str) -> anyhow::Result<String> {
    let canonical = board_path.canonicalize()?;
    let mut digest = Sha256::new();
    digest.update(source.as_bytes());
    digest.update([0]);
    digest.update(canonical.to_string_lossy().as_bytes());
    digest.update([0]);
    digest.update(snapshot.as_bytes());
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn zone_json(zone: &ZoneRecord) -> Value {
    json!({
        "uuid": zone.uuid,
        "name": zone.name,
        "kind": zone.kind.as_str(),
        "layers": zone.layers,
        "net": zone.net,
    })
}

fn canonical_board(args: &Value) -> Result<PathBuf, CallToolResult> {
    let path = get_path(args, "board").map_err(|error| CallToolResult::error(error.to_string()))?;
    path.canonicalize().map_err(|error| {
        CallToolResult::error(format!(
            "Could not resolve board path '{}': {error}",
            path.display()
        ))
    })
}

fn optional_name(args: &Value) -> Result<Option<String>, CallToolResult> {
    match args.get("name") {
        None => Ok(None),
        Some(Value::String(name)) => Ok(Some(name.clone())),
        Some(_) => Err(crate::tools::invalid_arg("name", "must be a string")),
    }
}

fn invalid_source(source: &str) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::InvalidArgument {
            field: "source".to_string(),
            reason: "must match list_zones source 'ipc' or 'saved_board'".to_string(),
        },
        format!("Unsupported zone observation source '{source}'."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_COPPER_BOARD: &str =
        include_str!("../../../konnect-sexp/tests/fixtures/zone_outline_elements.kicad_pcb");
    const REAL_RULE_AREA_BOARD: &str =
        include_str!("../../../konnect-ipc/tests/fixtures/live_ipc.kicad_pcb");

    fn ctx() -> ToolContext {
        use crate::router::ToolRouter;
        use crate::tools::ServerConfig;
        use std::sync::Arc;
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

    fn body(result: &CallToolResult) -> Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("text result expected")
        };
        serde_json::from_str(text).expect("JSON response")
    }

    fn path_with(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    fn rewrite_saved_zone(
        content: &str,
        index: usize,
        rewrite: impl FnOnce(&str) -> String,
    ) -> String {
        let blocks = konnect_sexp::writer::find_direct_child_blocks(content, "kicad_pcb")
            .into_iter()
            .filter(|(start, end)| block_tag(&content[*start..*end]) == Some("zone"))
            .collect::<Vec<_>>();
        let (start, end) = blocks[index];
        let replacement = rewrite(&content[start..end]);
        let mut edited = content.to_string();
        edited.replace_range(start..end, &replacement);
        edited
    }

    fn fixture_ipc_zone(bytes: &[u8]) -> kiapi::board::types::Zone {
        kiapi::board::types::Zone::decode(bytes).unwrap()
    }

    fn ipc_item(zone: kiapi::board::types::Zone) -> prost_types::Any {
        prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.Zone".to_string(),
            value: zone.encode_to_vec(),
        }
    }

    #[test]
    fn real_ipc_zone_fixtures_distinguish_copper_and_netless_rule_areas() {
        let copper = prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.Zone".to_string(),
            value: include_bytes!("../../tests/fixtures/issue_474_copper_zone_0.ipc.bin").to_vec(),
        };
        let keepout = prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.Zone".to_string(),
            value: include_bytes!("../../tests/fixtures/issue_474_zone_0.ipc.bin").to_vec(),
        };
        let zones = decode_ipc_zones(&[copper.clone(), keepout]).unwrap();
        assert_eq!(zones[0].kind, ZoneKind::Copper);
        assert_eq!(zones[0].net.as_deref(), Some("GND"));
        assert_eq!(zones[1].kind, ZoneKind::RuleArea);
        assert_eq!(zones[1].net, None);
        assert!(!zones.iter().any(|zone| zone.uuid.is_empty()));

        let mut netless = kiapi::board::types::Zone::decode(copper.value.as_slice()).unwrap();
        if let Some(kiapi::board::types::zone::Settings::CopperSettings(settings)) =
            netless.settings.as_mut()
        {
            settings.net = None;
        } else {
            panic!("fixture should prove copper zone settings");
        }
        let netless = prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.Zone".to_string(),
            value: netless.encode_to_vec(),
        };
        assert_eq!(decode_ipc_zones(&[netless]).unwrap()[0].net, None);
    }

    #[test]
    fn ipc_inventory_rejects_invalid_identity_type_layers_and_settings() {
        use kiapi::board::types::{BoardLayer, ZoneType};

        let copper_bytes = include_bytes!("../../tests/fixtures/issue_474_copper_zone_0.ipc.bin");
        let rule_area_bytes = include_bytes!("../../tests/fixtures/issue_474_zone_0.ipc.bin");
        let copper = fixture_ipc_zone(copper_bytes);
        let mut invalid_type = copper.clone();
        invalid_type.r#type = i32::MAX;
        let mut no_uuid = copper.clone();
        no_uuid.id = None;
        let mut unknown_layer = copper.clone();
        unknown_layer.layers = vec![i32::MAX];
        let mut undefined_layer = copper.clone();
        undefined_layer.layers = vec![BoardLayer::BlUndefined as i32];
        let mut no_copper_settings = copper;
        no_copper_settings.settings = None;
        let mut no_rule_area_settings = fixture_ipc_zone(rule_area_bytes);
        no_rule_area_settings.settings = None;

        let cases = [
            ("unknown type", vec![ipc_item(invalid_type)]),
            ("missing UUID", vec![ipc_item(no_uuid)]),
            (
                "duplicate UUID",
                vec![ipc_item(fixture_ipc_zone(copper_bytes)); 2],
            ),
            ("unknown layer", vec![ipc_item(unknown_layer)]),
            ("undefined layer", vec![ipc_item(undefined_layer)]),
            (
                "missing copper settings",
                vec![ipc_item(no_copper_settings)],
            ),
            (
                "missing rule-area settings",
                vec![ipc_item(no_rule_area_settings)],
            ),
        ];
        for (guard, items) in cases {
            assert!(
                decode_ipc_zones(&items).is_err(),
                "{guard} must be rejected"
            );
        }

        let mut unsupported = fixture_ipc_zone(copper_bytes);
        unsupported.r#type = ZoneType::ZtGraphical as i32;
        assert_eq!(
            decode_ipc_zones(&[ipc_item(unsupported)]).unwrap()[0].kind,
            ZoneKind::Unsupported("graphical")
        );
    }

    #[test]
    fn saved_inventory_rejects_wrong_root_bad_uuid_layer_and_unknown_kind() {
        let wrong_root = REAL_COPPER_BOARD.replacen("kicad_pcb", "not_a_board", 1);
        assert!(parse_file_zones(&wrong_root).is_err());

        let valid_zones = parse_file_zones(REAL_COPPER_BOARD).unwrap();
        let missing_uuid = rewrite_saved_zone(REAL_COPPER_BOARD, 0, |block| {
            block.replacen(&format!("(uuid \"{}\")", valid_zones[0].uuid), "", 1)
        });
        assert!(parse_file_zones(&missing_uuid).is_err());

        let duplicate_uuid = rewrite_saved_zone(REAL_COPPER_BOARD, 1, |block| {
            block.replacen(
                &format!("(uuid \"{}\")", valid_zones[1].uuid),
                &format!("(uuid \"{}\")", valid_zones[0].uuid),
                1,
            )
        });
        assert!(parse_file_zones(&duplicate_uuid).is_err());

        let missing_layer = rewrite_saved_zone(REAL_COPPER_BOARD, 0, |block| {
            block.replacen(&format!("(layer \"{}\")", valid_zones[0].layers[0]), "", 1)
        });
        assert_ne!(missing_layer, REAL_COPPER_BOARD);
        assert!(parse_file_zones(&missing_layer).is_err());

        let unsupported_attr = rewrite_saved_zone(REAL_COPPER_BOARD, 0, |block| {
            block.replacen("(zone", "(zone (attr future_kind)", 1)
        });
        assert!(parse_file_zones(&unsupported_attr).is_err());
    }

    #[test]
    fn exact_zone_requires_one_supported_uuid_match() {
        let zones = parse_file_zones(REAL_COPPER_BOARD).unwrap();
        assert!(exact_zone(&zones, "not-present").is_err());

        let mut duplicate = zones.clone();
        duplicate.push(zones[0].clone());
        assert!(exact_zone(&duplicate, &zones[0].uuid).is_err());

        let unsupported = ZoneRecord {
            kind: ZoneKind::Unsupported("graphical"),
            ..zones[0].clone()
        };
        assert!(exact_zone(&[unsupported], &zones[0].uuid).is_err());
    }

    #[test]
    fn revision_binds_source_canonical_path_and_full_snapshot() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first = first_dir.path().join("board.kicad_pcb");
        let second = second_dir.path().join("board.kicad_pcb");
        std::fs::write(&first, REAL_COPPER_BOARD).unwrap();
        std::fs::write(&second, REAL_COPPER_BOARD).unwrap();

        let saved = revision(&first, "saved_board", REAL_COPPER_BOARD).unwrap();
        assert_eq!(
            saved,
            revision(&first, "saved_board", REAL_COPPER_BOARD).unwrap()
        );
        assert_ne!(
            saved,
            revision(&second, "saved_board", REAL_COPPER_BOARD).unwrap()
        );
        assert_ne!(saved, revision(&first, "ipc", REAL_COPPER_BOARD).unwrap());
        let edited_snapshot = format!("{REAL_COPPER_BOARD}\n; full-snapshot change\n");
        assert_ne!(
            saved,
            revision(&first, "saved_board", &edited_snapshot).unwrap()
        );
    }

    #[tokio::test]
    async fn argument_guards_reject_wrong_optional_name_type_and_unknown_source() {
        assert_eq!(optional_name(&json!({})).unwrap(), None);
        assert_eq!(
            optional_name(&json!({"name":"GND"})).unwrap(),
            Some("GND".into())
        );
        assert!(optional_name(&json!({"name":7})).is_err());

        let (_dir, path) = path_with(REAL_COPPER_BOARD);
        let result = handle_delete_zone(&json!({
            "board": path, "uuid": "observed-uuid", "revision": "observed-revision", "source": "mystery"
        }), &ctx()).await.unwrap();
        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
        assert!(body(&result)["message"]
            .as_str()
            .unwrap()
            .contains("mystery"));
    }

    #[test]
    fn settled_inventory_needs_two_matching_reads_and_retries_only_busy() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let inventory = ZoneInventory {
            revision: "stable-revision".to_string(),
            zones: parse_file_zones(REAL_COPPER_BOARD).unwrap(),
        };
        let reads = Arc::new(AtomicUsize::new(0));
        let counted_reads = Arc::clone(&reads);
        let settled = read_settled_inventory(std::time::Duration::from_secs(1), || {
            counted_reads.fetch_add(1, Ordering::SeqCst);
            Ok(inventory.clone())
        })
        .unwrap();
        assert_eq!(settled.revision, inventory.revision);
        assert_eq!(reads.load(Ordering::SeqCst), 2);

        let busy_reads = Arc::new(AtomicUsize::new(0));
        let counted_busy_reads = Arc::clone(&busy_reads);
        let busy = || {
            anyhow::Error::new(konnect_ipc::client::ApiStatusError {
                code: kiapi::common::ApiStatusCode::AsBusy as i32,
                code_name: "AS_BUSY".to_string(),
                message: "editor is busy".to_string(),
            })
        };
        let settled =
            read_settled_inventory(
                std::time::Duration::from_secs(1),
                || match counted_busy_reads.fetch_add(1, Ordering::SeqCst) {
                    0 => Err(busy()),
                    _ => Ok(inventory.clone()),
                },
            )
            .unwrap();
        assert_eq!(settled.revision, inventory.revision);
        assert_eq!(busy_reads.load(Ordering::SeqCst), 3);

        let reads = AtomicUsize::new(0);
        let error = read_settled_inventory(std::time::Duration::from_secs(1), || {
            reads.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("non-busy read failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("non-busy read failure"));
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        let reads = AtomicUsize::new(0);
        let error = read_settled_inventory(std::time::Duration::ZERO, || {
            reads.fetch_add(1, Ordering::SeqCst);
            Ok(inventory.clone())
        })
        .unwrap_err();
        assert!(error.to_string().contains("before the deadline"));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_empty_ipc_response_cannot_override_zones_in_the_native_snapshot() {
        let snapshot = parse_file_zones(REAL_COPPER_BOARD).unwrap();
        assert!(!same_inventory(&[], &snapshot));
        assert!(same_inventory(&snapshot, &snapshot));
    }

    #[test]
    fn malformed_or_unsupported_inventory_is_not_an_empty_pass() {
        assert!(parse_file_zones("(kicad_pcb (zone (uuid \"x\"))").is_err());
        let first_zone =
            konnect_sexp::writer::find_direct_child_blocks(REAL_COPPER_BOARD, "kicad_pcb")
                .into_iter()
                .find(|(start, end)| block_tag(&REAL_COPPER_BOARD[*start..*end]) == Some("zone"))
                .unwrap();
        let block = &REAL_COPPER_BOARD[first_zone.0..first_zone.1];
        let replacement = block.replacen("(zone", "(zone (attr graphical)", 1);
        let mut graphical = REAL_COPPER_BOARD.to_string();
        graphical.replace_range(first_zone.0..first_zone.1, &replacement);
        let zones = parse_file_zones(&graphical).unwrap();
        assert_eq!(zones[0].kind, ZoneKind::Unsupported("graphical"));
        assert!(exact_zone(&zones, &zones[0].uuid).is_err());
        let unsupported_ipc = prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.Zone".to_string(),
            value: {
                let mut zone = kiapi::board::types::Zone::decode(
                    include_bytes!("../../tests/fixtures/issue_474_copper_zone_0.ipc.bin")
                        .as_slice(),
                )
                .unwrap();
                zone.r#type = kiapi::board::types::ZoneType::ZtGraphical as i32;
                zone.encode_to_vec()
            },
        };
        let unsupported = decode_ipc_zones(&[unsupported_ipc]).unwrap();
        assert_eq!(unsupported[0].kind, ZoneKind::Unsupported("graphical"));
        assert!(exact_zone(&unsupported, &unsupported[0].uuid).is_err());
    }

    #[test]
    fn duplicate_names_remain_individually_addressable_by_uuid() {
        let zones = parse_file_zones(REAL_RULE_AREA_BOARD).unwrap();
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].name, zones[1].name);
        assert_eq!(
            exact_zone(&zones, &zones[0].uuid).unwrap().uuid,
            zones[0].uuid
        );
        assert_eq!(
            exact_zone(&zones, &zones[1].uuid).unwrap().uuid,
            zones[1].uuid
        );
    }

    #[test]
    fn saved_inventory_includes_copper_and_keepout_metadata() {
        let copper = parse_file_zones(REAL_COPPER_BOARD).unwrap();
        assert_eq!(copper.len(), 3);
        assert!(copper.iter().all(|zone| zone.kind == ZoneKind::Copper));
        let rule_areas = parse_file_zones(REAL_RULE_AREA_BOARD).unwrap();
        assert_eq!(rule_areas.len(), 2);
        assert!(rule_areas
            .iter()
            .all(|zone| zone.kind == ZoneKind::RuleArea && zone.net.is_none()));
    }

    #[tokio::test]
    async fn name_filter_does_not_change_complete_board_revision() {
        let (_dir, path) = path_with(REAL_COPPER_BOARD);
        let args = json!({"board": path, "board_source": "saved"});
        let all = handle_list_zones(&args, &ctx()).await.unwrap();
        let all = body(&all);
        let first_name = all["zones"][0]["name"].as_str().unwrap();
        let filtered = handle_list_zones(
            &json!({"board": path, "board_source":"saved", "name": first_name}),
            &ctx(),
        )
        .await
        .unwrap();
        let filtered = body(&filtered);
        assert_eq!(all["revision"], filtered["revision"]);
        assert_eq!(filtered["count"], 1);
    }

    #[tokio::test]
    async fn saved_delete_is_uuid_exact_atomic_and_reads_back_absence() {
        let (_dir, path) = path_with(REAL_COPPER_BOARD);
        let observed = body(
            &handle_list_zones(&json!({"board":path,"board_source":"saved"}), &ctx())
                .await
                .unwrap(),
        );
        let target = observed["zones"][0].clone();
        let remaining = observed["zones"].as_array().unwrap()[1..].to_vec();
        let result = handle_delete_zone(&json!({
            "board":path, "uuid":target["uuid"], "revision":observed["revision"], "source":"saved_board"
        }), &ctx()).await.unwrap();
        assert!(!result.is_error, "{}", body(&result));
        let result = body(&result);
        assert_eq!(result["outcome"]["status"], "complete");
        let after = body(
            &handle_list_zones(&json!({"board":path,"board_source":"saved"}), &ctx())
                .await
                .unwrap(),
        );
        assert_eq!(after["zones"].as_array().unwrap(), &remaining);
    }

    #[tokio::test]
    async fn stale_saved_revision_refuses_without_writing() {
        let (_dir, path) = path_with(REAL_COPPER_BOARD);
        let before = std::fs::read(&path).unwrap();
        let observed = body(
            &handle_list_zones(&json!({"board":path,"board_source":"saved"}), &ctx())
                .await
                .unwrap(),
        );
        // Whitespace changes the observed revision without making the board invalid.
        std::fs::write(&path, format!("{}\n", REAL_COPPER_BOARD)).unwrap();
        saved_inventory(&path).expect("concurrent edit remains a readable board");
        let changed = std::fs::read(&path).unwrap();
        let result = handle_delete_zone(&json!({
            "board":path, "uuid":observed["zones"][0]["uuid"], "revision":observed["revision"], "source":"saved_board"
        }), &ctx()).await.unwrap();
        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("stale_target")
        );
        assert_eq!(std::fs::read(&path).unwrap(), changed);
        assert_ne!(changed, before);
    }

    #[test]
    fn unreadable_postdelete_inventory_reports_uncertain_outcome() {
        let before = ZoneInventory {
            revision: "rev".into(),
            zones: parse_file_zones(REAL_COPPER_BOARD).unwrap(),
        };
        let target = before.zones[0].clone();
        let evidence = live_delete_evidence(
            &before,
            &target,
            Ok(()),
            Ok(()),
            Err(anyhow::anyhow!("no GetItems payload")),
        );
        assert_eq!(evidence.deleted, None);
        let result = evidence.into_result(Path::new("board.kicad_pcb"));
        assert_eq!(body(&result)["outcome"]["status"], "uncertain");
        assert_eq!(body(&result)["deleted"], Value::Null);
        assert_eq!(body(&result)["readback_verified"], false);
        assert_eq!(body(&result)["unrelated_zones_preserved"], Value::Null);
        assert_eq!(body(&result)["success"], false);
    }

    #[test]
    fn live_delete_needs_readback_and_all_remaining_zone_proofs_for_completion() {
        let before = ZoneInventory {
            revision: "before".into(),
            zones: parse_file_zones(REAL_COPPER_BOARD).unwrap(),
        };
        let target = before.zones[0].clone();

        let mut missing_fill_proof = before.zones[1..].to_vec();
        missing_fill_proof[0].filled = None;
        let missing_fill = live_delete_evidence(
            &before,
            &target,
            Ok(()),
            Ok(()),
            Ok(ZoneInventory {
                revision: "after".into(),
                zones: missing_fill_proof,
            }),
        )
        .into_result(Path::new("board.kicad_pcb"));
        assert_eq!(body(&missing_fill)["outcome"]["status"], "partial");
        assert_eq!(body(&missing_fill)["remaining_copper_filled"], false);
        assert_eq!(body(&missing_fill)["success"], false);

        let mut wrong_other_zone = before.zones[1..].to_vec();
        wrong_other_zone[0].name.push_str(" changed");
        let wrong_inventory = live_delete_evidence(
            &before,
            &target,
            Ok(()),
            Ok(()),
            Ok(ZoneInventory {
                revision: "after".into(),
                zones: wrong_other_zone,
            }),
        )
        .into_result(Path::new("board.kicad_pcb"));
        assert_eq!(body(&wrong_inventory)["outcome"]["status"], "partial");
        assert_eq!(body(&wrong_inventory)["unrelated_zones_preserved"], false);
        assert_eq!(body(&wrong_inventory)["success"], false);

        let target_still_present =
            live_delete_evidence(&before, &target, Ok(()), Ok(()), Ok(before.clone()))
                .into_result(Path::new("board.kicad_pcb"));
        assert_eq!(
            body(&target_still_present)["outcome"]["status"],
            "uncertain"
        );
        assert_eq!(body(&target_still_present)["readback_verified"], false);
        assert_eq!(body(&target_still_present)["success"], false);

        let mut remaining = before.zones[1..].to_vec();
        for zone in &mut remaining {
            zone.filled = Some(true);
        }
        let readiness_failed = live_delete_evidence(
            &before,
            &target,
            Ok(()),
            Err(anyhow::anyhow!("board readiness timeout")),
            Ok(ZoneInventory {
                revision: "after".into(),
                zones: remaining,
            }),
        )
        .into_result(Path::new("board.kicad_pcb"));
        assert_eq!(body(&readiness_failed)["outcome"]["status"], "partial");
        assert_eq!(body(&readiness_failed)["refill_request_accepted"], true);
        assert_eq!(body(&readiness_failed)["refill_completed"], false);
        assert_eq!(body(&readiness_failed)["board_ready_after_refill"], false);
        assert_eq!(body(&readiness_failed)["success"], false);
    }

    #[test]
    fn proven_delete_with_refill_failure_reports_partial_without_claiming_refill() {
        let before = ZoneInventory {
            revision: "rev".into(),
            zones: parse_file_zones(REAL_COPPER_BOARD).unwrap(),
        };
        let target = before.zones[0].clone();
        let after = ZoneInventory {
            revision: "rev2".into(),
            zones: before
                .zones
                .iter()
                .filter(|zone| zone.uuid != target.uuid)
                .cloned()
                .collect(),
        };
        let result = live_delete_evidence(
            &before,
            &target,
            Err(anyhow::anyhow!("refill rejected")),
            Err(anyhow::anyhow!("board readiness timeout")),
            Ok(after),
        )
        .into_result(Path::new("board.kicad_pcb"));
        let body = body(&result);
        assert_eq!(body["outcome"]["status"], "partial");
        assert_eq!(body["deleted"], true);
        assert_eq!(body["refill_request_accepted"], false);
        assert_eq!(body["refill_completed"], false);
        assert_eq!(body["success"], false);
        assert_eq!(body["unrelated_zones_preserved"], true);
    }

    #[test]
    fn accepted_refill_with_unfilled_remaining_copper_is_partial() {
        let before = ZoneInventory {
            revision: "before".into(),
            zones: parse_file_zones(REAL_COPPER_BOARD).unwrap(),
        };
        let target = before.zones[0].clone();
        let mut remaining = before.zones[1..].to_vec();
        for zone in &mut remaining {
            zone.filled = Some(false);
        }
        let after = ZoneInventory {
            revision: "after".into(),
            zones: remaining,
        };
        let result = live_delete_evidence(&before, &target, Ok(()), Ok(()), Ok(after))
            .into_result(Path::new("board.kicad_pcb"));
        let evidence = body(&result);
        assert_eq!(evidence["outcome"]["status"], "partial");
        assert_eq!(evidence["deleted"], true);
        assert_eq!(evidence["refill_request_accepted"], true);
        assert_eq!(evidence["refill_completed"], false);
        assert_eq!(evidence["remaining_copper_filled"], false);
    }

    #[test]
    fn declared_protobuf_type_url_is_part_of_zone_type_proof() {
        let item = prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.BoardText".to_string(),
            value: include_bytes!("../../tests/fixtures/issue_474_copper_zone_0.ipc.bin").to_vec(),
        };
        let error = decode_ipc_zones(&[item])
            .expect_err("wrong Any type URL must not be decoded as a zone");
        assert!(format!("{error:#}").contains("unexpected type URL"));
    }

    #[tokio::test]
    #[ignore = "requires a disposable board open in an isolated KiCad IPC endpoint; see KONNECT_ZONE_TEST_BOARD and KONNECT_ZONE_TEST_IPC_ADDRESS"]
    async fn live_zone_delete_on_configured_disposable_board() {
        let board = PathBuf::from(
            std::env::var("KONNECT_ZONE_TEST_BOARD").expect("KONNECT_ZONE_TEST_BOARD"),
        );
        let endpoint =
            std::env::var("KONNECT_ZONE_TEST_IPC_ADDRESS").expect("KONNECT_ZONE_TEST_IPC_ADDRESS");
        let ctx = crate::tools::pcb_board::board_mock::ctx_talking_to(endpoint);
        let args = json!({"board":board,"board_source":"live"});
        let listed = handle_list_zones(&args, &ctx).await.unwrap();
        assert!(!listed.is_error, "{}", body(&listed));
        let inventory = body(&listed);
        assert!(inventory["zones"].as_array().unwrap().iter().filter(|zone| zone["kind"] == "copper").count() >= 2,
            "disposable board needs at least two copper zones so refill has remaining zones to verify");
        let target = inventory["zones"]
            .as_array()
            .unwrap()
            .iter()
            .find(|zone| zone["kind"] == "copper")
            .expect("disposable board needs at least one copper zone");
        let uuid = target["uuid"].as_str().unwrap();
        let deleted = handle_delete_zone(
            &json!({
                "board":board,
                "uuid":uuid,
                "revision":inventory["revision"],
                "source":"ipc"
            }),
            &ctx,
        )
        .await
        .unwrap();
        let result = body(&deleted);
        assert_eq!(result["outcome"]["status"], "complete", "{result}");
        assert_eq!(result["deleted"], true);
        let after = body(&handle_list_zones(&args, &ctx).await.unwrap());
        assert!(!after["zones"]
            .as_array()
            .unwrap()
            .iter()
            .any(|zone| zone["uuid"] == uuid));
    }
}
