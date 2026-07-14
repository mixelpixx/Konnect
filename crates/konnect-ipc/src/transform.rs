//! Rigid-body transforms for `FootprintInstance` round-trips (issue #23).
//!
//! KiCAD 10 serializes every child of a footprint (pads, fields, texts,
//! graphics, zones) in ABSOLUTE board coordinates — despite the proto comment
//! on `Pad.position` claiming footprint-relative coordinates — and
//! `FOOTPRINT::Deserialize` clears all children and re-creates them at exactly
//! the coordinates carried in the message. Round-tripping an instance with
//! only `position` or `orientation` changed therefore leaves the copper and
//! silkscreen at the old location, detached from the anchor. Before sending
//! `UpdateItems`, every child must get the same rigid-body transform KiCAD
//! would apply natively (`FOOTPRINT::SetPosition` / `SetOrientation`).

use crate::gen::kiapi;
use anyhow::Result;
use kiapi::board::types as board;
use kiapi::common::types as common;
use prost::Message;

/// The transform to apply to a footprint's children.
#[derive(Clone, Copy)]
pub enum Xform {
    /// Shift every child by (dx, dy) nanometers.
    Translate { dx_nm: i64, dy_nm: i64 },
    /// Rotate every child by `delta_deg` around (cx, cy), matching KiCAD's
    /// convention (Y axis down; positive angles counterclockwise on screen).
    Rotate {
        cx_nm: i64,
        cy_nm: i64,
        delta_deg: f64,
    },
}

impl Xform {
    fn point(&self, x: i64, y: i64) -> (i64, i64) {
        match *self {
            Xform::Translate { dx_nm, dy_nm } => (x + dx_nm, y + dy_nm),
            Xform::Rotate {
                cx_nm,
                cy_nm,
                delta_deg,
            } => {
                // KiCAD's RotatePoint (libs/kimath/src/trigo.cpp):
                //   x' = x·cosθ + y·sinθ ;  y' = y·cosθ − x·sinθ
                let (rx, ry) = ((x - cx_nm) as f64, (y - cy_nm) as f64);
                let (s, c) = delta_deg.to_radians().sin_cos();
                (
                    cx_nm + (rx * c + ry * s).round() as i64,
                    cy_nm + (ry * c - rx * s).round() as i64,
                )
            }
        }
    }

    fn angle_delta_deg(&self) -> f64 {
        match *self {
            Xform::Translate { .. } => 0.0,
            Xform::Rotate { delta_deg, .. } => delta_deg,
        }
    }

    /// True when the rotation keeps axis-aligned shapes axis-aligned.
    fn is_cardinal(&self) -> bool {
        let r = self.angle_delta_deg().rem_euclid(90.0);
        r.abs() < 1e-9 || (90.0 - r).abs() < 1e-9
    }
}

/// Normalize to (-180, 180], the range KiCAD's `EDA_ANGLE::Normalize180` uses.
fn normalize_deg(deg: f64) -> f64 {
    let mut d = deg % 360.0;
    if d <= -180.0 {
        d += 360.0;
    } else if d > 180.0 {
        d -= 360.0;
    }
    d
}

fn xform_vec2(v: &mut Option<common::Vector2>, xf: &Xform) {
    if let Some(p) = v.as_mut() {
        let (x, y) = xf.point(p.x_nm, p.y_nm);
        p.x_nm = x;
        p.y_nm = y;
    }
}

fn xform_angle(a: &mut Option<common::Angle>, xf: &Xform) {
    let delta = xf.angle_delta_deg();
    if delta != 0.0 {
        let base = a.as_ref().map(|x| x.value_degrees).unwrap_or(0.0);
        *a = Some(common::Angle {
            value_degrees: normalize_deg(base + delta),
        });
    }
}

fn xform_text(t: &mut Option<common::Text>, xf: &Xform) {
    if let Some(text) = t.as_mut() {
        xform_vec2(&mut text.position, xf);
        if let Some(attrs) = text.attributes.as_mut() {
            xform_angle(&mut attrs.angle, xf);
        }
    }
}

fn xform_board_text(bt: &mut board::BoardText, xf: &Xform) {
    xform_text(&mut bt.text, xf);
}

