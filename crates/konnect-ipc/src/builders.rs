//! Protobuf message builders for KiCAD 10 IPC API.
//!
//! These helpers construct the protobuf messages needed to create, update, and
//! delete PCB items via the IPC API.

use crate::gen::kiapi;

/// Converts millimeters to KiCAD nanometers.
pub fn mm_to_nm(mm: f64) -> i64 {
    (mm * 1_000_000.0) as i64
}

/// Converts KiCAD nanometers to millimeters.
pub fn nm_to_mm(nm: i64) -> f64 {
    nm as f64 / 1_000_000.0
}

/// Build a Vector2 in nanometers from mm coordinates.
pub fn vec2(x_mm: f64, y_mm: f64) -> kiapi::common::types::Vector2 {
    kiapi::common::types::Vector2 {
        x_nm: mm_to_nm(x_mm),
        y_nm: mm_to_nm(y_mm),
    }
}

/// Build a Distance in nanometers from mm.
pub fn distance(mm: f64) -> kiapi::common::types::Distance {
    kiapi::common::types::Distance {
        value_nm: mm_to_nm(mm),
    }
}

/// Build a Net message.
pub fn net(name: &str, code: i32) -> kiapi::board::types::Net {
    kiapi::board::types::Net {
        code: Some(kiapi::board::types::NetCode { value: code }),
        name: name.to_string(),
    }
}

/// Map a layer name string to the BoardLayer enum value, or `BlUndefined` when
/// this build has no representation for it.
///
/// Covers every layer a KiCAD 10 board or footprint can legally draw on. It
/// used to stop at `In2.Cu` and omit the four user layers, the two adhesive
/// layers, `Margin` and `Rescue`, so an ordinary official footprint with a
/// `Dwgs.User` outline serialised as `BL_UNDEFINED` (#237).
///
/// Callers building a message for KiCAD want [`try_layer_from_name`] instead:
/// `BL_UNDEFINED` is not something KiCAD rejects, it is something it crashes
/// on.
pub fn layer_from_name(name: &str) -> kiapi::board::types::BoardLayer {
    use kiapi::board::types::BoardLayer;

    // `In1.Cu`..=`In30.Cu` are contiguous from `BL_F_Cu`, which is why they are
    // computed rather than listed: `BL_F_Cu = 3`, `BL_In1_Cu = 4`, …,
    // `BL_In30_Cu = 33` (board_types.proto).
    if let Some(number) = name
        .strip_prefix("In")
        .and_then(|value| value.strip_suffix(".Cu"))
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|number| (1..=30).contains(number))
    {
        return BoardLayer::try_from(BoardLayer::BlFCu as i32 + number)
            .unwrap_or(BoardLayer::BlUndefined);
    }
    // `User.1`..=`User.45` are contiguous in two runs: `BL_User_1 = 53` …
    // `BL_User_9 = 61`, then `BL_Rescue = 62` interrupts, and `BL_User_10 = 63`
    // … `BL_User_45 = 98`. Stepping over that hole is the whole reason for the
    // split.
    if let Some(number) = name
        .strip_prefix("User.")
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|number| (1..=45).contains(number))
    {
        let value = if number <= 9 {
            BoardLayer::BlUser1 as i32 + number - 1
        } else {
            BoardLayer::BlUser10 as i32 + number - 10
        };
        return BoardLayer::try_from(value).unwrap_or(BoardLayer::BlUndefined);
    }

    match name {
        "F.Cu" => BoardLayer::BlFCu,
        "B.Cu" => BoardLayer::BlBCu,
        "F.Adhes" | "F.Adhesive" => BoardLayer::BlFAdhes,
        "B.Adhes" | "B.Adhesive" => BoardLayer::BlBAdhes,
        "F.SilkS" | "F.Silkscreen" => BoardLayer::BlFSilkS,
        "B.SilkS" | "B.Silkscreen" => BoardLayer::BlBSilkS,
        "F.Mask" => BoardLayer::BlFMask,
        "B.Mask" => BoardLayer::BlBMask,
        "F.Paste" => BoardLayer::BlFPaste,
        "B.Paste" => BoardLayer::BlBPaste,
        "F.CrtYd" | "F.Courtyard" => BoardLayer::BlFCrtYd,
        "B.CrtYd" | "B.Courtyard" => BoardLayer::BlBCrtYd,
        "F.Fab" => BoardLayer::BlFFab,
        "B.Fab" => BoardLayer::BlBFab,
        "Dwgs.User" | "User.Drawings" => BoardLayer::BlDwgsUser,
        "Cmts.User" | "User.Comments" => BoardLayer::BlCmtsUser,
        "Eco1.User" | "User.Eco1" => BoardLayer::BlEco1User,
        "Eco2.User" | "User.Eco2" => BoardLayer::BlEco2User,
        "Edge.Cuts" => BoardLayer::BlEdgeCuts,
        "Margin" => BoardLayer::BlMargin,
        "Rescue" => BoardLayer::BlRescue,
        _ => BoardLayer::BlUndefined,
    }
}

