//! KiCad 10 IPC API client using NNG + Protocol Buffers.
//!
//! KiCad 10 exposes an IPC API over NNG (nanomsg-next-gen) using protobuf messages.
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
use std::path::{Path, PathBuf};
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

/// A placed footprint's anchor in board nanometres, and its orientation in
/// degrees.
///
/// KiCad keeps a footprint's children in absolute board coordinates, so this
/// is the frame that turns them back into the footprint-local numbers a
/// `.kicad_mod` shows, and back again.
fn footprint_frame(fp: &kiapi::board::types::FootprintInstance) -> ((i64, i64), f64) {
    let pos = fp.position.unwrap_or_default();
    let angle = fp
        .orientation
        .as_ref()
        .map(|a| a.value_degrees)
        .unwrap_or(0.0);
    ((pos.x_nm, pos.y_nm), angle)
}

/// The shape family of a graphic geometry, named as the tool schemas name it.
fn geometry_kind(geometry: &kiapi::common::types::graphic_shape::Geometry) -> &'static str {
    use kiapi::common::types::graphic_shape::Geometry;
    match geometry {
        Geometry::Segment(_) => "segment",
        Geometry::Rectangle(_) => "rectangle",
        Geometry::Arc(_) => "arc",
        Geometry::Circle(_) => "circle",
        Geometry::Polygon(_) => "polygon",
        Geometry::Bezier(_) => "bezier",
    }
}

/// How many outlines a polygon's `PolySet` carries and how many holes across
/// them. `(0, 0)` for any other geometry.
///
/// `geometry_points` reports the first outline only, and the edit path rebuilds
/// the shape from a single outline — so a footprint carrying a cutout would read
/// back looking correct and be flattened on the next write. Counting them is
/// what lets both ends say so.
fn polygon_extent(geometry: &kiapi::common::types::graphic_shape::Geometry) -> (usize, usize) {
    use kiapi::common::types::graphic_shape::Geometry;
    match geometry {
        Geometry::Polygon(ps) => (
            ps.polygons.len(),
            ps.polygons.iter().map(|p| p.holes.len()).sum(),
        ),
        _ => (0, 0),
    }
}

/// Refuse or rewrite `shape`'s outline, returning the kind it was.
///
/// Split out of [`KiCadIpcClient::set_footprint_graphic_points`] so the refusals
/// can be tested. They previously sat inside the loop that walks a live
/// footprint's items, so reaching any of them needed a running KiCad — which
/// meant the only guarantee that a bad request writes nothing was that nobody
/// had tried it. That is the shape #117 shipped in: an outbound message that
/// encodes cleanly and is semantically wrong.
///
/// Every refusal happens before any mutation, so a caller that gets an `Err`
/// here has an untouched shape and the caller sends no `UpdateItems`.
fn replace_polygon_outline(
    shape: &mut kiapi::board::types::BoardGraphicShape,
    reference: &str,
    uuid: &str,
    points_mm: &[(f64, f64)],
    anchor: (i64, i64),
    to_board: &crate::transform::Xform,
) -> Result<&'static str> {
    let kind = shape
        .shape
        .as_ref()
        .and_then(|s| s.geometry.as_ref())
        .map(geometry_kind)
        .unwrap_or("unknown");
    if kind != "polygon" {
        anyhow::bail!(
            "graphic '{}' on '{}' is a {}; only polygons take a vertex list",
            uuid,
            reference,
            kind
        );
    }
    if points_mm.len() < 3 {
        anyhow::bail!("a polygon needs at least 3 points, got {}", points_mm.len());
    }
    // The rebuild below emits one outline with no holes. Rejecting a shape that
    // carries more is the difference between refusing the edit and silently
    // deleting a cutout the caller never mentioned.
    let (outlines, holes) = shape
        .shape
        .as_ref()
        .and_then(|s| s.geometry.as_ref())
        .map(polygon_extent)
        .unwrap_or((0, 0));
    if outlines > 1 || holes > 0 {
        anyhow::bail!(
            "polygon '{}' on '{}' carries {} outline(s) and {} hole(s); this tool \
             replaces a single outline and would discard the rest, so it will not \
             write it — edit this one in KiCad",
            uuid,
            reference,
            outlines,
            holes
        );
    }

    let nodes = points_mm
        .iter()
        .map(|&(x, y)| {
            let (rx, ry) =
                to_board.point(crate::builders::mm_to_nm(x), crate::builders::mm_to_nm(y));
            kiapi::common::types::PolyLineNode {
                geometry: Some(kiapi::common::types::poly_line_node::Geometry::Point(
                    kiapi::common::types::Vector2 {
                        x_nm: anchor.0 + rx,
                        y_nm: anchor.1 + ry,
                    },
                )),
            }
        })
        .collect();

    if let Some(s) = shape.shape.as_mut() {
        s.geometry = Some(kiapi::common::types::graphic_shape::Geometry::Polygon(
            kiapi::common::types::PolySet {
                polygons: vec![kiapi::common::types::PolygonWithHoles {
                    outline: Some(kiapi::common::types::PolyLine {
                        nodes,
                        closed: true,
                    }),
                    holes: vec![],
                }],
            },
        ));
    }
    Ok(kind)
}

