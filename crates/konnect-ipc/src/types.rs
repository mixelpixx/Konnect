use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// KiCad design editor addressed by the typed IPC API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcEditorKind {
    Schematic,
    Pcb,
}

impl IpcEditorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Schematic => "schematic",
            Self::Pcb => "pcb",
        }
    }
}

/// Runtime availability of one semantic editor capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcCapabilityAvailability {
    Available,
    Unsupported,
    Unknown,
}

/// One capability statement and the evidence used to make it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcCapability {
    pub availability: IpcCapabilityAvailability,
    pub evidence_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Capabilities relevant to the Priority 1 semantic navigation surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEditorCapabilities {
    pub observe_documents: IpcCapability,
    pub observe_active_context: IpcCapability,
    pub read_selection: IpcCapability,
    pub mutate_selection: IpcCapability,
    pub activate_document: IpcCapability,
    pub activate_sheet: IpcCapability,
    pub reveal_object: IpcCapability,
    pub center_object: IpcCapability,
    pub fit_view: IpcCapability,
    pub cross_probe: IpcCapability,
}

/// Running KiCad version observed through `GetVersion`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcKiCadVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub full_version: String,
}

/// Project identity carried by a live KiCad `DocumentSpecifier`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcProjectIdentity {
    pub name: String,
    pub path: String,
}

/// Canonical schematic instance identity carried by KiCad IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcSheetInstancePath {
    pub kiids: Vec<String>,
    pub human_readable: String,
}

/// One exact live document identity observed through KiCad IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEditorDocument {
    pub editor: IpcEditorKind,
    pub project: Option<IpcProjectIdentity>,
    /// Exact board path when KiCad provides one. KiCad 10 schematic document
    /// specifiers carry a sheet path rather than a schematic filename, so this
    /// is deliberately null for schematics instead of being inferred from disk.
    pub document_path: Option<String>,
    pub sheet_instance_path: Option<IpcSheetInstancePath>,
}

/// Observation for one editor kind on the configured IPC endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEditorObservation {
    pub editor: IpcEditorKind,
    pub addressable: bool,
    pub documents: Vec<IpcEditorDocument>,
    pub capabilities: IpcEditorCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

/// Result of observing the configured KiCad IPC endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEditorStateObservation {
    pub kicad_version: IpcKiCadVersion,
    pub evidence_source: String,
    pub editors: Vec<IpcEditorObservation>,
    /// KiCad 10 has no stable typed foreground-frame or active-document query.
    /// These fields remain null rather than treating open-document order as
    /// active state.
    pub active_editor: Option<IpcEditorKind>,
    pub active_document: Option<IpcEditorDocument>,
    pub active_sheet_instance: Option<IpcSheetInstancePath>,
    pub limitations: Vec<String>,
}

/// One selected object observed from the exact requested live document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcSelectedObject {
    /// Stable KiCad object identifier (KIID/UUID), never a display reference.
    pub kiid: String,
    /// Semantic object kind derived from the exact protobuf type URL.
    pub object_type: String,
    /// Full protobuf type carried by KiCad, retained as decoding evidence.
    pub protocol_type: String,
}

/// Selection readback bound to one exact live editor/document/sheet context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcSelectionObservation {
    pub project: Option<IpcProjectIdentity>,
    pub document: IpcEditorDocument,
    pub editor: IpcEditorKind,
    pub sheet_instance_path: Option<IpcSheetInstancePath>,
    pub selected_objects: Vec<IpcSelectedObject>,
    pub evidence_source: String,
}

/// Semantic selection change requested from one exact editor context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcSelectionMutation {
    Clear,
    Add,
    Remove,
}

impl IpcSelectionMutation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

/// Verified selection mutation. Success means the post-operation observation
/// exactly matched the requested set transition, not merely that IPC replied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcSelectionMutationResult {
    pub operation: IpcSelectionMutation,
    pub requested_kiids: Vec<String>,
    pub before: IpcSelectionObservation,
    pub after: IpcSelectionObservation,
    pub evidence_source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcSelectionMutationErrorKind {
    InvalidRequest,
    ReadbackMismatch,
}

/// A semantic selection mutation was invalid or its observed result did not
/// prove the requested change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcSelectionMutationError {
    pub kind: IpcSelectionMutationErrorKind,
    pub operation: IpcSelectionMutation,
    pub requested_kiids: Vec<String>,
    pub before_kiids: Vec<String>,
    pub after_kiids: Vec<String>,
    pub reason: String,
}

impl IpcSelectionMutationError {
    pub fn from_error(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

impl std::fmt::Display for IpcSelectionMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cannot verify {} selection mutation: {}",
            self.operation.as_str(),
            self.reason
        )
    }
}