/// The canonical KiCAD name for a board layer — the exact inverse of
/// [`layer_from_name`] over every layer it can represent.
///
/// `None` for a value with no name (including `BL_UNDEFINED`); callers
/// rendering a response decide how to say "unknown" rather than this map
/// quietly inventing one. Kept next to the forward map because #237's fix
/// widened only the forward direction, and the narrow reverse map surfaced
/// as 28 `"Unknown"` strings in every through-hole pad's `layers` list.
pub fn layer_name(layer: kiapi::board::types::BoardLayer) -> Option<&'static str> {
    use kiapi::board::types::BoardLayer;

    const IN_CU: [&str; 30] = [
        "In1.Cu", "In2.Cu", "In3.Cu", "In4.Cu", "In5.Cu", "In6.Cu", "In7.Cu", "In8.Cu", "In9.Cu",
        "In10.Cu", "In11.Cu", "In12.Cu", "In13.Cu", "In14.Cu", "In15.Cu", "In16.Cu", "In17.Cu",
        "In18.Cu", "In19.Cu", "In20.Cu", "In21.Cu", "In22.Cu", "In23.Cu", "In24.Cu", "In25.Cu",
        "In26.Cu", "In27.Cu", "In28.Cu", "In29.Cu", "In30.Cu",
    ];
    const USER: [&str; 45] = [
        "User.1", "User.2", "User.3", "User.4", "User.5", "User.6", "User.7", "User.8", "User.9",
        "User.10", "User.11", "User.12", "User.13", "User.14", "User.15", "User.16", "User.17",
        "User.18", "User.19", "User.20", "User.21", "User.22", "User.23", "User.24", "User.25",
        "User.26", "User.27", "User.28", "User.29", "User.30", "User.31", "User.32", "User.33",
        "User.34", "User.35", "User.36", "User.37", "User.38", "User.39", "User.40", "User.41",
        "User.42", "User.43", "User.44", "User.45",
    ];

    let value = layer as i32;
    // The same contiguous runs the forward map computes over: In1..=In30
    // directly after BL_F_Cu, and User.1..=User.45 split around BL_Rescue.
    if value > BoardLayer::BlFCu as i32 && value <= BoardLayer::BlFCu as i32 + 30 {
        return Some(IN_CU[(value - BoardLayer::BlFCu as i32 - 1) as usize]);
    }
    if (BoardLayer::BlUser1 as i32..=BoardLayer::BlUser9 as i32).contains(&value) {
        return Some(USER[(value - BoardLayer::BlUser1 as i32) as usize]);
    }
    if (BoardLayer::BlUser10 as i32..=BoardLayer::BlUser10 as i32 + 35).contains(&value) {
        return Some(USER[(value - BoardLayer::BlUser10 as i32 + 9) as usize]);
    }

    match layer {
        BoardLayer::BlFCu => Some("F.Cu"),
        BoardLayer::BlBCu => Some("B.Cu"),
        BoardLayer::BlFAdhes => Some("F.Adhes"),
        BoardLayer::BlBAdhes => Some("B.Adhes"),
        BoardLayer::BlFSilkS => Some("F.SilkS"),
        BoardLayer::BlBSilkS => Some("B.SilkS"),
        BoardLayer::BlFMask => Some("F.Mask"),
        BoardLayer::BlBMask => Some("B.Mask"),
        BoardLayer::BlFPaste => Some("F.Paste"),
        BoardLayer::BlBPaste => Some("B.Paste"),
        BoardLayer::BlFCrtYd => Some("F.CrtYd"),
        BoardLayer::BlBCrtYd => Some("B.CrtYd"),
        BoardLayer::BlFFab => Some("F.Fab"),
        BoardLayer::BlBFab => Some("B.Fab"),
        BoardLayer::BlDwgsUser => Some("Dwgs.User"),
        BoardLayer::BlCmtsUser => Some("Cmts.User"),
        BoardLayer::BlEco1User => Some("Eco1.User"),
        BoardLayer::BlEco2User => Some("Eco2.User"),
        BoardLayer::BlEdgeCuts => Some("Edge.Cuts"),
        BoardLayer::BlMargin => Some("Margin"),
        BoardLayer::BlRescue => Some("Rescue"),
        _ => None,
    }
}

/// As [`layer_from_name`], but refusing a name this build cannot represent.
///
/// Use this on every path that puts a layer into a message bound for KiCAD.
/// KiCAD 10.0.5 does not validate a scalar layer field: it indexes its layer
/// bitset with whatever arrives, so `BL_UNDEFINED` is not received as an error
/// but as an access violation that terminates the process and discards the
/// user's unsaved board (#237). An unrepresentable layer has to stop here,
/// where it can still be reported, rather than downstream where it cannot.
pub fn try_layer_from_name(name: &str) -> anyhow::Result<kiapi::board::types::BoardLayer> {
    match layer_from_name(name) {
        kiapi::board::types::BoardLayer::BlUndefined => anyhow::bail!(
            "layer '{name}' has no KiCAD board layer this build can represent, so the \
             request was not sent"
        ),
        layer => Ok(layer),
    }
}

/// Build a Track protobuf message.
#[allow(clippy::too_many_arguments)]
pub fn build_track(
    net_name: &str,
    net_code: i32,
    layer: &str,
    width_mm: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
) -> kiapi::board::types::Track {
    kiapi::board::types::Track {
        id: None, // KiCAD assigns the ID
        start: Some(vec2(x1, y1)),
        end: Some(vec2(x2, y2)),
        width: Some(distance(width_mm)),
        locked: kiapi::common::types::LockedState::LsUnlocked as i32,
        layer: layer_from_name(layer) as i32,
        net: Some(net(net_name, net_code)),
        parent: None,
    }
}

/// Build an arc-shaped copper track from KiCad's native start/mid/end form.
#[allow(clippy::too_many_arguments)]
pub fn build_track_arc(
    net_name: &str,
    net_code: i32,
    layer: &str,
    width_mm: f64,
    start_x: f64,
    start_y: f64,
    mid_x: f64,
    mid_y: f64,
    end_x: f64,
    end_y: f64,
) -> kiapi::board::types::Arc {
    kiapi::board::types::Arc {
        id: None,
        start: Some(vec2(start_x, start_y)),
        mid: Some(vec2(mid_x, mid_y)),
        end: Some(vec2(end_x, end_y)),
        width: Some(distance(width_mm)),
        locked: kiapi::common::types::LockedState::LsUnlocked as i32,
        layer: layer_from_name(layer) as i32,
        net: Some(net(net_name, net_code)),
        parent: None,
    }
}