/// The defining vertices of a graphic geometry, in absolute board nanometres.
fn geometry_points(geometry: &kiapi::common::types::graphic_shape::Geometry) -> Vec<(i64, i64)> {
    use kiapi::common::types::graphic_shape::Geometry;
    // An absent vertex yields nothing rather than the origin. Substituting
    // (0, 0) would report a shape that looks well-formed and is silently in the
    // wrong place; a short list is visibly wrong at the point of use.
    let pt = |v: &Option<kiapi::common::types::Vector2>| v.map(|p| (p.x_nm, p.y_nm));
    let pts = |vs: [&Option<kiapi::common::types::Vector2>; 4], n: usize| {
        vs[..n].iter().filter_map(|v| pt(v)).collect::<Vec<_>>()
    };
    let none = &None;
    match geometry {
        Geometry::Segment(s) => pts([&s.start, &s.end, none, none], 2),
        Geometry::Rectangle(r) => pts([&r.top_left, &r.bottom_right, none, none], 2),
        Geometry::Arc(a) => pts([&a.start, &a.mid, &a.end, none], 3),
        Geometry::Circle(c) => pts([&c.center, &c.radius_point, none, none], 2),
        Geometry::Bezier(b) => pts([&b.start, &b.control1, &b.control2, &b.end], 4),
        Geometry::Polygon(ps) => ps
            .polygons
            .first()
            .and_then(|p| p.outline.as_ref())
            .map(|o| {
                o.nodes
                    .iter()
                    .filter_map(|n| match n.geometry.as_ref() {
                        Some(kiapi::common::types::poly_line_node::Geometry::Point(p)) => {
                            Some((p.x_nm, p.y_nm))
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
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

fn unpack_required<M: Message + Default>(
    response: Option<prost_types::Any>,
    command_name: &str,
) -> Result<M> {
    let response =
        response.with_context(|| format!("KiCad returned no {command_name} response payload"))?;
    unpack_any(&response)
}

fn ensure_item_request_ok(status: i32, operation: &str) -> Result<()> {
    let status = kiapi::common::types::ItemRequestStatus::try_from(status)
        .unwrap_or(kiapi::common::types::ItemRequestStatus::IrsUnknown);
    if status != kiapi::common::types::ItemRequestStatus::IrsOk {
        anyhow::bail!(
            "KiCad {operation} request failed with {}",
            status.as_str_name()
        );
    }
    Ok(())
}

/// Marker error carried (via anyhow's error chain) by every failure where no
/// request completed a round-trip with a live KiCad: no socket path
/// configured, or the NNG dial/send failed.
///
/// Callers must classify with [`IpcFailure::from_error`], never by matching
/// error text.
#[derive(Debug)]
pub struct TransportUnreachable;

impl std::fmt::Display for TransportUnreachable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "KiCad IPC transport unreachable")
    }
}

impl std::error::Error for TransportUnreachable {}

/// Why an IPC operation failed, for callers deciding whether a file-based
/// fallback is safe.
///
/// `Unreachable` means the transport never delivered the request — IPC is
/// unconfigured (empty socket path) or the dial/send failed — so no live
/// KiCad can be holding the board, and editing the board file directly
/// cannot race an editor.
///
/// `Rejected` is everything else, including any error after a request was
/// delivered (a receive timeout may mean KiCad is still processing it).
/// KiCad is — or may be — alive on the other end, so a file edit could be
/// silently overwritten on its next save. Fail closed.
#[derive(Debug)]
pub enum IpcFailure {
    Unreachable(String),
    Rejected(String),
}

impl IpcFailure {
    /// Classify an error from any [`KiCadIpcClient`] operation by walking its
    /// chain for the [`TransportUnreachable`] marker — never by matching
    /// message text.
    pub fn from_error(error: anyhow::Error) -> Self {
        let message = format!("{error:#}");
        if error
            .chain()
            .any(|cause| cause.is::<TransportUnreachable>())
        {
            IpcFailure::Unreachable(message)
        } else {
            IpcFailure::Rejected(message)
        }
    }

    pub fn message(&self) -> &str {
        match self {
            IpcFailure::Unreachable(message) | IpcFailure::Rejected(message) => message,
        }
    }
}

impl std::fmt::Display for IpcFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message())
    }
}

pub struct KiCadIpcClient {
    socket_path: String,
    kicad_token: String,
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
            kicad_token: std::env::var("KICAD_API_TOKEN").unwrap_or_default(),
            client_name: format!("konnect-{}", std::process::id()),
        }
    }

    /// Create a client with an explicit API token.
    ///
    /// KiCad supplies this token to executable plugins through
    /// `KICAD_API_TOKEN`. This constructor is useful for clients that obtain
    /// the connection details through another discovery mechanism.
    pub fn new_with_token(socket_path: impl Into<String>, token: impl Into<String>) -> Self {
        let mut client = Self::new(socket_path);
        client.kicad_token = token.into();
        client
    }

    /// Send a protobuf command and return the response Any.
    fn send_command(
        &self,
        command: &impl Message,
        type_name: &str,
    ) -> Result<Option<prost_types::Any>> {
        if self.socket_path.is_empty() {
            return Err(anyhow::Error::new(TransportUnreachable).context(
                "KiCAD IPC socket path not configured. To fix: \
                 (1) in KiCAD, enable Edit > Preferences > Plugins > 'Enable KiCad API' \
                 and copy the listed ipc:// address; \
                 (2) paste it into the 'IPC Socket' field of the Konnect settings dialog \
                 (Tools > External Plugins > Konnect) and save; \
                 (3) restart the AI client so the server rereads settings. \
                 Alternatively set ipc_socket_path in konnect-settings.json or launch \
                 via KiCAD (which sets KICAD_API_SOCKET). \
                 Full guide: https://github.com/mixelpixx/Konnect/blob/main/docs/TROUBLESHOOTING.md",
            ));
        }

        let request = kiapi::common::ApiRequest {
            header: Some(kiapi::common::ApiRequestHeader {
                kicad_token: self.kicad_token.clone(),
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

        // Build the dial URL. inproc:// is same-process only — used by the
        // mock-KiCAD test servers, where it avoids TCP port races entirely.
        let dial_url = if self.socket_path.starts_with("ipc://")
            || self.socket_path.starts_with("tcp://")
            || self.socket_path.starts_with("inproc://")
        {
            self.socket_path.clone()
        } else {
            format!("ipc://{}", self.socket_path)
        };

        socket.dial(&dial_url).map_err(|error| {
            anyhow::Error::new(TransportUnreachable).context(format!(
                "Cannot connect to KiCAD IPC at {dial_url}: {error}"
            ))
        })?;

        // Send request
        let msg = nng::Message::from(request_bytes.as_slice());
        socket.send(msg).map_err(|(_, error)| {
            anyhow::Error::new(TransportUnreachable).context(format!("NNG send failed: {error}"))
        })?;

        // Receive response
        let reply = socket
            .recv()
            .map_err(|e| anyhow::anyhow!("NNG recv failed: {}", e))?;

        let response = kiapi::common::ApiResponse::decode(reply.as_slice())
            .context("Failed to decode ApiResponse")?;

        // A response without an envelope status is malformed, not success.
        let status = response
            .status
            .as_ref()
            .context("KiCad IPC response is missing its status")?;
        let code = status.status();
        if code != kiapi::common::ApiStatusCode::AsOk {
            let msg = if status.error_message.is_empty() {
                format!("{:?}", code)
            } else {
                status.error_message.clone()
            };
            debug!("[BETA] IPC ← error: {} ({})", msg, code.as_str_name());
            anyhow::bail!("KiCad IPC error: {} ({})", msg, code.as_str_name());
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

    /// Find the open document matching `requested`, so a path-bearing MCP
    /// request operates on the board it names — not whichever board happens
    /// to be first in KiCad's open-document list. Live verification caught
    /// exactly this: with the user's own project focused and the target
    /// board open behind it, first-document targeting either fails or, worse,
    /// would mutate the wrong board.
    pub fn find_open_board(
        &self,
        requested: &Path,
    ) -> Result<kiapi::common::types::DocumentSpecifier> {
        let docs = self.get_open_documents()?;
        if docs.is_empty() {
            anyhow::bail!("No PCB document is open in KiCAD. Open a board file first.");
        }
        let mut open_names = Vec::new();
        for doc in docs {
            if let Some(path) = board_document_path(&doc) {
                if paths_refer_to_same_board(requested, &path) {
                    return Ok(doc);
                }
                open_names.push(path.display().to_string());
            }
        }
        anyhow::bail!(
            "requested board '{}' is not open in KiCAD (open boards: {})",
            requested.display(),
            open_names.join(", ")
        )
    }

    /// Fail closed unless the requested board is open in the IPC session.
    pub fn ensure_board_is_active(&self, requested: &Path) -> Result<()> {
        self.find_open_board(requested).map(|_| ())
    }

    fn make_header(&self) -> Result<kiapi::common::types::ItemHeader> {
        Ok(header_for(self.get_board_document()?))
    }

    /// Get all nets on the board.
    pub fn get_nets(&self) -> Result<Vec<IpcNet>> {
        self.get_nets_in(self.get_board_document()?)
    }

    /// As [`Self::get_nets`], targeting a specific open document.
    pub fn get_nets_in(&self, doc: kiapi::common::types::DocumentSpecifier) -> Result<Vec<IpcNet>> {
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
        self.get_items_in(self.get_board_document()?, item_type)
    }

    /// As [`Self::get_items`], targeting a specific open document.
    pub fn get_items_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
        item_type: kiapi::common::types::KiCadObjectType,
    ) -> Result<Vec<prost_types::Any>> {
        let header = header_for(document);
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
        self.list_footprints_in(self.get_board_document()?)
    }

    /// As [`Self::list_footprints`], targeting a specific open document.
    pub fn list_footprints_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
    ) -> Result<Vec<IpcFootprint>> {
        let items = self.get_items_in(
            document,
            kiapi::common::types::KiCadObjectType::KotPcbFootprint,
        )?;
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
    ///
    /// `created_items` is documented as the status of each item *to be*
    /// created: a rejected item still occupies a result slot (with e.g.
    /// `ISC_INVALID_DATA` and no item attached), so counting the vector's
    /// length would report phantom successes. Only results whose status is
    /// `ISC_OK` — or whose status was left at the protobuf default while an
    /// item came back, since proto3 cannot distinguish "unset" from an
    /// explicit `ISC_UNKNOWN` — count as created; anything else fails with
    /// KiCad's own per-item reasons. (Outcome semantics follow emolitor's
    /// PR #66.)
    pub fn create_items(&self, items: Vec<prost_types::Any>) -> Result<()> {
        self.create_items_in(self.get_board_document()?, items)
    }

    /// As [`Self::create_items`], targeting a specific open document.
    pub fn create_items_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
        items: Vec<prost_types::Any>,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let expected_count = items.len();
        let header = header_for(document);
        let cmd = kiapi::common::commands::CreateItems {
            header: Some(header),
            items,
            container: None,
        };
        let response = unpack_required::<kiapi::common::commands::CreateItemsResponse>(
            self.send_command(&cmd, "kiapi.common.commands.CreateItems")?,
            "CreateItems",
        )?;
        ensure_item_request_ok(response.status, "item creation")?;
        if response.created_items.is_empty() {
            // KiCad 10.0's observed behaviour for a request it ignores: an
            // IRS_OK response carrying no per-item results at all.
            anyhow::bail!(
                "KiCad created no items: the CreateItems response carried no items at all \
                 ({expected_count} requested)"
            );
        }
        let mut created = 0usize;
        let mut rejections = Vec::new();
        for (index, result) in response.created_items.iter().enumerate() {
            use kiapi::common::commands::ItemStatusCode;
            let code = result
                .status
                .as_ref()
                .map(|status| status.code())
                .unwrap_or(ItemStatusCode::IscUnknown);
            let is_created = match code {
                ItemStatusCode::IscOk => true,
                // A defaulted status is only evidence of success when the
                // created item itself came back alongside it.
                ItemStatusCode::IscUnknown => result.item.is_some(),
                _ => false,
            };
            if is_created {
                created += 1;
            } else {
                let message = result
                    .status
                    .as_ref()
                    .map(|status| status.error_message.as_str())
                    .filter(|message| !message.is_empty())
                    .unwrap_or("no error message");
                rejections.push(format!("item {index}: {} ({message})", code.as_str_name()));
            }
        }
        if created != expected_count {
            anyhow::bail!(
                "KiCad created {created} of {expected_count} requested items{}{}",
                if rejections.is_empty() { "" } else { ": " },
                rejections.join("; ")
            );
        }
        Ok(())
    }

    /// Update existing items by KIID. Generic wrapper mirroring create_items/delete_items;
    /// each `Any` must be a fully-formed board item with an existing `id` populated.
    pub fn update_items(&self, items: Vec<prost_types::Any>) -> Result<()> {
        self.update_items_in(self.get_board_document()?, items)
    }

    /// As [`Self::update_items`], targeting a specific open document.
    pub fn update_items_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
        items: Vec<prost_types::Any>,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let expected_count = items.len();
        let header = header_for(document);
        let cmd = kiapi::common::commands::UpdateItems {
            header: Some(header),
            items,
        };
        let response = unpack_required::<kiapi::common::commands::UpdateItemsResponse>(
            self.send_command(&cmd, "kiapi.common.commands.UpdateItems")?,
            "UpdateItems",
        )?;
        ensure_item_request_ok(response.status, "item update")?;
        if response.updated_items.len() != expected_count {
            anyhow::bail!(
                "KiCad returned {} update results for {} requested items",
                response.updated_items.len(),
                expected_count
            );
        }
        for result in response.updated_items {
            let Some(status) = result.status else {
                anyhow::bail!("KiCad returned an update result without item status");
            };
            if status.code() != kiapi::common::commands::ItemStatusCode::IscOk {
                anyhow::bail!(
                    "KiCad item update failed: {} ({})",
                    status.error_message,
                    status.code().as_str_name()
                );
            }
        }
        Ok(())
    }

    /// Delete items by KIID.
    pub fn delete_items(&self, ids: Vec<String>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let expected_count = ids.len();
        let mut expected_ids = ids
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        if expected_ids.len() != expected_count {
            anyhow::bail!("delete request contains duplicate item identifiers");
        }
        let header = self.make_header()?;
        let cmd = kiapi::common::commands::DeleteItems {
            header: Some(header),
            item_ids: ids
                .iter()
                .map(|id| kiapi::common::types::Kiid { value: id.clone() })
                .collect(),
        };
        let response = unpack_required::<kiapi::common::commands::DeleteItemsResponse>(
            self.send_command(&cmd, "kiapi.common.commands.DeleteItems")?,
            "DeleteItems",
        )?;
        ensure_item_request_ok(response.status, "item deletion")?;
        // KiCad 10 builds per-item results in
        // API_HANDLER_EDITOR::handleDeleteItems and then never attaches them
        // to the response, so `deleted_items` comes back empty on every
        // successful delete. Treating that as failure made delete_component
        // report "0 deletion results" for deletions that had in fact happened
        // (#116). An empty list carries no information either way, so callers
        // that need certainty verify the item is gone; a NON-empty list is
        // still validated strictly, so this stays correct if KiCad starts
        // populating it.
        if response.deleted_items.is_empty() {
            return Ok(());
        }
        if response.deleted_items.len() != expected_count {
            anyhow::bail!(
                "KiCad returned {} deletion results for {} requested items",
                response.deleted_items.len(),
                expected_count
            );
        }
        for result in response.deleted_items {
            let status = result.status();
            let id = result
                .id
                .context("KiCad returned a deletion result without an item identifier")?
                .value;
            if !expected_ids.remove(&id) {
                anyhow::bail!("KiCad returned a deletion result for unexpected item '{id}'");
            }
            if status != kiapi::common::commands::ItemDeletionStatus::IdsOk {
                anyhow::bail!(
                    "KiCad failed to delete item '{}': {}",
                    id,
                    status.as_str_name()
                );
            }
        }
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
        let response: kiapi::common::commands::BeginCommitResponse =
            unpack_required(response_any, "BeginCommit")?;
        let id = response
            .id
            .context("KiCad returned BeginCommit without a commit identifier")?
            .value;
        if id.is_empty() {
            anyhow::bail!("KiCad returned an empty commit identifier");
        }
        Ok(id)
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
        let _: kiapi::common::commands::EndCommitResponse = unpack_required(
            self.send_command(&cmd, "kiapi.common.commands.EndCommit")?,
            "EndCommit",
        )?;
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

    /// Run a multi-step mutation as one KiCad undo transaction.
    ///
    /// Any operation error, or a failure to publish the commit, triggers a
    /// best-effort drop so callers never knowingly leave a partial batch.
    pub fn run_commit<T>(
        &self,
        description: &str,
        operation: impl FnOnce(&Self) -> Result<T>,
    ) -> Result<T> {
        let commit_id = self.begin_commit()?;
        let operation_result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(self)));
        match operation_result {
            Err(panic) => {
                if let Err(rollback_error) = self.drop_commit(&commit_id) {
                    anyhow::bail!("KiCad batch panicked and rollback failed ({rollback_error})");
                }
                std::panic::resume_unwind(panic)
            }
            Ok(Ok(value)) => {
                if let Err(commit_error) = self.push_commit(&commit_id, description) {
                    let rollback_error = self.drop_commit(&commit_id).err();
                    if let Some(rollback_error) = rollback_error {
                        anyhow::bail!(
                            "failed to publish KiCad commit ({commit_error}); rollback also failed ({rollback_error})"
                        );
                    }
                    return Err(commit_error)
                        .context("failed to publish KiCad commit; changes dropped");
                }
                Ok(value)
            }
            Ok(Err(operation_error)) => {
                if let Err(rollback_error) = self.drop_commit(&commit_id) {
                    anyhow::bail!(
                        "KiCad batch failed ({operation_error}); rollback also failed ({rollback_error})"
                    );
                }
                Err(operation_error).context("KiCad batch failed; changes dropped")
            }
        }
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

    /// Read a placed footprint's pads from the open board, or `None` when no
    /// footprint carries `reference`.
    ///
    /// Pads come back in absolute board coordinates, because that is how
    /// KiCad serializes a footprint's children (see the `transform` module) —
    /// the anchor/rotation transform the file path has to apply is already
    /// baked in here.
    pub fn get_footprint_pads_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
        reference: &str,
    ) -> Result<Option<Vec<IpcPad>>> {
        let items = self.get_items_in(
            document,
            kiapi::common::types::KiCadObjectType::KotPcbFootprint,
        )?;
        let mut found = None;
        for item in &items {
            if !crate::builders::any_is(item, "kiapi.board.types.FootprintInstance") {
                continue;
            }
            let fp = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
                .context("KiCad returned an unreadable footprint instance")?;
            if footprint_reference(&fp) != reference {
                continue;
            }
            if found.is_some() {
                anyhow::bail!(
                    "footprint reference '{}' appears more than once on the board",
                    reference
                );
            }

            let definition = fp
                .definition
                .as_ref()
                .with_context(|| format!("footprint '{reference}' has no definition"))?;
            let mut pads = Vec::new();
            for child in &definition.items {
                if !crate::builders::any_is(child, "kiapi.board.types.Pad") {
                    continue;
                }
                let pad = kiapi::board::types::Pad::decode(child.value.as_slice())
                    .with_context(|| format!("footprint '{reference}' has an unreadable pad"))?;
                let position = pad.position.with_context(|| {
                    format!(
                        "footprint '{reference}' pad '{}' has no position",
                        pad.number
                    )
                })?;
                let layers = pad
                    .pad_stack
                    .as_ref()
                    .map(|stack| {
                        stack
                            .layers
                            .iter()
                            .map(|layer| layer_enum_to_name(*layer).to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                pads.push(IpcPad {
                    number: pad.number,
                    x: nm_to_mm(position.x_nm),
                    y: nm_to_mm(position.y_nm),
                    net: pad.net.map(|net| net.name).unwrap_or_default(),
                    layers,
                });
            }
            found = Some(pads);
        }
        Ok(found)
    }

    /// Read the title block of a specific open document.
    pub fn get_title_block_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
    ) -> Result<IpcTitleBlock> {
        let cmd = kiapi::common::commands::GetTitleBlockInfo {
            document: Some(document),
        };
        let response = self.send_command(&cmd, "kiapi.common.commands.GetTitleBlockInfo")?;
        let info: kiapi::common::types::TitleBlockInfo =
            unpack_required(response, "GetTitleBlockInfo")?;
        Ok(IpcTitleBlock {
            title: info.title,
            date: info.date,
            revision: info.revision,
            company: info.company,
        })
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

    /// Add a through via (F.Cu → B.Cu) to the board.
    ///
    /// Uses the protobuf `Via` message via `create_items`, the same path as
    /// [`add_track`](Self::add_track). The previous implementation sent a bare
    /// `(via …)` S-expression through `ParseAndCreateItemsFromString`, which
    /// returned success but never created the via (see `builders::build_via`).
    pub fn add_via(&self, net_name: &str, x: f64, y: f64, drill: f64, pad_size: f64) -> Result<()> {
        let net_code = self.resolve_net_code(net_name)?;
        let via = crate::builders::build_via(net_name, net_code, x, y, drill, pad_size);
        let any = crate::builders::pack_any(&via, "kiapi.board.types.Via");
        self.create_items(vec![any])?;
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
                let uuid = track
                    .id
                    .as_ref()
                    .map(|id| id.value.clone())
                    .unwrap_or_default();
                tracks.push(IpcTrack {
                    uuid,
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
                    self.update_items(vec![any])?;
                    return Ok(());
                }
            }
        }
        anyhow::bail!("Footprint '{}' not found", reference)
    }

    /// List the graphic items inside a placed footprint.
    ///
    /// Points come back in footprint-local millimetres — the coordinates the
    /// `.kicad_mod` shows — not the absolute board coordinates KiCad carries
    /// them in over IPC. See `set_footprint_graphic_points` for why.
    pub fn list_footprint_graphics(&self, reference: &str) -> Result<Vec<IpcFootprintGraphic>> {
        let (fp, _) = self.find_footprint_instance(reference)?;
        let (anchor, angle) = footprint_frame(&fp);
        let to_local = crate::transform::Xform::Rotate {
            cx_nm: anchor.0,
            cy_nm: anchor.1,
            delta_deg: -angle,
        };

        let mut out = Vec::new();
        for any in &fp
            .definition
            .as_ref()
            .map(|d| d.items.clone())
            .unwrap_or_default()
        {
            if !crate::builders::any_is(any, "kiapi.board.types.BoardGraphicShape") {
                continue;
            }
            let Ok(shape) = kiapi::board::types::BoardGraphicShape::decode(any.value.as_slice())
            else {
                continue;
            };
            let Some(geometry) = shape.shape.as_ref().and_then(|s| s.geometry.as_ref()) else {
                continue;
            };
            let uuid = shape
                .id
                .as_ref()
                .map(|k| k.value.clone())
                .unwrap_or_default();
            let kind = geometry_kind(geometry).to_string();
            let (outlines, holes) = polygon_extent(geometry);
            out.push(IpcFootprintGraphic {
                editable: kind == "polygon" && outlines == 1 && holes == 0 && !uuid.is_empty(),
                uuid,
                kind,
                outlines,
                holes,
                layer: layer_enum_to_name(shape.layer).to_string(),
                points: geometry_points(geometry)
                    .into_iter()
                    .map(|(x, y)| {
                        let (lx, ly) = to_local.point(x, y);
                        IpcVector2 {
                            x: nm_to_mm(lx - anchor.0),
                            y: nm_to_mm(ly - anchor.1),
                        }
                    })
                    .collect(),
            });
        }
        Ok(out)
    }

    /// Replace the vertices of one graphic item inside a placed footprint.
    ///
    /// `points_mm` are footprint-local, matching what the `.kicad_mod` shows.
    /// KiCad carries a footprint's children in absolute board coordinates over
    /// IPC and re-creates them verbatim on update (the same reason
    /// `move_footprint` has to shift them), so they are rotated by the
    /// instance's orientation and translated onto its position here rather
    /// than making every caller redo that arithmetic against the wrong frame.
    pub fn set_footprint_graphic_points(
        &self,
        reference: &str,
        uuid: &str,
        points_mm: &[(f64, f64)],
    ) -> Result<String> {
        let (mut fp, _) = self.find_footprint_instance(reference)?;
        let (anchor, angle) = footprint_frame(&fp);
        let to_board = crate::transform::Xform::Rotate {
            cx_nm: 0,
            cy_nm: 0,
            delta_deg: angle,
        };

        let definition = fp
            .definition
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("footprint '{}' carries no definition", reference))?;

        for any in definition.items.iter_mut() {
            // Discriminate before decoding. A pad decodes happily as a
            // `BoardGraphicShape`, so this loop was selecting candidates the
            // same way `apply_footprint_fields` did when it destroyed every
            // footprint's artwork (#244).
            //
            // Measured, so as not to overstate it: today the pad cannot
            // actually reach the write. `Pad.id` is tag 1 and
            // `BoardGraphicShape.id` is tag 4, so a pad's KIID does *not* land
            // in `shape.id`, the uuid never matches, and the geometry guard
            // would refuse it anyway. Both of those are accidents of the
            // current schema. The check makes the safety a property of this
            // code instead of of KiCad's field numbering.
            if !crate::builders::any_is(any, "kiapi.board.types.BoardGraphicShape") {
                continue;
            }
            let Ok(mut shape) =
                kiapi::board::types::BoardGraphicShape::decode(any.value.as_slice())
            else {
                continue;
            };
            if shape.id.as_ref().map(|k| k.value.as_str()) != Some(uuid) {
                continue;
            }

            let kind =
                replace_polygon_outline(&mut shape, reference, uuid, points_mm, anchor, &to_board)?;

            *any = pack_any(&shape, "kiapi.board.types.BoardGraphicShape");
            let packed = pack_any(&fp, "kiapi.board.types.FootprintInstance");
            self.update_items(vec![packed])?;
            return Ok(kind.to_string());
        }

        anyhow::bail!(
            "graphic '{}' not found on footprint '{}' — call list_board_footprint_graphics \
             for the UUIDs it does have",
            uuid,
            reference
        )
    }

    /// The placed footprint carrying `reference`, and its index among items.
    fn find_footprint_instance(
        &self,
        reference: &str,
    ) -> Result<(kiapi::board::types::FootprintInstance, usize)> {
        let items = self.get_items(kiapi::common::types::KiCadObjectType::KotPcbFootprint)?;
        for (i, item) in items.iter().enumerate() {
            if let Ok(fp) = kiapi::board::types::FootprintInstance::decode(item.value.as_slice()) {
                let ref_text = fp
                    .reference_field
                    .as_ref()
                    .and_then(|f| f.text.as_ref())
                    .and_then(|bt| bt.text.as_ref())
                    .map(|t| t.text.as_str())
                    .unwrap_or("");
                if ref_text == reference {
                    return Ok((fp, i));
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
                    self.update_items(vec![any])?;
                    return Ok(());
                }
            }
        }
        anyhow::bail!("Footprint '{}' not found", reference)
    }

    /// Update the visible value field of an existing footprint.
    pub fn set_footprint_value(&self, reference: &str, value: &str) -> Result<()> {
        let items = self.get_items(kiapi::common::types::KiCadObjectType::KotPcbFootprint)?;
        for item in items {
            if let Ok(mut footprint) =
                kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
            {
                let current_reference = footprint
                    .reference_field
                    .as_ref()
                    .and_then(|field| field.text.as_ref())
                    .and_then(|board_text| board_text.text.as_ref())
                    .map(|text| text.text.as_str())
                    .unwrap_or("");
                if current_reference != reference {
                    continue;
                }
                if let Some(text) = footprint
                    .value_field
                    .as_mut()
                    .and_then(|field| field.text.as_mut())
                    .and_then(|board_text| board_text.text.as_mut())
                {
                    text.text = value.to_string();
                } else {
                    anyhow::bail!("Footprint '{reference}' has no editable value field");
                }
                if let Some(text) = footprint
                    .definition
                    .as_mut()
                    .and_then(|definition| definition.value_field.as_mut())
                    .and_then(|field| field.text.as_mut())
                    .and_then(|board_text| board_text.text.as_mut())
                {
                    text.text = value.to_string();
                }
                let any =
                    crate::builders::pack_any(&footprint, "kiapi.board.types.FootprintInstance");
                self.update_items(vec![any])?;
                return Ok(());
            }
        }
        anyhow::bail!("Footprint '{reference}' not found")
    }

    /// Delete a footprint by reference.
    pub fn delete_footprint(&self, reference: &str) -> Result<()> {
        let kiid = self.find_footprint_kiid(reference)?;
        self.delete_items(vec![kiid])?;
        // KiCad returns no per-item deletion results (#116), so the response
        // alone cannot distinguish "deleted" from "silently skipped". Ask the
        // board instead — reporting success without evidence is the failure
        // mode #99 was closed to eliminate.
        match self.find_footprint_kiid(reference) {
            Err(_) => Ok(()),
            Ok(_) => anyhow::bail!(
                "KiCad reported no error but footprint '{reference}' is still on the board"
            ),
        }
    }

    /// Build a typed footprint item suitable for [`Self::create_items`].
    ///
    /// Children (pads and graphics) are emitted in ABSOLUTE board
    /// coordinates: KiCAD serializes `FootprintInstance` children that way
    /// and re-creates them verbatim (issue #23 — see the `transform` module
    /// docs), so every footprint-local point is rotated and translated here.
    #[allow(clippy::too_many_arguments)]
    pub fn build_footprint_item(
        &self,
        lib_id: &str,
        reference: &str,
        value: &str,
        pads: &[IpcPadDefinition],
        graphics: &[IpcGraphicDefinition],
        fields: &IpcFieldPlacement,
        x: f64,
        y: f64,
        rotation: f64,
        layer: &str,
    ) -> Result<prost_types::Any> {
        let (library_nickname, entry_name) = lib_id
            .split_once(':')
            .context("footprint identifier must use Library:Footprint syntax")?;

        // Refuse an unrepresentable layer here, before a single child is built.
        //
        // KiCAD 10.0.5 does not validate the scalar layer field on an incoming
        // item: it indexes its layer bitset with whatever value arrives, so a
        // `BL_UNDEFINED` is answered with an access violation that kills the
        // process and takes the user's unsaved board with it (#237). There is
        // no error to catch downstream — the transport just times out — so the
        // check has to happen while a refusal is still possible.
        crate::builders::try_layer_from_name(layer)
            .with_context(|| format!("footprint '{lib_id}' cannot be placed"))?;
        for pad in pads {
            for name in &pad.layers {
                // `*.Cu`/`*.Mask`/`*.Paste` are KiCAD's own wildcards, expanded
                // below rather than mapped.
                if name.starts_with("*.") {
                    continue;
                }
                crate::builders::try_layer_from_name(name).with_context(|| {
                    format!("footprint '{lib_id}' pad '{}' cannot be placed", pad.number)
                })?;
            }
        }
        for graphic in graphics {
            crate::builders::try_layer_from_name(graphic.layer()).with_context(|| {
                format!(
                    "footprint '{lib_id}' has a {} this build cannot place",
                    graphic.kind()
                )
            })?;
        }

        let text_field = |name: &str, text: &str, local: (f64, f64, f64), visible: bool| {
            // Field text positions come footprint-local from the library and
            // are transformed exactly like pads, so the placed part keeps the
            // library's text layout instead of a synthesized offset that can
            // sit on the part's own silkscreen.
            let (fx, fy, frot) = local;
            let (bx, by) = konnect_sexp::geometry::transform_pad(fx, fy, x, y, rotation);
            kiapi::board::types::Field {
                id: None,
                name: name.to_string(),
                text: Some(kiapi::board::types::BoardText {
                    id: None,
                    text: Some(kiapi::common::types::Text {
                        position: Some(crate::builders::vec2(bx, by)),
                        attributes: Some(kiapi::common::types::TextAttributes {
                            size: Some(crate::builders::vec2(1.0, 1.0)),
                            angle: Some(kiapi::common::types::Angle {
                                value_degrees: readable_text_angle(rotation + frot),
                            }),
                            ..Default::default()
                        }),
                        text: text.to_string(),
                        hyperlink: String::new(),
                    }),
                    layer: crate::builders::layer_from_name(if layer == "B.Cu" {
                        "B.SilkS"
                    } else {
                        "F.SilkS"
                    }) as i32,
                    knockout: false,
                    locked: kiapi::common::types::LockedState::LsUnlocked as i32,
                }),
                visible,
            }
        };
        let reference_field = text_field(
            "Reference",
            reference,
            fields.reference_at.unwrap_or((0.0, -1.0, 0.0)),
            true,
        );
        let value_field = text_field(
            "Value",
            value,
            fields.value_at.unwrap_or((0.0, 1.0, 0.0)),
            false,
        );
        let mut child_items: Vec<prost_types::Any> = pads
            .iter()
            .map(|pad| {
                // Canonical KiCAD footprint-local → board transform; see
                // konnect_sexp::geometry::transform_pad for why the sin terms
                // are not the textbook rotation matrix (Y axis points down).
                let (board_x, board_y) =
                    konnect_sexp::geometry::transform_pad(pad.x, pad.y, x, y, rotation);
                let mut layers = Vec::new();
                for name in &pad.layers {
                    match name.as_str() {
                        "*.Cu" => layers.extend(3..=34),
                        "*.Mask" => layers.extend([
                            kiapi::board::types::BoardLayer::BlFMask as i32,
                            kiapi::board::types::BoardLayer::BlBMask as i32,
                        ]),
                        "*.Paste" => layers.extend([
                            kiapi::board::types::BoardLayer::BlFPaste as i32,
                            kiapi::board::types::BoardLayer::BlBPaste as i32,
                        ]),
                        name => layers.push(crate::builders::layer_from_name(name) as i32),
                    }
                }
                layers
                    .retain(|layer| *layer != kiapi::board::types::BoardLayer::BlUndefined as i32);
                layers.sort_unstable();
                layers.dedup();

                let shape = match pad.shape.as_str() {
                    "circle" => kiapi::board::types::PadStackShape::PssCircle,
                    "rect" => kiapi::board::types::PadStackShape::PssRectangle,
                    "oval" => kiapi::board::types::PadStackShape::PssOval,
                    "trapezoid" => kiapi::board::types::PadStackShape::PssTrapezoid,
                    "roundrect" => kiapi::board::types::PadStackShape::PssRoundrect,
                    "chamfered_rect" => kiapi::board::types::PadStackShape::PssChamferedrect,
                    _ => kiapi::board::types::PadStackShape::PssRectangle,
                };
                // Always F_Cu: it is KiCad's ALL_LAYERS sentinel for a
                // PST_NORMAL stack, not a statement about which side the pad
                // is on (that is `layers`). PADSTACK::unpackCopperLayer
                // rejects any other value while the mode is NORMAL, failing
                // the whole deserialization — see #117.
                let copper = kiapi::board::types::PadStackLayer {
                    layer: kiapi::board::types::BoardLayer::BlFCu as i32,
                    shape: shape as i32,
                    size: Some(crate::builders::vec2(pad.size_x, pad.size_y)),
                    corner_rounding_ratio: pad.roundrect_ratio,
                    custom_anchor_shape: shape as i32,
                    offset: Some(crate::builders::vec2(0.0, 0.0)),
                    ..Default::default()
                };
                let drill = pad
                    .drill_x
                    .map(|drill_x| kiapi::board::types::DrillProperties {
                        start_layer: kiapi::board::types::BoardLayer::BlFCu as i32,
                        end_layer: kiapi::board::types::BoardLayer::BlBCu as i32,
                        diameter: Some(crate::builders::vec2(
                            drill_x,
                            pad.drill_y.unwrap_or(drill_x),
                        )),
                        shape: if pad.drill_oval {
                            kiapi::board::types::DrillShape::DsOblong as i32
                        } else {
                            kiapi::board::types::DrillShape::DsCircle as i32
                        },
                        ..Default::default()
                    });
                let stack = kiapi::board::types::PadStack {
                    r#type: kiapi::board::types::PadStackType::PstNormal as i32,
                    layers,
                    drill,
                    unconnected_layer_removal: kiapi::board::types::UnconnectedLayerRemoval::UlrKeep
                        as i32,
                    copper_layers: vec![copper],
                    angle: Some(kiapi::common::types::Angle {
                        value_degrees: rotation + pad.rotation,
                    }),
                    ..Default::default()
                };
                let pad_type = match pad.pad_type.as_str() {
                    "thru_hole" => kiapi::board::types::PadType::PtPth,
                    "np_thru_hole" => kiapi::board::types::PadType::PtNpth,
                    "connect" => kiapi::board::types::PadType::PtEdgeConnector,
                    _ => kiapi::board::types::PadType::PtSmd,
                };
                let item = kiapi::board::types::Pad {
                    number: pad.number.clone(),
                    r#type: pad_type as i32,
                    pad_stack: Some(stack),
                    position: Some(crate::builders::vec2(board_x, board_y)),
                    locked: kiapi::common::types::LockedState::LsUnlocked as i32,
                    ..Default::default()
                };
                crate::builders::pack_any(&item, "kiapi.board.types.Pad")
            })
            .collect();
        child_items.extend(
            graphics
                .iter()
                .map(|graphic| build_graphic_child(graphic, x, y, rotation)),
        );
        let definition = kiapi::board::types::Footprint {
            id: Some(kiapi::common::types::LibraryIdentifier {
                library_nickname: library_nickname.to_string(),
                entry_name: entry_name.to_string(),
            }),
            reference_field: Some(reference_field.clone()),
            value_field: Some(value_field.clone()),
            items: child_items,
            ..Default::default()
        };
        let footprint = kiapi::board::types::FootprintInstance {
            position: Some(crate::builders::vec2(x, y)),
            orientation: Some(kiapi::common::types::Angle {
                value_degrees: rotation,
            }),
            layer: crate::builders::layer_from_name(layer) as i32,
            locked: kiapi::common::types::LockedState::LsUnlocked as i32,
            definition: Some(definition),
            reference_field: Some(reference_field),
            value_field: Some(value_field),
            ..Default::default()
        };
        Ok(crate::builders::pack_any(
            &footprint,
            "kiapi.board.types.FootprintInstance",
        ))
    }

    /// Place and verify a footprint instance through the typed KiCad API.
    #[allow(clippy::too_many_arguments)]
    pub fn place_footprint(
        &self,
        board: &Path,
        lib_id: &str,
        reference: &str,
        value: &str,
        pads: &[IpcPadDefinition],
        graphics: &[IpcGraphicDefinition],
        fields: &IpcFieldPlacement,
        x: f64,
        y: f64,
        rotation: f64,
        layer: &str,
    ) -> Result<IpcFootprint> {
        // Target the document matching `board`, not whichever board is first
        // in KiCad's open list — with several boards open, first-document
        // targeting mutates the wrong one (caught in live verification).
        let document = self.find_open_board(board)?;
        if self
            .list_footprints_in(document.clone())?
            .iter()
            .any(|footprint| footprint.reference == reference)
        {
            anyhow::bail!("footprint reference '{reference}' already exists on the board");
        }
        let item = self.build_footprint_item(
            lib_id, reference, value, pads, graphics, fields, x, y, rotation, layer,
        )?;
        self.create_items_in(document.clone(), vec![item])?;
        let footprints = self.list_footprints_in(document)?;
        footprints
            .iter()
            .find(|footprint| footprint.reference == reference)
            .cloned()
            .with_context(|| {
                let references = footprints
                    .iter()
                    .map(|footprint| footprint.reference.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "KiCad created the footprint but reference '{reference}' was not found (board references: {references})"
                )
            })
    }

    /// Get board extents (bounding box of all items).
    pub fn get_board_extents(&self) -> Result<IpcBoardExtents> {
        self.get_board_extents_in(self.get_board_document()?)
    }

    /// As [`Self::get_board_extents`], targeting a specific open document.
    pub fn get_board_extents_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
    ) -> Result<IpcBoardExtents> {
        self.get_optional_board_extents_in(document)?
            .context("No bounding box returned from KiCAD")
    }

    /// Return no bounds for a completely empty board instead of treating the
    /// valid empty `GetBoundingBox` response as an IPC failure.
    pub fn get_optional_board_extents_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
    ) -> Result<Option<IpcBoardExtents>> {
        // Use GetBoundingBox with no specific items = board extents
        let header = header_for(document);
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
                return Ok(Some(IpcBoardExtents {
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
                }));
            }
        }
        Ok(None)
    }

    /// Get enabled layers.
    pub fn get_layers(&self) -> Result<Vec<IpcLayer>> {
        self.get_layers_in(self.get_board_document()?)
    }

    /// As [`Self::get_layers`], targeting a specific open document.
    pub fn get_layers_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
    ) -> Result<Vec<IpcLayer>> {
        Ok(self.get_enabled_layers_in(document)?.layers)
    }

    /// As [`Self::get_layers_in`], keeping the copper count KiCad reports
    /// beside the layer list instead of leaving callers to derive it.
    pub fn get_enabled_layers_in(
        &self,
        document: kiapi::common::types::DocumentSpecifier,
    ) -> Result<IpcEnabledLayers> {
        let cmd = kiapi::board::commands::GetBoardEnabledLayers {
            board: Some(document),
        };
        let resp_any = self.send_command(&cmd, "kiapi.board.commands.GetBoardEnabledLayers")?;
        let Some(any) = resp_any else {
            return Ok(IpcEnabledLayers {
                copper_layer_count: 0,
                layers: vec![],
            });
        };
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
        Ok(IpcEnabledLayers {
            copper_layer_count: resp.copper_layer_count,
            layers,
        })
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

/// True when `rotation` keeps axis-aligned shapes axis-aligned.
fn is_cardinal_rotation(rotation_deg: f64) -> bool {
    let r = rotation_deg.rem_euclid(90.0);
    r.abs() < 1e-9 || (90.0 - r).abs() < 1e-9
}

/// Normalize a text angle the way KiCAD keeps labels readable: fold into
/// [0, 360), then flip anything that would read upside down by 180°.
fn readable_text_angle(deg: f64) -> f64 {
    let mut angle = deg.rem_euclid(360.0);
    if angle > 90.0 && angle <= 270.0 {
        angle -= 180.0;
    }
    angle
}

/// Transform one footprint-local graphic into an absolute-board-space child
/// item for a `FootprintInstance` (see [`KiCadIpcClient::build_footprint_item`]).
fn build_graphic_child(
    graphic: &IpcGraphicDefinition,
    x: f64,
    y: f64,
    rotation: f64,
) -> prost_types::Any {
    use crate::builders;
    const SHAPE: &str = "kiapi.board.types.BoardGraphicShape";
    let xf = |(px, py): (f64, f64)| konnect_sexp::geometry::transform_pad(px, py, x, y, rotation);
    match graphic {
        IpcGraphicDefinition::Line {
            start,
            end,
            layer,
            width,
        } => {
            let (x1, y1) = xf(*start);
            let (x2, y2) = xf(*end);
            builders::pack_any(
                &builders::board_segment(layer, *width, x1, y1, x2, y2),
                SHAPE,
            )
        }
        IpcGraphicDefinition::Rect {
            start,
            end,
            layer,
            width,
            filled,
        } => {
            if is_cardinal_rotation(rotation) {
                // A 90°-multiple keeps the rectangle axis-aligned; rotate the
                // corners and re-normalize which one is top-left.
                let (x1, y1) = xf(*start);
                let (x2, y2) = xf(*end);
                builders::pack_any(
                    &builders::board_rectangle(
                        layer,
                        *width,
                        x1.min(x2),
                        y1.min(y2),
                        x1.max(x2),
                        y1.max(y2),
                        *filled,
                    ),
                    SHAPE,
                )
            } else {
                // The Rectangle message is axis-aligned by construction, so a
                // non-cardinal rotation emits the four rotated corners as a
                // polygon — the same degradation EDA_SHAPE::Rotate applies.
                let corners = vec![
                    xf(*start),
                    xf((end.0, start.1)),
                    xf(*end),
                    xf((start.0, end.1)),
                ];
                builders::pack_any(
                    &builders::board_polygon(layer, *width, *filled, &[corners]),
                    SHAPE,
                )
            }
        }
        IpcGraphicDefinition::Circle {
            center,
            end,
            layer,
            width,
            filled,
        } => {
            let (cx, cy) = xf(*center);
            // The radius is rotation-invariant; keep KiCAD's center +
            // circumference-point encoding by re-deriving it from the length.
            let radius = ((end.0 - center.0).powi(2) + (end.1 - center.1).powi(2)).sqrt();
            builders::pack_any(
                &builders::board_circle(layer, *width, cx, cy, radius, *filled),
                SHAPE,
            )
        }
        IpcGraphicDefinition::Arc {
            start,
            mid,
            end,
            layer,
            width,
        } => {
            let (sx, sy) = xf(*start);
            let (mx, my) = xf(*mid);
            let (ex, ey) = xf(*end);
            builders::pack_any(
                &builders::board_arc(layer, *width, sx, sy, mx, my, ex, ey),
                SHAPE,
            )
        }
        IpcGraphicDefinition::Poly {
            points,
            layer,
            width,
            filled,
        } => {
            let transformed: Vec<(f64, f64)> = points.iter().map(|p| xf(*p)).collect();
            builders::pack_any(
                &builders::board_polygon(layer, *width, *filled, &[transformed]),
                SHAPE,
            )
        }
        IpcGraphicDefinition::Text {
            text,
            position,
            rotation: text_rotation,
            layer,
            size,
        } => {
            let (tx, ty) = xf(*position);
            builders::pack_any(
                &builders::board_text(
                    layer,
                    text,
                    tx,
                    ty,
                    *size,
                    readable_text_angle(text_rotation + rotation),
                    false,
                ),
                "kiapi.board.types.BoardText",
            )
        }
    }
}

/// The reference designator text of a placed footprint, or `""` when the
/// instance carries no reference field.
fn footprint_reference(footprint: &kiapi::board::types::FootprintInstance) -> &str {
    footprint
        .reference_field
        .as_ref()
        .and_then(|field| field.text.as_ref())
        .and_then(|text| text.text.as_ref())
        .map(|text| text.text.as_str())
        .unwrap_or("")
}

fn header_for(
    document: kiapi::common::types::DocumentSpecifier,
) -> kiapi::common::types::ItemHeader {
    kiapi::common::types::ItemHeader {
        document: Some(document),
        container: None,
        field_mask: None,
    }
}

fn board_document_path(document: &kiapi::common::types::DocumentSpecifier) -> Option<PathBuf> {
    use kiapi::common::types::document_specifier::Identifier;

    let Identifier::BoardFilename(filename) = document.identifier.as_ref()? else {
        return None;
    };
    let path = PathBuf::from(filename);
    if path.is_absolute() {
        return Some(path);
    }
    document
        .project
        .as_ref()
        .filter(|project| !project.path.is_empty())
        .map(|project| PathBuf::from(&project.path).join(&path))
        .or(Some(path))
}

fn paths_refer_to_same_board(requested: &Path, active: &Path) -> bool {
    match (requested.canonicalize(), active.canonicalize()) {
        (Ok(requested), Ok(active)) => requested == active,
        _ if active.components().count() == 1 => {
            requested.components().count() == 1 && requested.file_name() == active.file_name()
        }
        _ => requested == active,
    }
}

#[cfg(test)]
mod document_path_tests {
    use super::*;

    #[test]
    fn relative_board_filename_is_resolved_against_project_path() {
        let document = kiapi::common::types::DocumentSpecifier {
            r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
            identifier: Some(
                kiapi::common::types::document_specifier::Identifier::BoardFilename(
                    "controller.kicad_pcb".to_string(),
                ),
            ),
            project: Some(kiapi::common::types::ProjectSpecifier {
                name: "controller".to_string(),
                path: "/work/controller".to_string(),
            }),
        };

        assert_eq!(
            board_document_path(&document).unwrap(),
            PathBuf::from("/work/controller/controller.kicad_pcb")
        );
    }

    #[test]
    fn bare_kicad_filename_is_not_enough_to_authorize_an_absolute_request() {
        assert!(!paths_refer_to_same_board(
            Path::new("/work/controller/controller.kicad_pcb"),
            Path::new("controller.kicad_pcb")
        ));
        assert!(paths_refer_to_same_board(
            Path::new("controller.kicad_pcb"),
            Path::new("controller.kicad_pcb")
        ));
        assert!(!paths_refer_to_same_board(
            Path::new("/work/controller/other.kicad_pcb"),
            Path::new("controller.kicad_pcb")
        ));
    }
}

#[cfg(test)]
mod footprint_graphics_tests {
    use super::*;
    use prost::Message;

    fn build(
        graphics: &[IpcGraphicDefinition],
        x: f64,
        y: f64,
        rotation: f64,
    ) -> kiapi::board::types::FootprintInstance {
        // build_footprint_item is pure — no IPC round-trip — so the socket
        // path is never dialed.
        let client = KiCadIpcClient::new("tcp://never-dialed");
        let any = client
            .build_footprint_item(
                "Lib:Fp",
                "R1",
                "R",
                &[],
                graphics,
                &crate::types::IpcFieldPlacement::default(),
                x,
                y,
                rotation,
                "F.Cu",
            )
            .unwrap();
        kiapi::board::types::FootprintInstance::decode(any.value.as_slice()).unwrap()
    }

    fn shapes(
        fp: &kiapi::board::types::FootprintInstance,
    ) -> Vec<kiapi::board::types::BoardGraphicShape> {
        fp.definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|any| any.type_url.ends_with("BoardGraphicShape"))
            .map(|any| {
                kiapi::board::types::BoardGraphicShape::decode(any.value.as_slice()).unwrap()
            })
            .collect()
    }

    fn texts(fp: &kiapi::board::types::FootprintInstance) -> Vec<kiapi::board::types::BoardText> {
        fp.definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|any| any.type_url.ends_with("BoardText"))
            .map(|any| kiapi::board::types::BoardText::decode(any.value.as_slice()).unwrap())
            .collect()
    }

    /// Courtyard rect + silk line + fab text at rotation 90 must come out on
    /// their own layers, at absolute board coordinates rotated with the part.
    ///
    /// transform_pad at 90°: (lx, ly) → (x + ly, y − lx).
    #[test]
    fn rotation_90_pretransforms_graphics_to_absolute_board_space() {
        let graphics = vec![
            IpcGraphicDefinition::Rect {
                start: (-1.0, -2.0),
                end: (1.0, 2.0),
                layer: "F.CrtYd".to_string(),
                width: 0.05,
                filled: false,
            },
            IpcGraphicDefinition::Line {
                start: (-1.0, 0.0),
                end: (1.0, 0.0),
                layer: "F.SilkS".to_string(),
                width: 0.12,
            },
            IpcGraphicDefinition::Text {
                text: "hello".to_string(),
                position: (0.0, 2.0),
                rotation: 0.0,
                layer: "F.Fab".to_string(),
                size: 0.5,
            },
        ];
        let fp = build(&graphics, 100.0, 50.0, 90.0);

        let shapes = shapes(&fp);
        assert_eq!(shapes.len(), 2);

        // The rectangle stays axis-aligned at a cardinal rotation: corners
        // (-1,-2) → (98, 51) and (1,2) → (102, 49), re-normalized so the
        // top-left really is top-left.
        let rect = &shapes[0];
        assert_eq!(rect.layer, kiapi::board::types::BoardLayer::BlFCrtYd as i32);
        let geometry = rect.shape.as_ref().unwrap().geometry.as_ref().unwrap();
        let kiapi::common::types::graphic_shape::Geometry::Rectangle(r) = geometry else {
            panic!("expected Rectangle geometry, got {geometry:?}");
        };
        assert_eq!(r.top_left.unwrap().x_nm, 98_000_000);
        assert_eq!(r.top_left.unwrap().y_nm, 49_000_000);
        assert_eq!(r.bottom_right.unwrap().x_nm, 102_000_000);
        assert_eq!(r.bottom_right.unwrap().y_nm, 51_000_000);

        // Silk line (-1,0)→(1,0) rotates to (100,51)→(100,49).
        let line = &shapes[1];
        assert_eq!(line.layer, kiapi::board::types::BoardLayer::BlFSilkS as i32);
        let geometry = line.shape.as_ref().unwrap().geometry.as_ref().unwrap();
        let kiapi::common::types::graphic_shape::Geometry::Segment(s) = geometry else {
            panic!("expected Segment geometry, got {geometry:?}");
        };
        assert_eq!(s.start.unwrap().x_nm, 100_000_000);
        assert_eq!(s.start.unwrap().y_nm, 51_000_000);
        assert_eq!(s.end.unwrap().x_nm, 100_000_000);
        assert_eq!(s.end.unwrap().y_nm, 49_000_000);

        // Fab text at local (0,2) lands at (102, 50) with its angle rotated.
        let texts = texts(&fp);
        assert_eq!(texts.len(), 1);
        let text = texts[0].text.as_ref().unwrap();
        assert_eq!(
            texts[0].layer,
            kiapi::board::types::BoardLayer::BlFFab as i32
        );
        assert_eq!(text.position.unwrap().x_nm, 102_000_000);
        assert_eq!(text.position.unwrap().y_nm, 50_000_000);
        assert_eq!(
            text.attributes
                .as_ref()
                .unwrap()
                .angle
                .unwrap()
                .value_degrees,
            90.0
        );
    }

    /// At 180° the text would read upside down; KiCAD keeps labels readable by
    /// flipping such angles 180°, and so does the emitted child.
    #[test]
    fn text_angles_are_kept_readable() {
        let graphics = vec![IpcGraphicDefinition::Text {
            text: "hello".to_string(),
            position: (0.0, 0.0),
            rotation: 0.0,
            layer: "F.Fab".to_string(),
            size: 0.5,
        }];
        let fp = build(&graphics, 0.0, 0.0, 180.0);
        let texts = texts(&fp);
        assert_eq!(
            texts[0]
                .text
                .as_ref()
                .unwrap()
                .attributes
                .as_ref()
                .unwrap()
                .angle
                .unwrap()
                .value_degrees,
            0.0
        );
    }

    /// The Rectangle protobuf message is axis-aligned by construction, so a
    /// non-cardinal rotation must degrade the rect to its four rotated corners
    /// as a polygon — mirroring EDA_SHAPE::Rotate.
    #[test]
    fn non_cardinal_rotation_emits_rect_as_polygon() {
        let graphics = vec![IpcGraphicDefinition::Rect {
            start: (-1.0, -1.0),
            end: (1.0, 1.0),
            layer: "F.CrtYd".to_string(),
            width: 0.05,
            filled: false,
        }];
        let fp = build(&graphics, 0.0, 0.0, 45.0);
        let shapes = shapes(&fp);
        assert_eq!(shapes.len(), 1);
        let geometry = shapes[0].shape.as_ref().unwrap().geometry.as_ref().unwrap();
        let kiapi::common::types::graphic_shape::Geometry::Polygon(poly) = geometry else {
            panic!("expected Polygon geometry for a 45-degree rect, got {geometry:?}");
        };
        let outline = poly.polygons[0].outline.as_ref().unwrap();
        assert!(outline.closed);
        assert_eq!(outline.nodes.len(), 4);
        // Corner (-1,-1) at 45°: bx = -cos45 - sin45 = -√2, by = sin45 - cos45 = 0.
        let first = match outline.nodes[0].geometry.as_ref().unwrap() {
            kiapi::common::types::poly_line_node::Geometry::Point(p) => p,
            other => panic!("expected Point node, got {other:?}"),
        };
        let sqrt2_nm = (std::f64::consts::SQRT_2 * 1_000_000.0) as i64;
        assert!((first.x_nm + sqrt2_nm).abs() < 10, "x={}", first.x_nm);
        assert!(first.y_nm.abs() < 10, "y={}", first.y_nm);
    }

    /// A circle's radius survives rotation via the circumference-point
    /// encoding.
    #[test]
    fn circle_center_rotates_and_radius_is_preserved() {
        let graphics = vec![IpcGraphicDefinition::Circle {
            center: (1.0, 0.0),
            end: (1.5, 0.0),
            layer: "F.Fab".to_string(),
            width: 0.1,
            filled: true,
        }];
        let fp = build(&graphics, 10.0, 10.0, 90.0);
        let shapes = shapes(&fp);
        let geometry = shapes[0].shape.as_ref().unwrap().geometry.as_ref().unwrap();
        let kiapi::common::types::graphic_shape::Geometry::Circle(c) = geometry else {
            panic!("expected Circle geometry, got {geometry:?}");
        };
        // Center (1,0) at 90° around (10,10): (10, 9).
        assert_eq!(c.center.unwrap().x_nm, 10_000_000);
        assert_eq!(c.center.unwrap().y_nm, 9_000_000);
        // Radius 0.5 mm regardless of rotation.
        assert_eq!(
            c.radius_point.unwrap().x_nm - c.center.unwrap().x_nm,
            500_000
        );
    }

    /// #117 guard for the pad path: the same PST_NORMAL rule that broke
    /// `add_via` applies to every pad we build, including the back-side case
    /// that B.Cu placement (#115) will eventually exercise.
    #[test]
    fn pad_stacks_are_unpackable_on_both_sides() {
        use crate::builders::tests::assert_normal_padstack_is_unpackable;

        let pad = |layer: &str| IpcPadDefinition {
            number: "1".to_string(),
            pad_type: "smd".to_string(),
            shape: "rect".to_string(),
            x: 0.0,
            y: 0.0,
            size_x: 1.0,
            size_y: 1.0,
            layers: vec![layer.to_string()],
            drill_x: None,
            drill_y: None,
            drill_oval: false,
            roundrect_ratio: 0.0,
            rotation: 0.0,
        };

        for layer in ["F.Cu", "B.Cu"] {
            let client = KiCadIpcClient::new("tcp://never-dialed");
            let any = client
                .build_footprint_item(
                    "Lib:Fp",
                    "R1",
                    "R",
                    &[pad(layer)],
                    &[],
                    &crate::types::IpcFieldPlacement::default(),
                    10.0,
                    10.0,
                    0.0,
                    "F.Cu",
                )
                .unwrap();
            let fp = kiapi::board::types::FootprintInstance::decode(any.value.as_slice()).unwrap();
            let pad_any = fp
                .definition
                .as_ref()
                .unwrap()
                .items
                .iter()
                .find(|any| any.type_url.ends_with("types.Pad"))
                .expect("pad item");
            let decoded = kiapi::board::types::Pad::decode(pad_any.value.as_slice()).unwrap();
            assert_normal_padstack_is_unpackable(
                decoded.pad_stack.as_ref().expect("pad_stack"),
                &format!("build_footprint_item pad on {layer}"),
            );
        }
    }

    /// A `Dwgs.User` graphic is ordinary, official-library content and must
    /// place — this is the exact shape that terminated KiCAD 10.0.5 in #237,
    /// via `Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal`
    /// (two such children) and
    /// `Connector:BJB_Pico_46.110.1001_Receptacle_Horizontal` (eight).
    #[test]
    fn user_layer_graphics_reach_kicad_on_their_real_layer() {
        let graphics = vec![
            IpcGraphicDefinition::Line {
                start: (0.0, 0.0),
                end: (1.0, 0.0),
                layer: "Dwgs.User".to_string(),
                width: 0.1,
            },
            IpcGraphicDefinition::Text {
                text: "PCB Edge".to_string(),
                position: (0.0, 3.1),
                rotation: 0.0,
                layer: "Dwgs.User".to_string(),
                size: 1.0,
            },
        ];
        let client = KiCadIpcClient::new("tcp://never-dialed");
        let any = client
            .build_footprint_item(
                "Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal",
                "J1",
                "USB_C",
                &[],
                &graphics,
                &crate::types::IpcFieldPlacement::default(),
                100.0,
                100.0,
                0.0,
                "F.Cu",
            )
            .expect("a Dwgs.User graphic must place");

        let fp = kiapi::board::types::FootprintInstance::decode(any.value.as_slice()).unwrap();
        let undefined = kiapi::board::types::BoardLayer::BlUndefined as i32;
        let drawings = kiapi::board::types::BoardLayer::BlDwgsUser as i32;
        let mut seen = 0;
        for item in &fp.definition.as_ref().unwrap().items {
            let layer = if item.type_url.ends_with("BoardGraphicShape") {
                kiapi::board::types::BoardGraphicShape::decode(item.value.as_slice())
                    .unwrap()
                    .layer
            } else if item.type_url.ends_with("BoardText") {
                kiapi::board::types::BoardText::decode(item.value.as_slice())
                    .unwrap()
                    .layer
            } else {
                continue;
            };
            if layer == drawings {
                seen += 1;
            }
            assert_ne!(
                layer, undefined,
                "no child may carry BL_UNDEFINED — KiCAD crashes on it, it does not refuse it"
            );
        }
        assert_eq!(seen, 2, "both Dwgs.User children must survive");
    }

    /// And a layer this build genuinely cannot represent is refused before any
    /// item is constructed, rather than silently downgraded to `BL_UNDEFINED`.
    ///
    /// Widening `layer_from_name` fixed the layers KiCAD 10 has today; this is
    /// what stops the next one KiCAD adds from crashing it again.
    #[test]
    fn an_unmappable_layer_refuses_the_whole_footprint() {
        let client = KiCadIpcClient::new("tcp://never-dialed");
        let build = |pads: &[IpcPadDefinition], graphics: &[IpcGraphicDefinition], layer: &str| {
            client.build_footprint_item(
                "Lib:Fp",
                "R1",
                "R",
                pads,
                graphics,
                &crate::types::IpcFieldPlacement::default(),
                0.0,
                0.0,
                0.0,
                layer,
            )
        };

        let graphic = vec![IpcGraphicDefinition::Line {
            start: (0.0, 0.0),
            end: (1.0, 1.0),
            layer: "In99.Cu".to_string(),
            width: 0.1,
        }];
        let error = format!(
            "{:#}",
            build(&[], &graphic, "F.Cu").expect_err("graphic layer")
        );
        assert!(error.contains("In99.Cu"), "{error}");
        assert!(error.contains("fp_line"), "{error}");

        let pad = vec![IpcPadDefinition {
            number: "1".to_string(),
            pad_type: "smd".to_string(),
            shape: "rect".to_string(),
            x: 0.0,
            y: 0.0,
            size_x: 1.0,
            size_y: 1.0,
            layers: vec!["Nope.Cu".to_string()],
            drill_x: None,
            drill_y: None,
            drill_oval: false,
            roundrect_ratio: 0.0,
            rotation: 0.0,
        }];
        let error = format!("{:#}", build(&pad, &[], "F.Cu").expect_err("pad layer"));
        assert!(error.contains("Nope.Cu"), "{error}");

        let error = format!(
            "{:#}",
            build(&[], &[], "Middle.Cu").expect_err("root layer")
        );
        assert!(error.contains("Middle.Cu"), "{error}");

        // The wildcards KiCAD itself writes are expanded, not mapped, so they
        // must keep working.
        let through_hole = vec![IpcPadDefinition {
            layers: vec!["*.Cu".to_string(), "*.Mask".to_string()],
            ..pad[0].clone()
        }];
        assert!(build(&through_hole, &[], "F.Cu").is_ok());
    }
}