impl std::error::Error for IpcSelectionMutationError {}

/// Stable classification for a fail-closed selection observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcSelectionObservationErrorKind {
    WrongProject,
    WrongDocument,
    WrongSheetInstance,
    AmbiguousDocument,
    StaleEditorState,
    UnsupportedObjectType,
    MalformedSelectedObject,
}

/// A selection could not be attributed to the exact requested live context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcSelectionObservationError {
    pub kind: IpcSelectionObservationErrorKind,
    pub editor: IpcEditorKind,
    pub requested: String,
    pub candidates: Vec<String>,
    pub reason: String,
}

impl IpcSelectionObservationError {
    pub fn from_error(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

impl std::fmt::Display for IpcSelectionObservationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cannot observe {} selection for {}: {}",
            self.editor.as_str(),
            self.requested,
            self.reason
        )
    }
}

impl std::error::Error for IpcSelectionObservationError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpcVector2 {
    pub x: f64,
    pub y: f64,
}

/// One copper-layer shape from a KiCad pad stack.
///
/// `layer` is the native KiCad layer name.  A normal stack may expose one
/// representative copper entry while `IpcPad::layers` still names every layer
/// on which the pad exists. Front/inner/back stacks retain every distinct
/// entry here; custom shapes are identified as `custom` rather than reduced to
/// a guessed ordinary outline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpcPadLayerGeometry {
    pub layer: String,
    pub shape: Option<String>,
    pub size: Option<IpcVector2>,
    pub offset: Option<IpcVector2>,
    pub corner_rounding_ratio: Option<f64>,
    pub chamfer_ratio: Option<f64>,
}

/// Native drill information for a pad, when it has a drilled hole.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpcPadDrill {
    pub shape: Option<String>,
    pub size: Option<IpcVector2>,
    pub start_layer: Option<String>,
    pub end_layer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcFootprint {
    pub reference: String,
    pub value: String,
    pub footprint: String,
    pub position: IpcVector2,
    pub rotation: f64,
    pub layer: String,
}

/// A pad of a footprint placed on the board, read back from KiCad.
///
/// Coordinates are absolute board millimetres: KiCad serializes a
/// `FootprintInstance`'s children in board space (see the `transform` module),
/// so no anchor or rotation transform is applied on the way out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcPad {
    /// Stable KiCad item identifier, or `None` when the source did not carry
    /// one.  Never synthesize an identifier for a pad that KiCad did not name.
    pub uuid: Option<String>,
    pub number: String,
    pub x: f64,
    pub y: f64,
    /// Net name, empty when the pad carries no net.
    pub net: String,
    /// KiCad layer names from the live pad stack.
    pub layers: Vec<String>,
    /// Native KiCad pad type (`smd`, `thru_hole`, `np_thru_hole`, or
    /// `edge_connector`), or `None` for an unknown future type.
    pub pad_type: Option<String>,
    /// Effective pad-stack rotation in board space, in degrees.
    pub rotation_deg: Option<f64>,
    /// Compact geometry for ordinary pads.  These mirror the first copper
    /// entry; callers needing custom/front-inner-back detail use
    /// `copper_layers` instead.
    pub shape: Option<String>,
    pub size: Option<IpcVector2>,
    pub drill: Option<IpcPadDrill>,
    /// Every copper geometry entry KiCad supplied.  This prevents custom and
    /// front/inner/back pad stacks from being flattened into a guessed shape.
    pub copper_layers: Vec<IpcPadLayerGeometry>,
}

/// The document's title block, which the board file also carries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpcTitleBlock {
    pub title: String,
    pub date: String,
    pub revision: String,
    pub company: String,
}

/// Complete target placement for one existing footprint.
///
/// Keeping the four values together lets the IPC client transform all selected
/// footprints from one board snapshot and publish them in one undoable update,
/// instead of issuing a move and a rotation as separate round trips.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpcFootprintPlacement {
    pub reference: String,
    pub x: f64,
    pub y: f64,
    pub rotation: f64,
}

#[derive(Debug, Clone)]
pub struct IpcPadDefinition {
    pub number: String,
    pub pad_type: String,
    pub shape: String,
    pub x: f64,
    pub y: f64,
    pub rotation: f64,
    pub size_x: f64,
    pub size_y: f64,
    pub drill_x: Option<f64>,
    pub drill_y: Option<f64>,
    pub drill_oval: bool,
    pub layers: Vec<String>,
    pub roundrect_ratio: f64,
}

