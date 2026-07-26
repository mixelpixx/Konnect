//! Pure Vello scene encoding for the native schematic renderer.

use crate::kicad_font::{draw_text as draw_kicad_text, StrokeTextRun};
use crate::native_scene::{ColorRole, Point as SchPoint, Primitive, SchematicScene};
use crate::vello_app::Palette;
use std::ops::Range;
use vello::kurbo::{Affine, Arc as KurboArc, BezPath, Cap, Join, Line, Rect, Stroke};
use vello::peniko::{Color, Fill};
use vello::Scene;

pub(crate) fn encode_scene(source: &SchematicScene, palette: Palette) -> Scene {
    encode_primitives(&source.primitives, palette)
}

/// Encode a drag base without the objects that will be redrawn at preview
/// positions. This is built once at drag start, not in the frame loop.
pub(crate) fn encode_scene_without_ranges(
    source: &SchematicScene,
    excluded: &[Range<usize>],
    palette: Palette,
) -> Scene {
    let retained = source
        .primitives
        .iter()
        .enumerate()
        .filter(|(index, _)| !excluded.iter().any(|range| range.contains(index)))
        .map(|(_, primitive)| primitive.clone())
        .collect::<Vec<_>>();
    encode_primitives(&retained, palette)
}

pub(crate) fn encode_primitives(primitives: &[Primitive], palette: Palette) -> Scene {
    let mut scene = Scene::new();
    for primitive in primitives {
        match primitive {
            Primitive::Line { from, to, style } => {
                scene.stroke(
                    &round_stroke(style.width_mm.max(0.05)),
                    Affine::IDENTITY,
                    role_color(style.role, palette),
                    None,
                    &svg_line(*from, *to),
                );
            }
            Primitive::Polyline {
                points,
                closed,
                style,
                fill,
            } => {
                let path = polyline_path(points, *closed);
                if *fill {
                    scene.fill(Fill::NonZero, Affine::IDENTITY, palette.fill, None, &path);
                }
                if style.width_mm > 0.0 {
                    scene.stroke(
                        &round_stroke(style.width_mm),
                        Affine::IDENTITY,
                        role_color(style.role, palette),
                        None,
                        &path,
                    );
                }
            }
            Primitive::Rect {
                bounds,
                style,
                fill,
            } => {
                let rect = svg_rect(*bounds);
                if *fill {
                    scene.fill(
                        Fill::NonZero,
                        Affine::IDENTITY,
                        if style.role == ColorRole::Symbol {
                            palette.fill
                        } else {
                            role_color(style.role, palette)
                        },
                        None,
                        &rect,
                    );
                }
                if style.width_mm > 0.0 {
                    scene.stroke(
                        &round_stroke(style.width_mm),
                        Affine::IDENTITY,
                        role_color(style.role, palette),
                        None,
                        &rect,
                    );
                }
            }
            Primitive::Circle {
                center,
                radius,
                style,
                fill,
            } => {
                let circle = svg_circle_path(*center, *radius);
                if *fill {
                    scene.fill(
                        Fill::NonZero,
                        Affine::IDENTITY,
                        if style.role == ColorRole::Junction {
                            palette.junction
                        } else {
                            palette.fill
                        },
                        None,
                        &circle,
                    );
                }
                if style.width_mm > 0.0 {
                    scene.stroke(
                        &round_stroke(style.width_mm),
                        Affine::IDENTITY,
                        role_color(style.role, palette),
                        None,
                        &circle,
                    );
                }
            }
            Primitive::Arc {
                start,
                mid,
                end,
                style,
            } => {
                if let Some(arc) = svg_arc_path(*start, *mid, *end) {
                    scene.stroke(
                        &round_stroke(style.width_mm.max(0.05)),
                        Affine::IDENTITY,
                        role_color(style.role, palette),
                        None,
                        &arc,
                    );
                }
            }
            Primitive::Bezier { points, style } => {
                let mut path = BezPath::new();
                if let Some(first) = points.first() {
                    path.move_to(svg_point(*first));
                    for controls in points[1..].chunks_exact(3) {
                        path.curve_to(
                            svg_point(controls[0]),
                            svg_point(controls[1]),
                            svg_point(controls[2]),
                        );
                    }
                    scene.stroke(
                        &round_stroke(style.width_mm.max(0.05)),
                        Affine::IDENTITY,
                        role_color(style.role, palette),
                        None,
                        &path,
                    );
                }
            }
            Primitive::Text {
                position,
                rotation_deg,
                size_mm,
                stroke_width_mm,
                align,
                italic,
                role,
                text,
            } => draw_kicad_text(
                &mut scene,
                text,
                StrokeTextRun {
                    size_mm: *size_mm,
                    stroke_width_mm: *stroke_width_mm,
                    position: (position.x, position.y),
                    rotation_deg: *rotation_deg,
                    align: *align,
                    italic: *italic,
                    coordinate_quantum: 0.0001,
                    coordinate_epsilon: 1e-7,
                    color: role_color(*role, palette),
                },
            ),
        }
    }
    scene
}