/// Build a through-via `Via` protobuf message (F.Cu → B.Cu).
///
/// Mirrors [`build_track`]: the caller `pack_any`s the result and hands it to
/// `create_items`. The earlier implementation built a bare `(via …)`
/// S-expression string and fed it to `ParseAndCreateItemsFromString`; that
/// paste path silently created nothing (the command returns a
/// `CreateItemsResponse` whose overall status is `IRS_OK` even when zero items
/// are created), so `add_via` reported success while no via ever appeared.
/// Building the protobuf and going through `create_items` is the same path that
/// `add_track` (and the reference `kipy` client) use, and it actually persists.
pub fn build_via(
    net_name: &str,
    net_code: i32,
    x: f64,
    y: f64,
    drill_mm: f64,
    size_mm: f64,
) -> kiapi::board::types::Via {
    use kiapi::board::types::{
        BoardLayer, DrillProperties, DrillShape, PadStack, PadStackLayer, PadStackShape,
        PadStackType, ViaType,
    };

    // A PST_NORMAL padstack carries exactly ONE copper-layer entry, keyed to
    // KiCad's ALL_LAYERS sentinel (== F_Cu). PADSTACK::unpackCopperLayer
    // rejects any other layer while the mode is NORMAL, which fails the whole
    // PCB_VIA deserialization ("could not unpack PCB_VIA", AS_BAD_REQUEST) —
    // sending an F.Cu + B.Cu pair here is what broke add_via in v0.2.1 (#117).
    // The through span is defined by the drill's start/end layers, not by the
    // copper entries.
    let copper_pad = PadStackLayer {
        layer: BoardLayer::BlFCu as i32,
        shape: PadStackShape::PssCircle as i32,
        size: Some(vec2(size_mm, size_mm)),
        ..PadStackLayer::default()
    };

    let pad_stack = PadStack {
        r#type: PadStackType::PstNormal as i32,
        layers: vec![BoardLayer::BlFCu as i32, BoardLayer::BlBCu as i32],
        drill: Some(DrillProperties {
            start_layer: BoardLayer::BlFCu as i32,
            end_layer: BoardLayer::BlBCu as i32,
            diameter: Some(vec2(drill_mm, drill_mm)),
            shape: DrillShape::DsCircle as i32,
            ..DrillProperties::default()
        }),
        copper_layers: vec![copper_pad],
        ..PadStack::default()
    };

    kiapi::board::types::Via {
        id: None, // KiCAD assigns the ID
        position: Some(vec2(x, y)),
        pad_stack: Some(pad_stack),
        locked: kiapi::common::types::LockedState::LsUnlocked as i32,
        net: Some(net(net_name, net_code)),
        r#type: ViaType::VtThrough as i32,
        parent: None,
    }
}

/// KiCad's default thermal-relief geometry. Kept as a constant because the
/// S-expression fallback in `add_zone` writes the same numbers as
/// `(thermal_gap …)` / `(thermal_bridge_width …)` — the two paths must produce
/// the same zone or a board grown half over IPC and half over the file would
/// fill differently.
pub const ZONE_THERMAL_RELIEF_MM: f64 = 0.5;

/// `(hatch edge 0.508)` — the border hatch pcbnew gives a freshly drawn zone.
pub const ZONE_BORDER_PITCH_MM: f64 = 0.508;

/// Everything a caller may choose about a copper pour. The net *code* is not
/// in here: it is resolved from `net_name` against the live board by
/// [`crate::client::KiCadIpcClient::add_zone`], which keeps this a pure
/// builder that unit tests can drive without a socket.
pub struct ZoneSpec<'a> {
    /// Copper layer name, e.g. `"F.Cu"`.
    pub layer: &'a str,
    /// Net the pour is bound to.
    pub net_name: &'a str,
    /// Outline vertices in mm. The loop is closed for you.
    pub points: &'a [(f64, f64)],
    pub clearance_mm: f64,
    pub min_thickness_mm: f64,
    /// Zone name as shown in pcbnew's zone properties; empty leaves it unnamed.
    pub name: &'a str,
    /// Higher priority wins where two zones overlap. KiCad's default is 0.
    pub priority: u32,
    /// How the pour attaches to pads on its net.
    pub connection: kiapi::board::types::ZoneConnectionStyle,
}

/// Build a copper `Zone` protobuf message for `create_items`.
///
/// The zone is deliberately sent **unfilled** (`filled: false`, no
/// `filled_polygons`): the fill polygons are KiCad's to compute from the
/// outline and the design rules, so [`crate::client::KiCadIpcClient::add_zone`]
/// follows the create with a `RefillZones`. Sending our own polygons would
/// hand KiCad a fill that ignores every clearance it knows about.
pub fn build_zone(spec: &ZoneSpec<'_>, net_code: i32) -> kiapi::board::types::Zone {
    use kiapi::board::types::{
        CopperZoneSettings, IslandRemovalMode, TeardropSettings, TeardropType,
        ThermalSpokeSettings, ZoneBorderSettings, ZoneBorderStyle, ZoneConnectionSettings,
        ZoneFillMode, ZoneType,
    };

    let outline = kiapi::common::types::PolySet {
        polygons: vec![kiapi::common::types::PolygonWithHoles {
            outline: Some(kiapi::common::types::PolyLine {
                nodes: spec
                    .points
                    .iter()
                    .map(|&(x, y)| kiapi::common::types::PolyLineNode {
                        geometry: Some(kiapi::common::types::poly_line_node::Geometry::Point(
                            vec2(x, y),
                        )),
                    })
                    .collect(),
                closed: true,
            }),
            holes: vec![],
        }],
    };

    let copper = CopperZoneSettings {
        connection: Some(ZoneConnectionSettings {
            zone_connection: spec.connection as i32,
            thermal_spokes: Some(ThermalSpokeSettings {
                width: Some(distance(ZONE_THERMAL_RELIEF_MM)),
                gap: Some(distance(ZONE_THERMAL_RELIEF_MM)),
                angle: Some(kiapi::common::types::Angle {
                    value_degrees: 45.0,
                }),
            }),
        }),
        clearance: Some(distance(spec.clearance_mm)),
        min_thickness: Some(distance(spec.min_thickness_mm)),
        island_mode: IslandRemovalMode::IrmAlways as i32,
        min_island_area: 0,
        fill_mode: ZoneFillMode::ZfmSolid as i32,
        // Only read when fill_mode is ZFM_HATCHED.
        hatch_settings: None,
        net: Some(net(spec.net_name, net_code)),
        teardrop: Some(TeardropSettings {
            r#type: TeardropType::TdtNone as i32,
        }),
    };

    kiapi::board::types::Zone {
        id: None, // KiCAD assigns the ID
        r#type: ZoneType::ZtCopper as i32,
        layers: vec![layer_from_name(spec.layer) as i32],
        outline: Some(outline),
        name: spec.name.to_string(),
        priority: spec.priority,
        filled: false,
        filled_polygons: vec![],
        border: Some(ZoneBorderSettings {
            style: ZoneBorderStyle::ZbsDiagonalEdge as i32,
            pitch: Some(distance(ZONE_BORDER_PITCH_MM)),
        }),
        locked: kiapi::common::types::LockedState::LsUnlocked as i32,
        layer_properties: vec![],
        settings: Some(kiapi::board::types::zone::Settings::CopperSettings(copper)),
        parent: None,
    }
}

/// Pack a protobuf message into a prost_types::Any.
pub fn pack_any<M: prost::Message>(msg: &M, type_name: &str) -> prost_types::Any {
    let mut buf = Vec::new();
    msg.encode(&mut buf).expect("protobuf encode failed");
    prost_types::Any {
        type_url: format!("type.googleapis.com/{}", type_name),
        value: buf,
    }
}