/// A footprint graphic item in footprint-local coordinates (mm), parsed from
/// the library `.kicad_mod` source.
///
/// Points are pre-transform: `build_footprint_item` rotates and translates
/// them into absolute board coordinates before emission, because KiCAD
/// serializes `FootprintInstance` children in absolute board space (see the
/// `transform` module docs / issue #23).
#[derive(Debug, Clone, PartialEq)]
pub enum IpcGraphicDefinition {
    /// `fp_line` — straight segment.
    Line {
        start: (f64, f64),
        end: (f64, f64),
        layer: String,
        width: f64,
    },
    /// `fp_rect` — axis-aligned rectangle between two opposite corners.
    Rect {
        start: (f64, f64),
        end: (f64, f64),
        layer: String,
        width: f64,
        filled: bool,
    },
    /// `fp_circle` — center plus a point on the circumference.
    Circle {
        center: (f64, f64),
        end: (f64, f64),
        layer: String,
        width: f64,
        filled: bool,
    },
    /// `fp_arc` — start / mid / end points.
    Arc {
        start: (f64, f64),
        mid: (f64, f64),
        end: (f64, f64),
        layer: String,
        width: f64,
    },
    /// `fp_poly` — closed outline.
    Poly {
        points: Vec<(f64, f64)>,
        layer: String,
        width: f64,
        filled: bool,
    },
    /// Visible `fp_text` / `property` text.
    Text {
        text: String,
        position: (f64, f64),
        /// Text angle in degrees, footprint-local.
        rotation: f64,
        layer: String,
        /// Glyph size (width and height) in mm.
        size: f64,
        /// Font stroke width in mm.
        stroke_width_mm: f64,
    },
}

impl IpcGraphicDefinition {
    /// The KiCAD layer name this item draws on.
    pub fn layer(&self) -> &str {
        match self {
            Self::Line { layer, .. }
            | Self::Rect { layer, .. }
            | Self::Circle { layer, .. }
            | Self::Arc { layer, .. }
            | Self::Poly { layer, .. }
            | Self::Text { layer, .. } => layer,
        }
    }

    /// What this item is, for an error that has to name it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Line { .. } => "fp_line",
            Self::Rect { .. } => "fp_rect",
            Self::Circle { .. } => "fp_circle",
            Self::Arc { .. } => "fp_arc",
            Self::Poly { .. } => "fp_poly",
            Self::Text { .. } => "fp_text",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcTrack {
    /// KIID of the track, needed to delete it via delete_track. Empty only if
    /// KiCAD returned a track without an id.
    pub uuid: String,
    pub net_name: String,
    pub layer: String,
    pub width: f64,
    pub start: IpcVector2,
    pub end: IpcVector2,
}

/// A graphic item inside a placed footprint — silkscreen, fabrication, or
/// courtyard artwork, not a pad.
///
/// `points` are footprint-local millimetres, matching what the `.kicad_mod`
/// shows, even though KiCad carries them in absolute board coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcFootprintGraphic {
    pub uuid: String,
    pub kind: String,
    pub layer: String,
    pub points: Vec<IpcVector2>,
    /// How many outlines a polygon's `PolySet` carries, and how many holes
    /// across them; `0` for every other kind. `points` reports the first
    /// outline only, so anything above `1` outline or above `0` holes means
    /// this listing is not the whole shape — hence stating it rather than
    /// letting the caller infer a simple triangle from three points.
    pub outlines: usize,
    pub holes: usize,
    /// Whether `edit_board_footprint_graphic` can address this item: a
    /// single-outline polygon with no holes, carrying a UUID.
    pub editable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcNet {
    pub name: String,
    pub netcode: i32,
}

/// A board graphic — a shape, text, textbox, or dimension — read back from
/// KiCad.
///
/// `kind` is normalized (`line`, `rect`, `arc`, `circle`, `poly`, `curve`,
/// `text`, `textbox`, `dimension`) so the live and the file reader answer in
/// one vocabulary rather than protobuf names on one side and `gr_*` file tags
/// on the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcGraphic {
    pub uuid: String,
    pub kind: String,
    pub layer: String,
    /// First defining point in mm: a segment's start, a rectangle's top-left,
    /// an arc's start, a circle's centre, a polygon's first vertex, a text's
    /// position. `None` when KiCad sent no geometry.
    pub origin: Option<IpcVector2>,
}

/// Effective PCB routing rules returned by KiCad for one net.
///
/// Values are optional because KiCad's protobuf permits an incomplete class.
/// A routing exporter must refuse an incomplete rule set rather than inventing
/// manufacturing constraints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpcRoutingRules {
    pub class_name: String,
    pub constituents: Vec<String>,
    pub track_width_mm: Option<f64>,
    pub clearance_mm: Option<f64>,
    pub via_diameter_mm: Option<f64>,
    pub via_drill_mm: Option<f64>,
}

