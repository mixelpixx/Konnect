use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{ToolContext, ToolDef};
use konnect_schematic_editor::types::fmt_f64;
use konnect_sexp::writer::{
    apply_edits, find_balanced_block, find_block_with_leading_whitespace, find_direct_child_blocks,
    read_consistent, write_atomic_if_unchanged, SexpEdit,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub(super) fn point_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "x": { "type": "number" },
            "y": { "type": "number" }
        },
        "required": ["x", "y"],
        "additionalProperties": false
    })
}

/// Shared shape properties, with the fill vocabulary of the calling domain.
/// Footprints fill or they do not; a symbol also has KiCad's pale `background`
/// body fill, so the geometry is shared and only this list differs.
pub(super) fn common_shape_properties_with_fill(
    fill_values: &[&str],
) -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        (
            "stroke_width_mm".to_string(),
            json!({
                "type": "number",
                "minimum": 0,
                "description": "Stroke width in millimetres"
            }),
        ),
        (
            "fill".to_string(),
            json!({
                "type": "string",
                "enum": fill_values
            }),
        ),
    ])
}

pub(super) fn primitive_schema(
    primitive_type: &str,
    mut properties: serde_json::Map<String, serde_json::Value>,
    required: &[&str],
) -> serde_json::Value {
    properties.insert("type".to_string(), json!({ "const": primitive_type }));
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn graphics_schema() -> serde_json::Value {
    graphics_schema_with_fill(&["none", "solid"])
}

/// The primitive vocabulary, shared with `create_symbol` so the two domains
/// cannot drift apart. Only the fill values differ.
pub(super) fn graphics_schema_with_fill(fill_values: &[&str]) -> serde_json::Value {
    let mut line = serde_json::Map::new();
    line.insert("start".to_string(), point_schema());
    line.insert("end".to_string(), point_schema());
    line.insert(
        "stroke_width_mm".to_string(),
        common_shape_properties_with_fill(fill_values)["stroke_width_mm"].clone(),
    );

    let mut arc = line.clone();
    arc.insert("mid".to_string(), point_schema());

    let mut rect = common_shape_properties_with_fill(fill_values);
    rect.insert("start".to_string(), point_schema());
    rect.insert("end".to_string(), point_schema());

    let mut circle = common_shape_properties_with_fill(fill_values);
    circle.insert("center".to_string(), point_schema());
    circle.insert(
        "radius_mm".to_string(),
        json!({
            "type": "number",
            "exclusiveMinimum": 0,
            "description": "Circle radius in millimetres"
        }),
    );

    let mut poly = common_shape_properties_with_fill(fill_values);
    poly.insert(
        "points".to_string(),
        json!({
            "type": "array",
            "minItems": 3,
            "items": point_schema()
        }),
    );

    json!({
        "type": "array",
        "items": {
            "oneOf": [
                primitive_schema(
                    "line",
                    line,
                    &["type", "start", "end", "stroke_width_mm"]
                ),
                primitive_schema(
                    "arc",
                    arc,
                    &["type", "start", "mid", "end", "stroke_width_mm"]
                ),
                primitive_schema(
                    "rect",
                    rect,
                    &["type", "start", "end", "stroke_width_mm", "fill"]
                ),
                primitive_schema(
                    "circle",
                    circle,
                    &["type", "center", "radius_mm", "stroke_width_mm", "fill"]
                ),
                primitive_schema(
                    "poly",
                    poly,
                    &["type", "points", "stroke_width_mm", "fill"]
                )
            ]
        }
    })
}

pub(super) fn tool() -> ToolDef {
    tool!(
        "set_footprint_graphics",
        "Atomically append, replace, or delete graphical primitives on one layer of an existing \
         .kicad_mod footprint. Replacement and deletion refuse graphics referenced by a group.",
        json!({
            "type": "object",
            "properties": {
                "footprint_path": {
                    "type": "string",
                    "description": "Path to an existing .kicad_mod footprint file"
                },
                "selector": {
                    "type": "object",
                    "properties": {
                        "layer": {
                            "type": "string",
                            "description": "Canonical KiCad layer name, such as B.CrtYd"
                        }
                    },
                    "required": ["layer"],
                    "additionalProperties": false
                },
                "mode": {
                    "type": "string",
                    "enum": ["append", "replace", "delete"]
                },
                "graphics": graphics_schema()
            },
            "required": ["footprint_path", "selector", "mode"],
            "additionalProperties": false
        }),
        |args, ctx| async move { handle_set_footprint_graphics(args, ctx).await }
    )
}

async fn handle_set_footprint_graphics(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let Some(path) = args["footprint_path"]
        .as_str()
        .map(std::path::PathBuf::from)
    else {
        return Ok(invalid_result("footprint_path", "missing or not a string"));
    };
    if path.extension().and_then(|extension| extension.to_str()) != Some("kicad_mod") {
        return Ok(invalid_result("footprint_path", "must end in .kicad_mod"));
    }
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::FileNotFound {
                    path: path.display().to_string(),
                },
                format!("Footprint file not found: {}", path.display()),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Ok(invalid_result("footprint_path", "must name a regular file"));
    }

    let Some(layer) = args["selector"]["layer"].as_str() else {
        return Ok(invalid_result("selector.layer", "missing or not a string"));
    };
    let mode: GraphicsMode = match serde_json::from_value(args["mode"].clone()) {
        Ok(mode) => mode,
        Err(error) => return Ok(invalid_result("mode", error.to_string())),
    };
    let graphics = if matches!(mode, GraphicsMode::Delete) {
        match args.get("graphics") {
            Some(value) if value.as_array().is_some_and(Vec::is_empty) => Vec::new(),
            Some(_) => {
                return Ok(invalid_result(
                    "graphics",
                    "must be omitted or empty in delete mode",
                ));
            }
            None => Vec::new(),
        }
    } else {
        let Some(value) = args.get("graphics") else {
            return Ok(invalid_result(
                "graphics",
                "is required for append and replace modes",
            ));
        };
        match parse_graphics(value) {
            Ok(graphics) if !graphics.is_empty() => graphics,
            Ok(_) => {
                return Ok(invalid_result(
                    "graphics",
                    "must contain at least one primitive",
                ));
            }
            Err(reason) => return Ok(invalid_result("graphics", reason)),
        }
    };

    let source = match read_consistent(&path) {
        Ok(source) => source,
        Err(konnect_sexp::SexpError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::FileNotFound {
                    path: path.display().to_string(),
                },
                format!("Footprint file not found: {}", path.display()),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let prepared = match prepare_mutation(&source, layer, mode, &graphics) {
        Ok(prepared) => prepared,
        Err(FootprintGraphicsError::InvalidArgument { field, reason }) => {
            return Ok(invalid_result(&field, reason));
        }
        Err(FootprintGraphicsError::Conflict(reason)) => {
            return Ok(conflict_result(&path, reason));
        }
    };
    if let Err(error) = persist_replacement(&path, &source, &prepared.replacement) {
        return match error {
            konnect_sexp::SexpError::Conflict { .. } => Ok(conflict_result(
                &path,
                "footprint changed after it was read",
            )),
            other => Err(other.into()),
        };
    }

    Ok(CallToolResult::json(&json!({
        "success": true,
        "footprint_path": path.display().to_string(),
        "layer": layer,
        "mode": args["mode"],
        "matched_count": prepared.matched_count,
        "added_count": prepared.added_count
    })))
}

fn invalid_result(field: &str, reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.clone(),
        },
        format!("Argument '{field}' is invalid: {reason}"),
    )
}