/// Whether a packed `Any` carries the named KiCAD message type.
///
/// A footprint definition keeps its pads, graphic shapes, text, fields and
/// zones in **one** repeated `Any` field, so anything picking a particular kind
/// out of that list has to discriminate first — and the only honest
/// discriminator is the type URL.
///
/// Decoding is not a discriminator. proto3 skips field numbers it does not
/// recognise instead of failing, so a `BoardGraphicShape` decodes cleanly as a
/// near-empty `Pad`. Code that used `Pad::decode(...)` as its filter therefore
/// accepted every graphic in the footprint and wrote each one back *as a pad*,
/// which is how `update_pcb_from_schematic` silently destroyed the artwork of
/// every footprint it touched (#244).
pub fn any_is(item: &prost_types::Any, type_name: &str) -> bool {
    any_type_name(item) == type_name
}

/// The fully-qualified message name a packed `Any` declares, without the
/// `type.googleapis.com/` prefix.
///
/// Compare this for equality rather than testing the raw `type_url` with
/// `ends_with`: a suffix test also accepts a differently-namespaced message
/// whose qualified name happens to end the same way.
pub fn any_type_name(item: &prost_types::Any) -> &str {
    item.type_url.rsplit('/').next().unwrap_or("")
}

// --- Graphic primitive builders (BoardGraphicShape + BoardText) --------------
//
// All wrap a common `stroke(width_mm)` + `fill(filled)` into `GraphicAttributes`,
// then pack the geometry into `GraphicShape::geometry` (a oneof). Callers `pack_any`
// the result and hand it to `create_items` / `update_items`, same shape as
// `add_track` already uses.

fn stroke(width_mm: f64) -> kiapi::common::types::StrokeAttributes {
    kiapi::common::types::StrokeAttributes {
        width: Some(distance(width_mm)),
        // KiCad's `(type default)`; `with_line_style` sets another. Color stays
        // the board default.
        style: kiapi::common::types::StrokeLineStyle::SlsDefault as i32,
        color: None,
    }
}

/// Give a shape built by the `board_*` builders the line style `style`.
pub fn with_line_style(
    mut shape: kiapi::board::types::BoardGraphicShape,
    style: crate::IpcLineStyle,
) -> kiapi::board::types::BoardGraphicShape {
    use crate::IpcLineStyle;
    use kiapi::common::types::StrokeLineStyle;
    let style = match style {
        IpcLineStyle::Default => StrokeLineStyle::SlsDefault,
        IpcLineStyle::Solid => StrokeLineStyle::SlsSolid,
        IpcLineStyle::Dash => StrokeLineStyle::SlsDash,
        IpcLineStyle::Dot => StrokeLineStyle::SlsDot,
        IpcLineStyle::DashDot => StrokeLineStyle::SlsDashdot,
        IpcLineStyle::DashDotDot => StrokeLineStyle::SlsDashdotdot,
    };
    if let Some(stroke) = shape
        .shape
        .as_mut()
        .and_then(|shape| shape.attributes.as_mut())
        .and_then(|attributes| attributes.stroke.as_mut())
    {
        stroke.style = style as i32;
    }
    shape
}

fn attrs(width_mm: f64, filled: bool) -> kiapi::common::types::GraphicAttributes {
    kiapi::common::types::GraphicAttributes {
        stroke: Some(stroke(width_mm)),
        fill: Some(kiapi::common::types::GraphicFillAttributes {
            fill_type: if filled {
                kiapi::common::types::GraphicFillType::GftFilled as i32
            } else {
                kiapi::common::types::GraphicFillType::GftUnfilled as i32
            },
            color: None,
        }),
    }
}

fn board_shape(
    layer: &str,
    attrs: kiapi::common::types::GraphicAttributes,
    geometry: kiapi::common::types::graphic_shape::Geometry,
) -> kiapi::board::types::BoardGraphicShape {
    kiapi::board::types::BoardGraphicShape {
        shape: Some(kiapi::common::types::GraphicShape {
            attributes: Some(attrs),
            geometry: Some(geometry),
        }),
        layer: layer_from_name(layer) as i32,
        net: None,
        id: None, // KiCAD assigns
        locked: kiapi::common::types::LockedState::LsUnlocked as i32,
        parent: None,
    }
}

/// Build a BoardGraphicShape for a straight segment.
#[allow(clippy::too_many_arguments)]
pub fn board_segment(
    layer: &str,
    width_mm: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
) -> kiapi::board::types::BoardGraphicShape {
    board_shape(
        layer,
        attrs(width_mm, false),
        kiapi::common::types::graphic_shape::Geometry::Segment(
            kiapi::common::types::GraphicSegmentAttributes {
                start: Some(vec2(x1, y1)),
                end: Some(vec2(x2, y2)),
            },
        ),
    )
}

/// Build a BoardGraphicShape rectangle. Corners are (x1,y1) and (x2,y2) in mm.
#[allow(clippy::too_many_arguments)]
pub fn board_rectangle(
    layer: &str,
    width_mm: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    filled: bool,
) -> kiapi::board::types::BoardGraphicShape {
    board_shape(
        layer,
        attrs(width_mm, filled),
        kiapi::common::types::graphic_shape::Geometry::Rectangle(
            kiapi::common::types::GraphicRectangleAttributes {
                top_left: Some(vec2(x1, y1)),
                bottom_right: Some(vec2(x2, y2)),
                corner_radius: None,
            },
        ),
    )
}

/// Build a BoardGraphicShape circle at (cx,cy) with radius r_mm.
pub fn board_circle(
    layer: &str,
    width_mm: f64,
    cx: f64,
    cy: f64,
    r_mm: f64,
    filled: bool,
) -> kiapi::board::types::BoardGraphicShape {
    board_shape(
        layer,
        attrs(width_mm, filled),
        kiapi::common::types::graphic_shape::Geometry::Circle(
            kiapi::common::types::GraphicCircleAttributes {
                center: Some(vec2(cx, cy)),
                // Point on the circumference -- KiCAD stores this rather than a radius scalar.
                radius_point: Some(vec2(cx + r_mm, cy)),
            },
        ),
    )
}

