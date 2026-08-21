use serde::{Deserialize, Serialize};

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
