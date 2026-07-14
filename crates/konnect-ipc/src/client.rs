//! KiCAD 10 IPC API client using NNG + Protocol Buffers.
//!
//! KiCAD 10 exposes an IPC API over NNG (nanomsg-next-gen) using protobuf messages.
//! The transport is NNG req/rep over IPC (Unix sockets / Windows named pipes).
//!
//! Socket path: set by KICAD_API_SOCKET env var when KiCAD launches a plugin,
//! or can be manually specified.
//!
//! Protocol: ApiRequest envelope containing a google.protobuf.Any body → ApiResponse.

use crate::gen::kiapi;
use crate::types::*;
use anyhow::{Context, Result};
// NNG SetOpt trait is brought in scope automatically by the nng crate's prelude
use prost::Message;
use tracing::{debug, warn};

/// Converts KiCAD nanometers to millimeters.
fn nm_to_mm(nm: i64) -> f64 {
    nm as f64 / 1_000_000.0
}

/// Map a BoardLayer enum integer back to a KiCAD layer name string.
fn layer_enum_to_name(layer: i32) -> &'static str {
    match kiapi::board::types::BoardLayer::try_from(layer) {
        Ok(l) => match l {
            kiapi::board::types::BoardLayer::BlFCu => "F.Cu",
            kiapi::board::types::BoardLayer::BlBCu => "B.Cu",
            kiapi::board::types::BoardLayer::BlIn1Cu => "In1.Cu",
            kiapi::board::types::BoardLayer::BlIn2Cu => "In2.Cu",
            kiapi::board::types::BoardLayer::BlFSilkS => "F.SilkS",
            kiapi::board::types::BoardLayer::BlBSilkS => "B.SilkS",
            kiapi::board::types::BoardLayer::BlFMask => "F.Mask",
            kiapi::board::types::BoardLayer::BlBMask => "B.Mask",
            kiapi::board::types::BoardLayer::BlFPaste => "F.Paste",
            kiapi::board::types::BoardLayer::BlBPaste => "B.Paste",
            kiapi::board::types::BoardLayer::BlFCrtYd => "F.CrtYd",
            kiapi::board::types::BoardLayer::BlBCrtYd => "B.CrtYd",
            kiapi::board::types::BoardLayer::BlFFab => "F.Fab",
            kiapi::board::types::BoardLayer::BlBFab => "B.Fab",
            kiapi::board::types::BoardLayer::BlEdgeCuts => "Edge.Cuts",
            _ => "Unknown",
        },
        Err(_) => "Unknown",
    }
}

/// Wrap a protobuf message into a prost_types::Any with the correct type_url.
fn pack_any<M: Message>(msg: &M, type_name: &str) -> prost_types::Any {
    let mut buf = Vec::new();
    msg.encode(&mut buf).expect("protobuf encode failed");
    prost_types::Any {
        type_url: format!("type.googleapis.com/{}", type_name),
        value: buf,
    }
}

/// Decode a prost_types::Any into a specific protobuf message type.
fn unpack_any<M: Message + Default>(any: &prost_types::Any) -> Result<M> {
    M::decode(any.value.as_slice()).context("Failed to decode protobuf Any body")
}

pub struct KiCadIpcClient {
    socket_path: String,
    client_name: String,
}

impl KiCadIpcClient {
    /// Create a client connecting to the given IPC socket path.
    /// If empty, tries KICAD_API_SOCKET environment variable.
    pub fn new(socket_path: impl Into<String>) -> Self {
        let path = socket_path.into();
        let effective_path = if path.is_empty() {
            std::env::var("KICAD_API_SOCKET").unwrap_or_default()
        } else {
            path
        };
        KiCadIpcClient {
            socket_path: effective_path,
            client_name: format!("konnect-{}", std::process::id()),
        }
    }