#[cfg(test)]
mod footprint_graphic_tests {
    use super::*;
    use crate::builders;
    use crate::transform::Xform;

    /// A footprint at (100, 100) rotated `angle` degrees, carrying `items`.
    fn instance(
        angle: f64,
        items: Vec<prost_types::Any>,
    ) -> kiapi::board::types::FootprintInstance {
        kiapi::board::types::FootprintInstance {
            position: Some(builders::vec2(100.0, 100.0)),
            orientation: Some(kiapi::common::types::Angle {
                value_degrees: angle,
            }),
            definition: Some(kiapi::board::types::Footprint {
                items,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn frame_reports_anchor_and_orientation() {
        let ((x, y), angle) = footprint_frame(&instance(90.0, vec![]));
        assert_eq!(
            (x, y),
            (builders::mm_to_nm(100.0), builders::mm_to_nm(100.0))
        );
        assert_eq!(angle, 90.0);
    }

    #[test]
    fn frame_defaults_to_the_origin_unrotated() {
        let fp = kiapi::board::types::FootprintInstance::default();
        assert_eq!(footprint_frame(&fp), ((0, 0), 0.0));
    }

    #[test]
    fn geometry_kind_names_each_shape() {
        use kiapi::common::types::graphic_shape::Geometry;
        let poly = builders::board_polygon("F.SilkS", 0.1, true, &[vec![(0.0, 0.0)]]);
        let seg = builders::board_segment("F.SilkS", 0.1, 0.0, 0.0, 1.0, 0.0);
        let of = |s: &kiapi::board::types::BoardGraphicShape| -> &'static str {
            geometry_kind(s.shape.as_ref().unwrap().geometry.as_ref().unwrap())
        };
        assert_eq!(of(&poly), "polygon");
        assert_eq!(of(&seg), "segment");
        // The remaining names are not decoration: `replace_polygon_outline`
        // interpolates this string into the refusal a caller reads when they
        // aim a vertex list at the wrong graphic. A wrong name there sends
        // someone looking for a shape that isn't the one they picked.
        assert_eq!(
            of(&builders::board_rectangle(
                "F.SilkS", 0.1, 0.0, 0.0, 1.0, 1.0, false
            )),
            "rectangle"
        );
        assert_eq!(
            of(&builders::board_circle(
                "F.SilkS", 0.1, 0.0, 0.0, 1.0, false
            )),
            "circle"
        );
        assert_eq!(
            of(&builders::board_arc(
                "F.SilkS", 0.1, 0.0, 0.0, 1.0, 1.0, 2.0, 0.0
            )),
            "arc"
        );
        // Exhaustiveness is enforced by the match, not by listing every arm here.
        let _ = |g: &Geometry| geometry_kind(g);
    }

    #[test]
    fn geometry_points_reads_a_polygon_outline_in_order() {
        let poly = builders::board_polygon(
            "F.SilkS",
            0.1,
            true,
            &[vec![(13.9, -0.5), (13.9, 0.5), (13.0, 0.0)]],
        );
        let pts = geometry_points(poly.shape.as_ref().unwrap().geometry.as_ref().unwrap());
        assert_eq!(
            pts,
            vec![
                (builders::mm_to_nm(13.9), builders::mm_to_nm(-0.5)),
                (builders::mm_to_nm(13.9), builders::mm_to_nm(0.5)),
                (builders::mm_to_nm(13.0), builders::mm_to_nm(0.0)),
            ]
        );
    }

    #[test]
    fn geometry_points_reads_a_segment_as_start_then_end() {
        let seg = builders::board_segment("F.SilkS", 0.1, -19.0, 0.0, -17.0, 0.0);
        let pts = geometry_points(seg.shape.as_ref().unwrap().geometry.as_ref().unwrap());
        assert_eq!(
            pts,
            vec![
                (builders::mm_to_nm(-19.0), builders::mm_to_nm(0.0)),
                (builders::mm_to_nm(-17.0), builders::mm_to_nm(0.0)),
            ]
        );
    }

    /// A vertex KiCad did not populate is dropped, not reported as the origin.
    /// (0, 0) is a plausible-looking coordinate, so substituting it turns a
    /// malformed shape into a well-formed one in the wrong place.
    #[test]
    fn geometry_points_omits_a_missing_vertex() {
        let mut seg = builders::board_segment("F.SilkS", 0.1, -19.0, 0.0, -17.0, 0.0);
        match seg
            .shape
            .as_mut()
            .and_then(|s| s.geometry.as_mut())
            .unwrap()
        {
            kiapi::common::types::graphic_shape::Geometry::Segment(s) => s.end = None,
            other => panic!("expected a segment, got {other:?}"),
        }

        let pts = geometry_points(seg.shape.as_ref().unwrap().geometry.as_ref().unwrap());
        assert_eq!(
            pts,
            vec![(builders::mm_to_nm(-19.0), builders::mm_to_nm(0.0))],
            "the absent end is missing from the list, not reported as (0, 0)"
        );
    }

    /// A polygon carrying a cutout or a second outline must be countable, since
    /// both the read and the write path only handle the first outline.
    #[test]
    fn polygon_extent_counts_outlines_and_holes() {
        let simple = builders::board_polygon(
            "F.SilkS",
            0.1,
            true,
            &[vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]],
        );
        let geom = |s: &kiapi::board::types::BoardGraphicShape| {
            s.shape.as_ref().unwrap().geometry.as_ref().unwrap().clone()
        };
        assert_eq!(polygon_extent(&geom(&simple)), (1, 0));

        let two = builders::board_polygon(
            "F.SilkS",
            0.1,
            true,
            &[
                vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)],
                vec![(5.0, 5.0), (6.0, 5.0), (6.0, 6.0)],
            ],
        );
        assert_eq!(polygon_extent(&geom(&two)), (2, 0));

        // A hole on the first outline — what board_polygon never builds.
        let mut holed = simple.clone();
        if let kiapi::common::types::graphic_shape::Geometry::Polygon(ps) = holed
            .shape
            .as_mut()
            .and_then(|s| s.geometry.as_mut())
            .unwrap()
        {
            ps.polygons[0].holes.push(kiapi::common::types::PolyLine {
                nodes: vec![],
                closed: true,
            });
        }
        assert_eq!(polygon_extent(&geom(&holed)), (1, 1));

        // Every other kind reports nothing rather than a misleading 1.
        let seg = builders::board_segment("F.SilkS", 0.1, 0.0, 0.0, 1.0, 0.0);
        assert_eq!(polygon_extent(&geom(&seg)), (0, 0));
    }