fn xform_field(f: &mut Option<board::Field>, xf: &Xform) {
    if let Some(field) = f.as_mut() {
        if let Some(text) = field.text.as_mut() {
            xform_board_text(text, xf);
        }
    }
}

fn xform_polyline(pl: &mut common::PolyLine, xf: &Xform) {
    for node in pl.nodes.iter_mut() {
        match node.geometry.as_mut() {
            Some(common::poly_line_node::Geometry::Point(p)) => {
                let (x, y) = xf.point(p.x_nm, p.y_nm);
                p.x_nm = x;
                p.y_nm = y;
            }
            Some(common::poly_line_node::Geometry::Arc(arc)) => {
                xform_vec2(&mut arc.start, xf);
                xform_vec2(&mut arc.mid, xf);
                xform_vec2(&mut arc.end, xf);
            }
            None => {}
        }
    }
}

fn xform_polyset(ps: &mut common::PolySet, xf: &Xform) {
    for poly in ps.polygons.iter_mut() {
        if let Some(outline) = poly.outline.as_mut() {
            xform_polyline(outline, xf);
        }
        for hole in poly.holes.iter_mut() {
            xform_polyline(hole, xf);
        }
    }
}

fn xform_graphic_shape(gs: &mut board::BoardGraphicShape, xf: &Xform) -> Result<()> {
    use common::graphic_shape::Geometry;

    let Some(shape) = gs.shape.as_mut() else {
        return Ok(());
    };
    match shape.geometry.as_mut() {
        Some(Geometry::Segment(seg)) => {
            xform_vec2(&mut seg.start, xf);
            xform_vec2(&mut seg.end, xf);
        }
        Some(Geometry::Rectangle(rect)) => {
            if xf.is_cardinal() {
                xform_vec2(&mut rect.top_left, xf);
                xform_vec2(&mut rect.bottom_right, xf);
            } else if rect.corner_radius.is_some() {
                // A rounded rectangle can't be represented off-axis; KiCAD
                // itself converts plain rectangles to polygons here, but has
                // no lossless form for rounded ones.
                anyhow::bail!(
                    "cannot rotate a footprint containing a rounded-rectangle \
                     graphic by a non-90° angle; use a multiple of 90°"
                );
            } else {
                // Mirror KiCAD's EDA_SHAPE::Rotate: an off-axis rectangle
                // becomes a closed polygon of its four rotated corners.
                let (tl, br) = (
                    rect.top_left.unwrap_or_default(),
                    rect.bottom_right.unwrap_or_default(),
                );
                let corners = [
                    (tl.x_nm, tl.y_nm),
                    (br.x_nm, tl.y_nm),
                    (br.x_nm, br.y_nm),
                    (tl.x_nm, br.y_nm),
                ];
                let nodes = corners
                    .iter()
                    .map(|&(x, y)| {
                        let (nx, ny) = xf.point(x, y);
                        common::PolyLineNode {
                            geometry: Some(common::poly_line_node::Geometry::Point(
                                common::Vector2 { x_nm: nx, y_nm: ny },
                            )),
                        }
                    })
                    .collect();
                shape.geometry = Some(Geometry::Polygon(common::PolySet {
                    polygons: vec![common::PolygonWithHoles {
                        outline: Some(common::PolyLine {
                            nodes,
                            closed: true,
                        }),
                        holes: vec![],
                    }],
                }));
            }
        }
        Some(Geometry::Arc(arc)) => {
            xform_vec2(&mut arc.start, xf);
            xform_vec2(&mut arc.mid, xf);
            xform_vec2(&mut arc.end, xf);
        }
        Some(Geometry::Circle(circle)) => {
            xform_vec2(&mut circle.center, xf);
            xform_vec2(&mut circle.radius_point, xf);
        }
        Some(Geometry::Polygon(polyset)) => {
            xform_polyset(polyset, xf);
        }
        Some(Geometry::Bezier(bez)) => {
            xform_vec2(&mut bez.start, xf);
            xform_vec2(&mut bez.control1, xf);
            xform_vec2(&mut bez.control2, xf);
            xform_vec2(&mut bez.end, xf);
        }
        None => {}
    }
    Ok(())
}

