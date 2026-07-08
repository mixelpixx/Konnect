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

/// Map a layer name string to the BoardLayer enum value.
pub fn layer_from_name(name: &str) -> kiapi::board::types::BoardLayer {
    match name {
        "F.Cu" => kiapi::board::types::BoardLayer::BlFCu,
        "B.Cu" => kiapi::board::types::BoardLayer::BlBCu,
        "In1.Cu" => kiapi::board::types::BoardLayer::BlIn1Cu,
        "In2.Cu" => kiapi::board::types::BoardLayer::BlIn2Cu,
        "F.SilkS" | "F.Silkscreen" => kiapi::board::types::BoardLayer::BlFSilkS,
        "B.SilkS" | "B.Silkscreen" => kiapi::board::types::BoardLayer::BlBSilkS,
        "F.Mask" => kiapi::board::types::BoardLayer::BlFMask,
        "B.Mask" => kiapi::board::types::BoardLayer::BlBMask,
        "F.Paste" => kiapi::board::types::BoardLayer::BlFPaste,
        "B.Paste" => kiapi::board::types::BoardLayer::BlBPaste,
        "F.CrtYd" | "F.Courtyard" => kiapi::board::types::BoardLayer::BlFCrtYd,
        "B.CrtYd" | "B.Courtyard" => kiapi::board::types::BoardLayer::BlBCrtYd,
        "F.Fab" => kiapi::board::types::BoardLayer::BlFFab,
        "B.Fab" => kiapi::board::types::BoardLayer::BlBFab,
        "Edge.Cuts" => kiapi::board::types::BoardLayer::BlEdgeCuts,
        _ => kiapi::board::types::BoardLayer::BlUndefined,
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
    }
}

/// Build S-expression for a via (used with ParseAndCreateItemsFromString).
/// Complex protobuf PadStack construction is avoided this way.
pub fn via_sexp(
    net_name: &str,
    net_code: i32,
    x: f64,
    y: f64,
    drill_mm: f64,
    size_mm: f64,
) -> String {
    format!(
        r#"(via (at {} {}) (size {}) (drill {}) (layers "F.Cu" "B.Cu") (net {} "{}"))"#,
        x, y, size_mm, drill_mm, net_code, net_name
    )
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

// --- Graphic primitive builders (BoardGraphicShape + BoardText) --------------
//
// All wrap a common `stroke(width_mm)` + `fill(filled)` into `GraphicAttributes`,
// then pack the geometry into `GraphicShape::geometry` (a oneof). Callers `pack_any`
// the result and hand it to `create_items` / `update_items`, same shape as
// `add_track` already uses.

fn stroke(width_mm: f64) -> kiapi::common::types::StrokeAttributes {
    kiapi::common::types::StrokeAttributes {
        width: Some(distance(width_mm)),
        // ponytail: leave style/color at proto default (solid, board default color).
        // Add args when a caller needs dashed/colored graphics.
        style: 0,
        color: None,
    }
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
) -> kiapi::board::types::BoardGraphicShape {
    board_shape(
        layer,
        attrs(width_mm, false),
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
                stroke_width: Some(distance(size_mm * 0.15)),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiapi::common::types::graphic_shape::Geometry;

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
        let s = board_circle("F.SilkS", 0.1, 5.0, 5.0, 2.5);
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
        assert_eq!(attrs.angle.unwrap().value_degrees, 90.0);
        assert!(!attrs.mirrored);
    }
}