    /// The arithmetic both new methods stand on: a footprint-local point
    /// pushed into board space and read back must land where it started, at a
    /// rotation where getting the sign wrong is visible.
    #[test]
    fn local_to_board_and_back_round_trips_under_rotation() {
        let fp = instance(90.0, vec![]);
        let (anchor, angle) = footprint_frame(&fp);

        let to_board = Xform::Rotate {
            cx_nm: 0,
            cy_nm: 0,
            delta_deg: angle,
        };
        let to_local = Xform::Rotate {
            cx_nm: anchor.0,
            cy_nm: anchor.1,
            delta_deg: -angle,
        };

        for &(x_mm, y_mm) in &[(13.5, -0.5), (13.5, 0.5), (12.7, 0.0), (-19.0, 3.25)] {
            let (rx, ry) = to_board.point(builders::mm_to_nm(x_mm), builders::mm_to_nm(y_mm));
            let (bx, by) = (anchor.0 + rx, anchor.1 + ry);
            let (lx, ly) = to_local.point(bx, by);
            assert_eq!(
                (
                    builders::nm_to_mm(lx - anchor.0),
                    builders::nm_to_mm(ly - anchor.1)
                ),
                (x_mm, y_mm),
                "round trip lost ({x_mm}, {y_mm})"
            );
        }
    }
    // ─── replace_polygon_outline: every refusal, and the rebuild ───────────
    //
    // These guards sit between a caller's request and an `UpdateItems` that
    // KiCad applies to a live board. Until this module they were unreachable
    // without a running KiCad, so nothing checked that a refused request
    // leaves the shape alone — which is what a test has to prove, not just
    // that an `Err` comes back.