fn xform_zone(zone: &mut board::Zone, xf: &Xform) {
    if let Some(outline) = zone.outline.as_mut() {
        xform_polyset(outline, xf);
    }
    for filled in zone.filled_polygons.iter_mut() {
        if let Some(shapes) = filled.shapes.as_mut() {
            xform_polyset(shapes, xf);
        }
    }
}

fn xform_dimension(dim: &mut board::Dimension, xf: &Xform) {
    use board::dimension::DimensionStyle;

    xform_text(&mut dim.text, xf);
    match dim.dimension_style.as_mut() {
        Some(DimensionStyle::Aligned(a)) => {
            xform_vec2(&mut a.start, xf);
            xform_vec2(&mut a.end, xf);
        }
        Some(DimensionStyle::Orthogonal(o)) => {
            xform_vec2(&mut o.start, xf);
            xform_vec2(&mut o.end, xf);
        }
        Some(DimensionStyle::Radial(r)) => {
            xform_vec2(&mut r.center, xf);
            xform_vec2(&mut r.radius_point, xf);
        }
        Some(DimensionStyle::Leader(l)) => {
            xform_vec2(&mut l.start, xf);
            xform_vec2(&mut l.end, xf);
        }
        Some(DimensionStyle::Center(c)) => {
            xform_vec2(&mut c.center, xf);
            xform_vec2(&mut c.end, xf);
        }
        None => {}
    }
}

