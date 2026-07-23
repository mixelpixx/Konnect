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
        BoardLayer, DrillProperties, PadStack, PadStackLayer, PadStackShape, PadStackType, ViaType,
    };

    // A round copper pad of `size_mm` diameter on the given layer.
    let copper_pad = |layer: BoardLayer| kiapi::board::types::PadStackLayer {
        layer: layer as i32,
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
            ..DrillProperties::default()
        }),
        copper_layers: vec![copper_pad(BoardLayer::BlFCu), copper_pad(BoardLayer::BlBCu)],
        ..PadStack::default()
    };

    kiapi::board::types::Via {
        id: None, // KiCAD assigns the ID
        position: Some(vec2(x, y)),
        pad_stack: Some(pad_stack),
        locked: kiapi::common::types::LockedState::LsUnlocked as i32,
        net: Some(net(net_name, net_code)),
        r#type: ViaType::VtThrough as i32,
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

/// Build a BoardGraphicShape polygon (or set of polygons) from closed point
/// loops in mm — one `PolygonWithHoles` per outline, no holes. Used by
/// `import_svg_logo` to place flattened SVG artwork as filled board graphics.
pub fn board_polygon(
    layer: &str,
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
        attrs(0.0, filled),
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

    #[test]
    fn polygon_builds_one_polygon_with_holes_per_outline() {
        let outlines = vec![
            vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)],
            vec![(5.0, 5.0), (6.0, 5.0), (6.0, 6.0)],
        ];
        let s = board_polygon("F.SilkS", true, &outlines);
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
        let s = board_polygon("F.Cu", false, &outlines);
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
        let s = board_polygon("F.SilkS", true, &[]);
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

        // Drill diameter honored on both outer layers.
        let drill = ps.drill.expect("drill");
        assert_eq!(drill.start_layer, BoardLayer::BlFCu as i32);
        assert_eq!(drill.end_layer, BoardLayer::BlBCu as i32);
        assert_eq!(drill.diameter.unwrap().x_nm, 200_000);

        // Copper pad on both layers, round, at the requested diameter.
        assert_eq!(ps.copper_layers.len(), 2);
        for layer in &ps.copper_layers {
            assert_eq!(layer.shape, PadStackShape::PssCircle as i32);
            assert_eq!(layer.size.unwrap().x_nm, 450_000);
        }
    }
}