    fn no_rotation() -> Xform {
        Xform::Rotate {
            cx_nm: 0,
            cy_nm: 0,
            delta_deg: 0.0,
        }
    }

    fn square() -> Vec<(f64, f64)> {
        vec![(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)]
    }

    /// The outline vertices of a polygon shape, in board nanometres.
    fn outline_of(shape: &kiapi::board::types::BoardGraphicShape) -> Vec<(i64, i64)> {
        geometry_points(shape.shape.as_ref().unwrap().geometry.as_ref().unwrap())
    }

    #[test]
    fn a_non_polygon_is_refused_by_name_and_left_alone() {
        let mut seg = builders::board_segment("F.SilkS", 0.1, 0.0, 0.0, 1.0, 0.0);
        let before = seg.clone();

        let err = replace_polygon_outline(&mut seg, "J2", "u-1", &square(), (0, 0), &no_rotation())
            .expect_err("a segment must not take a vertex list");

        let message = format!("{err:#}");
        assert!(
            message.contains("is a segment"),
            "the refusal must name the kind it found, so the caller knows why: {message}"
        );
        assert_eq!(
            seg, before,
            "a refused edit must not mutate the shape — the caller still sends it"
        );
    }

    #[test]
    fn fewer_than_three_points_is_refused_and_leaves_the_outline_intact() {
        let mut poly = builders::board_polygon("F.SilkS", 0.1, true, &[square()]);
        let before = outline_of(&poly);

        let err = replace_polygon_outline(
            &mut poly,
            "J2",
            "u-1",
            &[(0.0, 0.0), (1.0, 0.0)],
            (0, 0),
            &no_rotation(),
        )
        .expect_err("two points cannot be a polygon");

        assert!(format!("{err:#}").contains("at least 3 points"), "{err:#}");
        assert_eq!(
            outline_of(&poly),
            before,
            "the original outline must survive a refused edit"
        );
    }