fn conflict_result(path: &std::path::Path, reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        ToolErrorKind::Conflict {
            paths: vec![path.display().to_string()],
        },
        format!("Footprint graphics were not changed: {reason}"),
    )
}

fn persist_replacement(
    path: &std::path::Path,
    expected: &str,
    replacement: &str,
) -> Result<(), konnect_sexp::SexpError> {
    write_atomic_if_unchanged(path, expected, replacement)
}

fn parse_point(node: &konnect_sexp::SexpNode, tag: &str) -> Result<Point, FootprintGraphicsError> {
    let point = node
        .find(tag)
        .ok_or_else(|| invalid("footprint_path", format!("{tag} is missing")))?;
    let x = point
        .get_f64(1)
        .ok_or_else(|| invalid("footprint_path", format!("{tag}.x is invalid")))?;
    let y = point
        .get_f64(2)
        .ok_or_else(|| invalid("footprint_path", format!("{tag}.y is invalid")))?;
    let point = Point { x, y };
    point
        .validate(tag)
        .map_err(|reason| invalid("footprint_path", reason))?;
    Ok(point)
}

fn parse_stroke_width(node: &konnect_sexp::SexpNode) -> Result<f64, FootprintGraphicsError> {
    let width = node
        .find("stroke")
        .and_then(|stroke| stroke.find_f64("width"))
        .ok_or_else(|| {
            invalid(
                "footprint_path",
                "graphic stroke width is missing or invalid",
            )
        })?;
    if !width.is_finite() || width < 0.0 {
        return Err(invalid(
            "footprint_path",
            "graphic stroke width must be finite and non-negative",
        ));
    }
    Ok(width)
}

fn parse_fill(node: &konnect_sexp::SexpNode) -> Result<Fill, FootprintGraphicsError> {
    match node.find("fill").and_then(|fill| fill.get(1)) {
        Some(fill) if matches!(fill.as_str(), Some("none" | "no")) => Ok(Fill::None),
        Some(fill) if matches!(fill.as_str(), Some("solid" | "yes")) => Ok(Fill::Solid),
        _ => Err(invalid(
            "footprint_path",
            "graphic fill is missing or unsupported",
        )),
    }
}

fn graphic_info_base(
    graphic_type: &str,
    node: &konnect_sexp::SexpNode,
) -> Result<FootprintGraphicInfo, FootprintGraphicsError> {
    let layer = node
        .find_str("layer")
        .ok_or_else(|| invalid("footprint_path", "graphic layer is missing"))?
        .to_string();
    let item_id = node
        .find_str("uuid")
        .or_else(|| node.find_str("tstamp"))
        .map(str::to_string);
    Ok(FootprintGraphicInfo {
        graphic_type: graphic_type.to_string(),
        layer,
        start: None,
        mid: None,
        end: None,
        center: None,
        radius_mm: None,
        points: None,
        point_count: None,
        closed: None,
        stroke_width_mm: parse_stroke_width(node)?,
        fill: None,
        item_id,
    })
}

fn inspect_graphic(
    node: &konnect_sexp::SexpNode,
) -> Result<FootprintGraphicInfo, FootprintGraphicsError> {
    let tag = node
        .head()
        .ok_or_else(|| invalid("footprint_path", "graphic type is missing"))?;
    let graphic_type = tag
        .strip_prefix("fp_")
        .ok_or_else(|| invalid("footprint_path", "unsupported graphic type"))?;
    let mut info = graphic_info_base(graphic_type, node)?;
    match tag {
        "fp_line" => {
            info.start = Some(parse_point(node, "start")?);
            info.end = Some(parse_point(node, "end")?);
        }
        "fp_arc" => {
            info.start = Some(parse_point(node, "start")?);
            info.mid = Some(parse_point(node, "mid")?);
            info.end = Some(parse_point(node, "end")?);
        }
        "fp_rect" => {
            info.start = Some(parse_point(node, "start")?);
            info.end = Some(parse_point(node, "end")?);
            info.fill = Some(parse_fill(node)?);
        }
        "fp_circle" => {
            let center = parse_point(node, "center")?;
            let end = parse_point(node, "end")?;
            info.center = Some(center);
            info.radius_mm = Some((end.x - center.x).hypot(end.y - center.y));
            info.fill = Some(parse_fill(node)?);
        }
        "fp_poly" => {
            let points = node
                .find("pts")
                .ok_or_else(|| invalid("footprint_path", "polygon points are missing"))?
                .find_all("xy")
                .into_iter()
                .map(|point| {
                    let x = point
                        .get_f64(1)
                        .ok_or_else(|| invalid("footprint_path", "polygon point x is invalid"))?;
                    let y = point
                        .get_f64(2)
                        .ok_or_else(|| invalid("footprint_path", "polygon point y is invalid"))?;
                    let point = Point { x, y };
                    point
                        .validate("polygon point")
                        .map_err(|reason| invalid("footprint_path", reason))?;
                    Ok(point)
                })
                .collect::<Result<Vec<_>, FootprintGraphicsError>>()?;
            info.point_count = Some(points.len());
            info.closed = Some(true);
            info.points = Some(points);
            info.fill = Some(parse_fill(node)?);
        }
        _ => return Err(invalid("footprint_path", "unsupported graphic type")),
    }
    Ok(info)
}