    /// Send a protobuf command and return the response Any.
    fn send_command(
        &self,
        command: &impl Message,
        type_name: &str,
    ) -> Result<Option<prost_types::Any>> {
        if self.socket_path.is_empty() {
            anyhow::bail!(
                "KiCAD IPC socket path not configured. To fix: \
                 (1) in KiCAD, enable Edit > Preferences > Plugins > 'Enable KiCad API' \
                 and copy the listed ipc:// address; \
                 (2) paste it into the 'IPC Socket' field of the Konnect settings dialog \
                 (Tools > External Plugins > Konnect) and save; \
                 (3) restart the AI client so the server rereads settings. \
                 Alternatively set ipc_socket_path in konnect-settings.json or launch \
                 via KiCAD (which sets KICAD_API_SOCKET). \
                 Full guide: https://github.com/mixelpixx/Konnect/blob/main/docs/TROUBLESHOOTING.md"
            );
        }

        let request = kiapi::common::ApiRequest {
            header: Some(kiapi::common::ApiRequestHeader {
                kicad_token: String::new(), // Empty = accept any instance
                client_name: self.client_name.clone(),
            }),
            message: Some(pack_any(command, type_name)),
        };

        let request_bytes = request.encode_to_vec();
        debug!(
            "[BETA] IPC → {} ({} bytes) to {}",
            type_name,
            request_bytes.len(),
            self.socket_path
        );

        // Connect via NNG req0 socket
        let socket =
            nng::Socket::new(nng::Protocol::Req0).context("Failed to create NNG socket")?;

        // Bound every step: a busy or wedged KiCAD must produce an error the
        // tools can surface, never an indefinite hang (the predecessor
        // project's sync/autoroute hangs blocked for >600 s on exactly this).
        // 30 s receive allows slow board operations like zone refills.
        use nng::options::Options;
        socket
            .set_opt::<nng::options::SendTimeout>(Some(std::time::Duration::from_secs(5)))
            .context("Failed to set NNG send timeout")?;
        socket
            .set_opt::<nng::options::RecvTimeout>(Some(std::time::Duration::from_secs(30)))
            .context("Failed to set NNG receive timeout")?;

        // Build the dial URL
        let dial_url =
            if self.socket_path.starts_with("ipc://") || self.socket_path.starts_with("tcp://") {
                self.socket_path.clone()
            } else {
                format!("ipc://{}", self.socket_path)
            };

        socket
            .dial(&dial_url)
            .with_context(|| format!("Cannot connect to KiCAD IPC at {}", dial_url))?;

        // Send request
        let msg = nng::Message::from(request_bytes.as_slice());
        socket
            .send(msg)
            .map_err(|(_, e)| anyhow::anyhow!("NNG send failed: {}", e))?;

        // Receive response
        let reply = socket
            .recv()
            .map_err(|e| anyhow::anyhow!("NNG recv failed: {}", e))?;

        let response = kiapi::common::ApiResponse::decode(reply.as_slice())
            .context("Failed to decode ApiResponse")?;

        // Check status
        if let Some(ref status) = response.status {
            let code = status.status();
            if code != kiapi::common::ApiStatusCode::AsOk {
                let msg = if status.error_message.is_empty() {
                    format!("{:?}", code)
                } else {
                    status.error_message.clone()
                };
                debug!("[BETA] IPC ← error: {} ({})", msg, code.as_str_name());
                anyhow::bail!("KiCAD IPC error: {} ({})", msg, code.as_str_name());
            }
        }

        debug!("[BETA] IPC ← OK");
        Ok(response.message)
    }

    // ─── Public API (same interface as before, tools don't change) ───────

    /// Check if KiCAD is reachable.
    pub fn ping(&self) -> Result<bool> {
        let ping = kiapi::common::commands::Ping {};
        match self.send_command(&ping, "kiapi.common.commands.Ping") {
            Ok(_) => Ok(true),
            Err(e) => {
                warn!("[BETA] Ping failed: {}", e);
                Ok(false)
            }
        }
    }

    /// Get the list of open documents (boards).
    pub fn get_open_documents(&self) -> Result<Vec<kiapi::common::types::DocumentSpecifier>> {
        let cmd = kiapi::common::commands::GetOpenDocuments {
            r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
        };
        let response_any = self.send_command(&cmd, "kiapi.common.commands.GetOpenDocuments")?;
        if let Some(any) = response_any {
            let resp: kiapi::common::commands::GetOpenDocumentsResponse = unpack_any(&any)?;
            Ok(resp.documents)
        } else {
            Ok(vec![])
        }
    }

