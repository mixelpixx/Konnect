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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcVector2 {
    pub x: f64,
    pub y: f64,
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
    pub number: String,
    pub x: f64,
    pub y: f64,
    /// Net name, empty when the pad carries no net.
    pub net: String,
    /// KiCad layer names from the live pad stack.
    pub layers: Vec<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcBoardExtents {
    pub min: IpcVector2,
    pub max: IpcVector2,
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