    #[test]
    fn a_second_outline_is_refused_rather_than_discarded() {
        let mut poly = builders::board_polygon(
            "F.SilkS",
            0.1,
            true,
            &[square(), vec![(5.0, 5.0), (6.0, 5.0), (6.0, 6.0)]],
        );
        let before = poly.clone();

        let err =
            replace_polygon_outline(&mut poly, "J2", "u-1", &square(), (0, 0), &no_rotation())
                .expect_err("two outlines cannot be rebuilt as one");

        let message = format!("{err:#}");
        assert!(message.contains("2 outline(s)"), "{message}");
        assert_eq!(
            poly, before,
            "refusing is the whole point: rebuilding would delete the second outline"
        );
    }

    #[test]
    fn a_hole_is_refused_rather_than_discarded() {
        use kiapi::common::types::graphic_shape::Geometry;
        let mut poly = builders::board_polygon("F.SilkS", 0.1, true, &[square()]);
        // `board_polygon` cannot build a hole, so inject one.
        if let Some(Geometry::Polygon(ps)) = poly.shape.as_mut().and_then(|s| s.geometry.as_mut()) {
            ps.polygons[0].holes.push(kiapi::common::types::PolyLine {
                nodes: vec![],
                closed: true,
            });
        }
        let before = poly.clone();

        let err =
            replace_polygon_outline(&mut poly, "J2", "u-1", &square(), (0, 0), &no_rotation())
                .expect_err("a cutout must not be silently dropped");

        assert!(format!("{err:#}").contains("1 hole(s)"), "{err:#}");
        assert_eq!(poly, before, "the cutout must still be there");
    }