/// Net name to its effective (merged) KiCad routing rules.
pub type IpcEffectiveRoutingRules = BTreeMap<String, IpcRoutingRules>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcLayer {
    pub name: String,
    pub id: i32,
    pub kind: String,
    /// The name KiCad shows for the layer, which the user may have renamed.
    /// `None` when the enabled set was read without asking for it — an empty
    /// string is a name KiCad could have answered with, so absence needs its
    /// own spelling.
    pub display_name: Option<String>,
}

/// The enabled layer set as KiCad reports it.
///
/// `copper_layer_count` is the response's own field, not a count of `layers`
/// whose name ends in `.Cu` — the two agree on an ordinary stackup, and that
/// agreement is exactly what stops holding on an unusual one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEnabledLayers {
    pub copper_layer_count: u32,
    pub layers: Vec<IpcLayer>,
}

/// A board's physical stackup, as KiCad's `GetBoardStackup` serves it.
///
/// KiCad answers with the board's own stackup, or with its default one when
/// the board defines none (`BOARD::GetStackupOrDefault`), and the message does
/// not say which. Values KiCad marks "Not specified" are kept as it sends them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcBoardStackup {
    /// The copper finish KiCad names, e.g. "ENIG"; empty or "Not specified"
    /// when the board names none.
    pub finish: String,
    pub impedance_controlled: bool,
    /// "none", "plain", "beveled" or "unknown".
    pub edge_connector: String,
    pub has_edge_plating: bool,
    /// Top to bottom, in KiCad's order.
    pub layers: Vec<IpcStackupLayer>,
}

/// One entry of a physical stackup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcStackupLayer {
    /// KiCad's canonical layer name, e.g. "F.Cu" or "F.Mask". `None` for a
    /// dielectric, which is not a board layer of its own.
    pub layer: Option<String>,
    /// The name the board gives the layer, when KiCad reports one.
    pub user_name: Option<String>,
    /// "copper", "dielectric", "soldermask", "silkscreen", "solderpaste" or
    /// "undefined".
    pub kind: String,
    pub enabled: bool,
    /// The entry's thickness in nanometres. A dielectric's own value is its
    /// first sub-layer's; `dielectric` carries every sub-layer.
    pub thickness_nm: i64,
    /// Empty when KiCad reports none. A dielectric's materials are per
    /// sub-layer, in `dielectric`.
    pub material: String,
    /// `#RRGGBBAA`, only when the board specifies a color.
    pub color: Option<String>,
    /// "core", "prepreg", "none", or "unknown" for a value this crate does not
    /// know, for a dielectric.
    pub dielectric_type: Option<String>,
    /// A dielectric's physical sub-layers, top to bottom.
    pub dielectric: Vec<IpcStackupDielectric>,
    /// A solder mask's relative permittivity.
    pub epsilon_r: Option<f64>,
    /// A solder mask's loss tangent.
    pub loss_tangent: Option<f64>,
}

/// One physical sub-layer of a dielectric slot. A slot between two copper
/// layers can be built from several.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcStackupDielectric {
    pub thickness_nm: i64,
    pub material: String,
    pub epsilon_r: f64,
    pub loss_tangent: f64,
    pub thickness_locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcBoardExtents {
    pub min: IpcVector2,
    pub max: IpcVector2,
}

/// What [`crate::client::KiCadIpcClient::get_board_bounds_in`] measured on a
/// live board.
#[derive(Debug, Clone)]
pub struct IpcBoardBounds {
    /// The union of every item's box; `None` only for a board with no items.
    pub extents: Option<IpcBoardExtents>,
    /// How many items of each class KiCad listed and measured, by the name a
    /// response reports the class under. Classes with no items are absent.
    pub measured: std::collections::BTreeMap<String, usize>,
    /// Items KiCad listed under a KIID it had already listed. Each KIID is
    /// measured once, so these have no box of their own.
    pub shared_kiid_count: usize,
    /// Board item classes whose items are not in `extents`, because KiCad
    /// would not list them or listed them in a form this protocol cannot read.
    pub unavailable: Vec<IpcUnavailableItemClass>,
}

/// A class of board items a live bounds read could not measure, and why. A
/// class is named here rather than left out, so an answer never reads as
/// complete when it is not (#688).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcUnavailableItemClass {
    /// The class, by the name a response reports it under (`tables`).
    pub class: String,
    pub reason: UnavailableReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnavailableReason {
    /// KiCad declined to list the class. KiCad 10.0.5 answers `GetItems` for
    /// tables and generators with `AS_BAD_REQUEST`.
    Refused { status: String, message: String },
    /// KiCad listed items of a class the bundled protocol has no message for,
    /// so their KIIDs cannot be read to measure them.
    Undecodable { listed_count: usize },
}