pub(crate) fn svg_arc_path(start: SchPoint, mid: SchPoint, end: SchPoint) -> Option<BezPath> {
    let arc = kicad_svg_arc(start, mid, end)?;
    let mut path = BezPath::new();
    path.move_to(svg_f32_point(arc.from));
    KurboArc::from_svg_arc(&arc)?.to_cubic_beziers(0.1, |first, second, end| {
        path.curve_to(
            svg_f32_point(first),
            svg_f32_point(second),
            svg_f32_point(end),
        );
    });
    Some(path)
}

pub(crate) fn kicad_svg_arc(
    start: SchPoint,
    mid: SchPoint,
    end: SchPoint,
) -> Option<vello::kurbo::SvgArc> {
    let scale = 10_000.0;
    let mut start = SchPoint {
        x: (start.x * scale).round(),
        y: (start.y * scale).round(),
    };
    let input_mid = SchPoint {
        x: (mid.x * scale).round(),
        y: (mid.y * scale).round(),
    };
    let mut end = SchPoint {
        x: (end.x * scale).round(),
        y: (end.y * scale).round(),
    };
    let initial_center = kicad_arc_center_iu(start, input_mid, end);
    let center = SchPoint {
        x: initial_center.x.round(),
        y: initial_center.y.round(),
    };
    let initial_mid = rotate_iu(
        start,
        center,
        -increasing_arc_angle(start, end, center) / 2.0,
    );
    let separation = (initial_mid.x - input_mid.x).powi(2) + (initial_mid.y - input_mid.y).powi(2);
    let radius_sq = (initial_mid.x - center.x).powi(2) + (initial_mid.y - center.y).powi(2);
    let plot_mid = if separation > radius_sq {
        std::mem::swap(&mut start, &mut end);
        let recalculated = rotate_iu(
            start,
            center,
            -increasing_arc_angle(start, end, center) / 2.0,
        );
        // KiCad's plotted winding-correction result retains the transverse
        // integer coordinate calculated before the endpoint swap.  Preserving
        // it here is required to reproduce the four-decimal SVG arc emitted by
        // KiCad 10 (and is locked down by the inductor regression below).
        SchPoint {
            x: initial_mid.x,
            y: recalculated.y,
        }
    } else {
        input_mid
    };

    plotter_svg_arc(start, plot_mid, end, scale)
}