pub(super) fn inspect_graphics(
    source: &str,
    layer_filter: Option<&str>,
) -> Result<Vec<FootprintGraphicInfo>, FootprintGraphicsError> {
    if let Some(layer) = layer_filter {
        if !konnect_sexp::layers::is_canonical_name(layer) {
            return Err(invalid("graphics_layer", "not a canonical KiCad layer"));
        }
    }
    let tree = konnect_sexp::parse_sexp(source)
        .map_err(|error| invalid("footprint_path", format!("invalid S-expression: {error}")))?;
    if tree.head() != Some("footprint") {
        return Err(invalid(
            "footprint_path",
            crate::tools::footprint_root_reason(tree.head()),
        ));
    }

    let mut graphics = Vec::new();
    for (start, end) in find_direct_child_blocks(source, "footprint") {
        let block = konnect_sexp::parse_sexp(&source[start..end]).map_err(|error| {
            invalid("footprint_path", format!("invalid top-level item: {error}"))
        })?;
        let Some(tag) = block.head() else {
            continue;
        };
        if !is_supported_graphic(tag) {
            continue;
        }
        let info = inspect_graphic(&block)?;
        if layer_filter.is_none_or(|layer| info.layer == layer) {
            graphics.push(info);
        }
    }
    Ok(graphics)
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Fill {
    None,
    Solid,
}

impl Fill {
    fn as_kicad(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Solid => "solid",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
enum FootprintGraphic {
    Line {
        start: Point,
        end: Point,
        stroke_width_mm: f64,
    },
    Arc {
        start: Point,
        mid: Point,
        end: Point,
        stroke_width_mm: f64,
    },
    Rect {
        start: Point,
        end: Point,
        stroke_width_mm: f64,
        fill: Fill,
    },
    Circle {
        center: Point,
        radius_mm: f64,
        stroke_width_mm: f64,
        fill: Fill,
    },
    Poly {
        points: Vec<Point>,
        stroke_width_mm: f64,
        fill: Fill,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum GraphicsMode {
    Append,
    Replace,
    Delete,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum FootprintGraphicsError {
    #[error("invalid {field}: {reason}")]
    InvalidArgument { field: String, reason: String },
    #[error("{0}")]
    Conflict(String),
}

#[derive(Debug)]
struct PreparedMutation {
    replacement: String,
    matched_count: usize,
    added_count: usize,
}

#[derive(Debug, Serialize)]
pub(super) struct FootprintGraphicInfo {
    #[serde(rename = "type")]
    graphic_type: String,
    pub(super) layer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mid: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    center: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    radius_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    points: Option<Vec<Point>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    point_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed: Option<bool>,
    stroke_width_mm: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    fill: Option<Fill>,
    item_id: Option<String>,
}

fn parse_graphics(value: &serde_json::Value) -> Result<Vec<FootprintGraphic>, String> {
    let mut graphics: Vec<FootprintGraphic> =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    for graphic in &mut graphics {
        graphic.validate_and_normalize()?;
    }
    Ok(graphics)
}

impl Point {
    fn validate(self, field: &str) -> Result<(), String> {
        if self.x.is_finite() && self.y.is_finite() {
            Ok(())
        } else {
            Err(format!("{field} coordinates must be finite"))
        }
    }
}

fn validate_stroke(width_mm: f64, fill: Option<&Fill>) -> Result<(), String> {
    if !width_mm.is_finite() || width_mm < 0.0 {
        return Err("stroke_width_mm must be finite and non-negative".to_string());
    }
    if width_mm == 0.0 && !matches!(fill, Some(Fill::Solid)) {
        return Err("an unfilled graphic requires a positive stroke_width_mm".to_string());
    }
    Ok(())
}

fn points_differ(a: Point, b: Point) -> bool {
    a.x != b.x || a.y != b.y
}

impl FootprintGraphic {
    fn validate_and_normalize(&mut self) -> Result<(), String> {
        match self {
            Self::Line {
                start,
                end,
                stroke_width_mm,
            } => {
                start.validate("start")?;
                end.validate("end")?;
                validate_stroke(*stroke_width_mm, None)?;
                if !points_differ(*start, *end) {
                    return Err("line start and end must differ".to_string());
                }
            }
            Self::Arc {
                start,
                mid,
                end,
                stroke_width_mm,
            } => {
                start.validate("start")?;
                mid.validate("mid")?;
                end.validate("end")?;
                validate_stroke(*stroke_width_mm, None)?;
                if !points_differ(*start, *mid)
                    || !points_differ(*mid, *end)
                    || !points_differ(*start, *end)
                {
                    return Err("arc start, mid, and end must be distinct".to_string());
                }
                let twice_area =
                    (mid.x - start.x) * (end.y - start.y) - (mid.y - start.y) * (end.x - start.x);
                if twice_area.abs() <= f64::EPSILON {
                    return Err("arc start, mid, and end must not be collinear".to_string());
                }
            }
            Self::Rect {
                start,
                end,
                stroke_width_mm,
                fill,
            } => {
                start.validate("start")?;
                end.validate("end")?;
                validate_stroke(*stroke_width_mm, Some(fill))?;
                if start.x == end.x || start.y == end.y {
                    return Err("rectangle width and height must be positive".to_string());
                }
            }
            Self::Circle {
                center,
                radius_mm,
                stroke_width_mm,
                fill,
            } => {
                center.validate("center")?;
                validate_stroke(*stroke_width_mm, Some(fill))?;
                if !radius_mm.is_finite() || *radius_mm <= 0.0 {
                    return Err("radius_mm must be finite and positive".to_string());
                }
                if !(center.x + *radius_mm).is_finite() {
                    return Err("circle end point must remain finite".to_string());
                }
            }
            Self::Poly {
                points,
                stroke_width_mm,
                fill,
            } => {
                validate_stroke(*stroke_width_mm, Some(fill))?;
                for (index, point) in points.iter().copied().enumerate() {
                    point.validate(&format!("points[{index}]"))?;
                }
                if points.len() > 1 && points.first() == points.last() {
                    points.pop();
                }
                if points.len() < 3 {
                    return Err("polygon requires at least three points".to_string());
                }
                let mut distinct = Vec::new();
                for point in points.iter().copied() {
                    if !distinct.contains(&point) {
                        distinct.push(point);
                    }
                }
                if distinct.len() < 3 {
                    return Err("polygon requires at least three distinct points".to_string());
                }
            }
        }
        Ok(())
    }
}

fn point_clause(tag: &str, point: Point) -> String {
    format!("({tag} {} {})", fmt_f64(point.x), fmt_f64(point.y))
}

fn stroke_clause(width_mm: f64) -> String {
    format!("(stroke (width {}) (type solid))", fmt_f64(width_mm))
}

fn serialize_graphics(layer: &str, graphics: &[FootprintGraphic], indent: &str) -> String {
    let mut output = String::new();
    for graphic in graphics {
        output.push('\n');
        output.push_str(indent);
        match graphic {
            FootprintGraphic::Line {
                start,
                end,
                stroke_width_mm,
            } => output.push_str(&format!(
                "(fp_line {} {} {} (layer \"{}\"))",
                point_clause("start", *start),
                point_clause("end", *end),
                stroke_clause(*stroke_width_mm),
                layer
            )),
            FootprintGraphic::Arc {
                start,
                mid,
                end,
                stroke_width_mm,
            } => output.push_str(&format!(
                "(fp_arc {} {} {} {} (layer \"{}\"))",
                point_clause("start", *start),
                point_clause("mid", *mid),
                point_clause("end", *end),
                stroke_clause(*stroke_width_mm),
                layer
            )),
            FootprintGraphic::Rect {
                start,
                end,
                stroke_width_mm,
                fill,
            } => output.push_str(&format!(
                "(fp_rect {} {} {} (fill {}) (layer \"{}\"))",
                point_clause("start", *start),
                point_clause("end", *end),
                stroke_clause(*stroke_width_mm),
                fill.as_kicad(),
                layer
            )),
            FootprintGraphic::Circle {
                center,
                radius_mm,
                stroke_width_mm,
                fill,
            } => {
                let end = Point {
                    x: center.x + radius_mm,
                    y: center.y,
                };
                output.push_str(&format!(
                    "(fp_circle {} {} {} (fill {}) (layer \"{}\"))",
                    point_clause("center", *center),
                    point_clause("end", end),
                    stroke_clause(*stroke_width_mm),
                    fill.as_kicad(),
                    layer
                ));
            }
            FootprintGraphic::Poly {
                points,
                stroke_width_mm,
                fill,
            } => {
                let points = points
                    .iter()
                    .map(|point| point_clause("xy", *point))
                    .collect::<Vec<_>>()
                    .join(" ");
                output.push_str(&format!(
                    "(fp_poly (pts {points}) {} (fill {}) (layer \"{}\"))",
                    stroke_clause(*stroke_width_mm),
                    fill.as_kicad(),
                    layer
                ));
            }
        }
    }
    output
}

fn invalid(field: &str, reason: impl Into<String>) -> FootprintGraphicsError {
    FootprintGraphicsError::InvalidArgument {
        field: field.to_string(),
        reason: reason.into(),
    }
}

fn is_supported_graphic(tag: &str) -> bool {
    matches!(
        tag,
        "fp_line" | "fp_arc" | "fp_rect" | "fp_circle" | "fp_poly"
    )
}

fn child_indent(source: &str, start: usize) -> String {
    let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
    let indent = &source[line_start..start];
    if !indent.is_empty()
        && indent
            .chars()
            .all(|character| matches!(character, ' ' | '\t'))
    {
        indent.to_string()
    } else {
        "  ".to_string()
    }
}

fn prepare_mutation(
    source: &str,
    layer: &str,
    mode: GraphicsMode,
    graphics: &[FootprintGraphic],
) -> Result<PreparedMutation, FootprintGraphicsError> {
    if !konnect_sexp::layers::is_canonical_name(layer) {
        return Err(invalid("selector.layer", "not a canonical KiCad layer"));
    }
    let tree = konnect_sexp::parse_sexp(source)
        .map_err(|error| invalid("footprint_path", format!("invalid S-expression: {error}")))?;
    if tree.head() != Some("footprint") {
        return Err(invalid(
            "footprint_path",
            crate::tools::footprint_root_reason(tree.head()),
        ));
    }

    let direct_children = find_direct_child_blocks(source, "footprint");
    let mut selected = Vec::new();
    let mut group_members = Vec::new();
    let mut insertion_anchor = None;
    for (start, end) in direct_children {
        let block = &source[start..end];
        let Ok(node) = konnect_sexp::parse_sexp(block) else {
            return Err(invalid(
                "footprint_path",
                "contains an invalid top-level item",
            ));
        };
        let Some(tag) = node.head() else {
            continue;
        };
        if insertion_anchor.is_none() && matches!(tag, "pad" | "group" | "model") {
            insertion_anchor =
                find_block_with_leading_whitespace(source, start).map(|range| range.0);
        }
        if tag == "group" {
            if let Some(members) = node.find("members").and_then(|members| members.children()) {
                group_members.extend(
                    members
                        .iter()
                        .skip(1)
                        .filter_map(|member| member.as_str())
                        .map(str::to_string),
                );
            }
        }
        if is_supported_graphic(tag) && node.find_str("layer") == Some(layer) {
            let range = find_block_with_leading_whitespace(source, start)
                .ok_or_else(|| invalid("footprint_path", "contains an unbalanced graphic"))?;
            let item_id = node
                .find_str("uuid")
                .or_else(|| node.find_str("tstamp"))
                .map(str::to_string);
            selected.push((range, item_id));
        }
    }

    if !matches!(mode, GraphicsMode::Append) {
        if let Some(item_id) = selected
            .iter()
            .filter_map(|(_, item_id)| item_id.as_deref())
            .find(|item_id| group_members.iter().any(|member| member == item_id))
        {
            return Err(FootprintGraphicsError::Conflict(format!(
                "selected graphic '{item_id}' is referenced by a footprint group"
            )));
        }
    }

    let indent = selected
        .first()
        .map(|(range, _)| {
            let block_start = find_balanced_block(source, range.0)
                .map(|range| range.0)
                .unwrap_or(range.0);
            child_indent(source, block_start)
        })
        .or_else(|| {
            insertion_anchor.map(|anchor| {
                let block_start = find_balanced_block(source, anchor)
                    .map(|range| range.0)
                    .unwrap_or(anchor);
                child_indent(source, block_start)
            })
        })
        .unwrap_or_else(|| "  ".to_string());
    let serialized = serialize_graphics(layer, graphics, &indent);
    let matched_count = selected.len();
    let added_count = if matches!(mode, GraphicsMode::Delete) {
        0
    } else {
        graphics.len()
    };

    let mut edits = Vec::new();
    match mode {
        GraphicsMode::Replace => {
            if let Some((first, rest)) = selected.split_first() {
                let (first_range, _) = first;
                edits.push(SexpEdit::replace(first_range.0, first_range.1, serialized));
                edits.extend(
                    rest.iter()
                        .map(|(range, _)| SexpEdit::delete(range.0, range.1)),
                );
            } else if !serialized.is_empty() {
                let root_end = find_balanced_block(source, 0)
                    .map(|range| range.1 - 1)
                    .ok_or_else(|| invalid("footprint_path", "unbalanced footprint root"))?;
                edits.push(SexpEdit::insert(
                    insertion_anchor.unwrap_or(root_end),
                    serialized,
                ));
            }
        }
        GraphicsMode::Append => {
            if !serialized.is_empty() {
                let root_end = find_balanced_block(source, 0)
                    .map(|range| range.1 - 1)
                    .ok_or_else(|| invalid("footprint_path", "unbalanced footprint root"))?;
                let offset = selected
                    .last()
                    .map(|(range, _)| range.1)
                    .or(insertion_anchor)
                    .unwrap_or(root_end);
                edits.push(SexpEdit::insert(offset, serialized));
            }
        }
        GraphicsMode::Delete => {
            edits.extend(
                selected
                    .iter()
                    .map(|(range, _)| SexpEdit::delete(range.0, range.1)),
            );
        }
    }

    let replacement = apply_edits(source.to_string(), edits);
    let parsed = konnect_sexp::parse_sexp(&replacement)
        .map_err(|error| invalid("graphics", format!("replacement does not parse: {error}")))?;
    if parsed.head() != Some("footprint") {
        return Err(FootprintGraphicsError::Conflict(
            "replacement changed the footprint root".to_string(),
        ));
    }

    Ok(PreparedMutation {
        replacement,
        matched_count,
        added_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use serde_json::json;
    use std::sync::Arc;

    const OLD_FOOTPRINT: &str = r#"(footprint "Fixture" (version 20221018) (generator pcbnew)
  (layer "F.Cu")
  (fp_line (start -1 -1) (end 1 -1)
    (stroke (width 0.1) (type solid)) (layer "F.SilkS")
    (tstamp 10000000-0000-4000-8000-000000000001))
  (fp_poly
    (pts (xy 0 0) (xy 2 0) (xy 2 2))
    (stroke (width 0.05) (type solid)) (fill none) (layer "B.CrtYd")
    (tstamp 20000000-0000-4000-8000-000000000001))
  (fp_text user "KEEP ON B.CrtYd" (at 0 0) (layer "B.CrtYd")
    (effects (font (size 1 1)))
    (tstamp 30000000-0000-4000-8000-000000000001))
  (fp_poly
    (pts (xy 3 3) (xy 4 3) (xy 4 4))
    (stroke (width 0.05) (type solid)) (fill none) (layer "B.CrtYd")
    (tstamp 20000000-0000-4000-8000-000000000002))
  (pad "1" thru_hole circle (at 0 0) (size 1.8 1.8) (drill 1)
    (layers "*.Cu" "*.Mask")
    (tstamp 40000000-0000-4000-8000-000000000001))
  (model "KEEP.step"
    (offset (xyz 0 0 0))
    (scale (xyz 1 1 1))
    (rotate (xyz 0 0 0)))
)
"#;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn result_body(result: &CallToolResult) -> serde_json::Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text response");
        };
        serde_json::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
    }

    #[test]
    fn serializes_every_supported_primitive_as_a_footprint_graphic() {
        let graphics = parse_graphics(&json!([
            {
                "type": "line",
                "start": {"x": 0.0, "y": 1.0},
                "end": {"x": 2.0, "y": 3.0},
                "stroke_width_mm": 0.05
            },
            {
                "type": "arc",
                "start": {"x": 0.0, "y": 0.0},
                "mid": {"x": 1.0, "y": 1.0},
                "end": {"x": 2.0, "y": 0.0},
                "stroke_width_mm": 0.05
            },
            {
                "type": "rect",
                "start": {"x": -1.0, "y": -2.0},
                "end": {"x": 3.0, "y": 4.0},
                "stroke_width_mm": 0.05,
                "fill": "none"
            },
            {
                "type": "circle",
                "center": {"x": 1.0, "y": 2.0},
                "radius_mm": 1.5,
                "stroke_width_mm": 0.05,
                "fill": "solid"
            },
            {
                "type": "poly",
                "points": [
                    {"x": 0.0, "y": 0.0},
                    {"x": 2.0, "y": 0.0},
                    {"x": 2.0, "y": 1.0}
                ],
                "stroke_width_mm": 0.05,
                "fill": "none"
            }
        ]))
        .expect("valid graphics");

        let output = serialize_graphics("B.CrtYd", &graphics, "  ");

        for tag in ["fp_line", "fp_arc", "fp_rect", "fp_circle", "fp_poly"] {
            assert!(
                output.contains(&format!("({tag}")),
                "missing {tag}:\n{output}"
            );
        }
        assert_eq!(output.matches("(layer \"B.CrtYd\")").count(), 5);
        assert_eq!(output.matches("(stroke (width 0.05)").count(), 5);
        assert_eq!(output.matches("(fill none)").count(), 2);
        assert_eq!(output.matches("(fill solid)").count(), 1);
        assert!(
            konnect_sexp::parse_sexp(&format!("(footprint \"T\" (layer \"F.Cu\"){output}\n)"))
                .is_ok()
        );
    }

    #[test]
    fn rejects_invalid_or_invisible_geometry() {
        let invalid = [
            json!([{
                "type": "line",
                "start": {"x": 0.0, "y": 0.0},
                "end": {"x": 1.0, "y": 1.0},
                "stroke_width_mm": -0.05
            }]),
            json!([{
                "type": "line",
                "start": {"x": 0.0, "y": 0.0},
                "end": {"x": 0.0, "y": 0.0},
                "stroke_width_mm": 0.05
            }]),
            json!([{
                "type": "rect",
                "start": {"x": 0.0, "y": 0.0},
                "end": {"x": 0.0, "y": 1.0},
                "stroke_width_mm": 0.05,
                "fill": "none"
            }]),
            json!([{
                "type": "circle",
                "center": {"x": 0.0, "y": 0.0},
                "radius_mm": 0.0,
                "stroke_width_mm": 0.05,
                "fill": "none"
            }]),
            json!([{
                "type": "arc",
                "start": {"x": 0.0, "y": 0.0},
                "mid": {"x": 1.0, "y": 1.0},
                "end": {"x": 2.0, "y": 2.0},
                "stroke_width_mm": 0.05
            }]),
            json!([{
                "type": "poly",
                "points": [
                    {"x": 0.0, "y": 0.0},
                    {"x": 1.0, "y": 0.0},
                    {"x": 0.0, "y": 0.0}
                ],
                "stroke_width_mm": 0.05,
                "fill": "none"
            }]),
            json!([{
                "type": "poly",
                "points": [
                    {"x": 0.0, "y": 0.0},
                    {"x": 1.0, "y": 0.0},
                    {"x": 1.0, "y": 1.0}
                ],
                "stroke_width_mm": 0.0,
                "fill": "none"
            }]),
        ];

        for value in invalid {
            assert!(parse_graphics(&value).is_err(), "accepted {value}");
        }

        let non_finite = json!([{
            "type": "circle",
            "center": {"x": 0.0, "y": 0.0},
            "radius_mm": 1.0,
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]);
        let mut non_finite = non_finite;
        non_finite[0]["center"]["x"] = serde_json::Value::from(f64::INFINITY);
        assert!(parse_graphics(&non_finite).is_err());
    }

    #[test]
    fn rejects_a_circle_whose_derived_end_point_overflows() {
        let value = json!([{
            "type": "circle",
            "center": {"x": f64::MAX, "y": 0.0},
            "radius_mm": f64::MAX,
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]);

        assert!(parse_graphics(&value).is_err());
    }

    #[test]
    fn normalizes_a_repeated_polygon_closing_point() {
        let graphics = parse_graphics(&json!([{
            "type": "poly",
            "points": [
                {"x": 0.0, "y": 0.0},
                {"x": 2.0, "y": 0.0},
                {"x": 2.0, "y": 1.0},
                {"x": 0.0, "y": 0.0}
            ],
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]))
        .unwrap();

        let output = serialize_graphics("B.CrtYd", &graphics, "  ");
        assert_eq!(output.matches("(xy 0 0)").count(), 1, "{output}");
    }

    #[test]
    fn replace_changes_only_supported_graphics_on_the_selected_layer() {
        let graphics = parse_graphics(&json!([{
            "type": "poly",
            "points": [
                {"x": -8.695, "y": 3.615},
                {"x": -8.690, "y": 3.566},
                {"x": -8.650, "y": 3.500}
            ],
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]))
        .unwrap();

        let prepared =
            prepare_mutation(OLD_FOOTPRINT, "B.CrtYd", GraphicsMode::Replace, &graphics).unwrap();

        assert_eq!(prepared.matched_count, 2);
        assert_eq!(prepared.added_count, 1);
        assert_eq!(prepared.replacement.matches("(fp_poly").count(), 1);
        assert!(!prepared
            .replacement
            .contains("20000000-0000-4000-8000-000000000001"));
        assert!(!prepared
            .replacement
            .contains("20000000-0000-4000-8000-000000000002"));
        for unchanged in [
            r#"(fp_line (start -1 -1) (end 1 -1)
    (stroke (width 0.1) (type solid)) (layer "F.SilkS")
    (tstamp 10000000-0000-4000-8000-000000000001))"#,
            r#"(fp_text user "KEEP ON B.CrtYd" (at 0 0) (layer "B.CrtYd")
    (effects (font (size 1 1)))
    (tstamp 30000000-0000-4000-8000-000000000001))"#,
            r#"(pad "1" thru_hole circle (at 0 0) (size 1.8 1.8) (drill 1)
    (layers "*.Cu" "*.Mask")
    (tstamp 40000000-0000-4000-8000-000000000001))"#,
            r#"(model "KEEP.step"
    (offset (xyz 0 0 0))
    (scale (xyz 1 1 1))
    (rotate (xyz 0 0 0)))"#,
        ] {
            assert!(
                prepared.replacement.contains(unchanged),
                "changed unrelated block:\n{unchanged}\n---\n{}",
                prepared.replacement
            );
        }
        assert!(konnect_sexp::parse_sexp(&prepared.replacement).is_ok());
    }

    #[test]
    fn append_preserves_existing_graphics_and_accepts_two_polygons() {
        let graphics = parse_graphics(&json!([
            {
                "type": "poly",
                "points": [
                    {"x": 10.0, "y": 10.0},
                    {"x": 12.0, "y": 10.0},
                    {"x": 12.0, "y": 12.0}
                ],
                "stroke_width_mm": 0.05,
                "fill": "none"
            },
            {
                "type": "poly",
                "points": [
                    {"x": -10.0, "y": -10.0},
                    {"x": -12.0, "y": -10.0},
                    {"x": -12.0, "y": -12.0}
                ],
                "stroke_width_mm": 0.05,
                "fill": "none"
            }
        ]))
        .unwrap();

        let prepared =
            prepare_mutation(OLD_FOOTPRINT, "B.CrtYd", GraphicsMode::Append, &graphics).unwrap();

        assert_eq!(prepared.matched_count, 2);
        assert_eq!(prepared.added_count, 2);
        assert_eq!(prepared.replacement.matches("(fp_poly").count(), 4);
        assert!(prepared
            .replacement
            .contains("20000000-0000-4000-8000-000000000001"));
        assert!(prepared
            .replacement
            .contains("20000000-0000-4000-8000-000000000002"));
        assert!(konnect_sexp::parse_sexp(&prepared.replacement).is_ok());
    }

    #[test]
    fn delete_removes_only_selected_supported_graphics() {
        let prepared =
            prepare_mutation(OLD_FOOTPRINT, "B.CrtYd", GraphicsMode::Delete, &[]).unwrap();

        assert_eq!(prepared.matched_count, 2);
        assert_eq!(prepared.added_count, 0);
        assert!(!prepared.replacement.contains("(fp_poly"));
        assert!(prepared
            .replacement
            .contains(r#"(fp_text user "KEEP ON B.CrtYd""#));
        assert!(prepared.replacement.contains("(pad \"1\""));
        assert!(prepared.replacement.contains("(model \"KEEP.step\""));
        assert!(konnect_sexp::parse_sexp(&prepared.replacement).is_ok());
    }

    #[test]
    fn replace_without_a_match_inserts_before_pads_and_models() {
        let source = r#"(footprint "NoGraphics" (version 20221018) (generator pcbnew)
  (layer "F.Cu")
  (pad "1" thru_hole circle (at 0 0) (size 1.8 1.8) (drill 1)
    (layers "*.Cu" "*.Mask"))
  (model "KEEP.step")
)
"#;
        let graphics = parse_graphics(&json!([{
            "type": "circle",
            "center": {"x": 0.0, "y": 0.0},
            "radius_mm": 2.0,
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]))
        .unwrap();

        let prepared =
            prepare_mutation(source, "B.CrtYd", GraphicsMode::Replace, &graphics).unwrap();

        assert_eq!(prepared.matched_count, 0);
        assert!(
            prepared.replacement.find("(fp_circle").unwrap()
                < prepared.replacement.find("(pad \"1\"").unwrap(),
            "{}",
            prepared.replacement
        );
        assert!(konnect_sexp::parse_sexp(&prepared.replacement).is_ok());
    }

    #[test]
    fn grouped_selected_graphics_are_not_deleted_in_old_or_new_syntax() {
        let old_group = OLD_FOOTPRINT.replace(
            "  (model \"KEEP.step\"",
            r#"  (group "owned" (id 50000000-0000-4000-8000-000000000001)
    (members
      20000000-0000-4000-8000-000000000001
    )
  )
  (model "KEEP.step""#,
        );
        let new_group = OLD_FOOTPRINT.replace(
            "  (model \"KEEP.step\"",
            r#"  (group "owned"
    (uuid "50000000-0000-4000-8000-000000000001")
    (members "20000000-0000-4000-8000-000000000002")
  )
  (model "KEEP.step""#,
        );

        for source in [old_group, new_group] {
            let error =
                prepare_mutation(&source, "B.CrtYd", GraphicsMode::Delete, &[]).unwrap_err();
            assert!(
                matches!(error, FootprintGraphicsError::Conflict(_)),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_non_footprint_roots_and_noncanonical_layers() {
        let graphics = parse_graphics(&json!([{
            "type": "line",
            "start": {"x": 0.0, "y": 0.0},
            "end": {"x": 1.0, "y": 1.0},
            "stroke_width_mm": 0.05
        }]))
        .unwrap();

        for (source, layer) in [
            ("(kicad_pcb (version 20240108))", "B.CrtYd"),
            (OLD_FOOTPRINT, "BottomCourtyard"),
        ] {
            assert!(matches!(
                prepare_mutation(source, layer, GraphicsMode::Replace, &graphics),
                Err(FootprintGraphicsError::InvalidArgument { .. })
            ));
        }
    }

    #[test]
    fn library_exposes_the_atomic_graphics_schema() {
        let tool = crate::tools::library::tools()
            .into_iter()
            .find(|tool| tool.name == "set_footprint_graphics")
            .expect("library must expose set_footprint_graphics");
        let schema = tool.input_schema;

        assert_eq!(
            schema["required"],
            json!(["footprint_path", "selector", "mode"])
        );
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            json!(["append", "replace", "delete"])
        );
        assert_eq!(
            schema["properties"]["selector"]["required"],
            json!(["layer"])
        );

        let variants = schema["properties"]["graphics"]["items"]["oneOf"]
            .as_array()
            .expect("graphics oneOf variants");
        let types = variants
            .iter()
            .map(|variant| {
                variant["properties"]["type"]["const"]
                    .as_str()
                    .expect("type const")
            })
            .collect::<Vec<_>>();
        assert_eq!(types, ["line", "arc", "rect", "circle", "poly"]);

        let required = variants
            .iter()
            .map(|variant| variant["required"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            required,
            vec![
                json!(["type", "start", "end", "stroke_width_mm"]),
                json!(["type", "start", "mid", "end", "stroke_width_mm"]),
                json!(["type", "start", "end", "stroke_width_mm", "fill"]),
                json!(["type", "center", "radius_mm", "stroke_width_mm", "fill"]),
                json!(["type", "points", "stroke_width_mm", "fill"]),
            ]
        );
    }

    #[test]
    fn inspects_old_and_new_footprint_graphic_syntax() {
        let source = r#"(footprint "Inspect" (version 20240108) (generator "pcbnew")
  (layer "F.Cu")
  (fp_line (start 0 1) (end 2 3)
    (stroke (width 0.05) (type solid)) (layer "F.SilkS")
    (tstamp 10000000-0000-4000-8000-000000000001))
  (fp_arc (start 0 0) (mid 1 1) (end 2 0)
    (stroke (width 0.06) (type solid)) (layer "B.CrtYd")
    (uuid "20000000-0000-4000-8000-000000000001"))
  (fp_rect (start -1 -2) (end 3 4)
    (stroke (width 0.07) (type solid)) (fill no) (layer "B.CrtYd")
    (uuid "30000000-0000-4000-8000-000000000001"))
  (fp_circle (center 1 2) (end 2.5 2)
    (stroke (width 0.08) (type solid)) (fill yes) (layer "B.CrtYd")
    (tstamp 40000000-0000-4000-8000-000000000001))
  (fp_poly
    (pts (xy 0 0) (xy 2 0) (xy 2 1))
    (stroke (width 0.09) (type solid)) (fill none) (layer "B.CrtYd"))
  (fp_text user "ignored" (at 0 0) (layer "B.CrtYd")
    (effects (font (size 1 1))))
  (pad "1" thru_hole circle (at 0 0) (size 1.8 1.8) (drill 1)
    (layers "*.Cu" "*.Mask"))
)
"#;

        let graphics = inspect_graphics(source, None).unwrap();

        assert_eq!(
            serde_json::to_value(&graphics).unwrap(),
            json!([
                {
                    "type": "line",
                    "layer": "F.SilkS",
                    "start": {"x": 0.0, "y": 1.0},
                    "end": {"x": 2.0, "y": 3.0},
                    "stroke_width_mm": 0.05,
                    "item_id": "10000000-0000-4000-8000-000000000001"
                },
                {
                    "type": "arc",
                    "layer": "B.CrtYd",
                    "start": {"x": 0.0, "y": 0.0},
                    "mid": {"x": 1.0, "y": 1.0},
                    "end": {"x": 2.0, "y": 0.0},
                    "stroke_width_mm": 0.06,
                    "item_id": "20000000-0000-4000-8000-000000000001"
                },
                {
                    "type": "rect",
                    "layer": "B.CrtYd",
                    "start": {"x": -1.0, "y": -2.0},
                    "end": {"x": 3.0, "y": 4.0},
                    "stroke_width_mm": 0.07,
                    "fill": "none",
                    "item_id": "30000000-0000-4000-8000-000000000001"
                },
                {
                    "type": "circle",
                    "layer": "B.CrtYd",
                    "center": {"x": 1.0, "y": 2.0},
                    "radius_mm": 1.5,
                    "stroke_width_mm": 0.08,
                    "fill": "solid",
                    "item_id": "40000000-0000-4000-8000-000000000001"
                },
                {
                    "type": "poly",
                    "layer": "B.CrtYd",
                    "points": [
                        {"x": 0.0, "y": 0.0},
                        {"x": 2.0, "y": 0.0},
                        {"x": 2.0, "y": 1.0}
                    ],
                    "point_count": 3,
                    "closed": true,
                    "stroke_width_mm": 0.09,
                    "fill": "none",
                    "item_id": null
                }
            ])
        );

        let courtyard = inspect_graphics(source, Some("B.CrtYd")).unwrap();
        assert_eq!(courtyard.len(), 4);
        assert!(courtyard.iter().all(|graphic| graphic.layer == "B.CrtYd"));
    }

    #[tokio::test]
    async fn handler_replaces_graphics_and_reports_counts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Fixture.kicad_mod");
        std::fs::write(&path, OLD_FOOTPRINT).unwrap();
        let args = json!({
            "footprint_path": path,
            "selector": {"layer": "B.CrtYd"},
            "mode": "replace",
            "graphics": [{
                "type": "poly",
                "points": [
                    {"x": -8.695, "y": 3.615},
                    {"x": -8.690, "y": 3.566},
                    {"x": -8.650, "y": 3.500}
                ],
                "stroke_width_mm": 0.05,
                "fill": "none"
            }]
        });

        let result = handle_set_footprint_graphics(&args, &test_ctx())
            .await
            .unwrap();

        assert!(!result.is_error, "{:?}", result.content);
        let response = result_body(&result);
        assert_eq!(response["mode"], "replace");
        assert_eq!(response["layer"], "B.CrtYd");
        assert_eq!(response["matched_count"], 2);
        assert_eq!(response["added_count"], 1);

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.matches("(fp_poly").count(), 1);
        assert!(written.contains("(xy -8.695 3.615)"));
        assert!(written.contains("(pad \"1\""));
        assert!(written.contains("(model \"KEEP.step\""));
        assert!(konnect_sexp::parse_sexp(&written).is_ok());
    }

    #[tokio::test]
    async fn handler_append_and_delete_follow_the_public_mode_contract() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Fixture.kicad_mod");
        std::fs::write(&path, OLD_FOOTPRINT).unwrap();
        let append = json!({
            "footprint_path": path,
            "selector": {"layer": "B.CrtYd"},
            "mode": "append",
            "graphics": [{
                "type": "circle",
                "center": {"x": 0.0, "y": 0.0},
                "radius_mm": 2.0,
                "stroke_width_mm": 0.05,
                "fill": "none"
            }]
        });

        let result = handle_set_footprint_graphics(&append, &test_ctx())
            .await
            .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result_body(&result)["matched_count"], 2);
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .matches("(fp_poly")
                .count(),
            2
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("(fp_circle"));

        let delete = json!({
            "footprint_path": path,
            "selector": {"layer": "B.CrtYd"},
            "mode": "delete"
        });
        let result = handle_set_footprint_graphics(&delete, &test_ctx())
            .await
            .unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result_body(&result)["matched_count"], 3);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("(fp_poly"));
        assert!(!written.contains("(fp_circle"));
        assert!(written.contains(r#"(fp_text user "KEEP ON B.CrtYd""#));
    }

    #[tokio::test]
    async fn handler_returns_structured_argument_and_file_errors() {
        let temp = tempfile::tempdir().unwrap();
        let valid_path = temp.path().join("Fixture.kicad_mod");
        std::fs::write(&valid_path, OLD_FOOTPRINT).unwrap();
        let directory_path = temp.path().join("Directory.kicad_mod");
        std::fs::create_dir(&directory_path).unwrap();
        let malformed_path = temp.path().join("Malformed.kicad_mod");
        std::fs::write(&malformed_path, "(kicad_pcb)").unwrap();
        let missing_path = temp.path().join("Missing.kicad_mod");

        let cases = [
            (
                json!({
                    "footprint_path": missing_path,
                    "selector": {"layer": "B.CrtYd"},
                    "mode": "delete"
                }),
                "file_not_found",
                None,
            ),
            (
                json!({
                    "footprint_path": temp.path().join("Fixture.txt"),
                    "selector": {"layer": "B.CrtYd"},
                    "mode": "delete"
                }),
                "invalid_argument",
                Some("footprint_path"),
            ),
            (
                json!({
                    "footprint_path": directory_path,
                    "selector": {"layer": "B.CrtYd"},
                    "mode": "delete"
                }),
                "invalid_argument",
                Some("footprint_path"),
            ),
            (
                json!({
                    "footprint_path": valid_path,
                    "selector": {"layer": "BottomCourtyard"},
                    "mode": "delete"
                }),
                "invalid_argument",
                Some("selector.layer"),
            ),
            (
                json!({
                    "footprint_path": valid_path,
                    "selector": {"layer": "B.CrtYd"},
                    "mode": "overwrite"
                }),
                "invalid_argument",
                Some("mode"),
            ),
            (
                json!({
                    "footprint_path": valid_path,
                    "selector": {"layer": "B.CrtYd"},
                    "mode": "replace"
                }),
                "invalid_argument",
                Some("graphics"),
            ),
            (
                json!({
                    "footprint_path": valid_path,
                    "selector": {"layer": "B.CrtYd"},
                    "mode": "delete",
                    "graphics": [{"type": "line"}]
                }),
                "invalid_argument",
                Some("graphics"),
            ),
            (
                json!({
                    "footprint_path": malformed_path,
                    "selector": {"layer": "B.CrtYd"},
                    "mode": "delete"
                }),
                "invalid_argument",
                Some("footprint_path"),
            ),
        ];

        for (args, kind, field) in cases {
            let result = handle_set_footprint_graphics(&args, &test_ctx())
                .await
                .unwrap_or_else(|error| panic!("handler bubbled an error for {args}: {error}"));
            assert!(result.is_error, "expected failure for {args}");
            let body = result_body(&result);
            assert_eq!(body["error"]["kind"], kind, "{args}: {body}");
            if let Some(field) = field {
                assert_eq!(body["error"]["field"], field, "{args}: {body}");
            }
        }
    }

    #[tokio::test]
    async fn handler_reports_group_references_as_a_structured_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Fixture.kicad_mod");
        let grouped = OLD_FOOTPRINT.replace(
            "  (model \"KEEP.step\"",
            r#"  (group "owned" (id 50000000-0000-4000-8000-000000000001)
    (members
      20000000-0000-4000-8000-000000000001
    )
  )
  (model "KEEP.step""#,
        );
        std::fs::write(&path, &grouped).unwrap();
        let args = json!({
            "footprint_path": path,
            "selector": {"layer": "B.CrtYd"},
            "mode": "delete"
        });

        let result = handle_set_footprint_graphics(&args, &test_ctx())
            .await
            .unwrap();

        assert!(result.is_error);
        assert_eq!(result_body(&result)["error"]["kind"], "conflict");
        assert_eq!(std::fs::read_to_string(path).unwrap(), grouped);
    }

    #[test]
    fn persistence_rejects_a_stale_source_without_overwriting_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Fixture.kicad_mod");
        std::fs::write(&path, OLD_FOOTPRINT).unwrap();
        let graphics = parse_graphics(&json!([{
            "type": "circle",
            "center": {"x": 0.0, "y": 0.0},
            "radius_mm": 2.0,
            "stroke_width_mm": 0.05,
            "fill": "none"
        }]))
        .unwrap();
        let prepared =
            prepare_mutation(OLD_FOOTPRINT, "B.CrtYd", GraphicsMode::Replace, &graphics).unwrap();

        let externally_changed = OLD_FOOTPRINT.replace("Fixture", "ExternallyChanged");
        std::fs::write(&path, &externally_changed).unwrap();

        let error = persist_replacement(&path, OLD_FOOTPRINT, &prepared.replacement).unwrap_err();

        assert!(matches!(error, konnect_sexp::SexpError::Conflict { .. }));
        assert_eq!(std::fs::read_to_string(path).unwrap(), externally_changed);
    }
}