/// Footprint-local placement of the Reference and Value text fields, read
/// from the library footprint so placed parts keep the library's text
/// positions. A hardcoded offset put the Reference on top of the part's own
/// silkscreen (silk_overlap DRC warnings in live verification).
#[derive(Debug, Clone, Copy, Default)]
pub struct IpcFieldPlacement {
    /// (x, y, rotation) of the Reference text, footprint-local mm/degrees.
    pub reference_at: Option<(f64, f64, f64)>,
    /// (x, y, rotation) of the Value text, footprint-local mm/degrees.
    pub value_at: Option<(f64, f64, f64)>,
}

/// A three-component value as KiCad's `Vector3D` message carries it.
///
/// The enclosing field names the quantity and unit. KiCad reuses this wire
/// shape for unitless scale, rotation in degrees, and offset in millimetres,
/// so a bare vector must never be exposed without that context.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IpcVector3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// A footprint's associated 3D model, as read back from a placed instance.
/// `flip_footprint` reports these so a caller can independently confirm the
/// model transform native `FlipItems` applied, without Konnect reimplementing
/// KiCad's own 3D-model flip math.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcFootprint3DModel {
    pub filename: String,
    /// Footprint-local model offset in millimetres.
    pub offset_mm: IpcVector3,
    /// Model rotation around each axis in degrees.
    pub rotation_degrees: IpcVector3,
    /// Unitless model scale along each axis.
    pub scale: IpcVector3,
    pub visible: bool,
    pub opacity: f64,
}

/// One exact live footprint and the ordered 3D-model entries KiCad returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcFootprint3DModelSnapshot {
    pub reference: String,
    pub kiid: String,
    pub models: Vec<IpcFootprint3DModel>,
}

/// A bounded edit to the ordered 3D-model entries on one placed footprint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcFootprint3DModelEdit {
    Append {
        model: IpcFootprint3DModel,
    },
    Replace {
        index: usize,
        model: IpcFootprint3DModel,
    },
    Remove {
        index: usize,
    },
}

/// Readback-derived result of editing a placed footprint's 3D-model entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcFootprint3DModelEditOutcome {
    Conflict {
        expected: IpcFootprint3DModelSnapshot,
        observed: IpcFootprint3DModelSnapshot,
    },
    Applied {
        before: IpcFootprint3DModelSnapshot,
        after: IpcFootprint3DModelSnapshot,
        changed: bool,
    },
    Uncertain {
        reference: String,
        kiid: String,
        expected_models: Vec<IpcFootprint3DModel>,
        observed_models: Option<Vec<IpcFootprint3DModel>>,
        reason: String,
    },
}

/// The result of a `flip_footprint` call against a live KiCad IPC session.
///
/// Three outcomes, not two (accepted contract for issue #604): a definite
/// success (`Flipped`), authoritative evidence that this KiCad build has no
/// native `FlipItems` handler (`Unsupported`), and a distinct "reconcile
/// before retrying" state for when the mutation looked like it went through
/// but a fresh readback could not confirm it (`Uncertain`). A hard failure
/// (footprint not found, wrong KIID/type, a non-`ISC_OK` per-item status, or
/// any other rejected request) is not represented here — it is returned as
/// `Err` and classified by the caller's normal IPC-failure handling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcFlipOutcome {
    /// KiCad flipped the footprint (or it already sat on the requested
    /// layer, in which case no `FlipItems` call was made at all) and a fresh
    /// readback — never the mutation response — independently confirmed the
    /// requested layer and the footprint's identity.
    Flipped {
        reference: String,
        kiid: String,
        previous_layer: String,
        layer: String,
        already_on_layer: bool,
        models: Vec<IpcFootprint3DModel>,
    },
    /// KiCad answered `AS_UNHANDLED`/`AS_UNIMPLEMENTED` for `FlipItems`,
    /// which the 10.0.6 protocol treats as authoritative capability
    /// evidence. Never inferred from a version string.
    Unsupported {
        kicad_version: Option<IpcKiCadVersion>,
    },
    /// The mutation appeared to succeed, but a fresh post-flip readback
    /// disagreed with it or could not be resolved. The board may or may not
    /// have changed; the caller must reconcile before retrying rather than
    /// reporting either success or failure.
    Uncertain {
        reference: String,
        kiid: String,
        requested_layer: String,
        reason: String,
    },
}