    #[test]
    fn the_rebuilt_outline_is_closed_and_anchored() {
        let mut poly = builders::board_polygon("F.SilkS", 0.1, true, &[vec![(9.0, 9.0)]]);
        let anchor = (builders::mm_to_nm(100.0), builders::mm_to_nm(50.0));

        let kind =
            replace_polygon_outline(&mut poly, "J2", "u-1", &square(), anchor, &no_rotation())
                .expect("a single-outline polygon is editable");
        assert_eq!(kind, "polygon");

        // Footprint-local mm in, absolute board nm out: anchor + local.
        let expected: Vec<(i64, i64)> = square()
            .iter()
            .map(|&(x, y)| {
                (
                    anchor.0 + builders::mm_to_nm(x),
                    anchor.1 + builders::mm_to_nm(y),
                )
            })
            .collect();
        assert_eq!(outline_of(&poly), expected);

        use kiapi::common::types::graphic_shape::Geometry;
        let Some(Geometry::Polygon(ps)) = poly.shape.as_ref().and_then(|s| s.geometry.as_ref())
        else {
            panic!("still a polygon");
        };
        assert_eq!(ps.polygons.len(), 1, "exactly one outline is written");
        assert!(ps.polygons[0].holes.is_empty());
        assert!(
            ps.polygons[0].outline.as_ref().unwrap().closed,
            "an open outline renders as a polyline, not a filled pour"
        );
    }

    #[test]
    fn the_rebuilt_outline_follows_the_footprints_rotation() {
        let mut poly = builders::board_polygon("F.SilkS", 0.1, true, &[vec![(0.0, 0.0)]]);
        let anchor = (builders::mm_to_nm(10.0), builders::mm_to_nm(20.0));
        let rotate = Xform::Rotate {
            cx_nm: 0,
            cy_nm: 0,
            delta_deg: 90.0,
        };

        replace_polygon_outline(&mut poly, "J2", "u-1", &square(), anchor, &rotate)
            .expect("editable");

        // A point 2mm along local +X lands 2mm along board -Y at 90 degrees.
        // Getting this backwards mirrors the artwork, which reads as plausible
        // silkscreen and is exactly what nobody would notice in a diff.
        let got = outline_of(&poly);
        assert_eq!(got[0], anchor, "the first vertex is the anchor itself");
        assert_eq!(
            got[1],
            (anchor.0, anchor.1 - builders::mm_to_nm(2.0)),
            "local +X must rotate to board -Y"
        );
    }
}