/// Build a BoardGraphicShape arc from start / mid / end points.
#[allow(clippy::too_many_arguments)]
pub fn board_arc(
    layer: &str,
    width_mm: f64,
    sx: f64,
    sy: f64,
    mx: f64,
    my: f64,
    ex: f64,
    ey: f64,
) -> kiapi::board::types::BoardGraphicShape {
    board_shape(
        layer,
        attrs(width_mm, false),
        kiapi::common::types::graphic_shape::Geometry::Arc(
            kiapi::common::types::GraphicArcAttributes {
                start: Some(vec2(sx, sy)),
                mid: Some(vec2(mx, my)),
                end: Some(vec2(ex, ey)),
            },
        ),
    )
}

/// Build a BoardGraphicShape polygon (or set of polygons) from closed point
/// loops in mm — one `PolygonWithHoles` per outline, no holes. Used by
/// `import_svg_logo` to place flattened SVG artwork as filled board graphics
/// (stroke width 0) and by footprint placement for `fp_poly` outlines and
/// non-cardinal-rotation rectangles, which keep their stroke width.
pub fn board_polygon(
    layer: &str,
    width_mm: f64,
    filled: bool,
    outlines: &[Vec<(f64, f64)>],
) -> kiapi::board::types::BoardGraphicShape {
    let polygons = outlines
        .iter()
        .map(|pts| kiapi::common::types::PolygonWithHoles {
            outline: Some(kiapi::common::types::PolyLine {
                nodes: pts
                    .iter()
                    .map(|&(x, y)| kiapi::common::types::PolyLineNode {
                        geometry: Some(kiapi::common::types::poly_line_node::Geometry::Point(
                            vec2(x, y),
                        )),
                    })
                    .collect(),
                closed: true,
            }),
            holes: vec![],
        })
        .collect();

    board_shape(
        layer,
        attrs(width_mm, filled),
        kiapi::common::types::graphic_shape::Geometry::Polygon(kiapi::common::types::PolySet {
            polygons,
        }),
    )
}

/// Build a BoardText. `size_mm` sets both width and height of the glyphs.
#[allow(clippy::too_many_arguments)]
pub fn board_text(
    layer: &str,
    text: &str,
    x: f64,
    y: f64,
    size_mm: f64,
    rotation_deg: f64,
    mirror: bool,
) -> kiapi::board::types::BoardText {
    board_text_with_stroke_width(
        layer,
        text,
        x,
        y,
        size_mm,
        size_mm * 0.15,
        rotation_deg,
        mirror,
    )
}