fn plotter_svg_arc(
    start: SchPoint,
    mid: SchPoint,
    end: SchPoint,
    scale: f64,
) -> Option<vello::kurbo::SvgArc> {
    let plot_center = kicad_arc_center_iu(start, mid, end);
    let radius = (start.x - plot_center.x).hypot(start.y - plot_center.y);
    if !radius.is_finite() || radius <= 0.0 {
        return None;
    }
    let start_angle = (start.y - plot_center.y).atan2(start.x - plot_center.x);
    let end_angle = (end.y - plot_center.y).atan2(end.x - plot_center.x);
    let determinant = (end.x - start.x) * (mid.y - start.y) - (end.y - start.y) * (mid.x - start.x);
    let sweep = if determinant <= 0.0 {
        (end_angle - start_angle).rem_euclid(std::f64::consts::TAU)
    } else {
        -((start_angle - end_angle).rem_euclid(std::f64::consts::TAU))
    };
    let mut svg_start_angle = -start_angle;
    let mut svg_end_angle = svg_start_angle - sweep;
    if svg_end_angle < svg_start_angle {
        std::mem::swap(&mut svg_start_angle, &mut svg_end_angle);
    }
    let from = svg_arc_endpoint(plot_center, radius, svg_start_angle, scale);
    let to = svg_arc_endpoint(plot_center, radius, svg_end_angle, scale);
    let radius_mm = svg_decimal(radius / scale);
    Some(vello::kurbo::SvgArc {
        from,
        to,
        radii: vello::kurbo::Vec2::new(radius_mm, radius_mm),
        x_rotation: 0.0,
        large_arc: (svg_end_angle - svg_start_angle).abs() > std::f64::consts::PI,
        sweep: false,
    })
}

fn increasing_arc_angle(start: SchPoint, end: SchPoint, center: SchPoint) -> f64 {
    let start = (start.y - center.y).atan2(start.x - center.x);
    let mut end = (end.y - center.y).atan2(end.x - center.x);
    while end < start {
        end += std::f64::consts::TAU;
    }
    if end == start {
        std::f64::consts::TAU
    } else {
        end - start
    }
}

fn rotate_iu(point: SchPoint, center: SchPoint, angle: f64) -> SchPoint {
    let x = point.x - center.x;
    let y = point.y - center.y;
    SchPoint {
        x: (y * angle.sin() + x * angle.cos()).round() + center.x,
        y: (y * angle.cos() - x * angle.sin()).round() + center.y,
    }
}

fn svg_arc_endpoint(center: SchPoint, radius: f64, angle: f64, scale: f64) -> vello::kurbo::Point {
    vello::kurbo::Point::new(
        svg_decimal((center.x + radius * angle.cos()) / scale),
        svg_decimal((center.y - radius * angle.sin()) / scale),
    )
}