/// Apply `xf` to every position-bearing child carried in `fp`: the four
/// mandatory fields plus all `definition.items`. Does NOT touch
/// `fp.position` / `fp.orientation` — the caller sets those.
pub fn transform_footprint_children(fp: &mut board::FootprintInstance, xf: &Xform) -> Result<()> {
    xform_field(&mut fp.reference_field, xf);
    xform_field(&mut fp.value_field, xf);
    xform_field(&mut fp.datasheet_field, xf);
    xform_field(&mut fp.description_field, xf);

    let Some(def) = fp.definition.as_mut() else {
        return Ok(());
    };

    for item in def.items.iter_mut() {
        let url = item.type_url.as_str();
        if url.ends_with("kiapi.board.types.Pad") {
            let mut pad = board::Pad::decode(item.value.as_slice())?;
            xform_vec2(&mut pad.position, xf);
            if let Some(ps) = pad.pad_stack.as_mut() {
                xform_angle(&mut ps.angle, xf);
            }
            item.value = pad.encode_to_vec();
        } else if url.ends_with("kiapi.board.types.BoardText") {
            let mut text = board::BoardText::decode(item.value.as_slice())?;
            xform_board_text(&mut text, xf);
            item.value = text.encode_to_vec();
        } else if url.ends_with("kiapi.board.types.BoardTextBox") {
            if !xf.is_cardinal() {
                anyhow::bail!(
                    "cannot rotate a footprint containing a textbox by a \
                     non-90° angle; use a multiple of 90°"
                );
            }
            let mut tb = board::BoardTextBox::decode(item.value.as_slice())?;
            if let Some(textbox) = tb.textbox.as_mut() {
                xform_vec2(&mut textbox.top_left, xf);
                xform_vec2(&mut textbox.bottom_right, xf);
                if let Some(attrs) = textbox.attributes.as_mut() {
                    xform_angle(&mut attrs.angle, xf);
                }
            }
            item.value = tb.encode_to_vec();
        } else if url.ends_with("kiapi.board.types.Field") {
            let mut field = board::Field::decode(item.value.as_slice())?;
            if let Some(text) = field.text.as_mut() {
                xform_board_text(text, xf);
            }
            item.value = field.encode_to_vec();
        } else if url.ends_with("kiapi.board.types.BoardGraphicShape") {
            let mut shape = board::BoardGraphicShape::decode(item.value.as_slice())?;
            xform_graphic_shape(&mut shape, xf)?;
            item.value = shape.encode_to_vec();
        } else if url.ends_with("kiapi.board.types.Zone") {
            let mut zone = board::Zone::decode(item.value.as_slice())?;
            xform_zone(&mut zone, xf);
            item.value = zone.encode_to_vec();
        } else if url.ends_with("kiapi.board.types.Dimension") {
            let mut dim = board::Dimension::decode(item.value.as_slice())?;
            xform_dimension(&mut dim, xf);
            item.value = dim.encode_to_vec();
        } else if url.ends_with("kiapi.board.types.Footprint3DModel")
            || url.ends_with("kiapi.board.types.Group")
        {
            // No board-space geometry of their own — nothing to shift.
        } else {
            // KiCAD would re-create this item wherever the stale message
            // says it is; refusing beats silently corrupting the board.
            anyhow::bail!(
                "footprint contains an item of unsupported type '{url}'; \
                 refusing to move/rotate it via IPC to avoid detaching it"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders;

    fn pad_at(x_mm: f64, y_mm: f64, angle_deg: f64) -> board::Pad {
        board::Pad {
            position: Some(builders::vec2(x_mm, y_mm)),
            pad_stack: Some(board::PadStack {
                angle: Some(common::Angle {
                    value_degrees: angle_deg,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn field_at(x_mm: f64, y_mm: f64) -> board::Field {
        board::Field {
            text: Some(board::BoardText {
                text: Some(common::Text {
                    position: Some(builders::vec2(x_mm, y_mm)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn instance_with(items: Vec<prost_types::Any>) -> board::FootprintInstance {
        board::FootprintInstance {
            position: Some(builders::vec2(100.0, 100.0)),
            reference_field: Some(field_at(100.0, 98.0)),
            definition: Some(board::Footprint {
                items,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn decode_pad(any: &prost_types::Any) -> board::Pad {
        board::Pad::decode(any.value.as_slice()).unwrap()
    }

    #[test]
    fn translate_shifts_pads_fields_and_graphics() {
        let pad = builders::pack_any(&pad_at(100.0, 100.0, 0.0), "kiapi.board.types.Pad");
        let seg = builders::pack_any(
            &builders::board_segment("F.SilkS", 0.1, 99.0, 99.0, 101.0, 99.0),
            "kiapi.board.types.BoardGraphicShape",
        );
        let mut fp = instance_with(vec![pad, seg]);

        // Move 100,100 -> 50,50: delta is -50mm on both axes.
        let xf = Xform::Translate {
            dx_nm: builders::mm_to_nm(-50.0),
            dy_nm: builders::mm_to_nm(-50.0),
        };
        transform_footprint_children(&mut fp, &xf).unwrap();

        let items = &fp.definition.as_ref().unwrap().items;
        let pad = decode_pad(&items[0]);
        assert_eq!(pad.position.unwrap(), builders::vec2(50.0, 50.0));

        let shape = board::BoardGraphicShape::decode(items[1].value.as_slice()).unwrap();
        match shape.shape.unwrap().geometry.unwrap() {
            common::graphic_shape::Geometry::Segment(s) => {
                assert_eq!(s.start.unwrap(), builders::vec2(49.0, 49.0));
                assert_eq!(s.end.unwrap(), builders::vec2(51.0, 49.0));
            }
            other => panic!("expected segment, got {other:?}"),
        }

        let ref_text = fp.reference_field.unwrap().text.unwrap().text.unwrap();
        assert_eq!(ref_text.position.unwrap(), builders::vec2(50.0, 48.0));
    }

    #[test]
    fn rotate_90_matches_kicad_convention() {
        // Pad 1mm to the right of the anchor, at pad angle 0.
        let pad = builders::pack_any(&pad_at(101.0, 100.0, 0.0), "kiapi.board.types.Pad");
        let mut fp = instance_with(vec![pad]);

        let xf = Xform::Rotate {
            cx_nm: builders::mm_to_nm(100.0),
            cy_nm: builders::mm_to_nm(100.0),
            delta_deg: 90.0,
        };
        transform_footprint_children(&mut fp, &xf).unwrap();

        // KiCAD positive rotation is counterclockwise on screen (Y down):
        // (+1, 0) relative -> (0, -1) relative.
        let pad = decode_pad(&fp.definition.as_ref().unwrap().items[0]);
        assert_eq!(pad.position.unwrap(), builders::vec2(100.0, 99.0));
        assert_eq!(pad.pad_stack.unwrap().angle.unwrap().value_degrees, 90.0);
    }

    #[test]
    fn rotate_angle_normalizes_to_plus_minus_180() {
        let pad = builders::pack_any(&pad_at(100.0, 100.0, 170.0), "kiapi.board.types.Pad");
        let mut fp = instance_with(vec![pad]);

        let xf = Xform::Rotate {
            cx_nm: builders::mm_to_nm(100.0),
            cy_nm: builders::mm_to_nm(100.0),
            delta_deg: 20.0,
        };
        transform_footprint_children(&mut fp, &xf).unwrap();

        let pad = decode_pad(&fp.definition.as_ref().unwrap().items[0]);
        assert_eq!(pad.pad_stack.unwrap().angle.unwrap().value_degrees, -170.0);
    }

    #[test]
    fn non_cardinal_rotation_turns_rectangle_into_polygon() {
        let rect = builders::pack_any(
            &builders::board_rectangle("F.Fab", 0.1, 99.0, 99.0, 101.0, 101.0, false),
            "kiapi.board.types.BoardGraphicShape",
        );
        let mut fp = instance_with(vec![rect]);

        let xf = Xform::Rotate {
            cx_nm: builders::mm_to_nm(100.0),
            cy_nm: builders::mm_to_nm(100.0),
            delta_deg: 45.0,
        };
        transform_footprint_children(&mut fp, &xf).unwrap();

        let shape = board::BoardGraphicShape::decode(
            fp.definition.as_ref().unwrap().items[0].value.as_slice(),
        )
        .unwrap();
        match shape.shape.unwrap().geometry.unwrap() {
            common::graphic_shape::Geometry::Polygon(ps) => {
                let outline = ps.polygons[0].outline.as_ref().unwrap();
                assert!(outline.closed);
                assert_eq!(outline.nodes.len(), 4);
            }
            other => panic!("expected polygon after 45° rotation, got {other:?}"),
        }
    }

    #[test]
    fn cardinal_rotation_keeps_rectangle_a_rectangle() {
        let rect = builders::pack_any(
            &builders::board_rectangle("F.Fab", 0.1, 99.0, 99.0, 101.0, 101.0, false),
            "kiapi.board.types.BoardGraphicShape",
        );
        let mut fp = instance_with(vec![rect]);

        let xf = Xform::Rotate {
            cx_nm: builders::mm_to_nm(100.0),
            cy_nm: builders::mm_to_nm(100.0),
            delta_deg: 180.0,
        };
        transform_footprint_children(&mut fp, &xf).unwrap();

        let shape = board::BoardGraphicShape::decode(
            fp.definition.as_ref().unwrap().items[0].value.as_slice(),
        )
        .unwrap();
        assert!(matches!(
            shape.shape.unwrap().geometry.unwrap(),
            common::graphic_shape::Geometry::Rectangle(_)
        ));
    }

    #[test]
    fn unknown_item_type_is_rejected_not_corrupted() {
        let bogus = prost_types::Any {
            type_url: "type.googleapis.com/kiapi.board.types.SomethingNew".to_string(),
            value: vec![],
        };
        let mut fp = instance_with(vec![bogus]);

        let xf = Xform::Translate {
            dx_nm: 1000,
            dy_nm: 0,
        };
        let err = transform_footprint_children(&mut fp, &xf).unwrap_err();
        assert!(err.to_string().contains("SomethingNew"));
    }

    #[test]
    fn model_and_group_items_pass_through_untouched() {
        let model = builders::pack_any(
            &board::Footprint3DModel::default(),
            "kiapi.board.types.Footprint3DModel",
        );
        let original = model.value.clone();
        let mut fp = instance_with(vec![model]);

        let xf = Xform::Translate {
            dx_nm: builders::mm_to_nm(10.0),
            dy_nm: 0,
        };
        transform_footprint_children(&mut fp, &xf).unwrap();
        assert_eq!(fp.definition.unwrap().items[0].value, original);
    }
}