/// Build a BoardText while preserving an explicit font stroke width.
#[allow(clippy::too_many_arguments)]
pub fn board_text_with_stroke_width(
    layer: &str,
    text: &str,
    x: f64,
    y: f64,
    size_mm: f64,
    stroke_width_mm: f64,
    rotation_deg: f64,
    mirror: bool,
) -> kiapi::board::types::BoardText {
    kiapi::board::types::BoardText {
        id: None,
        text: Some(kiapi::common::types::Text {
            position: Some(vec2(x, y)),
            attributes: Some(kiapi::common::types::TextAttributes {
                // ponytail: font/alignment/bold/italic left at proto default.
                // Add args (or a builder struct) when a caller needs them.
                font_name: String::new(),
                horizontal_alignment: kiapi::common::types::HorizontalAlignment::HaCenter as i32,
                vertical_alignment: kiapi::common::types::VerticalAlignment::VaCenter as i32,
                angle: Some(kiapi::common::types::Angle {
                    value_degrees: rotation_deg,
                }),
                line_spacing: 1.0,
                stroke_width: Some(distance(stroke_width_mm)),
                italic: false,
                bold: false,
                underlined: false,
                visible: true,
                mirrored: mirror,
                multiline: false,
                keep_upright: false,
                size: Some(vec2(size_mm, size_mm)),
            }),
            text: text.to_string(),
            hyperlink: String::new(),
        }),
        layer: layer_from_name(layer) as i32,
        knockout: false,
        locked: kiapi::common::types::LockedState::LsUnlocked as i32,
        parent: None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kiapi::common::types::graphic_shape::Geometry;

    /// A shape built without a style asks for KiCad's `default`, not the
    /// proto's `SLS_UNKNOWN` (#970).
    #[test]
    fn shapes_default_to_kicads_default_line_style() {
        let shape = board_segment("F.SilkS", 0.12, 0.0, 0.0, 1.0, 0.0);
        let stroke = shape.shape.unwrap().attributes.unwrap().stroke.unwrap();
        assert_eq!(
            stroke.style(),
            kiapi::common::types::StrokeLineStyle::SlsDefault
        );
    }

    /// `layer_name` is the exact inverse of `layer_from_name` over every
    /// representable layer — computed runs included. The forward map was
    /// widened for #237; a reverse map that lags it turns real layers into
    /// "Unknown" in pad and track responses.
    #[test]
    fn layer_name_round_trips_every_representable_layer() {
        use kiapi::board::types::BoardLayer;
        let mut named = 0;
        for value in 0..=200 {
            let Ok(layer) = BoardLayer::try_from(value) else {
                continue;
            };
            if layer == BoardLayer::BlUndefined {
                assert_eq!(layer_name(layer), None, "BL_UNDEFINED has no name");
                continue;
            }
            if let Some(name) = layer_name(layer) {
                named += 1;
                assert_eq!(
                    layer_from_name(name),
                    layer,
                    "'{name}' must map back to {layer:?}"
                );
            }
        }
        // 2 outer + 30 inner copper, 45 user, and the 19 named non-copper
        // layers the forward map lists.
        assert_eq!(named, 96, "every representable layer carries a name");

        for (name, expected) in [
            ("In3.Cu", true),
            ("In30.Cu", true),
            ("User.9", true),
            ("User.10", true),
            ("User.45", true),
            ("Rescue", true),
        ] {
            let layer = layer_from_name(name);
            assert_eq!(
                layer_name(layer) == Some(name),
                expected,
                "spot check {name} -> {layer:?}"
            );
        }
    }

    /// The builder is the only thing standing between an `add_zone` request
    /// and the board, so every user-visible choice is asserted here: the mock
    /// server echoes requests back rather than running KiCad's parser, so no
    /// transport test can check a zone's contents for us.
    #[test]
    fn build_zone_carries_layer_net_outline_and_the_caller_s_choices() {
        use kiapi::board::types::{IslandRemovalMode, ZoneConnectionStyle, ZoneFillMode, ZoneType};

        let points = [(1.0, 2.0), (11.0, 2.0), (11.0, 7.5)];
        let zone = build_zone(
            &ZoneSpec {
                layer: "In1.Cu",
                net_name: "GND",
                points: &points,
                clearance_mm: 0.3,
                min_thickness_mm: 0.25,
                name: "ground pour",
                priority: 2,
                connection: ZoneConnectionStyle::ZcsFull,
            },
            7,
        );

        assert_eq!(zone.r#type, ZoneType::ZtCopper as i32);
        assert_eq!(
            zone.layers,
            vec![kiapi::board::types::BoardLayer::BlIn1Cu as i32]
        );
        assert_eq!(zone.name, "ground pour");
        assert_eq!(zone.priority, 2);
        assert!(zone.id.is_none(), "KiCAD assigns the id");

        // mm → nm, and the loop is closed for the caller.
        let outline = zone.outline.expect("outline");
        assert_eq!(outline.polygons.len(), 1);
        let line = outline.polygons[0].outline.clone().expect("polyline");
        assert!(line.closed);
        let coords: Vec<(i64, i64)> = line
            .nodes
            .iter()
            .map(
                |node| match node.geometry.as_ref().expect("node geometry") {
                    kiapi::common::types::poly_line_node::Geometry::Point(p) => (p.x_nm, p.y_nm),
                    other => panic!("expected a point node, got {other:?}"),
                },
            )
            .collect();
        assert_eq!(
            coords,
            vec![
                (1_000_000, 2_000_000),
                (11_000_000, 2_000_000),
                (11_000_000, 7_500_000)
            ]
        );

        let settings = match zone.settings.expect("settings") {
            kiapi::board::types::zone::Settings::CopperSettings(s) => s,
            other => panic!("a copper zone must carry CopperZoneSettings, got {other:?}"),
        };
        let net = settings.net.expect("net");
        assert_eq!(net.name, "GND");
        assert_eq!(net.code.expect("net code").value, 7);
        assert_eq!(settings.clearance.expect("clearance").value_nm, 300_000);
        assert_eq!(
            settings.min_thickness.expect("min thickness").value_nm,
            250_000
        );
        assert_eq!(
            settings.connection.expect("connection").zone_connection,
            ZoneConnectionStyle::ZcsFull as i32
        );
        assert_eq!(settings.fill_mode, ZoneFillMode::ZfmSolid as i32);
        assert_eq!(settings.island_mode, IslandRemovalMode::IrmAlways as i32);
    }

    /// A zone must go over the wire unfilled: the fill polygons are KiCad's to
    /// compute against the design rules, which is why `add_zone` follows the
    /// create with a RefillZones. Sending our own would be a fill that honours
    /// no clearance KiCad knows about.
    #[test]
    fn build_zone_leaves_the_fill_for_kicad() {
        let zone = build_zone(
            &ZoneSpec {
                layer: "F.Cu",
                net_name: "GND",
                points: &[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)],
                clearance_mm: 0.2,
                min_thickness_mm: 0.2,
                name: "",
                priority: 0,
                connection: kiapi::board::types::ZoneConnectionStyle::ZcsThermal,
            },
            1,
        );
        assert!(!zone.filled);
        assert!(zone.filled_polygons.is_empty());
    }

    /// `BL_UNDEFINED` is not something KiCad rejects, it is something it
    /// crashes on (see `try_layer_from_name`), and a zone names its layer in a
    /// repeated field where a bad value is easy to miss.
    #[test]
    fn build_zone_resolves_every_copper_layer_name_it_is_given() {
        for layer in ["F.Cu", "In1.Cu", "In14.Cu", "B.Cu"] {
            let zone = build_zone(
                &ZoneSpec {
                    layer,
                    net_name: "GND",
                    points: &[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)],
                    clearance_mm: 0.2,
                    min_thickness_mm: 0.2,
                    name: "",
                    priority: 0,
                    connection: kiapi::board::types::ZoneConnectionStyle::ZcsThermal,
                },
                1,
            );
            assert_eq!(
                zone.layers,
                vec![try_layer_from_name(layer).expect("known layer") as i32],
                "{layer}"
            );
        }
    }

    /// `any_is` compares the whole message name, not a suffix of the URL.
    ///
    /// A suffix test accepts anything whose qualified name merely *ends* the
    /// same way — a different package with the same tail decodes as something
    /// else entirely. Being exact costs nothing and removes the question.
    #[test]
    fn any_is_matches_the_whole_type_name_not_a_suffix() {
        let pad = prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.Pad".to_string(),
            value: Vec::new(),
        };
        assert!(any_is(&pad, "kiapi.board.types.Pad"));
        assert!(!any_is(&pad, "kiapi.board.types.BoardGraphicShape"));
        assert_eq!(any_type_name(&pad), "kiapi.board.types.Pad");

        let other_package = prost_types::Any {
            type_url: "type.googleapis.com/vendorx.kiapi.board.types.Pad".to_string(),
            value: Vec::new(),
        };
        assert!(
            !any_is(&other_package, "kiapi.board.types.Pad"),
            "a different package that ends in the same name is not the same message"
        );

        // A bare name with no domain prefix still resolves.
        let bare = prost_types::Any {
            type_url: "kiapi.board.types.Pad".to_string(),
            value: Vec::new(),
        };
        assert!(any_is(&bare, "kiapi.board.types.Pad"));
    }

    /// Every layer a KiCAD 10 footprint may legally draw on has a `BoardLayer`.
    ///
    /// `Dwgs.User` is the one that mattered: two official library footprints
    /// (`Connector_USB:USB_C_Receptacle_GCT_USB4105-xx-A_16P_TopMnt_Horizontal`
    /// and `Connector:BJB_Pico_46.110.1001_Receptacle_Horizontal`) carry
    /// `Dwgs.User` children, this map returned `BL_UNDEFINED` for them, and
    /// KiCAD 10.0.5 faults consuming that value rather than refusing the
    /// message — taking the user's unsaved board with it (#237).
    #[test]
    fn every_footprint_drawing_layer_maps_to_a_real_board_layer() {
        use kiapi::board::types::BoardLayer;

        for (name, expected) in [
            ("Dwgs.User", BoardLayer::BlDwgsUser),
            ("Cmts.User", BoardLayer::BlCmtsUser),
            ("Eco1.User", BoardLayer::BlEco1User),
            ("Eco2.User", BoardLayer::BlEco2User),
            ("F.Adhes", BoardLayer::BlFAdhes),
            ("B.Adhes", BoardLayer::BlBAdhes),
            ("Margin", BoardLayer::BlMargin),
            ("Rescue", BoardLayer::BlRescue),
        ] {
            assert_eq!(layer_from_name(name), expected, "{name}");
        }
    }

    /// Inner copper and user layers are numbered, not enumerable by hand: a
    /// board may have up to `In30.Cu`, and `User.1`–`User.45` exist too. The
    /// enum is contiguous for both runs *except* that `BL_Rescue = 62` sits
    /// between `BL_User_9 = 61` and `BL_User_10 = 63`.
    #[test]
    fn numbered_copper_and_user_layers_step_over_the_rescue_hole() {
        use kiapi::board::types::BoardLayer;

        assert_eq!(layer_from_name("In1.Cu"), BoardLayer::BlIn1Cu);
        assert_eq!(layer_from_name("In3.Cu"), BoardLayer::BlIn3Cu);
        assert_eq!(layer_from_name("In30.Cu"), BoardLayer::BlIn30Cu);
        assert_eq!(layer_from_name("User.1"), BoardLayer::BlUser1);
        assert_eq!(layer_from_name("User.9"), BoardLayer::BlUser9);
        assert_eq!(layer_from_name("User.10"), BoardLayer::BlUser10);
        assert_eq!(layer_from_name("User.45"), BoardLayer::BlUser45);

        // Out of range on either side stays unrepresentable rather than
        // wrapping onto an unrelated layer.
        for out_of_range in ["In0.Cu", "In31.Cu", "User.0", "User.46"] {
            assert_eq!(
                layer_from_name(out_of_range),
                BoardLayer::BlUndefined,
                "{out_of_range}"
            );
        }
    }

    /// A name with no `BoardLayer` is refused rather than converted to
    /// `BL_UNDEFINED`, because nothing downstream of here can tell the two
    /// apart and KiCAD does not validate the field.
    #[test]
    fn an_unrepresentable_layer_is_refused_not_silently_undefined() {
        assert!(try_layer_from_name("F.Cu").is_ok());
        assert!(try_layer_from_name("Dwgs.User").is_ok());

        let error = try_layer_from_name("Not.A.Layer").expect_err("must refuse");
        let message = format!("{error:#}");
        assert!(message.contains("Not.A.Layer"), "{message}");
    }

    #[test]
    fn track_arc_carries_route_geometry_layer_width_and_net() {
        let arc = build_track_arc("GND", 3, "B.Cu", 0.25, 10.0, 20.0, 11.0, 21.0, 12.0, 20.0);
        assert_eq!(arc.start.unwrap().x_nm, 10_000_000);
        assert_eq!(arc.mid.unwrap().y_nm, 21_000_000);
        assert_eq!(arc.end.unwrap().x_nm, 12_000_000);
        assert_eq!(arc.width.unwrap().value_nm, 250_000);
        assert_eq!(arc.layer, kiapi::board::types::BoardLayer::BlBCu as i32);
        assert_eq!(arc.net.unwrap().name, "GND");
    }

    #[test]
    fn segment_populates_start_end_and_layer() {
        let s = board_segment("Edge.Cuts", 0.05, 1.0, 2.0, 3.0, 4.0);
        assert_eq!(s.layer, kiapi::board::types::BoardLayer::BlEdgeCuts as i32);
        let shape = s.shape.expect("shape");
        match shape.geometry.expect("geometry") {
            Geometry::Segment(g) => {
                assert_eq!(g.start.unwrap().x_nm, 1_000_000);
                assert_eq!(g.start.unwrap().y_nm, 2_000_000);
                assert_eq!(g.end.unwrap().x_nm, 3_000_000);
                assert_eq!(g.end.unwrap().y_nm, 4_000_000);
            }
            _ => panic!("expected Segment geometry"),
        }
        let a = shape.attributes.expect("attrs");
        assert_eq!(a.stroke.unwrap().width.unwrap().value_nm, 50_000);
        assert_eq!(
            a.fill.unwrap().fill_type,
            kiapi::common::types::GraphicFillType::GftUnfilled as i32
        );
    }

    #[test]
    fn rectangle_variant_and_filled_flag() {
        let s = board_rectangle("F.SilkS", 0.1, 0.0, 0.0, 10.0, 5.0, true);
        assert_eq!(s.layer, kiapi::board::types::BoardLayer::BlFSilkS as i32);
        let shape = s.shape.expect("shape");
        assert!(matches!(shape.geometry, Some(Geometry::Rectangle(_))));
        assert_eq!(
            shape.attributes.unwrap().fill.unwrap().fill_type,
            kiapi::common::types::GraphicFillType::GftFilled as i32
        );
    }

    #[test]
    fn circle_radius_point_is_center_plus_radius() {
        let s = board_circle("F.SilkS", 0.1, 5.0, 5.0, 2.5, false);
        match s.shape.unwrap().geometry.unwrap() {
            Geometry::Circle(c) => {
                assert_eq!(c.center.unwrap().x_nm, 5_000_000);
                assert_eq!(c.radius_point.unwrap().x_nm, 7_500_000);
                assert_eq!(c.radius_point.unwrap().y_nm, 5_000_000);
            }
            _ => panic!("expected Circle geometry"),
        }
    }

    #[test]
    fn arc_start_mid_end_populated() {
        let s = board_arc("F.SilkS", 0.1, 0.0, 0.0, 1.0, 1.0, 2.0, 0.0);
        match s.shape.unwrap().geometry.unwrap() {
            Geometry::Arc(a) => {
                assert_eq!(a.start.unwrap().x_nm, 0);
                assert_eq!(a.mid.unwrap().x_nm, 1_000_000);
                assert_eq!(a.end.unwrap().x_nm, 2_000_000);
            }
            _ => panic!("expected Arc geometry"),
        }
    }

    #[test]
    fn text_carries_position_size_layer_and_rotation() {
        let t = board_text("F.SilkS", "hi", 12.0, 34.0, 1.5, 90.0, false);
        assert_eq!(t.layer, kiapi::board::types::BoardLayer::BlFSilkS as i32);
        let text = t.text.expect("text");
        assert_eq!(text.text, "hi");
        assert_eq!(text.position.unwrap().x_nm, 12_000_000);
        let attrs = text.attributes.expect("attrs");
        assert_eq!(attrs.size.unwrap().x_nm, 1_500_000);
        assert!((attrs.stroke_width.unwrap().value_nm - 225_000).abs() <= 1);
        assert_eq!(attrs.angle.unwrap().value_degrees, 90.0);
        assert!(!attrs.mirrored);

        let explicit =
            board_text_with_stroke_width("F.Fab", "${REFERENCE}", 0.0, 0.0, 0.8, 0.11, 0.0, false);
        assert_eq!(
            explicit
                .text
                .unwrap()
                .attributes
                .unwrap()
                .stroke_width
                .unwrap()
                .value_nm,
            110_000
        );
    }

    #[test]
    fn polygon_builds_one_polygon_with_holes_per_outline() {
        let outlines = vec![
            vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)],
            vec![(5.0, 5.0), (6.0, 5.0), (6.0, 6.0)],
        ];
        let s = board_polygon("F.SilkS", 0.0, true, &outlines);
        assert_eq!(s.layer, kiapi::board::types::BoardLayer::BlFSilkS as i32);
        let shape = s.shape.expect("shape");
        assert_eq!(
            shape.attributes.unwrap().fill.unwrap().fill_type,
            kiapi::common::types::GraphicFillType::GftFilled as i32
        );
        match shape.geometry.expect("geometry") {
            Geometry::Polygon(poly_set) => {
                assert_eq!(poly_set.polygons.len(), 2);
                let first = &poly_set.polygons[0];
                assert!(first.holes.is_empty());
                let outline = first.outline.as_ref().expect("outline");
                assert!(outline.closed);
                assert_eq!(outline.nodes.len(), 3);
            }
            _ => panic!("expected Polygon geometry"),
        }
    }

    #[test]
    fn polygon_nodes_carry_point_coordinates_in_nanometers() {
        let outlines = vec![vec![(1.0, 2.0)]];
        let s = board_polygon("F.Cu", 0.0, false, &outlines);
        match s.shape.unwrap().geometry.unwrap() {
            Geometry::Polygon(poly_set) => {
                let node = &poly_set.polygons[0].outline.as_ref().unwrap().nodes[0];
                match node.geometry.as_ref().expect("node geometry") {
                    kiapi::common::types::poly_line_node::Geometry::Point(p) => {
                        assert_eq!(p.x_nm, 1_000_000);
                        assert_eq!(p.y_nm, 2_000_000);
                    }
                    _ => panic!("expected Point node"),
                }
            }
            _ => panic!("expected Polygon geometry"),
        }
    }

    #[test]
    fn polygon_empty_outlines_produces_empty_polyset() {
        let s = board_polygon("F.SilkS", 0.0, true, &[]);
        match s.shape.unwrap().geometry.unwrap() {
            Geometry::Polygon(poly_set) => assert!(poly_set.polygons.is_empty()),
            _ => panic!("expected Polygon geometry"),
        }
    }

    #[test]
    fn via_is_a_through_via_with_position_drill_size_and_net() {
        use kiapi::board::types::{BoardLayer, PadStackShape, PadStackType, ViaType};

        let v = build_via("VCC_BATT", 7, 146.268, 89.194, 0.2, 0.45);

        // Position, in nanometers.
        let pos = v.position.expect("position");
        assert_eq!(pos.x_nm, 146_268_000);
        assert_eq!(pos.y_nm, 89_194_000);

        // Net carried through.
        let net = v.net.expect("net");
        assert_eq!(net.name, "VCC_BATT");
        assert_eq!(net.code.unwrap().value, 7);

        // Through via (F.Cu → B.Cu), normal pad stack.
        assert_eq!(v.r#type, ViaType::VtThrough as i32);
        let ps = v.pad_stack.expect("pad_stack");
        assert_eq!(ps.r#type, PadStackType::PstNormal as i32);
        assert_eq!(
            ps.layers,
            vec![BoardLayer::BlFCu as i32, BoardLayer::BlBCu as i32]
        );

        // The drill's start/end layers are what make it a through via.
        let drill = ps.drill.expect("drill");
        assert_eq!(drill.start_layer, BoardLayer::BlFCu as i32);
        assert_eq!(drill.end_layer, BoardLayer::BlBCu as i32);
        assert_eq!(drill.diameter.unwrap().x_nm, 200_000);
        assert_eq!(
            drill.shape,
            kiapi::board::types::DrillShape::DsCircle as i32,
            "leaving the drill shape at the proto default (DS_UNKNOWN) is what \
             the working footprint-pad path avoids"
        );

        // Exactly one copper entry — see assert_normal_padstack_is_unpackable.
        assert_eq!(ps.copper_layers.len(), 1);
        assert_eq!(ps.copper_layers[0].shape, PadStackShape::PssCircle as i32);
        assert_eq!(ps.copper_layers[0].size.unwrap().x_nm, 450_000);

        assert_normal_padstack_is_unpackable(&ps, "build_via");
    }

    /// KiCad's own rule, enforceable without a running KiCAD.
    ///
    /// `PADSTACK::unpackCopperLayer` bails when `m_mode == MODE::NORMAL` and
    /// the entry's layer is not `ALL_LAYERS` (`== F_Cu`), which fails the
    /// enclosing `PCB_VIA::Deserialize` / `PAD::Deserialize` and comes back as
    /// `AS_BAD_REQUEST "could not unpack …"`. Sending F.Cu *and* B.Cu entries
    /// is what broke `add_via` in v0.2.1 (#117); the through span belongs to
    /// the drill, not to the copper list.
    ///
    /// Anything constructing a PST_NORMAL padstack should assert this — the
    /// mock server echoes requests back rather than running KiCad's parser, so
    /// no transport test can catch a malformed padstack for us.
    pub(crate) fn assert_normal_padstack_is_unpackable(
        ps: &kiapi::board::types::PadStack,
        what: &str,
    ) {
        use kiapi::board::types::{BoardLayer, PadStackType};

        if ps.r#type != PadStackType::PstNormal as i32 {
            return;
        }
        assert_eq!(
            ps.copper_layers.len(),
            1,
            "{what}: a PST_NORMAL padstack must carry exactly one copper layer, \
             got {} — KiCad rejects the whole message",
            ps.copper_layers.len()
        );
        assert_eq!(
            ps.copper_layers[0].layer,
            BoardLayer::BlFCu as i32,
            "{what}: the single copper entry of a PST_NORMAL padstack must be \
             keyed to F_Cu (KiCad's ALL_LAYERS sentinel)"
        );
    }
}