    /// Get the first open PCB's DocumentSpecifier (needed for most commands).
    fn get_board_document(&self) -> Result<kiapi::common::types::DocumentSpecifier> {
        let docs = self.get_open_documents()?;
        docs.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!("No PCB document is open in KiCAD. Open a board file first.")
        })
    }

    fn make_header(&self) -> Result<kiapi::common::types::ItemHeader> {
        Ok(kiapi::common::types::ItemHeader {
            document: Some(self.get_board_document()?),
            container: None,
            field_mask: None,
        })
    }

    /// Get all nets on the board.
    pub fn get_nets(&self) -> Result<Vec<IpcNet>> {
        let doc = self.get_board_document()?;
        let cmd = kiapi::board::commands::GetNets {
            board: Some(doc),
            netclass_filter: vec![],
        };
        let response_any = self.send_command(&cmd, "kiapi.board.commands.GetNets")?;
        if let Some(any) = response_any {
            let resp: kiapi::board::commands::NetsResponse = unpack_any(&any)?;
            Ok(resp
                .nets
                .iter()
                .map(|n| IpcNet {
                    name: n.name.clone(),
                    netcode: n.code.as_ref().map(|c| c.value).unwrap_or(0),
                })
                .collect())
        } else {
            Ok(vec![])
        }
    }

    /// Get board items by type.
    pub fn get_items(
        &self,
        item_type: kiapi::common::types::KiCadObjectType,
    ) -> Result<Vec<prost_types::Any>> {
        let header = self.make_header()?;
        let cmd = kiapi::common::commands::GetItems {
            header: Some(header),
            types: vec![item_type as i32],
        };
        let response_any = self.send_command(&cmd, "kiapi.common.commands.GetItems")?;
        if let Some(any) = response_any {
            let resp: kiapi::common::commands::GetItemsResponse = unpack_any(&any)?;
            Ok(resp.items)
        } else {
            Ok(vec![])
        }
    }

    /// List all footprints on the board.
    pub fn list_footprints(&self) -> Result<Vec<IpcFootprint>> {
        let items = self.get_items(kiapi::common::types::KiCadObjectType::KotPcbFootprint)?;
        let mut footprints = Vec::new();
        for item in &items {
            if let Ok(fp) = kiapi::board::types::FootprintInstance::decode(item.value.as_slice()) {
                let pos = fp.position.as_ref();
                let ref_text = fp
                    .reference_field
                    .as_ref()
                    .and_then(|f| f.text.as_ref())
                    .and_then(|bt| bt.text.as_ref())
                    .map(|t| t.text.clone())
                    .unwrap_or_default();
                let val_text = fp
                    .value_field
                    .as_ref()
                    .and_then(|f| f.text.as_ref())
                    .and_then(|bt| bt.text.as_ref())
                    .map(|t| t.text.clone())
                    .unwrap_or_default();
                let lib_id = fp
                    .definition
                    .as_ref()
                    .and_then(|d| d.id.as_ref())
                    .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
                    .unwrap_or_default();
                footprints.push(IpcFootprint {
                    reference: ref_text,
                    value: val_text,
                    footprint: lib_id,
                    position: IpcVector2 {
                        x: pos.map(|p| nm_to_mm(p.x_nm)).unwrap_or(0.0),
                        y: pos.map(|p| nm_to_mm(p.y_nm)).unwrap_or(0.0),
                    },
                    rotation: fp
                        .orientation
                        .as_ref()
                        .map(|a| a.value_degrees)
                        .unwrap_or(0.0),
                    layer: layer_enum_to_name(fp.layer).to_string(),
                });
            }
        }
        Ok(footprints)
    }

    /// Create items on the board.
    pub fn create_items(&self, items: Vec<prost_types::Any>) -> Result<()> {
        let header = self.make_header()?;
        let cmd = kiapi::common::commands::CreateItems {
            header: Some(header),
            items,
            container: None,
        };
        self.send_command(&cmd, "kiapi.common.commands.CreateItems")?;
        Ok(())
    }

    /// Update existing items by KIID. Generic wrapper mirroring create_items/delete_items;
    /// each `Any` must be a fully-formed board item with an existing `id` populated.
    pub fn update_items(&self, items: Vec<prost_types::Any>) -> Result<()> {
        let header = self.make_header()?;
        let cmd = kiapi::common::commands::UpdateItems {
            header: Some(header),
            items,
        };
        self.send_command(&cmd, "kiapi.common.commands.UpdateItems")?;
        Ok(())
    }

    /// Delete items by KIID.
    pub fn delete_items(&self, ids: Vec<String>) -> Result<()> {
        let header = self.make_header()?;
        let cmd = kiapi::common::commands::DeleteItems {
            header: Some(header),
            item_ids: ids
                .iter()
                .map(|id| kiapi::common::types::Kiid { value: id.clone() })
                .collect(),
        };
        self.send_command(&cmd, "kiapi.common.commands.DeleteItems")?;
        Ok(())
    }

    /// Refill zones on the board.
    pub fn refill_zones(&self) -> Result<()> {
        let doc = self.get_board_document()?;
        let cmd = kiapi::board::commands::RefillZones {
            board: Some(doc),
            zones: vec![],
        };
        self.send_command(&cmd, "kiapi.board.commands.RefillZones")?;
        Ok(())
    }

    /// Save the open board document.
    pub fn save_board(&self) -> Result<()> {
        let doc = self.get_board_document()?;
        let cmd = kiapi::common::commands::SaveDocument {
            document: Some(doc),
        };
        self.send_command(&cmd, "kiapi.common.commands.SaveDocument")?;
        Ok(())
    }

    /// Begin a commit (undo group).
    pub fn begin_commit(&self) -> Result<String> {
        let cmd = kiapi::common::commands::BeginCommit {};
        let response_any = self.send_command(&cmd, "kiapi.common.commands.BeginCommit")?;
        if let Some(any) = response_any {
            let resp: kiapi::common::commands::BeginCommitResponse = unpack_any(&any)?;
            Ok(resp.id.map(|id| id.value).unwrap_or_default())
        } else {
            Ok(String::new())
        }
    }

    /// End a commit (push or drop).
    pub fn end_commit(
        &self,
        commit_id: &str,
        action: kiapi::common::commands::CommitAction,
        message: &str,
    ) -> Result<()> {
        let cmd = kiapi::common::commands::EndCommit {
            id: Some(kiapi::common::types::Kiid {
                value: commit_id.to_string(),
            }),
            action: action as i32,
            message: message.to_string(),
        };
        self.send_command(&cmd, "kiapi.common.commands.EndCommit")?;
        Ok(())
    }

    /// Push (commit) changes.
    pub fn push_commit(&self, commit_id: &str, description: &str) -> Result<()> {
        self.end_commit(
            commit_id,
            kiapi::common::commands::CommitAction::CmaCommit,
            description,
        )
    }

    /// Drop (rollback) changes.
    pub fn drop_commit(&self, commit_id: &str) -> Result<()> {
        self.end_commit(
            commit_id,
            kiapi::common::commands::CommitAction::CmaDrop,
            "",
        )
    }

    // ─── PCB Item Operations (real protobuf implementations) ───────────

    /// Resolve a net name to its net code by querying GetNets.
    pub fn resolve_net_code(&self, net_name: &str) -> Result<i32> {
        let nets = self.get_nets()?;
        nets.iter()
            .find(|n| n.name == net_name)
            .map(|n| n.netcode)
            .ok_or_else(|| anyhow::anyhow!("Net '{}' not found on board", net_name))
    }

    /// Find a footprint by reference and return its IpcFootprint + KIID.
    pub fn get_footprint(&self, reference: &str) -> Result<Option<IpcFootprint>> {
        let footprints = self.list_footprints()?;
        Ok(footprints.into_iter().find(|fp| fp.reference == reference))
    }

    /// Find a footprint's KIID by reference.
    fn find_footprint_kiid(&self, reference: &str) -> Result<String> {
        let items = self.get_items(kiapi::common::types::KiCadObjectType::KotPcbFootprint)?;
        for item in &items {
            if let Ok(fp) = kiapi::board::types::FootprintInstance::decode(item.value.as_slice()) {
                let ref_text = fp
                    .reference_field
                    .as_ref()
                    .and_then(|f| f.text.as_ref())
                    .and_then(|bt| bt.text.as_ref())
                    .map(|t| t.text.as_str())
                    .unwrap_or("");
                if ref_text == reference {
                    if let Some(id) = &fp.id {
                        return Ok(id.value.clone());
                    }
                }
            }
        }
        anyhow::bail!("Footprint '{}' not found on board", reference)
    }

    /// Add a track segment to the board.
    #[allow(clippy::too_many_arguments)]
    pub fn add_track(
        &self,
        net_name: &str,
        layer: &str,
        width: f64,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
    ) -> Result<()> {
        let net_code = self.resolve_net_code(net_name)?;
        let track = crate::builders::build_track(net_name, net_code, layer, width, x1, y1, x2, y2);
        let any = crate::builders::pack_any(&track, "kiapi.board.types.Track");
        self.create_items(vec![any])?;
        Ok(())
    }

    /// Add a via to the board using S-expression string (simpler than full protobuf PadStack construction).
    pub fn add_via(&self, net_name: &str, x: f64, y: f64, drill: f64, pad_size: f64) -> Result<()> {
        let net_code = self.resolve_net_code(net_name)?;
        let sexp = crate::builders::via_sexp(net_name, net_code, x, y, drill, pad_size);
        let doc = self.get_board_document()?;
        let cmd = kiapi::common::commands::ParseAndCreateItemsFromString {
            document: Some(doc),
            contents: sexp,
        };
        self.send_command(&cmd, "kiapi.common.commands.ParseAndCreateItemsFromString")?;
        Ok(())
    }

    /// Delete a track by UUID.
    pub fn delete_track(&self, uuid: &str) -> Result<()> {
        self.delete_items(vec![uuid.to_string()])
    }

    /// Query tracks, optionally filtered by net and/or layer.
    pub fn get_tracks(
        &self,
        net_filter: Option<&str>,
        layer_filter: Option<&str>,
    ) -> Result<Vec<IpcTrack>> {
        let items = self.get_items(kiapi::common::types::KiCadObjectType::KotPcbTrace)?;
        let mut tracks = Vec::new();
        for item in &items {
            if let Ok(track) = kiapi::board::types::Track::decode(item.value.as_slice()) {
                let net_name = track.net.as_ref().map(|n| n.name.as_str()).unwrap_or("");
                let layer_name = layer_enum_to_name(track.layer);

                // Apply net filter
                if let Some(nf) = net_filter {
                    if net_name != nf {
                        continue;
                    }
                }
                // Apply layer filter
                if let Some(lf) = layer_filter {
                    if layer_name != lf {
                        continue;
                    }
                }

                let start = track.start.as_ref();
                let end = track.end.as_ref();
                tracks.push(IpcTrack {
                    net_name: net_name.to_string(),
                    layer: layer_name.to_string(),
                    width: track
                        .width
                        .as_ref()
                        .map(|w| crate::builders::nm_to_mm(w.value_nm))
                        .unwrap_or(0.25),
                    start: IpcVector2 {
                        x: start
                            .map(|p| crate::builders::nm_to_mm(p.x_nm))
                            .unwrap_or(0.0),
                        y: start
                            .map(|p| crate::builders::nm_to_mm(p.y_nm))
                            .unwrap_or(0.0),
                    },
                    end: IpcVector2 {
                        x: end
                            .map(|p| crate::builders::nm_to_mm(p.x_nm))
                            .unwrap_or(0.0),
                        y: end
                            .map(|p| crate::builders::nm_to_mm(p.y_nm))
                            .unwrap_or(0.0),
                    },
                });
            }
        }
        Ok(tracks)
    }

    /// Move a footprint to a new position.
    pub fn move_footprint(&self, reference: &str, x: f64, y: f64) -> Result<()> {
        // Find the footprint, update position, send UpdateItems
        let items = self.get_items(kiapi::common::types::KiCadObjectType::KotPcbFootprint)?;
        for item in &items {
            if let Ok(mut fp) =
                kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            {
                let ref_text = fp
                    .reference_field
                    .as_ref()
                    .and_then(|f| f.text.as_ref())
                    .and_then(|bt| bt.text.as_ref())
                    .map(|t| t.text.as_str())
                    .unwrap_or("");
                if ref_text == reference {
                    let old = fp.position.unwrap_or_default();
                    let new_pos = crate::builders::vec2(x, y);
                    fp.position = Some(new_pos);
                    // KiCAD carries the footprint's children (pads, silk,
                    // text) in absolute board coordinates and re-creates them
                    // verbatim on update, so they must be shifted along with
                    // the anchor (issue #23).
                    crate::transform::transform_footprint_children(
                        &mut fp,
                        &crate::transform::Xform::Translate {
                            dx_nm: new_pos.x_nm - old.x_nm,
                            dy_nm: new_pos.y_nm - old.y_nm,
                        },
                    )?;
                    let any = crate::builders::pack_any(&fp, "kiapi.board.types.FootprintInstance");

                    let header = self.make_header()?;
                    let cmd = kiapi::common::commands::UpdateItems {
                        header: Some(header),
                        items: vec![any],
                    };
                    self.send_command(&cmd, "kiapi.common.commands.UpdateItems")?;
                    return Ok(());
                }
            }
        }
        anyhow::bail!("Footprint '{}' not found", reference)
    }

    /// Rotate a footprint to a new angle.
    pub fn rotate_footprint(&self, reference: &str, angle: f64) -> Result<()> {
        let items = self.get_items(kiapi::common::types::KiCadObjectType::KotPcbFootprint)?;
        for item in &items {
            if let Ok(mut fp) =
                kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            {
                let ref_text = fp
                    .reference_field
                    .as_ref()
                    .and_then(|f| f.text.as_ref())
                    .and_then(|bt| bt.text.as_ref())
                    .map(|t| t.text.as_str())
                    .unwrap_or("");
                if ref_text == reference {
                    let old_deg = fp
                        .orientation
                        .as_ref()
                        .map(|a| a.value_degrees)
                        .unwrap_or(0.0);
                    fp.orientation = Some(kiapi::common::types::Angle {
                        value_degrees: angle,
                    });
                    // Children are carried in absolute board coordinates and
                    // angles; rotate them around the anchor like KiCAD's
                    // FOOTPRINT::SetOrientation does natively (issue #23).
                    let anchor = fp.position.unwrap_or_default();
                    crate::transform::transform_footprint_children(
                        &mut fp,
                        &crate::transform::Xform::Rotate {
                            cx_nm: anchor.x_nm,
                            cy_nm: anchor.y_nm,
                            delta_deg: angle - old_deg,
                        },
                    )?;
                    let any = crate::builders::pack_any(&fp, "kiapi.board.types.FootprintInstance");
                    let header = self.make_header()?;
                    let cmd = kiapi::common::commands::UpdateItems {
                        header: Some(header),
                        items: vec![any],
                    };
                    self.send_command(&cmd, "kiapi.common.commands.UpdateItems")?;
                    return Ok(());
                }
            }
        }
        anyhow::bail!("Footprint '{}' not found", reference)
    }

    /// Delete a footprint by reference.
    pub fn delete_footprint(&self, reference: &str) -> Result<()> {
        let kiid = self.find_footprint_kiid(reference)?;
        self.delete_items(vec![kiid])
    }

    /// Place a footprint — currently requires KiCAD's ParseAndCreateItemsFromString.
    pub fn place_footprint(
        &self,
        lib_id: &str,
        x: f64,
        y: f64,
        rotation: f64,
        layer: &str,
    ) -> Result<IpcFootprint> {
        // KiCAD 10 IPC doesn't have a direct "place footprint from library" command.
        // The CreateItems command requires a fully formed FootprintInstance protobuf,
        // which needs the complete footprint definition (pads, shapes, etc.) from the library.
        // For now, use ParseAndCreateItemsFromString with S-expression format.
        let sexp = format!(
            r#"(footprint "{lib_id}"
  (layer "{layer}")
  (at {x} {y} {rotation})
)"#,
            lib_id = lib_id,
            layer = layer,
            x = crate::builders::mm_to_nm(x) as f64 / 1_000_000.0,
            y = crate::builders::mm_to_nm(y) as f64 / 1_000_000.0,
            rotation = rotation,
        );

        let doc = self.get_board_document()?;
        let cmd = kiapi::common::commands::ParseAndCreateItemsFromString {
            document: Some(doc),
            contents: sexp,
        };
        self.send_command(&cmd, "kiapi.common.commands.ParseAndCreateItemsFromString")?;

        Ok(IpcFootprint {
            reference: String::new(),
            value: String::new(),
            footprint: lib_id.to_string(),
            position: IpcVector2 { x, y },
            rotation,
            layer: layer.to_string(),
        })
    }

    /// Get board extents (bounding box of all items).
    pub fn get_board_extents(&self) -> Result<IpcBoardExtents> {
        // Use GetBoundingBox with no specific items = board extents
        let header = self.make_header()?;
        let cmd = kiapi::common::commands::GetBoundingBox {
            header: Some(header),
            items: vec![], // empty = all items
            mode: kiapi::common::commands::BoundingBoxMode::BbmItemOnly as i32,
        };
        let resp_any = self.send_command(&cmd, "kiapi.common.commands.GetBoundingBox")?;
        if let Some(any) = resp_any {
            let resp: kiapi::common::commands::GetBoundingBoxResponse = unpack_any(&any)?;
            if let Some(bbox) = resp.boxes.first() {
                let pos = bbox.position.as_ref();
                let size = bbox.size.as_ref();
                return Ok(IpcBoardExtents {
                    min: IpcVector2 {
                        x: pos
                            .map(|p| crate::builders::nm_to_mm(p.x_nm))
                            .unwrap_or(0.0),
                        y: pos
                            .map(|p| crate::builders::nm_to_mm(p.y_nm))
                            .unwrap_or(0.0),
                    },
                    max: IpcVector2 {
                        x: pos
                            .map(|p| crate::builders::nm_to_mm(p.x_nm))
                            .unwrap_or(0.0)
                            + size
                                .map(|s| crate::builders::nm_to_mm(s.x_nm))
                                .unwrap_or(0.0),
                        y: pos
                            .map(|p| crate::builders::nm_to_mm(p.y_nm))
                            .unwrap_or(0.0)
                            + size
                                .map(|s| crate::builders::nm_to_mm(s.y_nm))
                                .unwrap_or(0.0),
                    },
                });
            }
        }
        anyhow::bail!("No bounding box returned from KiCAD")
    }

    /// Get enabled layers.
    pub fn get_layers(&self) -> Result<Vec<IpcLayer>> {
        let doc = self.get_board_document()?;
        let cmd = kiapi::board::commands::GetBoardEnabledLayers { board: Some(doc) };
        let resp_any = self.send_command(&cmd, "kiapi.board.commands.GetBoardEnabledLayers")?;
        if let Some(any) = resp_any {
            let resp: kiapi::board::commands::BoardEnabledLayersResponse = unpack_any(&any)?;
            let layers = resp
                .layers
                .iter()
                .map(|&l| {
                    let bl = kiapi::board::types::BoardLayer::try_from(l)
                        .unwrap_or(kiapi::board::types::BoardLayer::BlUndefined);
                    IpcLayer {
                        name: bl
                            .as_str_name()
                            .trim_start_matches("BL_")
                            .replace('_', ".")
                            .to_string(),
                        id: l,
                        kind: String::new(),
                    }
                })
                .collect();
            Ok(layers)
        } else {
            Ok(vec![])
        }
    }

    /// Run an arbitrary tool action in KiCAD (e.g. to trigger a refresh).
    pub fn run_action(&self, action: &str) -> Result<()> {
        let cmd = kiapi::common::commands::RunAction {
            action: action.to_string(),
        };
        self.send_command(&cmd, "kiapi.common.commands.RunAction")?;
        Ok(())
    }
}