fn svg_decimal(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn svg_f32(value: f64) -> f64 {
    f64::from(svg_decimal(value) as f32)
}

fn svg_f32_point(point: vello::kurbo::Point) -> vello::kurbo::Point {
    vello::kurbo::Point::new(f64::from(point.x as f32), f64::from(point.y as f32))
}

fn svg_circle_path(center: SchPoint, radius: f64) -> BezPath {
    // usvg parses KiCad's SVG circle coordinates as f32, converts each
    // quarter-circle SVG arc through kurbo, then stores the resulting cubic
    // controls as f32. Reproduce that pipeline so the native semantic scene
    // and the golden SVG scene feed byte-equivalent geometry to Vello.
    let center_x = svg_f32(center.x) as f32;
    let center_y = svg_f32(center.y) as f32;
    let radius = svg_f32(radius) as f32;
    let point = |x: f32, y: f32| vello::kurbo::Point::new(f64::from(x), f64::from(y));
    let endpoints = [
        point(center_x, center_y + radius),
        point(center_x - radius, center_y),
        point(center_x, center_y - radius),
        point(center_x + radius, center_y),
    ];
    let mut from = point(center_x + radius, center_y);
    let mut path = BezPath::new();
    path.move_to(from);
    for to in endpoints {
        let arc = vello::kurbo::SvgArc {
            from,
            to,
            radii: vello::kurbo::Vec2::new(f64::from(radius), f64::from(radius)),
            x_rotation: 0.0,
            large_arc: false,
            sweep: true,
        };
        if let Some(arc) = KurboArc::from_svg_arc(&arc) {
            arc.to_cubic_beziers(0.1, |first, second, end| {
                path.curve_to(
                    (f64::from(first.x as f32), f64::from(first.y as f32)),
                    (f64::from(second.x as f32), f64::from(second.y as f32)),
                    (f64::from(end.x as f32), f64::from(end.y as f32)),
                );
            });
        } else {
            path.line_to(to);
        }
        from = to;
    }
    path.close_path();
    path
}

fn svg_rect(bounds: crate::native_scene::Bounds) -> Rect {
    // usvg parses x/y/width/height independently as f32 and constructs the
    // far corner by adding width/height in f32. A direct f64 endpoint cast can
    // land on the opposite side of Vello's fixed-point coverage boundary.
    let x = bounds.min_x as f32;
    let y = bounds.min_y as f32;
    let width = (bounds.max_x - bounds.min_x) as f32;
    let height = (bounds.max_y - bounds.min_y) as f32;
    Rect::new(
        f64::from(x),
        f64::from(y),
        f64::from(x + width),
        f64::from(y + height),
    )
}

fn role_color(role: ColorRole, palette: Palette) -> Color {
    match role {
        ColorRole::Border => palette.border,
        ColorRole::Bus => palette.bus,
        ColorRole::GraphicText => palette.bus,
        ColorRole::Junction => palette.junction,
        ColorRole::Label => palette.label,
        ColorRole::NoConnect => palette.no_connect,
        ColorRole::Page => palette.page,
        ColorRole::Pin => palette.pin,
        ColorRole::PinName => palette.text,
        ColorRole::PinNumber => palette.pin,
        ColorRole::SheetFile => palette.sheet_file,
        ColorRole::Symbol => palette.symbol,
        ColorRole::Text => palette.text,
        ColorRole::Wire => palette.wire,
    }
}

pub(crate) fn round_stroke(width: f64) -> Stroke {
    Stroke {
        width: f64::from(width as f32),
        join: Join::Round,
        start_cap: Cap::Round,
        end_cap: Cap::Round,
        ..Stroke::default()
    }
}

pub(crate) fn polyline_path(points: &[SchPoint], closed: bool) -> BezPath {
    let mut path = BezPath::new();
    if let Some(first) = points.first() {
        path.move_to(svg_point(*first));
        for point in &points[1..] {
            path.line_to(svg_point(*point));
        }
        if closed {
            path.close_path();
        }
    }
    path
}

fn svg_point(point: SchPoint) -> vello::kurbo::Point {
    vello::kurbo::Point::new(svg_f32(point.x), svg_f32(point.y))
}

fn svg_line(from: SchPoint, to: SchPoint) -> Line {
    Line::new(svg_point(from), svg_point(to))
}

fn kicad_arc_center_iu(start: SchPoint, mid: SchPoint, end: SchPoint) -> SchPoint {
    // KiCad calculates library arcs in 0.1 µm internal units. Its center
    // algorithm carries the uncertainty introduced by integer input and snaps
    // a statistically equivalent center to a 1 mil/100 nm-friendly grid. A
    // conventional determinant circumcenter produces visibly different SVG
    // radii for common hand-authored inductor arcs.
    let y_delta_21 = mid.y - start.y;
    let mut x_delta_21 = mid.x - start.x;
    let y_delta_32 = end.y - mid.y;
    let mut x_delta_32 = end.x - mid.x;

    if (x_delta_21 == 0.0 && y_delta_32 == 0.0) || (y_delta_21 == 0.0 && x_delta_32 == 0.0) {
        return SchPoint {
            x: (start.x + end.x) / 2.0,
            y: (start.y + end.y) / 2.0,
        };
    }
    if x_delta_21 == 0.0 {
        x_delta_21 = f64::EPSILON;
    }
    if x_delta_32 == 0.0 {
        x_delta_32 = -f64::EPSILON;
    }

    let mut a_slope = y_delta_21 / x_delta_21;
    let mut b_slope = y_delta_32 / x_delta_32;
    let da_slope = a_slope * (0.5 / y_delta_21).hypot(0.5 / x_delta_21);
    let db_slope = b_slope * (0.5 / y_delta_32).hypot(0.5 / x_delta_32);
    if a_slope == b_slope {
        if start == end {
            return SchPoint {
                x: (start.x + mid.x) / 2.0,
                y: (start.y + mid.y) / 2.0,
            };
        }
        a_slope += f64::EPSILON;
        b_slope -= f64::EPSILON;
    }
    if a_slope == 0.0 {
        a_slope = 1e-10;
    }
    if b_slope == 0.0 {
        b_slope = 1e-10;
    }

    let ab_slope_start_end_y = a_slope * b_slope * (start.y - end.y);
    let d_ab_slope_start_end_y = ab_slope_start_end_y
        * ((da_slope / a_slope).powi(2)
            + (db_slope / b_slope).powi(2)
            + (std::f64::consts::FRAC_1_SQRT_2 / (start.y - end.y)).powi(2))
        .sqrt();
    let b_slope_start_mid_x = b_slope * (start.x + mid.x);
    let d_b_slope_start_mid_x = b_slope_start_mid_x
        * ((db_slope / b_slope).powi(2)
            + (std::f64::consts::FRAC_1_SQRT_2 / (start.x + mid.x)).powi(2))
        .sqrt();
    let a_slope_mid_end_x = a_slope * (mid.x + end.x);
    let d_a_slope_mid_end_x = a_slope_mid_end_x
        * ((da_slope / a_slope).powi(2)
            + (std::f64::consts::FRAC_1_SQRT_2 / (mid.x + end.x)).powi(2))
        .sqrt();
    let twice_ba_slope_diff = 2.0 * (b_slope - a_slope);
    let d_twice_ba_slope_diff = 2.0 * db_slope.hypot(da_slope);
    let center_numerator_x = ab_slope_start_end_y + b_slope_start_mid_x - a_slope_mid_end_x;
    let d_center_numerator_x = d_ab_slope_start_end_y
        .hypot(d_b_slope_start_mid_x)
        .hypot(d_a_slope_mid_end_x);
    let center_x = center_numerator_x / twice_ba_slope_diff;
    let d_center_x = center_x
        * ((d_center_numerator_x / center_numerator_x).powi(2)
            + (d_twice_ba_slope_diff / twice_ba_slope_diff).powi(2))
        .sqrt();
    let center_numerator_y = (start.x + mid.x) / 2.0 - center_x;
    let d_center_numerator_y = (0.125 + d_center_x.powi(2)).sqrt();
    let center_first_term = center_numerator_y / a_slope;
    let d_center_first_term_y = center_first_term
        * ((d_center_numerator_y / center_numerator_y).powi(2) + (da_slope / a_slope).powi(2))
            .sqrt();
    let center_y = center_first_term + (start.y + mid.y) / 2.0;
    let d_center_y = (d_center_first_term_y.powi(2) + 0.125).sqrt();

    let rounded_100_x = ((center_x + 50.0) / 100.0).floor() * 100.0;
    let rounded_100_y = ((center_y + 50.0) / 100.0).floor() * 100.0;
    let rounded_10_x = ((center_x + 5.0) / 10.0).floor() * 10.0;
    let rounded_10_y = ((center_y + 5.0) / 10.0).floor() * 10.0;
    let (center_x, center_y) = if (rounded_100_x - center_x).abs() < d_center_x
        && (rounded_100_y - center_y).abs() < d_center_y
    {
        (rounded_100_x, rounded_100_y)
    } else if (rounded_10_x - center_x).abs() < d_center_x
        && (rounded_10_y - center_y).abs() < d_center_y
    {
        (rounded_10_x, rounded_10_y)
    } else {
        (center_x, center_y)
    };
    SchPoint {
        x: center_x,
        y: center_y,
    }
}
